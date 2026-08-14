//! Regression for the state machine waiting on a `state-changed` GStreamer
//! refuses to post. `FCAST_NO_SYNTHETIC_STATE_EDGE=1` restores the old
//! behaviour.
//!
//! Buffering is open when the load reports `Loaded`, so `player.rs`
//! `uri_loaded` finds `set_playback_state` unable to act and drives the
//! pipeline to PLAYING itself. The machine consumes every edge while
//! buffering, so `buffering(100)` redispatches PLAYING into a pipeline
//! already at PLAYING. GStreamer posts nothing for a same-state change, so
//! the machine waits forever and `running()` returns None for good. No
//! transport, no progress, no subtitle.
//!
//! The harness mirrors only the three `player.rs` entry points involved, so a
//! failure here is the receiver's real sequence and not an artifact.

use std::{
    cell::{Cell, RefCell},
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, BufferingStateResult, FcastPlaybin, MediaInput, PlaybinEvent, RunningState, Sinks,
    StartPoint, StateChangeResult, StateMachine,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{BufferingSpec, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

const BOUND: Duration = Duration::from_secs(15);
/// The healthy path takes one state round trip.
const SETTLE_BOUND: Duration = Duration::from_secs(8);
/// Must outlast the load's climb to PLAYING, since the case needs the pipeline
/// to arrive there while the machine is still buffering. A short window lets
/// the refill win the race, and a gated refill cannot work because only
/// anchored dips can be gated and those fire after `Loaded`, too late. Hence
/// a generous window plus the post-hoc trace check.
const INITIAL_BUFFER_MS: u64 = 3000;

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

fn media(key: &str) -> ScenarioHandle {
    let video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(25, 1),
            keyframe_interval: 1,
        },
    );
    ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"))
        .duration(gst::ClockTime::from_seconds(20))
        .pacing(Pacing::Realtime)
        // Opens with the source, before any media flows, so `Loaded` lands
        // while it is still open. That is the DASH-resume shape.
        .buffering(BufferingSpec::new(20).with_initial_ms(INITIAL_BUFFER_MS))
        .register()
}

