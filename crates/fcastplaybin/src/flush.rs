//! Flush pairs and the flow census: the crate's only FLUSH senders, and
//! the counters that make them auditable.

use std::sync::atomic::{AtomicU64, Ordering};

use gst::prelude::*;
use tracing::{debug, error, warn};

use crate::{FcastPlaybin, Inner};

/// Why this crate sent a flush pair, for [`Inner::send_flush_pair`]'s census.
///
/// The whole point of naming them is that a removal is judged by an instrument
/// that already moves: every reason below has a live producer and a test that
/// watches it fire BEFORE anything is deleted, so a later "the count went to
/// zero" reads as a removal rather than as a census that was never wired to
/// anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlushReason {
    /// The teardown boundaries' text pair: [`Teardown::run`] and
    /// [`Inner::flush_parked_text_pushes`] on the live branches' queue sinks,
    /// waking parked cue pushes so the descent behind them does not deadlock.
    /// STAYS: every alternative was measured wrong.
    TeardownText,
    /// The same two boundaries, on every input's decodebin3 sink pads
    /// ([`Inner::flush_db3_sink_pads`], segment restore included). STAYS.
    TeardownDb3,
    /// A MID-PLAY disposal's queue pair (the tqueue sink), which now means the
    /// counted quiesce-timeout FALLBACK and nothing else. Zero is the
    /// invariant.
    DisposalQueue,
    /// The same pair at a TEARDOWN boundary.
    ///
    /// Split from the mid-play reason deliberately: a single `disposal_queue`
    /// count that both boundaries incremented could not express "mid-play sends
    /// none" as a property of the number. The merged counter made
    /// `flush_census`'s mid-play assertion fail whenever the teardown test ran
    /// beside it under `cargo test`'s thread pool, which is a real ambiguity in
    /// the instrument and not a test artefact.
    TeardownQueue,
    /// [`Inner::remove_input`]'s bare pair on a leaving input's decodebin3
    /// sink pads, sent on demand.
    RemoveInput,
    /// A MID-PLAY disposal's pair on a CONSUMER branch's own appsink sink pad.
    /// See [`Inner::dispose_text_branch_on`] for why the consumer branch needs
    /// it.
    ///
    /// The overlay-seat reasons are gone with subtitleoverlay rather than kept
    /// as tombstones, because `crate_flush_pairs_for` panics on an unknown
    /// name: an asserted zero on a reason with no producer left is an
    /// assertion wired to nothing.
    DisposalConsumer,
    /// The same pair at a TEARDOWN boundary, split from the mid-play one for
    /// the reason [`FlushReason::TeardownQueue`] documents.
    TeardownConsumer,
}

impl FlushReason {
    const ALL: [FlushReason; 7] = [
        FlushReason::TeardownText,
        FlushReason::TeardownDb3,
        FlushReason::DisposalQueue,
        FlushReason::TeardownQueue,
        FlushReason::RemoveInput,
        FlushReason::DisposalConsumer,
        FlushReason::TeardownConsumer,
    ];

    /// The name tests and logs use. Keep in sync with the plan's table; the
    /// accessors below resolve by this string, so renaming one is a test
    /// failure and not a silent zero.
    fn name(self) -> &'static str {
        match self {
            FlushReason::TeardownText => "teardown_text",
            FlushReason::TeardownDb3 => "teardown_db3",
            FlushReason::DisposalQueue => "disposal_queue",
            FlushReason::TeardownQueue => "teardown_queue",
            FlushReason::RemoveInput => "remove_input",
            FlushReason::DisposalConsumer => "disposal_consumer",
            FlushReason::TeardownConsumer => "teardown_consumer",
        }
    }
}

/// One counter per [`FlushReason`].
///
/// PROCESS-GLOBAL, not a field on `Inner`, and that is forced rather than
/// chosen: the teardown pairs are sent from [`Teardown::run`], which runs
/// after `Inner`'s memory is gone (see the comment above `impl Drop for
/// Inner`). A per-instance counter could never be READ for exactly the
/// reasons this census exists to watch. The cost is that a test binary's
/// counts are cumulative across its tests, so every assertion written against
/// this is either "this reason fired at all" or "this reason never fires
/// anywhere", both of which survive `cargo test`'s thread pool.
///
/// The plan asked for a `Mutex<HashMap<&'static str, u64>>`. The reason set
/// is fixed and small, so a lock-free array indexed by the enum is the same
/// observable instrument without a lock on the flush path or a non-const
/// hasher in a `static`.
///
/// # RELAXED, like every other counter in this file
///
/// Nothing branches on a census counter, so no reader needs the increment
/// ordered against anything else it might observe. `Relaxed` keeps per-counter
/// coherence and monotonicity, which is all the assertions ask for ("this
/// reason fired", "this reason never fires", `after - before` around a joined
/// thread). The only sites that keep a stronger ordering are the ones a
/// DECISION reads, and none of those are counters.
static FLUSH_PAIRS: [AtomicU64; FlushReason::ALL.len()] =
    [const { AtomicU64::new(0) }; FlushReason::ALL.len()];

/// Pairs whose target pad was ACTIVE before the pair and INACTIVE after.
///
/// That is the split-pair shape - a descent racing the flush deactivates the
/// pad between FLUSH_START and FLUSH_STOP, and gstpad.c:6258 DISCARDS the
/// FLUSH_STOP on an inactive pad, leaving the pad flushing for good. Crate
/// side, complementing the downstream-observed `flush_pairs_matched` census in
/// `fcasttest/src/sink.rs`.
pub(crate) static FLUSH_PAIR_ACTIVITY_TRANSITIONS: AtomicU64 = AtomicU64::new(0);

/// Where a flow census was taken: the four pad-surgery clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowStage {
    /// (a) [`Inner::detach_text_parts`], after the unlinks. decodebin3's src
    /// pad stays; a FLUSHING here is the slot latch that makes the NEXT
    /// stream to reuse that slot silently undeliverable.
    DetachTextParts,
    /// (c) [`Inner::remove_input`], after the Null and the unlinks: the pads
    /// the removal LEAVES BEHIND.
    RemoveInput,
    /// (d) [`Inner::remove_video_chain`], after the unlink: the
    /// streamsynchronizer src pad the video sink was hanging off.
    RemoveVideoChain,
    /// (d) [`Inner::ensure_video_chain`], the relink half: both ends of the
    /// freshly joined edge.
    EnsureVideoChain,
}

impl FlowStage {
    const ALL: [FlowStage; 4] = [
        FlowStage::DetachTextParts,
        FlowStage::RemoveInput,
        FlowStage::RemoveVideoChain,
        FlowStage::EnsureVideoChain,
    ];

    fn name(self) -> &'static str {
        match self {
            FlowStage::DetachTextParts => "detach_text_parts",
            FlowStage::RemoveInput => "remove_input",
            FlowStage::RemoveVideoChain => "remove_video_chain",
            FlowStage::EnsureVideoChain => "ensure_video_chain",
        }
    }
}

/// Pads that STAY in the graph and read `GST_FLOW_FLUSHING` after a pad
/// surgery, per [`FlowStage`] (see [`Inner::flow_census`]). Process-global for
/// the same reason as [`FLUSH_PAIRS`].
static FLOW_CENSUS_FLUSHING: [AtomicU64; FlowStage::ALL.len()] =
    [const { AtomicU64::new(0) }; FlowStage::ALL.len()];

/// Slots [`Inner::unlatch_db3_slot`] found latched by our own pair and
/// re-activated. Process-global for the same reason as [`FLUSH_PAIRS`].
///
/// This one's healthy value is ZERO and a nonzero count is not a failure but a
/// REPAIR: it says a disposal's flush pair caught a push and killed a text
/// stream that is now alive again. What it must never be is silent, which is
/// what it was before this counter existed: the whole defect fits between one
/// adaptivedemux2 warning and no cues at all.
pub(crate) static SLOT_UNLATCHES: AtomicU64 = AtomicU64::new(0);

