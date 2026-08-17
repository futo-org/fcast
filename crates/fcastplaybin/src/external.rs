//! External subtitle inputs: attach, detach, and the replay/reclaim
//! machinery that lands them on the playing timeline.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use gst::prelude::*;
use tracing::{debug, error, info, warn};

use crate::{
    FcastPlaybin, Inner,
    api::{ExternalSubId, PlaybinEvent},
    decisions, hands,
    hands::{Effect, Outcome},
    jobs::{Job, QueuedJob, ReplayJob, TimerJob},
    routing::{Input, StreamKind},
    teardown::StartTimeGuard,
};

/// How long an attached external subtitle input may take to produce its stream
/// before it is failed.
pub(crate) const EXTERNAL_SUB_TIMEOUT: Duration = Duration::from_secs(5);

/// How long after a replay its verification fires, and how many replays a
/// single trigger may issue (see `Job::VerifyReplay`).
pub(crate) const REPLAY_VERIFY_AFTER: Duration = Duration::from_millis(400);

const REPLAY_ATTEMPTS: u32 = 3;

/// How many times an external input that died before anything of it reached
/// decodebin3 is re-attached (see [`Job::RetrySub`]). A genuinely bad URL
/// exhausts these near-instantly and the materialization watchdog delivers
/// the verdict.
const MAX_ATTACH_RETRIES: u32 = 3;

/// External-subtitle bookkeeping for an [`Input`] (`None` for the main
/// input).
pub(crate) struct ExternalInput {
    pub(crate) id: ExternalSubId,
    /// The subtitle URI, kept for the never-linked attach retry (see
    /// `FcastPlaybin::retry_subtitle`).
    uri: String,
    /// Bumped per attach retry. Queued fail/check/retry/replay jobs carry
    /// the epoch they were decided against and no-op on a mismatch, so a
    /// stale job can never act on a different incarnation of the id.
    pub(crate) epoch: u32,
    /// The input's source task died deselected (its push hit the unlinked
    /// slot). Nothing more will EVER arrive from it, so a selection moving
    /// back onto it must replay eagerly: stored stickies drain from the
    /// slot and can look like delivery, but no cue follows. Set by the
    /// error classification's recover path, cleared by every replay.
    pub(crate) task_dead: bool,
    /// The timeline origin the input last had a seek applied for: ZERO at
    /// attach (a file plays from its start), updated by every replay and
    /// forwarded seek. The selection-time replay compares it against the
    /// video's origin: a mismatch means the switched-to cues WILL render
    /// shifted, so only then is the destructive replay justified.
    pub(crate) last_origin: gst::ClockTime,
    /// Block this input's buffers at its source pads until a selection
    /// naming its stream applies.
    ///
    /// An external input's buffers may only flow once decodebin3 has given
    /// its stream a multiqueue slot WITH an output, i.e. once the stream is
    /// selected. Pushing earlier dies not-linked (the slot's source pad has
    /// nothing behind it and multiqueue relays that upstream), taking the
    /// whole input down. Nothing about attaching makes selection win that
    /// race: decodebin3 only learns the stream from the events the first
    /// push carries, so the first push is inherently too early whenever the
    /// stream is not auto-selected (another text stream already is) or the
    /// caller attached it unselected.
    ///
    /// Held, the input survives indefinitely: its sticky events reach
    /// decodebin3, one seeded GAP gets the stream slotted (see
    /// [`Inner::seed_slot_for_held_pad`]) and therefore selectable, and the
    /// buffers follow once `STREAMS_SELECTED` confirms the selection (see
    /// [`Inner::unblock_selected_externals`]).
    pub(crate) hold_until_selected: bool,
    /// The selection that lifted `hold_until_selected` also owes this input a
    /// realigning replay, so its block probes are still installed and only
    /// that replay's OUTCOME may remove them (see
    /// [`Inner::release_owed_hold`], which names the three paths an outcome
    /// can arrive by).
    hold_release_owed: bool,
    /// A replay's flushing seek is HANDED OFF for this input and has not
    /// settled yet.
    ///
    /// Distinct from [`Inner::replay_inflight`], and the field defect is the
    /// difference: that bit is set by the EMITTERS, before `Job::ReplaySub` is
    /// queued, so it is already set by the time `replay_subtitle` runs and
    /// cannot tell "my own job" from "somebody else's replay". This flag is
    /// set at the hand-off itself, so it answers the only question the choke
    /// point has: is a seek for this input already out? Two triggers racing
    /// (the join-time one and the selection-time one) queue two jobs that both
    /// pass the emitters' guard, and both have been performed in practice: two
    /// flushing seeks 1.1 ms apart, and two whole-file redeliveries behind
    /// them.
    ///
    /// Lives ON the resource so it cannot orphan: an epoch bump builds a new
    /// [`ExternalInput`] and a detach drops it. Cleared by
    /// [`FcastPlaybin::replay_outcome`] and by the lane fallback, the same two
    /// paths that discharge `replay_inflight`.
    replay_seek_outstanding: bool,
    /// [`Inner::external_cues_fed`] read at the last replay hand-off, so
    /// `verify_replay` can require the chain to have actually delivered a
    /// cue rather than concluding on segment alignment alone.
    fed_baseline: u64,
}

/// Replay flushing seeks actually HANDED OFF (to the replay lane, or run
/// inline under the levers) by [`FcastPlaybin::replay_subtitle`].
///
/// `RECONCILE_EMITS` counts what the pass decided; this counts what reached
/// the graph, from every emitter. A test that wants "exactly one replay per
/// divergence" has to read this one: an emit that turned into two seeks, or a
/// second emitter slipping one in beside the pass, is invisible in the
/// decision count.
static REPLAY_SEEKS_SENT: AtomicU64 = AtomicU64::new(0);

/// `Job::ReplaySub` jobs that reached the worker queue.
///
/// The counter the ENQUEUE guard is visible in, and the one
/// `REPLAY_SEEKS_SENT` cannot stand in for. `replay_subtitle`'s choke point
/// (`ExternalInput::replay_seek_outstanding`) can only collapse a trigger
/// while a seek is TRAVELLING, so a second job queued behind the first is
/// invisible in the seek count until the first outcome lands early enough to
/// let it through - which is exactly the field ordering in
/// `subtitle-reenable-freeze.txt` and exactly what a test cannot schedule.
/// Counting the QUEUE makes the guard testable without racing it.
pub(crate) static REPLAY_JOBS_QUEUED: AtomicU64 = AtomicU64::new(0);

