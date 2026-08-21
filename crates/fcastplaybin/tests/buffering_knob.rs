//! End to end demonstration of fcasttest's buffering knob driving a
//! receiver-style consumer.
//!
//! The knob lets a scenario declare buffering behaviour, so a test can park
//! the receiver's state machine in Buffering on demand, hold it there
//! deterministically, observe what it refuses while parked, and prove the
//! state can be left.
//!
//! The consumer here is the exact machinery the receiver runs, not a mock. It
//! feeds `PlaybinEvent::Buffering` and pipeline state edges into
//! `fcastplaybin::StateMachine` the way receiver-core's player does, so the
//! asserted `Buffering` is the very state the receiver refuses
//! `ResumeOrPause` in.

use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, BufferingStateResult, FcastPlaybin, MediaInput, PlaybinEvent, RunningState,
    SelectionGate, Sinks, StartPoint, StateChangeResult, StateMachine,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::{FTestSink, Recording},
    spec::{BufferingDip, BufferingRecovery, BufferingSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The dip's anchor, far enough in that the load and the climb to a settled
/// PLAYING finish before the low percent can arrive.
const DIP_AT_VIDEO_BUFFER: u64 = 75;

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

/// The receiver's view of the player, derived exactly like receiver-core's
/// `player_state()`. `Buffering` is the state `Operation::ResumeOrPause`
/// refuses with `InvalidState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverState {
    Stopped,
    Paused,
    Playing,
    Buffering,
}

/// The receiver-shaped consumer. Every event is handled the way
/// receiver-core's player handles it, and every state the machine dispatches
/// is applied to the pipeline.
struct Receiver {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    sm: StateMachine,
    /// Every buffering percent the consumer saw, in order.
    saw_buffering: Vec<i32>,
}

