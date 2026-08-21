//! Teardown: the bounded descent to NULL, the wake rescue that unblocks
//! it, and `Inner`'s `Drop`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use tracing::{debug, error, warn};

use crate::{
    FcastPlaybin, Inner,
    flush::FlushReason,
    text_disposal::{DisposalBoundary, TextDisposal},
};

/// Teardown descents that blew [`TEARDOWN_DESCENT_BUDGET`] and were
/// detached ([`Teardown::run`]).
pub(crate) static TEARDOWN_DESCENT_STUCK: AtomicU64 = AtomicU64::new(0);

/// Rescue threads that blew [`RESCUE_DISARM_BUDGET`] in [`WakeRescue::disarm`]
/// and were detached, poisoning the crate ([`Inner::teardown_poisoned`]).
///
/// ZERO is the invariant. A nonzero count is a pipeline that could not be taken
/// down at all - the descent is stuck below this crate - and the crate has
/// stopped touching it. Loud on purpose: this is the counter that says "the
/// wedge happened, and the worker survived it", which is a different claim from
/// "nothing went wrong".
pub(crate) static RESCUE_DISARM_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Times [`StartTimeGuard`] had to put the pipeline's START TIME back after a
/// side-input flush moved it (see that type). Must be read as a delta, and a
/// nonzero delta is the FIELD DEFECT happening and being repaired, not a
/// health metric: the number exists so a test can prove the repair fired and
/// so a field log names the moment.
pub(crate) static START_TIME_RESTORES: AtomicU64 = AtomicU64::new(0);

/// Put the pipeline's START TIME back after an operation that pushes a
/// `FLUSH_STOP` with `reset_time = TRUE` into a SIDE branch.
///
/// # The defect, end to end
///
/// A replay is a FLUSHING seek sent to an external subtitle source's src pads
/// ([`FcastPlaybin::send_replay_seek`]). `gst_base_src_perform_seek` answers
/// every flushing seek with `gst_event_new_flush_stop (TRUE)`
/// (gstbasesrc.c:1819, hardcoded, no way to ask for anything else), and that
/// event travels down the external's own branch to its tail. Both tails the
/// crate builds for text are `GstBaseSink`s - the consumer appsink
/// ([`Inner::build_text_consumer_tail`]) and the parking fakesink
/// ([`Inner::park_stream`]) - and the SINK *flag* the crate unsets on them
/// changes only whether `gst_bin_iterate_sinks` finds them, not what
/// `gst_base_sink_flush_stop` does. On `reset_time` it posts
/// `GST_MESSAGE_RESET_TIME` (gstbasesink.c:3291-3294).
///
/// `GstBin`'s child bus handler is a SYNC handler, so `GstPipeline` takes that
/// message on the posting thread and calls `reset_start_time (pipeline, 0)`
/// (gstpipeline.c:619-628). That overwrites `GST_ELEMENT_START_TIME` - but only
/// when it is not `GST_CLOCK_TIME_NONE` (gstpipeline.c:318), and it is NONE for
/// the whole of PLAYING. **So the damage is PAUSED-only**, which is exactly
/// what the field report says ("re-enabling subtitles while paused").
///
/// While PAUSED that field holds the running time playback stopped at
/// (gstpipeline.c:377). Clobbering it to 0 makes the next PAUSED -> PLAYING
/// compute `new_base_time = now - 0` (gstpipeline.c:502-509), so:
///
/// * running time restarts at 0, and `gst_base_sink_get_position` answers from
///   `now - base_time` (gstbasesink.c:5334), so the reported position restarts
///   at 0.0 and climbs 1:1 - `subtitle-reenable-freeze.txt` verbatim;
/// * the video sink still has to sync frames whose running time is the OLD 18.8
///   s, so it waits ~19 s of wall clock before showing another one. That is the
///   "video froze" half of the same report.
///
/// # Why a save/restore, and why it is not racy
///
/// The flush is pushed INLINE by the source on the thread that sends the seek,
/// the RESET_TIME message is posted inline from the sink's `flush_stop`, and
/// `bin_bus_handler` is a SYNC handler. Every link in that chain runs before
/// `pad.send_event(seek)` returns, so bracketing the send is enough - there is
/// no later moment to wait for.
///
/// `last_start_time` is left at the -1 `reset_start_time` wrote, which is
/// correct: PAUSED -> PLAYING compares it against `start_time`
/// (gstpipeline.c:453) purely to decide whether to recompute the base time,
/// and a resume from PAUSED wants that recompute.
pub(crate) struct StartTimeGuard {
    pipeline: gst::Pipeline,
    /// Never `None`: [`Self::hold`] declines to exist when the pipeline's
    /// start time is NONE, because that IS the PLAYING case the reset skips.
    saved: gst::ClockTime,
}

impl StartTimeGuard {
    pub(crate) fn hold(pipeline: Option<&gst::Pipeline>) -> Option<Self> {
        let pipeline = pipeline?;
        // NONE is PLAYING, where `reset_start_time` returns without writing
        // ("application asked to not reset stream_time", gstpipeline.c:331).
        // Nothing to guard, and holding one would only risk WRITING a start
        // time onto a pipeline that must not have one.
        let saved = pipeline.start_time()?;
        Some(Self {
            pipeline: pipeline.clone(),
            saved,
        })
    }
}