/// External inputs that refused a FORWARDED seek
/// ([`Inner::forward_seek_to_live_externals`]).
///
/// A refusal means the input stayed on the old timeline while the video moved,
/// so every cue it delivers from now on renders shifted. It was silent apart
/// from a per-pad `warn!`; the count is what makes "did this happen" a
/// question a test and a field log can both answer.
static FORWARD_SEEK_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Slot-seeding GAPs pushed and refused for a held external input (see
/// [`Inner::seed_slot_for_held_pad`]). Read as deltas.
///
/// A refusal used to be terminal: the seeding was latched BEFORE the push, so
/// one refused GAP left the stream with no decodebin3 multiqueue slot for the
/// life of the item - every later buffer returning not-linked and killing the
/// source task, decodebin3's output pad never carrying a sticky CAPS, and the
/// text link loop's caps gate refusing the join forever while the selection
/// read as confirmed. The pair is what makes "was a seeding refused, and was it
/// retried" a question a test and a field log can both answer.
static SLOT_SEED_PUSHES: AtomicU64 = AtomicU64::new(0);
static SLOT_SEED_REFUSALS: AtomicU64 = AtomicU64::new(0);

impl Inner {
    /// Decide what a bus error from a live external subtitle input means,
    /// by its cause (see [`decisions::external_error_action`]). A
    /// transport-race death recovers in place: the join-time replay
    /// restarts the task, or the never-linked retry re-attaches. Anything
    /// else is detached and reported as
    /// [`PlaybinEvent::ExternalSubtitleFailed`]. Runs on the posting
    /// (streaming) thread, so it only queues worker follow-up.
    pub(crate) fn handle_external_error(
        &self,
        id: ExternalSubId,
        error: &gst::glib::Error,
        debug_info: Option<gst::glib::GString>,
    ) {
        let (epoch, never_linked) = {
            let routing = self.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| i.is_external(id)) else {
                debug!(?id, "error from an already-detached external input");
                return;
            };
            (
                input.external.as_ref().expect("external input").epoch,
                input.stream_ids().is_empty() && input.db3_sink_pads.is_empty(),
            )
        };

        use decisions::ExternalErrorAction as Action;
        match decisions::external_error_action(debug_info.as_deref()) {
            Action::Fail => {
                warn!(?id, %error, ?debug_info, "external subtitle input failed");
                self.queue_job(Job::FailSub { id, epoch });
            }
            // A linked input recovers through the join-time replay. One
            // that died before anything of it reached decodebin3 has
            // nothing to select, so the replay can never run: re-attach
            // it (safe exactly here, see `retry_subtitle`), a bounded
            // number of times.
            Action::Recover if never_linked && epoch < MAX_ATTACH_RETRIES => {
                info!(?id, %error, epoch, "the input died before reaching decodebin3; retrying the attach");
                self.queue_job(Job::RetrySub { id, epoch });
            }
            Action::Recover if never_linked => {
                debug!(?id, %error, "the input keeps dying unlinked; the watchdog owns the verdict");
            }
            Action::Recover => {
                debug!(?id, %error, "a transport race killed the input's task; the next replay restarts it");
                let mut routing = self.routing.lock();
                if let Some(external) = routing
                    .inputs
                    .iter_mut()
                    .filter_map(|input| input.external.as_mut())
                    .find(|external| external.id == id && external.epoch == epoch)
                {
                    external.task_dead = true;
                }
            }
        }
    }

    /// Whether a bus message originates inside an external subtitle input.
    /// Such inputs post their own PARTIAL stream collections straight to the
    /// bus (they are siblings of decodebin3, nothing aggregates them), and
    /// those must not be mistaken for the pipeline-wide collection. NOT
    /// applied to the main input: some media (plain mp3) only ever gets a
    /// collection message from the main input's parsebin, never decodebin3.
    pub(crate) fn message_from_external_input(&self, msg: &gst::Message) -> bool {
        let Some(src) = msg.src() else {
            return false;
        };
        let routing = self.routing.lock();
        routing
            .inputs
            .iter()
            .filter(|i| i.external.is_some())
            .any(|i| {
                src == i.element.upcast_ref::<gst::Object>() || src.has_as_ancestor(&i.element)
            })
    }

    /// Install the hold-until-selected block on one source pad of a held
    /// external input (see [`ExternalInput::hold_until_selected`]).
    /// Serialized events pass, so the stream's sticky events reach
    /// decodebin3 and it stays advertised; buffers hold until
    /// [`Inner::unblock_selected_externals`] removes the probe. A no-op for
    /// every other input.
    ///
    /// Events alone leave the stream advertised but NOT selectable, so the
    /// first held buffer also seeds one GAP (see
    /// [`Inner::seed_slot_for_held_pad`]).
    pub(crate) fn block_held_external_pad(
        inner: &Arc<Inner>,
        element: &gst::Element,
        pad: &gst::Pad,
    ) {
        {
            let routing = inner.routing.lock();
            let held = routing.inputs.iter().any(|i| {
                i.element == *element && i.external.as_ref().is_some_and(|e| e.hold_until_selected)
            });
            if !held {
                return;
            }
        }
        let seeded = AtomicBool::new(false);
        let probe = pad.add_probe(
            gst::PadProbeType::BLOCK
                | gst::PadProbeType::BUFFER
                | gst::PadProbeType::BUFFER_LIST
                | gst::PadProbeType::EVENT_DOWNSTREAM
                // FLUSH too, and only to re-arm the seeding below. The
                // realigning replay's seek is FLUSHING and travels this very
                // pad, so it is the one thing that can eat the seeding GAP.
                | gst::PadProbeType::EVENT_FLUSH,
            move |pad, info| {
                // Every event passes, GAP included: decodebin3 needs one to
                // give the stream a multiqueue slot, and the stream must be
                // slotted to be selectable at all.
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    // RE-ARM. A flush ends whatever seeding was in flight
                    // across it, and it is the reason a seeding can be
                    // refused at all: `pad.push_event` returns false on a pad
                    // that went flushing under the push. Forgetting that the
                    // GAP is still owed is what left an external permanently
                    // slotless (see [`Inner::seed_slot_for_held_pad`] for
                    // what that costs).
                    if event.type_() == gst::EventType::FlushStop {
                        seeded.store(false, Ordering::Relaxed);
                        return gst::PadProbeReturn::Pass;
                    }
                    // A cue-less subtitle reaches EOS without ever pushing a
                    // buffer. Seed off its EOS instead, BEFORE forwarding it:
                    // nothing may be pushed afterwards, and an unslotted
                    // stream would stay unselectable for good.
                    if event.type_() == gst::EventType::Eos
                        && !seeded.load(Ordering::Relaxed)
                        && Inner::seed_slot_for_held_pad(pad, None)
                    {
                        seeded.store(true, Ordering::Relaxed);
                    }
                    return gst::PadProbeReturn::Pass;
                }
                // A buffer means the sticky events have reached decodebin3
                // (`check_sticky` runs ahead of block probes) and this is the
                // input's streaming thread, parked here for as long as the
                // hold lasts: the one safe moment to seed the slot.
                //
                // LATCHED ON SUCCESS ONLY. This used to `swap(true)` before
                // the push and so recorded a seeding that never happened; one
                // refusal then left the stream slotless for the life of the
                // item, with every later buffer returning not-linked.
                if !seeded.load(Ordering::Relaxed) {
                    let pts = match &info.data {
                        Some(gst::PadProbeData::Buffer(buffer)) => buffer.pts(),
                        Some(gst::PadProbeData::BufferList(list)) => {
                            list.get(0).and_then(|b| b.pts())
                        }
                        _ => None,
                    };
                    if Inner::seed_slot_for_held_pad(pad, pts) {
                        seeded.store(true, Ordering::Relaxed);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
        let Some(probe) = probe else { return };
        debug!(pad = %pad.name(), "holding an external input's data until selected");
        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.block_probes.push((pad.clone(), probe));
        } else {
            drop(routing);
            pad.remove_probe(probe);
        }
    }

    /// Give a held external input's stream a decodebin3 multiqueue slot by
    /// pushing one GAP down the (blocked) pad.
    ///
    /// Without this a held stream is advertised but permanently
    /// UNSELECTABLE, and that deadlocks the hold. decodebin3 links an input
    /// stream to a slot in exactly three places: the STREAM_START handler on
    /// the input stream's own source pad, the first buffer's block probe, and
    /// the GAP handler. The first one cannot fire for a pre-parsed input
    /// (`urisourcebin parse-streams=true`): decodebin3 builds the input's
    /// `identity` only once the STREAM_COLLECTION arrives and seeds the input
    /// stream's `active_stream` from the sink pad's sticky STREAM_START, so
    /// when it then replays that STREAM_START the handler sees no change and
    /// skips the linking. The second is what the hold exists to prevent (a
    /// buffer pushed into a slotless or output-less stream returns
    /// not-linked and kills the source). That leaves the GAP.
    ///
    /// A stream with no slot is worse than merely unselectable: it can never
    /// be `all_streams_present`, so no collection containing it becomes
    /// decodebin3's output collection, and every SELECT_STREAMS naming it is
    /// then silently discarded (`handle_select_streams` binds the request to
    /// the collection and only switches for the output one). No selection
    /// applies, no `STREAMS_SELECTED` is posted, and the hold, which only
    /// [`Inner::unblock_selected_externals`] lifts, never lifts.
    ///
    /// The GAP is pushed from the block probe on purpose: on the input's
    /// streaming thread (serialized events must not come from anywhere else)
    /// and after the sticky events have reached decodebin3, so the slot link
    /// happens in the probe and the post-probe sticky re-push carries
    /// STREAM_START into the fresh slot, which is what finally marks the
    /// stream present.
    /// Returns whether the GAP was TAKEN. A refusal is transient and
    /// recoverable, and the caller must not record a seeding that did not
    /// happen (see the `seeded` latch in
    /// [`Inner::block_held_external_pad`]).
    ///
    /// Lever: `FCAST_FORCE_SLOT_SEED_REFUSAL` makes every push read as
    /// refused. The refusal comes from the pad going FLUSHING under the push
    /// - the realigning replay's own seek - which is a window no test can hit
    /// on demand, so the RECOVERY gets a lever rather than going unpinned
    /// (the same shape as `FCAST_FORCE_FORWARD_SEEK_REFUSAL`).
    ///
    /// The "held" in the name is historical. `Inner::link_input_pad` seeds the
    /// MAIN input's data-less streams the same way, for the same reason
    /// (UPSTREAM-GSTREAMER-ISSUES.md C15).
    pub(crate) fn seed_slot_for_held_pad(pad: &gst::Pad, pts: Option<gst::ClockTime>) -> bool {
        // Zero duration: this announces no missing content, it only gives
        // decodebin3 a data-like event to react to. A cue may legitimately
        // start at the same instant.
        let gap = gst::event::Gap::builder(pts.unwrap_or(gst::ClockTime::ZERO))
            .duration(gst::ClockTime::ZERO)
            .build();
        debug!(pad = %pad.name(), ?pts, "seeding a decodebin3 slot for the held external stream");
        SLOT_SEED_PUSHES.fetch_add(1, Ordering::SeqCst);
        let forced = std::env::var_os("FCAST_FORCE_SLOT_SEED_REFUSAL").is_some();
        if forced || !pad.push_event(gap) {
            SLOT_SEED_REFUSALS.fetch_add(1, Ordering::SeqCst);
            warn!(pad = %pad.name(), "the held external input refused the slot-seeding gap");
            return false;
        }
        true
    }

    /// Release the hold-until-selected blocks of every external input whose
    /// stream a just-applied selection names (see
    /// [`ExternalInput::hold_until_selected`]). Once decodebin3 confirmed
    /// the stream selected, the flowing buffers reach the stream's multiqueue
    /// slot, which now has an output, and the subtitle plays.
    ///
    /// `owed` names the one input, if any, whose realigning replay was queued
    /// by the same `STREAMS_SELECTED`. Its hold is NOT released here; the
    /// replay owes it (see [`Inner::release_owed_hold`]).
    pub(crate) fn unblock_selected_externals(
        &self,
        selected_ids: &[String],
        owed: Option<(ExternalSubId, u32)>,
    ) {
        let to_unblock: Vec<(gst::Pad, gst::PadProbeId)> = {
            let mut routing = self.routing.lock();
            let mut probes = Vec::new();
            for input in routing.inputs.iter_mut() {
                let held = input
                    .external
                    .as_ref()
                    .is_some_and(|e| e.hold_until_selected);
                if !held || input.block_probes.is_empty() {
                    continue;
                }
                let sids = input.stream_ids();
                if !sids.iter().any(|sid| selected_ids.iter().any(|s| s == sid)) {
                    continue;
                }
                let Some(external) = input.external.as_mut() else {
                    continue;
                };
                // Selected, so the hold's own condition is discharged either
                // way. What the owed replay holds back is only the PROBES.
                external.hold_until_selected = false;
                if owed == Some((external.id, external.epoch)) {
                    external.hold_release_owed = true;
                    continue;
                }
                probes.append(&mut input.block_probes);
            }
            probes
        };
        for (pad, probe) in to_unblock {
            debug!(pad = %pad.name(), "releasing a selected external input's data hold");
            pad.remove_probe(probe);
        }
    }

    /// Put every text branch of external `(id, epoch)` back on offset 0, which
    /// is what a replay to the video's origin makes correct (see the call in
    /// [`FcastPlaybin::replay_subtitle`]).
    ///
    /// Takes the routing guard rather than the lock, since both callers already
    /// hold it and the second across the whole alignment loop.
    pub(crate) fn zero_text_offsets_for(
        routing: &crate::routing::RoutingState,
        id: ExternalSubId,
        epoch: u32,
    ) {
        let sids: Vec<String> = routing
            .inputs
            .iter()
            .find(|input| {
                input
                    .external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            })
            .map(|input| input.stream_ids())
            .unwrap_or_default();
        for routed in routing.routed.iter().filter(|r| r.kind == StreamKind::Text) {
            let carries = routed
                .db3_src_pad
                .stream_id()
                .is_some_and(|sid| sids.contains(&sid.to_string()));
            if carries && routed.db3_src_pad.offset() != 0 {
                debug!(
                    pad = %routed.db3_src_pad.name(),
                    previous = routed.db3_src_pad.offset(),
                    "clearing a text branch's pad offset for its realigning replay"
                );
                routed.db3_src_pad.set_offset(0);
            }
        }
    }

    /// The stream ids of every external whose realigning replay seek is still
    /// out, so the alignment pass can leave those branches alone (see
    /// [`Inner::sync_text_running_time`]). Answered here because
    /// `ExternalInput`'s fields are private to this module.
    pub(crate) fn sids_awaiting_replay(routing: &crate::routing::RoutingState) -> Vec<String> {
        routing
            .inputs
            .iter()
            .filter(|input| {
                input
                    .external
                    .as_ref()
                    .is_some_and(|external| external.replay_seek_outstanding)
            })
            .flat_map(|input| input.stream_ids())
            .collect()
    }

    /// Settle [`ExternalInput::replay_seek_outstanding`]: this input's replay
    /// seek is done travelling and the next one may be sent.
    ///
    /// Called from exactly the two places that discharge
    /// [`Inner::replay_inflight`] - the outcome tail and the lane fallback -
    /// because the two flags answer for the same seek and a flag outliving its
    /// seek would silence every later replay for this input.
    pub(crate) fn settle_replay_seek(inner: &Arc<Inner>, id: ExternalSubId, epoch: u32) {
        let mut routing = inner.routing.lock();
        if let Some(external) = routing
            .inputs
            .iter_mut()
            .filter_map(|input| input.external.as_mut())
            .find(|external| external.id == id && external.epoch == epoch)
        {
            external.replay_seek_outstanding = false;
        }
    }

    /// Release the block probes of an external input whose hold was owed to
    /// its realigning replay seek (see
    /// [`ExternalInput::hold_release_owed`]). A no-op for every other input.
    ///
    /// This is the whole of the fix for cues rendering against a different
    /// origin than the video. A held external's `last_origin` stays ZERO,
    /// because a forwarded seek only reaches inputs with a live branch and a
    /// held one has none. `STREAMS_SELECTED` then used to release the hold
    /// SYNCHRONOUSLY while merely QUEUEING the realigning replay, and an
    /// as-fast-as-possible source needs no more than that gap to push its
    /// whole file against the stale `[0, ..)` segment. The reproducer's
    /// external has exactly 60 cues and the failure was exactly 60
    /// consecutive misaligned ones, released at 44.106355 against a replay
    /// issued at 44.109360. Ordering the two by MOVING the release later does
    /// not help, since the replay is queued to another thread either way. The
    /// release has to be CAUSED BY the replay, which is what this is.
    ///
    /// Called from [`FcastPlaybin::replay_outcome`] on ALL of its outcomes, the
    /// refusal included. A refused seek is RE-DERIVED, not remembered.
    /// [`Inner::reconcile_subtitle_delivery`] re-asks at the next settled
    /// PLAYING, which is also the first moment a flushing seek could be
    /// accepted, and the tick's 1 Hz poke guarantees the asking happens whether
    /// or not an edge comes. (Under `FCAST_NO_TEXT_RECONCILE` the v1 mechanism
    /// is back and the owing lives in [`Inner::deferred_replays`].) Either way
    /// the input would otherwise stay held for as long as that owing lasts,
    /// which is the liveness half of the problem, and a held external that
    /// never unblocks shows no subtitles at all, a strictly worse failure than
    /// shifted ones. Nothing is lost by releasing here, because the eventual
    /// seek is itself FLUSHING and wipes whatever escaped.
    ///
    /// Called on the paths where that outcome never gets decided too, i.e. an
    /// effect that unwound, was never enqueued, or lost its lane
    /// ([`hands::LaneFallback`]), and a report with no decider left to take it
    /// ([`hands::Outcome::owed`]). "On every outcome" is a contract about
    /// unconditionality, so it has to hold where there IS no outcome.
    /// Idempotent by construction, which is what lets three paths own it, since
    /// the probes are taken out of the input under the routing lock and a
    /// second release finds `hold_release_owed` already false.
    pub(crate) fn release_owed_hold(inner: &Arc<Inner>, id: ExternalSubId, epoch: u32) {
        let to_unblock: Vec<(gst::Pad, gst::PadProbeId)> = {
            let mut routing = inner.routing.lock();
            let Some(input) = routing.inputs.iter_mut().find(|input| {
                input
                    .external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch && e.hold_release_owed)
            }) else {
                return;
            };
            if let Some(external) = input.external.as_mut() {
                external.hold_release_owed = false;
            }
            std::mem::take(&mut input.block_probes)
        };
        for (pad, probe) in to_unblock {
            debug!(pad = %pad.name(), ?id, "releasing an external input's data hold owed to its replay");
            pad.remove_probe(probe);
        }
    }

    /// Queue [`Job::VerifyReplay`] after [`REPLAY_VERIFY_AFTER`], off the
    /// worker (a bounded timer, exactly like the sub watchdog).
    pub(crate) fn arm_replay_verification(&self, id: ExternalSubId, epoch: u32, attempt: u32) {
        // ONE chain per input. Two independent paths arm this for the same
        // event: `poll_text_policy` replays on every join of an external, and
        // the selection-time handler arms a check when the selection moves
        // onto one. Both fired for a single switch, each spawned its own
        // `VerifyReplay`, and each of those replayed and armed again, so the
        // attempt counters escalated in lockstep down two rival chains.
        // Observed in the field as paired `VerifyReplay ... attempt=0` a
        // millisecond apart, then paired replays at attempt=1, 2, 3.
        if !self.replay_checks_armed.lock().insert((id, epoch)) {
            debug!(
                ?id,
                epoch, attempt, "a replay verification is already armed for this input"
            );
            return;
        }
        // The tick's timer table, for the reason `arm_sub_watchdog` gives -
        // and here the infallibility is the point of the repair below: an
        // arm that cannot fail cannot strand the dedupe key it just took.
        // Levers: `FCAST_NO_TICK`, `FCAST_NO_TICK_TIMERS`.
        if self.tick_live() && std::env::var_os("FCAST_NO_TICK_TIMERS").is_none() {
            self.arm_timer(
                Instant::now() + REPLAY_VERIFY_AFTER,
                TimerJob::VerifyReplay { id, epoch, attempt },
            );
            return;
        }
        let work_tx = self.work_tx.clone();
        // Stamped at ARM time, for the reason `arm_sub_watchdog` gives.
        let queue_epoch = self.queue_epoch.load(Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("fpb-replay-check".into())
            .spawn(move || {
                std::thread::sleep(REPLAY_VERIFY_AFTER);
                let _ = work_tx.send(QueuedJob {
                    epoch: queue_epoch,
                    job: Job::VerifyReplay { id, epoch, attempt },
                });
            });
        if let Err(err) = spawned {
            // Release the dedupe key. Without this the arm has failed AND
            // poisoned the input incarnation: the key stays set, so every
            // later attempt to arm a verification for this (id, epoch) is
            // refused as "already armed" and no replay of it is ever checked
            // again.
            self.replay_checks_armed.lock().remove(&(id, epoch));
            warn!(?err, ?id, "failed to arm the replay verification");
        }
    }

    /// Forward a just-performed user seek into every external subtitle
    /// input whose stream is live in a text branch. A pipeline seek travels
    /// the sink chains and decodebin3 forwards it up the MAIN input only,
    /// so a side input's segment stays on the old timeline and its cues
    /// never sync against the sought video again. The same seek through
    /// the input aligns its segment and replays from the target
    /// (uridecodebin3 forwarded seeks to every source handler for the
    /// same reason). Deselected inputs are skipped: their replayed data
    /// would land in the parking sink, and the join-time replay owns
    /// their recovery.
    pub(crate) fn forward_seek_to_live_externals(&self, rate: f64, position: gst::ClockTime) {
        // COLLECTED PER INPUT, and `last_origin` is NOT written here.
        //
        // It used to be, inside this very `flat_map`, before a single event had
        // been sent - and `external-replay-seek-refused.txt` is what that costs
        // when the send is then refused ("rssubparse2: seek to 0 bytes failed"
        // followed by this function's own refusal warning). `last_origin` is
        // the crate's record of WHERE THIS INPUT'S TIMELINE IS, and the
        // selection-time replay trigger fires on `origin != last_origin` (see
        // the caller of `Inner::external_stream_slotless`). Recording a
        // realignment that did not happen therefore does not merely lose the
        // seek: it makes the input look ALIGNED, so the one trigger that would
        // have re-sent it never fires again and the cues stay on the old
        // timeline for the rest of the item. Write it per input, AFTER its own
        // send, and only when the send was taken.
        let targets: Vec<(Option<(ExternalSubId, u32)>, Vec<gst::Pad>)> = {
            let routing = self.routing.lock();
            let live_text: Vec<String> = routing
                .routed
                .iter()
                .filter(|r| r.kind == StreamKind::Text && r.downstream.is_some())
                .filter_map(|r| r.db3_src_pad.stream_id().map(|s| s.to_string()))
                .collect();
            routing
                .inputs
                .iter()
                .filter(|i| {
                    i.external.is_some() && i.stream_ids().iter().any(|sid| live_text.contains(sid))
                })
                .map(|i| {
                    let key = i.external.as_ref().map(|e| (e.id, e.epoch));
                    (key, i.element.src_pads())
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        // Mirror `send_rate_seek`'s event exactly so both sides of the
        // pipeline land on the same segment.
        // TODO: reconsider ACCURATE
        let mut flags = gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH;
        if rate < 0.0 || rate > 2.0 {
            flags |= gst::SeekFlags::TRICKMODE;
        }
        let event = if rate >= 0.0 {
            gst::event::Seek::builder(
                rate,
                flags,
                gst::SeekType::Set,
                position,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            )
            .build()
        } else {
            gst::event::Seek::builder(
                rate,
                flags,
                gst::SeekType::Set,
                gst::ClockTime::ZERO,
                gst::SeekType::End,
                position,
            )
            .build()
        };
        // Lever: `FCAST_FORCE_FORWARD_SEEK_REFUSAL` makes every send read as
        // refused. A refusal comes from a source's own seek handling deep
        // inside a parser chain (the field's was `rssubparse` converting the
        // TIME seek to BYTES and its upstream failing that), which no test can
        // arrange from the outside, so the RECOVERY gets a lever instead of
        // going unpinned.
        let force_refusal = std::env::var_os("FCAST_FORCE_FORWARD_SEEK_REFUSAL").is_some();
        for (key, pads) in targets {
            let mut accepted = 0usize;
            let total = pads.len();
            for pad in pads {
                debug!(pad = %pad.name(), "forwarding the seek to a live external subtitle input");
                if !force_refusal && pad.send_event(event.clone()) {
                    accepted += 1;
                } else {
                    warn!(pad = %pad.name(), "the external input refused the forwarded seek");
                }
            }
            let Some((id, epoch)) = key else { continue };
            // TOTAL REFUSAL ONLY, which is `accepted == 0 && total > 0` - the
            // same shape `FcastPlaybin::replay_outcome` calls a refusal, and
            // the field's (`accepted=0`).
            //
            // The first version of this recovery fired on `accepted != total`,
            // which swept in the two cases that are not refusals at all, and
            // `external_subtitle_lifecycle::attach_then_seek_keeps_the_
            // external_on_the_video_timeline` failed 5 runs in 6 on it:
            //
            // * `total == 0` - an input whose element has no src pads YET. Nothing was
            //   refused because nothing was asked; the seek is simply not owed to it.
            // * a PARTIAL take - some pads moved, so the input is on the new timeline as
            //   far as those streams go.
            //
            // In both, queueing a replay is active harm rather than recovery:
            // `FcastPlaybin::replay_subtitle` re-reads `video_timeline()` when
            // it RUNS, and at this point in a seek the video segment has not
            // moved yet, so the replay re-pushed the whole external against
            // the OLD origin and its cues rendered at 0 instead of the sought
            // position. The recovery must be reserved for the case where
            // nothing took the seek at all.
            if accepted == 0 && total > 0 {
                Self::forward_seek_refused(self, id, epoch, total, position);
                continue;
            }
            {
                // Taken, in whole or in part: the input has moved onto the new
                // timeline. `total == 0` records it too - there is no stream
                // to be misaligned, and leaving `last_origin` stale would make
                // the selection-time trigger replay an input that is fine.
                let mut routing = self.routing.lock();
                if let Some(external) = routing
                    .inputs
                    .iter_mut()
                    .filter_map(|input| input.external.as_mut())
                    .find(|external| external.id == id && external.epoch == epoch)
                {
                    external.last_origin = position;
                }
                continue;
            }
        }
    }

    /// One external took none of its forwarded seek: say so, count it, and
    /// hand it to the replay machinery.
    ///
    /// LOUD, and then OWNED. A refused forward used to be a `warn!` per pad
    /// and nothing else - two lines in a log that read like noise beside a
    /// `seek to 0 bytes failed` from the parser, with no statement anywhere
    /// that the track is now permanently misaligned. The replay is the one
    /// path that re-aligns an external, and it is idempotent and
    /// enqueue-deduped, so this cannot pile up.
    ///
    /// `last_origin` is deliberately NOT written by this path: the input is
    /// still on the old timeline, and leaving the divergence visible is what
    /// keeps the selection-time trigger able to see it (see the caller).
    fn forward_seek_refused(
        &self,
        id: ExternalSubId,
        epoch: u32,
        total: usize,
        position: gst::ClockTime,
    ) {
        FORWARD_SEEK_REFUSALS.fetch_add(1, Ordering::SeqCst);
        error!(
            ?id,
            epoch,
            total,
            %position,
            "an external subtitle input refused the forwarded seek on every pad, so its cues \
             are on the old timeline; replaying it to realign"
        );
        let replay_key = (id, epoch);
        if !self.replay_inflight.lock().insert(replay_key) {
            return;
        }
        if !self.queue_job(Job::ReplaySub {
            id,
            epoch,
            attempt: 0,
        }) {
            self.replay_inflight.lock().remove(&replay_key);
        }
    }

    /// Whether this external's stream is advertised but has NO decodebin3
    /// output slot, which no amount of replaying can change.
    ///
    /// decodebin3 REMOVES a multiqueue slot when the input feeding it drains:
    /// `remove_slot_from_streaming_thread` on EOS at the slot's src pad
    /// (gstdecodebin3.c:3717, whose own FIXME notes the removal is async), and
    /// slot creation only ever happens from a parsebin `pad-added`. An external
    /// subtitle's stream reaches EOS as soon as its last cue is parsed, which
    /// for a subtitle file is minutes into a feature-length item, and its
    /// parsebin pad never appears again. From then on the stream is in the
    /// collection, selectable by the caller, and SLOTLESS: a replay's data
    /// arrives at a pad whose multiqueue peer is gone and the source dies
    /// "streaming stopped, reason not-linked".
    ///
    /// Detected from two crate-side facts, no decodebin3 internals: none of the
    /// routed streams carries one of this input's text sids, and the crate's
    /// own EOS probe recorded that sid as drained.
    /// Never holds `input_eos_sids` and `routing` at the same time: the EOS
    /// probe that writes the first runs on streaming threads.
    fn external_stream_slotless(&self, id: ExternalSubId, epoch: u32) -> bool {
        let drained = self.input_eos_sids.lock().clone();
        let routing = self.routing.lock();
        let Some(input) = routing.inputs.iter().find(|i| {
            i.external
                .as_ref()
                .is_some_and(|e| e.id == id && e.epoch == epoch)
        }) else {
            return false;
        };
        let sids = input.text_stream_ids();
        if sids.is_empty() {
            // Nothing advertised yet: the materialization watchdog owns this.
            return false;
        }
        sids.iter().all(|sid| {
            drained.contains(sid)
                && !routing
                    .routed
                    .iter()
                    .any(|routed| routed.db3_src_pad.stream_id().as_deref() == Some(sid.as_str()))
        })
    }

    /// [`Self::external_stream_slotless`] minus the drain requirement: NO
    /// routed pad carries any of this input's text sids. The fully wedged
    /// attach never records a drain (its data never crossed into decodebin3),
    /// so the slotless read stays false while the stream is just as
    /// unservable: advertised, selectable, and without an output for 40 s
    /// (measured under parallel load, `text_pads=[]` through every replay).
    fn external_stream_outputless(&self, id: ExternalSubId, epoch: u32) -> bool {
        let routing = self.routing.lock();
        let Some(input) = routing.inputs.iter().find(|i| {
            i.external
                .as_ref()
                .is_some_and(|e| e.id == id && e.epoch == epoch)
        }) else {
            return false;
        };
        let sids = input.text_stream_ids();
        if sids.is_empty() {
            // Nothing advertised yet: the materialization watchdog owns this.
            return false;
        }
        sids.iter().all(|sid| {
            !routing
                .routed
                .iter()
                .any(|routed| routed.db3_src_pad.stream_id().as_deref() == Some(sid.as_str()))
        })
    }

    /// Whether one of this input's text streams is JOINED to a consumer
    /// branch (routed, downstream wired, sticky STREAM_START naming it).
    /// The same read `verify_replay` calls `delivered`.
    fn external_branch_joined(&self, id: ExternalSubId, epoch: u32) -> bool {
        let routing = self.routing.lock();
        let Some(input) = routing.inputs.iter().find(|i| {
            i.external
                .as_ref()
                .is_some_and(|e| e.id == id && e.epoch == epoch)
        }) else {
            return false;
        };
        let sids = input.stream_ids();
        routing.routed.iter().any(|routed| {
            routed.kind == StreamKind::Text
                && routed.downstream.is_some()
                && routed
                    .db3_src_pad
                    .sticky_event::<gst::event::StreamStart>(0)
                    .is_some_and(|event| sids.iter().any(|sid| *sid == event.stream_id()))
        })
    }
}

impl FcastPlaybin {
    /// Reserve an [`ExternalSubId`] without touching the pipeline. Lets a
    /// caller do its bookkeeping on one thread and run the actual attach
    /// ([`Self::attach_subtitle_with_id`]) on another: attaching drives the
    /// input element to the pipeline's state, and a source's `start()` may
    /// block on I/O, which must not run on an async event loop.
    pub fn allocate_subtitle_id(&self) -> ExternalSubId {
        let mut routing = self.inner.routing.lock();
        let id = ExternalSubId(routing.next_external_id);
        routing.next_external_id += 1;
        id
    }

    /// Live-attach an external subtitle by URI (file/http) under a
    /// pre-reserved id. Works in any pipeline state. The stream becomes
    /// selectable once decodebin3 announces the updated collection. The
    /// crate babysits the input from here: an input whose task dies in the
    /// deselect race recovers in place through the join-time replay, and
    /// one that fails for good (or never produces a stream within the
    /// bounded wait) is detached and reported as
    /// [`PlaybinEvent::ExternalSubtitleFailed`].
    ///
    /// A URI that is ALREADY attached is refused. With nothing upstream to
    /// inherit an id from, GStreamer derives a source pad's stream id by
    /// hashing the element's URI, so a second input on the same URI would
    /// report the first input's stream id, and every stream-id lookup in
    /// the crate and its callers resolves first match. Twins under one id
    /// answered about the wrong input everywhere at once (the hold release,
    /// the join and replay machinery, the selection engine's external map),
    /// and the observed end state was a subtitle path dead for the rest of
    /// the item. Detaching the existing input first makes the URI
    /// attachable again.
    pub fn attach_subtitle_with_id(&self, id: ExternalSubId, uri: &str) -> Result<()> {
        {
            let routing = self.inner.routing.lock();
            let twin = routing.inputs.iter().find_map(|input| {
                input
                    .external
                    .as_ref()
                    .filter(|external| external.uri == uri)
                    .map(|external| external.id)
            });
            if let Some(twin) = twin {
                return Err(anyhow!(
                    "subtitle URI is already attached as {twin:?}, and two inputs \
                     on one URI would share a URI-derived stream id"
                ));
            }
        }
        let generation = self.inner.current_generation();
        // NO buffering on subtitle side-inputs (uridecodebin3 also buffers
        // only the main item): a fresh input's own queue2 levels would drive
        // the caller's buffering state machine and wedge a paused pipeline
        // in "Buffering".
        let element = Inner::make_urisourcebin(uri, false)?;
        let external = ExternalInput {
            id,
            uri: uri.to_string(),
            epoch: 0,
            // Held from the very first attach: an unselected stream's first
            // push is fatal (see `ExternalInput::hold_until_selected`).
            hold_until_selected: true,
            hold_release_owed: false,
            task_dead: false,
            last_origin: gst::ClockTime::ZERO,
            replay_seek_outstanding: false,
            fed_baseline: 0,
        };
        Inner::add_input(&self.inner, element, generation, Some(external))?;
        info!(?id, uri, "attached external subtitle input");
        self.arm_sub_watchdog(id, 0);
        Ok(())
    }

    /// Arm the bounded materialization check for a just (re-)attached
    /// external input: [`Job::CheckSub`] is queued after the timeout. The job
    /// no-ops if the input produced streams, was detached, or was re-armed
    /// (epoch mismatch) in the meantime.
    fn arm_sub_watchdog(&self, id: ExternalSubId, epoch: u32) {
        let timeout = *self.inner.sub_timeout.lock();
        // The tick owns bounded timers when it runs: a table entry cannot
        // fail to arm and costs no thread. The sleeper below stays as the
        // lever arm, verbatim.
        // Levers: `FCAST_NO_TICK` (no tick at all), `FCAST_NO_TICK_TIMERS`.
        if self.inner.tick_live() && std::env::var_os("FCAST_NO_TICK_TIMERS").is_none() {
            self.inner
                .arm_timer(Instant::now() + timeout, TimerJob::CheckSub { id, epoch });
            return;
        }
        let work_tx = self.inner.work_tx.clone();
        // A sleeper holds only a `Sender`, so it cannot go through
        // `Inner::queue_job`. It stamps the queue epoch at ARM time, which is
        // when the decision to check was taken (and is inert either way: the
        // input `epoch` above is this job's real, sharper token).
        let queue_epoch = self.inner.queue_epoch.load(Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("fpb-sub-watchdog".into())
            .spawn(move || {
                std::thread::sleep(timeout);
                let _ = work_tx.send(QueuedJob {
                    epoch: queue_epoch,
                    job: Job::CheckSub { id, epoch },
                });
            });
        if let Err(err) = spawned {
            warn!(?err, ?id, "failed to arm the subtitle watchdog");
        }
    }

    /// Shorten the external-subtitle materialization timeout. For tests
    /// only: production callers keep [`EXTERNAL_SUB_TIMEOUT`].
    #[doc(hidden)]
    pub fn set_external_sub_timeout(&self, timeout: Duration) {
        *self.inner.sub_timeout.lock() = timeout;
    }

    /// Whether a replay for `(id, epoch)` is emitted and not yet settled (see
    /// [`Inner::replay_inflight`]). The in-flight guard, readable so a test
    /// can prove it is what suppresses a second emit. Not part of the public
    /// API.
    #[doc(hidden)]
    pub fn replay_inflight(&self, id: ExternalSubId, epoch: u32) -> bool {
        self.inner.replay_inflight.lock().contains(&(id, epoch))
    }

    /// Queue one `Job::ReplaySub` the way an emitter does (in-flight bit
    /// first, then the job) so a test can put TWO of them in the queue back to
    /// back and prove the second one sends nothing (see
    /// [`ExternalInput::replay_seek_outstanding`]). Reproduced rather than
    /// raced: the field's two triggers were 276 us apart, which no test can
    /// arrange by timing. Not part of the public API.
    #[doc(hidden)]
    pub fn queue_replay_sub(&self, id: ExternalSubId, epoch: u32) -> bool {
        self.inner.replay_inflight.lock().insert((id, epoch));
        let sent = self.inner.queue_job(Job::ReplaySub {
            id,
            epoch,
            attempt: 0,
        });
        if !sent {
            self.inner.replay_inflight.lock().remove(&(id, epoch));
        }
        sent
    }

    /// How many replay flushing seeks have actually been handed off (see
    /// [`REPLAY_SEEKS_SENT`]). Process-global, so read it as a delta. Not part
    /// of the public API.
    #[doc(hidden)]
    pub fn replay_seeks_sent() -> u64 {
        REPLAY_SEEKS_SENT.load(Ordering::SeqCst)
    }

    /// How many `Job::ReplaySub` jobs reached the worker queue (see
    /// [`REPLAY_JOBS_QUEUED`]). Process-global, so read it as a delta. Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn replay_jobs_queued() -> u64 {
        REPLAY_JOBS_QUEUED.load(Ordering::SeqCst)
    }

    /// How many forwarded seeks an external input refused (see
    /// [`FORWARD_SEEK_REFUSALS`]). Process-global, so read it as a delta. Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn forward_seek_refusals() -> u64 {
        FORWARD_SEEK_REFUSALS.load(Ordering::SeqCst)
    }

    /// Slot-seeding GAPs pushed / refused for held external inputs (see
    /// [`SLOT_SEED_PUSHES`]). Process-global, so read them as deltas. Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn slot_seed_pushes() -> u64 {
        SLOT_SEED_PUSHES.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn slot_seed_refusals() -> u64 {
        SLOT_SEED_REFUSALS.load(Ordering::SeqCst)
    }

    /// Run [`Inner::forward_seek_to_live_externals`] directly, so a test can
    /// exercise the forward and its refusal recovery without driving a whole
    /// transport seek through the state machine (which parks one behind the
    /// caller's seekability gate). Not part of the public API.
    #[doc(hidden)]
    pub fn forward_seek_to_externals(&self, rate: f64, position: gst::ClockTime) {
        self.inner.forward_seek_to_live_externals(rate, position);
    }

    /// In-flight replay bits whose `(id, epoch)` matches NO attached external.
    ///
    /// THE leak invariant, as a number. Every entry is discharged by
    /// `replay_outcome`, by a lane fallback, by one of `replay_subtitle`'s
    /// early returns, or by the input's removal - so an orphan means one of
    /// those paths was missed, and the consequence is a resource the reconcile
    /// pass will never act on again. Must be zero at any settled point. Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn replay_inflight_orphans(&self) -> usize {
        let live: Vec<(ExternalSubId, u32)> = {
            let routing = self.inner.routing.lock();
            routing
                .inputs
                .iter()
                .filter_map(|input| input.external.as_ref())
                .map(|external| (external.id, external.epoch))
                .collect()
        };
        self.inner
            .replay_inflight
            .lock()
            .iter()
            .filter(|key| !live.contains(key))
            .count()
    }

    /// Whether any external subtitle input is currently attached. Callers
    /// gate flushing operations on this: a flush races the external inputs'
    /// reconfiguration and can freeze the play item.
    pub fn has_external_subtitles(&self) -> bool {
        self.inner
            .routing
            .lock()
            .inputs
            .iter()
            .any(|i| i.external.is_some())
    }

    /// [`Self::allocate_subtitle_id`] + [`Self::attach_subtitle_with_id`] in
    /// one call, for callers without threading constraints.
    pub fn attach_subtitle(&self, uri: &str) -> Result<ExternalSubId> {
        let id = self.allocate_subtitle_id();
        self.attach_subtitle_with_id(id, uri)?;
        Ok(id)
    }

    /// Detach a live external subtitle input: stop it, unlink it from
    /// decodebin3 and release the request pads. Deliberately flush-based,
    /// with no draining of queued sparse data (uridecodebin3's drain is a
    /// known deactivation stall).
    pub fn detach_subtitle(&self, id: ExternalSubId) -> Result<()> {
        let inner = &self.inner;
        let mut routing = inner.routing.lock();
        let idx = routing
            .inputs
            .iter()
            .position(|i| i.is_external(id))
            .ok_or_else(|| anyhow!("no attached subtitle {id:?}"))?;
        let input = routing.inputs.remove(idx);
        drop(routing);
        Inner::remove_input_or_defer(inner, input);
        // A selection desire parked on this input must not park forever.
        inner.selection.lock().external_gone(id);
        info!(?id, "detached external subtitle input");
        Ok(())
    }

    /// The GStreamer stream ids produced by an attached external subtitle
    /// input. Empty until the input's source pads have appeared and carry
    /// their stream-start events, which is guaranteed by the time decodebin3
    /// posts the collection containing the streams, so collection handlers
    /// can rely on it to map an external input to its stream(s).
    pub fn subtitle_stream_ids(&self, id: ExternalSubId) -> Vec<String> {
        let routing = self.inner.routing.lock();
        let Some(input) = routing.inputs.iter().find(|i| i.is_external(id)) else {
            return Vec::new();
        };
        input.text_stream_ids()
    }

    /// Take an external input out of the routing table iff it still exists
    /// under the epoch the job was decided against. A `None` means the job
    /// is stale: the input was detached, replaced by a load, or re-armed
    /// since, and the current holder of the id must be left alone.
    fn take_external_input(&self, id: ExternalSubId, epoch: u32) -> Option<Input> {
        let mut routing = self.inner.routing.lock();
        let idx = routing.inputs.iter().position(|i| {
            i.external
                .as_ref()
                .is_some_and(|e| e.id == id && e.epoch == epoch)
        })?;
        Some(routing.inputs.remove(idx))
    }

    /// Worker side of a genuine external failure: detach the input and
    /// report it (see [`PlaybinEvent::ExternalSubtitleFailed`]).
    pub(crate) fn fail_subtitle(&self, id: ExternalSubId, epoch: u32) {
        let Some(input) = self.take_external_input(id, epoch) else {
            debug!(?id, epoch, "stale subtitle fail job; input already gone");
            return;
        };
        // `remove_input` retires this epoch's in-flight bit for us.
        Inner::remove_input(&self.inner, input);
        // A selection desire parked on this input must not park forever.
        self.inner.selection.lock().external_gone(id);
        warn!(?id, "external subtitle input failed; detached");
        self.inner.emit(PlaybinEvent::ExternalSubtitleFailed { id });
    }

    /// Worker side of the never-linked attach retry: an input killed
    /// BEFORE anything of it reached decodebin3 has nothing to select, so
    /// the join-time replay can never run and the watchdog would eat the
    /// user's subtitle. Replacing the element is safe exactly here,
    /// unlike the removed general re-arm: no pad locks for the NULL to
    /// deadlock on, no collection presence to churn. Epoch-capped by the
    /// caller ([`MAX_ATTACH_RETRIES`]); a genuinely bad URL exhausts the
    /// retries and the watchdog delivers the verdict.
    pub(crate) fn retry_subtitle(&self, id: ExternalSubId, epoch: u32) {
        // BEFORE the routing lock below: the checks take it too. Outputless
        // rides along for the give-up hand-off: that input HAS reached
        // decodebin3's sink pads, so without it the guard below would judge
        // it healthy and this retry a no-op.
        let slotless = self.inner.external_stream_slotless(id, epoch)
            || self.inner.external_stream_outputless(id, epoch);
        {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                debug!(?id, epoch, "stale subtitle retry; input already gone");
                return;
            };
            // Linked after all (a pad appeared between the error and this
            // job): the join-time replay owns recovery, and a replacement
            // would reintroduce the hazards the retry exists to avoid.
            //
            // A SLOTLESS stream is the exception: it is linked and advertised
            // and still cannot render, and only a fresh input gets a slot back
            // (see `Inner::external_stream_slotless`). The replay hands those
            // over here deliberately.
            if (!input.stream_ids().is_empty() || !input.db3_sink_pads.is_empty()) && !slotless {
                debug!(
                    ?id,
                    epoch, "input reached decodebin3 after all; leaving it be"
                );
                return;
            }
        }
        let Some(input) = self.take_external_input(id, epoch) else {
            return;
        };
        let uri = input.external.as_ref().expect("external input").uri.clone();
        Inner::remove_input(&self.inner, input);

        let attach = Inner::make_urisourcebin(&uri, false).and_then(|element| {
            Inner::add_input(
                &self.inner,
                element,
                self.inner.current_generation(),
                Some(ExternalInput {
                    id,
                    uri: uri.clone(),
                    epoch: epoch + 1,
                    hold_until_selected: true,
                    hold_release_owed: false,
                    task_dead: false,
                    last_origin: gst::ClockTime::ZERO,
                    replay_seek_outstanding: false,
                    fed_baseline: 0,
                }),
            )
        });
        match attach {
            Ok(()) => {
                info!(
                    ?id,
                    uri,
                    epoch = epoch + 1,
                    "retried the external subtitle attach"
                );
                self.arm_sub_watchdog(id, epoch + 1);
            }
            Err(err) => {
                error!(?err, ?id, uri, "the attach retry failed");
                self.inner.emit(PlaybinEvent::ExternalSubtitleFailed { id });
            }
        }
    }

    /// Worker side of the join-time replay: a flushing seek into the
    /// input's own source pads (a pipeline seek never reaches side
    /// inputs). The flush resets any slot queue state a previous drain
    /// left FLUSHING, restarts a task the deselect race killed, and the
    /// source re-pushes from the target, exactly like a fresh attach: past
    /// cues fall to sync, the current one shows. Epoch-guarded like the
    /// other subtitle jobs.
    ///
    /// The target is the video's running-time ORIGIN
    /// ([`Inner::video_timeline`]), not zero and not the current
    /// position. Zero replays the whole file shifted by the origin (the
    /// field bug: after any seek, a re-enable restarted the subtitles from
    /// cue one), and the current position would map that position's cue to
    /// running time zero, rendering everything later early. Only the origin
    /// gives the branch the same stream-time-to-running-time mapping the
    /// video has, which is what makes past cues late and droppable and the
    /// current one on time. Seeking the input, not the pipeline, so an
    /// unseekable main source is untouched.
    ///
    /// # THE in-flight bit is set and cleared HERE, and only here
    ///
    /// [`Inner::replay_inflight`] guards the reconcile pass against emitting a
    /// second replay while one is outstanding, so it has to cover EVERY replay,
    /// not the ones whose emit sites happened to remember. Five paths reach
    /// this function - the reconcile pass, the selection-time replay, the
    /// upstream adoption, `verify_replay`'s re-replay and the levered drain -
    /// and setting the bit at each of them left three uncovered. The one that
    /// bites: `verify_replay` clears `replay_checks_armed` at its top and hands
    /// the seek to a lane, so for the whole lane window BOTH guards read clear
    /// and the 1 Hz pass emits a rival replay against the same `(id, epoch)`.
    ///
    /// This function is the choke point every one of them funnels through, so
    /// the bit goes on immediately before the hand-off and comes off on every
    /// path that does NOT hand off. Discharged in
    /// [`FcastPlaybin::replay_outcome`], which every outcome reaches.
    pub(crate) fn replay_subtitle(&self, id: ExternalSubId, epoch: u32, attempt: u32) {
        // Set FIRST, cleared on each early return below. Setting it after the
        // guards would leave the same window this exists to close.
        self.inner.replay_inflight.lock().insert((id, epoch));
        let pads: Vec<gst::Pad> = {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                debug!(?id, epoch, "stale subtitle replay job; input already gone");
                self.inner.replay_inflight.lock().remove(&(id, epoch));
                return;
            };
            input.element.src_pads()
        };
        // A replay CANNOT fix a stream decodebin3 has no slot for, and a
        // drained external is exactly that (see
        // `Inner::external_stream_slotless`). Re-attaching is the only way to
        // get a slot back, so hand over to the retry instead of pushing a seek
        // into a pad whose multiqueue peer is gone (which kills the source with
        // "reason not-linked" and, four attempts later, gave up in silence).
        // Lever: `FCAST_NO_SLOTLESS_EXTERNAL_REATTACH`.
        if std::env::var_os("FCAST_NO_SLOTLESS_EXTERNAL_REATTACH").is_none()
            && self.inner.external_stream_slotless(id, epoch)
        {
            warn!(
                ?id,
                epoch, attempt, "the external's stream has no decodebin3 slot; re-attaching it"
            );
            // No seek is happening, so nothing will report an outcome and
            // nothing would ever clear the bit. `RetrySub` is not a
            // replacement discharge either: its "input reached decodebin3
            // after all; leaving it be" arm returns WITHOUT bumping the epoch,
            // so a bit left set here would silence the reconcile pass for this
            // (id, epoch) permanently - on an external that is alive and
            // possibly misaligned.
            self.inner.replay_inflight.lock().remove(&(id, epoch));
            self.inner.queue_job(Job::RetrySub { id, epoch });
            return;
        }
        let (rate, origin) = self.inner.video_timeline();
        {
            let mut routing = self.inner.routing.lock();
            if let Some(external) = routing
                .inputs
                .iter_mut()
                .filter_map(|input| input.external.as_mut())
                .find(|external| external.id == id && external.epoch == epoch)
            {
                // ONE seek per resource at a time. Two triggers may legitimately
                // decide a replay is owed at the same instant - the join-time one
                // and the selection-time one raced in the field, 276 us apart -
                // and `replay_inflight` cannot collapse them because the emitters
                // set it before they queue. So the collapse happens HERE, where
                // both jobs finally meet (see
                // `ExternalInput::replay_seek_outstanding`).
                //
                // The bit stays SET on this return: it belongs to the seek that
                // is already out, which discharges it in `replay_outcome`.
                // Clearing it here would hand the reconcile pass the very window
                // it exists to close.
                if std::mem::replace(&mut external.replay_seek_outstanding, true) {
                    drop(routing);
                    debug!(
                        ?id,
                        epoch,
                        attempt,
                        "a replay seek is already out for this input; not sending a second"
                    );
                    return;
                }
                external.last_origin = origin;
                // The replay seek restarts the source task.
                external.task_dead = false;
                // The delivery-evidence window opens at the hand-off: the
                // chain's verification requires the fed count to move past
                // this (see `Inner::external_cues_fed`).
                external.fed_baseline = self
                    .inner
                    .external_cues_fed
                    .lock()
                    .get(&id)
                    .copied()
                    .unwrap_or(0);
            }
        }
        // ZERO THE PAD OFFSET this input's text branches carry, before the seek
        // goes out. `Inner::sync_text_running_time` can compute its
        // compensation from the PRE-replay segment (an external attached
        // mid-item first pushes its own file-start segment, so a poll landing
        // there applies `offset = -origin`), and that stale offset is still on
        // the pad when the replay's own cues cross it. Every one of them clips
        // to a negative running time and `Inner::clipped_running_time` drops it
        // permanently, because `set_offset` marks the sticky not-received
        // (gstpad.c) and a sparse subtitle track has no next push to re-send
        // the corrected segment. The replay puts this branch ON the origin,
        // where offset 0 is right by construction.
        //
        // Measured on `subtitle_disable::external_enable_late_shows_position_
        // correct_cue`, 11 of 11 red with a stale offset and 38 of 38 green
        // without one. Lever: `FCAST_NO_REPLAY_OFFSET_RESET`.
        if std::env::var_os("FCAST_NO_REPLAY_OFFSET_RESET").is_none() {
            let routing = self.inner.routing.lock();
            Inner::zero_text_offsets_for(&routing, id, epoch);
        }
        let seek = gst::event::Seek::builder(
            rate,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            origin,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        )
        .build();
        // HANDED OFF, never sent from here. The source performs a flushing
        // seek inline on the sending thread and pushes its FLUSH_START down
        // the live graph from there, where a `queue` parked behind a
        // prerolling sink blocks the sender in `gst_pad_pause_task`
        // indefinitely. Only PLAYING ends that preroll, only `Job::SetState`
        // reaches PLAYING, and only this worker runs jobs, so sending here
        // makes the worker wait on itself. See [`ReplayJob`], and
        // `fuzz_buffering` seed 600055 (iters 4, actions 22) for the deterministic
        // reproducer, which fails 3 of 3 with the seek sent from here.
        //
        // The lever sends inline on the worker again, which is the whole of
        // the change: both arms run the identical send and the identical
        // outcome, and what differs is only which thread carries which half.
        let job = ReplayJob {
            pads,
            seek,
            id,
            epoch,
            attempt,
            origin,
            rate,
            pipeline: Some(self.inner.pipeline.clone()),
        };
        REPLAY_SEEKS_SENT.fetch_add(1, Ordering::SeqCst);
        // The inline lever runs both halves HERE, so the replay never enters
        // the hands at all: no envelope, nothing in flight, no `Done`.
        if std::env::var_os("FCAST_INLINE_REPLAY_SEEK").is_some() {
            Self::run_replay_seek(&self.inner, job);
        } else if let Err(effect) = self.inner.enqueue_effect(Effect::ReplaySeek(job)) {
            // No lane to send it, so the seek is not happening - but the hold
            // it owes is owed all the same, and nothing else will ever come
            // to release it (`queue_chain_join`'s lost-join fallback, for the
            // same reason). A dropped replay costs an unaligned subtitle; a
            // dropped release costs an input that never delivers again.
            warn!(?id, "the replay sender is gone, dropping the replay seek");
            Inner::run_lane_fallback(&self.inner, None, hands::LaneFallback::of(&effect));
        }
    }

    /// Send one replay's flushing seek and act on the outcome, in one call.
    ///
    /// The v1 shape, kept for the two arms that still want it on one thread:
    /// `FCAST_INLINE_REPLAY_SEEK` (worker) and `FCAST_NO_HANDS` /
    /// `FCAST_INLINE_REPLAY_OUTCOME` (the replay lane). The default arm runs
    /// the two halves where they belong - [`Self::send_replay_seek`] on the
    /// lane, [`Self::replay_outcome`] on the decider - which is the whole of
    /// the difference between them.
    pub(crate) fn run_replay_seek(inner: &Arc<Inner>, job: ReplayJob) -> Outcome {
        let outcome = Self::send_replay_seek(job);
        if let Outcome::ReplaySent {
            sub_id,
            epoch,
            attempt,
            accepted,
            total,
        } = outcome
        {
            Self::replay_outcome(inner, sub_id, epoch, attempt, accepted, total);
        }
        outcome
    }

    /// Send one replay's flushing seek. Runs on the replay lane (see
    /// [`ReplayJob`]), where BLOCKING IS ALLOWED. Nothing else waits behind
    /// that thread, so a seek parked on a queue's stream lock costs only this
    /// replay's latency and completes as soon as the pipeline flows again.
    ///
    /// Takes no `Inner` AT ALL, which is the point of the split: the lane
    /// pushes an event out of the crate and counts what happened, and every
    /// decision that follows from the count belongs to the decider (see
    /// [`Self::replay_outcome`]).
    pub(crate) fn send_replay_seek(job: ReplayJob) -> Outcome {
        let ReplayJob {
            pads,
            seek,
            id,
            epoch,
            attempt,
            origin,
            rate,
            pipeline,
        } = job;
        // HELD ACROSS THE WHOLE LOOP, not per pad. The source performs the
        // seek inline on this thread and its `FLUSH_STOP(reset_time = TRUE)`
        // reaches a text-branch sink before `send_event` returns, so this
        // bracket is the entire window (see [`StartTimeGuard`]). A re-enable
        // while PAUSED without it is `subtitle-reenable-freeze.txt`: position
        // restarting at 0.0 and ~19 s of frozen video on the next PLAYING.
        let start_time = StartTimeGuard::hold(pipeline.as_ref());
        let mut accepted = 0usize;
        for pad in &pads {
            info!(pad = %pad.name(), ?id, ?origin, rate, attempt, "replaying the spent external subtitle input");
            if pad.send_event(seek.clone()) {
                accepted += 1;
            } else {
                warn!(pad = %pad.name(), ?id, "the external input refused the replay seek");
            }
        }
        drop(start_time);
        Outcome::ReplaySent {
            sub_id: id,
            epoch,
            attempt,
            accepted,
            total: pads.len(),
        }
    }

    /// What a sent replay means: release the hold it owes, postpone a refused
    /// one, arm the verification, escalate an exhausted one.
    ///
    /// The decider's, from [`Job::EffectDone`] (the lever keeps it on the
    /// lane). Five crate-state mutations that a lane used to perform on
    /// whatever the pipeline's timing made of it; here they are sequenced
    /// against every other decision about this input - the drain that would
    /// re-queue the postponed replay, the detach that would remove it, the
    /// text policy the exhaustion pokes.
    ///
    /// `accepted`/`total` are the LANE's observation, and the only thing from
    /// there this can act on. Everything else it re-reads.
    ///
    /// The release is unconditional on EVERY path out of here, including the
    /// paths that never come through here at all: an effect that never
    /// finished, or never reached a lane to begin with, releases through
    /// [`hands::LaneFallback`], and a report with no decider left to take it
    /// releases on the lane ([`Outcome::owed`]). A second release is a no-op
    /// by construction (see the helper), so those paths need no coordination
    /// beyond existing.
    pub(crate) fn replay_outcome(
        inner: &Arc<Inner>,
        id: ExternalSubId,
        epoch: u32,
        attempt: u32,
        accepted: usize,
        total: usize,
    ) {
        // The source performed the flushing seek INLINE on the lane, so by
        // the time this outcome exists its segment is the realigned one and a
        // hold this replay owes can finally come off. Before the send it
        // could not: the whole point is that nothing may escape the input
        // until the seek has landed. On every outcome, including the refusal
        // below (see the helper).
        // The per-resource in-flight bit is discharged HERE and nowhere
        // earlier: `replay_outcome` is the decider tail every outcome reaches,
        // including the refusal below and the lane's exactly-once settlement,
        // so clearing it here cannot leak a bit that would suppress the
        // reconcile pass for this resource for good. See
        // [`Inner::replay_inflight`].
        inner.replay_inflight.lock().remove(&(id, epoch));
        // The seek this outcome reports is no longer travelling, so the choke
        // point may pass the next one (see `Inner::settle_replay_seek`).
        Inner::settle_replay_seek(inner, id, epoch);
        Inner::release_owed_hold(inner, id, epoch);
        // Not one pad took it. A pipeline at rest in PAUSED refuses a flushing
        // seek on every pad, every push logging `Failed to push event ...
        // state="paused"`, and the verification then correctly saw the stream
        // still unaligned and replayed again. Four rounds of work that could
        // not succeed by construction. Owe it to the moment the pipeline can
        // carry it instead, and do NOT arm a check that would only rediscover
        // the same thing.
        //
        // Decided from the OUTCOME rather than from the pipeline state: a
        // state check also matched a pipeline transiently at rest during a
        // seek, where the seek IS accepted, and postponing there left the
        // input unaligned for good.
        if accepted == 0 && total > 0 {
            // A new postponed item invalidates the last drain's no-op
            // verdict (see `Inner::drain_poke_parked`).
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
            if Inner::text_reconcile_levered() {
                let mut owed = inner.deferred_replays.lock();
                if !owed
                    .iter()
                    .any(|(oid, oepoch, _)| *oid == id && *oepoch == epoch)
                {
                    debug!(
                        ?id,
                        epoch, attempt, "postponing a replay the pipeline refused"
                    );
                    owed.push((id, epoch, attempt));
                }
                return;
            }
            // RETURN, owing nothing. The refusal left the input unaligned, and
            // unaligned is exactly what the reconcile pass observes at the
            // next settled PLAYING - which is also the first moment a flushing
            // seek could be accepted. Remembering the attempt would only have
            // reproduced that schedule from memory.
            debug!(
                ?id,
                epoch, attempt, "the pipeline refused the replay; the reconcile pass re-emits"
            );
            return;
        }
        // A replay can race the very slot swap that requested it (the
        // re-delivery drains into a slot decodebin3 is still relinking), so
        // check back once: a bounded sleeper, exactly like the sub watchdog.
        if attempt < REPLAY_ATTEMPTS {
            inner.arm_replay_verification(id, epoch, attempt);
            return;
        }
        // Out of attempts. This used to end HERE in silence, which left the
        // receiver reporting a subtitle track that renders nothing (in
        // upstream-selection mode the crate's own merged report has already
        // named this sid, so the caller believes the switch worked).
        //
        // It must NOT end in a detach either, which is what the first version
        // of this escalation did and what the field then punished: exhaustion
        // on an input that materialized and is DELIVERING into decodebin3 means
        // "its branch has not joined yet", not "the file is
        // bad". The join runs from `poll_text_policy` on the CALLER's cadence
        // while these attempts run on a 400ms worker timer, so a slow caller
        // loses the race and a perfectly servable external got detached and
        // dropped from the user's track list (`ResourceNotFound` at the
        // sender).
        //
        // So: only the case nothing can serve is failed, i.e. a stream with no
        // carrier pad at all AFTER a re-attach already had its chance
        // (`external_stream_slotless` hands the first occurrence to
        // `Job::RetrySub`, which bumps the epoch). Anything with a carrier is
        // left ATTACHED and merely reported loudly: the branch can still join
        // on any later poll, join-time replay included, and the next selection
        // finds a live input instead of a missing track.
        // Lever: `FCAST_NO_REPLAY_GIVEUP_ESCALATION` (set = the original
        // silent give-up, no warning and no failure either way).
        if std::env::var_os("FCAST_NO_REPLAY_GIVEUP_ESCALATION").is_some() {
            return;
        }
        // Slotless OR outputless: either way no pad can ever carry this
        // stream, and only a fresh input gets a slot back. Outputless covers
        // the fully wedged attach the slotless read is blind to (no drain
        // was ever recorded, see `external_stream_outputless`).
        let unservable = inner.external_stream_slotless(id, epoch)
            || inner.external_stream_outputless(id, epoch);
        // A JOINED branch whose tail never received a SEGMENT after a whole
        // chain of flushing replays is just as unservable: the multiqueue
        // destroyed the segment in flight (C12 family, the sibling of the
        // rescued CAPS) and a replay cannot re-send what dies inside the
        // slot, so left alone the reconcile pass re-emits a doomed chain
        // every 2 s for the rest of the item. Scoped to a joined branch so a
        // slow caller's not-yet-joined input keeps the mild arm below.
        // Lever: `FCAST_NO_SEGMENTLESS_TAIL_ESCALATION`.
        let segmentless = std::env::var_os("FCAST_NO_SEGMENTLESS_TAIL_ESCALATION").is_none()
            && inner.external_branch_joined(id, epoch)
            && inner.text_tail_segment().is_none();
        if (unservable || segmentless) && epoch > 0 {
            warn!(
                ?id,
                epoch,
                attempt,
                segmentless,
                "a re-attached external still cannot deliver; failing it"
            );
            inner.queue_job(Job::FailSub { id, epoch });
            return;
        }
        if unservable || segmentless {
            warn!(
                ?id,
                epoch,
                attempt,
                segmentless,
                "the external's stream cannot deliver through decodebin3; re-attaching it"
            );
            inner.queue_job(Job::RetrySub { id, epoch });
            return;
        }
        // Loud, but not fatal: the input stays attached and servable.
        warn!(
            ?id,
            epoch,
            attempt,
            "the external subtitle has not rendered after every replay attempt; \
             leaving it attached for the next join"
        );
        // The join is what is missing, so poke the thing that performs it
        // rather than waiting for the caller's next settle point.
        //
        // A DIRECT call, not a queued poll: by default this outcome is
        // already running on the decider, which is the thread the policy
        // wants, and going through the queue would only postpone it behind
        // whatever the receiver has just poked. Under the levers this runs on
        // the replay lane, where a direct call is what v1 did.
        Inner::poll_text_policy(inner);
    }

    /// Worker side of the replay verification: the replay took iff the
    /// input's stream reached its decodebin3 OUTPUT pad (the sticky
    /// STREAM_START names it, pad reuse included). If it has not, the
    /// re-delivery was eaten by the racing reconfiguration: replay again,
    /// bounded.
    ///
    /// A verdict is only ever reached at a pipeline settled at PLAYING.
    /// Anywhere below, the check holds itself into
    /// [`Inner::deferred_verifications`] and the deferred-work drain re-arms
    /// it later, because a pipeline that is not flowing leaves the stickies
    /// exactly as the input's previous tenure left them and they prove
    /// nothing about this one.
    pub(crate) fn verify_replay(&self, id: ExternalSubId, epoch: u32, attempt: u32) {
        // The chain this check belongs to has now run, so the next legitimate
        // arming is allowed. See `arm_replay_verification`.
        self.inner.replay_checks_armed.lock().remove(&(id, epoch));
        // A verdict needs evidence, and a pipeline below a settled PLAYING
        // has none. Nothing flows there, so the stickies read below are
        // leftovers of the input's previous tenure, and a spent input that
        // will never push another buffer still passes as aligned delivery.
        // Concluding here is what left the field's renderer linked to a dead
        // input after rapid switches at a pipeline parked in Buffering. Hold
        // the verdict instead. The deferred-work drain re-arms it once the
        // pipeline is settled at PLAYING, where the postponed flush has run
        // and delivery is observable. Checked before the routing lock so the
        // state read never nests inside it. The lever restores the old
        // conclude-anywhere behavior for interleaved A/B measurement and
        // gates this whole change (the hold here, the drain of held
        // verdicts, and the drain's selection re-verification).
        if std::env::var_os("FCAST_NO_REPLAY_VERDICT_DEFERRAL").is_none() {
            let (_, current, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
            if current != gst::State::Playing || pending != gst::State::VoidPending {
                // A new postponed item invalidates the last drain's no-op
                // verdict (see `Inner::drain_poke_parked`).
                self.inner.drain_poke_parked.store(false, Ordering::SeqCst);
                if Inner::text_reconcile_levered() {
                    let mut held = self.inner.deferred_verifications.lock();
                    if !held
                        .iter()
                        .any(|(hid, hepoch, _)| *hid == id && *hepoch == epoch)
                    {
                        debug!(
                            ?id,
                            epoch, attempt, "holding a replay verdict below a settled PLAYING"
                        );
                        held.push((id, epoch, attempt));
                    }
                    return;
                }
                // RETURN WITHOUT A VERDICT. Nothing is remembered, because
                // nothing needs to be: the next drain's reconcile pass asks
                // the same question against a pipeline that can answer it, and
                // the tick queues a pass every second whether or not an edge
                // ever comes. A held list was only ever a way of arranging to
                // be asked again.
                debug!(
                    ?id,
                    epoch,
                    attempt,
                    "no verdict below a settled PLAYING; the reconcile pass re-asks"
                );
                return;
            }
        }
        let (delivered, fed_baseline) = {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                return;
            };
            let fed_baseline = input
                .external
                .as_ref()
                .map(|e| e.fed_baseline)
                .unwrap_or(0);
            let sids = input.stream_ids();
            // Only meaningful while this input's stream is still WANTED: a
            // selection that moved on owns its own replay. `applied` alone
            // cannot answer that: decodebin3 retracting the stream (slot
            // destroyed on side-input EOS) forces an applied subtitle-None
            // while the desire still names this external, and this chain
            // (replay -> slotless -> re-attach) is the only way back.
            let still_selected = {
                let selection = self.inner.selection.lock();
                selection
                    .subtitle_sid()
                    .is_some_and(|sid| sids.contains(&sid))
                    || selection.desires_external(id)
            };
            if !still_selected {
                debug!(
                    ?id,
                    attempt,
                    ?sids,
                    "replay check: selection moved on; not replaying"
                );
                return;
            }
            let delivered = routing.routed.iter().any(|routed| {
                routed.kind == StreamKind::Text
                    && routed.downstream.is_some()
                    && routed
                        .db3_src_pad
                        .sticky_event::<gst::event::StreamStart>(0)
                        .is_some_and(|event| sids.iter().any(|sid| *sid == event.stream_id()))
            });
            (delivered, fed_baseline)
        };
        // Delivered is not enough: an input that joined the branch WITHOUT a
        // replay carries its own file-origin segment, and its cues render
        // shifted whenever the video's origin moved (a started-at or sought
        // item). Only aligned delivery needs no replay.
        // THE SAME predicate the reconcile pass uses, by construction rather
        // than by two copies that agree today (see
        // [`Inner::subtitle_origin_matches_video`]). A drift between them
        // would be a check and a pass that disagree about what "aligned"
        // means, which is the one thing neither is allowed to do.
        let aligned = delivered && self.inner.subtitle_origin_matches_video();
        // Alignment alone cannot prove a cue survived the trip (see
        // [`Inner::external_cues_fed`]): a burst the multiqueue destroyed in
        // flight leaves a seated, aligned, silent branch, and concluding on
        // it here is what made that silence permanent. The chain succeeds
        // only when a cue reached the consumer since its hand-off.
        // Lever: `FCAST_NO_REPLAY_DELIVERY_EVIDENCE` (set = alignment only).
        let progressed = std::env::var_os("FCAST_NO_REPLAY_DELIVERY_EVIDENCE").is_some()
            || self
                .inner
                .external_cues_fed
                .lock()
                .get(&id)
                .copied()
                .unwrap_or(0)
                > fed_baseline;
        if aligned && progressed {
            return;
        }
        debug!(
            ?id,
            attempt,
            delivered,
            progressed,
            "the switched-to stream is not rendering aligned; replaying"
        );
        self.replay_subtitle(id, epoch, attempt + 1);
    }

    /// Worker side of the materialization watchdog: an input still without
    /// streams when its check fires never worked (a bad URL can die without
    /// a bus error). Epoch-guarded like the other subtitle jobs.
    pub(crate) fn check_subtitle(&self, id: ExternalSubId, epoch: u32) {
        let materialized = {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                // Detached or re-armed since this check was armed (the
                // re-arm brought its own watchdog).
                return;
            };
            // An external that produced only audio or video has NOT
            // materialized as a subtitle, however healthy it looks. Asking
            // `stream_ids` here let an audio file handed in as "the subtitle"
            // pass the watchdog, and its audio stream was then advertised to
            // the caller as a subtitle track.
            //
            // An input that has not classified its streams yet is left alone.
            // Only a positively known absence of text counts as a failure.
            !input.text_stream_ids().is_empty() || input.has_unclassified_stream()
        };
        if materialized {
            return;
        }
        warn!(
            ?id,
            "external subtitle produced no text stream within the timeout"
        );
        self.fail_subtitle(id, epoch);
    }
}