/// Slots [`Inner::unlatch_db3_slot`] read as healthy, i.e. there was nothing
/// to repair. The overwhelmingly common case, counted so that a zero in
/// [`SLOT_UNLATCHES`] can be told apart from an un-latch that never ran.
pub(crate) static SLOT_UNLATCH_CLEAN: AtomicU64 = AtomicU64::new(0);

/// Latched slots the re-activation did NOT clear. Zero is the invariant, and a
/// nonzero count means a text track is dead with the repair in place.
pub(crate) static SLOT_UNLATCH_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Text slots whose sticky CAPS multiqueue destroyed in flight and
/// [`Inner::rescue_lost_text_slot_caps`] put back.
///
/// Like [`SLOT_UNLATCHES`] this reads as a REPAIR rather than a failure: every
/// count is one external subtitle track that would otherwise have been
/// selected, confirmed, parked and silent for the life of the item. Healthy
/// value is zero and it only moves on the race described at the repair.
pub(crate) static TEXT_CAPS_RESCUES: AtomicU64 = AtomicU64::new(0);

/// Lost text CAPS the restore did NOT get back onto the slot. Zero is the
/// invariant, and a nonzero count means a subtitle track is silent for the item
/// with the repair in place.
pub(crate) static TEXT_CAPS_RESCUE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// One pass over the routing table's TEXT entries, for the slot repairs that
/// run at the tail of [`Inner::poll_text_policy`].
///
/// They each used to walk `routing.routed` themselves, which is one lock
/// acquisition per repair on the decider for three filtered scans over the
/// same handful of entries. The buckets are disjoint by `downstream`, so one
/// pass fills all three and every repair reads the SAME instant of the routing
/// table instead of three that can disagree.
///
/// Built under the lock and read outside it, the `refusals` discipline the
/// rest of this crate is written to: the consumers join a multiqueue task or
/// take pad object locks that streaming threads hold, and neither may happen
/// under a crate lock.
struct TextPadSurvey {
    /// Text entries with a LIVE branch that is not flushing right now. The
    /// scope [`Inner::heal_latched_text_slots`] argues for: joined only, with
    /// parked branches deliberately excluded.
    live: Vec<gst::Pad>,
    /// Text entries with NO live branch whose decodebin3 ghost carries no
    /// caps, the visible symptom [`Inner::rescue_lost_text_slot_caps`] repairs.
    capsless_parked: Vec<gst::Pad>,
    /// Text entries with no live branch that DO carry caps, the only ones
    /// [`Inner::stage_text_caps_loss`] can take a caps away from.
    capsful_parked: Vec<gst::Pad>,
}

impl Inner {
    /// THE crate's flush pair, on one pad, counted.
    ///
    /// Every crate-origin pair goes through here so three things hold by
    /// construction rather than by review: the pair is a PAIR (a FLUSH_START
    /// this crate sends is always matched, which is what the downstream
    /// `flush_pairs_matched` census asserts), it is attributed to a
    /// [`FlushReason`] a test can count, and the activity check runs across it.
    ///
    /// # The pad must not go inactive across the pair
    ///
    /// `gst_pad_send_event_unchecked` DISCARDS a FLUSH_STOP on an inactive
    /// pad (gstpad.c:6258). A descent racing this pair - a `set_state(Null)`
    /// on the element between the two sends - therefore leaves the pad
    /// flushing permanently, with a FLUSH_START that no FLUSH_STOP ever
    /// answers. The transition is cheap to observe and impossible to spot in
    /// a log, so it is counted and shouted about instead.
    pub(crate) fn send_flush_pair(pad: &gst::Pad, reason: FlushReason) {
        FLUSH_PAIRS[reason as usize].fetch_add(1, Ordering::Relaxed);
        let active_before = pad.is_active();
        let _ = pad.send_event(gst::event::FlushStart::new());
        // `reset_time = FALSE`, and the flag is the whole point of this line.
        //
        // A crate-injected pair exists to WAKE A PARKED PUSH. It is not a seek,
        // nothing about the timeline moved, and claiming otherwise is
        // expensive: `gst_base_sink_flush_stop` on `reset_time` re-inits the
        // sink's segment to `GST_FORMAT_UNDEFINED` (gstbasesink.c:3276-3280)
        // and posts `GST_MESSAGE_RESET_TIME` (3291-3294), which
        // `GstPipeline` turns into `reset_start_time (pipeline, 0)`
        // (gstpipeline.c:619-628). See [`StartTimeGuard`] for what that costs
        // a PAUSED pipeline. An UNDEFINED-format segment also makes the sink
        // stop answering POSITION queries outright (`no_segment`,
        // gstbasesink.c:5168-5169 -> 5378-5386, with no upstream fallback) and
        // is what feeds the unguarded `gst_segment_to_stream_time
        // (&dec->output_segment, GST_FORMAT_TIME, ...)` in
        // `gst_video_decoder_src_query_default` (gstvideodecoder.c:2071) the
        // GStreamer-CRITICAL in `dash-embedded-subs-delayed.txt`.
        //
        // What the flag does NOT change is the pad-level damage this pair
        // does: `remove_event_by_type (pad, GST_EVENT_SEGMENT)`
        // (gstpad.c:5919) is unconditional, which is why
        // [`Inner::flush_db3_sink_pads`] still has to replay the sticky.
        let _ = pad.send_event(gst::event::FlushStop::new(false));
        if active_before && !pad.is_active() {
            FLUSH_PAIR_ACTIVITY_TRANSITIONS.fetch_add(1, Ordering::Relaxed);
            error!(
                pad = %pad.name(),
                parent = ?pad.parent_element().map(|element| element.name().to_string()),
                reason = reason.name(),
                "a flush pair's pad was deactivated across the pair, so its FLUSH_STOP was \
                 discarded and the pad is flushing for good"
            );
        }
    }

    /// Whether NO serialized push is in flight on `pad` right now.
    ///
    /// A chain function holds its pad's STREAM LOCK for the whole call, by
    /// GStreamer's own contract, so "the stream lock is free" is exactly "no
    /// serialized data or event is being processed on this pad". That is the
    /// question `remove_input` needs: a flush pair exists solely to wake a
    /// parked push, and sending one where nothing is parked is pure damage
    /// (it forwards downstream and de-PLAYs both sinks).
    ///
    /// TRYLOCK, never lock: this runs on the decider, and blocking on a
    /// streaming thread's stream lock is the deadlock this guards against. The
    /// safe binding offers only the blocking `stream_lock()`, so
    /// the try goes through the ffi.
    ///
    /// # Why the answer is stable after the lock is released
    ///
    /// It would be a TOCTOU if a new push could start. Callers unlink the pad
    /// from its peer FIRST, and a push cannot resolve a peer that is gone, so
    /// at most ONE push can be in flight and it can only ever finish. A "free"
    /// answer therefore stays free; a "held" answer may become free, which
    /// only costs a pair that was not needed (v1 behaviour, counted).
    ///
    /// # WHAT IT DOES NOT ANSWER
    ///
    /// Only this pad. A thread parked deeper in the same element - inside
    /// decodebin3's multiqueue, or in a parsebin below it - holds no lock this
    /// can see, and a "quiescent" verdict says nothing about it. That is not a
    /// hypothetical: it is exactly why `remove_input`'s skip was measured
    /// wrong (see there). Use this where the pad IS the boundary of the thing
    /// being proved idle - the branch-local text queue in
    /// `dispose_text_branch_on` - and not as a general "is this subgraph
    /// idle".
    ///
    /// # Recursion
    ///
    /// `g_rec_mutex_trylock` succeeds if THIS thread already holds the lock.
    /// No caller does: the decider is never inside a chain function, which is
    /// the whole point of the ownership split. A future caller that is would
    /// read a false "quiescent".
    pub(crate) fn pad_is_quiescent(pad: &gst::Pad) -> bool {
        // SAFETY: `pad` is a live `GstPad` for the duration of this call (the
        // caller holds a strong ref). `stream_rec_lock` is a public,
        // ABI-stable field of `GstPad` (gstpad.h), never reallocated for the
        // life of the pad. The lock is released before returning and on the
        // same thread that took it, which is `GRecMutex`'s requirement, and
        // nothing between the two calls can panic or unwind.
        unsafe {
            let stream_lock = &raw mut (*pad.as_ptr()).stream_rec_lock;
            if gst::glib::ffi::g_rec_mutex_trylock(stream_lock) == gst::glib::ffi::GFALSE {
                return false;
            }
            gst::glib::ffi::g_rec_mutex_unlock(stream_lock);
            true
        }
    }