impl Receiver {
    fn build() -> (Self, Recording) {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(video_sink.upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        (
            Self {
                playbin,
                events,
                sm: StateMachine::new(),
                saw_buffering: Vec::new(),
            },
            video,
        )
    }

    fn state(&self) -> ReceiverState {
        if self.sm.is_stopped() {
            return ReceiverState::Stopped;
        }
        match self.sm.running() {
            Some(RunningState::Paused) => ReceiverState::Paused,
            Some(RunningState::Playing) => ReceiverState::Playing,
            None => ReceiverState::Buffering,
        }
    }

    fn load(&mut self, uri: String) {
        self.sm.begin_load();
        self.playbin.load_async(
            MediaInput::Uri(uri),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
    }

    fn apply(&self, state: gst::State) {
        match state {
            gst::State::Playing => self.playbin.play().expect("play"),
            gst::State::Paused => self.playbin.pause().expect("pause"),
            other => panic!("the machine never dispatches {other:?} in this test"),
        }
    }

    fn handle(&mut self, event: PlaybinEvent) {
        match event {
            PlaybinEvent::Loaded { .. } => {
                // uri_loaded's transport commit, with Playing as the desired
                // transport. The fallback drive mirrors the receiver's too.
                if let Some(state) = self.sm.set_playback_state(RunningState::Playing) {
                    self.apply(state);
                } else if self.sm.running() != Some(RunningState::Playing) {
                    self.playbin.play().expect("play");
                }
            }
            PlaybinEvent::Buffering(percent) => {
                self.saw_buffering.push(percent);
                match self.sm.buffering(percent) {
                    BufferingStateResult::Started => self.apply(gst::State::Paused),
                    BufferingStateResult::Finished(Some(state)) => self.apply(state),
                    BufferingStateResult::Finished(None) | BufferingStateResult::Buffering => {}
                    BufferingStateResult::FinishedWithSeek(seek) => {
                        panic!("no seek was ever issued, the machine invented {seek:?}")
                    }
                }
            }
            PlaybinEvent::StateChanged {
                old,
                current,
                pending,
            } => match self.sm.state_changed(old, current, pending) {
                StateChangeResult::ChangeState(state) => self.apply(state),
                StateChangeResult::NewPlaybackState(_) | StateChangeResult::Waiting => {}
                StateChangeResult::Seek(seek) => {
                    panic!("no seek was ever parked, the machine dispatched {seek:?}")
                }
            },
            PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
            _ => {}
        }
    }

    fn pump(&mut self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            self.handle(event);
        }
    }

    fn wait_until(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} (receiver state {:?}, buffering posts {:?})",
                self.state(),
                self.saw_buffering
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(self) {
        let (done_tx, done_rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = done_tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match done_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.playbin.poll_text_policy();
                    self.playbin.pump_selection(SelectionGate {
                        quiet: true,
                        paused: false,
                        seekable: false,
                    });
                    while self.events.try_recv().is_ok() {}
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

fn media(key: &str, buffering: Option<BufferingSpec>) -> ScenarioHandle {
    let mut builder = ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(40))
        .pacing(Pacing::Realtime);
    if let Some(buffering) = buffering {
        builder = builder.buffering(buffering);
    }
    builder.register()
}

/// The whole point of the knob in one run. The receiver-style consumer plays
/// ftest media, is parked in Buffering by a dip the scenario anchored to a
/// video buffer, refuses transport work while parked exactly like the field
/// receiver, and comes back to PLAYING when the test releases the dip.
#[test]
fn a_scenario_dip_parks_the_receiver_in_buffering_and_the_release_frees_it() {
    init();
    let media = media(
        "bufknobdip",
        Some(BufferingSpec::new(10).with_dip(BufferingDip {
            stream: "video_0".to_owned(),
            buffer_index: DIP_AT_VIDEO_BUFFER,
            recovery: BufferingRecovery::OnSyncPoint("refill".to_owned()),
        })),
    );
    let (mut receiver, video) = Receiver::build();

    receiver.load(media.uri());
    receiver.wait_until("the receiver to settle Playing", |receiver| {
        receiver.state() == ReceiverState::Playing
    });
    assert!(
        receiver.saw_buffering.is_empty(),
        "the dip is anchored three seconds in and must not fire during the load"
    );

    // The dip. The low percent reaches the consumer, the machine dispatches
    // the buffering PAUSE, and the receiver-visible state becomes Buffering.
    receiver.wait_until("the receiver to enter Buffering", |receiver| {
        receiver.state() == ReceiverState::Buffering
    });
    assert_eq!(receiver.saw_buffering, vec![10]);

    // In this state the receiver refuses ResumeOrPause, and a transport
    // request into the machine is a retarget that dispatches nothing.
    assert_eq!(
        receiver.sm.set_playback_state(RunningState::Playing),
        None,
        "a transport request while buffering must be a bare retarget"
    );
    assert_eq!(receiver.state(), ReceiverState::Buffering);

    // The consumer stays parked exactly as long as the gate is held.
    let parked_at = Instant::now();
    while parked_at.elapsed() < Duration::from_millis(400) {
        receiver.pump();
        assert_eq!(
            receiver.state(),
            ReceiverState::Buffering,
            "left Buffering without any 100 having been posted"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // The release. The recovery posts 100, completion redispatches the
    // remembered PLAYING, and the pipeline must actually run again.
    let frames_before_release = video.buffer_count();
    media.release("refill");
    receiver.wait_until("the receiver to leave Buffering", |receiver| {
        receiver.state() == ReceiverState::Playing
    });
    assert_eq!(receiver.saw_buffering, vec![10, 100]);
    receiver.wait_until("video to flow again after the recovery", |_| {
        video.buffer_count() > frames_before_release + 3
    });

    receiver.shutdown();
    media.unregister();
}

/// The control. Identical media without the knob never posts buffering, so
/// the Buffering entry above is the knob's doing and nothing else's.
#[test]
fn the_same_media_without_the_knob_never_buffers() {
    init();
    let media = media("bufknobnone", None);
    let (mut receiver, video) = Receiver::build();

    receiver.load(media.uri());
    receiver.wait_until("the receiver to settle Playing", |receiver| {
        receiver.state() == ReceiverState::Playing
    });
    // Play well past the point where the knobbed twin dips, so this is a
    // comparison of the same playback window and not a shorter one.
    receiver.wait_until("playback to pass the twin's dip point", |_| {
        video.buffer_count() as u64 > DIP_AT_VIDEO_BUFFER + 25
    });
    assert!(
        receiver.saw_buffering.is_empty(),
        "media without the knob drove the buffering machine: {:?}",
        receiver.saw_buffering
    );
    assert_eq!(receiver.state(), ReceiverState::Playing);

    receiver.shutdown();
    media.unregister();
}
