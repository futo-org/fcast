//! The job model and the threads that run it: the `Job` vocabulary, the
//! supersession policy, the worker/tick/timer/lane loops and the dispatcher.

use std::{
    sync::{Arc, Weak, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use gst::prelude::*;
use tracing::{debug, debug_span, error, trace, warn};

use crate::{
    Counters, FcastPlaybin, Inner,
    api::{AfterCancel, ErrorOrigin, ExternalSubId, MediaInput, PlaybinEvent, StartPoint},
    decisions,
    external::REPLAY_JOBS_QUEUED,
    gapless::{CancelOutcome, PreparedNext, SwapState},
    graph, hands,
    hands::{EFFECT_WEDGE_WARN, Effect, EffectId, Lane, Outcome},
    pipeline::{rate_seek_event, send_rate_seek},
    routing::{Input, StreamKind},
    selection,
    state_machine::Seek,
};

/// Tick period for `fpb-tick`, the crate's one periodic thread (see
/// [`Inner::run_tick`]). Coarse on purpose. Every consumer is a deadline an
/// order of magnitude larger, except [`REPLAY_VERIFY_AFTER`], which tolerates
/// +200ms (a later verification only ever sees a MORE settled pipeline, and
/// the dedupe and epoch guards are unchanged).
pub(crate) const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Tick multiple for the deferred-drain liveness re-poke (5 * 200ms = 1s).
/// See [`Inner::run_tick`].
pub(crate) const DRAIN_REPOKE_TICKS: u64 = 5;

/// A worker-thread callback carried by a [`Job`], wrapped so the enum can
/// derive `Debug`. A closure has no debug shape worth printing, so it prints
/// as its own name and the derive keeps the variant's other fields.
///
/// Not generic over the argument: the barrier's `FnOnce()` and the snapshot's
/// `FnOnce(T)` are different traits, and a generic `Callback<()>` would need
/// an adapter closure (and a second box) at every barrier site.
pub(crate) struct Callback(pub(crate) Box<dyn FnOnce() + Send>);

impl Callback {
    /// Run it, consuming the callback.
    pub(crate) fn call(self) {
        (self.0)()
    }
}

impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Callback")
    }
}

/// [`Callback`] for the one job whose callback takes an argument.
pub(crate) struct SnapshotCallback(pub(crate) Box<dyn FnOnce(graph::GraphSnapshot) + Send>);

impl SnapshotCallback {
    pub(crate) fn call(self, snapshot: graph::GraphSnapshot) {
        (self.0)(snapshot)
    }
}

impl std::fmt::Debug for SnapshotCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Callback")
    }
}

/// A queue-time stamp carried for a diagnostic counter, wrapped so the
/// derived `Debug` stays as short as the hand-written one was: the raw
/// `Instant` prints its clock internals into every "Got job" line and no
/// reader of that line has ever wanted them.
pub(crate) struct QueuedAt(pub(crate) Instant);

impl std::fmt::Debug for QueuedAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueuedAt")
    }
}

/// Work executed on the crate's worker thread (the `_async` methods). A
/// dedicated thread because these calls can block (a state change waits on
/// streaming threads, an attach's `start()` may perform I/O) and must not
/// run on the caller's event loop. A single queue keeps them ordered.
#[derive(Debug)]
pub(crate) enum Job {
    SetState {
        target: gst::State,
    },
    /// Full teardown to `target` (see [`FcastPlaybin::stop_async`]).
    Stop {
        target: gst::State,
        done: Option<Callback>,
    },
    Load {
        input: MediaInput,
        start: StartPoint,
        generation: u64,
    },
    Seek(Seek),
    RefreshSeek {
        seqnum: gst::Seqnum,
    },
    RecoverClock,
    /// Re-run the pipeline's latency query and redistribute (answers a
    /// `GST_MESSAGE_LATENCY`, e.g. after the video sink's render-delay
    /// changed). Runs on the worker because it queries upstream and pushes a
    /// latency event, which must not happen inline on the bus (streaming)
    /// thread.
    RecalculateLatency,
    AttachSub {
        id: ExternalSubId,
        url: String,
    },
    DetachSub {
        id: ExternalSubId,
    },
    /// Fail an external subtitle input: detach it and report
    /// [`PlaybinEvent::ExternalSubtitleFailed`]. Queued by the bus error
    /// handler (the detach must not run on the posting streaming thread).
    /// `epoch` guards against acting on an input that was re-armed or
    /// replaced after the job was queued.
    FailSub {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Bounded materialization check, armed per (re-)attach. An input still
    /// without streams when this fires is dead (bad URL that never errors)
    /// and is failed.
    CheckSub {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Re-attach an external input that died before anything of it
    /// reached decodebin3 (see `FcastPlaybin::retry_subtitle`).
    RetrySub {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Unlock a materialized external input and join it to the pipeline
    /// state. Attach leaves externals STATE-LOCKED (see `Inner::add_input`),
    /// so a pipeline state change cannot recurse into an input whose
    /// typefind/parsebin machinery is still plugging (that recursion is an
    /// ABBA deadlock against the plugging thread's sync_state_with_parent).
    /// Materialized means the plugging finished, which makes this join safe.
    AdoptSubState {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Did a replay actually take? A replay racing decodebin3's own slot
    /// swap (the selection that triggered it is still reconfiguring) can
    /// pour its one-shot re-delivery into a slot the swap then drains, so
    /// each replay arms one bounded re-check. If the input's stream has not
    /// reached its decodebin3 output pad, replay again.
    VerifyReplay {
        id: ExternalSubId,
        epoch: u32,
        attempt: u32,
    },
    /// Replay an external input whose stream just joined its branch, via a
    /// flushing zero-seek into the input (see `Inner::poll_text_policy`,
    /// which queues one on EVERY join).
    ReplaySub {
        id: ExternalSubId,
        /// Which retry this is, see [`Job::VerifyReplay`].
        attempt: u32,
        epoch: u32,
    },
    /// Snapshot the pipeline graph for the inspector. On the worker so the
    /// element walk cannot race a load's sink teardown.
    DumpGraph {
        done: SnapshotCallback,
    },
    /// Call `done` on the worker and nothing else (see
    /// [`FcastPlaybin::barrier_async`]). FIFO makes it a "every job queued
    /// before me has finished" signal for the caller waiting on it.
    ///
    /// Its own variant rather than a [`Job::DumpGraph`] whose snapshot is
    /// thrown away: the walk serializes every readable property of every
    /// element in a LIVE pipeline, which the barrier does not need and the
    /// fuzz harnesses paid for once per iteration.
    Barrier {
        done: Callback,
    },
    /// Pre-arm the next item on the live core (gapless transition). See
    /// [`FcastPlaybin::prepare_next_async`].
    PrepareNext {
        input: MediaInput,
        generation: u64,
    },
    /// Drop a prepared next input that will not be needed (seek away from
    /// the end, queue mutation, autoplay turned off).
    CancelPrepared {
        /// Report the outcome ([`PlaybinEvent::PreparedCancelled`] /
        /// [`PlaybinEvent::PreparedCancelDeclined`]). Only a CALLER's cancel
        /// wants that. The crate's own self-cancels follow a
        /// [`PlaybinEvent::PreparedFailed`] that already told the caller its
        /// prepare is gone.
        notify: bool,
        /// What the caller does to the pipeline next, which decides the
        /// consumed-end synthesis (see [`AfterCancel`] and
        /// [`FcastPlaybin::cancel_prepared`]). The crate's own self-cancels
        /// pass [`AfterCancel::Nothing`]: nothing follows them either.
        after: AfterCancel,
    },
    /// Post-activation cleanup: remove every input older than the newly
    /// activated generation (the drained main input and the previous item's
    /// external subtitles). Queued by the activation detection, which runs
    /// on a posting (streaming) thread where pipeline surgery is forbidden.
    FinishActivation,
    /// Re-align the text branches' running time with the A/V branches' (see
    /// [`Inner::sync_text_running_time`]). Queued by the SEGMENT probe on
    /// the video sink's own sink pad, which runs on the VIDEO streaming
    /// thread. The probe itself must take no lock, so it only posts this and
    /// the worker does the routing-lock work, same reason as
    /// [`Job::FinishActivation`].
    SyncTextRunningTime,
    /// Drain whatever text-branch work was postponed at a moment the
    /// pipeline could not carry it out (see [`Inner::run_deferred_text_work`]).
    ///
    /// Queued by the bus translation on every PIPELINE state edge, so the
    /// drain can never depend on the caller's poll cadence. The job itself
    /// re-checks what the pipeline currently allows, so a drain queued on an
    /// edge that still cannot complete simply leaves the work pending for the
    /// next edge.
    DrainTextWork,
    /// The item's video stream left decodebin3. Park any linked text
    /// and take the video chain out of the pipeline (see
    /// [`Inner::unroute_db3_pad`]).
    ///
    /// Queued rather than run inline for the reason
    /// [`Job::FinishActivation`] gives. `unroute_db3_pad` runs on
    /// decodebin3's `pad-removed` callback, a STREAMING thread, and both
    /// halves of this are pipeline surgery. Running them there can deadlock
    /// state changes against that same streaming thread.
    VideoChainGone,
    /// Re-commit a pipeline whose state changes GStreamer has latched off.
    ///
    /// ANY child error makes `gst_bin_handle_message_func` set
    /// `GST_STATE_RETURN = FAILURE`, after which `bin_handle_async_done`
    /// refuses every commit, so a pipeline that lost state keeps `pending`
    /// forever even once all sinks have prerolled, and posts no further
    /// `state-changed`. Nothing is unsettled, only uncommitted, hence
    /// `unsettled []`. Only a fresh `set_state` clears it. So an error this
    /// crate CONSUMES rather than surfaces has to re-commit, or the caller
    /// waits for a settle that can no longer happen.
    ///
    /// Re-commit `GST_STATE_PENDING`, the transition actually refused, not
    /// `current`. On a pipeline caught mid-climb the two differ and
    /// re-committing `current` cancels the climb.
    ClearStateFailure,
    /// A dispatched `SELECT_STREAMS` has waited [`SELECTION_DEADLINE`] for a
    /// `STREAMS_SELECTED` that never came (fired by [`Inner::run_tick`]).
    ///
    /// The tick decides only WHEN to look. Everything that reads the pipeline
    /// or re-decides happens here, on the worker. `seqnum` is the dispatch
    /// this deadline was armed for, re-validated at execution, so a
    /// confirmation that raced the fire leaves nothing in flight and the job
    /// returns.
    SelectionDeadline {
        seqnum: gst::Seqnum,
    },
    /// A dispatched refresh seek has waited [`REFRESH_DEADLINE`] for the
    /// top-level `ASYNC_DONE` that settles it. Same division of labour as
    /// [`Job::SelectionDeadline`].
    RefreshDeadline {
        seqnum: gst::Seqnum,
    },
    /// Re-drive the text link policy ([`Inner::poll_text_policy`]).
    ///
    /// The policy is bin surgery under the routing lock. This job is how
    /// threads that are not the decider ask for it.
    ///
    /// COALESCED at the asking side ([`Inner::request_text_policy_poll`]).
    /// One may be queued at a time, because the policy re-reads the whole
    /// world when it runs and N queued polls decide exactly what one queued
    /// poll decides. The job clears that bit BEFORE running, so a poke that
    /// lands mid-run is a new question rather than a lost edge.
    PollTextPolicy,
    /// Carry out a selection the engine has already dispatched. The eager
    /// text-branch work, then the `SELECT_STREAMS` that goes with it (see
    /// [`FcastPlaybin::dispatch_selection`]).
    ///
    /// The split is deliberate and it is the ENGINE that stays behind.
    /// [`FcastPlaybin::pump_selection`] decides, records the dispatch and
    /// arms its deadline under ONE engine lock on the calling thread, so a
    /// confirmation can never find a wait that has not been recorded yet.
    /// What travels here is the EXECUTION: bin surgery on the text branch,
    /// which is the decider's to perform, and the send.
    ///
    /// PARK BEFORE SEND is what makes a subtitle switch instant rather than
    /// waiting out a cue period (see `pump_selection`'s Select arm for the
    /// mechanism). The ordering holds STRUCTURALLY rather than by timing.
    /// This job parks and then enqueues the select effect from one thread
    /// onto a FIFO lane, so the park precedes the send by construction. The
    /// PARK is the only eager work there is.
    ///
    /// Dropped when superseded (see [`stale_policy`]), and the drop is
    /// REPORTED via `dispatch_failed` for the seqnum, so an engine that was
    /// not reset re-decides instead of waiting out a deadline for a send that
    /// is never going to happen.
    DispatchSelection {
        target: selection::TrackSelection,
        seqnum: gst::Seqnum,
        /// Whether this selection MOVES the subtitle slot off a track that
        /// is currently confirmed on something else, the input the eager
        /// park decision turns on.
        ///
        /// Carried rather than re-read on the decider because it is a
        /// statement about the selection this one REPLACES, so its answer
        /// has to date from the decision. See `FcastPlaybin::pump_selection`
        /// for the interleaving that makes a fresh read wrong, and
        /// `generation` below for the one event that makes the carried
        /// answer wrong instead.
        replacing: bool,
        /// The item this selection was decided for
        /// ([`Inner::current_generation`] at pump time).
        ///
        /// A gapless activation is the one thing that invalidates
        /// `replacing` while this job waits: `Inner::activate_prepared_now`
        /// clears `last_applied_subtitle` for the incoming item WITHOUT
        /// bumping `queue_epoch` (S-6: the seam must not drop queued work),
        /// so a stale `replacing = true` would eagerly PARK the NEW item's
        /// live text branch, taking the incoming track off its renderer for
        /// a handover that already happened. Crossing generations therefore
        /// re-reads the answer instead of trusting it.
        generation: u64,
        /// When the pump enqueued this job, for the queue-delay counter (see
        /// [`FcastPlaybin::dispatch_queue_age`]). Diagnostic only.
        queued: QueuedAt,
    },
    /// A hands lane has finished with an effect (see the [`hands`] module).
    /// Exactly one of these follows every enqueue, and it is the ONLY way a
    /// lane reports anything. The decider retires the in-flight entry and
    /// acts on the outcome, so no lane ever writes the crate state an
    /// outcome implies.
    EffectDone {
        id: EffectId,
        outcome: Outcome,
    },
}

/// The lane's undo: what runs if an effect never reaches its report (see the
/// [`hands`] module, rule 4).
///
/// A body that UNWINDS (a panic in an effect, or in the GStreamer callbacks
/// it re-enters) would otherwise leave the id in the in-flight table for the
/// life of the instance. The tick would warn about a lane that is in fact
/// idle, and the selection deadline would defer against a send that already
/// died.
///
/// The shipping receiver builds `panic = "abort"`, so this is a DEV AND TEST
/// guarantee. The test suites panic on purpose, and one poisoned table entry
/// would silently change what every later assertion in that instance
/// measures.
struct EffectGuard<'a> {
    inner: &'a Arc<Inner>,
    id: EffectId,
    /// Taken by [`Self::disarm`] on the reporting path. Still present on an
    /// unwind, which is exactly the condition `Drop` acts on.
    owed: Option<hands::LaneFallback>,
}

impl EffectGuard<'_> {
    /// The body reported for itself. There is nothing left to undo.
    fn disarm(mut self) {
        self.owed = None;
    }
}

impl Drop for EffectGuard<'_> {
    fn drop(&mut self) {
        let Some(owed) = self.owed.take() else { return };
        error!(
            id = self.id,
            "a hands lane abandoned an effect without reporting it; settling what it owed"
        );
        Inner::run_lane_fallback(self.inner, Some(self.id), owed);
    }
}

/// A [`Job`] stamped with the queue-supersession epoch that was current when
/// it was enqueued (see [`Inner::queue_epoch`]). The stamp is compared once,
/// at the top of [`FcastPlaybin::run_job`], against the epoch the worker
/// finds when it actually gets to the job.
pub(crate) struct QueuedJob {
    pub(crate) epoch: u64,
    pub(crate) job: Job,
}

/// What a stale stamp means for a job kind (see [`stale_policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StalePolicy {
    /// Exempt. Either the variant carries its OWN token and re-validates at
    /// execution (strictly stronger than a queue epoch), or it is idempotent
    /// against whatever world it finds, or a caller is blocked on its
    /// completion and dropping it would strand that caller.
    Run,
    /// Runs, but says so. For variants whose intent legitimately outlives an
    /// item change, so that a wrong drop would be user-visible (a lost pause)
    /// while a stale run is at worst a transient the next job corrects. The
    /// log is the evidence base for ever promoting one of these to `Drop`.
    LogAndRun,
    /// Dropped. Running it would apply an intent formed for one item to the
    /// item that replaced it.
    Drop,
}

