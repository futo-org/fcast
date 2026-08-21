//! CLEANUP.md invariant 2, the defer-and-drain route gate.
//!
//! `route_db3_pad` takes `Inner::route_gate` with try_lock and is
//! DELIBERATELY allowed to fail. It runs on a decodebin3 streaming thread
//! that holds decodebin3's SELECTION_LOCK, so blocking there while a downward
//! transition holds the gate deadlocks the transition itself (the descent
//! needs that same SELECTION_LOCK to take decodebin3 down). A refused pad
//! from the CURRENT core is pushed to `Inner::deferred_pads` and re-attempted
//! on every `RouteGate` release (`RouteGate::drop` runs
//! `drain_deferred_pads`), where the routing guards re-reject any that are
//! genuinely stale. The invariant is "every gate release drains". Two
//! simplifications reintroduce the load-stall wedge. Turning the try_lock
//! into a blocking lock wedges a downward transition against a live exposure
//! for good, and dropping the drain leaves refused pads dead instead of
//! re-attempted.
//!
//! # How the contention is manufactured
//!
//! One instance, a load racing a stop, with both sides held open so the
//! overlap is a wide engineered window instead of a microsecond race. The
//! item's VIDEO stream carries `Fault::StallAt` on its first buffer, so its
//! decodebin3 pad cannot expose until the test releases a named sync point.
//! The AUDIO sink stalls once inside Ready->Paused, which is the chain join
//! on `fpb-join`, and that join holds `join_gate` for the whole stall. A
//! `stop()` spawned meanwhile acquires `route_gate` inside `Inner::gate` and
//! parks on `join_gate` behind the stalled join. That park is the held-gate
//! window, and the video pad released into it try-locks a held gate and must
//! defer.
//!
//! The defer and its drain only speak through the crate's tracing, so `init`
//! installs a subscriber that captures it in process and the assertions read
//! it back. A build whose try_lock blocks hangs the stop and fails the
//! bounded join. A build whose drain is gone shows a defer with no
//! re-attempt.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{Fault, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;
use parking_lot::Mutex;

/// How long the audio chain join is held inside the sink's Ready->Paused.
/// This bounds the held-gate window, so it is generous against the release
/// choreography inside it (spawn the stop, let it park, release the video).
const HELD: Duration = Duration::from_millis(4500);

/// From spawning the stop to releasing the parked video buffer. The stop's
/// pre-gate steps are milliseconds, so by now it holds `route_gate` and is
/// parked on `join_gate`.
const STOP_ENTERS_GATE: Duration = Duration::from_millis(400);

/// Generous event bound, the whole crate's tests run concurrently.
const EVENT_BOUND: Duration = Duration::from_secs(40);

/// A pipeline call wedged past this is the deadlock this file exists to
/// catch. Healthy calls return in well under a second.
const CALL_BOUND: Duration = Duration::from_secs(30);

/// Engineered defer windows before the defer-never-fired assert trips.
const MAX_ROUNDS: usize = 4;

/// The sync point parking the video stream's first buffer.
const VIDEO_GATE: &str = "video-first-buffer";

/// The defer site's log line (`route_db3_pad`, the try_gate failure arm).
const DEFER_MARK: &str = "deferring active-core pad past a teardown";

/// Lines proving a deferred pad was re-attempted by a drain. A re-attempt
/// re-rejects the stale pad through the routing guards, fails, or re-defers
/// behind the next holder. pad-added fires once per pad, so a SECOND defer
/// line for the same pad can only come from a drain.
const DRAIN_MARKS: &[&str] = &[
    "ignoring stray pad from a superseded load",
    "ignoring pad from a superseded core",
    "failed to route deferred pad",
    DEFER_MARK,
];

static CAPTURED: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static TEE: AtomicBool = AtomicBool::new(false);

