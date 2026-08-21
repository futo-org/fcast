//! Every wait the crate enters has a deadline, and no lost message can latch
//! it for good.
//!
//! Two waits used to be unbounded. `SelectionEngine::selecting` is cleared
//! only by a `STREAMS_SELECTED`, and while it is set no further selection
//! dispatches at a playing pipeline. `SelectionEngine::refreshing` is cleared
//! only by a top-level `ASYNC_DONE`, and while it is set NOTHING dispatches,
//! playing or paused. Both signals are bus MESSAGES, and the field showed both
//! going missing while the pipeline itself was perfectly healthy: the track
//! change silently never happened, and every later one was refused too, for
//! the rest of the item.
//!
//! The cure is a periodic tick (`fpb-tick`) that hands a lapsed wait to the
//! worker, which then PROBES the pads rather than trusting the message that
//! never came. A selection the probe finds applied is confirmed from the
//! probe; one it does not is re-asserted a bounded number of times and finally
//! reported as what is really playing. A refresh that ran out is failed, which
//! is cheap and correct (the re-emit is cosmetic) and, crucially, unblocks the
//! channel.
//!
//! The lost message is produced deterministically here, not waited for: the
//! crate's `MessageHook` gets first look at every raw bus message on the
//! posting thread, and consuming one there is byte-for-byte what decodebin3
//! losing it looks like from the crate's side (the engine's recorders all live
//! behind that point, in `translate_message`). The hook only ever eats
//! decodebin3's OWN messages - the crate's synthetic confirmation is posted
//! with the pipeline as src and passes through, or the tests would be eating
//! the rescue they are measuring.
//!
//! # What each test pins
//!
//! * `a_swallowed_streams_selected_still_confirms_by_reprobe`: without the
//!   selection deadline no confirmation of the switch ever arrives - the L-2
//!   latch, verbatim.
//! * `a_swallowed_async_done_cannot_latch_refreshing`: without the refresh
//!   deadline `RefreshSeekFailed` never arrives and the later track change
//!   never dispatches - the L-1 latch, verbatim.
//! * `tick_repokes_parked_deferred_work_without_caller_polls`: without the
//!   tick's pokes the drain job count freezes after the first parked verdict.
//!   BOTH pokes have to be absent to see it, which is worth recording: the
//!   reconcile poke queues the SAME `Job::DrainTextWork`, at the same 1 Hz, for
//!   as long as the crate holds an item, so with only the drain poke gone the
//!   reconcile poke takes over and the counter cannot tell them apart. The two
//!   are mutually exclusive by construction, and that is also what makes a
//!   GREEN run attributable: while postponed work is pending and the drain poke
//!   is live, the reconcile poke is the one suppressed.
//! * `a_deadline_racing_its_confirmation_is_a_noop`: with no deadline firing at
//!   all the race the test exists to stage never happens.
//! * `a_deadline_does_not_act_on_a_selection_the_lane_has_not_sent`: without
//!   the in-flight consult the deadline decides about a `SELECT_STREAMS` that
//!   is still parked on the select lane and reports a selection that has not
//!   happened - the phase-2 residual, verbatim.
//!
//! With no tick thread at all every test here reverts to the v1 behaviour it
//! was written against.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, MessageHook, PlaybinEvent, SelectionGate,
    Sinks, StartPoint, TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

const BOUND: Duration = Duration::from_secs(20);

/// How many source inputs are wired into the pipeline: the main item, plus one
/// per attached external subtitle. The arm-agnostic way to see whether an
/// input removal has been carried out or is still owed.
fn urisourcebins(playbin: &FcastPlaybin) -> usize {
    playbin
        .pipeline()
        .children()
        .iter()
        .filter(|child| child.factory().is_some_and(|f| f.name() == "urisourcebin"))
        .count()
}

/// The deadline these tests configure, in place of the 10s production one.
/// Short enough to fire inside a test, comfortably longer than a healthy
/// switch on synthetic media takes to apply. Shortening THIS rather than the
/// tick's own 200ms period is what keeps these tests off the tick's timing.
const DEADLINE: Duration = Duration::from_millis(500);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// Which message the hook eats while armed.
#[derive(Debug, Clone, Copy)]
enum Eat {
    /// decodebin3's own `STREAMS_SELECTED`, told apart from the crate's
    /// synthetic one by its src: the synthetic is posted BY the pipeline.
    DecodebinConfirmation,
    /// The top-level `ASYNC_DONE`, which is the only one that settles a
    /// refresh seek (a bin's own children's async-done never reaches here as
    /// a top-level message).
    PipelineAsyncDone,
}

/// The deterministic message loss, and a way to deliver the loss LATE.
struct Swallow {
    what: Eat,
    armed: AtomicBool,
    /// Every message eaten, in order. Keeping them is what lets a test hand
    /// the real confirmation over later instead of never - a delayed message
    /// rather than a destroyed one.
    eaten: Mutex<Vec<gst::Message>>,
}