/// The staleness policy of every job kind.
///
/// EXHAUSTIVE on purpose (no `_` arm). A new variant must consciously say
/// whether its intent survives a load or a stop, rather than inherit an
/// answer. The drop set is deliberately tiny, because a WRONGLY dropped job
/// is the one new failure class this gate can introduce, and for most work
/// generation-freedom is the correct behavior. Transport intents are
/// re-driven by the caller across items, and the id/epoch/seqnum families
/// already carry a sharper token than "some load happened since".
pub(crate) fn stale_policy(job: &Job) -> StalePolicy {
    match job {
        // A superseded load re-wires an item the caller has already moved
        // past. After a SYNC load or stop it resurrects one that was torn
        // down. Nothing is stranded: only a newer load or stop can make it
        // stale, that newer Load is stamped after the bump and still runs,
        // and the caller correlates by generation.
        Job::Load { .. } => StalePolicy::Drop,
        // Drives Paused->Playing unconditionally. Queued for the item that
        // lost its clock, executed after a load or stop, it forces a stopped
        // or freshly loading pipeline upward uninvited. Fire-and-forget, so
        // no caller is stranded, and a fresh load elects a clock at preroll
        // anyway.
        Job::RecoverClock => StalePolicy::Drop,
        // An attach that outlives its item hangs the previous item's subtitle
        // URL onto the new one, a cross-item ghost external, which suppresses
        // refresh seeks, refuses prepares and can wedge selection. The attach
        // meant for the NEW item is issued after the load and carries the
        // post-bump stamp, so it is unaffected.
        Job::AttachSub { .. } => StalePolicy::Drop,
        // A selection formed for one item names that item's collection. Sent
        // after a load or a stop it either bounces (ids nothing carries) or,
        // worse, applies streams the caller has already moved past, and it
        // would do the outgoing item's eager text-branch surgery on the new
        // item's branch on the way. The item that replaced it dispatches its
        // own selection, stamped after the bump. Unlike the other three
        // drops, this one is REPORTED rather than silent: `run_job` calls
        // `SelectionEngine::dispatch_failed` for the seqnum on the drop path,
        // because the engine recorded the wait when the caller decided and
        // nothing else would ever answer it.
        Job::DispatchSelection { .. } => StalePolicy::Drop,

        // Transport intent, re-driven per item by the caller. A pause queued
        // across a gapless boundary or a load is still WANTED. FIFO already
        // orders it against queued loads and stops.
        Job::SetState { .. } => StalePolicy::LogAndRun,
        // The caller owns the seek queue and waits for exactly one of
        // RateChanged/SeekFailed/QueueSeek per dispatched seek. A silent drop
        // strands that slot. The settled-PAUSED guard plus the QueueSeek
        // handback in its own `run_job` arm IS this variant's validation.
        Job::Seek(_) => StalePolicy::LogAndRun,
        // Idempotent read-and-redistribute against the CURRENT topology. A
        // stale one computes a valid answer, a dropped one leaves sinks on a
        // stale latency.
        Job::RecalculateLatency => StalePolicy::LogAndRun,
        // Already guarded, and by the counter that belongs to the gapless
        // seam (`generation != next_generation`, plus the externals and
        // performed-swap refusals). Layering a drop on top would fight the
        // seam; log only.
        Job::PrepareNext { .. } => StalePolicy::LogAndRun,
        // Re-checks routing for a linked video entry and no-ops when the
        // world moved on. That re-check is the guard; log for forensics.
        Job::VideoChainGone => StalePolicy::LogAndRun,

        // Stop IS the supersession event, and `done` is a shutdown barrier.
        // Dropping it deadlocks shutdown.
        Job::Stop { .. } => StalePolicy::Run,
        // Its own seqnum supersession plus a four-part precondition re-check
        // at execution, which also catches invalidations a load epoch cannot
        // see (buffering, a fresh external attach).
        Job::RefreshSeek { .. } => StalePolicy::Run,
        // Id-keyed and idempotent: a stale detach finds no matching id and
        // no-ops. Never drop. A dead external left attached is the
        // ghost-stream wedge.
        Job::DetachSub { .. } => StalePolicy::Run,
        // The per-input `epoch` these carry is incarnation-scoped and
        // strictly stronger than a load epoch. Inputs are re-armed WITHIN one
        // item, and a load clears `routing.inputs` outright, so a stale epoch
        // can never match across loads either.
        Job::FailSub { .. }
        | Job::CheckSub { .. }
        | Job::RetrySub { .. }
        | Job::AdoptSubState { .. }
        | Job::VerifyReplay { .. }
        | Job::ReplaySub { .. } => StalePolicy::Run,
        // Read-only element walk whose `done` blocks the inspector.
        Job::DumpGraph { .. } => StalePolicy::Run,
        // Touches nothing at all, and a caller is parked on `done`. A drop
        // would strand that caller for the length of its own bound, which is
        // the one thing a barrier must never do.
        Job::Barrier { .. } => StalePolicy::Run,
        // Outcome-driven and idempotent. `notify: true` callers await exactly
        // one outcome event, so a drop strands the cancel matrix.
        Job::CancelPrepared { .. } => StalePolicy::Run,
        // The seam's own machinery. It reads the CURRENT generation at
        // execution as its partition pivot, by design. Any generation compare
        // in front of it fights that design.
        Job::FinishActivation => StalePolicy::Run,
        // Re-derives everything from the current pads under the routing lock.
        // Stale input state cannot survive into the operation.
        Job::SyncTextRunningTime => StalePolicy::Run,
        // An edge-triggered POKE whose payload is re-read from the deferred
        // collections at execution (which loads and teardowns clear), with
        // its own settled-PLAYING re-check. Dropping a poke recreates the
        // "work sits forever" latch. Also the noisiest variant, so exempting
        // it keeps the stale-log signal clean.
        Job::DrainTextWork => StalePolicy::Run,
        // Re-reads the pipeline's latched FAILURE state and no-ops when
        // clear. Never drop. A dropped unlatch means every later commit is
        // silently refused for good.
        Job::ClearStateFailure => StalePolicy::Run,
        // Self-validating by construction: a load or a stop RESETS the
        // selection engine, so a fire stamped before one finds nothing in
        // flight for its seqnum and returns without touching anything. A
        // wrongly dropped fire is a deadline that never fires again for that
        // dispatch.
        Job::SelectionDeadline { .. } | Job::RefreshDeadline { .. } => StalePolicy::Run,
        // Self-validating in full: the policy re-reads the pipeline state,
        // the engine's applied selection and the routing table at execution.
        // Dropping one recreates the missed-join latch, a routed text stream
        // whose only remaining trigger was the dropped poke.
        Job::PollTextPolicy => StalePolicy::Run,
        // Table hygiene must ALWAYS happen. A dropped `Done` leaves an
        // in-flight entry nothing will ever retire, which reads to the tick
        // as a wedged lane and to the selection deadline as a send that has
        // not happened yet. The carried actions are safe against a superseded
        // item because each carries its own sharper token (seqnum for the
        // engine, id + epoch for the replay). The replay half also carries an
        // owed hold release, and a superseded item's external is exactly as
        // held as a current one's.
        Job::EffectDone { .. } => StalePolicy::Run,
    }
}

/// What the poison gate owes the caller of a job it refuses (see the gate at
/// the top of [`FcastPlaybin::run_job`]). Carries the payload so the decision
/// is one exhaustive place and can be pinned without a wedged pipeline.
pub(crate) enum Refusal {
    /// Nobody waits on it. Fire-and-forget transport intent, idempotent
    /// hygiene, or work that re-derives everything at execution.
    Nothing,
    /// A blocking barrier the caller is parked on.
    Barrier(Callback),
    /// An EMPTY graph snapshot rather than a walked one: the walk reads live
    /// element state on a pipeline mid-descent on another thread, the one
    /// thing this gate exists not to do.
    Snapshot(SnapshotCallback),
    /// The caller-visible event that reports the refusal. `generation`
    /// overrides the current stamp, for a load that never adopted its own.
    Event {
        event: PlaybinEvent,
        generation: Option<u64>,
    },
    /// The selection engine recorded the wait when the CALLER decided to
    /// dispatch, so only a report on its own side can release it.
    DispatchFailed(gst::Seqnum),
    /// Same, for a refresh seek's in-flight slot.
    RefreshFailed(gst::Seqnum),
}