/// Sink for the tracing subscriber. Captures for the assertions, tees to
/// stderr when FCASTPLAYBIN_TEST_LOG is set.
struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURED.lock().extend_from_slice(buf);
        if TEE.load(Ordering::Relaxed) {
            let _ = std::io::stderr().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_len() -> usize {
    CAPTURED.lock().len()
}

fn log_text_from(start: usize) -> String {
    let log = CAPTURED.lock();
    String::from_utf8_lossy(&log[start.min(log.len())..]).into_owned()
}

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        TEE.store(
            std::env::var("FCASTPLAYBIN_TEST_LOG").is_ok(),
            Ordering::Relaxed,
        );
        let _ = tracing_subscriber::fmt()
            .with_env_filter("fcastplaybin=debug")
            .with_ansi(false)
            .with_writer(|| CaptureWriter)
            .try_init();
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// The `pad=NAME` token on the line around byte offset `at`.
fn pad_token(text: &str, at: usize) -> Option<String> {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    let line = &text[line_start..line_end];
    let field = line.find("pad=")?;
    let token = line[field + 4..]
        .split_whitespace()
        .next()?
        .trim_matches('"');
    (!token.is_empty()).then(|| token.to_owned())
}

/// Whether `text` shows a drain re-attempt of the pad deferred at byte
/// offset `defer_at`, on a line after that defer line.
fn drained_after(text: &str, defer_at: usize) -> bool {
    let line_end = text[defer_at..]
        .find('\n')
        .map_or(text.len(), |i| defer_at + i);
    let pad = pad_token(text, defer_at);
    let tail = &text[line_end..];
    DRAIN_MARKS.iter().any(|mark| {
        tail.match_indices(mark).any(|(at, _)| match &pad {
            Some(pad) => pad_token(tail, at).as_deref() == Some(pad),
            None => true,
        })
    })
}

/// A/V item, nothing held. The recovery and churn item.
fn plain_media(key: &str) -> ScenarioHandle {
    ScenarioBuilder::new(key)
        .stream(StreamSpec::new(
            "video_0",
            StreamKind::Video {
                width: 16,
                height: 16,
                fps: gst::Fraction::new(10, 1),
                keyframe_interval: 1,
            },
        ))
        .stream(StreamSpec::audio("audio_0"))
        .duration(gst::ClockTime::from_seconds(40))
        .pacing(Pacing::Jitter {
            base_ms: 2,
            jitter_ms: 0,
        })
        .register()
}

/// Audio flows normally, the video stream's FIRST buffer is parked on
/// [`VIDEO_GATE`] until the test releases it. Releasing it into a held route
/// gate is what makes the video pad's route defer.
fn held_video_media(key: &str) -> ScenarioHandle {
    ScenarioBuilder::new(key)
        .stream(
            StreamSpec::new(
                "video_0",
                StreamKind::Video {
                    width: 16,
                    height: 16,
                    fps: gst::Fraction::new(10, 1),
                    keyframe_interval: 1,
                },
            )
            .with_fault(Fault::StallAt {
                buffer_index: 0,
                sync_point: VIDEO_GATE.to_owned(),
            })
            .with_pacing(Pacing::Jitter {
                base_ms: 2,
                jitter_ms: 0,
            }),
        )
        .stream(StreamSpec::audio("audio_0").with_pacing(Pacing::Jitter {
            base_ms: 2,
            jitter_ms: 0,
        }))
        .duration(gst::ClockTime::from_seconds(90))
        .register()
}

/// Transition and hold for the next audio sink the factory builds.
type StallPlan = Option<(&'static str, u64)>;

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<(PlaybinEvent, u64)>>,
    /// Consumed by the audio factory at the next build.
    stall_next_audio: Arc<Mutex<StallPlan>>,
    /// Every audio sink built, in build order. Per load by construction.
    audio_sinks: Arc<Mutex<Vec<FTestSink>>>,
}

impl Harness {
    fn new() -> Self {
        let stall_next_audio: Arc<Mutex<StallPlan>> = Arc::new(Mutex::new(None));
        let audio_sinks: Arc<Mutex<Vec<FTestSink>>> = Arc::new(Mutex::new(Vec::new()));
        let stall = stall_next_audio.clone();
        let sinks = audio_sinks.clone();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(move || {
                let sink = FTestSink::new();
                if let Some((transition, ms)) = stall.lock().take() {
                    sink.set_property("stall-transition", transition);
                    sink.set_property("stall-ms", ms);
                }
                sinks.lock().push(sink.clone());
                Ok(sink.upcast())
            })),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin,
            events,
            log: RefCell::new(Vec::new()),
            stall_next_audio,
            audio_sinks,
        }
    }

    /// The receiver's settle-point calls plus the event drain. Nothing here
    /// may block.
    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
        while let Ok(entry) = self.events.try_recv() {
            self.log.borrow_mut().push(entry);
        }
    }

    /// Pump until `done`, else panic with the event log after `bound`.
    fn pump_until(&self, what: &str, bound: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + bound;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} within {bound:?}; events: {:#?}",
                self.log.borrow()
            );
            self.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn load(&self, uri: &str) -> u64 {
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        )
    }

    /// Wait for `generation`'s Loaded. An Error carrying the same generation
    /// is fatal, errors from raced or superseded loads are expected debris.
    fn wait_loaded(&self, what: &str, generation: u64) {
        let mut seen = 0;
        self.pump_until(what, EVENT_BOUND, || {
            let log = self.log.borrow();
            for (event, generation_of) in log.iter().skip(seen) {
                if *generation_of != generation {
                    continue;
                }
                if let PlaybinEvent::Error { error, .. } = event {
                    panic!("pipeline error while waiting for {what}: {error}");
                }
                if matches!(event, PlaybinEvent::Loaded { .. }) {
                    return true;
                }
            }
            seen = log.len();
            false
        });
    }

    fn wait_settled_playing(&self, what: &str, generation: u64) {
        let mut seen = 0;
        self.pump_until(what, EVENT_BOUND, || {
            let log = self.log.borrow();
            for (event, generation_of) in log.iter().skip(seen) {
                if *generation_of != generation {
                    continue;
                }
                if let PlaybinEvent::Error { error, .. } = event {
                    panic!("pipeline error while waiting for {what}: {error}");
                }
                if matches!(
                    event,
                    PlaybinEvent::StateChanged {
                        current: gst::State::Playing,
                        pending: gst::State::VoidPending,
                        ..
                    }
                ) {
                    return true;
                }
            }
            seen = log.len();
            false
        });
    }

    /// Load, wait for its Loaded, play to a settled PLAYING with the
    /// position moving. The end-to-end proof that routing still works.
    fn load_plays(&self, uri: &str, what: &str) {
        let generation = self.load(uri);
        self.wait_loaded(what, generation);
        self.playbin.play().expect("play");
        self.wait_settled_playing(what, generation);
        let target = gst::ClockTime::from_mseconds(200);
        self.pump_until(&format!("{what} to advance"), EVENT_BOUND, || {
            self.playbin
                .position()
                .is_some_and(|position| position >= target)
        });
    }

    /// The worker answers a queued job. A wedged worker answers nothing.
    fn assert_worker_alive(&self, what: &str) {
        let (tx, rx) = mpsc::channel();
        self.playbin.barrier_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + CALL_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "the worker never answered ({what})"
                    );
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the worker died ({what})")
                }
            }
        }
    }

    /// Blocks until the worker tore the pipeline down, so a finished test's
    /// realtime media stops competing with the rest of the suite.
    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + CALL_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "shutdown never finished");
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the worker died during shutdown")
                }
            }
        }
    }
}

