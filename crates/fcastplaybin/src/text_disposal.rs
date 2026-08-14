//! Taking a text branch apart: detaching its parts and disposing of
//! them without stalling the thread that asked.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gst::prelude::*;
use tracing::{debug, warn};

use crate::{
    FcastPlaybin, Inner,
    flush::{FlowStage, FlushReason},
    jobs::Job,
    routing::{RoutedStream, StreamKind},
};

/// A text branch already taken out of the graph, waiting for its blocking
/// teardown. See [`Inner::detach_text_parts`].
pub(crate) struct TextDisposal {
    /// decodebin3's output pad for this stream, the pad that STAYS when the
    /// branch below it leaves.
    ///
    /// Carried purely so the disposal can repair what its own flush pairs may
    /// have done ABOVE it: for a text stream that ghost pad targets the
    /// multiqueue slot directly, and a push caught in a pair's window latches
    /// that slot for good (see [`Inner::unlatch_db3_slot`]).
    db3_src_pad: gst::Pad,
    /// The text queue's sink pad, which the flush pair goes to.
    downstream: gst::Pad,
    /// The per-stream queue, to be NULLed and dropped from the pipeline.
    tqueue: Option<gst::Element>,
    /// The branch's tail: the per-stream `appsink` that feeds the subtitle
    /// consumer, to be NULLed and dropped behind the queue. It is the target
    /// of pair D ([`Inner::dispose_text_branch_on`]).
    ///
    /// `Option` because [`RoutedStream::appsink`] is: it mirrors the slot the
    /// disposal took the branch apart from, and every LIVE branch has one.
    appsink: Option<gst::Element>,
}

// THE OTHER EAGER WORK, and why only the park is left.
//
// A dispatched selection used to choose between two eager text-branch
// actions, `DeferredTextWork::Park` and `::Flush`. The FLUSH sent a pair
// through the outgoing branch's queue so the switch was not queued behind its
// backlog -- which subtitleoverlay made necessary, because it prefetch-blocked
// the next cue's push and the outgoing slot's multiqueue src pad therefore sat
// mid-push for the media's whole cue cadence (measured 1.6s at a 2s cue period,
// 4.6s at 4s). Every arm became a PARK after that flush was caught deadlocking
// the decider against the overlay's sticky re-push cascade, with the flush kept
// reachable behind `FCAST_EAGER_REPLACE_FLUSH` so the lever restored v1
// exactly.
//
// There is no v1 left to restore. The flush, its intent slot, its counter, its
// five levers and its `eager_branch` census reason are gone; what carried the
// work all along -- the park, plus the branch disposal behind it -- is the
// whole of it now. See `decisions::park_text_before_dispatch`.

/// Mid-play disposals whose branch would not quiesce inside the budget and
/// fell back to the v1 queue pair ([`Inner::dispose_text_branch_on`]).
///
/// Not a failure: it is the accepted residual of the double-block geometry,
/// where a cue push parked inside the branch's tail holds the queue's src
/// stream lock while the feeder push is blocked on buffer-count fullness the
/// time-uncap cannot relieve. Worst case equals v1. What it must not be is
/// SILENT, and what a nonzero count on a default suite means is "look".
static DISPOSAL_QUIESCE_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Which boundary a text-branch disposal is running at.
///
/// The two are genuinely different problems. At a TEARDOWN the graph is on its
/// way to NULL, a latched slot
/// harms nobody, and the flush choreography is measured-tuned and pinned by
/// three fuzz seeds. MID-PLAY the same pair latches a slot decodebin3 is about
/// to reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisposalBoundary {
    /// A replace, a park, a dead-branch reclaim, a leaving input, the LOAD
    /// boundary, or the drain of any of those. The graph outlives this
    /// disposal.
    ///
    /// The load boundary is easy to miss and belongs here: `teardown_core`'s
    /// `detach_text_branch(.., false)` runs at a pipeline at READY and takes
    /// this arm. Two consequences
    /// worth knowing. The mid-play census reasons (`disposal_consumer`,
    /// `disposal_queue`) are therefore reachable from a plain LOAD and not
    /// only from playback. And the quiesce budget is spent on the CALLER's
    /// thread there - up to `FCAST_DISPOSAL_QUIESCE_MS` per branch - which is
    /// harmless in practice because a branch at READY is trivially quiescent
    /// and the first trylock succeeds, but it is the caller's time, not a
    /// worker's.
    MidPlay,
    /// `Teardown::run` and `Inner::drain_disposals_for_teardown`. Bit-for-bit
    /// v1, deliberately.
    Teardown,
}

