//! The gapless path: preparing the next item behind the playing one and
//! swapping it in at the boundary.

use std::sync::{Arc, Weak, atomic::Ordering};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use parking_lot::{Condvar, Mutex};
use tracing::{debug, error, info};

use crate::{
    FcastPlaybin, Inner,
    api::PlaybinEvent,
    jobs::Job,
    routing::{Input, StreamKind},
    selection,
};

/// A pre-armed next item (see [`FcastPlaybin::prepare_next_async`]). Its
/// input element is ALSO registered in `RoutingState::inputs` (under its
/// future generation), so the ordinary input machinery covers linking,
/// bitrate taps, and removal. This record carries what the gapless
/// transition additionally needs: the activation identity (which element,
/// which generation) and the held-back collection.
pub(crate) struct PreparedNext {
    pub(crate) element: gst::Element,
    /// The generation the item adopts when it activates (returned by
    /// `prepare_next_async` so the caller can correlate).
    pub(crate) generation: u64,
    /// The next item's stream collection, held back until activation so the
    /// caller sees it AFTER [`PlaybinEvent::PreparedActivated`], stamped
    /// with the new generation: the same collection-then-selected order a
    /// fresh load produces.
    pub(crate) pending_collection: Option<gst::StreamCollection>,
}

impl PreparedNext {
    /// The stream ids the prepared input has produced so far (empty until
    /// its pads exist, guaranteed populated by the time decodebin3 selects
    /// them).
    fn stream_ids(&self) -> Vec<String> {
        self.element
            .src_pads()
            .iter()
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
    }
}

/// Coordination between the current input's drain watches and the prepared
/// input's blocked streaming threads (the uridecodebin3 recipe: the
/// prepared input's first buffer blocks in a pad probe until the current
/// input has pushed EOS into decodebin3 on every pad, then that same probe
/// performs the relink).
#[derive(Default)]
pub(crate) struct SwapState {
    /// The generation of the pending prepared input. `None`: no gapless
    /// swap pending (never armed, cancelled, or already activated).
    pub(crate) pending: Option<u64>,
    /// Every main-input pad has pushed its EOS into decodebin3.
    pub(crate) drained: bool,
    /// The relink surgery ran; remaining block probes just remove
    /// themselves and let their data flow.
    pub(crate) swapped: bool,
    /// The output-side hold dropped an EOS while this swap was pending: the
    /// current item's end has been consumed for good, so a cancel must
    /// synthesize it or the caller never learns the item ended. An input
    /// pushing EOS into decodebin3 (`drained`) is NOT that: until the EOS
    /// re-emerges at the outputs it still flows normally once the hold
    /// disarms.
    pub(crate) dropped_eos: bool,
}

impl SwapState {
    /// The generation of a swap that has already PERFORMED and is still
    /// waiting to activate, if any. In that window the prepared input is the
    /// only linked upstream, so anything asking upstream (a timeline query, a
    /// cancel's surgery) is describing the SUCCESSOR item, not the one still
    /// coming out of the sinks.
    ///
    /// `swapped` alone is not the window: a long-completed activation leaves
    /// `swapped` set with `pending` cleared.
    pub(crate) fn activation_pending(&self) -> Option<u64> {
        self.pending.filter(|_| self.swapped)
    }
}

#[derive(Default)]
pub(crate) struct SwapGate {
    pub(crate) state: Mutex<SwapState>,
    pub(crate) cond: Condvar,
}

impl SwapGate {
    /// Abort any pending swap and wake every thread parked on the gate.
    /// MUST run before any downward pipeline transition while a prepare may
    /// be pending: a state change joins streaming threads, and a prepared
    /// pad's thread parked in the gate's condvar would deadlock it.
    /// Returns the aborted state for the caller's cleanup decisions.
    pub(crate) fn abort(&self) -> SwapState {
        let mut state = self.state.lock();
        let aborted = std::mem::take(&mut *state);
        self.cond.notify_all();
        aborted
    }
}

/// What a `cancel_prepared` did. The distinction is load-bearing for the
/// caller's pre-arm bookkeeping (see [`PlaybinEvent::PreparedCancelDeclined`])
/// and for a `Job::PrepareNext` deciding whether it may arm over the slot.
pub(crate) enum CancelOutcome {
    /// Nothing is prepared any more and no activation will fire.
    /// `generation` names the dropped prepare, `None` for a no-op cancel
    /// (nothing was prepared).
    Cancelled { generation: Option<u64> },
    /// A performed swap made cancellation impossible: the relink is live, the
    /// activation of `generation` is imminent and was left to finish.
    Declined { generation: u64 },
}