/// A pipeline call on its own thread, joined with a bound, so a wedged call
/// fails the test instead of hanging the harness. The thread is leaked on
/// timeout, the process is on its way out anyway.
struct BoundedCall<T> {
    name: &'static str,
    rx: mpsc::Receiver<T>,
}

fn spawn_call<T: Send + 'static>(
    name: &'static str,
    call: impl FnOnce() -> T + Send + 'static,
) -> BoundedCall<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(call());
        })
        .expect("spawning a bounded call");
    BoundedCall { name, rx }
}

impl<T> BoundedCall<T> {
    /// Wait for the call while keeping the harness pumped.
    fn join_pumping(self, bound: Duration, harness: &Harness) -> T {
        let deadline = Instant::now() + bound;
        loop {
            match self.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(value) => return value,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "{} did not return within {bound:?}",
                        self.name
                    );
                    harness.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{} panicked before answering", self.name)
                }
            }
        }
    }
}

/// SUBJECT, invariant 2 both ways. A stop parked inside `Inner::gate` behind
/// a stalled chain join holds `route_gate` while the racing load's video pad
/// exposes, so that pad's route must DEFER (proven from the crate's own
/// tracing, in an engineered window rather than a sampled race), every gate
/// release must DRAIN it (the re-attempt is re-rejected through the routing
/// guards, the load it belonged to was stopped), and nothing may wedge. A
/// blocking try_lock turns the bounded stop join red. A dropped drain turns
/// the re-attempt wait red.
#[test]
fn a_pad_deferred_by_a_held_route_gate_still_routes() {
    init();
    let harness = Harness::new();
    let mut media = Vec::new();
    let mut defer_proven = false;

    for round in 0..MAX_ROUNDS {
        let start = log_len();
        // The audio chain join stalls inside the sink's Ready->Paused on
        // fpb-join, holding join_gate for HELD.
        *harness.stall_next_audio.lock() = Some(("ReadyToPaused", HELD.as_millis() as u64));
        let item = held_video_media(&format!("rgd_defer_{round}"));
        let sinks_before = harness.audio_sinks.lock().len();
        harness.load(&item.uri());

        // Audio routes on its own (the video data is parked), so its join
        // must engage the stall.
        harness.pump_until("the audio join stall to engage", EVENT_BOUND, || {
            harness
                .audio_sinks
                .lock()
                .get(sinks_before)
                .is_some_and(|sink| sink.property::<Option<String>>("stalled-thread").is_some())
        });

        // The stop's pre-gate steps run, then Inner::gate acquires
        // route_gate and parks on join_gate behind the stalled join. That
        // park is the held-gate window.
        let stopper = harness.playbin.clone();
        let stop = spawn_call("stop-under-contention", move || stopper.stop());
        std::thread::sleep(STOP_ENTERS_GATE);

        // Release the video stream's first buffer INTO the window. Its pad
        // exposes on a streaming thread and the route must defer.
        item.release(VIDEO_GATE);
        let defer_deadline = Instant::now() + HELD;
        let mut deferred_at = None;
        while Instant::now() < defer_deadline {
            if let Some(at) = log_text_from(start).find(DEFER_MARK) {
                deferred_at = Some(at);
                break;
            }
            harness.pump();
            std::thread::sleep(Duration::from_millis(10));
        }

        // The blocking-lock detector. A route that blocks on the gate parks
        // decodebin3's SELECTION_LOCK under it and this descent never ends.
        stop.join_pumping(HELD + CALL_BOUND, &harness)
            .expect("stop under contention");

        // Late chance, the defer can land while the stop join was waited on.
        if deferred_at.is_none() {
            deferred_at = log_text_from(start).find(DEFER_MARK);
        }

        match deferred_at {
            Some(at) => {
                // Every gate release drains. The deferred pad must have been
                // re-attempted after its defer line.
                harness.pump_until("the deferred pad's drain re-attempt", EVENT_BOUND, || {
                    drained_after(&log_text_from(start), at)
                });
                defer_proven = true;
                println!("round {round} deferred a pad and drained it");
            }
            None => println!("round {round} saw no defer, retrying with a fresh window"),
        }

        // Either way the machine must come back. The next load's streams
        // route and play.
        let next = plain_media(&format!("rgd_defer_next_{round}"));
        harness.load_plays(&next.uri(), "the load after the contended stop");
        media.push(item);
        media.push(next);
        if defer_proven {
            break;
        }
    }

    assert!(
        defer_proven,
        "no route deferred across {MAX_ROUNDS} engineered windows, the defer path was never exercised"
    );
    harness.assert_worker_alive("after the defer rounds");
    harness.shutdown();
}

