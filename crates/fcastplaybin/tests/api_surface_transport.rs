//! Pins for the transport-and-volume public API surface: the volume
//! subsystem, start rates (trickmode boundary, reverse), the actual seek
//! flags versus their docs, out-of-order transport calls, `StartOutcome`,
//! and the duration re-query contract.
//!
//! Harness modelled on `tests/scenarios.rs`. Deterministic `ftest://` media
//! where the scenario elements suffice. ftestsrc refuses every seek whose
//! rate is not 1.0 (see `fcasttest::src_bin::handle_seek`), so the rate
//! tests use real sample media instead.

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::Pacing,
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// Source pacing for every scenario here, see `tests/scenarios.rs`.
const PACING: Pacing = Pacing::AsFastAsPossible;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // FCASTPLAYBIN_TEST_LOG=debug shows the crate's tracing.
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        // Registered by the application in production, see scenarios.rs.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// A real sample file as a file URI, `None` when the checkout has no media.
fn sample_media(rel: &str) -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fcast-sample-media")
        .join(rel);
    path.is_file().then(|| format!("file://{}", path.display()))
}

/// A playbin whose sinks record, plus every event its callback produced.
/// The audio SINK ELEMENTS are kept (not just their recordings) so tests can
/// read sticky segments off the per-load sink pads.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<(PlaybinEvent, u64)>>,
    paused: Cell<bool>,
    /// A seek the crate refused and handed back (`QueueSeek`); re-driven at
    /// the next settled PAUSED, as the receiver's state machine does.
    parked_seek: Cell<Option<fcastplaybin::state_machine::Seek>>,
    video_sink: gst::Element,
    /// One entry per load, the audio sink is rebuilt per load.
    audio_sinks: Arc<Mutex<Vec<gst::Element>>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink: gst::Element = FTestSink::new().upcast();
        let audio_sinks: Arc<Mutex<Vec<gst::Element>>> = Arc::new(Mutex::new(Vec::new()));
        let audio_slot = audio_sinks.clone();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.clone()),
            audio: AudioSink::Factory(Box::new(move || {
                let sink = FTestSink::new();
                audio_slot
                    .lock()
                    .expect("audio sink slot")
                    .push(sink.clone().upcast());
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
            paused: Cell::new(false),
            parked_seek: Cell::new(None),
            video_sink,
            audio_sinks,
        }
    }

    fn redrive_transport(&self, event: &PlaybinEvent) {
        match event {
            PlaybinEvent::QueueSeek(seek) => self.parked_seek.set(Some(*seek)),
            PlaybinEvent::StateChanged {
                current: gst::State::Paused,
                pending: gst::State::VoidPending,
                ..
            } => {
                if let Some(seek) = self.parked_seek.take() {
                    self.playbin.seek_async(seek);
                }
            }
            _ => {}
        }
    }

    /// Wait (pumping) until at least `count` audio sinks were built, return
    /// the newest. `Loaded` does NOT imply the audio sink exists yet, the
    /// preroll token retires on the first prerolled sink while chains are
    /// still joining, so tests wait for the count they expect.
    fn wait_audio_sink(&self, count: usize) -> gst::Element {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            {
                let sinks = self.audio_sinks.lock().expect("audio sink slot");
                if sinks.len() >= count {
                    return sinks.last().cloned().expect("nonempty");
                }
            }
            assert!(
                Instant::now() < deadline,
                "audio sink {count} was never built; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        }
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok((event, generation)) = self.events.try_recv() {
            self.redrive_transport(&event);
            self.log.borrow_mut().push((event, generation));
        }
    }

    /// New VolumeChanged payloads since the last drain, in arrival order.
    fn drain_volume_events(&self) -> Vec<f64> {
        let before = self.log.borrow().len();
        self.drain_events();
        self.log.borrow()[before..]
            .iter()
            .filter_map(|(event, _)| match event {
                PlaybinEvent::VolumeChanged(v) => Some(*v),
                _ => None,
            })
            .collect()
    }

    /// Wait until `pred` matches a newly received event, pumping between
    /// polls. Returns the matched event's generation stamp. Panics with the
    /// log on timeout or pipeline error.
    fn wait_for(&self, what: &str, pred: impl FnMut(&PlaybinEvent) -> bool) -> u64 {
        self.wait_for_within(what, EVENT_TIMEOUT, pred)
    }

    fn wait_for_within(
        &self,
        what: &str,
        bound: Duration,
        mut pred: impl FnMut(&PlaybinEvent) -> bool,
    ) -> u64 {
        let deadline = Instant::now() + bound;
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what} within {bound:?}; log: {:#?}",
                    self.log.borrow()
                );
            }
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "pipeline error while waiting for {what}: {error} (log: {:#?})",
                            self.log.borrow()
                        );
                    }
                    self.redrive_transport(&event);
                    let hit = pred(&event);
                    self.log.borrow_mut().push((event, generation));
                    if hit {
                        return generation;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "event channel closed while waiting for {what}; log: {:#?}",
                    self.log.borrow()
                ),
            }
        }
    }

    /// Load `uri` async and wait for its `Loaded`. Returns the generation
    /// `load_async` handed out and the event's `live` payload.
    fn load_and_wait(&self, uri: &str, start: StartPoint) -> (u64, bool) {
        self.drain_events();
        let generation = self
            .playbin
            .load_async(MediaInput::Uri(uri.to_owned()), start);
        let live = Cell::new(false);
        let stamped = self.wait_for("Loaded", |event| match event {
            PlaybinEvent::Loaded { live: l } => {
                live.set(*l);
                true
            }
            _ => false,
        });
        assert_eq!(
            stamped, generation,
            "the Loaded event must be stamped with the generation load_async returned"
        );
        (generation, live.get())
    }

    fn play_and_settle(&self) {
        self.playbin.play().expect("play");
        self.wait_for("settled PLAYING", |event| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        });
    }

    /// The sticky SEGMENT on `sink`'s sink pad, waited for pumping (the
    /// chain the pad belongs to can join after `Loaded`). A sink already
    /// torn down never delivers, its pad lost its stickies on deactivation.
    fn sink_segment(
        &self,
        sink: &gst::Element,
        what: &str,
    ) -> gst::FormattedSegment<gst::ClockTime> {
        let pad = sink.static_pad("sink").expect("sink pad");
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(event) = pad.sticky_event::<gst::event::Segment>(0) {
                return event
                    .segment()
                    .downcast_ref::<gst::ClockTime>()
                    .expect("a TIME segment")
                    .clone();
            }
            assert!(
                Instant::now() < deadline,
                "no SEGMENT sticky appeared on the {what} sink pad"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + TEARDOWN_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "shutdown never finished");
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the worker died during shutdown")
                }
            }
        }
    }
}

