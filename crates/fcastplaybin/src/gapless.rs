//! The gapless path: preparing the next item behind the playing one and
//! swapping it in at the boundary.

use std::sync::{Arc, Weak, atomic::Ordering};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use parking_lot::{Condvar, Mutex};
use tracing::{debug, error, info};

use crate::{
    FcastPlaybin, Inner,
    api::{AfterCancel, PlaybinEvent},
    decisions::{EosGate, gapless_eos_decision},
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

    /// Whether the relink has run for this prepared input. Its source pads
    /// are UNLINKED by construction until [`Inner::perform_gapless_swap`]
    /// links them into decodebin3 (that is the hold itself, see
    /// [`Inner::add_prepared_input`]), so one linked pad means the swap has
    /// started.
    ///
    /// Deliberately lock-free. The one caller runs inside the bus SYNC
    /// handler, which can be re-entered on the very thread performing the
    /// swap while it holds the swap gate, so asking the gate instead would
    /// be a self-deadlock.
    fn relinked(&self) -> bool {
        self.element.src_pads().iter().any(|pad| pad.is_linked())
    }
}

/// Whether `selected_ids` names something and names only ids from `known`.
/// The containment test both halves of [`activation_decision`] use.
fn selection_covered_by(selected_ids: &[String], known: &[String]) -> bool {
    !selected_ids.is_empty()
        && selected_ids
            .iter()
            .all(|sel| known.iter().any(|id| id == sel))
}

