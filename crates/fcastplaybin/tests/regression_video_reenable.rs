//! RED regression for the video-reenable wedge, reduced from `fuzz_buffering`
//! seed 1600031. Lever: `FCAST_READY_PARK_DESELECTED_VIDEO=1` restores the old
//! behaviour and wedges this test. Long-form record: NEXT-FIXES-PLAN.md.
//!
//! A mid-item video deselect used to park the chain with a sink-first READY
//! descent, aborting the clock wait of decodebin3's multiqueue slot task
//! mid-`gst_pad_push`. `gst_multi_queue_loop` treats the resulting
//! GST_FLOW_FLUSHING as fatal, flushes the video single-queue and parks the
//! demuxer's task on a FLUSH_STOP nobody will send: video dies with no EOS and
//! no error. The re-select gets a fresh pad and no data, the pipeline stays
//! ASYNC, and the receiver's quiet gate (`running && !has_async_transition()`)
//! never dispatches the corrective re-assert.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

const BOUND: Duration = Duration::from_secs(15);
/// Generous: the healthy path settles well under a second, the wedge never.
const SETTLE_BOUND: Duration = Duration::from_secs(12);

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
    // Two scenario constraints, both measured:
    //
    // * the source must run FAR ahead of the clock-synced sink so the slot task
    //   sits inside `gst_base_sink_wait_clock` and the deselect lands on an
    //   in-flight push by construction. `Pacing::Realtime` keeps the queue
    //   near-empty and the test then passes in BOTH arms.
    // * the video stream must still have media left at the re-enable. Nothing
    //   throttles a DESELECTED stream, so a short clip hits EOS within ms and the
    //   drained-resurrect park rightly refuses the re-route: fails in BOTH arms.
    //
    // 5 ms/buffer against a 10 fps sink gives the first, 90 s of media (900
    // buffers, 4.5 s unthrottled) the second against the 0.5 s deselect hold.
    let video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(10, 1),
            keyframe_interval: 1,
        },
    );
    ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"))
        .duration(gst::ClockTime::from_seconds(90))
        .pacing(Pacing::Jitter {
            base_ms: 5,
            jitter_ms: 0,
        })
        .register()
}

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    video_sink: gst::Element,
    collection: std::cell::RefCell<Option<gst::StreamCollection>>,
    selected_video: std::cell::RefCell<Option<Option<String>>>,
    loaded: std::cell::Cell<bool>,
    error: std::cell::RefCell<Option<String>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.clone().upcast()),
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
            video_sink: video_sink.upcast(),
            collection: std::cell::RefCell::new(None),
            selected_video: std::cell::RefCell::new(None),
            loaded: std::cell::Cell::new(false),
            error: std::cell::RefCell::new(None),
        }
    }

    /// The receiver's own gate, derived the way `player.rs` derives it. An
    /// unconditional `quiet: true` would let the engine self-heal the wedge and
    /// the test would prove nothing (see `video_resurrect_controls`).
    fn pump(&self) {
        self.playbin.poll_text_policy();
        let (current, pending) = self.playbin.state_summary();
        let quiet = current == gst::State::Playing
            && pending == gst::State::VoidPending
            && !self.playbin.has_async_transition();
        self.playbin.pump_selection(SelectionGate {
            quiet,
            paused: false,
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlaybinEvent::StreamCollection(collection) => {
                    *self.collection.borrow_mut() = Some(collection)
                }
                PlaybinEvent::StreamsSelected { video, .. } => {
                    *self.selected_video.borrow_mut() = Some(video)
                }
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
                "timed out waiting for {what} (pipeline {:?}, unsettled {:?}, selected video \
                 {:?})",
                self.playbin.state_summary(),
                self.playbin.unsettled_elements(),
                self.selected_video.borrow(),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn video_sid(&self) -> Option<String> {
        let collection = self.collection.borrow().clone()?;
        collection.iter().find_map(|stream| {
            stream
                .stream_type()
                .contains(gst::StreamType::VIDEO)
                .then(|| stream.stream_id().map(|id| id.to_string()))
                .flatten()
        })
    }

    fn rendered(&self) -> usize {
        self.video_sink
            .downcast_ref::<FTestSink>()
            .expect("the video sink is an FTestSink")
            .recording()
            .buffer_count()
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

/// Video off mid-item then on again must leave the pipeline settled and
/// rendering. Before the drop-probe park this wedged with the video sink stuck
/// at `Ready->Paused`, the deselect having killed the source's video stream.
#[test]
fn video_off_then_on_mid_item_keeps_rendering() {
    init();
    let media = media("vreenable1");
    let harness = Harness::new();

    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_until("the load to report Loaded", BOUND, |h| h.loaded.get());
    harness.playbin.play().expect("play after the load");
    harness.wait_until("a settled PLAYING", BOUND, |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });
    // Real playback first, so the deselect lands mid-stream, not on a preroll.
    harness.wait_until("the first video frames to render", BOUND, |h| {
        h.rendered() >= 5
    });
    harness.wait_until("the collection to name a video stream", BOUND, |h| {
        h.video_sid().is_some()
    });
    let sid = harness.video_sid().expect("checked just above");

    harness
        .playbin
        .request_track(TrackSlot::Video, TrackTarget::Stream(None));
    harness.pump();
    harness.wait_until("the video deselect to confirm", BOUND, |h| {
        matches!(&*h.selected_video.borrow(), Some(None))
    });
    // Let the chain teardown finish: the re-enable must route a fresh pad, not
    // race the old one's removal.
    let hold = Instant::now() + Duration::from_millis(500);
    while Instant::now() < hold {
        harness.pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    let before = harness.rendered();
    harness
        .playbin
        .request_track(TrackSlot::Video, TrackTarget::Stream(Some(sid.clone())));
    harness.pump();
    harness.wait_until(
        "the video re-enable to confirm",
        SETTLE_BOUND,
        |h| matches!(&*h.selected_video.borrow(), Some(Some(got)) if *got == sid),
    );
    harness.wait_until(
        "the pipeline to settle after the re-enable",
        SETTLE_BOUND,
        |h| {
            h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
                && !h.playbin.has_async_transition()
        },
    );
    // Settled is not enough: a joined chain that gets no data reads settled too.
    harness.wait_until(
        "video to render again after the re-enable",
        SETTLE_BOUND,
        |h| h.rendered() > before,
    );

    media.release_all();
    harness.shutdown();
    media.unregister();
}