/// What a poisoned crate owes each job kind's caller.
///
/// EXHAUSTIVE on purpose, like [`stale_policy`]: a new variant must say
/// whether someone is left waiting on it, because a refusal that settles
/// nothing moves the wedge from the worker to the callers. The settlement is
/// the one the variant's ORDINARY failure path already emits, so no caller
/// learns a new vocabulary from the poison.
pub(crate) fn poison_refusal(job: Job) -> Refusal {
    match job {
        // The shutdown barrier (`shutdown_async`). A barrier nobody signals
        // is exactly the hang the driver reports.
        Job::Stop { done, .. } => match done {
            Some(done) => Refusal::Barrier(done),
            None => Refusal::Nothing,
        },
        Job::DumpGraph { done } => Refusal::Snapshot(done),
        // A parked caller, exactly like the shutdown barrier above. Nothing
        // else would ever release it.
        Job::Barrier { done } => Refusal::Barrier(done),
        // An async load's caller waits for `Loaded` or the loud load-failure
        // `Error` and has nothing else to learn from (the pipeline this
        // would have errored on is the wedged one). Stamped with THIS job's
        // generation for the reason the ordinary arm gives at length: the
        // current one still names the item being replaced, and the caller's
        // load-scope gate would drop the very report it waits for.
        Job::Load {
            input, generation, ..
        } => Refusal::Event {
            event: PlaybinEvent::Error {
                origin: ErrorOrigin::Main,
                error: gst::glib::Error::new(
                    gst::LibraryError::Failed,
                    "loading the media failed: the pipeline is descending below the crate",
                ),
                failed_uri: failed_uri_of(&input),
            },
            generation: Some(generation),
        },
        // The caller owns a one-in-flight seek slot and waits for exactly one
        // of RateChanged/SeekFailed/QueueSeek; a silent drop strands it.
        Job::Seek(_) => Refusal::Event {
            event: PlaybinEvent::SeekFailed,
            generation: None,
        },
        Job::RefreshSeek { seqnum } => Refusal::Event {
            event: PlaybinEvent::RefreshSeekFailed { seqnum },
            generation: None,
        },
        // The cancel matrix awaits exactly one outcome event. Nothing was
        // cancelled, hence no generation. A self-cancel notifies nobody.
        Job::CancelPrepared { notify, .. } => {
            if notify {
                Refusal::Event {
                    event: PlaybinEvent::PreparedCancelled { generation: None },
                    generation: None,
                }
            } else {
                Refusal::Nothing
            }
        }
        // `prepare_next_async` hands back a generation the caller correlates
        // against exactly one of PreparedActivated/PreparedFailed.
        Job::PrepareNext { generation, .. } => Refusal::Event {
            event: PlaybinEvent::PreparedFailed { generation },
            generation: None,
        },
        // The attach's one caller-visible outcome, as on the epoch gate's
        // drop path.
        Job::AttachSub { id, .. } => Refusal::Event {
            event: PlaybinEvent::ExternalSubtitleFailed { id },
            generation: None,
        },
        // The engine's waits. `DispatchSelection` is the send that will not
        // happen; the deadline is the only thing that would ever have noticed
        // it, and it is refused here too, so it must settle in its place.
        // Both are seqnum-guarded on the engine's side, so a wait that has
        // since confirmed is a no-op.
        Job::DispatchSelection { seqnum, .. } | Job::SelectionDeadline { seqnum } => {
            Refusal::DispatchFailed(seqnum)
        }
        Job::RefreshDeadline { seqnum } => Refusal::RefreshFailed(seqnum),

        // Nobody waits. Transport intent the caller re-drives, id/epoch-keyed
        // work whose input is gone with the pipeline, and internal hygiene
        // that re-derives everything at execution.
        Job::SetState { .. }
        | Job::RecoverClock
        | Job::RecalculateLatency
        | Job::DetachSub { .. }
        | Job::FailSub { .. }
        | Job::CheckSub { .. }
        | Job::RetrySub { .. }
        | Job::AdoptSubState { .. }
        | Job::VerifyReplay { .. }
        | Job::ReplaySub { .. }
        | Job::FinishActivation
        | Job::SyncTextRunningTime
        | Job::DrainTextWork
        | Job::VideoChainGone
        | Job::ClearStateFailure
        | Job::PollTextPolicy
        // A lane's report retires an in-flight table entry, and the table
        // dies with the instance a poisoned teardown is ending.
        | Job::EffectDone { .. } => Refusal::Nothing,
    }
}

/// The URI a load failure should name, for the report that makes it
/// actionable. The load consumes its input, so this is read before it runs.
fn failed_uri_of(input: &MediaInput) -> Option<String> {
    match input {
        MediaInput::Uri(uri) => Some(uri.clone()),
        MediaInput::Element(element) => element
            .dynamic_cast_ref::<gst::URIHandler>()
            .and_then(|handler| handler.uri())
            .map(|uri| uri.to_string()),
    }
}

// The consumer transport has no shared text-sink entry to contend for. Each
// live text branch ends in its OWN appsink, built and destroyed with the
// branch, so a disposal's check-then-flush cannot race a link: the branch it
// disposes of is already out of the routing table. The one-live-branch rule
// is stated by `consumer_branch_live` in [`Inner::poll_text_policy`] rather
// than inherited from the graph.

/// One armed bounded timer, waiting in [`Inner::pending_timers`] until
/// [`Inner::run_tick`] finds it due and queues its job.
///
/// The table replaces one one-shot sleeper THREAD per arm. Those could fail
/// to spawn, and a failed spawn is not merely a lost check. The replay
/// verification's dedupe flag stays set on the input for good (see
/// [`crate::external::ExternalInput::verification_armed`]), so no later
/// verification for that incarnation can ever be armed either. A push into a
/// `Vec` cannot fail that way.
pub(crate) struct TimerEntry {
    due: Instant,
    job: TimerJob,
}

/// What a due [`TimerEntry`] queues. A subset of [`Job`] rather than a `Job`
/// because only these two are timer-armed, and because `Job` carries variants
/// (the `DumpGraph` callback) that could not sit in a table at all.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TimerJob {
    VerifyReplay {
        id: ExternalSubId,
        epoch: u32,
        attempt: u32,
    },
    CheckSub {
        id: ExternalSubId,
        epoch: u32,
    },
}

impl TimerJob {
    /// The worker job this timer stands for.
    fn job(self) -> Job {
        match self {
            TimerJob::VerifyReplay { id, epoch, attempt } => {
                Job::VerifyReplay { id, epoch, attempt }
            }
            TimerJob::CheckSub { id, epoch } => Job::CheckSub { id, epoch },
        }
    }
}

/// A queued `SELECT_STREAMS` (see [`FcastPlaybin::select_streams`]). Sent
/// from a dedicated thread, NOT the crate worker. A wedged send must not
/// block the queued Stop/Load whose flush is what releases such a wedge.
pub(crate) struct SelectJob {
    /// The decodebin3 the selection was built against. The sender skips the
    /// job if a core swap superseded it (the selection could never confirm).
    pub(crate) db3: gst::Element,
    /// Where the event goes: decodebin3 normally; the MAIN input when
    /// upstream owns selection (an adaptive demuxer rejects any event that
    /// names a foreign stream, and an external input's pads poison
    /// `gst_bin_send_event`'s AND-fold through decodebin3).
    pub(crate) target: gst::Element,
    pub(crate) event: gst::Event,
    /// The selected ids, kept for the video-deselect check after the send.
    pub(crate) stream_ids: Vec<String>,
    /// The TEXT stream this selection names, when it names one. Carried for
    /// the re-select drain interlock in [`Inner::send_select_streams`], the
    /// only thing that needs to tell the subtitle id apart from the rest of
    /// `stream_ids` (a bare id says nothing about its kind). `None` for
    /// every send that selects no subtitle, and for the public
    /// [`FcastPlaybin::select_streams`], whose caller names raw ids.
    pub(crate) text_sid: Option<String>,
}

/// A queued external-subtitle replay seek (see
/// [`FcastPlaybin::replay_subtitle`]). Sent from a dedicated thread for the
/// same reason as [`SelectJob`]. A
/// FLUSHING seek is performed INLINE by the source on the sending thread,
/// which pushes `FLUSH_START` down the whole live graph from there. When that
/// reaches a `queue` whose src task is parked behind a sink in
/// `gst_base_sink_wait_preroll`, the queue's own handler calls
/// `gst_pad_pause_task` and the sender waits on that task's stream lock. The
/// only thing that ends such a preroll wait is the pipeline reaching PLAYING,
/// and the only thread that can carry [`Job::SetState`] is the worker, so
/// sending from the worker closes a cycle through the worker itself.
pub(crate) struct ReplayJob {
    /// The input's source pads, resolved on the worker before the handover
    /// so the send never re-enters the routing lock.
    pub(crate) pads: Vec<gst::Pad>,
    pub(crate) seek: gst::Event,
    pub(crate) id: ExternalSubId,
    pub(crate) epoch: u32,
    pub(crate) attempt: u32,
    /// Log-only, so the replay's line reads exactly as it did when the send
    /// was inline on the worker.
    pub(crate) origin: gst::ClockTime,
    pub(crate) rate: f64,
    /// The pipeline whose START TIME the seek's own `FLUSH_STOP` would
    /// otherwise reset (see [`StartTimeGuard`]). `None` only in the hands unit
    /// tests, which never reach a graph.
    pub(crate) pipeline: Option<gst::Pipeline>,
}

/// A queued chain join: the blocking half of [`Inner::route_db3_pad`] (see
/// [`Inner::run_chain_join`]).
///
/// Sent from a dedicated thread for the same reason as [`SelectJob`] and
/// [`ReplayJob`], and with one extra constraint that rules the crate worker
/// out entirely: `Inner::apply_start_seek` waits for the preroll on the
/// worker (`pipeline.state(PREROLL_TIMEOUT)`), and the preroll cannot finish
/// until this join has run. Queuing it behind that wait would be the
/// postponed-work-blocked-by-its-own-drain shape a seventh time.
pub(crate) struct ChainJoinJob {
    /// The decodebin3 the route was made against. A core swap makes the join
    /// stale (see `Inner::run_chain_join`).
    pub(crate) db3: gst::Element,
    /// The routed decodebin3 source pad, to re-check that the stream is still
    /// routed by the time the join runs.
    pub(crate) pad: gst::Pad,
    pub(crate) kind: StreamKind,
    /// The blocking probe holding this stream at the streamsynchronizer src
    /// pad until the chain is up. Released by the join, whatever its outcome,
    /// and by the lane's undo if the join never runs (see [`hands::JoinHold`]).
    pub(crate) hold: hands::JoinHold,
}