/// How long a mid-play disposal waits for its branch to go quiet before
/// falling back to the v1 flush pair. Overridden by
/// `FCAST_DISPOSAL_QUIESCE_MS`.
const DISPOSAL_QUIESCE_MS: Duration = Duration::from_millis(50);

/// The poll step inside [`DISPOSAL_QUIESCE_MS`].
const DISPOSAL_QUIESCE_STEP: Duration = Duration::from_millis(2);

impl Inner {
    /// Take a live text stream out of the graph: unlink it, wake anything
    /// parked in it, and drop its queue and tail.
    ///
    /// WAKING the branch is load-bearing, whatever is holding it. A tail that
    /// blocks its push leaves the text pad never idling, and decodebin3's
    /// IDLE-probe deactivation then hangs (the same deadlock class as
    /// playsink's text-chain teardown). subtitleoverlay produced that state on
    /// every cue -- it prefetched the next one and BLOCKED the push until
    /// video reached its timestamp -- and an appsink at PAUSED still produces
    /// it in `gst_base_sink_wait_preroll`. See [`Inner::detach_text_parts`],
    /// which is where the waking happens.
    pub(crate) fn detach_text_branch(
        inner: &Arc<Inner>,
        routed: &mut RoutedStream,
        defer_disposal: bool,
    ) {
        let Some(downstream) = routed.downstream.take() else {
            return;
        };
        let tqueue = routed.tqueue.take();
        let appsink = routed.appsink.take();
        Inner::detach_text_parts(
            inner,
            &routed.db3_src_pad,
            &downstream,
            tqueue,
            appsink,
            defer_disposal,
        );
    }