/// A plain 1200 ms audio-only scenario.
fn audio_scenario(key: &str) -> fcasttest::scenario::ScenarioHandle {
    ScenarioBuilder::new(key)
        .audio("audio_0")
        .duration(gst::ClockTime::from_mseconds(1200))
        .pacing(PACING)
        .register()
}

/// set_volume, volume, renotify_volume and the clamp, all on the dedicated
/// instance-scoped volume element (src/pipeline.rs). No media needed, the
/// element exists from construction and notifies synchronously.
#[test]
fn volume_notify_contract() {
    init();
    let harness = Harness::new();

    assert_eq!(harness.playbin.volume(), 1.0, "default volume");

    harness.playbin.set_volume(0.5);
    assert_eq!(harness.playbin.volume(), 0.5);
    assert_eq!(
        harness.drain_volume_events(),
        vec![0.5],
        "one VolumeChanged per value change"
    );

    harness.playbin.renotify_volume();
    assert_eq!(
        harness.drain_volume_events(),
        vec![0.5],
        "renotify_volume re-emits the current value"
    );

    // Clamp above.
    harness.playbin.set_volume(2.0);
    assert_eq!(harness.playbin.volume(), 1.0, "clamped to 1.0");
    assert_eq!(harness.drain_volume_events(), vec![1.0]);

    // Clamp below.
    harness.playbin.set_volume(-1.0);
    assert_eq!(harness.playbin.volume(), 0.0, "clamped to 0.0");
    assert_eq!(harness.drain_volume_events(), vec![0.0]);
}