impl Inner {
    /// The generation the NEXT load will run under (see
    /// [`FcastPlaybin::load_async`]).
    pub(crate) fn allocate_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The generation of the current load (adopted at its reset point).
    pub(crate) fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Keep the longest dispatch queue delay seen (see
    /// [`Counters::dispatch_queue_age_us`]). Monotone maximum rather than a
    /// last-value, so a single bad hop cannot be averaged away by the
    /// instant ones that follow it.
    pub(crate) fn record_dispatch_queue_age(&self, age: Duration) {
        let micros = age.as_micros().min(u128::from(u64::MAX)) as u64;
        self.counters
            .dispatch_queue_age_us
            .fetch_max(micros, Ordering::Relaxed);
    }

    /// Mark everything currently queued as belonging to a superseded item
    /// (see [`Inner::queue_epoch`]). Called FIRST by the five APIs whose
    /// effect replaces the item queued work was formed for: the sync and
    /// async loads, the sync and async stops, and the shutdown. Calling it
    /// before enqueueing the load or stop itself is what keeps that job on
    /// the near side of its own supersession.
    pub(crate) fn supersede_queued_work(&self) {
        self.queue_epoch.fetch_add(1, Ordering::SeqCst);
        // The item whose cues the consumer is holding is the item being
        // replaced. Half of the `Clear` protocol's driver side, and the half
        // no pad probe can stand in for: a load that never gets as far as
        // flushing the old text branch still invalidates every cue it
        // delivered. See [`SubtitleFeedItem::Clear`].
        self.send_subtitle_clear();
    }

    /// Stamp a job with the current [`queue_epoch`](Inner::queue_epoch) and
    /// hand it to the worker. THE single enqueue point: a job that reached
    /// the worker unstamped could not be judged at all. The bounded timers
    /// ([`FcastPlaybin::arm_sub_watchdog`] and
    /// [`Inner::arm_replay_verification`]) come through here too, stamped when
    /// the tick finds them due (see [`TimerEntry`]).
    ///
    /// Returns whether the job is on its way. Only the callers that hand a
    /// duty (an owed hold release) to the job they just queued care.
    pub(crate) fn queue_job(&self, job: Job) -> bool {
        let epoch = self.queue_epoch.load(Ordering::SeqCst);
        // Counted HERE rather than at the emitters, so no emitter can be
        // added later that the count misses (see [`REPLAY_JOBS_QUEUED`]).
        if matches!(job, Job::ReplaySub { .. }) {
            REPLAY_JOBS_QUEUED.fetch_add(1, Ordering::Relaxed);
        }
        // Send can only fail if the worker died (it holds the receiver for
        // as long as it runs), and the pipeline is unusable then anyway.
        if self.work_tx.send(QueuedJob { epoch, job }).is_err() {
            error!("fcastplaybin worker is gone; dropping the job");
            return false;
        }
        true
    }

    /// [`Inner::queue_job`]'s counterpart for the hands (see the [`hands`]
    /// module): stamp the effect with the current queue epoch, register it in
    /// flight and hand it to its lane. THE single enqueue point for the three
    /// effects, the way `queue_job` is for jobs.
    ///
    /// The epoch is stamped here and CHECKED on the lane, immediately before
    /// the irreversible send. `Err` returns the effect for the caller's
    /// inline fallback.
    pub(crate) fn enqueue_effect(&self, effect: Effect) -> std::result::Result<EffectId, Effect> {
        self.hands
            .enqueue(effect, self.queue_epoch.load(Ordering::SeqCst))
    }

    /// The ownership proof: this text-branch surgery is running on the
    /// DECIDER (see [`Inner::worker_loop`] and [`Inner::decider`]).
    ///
    /// The disposal-versus-link TOCTOU is removed by making one thread
    /// decide, and an ownership argument that is only argued decays the
    /// first time a new call site is added from a callback. So the three
    /// decider-owned sites say so here: the policy's surgery section
    /// ([`Inner::poll_text_policy`], seat reclaim, eviction and the link
    /// loop), the eager park ([`Inner::park_text_streams`]) and the
    /// postponed disposals ([`Inner::run_deferred_text_work`]). A debug
    /// build panics if any of them is ever reached from anywhere else.
    ///
    /// WHAT IS DELIBERATELY NOT COVERED, each because it is a different
    /// question than "who decides about the seat":
    ///
    /// * The TEARDOWN boundary. [`Teardown::run`] and
    ///   [`Inner::drain_disposals_for_teardown`] dispose from `fpb-teardown`,
    ///   from a caller's synchronous `stop`, or from whichever thread
    ///   [`Inner::drop`] lands on. `Inner` is gone or dying there and the link
    ///   side cannot run at all, so there is nothing for this assertion to
    ///   protect.
    /// * The LOAD boundary (`teardown_core`, `remove_all_inputs`), which runs
    ///   on the caller under the route gate with the pipeline at READY: no
    ///   routing, so nothing to race.
    /// * The pad-removed detach ([`Inner::unroute_db3_pad`]'s Text arm). That
    ///   is an OBSERVED eviction rather than a decision (the pad is going away
    ///   now, whoever asked), and its blocking half is already handed to the
    ///   decider through `deferred_text_disposal`.
    ///
    /// Worker-thread ownership is unconditional, so a violation is a defect
    /// with no arm that excuses it: a debug build panics, a release build
    /// leaves the WARN as the field signal.
    #[track_caller]
    pub(crate) fn decider_only(&self, what: &'static str) {
        // Before the worker has named itself nothing has been decided either,
        // so there is nothing to be off.
        let Some(decider) = self.decider.get() else {
            return;
        };
        if std::thread::current().id() == *decider {
            return;
        }
        warn!(
            what,
            thread = std::thread::current().name().unwrap_or("<unnamed>"),
            "text-branch surgery ran off the deciding thread"
        );
        debug_assert!(false, "{what} ran off the deciding thread");
    }

    /// THE DECIDER (see [`Job`]).
    ///
    /// This thread is not merely "the worker": it is the one thread that
    /// DECIDES. Every text-branch decision runs here (the link loop, the
    /// seat reclaim and eviction, the eager park, the postponed disposals),
    /// and so does every selection dispatch and every effect outcome the
    /// [`hands`] lanes report back. What stays off it is stated
    /// in three places and nowhere else: the lanes (three sends that must not
    /// be able to block a queued Stop, see the [`hands`] module), the tick
    /// (its clock must survive a wedged decider, see [`Inner::run_tick`]) and
    /// the teardown/rescue paths (which must run while this thread is wedged,
    /// see [`Teardown`] and [`WakeRescue`]). The topology RECORDING stays
    /// inline on streaming threads, because it has to be visible the moment
    /// `send_event` returns. The DECISIONS it triggers are messages to here.
    ///
    /// Holds only a `Weak` between jobs so it never keeps the pipeline alive,
    /// and exits when every handle is gone (the channel closes). If a job's
    /// temporary upgrade turns out to be the LAST strong ref, `Inner::drop`
    /// (pipeline to NULL) simply runs here after the job. THIS thread is a
    /// safe one for it, because nothing GStreamer owns is waiting on it. That
    /// is a property of this thread and NOT of temporary upgrades in general.
    /// The bus sync handler runs on whichever STREAMING thread posted the
    /// message, and NULLing the pipeline there deadlocks the descent against
    /// the caller's own task. See [`Inner::drop`], which decides from the
    /// thread it finds itself on and hands the descent off when that thread
    /// is one the crate does not own.
    pub(crate) fn worker_loop(weak: Weak<Inner>, work_rx: mpsc::Receiver<QueuedJob>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        // Name this thread as the decider BEFORE the first job, so the
        // ownership assertions (see [`Inner::decider_only`]) have an answer
        // from the crate's first moment rather than from its first job. The
        // upgrade cannot be the last strong reference here (`Inner::new`
        // holds one until it returns the handle) and is released immediately.
        if let Some(inner) = weak.upgrade() {
            let _ = inner.decider.set(std::thread::current().id());
        }

        while let Ok(queued) = work_rx.recv() {
            let Some(inner) = weak.upgrade() else { break };
            let QueuedJob { epoch, job } = queued;
            // `DrainTextWork` at TRACE, everything else at DEBUG. The tick's
            // reconcile trigger queues one per second for as long as an item
            // is live, by design, which would drown DEBUG logs. It is still
            // the only record that the trigger is alive, so it is demoted
            // rather than deleted.
            if matches!(job, Job::DrainTextWork) {
                trace!(?job, epoch, "Got job");
            } else {
                debug!(?job, epoch, "Got job");
            }
            FcastPlaybin { inner }.run_job(QueuedJob { epoch, job });
        }

        debug!("fcastplaybin worker finished");
    }

    /// The periodic tick thread. Same lifetime discipline as `worker_loop`
    /// (only a `Weak` between ticks), but driven by a timeout rather than by
    /// work. `hangup` never carries a message, it only closes when `Inner`
    /// drops its sender, and the thread then exits within one interval.
    ///
    /// Nothing joins this thread, and nothing needs to. [`Inner::run_tick`]
    /// only queues jobs, and those queue BEHIND whatever teardown is already
    /// in the worker's queue and re-validate themselves when they run.
    pub(crate) fn tick_loop(weak: Weak<Inner>, hangup: mpsc::Receiver<()>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        loop {
            if hangup.recv_timeout(TICK_INTERVAL) != Err(mpsc::RecvTimeoutError::Timeout) {
                // A hangup (nobody ever sends). `Inner` is going away.
                break;
            }
            let Some(inner) = weak.upgrade() else { break };
            Inner::run_tick(&inner, Instant::now());
            // The strong ref dies here, once per interval. If it is ever the
            // LAST one, `Inner::drop` runs on THIS thread, which is safe for
            // exactly the worker's reason (see `worker_loop`).
        }

        debug!("fcastplaybin tick finished");
    }

    /// One tick: the crate's only periodic work.
    ///
    /// The discipline here is what makes a tick safe against a wedged
    /// pipeline, and it is deliberately narrow. This function takes crate
    /// mutexes ONE AT A TIME, touches no GStreamer object at all (not even a
    /// state read, which can block on a STATE_LOCK the wedge holds), and
    /// carries out no action inline. It only decides that something is DUE
    /// and queues a [`Job`]; every probe and every blocking operation belongs
    /// to the worker.
    fn run_tick(inner: &Arc<Inner>, now: Instant) {
        let tick = inner.tick_count.fetch_add(1, Ordering::SeqCst);

        // (1) Selection and refresh deadlines. The engine says only WHICH
        //     wait ran out and re-arms it. What a lapsed wait MEANS needs the
        //     pipeline, so the decision is a job (see `Job::SelectionDeadline`
        //     and `Inner::selection_deadline_fired`). Re-arming bounds the
        //     fire rate: a slow worker cannot accumulate a queue of fires.
        //
        //     The re-arm period is the SHORTER of the two configured
        //     deadlines, so neither family is looked at less often than it
        //     asked to be.
        //
        //     Both come out of ONE `Deadlines` guard and are combined after
        //     it is dropped, which is one mutex held once, as this function's
        //     discipline requires.
        let deadlines = *inner.deadlines.lock();
        let rearm = deadlines.selection.min(deadlines.refresh);
        let fires = inner.selection.lock().due_deadlines(now, rearm);
        for fire in fires {
            let job = match fire {
                selection::DeadlineFire::Selection(seqnum) => Job::SelectionDeadline { seqnum },
                selection::DeadlineFire::Refresh(seqnum) => Job::RefreshDeadline { seqnum },
            };
            debug!(?job, "a selection wait ran out of time");
            Counters::bump(&inner.counters.deadline_fires);
            inner.queue_job(job);
        }

        // (2) Bounded timers whose moment has come (see `TimerEntry`).
        //     Unlike the sleepers these replace, the enqueue stamps the
        //     CURRENT queue epoch rather than the arm-time one. Inert either
        //     way: both timer jobs are `StalePolicy::Run` and carry the
        //     sharper per-input epoch as their real token.
        let due = {
            let mut timers = inner.pending_timers.lock();
            let mut due = Vec::new();
            timers.retain(|entry| {
                if entry.due <= now {
                    due.push(entry.job);
                    return false;
                }
                true
            });
            due
        };
        for job in due {
            debug!(?job, "a bounded timer came due");
            inner.queue_job(job.job());
        }

        // (3/3b) The 1 Hz text pokes, gated on LIVENESS so an idle crate
        //     queues nothing (see `decisions::replay::tick_pokes`, which is
        //     the whole interlock).
        //
        //     The READS STAY HERE, one mutex at a time, because this thread's
        //     clock must survive a wedged decider. Each is skipped once an
        //     earlier one has decided the answer, and skipped entirely off a
        //     poke tick.
        //
        //     Every divergence the reconcile pass can act on needs an input
        //     carrying the selected sid, and every input the crate owns is in
        //     `routing.inputs` from before its first pad until teardown, so
        //     item live => `holds_an_item` => poked.
        //
        //     `has_deferred_text_work` is read ONCE and reused. It is both an
        //     arm of the liveness union and the discriminator between the two
        //     pokes, and reading it twice could see it change and fire both.
        let mut facts = decisions::replay::TickFacts::default();
        if decisions::replay::repoke_due(tick) {
            facts.deferred_work = inner.has_deferred_text_work();
            facts.holds_an_item = !facts.deferred_work && inner.holds_an_item();
            facts.unconverged = !facts.deferred_work
                && !facts.holds_an_item
                && inner.selection.lock().unconverged();
        }
        match decisions::replay::tick_pokes(tick, facts) {
            decisions::replay::Pokes::None => {}
            decisions::replay::Pokes::DrainDeferred => {
                debug!("re-poking the postponed-work drain from the tick");
                inner.queue_job(Job::DrainTextWork);
            }
            // DELIBERATELY `Job::DrainTextWork` and not
            // `request_text_policy_poll`. The reconcile pass lives at the
            // drain's tail, and a text-policy poll would not run it.
            decisions::replay::Pokes::DrainReconcile => {
                inner.queue_job(Job::DrainTextWork);
            }
        }

        // (4) Wedged effects (see the `hands` module). SUPERVISION, not
        //     self-rescue. A lane parked in a send is doing exactly what its
        //     thread exists to absorb; this only makes the wedge VISIBLE, in
        //     logs here and to `Inner::selection_deadline_fired`, which
        //     consults the same table before it adopts a routed reality.
        //     Once per effect, not once per tick.
        for wedged in inner.hands.wedged(now, EFFECT_WEDGE_WARN) {
            warn!(
                id = wedged.id,
                lane = ?wedged.lane,
                age = ?wedged.age,
                "an effect has been in flight far too long; its lane is wedged"
            );
        }
    }

    /// Arm a bounded timer (see [`TimerEntry`]). Infallible, which is the
    /// whole point of the table.
    pub(crate) fn arm_timer(&self, due: Instant, job: TimerJob) {
        debug!(?job, "arming a bounded timer on the tick");
        self.pending_timers.lock().push(TimerEntry { due, job });
    }

    /// Drop every armed timer, where the other per-item deferral slots are
    /// cleared.
    ///
    /// Releasing the replay bookkeeping is NOT optional hygiene. A dropped
    /// `VerifyReplay` that left its arming flag behind poisons that input
    /// incarnation exactly the way a failed sleeper spawn used to (see
    /// [`TimerEntry`]), and a dropped `ReplaySub` that left its in-flight bit
    /// behind suppresses every future reconcile pass for that resource.
    ///
    /// Both are per-resource flags now, and this drops EVERY timer and every
    /// queued job, so the release is one walk of the attached externals rather
    /// than a per-entry undo. The pending-timer guard is released before the
    /// routing lock is taken, because `arm_replay_verification` takes them the
    /// other way round.
    pub(crate) fn clear_pending_timers(&self) {
        self.pending_timers.lock().clear();
        let mut routing = self.routing.lock();
        for external in routing
            .inputs
            .iter_mut()
            .filter_map(|input| input.external.as_mut())
        {
            external.replay_inflight = false;
            external.verification_armed = false;
        }
        self.replaying_externals.lock().clear();
    }

    /// One hands lane (see the [`hands`] module): receive an envelope,
    /// revalidate it, run the effect, report exactly one outcome. Same
    /// lifetime discipline as `worker_loop`: only a `Weak` between effects,
    /// exits when the channel closes. One body for all three lanes, so which
    /// lane a thread is running is data (see `FcastPlaybin::new`).
    ///
    /// BLOCKING IS ALLOWED here, and that is the point of the thread. What is
    /// NOT allowed is waiting for the decider: nothing in an effect body may
    /// depend on a job running, or the cycle each lane exists to break is
    /// closed again through the executor.
    pub(crate) fn lane_loop(weak: Weak<Inner>, lane: Lane, rx: mpsc::Receiver<hands::Envelope>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        while let Ok(envelope) = rx.recv() {
            let Some(inner) = weak.upgrade() else { break };
            let id = envelope.id;
            let current = inner.queue_epoch.load(Ordering::SeqCst);
            // Armed BEFORE the body and disarmed only by reaching the report,
            // so an effect that unwinds out of its body still settles what it
            // owes (see `EffectGuard`).
            let guard = EffectGuard {
                inner: &inner,
                id,
                owed: Some(hands::LaneFallback::of(&envelope.effect)),
            };
            let outcome = hands::execute(envelope, current, |effect| {
                Inner::run_effect(&inner, effect)
            });
            guard.disarm();
            Inner::report_effect(&inner, id, outcome);
        }

        debug!(?lane, "a fcastplaybin hands lane finished");
    }

    /// Settle an effect nobody executed (see [`hands::LaneFallback`]).
    ///
    /// `id` is `None` where the effect was never registered, i.e. where the
    /// ENQUEUE failed. There is no table entry to retire, only the physical
    /// undo to perform. Where there is one, a select is retired through the
    /// decider (the ENGINE has to hear that the dispatch is not coming) and
    /// the other two locally, because their outcomes carry no decision and
    /// inventing a `ReplaySent` for a seek nothing sent would be a lie in the
    /// log.
    pub(crate) fn run_lane_fallback(
        inner: &Arc<Inner>,
        id: Option<EffectId>,
        owed: hands::LaneFallback,
    ) {
        let retire = |id: Option<EffectId>| {
            if let Some(id) = id {
                inner.hands.complete(id);
            }
        };
        match owed {
            hands::LaneFallback::Select { seqnum } => match id {
                Some(id) => Inner::report_effect(
                    inner,
                    id,
                    Outcome::SelectSkipped {
                        seqnum,
                        reason: hands::SKIPPED_LANE_LOST,
                    },
                ),
                // The enqueue's own caller answers this one:
                // `select_streams_to` returns `Err` and the pump reports the
                // failure to the engine itself.
                None => warn!(
                    ?seqnum,
                    "a selection was abandoned before it was registered"
                ),
            },
            hands::LaneFallback::Replay { id: sub_id, epoch } => {
                // The lane is gone, so no `Outcome::ReplaySent` will ever
                // arrive and `replay_outcome` (the normal discharge) never
                // runs. Same argument as the owed hold below it: an abandoned
                // effect still has to settle everything it was carrying, and a
                // bit left set would silence the reconcile pass for this
                // resource for good (see
                // [`crate::external::ExternalInput::replay_inflight`]).
                Inner::settle_replay(inner, sub_id, epoch);
                inner.release_owed_hold(sub_id, epoch);
                retire(id);
            }
            hands::LaneFallback::Join { hold } => {
                hold.release("the join was abandoned");
                retire(id);
            }
        }
    }

    /// Execute one effect on its lane. The bodies are the v1 ones, unchanged;
    /// what moved is that each now RETURNS what happened instead of writing
    /// it into the crate from the lane.
    pub(crate) fn run_effect(inner: &Arc<Inner>, effect: Effect) -> Outcome {
        match effect {
            Effect::SelectStreams(job) => Inner::send_select_streams(inner, job),
            // The lane only SENDS; what the outcome means is decided on the
            // decider, from `Job::EffectDone` (see
            // `FcastPlaybin::replay_outcome`).
            Effect::ReplaySeek(job) => FcastPlaybin::send_replay_seek(job),
            Effect::ChainJoin(job) => {
                let kind = job.kind;
                Inner::run_chain_join(inner, job);
                Outcome::JoinFinished { kind }
            }
        }
    }

    /// Report an effect's outcome to the decider (see [`Job::EffectDone`]).
    ///
    /// The lane never acts on the outcome itself. If there is no decider left
    /// to tell (the worker channel is closed, so the crate is being torn
    /// down), the entry is retired here and whatever the outcome still owes
    /// is settled here too ([`Outcome::owed`]). For a replay that is its hold
    /// release, which no later moment can make up for. What an effect owes
    /// when the body did NOT finish is [`hands::LaneFallback`]'s job, not
    /// this one's.
    ///
    /// The recursion this reads like cannot happen. A select is the only
    /// fallback that settles THROUGH the decider, and `Outcome::owed` never
    /// yields one, precisely because the decider is what is missing here.
    fn report_effect(inner: &Arc<Inner>, id: EffectId, outcome: Outcome) {
        inner.hands.report(
            id,
            outcome,
            |outcome| inner.queue_job(Job::EffectDone { id, outcome }),
            |owed| Inner::run_lane_fallback(inner, None, owed),
        );
    }
}

impl FcastPlaybin {
    /// How many effects the hands are carrying right now (see the [`hands`]
    /// module). Zero at rest, and a test that wants to manufacture a wedged
    /// lane has no other way to know it succeeded. Not part of the public API.
    #[doc(hidden)]
    pub fn effects_in_flight(&self) -> usize {
        self.inner.hands.in_flight()
    }

