//! Track-selection dispatch: pumping the selection engine, sending
//! `SELECT_STREAMS`, and the deadlines that bound an unanswered dispatch.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use gst::prelude::*;
use tracing::{debug, warn};

use crate::{
    Counters, FcastPlaybin, Inner,
    api::{ExternalSubId, PlaybinEvent},
    decisions,
    hands::{Effect, Outcome},
    jobs::{Job, QueuedAt, SelectJob},
    routing::{RoutedSids, StreamKind},
    selection,
    selection::{SelectionGate, TrackSelection, TrackSlot, TrackTarget},
};

/// How long a SENT upstream selection may go unconfirmed before the crate
/// confirms it locally (see `Inner::arm_upstream_confirm_fallback`). Longer
/// than any demuxer edge takes, short enough that a user waiting for the track
/// list to update does not notice.
const UPSTREAM_CONFIRM_FALLBACK: Duration = Duration::from_millis(700);

/// How many [`UPSTREAM_CONFIRM_FALLBACK`] budgets the fallback spends waiting
/// for a `SELECT_STREAMS` that is still on the select lane before it gives the
/// case to the selection deadline. Two, so a slow send costs one extra budget
/// and a WEDGED one is escalated well inside [`SELECTION_DEADLINE`].
const UPSTREAM_CONFIRM_ATTEMPTS: u32 = 2;

/// How long a dispatched `SELECT_STREAMS` may wait for its `STREAMS_SELECTED`
/// before the worker probes what actually got routed.
///
/// Sits deliberately in the middle of the ladder: above legitimate
/// confirmation latency so a healthy switch never reaches it, at the adaptive
/// demuxer drain window so no upstream re-dispatch lands inside it, and well
/// below the starves seen in the field.
pub(crate) const SELECTION_DEADLINE: Duration = Duration::from_secs(10);

/// How many times a selection whose probe says it was NOT applied is
/// re-dispatched before the crate gives up and reports what is actually
/// playing. Worst case is ~30s of SETTLED PLAYING, which beats permanent.
const SELECTION_DEADLINE_RETRIES: u32 = 2;

/// How long a dispatched refresh seek may wait for the top-level `ASYNC_DONE`
/// that settles it. Equal to [`PREROLL_TIMEOUT`]: a settled pipeline's
/// aggregated ASYNC_DONE latency is preroll-shaped.
pub(crate) const REFRESH_DEADLINE: Duration = Duration::from_secs(10);

/// How long a selection deadline defers to a select still on its lane before
/// timing it out anyway (see [`FcastPlaybin::selection_deadline_fired`] step
/// 4).
///
/// Twice [`hands::EFFECT_WEDGE_WARN`]. A lane that has held an effect past
/// the wedge warning has already been reported as wedged, so the fall-through
/// never fires before the evidence is in the log. And the first deadline fire
/// happens one [`SELECTION_DEADLINE`] after queueing, so a budget of one
/// wedge warning would race that first fire and could skip the deferral.
pub(crate) const SELECT_DEFER_BUDGET: Duration = Duration::from_secs(20);

/// How long [`Inner::await_text_input_drain`] holds a text re-select waiting
/// for the demuxer to finish draining that stream's previous pad.
///
/// Sized an order of magnitude over the worst observed drain (a whole
/// unsegmented text track in flight at once), and well under the selection
/// deadline, so a wait that goes the distance still leaves the deadline
/// machinery its own window.
const TEXT_DRAIN_INTERLOCK_BUDGET: Duration = Duration::from_secs(5);

/// Text re-selects [`Inner::await_text_input_drain`] held back (see there).
///
/// A repair count. Every one is a subtitle track that would otherwise have
/// been selected into a demuxer that would swallow the request.
pub(crate) static TEXT_DRAIN_INTERLOCKS: AtomicU64 = AtomicU64::new(0);

/// Those of [`TEXT_DRAIN_INTERLOCKS`] that hit the budget and sent anyway.
/// Zero is the invariant. Nonzero means a drain longer than the budget
/// believes possible.
pub(crate) static TEXT_DRAIN_INTERLOCK_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

impl Inner {
    /// Send one queued `SELECT_STREAMS` (see [`SelectJob`]). The only place
    /// the event leaves this crate, on the select lane, where blocking is
    /// allowed.
    ///
    /// Reports what happened rather than acting on it. The refusal feedback
    /// and the superseded-core skip are decisions about the engine, and the
    /// engine belongs to the decider. Two crate-state touches stay here as
    /// documented exceptions. The `video_deselected` mirror must flip exactly
    /// at send time for pads decodebin3 exposes inside the send, and the
    /// video-chain park wants to run after the send on this thread.
    pub(crate) fn send_select_streams(inner: &Arc<Inner>, job: SelectJob) -> Outcome {
        // A selection built against a superseded core can never confirm.
        // Don't run decodebin3's inline switch machinery on a dying
        // instance for nothing.
        let stale = inner.core.lock().as_ref().map(|c| &c.db3) != Some(&job.db3);
        if stale {
            debug!("dropping a stream selection for a superseded core");
            return Outcome::SelectSkipped {
                seqnum: job.event.seqnum(),
                reason: "the core it was built against was superseded",
            };
        }

        // Record the selection's video intent BEFORE the send.
        // decodebin3 can expose pads inline inside send_event, and the
        // route decision reading this mirror must see the intent THIS
        // selection carries, not the previous one. Pure intent on
        // purpose, unlike the park decision after the send, which also
        // wants a linked chain. An empty `video_ids` means kinds are
        // unknowable and never counts as off, matching
        // `decisions::deselects_video`.
        {
            let video_ids = inner.selection.lock().video_ids();
            let video_off =
                !video_ids.is_empty() && !video_ids.iter().any(|vid| job.stream_ids.contains(vid));
            inner.video_deselected.store(video_off, Ordering::SeqCst);
        }

        // The re-select drain interlock. See
        // [`Inner::await_text_input_drain`]; this is the one place in the
        // crate that can wait for it, and the wait is why it is here.
        if let Some(sid) = job.text_sid.as_deref() {
            Inner::await_text_input_drain(inner, sid, &job.stream_ids, &job.db3);
        }

        // send_event runs decodebin3's selection handling inline on THIS
        // thread. It may stall behind streaming threads, which is the
        // point of this thread (see `select_streams`).
        let seqnum = job.event.seqnum();
        // An upstream-split send is the one whose target is NOT the
        // decodebin3 the selection was built against. Taken before the send
        // so the report can be built from what left, not from what the
        // decider hoped would leave.
        let upstream_ids = (job.target != job.db3).then(|| job.stream_ids.clone());
        if !job.target.send_event(job.event) {
            warn!(target = %job.target.name(), "SELECT_STREAMS event refused");
            return Outcome::SelectRefused { seqnum };
        }
        debug!(?seqnum, ids = ?job.stream_ids, "sent SELECT_STREAMS");

        // A selection that drops video entirely must not leave the video
        // branch able to block on the pipeline clock (see
        // `park_video_chain_for_deselect`). Not on a video-to-video
        // switch: decodebin3 reuses the routed pad for those (no
        // pad-removed/added), so a parked chain would never re-join.
        // Hence the check against the collection's video ids, not just
        // the routed pad's. Running after `send_event` lets decodebin3's
        // armed slot deactivation complete rather than racing it.
        let deselects_video = {
            // Routing then selection, the crate's lock order (see
            // `Inner::routing`), so the linked chain and the collection come
            // from one snapshot the way they did off the routing mirror.
            let routing = inner.routing.lock();
            let video_linked = routing
                .routed
                .iter()
                .any(|r| r.kind == StreamKind::Video && r.downstream.is_some());
            let video_ids = inner.selection.lock().video_ids();
            decisions::deselects_video(video_linked, &video_ids, &job.stream_ids)
        };
        if deselects_video {
            inner.park_video_chain_for_deselect();
        }
        Outcome::SelectSent {
            seqnum,
            upstream_ids,
        }
    }