impl Drop for StartTimeGuard {
    fn drop(&mut self) {
        if self.pipeline.start_time() == Some(self.saved) {
            return;
        }
        START_TIME_RESTORES.fetch_add(1, Ordering::Relaxed);
        warn!(
            saved = %self.saved,
            observed = ?self.pipeline.start_time(),
            "a side-input flush reset the pipeline's start time; putting it back so the \
             next PLAYING does not restart running time at zero"
        );
        self.pipeline.set_start_time(self.saved);
    }
}

/// How long [`FcastPlaybin::teardown`]'s pre-descent wake gets before
/// [`WakeRescue`] performs the pipeline's state change on its behalf.
///
/// Both bounds are load-bearing. It is ABOVE the three seconds
/// `tests/regression_deadlock.rs` deliberately pins the teardown flush for,
/// so a wake that is merely slow is never rescued and every gate written
/// against the inline ordering keeps exactly that ordering. It is well BELOW
/// the fifteen seconds the fuzz drivers give a stop or a graph dump, so a
/// rescued teardown still answers inside their bound.
const TEARDOWN_WAKE_BUDGET: Duration = Duration::from_secs(5);

/// How long [`Teardown::run`]'s descent gets before it is declared wedged,
/// counted, and LEAKED.
///
/// `gst_element_set_state(NULL)` on a pipeline whose adaptivedemux2 has
/// leaked its scheduler lock never returns, and the flushing-seek recovery
/// this crate has for the live case is dead against a descent. There is no
/// third option once that has happened, so the choice is between a receiver
/// that stops responding and a receiver that leaks one thread plus one graph
/// and keeps going. Fifteen seconds is above every legitimate descent this
/// crate has ever measured (the slowest gated one is the teardown flush
/// `tests/regression_deadlock.rs` deliberately parks for three) and is the
/// bound the fuzz drivers already give a stop, so a bounded teardown still
/// answers inside theirs.
///
/// # SCOPE: the DROP path only
///
/// This bounds [`Teardown::run`], which is the descent that happens when the
/// last reference goes away. [`FcastPlaybin::teardown`]'s STOP-path descent is
/// deliberately left unbounded: the wedge's named site is `Teardown::run`,
/// the stop path already has [`WakeRescue`] guarding the window in front of
/// its descent, and giving it a second bound is a change to the stop
/// choreography that the phase-4 gate does not measure. Recorded as follow-up
/// rather than done quietly, so nobody reads "the teardown descent is bounded"
/// as covering both.
const TEARDOWN_DESCENT_BUDGET: Duration = Duration::from_secs(15);

/// How long [`WakeRescue::disarm`] waits for the rescue thread before it
/// DETACHES it and poisons the crate.
///
/// Chosen against the callers' bound, not against the descent. By the time a
/// disarm has anything to wait for, [`TEARDOWN_WAKE_BUDGET`] is already spent
/// (the rescue only exists as a thread to wait for once it fired), so the
/// worker-visible cost of a stop that hits this is the SUM: five seconds of
/// wake plus six here, eleven, against the fifteen the fuzz drivers give a stop
/// or a graph dump. A stopped-but-alive worker therefore still answers inside
/// their bound, which is the whole point of bounding this at all.
///
/// Generous against a healthy descent, which is milliseconds: the only thing
/// this can cost a working teardown is the case where the rescue's own
/// `set_state` is merely slow, and six seconds is far above anything measured.
const RESCUE_DISARM_BUDGET: Duration = Duration::from_secs(6);

/// What [`WakeRescue::disarm`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisarmOutcome {
    /// The rescue is over (or never fired). The teardown continues exactly as
    /// it always has.
    Joined,
    /// The rescue's descent never returned and its thread was detached. The
    /// pipeline is still descending, on a thread this crate no longer waits
    /// for, and nothing may touch it again.
    Detached,
}

/// Performs the teardown's pipeline descent from a second thread when the
/// wake ahead of it has parked. See [`FcastPlaybin::teardown`] for the
/// deadlock and for why the DESCENT is what moves.
///
/// Armed, never unconditional. The rescue thread waits on a hangup channel
/// for [`TEARDOWN_WAKE_BUDGET`]; [`WakeRescue::disarm`] hangs the channel up,
/// so a wake that completes normally cancels it before it can act and the
/// teardown is bit-for-bit the one that ran before this existed.
pub(crate) struct WakeRescue {
    /// Hanging this up is the cancellation. Dropped by `disarm`.
    cancel: Option<mpsc::Sender<()>>,
    /// The rescue thread, waited for with a BOUND rather than with `join`
    /// (see [`BoundedHelper`] and [`WakeRescue::disarm`]). `None` only when
    /// the thread could not be spawned at all.
    helper: Option<BoundedHelper>,
}

impl WakeRescue {
    fn arm(inner: &Arc<Inner>, target: gst::State) -> Self {
        let (cancel, cancelled) = mpsc::channel();
        let inner = Arc::clone(inner);
        let helper = BoundedHelper::spawn("fpb-tdrescue", move || {
            // Anything but a timeout means the wake finished (or the
            // teardown thread went away): nothing to rescue.
            if cancelled.recv_timeout(TEARDOWN_WAKE_BUDGET) == Err(mpsc::RecvTimeoutError::Timeout)
            {
                warn!(
                    ?target,
                    "the teardown wake is still parked after {TEARDOWN_WAKE_BUDGET:?}, \
                     taking the pipeline down from the rescue thread"
                );
                let _gate = Inner::gate(&inner);
                let _ = inner.pipeline.set_state(target);
                debug!(?target, "the rescue descent finished");
            }
        });
        WakeRescue {
            cancel: Some(cancel),
            helper,
        }
    }