    /// The longest a [`Job::DispatchSelection`] has waited between being
    /// enqueued by the pump and running on the decider. Zero on a run where
    /// the decider was never busy. The queue delay a slow switch is made of.
    /// Not part of the public API.
    #[doc(hidden)]
    pub fn dispatch_queue_age(&self) -> Duration {
        let counters = &self.inner.counters;
        Duration::from_micros(counters.dispatch_queue_age_us.load(Ordering::Relaxed))
    }

    /// Queue a pipeline state change on the worker thread. Downward
    /// transitions take the route gate there, exactly like
    /// [`set_pipeline_state`](Self::set_pipeline_state).
    pub fn set_state_async(&self, state: gst::State) {
        self.queue_job(Job::SetState { target: state });
    }

    /// Queue a [`load`](Self::load) on the worker thread. Completion is
    /// reported as [`PlaybinEvent::Loaded`]. A failed load only logs: any
    /// user-visible failure arrives through the pipeline error path.
    ///
    /// Returns the load's GENERATION. Every event is delivered together with
    /// the generation it belongs to, so the caller can drop events from
    /// superseded loads by comparing against this value: events posted by
    /// the previous item (even ones still queued when this load is
    /// requested) carry an older generation.
    pub fn load_async(&self, input: MediaInput, start: StartPoint) -> u64 {
        // Before the enqueue below, so this Load lands on the near side of
        // its own supersession while everything queued earlier goes stale.
        self.inner.supersede_queued_work();
        let generation = self.inner.allocate_generation();
        self.queue_job(Job::Load {
            input,
            start,
            generation,
        });
        generation
    }

    /// Pre-arm the next media input on the LIVE core for a gapless
    /// transition. The input element is created, added to the running
    /// pipeline, and its parsed streams link into decodebin3 alongside the
    /// current item's; when the current item drains, decodebin3 switches to
    /// the prepared streams with no state change, no flush, and no
    /// pipeline EOS in between. The switch surfaces as
    /// [`PlaybinEvent::PreparedActivated`] followed by the new item's
    /// collection and selection, all stamped with the returned generation.
    ///
    /// Constraints the caller upholds: the pipeline is in steady playback of
    /// a finite (non-live) item, and the prepared input is a plain A/V item
    /// (no start seek, images and live sources go through a normal load).
    /// A prepare while another is pending replaces it (latest wins). A
    /// normal `load`/`stop` drops any pending prepare. If the prepared
    /// input fails before activating, [`PlaybinEvent::PreparedFailed`] is
    /// emitted and playback of the current item is unaffected: its
    /// end-of-stream then arrives normally and the caller advances through
    /// its ordinary path.
    ///
    /// The same failure path also covers prepares the crate REFUSES or
    /// demotes on its own: external subtitles attached to the current item
    /// (a swap would carry them into the next item's collections), a
    /// prepare arriving while a performed swap is mid-activation, and a
    /// prepared item that lacks a stream type the current item is playing
    /// (the abandoned sink would block the next end-of-stream forever).
    ///
    /// Returns the generation the prepared item will carry once active.
    pub fn prepare_next_async(&self, input: MediaInput) -> u64 {
        let generation = self.inner.allocate_generation();
        self.queue_job(Job::PrepareNext { input, generation });
        generation
    }

    /// Drop a pending prepared next input (see
    /// [`prepare_next_async`](Self::prepare_next_async)). The caller seeked
    /// away from the end, the queue changed, or autoplay was turned off.
    /// A no-op when nothing is prepared or it already activated.
    ///
    /// The outcome comes back as exactly one event, because a cancel RACES the
    /// swap and commonly loses (the swap performs at pre-arm time for a small
    /// or cached item): [`PlaybinEvent::PreparedCancelled`] means the prepare
    /// is gone, [`PlaybinEvent::PreparedCancelDeclined`] means it is
    /// activating anyway. On the latter the caller MUST keep its pre-arm
    /// bookkeeping so the imminent [`PlaybinEvent::PreparedActivated`] is
    /// adopted rather than treated as unmatched.
    ///
    /// `after` states what the caller does to the pipeline next. It is not
    /// advisory: it decides whether the item end the gapless hold consumed
    /// is synthesized here or left to the flushing seek that follows (see
    /// [`AfterCancel`]).
    pub fn cancel_prepared_async(&self, after: AfterCancel) {
        self.queue_job(Job::CancelPrepared {
            notify: true,
            after,
        });
    }

    /// Queue a full stop on the worker thread: pipeline to READY, every
    /// input removed (its network/file resources released now, not at the
    /// next load) and the per-load audio sink dropped.
    pub fn stop_async(&self) {
        self.inner.supersede_queued_work();
        self.queue_job(Job::Stop {
            target: gst::State::Ready,
            done: None,
        });
    }

    /// Like [`stop_async`](Self::stop_async) but to NULL, invoking `done`
    /// once the teardown finished (a shutdown barrier).
    pub fn shutdown_async(&self, done: Box<dyn FnOnce() + Send>) {
        self.inner.supersede_queued_work();
        self.queue_job(Job::Stop {
            target: gst::State::Null,
            done: Some(Callback(done)),
        });
    }

    /// Queue a [`graph::snapshot`] of the pipeline graph, delivered to `done`
    /// ON THE WORKER THREAD (hand it off, do not block). Queued so the
    /// element walk cannot race a concurrent load or teardown.
    pub fn debug_graph_async(&self, done: Box<dyn FnOnce(graph::GraphSnapshot) + Send>) {
        self.queue_job(Job::DumpGraph {
            done: SnapshotCallback(done),
        });
    }