    /// The teardown half of [`Inner::detach_text_branch`], split out so
    /// a caller holding the routing lock can take the pads out of the entry,
    /// RELEASE the lock, and only then run this.
    ///
    /// It must not run under that lock. The flush and the NULL both block on
    /// pad stream locks held by streaming threads that are themselves waiting
    /// on routing (see [`Inner::live_text_downstream_pads`]).
    /// `defer_disposal` forces the blocking disposal onto the worker no
    /// matter the pipeline state. Passed by the pad-removed path, which runs
    /// on a streaming thread where the disposal's flush is forbidden (the
    /// crate rule stated on [`Job::FinishActivation`]). Every other caller
    /// passes false and keeps the resting-PAUSED postponement exactly as it
    /// was.
    pub(crate) fn detach_text_parts(
        inner: &Arc<Inner>,
        db3_src_pad: &gst::Pad,
        downstream: &gst::Pad,
        tqueue: Option<gst::Element>,
        appsink: Option<gst::Element>,
        defer_disposal: bool,
    ) {
        // UNLINKING FIRST, and it does not block. `gst_pad_unlink` needs only
        // the two pads' object locks, never a stream lock, so it works even
        // while the branch's task is stuck inside its tail. Taking the
        // branch out of the graph immediately is the part the gapless
        // transition depends on, which is why postponing the whole park
        // regressed it (15 failures in 22 runs against 11 in 22, see
        // fuzz-campaign-findings.md).
        //
        // WHAT THE BRANCH'S OWN QUEUE DOES WHILE IT IS BEING TAKEN APART. The
        // unlink below leaves `tqueue`'s src pad without a peer, and the next
        // `gst_queue_loop` push into it returns GST_FLOW_NOT_LINKED, on which
        // the queue posts "Internal data stream error" and the run fails
        // (measured at 5 runs in 20 by the buffering fuzz seed). The waking
        // flush only reaches it later, in
        // `dispose_text_branch`, and can be postponed to a state edge, so
        // there is a window of arbitrary length where the queue is running
        // into a hole.
        //
        // A DROP probe closes it. gstpad.c runs push probes BEFORE the peer
        // lookup, so a dropped buffer returns GST_FLOW_OK and the queue never
        // sees NOT_LINKED. Nothing is lost: everything this queue still holds
        // belongs to a branch that is being disposed of, and the disposal
        // flushes it away regardless.
        //
        // NOT the flush the older note in `park_video_chain_for_deselect`
        // implies. Sending FLUSH_START to `downstream` ahead of the unlink
        // would work through `gst_queue_handle_sink_event`, which forwards the
        // event and then calls `gst_pad_pause_task` on the src pad, waiting for
        // a stream lock the queue's own blocked push holds. That is the exact
        // deadlock `tests/teardown_races.rs` is built around, and it would put
        // it at the one call site whose non-blocking property the gapless
        // transition depends on. `gst_pad_add_probe` takes only the pad's
        // object lock and returns at once, so this keeps that property. It also
        // avoids the hazard a flush here would create, of leaving the branch's
        // tail (still linked at this instant) flushing until a disposal that a
        // resting PAUSED can defer indefinitely.
        // WHAT THE DROP PROBE CANNOT COVER: a push ALREADY blocked inside the
        // tqueue's CHAIN function, waiting for room. The probe sits on the
        // queue's src pad and the flush that eventually lands on its sink pad
        // sets `srcresult = FLUSHING` and wakes that chain call, which then
        // returns GST_FLOW_FLUSHING (`gstqueue.c` `out_flushing`) into
        // decodebin3's multiqueue loop. That latches the single-queue for good
        // and adaptivedemux2's ONE output task pauses with nothing posted:
        // FREEZE-DIAGN.md sections 1 and 5. No probe can help, the push is
        // past every probe point by then.
        //
        // Raising the limit DOES help, and it is the documented purpose of the
        // signal: `gst_queue_set_property` -> `queue_capacity_change` signals
        // `item_del` precisely so "the _chain function ... might have more room
        // now". So the blocked chain wakes with `srcresult` still OK, enqueues,
        // and returns GST_FLOW_OK.
        //
        // ONLY max-size-TIME, which is the dimension that actually blocks here:
        // this queue reports itself full on dead air between sparse cues
        // (`gst_queue_apply_gap` advances the time level off GAP events) while
        // holding zero buffers and zero bytes, which is the whole reason the
        // push is parked. Lifting all three instead was a real defect: the queue
        // then accepts unboundedly, materially more of the external's data is
        // pulled through its own chain inside the detach window, and that
        // exposes an unguarded NOT_LINKED there. A fuzz seed detaching a
        // SELECTED external right after a seek failed 8 of 11 runs with all
        // three lifted and 0 of 7 without, on "the
        // pipeline posted an error: Internal data stream error" attributed to
        // the external's own uri. The buffer and byte caps stay at their
        // defaults, so the data volume in flight is unchanged from before this
        // wake existed. Lever: `FCAST_NO_TQUEUE_UNCAP_ON_DETACH`.
        //
        // And only on an ADAPTIVE main input, which is where the hazard this
        // wake exists for lives: adaptivedemux2 serves every track from ONE
        // output loop and pauses it for good on a FLUSHING return, killing the
        // whole item. A non-adaptive demuxer loses only the flushing stream to
        // the multiqueue latch, which is the status quo this wake never had to
        // change, and time-only still cost that seed one failure in seven
        // there.
        //
        // PROMOTED TO ALL MODES. The wake now stands in for the disposal's
        // queue pair, and that pair was not adaptive-only, so its replacement
        // cannot be either: a non-adaptive item that loses its text stream to
        // a multiqueue latch is a silently dead
        // subtitle track, which is exactly the "silent corruption" the slot
        // reuse causes. The three-cap lift stays refused (the measured 8-of-11
        // failure above); this is still time-only.
        // Lever: `FCAST_UNCAP_ADAPTIVE_ONLY` restores the adaptive-only
        // conditional, `FCAST_NO_TQUEUE_UNCAP_ON_DETACH` still kills the wake
        // everywhere.
        //
        // LEVER INTERACTION, worth knowing before an A/B: the uncap is what
        // makes a mid-play branch QUIESCE, so killing it no longer merely
        // restores the old timing. It makes
        // `dispose_text_branch_on`'s quiescence probe fail and fall back to
        // the v1 flush pair, counted in `disposal_quiesce_timeouts`. Measured:
        // `subtitle_disable` runs ~123 s on the default arm, ~234 s with the
        // uncap off and ~237 s with the pair restored outright. The two levers
        // are not independent, and a run with both set is v1 rather than a
        // third arm.
        if let Some(tqueue) = &tqueue
            && (std::env::var_os("FCAST_UNCAP_ADAPTIVE_ONLY").is_none()
                || inner.upstream_owns_selection())
            && std::env::var_os("FCAST_NO_TQUEUE_UNCAP_ON_DETACH").is_none()
        {
            debug!(tqueue = %tqueue.name(), "lifting the text queue's time cap to wake a parked push");
            tqueue.set_property("max-size-time", 0u64);
        }

        if let Some(tqueue) = &tqueue
            && std::env::var_os("FCAST_NO_DETACH_DROP_PROBE").is_none()
            && let Some(qsrc) = tqueue.static_pad("src")
        {
            // Never removed: this pad goes to NULL and leaves the pipeline
            // with its queue in `dispose_text_branch`.
            qsrc.add_probe(gst::PadProbeType::DATA_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Drop
            });
        }