    /// After a pad surgery, does a pad that STAYS in the graph read
    /// `GST_FLOW_FLUSHING`?
    ///
    /// That is the latch the disposal family injects: a push woken by a flush
    /// (or by a `set_state(Null)` deactivating pads under it) returns FLUSHING
    /// into `gst_multi_queue_loop`, which latches the slot's `srcresult` for
    /// good. decodebin3 then REUSES that slot for the next stream and nothing
    /// ever delivers again, and an adaptive demuxer's single output loop
    /// pauses with nothing posted. Silent in both cases, which is the whole
    /// reason for counting it.
    ///
    /// `NOT_LINKED` is expected and benign right after an unlink and is not
    /// counted. Only pads the surgery LEAVES BEHIND are worth passing here; a
    /// pad on its way out of the graph reading FLUSHING harms nobody.
    ///
    /// # INACTIVE pads are skipped, and that is not a loophole
    ///
    /// `last_flow_result` is a RECORD of the last flow return, not a live
    /// state, and gstpad writes FLUSHING into it in two places that have
    /// nothing to do with a latch: at pad construction (gstpad.c:441) and on
    /// every deactivation (gstpad.c:1143), against `GST_FLOW_OK` on every
    /// activation (1033/1128). Only `gst_pad_chain_data_unchecked` and
    /// `gst_pad_push_data` write a real flow result. An INACTIVE pad reading
    /// FLUSHING is therefore gstpad's own bookkeeping - measured, not argued:
    /// this census fired on `fpb-decodebin:text_0` with `active=false` right
    /// after a detach that raced a stop. An ACTIVE pad reading FLUSHING is a
    /// push that actually returned FLUSHING, which is the latch.
    ///
    /// # Every stage's invariant is ZERO, and it was not always so
    ///
    /// One stage used to be nonzero by construction: `dispose_text_branch`
    /// surveyed subtitleoverlay's shared `subtitle_sink`, which read `Ok`
    /// immediately BEFORE the disposal's own seat
    /// pair and `Flushing` immediately after it, on a pad that was active
    /// and unlinked, put there by the crate's own pair. It was benign (the next
    /// `poll_text_policy` link re-activated the pad and delivered) and it left
    /// with subtitleoverlay: a per-stream tail leaves with its branch, so there
    /// is no staying pad left to latch. The
    /// counter is still read PER STAGE, because a stage is a place and the
    /// total tells you nothing about which one moved.
    pub(crate) fn flow_census(stage: FlowStage, pads: &[gst::Pad]) {
        for pad in pads {
            if pad.is_active() && matches!(pad.last_flow_result(), Err(gst::FlowError::Flushing)) {
                FLOW_CENSUS_FLUSHING[stage as usize].fetch_add(1, Ordering::Relaxed);
                warn!(
                    stage = stage.name(),
                    pad = %pad.name(),
                    parent = ?pad.parent_element().map(|element| element.name().to_string()),
                    active = pad.is_active(),
                    linked = pad.is_linked(),
                    "a pad that stays in the graph reads FLUSHING after the surgery"
                );
            }
        }
    }

    /// The multiqueue SRC pad a decodebin3 output pad is ghosting, when there
    /// is one.
    ///
    /// TEXT is the case this exists for and the only one it can answer: with
    /// no decoder to autoplug, `db_output_stream_setup_decoder` takes
    /// `output->decoder_src = slot->src_pad` and ghosts the output straight at
    /// the multiqueue (gstdecodebin3.c, the `gst_caps_can_intersect (new_caps,
    /// dbin->caps)` arm), so the ghost's target IS the slot. A decoded stream
    /// ghosts its DECODER's src pad instead and this answers `None`, which is
    /// correct: the slot is then one element further up and a flush of ours
    /// never reaches it anyway.
    ///
    /// The factory check is not belt-and-braces. It is what keeps this honest
    /// across a decodebin3 that changes its mind about the geometry: if the
    /// target stops being a multiqueue pad, the un-latch below must do NOTHING
    /// rather than deactivate some unrelated element's pad.
    fn multiqueue_slot_behind(db3_src_pad: &gst::Pad) -> Option<gst::Pad> {
        let target = db3_src_pad
            .downcast_ref::<gst::GhostPad>()
            .and_then(|ghost| ghost.target())?;
        let factory = target.parent_element()?.factory()?;
        (factory.name() == "multiqueue").then_some(target)
    }

    /// THE LATCH TEST, in one place: is this multiqueue slot's SRC pad latched
    /// FLUSHING?
    ///
    /// [`Inner::slot_reads_latched`] (the read) and [`Inner::unlatch_db3_slot`]
    /// (the repair) both gate on it, and they must keep agreeing by
    /// construction: a read that says "latched" where the repair says "clean"
    /// is an instrument pointing at nothing.
    ///
    /// The `is_active` conjunct is required, never optional. gstpad writes
    /// FLUSHING into `last_flow_result` as bookkeeping at construction
    /// (gstpad.c:441) and on every deactivation (1143), so without it the read
    /// is a noise source and the repair a data destroyer; see
    /// [`Inner::flow_census`] for the measurement. Two atomic reads, no wait,
    /// safe to call from a log line.
    fn slot_pad_is_latched(slot_src: &gst::Pad) -> bool {
        slot_src.is_active() && matches!(slot_src.last_flow_result(), Err(gst::FlowError::Flushing))
    }

    /// The multiqueue SINK pad that pairs with a slot's SRC pad.
    ///
    /// multiqueue names a slot's two pads `src_%u` and `sink_%u` off one id, so
    /// the sink is derivable from the src, which is what every caps repair here
    /// needs (the fingerprint is "the sink has the caps and the src does not").
    /// `None` for a pad that is not a slot src.
    fn slot_sink_of(slot_src: &gst::Pad) -> Option<gst::Pad> {
        let element = slot_src.parent_element()?;
        let name = slot_src.name();
        let id = name.strip_prefix("src_")?;
        element.static_pad(&format!("sink_{id}"))
    }

    /// A pad's sticky inventory as `Type+Type+...`, for a log line.
    fn sticky_names(pad: &gst::Pad) -> String {
        let mut out = Vec::new();
        pad.sticky_events_foreach(|event| {
            out.push(format!("{:?}", event.type_()));
            std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
        });
        out.join("+")
    }

    /// A pad's sticky events, cloned, in the order gstpad hands them out.
    fn sticky_events(pad: &gst::Pad) -> Vec<gst::Event> {
        let mut out = Vec::new();
        pad.sticky_events_foreach(|event| {
            out.push(event.clone());
            std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
        });
        out
    }