/// The decision core of [`Inner::try_activate_prepared`], with the pipeline
/// reads supplied so the identical-sid shape can be pinned without one.
///
/// Containment in the prepared ids alone is NOT the boundary. Two queue
/// items from the same URI carry identical stream ids (the degradation the
/// arm-time sticky check in [`Inner::activate_prepared_now`] documents), so
/// a `STREAMS_SELECTED` decodebin3 posts about the CURRENT item, e.g. for a
/// mid-item track change, satisfies it too. Adopting the next generation
/// there unblocks the prepared pads into unlinked decodebin3 pads and has
/// [`crate::jobs::Job::FinishActivation`] remove the still-playing input.
///
/// `relinked` is what tells them apart: before the relink decodebin3 has
/// none of the prepared item's streams to select, so an ambiguous report can
/// only be about the item still playing. A selection naming ids the current
/// item does NOT carry is unambiguous and activates as before, which keeps
/// the `FCAST_NO_ADAPTIVE_PREPARE_HOLD` reading intact.
fn activation_decision(
    selected_ids: &[String],
    prepared_ids: &[String],
    routed_ids: &[String],
    relinked: bool,
) -> bool {
    if !selection_covered_by(selected_ids, prepared_ids) {
        return false;
    }
    !selection_covered_by(selected_ids, routed_ids) || relinked
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

/// The matching rule of [`Inner::collection_is_prepared`], with the prepared
/// ids and the collection's sids supplied so it can be pinned without a
/// pipeline: ANY stream matched AND NONE foreign, and no prepared ids at all
/// (pads not produced yet) answers false.
fn collection_matches_prepared(
    ids: &[String],
    collection_sids: impl IntoIterator<Item = Option<gst::glib::GString>>,
) -> bool {
    if ids.is_empty() {
        return false;
    }
    let mut any = false;
    for sid in collection_sids {
        let Some(sid) = sid else {
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

/// Whether a cancel owes the caller a synthesized [`PlaybinEvent::EndOfStream`]
/// (see the rationale at the bottom of [`FcastPlaybin::cancel_prepared`]).
/// Only a consumed end is owed at all, and only when nothing is about to
/// regenerate it.
fn cancel_synthesizes_eos(dropped_eos: bool, after: AfterCancel) -> bool {
    dropped_eos && matches!(after, AfterCancel::Nothing)
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
        collection_matches_prepared(
            &prepared.stream_ids(),
            collection.iter().map(|stream| stream.stream_id()),
        )
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
    pub(crate) fn note_output_stream_start(&self, group: Option<gst::GroupId>, pad_name: &str) {
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
            // The pad names the thread the activation runs on, which decides
            // what the arm below can race (see the aqueue sticky check).
            info!(
                generation = prepared.generation,
                pad = pad_name,
                "gapless activation: the prepared item's group reached the output"
            );
            self.activate_prepared_now(prepared, retired);
        }
    }

    /// The post-streamsynchronizer gate's half of the EOS hold: the same
    /// decision ([`gapless_eos_decision`]) with no sibling-pass arm and
    /// nothing to commit, because ssync already completed the group.
    /// Returns (drop, pending, behind).
    pub(crate) fn gapless_eos_check_and_mark(
        &self,
        pad_group: Option<gst::GroupId>,
    ) -> (bool, bool, bool) {
        let active_group = *self.active_group.lock();
        let retired_group = *self.retired_group.lock();
        // One lock hold for the check AND the drop record: a cancel between
        // them would zero the state and the mark would pollute the next
        // prepare's gate.
        let mut state = self.swap_gate.state.lock();
        let gate = gapless_eos_decision(
            pad_group,
            active_group,
            retired_group,
            None,
            state.pending.is_some(),
            false,
        );
        match gate {
            EosGate::Drop { pending, behind } => {
                if pending {
                    // The item's end is consumed for good; a cancelled swap
                    // must synthesize it (see `SwapState::dropped_eos`).
                    state.dropped_eos = true;
                }
                (true, pending, behind)
            }
            // Neither reason held, so both are false by construction.
            _ => (false, false, false),
        }
    }

    /// The routed-pad EOS gate: decide and commit under ONE critical
    /// section, with the passing mirror's lock taken first and held across
    /// the verdict and the commit.
    ///
    /// The atomicity is the point. With the sibling-pass read, the
    /// pending/behind verdict and the mirror commit as three separate lock
    /// holds, a pre-arm (`Job::PrepareNext`) landing between one sibling's
    /// PASS verdict and its commit makes the next sibling read an empty
    /// mirror and drop on the fresh `pending`. That subset-drop parks the
    /// first stream's multiqueue task inside ssync's group wait forever
    /// (CLEANUP invariant 12). Holding the mirror serializes the siblings:
    /// the second one cannot look until the first has published.
    ///
    /// Lock order: `passing_eos_group` outermost, then `active_group`,
    /// `retired_group`, `swap_gate.state`. Nothing takes the mirror while
    /// holding any of those, and every hold here is a plain field read.
    pub(crate) fn gapless_eos_gate(&self, pad_group: Option<gst::GroupId>, av: bool) -> EosGate {
        let mut passing = self.passing_eos_group.lock();
        let active_group = *self.active_group.lock();
        let retired_group = *self.retired_group.lock();
        let mut state = self.swap_gate.state.lock();
        let gate = gapless_eos_decision(
            pad_group,
            active_group,
            retired_group,
            *passing,
            state.pending.is_some(),
            av,
        );
        match gate {
            EosGate::Drop { pending: true, .. } => state.dropped_eos = true,
            // This EOS enters streamsynchronizer, so the whole group is
            // committed to passing before any sibling can be judged.
            EosGate::Pass {
                commit: Some(group),
            } => *passing = Some(group),
            _ => {}
        }
        gate
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
            let prepared_ids = prepared.stream_ids();
            // Cheap prefilter of the same predicate, so the routing read
            // below stays off this hot path (every STREAMS_SELECTED lands
            // here).
            if !selection_covered_by(selected_ids, &prepared_ids) {
                return;
            }
            // The current item's routed output streams, i.e. what a report
            // about the item still playing would name. Lock order is
            // `prepared` then `routing`, the one `cancel_prepared` takes.
            let routed_ids: Vec<String> = self
                .routing
                .lock()
                .routed
                .iter()
                .filter_map(|r| r.db3_src_pad.stream_id().map(|sid| sid.to_string()))
                .collect();
            if !activation_decision(
                selected_ids,
                &prepared_ids,
                &routed_ids,
                prepared.relinked(),
            ) {
                debug!(
                    generation = prepared.generation,
                    "a selection naming the current item's ids too is not the boundary \
                     until the relink"
                );
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
        // TEST FAULT INJECTION (see [`Inner::stage_activation_delay_ms`]).
        // Before any effect of the activation, so the whole function is late
        // the way a busy bus thread makes it late.
        let staged_delay = self.stage_activation_delay_ms.load(Ordering::SeqCst);
        if staged_delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(staged_delay));
        }

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
        let audio_sids: Vec<String> = prepared
            .pending_collection
            .as_ref()
            .map(|collection| {
                collection
                    .iter()
                    .filter(|s| s.stream_type().contains(gst::StreamType::AUDIO))
                    .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let held = HeldActivation {
            collection: prepared.pending_collection,
        };
        if has_audio {
            // The release edge may ALREADY have passed. This function can run
            // on a bus posting thread (the selection trigger) with no ordering
            // against the audio data plane, and on a reused slot the new
            // item's STREAM_START can cross a near-empty fpb-aqueue before the
            // arm below. Exactly one STREAM_START crosses per item, so arming
            // after the edge would hold forever, which was the rare
            // queue_autoplay boundary wedge (tracks never advertised).
            //
            // The aqueue src sticky answers "did it cross" race-free. gstpad
            // stores a serialized sticky BEFORE the probes run
            // (gst_pad_push_event, store_sticky_event ahead of check_sticky),
            // so under this lock either the sticky still names the old item
            // and the release probe has not fired (arming is safe, the probe
            // must wait for this lock), or it names the new item and the edge
            // is spent (emit now). Two queue items with identical stream ids
            // read as already-crossed and emit a queue-depth early, the
            // pre-hold seam behavior, degraded but never wedged.
            let mut slot = self.held_activation.lock();
            let crossed = self
                .audio_entry
                .static_pad("src")
                .and_then(|src| src.sticky_event::<gst::event::StreamStart>(0))
                .is_some_and(|event| {
                    let sid = event.stream_id();
                    audio_sids.iter().any(|known| known.as_str() == sid)
                });
            if crossed {
                drop(slot);
                // Counted PER INSTANCE so a test can pin this branch without
                // scanning a process-global log buffer every other test in
                // the binary writes into (see
                // `FcastPlaybin::arm_time_activation_releases`).
                self.arm_time_releases.fetch_add(1, Ordering::SeqCst);
                info!(
                    generation = prepared.generation,
                    "gapless activation released at arm, the audio boundary had already crossed"
                );
                self.emit_held(held);
            } else {
                *slot = Some(held);
            }
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
                inner.queue_job(Job::CancelPrepared {
                    notify: false,
                    after: AfterCancel::Nothing,
                });
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
        //
        // The NEW side needs the mirror preference. src_pads() order comes
        // from urisourcebin's parallel parse chains and is racy, and the
        // first same-type pad claims the reused slot. When a multi-track
        // item's UNSELECTED sibling won that race, the stream decodebin3
        // will actually select landed on a fresh slot whose output decision
        // ran before the new collection was current, was refused, and was
        // never revisited (the R1 boundary wedge, half of it). decodebin3's
        // default selection takes the first stream of each type in
        // collection order, collection order is container track order, and
        // the parsed stream ids embed the track number, so sorting the pads
        // by stream id hands the reused slot to the stream that will play.
        // The collection itself cannot be the rank: the prepared input often
        // posts it only AFTER the swap. Pads without a sticky stream-start
        // sort last.
        let mut new_pads = new_pads;
        new_pads.sort_by_key(|pad| {
            let sid = pad.stream_id().map(|s| s.to_string());
            (sid.is_none(), sid)
        });
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
    /// TEST FAULT INJECTION: delay the next gapless activation by `delay`,
    /// staging the window between the boundary's data flow and the
    /// activation's arm of `held_activation` (see
    /// [`Inner::stage_activation_delay_ms`]). Per instance so it is safe
    /// under a test binary's thread pool. Not part of the public API.
    #[doc(hidden)]
    pub fn stage_activation_delay(&self, delay: std::time::Duration) {
        self.inner
            .stage_activation_delay_ms
            .store(delay.as_millis() as u64, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many gapless activations found the audio boundary already crossed
    /// at arm time and released the held events right there (see the sticky
    /// check in `Inner::activate_prepared_now`). PER INSTANCE, which is the
    /// point: the crate's tracing goes to one process-global subscriber, so a
    /// test binary running several pipelines at once cannot tell whose line
    /// it is reading. Not part of the public API.
    #[doc(hidden)]
    pub fn arm_time_activation_releases(&self) -> u64 {
        self.inner
            .arm_time_releases
            .load(std::sync::atomic::Ordering::SeqCst)
    }

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
    ///
    /// `after` says what the caller does to the pipeline next, which is the
    /// only thing that can decide the consumed-end synthesis at the bottom
    /// (see [`AfterCancel`]).
    pub(crate) fn cancel_prepared(&self, after: AfterCancel) -> CancelOutcome {
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
        //
        // A FLUSHING SEEK FOLLOWING THIS CANCEL has that same effect, and
        // it is the invariant-8 path: the app parks the seek precisely
        // BECAUSE a prepare is pending. The drop happens up to a video
        // queue depth (30 s) before the item is audibly over, so the seek
        // lands with the item still playing, the seek restarts the sources
        // and regenerates the real end, and a synthesis here would make the
        // caller advance its queue instead of replaying the seek. Only the
        // caller knows which cancel this is, hence `after`.
        if cancel_synthesizes_eos(dropped_eos, after) {
            debug!("prepare cancelled after its end was consumed: synthesizing end-of-stream");
            self.inner.emit(PlaybinEvent::EndOfStream);
        } else if dropped_eos {
            debug!("prepare cancelled for a flushing seek: leaving the end to the seek");
        }
        CancelOutcome::Cancelled {
            generation: cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedNext, SwapState, activation_decision, cancel_synthesizes_eos,
        collection_matches_prepared, gapless_eos_decision,
    };
    use crate::api::AfterCancel;
    use gst::prelude::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn sids(names: &[&str]) -> Vec<Option<gst::glib::GString>> {
        names.iter().map(|s| Some((*s).into())).collect()
    }
    /// Any-matched AND none-foreign: one foreign stream disqualifies the
    /// whole collection, and an empty side never matches.
    #[test]
    fn a_collection_is_prepared_only_when_purely_the_prepared_streams() {
        let prepared = ids(&["p-1", "p-2"]);
        // A subset of the prepared streams is enough.
        assert!(collection_matches_prepared(&prepared, sids(&["p-1"])));
        assert!(collection_matches_prepared(
            &prepared,
            sids(&["p-2", "p-1"])
        ));
        // One foreign stream makes it a combined or current-item collection.
        assert!(!collection_matches_prepared(
            &prepared,
            sids(&["p-1", "cur-1"])
        ));
        assert!(!collection_matches_prepared(&prepared, sids(&["cur-1"])));
        // An empty collection matches nothing.
        assert!(!collection_matches_prepared(&prepared, sids(&[])));
        // No prepared ids yet (pads not produced): never claim a match.
        assert!(!collection_matches_prepared(&ids(&[]), sids(&["p-1"])));
        // Streams without ids are skipped, not read as foreign, and alone
        // they match nothing.
        assert!(!collection_matches_prepared(&prepared, vec![None]));
        assert!(collection_matches_prepared(
            &prepared,
            vec![None, Some("p-1".into())]
        ));
    }

    /// `stream_ids` reads only pads that already carry a stream-start, so a
    /// half-parsed prepared input reports the ids it has, not phantoms.
    #[test]
    fn prepared_stream_ids_skip_pads_without_a_stream_start() {
        gst::init().unwrap();
        let bin = gst::Bin::new();
        let with_sid = gst::Pad::builder(gst::PadDirection::Src)
            .name("src_0")
            .build();
        with_sid.set_active(true).unwrap();
        with_sid
            .store_sticky_event(&gst::event::StreamStart::new("prep-sid"))
            .unwrap();
        let without_sid = gst::Pad::builder(gst::PadDirection::Src)
            .name("src_1")
            .build();
        bin.add_pad(&with_sid).unwrap();
        bin.add_pad(&without_sid).unwrap();
        let prepared = PreparedNext {
            element: bin.upcast(),
            generation: 1,
            pending_collection: None,
        };
        assert_eq!(prepared.stream_ids(), ids(&["prep-sid"]));
    }

    /// The selection-side activation trigger's whole truth table. The rows
    /// that matter are the IDENTICAL-SID ones: two queue items from the same
    /// URI share stream ids, so a `STREAMS_SELECTED` decodebin3 posts about
    /// the CURRENT item (a mid-item track change) names exactly the prepared
    /// ids. Activating there adopts the next generation while the old item
    /// still plays and `Job::FinishActivation` then removes the live input.
    #[test]
    fn the_selection_activation_truth_table() {
        // (name, selected, prepared, routed, relinked) -> activate
        let cases = [
            // The ordinary switch: the successor's ids are its own.
            (
                "distinct ids, relinked",
                &["b-v", "b-a"][..],
                &["b-v", "b-a"][..],
                &["a-v", "a-a"][..],
                true,
                true,
            ),
            // Still unambiguous before the relink: only the prepared item
            // carries these ids, so nothing else could be reporting them.
            // Keeps the FCAST_NO_ADAPTIVE_PREPARE_HOLD reading working.
            (
                "distinct ids, not relinked",
                &["b-v", "b-a"][..],
                &["b-v", "b-a"][..],
                &["a-v", "a-a"][..],
                false,
                true,
            ),
            // A foreign id means the report is not purely the prepared item.
            (
                "one foreign id",
                &["b-v", "a-a"][..],
                &["b-v", "b-a"][..],
                &["a-v", "a-a"][..],
                true,
                false,
            ),
            // An empty selection names nothing.
            (
                "empty selection",
                &[][..],
                &["b-v", "b-a"][..],
                &["a-v", "a-a"][..],
                true,
                false,
            ),
            // No prepared pads yet: nothing to match against.
            (
                "prepared has no pads",
                &["a-v"][..],
                &[][..],
                &["a-v"][..],
                false,
                false,
            ),
            // THE DEFECT ROW. Same-URI successor, the report is about the
            // playing item, the relink has not happened.
            (
                "identical ids, not relinked",
                &["v", "a"][..],
                &["v", "a"][..],
                &["v", "a"][..],
                false,
                false,
            ),
            // Same shape after the relink: decodebin3 now has the prepared
            // item's streams, so this IS the switch.
            (
                "identical ids, relinked",
                &["v", "a"][..],
                &["v", "a"][..],
                &["v", "a"][..],
                true,
                true,
            ),
            // A subset of the identical ids (a subtitle toggle leaves the
            // A/V pair selected) is ambiguous just the same.
            (
                "identical ids, subset, not relinked",
                &["a"][..],
                &["v", "a"][..],
                &["v", "a"][..],
                false,
                false,
            ),
            // A same-URI successor whose report names a stream the current
            // item does NOT have routed is unambiguous: only a switch can
            // put a new id on the wire.
            (
                "identical ids, new stream routed nowhere",
                &["v", "a", "t"][..],
                &["v", "a", "t"][..],
                &["v", "a"][..],
                false,
                true,
            ),
        ];
        for (name, selected, prepared, routed, relinked, expected) in cases {
            let got = activation_decision(&ids(selected), &ids(prepared), &ids(routed), relinked);
            assert_eq!(got, expected, "{name}");
        }
    }

    /// Only a CONSUMED end is owed, and only when nothing regenerates it.
    /// The flushing-seek row is the one that matters: the output hold drops
    /// the EOS up to a video queue depth before the item is audibly over, so
    /// a synthesis there turns the caller's seek-back into a queue skip.
    #[test]
    fn a_cancel_synthesizes_the_consumed_end_only_when_nothing_replays_it() {
        let cases = [
            ("nothing consumed", false, AfterCancel::Nothing, false),
            ("nothing consumed, seek", false, AfterCancel::FlushingSeek, false),
            ("consumed, plain cancel", true, AfterCancel::Nothing, true),
            ("consumed, seek follows", true, AfterCancel::FlushingSeek, false),
        ];
        for (name, dropped_eos, after, expected) in cases {
            assert_eq!(cancel_synthesizes_eos(dropped_eos, after), expected, "{name}");
        }
    }

    /// The swap gate's drain edge fires ONCE per pad and is discarded while
    /// no swap is armed, so `Job::PrepareNext` sampling the taps before it
    /// writes the armed state can lose the last one for good (the prepared
    /// input's threads then park on the condvar forever). The arm re-derives
    /// under the armed state instead, which is what this models.
    #[test]
    fn an_arm_recovers_a_drain_edge_that_fired_just_before_it() {
        // The tap's own EOS probe, which is `Inner::note_input_pad_eos`'s
        // gate half: it only lands while a swap is pending.
        let edge = |state: &mut SwapState| {
            if state.pending.is_some() && !state.drained {
                state.drained = true;
                return true;
            }
            false
        };

        // The lost edge: it fires between the worker's sample and its write.
        let mut state = SwapState::default();
        assert!(!edge(&mut state), "no swap armed yet, the edge is discarded");
        // The arm, with `drained` no longer sampled ahead of the lock.
        state = SwapState {
            pending: Some(7),
            ..Default::default()
        };
        assert!(!state.drained);
        // The re-derive the arm now performs, the taps having already
        // stored their EOS.
        assert!(edge(&mut state), "the armed state must take the edge");
        assert!(state.drained);
        // Idempotent: the real probe's later calls change nothing.
        assert!(!edge(&mut state));
        assert!(state.drained);
    }
}