    /// Cancel the rescue and wait [`RESCUE_DISARM_BUDGET`] for it, so the
    /// teardown never runs on past a descent that is still in flight. On
    /// timeout: shout, count, DETACH.
    ///
    /// # An unconditional join was the last unbounded wait on the worker
    ///
    /// This used to be `handle.join()`, and the rescue was assumed to finish
    /// because its whole job is one `set_state`. `fuzz_buffering` seed 1800015
    /// (ITERS=1 ACTIONS=42) says otherwise, 3 runs in 10, and a core taken from
    /// the wedged process names all three links:
    ///
    /// * `fpb-tdrescue` in `pipeline.set_state(Ready)` ->
    ///   `gst_decodebin3_change_state(PAUSED_TO_READY)` -> the internal
    ///   multiqueue's `gst_element_pads_activate` -> `gst_pad_set_active` ->
    ///   `activate_mode_internal`, waiting on a pad STREAM LOCK;
    /// * that lock held by a multiqueue loop task parked in
    ///   `do_probe_callbacks` - a blocking probe on a decodebin3 ghost source
    ///   pad, across a `db_output_stream_reconfigure` retarget;
    /// * and `fpb-worker` HERE, in `join`, waiting for the first.
    ///
    /// The first two links are below this crate (see
    /// `UPSTREAM-GSTREAMER-ISSUES.md`); the third is ours, and it is the one
    /// that turns a stuck GStreamer descent into a stuck PROCESS. The worker
    /// answers nothing after it - not a graph dump, not a shutdown barrier -
    /// so every caller waiting on the worker waits forever.
    ///
    /// # Detaching is the lesser evil, and what it costs
    ///
    /// The detached thread owns everything it needs: its own `gst::State`, its
    /// own `Arc<Inner>` clone, and the `RouteGate` it is holding. It therefore
    /// keeps `Inner` - and with it the pipeline and every element in it - ALIVE
    /// until the descent unwedges or the process ends. That is a leak, and it
    /// is deliberately preferred to the alternative, which was a permanently
    /// wedged worker: a leaked graph costs memory in a process that is on its
    /// way out anyway, a wedged worker costs the receiver.
    ///
    /// It also means the gate is gone for good, so the teardown MUST NOT
    /// continue (see [`FcastPlaybin::teardown`]): its very next statement takes
    /// that same gate.
    fn disarm(mut self) -> DisarmOutcome {
        drop(self.cancel.take());
        let Some(helper) = self.helper.take() else {
            return DisarmOutcome::Joined;
        };
        if helper.wait(RESCUE_DISARM_BUDGET) {
            return DisarmOutcome::Joined;
        }
        RESCUE_DISARM_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        error!(
            "the rescue descent has not returned after {RESCUE_DISARM_BUDGET:?}; detaching it \
             so the worker is released. The pipeline is descending on a thread below this \
             crate (a multiqueue loop parked in a blocking probe across a decodebin3 ghost-pad \
             retarget, see UPSTREAM-GSTREAMER-ISSUES.md) and nothing here may touch it again"
        );
        DisarmOutcome::Detached
    }
}

/// A helper thread that is waited for with a BUDGET and DETACHED on timeout,
/// the teardown's one spawn-wait-detach idiom.
///
/// Both users are threads that exist precisely because the work they carry can
/// stop returning: [`WakeRescue`]'s descent and
/// [`Teardown::bounded_descent`]'s. A `join` on either is an unbounded wait on
/// a wedge below this crate, which is the thing being escaped, so neither may
/// join without a bound. The two wrote the same protocol twice and each cited
/// the other as its source.
///
/// The "done" send is the wait's signal, and it lives HERE rather than in the
/// bodies so it cannot be forgotten on a path: it is sent LAST, after the body
/// returns, and a hangup with no send means the body panicked, which is a
/// finished helper just the same.
struct BoundedHelper {
    finished: mpsc::Receiver<()>,
    handle: std::thread::JoinHandle<()>,
}

impl BoundedHelper {
    /// `None` when the thread could not be spawned; the caller decides what a
    /// missing helper means (inline work, or nothing to wait for).
    fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Option<Self> {
        let (done, finished) = mpsc::channel();
        match std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                body();
                let _ = done.send(());
            }) {
            Ok(handle) => Some(Self { finished, handle }),
            Err(err) => {
                warn!(?err, name, "could not spawn a bounded teardown helper");
                None
            }
        }
    }

    /// `true` when the helper finished inside `budget` (it is then joined),
    /// `false` when it did not and was DETACHED. A detached helper owns
    /// everything it needs and keeps its graph alive; the caller counts and
    /// shouts, because only the caller knows what was leaked.
    fn wait(self, budget: Duration) -> bool {
        // Anything but a timeout means the body is over: a value, or a hangup
        // from a panicking helper.
        if self.finished.recv_timeout(budget) != Err(mpsc::RecvTimeoutError::Timeout) {
            let _ = self.handle.join();
            return true;
        }
        drop(self.handle);
        false
    }
}

/// NULL an element the pipeline's descent could not reach, because it parks
/// outside the pipeline. See the call site in [`Teardown::run`] for why.
fn null_if_orphaned(element: &gst::Element) {
    if element.parent().is_none() {
        let _ = element.set_state(gst::State::Null);
    }
}