/// `set_pipeline_state`'s downward arm takes the full gate on the CALLER's
/// thread, racing worker loads and streaming-thread routes by design. Five
/// differently phased Null races. Each must return bounded, and a fresh load
/// afterwards must work. Even rounds anchor on Loaded (pads can still be
/// exposing), odd rounds race the whole load from its enqueue.
#[test]
fn set_pipeline_state_null_concurrent_with_routing_is_bounded() {
    init();
    let harness = Harness::new();
    let mut media = Vec::new();
    let phases_ms = [0u64, 15, 40, 90, 180];

    for (round, phase_ms) in phases_ms.into_iter().enumerate() {
        let item = plain_media(&format!("rgd_null_{round}"));
        let generation = harness.load(&item.uri());
        if round % 2 == 0 {
            harness.wait_loaded("the raced load", generation);
        }
        std::thread::sleep(Duration::from_millis(phase_ms));

        let setter = harness.playbin.clone();
        spawn_call("null-under-routing", move || {
            setter.set_pipeline_state(gst::State::Null)
        })
        .join_pumping(CALL_BOUND, &harness)
        .expect("a Null during routing");

        let next = plain_media(&format!("rgd_null_next_{round}"));
        if round + 1 == phases_ms.len() {
            harness.load_plays(&next.uri(), "the load after the last Null race");
        } else {
            let generation = harness.load(&next.uri());
            harness.wait_loaded("the load after a Null race", generation);
        }
        media.push(item);
        media.push(next);
    }

    harness.assert_worker_alive("after the Null races");
    harness.shutdown();
}