    /// STREAM_START FIRST, but only when the slot's src pad has gone EOS: an
    /// EOS pad refuses every store (`goto eos`) and STREAM_START is the only
    /// event that clears the flag. Re-storing the pad's own STREAM_START does
    /// that and nothing else, since `gst_event_replace` with the identical
    /// pointer changes no state; the sink's is the fallback for a slot whose
    /// src never carried one.
    ///
    /// Both caps repairs need this preamble before their own store, and they
    /// need the SAME one: a slot the preamble leaves EOSed swallows the store
    /// that follows and the track stays dead with the repair in place.
    fn clear_slot_eos_for_store(slot_src: &gst::Pad, slot_sink: &gst::Pad) {
        if slot_src.pad_flags().contains(gst::PadFlags::EOS)
            && let Some(stream_start) = Self::sticky_event_of(slot_src, gst::EventType::StreamStart)
                .or_else(|| Self::sticky_event_of(slot_sink, gst::EventType::StreamStart))
            && let Err(err) = slot_src.store_sticky_event(&stream_start)
        {
            warn!(slot_pad = %slot_src.name(), ?err, "could not clear the slot's EOS flag");
        }
    }

    /// The latch READ on its own, with no repair attached: `Some(true)` if the
    /// slot behind this decodebin3 output is latched FLUSHING, `Some(false)` if
    /// it is healthy, `None` if there is no multiqueue slot to read (a decoded
    /// stream, see [`Inner::multiqueue_slot_behind`]).
    ///
    /// The same test [`Inner::unlatch_db3_slot`] gates its repair on, shared
    /// rather than copied; see [`Inner::slot_pad_is_latched`].
    pub(crate) fn slot_reads_latched(db3_src_pad: &gst::Pad) -> Option<bool> {
        let slot_src = Self::multiqueue_slot_behind(db3_src_pad)?;
        Some(Self::slot_pad_is_latched(&slot_src))
    }

    /// The whole caps path behind a routed text pad, rendered for one log line:
    /// the decodebin3 ghost, its multiqueue target, that slot's sink pad and
    /// the element feeding it, each with its mode, flags, flow result, caps and
    /// sticky inventory.
    ///
    /// Formed for the capsless escalation only (once per stream and load). It
    /// turns "the caps stop somewhere above the gate" into an address: `mqsink`
    /// carrying `caps=Some(text/x-raw) sticky=StreamStart+Caps+Segment+...`
    /// beside a `target` whose sticky set has everything EXCEPT the caps is the
    /// lost-caps fingerprint, and it is one grep rather than a 76 MB
    /// `GST_DEBUG` capture.
    pub(crate) fn caps_path_dump(db3_src_pad: &gst::Pad) -> String {
        fn describe(tag: &str, pad: &gst::Pad) -> String {
            format!(
                "{tag}[{}:{} mode={:?} flags={:?} active={} flow={:?} caps={:?} sticky={}]",
                pad.parent_element()
                    .map(|e| e.name().to_string())
                    .unwrap_or_default(),
                pad.name(),
                pad.mode(),
                pad.pad_flags(),
                pad.is_active(),
                pad.last_flow_result(),
                pad.current_caps().map(|c| c.to_string()),
                Inner::sticky_names(pad),
            )
        }
        let mut parts = vec![describe("ghost", db3_src_pad)];
        let target = db3_src_pad
            .downcast_ref::<gst::GhostPad>()
            .and_then(|ghost| ghost.target());
        match target {
            None => parts.push("target[NONE]".to_string()),
            Some(target) => {
                parts.push(describe("target", &target));
                if let Some(element) = target.parent_element() {
                    parts.push(format!(
                        "targetfactory[{}]",
                        element
                            .factory()
                            .map(|f| f.name().to_string())
                            .unwrap_or_default()
                    ));
                    if let Some(mq_sink) = Self::slot_sink_of(&target) {
                        parts.push(describe("mqsink", &mq_sink));
                        if let Some(peer) = mq_sink.peer() {
                            parts.push(describe("mqsinkpeer", &peer));
                        }
                    }
                }
            }
        }
        parts.join(" ")
    }

    /// Clear the decodebin3 multiqueue slot THIS CRATE'S OWN flush pair just
    /// latched, if it latched one.
    ///
    /// # What is being repaired
    ///
    /// Every pad `dispose_text_branch_on` flushes is BELOW decodebin3: the
    /// branch's appsink and its queue's sink. A push caught
    /// inside the FLUSH_START..STOP window returns `GST_FLOW_FLUSHING` into
    /// `gst_single_queue_push_one`, which writes it to `sq->srcresult`
    /// (gstmultiqueue.c:2498). From then on the slot's SINK chain returns that
    /// result to upstream unconditionally, on every path including success
    /// (`:2643`), and our FLUSH_STOP goes nowhere near the multiqueue's sink
    /// pad, which is the only event that would clear it. The track is dead for
    /// the rest of the item, silently, with one adaptivedemux2 warning and
    /// nothing else.
    ///
    /// On a WHOLE-PERIOD text representation that is fatal rather than
    /// cosmetic: the demuxer pushes the entire track in one push, the parser
    /// turns it into the item's whole cue stream, and the FLUSHING the latch
    /// hands back travels up through the parser to `adaptivedemux2`, which
    /// discards it. There is no second push to recover with.
    ///
    /// # Which of the two un-latches this is, and why
    ///
    /// SRC-PAD RE-ACTIVATION (`gst_multi_queue_src_activate_mode`,
    /// gstmultiqueue.c:3020-3028), not the FLUSH_STOP on the multiqueue's sink
    /// pad. Both clear `srcresult` through the same
    /// `gst_single_queue_flush (flush=FALSE)`; they differ in blast radius,
    /// and the difference is decisive:
    ///
    ///  * The sink-pad FLUSH_STOP is FORWARDED DOWNSTREAM FIRST
    ///    (`gst_pad_push_event (srcpad, event)`, `:2787`) before the slot is
    ///    touched. By the time a disposal runs, that src pad may already be
    ///    ghosting a re-linked branch, since decodebin3 recycles text outputs
    ///    in both directions, so the repair would flush the INCOMING track's
    ///    queue. It also deletes the SEGMENT sticky off the multiqueue's sink
    ///    pad (`remove_event_by_type`, gstpad.c:5919), which nothing replays,
    ///    on a pad the crate does not own.
    ///  * Re-activation pushes no event anywhere. It touches this slot and
    ///    nothing else.
    ///
    /// Measured, not argued: `multiqueue_slot_unlatch` stages the latch
    /// directly and drives both candidates through the same rig.
    ///
    /// # Gated on the latch actually being there
    ///
    /// Re-activation is not free (`gst_single_queue_flush (FALSE, full=TRUE)`
    /// drops what the slot holds and re-inits its segments) so this runs only
    /// when the slot READS latched, never speculatively. The signal is the
    /// multiqueue src pad's `last_flow_result`, which is exactly where
    /// `gst_pad_push_data` records what `push_one` got, ANDed with
    /// `is_active`: gstpad writes FLUSHING into that record on every
    /// deactivation as bookkeeping (gstpad.c:1143), and reading that as a latch
    /// is the trap [`Inner::flow_census`] documents at length.
    ///
    /// ONE PROBE, NEVER A WAIT, and the first version of this got that wrong
    /// at real cost.
    ///
    /// The latch is written on a streaming thread racing this one, so the
    /// obvious reading is that the caller should wait a little for it to land,
    /// and the disposal call site did, for 50 ms, whenever the slot came up
    /// CLEAN. That is a stall on the DECIDER, once per mid-play text disposal,
    /// on the overwhelmingly common path where there was nothing to repair.
    /// `external_subtitle_lifecycle` went from 20 passed in 2.4 s to 19 passed
    /// / 1 failed in 40 s under default parallelism, 6 runs out of 6,
    /// against 6 of 6 green with the budget at zero. The heal fired ZERO
    /// times in every one of those runs: the repair was never the cost, the
    /// waiting was.
    ///
    /// The wait was never load-bearing anyway, and
    /// [`Inner::heal_latched_text_slots`] is why: it re-reads every live text
    /// branch's slot on EVERY text poll, so "not latched yet" costs nothing but
    /// one more tick, and a slot latched after a disposal cannot deliver
    /// anything until a branch joins it, at which point the sentinel runs in
    /// the same `poll_text_policy` call that joined it. Waiting here bought at
    /// most one tick of promptness and paid for it in decider latency on every
    /// disposal that had nothing wrong with it.
    pub(crate) fn unlatch_db3_slot(db3_src_pad: &gst::Pad) {
        let Some(slot_src) = Self::multiqueue_slot_behind(db3_src_pad) else {
            return;
        };
        if !Self::slot_pad_is_latched(&slot_src) {
            SLOT_UNLATCH_CLEAN.fetch_add(1, Ordering::Relaxed);
            return;
        }
        warn!(
            db3_pad = %db3_src_pad.name(),
            slot_pad = %slot_src.name(),
            "a decodebin3 multiqueue slot is latched FLUSHING, which is a silently dead text \
             track for the rest of this item, re-activating it"
        );
        // THE STICKIES COME BACK, and leaving that out was a real defect
        // rather than a nicety.
        //
        // Deactivating a pad calls `remove_events` (gstpad.c's `post_activate`),
        // which destroys its STREAM_START, CAPS and SEGMENT. Upstream will
        // re-push them on its next buffer, for a stream that HAS a next
        // buffer. A whole-period text Representation does not: the demuxer
        // pushed the entire track once and will not push again, so a repair
        // that clears the caps trades a latched slot for a capsless one, and
        // this crate's own caps gate then refuses to build a branch on it for
        // the life of the item. Measured, not feared:
        // `sink_subtitles::a_paused_disposal_frees_the_branch_for_the_next_link`
        // went from green to "timed out waiting for a fresh consumer branch to
        // be wired while PAUSED" the moment the repair shipped without this,
        // and back to green with it.
        //
        // Captured BEFORE the deactivation, in the order gstpad hands them
        // out, and stored back after. `gst_pad_store_sticky_event` puts them
        // in the pad's own store without pushing anything downstream, which is
        // the property that made re-activation the winning candidate in the
        // first place.
        let stickies = Self::sticky_events(&slot_src);
        // Deactivate THEN activate: `gst_pad_set_active (pad, TRUE)` on a pad
        // that is already active is a no-op, so the clearing half
        // (`gst_single_queue_flush (mq, sq, FALSE, TRUE)`) is only reachable
        // through a real mode transition.
        let _ = slot_src.set_active(false);
        let reactivated = slot_src.set_active(true).is_ok();
        for event in stickies {
            if let Err(err) = slot_src.store_sticky_event(&event) {
                warn!(
                    slot_pad = %slot_src.name(),
                    event = ?event.type_(),
                    ?err,
                    "could not restore a sticky event the slot re-activation removed"
                );
            }
        }
        if reactivated && slot_src.is_active() {
            SLOT_UNLATCHES.fetch_add(1, Ordering::Relaxed);
        } else {
            SLOT_UNLATCH_FAILURES.fetch_add(1, Ordering::Relaxed);
            error!(
                slot_pad = %slot_src.name(),
                active = slot_src.is_active(),
                "re-activating a latched multiqueue slot FAILED; the text track is dead \
                 for this item"
            );
        }
    }