/// Everything the blocking half of the teardown needs, in OWNED form. Every
/// field is a refcounted GStreamer handle, so this keeps the graph alive by
/// itself once `Inner`'s memory is gone, which is what lets the descent run
/// somewhere other than the thread that dropped the last reference. See
/// [`Teardown::run`] and the comment above `impl Drop for Inner`.
pub(crate) struct Teardown {
    pipeline: gst::Pipeline,
    video_sink: gst::Element,
    video_entry: gst::Element,
    inputs: Vec<gst::Element>,
    disposals: Vec<TextDisposal>,
    /// Pads of live text branches, plus the decodebin3 sink pads of every
    /// input. Collected under the routing lock while `Inner` still existed
    /// (see [`Inner::flush_parked_text_pushes`], which this replaces at the
    /// teardown boundary).
    text_pads: Vec<gst::Pad>,
    db3_sink_pads: Vec<gst::Pad>,
}

impl Teardown {
    /// The former body of `Inner::drop` from the disposals onward. Every call
    /// here can block on a pad stream lock, which is precisely why it is
    /// separated from the reference count that triggered it.
    ///
    /// # The PIPELINE goes down before the inputs do
    ///
    /// The input NULL loop used to run first, and wedged the whole process on
    /// `fuzz_buffering` seed 1600058, deterministic 3 of 3 at the full 600 s
    /// timeout. Captured with gdb:
    ///
    /// * this thread in `element.set_state(Null)` ->
    ///   `gst_uri_source_bin_change_state` -> `gst_bin_src_pads_activate` ->
    ///   `activate_mode_internal`, waiting for a pad STREAM LOCK,
    /// * the input's own `ftestsrc:text` task holding it, blocked in
    ///   `gst_multi_queue_sink_event` -> `gst_data_queue_push` on decodebin3's
    ///   FULL multiqueue,
    /// * that multiqueue's src task parked in `gst_base_sink_wait_preroll`.
    ///
    /// This is the campaign-5 wedge (`tests/regression_teardown_flush.rs`,
    /// seeds 100014, 200030, 300046, 200057). Its window is the flush PAIR:
    /// the FLUSH_STOP re-arms the pad and an as-fast-as-possible source refills
    /// the slot and blocks on a serialized event again before the loop reaches
    /// its NULL. Sending FLUSH_START alone was implemented and MEASURED WRONG,
    /// so the pairing stays and the input NULLs move BEHIND the pipeline's own
    /// descent instead. That descent takes the sinks out of `wait_preroll`, the
    /// multiqueue drains, and the input's push returns before anything asks for
    /// its stream lock. The explicit NULLs stay, for an input the descent
    /// cannot reach (one already out of the pipeline).
    ///
    /// The wake still runs FIRST here, unlike in [`FcastPlaybin::teardown`].
    /// It is not what wedged, and a push parked on something the descent does
    /// not signal still needs it (see that function for the two-obstacle
    /// argument).
    fn run(self) {
        let Teardown {
            pipeline,
            video_sink,
            video_entry,
            inputs,
            disposals,
            text_pads,
            db3_sink_pads,
        } = self;

        // Postponed branch disposals first, the parked-push flush cannot
        // see them (see `Inner::drain_disposals_for_teardown`). Then every
        // parked text push, or the NULLs below deadlock on the pad locks
        // those pushes hold (see `Inner::flush_parked_text_pushes`).
        for disposal in disposals {
            debug!("disposing of a postponed text branch at teardown");
            Inner::dispose_text_branch_on(&pipeline, disposal, DisposalBoundary::Teardown);
        }
        Inner::flush_pads(&text_pads, FlushReason::TeardownText);
        Inner::flush_db3_sink_pads(&db3_sink_pads);
        debug!("drop: parked pushes flushed");

        // A state-locked prepared input does not follow the pipeline down, and
        // its unref at PLAYING trips a CRITICAL. Unlocked BEFORE the descent so
        // the descent carries it.
        for element in &inputs {
            element.set_locked_state(false);
        }

        // THE DESCENT, bounded. Its handles are refcounted, so the helper it
        // may run on keeps its own graph alive and this thread owes it nothing.
        Self::bounded_descent(&pipeline, &inputs);

        // Between video items the caller sink and the chain's queue park at
        // READY OUTSIDE the pipeline (`remove_video_chain`), so the NULL above
        // never reaches them and the final unref would trip GStreamer's
        // dispose-in-READY CRITICAL. Down them explicitly when orphaned. THE
        // one place the crate NULLs the caller's sink, and it is a teardown:
        // the pipeline it belonged to is gone.
        null_if_orphaned(&video_sink);
        null_if_orphaned(&video_entry);
    }

