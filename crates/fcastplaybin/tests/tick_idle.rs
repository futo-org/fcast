//! At rest the tick queues nothing, and it starts again when an item is live.
//!
//! The periodic text pokes are the crate's only unsolicited work, and they
//! are only justified for a live item (postponed work, or a delivery
//! divergence with no edge coming to re-ask about it). After a `stop()` they
//! must cease, or an idle process pays a no-op job, a log line and a worker
//! wakeup every second forever.
//!
//! This is its own binary because `regression_text_reconcile.rs` asserts on
//! process-global counters that only hold when nothing else in the process is
//! playing, and this test has to sit still for seconds at a time. A separate
//! binary is a separate process, so the globals cannot be shared.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{scenario::ScenarioBuilder, sink::FTestSink, spec::Pacing};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the stop's own in-flight work is given before the idle window
/// opens. Deliberately a fixed wait rather than a quiescence detector. A
/// periodic poke satisfies any short "nothing moved" window by construction,
/// so a detector would call the bug settled.
const STOP_SETTLE: Duration = Duration::from_secs(2);

/// The idle window, three full tick seconds plus change. The bug puts three
/// jobs in here, the fix puts none.
const IDLE_OBSERVE: Duration = Duration::from_millis(3500);

/// How long a live crate may take to produce a tick-driven drain with nobody
/// polling. Several tick periods of slack.
const LIVE_POKE_BOUND: Duration = Duration::from_secs(4);

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

struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    /// Latched by the ONE reader of the channel, so a `wait_for` predicate
    /// never races the pump for the event it is waiting on.
    loaded: std::cell::Cell<bool>,
    /// Kept so the event sender stays alive for the handle's lifetime.
    _keep: Arc<Mutex<()>>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        Self {
            playbin: Arc::new(playbin),
            events,
            loaded: std::cell::Cell::new(false),
            _keep: Arc::new(Mutex::new(())),
        }
    }

    /// THE only reader of the event channel.
    fn drain_events(&self) {
        while let Ok(event) = self.events.try_recv() {
            match &event {
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                PlaybinEvent::Loaded { .. } => self.loaded.set(true),
                _ => {}
            }
        }
    }

    /// Drain events and poke everything a caller normally pokes. Only for
    /// use outside a measurement window.
    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
        self.drain_events();
    }

    /// Drain events and nothing else. A `poll_text_policy` inside a
    /// measurement window would be the test poking the machinery it is
    /// watching, since a poll is a job of its own and can queue a drain.
    fn pump_events_only(&self) {
        self.drain_events();
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; pipeline {:?}",
                self.playbin.state_summary()
            );
            self.pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Watch the channel without asking the crate for anything.
    fn quiet_for(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        while Instant::now() < until {
            self.pump_events_only();
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// The liveness half keeps the gate honest. An "idle" predicate that
/// answered `false` too eagerly would pass the first assertion and silently
/// retire the divergence trigger the reconcile pass depends on.
#[test]
fn the_tick_queues_nothing_once_the_crate_is_idle() {
    init();
    let media = ScenarioBuilder::new("tickidle")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    let load_and_play = |harness: &Harness| {
        harness.loaded.set(false);
        harness.playbin.load_async(
            MediaInput::Uri(media.uri()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        harness.wait_for("the load to report Loaded", || harness.loaded.get());
        harness.playbin.play().expect("play");
        harness.wait_for("the pipeline to settle at PLAYING", || {
            harness.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
                && !harness.playbin.has_async_transition()
        });
    };

    load_and_play(&harness);
    harness.playbin.stop().expect("stop");
    harness.quiet_for(STOP_SETTLE);

    // The measurement. Nothing is loaded, routed, or desired, so the bound
    // is zero new jobs, not "few".
    let drains_before = harness.playbin.stats().drain_text_job_count;
    let polls_before = harness.playbin.stats().poll_policy_job_count;
    harness.quiet_for(IDLE_OBSERVE);
    let drains = harness.playbin.stats().drain_text_job_count - drains_before;
    let polls = harness.playbin.stats().poll_policy_job_count - polls_before;
    assert_eq!(
        drains, 0,
        "{drains} drain job(s) reached the worker over {IDLE_OBSERVE:?} with nothing loaded \
         and nobody polling. The tick's periodic pokes are ungated again: a stopped process \
         pays a no-op job, a log line and a wakeup every second forever"
    );
    assert_eq!(
        polls, 0,
        "{polls} text-policy job(s) reached the worker over {IDLE_OBSERVE:?} at rest; \
         something is polling an idle crate on a timer"
    );

    // And it comes back. A gate that simply switched the trigger off would
    // pass the assertions above and take the divergence liveness with it.
    load_and_play(&harness);
    // The `wait_for` above polls. Let its last drain land before sampling,
    // or the residue of the load would answer for the tick.
    harness.quiet_for(Duration::from_millis(700));
    let before = harness.playbin.stats().drain_text_job_count;
    let deadline = Instant::now() + LIVE_POKE_BOUND;
    while harness.playbin.stats().drain_text_job_count == before {
        assert!(
            Instant::now() < deadline,
            "no drain job reached the worker in {LIVE_POKE_BOUND:?} at a settled PLAYING with \
             an item loaded. The tick's 1 Hz trigger is gated off while the crate is LIVE, so \
             a delivery divergence with no edge coming for it is never re-asked"
        );
        harness.pump_events_only();
        thread::sleep(Duration::from_millis(20));
    }

    media.release_all();
    harness.shutdown();
    media.unregister();
}