/// A same-value set emits no VolumeChanged; renotify_volume is the escape
/// hatch for callers that need a confirmation anyway. The skip lives in
/// set_volume itself, since a plain GObject set_property notifies
/// unconditionally (the defect this test caught when it was written).
#[test]
fn volume_idempotent_set_emits_no_event() {
    init();
    let harness = Harness::new();

    harness.playbin.set_volume(0.5);
    assert_eq!(harness.drain_volume_events(), vec![0.5]);

    harness.playbin.set_volume(0.5);
    assert_eq!(
        harness.drain_volume_events(),
        Vec::<f64>::new(),
        "an idempotent set emits no VolumeChanged"
    );

    // A clamped set landing on the current value is idempotent too.
    harness.playbin.set_volume(2.0);
    assert_eq!(harness.drain_volume_events(), vec![1.0]);
    harness.playbin.set_volume(1.5);
    assert_eq!(
        harness.drain_volume_events(),
        Vec::<f64>::new(),
        "a clamped set landing on the current value emits nothing"
    );
}

/// The volume element is instance-scoped while the audio sink is per-load
/// (src/pipeline.rs set_volume doc), so a load must not reset the volume and
/// VolumeChanged must keep working after one.
#[test]
fn volume_survives_a_load() {
    init();
    let first = audio_scenario("apivolload1");
    let second = audio_scenario("apivolload2");
    let harness = Harness::new();

    harness.playbin.set_volume(0.25);
    assert_eq!(harness.drain_volume_events(), vec![0.25]);

    let zero = StartPoint::Seek {
        position: gst::ClockTime::ZERO,
        rate: 1.0,
    };
    harness.load_and_wait(&first.uri(), zero);
    harness.wait_audio_sink(1);
    harness.load_and_wait(&second.uri(), zero);

    // Two loads, two fresh audio sinks, one untouched volume.
    harness.wait_audio_sink(2);
    assert_eq!(
        harness.playbin.volume(),
        0.25,
        "volume survives the per-load sink swap"
    );
    assert_eq!(
        harness.drain_volume_events(),
        Vec::<f64>::new(),
        "loading emits no VolumeChanged"
    );

    harness.playbin.set_volume(0.75);
    assert_eq!(harness.playbin.volume(), 0.75);
    assert_eq!(harness.drain_volume_events(), vec![0.75]);

    harness.shutdown();
    first.unregister();
    second.unregister();
}

/// A 3.0x start rate crosses the trickmode boundary (seek_flags_for,
/// src/pipeline.rs, sets TRICKMODE for rate > 2.0). The video sink sees the
/// raw upstream segment. fcastaudiostretch consumes the rate on the audio
/// branch (rate becomes applied_rate, downstream plays 1.0 over stretched
/// samples), so the audio sink pins that rewrite instead.
#[test]
fn start_rate_three_is_trickmode_at_the_sink() {
    init();
    let Some(uri) = sample_media("video/short_clip.mkv") else {
        eprintln!("skipping: fcast-sample-media/video/short_clip.mkv is not present");
        return;
    };
    let harness = Harness::new();
    harness.load_and_wait(
        &uri,
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 3.0,
        },
    );

    let video = harness.sink_segment(&harness.video_sink, "video");
    eprintln!(
        "video segment rate={} applied={} flags={:?}",
        video.rate(),
        video.applied_rate(),
        video.flags()
    );
    assert_eq!(video.rate(), 3.0, "the video sink sees the 3.0x segment");
    assert!(
        video.flags().contains(gst::SegmentFlags::TRICKMODE),
        "rate 3.0 is past the trickmode boundary, flags {:?}",
        video.flags()
    );

    let audio = harness.sink_segment(&harness.wait_audio_sink(1), "audio");
    eprintln!(
        "audio segment rate={} applied={} flags={:?}",
        audio.rate(),
        audio.applied_rate(),
        audio.flags()
    );
    assert_eq!(
        audio.applied_rate(),
        3.0,
        "fcastaudiostretch consumes the rate into applied_rate"
    );
    assert_eq!(audio.rate(), 1.0, "the audio sink plays stretched 1.0x");
    assert!(
        audio.flags().contains(gst::SegmentFlags::TRICKMODE),
        "TRICKMODE survives the stretch rewrite, flags {:?}",
        audio.flags()
    );

    harness.shutdown();
}