    /// Run the teardown's descent on `fpb-descent` and wait
    /// [`TEARDOWN_DESCENT_BUDGET`] for it. On timeout: shout, count, DETACH.
    ///
    /// # Why the descent moves and not the caller
    ///
    /// The wedge is a `set_state(Null)` that never returns. Whatever thread
    /// is carrying the teardown when that happens stops being available for
    /// anything else, and that thread is whichever one dropped the last
    /// reference: the caller's main loop, the worker, fpb-tick, or the
    /// fpb-teardown handoff. A descent nobody can wait out therefore takes an
    /// arbitrary thread of the receiver with it. Bounding it here releases
    /// that thread, and the process stays killable.
    ///
    /// The leak is deliberate and total: the helper keeps the pipeline, the
    /// inputs and every refcount they hold, forever. There is no way to
    /// reclaim a graph whose state change is stuck inside a mechanism
    /// element's lock, and forcing one would be a use-after-free rather than a
    /// leak. It is logged at `error!` with a counter
    /// ([`GlobalStats::teardown_descent_stuck`](crate::GlobalStats)) so it can
    /// never be a quiet regression, and only after fifteen seconds of a
    /// descent that has genuinely stopped.
    ///
    /// # Thread identity is not violated
    ///
    /// NULLing from a helper is already this crate's normal mode at a
    /// teardown: the foreign-thread handoff runs the whole `Teardown` on
    /// `fpb-teardown` and is never joined, and [`WakeRescue`] performs the
    /// pipeline's descent from `fpb-tdrescue` when the wake ahead of it parks.
    /// `Teardown`'s fields are owned handles by design for exactly this.
    fn bounded_descent(pipeline: &gst::Pipeline, inputs: &[gst::Element]) {
        // The PIPELINE first, then the inputs (see `Teardown::run` for the
        // seed-1600058 wedge the other order deterministically reproduced).
        fn descend(pipeline: &gst::Pipeline, inputs: &[gst::Element]) {
            let _ = pipeline.set_state(gst::State::Null);
            debug!("drop: pipeline down");
            for element in inputs {
                let _ = element.set_state(gst::State::Null);
            }
            debug!("drop: inputs down");
        }

        let carried_pipeline = pipeline.clone();
        let carried_inputs = inputs.to_vec();
        let helper = BoundedHelper::spawn("fpb-descent", move || {
            descend(&carried_pipeline, &carried_inputs);
        });
        let Some(helper) = helper else {
            // Out of threads at a teardown. An unbounded descent is worse
            // than a bounded one and far better than no descent at all: a
            // pipeline unref'd from PLAYING trips CRITICALs and leaks
            // every element in it.
            warn!("could not spawn the teardown descent thread; descending inline");
            descend(pipeline, inputs);
            return;
        };
        if helper.wait(TEARDOWN_DESCENT_BUDGET) {
            return;
        }
        TEARDOWN_DESCENT_STUCK.fetch_add(1, Ordering::Relaxed);
        error!(
            "the teardown descent has not returned after {TEARDOWN_DESCENT_BUDGET:?}; \
             leaking the descent thread and its graph so the carrying thread is released. \
             A set_state(NULL) that never returns means adaptivedemux2 leaked its \
             scheduler lock and no recovery reaches a descent"
        );
    }
}