    /// THE SENTINEL, run for every live text branch on every text poll: is the
    /// slot above it latched?
    ///
    /// # Why this is not enough to do it at the disposal
    ///
    /// The disposal-side repair answers ONE trigger: our own flush pair caught
    /// a push. A capture at full debug rules that trigger out. The branch
    /// joined its consumer tail mid state-transition, with the pipeline
    /// still buffering at 76% and the audio sink still in READY_TO_PAUSED.
    /// The six seconds that follow contain no disposal, no flush, no policy
    /// job and no seek, and the demuxer's discard appears six seconds later
    /// still. On a whole-period text representation the demuxer pushes the
    /// entire track ONCE, so nothing crosses the slot in between: the
    /// discard is not when the track died, it is the first push through a
    /// slot that has been dead since the JOIN.
    ///
    /// The join can latch a slot with no flush anywhere near it, by the same
    /// mechanism read backwards: a pad that has never been
    /// activated is FLUSHING by construction (gstpad.c:441 sets it, activation
    /// clears it), so a link to a branch whose elements are still coming up
    /// hands FLUSHING back to the very first push. Bench-proved directly in
    /// `tests/multiqueue_slot_unlatch.rs`
    /// (`a_join_to_an_unactivated_branch_latches_the_slot_with_no_flush_at_all`),
    /// including the part that makes it lethal: the branch coming up a
    /// millisecond later does NOT revive the slot.
    ///
    /// So the repair is placed where it can see the CONSEQUENCE rather than any
    /// one cause. A latched slot is the same object however it got latched, it
    /// is cheap to read, and a poll that runs anyway is the natural carrier.
    /// This covers the join window, the disposal window, and whatever trigger
    /// nobody has met yet.
    ///
    /// # The guard, which is what makes this safe to run unconditionally
    ///
    /// A re-activation drops what the slot holds, so it must never fire on a
    /// slot that is merely passing through a flush. Three conditions, all
    /// required:
    ///
    ///  * the branch is LIVE, a `downstream` in the routing entry, i.e. this is
    ///    a joined branch and not one mid-disposal (a disposal takes the pad
    ///    out of the entry before it flushes anything) and not a PARKED one
    ///    (measured, see the scope note in the body);
    ///  * the branch is NOT flushing right now, since during a seek's flush
    ///    pair the whole branch reads flushing and the slot's record means
    ///    nothing;
    ///  * the slot's src pad is ACTIVE and records FLUSHING, the
    ///    [`Inner::flow_census`] rule, because gstpad writes FLUSHING into that
    ///    record on every deactivation as bookkeeping.
    ///
    /// Zero budget: this runs every tick, so there is nothing to wait for.
    ///
    /// # Locking
    ///
    /// The routing lock is taken to COLLECT and released before anything
    /// touches the graph, the `refusals` discipline the rest of this crate is
    /// written to. `unlatch_db3_slot` joins a multiqueue task, which is
    /// precisely the shape that must never run under a crate lock. The
    /// collecting is [`TextPadSurvey`]'s, shared with the repair below it.
    fn heal_latched_text_slots(survey: &TextPadSurvey) {
        // PARKED BRANCHES ARE DELIBERATELY NOT COVERED, and the reasoning
        // inverted once, so it is worth stating.
        //
        // A park is a link to a sink this crate owns, so it can latch a slot
        // exactly as a join can, which argued for scanning parked entries too,
        // so a park-phase latch would be healed on the next poll instead of at
        // the join. It was written and measured (10 runs per arm on the burst
        // test whose subject is cues in flight across an attach) and it lost:
        // 7 pass / 3 fail with the parked heal against 10 / 0 without it.
        // Healing a PARKED slot does the very damage this whole
        // defect is about: `gst_single_queue_flush_queue` destroys what the
        // slot holds (gstmultiqueue.c:3513-3538), and what a parked slot holds
        // is the backlog the park exists to keep. The extension was buying a
        // repair for a latch that [`Inner::retire_parking_sink`]'s PLAYING
        // barrier now prevents outright, and paying for it in exactly the
        // currency at issue.
        //
        // So the scope stays JOINED-ONLY: `downstream` in the routing entry,
        // and not flushing right now, which is `TextPadSurvey::live`.
        for pad in &survey.live {
            Self::unlatch_db3_slot(pad);
        }
    }

