//! The two passing CONTROLS for `regression_video_resurrect.rs`, in their
//! own binary because the red test over there ends in `std::process::exit`
//! (unwinding through the wedge it reports would hang the suite), and a
//! process exit would take sibling tests with it.
//!
//! Control 1 proves the attach alone is harmless while video stays
//! selected. Control 2 proves the receiver-shaped pump gate is load-bearing
//! in the failure, the identical wedge sequence pumped with `quiet: true`
//! unconditionally self-heals, which is also why `fuzz_scenarios` (which
//! pumps exactly that way) never found the bug.
//!
//! The harness is a duplicate of the red test's, kept in sync by hand. See
//! the module comment over there for the full mechanism.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
    TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{CueSpec, Fault, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

const BOUND: Duration = Duration::from_secs(15);
const GATE: &str = "starve";
const STALL_AT: u64 = 30;

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

fn starved_video_media(key: &str) -> ScenarioHandle {
    let video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(25, 1),
            keyframe_interval: 1,
        },
    )
    .with_fault(Fault::StallAt {
        buffer_index: STALL_AT,
        sync_point: GATE.to_owned(),
    });
    ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"))
        .duration(gst::ClockTime::from_seconds(8))
        .pacing(Pacing::Realtime)
        .register()
}

fn external_text(key: &str) -> ScenarioHandle {
    let cues = (0..40)
        .map(|index| {
            let start = gst::ClockTime::from_mseconds(200) * (index + 1);
            CueSpec::new(
                start,
                start + gst::ClockTime::from_mseconds(100),
                format!("EXT{index:02}"),
            )
        })
        .collect();
    ScenarioBuilder::new(key)
        .text("text_0", cues)
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register()
}

/// Releases every sync point of its scenarios when dropped, so a panicking
/// assertion cannot leave a parked source push to wedge the unwinding
/// test's pipeline teardown.
struct ReleaseOnDrop(Vec<ScenarioHandle>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.release_all();
        }
    }
}

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    selected: std::cell::RefCell<Option<(Option<String>, Option<String>, Option<String>)>>,
    loaded: std::cell::Cell<bool>,
    /// Pump with the receiver's gate (false) or the fuzz driver's
    /// unconditional quiet (true).
    ungated: bool,
}

impl Harness {
    fn new(ungated: bool) -> Self {
        let video_sink = FTestSink::new();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
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
            selected: std::cell::RefCell::new(None),
            loaded: std::cell::Cell::new(false),
            ungated,
        }
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        let (current, pending) = self.playbin.state_summary();
        let quiet = self.ungated
            || (current == gst::State::Playing
                && pending == gst::State::VoidPending
                && !self.playbin.has_async_transition());
        self.playbin.pump_selection(SelectionGate {
            quiet,
            paused: false,
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlaybinEvent::StreamsSelected {
                    video,
                    audio,
                    subtitle,
                    ..
                } => *self.selected.borrow_mut() = Some((video, audio, subtitle)),
                PlaybinEvent::Loaded { .. } => self.loaded.set(true),
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                _ => {}
            }
        }
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + BOUND;
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} (pipeline {:?}, selected {:?})",
                self.playbin.state_summary(),
                self.selected.borrow()
            );
            std::thread::sleep(Duration::from_millis(10));
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

/// Play to a settled PLAYING, park the video stream on its gate, deselect
/// video and confirm it, then attach an external subtitle.
fn drive_to_attach(harness: &Harness, media: &ScenarioHandle, external: &ScenarioHandle) {
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_until("the load to report Loaded", |h| h.loaded.get());
    harness.playbin.play().expect("play after the load");
    harness.wait_until("a settled PLAYING", |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });
    assert!(
        media.sync_point(GATE).wait_for_arrival(BOUND),
        "the video stream never reached its stall gate"
    );

    harness
        .playbin
        .request_track(TrackSlot::Video, TrackTarget::Stream(None));
    harness.wait_until("the video deselect to confirm", |h| {
        matches!(&*h.selected.borrow(), Some((None, Some(_), _)))
    });
    let removal = Instant::now() + Duration::from_millis(500);
    while Instant::now() < removal {
        harness.pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    let id = harness
        .playbin
        .attach_subtitle(&external.uri())
        .expect("attaching the external subtitle");
    harness.wait_until("the external stream to materialize", |h| {
        !h.playbin.subtitle_stream_ids(id).is_empty()
    });
}

/// Control 1. The identical sequence WITHOUT the deselect keeps the pipeline
/// settled through the attach, so the deselected video slot is what arms the
/// wedge and the attach alone is harmless.
#[test]
fn control_attach_with_video_selected_stays_settled() {
    init();
    let media = starved_video_media("vresctl1");
    let external = external_text("vresctl1e");
    let harness = Harness::new(false);
    let _guard = ReleaseOnDrop(vec![media.clone(), external.clone()]);

    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_until("the load to report Loaded", |h| h.loaded.get());
    harness.playbin.play().expect("play after the load");
    harness.wait_until("a settled PLAYING", |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });
    assert!(
        media.sync_point(GATE).wait_for_arrival(BOUND),
        "the video stream never reached its stall gate"
    );

    let id = harness
        .playbin
        .attach_subtitle(&external.uri())
        .expect("attaching the external subtitle");
    harness.wait_until("the external stream to materialize", |h| {
        !h.playbin.subtitle_stream_ids(id).is_empty()
    });
    // The attach churns the collection. Give the auto-select every chance,
    // then require the pipeline settled with video still on.
    let hold = Instant::now() + Duration::from_secs(2);
    while Instant::now() < hold {
        harness.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.wait_until("a settled pipeline with video on", |h| {
        let (current, pending) = h.playbin.state_summary();
        current == gst::State::Playing
            && pending == gst::State::VoidPending
            && matches!(&*h.selected.borrow(), Some((Some(_), Some(_), _)) | None)
    });

    media.release_all();
    harness.shutdown();
    media.unregister();
    external.unregister();
}

/// Control 2. The same wedge sequence pumped with `quiet: true`
/// unconditionally (the fuzz driver's gate) self-heals: the re-assert
/// dispatches mid transition, decodebin3 drops the resurrected video pad,
/// and the re-preroll unblocks.
#[test]
fn control_ungated_pump_self_heals_the_resurrect() {
    init();
    let media = starved_video_media("vresctl2");
    let external = external_text("vresctl2e");
    let harness = Harness::new(true);
    let _guard = ReleaseOnDrop(vec![media.clone(), external.clone()]);

    drive_to_attach(&harness, &media, &external);

    harness.wait_until("the re-assert to settle the pipeline again", |h| {
        let (_, pending) = h.playbin.state_summary();
        pending == gst::State::VoidPending
            && !h.playbin.has_async_transition()
            && matches!(&*h.selected.borrow(), Some((None, Some(_), _)))
    });

    media.release_all();
    harness.shutdown();
    media.unregister();
    external.unregister();
}