/// 2.0x sits exactly ON the boundary (seek_flags_for uses rate > 2.0), so
/// "watch faster" keeps full-quality decode with no TRICKMODE.
#[test]
fn start_rate_two_keeps_trickmode_off() {
    init();
    let Some(uri) = sample_media("video/short_clip.mkv") else {
        eprintln!("skipping: fcast-sample-media/video/short_clip.mkv is not present");
        return;
    };
    let harness = Harness::new();
    harness.load_and_wait(
        &uri,
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 2.0,
        },
    );

    let video = harness.sink_segment(&harness.video_sink, "video");
    assert_eq!(video.rate(), 2.0, "the video sink sees the 2.0x segment");
    assert!(
        !video.flags().contains(gst::SegmentFlags::TRICKMODE),
        "rate 2.0 must NOT set TRICKMODE, flags {:?}",
        video.flags()
    );

    let audio = harness.sink_segment(&harness.wait_audio_sink(1), "audio");
    assert_eq!(audio.applied_rate(), 2.0);
    assert_eq!(audio.rate(), 1.0);
    assert!(
        !audio.flags().contains(gst::SegmentFlags::TRICKMODE),
        "no TRICKMODE at the audio sink either, flags {:?}",
        audio.flags()
    );

    harness.shutdown();
}

/// A reverse start rate takes rate_seek_event's End-anchored branch
/// (src/pipeline.rs, start Set..ZERO stop End..position) and must land a
/// negative-rate segment at the sink.
#[test]
fn reverse_start_rate_at_the_sink() {
    init();
    let Some(uri) = sample_media("video/short_clip.mkv") else {
        eprintln!("skipping: fcast-sample-media/video/short_clip.mkv is not present");
        return;
    };
    let harness = Harness::new();
    harness.load_and_wait(
        &uri,
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: -1.0,
        },
    );

    let video = harness.sink_segment(&harness.video_sink, "video");
    eprintln!(
        "reverse video segment rate={} applied={} flags={:?} start={:?} stop={:?} time={:?} position={:?}",
        video.rate(),
        video.applied_rate(),
        video.flags(),
        video.start(),
        video.stop(),
        video.time(),
        video.position()
    );
    assert_eq!(video.rate(), -1.0, "the video sink sees the reverse segment");
    assert!(
        video.flags().contains(gst::SegmentFlags::TRICKMODE),
        "reverse always sets TRICKMODE, flags {:?}",
        video.flags()
    );
    // End-anchored, so the segment spans up to the clip's end (2.023 s).
    let stop = video.stop().expect("a reverse segment carries its stop");
    assert!(
        stop > gst::ClockTime::from_seconds(1) && stop < gst::ClockTime::from_seconds(3),
        "stop anchored at the clip end, got {stop:?}"
    );

    let audio = harness.sink_segment(&harness.wait_audio_sink(1), "audio");
    eprintln!(
        "reverse audio segment rate={} applied={} flags={:?}",
        audio.rate(),
        audio.applied_rate(),
        audio.flags()
    );
    assert_eq!(
        audio.applied_rate(),
        -1.0,
        "fcastaudiostretch consumes the reverse rate into applied_rate"
    );

    harness.shutdown();
}