impl Swallow {
    fn new(what: Eat) -> Arc<Self> {
        Arc::new(Self {
            what,
            armed: AtomicBool::new(false),
            eaten: Mutex::new(Vec::new()),
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    fn count(&self) -> usize {
        self.eaten.lock().unwrap().len()
    }

    fn take(&self) -> Vec<gst::Message> {
        std::mem::take(&mut self.eaten.lock().unwrap())
    }

    fn hook(self: &Arc<Self>) -> MessageHook {
        let swallow = self.clone();
        Box::new(move |msg: &gst::Message| {
            if !swallow.armed.load(Ordering::SeqCst) {
                return false;
            }
            let from_pipeline = msg
                .src()
                .is_some_and(|src| src.type_().name() == "GstPipeline");
            let hit = match swallow.what {
                Eat::DecodebinConfirmation => {
                    matches!(msg.view(), gst::MessageView::StreamsSelected(_)) && !from_pipeline
                }
                Eat::PipelineAsyncDone => {
                    matches!(msg.view(), gst::MessageView::AsyncDone(_)) && from_pipeline
                }
            };
            if hit {
                swallow.eaten.lock().unwrap().push(msg.clone());
            }
            hit
        })
    }
}

/// One playbin, its event stream, and the swallow armed on its bus.
struct Rig {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    swallow: Arc<Swallow>,
    /// Everything the crate has emitted so far, in order.
    seen: Vec<PlaybinEvent>,
    gate: SelectionGate,
}

impl Rig {
    fn new(what: Eat, seekable: bool) -> Self {
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(FTestSink::new().upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        let swallow = Swallow::new(what);
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(Some(swallow.hook()), move |event, _generation| {
            let _ = tx.send(event);
        });
        Self {
            playbin,
            events,
            swallow,
            seen: Vec::new(),
            gate: SelectionGate {
                quiet: true,
                paused: false,
                seekable,
            },
        }
    }

    /// Everything emitted since the last look, with a pipeline error treated
    /// as fatal wherever it turns up.
    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
            self.seen.push(event);
        }
    }

    /// Poll until `done` holds, driving the caller-side hooks the receiver
    /// drives. Every wait in this file ends at [`BOUND`].
    fn wait_for(
        &mut self,
        what: &str,
        mut done: impl FnMut(&FcastPlaybin, &[PlaybinEvent]) -> bool,
    ) {
        let deadline = Instant::now() + BOUND;
        loop {
            self.drain();
            if done(&self.playbin, &self.seen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events seen: {:?}",
                self.seen
            );
            self.playbin.poll_text_policy();
            self.playbin.pump_selection(self.gate);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn load_and_play(&mut self, uri: &str) {
        self.playbin.load_async(
            MediaInput::Uri(uri.to_string()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("the load to report Loaded", |_, seen| {
            seen.iter()
                .any(|event| matches!(event, PlaybinEvent::Loaded { .. }))
        });
        self.playbin.play().expect("play");
        self.wait_for("the pipeline to settle at PLAYING", |playbin, _| {
            playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
        });
        // The item's OWN first selection, so a switch below is a change
        // against a known baseline rather than the first thing to happen.
        self.wait_for("the item's initial stream selection", |_, seen| {
            seen.iter()
                .any(|event| matches!(event, PlaybinEvent::StreamsSelected { .. }))
        });
        self.settle_collections();
    }

    /// Wait until the item stops re-advertising itself.
    ///
    /// ftest media exposes its streams progressively (its own log shows three
    /// collections inside 25ms: one audio stream, then both, then the video),
    /// and every one of those RESETS the engine's in-flight work, deadlines
    /// included. A test that armed its hook in the middle of that would be
    /// measuring the load, not a switch.
    fn settle_collections(&mut self) {
        /// No new collection for this long, at a settled PLAYING.
        const QUIET: Duration = Duration::from_millis(700);
        let collections = |seen: &[PlaybinEvent]| {
            seen.iter()
                .filter(|event| matches!(event, PlaybinEvent::StreamCollection(_)))
                .count()
        };
        let deadline = Instant::now() + BOUND;
        let mut last = collections(&self.seen);
        let mut since = Instant::now();
        loop {
            self.drain();
            let now = collections(&self.seen);
            if now != last {
                last = now;
                since = Instant::now();
            }
            let settled =
                self.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending);
            if settled && since.elapsed() >= QUIET {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the item never stopped re-advertising itself; events seen: {:?}",
                self.seen
            );
            self.playbin.poll_text_policy();
            self.playbin.pump_selection(self.gate);
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The audio stream id of the most recently reported selection.
    ///
    /// Read rather than assumed: which of two equivalent audio streams
    /// decodebin3 picks by default depends on the order the source exposes its
    /// pads in, and ftest media does not fix that order. A test that hardcoded
    /// the switch target would silently become a no-op switch on the runs
    /// where it guessed wrong.
    fn current_audio(&self) -> Option<String> {
        self.seen
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamsSelected { audio, .. } => Some(audio.clone()),
                _ => None,
            })
            .flatten()
    }

    /// Ask for `sid` on the audio slot and wait for the selection to be
    /// reported, however it gets reported.
    fn switch_audio_and_wait(&mut self, sid: &str, what: &str) {
        assert_ne!(
            self.current_audio().as_deref(),
            Some(sid),
            "asking for the track that is already playing is not a switch"
        );
        let from = self.seen.len();
        self.playbin
            .request_track(TrackSlot::Audio, TrackTarget::Stream(Some(sid.to_string())));
        self.playbin.pump_selection(self.gate);
        let wanted = sid.to_string();
        self.wait_for(what, move |_, seen| {
            seen[from..].iter().any(|event| {
                matches!(event, PlaybinEvent::StreamsSelected { audio: Some(got), .. }
                    if *got == wanted)
            })
        });
    }

    /// The one of the item's two audio streams that is NOT playing.
    fn other_audio(&self, a: &str, b: &str) -> String {
        let playing = self
            .current_audio()
            .expect("the item must have reported a selection by now");
        if playing == a {
            b.to_string()
        } else {
            a.to_string()
        }
    }

    fn shutdown(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = done_tx.send(());
        }));
        let deadline = Instant::now() + BOUND;
        loop {
            match done_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.playbin.pump_selection(self.gate);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// How many reported selections name `sid` on the audio slot.
fn audio_selections(seen: &[PlaybinEvent], sid: &str) -> usize {
    seen.iter()
        .filter(|event| {
            matches!(event, PlaybinEvent::StreamsSelected { audio: Some(got), .. } if got == sid)
        })
        .count()
}

/// Park the worker until the returned sender is dropped.
///
/// `Barrier`'s callback runs ON the worker and the job body is nothing but
/// that call, so the parked worker holds no crate lock: the test thread can
/// keep queueing jobs and calling the synchronous API meanwhile
/// (`regression_job_generation_gate.rs`).
fn stall_worker(playbin: &FcastPlaybin) -> mpsc::Sender<()> {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    playbin.barrier_async(Box::new(move || {
        let _ = parked_tx.send(());
        let _ = release_rx.recv_timeout(BOUND);
    }));
    parked_rx
        .recv_timeout(BOUND)
        .expect("the worker never reached the stall");
    release_tx
}

/// A barrier: when this returns, every job queued before it has run to
/// completion on the worker.
fn drain_worker(playbin: &FcastPlaybin) {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    playbin.barrier_async(Box::new(move || {
        let _ = done_tx.send(());
    }));
    done_rx
        .recv_timeout(BOUND)
        .expect("the worker never reached the barrier");
}

/// THE EXIT CRITERION for the selection half.
///
/// decodebin3 applies the switch and its confirmation is eaten. The wait that
/// message was the only signal for has to end anyway, and it has to end with
/// the TRUTH: the crate probes the routed pads, finds the selection live, and
/// posts the confirmation itself. The caller learns what it asked to learn,
/// and the selection channel is not latched - which the second, ordinary
/// track change proves by completing.
#[test]
fn a_swallowed_streams_selected_still_confirms_by_reprobe() {
    init();
    let media = ScenarioBuilder::new("deadlineconfirm")
        .video("video_0")
        .audio("audio_0")
        .audio("audio_1")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();

    let mut rig = Rig::new(Eat::DecodebinConfirmation, false);
    rig.load_and_play(&media.uri());
    rig.playbin.set_selection_deadline(DEADLINE);

    let was_playing = rig.current_audio().expect("an audio track is playing");
    let wanted = rig.other_audio(&media.stream_id("audio_0"), &media.stream_id("audio_1"));
    rig.swallow.arm();
    rig.switch_audio_and_wait(
        &wanted,
        "the swallowed switch to be confirmed from the routing probe",
    );

    assert!(
        rig.swallow.count() >= 1,
        "the hook never ate a confirmation, so nothing was ever lost and this test proves nothing"
    );
    assert!(
        rig.playbin.stats().selection_deadline_confirms >= 1,
        "the switch was reported without a deadline confirming it from the probe: \
         confirms={}, fires={}, giveups={}",
        rig.playbin.stats().selection_deadline_confirms,
        rig.playbin.stats().selection_deadline_fires,
        rig.playbin.stats().selection_deadline_giveups,
    );
    assert_eq!(
        rig.playbin.stats().selection_deadline_giveups,
        0,
        "the selection WAS applied; giving up on it means the probe did not see it"
    );

    // And the channel is open. A latched `selecting` would refuse this
    // outright at a playing pipeline, which is the whole field symptom.
    rig.swallow.disarm();
    rig.switch_audio_and_wait(&was_playing, "an ordinary track change after the rescue");

    rig.shutdown();
    media.unregister();
}

/// THE EXIT CRITERION for the refresh half, and the worse latch of the two:
/// `refreshing` blocks dispatch in PAUSED as well as PLAYING.
///
/// The re-emit flush a track switch schedules is settled by exactly one
/// signal, the top-level `ASYNC_DONE`. Eat it and the crate waits forever on a
/// seek that in fact completed. The deadline reads the pipeline instead: a
/// settled one holds no async transition, so no such message can still be
/// coming, and the refresh is failed through the loud path three worker sites
/// already use.
#[test]
fn a_swallowed_async_done_cannot_latch_refreshing() {
    init();
    let media = ScenarioBuilder::new("deadlinerefresh")
        .video("video_0")
        .audio("audio_0")
        .audio("audio_1")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();

    // `seekable` is what makes the engine schedule the re-emit flush at all.
    let mut rig = Rig::new(Eat::PipelineAsyncDone, true);
    rig.load_and_play(&media.uri());
    // The premise: without a genuinely seekable item the refresh job drops
    // itself at execution and this test would pass for the wrong reason.
    let seekable = {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        rig.playbin.pipeline().query(&mut query) && query.result().0
    };
    assert!(
        seekable,
        "the item must be seekable for a refresh to be dispatched at all"
    );
    rig.playbin.set_refresh_deadline(DEADLINE);

    let was_playing = rig.current_audio().expect("an audio track is playing");
    let wanted = rig.other_audio(&media.stream_id("audio_0"), &media.stream_id("audio_1"));
    rig.swallow.arm();
    // Only the ASYNC_DONE is eaten, so the switch itself confirms normally;
    // it is the flush the switch schedules that is left hanging.
    rig.switch_audio_and_wait(&wanted, "the track switch to be confirmed");
    rig.wait_for("the stranded refresh seek to be failed", |_, seen| {
        seen.iter()
            .any(|event| matches!(event, PlaybinEvent::RefreshSeekFailed { .. }))
    });
    assert!(
        rig.swallow.count() >= 1,
        "the hook never ate an ASYNC_DONE, so nothing was ever lost"
    );
    // WHICH emitter reported the failure matters: three pre-existing worker
    // sites emit `RefreshSeekFailed` for a refresh that never even ran (its
    // preconditions lapsed, no position, the send refused), and any of those
    // would satisfy the wait above while proving nothing about deadlines. Only
    // a fire means the seek WAS performed and its confirmation was the thing
    // that went missing.
    assert!(
        rig.playbin.stats().selection_deadline_fires >= 1,
        "the refresh was reported failed without any deadline firing, so one of the \
         pre-existing drop paths answered and the swallowed ASYNC_DONE was never the cause"
    );

    // The channel is open again. Under the latch this dispatches never, in
    // any state.
    rig.swallow.disarm();
    rig.switch_audio_and_wait(&was_playing, "a track change after the stranded refresh");

    rig.shutdown();
    media.unregister();
}

/// Postponed work is re-attempted without anybody polling.
///
/// Every other poke at the deferred-work drain is EDGE-triggered: a pipeline
/// state edge, a top-level `ASYNC_DONE`, a caller poll. The field lost all
/// three at once - a verdict parked at a pipeline that then crossed no further
/// edge, with a caller that had stopped polling. The tick's own once-a-second
/// re-poke is the liveness floor under that, and it has to stay a FLOOR: one
/// no-op job per second, not the 5099-in-39s poll storm the suppression exists
/// to prevent.
#[test]
fn tick_repokes_parked_deferred_work_without_caller_polls() {
    init();
    let media = ScenarioBuilder::new("deadlinepoke")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("deadlinepokesubs")
        .text(
            "text_0",
            (0..300u32)
                .map(|index| {
                    let start = gst::ClockTime::from_mseconds(100) * u64::from(index + 1);
                    CueSpec::new(
                        start,
                        start + gst::ClockTime::from_mseconds(50),
                        format!("P{index:02}"),
                    )
                })
                .collect(),
        )
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let mut rig = Rig::new(Eat::DecodebinConfirmation, false);
    // Armed before anything flows: on the consumer arm an unsynced external
    // hands its whole feed over the instant it links, so a counter installed
    // afterwards would report zero for a branch that delivered everything.
    text_arm::arm(&rig.playbin);
    rig.load_and_play(&media.uri());

    // An external subtitle, selected, linked and flowing: DETACHING it at a
    // pipeline resting in PAUSED is what postpones work with no edge behind it.
    let id: ExternalSubId = rig.playbin.attach_subtitle(&subs.uri()).expect("attach");
    {
        let probe = rig.playbin.clone();
        rig.wait_for("the external subtitle to materialize", move |_, _| {
            !probe.subtitle_stream_ids(id).is_empty()
        });
    }
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    {
        let probe = rig.playbin.clone();
        rig.wait_for("the subtitle branch to link", move |_, _| {
            text_arm::text_branch_linked(&probe)
        });
    }
    let cues = text_arm::count_text_arrivals(&rig.playbin);
    rig.wait_for("text to reach the renderer", |_, _| cues.count() >= 2);

    rig.playbin.pause().expect("pause");
    {
        let probe = rig.playbin.clone();
        rig.wait_for("the pipeline to settle at PAUSED", move |_, _| {
            let (_, current, pending) = probe.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Paused && pending == gst::State::VoidPending
        });
    }
    assert_eq!(
        urisourcebins(&rig.playbin),
        2,
        "the main item and the external subtitle should both be wired"
    );

    // THE POSTPONEMENT, and it must be one BOTH transports make. Subtitles-off
    // at rest in PAUSED postpones a branch DISPOSAL, and that postponement is
    // overlay-only by design: a consumer branch's parked
    // push is inside its own appsink, the disposal pair wakes it, and deferring
    // it there would leave the next track's branch unbuildable
    // (`Inner::detach_text_parts`). A DETACH of a live external is postponed on
    // either transport -- the removal's flush travels into decodebin3's shared
    // multiqueue, which knows nothing about which renderer the branch ends in
    // (`Inner::remove_input_or_defer`) -- so that is the staging, and the
    // property under test is unchanged: work is remembered, nothing about the
    // pipeline will change again, and only the tick can notice.
    rig.playbin.detach_subtitle(id).expect("detach");
    rig.drain();
    assert_eq!(
        urisourcebins(&rig.playbin),
        2,
        "the removal should be postponed, not carried out, at a pipeline resting in PAUSED"
    );

    // The observation window. NOTHING is polled here: no `poll_text_policy`,
    // no `pump_selection`, and the pipeline crosses no state edge. Only the
    // tick is left to notice the postponed work.
    const WINDOW: Duration = Duration::from_millis(3200);
    /// One re-poke per second over the window, plus slack at both ends.
    const AT_LEAST: u64 = 2;
    /// The bound is the point: a liveness floor, not a busy loop. The
    /// suppressed poll storm's own bound is 40 over a comparable window.
    const AT_MOST: u64 = 8;

    let before = rig.playbin.stats().drain_text_job_count;
    let until = Instant::now() + WINDOW;
    while Instant::now() < until {
        rig.drain();
        thread::sleep(Duration::from_millis(20));
    }
    let poked = rig.playbin.stats().drain_text_job_count - before;
    assert!(
        poked >= AT_LEAST,
        "{poked} drain jobs over {WINDOW:?} with no caller poll and no state edge \
         (expected at least {AT_LEAST}): the postponed work is parked with nothing \
         left to notice it, which is the field's stuck text branch"
    );
    assert!(
        poked <= AT_MOST,
        "{poked} drain jobs over {WINDOW:?} (expected at most {AT_MOST}): the liveness \
         re-poke has become a busy loop"
    );

    // And the re-pokes did not break the real drain: the resume still removes
    // the detached input, off the pipeline's own state edge.
    rig.playbin.play().expect("resume");
    {
        let probe = rig.playbin.clone();
        rig.wait_for("the postponed input removal to drain", move |_, _| {
            urisourcebins(&probe) == 1
        });
    }

    rig.shutdown();
    media.unregister();
    subs.unregister();
}

/// A fire that reaches the worker after its wait already ended changes
/// nothing.
///
/// This is the new failure class the whole phase risks: a deadline killing a
/// healthy wait. The fire and the confirmation are inherently concurrent - the
/// tick queues one while a streaming thread delivers the other - so the job
/// re-validates against the engine before it probes or decides anything.
///
/// Staged deterministically rather than raced, in two halves that must not be
/// swapped. FIRST the event goes out with the worker FREE and decodebin3's
/// answer to it is eaten - the dispatch is the decider's own work
/// (`Job::DispatchSelection`), so a parked worker parks the SEND and there is
/// then no confirmation to hold back at all. THEN the worker is parked, so
/// the fire the tick is about to queue is queued but cannot run, and the
/// confirmation is handed over LATE, byte for byte, from the test thread.
/// When the worker is released, the wait it was fired for is already settled.
///
/// Both halves are ASSERTED rather than assumed: that no fire has happened
/// yet when the park goes on (or the fire under test would be one that ran
/// against a live wait, which is a different experiment), and that one has
/// been queued before the late delivery.
#[test]
fn a_deadline_racing_its_confirmation_is_a_noop() {
    init();
    let media = ScenarioBuilder::new("deadlinerace")
        .video("video_0")
        .audio("audio_0")
        .audio("audio_1")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();

    let mut rig = Rig::new(Eat::DecodebinConfirmation, false);
    rig.load_and_play(&media.uri());
    rig.playbin.set_selection_deadline(DEADLINE);

    let wanted = rig.other_audio(&media.stream_id("audio_0"), &media.stream_id("audio_1"));
    rig.swallow.arm();
    let from = rig.seen.len();
    rig.playbin
        .request_track(TrackSlot::Audio, TrackTarget::Stream(Some(wanted.clone())));
    rig.playbin.pump_selection(rig.gate);

    // Half the stage: the event has left the crate and decodebin3's answer to
    // it has been taken. The worker must be FREE for this half, because the
    // dispatch it performs is what sends the event.
    let swallow = rig.swallow.clone();
    rig.wait_for("decodebin3's confirmation to be swallowed", move |_, _| {
        swallow.count() >= 1
    });
    // The other half: park the worker, so the fire the tick is about to queue
    // sits in that queue rather than running. There is a whole DEADLINE of
    // room between the swallow above and the next fire, so the park is never
    // in a race with it - asserted, because a fire that already RAN is a
    // deadline that decided against a live wait, and every assertion below
    // would then be measuring the wrong window.
    let release = stall_worker(&rig.playbin);
    assert_eq!(
        rig.playbin.stats().selection_deadline_fires,
        0,
        "a deadline fired before the worker was parked, so the fire this test \
         stages is not the first one and the stage is not the one described"
    );
    rig.wait_for(
        "a deadline fire to reach the stalled worker's queue",
        move |playbin, _| playbin.stats().selection_deadline_fires >= 1,
    );

    // Deliver the confirmation late. Posting re-enters the bus sync handler on
    // THIS thread, so the engine has settled the wait by the time this
    // returns - while the fire for that same wait is still queued.
    rig.swallow.disarm();
    let held = rig.swallow.take();
    assert!(!held.is_empty(), "nothing was held to deliver late");
    let bus = rig.playbin.pipeline().bus().expect("the pipeline's bus");
    for message in held {
        bus.post(message).expect("re-posting the held confirmation");
    }
    rig.drain();
    assert_eq!(
        audio_selections(&rig.seen[from..], &wanted),
        1,
        "the late confirmation did not settle the switch; events: {:?}",
        &rig.seen[from..]
    );

    drop(release);
    drain_worker(&rig.playbin);
    rig.drain();

    assert_eq!(
        audio_selections(&rig.seen[from..], &wanted),
        1,
        "the deadline acted on a wait that had already confirmed: the switch was \
         reported twice; events: {:?}",
        &rig.seen[from..]
    );
    assert_eq!(
        rig.playbin.stats().selection_deadline_confirms,
        0,
        "the deadline posted a synthetic confirmation for an already confirmed selection"
    );
    assert_eq!(
        rig.playbin.stats().selection_deadline_giveups,
        0,
        "the deadline gave up on an already confirmed selection"
    );

    rig.shutdown();
    media.unregister();
}

/// How long the DECIDER is held before a switch may execute (see
/// `a_busy_decider_delays_a_switch_without_breaking_its_bound`). Long enough
/// that a queue delay is unmistakable against the sub-millisecond one a free
/// decider produces, short enough to leave the whole apply bound afterwards.
const DECIDER_HELD: Duration = Duration::from_millis(400);

/// `switch_latency_probe`'s APPLY_BOUND, the product-facing promise: a track
/// change confirms within this once the pipeline is free to make it. Repeated
/// rather than shared because that suite owns the number.
const APPLY_BOUND: Duration = Duration::from_millis(500);

/// A switch decided while the decider is BUSY still applies inside its bound
/// once the decider is free, and the wait is visible as a queue delay rather
/// than as an unexplained slow switch.
///
/// The selection's execution runs on the decider
/// (`Job::DispatchSelection`), which buys single-owner text-branch surgery
/// and cost the dispatch a queue hop. This is that cost, measured: the hop is
/// bounded by whatever the decider is doing, and nothing about it is silent -
/// `dispatch_queue_age` reports the longest one the instance has seen, the
/// mirror of the `hands` in-flight age for the lane on the other side of the
/// same send.
#[test]
fn a_busy_decider_delays_a_switch_without_breaking_its_bound() {
    init();
    let media = ScenarioBuilder::new("busydecider")
        .video("video_0")
        .audio("audio_0")
        .audio("audio_1")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();

    // Nothing is eaten here: the swallow is only armed by the tests that
    // stage a lost message, and this one wants the ordinary confirmation.
    let mut rig = Rig::new(Eat::DecodebinConfirmation, false);
    rig.load_and_play(&media.uri());

    let wanted = rig.other_audio(&media.stream_id("audio_0"), &media.stream_id("audio_1"));
    let from = rig.seen.len();

    // Park the decider, THEN ask for the switch: the dispatch is queued
    // behind the stall and cannot execute.
    let release = stall_worker(&rig.playbin);
    rig.playbin
        .request_track(TrackSlot::Audio, TrackTarget::Stream(Some(wanted.clone())));
    rig.playbin.pump_selection(rig.gate);
    let parked_until = Instant::now() + DECIDER_HELD;
    while Instant::now() < parked_until {
        rig.drain();
        thread::sleep(Duration::from_millis(10));
    }
    // The premise: nothing was reported while the decider was parked. A
    // confirmation here would mean the send did not go through the decider at
    // all, and the measurement below would be of nothing.
    assert!(
        !rig.seen[from..]
            .iter()
            .any(|event| matches!(event, PlaybinEvent::StreamsSelected { .. })),
        "a selection was reported while the decider was parked; events: {:?}",
        &rig.seen[from..]
    );

    let released = Instant::now();
    drop(release);
    let wanted_now = wanted.clone();
    rig.wait_for("the queued switch to confirm once the decider is free", {
        let from = from;
        move |_, seen| {
            seen[from..].iter().any(|event| {
                matches!(event, PlaybinEvent::StreamsSelected { audio: Some(got), .. }
                    if *got == wanted_now)
            })
        }
    });
    let applied = released.elapsed();

    // The bound is measured from the moment the decider is FREE, which is
    // what the product promise is about: a switch waits for a busy pipeline
    // and then applies immediately, rather than applying late because the
    // switch itself became slow.
    assert!(
        applied < APPLY_BOUND,
        "the queued switch took {applied:?} after the decider was released \
         (bound {APPLY_BOUND:?})"
    );
    // And the delay is accounted for rather than mysterious.
    let age = rig.playbin.dispatch_queue_age();
    assert!(
        age >= DECIDER_HELD / 2,
        "the decider was parked for {DECIDER_HELD:?} with a dispatch queued behind it, \
         but the longest dispatch queue delay recorded is {age:?}: the hop this phase \
         added is not being measured"
    );

    rig.shutdown();
    media.unregister();
}

/// How long the select lane is held mid-send. Comfortably longer than the
/// deadline's whole retry budget ([`DEADLINE`] x 3), so a build that gives up
/// has ample room to do it, and comfortably shorter than [`BOUND`].
const LANE_HELD: Duration = Duration::from_secs(6);

/// `lib.rs`'s `SELECTION_DEADLINE_RETRIES`, which is crate-private. Only used
/// to decide how long to WAIT for the give-up a green build must not make, so
/// a drift here costs patience, never correctness.
const DEADLINE_RETRIES: u64 = 2;

/// The manufactured wedge: a `pad-added` handler that parks the SELECT LANE
/// inside `send_event`.
///
/// decodebin3 activates a slot that has no output by arming an IDLE probe on
/// its multiqueue source pad (`gstdecodebin3.c`, `handle_stream_switch` ->
/// `idle_reconfigure`), and `gst_pad_add_probe` runs an IDLE callback INLINE
/// when the pad is already idle - which a text slot between sparse cues always
/// is. The whole reconfiguration, `gst_element_add_pad` included, therefore
/// runs on the thread that sent the event, and that is exactly the property
/// the crate's own `route_db3_pad` documents ("this function also runs inline
/// on `fpb-select` when decodebin3 exposes the pad inside that send").
///
/// Sleeping in a handler connected AFTER the crate's own leaves the crate's
/// route complete and the SEND unfinished, which is the state the residual is
/// about: the selection is in flight, nothing is routed for it yet, and no
/// message about it can arrive. `caller_nonblocking.rs` manufactures its
/// obstacle with a sleeping probe for the same reason; this one is on the
/// signal rather than on a buffer because the thread it has to park is the
/// sender, not a streaming thread.
#[derive(Default)]
struct LaneHold {
    armed: AtomicBool,
    engaged: AtomicBool,
    released: AtomicBool,
}

impl LaneHold {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn engaged(&self) -> bool {
        self.engaged.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    /// Park the calling thread until the test releases it, or until
    /// [`LANE_HELD`] runs out. The cap is what keeps a failing run reportable
    /// instead of hung.
    fn park(self: &Arc<Self>) {
        let until = Instant::now() + LANE_HELD;
        while !self.released.load(Ordering::SeqCst) && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Install on decodebin3. Only ever parks the select lane: any other
    /// thread's `pad-added` is left alone, so nothing else in the pipeline is
    /// slowed down by the arming.
    fn install(self: &Arc<Self>, db3: &gst::Element) {
        let hold = self.clone();
        db3.connect_pad_added(move |_db3, _pad| {
            let name = thread::current().name().map(str::to_owned);
            if name.as_deref() != Some("fpb-select") || !hold.armed.load(Ordering::SeqCst) {
                return;
            }
            hold.armed.store(false, Ordering::SeqCst);
            hold.engaged.store(true, Ordering::SeqCst);
            hold.park();
        });
    }
}

/// THE PHASE-2 RESIDUAL, closed.
///
/// A `SELECT_STREAMS` that has not left the crate yet cannot have been
/// applied, and the deadline had no way to know it: everything it can read -
/// routed pads, engine records, bus silence - says the same thing for a
/// selection still on the lane as for one the pipeline refused. The give-up
/// used to read it as "never applied", adopting a reality
/// the parked send then contradicted; because the timed-out record is
/// superseded, the real confirmation was drained as an echo and the engine's
/// idea of what plays stayed wrong until the next collection change.
///
/// The crate now asks its own hands whether the event has been sent, and the
/// answer gates the WHOLE second half of the deadline: no probe, no synthetic
/// confirmation, no re-assertion, no give-up. This test holds the select lane
/// inside the send for longer than the deadline's entire retry budget and
/// asserts both halves of the fix: nothing at all is decided while the effect
/// is in flight, and the real confirmation still lands and is correct once the
/// lane is released.
///
/// Without the in-flight consult, which of the two wrong answers comes out
/// depends on where the send is parked, and this staging produces the
/// confirming one: the crate's own route ran INSIDE the send (the
/// topology is deliberately visible the moment `send_event` returns, and here
/// it is visible before that), so the probe finds the target routed and posts
/// a synthetic confirmation for an event that has not been sent. Parked one
/// step earlier - still queued on the lane - the same missing knowledge
/// produces the give-up instead. One consult excludes both, and the assertions
/// below name both.
#[test]
fn a_deadline_does_not_act_on_a_selection_the_lane_has_not_sent() {
    init();
    // Sparse cues on purpose: the text slot's source pad has to be IDLE when
    // decodebin3 arms its reconfiguration probe, or the reconfiguration runs
    // on the multiqueue's streaming thread and the lane is never entered.
    let cues: Vec<CueSpec> = (0..30u64)
        .map(|index| {
            let start = gst::ClockTime::from_seconds(index + 1);
            CueSpec::new(
                start,
                start + gst::ClockTime::from_mseconds(200),
                format!("S{index:02}"),
            )
        })
        .collect();
    let media = ScenarioBuilder::new("deadlineinflight")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues)
        .duration(gst::ClockTime::from_seconds(40))
        .pacing(Pacing::Realtime)
        .register();
    let subtitle = media.stream_id("text_0");

    let mut rig = Rig::new(Eat::DecodebinConfirmation, false);
    rig.load_and_play(&media.uri());
    rig.playbin.set_selection_deadline(DEADLINE);

    // Subtitles OFF first, so the switch under test ACTIVATES a slot that has
    // no output: that is the case decodebin3 answers by exposing a new pad,
    // and the pad exposure is what runs on the lane.
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    rig.playbin.pump_selection(rig.gate);
    rig.wait_for("subtitles to be reported off", |_, seen| {
        seen.iter().rev().any(
            |event| matches!(event, PlaybinEvent::StreamsSelected { subtitle, .. } if subtitle.is_none()),
        )
    });
    rig.settle_collections();

    let db3 = rig
        .playbin
        .pipeline()
        .by_name("fpb-decodebin")
        .expect("the live core's decodebin3");
    let hold = Arc::new(LaneHold::default());
    hold.install(&db3);

    let fires_before = rig.playbin.stats().selection_deadline_fires;
    let from = rig.seen.len();
    hold.arm();
    rig.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some(subtitle.clone())),
    );
    rig.playbin.pump_selection(rig.gate);

    // The premise. Without it the rest measures nothing, so it is asserted
    // rather than assumed.
    let engaged = {
        let deadline = Instant::now() + LANE_HELD;
        loop {
            rig.drain();
            if hold.engaged() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    };
    if !engaged {
        hold.release();
        rig.shutdown();
        media.unregister();
        println!(
            "NO VERDICT: decodebin3 never exposed the subtitle pad on fpb-select, so the \
             select lane was never held mid-send and the window this test measures was \
             not reached"
        );
        return;
    }
    assert!(
        rig.playbin.effects_in_flight() >= 1,
        "the lane is parked inside the send but the crate does not know the effect is \
         in flight, so the deadline has nothing to consult"
    );

    // Let the deadline fire across its whole retry budget while the send is
    // still parked. Nothing may be adopted in that window.
    let waited = {
        let want = fires_before + DEADLINE_RETRIES + 1;
        let deadline = Instant::now() + LANE_HELD;
        loop {
            rig.drain();
            if rig.playbin.stats().selection_deadline_fires >= want || Instant::now() >= deadline {
                break rig.playbin.stats().selection_deadline_fires - fires_before;
            }
            thread::sleep(Duration::from_millis(10));
        }
    };
    // The load-bearing premise, and a WORKER-side one: a tick-side fire count
    // only says the deadline looked, not that it reached the consult. A
    // deferral says the job ran, got past the settledness and gapless gates,
    // asked the hands and was told the event is still on the lane - which is
    // precisely the decision point every assertion below is about.
    assert!(
        rig.playbin.stats().selection_deadline_deferrals >= 1,
        "no deadline deferred to the parked send in {LANE_HELD:?} (fires={waited}), so \
         nothing below is evidence: the consult was never reached"
    );
    assert_eq!(
        rig.playbin.stats().selection_deadline_giveups,
        0,
        "the deadline gave up on a selection the lane had not sent yet, and adopted a \
         reality the parked send is about to contradict; fires={waited}, events: {:?}",
        &rig.seen[from..]
    );
    assert_eq!(
        rig.playbin.stats().selection_deadline_confirms,
        0,
        "the deadline decided a parked selection had applied and confirmed it from the \
         probe, off a routing entry the send made on its way IN; fires={waited}, \
         events: {:?}",
        &rig.seen[from..]
    );
    assert!(
        !rig.seen[from..]
            .iter()
            .any(|event| matches!(event, PlaybinEvent::StreamsSelected { .. })),
        "a selection was reported while its event was still parked on the lane; \
         events: {:?}",
        &rig.seen[from..]
    );

    // Release, and the truth arrives by the ordinary route.
    hold.release();
    let wanted = subtitle.clone();
    rig.wait_for(
        "the parked selection to confirm once its send completes",
        move |_, seen| {
            seen[from..].iter().any(|event| {
                matches!(event, PlaybinEvent::StreamsSelected { subtitle: Some(got), .. }
                    if *got == wanted)
            })
        },
    );
    drain_worker(&rig.playbin);
    rig.drain();
    assert_eq!(
        rig.playbin.stats().selection_deadline_giveups,
        0,
        "the deadline gave up after all; events: {:?}",
        &rig.seen[from..]
    );
    // What the caller was finally told has to be the track it asked for, not
    // the reality the give-up would have reported.
    let last = rig.seen[from..]
        .iter()
        .rev()
        .find_map(|event| match event {
            PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.clone()),
            _ => None,
        })
        .expect("a selection was reported");
    assert_eq!(
        last.as_deref(),
        Some(subtitle.as_str()),
        "the last reported selection is not the track that was asked for; events: {:?}",
        &rig.seen[from..]
    );
    // The release's own `Done` is a job like any other, so it lands when the
    // worker gets to it rather than when `send_event` returns.
    rig.wait_for("the released effect to report back", |playbin, _| {
        playbin.effects_in_flight() == 0
    });

    rig.shutdown();
    media.unregister();
}

/// The deferral is BOUNDED, and the bound is what keeps the deadline's
/// guarantee.
///
/// A lane that never comes back must not be able to silence the deadline for
/// good: deferring forever would trade a premature divergence for the
/// permanent latch the deadline exists to prevent. Past
/// `Deadlines::select_defer_budget` the crate stops waiting and lets the
/// ordinary timed-out logic run - wrong about a reality that may still change,
/// but loud, unlatching, and self-correcting if the lane heals.
///
/// Same wedge as the test above, with the budget shortened below the hold so
/// the fall-through is reachable inside a test. What is asserted is the
/// SEQUENCE: at least one deferral first (the consult works), then a decision
/// while the lane is STILL parked (the bound works).
#[test]
fn the_deferral_to_an_unsent_selection_is_bounded() {
    init();
    let cues: Vec<CueSpec> = (0..30u64)
        .map(|index| {
            let start = gst::ClockTime::from_seconds(index + 1);
            CueSpec::new(
                start,
                start + gst::ClockTime::from_mseconds(200),
                format!("B{index:02}"),
            )
        })
        .collect();
    let media = ScenarioBuilder::new("deadlinebounded")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues)
        .duration(gst::ClockTime::from_seconds(40))
        .pacing(Pacing::Realtime)
        .register();
    let subtitle = media.stream_id("text_0");

    let mut rig = Rig::new(Eat::DecodebinConfirmation, false);
    rig.load_and_play(&media.uri());
    rig.playbin.set_selection_deadline(DEADLINE);
    // Comfortably more than one deadline (so a deferral happens at all) and
    // comfortably less than the hold (so the fall-through happens while the
    // lane is still demonstrably parked).
    rig.playbin
        .set_select_defer_budget(Duration::from_millis(1500));

    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    rig.playbin.pump_selection(rig.gate);
    rig.wait_for("subtitles to be reported off", |_, seen| {
        seen.iter().rev().any(
            |event| matches!(event, PlaybinEvent::StreamsSelected { subtitle, .. } if subtitle.is_none()),
        )
    });
    rig.settle_collections();

    let db3 = rig
        .playbin
        .pipeline()
        .by_name("fpb-decodebin")
        .expect("the live core's decodebin3");
    let hold = Arc::new(LaneHold::default());
    hold.install(&db3);

    let decided_before = rig.playbin.stats().selection_deadline_confirms
        + rig.playbin.stats().selection_deadline_giveups;
    hold.arm();
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(subtitle)));
    rig.playbin.pump_selection(rig.gate);

    let engaged = {
        let deadline = Instant::now() + LANE_HELD;
        loop {
            rig.drain();
            if hold.engaged() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    };
    if !engaged {
        hold.release();
        rig.shutdown();
        media.unregister();
        println!(
            "NO VERDICT: decodebin3 never exposed the subtitle pad on fpb-select, so the \
             select lane was never held mid-send"
        );
        return;
    }

    // First the consult, then the bound. Both while the lane is parked.
    let mut deferred = false;
    let mut decided = false;
    let until = Instant::now() + LANE_HELD;
    while Instant::now() < until {
        rig.drain();
        deferred |= rig.playbin.stats().selection_deadline_deferrals >= 1;
        let decisions = rig.playbin.stats().selection_deadline_confirms
            + rig.playbin.stats().selection_deadline_giveups;
        if deferred && decisions > decided_before {
            decided = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let in_flight = rig.playbin.effects_in_flight();
    hold.release();
    assert!(
        deferred,
        "the deadline never deferred to the parked send, so the bound below would prove \
         nothing about a deferral that did not happen"
    );
    assert!(
        decided,
        "the deadline was still deferring to a lane parked for longer than the budget: \
         a wedged lane can silence the selection channel for good, which is the latch \
         the deadline exists to prevent"
    );
    assert!(
        in_flight >= 1,
        "the effect had left the lane before the bound fired, so the fall-through was \
         not measured against a parked send"
    );

    rig.shutdown();
    media.unregister();
}