/// The receiver, reduced to the three entry points this bug runs through.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    sm: RefCell<StateMachine>,
    desired: Cell<RunningState>,
    loaded: Cell<bool>,
    /// Set once a low buffering post reached the machine (`Phase::Buffering`).
    buffering_open: Cell<bool>,
    error: RefCell<Option<String>>,
    /// Every percent and edge the machine saw, for the failure message.
    log: RefCell<Vec<String>>,
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
            playbin,
            events,
            sm: RefCell::new(StateMachine::new()),
            desired: Cell::new(RunningState::Playing),
            loaded: Cell::new(false),
            buffering_open: Cell::new(false),
            error: RefCell::new(None),
            log: RefCell::new(Vec::new()),
        }
    }

    /// `player.rs` dispatch: always to the worker, never a blocking set_state.
    fn apply(&self, state: gst::State) {
        self.log.borrow_mut().push(format!("apply({state:?})"));
        self.playbin.set_state_async(state);
    }

    /// `player.rs` `uri_loaded`.
    fn on_loaded(&self) {
        let desired = self.desired.get();
        let dispatched = self.sm.borrow_mut().set_playback_state(desired);
        if let Some(state) = dispatched {
            self.apply(state);
        } else if self.sm.borrow().running() != Some(desired) {
            self.apply(desired.into());
        }
    }

    /// `player.rs` `buffering`.
    fn on_buffering(&self, percent: i32) {
        if percent < 100 {
            self.buffering_open.set(true);
        }
        let result = self.sm.borrow_mut().buffering(percent);
        self.log
            .borrow_mut()
            .push(format!("buffering({percent}) -> {result:?}"));
        match result {
            BufferingStateResult::Started(state) => self.apply(state),
            BufferingStateResult::Buffering => {}
            BufferingStateResult::FinishedWithSeek(seek) => self.playbin.seek_async(seek),
            BufferingStateResult::FinishedButWaitingSeek => {}
            BufferingStateResult::Finished(state) => {
                if let Some(state) = state {
                    self.apply(state);
                }
            }
        }
    }

    /// `player.rs` `state_changed`.
    fn on_state_changed(&self, old: gst::State, current: gst::State, pending: gst::State) {
        let result = self.sm.borrow_mut().state_changed(old, current, pending);
        self.log
            .borrow_mut()
            .push(format!("edge({current:?},{pending:?}) -> {result:?}"));
        match result {
            StateChangeResult::NewPlaybackState(_) | StateChangeResult::Waiting => {}
            StateChangeResult::Seek(seek) => self.playbin.seek_async(seek),
            StateChangeResult::ChangeState(state) => self.apply(state),
        }
    }

    fn pump(&self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlaybinEvent::Buffering(percent) => self.on_buffering(percent),
                PlaybinEvent::StateChanged {
                    old,
                    current,
                    pending,
                } => self.on_state_changed(old, current, pending),
                // Not dispatched here. `uri_loaded` and the first buffering
                // post race in the crate, and the case needs buffering open
                // first, so the test body orders it.
                PlaybinEvent::Loaded { .. } => self.loaded.set(true),
                PlaybinEvent::Error { error, .. } => {
                    *self.error.borrow_mut() = Some(error.to_string())
                }
                _ => {}
            }
        }
    }

    fn wait_until(&self, what: &str, bound: Duration, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + bound;
        loop {
            self.pump();
            if let Some(error) = self.error.borrow().clone() {
                panic!("the pipeline posted an error while waiting for {what}: {error}");
            }
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\n  pipeline {:?}, unsettled {:?}\n  \
                 machine running {:?}\n  what the machine saw:\n    {}",
                self.playbin.state_summary(),
                self.playbin.unsettled_elements(),
                self.sm.borrow().running(),
                self.log.borrow().join("\n    "),
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
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

/// A load that reports `Loaded` while buffering is still open must still come
/// back to Running once buffering completes, even though the transport the
/// receiver drove mid-buffer already put the pipeline in the target state.
#[test]
fn buffering_that_completes_at_the_target_state_still_reaches_running() {
    init();
    let media = media("bufnoop1");
    let harness = Harness::new();

    harness.desired.set(RunningState::Playing);
    harness.sm.borrow_mut().begin_load();
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );

    // The field order, enforced rather than raced. Buffering opens before
    // `uri_loaded`, which sends it down the drive-directly branch.
    harness.wait_until("buffering to open", BOUND, |h| h.buffering_open.get());
    harness.wait_until("the load to report Loaded", BOUND, |h| h.loaded.get());
    assert_eq!(
        harness.sm.borrow().running(),
        None,
        "buffering must still be open when uri_loaded runs; what the machine \
         saw:\n    {}",
        harness.log.borrow().join("\n    ")
    );
    harness.on_loaded();
    harness.wait_until("the pipeline to reach PLAYING mid-buffer", BOUND, |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });

    // The refill redispatches PLAYING into a pipeline already at PLAYING.
    // Nothing must be left waiting on a message GStreamer will not send.
    harness.wait_until("the machine to come back to Running", SETTLE_BOUND, |h| {
        h.sm.borrow().running().is_some()
    });
    assert_eq!(
        harness.sm.borrow().running(),
        Some(RunningState::Playing),
        "the machine came back to the wrong transport; what it saw:\n    {}",
        harness.log.borrow().join("\n    ")
    );

    // Guard against passing for the wrong reason. The pipeline must really
    // have been at the target state when the refill redispatched. Checked on
    // the trace, not by polling, since a poll can miss an intermediate state
    // that genuinely occurred.
    let log = harness.log.borrow().join("\n");
    let refill = log
        .find("buffering(100)")
        .expect("the refill must have been delivered");
    assert!(
        log[..refill].contains("edge(Playing, VoidPending)")
            || log[..refill].contains("edge(Playing,VoidPending)"),
        "the pipeline never reached PLAYING before the refill, so the no-op \
         redispatch under test never happened and this run proved nothing; \
         what the machine saw:\n    {}",
        harness.log.borrow().join("\n    ")
    );

    media.release_all();
    harness.shutdown();
    media.unregister();
}