    /// Hold a text re-select until the demuxer has finished draining the pad
    /// it exposed for that stream last time. A re-select landing mid-drain is
    /// swallowed and the track never comes back.
    ///
    /// An adaptive demuxer answers a text (de)selection by exposing or
    /// draining a pad, and a drain is not instant. It pushes the track's
    /// backlog through decodebin3 and only then sends EOS. A re-select that
    /// lands before that EOS is swallowed. No pad is exposed, decodebin3
    /// keeps the stream in a slot it never builds an output for, the crate
    /// seats the dead pad from the previous incarnation, and the
    /// one-live-branch rule refuses everything else for the life of the item.
    ///
    /// Waiting, not re-sending. A second `SELECT_STREAMS` at an adaptive
    /// demuxer mid-drain trips the demuxer's own draining assertion. The
    /// event goes out once, after the state that would have swallowed it is
    /// over.
    ///
    /// Blocking is allowed here. The select lane exists because a send can
    /// stall behind streaming threads. The wait holds no crate lock and no
    /// gst object lock, and the drain it waits for touches nothing this crate
    /// holds, so it cannot be waiting on us in turn. The lane is FIFO, so a
    /// newer selection queued during the wait supersedes this one afterwards.
    ///
    /// What it waits for, read off the pads rather than crate memory:
    ///
    ///  * the send re-adds the stream (in this event, not in
    ///    [`Inner::last_upstream_ids`]);
    ///  * a decodebin3 sink pad of the main input still carries the stream id;
    ///  * that pad carries no sticky EOS.
    ///
    /// The last two together are "still draining", given the first. When the
    /// drain has already landed the pad carries EOS and this returns without
    /// waiting.
    ///
    /// Bounded by [`TEXT_DRAIN_INTERLOCK_BUDGET`] and loud when the bound is
    /// hit. An unbounded wait would trade a dead subtitle track for a dead
    /// select lane. Past the bound the event goes out anyway.
    ///
    /// Counted in [`TEXT_DRAIN_INTERLOCKS`] /
    /// [`TEXT_DRAIN_INTERLOCK_TIMEOUTS`].
    fn await_text_input_drain(
        inner: &Arc<Inner>,
        sid: &str,
        stream_ids: &[String],
        db3: &gst::Element,
    ) {
        // Only a send that actually names the stream, and only in the mode
        // where the demuxer is the one being asked.
        if !stream_ids.iter().any(|id| id == sid) || !inner.upstream_owns_selection() {
            return;
        }
        // Already selected upstream: this send is not a re-add, so there is no
        // drain of it in flight to race.
        if inner.last_upstream_ids.lock().iter().any(|id| id == sid) {
            return;
        }
        // The MAIN input's decodebin3 sink pads carrying this stream, with no
        // sticky EOS on them. Re-read every iteration: the pad set changes
        // under us (that is the whole point) and a cached list would wait on a
        // pad that has been released.
        let draining = || {
            inner.main_input_db3_sink_pads().into_iter().find(|pad| {
                pad.stream_id().as_deref() == Some(sid)
                    && pad.sticky_event::<gst::event::Eos>(0).is_none()
            })
        };
        let Some(pad) = draining() else {
            return;
        };
        let started = Instant::now();
        debug!(
            %sid,
            pad = %pad.name(),
            "a text re-select would land while the demuxer is still draining this stream's \
             previous pad; holding the send until the drain lands"
        );
        TEXT_DRAIN_INTERLOCKS.fetch_add(1, Ordering::Relaxed);
        while started.elapsed() < TEXT_DRAIN_INTERLOCK_BUDGET {
            // A core swap means this selection can never confirm anyway, and
            // the send below drops it; stop waiting for an item that is gone.
            if inner.core.lock().as_ref().map(|c| &c.db3) != Some(db3) {
                return;
            }
            if draining().is_none() {
                debug!(
                    %sid,
                    waited = ?started.elapsed(),
                    "the demuxer finished draining this stream; sending the re-select"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        TEXT_DRAIN_INTERLOCK_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        warn!(
            %sid,
            budget = ?TEXT_DRAIN_INTERLOCK_BUDGET,
            "the demuxer has not finished draining this text stream within the interlock \
             budget; sending the re-select anyway, which the demuxer may swallow"
        );
    }

    /// Whether the MAIN input's upstream answers the SELECTABLE query, i.e.
    /// an adaptive demuxer owns stream selection. Mirrors decodebin3, which
    /// flips into upstream-selection mode when ANY input answers TRUE (its
    /// own FIXME: "things might break if there's a mix", and an external
    /// subtitle IS the mix). Cached per load: the answer cannot change
    /// mid-item.
    ///
    /// Reads the tri-state below as "no upstream owner" when the query cannot
    /// be answered yet, which is what every caller here needs. Callers that
    /// must tell ignorance from a definite no call
    /// [`Inner::upstream_selection_mode`] instead. (The dispatch's eager text
    /// work was one, until every mode came to answer PARK.)
    pub(crate) fn upstream_owns_selection(&self) -> bool {
        self.upstream_selection_mode().unwrap_or(false)
    }

    /// [`Inner::upstream_owns_selection`] as a TRI-STATE: `None` while the
    /// main input has no decodebin3 sink pad linked, so there is nobody to ask
    /// and the answer is genuinely unknown rather than false. A definite
    /// answer is cached (the mode cannot change mid-item), ignorance is not.
    fn upstream_selection_mode(&self) -> Option<bool> {
        if let Some(known) = *self.upstream_selection.lock() {
            return Some(known);
        }
        let pads = self.main_input_db3_sink_pads();
        if pads.is_empty() {
            // Not linked yet: don't cache ignorance.
            return None;
        }
        // An UNHANDLED query is a definite no, not ignorance: SELECTABLE is
        // answered by adaptive demuxers and forwarded upstream by everything
        // else, so a plain source leaves it unhandled and `peer_query` returns
        // false. decodebin3 reads it exactly this way. Treating the refusal as
        // "not decidable yet" was measured WRONG: every non-adaptive replace
        // then took the PARK arm, which runs inline, and
        // `caller_bounded_switch.rs` caught the caller blocking behind the
        // text branch. Only "no pad to ask at all" is ignorance.
        let owns = pads.iter().any(|pad| {
            let mut query = gst::query::Selectable::new();
            pad.peer_query(&mut query) && query.selectable()
        });
        debug!(owns, "queried whether upstream owns stream selection");
        *self.upstream_selection.lock() = Some(owns);
        Some(owns)
    }

    /// Post the STREAMS_SELECTED that decodebin3 never posts in
    /// upstream-selection mode (`is_selection_done` returns early there), for
    /// a dispatch whose upstream-owned part did not change (an adaptive
    /// demuxer only confirms an activation edge). Everything downstream of
    /// the bus arm (engine settle, external hold release, replay) runs off
    /// the post, unchanged.
    /// A SENT upstream selection whose confirmation never arrives is confirmed
    /// locally after a bounded wait.
    ///
    /// The split sends only a CHANGED upstream part precisely because an
    /// adaptive demuxer confirms activation EDGES and a no-op send has none. A
    /// changed part is supposed to produce one, and the field showed two ways
    /// it does not:
    ///
    /// * The change can be real to the crate and a no-op to the demuxer. The
    ///   record it compares against (`last_upstream_ids`) is seeded from
    ///   observed reports (see the `StreamsSelected` arm) AND from what was
    ///   sent, so an item that never got an initial report compares against an
    ///   empty set and reads its first dispatch as changed whatever it names.
    /// * The edge can be real and unable to complete. At a settled PAUSED
    ///   nothing flows, and a track deactivation that needs its pad to go idle
    ///   waits for a push parked in a sink's preroll (the same mechanism as
    ///   `Inner::lift_deselected_video_sink`, one element further upstream
    ///   where this crate owns nothing to lift).
    ///
    /// Either way the caller is left waiting on a seqnum nothing will answer:
    /// the receiver never relays, never sends SetTrackIds, and the UI keeps
    /// showing the previous track while the new one is audibly/visibly active.
    /// So: after [`UPSTREAM_CONFIRM_FALLBACK`], if the engine still awaits this
    /// exact seqnum, post the confirmation the crate already knows how to
    /// build. Keyed on the seqnum, so a real confirmation that arrives
    /// first (or a selection that moved on) makes this a no-op.
    ///
    /// Sleeper thread, the pattern `Inner::arm_sub_watchdog` and
    /// `Inner::arm_replay_verification` already use, with the same in-flight
    /// consult the selection deadline makes: a demuxer that has not been
    /// ASKED yet is not a demuxer that failed to answer. If the
    /// `SELECT_STREAMS` is still sitting on the select lane when the budget
    /// runs out, the fallback re-arms rather than confirming - it would
    /// otherwise manufacture a confirmation for an event the crate has not
    /// sent, which is the exact divergence the phase-2 deadline residual was
    /// about, one order of magnitude earlier in time.
    ///
    /// Bounded at [`UPSTREAM_CONFIRM_ATTEMPTS`] budgets: past that the send
    /// is wedged rather than slow, and the selection deadline (ten times
    /// this budget, with its own in-flight consult and its own give-up) is
    /// the right instrument. The fallback then skips, loudly.
    fn arm_upstream_confirm_fallback(
        inner: &Arc<Inner>,
        target: selection::TrackSelection,
        seqnum: gst::Seqnum,
    ) {
        let weak = Arc::downgrade(inner);
        let spawned = std::thread::Builder::new()
            .name("fpb-confirm-fallback".into())
            .spawn(move || {
                for attempt in 0..UPSTREAM_CONFIRM_ATTEMPTS {
                    std::thread::sleep(UPSTREAM_CONFIRM_FALLBACK);
                    let Some(inner) = weak.upgrade() else { return };
                    if !inner.selection.lock().selection_in_flight(seqnum) {
                        return;
                    }
                    if let Some(age) = inner.hands.select_age(seqnum, Instant::now()) {
                        debug!(
                            ?seqnum,
                            ?age,
                            attempt,
                            "the upstream selection has not left the select lane yet; \
                             not confirming a send that has not happened"
                        );
                        continue;
                    }
                    warn!(
                        ?seqnum,
                        "the upstream selection was never confirmed; confirming it locally"
                    );
                    inner.post_synthetic_streams_selected(
                        &target,
                        seqnum,
                        "the upstream confirmation never arrived",
                    );
                    return;
                }
                let Some(inner) = weak.upgrade() else { return };
                if inner.selection.lock().selection_in_flight(seqnum) {
                    warn!(
                        ?seqnum,
                        "the upstream selection is still on the lane after every fallback \
                         budget; leaving it to the selection deadline"
                    );
                }
            });
        if let Err(err) = spawned {
            warn!(?err, "failed to arm the upstream confirmation fallback");
        }
    }

    /// Read what is really routed out of decodebin3 (see [`RoutedSids`]).
    ///
    /// Read-only and worker-only. Takes the `routing` lock ALONE and holds it
    /// across nothing but sticky reads on pads this crate owns, verbatim the
    /// read `Inner::verify_replay` already does for the text pad, which is the
    /// precedent for reading a pad's `STREAM_START` off the worker.
    fn probe_routed_selection(&self) -> RoutedSids {
        let routing = self.routing.lock();
        let mut probed = RoutedSids::default();
        for routed in &routing.routed {
            // Parked or not linked into a chain: routed, but not playing.
            if routed.downstream.is_none() {
                continue;
            }
            let Some(sid) = routed
                .db3_src_pad
                .sticky_event::<gst::event::StreamStart>(0)
                .map(|event| event.stream_id().to_string())
            else {
                continue;
            };
            match routed.kind {
                StreamKind::Video => probed.video.push(sid),
                StreamKind::Audio => probed.audio.push(sid),
                StreamKind::Text => probed.text.push(sid),
            }
        }
        probed
    }

    /// Build and post the `STREAMS_SELECTED` decodebin3 did not.
    ///
    /// FOUR callers now, and they no longer share a reason: the
    /// upstream-selection split's no-op confirm and its bounded fallback (a
    /// mode where decodebin3 posts nothing at all), the pump's
    /// `ConfirmApplied`, and the selection deadline's probe-backed rescue in
    /// db3-owned mode (a mode where decodebin3 normally DOES post, and the
    /// message was simply lost). Everything downstream of the bus arm - engine
    /// settle, external hold release, replay arming - runs off the post
    /// unchanged in all four cases, which is why one builder serves them all;
    /// only the log line has to say which one it is, hence `why`.
    fn post_synthetic_streams_selected(
        &self,
        target: &selection::TrackSelection,
        seqnum: gst::Seqnum,
        why: &'static str,
    ) {
        let mut collection = gst::StreamCollection::builder(None);
        let mut selected = Vec::new();
        for (sid, kind) in [
            (&target.video, gst::StreamType::VIDEO),
            (&target.audio, gst::StreamType::AUDIO),
            (&target.subtitle, gst::StreamType::TEXT),
        ] {
            if let Some(sid) = sid {
                let stream = gst::Stream::new(Some(sid), None, kind, gst::StreamFlags::empty());
                selected.push(stream.clone());
                collection = collection.stream(stream);
            }
        }
        debug!(?seqnum, why, "posting a local selection confirmation");
        let message = gst::message::StreamsSelected::builder(&collection.build())
            .streams(selected)
            .src(&self.pipeline)
            .seqnum(seqnum)
            .build();
        if self.pipeline.post_message(message).is_err() {
            warn!("failed to post the local selection confirmation");
        }
    }
}

impl FcastPlaybin {
    /// Shorten the selection-confirmation deadline. For tests only:
    /// production callers keep [`SELECTION_DEADLINE`]. Takes effect at the
    /// next dispatch, since a deadline is armed with its absolute due time.
    #[doc(hidden)]
    pub fn set_selection_deadline(&self, deadline: Duration) {
        self.inner.deadlines.lock().selection = deadline;
    }

    /// Shorten the refresh-seek deadline. For tests only: production callers
    /// keep [`REFRESH_DEADLINE`].
    #[doc(hidden)]
    pub fn set_refresh_deadline(&self, deadline: Duration) {
        self.inner.deadlines.lock().refresh = deadline;
    }

    /// Shorten the deferral budget ([`SELECT_DEFER_BUDGET`]) so a test can
    /// reach the fall-through without wedging a lane for 20 seconds. Same
    /// shape and purpose as [`Self::set_selection_deadline`].
    #[doc(hidden)]
    pub fn set_select_defer_budget(&self, budget: Duration) {
        self.inner.deadlines.lock().select_defer_budget = budget;
    }

    /// State a slot's desired track (latest wins): a stream id from the
    /// advertised collection, `None` for "slot off", or (subtitle only) an
    /// attached external input's stream once it materializes. The engine
    /// dispatches, confirms and re-asserts on its own. Call
    /// [`pump_selection`](Self::pump_selection) afterwards (and at every
    /// transport settle point) to let it act.
    pub fn request_track(&self, slot: TrackSlot, target: TrackTarget) {
        self.inner.selection.lock().request(slot, target);
    }

    /// A user-initiated flushing seek re-emits the current subtitle cue by
    /// itself, making a scheduled switch-refresh flush redundant.
    pub fn cancel_selection_refresh(&self) {
        self.inner.selection.lock().cancel_refresh();
    }

    /// Let the selection engine dispatch whatever the transport now allows:
    /// a composed `SELECT_STREAMS` (stamped with a fresh seqnum, confirmed
    /// by the matching `StreamsSelected` event) or the switch's re-emit
    /// flush. The caller owns the transport state machine and calls this at
    /// its settle points. The gate deliberately comes from there, not from
    /// a pipeline query: only the transport machine knows about queued
    /// seeks and the mid-cascade quiet instants that wedge a dispatched
    /// reconfigure (see the [`selection`] module docs).
    pub fn pump_selection(&self, gate: SelectionGate) {
        loop {
            enum Dispatch {
                Select(TrackSelection, gst::Seqnum),
                Refresh(gst::Seqnum),
                ConfirmApplied(TrackSelection, gst::Seqnum),
            }

            let (externals_attached, externals) = {
                let routing = self.inner.routing.lock();
                let externals: Vec<(ExternalSubId, Vec<String>)> = routing
                    .inputs
                    .iter()
                    .filter_map(|i| i.external.as_ref().map(|e| (e.id, i.stream_ids())))
                    .collect();
                (!externals.is_empty(), externals)
            };
            // Read BEFORE the engine lock: the query takes the routing lock,
            // and the order is routing then selection (see `Inner::routing`).
            // NOT during a gapless activation. `applied` there already names
            // the INCOMING item's streams, and a synthetic report naming them
            // reaches `Inner::try_activate_prepared`, which IS the activation
            // trigger: measured, confirming through that window took
            // `gapless_switch_into_a_text_bearing_item` from its documented
            // ~1-in-3 flake to 3 failures in 4 ("playback did not advance ... a
            // parked streaming thread"). An activation posts a real collection
            // and selection of its own, which answers anything waiting behind
            // it, so nothing is owed here.
            let activating = self
                .inner
                .swap_gate
                .state
                .lock()
                .activation_pending()
                .is_some();
            let upstream_owns = self.inner.upstream_owns_selection() && !activating;
            // How long the dispatch below may wait for its confirmation, read
            // BEFORE the engine lock so the lock order here stays flat. One
            // lock round for both: they live together (see `Deadlines`). The
            // engine wants an ABSOLUTE due time (it never reads a clock, see
            // `SelectionEngine::arm_selection_deadline`), so only the length
            // comes from here.
            let deadlines = *self.inner.deadlines.lock();
            let dispatch = {
                let ctx = selection::PumpCtx {
                    gate,
                    externals_attached,
                    // Moved, not cloned: the dispatch's own view of the
                    // externals is read on the decider, at send time (see
                    // `Self::dispatch_selection`).
                    externals,
                    upstream_owns,
                    now: Instant::now(),
                };
                let mut engine = self.inner.selection.lock();
                match engine.pump(&ctx) {
                    None => break,
                    Some(selection::Command::SelectStreams(target)) => {
                        let seqnum = gst::Seqnum::next();
                        engine.selection_dispatched(seqnum, target.clone());
                        engine.arm_selection_deadline(
                            seqnum,
                            Instant::now() + deadlines.selection,
                            SELECTION_DEADLINE_RETRIES,
                        );
                        Dispatch::Select(target, seqnum)
                    }
                    Some(selection::Command::RefreshSeek) => {
                        let seqnum = gst::Seqnum::next();
                        engine.refresh_dispatched(seqnum);
                        engine.arm_refresh_deadline(seqnum, Instant::now() + deadlines.refresh);
                        Dispatch::Refresh(seqnum)
                    }
                    // Recorded as a dispatch so the confirmation below settles
                    // it through the ONE seqnum-keyed path every other
                    // confirmation uses: nothing downstream can tell this from
                    // a no-op dispatch's local confirm, which is the point.
                    Some(selection::Command::ConfirmApplied(target)) => {
                        let seqnum = gst::Seqnum::next();
                        engine.selection_dispatched(seqnum, target.clone());
                        // Armed like any other dispatch even though the
                        // confirmation is posted microseconds from here: the
                        // post itself can FAIL (see
                        // `post_synthetic_streams_selected`), and a failed
                        // post used to strand the wait for good. The deadline
                        // simply posts it again, which is a rescue this path
                        // never had.
                        engine.arm_selection_deadline(
                            seqnum,
                            Instant::now() + deadlines.selection,
                            SELECTION_DEADLINE_RETRIES,
                        );
                        Dispatch::ConfirmApplied(target, seqnum)
                    }
                }
            };

            // Execute outside the engine lock: `select_streams` touches the
            // core, and the recorders (translate-time) take the engine lock
            // on streaming threads.
            match dispatch {
                // Handed to the decider, which is the thread that owns the
                // text branch this dispatch has to operate on (see
                // [`Job::DispatchSelection`] and `Self::dispatch_selection`).
                // What stays HERE is only what the engine recorded above: the
                // dispatch and its deadline are written under the engine lock
                // at the instant the decision is taken, because a
                // confirmation racing the queued job must never find a wait
                // that has not been recorded yet.
                Dispatch::Select(target, seqnum) => {
                    // The one decision input that is READ HERE and carried:
                    // whether this selection moves the subtitle slot off a
                    // track it is currently confirmed on. It is a statement
                    // about the selection being REPLACED, so its answer has
                    // to date from the decision, and a fresh read on the
                    // decider is not merely later - it can be WRONG.
                    //
                    // `last_applied_subtitle` does not move on confirmations
                    // alone. Upstream-selection mode has no confirmation
                    // channel for an external's sid at all, so
                    // `Inner::poll_text_policy` adopts the engine's
                    // OPTIMISTIC applied slot as the confirmed one when it
                    // releases the held external. The engine's applied
                    // already names the target the moment the dispatch above
                    // is recorded, so a poll job landing between this pump
                    // and the dispatch makes the outgoing track look like the
                    // incoming one: the replace reads as a no-op, the eager
                    // park never runs, and the outgoing branch keeps the
                    // one live text slot for good ("another text branch
                    // already feeds the consumer"). Measured,
                    // deterministic, on both adaptive switch tests of
                    // `regression_upstream_selection_extsub`.
                    //
                    // Read after the engine lock, where v1 read it, but the
                    // window between the READ and the USE is not v1's any
                    // more: v1 used the answer in the next statement, this
                    // one uses it a worker-queue depth later. What re-closes
                    // that window is the generation stamped beside it - the
                    // only writer that can invalidate the answer without
                    // superseding the job is the gapless activation, and that
                    // one moves the generation (see `Job::DispatchSelection`).
                    // `last_applied_subtitle` is the CONFIRMED slot; the
                    // engine's own `applied` is optimistic and already names
                    // the new target by now.
                    let replacing = {
                        let applied = self.inner.last_applied_subtitle.lock();
                        applied.is_some() && *applied != target.subtitle
                    };
                    let generation = self.inner.current_generation();
                    if !self.inner.queue_job(Job::DispatchSelection {
                        target,
                        seqnum,
                        replacing,
                        generation,
                        queued: QueuedAt(Instant::now()),
                    }) {
                        // No decider: nothing is ever going to send this, so
                        // the wait recorded a few lines above would sit until
                        // its deadline. Same report the refusal path makes.
                        warn!(?seqnum, "selection dispatch refused: no worker");
                        self.inner.selection.lock().dispatch_failed(seqnum);
                        break;
                    }
                }
                Dispatch::Refresh(seqnum) => self.queue_job(Job::RefreshSeek { seqnum }),
                // No event goes anywhere: the request was already satisfied, so
                // the pipeline needs nothing and only the CALLER is owed an
                // answer.
                Dispatch::ConfirmApplied(target, seqnum) => {
                    self.inner.post_synthetic_streams_selected(
                        &target,
                        seqnum,
                        "the request was already satisfied",
                    )
                }
            }
        }
    }

    /// Carry out a selection [`Self::pump_selection`] has already dispatched:
    /// the eager text-branch work, then the `SELECT_STREAMS` itself.
    ///
    /// Runs on the DECIDER through [`Job::DispatchSelection`], never on the
    /// pumping caller. Decision inputs that
    /// describe the world the send lands IN - the upstream-selection mode,
    /// the attached externals, whether upstream owns selection - are read
    /// HERE, fresher than the caller could have read them and never staler
    /// than the send, which is enqueued from this same thread onto a FIFO
    /// lane afterwards. The one input that describes the world being LEFT,
    /// `replacing`, is decided by the pump and carried (see there).
    ///
    /// Returns whether the dispatch left the crate. `false` means it was
    /// refused synchronously and
    /// [`selection::SelectionEngine::dispatch_failed`] has already been
    /// reported for `seqnum` - the pump's loop must BREAK on that, or it
    /// re-decides the very selection that just failed, forever. A dispatch
    /// the guard below drops returns `true`: nothing failed, there
    /// was simply nothing left to send.
    pub(crate) fn dispatch_selection(
        &self,
        target: selection::TrackSelection,
        seqnum: gst::Seqnum,
        replacing: bool,
        generation: u64,
        queued: Instant,
    ) -> bool {
        let inner = &self.inner;
        inner.record_dispatch_queue_age(queued.elapsed());

        // THE WAIT IS THE PERMISSION TO SEND. Between the pump's decision and
        // this job, the engine can settle or drop the very wait this dispatch
        // belongs to, and neither of those two ways leaves anything for an
        // event to confirm:
        //
        // * a `STREAMS_SELECTED` whose CONTENT matches the target settles the wait even
        //   though it carries a different seqnum, and
        // * a new collection clears the wait outright
        //   (`SelectionEngine::collection_changed`), which is exactly the moment the
        //   ids this job holds stop existing.
        //
        // The second one is the dangerous one: sending ids from a collection
        // decodebin3 has already replaced is the shape that walks into
        // `gst_stream_collection_get_stream`'s `g_assert`. v1 closed the
        // window by sending in the same breath as the decision; this closes
        // it by asking whether the decision still stands.
        //
        // No `dispatch_failed` here, deliberately: there is no wait left to
        // fail, and reporting one would clear a NEWER dispatch's re-emit
        // intent (`dispatch_failed` also drops `refresh_wanted`).
        //
        // This does change one v1 behaviour on purpose: two selections
        // dispatched back to back at a resting PAUSED both used to send, and
        // now only the one the engine is still waiting on does. That is
        // strictly closer to the engine's own model of one wait at a time,
        // and it removes a stale event racing the newer one at the demuxer.
        if !inner.selection.lock().selection_in_flight(seqnum) {
            debug!(
                ?seqnum,
                ?target,
                "the wait this selection was dispatched for is gone; not sending it"
            );
            return true;
        }

        // A gapless activation between the pump and here has re-seeded the
        // confirmed subtitle slot for the INCOMING item, so the carried
        // answer describes a track that no longer plays. Re-read it: post
        // activation `last_applied_subtitle` is `None`, which reads as "not
        // replacing anything", which is the truth - the new item has not
        // confirmed a subtitle yet, and there is no outgoing push to wake.
        //
        // Only across a generation change. Within one item the carried
        // answer is the load-bearing one (see `Self::pump_selection`).
        let replacing = if generation != inner.current_generation() {
            let fresh = {
                let applied = inner.last_applied_subtitle.lock();
                applied.is_some() && *applied != target.subtitle
            };
            debug!(
                ?seqnum,
                carried = replacing,
                fresh,
                generation,
                live = inner.current_generation(),
                "an item boundary crossed this dispatch; re-reading what it replaces"
            );
            fresh
        } else {
            replacing
        };

        // The subtitle slot MOVES (off, on, or to another track): detach the
        // text branch now, before the send.
        //
        // Two reasons, and they are the same reason. Waiting for
        // decodebin3's pad removal queues behind whatever the branch's
        // tail is doing with the cue in flight, so the on-screen cue would
        // linger until its line ends. And the OUTGOING text slot cannot
        // even release its multiqueue src pad while that push is in
        // flight: `RoutedStream::tqueue` is a plain `queue`, so its
        // default `max-size-time` of 1s counts the DEAD AIR between
        // sparse cues (`gst_queue_apply_gap` advances the time level
        // off GAP events), and it reports itself full holding zero
        // buffers and zero bytes. decodebin3's switch then waits out
        // the outgoing track's cue cadence: measured 1.6s at a 2s cue
        // period and 4.6s at 4s, with the whole latency sitting in
        // that one blocked push. Parking flushes the branch, wakes
        // the push and moves the pad to an unsynced fakesink, which
        // is why turning subtitles OFF was always instant; a REPLACE
        // has the identical cause and now takes the identical path.
        //
        // "Before the send" is the load-bearing half and it is now
        // STRUCTURAL: this function parks and then enqueues the select
        // effect, both from the deciding thread, and the select lane is
        // FIFO, so no interleaving can put the send first.
        //
        // A REPLACE used to take a FLUSH instead of the re-parenting: the
        // flush woke the push and dropped the outgoing backlog, and moving the
        // pad to a parking sink on top of that regressed the gapless
        // text-to-text switch (3 of 6 runs of
        // `gapless_switch_between_text_bearing_items` failed under CPU load
        // with the park, 0 of 6 without). Every arm parks now anyway, because
        // that flush was the trigger of a captured deadlock against
        // subtitleoverlay's sticky re-push cascade, and it went with the
        // element. What it was FOR is carried by the park's own disposal
        // and by the tqueue time-uncap at detach.
        //
        // A replace normally rides decodebin3's pad swap to free the text
        // slot; in upstream-selection mode no SELECT_STREAMS ever reaches
        // decodebin3, no swap comes, and the outgoing branch would hold the
        // slot forever (`ext-subtitle-regression-2.txt`). The park covers
        // both, which is why the mode is no longer read here at all.
        //
        // Skipped for a same-track re-assertion, which must not blink the cue
        // that is on screen, and for a slot not confirmed on anything (a fresh
        // load, and every gapless activation, which re-seeds it): there is no
        // outgoing push to hand over there. `last_applied_subtitle` is the
        // CONFIRMED slot; the engine's own `applied` is optimistic and already
        // names the new target by now. `replacing` comes from the pump, and
        // the reason it is not read here is written out at its call site.
        //
        // THE PARK RUNS RIGHT HERE, on the thread that owns the text branch.
        // It is the non-blocking half by construction (`detach_text_parts`
        // needs object locks, a DROP probe and a property set, nothing that
        // waits on a streaming thread), which is what licenses it on the
        // decider.
        //
        // It must not be postponed either. The park moves a deselected text
        // stream onto its parking sink, and leaving it linked to a live branch
        // for longer is exactly what stops decodebin3 reconfiguring (see
        // `park_text_streams`). Measured when the park was postponed as well,
        // `regression_gapless` went to 15 failures in 22 runs against 11 in 22
        // for the unchanged code, with
        // `subtitle_disable_survives_a_gapless_transition` twice as likely to
        // fail. The park's own blocking half (the branch disposal) already
        // routes through the deferred-work drain.
        if decisions::park_text_before_dispatch(target.subtitle.is_none(), replacing) {
            Inner::park_text_streams(&self.inner);
        }
        let ids: Vec<&str> = [&target.video, &target.audio, &target.subtitle]
            .into_iter()
            .filter_map(|sid| sid.as_deref())
            .collect();
        // An adaptive main input owns selection (SELECTABLE), and
        // decodebin3 defers to it wholesale. That demuxer rejects
        // any event naming a stream it does not carry
        // ("Unrecognized stream_id", the WHOLE event refused),
        // never posts about streams it cannot know, and decodebin3
        // posts nothing at all in this mode. So external-input
        // streams stay out of the event (the crate routes them
        // itself), only a CHANGED upstream-owned part is sent (a
        // no-op has no activation edge to confirm it), and an
        // unchanged one is confirmed locally.
        if self.inner.upstream_owns_selection() {
            // Sampled here rather than carried from the pump: what this
            // filter needs is which sids belong to an external input AT SEND
            // TIME, since those are precisely the ones the demuxer would
            // refuse the whole event over.
            let externals: Vec<Vec<String>> = {
                let routing = self.inner.routing.lock();
                routing
                    .inputs
                    .iter()
                    .filter(|i| i.external.is_some())
                    .map(|i| i.stream_ids())
                    .collect()
            };
            let external_sids: Vec<&str> = externals
                .iter()
                .flat_map(|sids| sids.iter().map(String::as_str))
                .collect();
            let upstream_ids: Vec<&str> = ids
                .iter()
                .copied()
                .filter(|sid| !external_sids.contains(sid))
                .collect();
            // COMPARED here, RECORDED when the event is actually sent (see
            // the `Outcome::SelectSent` arm of `Self::effect_done`); the
            // reasons live with the rule.
            let split = {
                let last_sent = self.inner.last_upstream_ids.lock();
                decisions::select::upstream_split(&last_sent, &upstream_ids)
            };
            match split {
                decisions::select::UpstreamSplit::RefuseEmptyDeselect => {
                    warn!(
                        ?target,
                        "an upstream-owned deselect names no stream; refusing it"
                    );
                    self.inner.selection.lock().dispatch_failed(seqnum);
                    return false;
                }
                decisions::select::UpstreamSplit::Send => {
                    let main_input = {
                        let routing = self.inner.routing.lock();
                        routing
                            .inputs
                            .iter()
                            .find(|input| input.external.is_none())
                            .map(|input| input.element.clone())
                    };
                    if let Err(err) = self.select_streams_to(
                        main_input,
                        &upstream_ids,
                        Some(seqnum),
                        target.subtitle.clone(),
                    ) {
                        warn!(?err, "selection dispatch refused");
                        self.inner.selection.lock().dispatch_failed(seqnum);
                        return false;
                    }
                    // Confirmation arrives from the demuxer with this
                    // seqnum; the translate arm keeps the crate-owned
                    // subtitle slot (see MessageView::StreamsSelected).
                    // Unless it does not, which is what the field
                    // showed: bounded fallback below.
                    Inner::arm_upstream_confirm_fallback(&self.inner, target.clone(), seqnum);
                }
                decisions::select::UpstreamSplit::ConfirmLocally => {
                    self.inner.post_synthetic_streams_selected(
                        &target,
                        seqnum,
                        "the upstream-owned part did not change",
                    );
                }
            }
        } else if let Err(err) = self.select_streams(&ids, Some(seqnum)) {
            warn!(?err, "selection dispatch refused");
            self.inner.selection.lock().dispatch_failed(seqnum);
            return false;
        }
        true
    }

    /// Queue a stream selection (ids from the current stream collection).
    /// `seqnum` is stamped on the event so the confirming `StreamsSelected`
    /// message can be attributed to this request (`None` for a fresh one).
    /// Sent to decodebin3 directly, no detour through the sinks.
    ///
    /// An id absent from the advertised collection is refused with `Err`
    /// before anything is queued: decodebin3 ignores such an event wholesale
    /// and never posts a confirmation, so queueing it would starve a caller
    /// waiting on the seqnum forever (measured by
    /// `streams_selected_carries_the_request_seqnum`). A collection change
    /// racing this check can still eat a selection inside decodebin3, which
    /// is what the selection deadline exists for; the gate only closes the
    /// case that could NEVER confirm.
    ///
    /// The send happens on a dedicated thread, NOT inline: decodebin3
    /// handles `SELECT_STREAMS` on the sending thread, and its stream-switch
    /// machinery takes slot pad object locks that a live-spinning slot
    /// streaming thread can starve for seconds to forever (the sticky-event
    /// re-push livelock, which zombified the app's event loop mid switch).
    /// The single queue keeps back-to-back selections ordered. `Ok` means
    /// queued, not applied: confirmation arrives as the `StreamsSelected`
    /// bus message, and a selection superseded by a core swap before it
    /// sends is silently dropped (it could never confirm anyway).
    pub fn select_streams(&self, stream_ids: &[&str], seqnum: Option<gst::Seqnum>) -> Result<()> {
        {
            let selection = self.inner.selection.lock();
            if let Some(unknown) = stream_ids.iter().find(|sid| !selection.knows_stream(sid)) {
                return Err(anyhow!(
                    "stream {unknown} is not in the advertised collection"
                ));
            }
        }
        self.select_streams_to(None, stream_ids, seqnum, None)
    }

    /// [`Self::select_streams`] with an explicit send target. `None` sends
    /// to decodebin3; the upstream-selection split passes the main input.
    ///
    /// `text_sid` names the subtitle stream the selection carries, for the
    /// drain interlock (see [`SelectJob::text_sid`]).
    fn select_streams_to(
        &self,
        target: Option<gst::Element>,
        stream_ids: &[&str],
        seqnum: Option<gst::Seqnum>,
        text_sid: Option<String>,
    ) -> Result<()> {
        if stream_ids.is_empty() {
            return Err(anyhow!("refusing an empty stream selection"));
        }
        let mut builder = gst::event::SelectStreams::builder(stream_ids.iter().copied());
        if let Some(seqnum) = seqnum {
            builder = builder.seqnum(seqnum);
        }
        let Some(db3) = self.inner.core.lock().as_ref().map(|c| c.db3.clone()) else {
            return Err(anyhow!("no dynamic core"));
        };
        let job = SelectJob {
            target: target.unwrap_or_else(|| db3.clone()),
            db3,
            event: builder.build(),
            stream_ids: stream_ids.iter().map(|s| s.to_string()).collect(),
            text_sid,
        };
        self.inner
            .enqueue_effect(Effect::SelectStreams(job))
            .map(|_id| ())
            .map_err(|_| anyhow!("the select sender thread is gone"))
    }

    // Blocking state entry points (see the struct-level Threading docs:
    // MT-safe, but not from streaming threads or the event callback).

    /// Worker side of [`Job::SelectionDeadline`]: a dispatched
    /// `SELECT_STREAMS` waited out its deadline.
    ///
    /// The order of the checks below is the whole safety argument, so it is
    /// numbered. Everything cheap and everything read-only happens before
    /// anything is changed, and the one action that touches the pipeline is
    /// the same `SELECT_STREAMS` the pump would have sent.
    ///
    /// What this function ITSELF issues is flush-free, latency-free and
    /// state-change-free. It does not follow that nothing downstream of it
    /// ever flushes, and the honest statement is the narrower one: a synthetic
    /// confirmation re-enters `translate_message`, whose `StreamsSelected` arm
    /// can arm a subtitle replay (a FLUSH seek on the external input's pads),
    /// and the re-assertion rides `fpb-select`, which can park a deselected
    /// video chain after the send. Both are the ORDINARY consequences of a
    /// confirmation and a selection respectively, reached by the ordinary
    /// code; the deadline adds no flush of its own and never touches the
    /// eager text work the pump does around a dispatch (see the `Retry` arm).
    ///
    /// No lock is ever held across `post_message`, `select_streams` or
    /// `emit`: the post re-enters the bus sync handler ON THIS THREAD, and
    /// translate takes both `routing` and `selection` itself. Where the two
    /// are needed in sequence it is routing first, then selection.
    pub(crate) fn selection_deadline_fired(&self, seqnum: gst::Seqnum) {
        let inner = &self.inner;

        // (1) Revalidate. The confirmation racing the fire is the common and
        //     healthy outcome, and it must cost nothing at all.
        let Some(target) = inner.selection.lock().selecting_target(seqnum) else {
            debug!(
                ?seqnum,
                "a selection deadline fired for a wait that has since confirmed"
            );
            return;
        };

        // (2) Settledness, read HERE and from the pipeline rather than from a
        //     mirrored flag. A buffering park holds an async transition for
        //     its whole duration (the field's was 39s), a settled PAUSED
        //     cannot complete a switch at all (nothing flows), and mid-load
        //     there is nothing to probe. In every one of those the wait is
        //     legitimately outstanding, so the deadline simply extends: the
        //     advisory was re-armed when it fired.
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::replay::settled_playing(current, pending) {
            debug!(
                ?seqnum,
                ?current,
                ?pending,
                "a selection deadline fired below a settled PLAYING; waiting"
            );
            return;
        }

        // (3) A gapless activation owns this seam. A synthetic confirmation
        //     reaching `Inner::try_activate_prepared` mid-window was measured
        //     at 3 failures in 4 (see `pump_selection`), and the activation
        //     posts a collection and a selection of its own that answer
        //     anything waiting behind it.
        if inner.swap_gate.state.lock().activation_pending().is_some() {
            debug!(
                ?seqnum,
                "a selection deadline fired inside a gapless activation; leaving it to the swap"
            );
            return;
        }

        // (4) Has the selection even LEFT the crate yet? Everything below
        //     infers what happened from the pads, and a `SELECT_STREAMS`
        //     still sitting on the select lane (queued behind a wedged send,
        //     or mid-send in decodebin3's own switch machinery) routes
        //     nothing at all - which reads exactly like a selection that was
        //     refused. Those two want opposite answers, and the crate shipped
        //     with the wrong one: the give-up adopted a reality the pending
        //     send then contradicted, and because the timed-out record is
        //     superseded, its late confirmation was drained as an echo. The
        //     engine's idea of what plays stayed wrong until the next
        //     collection change.
        //
        //     So ask the hands. Still in flight means the wait is
        //     legitimately outstanding, exactly as in (2) and (3), and the
        //     deadline simply extends: the advisory was re-armed when it
        //     fired. The retry budget is deliberately NOT spent on this -
        //     a re-assertion would queue a SECOND event behind the first on
        //     the same wedged lane.
        //
        //     BOUNDED, because "wait for the lane" cannot be the last word.
        //     Deferring forever would trade the premature divergence for the
        //     permanent latch the deadline exists to prevent, and this
        //     function is the only thing standing between a wedged lane and
        //     a channel that never answers again. So the deferral holds only
        //     while the lane MIGHT still deliver: past
        //     [`Deadlines::select_defer_budget`] the entry has already produced
        //     the tick's wedge WARN, the lane is provably not coming back on
        //     any timescale a caller cares about, and the fall-through to the
        //     timed-out logic below is the right answer - wrong about a
        //     reality that may still change, but LOUD, unlatching, and
        //     self-correcting if the lane ever heals (a late send's
        //     confirmation arrives against a superseded record and is drained
        //     as an echo, exactly as the give-up's own doc describes).
        if let Some(age) = inner.hands.select_age(seqnum, Instant::now()) {
            let budget = inner.deadlines.lock().select_defer_budget;
            if age < budget {
                debug!(
                    ?seqnum,
                    ?age,
                    "a selection deadline fired for a dispatch the hands have not sent yet; waiting"
                );
                Counters::bump(&inner.counters.deadline_deferrals);
                return;
            }
            warn!(
                ?seqnum,
                ?age,
                ?budget,
                "a selection has been on its lane past the deferral budget; \
                 timing it out anyway rather than waiting for a wedged lane forever"
            );
        }

        // (5) Upstream-owned selection: confirm, NEVER re-dispatch. Re-sending
        //     a SELECT_STREAMS at an adaptive demuxer mid-drain is the
        //     `g_assert(track->draining && !track->selected)` abort, and
        //     decodebin3 posts nothing in this mode anyway, so a missing
        //     confirmation says nothing about whether the selection applied.
        //     Normally the 700ms `fpb-confirm-fallback` has settled this long
        //     before; arriving here means that thread failed to spawn.
        if inner.upstream_owns_selection() {
            warn!(
                ?seqnum,
                ?target,
                "an upstream-owned selection was never confirmed; confirming it locally"
            );
            inner.post_synthetic_streams_selected(
                &target,
                seqnum,
                "the upstream selection deadline ran out",
            );
            Counters::bump(&inner.counters.deadline_confirms);
            return;
        }

        // (6) decodebin3-owned: ask the PADS what is playing, because the
        //     message is exactly what went missing.
        let probed = inner.probe_routed_selection();

        // (7) Applied after all: only the confirmation died. Re-post it. This
        //     is the shape the field kept producing (a freed collection, a
        //     swallowed message), and the one where doing anything MORE than
        //     re-announcing the truth would be churn.
        if probed.matches(&target) {
            warn!(
                ?seqnum,
                ?target,
                ?probed,
                "STREAMS_SELECTED never arrived for a selection the probe finds applied; \
                 confirming from the probe"
            );
            inner.post_synthetic_streams_selected(
                &target,
                seqnum,
                "the selection deadline probe found it applied",
            );
            Counters::bump(&inner.counters.deadline_confirms);
            return;
        }

        // (8) Not applied. Time the dispatch out and act on the verdict.
        let outcome = inner.selection.lock().selection_timed_out(seqnum);
        match outcome {
            // A dispatch that is no longer in flight raced us between (1) and
            // here; there is nothing left to decide.
            selection::TimeoutOutcome::NotInFlight => {}
            selection::TimeoutOutcome::Retry {
                target,
                retries_left,
            } => {
                // Re-assert the SAME target under a FRESH seqnum. A-4 is
                // satisfied by construction: the previous dispatch has already
                // TIMED OUT (it is a superseded record now, so its late
                // confirmation is a harmless echo), and this arm is
                // unreachable in upstream mode, where a re-dispatch is the
                // adaptivedemux2 abort.
                //
                // Deliberately WITHOUT the pump's eager text work: the park
                // already ran for the original dispatch of this same target,
                // and parking a branch again for a re-assertion would take
                // the very track being re-asserted off its renderer.
                let retry = gst::Seqnum::next();
                let ids: Vec<String> = [&target.video, &target.audio, &target.subtitle]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect();
                let dur = inner.deadlines.lock().selection;
                // The engine lock was released between timing the old dispatch
                // out and here, and a caller pump can dispatch the user's NEWER
                // request inside that window. Re-asserting the OLD target then
                // displaces a live wait for a selection somebody actually
                // asked for: it converges, but only after a wrong round trip.
                // The newer wait owns the engine, exactly as in
                // `SelectionEngine::selection_gave_up`.
                let overtaken = {
                    let mut engine = inner.selection.lock();
                    if engine.selection_pending() {
                        true
                    } else {
                        engine.selection_dispatched(retry, target.clone());
                        engine.arm_selection_deadline(retry, Instant::now() + dur, retries_left);
                        false
                    }
                };
                if overtaken {
                    debug!(
                        ?seqnum,
                        ?target,
                        "a newer dispatch overtook the re-assertion; leaving it to that one"
                    );
                    return;
                }
                warn!(
                    ?seqnum,
                    ?retry,
                    ?target,
                    ?probed,
                    retries_left,
                    "the probe does not find a dispatched selection applied; re-asserting it"
                );
                // Through the normal sender, which stays the ONLY place a
                // SELECT_STREAMS leaves this crate.
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                if let Err(err) = self.select_streams(&ids, Some(retry)) {
                    warn!(?err, ?retry, "the deadline's re-assertion was refused");
                    inner.selection.lock().dispatch_failed(retry);
                }
            }
            selection::TimeoutOutcome::Exhausted { target } => {
                // The loud, truthful completion: report what IS playing, not
                // what was asked for. The caller's handlers are exhaustive and
                // re-sync its UI and its senders to reality, so the request is
                // answered rather than left hanging forever.
                //
                // "Never applied" is inferred from the pads, and one way to
                // get there used to be a select still sitting on its lane:
                // the event had not been sent yet, so of course nothing was
                // routed. The give-up then adopted a reality the selection
                // contradicted as soon as it did send, and because the
                // timed-out entry is a SUPERSEDED record its late
                // confirmation was drained as an echo without updating
                // `applied` - the engine's idea of what plays stayed wrong
                // until the next collection change. Step (4) above closes
                // that: an unsent selection is now a fact the crate can read
                // rather than one it has to infer.
                //
                // What remains here is the honest case - the event WAS sent,
                // the pads say it did not take, and the retries are spent.
                let actual = probed.actual(&target);
                let adopted = inner.selection.lock().selection_gave_up(seqnum, &actual);
                if !adopted {
                    // Overtaken between the timeout and here. Reporting a
                    // probed reality that a live newer dispatch is already
                    // moving away from would hand the caller a selection to
                    // display that is obsolete before it arrives, and the
                    // caller's own UI/wire sync would then fight the switch it
                    // asked for. The newer wait has its own deadline.
                    debug!(
                        ?seqnum,
                        ?actual,
                        "a newer dispatch overtook the give-up; not reporting the probed reality"
                    );
                    return;
                }
                warn!(
                    ?seqnum,
                    ?target,
                    ?actual,
                    "giving up on a selection the pipeline never applied; reporting what plays"
                );
                // Emitted DIRECTLY rather than posted on the bus. A failure
                // report must not run translate's side effects (replay arming,
                // gapless activation triggers) off a selection that never
                // happened; it is a report, not a confirmation.
                //
                // The price is that translate's MIRRORS are not updated
                // either, so each one is a deliberate omission:
                //
                // * `last_applied_subtitle` - it tracks the CONFIRMED subtitle slot and drives
                //   the pump's eager text park on a replace. Leaving it naming the previous
                //   slot is right: the switch did not happen, so there is no new outgoing
                //   branch to hand over next time, and a wrong value here costs a missed park,
                //   never a spurious one.
                // * `last_upstream_ids` - upstream-selection bookkeeping only, and this arm is
                //   unreachable in that mode (step (5) returns first).
                // * the engine's `ReportProgress` ladder - it climbs on decodebin3's REAL
                //   reports; a probe is not one, and claiming a rung from a selection that
                //   never applied is exactly the seeding `collection_changed` is documented to
                //   refuse.
                // * `unblock_selected_externals` - releases holds for an external that BECAME
                //   selected. Nothing became selected here; the external's own hold release
                //   rides its next real selection or the drain.
                inner.emit(PlaybinEvent::StreamsSelected {
                    video: actual.video.clone(),
                    audio: actual.audio.clone(),
                    subtitle: actual.subtitle.clone(),
                    seqnum,
                });
                inner.emit(PlaybinEvent::Warning {
                    error: gst::glib::Error::new(
                        gst::CoreError::Failed,
                        &format!(
                            "stream selection was never applied after \
                             {SELECTION_DEADLINE_RETRIES} re-assertions; playing {actual:?}"
                        ),
                    ),
                    src: None,
                    debug: None,
                });
                Counters::bump(&inner.counters.deadline_giveups);
            }
        }
    }

    /// Worker side of [`Job::RefreshDeadline`]: the top-level `ASYNC_DONE`
    /// that settles a refresh seek never arrived.
    ///
    /// No retries, unlike a selection. A refresh is a cosmetic re-emit (the
    /// freshly selected track renders at its next cue either way), so failing
    /// it costs nothing and is the correct answer; what CANNOT be tolerated is
    /// leaving `refreshing` set, because that blocks every later dispatch in
    /// both PLAYING and PAUSED. The event this ends with is the one three
    /// worker sites already emit for exactly this outcome.
    pub(crate) fn refresh_deadline_fired(&self, seqnum: gst::Seqnum) {
        let inner = &self.inner;

        if !inner.selection.lock().refresh_in_flight(seqnum) {
            debug!(
                ?seqnum,
                "a refresh deadline fired for a seek that has since settled"
            );
            return;
        }

        // Settled means the ASYNC_DONE can no longer be coming: ANY top-level
        // one clears the wait (attribution is by exclusivity, see
        // `SelectionEngine::refresh_done`), so a pipeline that holds no async
        // transition has no such message left to post. Unsettled is the
        // buffering park again, where the deadline just extends.
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if current < gst::State::Paused || pending != gst::State::VoidPending {
            debug!(
                ?seqnum,
                ?current,
                ?pending,
                "a refresh deadline fired at an unsettled pipeline; waiting"
            );
            return;
        }
        if inner.swap_gate.state.lock().activation_pending().is_some() {
            debug!(
                ?seqnum,
                "a refresh deadline fired inside a gapless activation; leaving it to the swap"
            );
            return;
        }

        warn!(
            ?seqnum,
            "a refresh seek's ASYNC_DONE never arrived at a settled pipeline; failing it"
        );
        inner.selection.lock().refresh_failed(seqnum);
        inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
    }
}