/// DOC DIVERGENCE, pinned on purpose. `StartPoint::Seek` (src/api.rs) and
/// `rate_seek_event` (src/pipeline.rs) both document "flushing ACCURATE"
/// seeks, but seek_flags_for never sets ACCURATE and `seek()` uses
/// seek_simple with FLUSH alone. This test captures the seek events that
/// actually reach the source and pins the CURRENT flags, FLUSH only.
#[test]
fn seek_flags_doc_divergence() {
    init();
    let scenario = audio_scenario("apiseekflags");
    let harness = Harness::new();

    // Capture every upstream Seek arriving at ftestsrc's src pads.
    let captured: Arc<Mutex<Vec<(f64, gst::SeekFlags)>>> = Arc::new(Mutex::new(Vec::new()));
    let slot = captured.clone();
    harness
        .playbin
        .pipeline()
        .connect_deep_element_added(move |_, _, element| {
            if element
                .factory()
                .is_none_or(|f| f.name().as_str() != "ftestsrc")
            {
                return;
            }
            for pad in element.src_pads() {
                arm_seek_capture(&pad, &slot);
            }
            let slot = slot.clone();
            element.connect_pad_added(move |_, pad| arm_seek_capture(pad, &slot));
        });

    // A non-zero start position forces the start seek (a 1.0x zero start is
    // skipped by design).
    harness.load_and_wait(
        &scenario.uri(),
        StartPoint::Seek {
            position: gst::ClockTime::from_mseconds(300),
            rate: 1.0,
        },
    );
    {
        let seeks = captured.lock().expect("seek capture");
        assert!(!seeks.is_empty(), "the start seek never reached the source");
        for (rate, flags) in seeks.iter() {
            assert_eq!(*rate, 1.0);
            assert_eq!(
                *flags,
                gst::SeekFlags::FLUSH,
                "start seek flags are FLUSH only, ACCURATE is documented but never set"
            );
        }
    }

    // The plain seek() path, seek_simple(FLUSH) at src/pipeline.rs.
    captured.lock().expect("seek capture").clear();
    harness
        .playbin
        .seek(gst::ClockTime::from_mseconds(600))
        .expect("seek");
    {
        let seeks = captured.lock().expect("seek capture");
        assert!(!seeks.is_empty(), "the plain seek never reached the source");
        for (rate, flags) in seeks.iter() {
            assert_eq!(*rate, 1.0);
            assert_eq!(
                *flags,
                gst::SeekFlags::FLUSH,
                "seek() flags are FLUSH only, no ACCURATE despite the api.rs doc"
            );
        }
    }

    harness.shutdown();
    scenario.unregister();
}

fn arm_seek_capture(pad: &gst::Pad, captured: &Arc<Mutex<Vec<(f64, gst::SeekFlags)>>>) {
    let captured = captured.clone();
    pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && let gst::EventView::Seek(seek) = event.view()
        {
            let (rate, flags, ..) = seek.get();
            captured.lock().expect("seek capture").push((rate, flags));
        }
        gst::PadProbeReturn::Ok
    });
}

/// Transport calls in the wrong order must return instead of wedging, and a
/// load afterwards must still work. Pins the actual Results.
#[test]
fn out_of_order_transport_calls() {
    init();
    let first = audio_scenario("apiorder1");
    let second = audio_scenario("apiorder2");
    let harness = Harness::new();
    let zero = StartPoint::Seek {
        position: gst::ClockTime::ZERO,
        rate: 1.0,
    };

    // play() with nothing loaded returns Ok (the state change goes async on
    // the empty pipeline) and must not wedge the instance.
    harness.playbin.play().expect("play before any load");

    harness.load_and_wait(&first.uri(), zero);
    harness.play_and_settle();

    // stop() twice in a row, both Ok.
    harness.playbin.stop().expect("first stop");
    harness.playbin.stop().expect("second stop on a stopped instance");

    // pause() on the stopped instance.
    harness.playbin.pause().expect("pause on a stopped instance");

    // And a load still works after all of it.
    harness.load_and_wait(&second.uri(), zero);
    harness.play_and_settle();

    harness.shutdown();
    first.unregister();
    second.unregister();
}