    /// Put back the sticky CAPS decodebin3's multiqueue destroyed while it
    /// was in flight across a flush, so a text stream that can never negotiate
    /// again gets its one chance back.
    ///
    /// # The defect this repairs
    ///
    /// A held external subtitle input seeds ONE gap so decodebin3 gives its
    /// stream a multiqueue slot ([`Inner::seed_slot_for_held_pad`]). That push
    /// carries the pad's sticky events into the brand-new slot, and they are
    /// ENQUEUED: `gst_multi_queue_sink_event` puts every serialized event on
    /// the single-queue and the slot's own loop thread drains it later
    /// (`gstmultiqueue.c:2743`). The crate's realigning replay seek is FLUSHING
    /// and travels the same pad milliseconds afterwards. If it arrives while
    /// the loop is between "popped the CAPS" and "pushed the CAPS", the loop
    /// takes `out_flushing` and the popped object is destroyed with a bare
    /// `gst_mini_object_unref` (`:2445`), outside the sticky RESCUE that
    /// `gst_single_queue_flush_queue` performs for everything still IN the
    /// queue (`:3441`, which is why STREAM_COLLECTION survives and CAPS does
    /// not).
    ///
    /// Nothing re-sends it. After the flush the source re-pushes from the
    /// origin, but `push_sticky` skips events already marked `received`
    /// (`gstpad.c`), the CAPS event on the feeding pad is one of those, and
    /// only a re-LINK (`schedule_events`) would mark it pending again. So
    /// the slot's src pad (and with it decodebin3's `text_%u` ghost, which
    /// targets it directly for a stream with no decoder) then carries
    /// STREAM_START, SEGMENT and STREAM_COLLECTION but never a CAPS, for
    /// the life of the item. The crate's own caps gate refuses to build a
    /// branch forever, which is exactly the capsless silhouette.
    ///
    /// # Why the repair is here and not on the trigger
    ///
    /// The trigger is a race between two threads inside GStreamer that this
    /// crate cannot serialise: the seeding gap must be pushed from the input's
    /// streaming thread and the replay seek must be sent from the replay lane.
    /// Delaying either one is a timing guess, and this defect already cost one
    /// retracted fix that was a timing guess (the slot-seeding latch, measured
    /// as noise at 384 runs an arm). The CONSEQUENCE is a fact about two pads
    /// that is cheap to read and unambiguous (the slot's SINK pad has the caps
    /// and its SRC pad does not) so the repair sits on the state, on a poll
    /// that runs anyway.
    ///
    /// # The guard, which is what makes this safe to run unconditionally
    ///
    /// Four conditions, all required:
    ///
    ///  * the routed stream is TEXT and has NO live branch. A joined branch
    ///    passed the caps gate to exist, so a capsless one is either impossible
    ///    or already someone else's defect, and rewriting the stickies under a
    ///    branch that is carrying cues is a blast radius with no case for it;
    ///  * the decodebin3 ghost has no caps, which is the visible symptom;
    ///  * the multiqueue slot's SRC pad has no sticky CAPS either. If it has
    ///    one the event simply has not been forwarded yet, which is the gate's
    ///    ordinary sub-millisecond transient and must not be touched;
    ///  * the slot's SINK pad HAS one, the proof that a caps really did reach
    ///    this slot and really was lost, rather than never having existed.
    ///
    /// # Zero wait, and nothing is pushed
    ///
    /// The restore is `gst_pad_store_sticky_event`, which puts events in the
    /// pad's own store WITHOUT pushing anything downstream (the property that
    /// made src-pad re-activation win over a FLUSH_STOP) and it sets the
    /// pad's `PENDING_EVENTS`, so the slot's own streaming thread delivers the
    /// corrected set on its next serialized push. This thread pushes nothing,
    /// waits for nothing and takes no stream lock.
    ///
    /// ONE PAD, and nothing is removed from it. See the store site for why an
    /// earlier version that cleared and re-stored the whole sticky set in
    /// ascending order was measurably wrong. The ghost is not touched at all:
    /// the slot's next push carries the restored caps down to it, which is also
    /// what makes the repair land in the right order relative to whatever else
    /// the slot is about to send.
    ///
    /// Counted in [`TEXT_CAPS_RESCUES`].
    fn rescue_lost_text_slot_caps(survey: &TextPadSurvey) {
        // Collected under the routing lock and repaired outside it, the
        // `refusals` discipline: the stores below take pad object locks that
        // streaming threads hold. See [`TextPadSurvey`].
        for pad in &survey.capsless_parked {
            Self::rescue_slot_caps(pad);
        }
    }

    /// [`TextPadSurvey`] taken, off one routing-lock acquisition.
    fn text_pad_survey(inner: &std::sync::Arc<Inner>) -> TextPadSurvey {
        let mut survey = TextPadSurvey {
            live: Vec::new(),
            capsless_parked: Vec::new(),
            capsful_parked: Vec::new(),
        };
        let routing = inner.routing.lock();
        for routed in routing
            .routed
            .iter()
            .filter(|routed| routed.kind == crate::routing::StreamKind::Text)
        {
            match &routed.downstream {
                Some(downstream) if !downstream.pad_flags().contains(gst::PadFlags::FLUSHING) => {
                    survey.live.push(routed.db3_src_pad.clone());
                }
                // A branch that IS flushing right now says nothing about its
                // slot, which is the second of the heal's three conditions.
                Some(_) => {}
                None if routed.db3_src_pad.current_caps().is_none() => {
                    survey.capsless_parked.push(routed.db3_src_pad.clone());
                }
                None => survey.capsful_parked.push(routed.db3_src_pad.clone()),
            }
        }
        survey
    }

    /// The two slot repairs that run back to back at the tail of
    /// [`Inner::poll_text_policy`], off ONE survey.
    ///
    /// ORDER IS PART OF THE CONTRACT: the heal first, because a branch it
    /// repairs was about to become a silent dead track and the loudest thing in
    /// the log should be the repair; the rescue after, because it only ever
    /// matters for a stream the caps gate has just refused and running it here
    /// means the very next poll is the one that joins.
    ///
    /// Safe to survey together because the two buckets are disjoint by
    /// `downstream` and the heal touches neither: `unlatch_db3_slot`
    /// re-activates a multiqueue slot's src pad and stores its stickies back,
    /// which changes no routing entry and no parked ghost's caps.
    pub(crate) fn heal_and_rescue_text_slots(inner: &std::sync::Arc<Inner>) {
        let survey = Self::text_pad_survey(inner);
        Self::heal_latched_text_slots(&survey);
        Self::rescue_lost_text_slot_caps(&survey);
    }