    /// Queue a no-op whose only effect is calling `done` ON THE WORKER
    /// THREAD. FIFO makes it a barrier: when `done` runs, every job queued
    /// before it has finished.
    ///
    /// For test and diagnostic drivers that need "the worker got here" and
    /// nothing else. [`debug_graph_async`](Self::debug_graph_async) answers
    /// the same question but pays a full recursive element walk for it.
    ///
    /// A worker that is already gone drops the job and with it `done`,
    /// UNCALLED, exactly as `debug_graph_async` does. A caller parked on a
    /// channel therefore learns it as a disconnect rather than as a hang.
    pub fn barrier_async(&self, done: Box<dyn FnOnce() + Send>) {
        self.queue_job(Job::Barrier {
            done: Callback(done),
        });
    }

    /// Queue a position/rate seek. If the pipeline is not settled in PAUSED
    /// the seek is handed back via [`PlaybinEvent::QueueSeek`] while the
    /// worker drives to PAUSED (the caller owns the seek queue and re-issues
    /// it once settled). Outcomes are [`PlaybinEvent::RateChanged`] and
    /// [`PlaybinEvent::SeekFailed`].
    pub fn seek_async(&self, seek: Seek) {
        self.queue_job(Job::Seek(seek));
    }

    /// Queue a flushing seek to the CURRENT position that keeps the pipeline
    /// in its current state, stamped with `seqnum` (failures come back as
    /// [`PlaybinEvent::RefreshSeekFailed`] with that seqnum). Used to force a
    /// freshly selected sparse subtitle track to re-emit its active cue. It
    /// deliberately bypasses any Paused round-trip a normal seek performs.
    pub fn refresh_seek_async(&self, seqnum: gst::Seqnum) {
        self.queue_job(Job::RefreshSeek { seqnum });
    }

    /// Queue a Paused->Playing cycle so the pipeline elects a new clock after
    /// [`PlaybinEvent::ClockLost`]. Without it every sink keeps waiting on
    /// the dead clock and playback stalls.
    pub fn recover_clock_async(&self) {
        self.queue_job(Job::RecoverClock);
    }

    /// Queue a live external-subtitle attach under a pre-reserved id
    /// ([`allocate_subtitle_id`](Self::allocate_subtitle_id)) on the worker
    /// thread. Attaching drives the source to the pipeline's state, and a
    /// source's `start()` may block on I/O. An attach that fails never
    /// produces a stream and emits no event of its own (a caller-side
    /// watchdog is the deterministic detector for that). The one exception is
    /// an attach a later load or stop supersedes before the worker runs it:
    /// that drop reports [`PlaybinEvent::ExternalSubtitleFailed`].
    pub fn attach_subtitle_async(&self, id: ExternalSubId, url: String) {
        self.queue_job(Job::AttachSub { id, url });
    }

    /// Queue a live external-subtitle detach. Best effort: the input is
    /// leaving regardless, and detaching an attach that already failed is
    /// harmless.
    pub fn detach_subtitle_async(&self, id: ExternalSubId) {
        self.queue_job(Job::DetachSub { id });
    }

    pub(crate) fn queue_job(&self, job: Job) {
        self.inner.queue_job(job);
    }

    /// Carry out what a refused job owed its caller (see [`poison_refusal`]).
    fn settle_refusal(&self, refusal: Refusal) {
        let inner = &self.inner;
        match refusal {
            Refusal::Nothing => {}
            Refusal::Barrier(done) => done.call(),
            Refusal::Snapshot(done) => done.call(graph::GraphSnapshot::default()),
            Refusal::Event { event, generation } => match generation {
                Some(generation) => inner.emit_with_generation(event, generation),
                None => inner.emit(event),
            },
            Refusal::DispatchFailed(seqnum) => inner.selection.lock().dispatch_failed(seqnum),
            Refusal::RefreshFailed(seqnum) => inner.selection.lock().refresh_failed(seqnum),
        }
    }

    /// Execute one queued job on the worker thread.
    fn run_job(&self, queued: QueuedJob) {
        let QueuedJob { epoch, job } = queued;
        let inner = &self.inner;

        // THE POISON GATE, ahead of everything including the supersession one.
        // A crate whose pipeline is wedged below it (see
        // `Inner::teardown_poisoned`) has no job it can run, whatever epoch the
        // job carries. Refusing is not the same as ignoring. Every job that
        // has a caller waiting on it settles that wait here, or the bounded
        // disarm would have moved the wedge from the worker to the callers.
        // `poison_refusal` is where that per-variant decision lives.
        if inner.teardown_poisoned.load(Ordering::SeqCst) {
            warn!(
                ?job,
                "refusing a job: a teardown left this pipeline descending below the crate \
                 (see GlobalStats::rescue_disarm_timeouts)"
            );
            self.settle_refusal(poison_refusal(job));
            return;
        }

        // The queue-supersession gate. `queue_epoch` only ever moves forward
        // and only for a load or a stop (see `Inner::supersede_queued_work`),
        // so a mismatch means exactly one thing: this job was formed for an
        // item a later load or stop has already replaced. What that is worth
        // differs per variant, hence `stale_policy`.
        //
        // ADDITIVE. Every arm below keeps its own guard. This is a coarse
        // "the item moved on" filter in front of the sharp per-variant
        // tokens, never a replacement for one.
        let current = inner.queue_epoch.load(Ordering::SeqCst);
        if epoch != current {
            match stale_policy(&job) {
                StalePolicy::Run => {}
                StalePolicy::LogAndRun => {
                    debug!(
                        ?job,
                        epoch,
                        current,
                        "running a job that was enqueued before a later load or stop"
                    );
                }
                StalePolicy::Drop => {
                    debug!(
                        ?job,
                        epoch, current, "dropping a job superseded by a later load or stop"
                    );
                    Counters::bump(&inner.counters.stale_jobs_dropped);
                    // The two drops a caller can be waiting on settle exactly
                    // the way the poison gate settles them, out of
                    // [`poison_refusal`] rather than a third hand-rolled copy
                    // of those arms. NOT every dropped variant: a superseded
                    // `Job::Load` owes its caller nothing here (the ordinary
                    // load path is still live and its next load reports for
                    // itself), unlike under the poison gate, so this set stays
                    // explicit.
                    //
                    // `AttachSub`'s event is one the caller already handles.
                    // It carries the CURRENT generation, which is still the
                    // SUPERSEDED item's: FIFO puts this drop ahead of the load
                    // that bumped the epoch, and a stop leaves the receiver
                    // expecting no generation at all. So the receiver gates it
                    // away in practice, correctly - its per-item subtitle
                    // bookkeeping resets on the load anyway - and the emit
                    // serves the tests and non-receiver embedders. Crate-side
                    // there is nothing to clean: the attach never ran, so no
                    // watchdog was armed.
                    //
                    // `DispatchSelection`'s wait was recorded when the CALLER
                    // decided to dispatch (see `Job::DispatchSelection`), so a
                    // dropped execution leaves a wait nothing can answer: no
                    // event is sent, no confirmation comes, and the deadline is
                    // the only thing left - a selection that reports "never
                    // applied" seconds later for a send that was deliberately
                    // not made. Seqnum-guarded on the engine's side, so the
                    // common case (a load or a stop RESET the engine, which is
                    // what made this job stale) is a no-op and only an engine
                    // still waiting re-decides.
                    if matches!(job, Job::AttachSub { .. } | Job::DispatchSelection { .. }) {
                        self.settle_refusal(poison_refusal(job));
                    }
                    return;
                }
            }
        }

        match job {
            Job::SetState { target } => self.run_set_state(target),
            Job::Stop { target, done } => self.run_stop(target, done),
            Job::Load {
                input,
                start,
                generation,
            } => self.run_load(input, start, generation),
            Job::Seek(seek) => self.run_seek(seek),
            Job::RefreshSeek { seqnum } => self.run_refresh_seek(seqnum),
            Job::RecoverClock => self.run_recover_clock(),
            Job::RecalculateLatency => self.run_recalculate_latency(),
            Job::AttachSub { id, url } => {
                if let Err(err) = self.attach_subtitle_with_id(id, &url) {
                    error!(?err, url, "fcastplaybin subtitle attach failed");
                    inner.emit(PlaybinEvent::ExternalSubtitleFailed { id });
                }
            }
            Job::DetachSub { id } => {
                if let Err(err) = self.detach_subtitle(id) {
                    // Possible for an attach that already failed (nothing
                    // registered), harmless.
                    debug!(?err, ?id, "fcastplaybin subtitle detach failed");
                }
            }
            Job::FailSub { id, epoch } => self.fail_subtitle(id, epoch),
            Job::CheckSub { id, epoch } => self.check_subtitle(id, epoch),
            Job::RetrySub { id, epoch } => self.retry_subtitle(id, epoch),
            Job::AdoptSubState { id, epoch } => self.run_adopt_sub_state(id, epoch),
            Job::ReplaySub { id, epoch, attempt } => self.replay_subtitle(id, epoch, attempt),
            Job::VerifyReplay { id, epoch, attempt } => self.verify_replay(id, epoch, attempt),
            Job::DumpGraph { done } => done.call(graph::snapshot(inner.pipeline.upcast_ref())),
            Job::Barrier { done } => done.call(),
            Job::PrepareNext { input, generation } => self.run_prepare_next(input, generation),
            Job::CancelPrepared { notify, after } => self.run_cancel_prepared(notify, after),
            Job::FinishActivation => self.run_finish_activation(),
            Job::SyncTextRunningTime => inner.sync_text_running_time(),
            Job::DrainTextWork => self.run_drain_text_work(epoch),
            Job::VideoChainGone => self.run_video_chain_gone(),
            Job::ClearStateFailure => self.run_clear_state_failure(),
            Job::SelectionDeadline { seqnum } => self.selection_deadline_fired(seqnum),
            Job::RefreshDeadline { seqnum } => self.refresh_deadline_fired(seqnum),
            Job::PollTextPolicy => self.run_poll_text_policy(),
            Job::DispatchSelection {
                target,
                seqnum,
                replacing,
                generation,
                queued,
            } => {
                // The caller's loop breaks on a refusal; there is no loop
                // here, and nothing to break out of - `dispatch_selection`
                // has already told the engine.
                let _sent =
                    self.dispatch_selection(target, seqnum, replacing, generation, queued.0);
            }
            Job::EffectDone { id, outcome } => self.effect_done(id, outcome),
        }
    }