// Teardown lives on `Inner`, NOT on the cloneable handle: a `Drop` on
// `FcastPlaybin` fires for EVERY dropped clone, including the worker's
// per-job temporaries. A handle-level Drop once NULLed the pipeline from a
// streaming thread mid-post and deadlocked a concurrent load's state change.
//
// # Which thread this runs on is NOT the caller's choice
//
// Every internal callback holds a `Weak` and upgrades for the duration of its
// work, so ANY of them is the last strong reference whenever the caller drops
// its final handle inside that window: the bus sync handler, decodebin3's
// pad-added, an EOS probe. The pipeline's descent to NULL then runs on
// whatever thread that was, and on a streaming thread it CANNOT work. Captured
// (`toml_scenarios`, measured here at 2 runs in 42 ending in signal 11):
//
//     gst_multi_queue_loop -> sticky push into decodebin3's sink
//       -> gst_decodebin_input_setup_identity ->
// gst_element_sync_state_with_parent       -> state-changed -> gst_bus_post ->
// the crate's bus sync handler       -> Arc<Inner>::drop_slow -> Inner::drop ->
// set_state(Null)
//
// The descent tries to deactivate the pad whose task IS the calling thread
// ("Trying to join task ... from its thread would deadlock", "Failed to
// deactivate pad multiqueueN:src_0, very bad"), gives up half-way with the
// pipeline at READY, and the dispose cascade over the still-live elements
// segfaults.
//
// So the blocking half is COLLECTED into an owned [`Teardown`] and, on a
// thread this process did not create, handed to one it did. Fixing the
// callbacks instead was tried FIRST and is not enough: retiring the bus sync
// handler's reference through a dedicated thread left the crash rate
// unchanged (1 run in 36 against 1 in 38), because the terminal reference
// simply lands on the next callback instead, and converting every
// `Weak::upgrade` in the crate still left the warnings at 1 run in 42. There
// is no finite set of call sites to fix; the guarantee belongs here, where the
// thread is known.
//
// # This is NOT the whole class, and the numbers say so
//
// `tests/regression_teardown_thread.rs` manufactures the race and goes from 3
// failures in 3 to 3 passes in 3, so the path it walks is closed. The
// `toml_scenarios` soak did NOT follow: 42 runs per arm, interleaved on one
// binary, gave 2 signal-11 runs and 2 warning-emitting runs with the descent
// forced to run in place whatever the thread, against 1 and 1 with the handoff
// below. At that base rate 2 against 1 is noise, so a second route to the same
// symptom is still open and none of this should be read as closing it.
//
// The likeliest remaining one, UNPROVEN and stated so nobody has to rediscover
// the shape of the search: `Inner`'s OWN fields drop on the calling thread
// after this function returns, concurrently with the handed-off descent. An
// element whose last reference happens to be one of them is then disposed
// from there, and GStreamer's dispose deactivates its pads exactly as the
// descent would have. `Teardown` holds the pipeline, the video chain and the
// inputs; it does not hold every element `Inner` owns.
impl Drop for Inner {
    fn drop(&mut self) {
        debug!("dropping the playbin core");
        // A chain parked by a mid-item deselect is state-locked. Unlock it
        // so it follows the pipeline down.
        self.video_sink.set_locked_state(false);
        // Wake any prepared-input thread parked on the swap gate before the
        // state change joins streaming threads.
        self.swap_gate.abort();

        // Collected BEFORE anything blocks, and each under the routing lock
        // on its own: a downward state change joins streaming threads, and
        // those run pad probes that take that lock (the inversion
        // `Inner::live_text_downstream_pads` documents).
        let teardown = Teardown {
            pipeline: self.pipeline.clone(),
            video_sink: self.video_sink.clone(),
            video_entry: self.video_entry.clone(),
            disposals: std::mem::take(&mut *self.deferred_text_disposal.lock()),
            text_pads: self.live_text_downstream_pads(),
            db3_sink_pads: {
                let routing = self.routing.lock();
                routing
                    .inputs
                    .iter()
                    .flat_map(|input| input.db3_sink_pads.iter().cloned())
                    .collect()
            },
            inputs: self
                .routing
                .lock()
                .inputs
                .iter()
                .map(|input| input.element.clone())
                .collect(),
        };

        // A thread with no Rust name is one this process never spawned, i.e.
        // a GStreamer task thread. Every thread that may legitimately carry
        // the descent has one: the caller's `main` or its test thread, and
        // the crate's own `fcastplaybin` (the worker), `fpb-select`,
        // `fpb-replay`, `fpb-join` and `fpb-tick`. An unnamed
        // `std::thread::spawn` in a caller reads as foreign too, which only
        // costs it an asynchronous teardown.
        //
        // The three lane names outlived the loops they were introduced for:
        // they are the `hands` executor's lanes now (see that module), and
        // they hold a `Weak` between effects exactly as those loops did. The
        // lifetime discipline is what this test depends on, so it carries
        // over verbatim - and keeping the NAMES is why nothing else here,
        // nor the two tests that read a carrier's name, had to change.
        //
        // `fpb-tick` is the one that can carry this while the crate is
        // completely IDLE: it upgrades its `Weak` once per interval whether or
        // not there is work, so a caller that drops its last handle between
        // two ticks hands the terminal reference to it rather than to the
        // worker. That is safe for the same reason the worker is (see
        // `Inner::tick_loop`) - nothing GStreamer owns waits on the tick, so
        // the descent runs INLINE here with no rescue thread involved. It also
        // means teardown can be carried by a thread with no `WakeRescue`
        // armed; the rescue exists for the blocking flushes in
        // `FcastPlaybin::teardown`, which this path does not run.
        //
        // That list is the crate's thread census, and it
        // is the same one it started with: one decider (`fcastplaybin`), three
        // lanes, one tick, plus the teardown and rescue threads that exist to
        // outlive a wedge. The phase moved work BETWEEN those threads, not the
        // threads themselves. The disposals above are also the reason this
        // list matters twice over: they are the one text-branch surgery that
        // legitimately runs off the decider, because `Inner` is already gone
        // when it happens and the link side cannot run at all (the
        // teardown-boundary exemption spelled out in `Inner::decider_only`).
        let foreign = std::thread::current().name().is_none();
        if !foreign {
            teardown.run();
            return;
        }
        debug!("handing the teardown off a thread the crate does not own");
        // NOT joined. Joining would wait for the very descent that is about
        // to join THIS thread's task, which is the deadlock in a new place.
        // The handles above keep the graph alive without `Inner`, so letting
        // it run on is safe.
        if let Err(err) = std::thread::Builder::new()
            .name("fpb-teardown".to_owned())
            .spawn(move || teardown.run())
        {
            error!(
                ?err,
                "could not hand off the teardown; the pipeline stays up"
            );
        }
    }
}