/// The user-facing half of a gapless activation, held back from decodebin3's
/// output until the new item's audio actually reaches the sink. See
/// [`Inner::held_activation`].
pub(crate) struct HeldActivation {
    /// The new item's stream collection, re-emitted right after
    /// [`PlaybinEvent::PreparedActivated`] (the fresh-load ordering the caller
    /// relies on).
    collection: Option<gst::StreamCollection>,
}

impl Inner {
    /// Whether a bus message originates inside the prepared next input.
    /// Its buffering messages must not drive the caller's buffering state
    /// machine while the CURRENT item plays, its own (parsebin-posted) stream
    /// collection belongs to the next item, and so does any duration it
    /// refines.
    pub(crate) fn message_from_prepared_input(&self, msg: &gst::Message) -> bool {
        let Some(src) = msg.src() else {
            return false;
        };
        let prepared = self.prepared.lock();
        prepared.as_ref().is_some_and(|p| {
            src == p.element.upcast_ref::<gst::Object>() || src.has_as_ancestor(&p.element)
        })
    }

    /// Whether the prepared input's output is HELD until the boundary
    /// activation links it. The crate's default, and what makes an ADAPTIVE
    /// item preparable at all.
    ///
    /// The hold itself is the block probes [`Self::add_prepared_input`]
    /// installs, and they are enough on their own: a probe blocks before the
    /// peer check (`gst_pad_push_data` runs its probes, THEN looks for a
    /// peer), so a demuxer that runs its own output loop the instant it is
    /// PAUSED (every adaptive one does) parks in the probe instead of
    /// pushing into the unlinked pad. What the hold needs is for nothing to
    /// dismantle it early, and exactly one thing did: the prepared input's
    /// OWN streams-selected, mistaken for the boundary (see the filter in
    /// `Inner::translate_message`). With the generation adopted, the probes
    /// take themselves off as stragglers and the hold is gone.
    ///
    /// Lever: `FCAST_NO_ADAPTIVE_PREPARE_HOLD` (set = off) restores the
    /// pre-fix reading, i.e. the prepared input's self-report counts as the
    /// activation. It is the bite: with it set, a DASH prepare errors
    /// `not-linked` within ~50 ms and takes the playing item with it.
    pub(crate) fn adaptive_prepare_hold(&self) -> bool {
        std::env::var_os("FCAST_NO_ADAPTIVE_PREPARE_HOLD").is_none()
    }

    /// Whether a stream collection consists purely of the prepared input's
    /// streams. Catches the decodebin3-posted form of the next item's
    /// collection (whose message src is decodebin3, not the input).
    pub(crate) fn collection_is_prepared(&self, collection: &gst::StreamCollection) -> bool {
        let prepared = self.prepared.lock();
        let Some(prepared) = prepared.as_ref() else {
            return false;
        };
        let ids = prepared.stream_ids();
        if ids.is_empty() {
            return false;
        }
        let mut any = false;
        for stream in collection.iter() {
            let Some(sid) = stream.stream_id() else {
                continue;
            };
            if ids.iter().any(|id| *id == sid) {
                any = true;
            } else {
                // A stream from elsewhere: a combined or current-item
                // collection, not the prepared item's.
                return false;
            }
        }
        any
    }

    /// Refresh the recorded output groups from each routed pad's sticky
    /// stream-start. Route-time recording is best-effort (the first
    /// stream-start can pass while the pad is being routed, before the
    /// probe exists); by prepare time the stickies are authoritative.
    pub(crate) fn refresh_output_groups(inner: &Arc<Inner>) {
        let mut routing = inner.routing.lock();
        let mut seen = None;
        for routed in routing.routed.iter_mut() {
            if let Some(group) = routed
                .db3_src_pad
                .sticky_event::<gst::event::StreamStart>(0)
                .and_then(|event| event.group_id())
            {
                routed.group = Some(group);
                seen = Some(group);
            }
        }
        drop(routing);
        if let Some(group) = seen {
            let mut active = inner.active_group.lock();
            if active.is_none() {
                *active = Some(group);
            }
        }
    }