/// The wedge-class soak. Rapid load, brief life, stop, next load. Every
/// cycle must reach its Loaded, every stop must return bounded, and the
/// worker must still answer at the end. This is the churn that wedges when
/// the defer-and-drain machinery regresses in any direction.
#[test]
fn rapid_load_stop_load_churn_never_wedges() {
    init();
    let harness = Harness::new();
    let mut media = Vec::new();
    // Deterministic per-cycle dwell between Loaded and stop, shaped to land
    // stops from mid-exposure to settled.
    let dwell_ms = [0u64, 6, 17, 3, 42, 11, 28, 80];

    for (cycle, dwell) in dwell_ms.into_iter().enumerate() {
        let item = plain_media(&format!("rgd_churn_{cycle}"));
        let generation = harness.load(&item.uri());
        harness.wait_loaded(&format!("cycle {cycle} Loaded"), generation);
        if cycle % 2 == 0 {
            harness.playbin.play().expect("play");
        }
        std::thread::sleep(Duration::from_millis(dwell));

        let stopper = harness.playbin.clone();
        spawn_call("churn-stop", move || stopper.stop())
            .join_pumping(CALL_BOUND, &harness)
            .expect("stop under churn");
        media.push(item);
    }

    let last = plain_media("rgd_churn_final");
    harness.load_plays(&last.uri(), "the load after the churn");
    media.push(last);
    harness.assert_worker_alive("after the churn");
    // Reported, not asserted. Churn defers are timing dependent, the
    // deterministic proof lives in the defer test above.
    let defers = log_text_from(0).matches(DEFER_MARK).count();
    println!("the churn deferred {defers} pad(s)");
    harness.shutdown();
}