    /// Worker side of [`Job::SetState`].
    fn run_set_state(&self, target: gst::State) {
        let inner = &self.inner;
        // Downward transitions take the route gate (a pad routed
        // into the descending pipeline deadlocks it).
        //
        // A change to the state the pipeline is ALREADY in posts NO
        // `state-changed`: `gst_element_continue_state` guards it with
        // `old_state != old_next || old_ret == ASYNC` ("don't post silly
        // messages with the same state"). The caller's machine advances
        // only on that message, so a no-op dispatch parks it in
        // `Phase::Changing` for good. Reached routinely, because
        // `StateMachine::buffering` always redispatches a PLAYING target
        // and `player.rs` `uri_loaded` has usually driven it there
        // already (`dash-start-seek-text-join-race.md`).
        //
        // Read BEFORE the call: afterwards "already there" and "just
        // arrived" are indistinguishable. `Ok(Async)` is excluded, that
        // is the one case GStreamer does post.
        let (before, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        let silent = current == target
            && pending == gst::State::VoidPending
            && !matches!(before, Ok(gst::StateChangeSuccess::Async));
        let _ = self.set_pipeline_state(target);
        if silent {
            debug!(
                ?target,
                "the pipeline was already in the requested state; \
                 reporting the state edge GStreamer suppresses"
            );
            inner.emit(PlaybinEvent::StateChanged {
                old: target,
                current: target,
                pending: gst::State::VoidPending,
            });
        }
    }

    /// Worker side of [`Job::Stop`].
    fn run_stop(&self, target: gst::State, done: Option<Callback>) {
        if let Err(err) = self.teardown(target) {
            warn!(?err, ?target, "fcastplaybin teardown failed");
        }
        if let Some(done) = done {
            done.call();
            debug!("Sent stop feedback signal");
        }
    }

    /// Worker side of [`Job::Load`].
    fn run_load(&self, input: MediaInput, start: StartPoint, generation: u64) {
        let inner = &self.inner;
        // Kept for the failure report below: the load consumes the
        // input, and the URI is what makes the report actionable.
        let failed_uri = failed_uri_of(&input);
        match self.load_with_generation(input, start, generation) {
            Ok(outcome) => {
                if outcome.live {
                    debug!("Pipeline is live");
                }
                inner.emit(PlaybinEvent::Loaded { live: outcome.live });
            }
            Err(err) => {
                error!(?err, "fcastplaybin load failed");
                // An ASYNC load that fails HERE fails before the
                // pipeline exists to error on, so the "any user-visible
                // failure arrives through the pipeline error path"
                // assumption does not hold for it: the caller was left
                // waiting for a `Loaded` that can never come, with
                // nothing on the bus and only a log line to show for
                // it. Reported as an ordinary main-input error, which
                // is the load-failure path the caller already has (a
                // typefind error takes it too).
                //
                // Stamped with THIS job's generation rather than the
                // pipeline's current one: most of what can fail here
                // fails before the load adopts its generation, so the
                // current one still names the item being replaced and
                // the caller's load-scope gate would drop the very
                // report it is waiting for (see
                // `Inner::emit_with_generation`).
                inner.emit_with_generation(
                    PlaybinEvent::Error {
                        origin: ErrorOrigin::Main,
                        error: gst::glib::Error::new(
                            gst::LibraryError::Failed,
                            &format!("loading the media failed: {err:#}"),
                        ),
                        failed_uri,
                    },
                    generation,
                );
            }
        }
    }

    /// Worker side of [`Job::Seek`].
    fn run_seek(&self, seek: Seek) {
        let inner = &self.inner;
        // Non-blocking query: a zero timeout returns the in-flight
        // transition instead of waiting for it. An unbounded
        // `state(None)` here wedged the whole worker when a seek
        // arrived mid-preroll and the preroll stalled, queueing
        // every later job behind it forever.
        let (_, state, pending) = inner.pipeline.state(gst::ClockTime::ZERO);

        if state != gst::State::Paused || pending != gst::State::VoidPending {
            inner.emit(PlaybinEvent::QueueSeek(seek));
            let _ = inner.pipeline.set_state(gst::State::Paused);
            // An async transition read above can commit between the
            // query and the call. The call is then a same-state no-op
            // posting nothing, and the real settle edge was emitted
            // BEFORE the hand-back (sync bus handler), so the parked
            // seek would wait forever for an edge that already
            // passed. Report the settle GStreamer will not repeat.
            if pending != gst::State::VoidPending {
                let (_, now, now_pending) = inner.pipeline.state(gst::ClockTime::ZERO);
                if now == gst::State::Paused && now_pending == gst::State::VoidPending {
                    debug!(
                        "the refused seek's PAUSED request was a no-op on an \
                         already-settled pipeline; reporting the missed settle"
                    );
                    inner.emit(PlaybinEvent::StateChanged {
                        old: gst::State::Paused,
                        current: gst::State::Paused,
                        pending: gst::State::VoidPending,
                    });
                }
            }
            return;
        }

        let position = match seek.position {
            Some(pos) => pos,
            None => {
                // A rate-only seek (SetSpeed) has to ask where the
                // playhead is. Failing SILENTLY here left the caller's
                // seek slot in flight with nothing to settle it (it
                // owns the seek queue and waits for an outcome), so
                // every later seek parked behind a job that had already
                // given up. Report it like any other failed seek.
                let Some(pos) = inner.pipeline.query_position::<gst::ClockTime>() else {
                    error!("Failed to query playback position");
                    inner.emit(PlaybinEvent::SeekFailed);
                    return;
                };
                pos
            }
        };

        // Backstop for `gst_event_new_seek`'s `rate != 0.0` assert
        // (NULL event, binding panic, dead worker). Refused rather
        // than coerced, and reported so the caller's seek slot
        // settles instead of parking every later seek behind it.
        let rate = seek.rate.unwrap_or(1.0);
        if !Seek::rate_is_safe(rate) {
            error!(rate, "refusing a seek with an invalid rate");
            inner.emit(PlaybinEvent::SeekFailed);
            return;
        }
        let rate = rate as f64;
        debug!(rate, ?position, "Performing seek");

        if let Err(err) = send_rate_seek(&inner.pipeline, rate, position) {
            error!(?err, "Failed to seek");
            inner.emit(PlaybinEvent::SeekFailed);
        } else {
            // The seek's flush restarted the LIVE branches, so for
            // them "this group's end has entered ssync" no longer
            // holds, and a stale mirror would wrongly park the next
            // video re-enable (see the drained-resurrect arm in
            // `route_db3_pad`, whose lever also gates this clear).
            // Only a seek that reached a live video branch clears
            // it. A stream DESELECTED at seek time is not restarted
            // by the seek (measured on seed 1600058, the source's
            // video task stays idle at EOS through the seek), so
            // its pre-seek drained state stands and the park must
            // still see it.
            inner.clear_passing_eos_after_flush();
            *inner.intended_timeline.lock() = (rate, position);
            inner.forward_seek_to_live_externals(rate, position);
            inner.emit(PlaybinEvent::RateChanged(rate));
        }
    }

    /// Worker side of [`Job::RefreshSeek`].
    fn run_refresh_seek(&self, seqnum: gst::Seqnum) {
        let inner = &self.inner;
        // RE-VALIDATED HERE, not only where it was scheduled.
        // `SelectionEngine::pump` sampled its gates (the caller's
        // quiet, seekable, no external subtitle attached) at dispatch
        // and this job then queued behind SetState/Load/DrainTextWork
        // and the branch disposals. By the time it runs the pipeline
        // can be mid-load, mid-buffering, mid-seek or carrying an
        // external input, and a flushing seek landing there is the
        // FLUSHING-into-an-adaptive-output-loop hazard
        // (FREEZE-DIAGN.md 8.2 #2: adaptivedemux2 serves every track
        // from one task and pauses it for good). `Job::Seek` has such
        // a guard; this one had none.
        //
        // A stale refresh is DROPPED, never re-parked: it is only ever
        // a nicety (the freshly selected track re-emits at its next
        // cue either way), and reporting the failure is what clears
        // the engine's `refreshing` slot, which otherwise blocks every
        // later dispatch.
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        let settled = pending == gst::State::VoidPending && current >= gst::State::Paused;
        let externals = {
            let routing = inner.routing.lock();
            routing.inputs.iter().any(|i| i.external.is_some())
        };
        let seekable = {
            let mut query = gst::query::Seeking::new(gst::Format::Time);
            inner.pipeline.query(&mut query) && query.result().0
        };
        let superseded = inner.selection.lock().refresh_superseded(seqnum);
        if !settled || externals || !seekable || superseded {
            debug!(
                ?current,
                ?pending,
                externals,
                seekable,
                superseded,
                ?seqnum,
                "dropping a refresh seek whose preconditions no longer hold"
            );
            inner.selection.lock().refresh_failed(seqnum);
            inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
            return;
        }
        let Some(position) = inner.pipeline.query_position::<gst::ClockTime>() else {
            debug!("Skipping the refresh seek: no position");
            inner.selection.lock().refresh_failed(seqnum);
            inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
            return;
        };

        // The refresh is a RE-EMIT, not a transport change: it must
        // land on the timeline the item already runs on. Hard-coding
        // rate 1.0 here made every track switch at a non-1.0 speed
        // silently drop the pipeline back to 1.0x, and since a refresh
        // emits no `RateChanged` the caller (and the sender's UI) kept
        // reporting the old speed over 1.0x audio.
        let rate = inner.intended_timeline.lock().0;

        // A flushing seek to the current position in the current
        // state: re-emits the subtitle cue active NOW and flushes
        // the stale one, without a normal seek's Paused round-trip.
        debug!(
            ?position,
            rate,
            ?seqnum,
            "Refresh seek (flushing, current position)"
        );
        let event = rate_seek_event(rate, position, Some(seqnum));
        if !inner.pipeline.send_event(event) {
            warn!("Refresh seek failed");
            inner.selection.lock().refresh_failed(seqnum);
            inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
        } else {
            // Same full-pipeline flush as Job::Seek above, same
            // conditional reset of the passing-EOS mirror.
            inner.clear_passing_eos_after_flush();
        }
    }

    /// Worker side of [`Job::RecoverClock`].
    fn run_recover_clock(&self) {
        let inner = &self.inner;
        debug!("Recovering from clock loss");
        if let Err(err) = inner.pipeline.set_state(gst::State::Paused) {
            warn!(?err, "Clock recovery: failed to reach Paused");
            return;
        }
        if let Err(err) = inner.pipeline.set_state(gst::State::Playing) {
            warn!(?err, "Clock recovery: failed to reach Playing");
        }
    }

    /// Worker side of [`Job::RecalculateLatency`].
    fn run_recalculate_latency(&self) {
        let inner = &self.inner;
        if let Err(err) = inner.pipeline.recalculate_latency() {
            warn!(?err, "failed to recalculate pipeline latency");
        }
        // Every field freeze so far follows one of these within ~1s;
        // the value the pipeline settled on is the missing datum.
        let mut query = gst::query::Latency::new();
        if inner.pipeline.query(&mut query) {
            let (live, min, max) = query.result();
            debug!(live, %min, ?max, "pipeline latency recalculated");
        }
    }

    /// Worker side of [`Job::AdoptSubState`].
    fn run_adopt_sub_state(&self, id: ExternalSubId, epoch: u32) {
        let element = {
            let routing = self.inner.routing.lock();
            routing
                .inputs
                .iter()
                .find(|input| {
                    input
                        .external
                        .as_ref()
                        .is_some_and(|e| e.id == id && e.epoch == epoch)
                })
                .map(|input| input.element.clone())
        };
        let Some(element) = element else {
            debug!(?id, epoch, "stale state-adopt job; input already gone");
            return;
        };
        element.set_locked_state(false);
        if let Err(err) = element.sync_state_with_parent() {
            warn!(
                ?err,
                ?id,
                "the materialized external refused the state join"
            );
        }
        debug!(
            ?id,
            "external input unlocked and joined to the pipeline state"
        );
    }

    /// Worker side of [`Job::PrepareNext`].
    fn run_prepare_next(&self, input: MediaInput, generation: u64) {
        let inner = &self.inner;
        // A newer load or prepare was requested after this one was
        // queued; its reset would remove this input right away.
        if generation != inner.next_generation.load(Ordering::SeqCst) {
            debug!(generation, "skipping a superseded prepare");
            return;
        }
        // External subtitle inputs are per-item side inputs on the
        // live core. A swap would carry them across: decodebin3
        // keeps their streams in the (combined) collections it
        // posts for the NEXT item, which corrupts the held-back
        // collection and the per-item selection state. Refuse; the
        // caller's ordinary end-of-stream advance owns that
        // transition.
        if inner
            .routing
            .lock()
            .inputs
            .iter()
            .any(|i| i.external.is_some())
        {
            debug!(
                generation,
                "external subtitles attached; refusing the gapless prepare"
            );
            inner.emit(PlaybinEvent::PreparedFailed { generation });
            return;
        }
        // Latest wins: replace a still-pending previous prepare. A
        // swap already PERFORMED cannot unwind: its activation is
        // imminent, and arming over it would hand the activation
        // this prepare's record while the pipeline plays the other
        // item. Refuse instead.
        if matches!(
            self.cancel_prepared(AfterCancel::Nothing),
            CancelOutcome::Declined { .. }
        ) {
            debug!(
                generation,
                "a performed swap is activating; refusing the prepare"
            );
            inner.emit(PlaybinEvent::PreparedFailed { generation });
            return;
        }

        // The current item is in steady playback, so every routed
        // pad's sticky stream-start is present: make sure the active
        // group is known now. Activation detection (a group CHANGE at
        // the output) and the old-item EOS drop both depend on the
        // current group being positively known before the switch.
        Inner::seed_active_group(inner);

        // Arm the swap gate BEFORE the input exists: from here on
        // any EOS at the decodebin3 outputs is held back, so a
        // drain racing the prepare cannot leak to the sinks. The
        // INPUT side commonly drained long ago (a small or resident
        // file is swallowed whole by the multiqueue at load), which
        // is fine: the swap proceeds immediately and the OUTPUT
        // side still paces the actual switch. Only an output EOS
        // that fully escaped before this point misses the handoff,
        // and then the caller's ordinary end-of-stream advance owns
        // the transition.
        *inner.swap_gate.state.lock() = SwapState {
            pending: Some(generation),
            drained: false,
            swapped: false,
            dropped_eos: false,
        };
        // `drained` is derived AFTER the arm, never sampled into the
        // literal above. Rust evaluates the assigned value before the
        // place expression, so a sample there completes BEFORE the
        // gate lock is taken, and the input's final EOS landing in
        // that window is lost both ways: `note_input_pad_eos` no-ops
        // against a gate with no pending swap, and EOS fires once per
        // pad, so nothing sets it again. The prepared input's blocked
        // threads then park on the condvar forever and the boundary
        // silently hangs. This call re-reads the taps under the armed
        // state, in the tap probe's own lock order (routing, then the
        // gate).
        Inner::note_input_pad_eos(inner);

        let built = match input {
            MediaInput::Uri(uri) => Inner::make_urisourcebin(&uri, true),
            MediaInput::Element(element) => Ok(element),
        };
        let attached = built.and_then(|element| {
            Inner::add_prepared_input(inner, element.clone(), generation).map(|_| element)
        });
        match attached {
            Ok(element) => {
                debug!(generation, "prepared the next input (blocked, unlinked)");
                *inner.prepared.lock() = Some(PreparedNext {
                    element,
                    generation,
                    pending_collection: None,
                });
            }
            Err(err) => {
                error!(?err, generation, "failed to prepare the next input");
                // Disarm what was armed above.
                let aborted = inner.swap_gate.abort();
                // A prepare failing WHILE the pipeline transitions
                // (its error message aborts a bin's in-flight
                // async commit) must not strand playback below the
                // caller's target: re-commit the transition.
                self.recommit_pipeline_state();
                inner.emit(PlaybinEvent::PreparedFailed { generation });
                // The hold may have consumed the current item's
                // end between the arm and this failure (see
                // `SwapState::dropped_eos` and `cancel_prepared`).
                if aborted.pending.is_some() && aborted.dropped_eos {
                    debug!("the item's end was consumed while arming: synthesizing it");
                    inner.emit(PlaybinEvent::EndOfStream);
                }
            }
        }
    }

    /// Worker side of [`Job::CancelPrepared`].
    fn run_cancel_prepared(&self, notify: bool, after: AfterCancel) {
        let inner = &self.inner;
        let outcome = self.cancel_prepared(after);
        if notify {
            inner.emit(match outcome {
                CancelOutcome::Cancelled { generation } => {
                    PlaybinEvent::PreparedCancelled { generation }
                }
                CancelOutcome::Declined { generation } => {
                    PlaybinEvent::PreparedCancelDeclined { generation }
                }
            });
        }
    }

    /// Worker side of [`Job::FinishActivation`].
    fn run_finish_activation(&self) {
        let inner = &self.inner;
        // The prepared item is live (its generation is current):
        // every older input is drained history. This removes the
        // previous item's main input and its external subtitles.
        let current = inner.current_generation();
        let old: Vec<Input> = {
            let mut routing = inner.routing.lock();
            let (old, keep) = routing
                .inputs
                .drain(..)
                .partition(|input| input.generation < current);
            routing.inputs = keep;
            old
        };
        for input in old {
            debug!(
                generation = input.generation,
                "removing a drained input after the gapless activation"
            );
            Inner::remove_input(inner, input);
        }
    }

    /// Worker side of [`Job::DrainTextWork`]. `epoch` is the queue stamp the
    /// job was formed under, which the reconcile pass at the tail reads.
    fn run_drain_text_work(&self, epoch: u64) {
        let inner = &self.inner;
        // Diagnostic only. The busy-loop regression test counts the
        // drains the worker actually received.
        Counters::bump(&inner.counters.drain_jobs_seen);
        // The stamp travels with the drain: the reconcile pass at its
        // tail wants to know whether a stop or a load was requested
        // AFTER this drain was queued (see there). The drain itself
        // still runs - it is `StalePolicy::Run`, and postponed work
        // outlives an item change on purpose.
        Inner::run_deferred_text_work(inner, epoch)
    }

    /// Worker side of [`Job::VideoChainGone`].
    fn run_video_chain_gone(&self) {
        let inner = &self.inner;
        // Text is consumed synchronized against VIDEO buffers, so a
        // text stream left linked after video stops can never
        // drain and blocks decodebin3's reconfiguration until the next
        // flush. Park it, and the policy brings it back once a video
        // stream routes again.
        Inner::park_text_streams(inner);
        // The video pad is gone for good (a mid-item deselect, an
        // input teardown), so take the chain out of the pipeline.
        // Nothing can then aggregate over, or later lift, a sink that
        // will never see data again. A re-select routes a fresh pad
        // and rebuilds.
        //
        // Re-checked here rather than trusted from the posting side:
        // between the pad-removed callback and this job a new video
        // stream may already have routed, and tearing the chain down
        // under it would strand the item with no video at all.
        // LINKED video only. A parked video entry (the resurrected
        // pad of a deselected stream) is not a reason to keep the
        // chain, it is the very thing the deselect is waiting out.
        // Inert before the resurrect park existed, since every
        // routed video entry was linked by construction.
        let video_routed = inner
            .routing
            .lock()
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Video && r.downstream.is_some());
        if video_routed {
            debug!("a video stream routed again before the chain teardown ran; keeping it");
        } else {
            inner.remove_video_chain();
        }
    }