        // Text bypasses ssync, so its source is the decodebin3 pad itself.
        let _ = db3_src_pad.unlink(downstream);
        if let Some(tqueue) = &tqueue {
            // The tail must not stay wired without a live stream: stale caps
            // carried across a load wedge the next preroll (subtitleoverlay's
            // renderer state did this spectacularly on a VOBSUB dvdspu
            // splice; an appsink is tamer and the rule is the same).
            if let Some(qsrc) = tqueue.static_pad("src")
                && let Some(peer) = qsrc.peer()
            {
                let _ = qsrc.unlink(&peer);
            }
        }
        // Cluster (a) of the four surgery sites. The queue and its pads leave
        // with the disposal; decodebin3's src pad STAYS, and a FLUSHING on it
        // is the slot latch that makes the next stream to reuse that slot
        // silently undeliverable.
        Self::flow_census(
            FlowStage::DetachTextParts,
            std::slice::from_ref(db3_src_pad),
        );

        // THE DRIVER'S `Clear`, at the one point every mid-play removal of a
        // consumer branch funnels through: a track switch, a switch to
        // subtitles-off, a video unroute's park, the seat reclaim, an external
        // detach.
        //
        // AFTER the unlinks and the DROP probe above, not before: until then
        // the branch is still live and one more cue could arrive to outlive
        // the `Clear`. From here nothing can reach the consumer through this
        // branch again. Redundant with the pad probe's FLUSH_STOP `Clear` on
        // the paths that do flush, deliberately: two independent producers.
        //
        // # SCOPED: only when nothing else is feeding
        //
        // A `Clear` says "everything you hold is stale", and this branch is
        // only entitled to say that about ITS OWN deliveries. At a gapless
        // boundary the outgoing item's input removal runs AFTER the incoming
        // item's branch may already have linked and delivered, and an
        // unscoped `Clear` there wipes the new item's cue, a blank subtitle
        // track for one cue interval, with nothing in the log to explain it.
        //
        // The graph answers this without a generation token on the wire: if
        // another routed Text entry is live on the consumer, the feed has
        // already changed hands and this disposal has nothing to retract.
        // Read under the routing lock and released before the send, because
        // `feed_subtitle` calls foreign code.
        //
        // A wire-level alternative (stamp cues and clears with the load
        // generation, let the consumer drop stale ones) was considered and
        // refused: it pushes filtering state into EVERY consumer to close a
        // race the driver can see directly, and the driver is the only party
        // that knows which branch a `Clear` came from.
        if appsink.is_some() {
            let superseded = {
                let routing = inner.routing.lock();
                routing
                    .routed
                    .iter()
                    .any(|r| r.kind == StreamKind::Text && Inner::consumer_branch_is_live(r))
            };
            if superseded {
                debug!(
                    "another text branch already feeds the consumer; \
                     not clearing on this branch's detach"
                );
            } else {
                inner.send_subtitle_clear();
            }
        }