    /// Output-side activation trigger: a STREAM_START on a decodebin3
    /// output pad carries a new group id. When a prepared item is pending,
    /// the group change IS the switch (a same-slot continuation posts no
    /// new streams-selected, so the data plane is the reliable signal).
    pub(crate) fn note_output_stream_start(&self, group: Option<gst::GroupId>) {
        let Some(group) = group else { return };
        let retired = {
            let mut active = self.active_group.lock();
            if *active == Some(group) {
                return;
            }
            let first_of_load = active.is_none();
            let retired = *active;
            *active = Some(group);
            if first_of_load {
                return;
            }
            retired
        };
        let prepared = self.prepared.lock().take();
        if let Some(prepared) = prepared {
            info!(
                generation = prepared.generation,
                "gapless activation: the prepared item's group reached the output"
            );
            self.activate_prepared_now(prepared, retired);
        }
    }

    /// The gapless EOS-hold decision, shared by the output gate and the
    /// post-streamsynchronizer gate: an EOS on a pad whose stream group is
    /// `pad_group` must be dropped while a swap is pending (committed to a
    /// next item, nothing may end the pipeline) or while the pad still
    /// carries a non-active group, either lagging the active one or
    /// positively the RETIRED one (old-item drainage, see
    /// [`Inner::retired_group`]). Unknowns on either side never drop: only
    /// a positively known group mismatch is old-item drainage. A pending
    /// drop is recorded for the cancel synthesis (see
    /// [`SwapState::dropped_eos`]). Returns (drop, pending, behind).
    pub(crate) fn gapless_eos_check_and_mark(
        &self,
        pad_group: Option<gst::GroupId>,
    ) -> (bool, bool, bool) {
        let active_group = *self.active_group.lock();
        let retired_group = *self.retired_group.lock();
        let behind = match (pad_group, active_group) {
            (Some(pad_group), Some(active)) => pad_group != active,
            _ => false,
        } || (pad_group.is_some() && pad_group == retired_group);
        // One lock hold for the check AND the drop record: a cancel between
        // them would zero the state and the mark would pollute the next
        // prepare's gate.
        let mut state = self.swap_gate.state.lock();
        let pending = state.pending.is_some();
        if pending {
            // The item's end is consumed for good; a cancelled swap must
            // synthesize it (see `SwapState::dropped_eos`).
            state.dropped_eos = true;
        }
        (pending || behind, pending, behind)
    }