/// StartOutcome's fields mean what they say. The generation matches the one
/// stamped on events, live is false for a file source even under
/// StartPoint::Live, and Live skips the start seek entirely.
#[test]
fn start_outcome_and_live() {
    init();
    let scenario = ScenarioBuilder::new("apistartoutcome")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(5))
        .pacing(PACING)
        .register();
    let harness = Harness::new();

    // A seeked start first, proving this media honours start seeks, so the
    // Live load's zero below is Live's doing and not a broken seek path.
    let target = gst::ClockTime::from_seconds(2);
    let (seek_generation, live) = harness.load_and_wait(
        &scenario.uri(),
        StartPoint::Seek {
            position: target,
            rate: 1.0,
        },
    );
    assert!(!live, "a file scenario is not live");
    let seeked = harness.sink_segment(&harness.wait_audio_sink(1), "audio");
    eprintln!(
        "seeked start segment start={:?} time={:?} position={:?}",
        seeked.start(),
        seeked.time(),
        seeked.position()
    );
    assert_eq!(
        seeked.time(),
        Some(target),
        "the seeked start lands on the target timeline"
    );

    // The sync load returns the StartOutcome nothing read before.
    let outcome = harness
        .playbin
        .load(MediaInput::Uri(scenario.uri()), StartPoint::Live)
        .expect("sync load");
    assert!(
        !outcome.live,
        "StartPoint::Live does not make a preroll-capable source live"
    );
    assert!(
        outcome.generation > seek_generation,
        "each load gets a fresh generation, {} then {}",
        seek_generation,
        outcome.generation
    );

    // Live skips the start seek, so the fresh sink starts at zero.
    let live_segment = harness.sink_segment(&harness.wait_audio_sink(2), "audio");
    assert_eq!(
        live_segment.time(),
        Some(gst::ClockTime::ZERO),
        "a Live start performs no start seek"
    );
    let position = harness.playbin.position().expect("position after load");
    assert!(
        position < gst::ClockTime::from_mseconds(500),
        "position starts near zero, got {position:?}"
    );

    // Loaded { live } mirrors StartOutcome::live for the same input.
    let (_, live) = harness.load_and_wait(&scenario.uri(), StartPoint::Live);
    assert_eq!(
        live, outcome.live,
        "Loaded's live must carry what StartOutcome said"
    );

    harness.shutdown();
    scenario.unregister();
}

/// DurationChanged means the cached value is stale and only a re-query is
/// authoritative (the event carries no payload by design). Nothing in
/// production re-queries today, this pins that a re-query works and returns
/// the refined value. mpegaudioparse refines its estimate for this VBR-ish
/// mp3 shortly after data starts flowing.
#[test]
fn duration_refines() {
    init();
    let Some(uri) = sample_media("audio/Court_House_Blues_Take_1.mp3") else {
        eprintln!("skipping: fcast-sample-media/audio/Court_House_Blues_Take_1.mp3 is not present");
        return;
    };
    let harness = Harness::new();
    harness.load_and_wait(
        &uri,
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    let before = harness.playbin.duration();
    eprintln!("duration at Loaded: {before:?}");

    harness.play_and_settle();

    // The refinement may already sit in the log from the load waits.
    let logged = harness
        .log
        .borrow()
        .iter()
        .any(|(event, _)| matches!(event, PlaybinEvent::DurationChanged));
    if !logged {
        harness.wait_for("DurationChanged", |event| {
            matches!(event, PlaybinEvent::DurationChanged)
        });
    }

    let after = harness
        .playbin
        .duration()
        .expect("a re-query after DurationChanged answers");
    eprintln!("duration after DurationChanged: {after:?}");
    // The file is 3 min 19 s. The refined answer must describe it.
    assert!(
        after > gst::ClockTime::from_seconds(150) && after < gst::ClockTime::from_seconds(250),
        "the refined duration describes the file, got {after:?}"
    );

    harness.shutdown();
}