    /// One pad's worth of [`Inner::rescue_lost_text_slot_caps`]. Reads the
    /// preconditions and returns without touching anything when they do not
    /// hold, which is every call on a healthy item.
    fn rescue_slot_caps(db3_src_pad: &gst::Pad) {
        let Some(slot_src) = Self::multiqueue_slot_behind(db3_src_pad) else {
            return;
        };
        // A slot that HAS the caps is mid-forward, not broken.
        if slot_src.current_caps().is_some() {
            return;
        }
        let Some(slot_sink) = Self::slot_sink_of(&slot_src) else {
            return;
        };
        let Some(caps_event) = Self::sticky_caps_event(&slot_sink) else {
            // No caps ever reached this slot: not this defect, and inventing
            // one here would be the crate deciding a format it was never told.
            return;
        };
        warn!(
            db3_pad = %db3_src_pad.name(),
            slot_pad = %slot_src.name(),
            caps = %slot_sink.current_caps().map(|c| c.to_string()).unwrap_or_default(),
            "decodebin3's multiqueue lost this text stream's sticky CAPS in flight across a \
             flush, which is a track that can never negotiate again; restoring it from the \
             slot's sink pad"
        );
        // ONE STORE, AND NOTHING IS REMOVED. The first version of this cleared
        // the slot's sticky set so the caps could be re-inserted into an empty
        // array in ascending order, which is tidier and is WRONG: the clear and
        // the re-store are separate object-lock acquisitions and the slot's own
        // streaming thread pushes between them. Measured immediately, on the
        // staged test's very first run: `gstpad.c:4814/4819 Got data flow
        // before stream-start event` and `... before segment event` on the slot
        // and on the ghost, a real protocol hole traded for a
        // cosmetic one. `store_sticky_event` inserts at the right index by
        // itself (`gstpad.c`, the `sticky_order` walk), so the array comes out
        // correctly ordered either way and the hole buys nothing.
        Self::clear_slot_eos_for_store(&slot_src, &slot_sink);
        if let Err(err) = slot_src.store_sticky_event(&caps_event) {
            warn!(
                slot_pad = %slot_src.name(),
                ?err,
                "could not restore the lost text CAPS onto the multiqueue slot"
            );
        }
        // EXPECT A `g_warning` PER RESCUE, measured as exactly one on the staged
        // test: `store_sticky_event` reports "Sticky event misordering, got
        // 'segment' before 'caps'" whenever a caps lands on a pad that already
        // carries a SEGMENT (`gstpad.c:5491`), and it still inserts the event at
        // its right index. That is upstream describing the damage the caps loss
        // did rather than damage this repair does (the caps really did arrive
        // after the segment, because multiqueue destroyed it) and it is
        // accepted deliberately in exchange for never opening a window in which
        // a pad has no segment at all. Nothing in this tree runs with
        // `G_DEBUG=fatal-warnings`. If that ever changes, this is the line that
        // has to be revisited, not the counter.
        //
        // Checked, not assumed, and for the same reason [`SLOT_UNLATCHES`] is
        // split from [`SLOT_UNLATCH_FAILURES`]: the counter must mean the track
        // is alive again, or a green dashboard would outlive a dead subtitle.
        if slot_src.current_caps().is_some() {
            TEXT_CAPS_RESCUES.fetch_add(1, Ordering::Relaxed);
        } else {
            TEXT_CAPS_RESCUE_FAILURES.fetch_add(1, Ordering::Relaxed);
            error!(
                slot_pad = %slot_src.name(),
                "restoring a lost text CAPS onto a multiqueue slot FAILED; the text track is \
                 dead for this item"
            );
        }
    }

    /// The sticky event of `type_` on `pad`, as a plain event. See
    /// [`Inner::sticky_caps_event`] for why these are taken verbatim.
    fn sticky_event_of(pad: &gst::Pad, type_: gst::EventType) -> Option<gst::Event> {
        let mut found = None;
        pad.sticky_events_foreach(|event| {
            if event.type_() == type_ {
                found = Some(event.clone());
            }
            std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
        });
        found
    }

    /// TEST FAULT INJECTION (see `TestStaging::text_caps_loss`): destroy one
    /// parked text stream's sticky CAPS on the decodebin3 ghost and on the
    /// multiqueue slot behind it, leaving the slot's SINK pad holding it.
    ///
    /// That is the state a lost caps leaves and nothing else about it is
    /// simulated: the pads afterwards are the same objects in the same graph,
    /// with exactly the captured sticky set
    /// (`sticky=StreamStart+Segment+StreamCollection` on both, caps on the slot
    /// sink). One shot: it disarms itself once it has landed on a stream, so a
    /// test stages a single loss rather than a permanent one and the recovery
    /// is what is being measured.
    pub(crate) fn stage_text_caps_loss(inner: &std::sync::Arc<Inner>) {
        if !inner.stage_text_caps_loss_armed() {
            return;
        }
        // Runs far ahead of the repairs, before the gate rather than after it,
        // so it takes its own survey; see the call site for why the staging has
        // to land on the poll BEFORE the one that observes the loss.
        for pad in Self::text_pad_survey(inner).capsful_parked {
            let Some(slot_src) = Self::multiqueue_slot_behind(&pad) else {
                continue;
            };
            let strip = |pad: &gst::Pad| {
                pad.sticky_events_foreach(|event| {
                    std::ops::ControlFlow::Continue(if event.type_() == gst::EventType::Caps {
                        gst::EventForeachAction::Remove
                    } else {
                        gst::EventForeachAction::Keep
                    })
                });
            };
            strip(&pad);
            strip(&slot_src);
            warn!(
                pad = %pad.name(),
                slot_pad = %slot_src.name(),
                "TEST STAGING: destroying a parked text stream's sticky CAPS"
            );
            inner.stage_disarm_text_caps_loss();
            return;
        }
    }

    /// The sticky CAPS event on `pad`, as a plain event so it can be stored on
    /// another pad verbatim (a rebuilt one would be a different caps).
    fn sticky_caps_event(pad: &gst::Pad) -> Option<gst::Event> {
        Self::sticky_event_of(pad, gst::EventType::Caps)
    }

    /// Send the flush pair to `pads`. Callers must already have dropped the
    /// routing lock (see [`Inner::live_text_downstream_pads`]).
    pub(crate) fn flush_pads(pads: &[gst::Pad], reason: FlushReason) {
        for pad in pads {
            Self::send_flush_pair(pad, reason);
        }
    }

    /// [`Inner::flush_pads`] plus the SEGMENT the pair takes away. decodebin3
    /// SINK pads at a TEARDOWN only, see SCOPE.
    ///
    /// `FLUSH_STOP` deletes the pad's SEGMENT sticky (gstpad.c
    /// `remove_event_by_type`) and nothing re-arms it: `check_sticky` needs
    /// `PENDING_EVENTS`, which only `schedule_events` sets and no flush
    /// reaches. A flush from UPSTREAM costs nothing because it is a seek
    /// and the source re-segments; an injected pair has no such follow-up,
    /// so a straggler buffer chains segmentless. Replaying the captured
    /// sticky (not a rebuilt one, so the timeline is untouched) is the only
    /// fix: marking events pending cannot work, `push_sticky` skips
    /// anything already `received`. The pair is unchanged, so
    /// `flush_pairs_matched` still holds.
    ///
    /// SCOPE: widening this was measured WRONG. Replaying on every flushed pad
    /// regressed `external_subtitle_lifecycle` to 16 passed / 3 failed in 127 s
    /// against 19 passed in 8 s, all three on "no FSTA cue reached the
    /// renderer": a restored segment is stale where a branch is about to be
    /// re-linked or released. Text pads, `remove_input` and
    /// `dispose_text_branch_on` keep the bare pair.
    ///
    /// # RESIDUAL: the restore is not atomic with the pair, and the field has
    /// caught it
    ///
    /// `FLUSH_STOP` re-arms the pad and the upstream streaming thread can push
    /// into it before the `send_event` below runs.
    /// `dash-embedded-subs-delayed.txt` is that happening: eight pads restored
    /// one after another over 1.3 ms with adaptivedemux2 warning "Discarding
    /// data on video_00/audio_00: downstream returned FLUSHING while this
    /// element is not flushing" throughout, and between the pair on `sink_7`
    /// and its restore line the audio sink logged "Internal data flow problem"
    /// plus "Received buffer without a new-segment. Assuming timestamps start
    /// from 0" (gstbasesink.c:3799-3816). Once a buffer is through, the
    /// restored SEGMENT is behind it in every queue on the way down.
    ///
    /// The fix is to send the FLUSH_STOP and the SEGMENT under the pad's own
    /// STREAM LOCK (taken AFTER the FLUSH_START, which is what makes it
    /// obtainable). It was written and then NOT shipped, because it could not
    /// be bite-proven: every caller of this is a TEARDOWN
    /// ([`Teardown::run`], [`Inner::flush_parked_text_pushes`]), the sources
    /// stop pushing immediately after, and `segment_sticky_census`'s
    /// stop-driven rig therefore shows an empty window whether the restore is
    /// present or absent (measured: the ordering assertion passes with the
    /// restore taken out too). Taking a blocking stream lock on the
    /// teardown path is exactly the shape three fuzz seeds have wedged on
    /// before, and it is not worth doing unproven. What DID ship is the
    /// `reset_time = FALSE` half in [`Inner::send_flush_pair`], which is the
    /// sink-side damage and is provable.
    pub(crate) fn flush_db3_sink_pads(pads: &[gst::Pad]) {
        for pad in pads {
            // Read BEFORE the FLUSH_START. After the pair the pad has no
            // segment left to read.
            let segment = pad.sticky_event::<gst::event::Segment>(0);
            Self::send_flush_pair(pad, FlushReason::TeardownDb3);
            if let Some(segment) = segment {
                debug!(pad = %pad.name(), "restoring the segment the flush pair removed");
                let _ = pad.send_event(segment);
            }
        }
    }
}