    /// A full-pipeline flushing seek went out. It reset
    /// streamsynchronizer's per-pad EOS bookkeeping and restarted every
    /// branch it reached, so the passing-EOS mirror is stale FOR THE LIVE
    /// BRANCHES. It is cleared only when a live video branch received the
    /// flush. A video stream deselected at seek time is not restarted by
    /// the seek, so its drained state stands and the drained-resurrect park
    /// in `route_db3_pad` must still see the mirror. Gated on that park's
    /// lever, `FCAST_NO_DRAINED_RESURRECT_PARK` (without the park the
    /// mirror has no route-time reader and the pre-existing behavior never
    /// cleared it).
    pub(crate) fn clear_passing_eos_after_flush(&self) {
        if std::env::var_os("FCAST_NO_DRAINED_RESURRECT_PARK").is_some() {
            return;
        }
        let video_live = self
            .routing
            .lock()
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Video && r.downstream.is_some());
        if video_live {
            *self.passing_eos_group.lock() = None;
        }
    }

    /// Selection-side activation trigger, run against every
    /// STREAMS_SELECTED: when the selection names (only) the prepared
    /// input's streams, decodebin3 has switched to the next item. Some
    /// switches post this (fresh slots), same-slot continuations do not
    /// (see [`Self::note_output_stream_start`], the other trigger).
    pub(crate) fn try_activate_prepared(&self, selected_ids: &[String]) {
        let prepared = {
            let mut slot = self.prepared.lock();
            let Some(prepared) = slot.as_ref() else {
                return;
            };
            let ids = prepared.stream_ids();
            if selected_ids.is_empty()
                || !selected_ids
                    .iter()
                    .all(|sel| ids.iter().any(|id| id == sel))
            {
                return;
            }
            slot.take().expect("checked above")
        };
        info!(
            generation = prepared.generation,
            "gapless activation: the prepared item's streams are selected"
        );
        // This trigger fires input-side, BEFORE the output pads flip to the
        // new group: the still-active group is the one being retired.
        let retired = *self.active_group.lock();
        self.activate_prepared_now(prepared, retired);
    }

    /// The activation itself: adopt the prepared generation NOW, on the
    /// calling posting/streaming thread, so everything after it is stamped
    /// with the new one; re-seed the per-item state exactly like a load's
    /// reset; and queue the input surgery for the worker. The user-facing
    /// [`PlaybinEvent::PreparedActivated`] and the held-back collection are
    /// NOT emitted here for an item with audio: they are held and released
    /// when the item's audio reaches the sink (see [`Inner::held_activation`]),
    /// so the title/duration switch lands with the sound. Audio-less items
    /// emit them here, keeping the activation-then-collection order.
    fn activate_prepared_now(&self, prepared: PreparedNext, retired: Option<gst::GroupId>) {
        // A prior hold still waiting on its sink boundary (unreachable with
        // real media, two swaps within one queue depth) is flushed under the
        // outgoing generation, before we adopt this one, to keep order and
        // stamps correct.
        self.release_held_activation();

        self.generation.store(prepared.generation, Ordering::SeqCst);

        // Output pads still carrying the previous item's group keep their
        // EOS dropped until they flip or die (see `Inner::retired_group`):
        // the selection trigger can run this while the old tail is still
        // draining.
        *self.retired_group.lock() = retired;

        // The swap window closes: the output EOS hold disarms (the new
        // item's own end must flow) while `swapped` stays set so straggler
        // block probes on the now-live input self-remove. The next prepare
        // re-seeds the whole gate. Guarded by generation: a concurrent
        // prepare for the item after this one may have re-armed the gate
        // already, and ITS pending must survive this activation.
        {
            let mut state = self.swap_gate.state.lock();
            if state.pending == Some(prepared.generation) {
                state.pending = None;
                state.dropped_eos = false;
            }
        }

        // Per-item state rolls like a load's reset, EXCEPT the user's own track
        // intent: see `SelectionEngine::reset_across_gapless`. A plain `reset()`
        // here discarded a subtitle-off and the boundary relinked the text
        // branch the user had turned off
        // (`regression_gapless::subtitle_disable_survives_a_gapless_transition`).
        self.selection.lock().reset_across_gapless();
        *self.last_applied_subtitle.lock() = None;
        *self.upstream_selection.lock() = None;
        self.last_upstream_ids.lock().clear();
        // The outgoing item's cues describe a timeline this pipeline has just
        // left. Nothing else tells the renderer so at a gapless boundary: no
        // flush crosses it (that is the point of gapless), and the branch
        // disposal that eventually sends a `Clear` belongs to the input
        // removal, which happens later and may not happen at all before the
        // next cue is due. An OPEN-ENDED cue (a buffer with no duration, so
        // `end_rt` is `None`) would otherwise survive from item N into item
        // N+1 and sit there until N+1 happens to produce one of its own.
        self.send_subtitle_clear();
        // NOTHING TO CLEAR HERE ANY MORE, and the reason is worth keeping.
        // The eager text-branch work used to be a coalescing INTENT slot, and
        // this activation was the only per-item reset that never emptied it
        // (the load reset and the stop both did), so an intent recorded
        // against the outgoing item and still undrained at the boundary was
        // executed by the next settled-PLAYING drain against the INCOMING
        // item's branches, papered over for a while by `dispatch_selection`
        // re-reading `replacing`. Deleting subtitleoverlay took the slot with
        // the flush it carried: the only eager work left is the
        // PARK, and the park runs inline on the deciding thread at the moment
        // it is decided, so there is no per-item state left to leak across a
        // gapless boundary. `FCAST_NO_ACTIVATION_TEXT_WORK_CLEAR` went with it.
        *self.intended_timeline.lock() = (1.0, gst::ClockTime::ZERO);
        {
            let mut routing = self.routing.lock();
            routing.collection_video_ids = prepared
                .pending_collection
                .as_ref()
                .map(|collection| {
                    collection
                        .iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::VIDEO))
                        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                        .collect()
                })
                .unwrap_or_default();
        }

        // The selection engine is pipeline truth and updates NOW (a track
        // command must act on the item that is actually decoding), even though
        // the caller-facing collection event is held with the switch below.
        let has_audio = prepared
            .pending_collection
            .as_ref()
            .is_some_and(|collection| {
                collection
                    .iter()
                    .any(|s| s.stream_type().contains(gst::StreamType::AUDIO))
            });
        if let Some(collection) = prepared.pending_collection.as_ref() {
            let streams = collection
                .iter()
                .filter_map(|s| {
                    let sid = s.stream_id()?.to_string();
                    let typ = s.stream_type();
                    let kind = if typ.contains(gst::StreamType::VIDEO) {
                        StreamKind::Video
                    } else if typ.contains(gst::StreamType::AUDIO) {
                        StreamKind::Audio
                    } else if typ.contains(gst::StreamType::TEXT) {
                        StreamKind::Text
                    } else {
                        return None;
                    };
                    Some(selection::CollectionStream { sid, kind })
                })
                .collect();
            self.selection.lock().collection_changed(streams);
        }

        // The user-facing switch: hold it for the sink boundary if the item
        // has audio to anchor the release on, else emit it now (an audio-less
        // item never crosses the audio queue, so there is nothing to wait for).
        let held = HeldActivation {
            collection: prepared.pending_collection,
        };
        if has_audio {
            *self.held_activation.lock() = Some(held);
        } else {
            self.emit_held(held);
        }

        // Input surgery (removing the drained previous inputs) must not run
        // on this streaming thread.
        self.queue_job(Job::FinishActivation);
    }

    /// Emit a gapless activation's user-facing events in the canonical
    /// fresh-load order: [`PlaybinEvent::PreparedActivated`] (which the caller
    /// uses to adopt the new generation, letting the collection past its
    /// supersession guard) followed by the new item's collection.
    pub(crate) fn emit_held(&self, held: HeldActivation) {
        self.emit(PlaybinEvent::PreparedActivated);
        if let Some(collection) = held.collection {
            self.emit(PlaybinEvent::StreamCollection(collection));
        }
    }

    /// Release a held gapless activation, if one is waiting. Called from the
    /// `fpb-aqueue` src STREAM_START probe when the item's audio reaches the
    /// sink, and as a flush before a superseding activation. A no-op when
    /// nothing is held (the common case, and every non-boundary STREAM_START).
    pub(crate) fn release_held_activation(&self) {
        let held = self.held_activation.lock().take();
        if let Some(held) = held {
            self.emit_held(held);
        }
    }

    /// Register a prepared (gapless) next input: added to the running
    /// pipeline and activated, but its source pads are NOT linked into
    /// decodebin3. Each pad gets a block probe that lets serialized events
    /// through (so parsing completes and sticky stream-start/caps/segment
    /// accumulate on the unlinked pads) and holds DATA back; the first
    /// blocked buffer parks its streaming thread on the swap gate until the
    /// current item drains, then performs the relink
    /// ([`Self::perform_gapless_swap`]). The uridecodebin3 recipe.
    pub(crate) fn add_prepared_input(
        inner: &Arc<Inner>,
        element: gst::Element,
        generation: u64,
    ) -> Result<()> {
        // State-locked: the bin's state machinery skips this child, so a
        // broken prepared input (bad URL failing its state change) cannot
        // poison a concurrent pipeline transition (it wedged the current
        // item's PAUSED->PLAYING commit). The crate drives its state
        // explicitly: synced here, unlocked at the swap when it becomes
        // the live input, NULLed at removal.
        element.set_locked_state(true);
        inner
            .pipeline
            .add(&element)
            .context("adding the prepared input element")?;
        inner.routing.lock().inputs.push(Input {
            element: element.clone(),
            generation,
            external: None,
            db3_sink_pads: Vec::new(),
            taps: Vec::new(),
            pad_added_sig: None,
            block_probes: Vec::new(),
        });

        let pad_added_sig = element.connect_pad_added({
            let inner = Arc::downgrade(inner);
            move |element, pad| {
                let Some(inner) = inner.upgrade() else { return };
                Inner::block_prepared_pad(&inner, element, pad, generation);
            }
        });
        {
            let mut routing = inner.routing.lock();
            if let Some(input) = routing.inputs.iter_mut().find(|i| i.element == element) {
                input.pad_added_sig = Some(pad_added_sig);
            }
        }
        for pad in element.src_pads() {
            Inner::block_prepared_pad(inner, &element, &pad, generation);
        }

        if let Err(err) = element.sync_state_with_parent() {
            let mut routing = inner.routing.lock();
            if let Some(idx) = routing.inputs.iter().position(|i| i.element == element) {
                let input = routing.inputs.remove(idx);
                drop(routing);
                Inner::remove_input(inner, input);
            }
            return Err(err).context("syncing the prepared input element state");
        }
        Ok(())
    }

    /// Install the gapless block probe on one prepared-input source pad.
    fn block_prepared_pad(
        inner: &Arc<Inner>,
        element: &gst::Element,
        pad: &gst::Pad,
        generation: u64,
    ) {
        let probe = pad.add_probe(
            gst::PadProbeType::BLOCK
                | gst::PadProbeType::BUFFER
                | gst::PadProbeType::BUFFER_LIST
                | gst::PadProbeType::EVENT_DOWNSTREAM,
            {
                let weak = Arc::downgrade(inner);
                move |pad, info| Inner::prepared_block_probe(&weak, pad, info, generation)
            },
        );
        let Some(probe) = probe else { return };
        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.block_probes.push((pad.clone(), probe));
        } else {
            drop(routing);
            pad.remove_probe(probe);
        }
    }

    /// The prepared input's data gate (see [`Self::add_prepared_input`]).
    /// Runs on the prepared input's streaming threads.
    fn prepared_block_probe(
        weak: &Weak<Inner>,
        pad: &gst::Pad,
        info: &mut gst::PadProbeInfo,
        generation: u64,
    ) -> gst::PadProbeReturn {
        // Serialized events pass (parsing progresses, sticky events
        // accumulate on the unlinked pad). GAP is data-like and blocks
        // with the buffers, exactly like uridecodebin3's block probe.
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && event.type_() != gst::EventType::Gap
        {
            return gst::PadProbeReturn::Pass;
        }
        let Some(inner) = weak.upgrade() else {
            return gst::PadProbeReturn::Remove;
        };

        let mut state = inner.swap_gate.state.lock();
        loop {
            if state.swapped || inner.current_generation() == generation {
                // The swap already ran (this pad's link is live): flow.
                // The generation check covers a straggler probe firing
                // AFTER the next item's prepare re-armed the gate (a
                // sparse pad's first datum can come long past this input's
                // own activation, and the fresh gate no longer names this
                // generation): its input is the LIVE one and flushing here
                // would kill the playing stream.
                return gst::PadProbeReturn::Remove;
            }
            if state.pending != Some(generation) {
                // Cancelled or superseded: flush this input's streaming
                // thread out so teardown can join it.
                debug!(pad = %pad.name(), "prepared pad unblocked by a cancelled swap");
                drop(state);
                let _ = info.data.take();
                info.flow_res = Err(gst::FlowError::Flushing);
                return gst::PadProbeReturn::Handled;
            }
            if state.drained {
                break;
            }
            inner.swap_gate.cond.wait(&mut state);
        }

        // The current input is fully drained and this thread got here
        // first: relink the core to the prepared input for ALL its pads.
        match Inner::perform_gapless_swap(&inner, generation) {
            Ok(()) => {
                info!(generation, "gapless swap performed at drain");
                state.swapped = true;
                inner.swap_gate.cond.notify_all();
                gst::PadProbeReturn::Remove
            }
            Err(err) => {
                error!(?err, generation, "gapless swap failed");
                drop(state);
                // The cancel job cleans the input up and, since the item's
                // EOS was already consumed by the drain, synthesizes the
                // end-of-stream the caller now needs to advance normally.
                inner.emit(PlaybinEvent::PreparedFailed { generation });
                inner.queue_job(Job::CancelPrepared { notify: false });
                let _ = info.data.take();
                info.flow_res = Err(gst::FlowError::Flushing);
                gst::PadProbeReturn::Handled
            }
        }
    }

    /// Relink the live core from the drained current input to the prepared
    /// one: matching streams REUSE the same decodebin3 sink pad (unlink old
    /// pad, link new pad; the slot and its output pad survive, downstream
    /// sees only stream-start/caps/segment), unmatched old pads are
    /// released, unmatched new streams get fresh request pads. Runs on a
    /// prepared-input streaming thread, which is upstream's own recipe;
    /// element removal stays on the worker ([`Job::FinishActivation`]).
    fn perform_gapless_swap(inner: &Arc<Inner>, generation: u64) -> Result<()> {
        let db3 = inner
            .core
            .lock()
            .as_ref()
            .map(|c| c.db3.clone())
            .ok_or_else(|| anyhow!("no dynamic core"))?;
        let current = inner.current_generation();

        let (prepared_element, old_pairs, routed_ids, routed_kinds) = {
            let routing = inner.routing.lock();
            let prepared_element = routing
                .inputs
                .iter()
                .find(|i| i.generation == generation)
                .map(|i| i.element.clone())
                .ok_or_else(|| anyhow!("prepared input no longer registered"))?;
            // (src pad, decodebin3 sink pad) of the drained main input.
            let old_pairs: Vec<(gst::Pad, gst::Pad)> = routing
                .inputs
                .iter()
                .filter(|i| i.generation == current && i.external.is_none())
                .flat_map(|i| {
                    i.db3_sink_pads
                        .iter()
                        .filter_map(|sink| sink.peer().map(|src| (src, sink.clone())))
                })
                .collect();
            // The streams actually flowing OUT of decodebin3 (only selected
            // streams have output slots), by id and by kind.
            let routed_ids: Vec<String> = routing
                .routed
                .iter()
                .filter_map(|r| r.db3_src_pad.stream_id().map(|sid| sid.to_string()))
                .collect();
            let routed_kinds: Vec<StreamKind> = routing.routed.iter().map(|r| r.kind).collect();
            (prepared_element, old_pairs, routed_ids, routed_kinds)
        };

        // A live A/V output slot whose kind has NO successor stream cannot
        // switch gaplessly. Its sink outlives the stream: the hold drops
        // the old item's EOS, nothing removes an audio sink mid-item, and
        // the bin can then never aggregate the NEW item's end-of-stream
        // (a silent autoplay wedge); a dying video slot can conversely EOS
        // every remaining sink between items. Fail the swap BEFORE touching
        // any link: the caller reports PreparedFailed and the ordinary
        // end-of-stream advance owns the transition.
        let new_pads = prepared_element.src_pads();
        for (kind, want) in [
            (StreamKind::Video, gst::StreamType::VIDEO),
            (StreamKind::Audio, gst::StreamType::AUDIO),
        ] {
            if !routed_kinds.contains(&kind) {
                continue;
            }
            let covered = new_pads
                .iter()
                .any(|pad| pad.stream().is_some_and(|s| s.stream_type().contains(want)));
            if !covered {
                return Err(anyhow!(
                    "the prepared item has no {kind:?} stream to continue the live one"
                ));
            }
        }

        // Match new pads to old decodebin3 sink pads by stream type, the
        // uridecodebin3 criterion: a reused sink pad keeps the decodebin3
        // slot (and its output pad) alive across the switch. Prefer the old
        // pad whose stream is ROUTED (selected): a multi-track item also
        // has unselected sibling pads of the same type, and handing the
        // successor to one of those would let the playing slot die instead
        // (the exact wedge the coverage check above exists for). Plan
        // first, mutate after: a failed plan must leave the links alone.
        let mut taken = vec![false; old_pairs.len()];
        let mut links: Vec<(gst::Pad, gst::Pad)> = Vec::new();
        let mut fresh: Vec<gst::Pad> = Vec::new();
        for new_pad in &new_pads {
            let want = new_pad.stream().map(|s| s.stream_type());
            let type_matches = |idx: usize, taken: &[bool]| {
                !taken[idx]
                    && match want {
                        None => true,
                        Some(want) => {
                            old_pairs[idx].0.stream().map(|s| s.stream_type()) == Some(want)
                        }
                    }
            };
            let selected = |idx: usize| {
                old_pairs[idx]
                    .0
                    .stream_id()
                    .is_some_and(|sid| routed_ids.iter().any(|r| *r == sid))
            };
            let matched = (0..old_pairs.len())
                .find(|&idx| type_matches(idx, &taken) && selected(idx))
                .or_else(|| (0..old_pairs.len()).find(|&idx| type_matches(idx, &taken)));
            match matched {
                Some(idx) => {
                    taken[idx] = true;
                    debug!(
                        new = %new_pad.name(),
                        sink = %old_pairs[idx].1.name(),
                        "gapless: reusing the decodebin3 sink pad"
                    );
                    links.push((new_pad.clone(), old_pairs[idx].1.clone()));
                }
                None => fresh.push(new_pad.clone()),
            }
        }
        for (idx, (old_src, db3_sink)) in old_pairs.iter().enumerate() {
            let _ = old_src.unlink(db3_sink);
            if !taken[idx] {
                // An old stream with no successor: its slot ends here.
                debug!(sink = %db3_sink.name(), "gapless: releasing an unmatched old sink pad");
                db3.release_request_pad(db3_sink);
            }
        }
        for (new_pad, db3_sink) in &links {
            new_pad
                .link(db3_sink)
                .with_context(|| format!("relinking {} into decodebin3", new_pad.name()))?;
        }
        for new_pad in fresh {
            let db3_sink = {
                // Serialized: see `Inner::db3_pad_request`.
                let _serial = inner.db3_pad_request.lock();
                db3.request_pad_simple("sink_%u")
            }
            .ok_or_else(|| anyhow!("decodebin3 gave no request sink pad"))?;
            new_pad
                .link(&db3_sink)
                .with_context(|| format!("linking {} into decodebin3", new_pad.name()))?;
            debug!(new = %new_pad.name(), sink = %db3_sink.name(), "gapless: fresh sink pad");
            links.push((new_pad.clone(), db3_sink));
        }

        // The prepared input is the live input from here on: it follows
        // pipeline transitions again (the lock existed so a broken prepare
        // could not poison them, see `add_prepared_input`).
        prepared_element.set_locked_state(false);

        // Bookkeeping: the old input no longer owns any decodebin3 pads
        // (reused ones now belong to the new input, the rest were
        // released), so its later removal cannot touch them. The block
        // probe list clears: every remaining probe self-removes on its
        // next datum (`swapped` is set by our caller).
        {
            let mut routing = inner.routing.lock();
            for input in routing
                .inputs
                .iter_mut()
                .filter(|i| i.generation == current && i.external.is_none())
            {
                input.db3_sink_pads.clear();
            }
            if let Some(input) = routing
                .inputs
                .iter_mut()
                .find(|i| i.generation == generation)
            {
                input.block_probes.clear();
            }
        }
        for (new_pad, db3_sink) in links {
            Inner::record_linked_input_pad(inner, &prepared_element, &new_pad, db3_sink);
        }
        Ok(())
    }
}