    /// Worker side of [`Job::ClearStateFailure`].
    fn run_clear_state_failure(&self) {
        let inner = &self.inner;
        // `Err(_)` here IS `GST_STATE_RETURN == FAILURE`, so a
        // pipeline that can still commit is a no-op.
        let (before, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if before.is_ok() {
            return;
        }
        // Below PAUSED nothing can be stranded, and the next load or
        // teardown clears the latch itself. Staying out keeps this off
        // downward transitions.
        if current < gst::State::Paused {
            debug!(?current, "leaving the latch to the next load or teardown");
            return;
        }
        // `pending == current` non-void is what `bin_handle_async_start`
        // writes on a lost state. Re-committing it leaves
        // `old_state == old_next`, so GStreamer posts nothing and the
        // report below is the caller's only announcement.
        let lost_state = pending != gst::State::VoidPending && pending == current;
        // `bin_handle_async_done` owed PENDING, not current. Mid-climb
        // the two differ (an error during a PAUSED->PLAYING commit reads
        // `(Paused, Playing)`), and re-committing `current` there
        // cancels the climb and announces nothing. A void pending means
        // the bin had arrived, and then `current` is what it arrived at.
        let owed = if pending == gst::State::VoidPending {
            current
        } else {
            pending
        };
        warn!(
            ?current,
            ?pending,
            ?owed,
            lost_state,
            "the pipeline latched a state-change failure from an error the \
             crate consumed; re-committing so it can settle again"
        );
        // A pending BELOW paused is a teardown descending (a stop, a
        // load's reset). It clears the latch with its own `set_state`
        // and asserting anything here would fight it, which is this
        // crate's most expensive kind of mistake.
        if owed < gst::State::Paused {
            debug!(?owed, "leaving the latch to the descending transition");
            return;
        }
        // Re-commit the state the bin had already decided on and was
        // refused. The TARGET is NOT read back (GStreamer does not
        // expose it) and does not need to be: the caller's state
        // machine owns the transport target and re-asserts it from the
        // edge reported below, which is the same contract every other
        // correction here uses.
        let _ = inner.pipeline.set_state(owed);
        if !lost_state {
            // A re-committed climb announces itself (`old_state !=
            // old_next`), so nothing to report.
            //
            // UNCOVERED, deliberately: a latch caught with
            // `pending == VoidPending`. Nothing is owed
            // (`bin_handle_async_done` takes `nothing_pending` either
            // way) and no reproducer was ever built. If one turns up,
            // the predicate is `Job::SetState`'s. It is not applied
            // here because this job has no target to compare against,
            // so it would report on every consumed error against a
            // settled pipeline.
            return;
        }
        // Unannounced settle: re-committing the pipeline's own state
        // leaves `old_state == old_next`, which both of GStreamer's
        // "don't post silly messages with the same state" guards refuse
        // to post on, so this cannot duplicate a real edge. Not
        // conditioned on reading settled afterwards (measured on 400009
        // the re-commit returns with `pending` still set).
        debug!(
            ?current,
            "reporting the settle the latched pipeline could not post"
        );
        inner.emit(PlaybinEvent::StateChanged {
            old: current,
            current,
            pending: gst::State::VoidPending,
        });
    }

    /// Worker side of [`Job::PollTextPolicy`].
    fn run_poll_text_policy(&self) {
        let inner = &self.inner;
        // Diagnostic only, the counterpart of the drain's counter.
        Counters::bump(&inner.counters.poll_jobs_seen);
        // Cleared FIRST, and unconditionally. A poke that lands while
        // the policy below is running asks about a world this run has
        // already read, so it must be able to queue a fresh job; and
        // a flag left set by any path at all would swallow every poll
        // for the rest of the instance.
        inner.poll_queued.store(false, Ordering::SeqCst);
        Inner::poll_text_policy(inner);
    }

    /// Worker side of [`Job::EffectDone`]: retire the in-flight entry and act
    /// on what the lane found (see the [`hands`] module).
    ///
    /// The retirement is UNCONDITIONAL and comes first. Everything the table
    /// is read for - the tick's wedge warning, the selection deadline's
    /// "has this even been sent yet" - is wrong in the dangerous direction if
    /// an entry outlives its effect.
    fn effect_done(&self, id: EffectId, outcome: Outcome) {
        let inner = &self.inner;
        if !inner.hands.complete(id) {
            // Ids are never reused, so this is a double report or a report
            // for an effect the enqueue never registered. Neither is
            // reachable today; it is logged rather than asserted because the
            // cost of being wrong here is a log line.
            debug!(id, ?outcome, "an effect reported twice");
        }
        match outcome {
            Outcome::SelectSent {
                seqnum,
                upstream_ids,
            } => {
                debug!(id, ?seqnum, "a selection left the crate");
                // The mirror the upstream-selection split compares against,
                // written where the send is a FACT rather than where it was
                // decided (see `FcastPlaybin::dispatch_selection`). Sorted
                // there, kept sorted here: the comparison is set-shaped.
                //
                // Late reports are harmless in the same way the refusal's
                // are: a newer send's own report overwrites this one, and
                // both describe events that really went out, in the order
                // the lane sent them.
                if let Some(mut ids) = upstream_ids {
                    ids.sort();
                    *inner.last_upstream_ids.lock() = ids;
                }
            }
            // A refused dispatch never confirms, and a skipped one never
            // happened at all; leaving either in flight starves every later
            // change (`pump` refuses to dispatch over an unconfirmed
            // selection while playing). Field shape for the refusal:
            // `subtitle-regressions.txt`. The SKIP is the one v1 dropped in
            // silence, which is how a superseded-core selection could leave
            // the engine waiting for a confirmation nobody would ever send.
            //
            // On the decider rather than on the lane, so the engine is
            // touched by the thread that owns the decisions. Late reports are
            // ROUTINE on this path - a skip is what a load or a gapless swap
            // makes of an in-flight select - and what makes them harmless is
            // that `SelectionEngine::dispatch_failed` is seqnum-guarded in
            // full: it returns untouched unless the failure names the wait
            // the engine is actually in, so a stale skip can neither clear a
            // newer dispatch nor cancel its re-emit flush.
            Outcome::SelectRefused { seqnum } => {
                warn!(id, ?seqnum, "a selection was refused");
                inner.selection.lock().dispatch_failed(seqnum);
            }
            Outcome::SelectSkipped { seqnum, reason } => {
                debug!(id, ?seqnum, reason, "a selection was skipped");
                inner.selection.lock().dispatch_failed(seqnum);
            }
            // The lane only sent it. What it MEANS - the owed hold release,
            // the postponement of a refused seek, the verification, the
            // exhaustion escalation - is decided here, sequenced against
            // every other decision about this input.
            Outcome::ReplaySent {
                sub_id,
                epoch,
                attempt,
                accepted,
                total,
            } => {
                debug!(
                    id,
                    ?sub_id,
                    epoch,
                    attempt,
                    accepted,
                    total,
                    "a replay seek was sent"
                );
                Self::replay_outcome(inner, sub_id, epoch, attempt, accepted, total);
            }
            Outcome::JoinFinished { kind } => debug!(?kind, "a chain join finished"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExternalSubId, Job, TimerJob};

    /// The timer table's job mapping, one assertion per variant: a due
    /// timer must queue exactly the worker job it stands for, fields
    /// intact, or a verification fires as a re-check and vice versa.
    #[test]
    fn each_timer_job_maps_to_its_worker_job_with_its_fields() {
        let id = ExternalSubId(9);
        let verify = TimerJob::VerifyReplay {
            id,
            epoch: 3,
            attempt: 2,
        };
        match verify.job() {
            Job::VerifyReplay {
                id: got_id,
                epoch: 3,
                attempt: 2,
            } if got_id == id => {}
            other => panic!("VerifyReplay mapped to {other:?}"),
        }
        let check = TimerJob::CheckSub { id, epoch: 5 };
        match check.job() {
            Job::CheckSub {
                id: got_id,
                epoch: 5,
            } if got_id == id => {}
            other => panic!("CheckSub mapped to {other:?}"),
        }
    }
}