impl FcastPlaybin {
    /// Full teardown to `target` (READY or NULL): drop the pipeline, remove
    /// every input (releasing its network/file resources NOW rather than at
    /// the next load) and drop the per-load audio sink. The video chain, if
    /// present, follows the pipeline down and is removed by the pad-removed
    /// unroutes or the next load's reset.
    ///
    /// # OPEN, AND IT BLOCKS THE CALLER TOO. The highest-value thing left here.
    ///
    /// `fuzz_buffering` seeds 1600039 and 1800015 wedge the WORKER (through
    /// [`Job::Stop`]), deterministic 3 of 3. `fuzz_scenarios` seed 2700007
    /// (ITERS=4 ACTIONS=22) wedges the CALLER, 3 of 3, on `stop() never
    /// returned`: `stop` is this function called synchronously, so it strands
    /// the application thread rather than the worker. All three are ONE call.
    /// The proof is the A/B on attempt 1 below, which cleared 2700007 3 of 3
    /// (it falls through to the pre-existing cue-misalignment class) and both
    /// buffering seeds 3 of 3.
    ///
    /// The stacks, captured with gdb. The blocked thread parks in the wake
    /// below, on a pad stream lock held by a task parked (through a queue) in
    /// `gst_base_sink_wait_preroll`:
    ///
    /// * seed 1600039: `drain_disposals_for_teardown` -> `dispose_text_branch`
    ///   -> `send_event(FLUSH_STOP)` on the overlay's `subtitle_sink`, which is
    ///   SERIALIZED, so `gst_pad_send_event_unchecked` waits for that pad's
    ///   stream lock;
    /// * seed 1800015: `flush_parked_text_pushes` -> `flush_pads` ->
    ///   `send_event(FLUSH_START)` into a queue sink pad ->
    ///   `gst_queue_handle_sink_event` -> `gst_pad_pause_task`.
    ///
    /// Below PLAYING that preroll ends only when the SINKS move, which is what
    /// the `set_state(target)` below does. The worker is the only thread that
    /// runs [`Job::SetState`], so this is a self-deadlock through the job
    /// queue, the same shape as the replay seek (see [`ReplayJob`]) and the
    /// same shape as `Inner::remove_input_or_defer` guards against.
    ///
    /// # Four fixes were implemented and MEASURED WRONG. Start past them.
    ///
    /// What makes this hard is a SECOND obstacle that releases on the opposite
    /// thing: a push parked on something a state change does not signal (a
    /// blocking pad probe, the geometry `tests/deferred_drain.rs` manufactures)
    /// releases only on a FLUSH_START, and that FLUSH_START has to arrive
    /// BEFORE the pipeline descends. The descent's `pad-removed` unroutes empty
    /// `routed`, after which `live_text_downstream_pads` is empty and the flush
    /// has nothing left to travel down. Any fix has to clear both.
    ///
    /// 1. *Move `set_state(target)` ahead of the wake.* Fixed 1600039 and
    ///    1800015 3 of 3 and broke
    ///    `deferred_drain::stopping_after_a_paused_subtitle_off_does_not_wedge`
    ///    (15 s bound, no flush ever sent) and
    ///    `regression_deadlock::the_teardown_flush_does_not_hold_the_routing_lock`
    ///    (40 s waiting for a flush that never came), for exactly that reason.
    /// 2. *Run the wake on a helper thread, joined after the state change.* The
    ///    gates pass and the race is only sometimes won: 1600039 went to 3
    ///    failures in 7, twice reporting `pipeline (Paused, Ready)`, a descent
    ///    that could not finish. `dispose_text_branch_on`'s FLUSH_STOP HOLDS
    ///    the overlay's `subtitle_sink` stream lock while it forwards, so the
    ///    helper can hold the very lock the descent needs while waiting for the
    ///    preroll the descent would release. Concurrency buys a new cycle.
    /// 3. *Take the crate's own sinks (video chain, audio sink) to READY first,
    ///    leaving the order otherwise untouched.* No effect: 1600039 and
    ///    1800015 back to 4 of 4, and 1600058 moved its wedge EARLIER, from
    ///    `Inner::drop` to a stop-and-reload. So the parked push is not held by
    ///    the two sinks this crate owns, or the READY itself does not get past
    ///    them. That is the next thing to establish, with a stack.
    /// 4. *Attempt 1 plus collecting the wake's TARGETS (the disposals and the
    ///    pads) before the descent*, on the theory that the post-descent wake
    ///    failed the two gates only because it had nothing left to send to.
    ///    Both gates still fail identically, 1 of 1 each, so an empty target
    ///    list is not the whole reason. `regression_deadlock`'s probe still
    ///    never fires, which points at the SEND being short-circuited on a pad
    ///    the descent has already deactivated rather than at the lookup.
    ///
    /// # FIXED, by moving the DESCENT rather than the wake
    ///
    /// The recorded shape for a fifth attempt was "FLUSH_START before the
    /// descent from a thread holding no stream lock, the matching FLUSH_STOP
    /// after". A gdb capture of seed 1800015 says that shape cannot be reached
    /// by splitting the flush, and says why:
    ///
    /// * the worker parks in `gst_pad_pause_task` on the text queue's src
    ///   stream lock, inside the FLUSH_**START**, not the stop,
    /// * `queue1:src` holds that lock in `gst_queue_loop`, pushing a STICKY
    ///   event through `gst_subtitle_overlay_subtitle_sink_event`
    ///   (gstsubtitleoverlay.c:2259) and blocked acquiring a further stream
    ///   lock down that chain,
    /// * `multiqueue7:src` and `fpb-aqueue:src` hold the locks it wants, both
    ///   parked in `gst_base_sink_wait_preroll`.
    ///
    /// So dropping the FLUSH_STOP buys nothing here: it is the FLUSH_START
    /// that parks, and a pad set flushing does not release a thread already
    /// blocked on a mutex behind it. The wake and the descent have to run
    /// CONCURRENTLY, and the only question is which of the two moves.
    ///
    /// Moving the wake is attempt 2, and it races: the wake's serialized
    /// FLUSH_STOP takes the target pad's stream lock and HOLDS it while it
    /// forwards, so the descent can end up behind the wake. Moving the
    /// DESCENT does not have that failure mode. When the wake is parked it
    /// holds nothing at all (it is *acquiring* a stream lock in both captured
    /// stacks), and `gst_bin_iterate_sorted` takes a bin down "from the most
    /// downstream elements (sinks) to the sources", so the descent releases
    /// `wait_preroll` before it needs anything the wake is queued behind.
    ///
    /// [`WakeRescue`] is that thread. It is ARMED, not unconditional: it does
    /// nothing at all unless the wake is still running after
    /// [`TEARDOWN_WAKE_BUDGET`], so a healthy teardown, and every gate written
    /// against the inline ordering, keeps byte-for-byte the old behaviour:
    /// same thread, same order, flush PAIRS intact and issued before the
    /// descent. Only a teardown that would otherwise have wedged forever sees
    /// a descent from another thread.
    ///
    /// That also answers the open question the fourth attempt left, which was
    /// whether a post-descent FLUSH_STOP still reaches the sink. It does not:
    /// `gst_pad_send_event_unchecked` discards a FLUSH_STOP on an INACTIVE pad
    /// outright (gstpad.c:6258, "we can't accept flush-stop on inactive pads"),
    /// and a descent to READY or NULL deactivates every pad. Splitting a pair
    /// around the descent would therefore strand the FLUSH_START at the sink,
    /// which is exactly the `flush_pairs_matched` violation that
    /// [`Inner::flush_parked_text_pushes`] records. Keeping the pair on one
    /// thread keeps it matched; the rescue only ever moves the descent
    /// underneath a pair that is already stuck.
    ///
    /// # RESIDUAL, measured, not closed
    ///
    /// A rescued teardown can still report `pipeline (Paused, Ready)`, the
    /// attempt-2 signature of a descent that could not finish. Measured on
    /// `fuzz_buffering` seed 1600039, ITERS=4 ACTIONS=22: 0 failures in 15
    /// runs with only this fix in the tree, 2 in 18 with the `poll_text_policy`
    /// stomped-subtitle guard in as well (which changes what is linked to a
    /// renderer at teardown, hence what the wake is parked on), against 3
    /// of 3 failing before either. The window is the one interval in which the
    /// wake HOLDS rather than waits for a stream lock: `dispose_text_branch_on`
    /// acquires the branch tail's stream lock and keeps it while the
    /// FLUSH_STOP forwards, and a descent that reaches the tail inside that
    /// interval queues behind it. It is bounded, not a wedge (the failing runs
    /// report on the driver's 15 s worker bound and then unwind normally,
    /// in the same total runtime as a passing one), so what is left is a
    /// rescue budget that eats 5 s of that 15 s rather than a new deadlock.
    /// Closing it needs the wake to hold no serialized event at all, which
    /// is the shape the FLUSH_START split could not reach for the reason
    /// recorded above.
    pub(crate) fn teardown(&self, target: gst::State) -> Result<()> {
        // A chain parked by a mid-item deselect is state-locked. Unlock it
        // so it follows the pipeline down.
        self.inner.video_sink.set_locked_state(false);
        // Wake any prepared-input thread parked on the swap gate BEFORE
        // the state change, which joins streaming threads.
        self.inner.swap_gate.abort();
        *self.inner.prepared.lock() = None;
        // Both calls below can park forever on a push that only the descent
        // releases, and on the worker the descent is a job only this thread
        // runs. Arm the rescue across them (see the docs above).
        let rescue = WakeRescue::arm(&self.inner, target);
        // Postponed branch disposals first. The parked-push flush below
        // cannot see them, and it wedges on a multiqueue task blocked
        // mid-push into an undisposed queue (see
        // `Inner::drain_disposals_for_teardown`).
        self.inner.drain_disposals_for_teardown();
        // And wake every parked text push, or the downward change
        // deadlocks on the pad locks those pushes hold (see
        // `Inner::flush_parked_text_pushes`).
        self.inner.flush_parked_text_pushes();
        // Disarmed (and joined) BEFORE the gate below, so the two threads
        // never contend for it - and BOUNDED, so a rescue whose own descent is
        // stuck below the crate releases this thread instead of keeping it (see
        // `WakeRescue::disarm` for the captured stacks).
        //
        // # Why a detached rescue ENDS the teardown here
        //
        // Every remaining step of this function goes through the pipeline, and
        // the detached thread is inside the pipeline's state change holding the
        // route gate. So each of them blocks the worker UNBOUNDED, which is the
        // wedge the bound above just escaped:
        //
        // * `Inner::gate` below is the same mutex the rescue holds and will hold for as
        //   long as its `set_state` lasts - which is forever, by construction of this
        //   arm;
        // * `set_state(target)` queues on the pipeline's own STATE_LOCK behind that
        //   same call;
        // * `remove_all_inputs` flushes and NULLs each input, and `remove_audio_sink`
        //   NULLs and removes - all of them element state changes against a bin
        //   mid-descent.
        //
        // The crate-state resets further down are harmless in isolation, but
        // they are also pointless: nothing may load again (see
        // `Inner::teardown_poisoned`), so there is no next item for them to be
        // correct for. Skipping the whole tail keeps this arm to one rule -
        // TOUCH NOTHING - which is the only rule that is checkable by
        // inspection.
        //
        // The DROP path stays reachable and is already built for this:
        // `Teardown::run`'s descent is bounded and detached in turn
        // (`TEARDOWN_DESCENT_BUDGET`, counted by `teardown_descent_stuck`), so
        // the last reference going away cannot wedge its thread either. Its
        // pre-descent disposals and flushes carry the pre-existing exposure
        // that the scope note above already records, and nothing here makes them worse.
        if rescue.disarm() == DisarmOutcome::Detached {
            self.inner.teardown_poisoned.store(true, Ordering::SeqCst);
            return Err(anyhow!(
                "the teardown's rescue descent to {target:?} never returned; the pipeline is \
                 wedged below this crate and the worker has stopped touching it"
            ));
        }
        {
            let _gate = Inner::gate(&self.inner);
            self.inner
                .pipeline
                .set_state(target)
                .with_context(|| format!("pipeline to {target:?} for teardown"))?;
        }
        Inner::remove_all_inputs(&self.inner);
        self.inner.remove_audio_sink();
        // A stop IS an item ending, so it ends everything the item owned - the
        // same list a load clears, and the same list by construction (see
        // `Inner::reset_item_state`). No generation is allocated here: a stop
        // ends an item without starting one, and the events a straggler still
        // posts belong to the item that just ended.
        self.inner.reset_item_state();
        Ok(())
    }
}