impl FcastPlaybin {
    /// Drop a still-pending prepared next input: take it out of the
    /// prepared slot and remove its element from the pipeline. A no-op
    /// when nothing is prepared (or it already activated, which empties the
    /// slot). Worker-thread only (pipeline surgery).
    ///
    /// Returns [`CancelOutcome::Declined`] when a performed swap makes
    /// cancellation impossible: activation is imminent and must be left to
    /// finish (a caller-side load supersedes it normally anyway). Callers
    /// arming a NEW prepare must refuse on it rather than clobber the
    /// in-flight activation.
    pub(crate) fn cancel_prepared(&self) -> CancelOutcome {
        // Atomic against the block probe's surgery: past the swap the
        // relink is live and activation is imminent, cancelling would rip
        // the now-active input out mid-stream. `pending` distinguishes the
        // in-flight window from a long-completed activation (which leaves
        // `swapped` set but clears `pending`).
        let dropped_eos = {
            let mut state = self.inner.swap_gate.state.lock();
            if let Some(generation) = state.activation_pending() {
                debug!("swap already performed, leaving the activation to finish");
                return CancelOutcome::Declined { generation };
            }
            let aborted = std::mem::take(&mut *state);
            self.inner.swap_gate.cond.notify_all();
            aborted.pending.is_some() && aborted.dropped_eos
        };
        let mut cancelled = None;
        if let Some(prepared) = self.inner.prepared.lock().take() {
            debug!(
                generation = prepared.generation,
                "dropping the prepared next input"
            );
            cancelled = Some(prepared.generation);
            let input = {
                let mut routing = self.inner.routing.lock();
                routing
                    .inputs
                    .iter()
                    .position(|i| i.element == prepared.element)
                    .map(|idx| routing.inputs.remove(idx))
            };
            if let Some(input) = input {
                Inner::remove_input(&self.inner, input);
            }
            // A prepared input dying mid-transition can abort the
            // pipeline's in-flight commit (see Job::PrepareNext's failure
            // arm): re-assert it. A no-op when nothing was disturbed.
            self.recommit_pipeline_state();
        }
        // The hold DROPPED an output EOS while this swap was pending: that
        // end is gone for good and nothing else will surface it (a partial
        // drop even blocks the bin's EOS aggregation forever). Synthesize
        // it now; a duplicate (an EOS on a pad the hold had not covered
        // yet) is dropped by the caller's generation guard after it
        // advances. An EOS that only reached the INPUT side keeps flowing
        // normally once the hold disarms, so no synthesis there: ending
        // the item early would cut its buffered tail (and turn a seek-back
        // near the end into a skip to the next item).
        if dropped_eos {
            debug!("prepare cancelled after its end was consumed: synthesizing end-of-stream");
            self.inner.emit(PlaybinEvent::EndOfStream);
        }
        CancelOutcome::Cancelled {
            generation: cancelled,
        }
    }
}