        let disposal = TextDisposal {
            db3_src_pad: db3_src_pad.clone(),
            downstream: downstream.clone(),
            tqueue,
            appsink,
        };
        // THE REST CAN BLOCK, and it is postponed for exactly ONE reason now:
        // `defer_disposal`, set by the pad-removed path, which runs on a
        // streaming thread where the disposal's flush is forbidden outright.
        //
        // # A resting PAUSED used to postpone it too, and that is now WRONG
        //
        // With subtitleoverlay the flush could not complete at a resting
        // PAUSED: the queue's task was parked pushing INTO the overlay behind
        // sinks in `gst_base_sink_wait_preroll` and nothing in the overlay
        // could wake it, so turning subtitles off while paused wedged the
        // caller and detaching an external wedged the worker with every job
        // behind it. A consumer branch's parked push is inside its OWN
        // appsink, and the disposal pair ([`FlushReason::DisposalConsumer`])
        // flushes that appsink's sink pad, which is exactly what wakes it.
        // That is not an argument:
        // `sink_subtitles::an_inline_disposal_of_a_parked_paused_branch_does_
        // not_wedge` drives this very path at a resting PAUSED and fails
        // without the pair.
        //
        // And postponing here COSTS what the transport exists to buy. The
        // branch queue is named after the decodebin3 pad it serves
        // (`fpb-tqueue-<pad>`), a postponed disposal leaves it in the pipeline,
        // and `gst_bin_add` refuses a duplicate name -- so the incoming track's
        // branch could not be built at all while the outgoing one waited for a
        // state edge that will not come until the user resumes. Measured as
        // "failed to add the text queue" once per poll, for as long as the
        // pipeline stayed paused, with the incoming track never linked and no
        // cue behind it. Bite-proof: `sink_subtitles::
        // a_paused_disposal_frees_the_branch_for_the_next_link`.
        if defer_disposal && std::env::var_os("FCAST_NO_TEXT_WORK_DEFERRAL").is_none() {
            debug!(
                defer_disposal,
                "postponing a text branch disposal off the calling thread"
            );
            // A new postponed item invalidates the last drain's no-op
            // verdict (see `Inner::drain_poke_parked`).
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
            inner.deferred_text_disposal.lock().push(disposal);
            // The resting-PAUSED postponement waits for a state edge by
            // design (the drain cannot complete below PLAYING anyway). A
            // disposal handed over by the pad-removed path has no edge
            // coming, so poke the worker the way the flush intent does.
            if defer_disposal {
                inner.queue_job(Job::DrainTextWork);
            }
        } else {
            // The last silent FLUSHING injector: this runs on every mid-play
            // park or replace, and it sends flush pairs to the branch's tail
            // and to its queue. The deferred path above logs; this one did
            // not, which made a field log ambiguous when an adaptive demuxer
            // discarded a FLUSHING on a dash text track with no crate line
            // anywhere near it.
            debug!(
                downstream = %disposal.downstream.name(),
                tqueue = ?disposal.tqueue.as_ref().map(|q| q.name().to_string()),
                "disposing of a text branch inline"
            );
            inner.dispose_text_branch(disposal);
        }
    }

    /// The blocking half of a text detach: wake anything parked in the branch
    /// and drop its queue.
    ///
    /// THE MID-PLAY entry point: `dispose_text_branch_on` against `&self`,
    /// with the boundary fixed. The two teardown callers pass the other
    /// boundary and their own handles, because `Inner` may be gone by then
    /// (see [`Inner::drain_disposals_for_teardown`] and [`Teardown::run`]).
    ///
    /// It carried the flow census of cluster (b) until subtitleoverlay went:
    /// after the pair and the queue's NULL, was subtitleoverlay's shared
    /// `subtitle_sink` latched FLUSHING? That census had a subject only
    /// because the pad STAYED in the graph while branch after branch came and
    /// went. A consumer branch's tail leaves with the branch, so there is no
    /// staying pad left to survey and the stage is gone with the element.
    pub(crate) fn dispose_text_branch(&self, disposal: TextDisposal) {
        Self::dispose_text_branch_on(&self.pipeline, disposal, DisposalBoundary::MidPlay);
    }

    /// [`Inner::dispose_text_branch`] against handles rather than `&self`, so
    /// the teardown boundary can run it after `Inner` is gone (see
    /// [`Teardown`]).
    pub(crate) fn dispose_text_branch_on(
        pipeline: &gst::Pipeline,
        disposal: TextDisposal,
        boundary: DisposalBoundary,
    ) {
        // PAIR D. A cue push that was already parked inside the branch's tail
        // when the branch was severed cannot be reached THROUGH the branch any
        // more (both ends are unlinked), and it holds the queue's stream lock,
        // so the queue flush below would wait on it forever. The tail's own
        // static sink pad is the one remaining path to that push, and its
        // FLUSH_START needs no stream lock. Only while the pad is unlinked: a
        // linked pad belongs to a LIVE branch (a replace already relinked) and
        // flushing it would drop the new track's data.
        //
        // # The CONSUMER branch needs this pair, and the first attempt to skip
        // it was wrong
        //
        // This block was first SKIPPED for consumer branches, on the
        // argument that an appsink configured `sync=false async=false
        // drop=true max-buffers=32` contains no wait state. That argument is
        // WRONG, and the source says so twice:
        //
        //  * `async=false` does not disable prerolling. `READY_TO_PAUSED` sets
        //    `need_preroll = TRUE` unconditionally (`gstbasesink.c`, the
        //    READY_TO_PAUSED arm of `gst_base_sink_change_state`); `async` only governs
        //    whether the element reports ASYNC upward.
        //  * `drop`/`max-buffers` live in `gst_app_sink_render_common`, which is
        //    DOWNSTREAM of basesink's preroll block. In PAUSED the chain call never
        //    reaches them: it parks in `gst_base_sink_wait_preroll` with the queue's
        //    stream lock held.
        //
        // So a live consumer branch at PAUSED parks its tqueue loop task
        // inside the appsink, in exactly the geometry pair D exists for, and
        // the skip removed the only thing that could wake it. On the DROP path
        // that is unbounded: `Teardown::run` drains the disposals BEFORE
        // `bounded_descent`, with no rescue armed, so nothing else is coming.
        //
        // NO LOCK around the check and the pair. This used to be a block
        // aimed at subtitleoverlay's single shared `subtitle_sink`, and
        // a `TextSeat` mutex made the check and the pair one critical section
        // against a link that could occupy that pad between them. A per-stream
        // appsink cannot be linked by anyone else (it belongs to the branch
        // being disposed of, which is already out of the routing table) so
        // there is no TOCTOU here to serialize.
        //
        // `flushed_below` is what the slot repair at the bottom of this function
        // keys off: it is TRUE exactly when this disposal sent a pair at a pad
        // beneath decodebin3, which is the only way it can have latched the
        // slot above it.
        let mut flushed_below = false;
        if let Some(appsink) = &disposal.appsink
            && let Some(pad) = appsink.static_pad("sink")
            && !pad.is_linked()
        {
            Self::send_flush_pair(
                &pad,
                match boundary {
                    DisposalBoundary::MidPlay => FlushReason::DisposalConsumer,
                    DisposalBoundary::Teardown => FlushReason::TeardownConsumer,
                },
            );
            flushed_below = true;
        }
        // Pair E: the queue's own sink pad. At a TEARDOWN boundary it is sent
        // unconditionally, bit-for-bit v1 - seeds 500001/500002/500010 pin
        // that choreography, and it is not worth re-litigating.
        //
        // MID-PLAY it is an injector worth removing. The pair wakes
        // the branch's parked push with FLUSHING, which goes back into
        // `gst_multi_queue_loop` and latches that slot's `srcresult` for good:
        // decodebin3 REUSES the slot for the next stream (silently
        // undeliverable) and an adaptive demuxer's single output loop pauses
        // with nothing posted. So mid-play we PROVE there is nothing to wake
        // instead, and only fall back to the pair when we cannot.
        //
        // The proof is a bounded trylock on the pad's stream lock: a chain
        // call holds it for the whole call, so a free lock means no serialized
        // push is in flight. The upstream unlink already ran at detach
        // (`detach_text_parts`), so no NEW push can resolve a peer and a free
        // answer is stable - there is no TOCTOU after the release. What makes
        // a busy pad go quiet is the time-uncap at detach, which wakes the
        // parked chain call with `srcresult` still OK so it enqueues and
        // returns GST_FLOW_OK: no FLUSHING anywhere.
        //
        // Levers: `FCAST_DISPOSAL_QUEUE_FLUSH` restores the unconditional v1
        // pair; `FCAST_DISPOSAL_QUIESCE_MS` is the budget (0 = one probe, no
        // retry).
        let quiesced = match boundary {
            DisposalBoundary::Teardown => false,
            DisposalBoundary::MidPlay
                if std::env::var_os("FCAST_DISPOSAL_QUEUE_FLUSH").is_some() =>
            {
                false
            }
            DisposalBoundary::MidPlay => Self::await_pad_quiescence(&disposal.downstream),
        };
        if !quiesced {
            if boundary == DisposalBoundary::MidPlay
                && std::env::var_os("FCAST_DISPOSAL_QUEUE_FLUSH").is_none()
            {
                // The double-block geometry: a cue push parked INSIDE the
                // branch's tail holding the queue's src stream lock, with the
                // feeder push blocked on buffer-count fullness that the
                // time-uncap cannot relieve. Nothing non-flushing reaches that push, so
                // this is v1 - worst case, counted rather than silent.
                DISPOSAL_QUIESCE_TIMEOUTS.fetch_add(1, Ordering::SeqCst);
                warn!(
                    pad = %disposal.downstream.name(),
                    "a text branch would not quiesce; falling back to the v1 flush pair \
                     before its queue goes to NULL"
                );
            }
            Self::send_flush_pair(
                &disposal.downstream,
                match boundary {
                    DisposalBoundary::MidPlay => FlushReason::DisposalQueue,
                    DisposalBoundary::Teardown => FlushReason::TeardownQueue,
                },
            );
            flushed_below = true;
        }
        if let Some(tqueue) = disposal.tqueue {
            // The SINK side is now provably not deactivating under a blocked
            // push, which was the third FLUSHING injector in this function.
            //
            // Exactly the sink side, and no more. `post_activate` takes the
            // SINK pad's stream lock, and that is the injector the trylock
            // above proves absent. The SRC pad's deactivation is a different
            // mechanism - `gst_pad_stop_task` joining the queue's loop task -
            // and a loop task parked inside the branch's tail is not visible
            // to any trylock on the sink pad. Pair D is what covers that
            // geometry, which is also why it stays: v1's pair E hit the
            // identical block one step earlier.
            let _ = tqueue.set_state(gst::State::Null);
            let _ = pipeline.remove(&tqueue);
        }
        // The branch's tail, torn down behind its queue. It must not outlive
        // its branch for the same reason the queue
        // must not (`RoutedStream::appsink`), and NULLing it after the queue
        // means the queue's src pad is already deactivated when the sink goes
        // down, so nothing is mid-push into it.
        if let Some(appsink) = disposal.appsink {
            let _ = appsink.set_state(gst::State::Null);
            let _ = pipeline.remove(&appsink);
        }

        // THE SLOT REPAIR. Everything above this line happened BELOW decodebin3, and the
        // one thing this function cannot do from down there is un-break what
        // it broke UP there: a push caught inside a pair's window returns
        // FLUSHING into `gst_single_queue_push_one`, which latches the slot's
        // `srcresult` permanently, and our FLUSH_STOP never goes near the
        // multiqueue's sink pad, which is the only event that would clear it.
        // The track is then dead for the rest of the item with one
        // adaptivedemux2 warning and no other trace, and on a WHOLE-PERIOD
        // text representation, where the demuxer pushes the entire track once,
        // it is dead from the first cue.
        //
        // LAST, deliberately, and after both NULLs rather than straight after
        // the pairs. `set_state (Null)` joins the queue's task and takes its
        // sink pad's stream lock, so a push the pair woke has RETURNED by the
        // time this runs, and its FLUSHING is already in the slot where
        // `unlatch_db3_slot` reads it. Placing the repair between the pairs and
        // the NULLs would have to poll for a latch that the NULL had not
        // finished producing, and would re-activate the slot while the branch
        // under it was still half alive.
        //
        // MID-PLAY only. At a teardown the graph is on its way to NULL, a
        // latched slot harms nobody, and the read is a poll this crate refuses
        // to put on the descent path (`DisposalBoundary`). And only when a pair
        // actually went out: with nothing sent there is nothing to repair, and
        // the read is not free.
        if boundary == DisposalBoundary::MidPlay && flushed_below {
            Self::unlatch_db3_slot(&disposal.db3_src_pad);
        }
    }

    /// Wait, briefly, for `pad` to have no serialized push in flight.
    ///
    /// `FCAST_DISPOSAL_QUIESCE_MS` (default [`DISPOSAL_QUIESCE_MS`]) is the
    /// whole budget, polled in [`DISPOSAL_QUIESCE_STEP`] steps.
    ///
    /// ZERO MEANS NO WAITING, NOT NO SKIPPING. The plan described `0` as
    /// "never wait = always pair"; it is not. A zero budget still takes one
    /// immediate probe, and that probe usually SUCCEEDS - measured to make no
    /// difference at all on the default suites, because the detach-time uncap
    /// has already released the parked push by the time a disposal runs. The
    /// knob shortens the wait for a branch that is busy; it does not restore
    /// the pair. `FCAST_DISPOSAL_QUEUE_FLUSH` is the always-pair restore.
    ///
    /// Polling rather than blocking is the point: the decider must never wait
    /// on a streaming thread's stream lock (see [`Inner::pad_is_quiescent`]),
    /// and a budget in tens of milliseconds is far below any user-visible
    /// switch latency while being far above the microseconds a cue push takes
    /// once the uncap has released it.
    fn await_pad_quiescence(pad: &gst::Pad) -> bool {
        let budget = std::env::var("FCAST_DISPOSAL_QUIESCE_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map_or(DISPOSAL_QUIESCE_MS, Duration::from_millis);
        let deadline = Instant::now() + budget;
        loop {
            if Self::pad_is_quiescent(pad) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(DISPOSAL_QUIESCE_STEP);
        }
    }

    /// Run every pending branch disposal NOW, whatever the pipeline state.
    /// For the teardown boundaries only (stop and drop), which run
    /// [`Inner::flush_parked_text_pushes`] next. A postponed branch is
    /// invisible to that flush (its pads live only in the disposal list),
    /// and the decodebin3-sink flush there pauses a multiqueue task that
    /// may be blocked mid-push into the orphaned full queue, so the
    /// disposals MUST drain first or the teardown wedges on work it cannot
    /// see. Captured with gdb from the field sequence pause, subtitles
    /// off, stop.
    pub(crate) fn drain_disposals_for_teardown(&self) {
        let disposals = std::mem::take(&mut *self.deferred_text_disposal.lock());
        for disposal in disposals {
            debug!("disposing of a postponed text branch at teardown");
            // `dispose_text_branch_on`, not `dispose_text_branch`: this is a
            // teardown boundary and its flow census would be noise (see
            // `Inner::dispose_text_branch`). Byte-for-byte the same disposal.
            Self::dispose_text_branch_on(&self.pipeline, disposal, DisposalBoundary::Teardown);
        }
    }
}

impl FcastPlaybin {
    /// How many text-branch disposals are postponed, waiting for the
    /// worker's settled-PLAYING drain (see [`Inner::run_deferred_text_work`]).
    /// For tests that stage the dispose-versus-link interleaving and need to
    /// know the disposal is actually parked rather than already done.
    #[doc(hidden)]
    pub fn pending_text_disposals(&self) -> usize {
        self.inner.deferred_text_disposal.lock().len()
    }

    /// How many mid-play disposals fell back to the v1 queue pair because the
    /// branch would not quiesce inside `FCAST_DISPOSAL_QUIESCE_MS` (see
    /// [`DISPOSAL_QUIESCE_TIMEOUTS`]). Not a failure - the counted residual -
    /// but a nonzero count on a default suite is worth a look. Not part of the
    /// public API.
    #[doc(hidden)]
    pub fn disposal_quiesce_timeouts() -> u64 {
        DISPOSAL_QUIESCE_TIMEOUTS.load(Ordering::SeqCst)
    }
}