impl FcastPlaybin {
    /// How many flush pairs THIS CRATE has sent for ONE reason, by the name
    /// [`FlushReason::name`] gives it. Panics on an unknown name rather than
    /// answering zero: a typo in an assertion that silently passes is worse
    /// than a test that fails loudly. Not part of the public API.
    ///
    /// # These are associated functions, not methods
    ///
    /// The counters they read are process-global (see [`FLUSH_PAIRS`]): the
    /// teardown pairs are sent after `Inner` is gone, so there is no handle
    /// left to call a method on at the moment they matter most. A test that
    /// drops its playbin to force a teardown reads them as
    /// `FcastPlaybin::crate_flush_pairs_for("teardown_db3")`. The consequence
    /// to keep in mind is that a test binary's counts are CUMULATIVE across its
    /// tests, so useful assertions are "this reason fired" and "this reason
    /// never fires", never "this reason fired exactly twice".
    ///
    /// PER REASON, never a whole-sum: a total over all seven reasons cannot say
    /// which one moved, which is the only question this census exists to
    /// answer. [`FcastPlaybin::crate_flush_pair_breakdown`] is the "everything"
    /// reader, and it keeps the attribution.
    #[doc(hidden)]
    pub fn crate_flush_pairs_for(reason: &str) -> u64 {
        match FlushReason::ALL.iter().find(|kind| kind.name() == reason) {
            Some(kind) => FLUSH_PAIRS[*kind as usize].load(Ordering::Relaxed),
            None => panic!(
                "unknown flush reason {reason:?}; known reasons: {:?}",
                FlushReason::ALL.map(FlushReason::name)
            ),
        }
    }

    /// Every reason and its count, for a failure message. Not part of the
    /// public API.
    #[doc(hidden)]
    pub fn crate_flush_pair_breakdown() -> Vec<(&'static str, u64)> {
        FlushReason::ALL
            .iter()
            .map(|kind| {
                (
                    kind.name(),
                    FLUSH_PAIRS[*kind as usize].load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// [`Inner::unlatch_db3_slot`] against a pad a TEST built, so the probe
    /// that stages the latch drives the SHIPPED repair rather than a copy of
    /// it. Not part of the public API.
    #[doc(hidden)]
    pub fn unlatch_db3_slot_for_test(db3_src_pad: &gst::Pad) {
        Inner::unlatch_db3_slot(db3_src_pad);
    }

    /// [`Inner::retire_parking_sink`] against a sink a TEST added to its own
    /// pipeline, so `multiqueue_slot_unlatch`'s park rig drives the SHIPPED
    /// unpark rather than a copy of it. Not part of the public API.
    #[doc(hidden)]
    pub fn retire_parking_sink_for_test(sink: &gst::Element) {
        Inner::retire_parking_sink(sink);
    }

    /// [`Inner::slot_reads_latched`] for a test that stages a latch and needs
    /// to say so without repairing it. Not part of the public API.
    #[doc(hidden)]
    pub fn slot_reads_latched_for_test(db3_src_pad: &gst::Pad) -> Option<bool> {
        Inner::slot_reads_latched(db3_src_pad)
    }

    /// Pads that stayed in the graph reading FLUSHING after a pad surgery, for
    /// ONE stage, by the name [`FlowStage::name`] gives it (see
    /// [`Inner::flow_census`]). Panics on an unknown name. Not part of the
    /// public API.
    ///
    /// Every remaining stage's invariant is ZERO. The one that was nonzero by
    /// construction, `dispose_text_branch`, was subtitleoverlay's shared
    /// `subtitle_sink` and died with it.
    ///
    /// PER STAGE, never a whole-sum: a stage is a PLACE and the total tells you
    /// nothing about which one moved, the point [`Inner::flow_census`] makes at
    /// its own tail. [`FcastPlaybin::flow_census_breakdown`] is the
    /// "everything" reader, and it keeps the attribution.
    #[doc(hidden)]
    pub fn flow_census_flushing_for(stage: &str) -> u64 {
        match FlowStage::ALL.iter().find(|kind| kind.name() == stage) {
            Some(kind) => FLOW_CENSUS_FLUSHING[*kind as usize].load(Ordering::Relaxed),
            None => panic!(
                "unknown flow census stage {stage:?}; known stages: {:?}",
                FlowStage::ALL.map(FlowStage::name)
            ),
        }
    }

    /// Every stage and its count, for a failure message. Not part of the
    /// public API.
    #[doc(hidden)]
    pub fn flow_census_breakdown() -> Vec<(&'static str, u64)> {
        FlowStage::ALL
            .iter()
            .map(|kind| {
                (
                    kind.name(),
                    FLOW_CENSUS_FLUSHING[*kind as usize].load(Ordering::Relaxed),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{FlowStage, FlushReason};
    use crate::FcastPlaybin;

    /// Every census name pinned to its variant and every variant to its
    /// counter slot in ALL. The by-name accessors panic on an unknown name,
    /// so with the literals held here a rename is a red test, never an
    /// assertion silently reading zero from the wrong counter.
    #[test]
    fn flush_reason_names_and_counter_slots_round_trip() {
        for reason in FlushReason::ALL {
            // Exhaustive on purpose: a new reason fails to compile here
            // until it is named, ordered into ALL and counted.
            let name = match reason {
                FlushReason::TeardownText => "teardown_text",
                FlushReason::TeardownDb3 => "teardown_db3",
                FlushReason::DisposalQueue => "disposal_queue",
                FlushReason::TeardownQueue => "teardown_queue",
                FlushReason::RemoveInput => "remove_input",
                FlushReason::DisposalConsumer => "disposal_consumer",
                FlushReason::TeardownConsumer => "teardown_consumer",
            };
            assert_eq!(reason.name(), name);
            // `reason as usize` indexes FLUSH_PAIRS, so ALL must list the
            // variants in declaration order or counts land on the wrong row.
            assert_eq!(FlushReason::ALL[reason as usize], reason, "{name} slot");
            // The resolver a test asserts through reaches the same counter.
            let _ = FcastPlaybin::crate_flush_pairs_for(name);
        }
        let names = FlushReason::ALL.map(FlushReason::name);
        for (i, name) in names.iter().enumerate() {
            assert!(
                !names[..i].contains(name),
                "duplicate census name {name:?} merges two reasons"
            );
        }
    }

    /// The same pin for the flow census stages.
    #[test]
    fn flow_stage_names_and_counter_slots_round_trip() {
        for stage in FlowStage::ALL {
            let name = match stage {
                FlowStage::DetachTextParts => "detach_text_parts",
                FlowStage::RemoveInput => "remove_input",
                FlowStage::RemoveVideoChain => "remove_video_chain",
                FlowStage::EnsureVideoChain => "ensure_video_chain",
            };
            assert_eq!(stage.name(), name);
            assert_eq!(FlowStage::ALL[stage as usize], stage, "{name} slot");
            let _ = FcastPlaybin::flow_census_flushing_for(name);
        }
        let names = FlowStage::ALL.map(FlowStage::name);
        for (i, name) in names.iter().enumerate() {
            assert!(
                !names[..i].contains(name),
                "duplicate census name {name:?} merges two stages"
            );
        }
    }
}
