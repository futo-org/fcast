//! Deterministic scenario regressions: the REAL fcastplaybin driven through its
//! normal load API over `ftest://` media.
//!
//! Nothing about the crate's code path changes here. urisourcebin resolves the
//! scheme to ftestsrc, parsebin plugs ftestparse, decodebin3 autoplugs ftestdec,
//! and the sinks are fcasttest recording sinks handed in through [`Sinks`]. What
//! the harness buys over the real-media suite is control: a buffer parks on a
//! named gate instead of being pushed, so "stop while the source is mid-push" is
//! a fact rather than a race a 7 MB subtitle file makes likely.

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::{ScenarioBuilder, check_all_named, wait_quiescent},
    sink::{FTestSink, Recording, event_name},
    spec::{CueSpec, DecoderKnobs, Fault, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

/// Generous bound for anything the pipeline has to reach. Scenario media plays in
/// real time (the sinks sync), so a busy box must not flake: the suite now runs
/// eleven concurrent pipelines, and under a full-machine soak the mid-load dances
/// can trail a plain wait by tens of seconds without anything being wrong.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Bound for a stop or a load that has to tear down a parked source. The field
/// failure this replaces never returned at all.
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// No log growth for this long counts as quiescent for the invariant sweep.
const QUIESCENT_SETTLE: Duration = Duration::from_millis(200);

/// The crate's `PREROLL_TIMEOUT` (`src/lib.rs`), mirrored here because it is the
/// ONLY way a start seek is allowed to be skipped on a seekable source: with a
/// healthy preroll `apply_start_seek` either seeks or the source answered the
/// seeking query with "no", and both of those are regressions rather than load.
/// Used to decide whether a start that landed on zero may be forgiven, see
/// [`assert_seeked_start_attach_shares_the_timeline`].
const PREROLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Short clips keep the suite fast. 25 fps and one 20 ms audio packet per buffer,
/// so this is 30 video buffers and 60 audio buffers.
const CLIP: gst::ClockTime = gst::ClockTime::from_mseconds(1200);

/// Long enough that nothing ends while a test works with a parked source.
const LONG_CLIP: gst::ClockTime = gst::ClockTime::from_seconds(30);

/// Source-side pacing for every scenario here. Unpaced: ftestsrc pushes the whole
/// schedule as fast as the chain accepts it and the sinks do the syncing.
///
/// This used to be a 10 ms pre-push delay per buffer. fcasttest caps are sticky, so
/// typefind classifies from the CAPS event alone and forwards the first buffer into
/// the window where parsebin has unlinked `typefind:src` from its parse pad and not
/// yet linked it to the parser. The push returned NotLinked and ftestsrc posted a
/// stream error, so the delay was there to miss the window. ftestsrc now retries a
/// NotLinked push for a bounded while instead (see `NOT_LINKED_BOUND` in
/// `fcasttest::src_bin`), which removes the reason to pace at all.
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
        // The receiver's part of the pipeline: fcastaudiostretch is built by
        // the fcastplaybin constructor but registered by the application.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// Cues dense enough that one is on screen for most of a clip.
fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("CUE{index:02}"))
        })
        .collect()
}

/// A video stream at `fps`, for scenarios whose decoder knobs need a frame budget
/// wider than the 25 fps default.
fn video_at(id: &str, fps: i32) -> StreamSpec {
    StreamSpec::new(
        id,
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(fps, 1),
            keyframe_interval: 5,
        },
    )
}

/// A playbin whose sinks record, plus every event its callback produced. Modelled
/// on `tests/subtitle_disable.rs`: same gate, same pumped waits, same log-first
/// matching (events can arrive during preroll, before the wait that needs them).
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    /// Caller-owned, so one log spans every load of the test.
    video: Recording,
    /// One entry per load: the audio sink is rebuilt per load by construction.
    audio: Arc<Mutex<Vec<Recording>>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let audio: Arc<Mutex<Vec<Recording>>> = Arc::new(Mutex::new(Vec::new()));
        let audio_slot = audio.clone();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(move || {
                let sink = FTestSink::new();
                audio_slot
                    .lock()
                    .expect("audio recording slot")
                    .push(sink.recording());
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
            video,
            audio,
        }
    }

    /// The current load's audio recording. Panics before the first load.
    fn audio(&self) -> Recording {
        self.audio
            .lock()
            .expect("audio recording slot")
            .last()
            .cloned()
            .expect("an audio sink was built")
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        }
    }

    /// The receiver's settle-point calls, run from every wait loop.
    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok((event, _generation)) = self.events.try_recv() {
            self.log.borrow_mut().push(event);
        }
    }

    /// Wait until `pred` matches a newly received event, pumping between polls.
    /// Panics with the log on timeout or pipeline error.
    fn wait_for(&self, what: &str, pred: impl FnMut(&PlaybinEvent) -> bool) {
        self.wait_for_within(what, EVENT_TIMEOUT, pred);
    }

    /// [`Harness::wait_for`] with a caller-chosen bound. A test whose POINT is
    /// that something happens quickly must wait under its own bound: asserting
    /// on the elapsed time afterwards cannot fail below the wait's own timeout,
    /// so a 15 s claim checked under a 40 s wait only ever bites in the 15-40 s
    /// window and a true wedge is reported as a timeout instead.
    fn wait_for_within(
        &self,
        what: &str,
        bound: Duration,
        mut pred: impl FnMut(&PlaybinEvent) -> bool,
    ) {
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
                Ok((event, _generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "pipeline error while waiting for {what}: {error} (log: {:#?})",
                            self.log.borrow()
                        );
                    }
                    let hit = pred(&event);
                    self.log.borrow_mut().push(event);
                    if hit {
                        return;
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

    /// Load `uri`, start playback, wait for the settled PLAYING. Returns how long
    /// the load itself took (teardown of any previous item included: the worker
    /// resets before it wires the new input).
    fn load_and_play(&self, uri: &str) -> Duration {
        self.load_and_play_within(uri, EVENT_TIMEOUT)
    }

    /// [`Harness::load_and_play`] whose `Loaded` wait is bounded by `bound`, so
    /// a load that must be fast fails AT its bound rather than at the generic
    /// event timeout.
    fn load_and_play_within(&self, uri: &str, bound: Duration) -> Duration {
        // Every wait below must be satisfied by THIS load's events.
        self.drain_events();
        let started = Instant::now();
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for_within("Loaded", bound, |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        let loaded = started.elapsed();
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
        loaded
    }

    /// Stream types of the latest advertised collection in the log.
    fn collection_types(&self) -> Vec<gst::StreamType> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => Some(
                    collection
                        .iter()
                        .map(|stream| stream.stream_type())
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// First stream of `kind` in the latest advertised collection. Only used
    /// where no external input is attached, so every stream is the item's own.
    fn item_stream_id(&self, kind: gst::StreamType) -> Option<String> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => Some(collection.clone()),
                _ => None,
            })?
            .iter()
            .find_map(|stream| {
                stream
                    .stream_type()
                    .contains(kind)
                    .then(|| stream.stream_id().map(|id| id.to_string()))
                    .flatten()
            })
    }

    /// The latest `StreamsSelected` in the log (subtitle slot), if any.
    fn last_selected_subtitle(&self) -> Option<Option<String>> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.clone()),
                _ => None,
            })
    }

    /// The overlay element. The video chain (overlay included) is installed
    /// when the item's video ROUTES, which under load can trail the settled
    /// PLAYING the tests wait on, so lookups wait bounded instead of
    /// expecting instant presence.
    fn overlay(&self) -> gst::Element {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(overlay) = self.playbin.pipeline().by_name("fpb-suboverlay") {
                return overlay;
            }
            assert!(
                Instant::now() < deadline,
                "subtitleoverlay never joined the pipeline; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The overlay's subtitle input pad. Always present once the overlay is,
    /// only its peer comes and goes.
    fn overlay_subtitle_pad(&self) -> gst::Pad {
        self.overlay()
            .static_pad("subtitle_sink")
            .expect("subtitleoverlay has a subtitle_sink pad")
    }

    /// Wait (pumping) until the overlay's subtitle input is linked. Panics after
    /// `bound`.
    fn wait_subtitle_branch(&self, bound: Duration, what: &str) {
        let pad = self.overlay_subtitle_pad();
        let start = Instant::now();
        while !pad.is_linked() {
            assert!(
                start.elapsed() < bound,
                "the subtitle branch never reached the overlay within {bound:?} ({what})"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Blocks until the worker tore the pipeline down. Tests whose media keeps
    /// producing after the assertions (realtime pacing, long clips) must call
    /// this: a leaked running pipeline starves the tests that follow in the
    /// same process.
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

    /// Sweep every reachable sink once the logs stop growing. Only legal at a
    /// quiescent point: a snapshot taken mid-flushing-seek ends with an unmatched
    /// FLUSH_START and would report a violation.
    fn check_all(&self, what: &str) {
        let audio = self.audio();
        let recordings = [("video", &self.video), ("audio", &audio)];
        assert!(
            wait_quiescent(&recordings, QUIESCENT_SETTLE, EVENT_TIMEOUT),
            "the sinks never went quiescent ({what})"
        );
        if let Err(violations) = check_all_named(&recordings) {
            panic!("{what}: {violations}");
        }
    }
}

/// The first element of `factory` anywhere in the pipeline, autoplugged ones
/// included.
fn find_element(harness: &Harness, factory: &str) -> Option<gst::Element> {
    harness
        .playbin
        .pipeline()
        .iterate_recurse()
        .into_iter()
        .flatten()
        .find(|element| {
            element
                .factory()
                .is_some_and(|f| f.name().as_str() == factory)
        })
}

/// The worker must answer a queued job within a bound: a graph-dump round-trip
/// proves it is not wedged inside a previous job.
fn assert_worker_alive(harness: &Harness, what: &str) {
    let (tx, rx) = mpsc::channel();
    harness.playbin.debug_graph_async(Box::new(move |_| {
        let _ = tx.send(());
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(()) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the worker is wedged: {what}");
                harness.settle_pump();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died: {what}"),
        }
    }
}

/// Video + audio + text, played end to end. The baseline every other scenario
/// deviates from: the whole field topology over fake edges, no injected fault.
#[test]
fn smoke_full_pipeline() {
    init();
    // Longer than CLIP on purpose: the item has to outlive the text selection
    // and branch link asserted below. CLIP itself is left alone because other
    // scenarios time against it.
    let smoke_clip = gst::ClockTime::from_seconds(4);
    let scenario = ScenarioBuilder::new("smokeav")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(6, gst::ClockTime::from_mseconds(500)))
        .duration(smoke_clip)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());

    // The scenario builds a text stream, and nothing here used to look at it: the
    // baseline advertised text and then asserted only on video and audio, so the
    // whole text half of "the whole field topology" was decoration. Selected
    // EXPLICITLY rather than by leaning on the default policy (which is off, see
    // `selection.rs`: "text auto-select is off"), because a baseline that
    // silently depends on a default goes quiet the day the default moves.
    harness.drain_events();
    let text_sid = harness
        .item_stream_id(gst::StreamType::TEXT)
        .expect("the collection advertises the scenario's text stream");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(text_sid.clone())));
    harness.playbin.pump_selection(harness.gate());

    // The selection must be CONFIRMED by decodebin3 with this exact stream, and
    // the text branch must actually reach subtitleoverlay. Both are bounded
    // waits, so a text slot that silently never routes fails here instead of
    // being invisible.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(text_sid.clone())) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the embedded text selection never confirmed; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "smoke embedded text");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });

    // NOT asserted here: that a cue rendered. With `Pacing::AsFastAsPossible`
    // ftestsrc pushes the whole text schedule at t=0, and an embedded text
    // stream drains into its parking sink until `poll_text_policy` links the
    // branch, so every cue is gone before the branch exists (measured: the
    // selection confirms, the overlay's subtitle pad IS linked, and zero buffers
    // ever cross it). Asserting `cues > 0` here would need realtime pacing,
    // which is what tests/subtitle_disable.rs uses; adding a cue count to this
    // baseline without that change would just be an assertion that can never
    // hold. What IS asserted is the routing: advertised, confirmed, linked.

    harness.drain_events();
    let types = harness.collection_types();
    for want in [
        gst::StreamType::VIDEO,
        gst::StreamType::AUDIO,
        gst::StreamType::TEXT,
    ] {
        assert!(
            types.iter().any(|ty| ty.contains(want)),
            "the collection {types:?} misses {want:?}"
        );
    }

    let audio = harness.audio();
    assert!(
        harness.video.buffer_count() > 0,
        "no video reached the sink: {:?}",
        harness.video
    );
    assert!(
        audio.buffer_count() > 0,
        "no audio reached the sink: {audio:?}"
    );
    assert_eq!(
        harness.video.event_count(event_name::EOS),
        1,
        "exactly one EOS per sink"
    );
    harness.check_all("smoke");
    assert_worker_alive(&harness, "after playing to EOS");
    scenario.unregister();
}

/// `stop()` while the source is PARKED mid-push must return within a bound and
/// leave the worker answering. The deterministic replacement for the 7 MB-SRT
/// trick: the gate makes "still pushing" a fact, not a probability.
#[test]
fn stop_during_stalled_push_is_bounded() {
    init();
    let scenario = ScenarioBuilder::new("stopstall")
        .stream(StreamSpec::video("video_0").with_fault(Fault::StallAt {
            buffer_index: 4,
            sync_point: "midpush".to_owned(),
        }))
        .audio("audio_0")
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let timeline = scenario.timeline();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());
    timeline
        .on_sync_point_arrival("midpush")
        .expect("the video push parks");
    // Non-vacuous: only a flush or teardown unparks the gate, so the stream is
    // provably held at buffer 4 (0 through 3 rendered, 4 never pushed).
    assert!(
        wait_quiescent(
            &[("video", &harness.video)],
            QUIESCENT_SETTLE,
            EVENT_TIMEOUT
        ),
        "the video sink kept receiving with the source parked"
    );
    assert_eq!(
        harness.video.buffer_count(),
        4,
        "the parked stream delivered past its stall: {:?}",
        harness.video
    );
    assert_worker_alive(&harness, "with the source parked mid-push");

    // Stop on a helper thread so a wedge fails the test instead of hanging it.
    let playbin = harness.playbin.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(playbin.stop().is_ok());
    });
    match rx.recv_timeout(TEARDOWN_BOUND) {
        Ok(ok) => assert!(ok, "stop() failed"),
        Err(_) => panic!("stop() wedged with the source parked mid-push"),
    }
    assert_worker_alive(&harness, "after stopping a parked source");
    scenario.unregister();
}

/// A new load replaces media whose source is parked mid-push. The old input's
/// teardown must be bounded and the new item must play.
#[test]
fn load_replaces_stalled_media() {
    init();
    let stalled = ScenarioBuilder::new("swapstall")
        .stream(StreamSpec::video("video_0").with_fault(Fault::StallAt {
            buffer_index: 4,
            sync_point: "midpush".to_owned(),
        }))
        .audio("audio_0")
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let replacement = ScenarioBuilder::new("swapnext")
        .video("video_0")
        .audio("audio_0")
        .duration(CLIP)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&stalled.uri());
    stalled
        .timeline()
        .on_sync_point_arrival("midpush")
        .expect("the video push parks");

    // Bounded BY the claim, not merely measured against it. The old shape
    // waited under EVENT_TIMEOUT (40 s) and then asserted `swap <=
    // TEARDOWN_BOUND` (15 s) after the fact, so the assertion could only fire
    // in the 15-40 s window: a genuinely wedged teardown blew the 40 s wait and
    // was reported as "timed out waiting for Loaded", never as a breach of the
    // teardown bound. Now the wait itself carries the bound.
    // Measured at 11-13 ms, so the 15 s bound has a factor of ~1000 in hand: it
    // is a wedge detector, not a performance budget, and is deliberately left
    // at the shared TEARDOWN_BOUND that names the field failure it replaces.
    let swap = harness.load_and_play_within(&replacement.uri(), TEARDOWN_BOUND);
    assert!(
        swap <= TEARDOWN_BOUND,
        "replacing a parked source took {swap:?} to load"
    );
    assert_worker_alive(&harness, "after the load swap");

    // The replacement plays out on its own, which also proves the parked input is
    // gone: a surviving one would still hold decodebin3's slots.
    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let audio = harness.audio();
    assert!(
        audio.buffer_count() > 0,
        "the replacement produced no audio: {audio:?}"
    );
    // Only the fresh audio log is swept: the caller-owned video sink carries the
    // first item's teardown, which legally ends mid-flush.
    assert!(
        wait_quiescent(&[("audio", &audio)], QUIESCENT_SETTLE, EVENT_TIMEOUT),
        "the replacement's audio sink never went quiescent"
    );
    check_all_named(&[("audio", &audio)]).expect("the replacement's audio sequence");

    stalled.unregister();
    replacement.unregister();
}

/// A video decoder that declares (and spends) 150 ms per frame. 5 fps leaves a
/// 200 ms frame budget, so the decode path keeps up and the knob costs latency
/// rather than dropped frames (the video sink runs with QoS on).
#[test]
fn decoder_latency_smoke() {
    init();
    let latency = gst::ClockTime::from_mseconds(150);
    let scenario = ScenarioBuilder::new("declatency")
        .stream(video_at("video_0", 5).with_decoder(DecoderKnobs {
            latency: Some(latency),
            ..DecoderKnobs::default()
        }))
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(2))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let timeline = scenario.timeline();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());

    // Renders, not just prerolls.
    timeline
        .after_buffers(&harness.video, 3)
        .expect("video frames render through the slow decoder");
    timeline
        .after_buffers(&harness.audio(), 3)
        .expect("audio renders alongside");

    // PRECONDITION, not the property under test: this asserts the HARNESS's own
    // knob is in effect, so that the frame-count assertion below is known to have
    // run against a genuinely slow decoder rather than a knob that silently did
    // nothing. It says nothing about fcastplaybin.
    //
    // The knob was resolved through the REAL autoplug chain: ftestdec parses the
    // scenario key out of its sink-pad stream-id, which therefore survived
    // urisourcebin, parsebin and decodebin3 prefixing. Asked of the decoder and
    // not of the pipeline: GstBin only folds the latencies of LIVE children, so a
    // non-live pipeline answers zero by design.
    let decoder = find_element(&harness, "ftestvdec").expect("ftestvdec was autoplugged");
    let mut query = gst::query::Latency::new();
    assert!(
        decoder
            .static_pad("src")
            .expect("the decoder has a src pad")
            .query(&mut query),
        "the decoder answered no latency query"
    );
    let (_, min, _) = query.result();
    assert_eq!(
        min, latency,
        "the decoder advertised {min} instead of the knob's {latency}"
    );

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    // The claim in this test's own doc comment, which nothing used to check:
    // the knob costs LATENCY, not frames. 2 s at 5 fps is ten frames and the
    // 150 ms decode fits inside the 200 ms budget, so an intact decode path
    // delivers essentially all of them.
    //
    // Two guards on different axes, because the video sink runs with QoS on and
    // a count alone cannot be both tight and stable: measured 10/10 idle and
    // 9/10 under full-suite contention (one QoS drop), so the count floor is set
    // at 9 with that evidence rather than by taste. A decode path that stopped
    // keeping up does not drop one frame in ten, it drops most of them.
    let frames = harness.video.buffer_count();
    assert!(
        frames >= 9,
        "the slow decoder cost frames instead of latency: only {frames} of 10 reached \
         the sink: {:?}",
        harness.video
    );
    // The axis a QoS drop cannot fake: playback has to have TRAVERSED the clip.
    // Frames sit at 0, 200, .. 1800 ms, so the last one delivered must be near
    // the end however many were dropped along the way. A decoder that fell
    // behind and had playback cut short lands far below this.
    let last_pts = harness
        .video
        .snapshot()
        .iter()
        .filter_map(|entry| entry.pts())
        .max();
    let span_floor = gst::ClockTime::from_mseconds(1600);
    assert!(
        last_pts.is_some_and(|pts| pts >= span_floor),
        "the last video frame to reach the sink was at {last_pts:?}, short of {span_floor} \
         in a 2 s clip: the slow decoder cut the item short instead of merely adding latency"
    );
    assert_eq!(
        harness.audio().buffer_count(),
        100,
        "audio lost buffers alongside the slow video decoder: {:?}",
        harness.audio()
    );
    harness.check_all("decoder latency");
    scenario.unregister();
}

/// A text-only `ftest://` scenario attached as an EXTERNAL subtitle source. The
/// assertion is that text DATA reaches subtitleoverlay: 16x16 frames make a
/// pixel-level cue check meaningless, so the real-media suite keeps that job.
#[test]
fn external_subtitle_scenario_reaches_the_overlay() {
    init();
    let main = ScenarioBuilder::new("extmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(6))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let subs = ScenarioBuilder::new("extsubs")
        .text("text_0", cues(10, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(6))
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());

    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching the external scenario");
    // The stream has to materialize before it can be selected by id.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let sid = loop {
        if let Some(sid) = harness.playbin.subtitle_stream_ids(id).into_iter().next() {
            break sid;
        }
        assert!(
            Instant::now() < deadline,
            "the external stream never materialized"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        sid.contains("text_0"),
        "the external stream id {sid} is not the scenario's text stream"
    );

    // Count text buffers crossing into the overlay.
    let text_seen = Arc::new(Mutex::new(0usize));
    let counter = text_seen.clone();
    harness
        .overlay_subtitle_pad()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            *counter.lock().expect("text counter") += 1;
            gst::PadProbeReturn::Ok
        });

    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());
    // The slowest wait here: a mid-load selection stalls behind the start
    // dance's streaming threads, and under full-suite load its confirmation
    // can outlast the standard bound.
    let deadline = Instant::now() + EVENT_TIMEOUT * 2;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(sid.clone())) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the external selection never confirmed; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "external scenario subtitle");

    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if *text_seen.lock().expect("text counter") > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no cue reached subtitleoverlay; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_worker_alive(&harness, "with an external scenario selected");
    main.unregister();
    subs.unregister();
}

/// The timeline a pad renders against: the stream position whose running time
/// is zero, read off the pad's sticky segment exactly like the crate's
/// `overlay_timeline`. Cues sync against video iff both pads report the same
/// origin.
fn segment_origin(pad: &gst::Pad) -> Option<gst::ClockTime> {
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
    let rate = segment.rate();
    let start = segment.start().unwrap_or(gst::ClockTime::ZERO);
    let base =
        (segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as f64 * rate.abs()) as u64;
    Some(gst::ClockTime::from_nseconds(
        start.nseconds().saturating_sub(base),
    ))
}

/// The core of the seeked-start external-subtitle scenarios: load `main_uri`
/// with a non-zero start position and attach `sub_uri` WHILE the item is
/// still starting (the receiver's Load(time=X) + AddSubtitleSource sequence).
/// Asserts the external input ends up on the sought timeline: a cue at
/// stream-time T renders when video is at T, not at T plus the start offset.
///
/// The attach lands right after the collection is announced, which is inside
/// the load job, before the start seek is applied: the earliest instant an
/// attach can survive the load's input reset.
fn assert_seeked_start_attach_shares_the_timeline(
    main_uri: &str,
    sub_uri: &str,
    start: gst::ClockTime,
) {
    let harness = Harness::new();
    // The load's own duration is the discriminator the degradation check below
    // needs: `apply_start_seek` only skips the seek on a seekable source when
    // its preroll wait hit PREROLL_TIMEOUT, so a FAST load that nevertheless
    // starts at zero is a bug and not an overloaded box.
    let load_started = Instant::now();
    harness.playbin.load_async(
        MediaInput::Uri(main_uri.to_owned()),
        StartPoint::Seek {
            position: start,
            rate: 1.0,
        },
    );
    harness.wait_for("the collection", |event| {
        matches!(event, PlaybinEvent::StreamCollection(_))
    });

    // Mid-load: the worker is still inside the load job (preroll, then the
    // start seek). The receiver's attach runs on its own thread the same way.
    let id = harness
        .playbin
        .attach_subtitle(sub_uri)
        .expect("attaching during the load");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let sid = loop {
        if let Some(sid) = harness.playbin.subtitle_stream_ids(id).into_iter().next() {
            break sid;
        }
        assert!(
            Instant::now() < deadline,
            "the external stream never materialized"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    };
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());

    // The load finishes behind the attach; Loaded may already be in the log
    // by the time the selection settles, so scan rather than wait.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        let loaded = harness
            .log
            .borrow()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::Loaded { .. }));
        if loaded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the load never finished; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let load_elapsed = load_started.elapsed();
    harness.playbin.play().expect("play");
    harness.wait_for("settled PLAYING", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });

    // The selection has to confirm and the branch has to reach the overlay
    // before the timeline can be compared.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(sid.clone())) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the external selection never confirmed; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "seeked-start external subtitle");

    // Sticky segments only exist once data flowed: wait for a cue to cross
    // into the overlay.
    let text_pad = harness.overlay_subtitle_pad();
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while text_pad.sticky_event::<gst::event::Segment>(0).is_none() {
        assert!(
            Instant::now() < deadline,
            "no text segment reached the overlay; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    // The property under test: the external input renders against the SAME
    // timeline origin as the video. Bounded: the replay that aligns the
    // input is asynchronous, so the wrong origin may sit on the pad briefly
    // before the corrected segment replaces it.
    let video_pad = harness
        .overlay()
        .static_pad("video_sink")
        .expect("subtitleoverlay has a video_sink pad");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let video_origin = segment_origin(&video_pad);
        let text_origin = segment_origin(&text_pad);
        if video_origin == Some(start) && text_origin == Some(start) {
            break;
        }
        if Instant::now() >= deadline {
            // The start seek's preroll wait is bounded (PREROLL_TIMEOUT):
            // on an overloaded box the crate degrades BY DESIGN to playing
            // from zero. Both origins land on zero together then, which
            // still satisfies the shared-timeline property; only a text
            // origin that DIVERGES from the video is the bug.
            //
            // But that forgiveness is exactly the shape of the bug it would
            // hide: an item that never reaches the sought timeline at all
            // also parks both origins on zero, and this branch used to accept
            // it unconditionally. Verified by mutation: making
            // `apply_start_seek` return without ever seeking left BOTH
            // seeked-start tests GREEN (they just burned the 40 s bound and
            // took this branch).
            //
            // The load's own duration separates the two cases. The seek is
            // only skipped on a seekable source when the preroll wait
            // expired, so a load that finished well inside PREROLL_TIMEOUT
            // and STILL renders against zero did not degrade under load.
            let degraded = load_elapsed >= PREROLL_TIMEOUT;
            if degraded && video_origin.is_some() && video_origin == text_origin {
                eprintln!(
                    "note: the start seek degraded under load (load took \
                     {load_elapsed:?} >= {PREROLL_TIMEOUT:?}, both origins \
                     {video_origin:?}), asserting alignment only"
                );
                break;
            }
            assert!(
                degraded || video_origin == Some(start),
                "the item's VIDEO never reached the sought timeline: the origin at \
                 subtitleoverlay's video_sink is {video_origin:?}, expected \
                 {start:?}. The load finished in {load_elapsed:?}, far inside the \
                 {PREROLL_TIMEOUT:?} preroll bound the crate is allowed to degrade \
                 on, so this is not the by-design degradation. NOTE: the start seek \
                 IS issued and accepted here, so the loss is in its DELIVERY: the \
                 segment that reaches the overlay is the pre-seek one. The seek is \
                 delivered at the input (`Inner::seek_main_input`) exactly so that \
                 it cannot depend on which chains have joined yet; a broadcast \
                 `pipeline.seek()` reaches only the sink children present at that \
                 instant, and a video chain that joins afterwards then renders from \
                 the PRE-SEEK segment. Everything the crate aligns off \
                 `overlay_timeline` (external subtitles above all) is then shifted \
                 by {start:?} for the whole item. log: {:#?}",
                harness.log.borrow()
            );
            panic!(
                "the external subtitle never joined the sought timeline: \
                 video origin {video_origin:?}, text origin {text_origin:?}, \
                 start {start:?}; log: {:#?}",
                harness.log.borrow()
            );
        }
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_worker_alive(&harness, "after the seeked-start external attach");
    harness.shutdown();
}

/// Cues with a distinguishing payload prefix, so a probe can tell WHICH
/// external input a rendered cue came from.
fn prefixed_cues(prefix: &str, count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{prefix}{index:02}"))
        })
        .collect()
}

/// Records every text payload crossing the overlay's subtitle input together
/// with the origin of the segment governing it at that instant. The pad is
/// the overlay's own static sink, so the tap survives branch relinks.
type TextTap = Arc<Mutex<Vec<(String, Option<gst::ClockTime>)>>>;
fn tap_overlay_text(harness: &Harness) -> TextTap {
    let seen: TextTap = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    harness
        .overlay_subtitle_pad()
        .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let payload = buffer
                    .map_readable()
                    .map(|map| String::from_utf8_lossy(map.as_slice()).into_owned())
                    .unwrap_or_default();
                recorder
                    .lock()
                    .expect("text tap")
                    .push((payload, segment_origin(pad)));
            }
            gst::PadProbeReturn::Ok
        });
    seen
}

/// Payloads recorded by [`tap_overlay_text`] that start with `prefix`.
fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> Vec<(String, Option<gst::ClockTime>)> {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|(payload, _)| payload.starts_with(prefix))
        .cloned()
        .collect()
}

/// Request the subtitle slot onto `id` and pump until the selection confirms
/// with `sid`.
fn select_and_confirm(harness: &Harness, id: fcastplaybin::ExternalSubId, sid: &str) {
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(sid.to_owned())) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the selection of {sid} never confirmed; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Attach `uri`, wait for its stream to materialize, select it and wait for
/// the confirmation. Returns the id and the materialized stream id.
fn attach_and_select(harness: &Harness, uri: &str) -> (fcastplaybin::ExternalSubId, String) {
    let id = harness
        .playbin
        .attach_subtitle(uri)
        .expect("attaching the external input");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let sid = loop {
        if let Some(sid) = harness.playbin.subtitle_stream_ids(id).into_iter().next() {
            break sid;
        }
        assert!(
            Instant::now() < deadline,
            "the external stream never materialized"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    };
    select_and_confirm(harness, id, &sid);
    (id, sid)
}

/// Wait (pumping) until a cue with `prefix` beyond `already` recordings has
/// crossed into the overlay, and return the new recordings.
fn wait_for_prefixed_cue(
    harness: &Harness,
    tap: &TextTap,
    prefix: &str,
    already: usize,
    what: &str,
) -> Vec<(String, Option<gst::ClockTime>)> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let seen = tapped_with_prefix(tap, prefix);
        if seen.len() > already {
            return seen.into_iter().skip(already).collect();
        }
        assert!(
            Instant::now() < deadline,
            "no {prefix} cue reached the overlay ({what}); log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Switching between two external subtitles must put the NEWLY selected one
/// on the video's timeline. decodebin3 swaps the stream on the already-linked
/// text output pad (no unlink, no join), so the crate cannot rely on the
/// join-time replay alone: without a selection-time replay the new input
/// plays from the start of its file and every cue renders shifted by the
/// video's origin.
///
/// The 5s user seek is what makes the misalignment observable: it moves the
/// video's origin away from zero, so a cue rendered against an unaligned
/// [0, ..) text segment carries a provably wrong origin.
#[test]
fn switching_external_subtitles_realigns_the_new_input() {
    init();
    let origin = gst::ClockTime::from_seconds(5);
    let main = ScenarioBuilder::new("swrealignmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(15))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let sub_a = ScenarioBuilder::new("swrealigna")
        .text("text_0", prefixed_cues("AAA", 30, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(15))
        .pacing(PACING)
        .register();
    let sub_b = ScenarioBuilder::new("swrealignb")
        .text("text_0", prefixed_cues("BBB", 30, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(15))
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    // Installed after the load: the overlay only joins the pipeline once the
    // item's video routes.
    let tap = tap_overlay_text(&harness);

    // Move the timeline origin away from zero, like the field's mid-stream
    // position. A playing-state seek parks waiting for the caller's state
    // machine to re-drive it, so do what that machine does: seek from a
    // settled PAUSED, resume after.
    harness.playbin.pause().expect("pause for the seek");
    harness.paused.set(true);
    harness.wait_for("settled PAUSED", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Paused,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    harness
        .playbin
        .seek_async(fcastplaybin::state_machine::Seek {
            position: Some(origin),
            rate: None,
        });
    let video_pad = harness
        .overlay()
        .static_pad("video_sink")
        .expect("subtitleoverlay has a video_sink pad");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while segment_origin(&video_pad) != Some(origin) {
        assert!(
            Instant::now() < deadline,
            "the video origin never moved to {origin}; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.playbin.play().expect("resume after the seek");
    harness.paused.set(false);
    harness.wait_for("settled PLAYING after the seek", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });

    // Baseline: the first external joins fresh, and the join-time replay
    // aligns it.
    let (_id_a, _sid_a) = attach_and_select(&harness, &sub_a.uri());
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "first external subtitle");
    for (payload, cue_origin) in wait_for_prefixed_cue(&harness, &tap, "AAA", 0, "baseline") {
        assert_eq!(
            cue_origin,
            Some(origin),
            "baseline cue {payload} rendered against the wrong timeline"
        );
    }

    // The switch under test: the second external swaps onto the SAME linked
    // branch.
    let (_id_b, _sid_b) = attach_and_select(&harness, &sub_b.uri());
    let fresh = wait_for_prefixed_cue(&harness, &tap, "BBB", 0, "after the switch");
    for (payload, cue_origin) in
        fresh.iter().chain(tapped_with_prefix(&tap, "BBB").iter())
    {
        assert_eq!(
            *cue_origin,
            Some(origin),
            "switched-to cue {payload} rendered against the wrong timeline \
             (expected origin {origin}); the new input was never replayed"
        );
    }

    assert_worker_alive(&harness, "after the external-to-external switch");
    harness.shutdown();
    main.unregister();
    sub_a.unregister();
    sub_b.unregister();
}

/// Re-selecting an external subtitle that already played out (or whose task
/// died deselected) must bring its cues back WITHOUT a user seek. The input
/// is spent by then: only a replay restarts it, and the pad-reuse swap emits
/// no join to hang that replay on.
#[test]
fn reactivating_an_external_subtitle_replays_it() {
    init();
    let main = ScenarioBuilder::new("reactmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    // Unpaced, so A is free to run ahead. NOTE: it does NOT finish before the
    // switch away (measured: 2 of its 20 cues reach the overlay), so the
    // reactivation below is asserted by WHICH cues come back rather than by
    // assuming a spent input. See the assertion for why that distinction is the
    // whole test.
    let sub_a = ScenarioBuilder::new("reacta")
        .text("text_0", prefixed_cues("AAA", 20, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(PACING)
        .register();
    let sub_b = ScenarioBuilder::new("reactb")
        .text("text_0", prefixed_cues("BBB", 20, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    // Installed after the load: the overlay only joins the pipeline once the
    // item's video routes.
    let tap = tap_overlay_text(&harness);

    let (id_a, sid_a) = attach_and_select(&harness, &sub_a.uri());
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "first selection of A");
    wait_for_prefixed_cue(&harness, &tap, "AAA", 0, "first selection of A");

    // Switch away, then back. No seek anywhere in between.
    let (_id_b, _sid_b) = attach_and_select(&harness, &sub_b.uri());
    wait_for_prefixed_cue(&harness, &tap, "BBB", 0, "the switch to B");

    let before: Vec<String> = tapped_with_prefix(&tap, "AAA")
        .into_iter()
        .map(|(payload, _)| payload)
        .collect();
    let seen_before = before.len();
    assert!(
        seen_before > 0,
        "A delivered nothing before the switch away, so there is no reactivation to test"
    );
    select_and_confirm(&harness, id_a, &sid_a);
    let after = wait_for_prefixed_cue(&harness, &tap, "AAA", seen_before, "the reactivation of A");

    // The premise this test never checked. Its doc used to claim A was SPENT by
    // now ("A pushes its whole schedule and finishes"), which is measurably
    // false: A delivers only a couple of cues before the switch away (2 of 20
    // when measured). So "a cue with the AAA prefix arrived again" was satisfied
    // by an input that had simply never finished, and a crate that merely
    // RESUMED a paused input passed unchanged.
    //
    // What actually distinguishes a replay from a resume is WHICH cues come
    // back. A resume can only deliver cues A had not reached yet; a replay
    // restarts the input and re-delivers ones already seen. Measured: the
    // reactivation returns AAA00/AAA01, exactly the payloads delivered before.
    let replayed: Vec<&String> = after
        .iter()
        .map(|(payload, _)| payload)
        .filter(|payload| before.contains(payload))
        .collect();
    assert!(
        !replayed.is_empty(),
        "the reactivation delivered only cues A had never reached ({:?} after, {before:?} \
         before): the input was RESUMED, not replayed, so nothing restarted it and a spent \
         input would have stayed silent",
        after.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );

    // The reactivation must not have been reported as a failure.
    harness.drain_events();
    assert!(
        !harness
            .log
            .borrow()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::ExternalSubtitleFailed { .. })),
        "the reactivated input was reported failed; log: {:#?}",
        harness.log.borrow()
    );
    assert_worker_alive(&harness, "after the reactivation");
    harness.shutdown();
    main.unregister();
    sub_a.unregister();
    sub_b.unregister();
}

/// The field variant of the reactivation: the deselected input's task DIES
/// (its push returns not-linked once decodebin3 unlinks the slot, and the
/// source gives up), classification keeps the input attached, and the
/// re-selection must replay it back to life. Realtime pacing keeps A
/// mid-push at the switch, so its death is the not-linked kind.
#[test]
fn reactivating_a_dead_external_subtitle_replays_it() {
    init();
    let main = ScenarioBuilder::new("deadreactmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(25))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let sub_a = ScenarioBuilder::new("deadreacta")
        .text("text_0", prefixed_cues("AAA", 90, gst::ClockTime::from_mseconds(250)))
        .duration(gst::ClockTime::from_seconds(25))
        .pacing(Pacing::Realtime)
        .register();
    let sub_b = ScenarioBuilder::new("deadreactb")
        .text("text_0", prefixed_cues("BBB", 90, gst::ClockTime::from_mseconds(250)))
        .duration(gst::ClockTime::from_seconds(25))
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    // Installed after the load: the overlay only joins the pipeline once the
    // item's video routes.
    let tap = tap_overlay_text(&harness);

    let (id_a, sid_a) = attach_and_select(&harness, &sub_a.uri());
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "first selection of A");
    wait_for_prefixed_cue(&harness, &tap, "AAA", 0, "first selection of A");

    let (_id_b, _sid_b) = attach_and_select(&harness, &sub_b.uri());
    wait_for_prefixed_cue(&harness, &tap, "BBB", 0, "the switch to B");

    // A's next push lands in the unlinked slot; the source retries for its
    // bound (2s) and posts basesrc's not-linked death, which the crate
    // classifies as recoverable. Pump past the bound so the death has
    // provably landed before the reactivation.
    let dead_by = Instant::now() + Duration::from_secs(3);
    while Instant::now() < dead_by {
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    harness.drain_events();
    assert!(
        !harness
            .log
            .borrow()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::ExternalSubtitleFailed { .. })),
        "the not-linked death was misclassified as fatal; log: {:#?}",
        harness.log.borrow()
    );

    let seen_before = tapped_with_prefix(&tap, "AAA").len();
    select_and_confirm(&harness, id_a, &sid_a);
    wait_for_prefixed_cue(&harness, &tap, "AAA", seen_before, "reactivating the dead input");

    assert_worker_alive(&harness, "after reactivating the dead input");
    harness.shutdown();
    main.unregister();
    sub_a.unregister();
    sub_b.unregister();
}

/// Dropping the playbin while an external subtitle is attached but never
/// selected must complete. The held input's first push parks inside the
/// hold-until-selected block probe OWNING that pad's stream lock, and a
/// teardown that forgets to release the probe deadlocks its input NULL on
/// that lock (the drop-path twin of `remove_input`'s probe release).
#[test]
fn dropping_the_playbin_with_a_held_external_is_bounded() {
    init();
    let main = ScenarioBuilder::new("dropheldmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(10))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let subs = ScenarioBuilder::new("dropheldsubs")
        .text("text_0", prefixed_cues("HLD", 10, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(10))
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching the held external");
    // Wait for the stream to materialize: by then the input pushed its
    // sticky events and its first buffer is parked in the hold probe.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while harness.playbin.subtitle_stream_ids(id).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the held external never materialized"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    // Never selected: the hold stays. Drop on a helper thread so a wedged
    // teardown fails the test instead of hanging the suite.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(harness);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(TEARDOWN_BOUND).is_ok(),
        "dropping the playbin with a held external subtitle wedged"
    );
    main.unregister();
    subs.unregister();
}

/// The seeked-start attach over ftest media end to end: the external text is
/// served by ftestsrc, whose source accepts the crate's aligning seek.
#[test]
fn external_subtitle_attached_during_seeked_start_shares_the_timeline() {
    init();
    let start = gst::ClockTime::from_seconds(2);
    let main = ScenarioBuilder::new("seekstartmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(10))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();
    let subs = ScenarioBuilder::new("seekstartsubs")
        .text("text_0", cues(20, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(10))
        .pacing(PACING)
        .register();

    assert_seeked_start_attach_shares_the_timeline(&main.uri(), &subs.uri(), start);
    main.unregister();
    subs.unregister();
}

/// The seeked-start attach through the REAL external chain the receiver
/// ships: an .srt file parsed by filesrc -> typefind -> rssubparse (the Rust
/// subparse, ranked over the C one exactly like receiver-core ranks it). The
/// crate's aligning seek lands on the parser here, so this is the
/// configuration where a parser without the C's bytes-seek fallback pins the
/// subtitles to the wrong timeline.
#[test]
fn external_srt_attached_during_seeked_start_shares_the_timeline() {
    init();
    {
        // The receiver's rank swap (see receiver-core/src/gstreamer.rs): the
        // Rust parsers own every subtitle stream, the C ones stay registered
        // at NONE. Process-global, applied once.
        static SWAP: std::sync::Once = std::sync::Once::new();
        SWAP.call_once(|| {
            gstrssubparse::plugin_register_static().expect("registering rssubparse");
            let registry = gst::Registry::get();
            for c_name in ["subparse", "ssaparse"] {
                if let Some(feature) = registry.lookup_feature(c_name) {
                    feature.set_rank(gst::Rank::NONE);
                }
            }
            for rs_name in ["rssubparse", "rsssaparse"] {
                registry
                    .lookup_feature(rs_name)
                    .expect("registered by gstrssubparse above")
                    .set_rank(gst::Rank::PRIMARY);
            }
        });
    }

    let start = gst::ClockTime::from_seconds(2);
    let main = ScenarioBuilder::new("seeksrtmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(10))
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();

    // Cues every 400 ms across the whole clip, like the ftest variant's.
    let mut srt = String::new();
    for index in 0..20u32 {
        let begin = 400 * (index + 1) as u64;
        let fmt = |ms: u64| {
            format!(
                "{:02}:{:02}:{:02},{:03}",
                ms / 3_600_000,
                (ms / 60_000) % 60,
                (ms / 1000) % 60,
                ms % 1000
            )
        };
        srt.push_str(&format!(
            "{}\n{} --> {}\nCUE{index:02}\n\n",
            index + 1,
            fmt(begin),
            fmt(begin + 200)
        ));
    }
    let path = std::env::temp_dir().join(format!(
        "fcastplaybin-seekstart-{}.srt",
        std::process::id()
    ));
    std::fs::write(&path, srt).expect("writing the srt");
    let sub_uri = format!("file://{}", path.display());

    // Owned captures only: the handle itself is not unwind-safe, and the
    // cleanup below must run whether or not the assertion panics.
    let main_uri = main.uri();
    let outcome = std::panic::catch_unwind(move || {
        assert_seeked_start_attach_shares_the_timeline(&main_uri, &sub_uri, start);
    });
    let _ = std::fs::remove_file(&path);
    main.unregister();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The start seek has to reach EVERY elementary stream of the item, not just
/// the ones that happen to own a sink in the pipeline at the instant it is
/// issued. The item's OWN text stream is that property with the race taken
/// out: a seek is an upstream event, so a `pipeline.seek()` reaches only the
/// bin's SINK-flagged children, and text is never one of them. While parked it
/// drains into a `fakesink` whose `GstElementFlags::SINK` the crate clears on
/// purpose (`Inner::park_stream`), and once selected it hangs off
/// subtitleoverlay's `subtitle_sink`, which no upstream event travelling up
/// from the video sink ever crosses. So a broadcast start seek misses the text
/// branch EVERY time, not just under load: the item's own subtitles then render
/// against stream zero while the video renders against the start position, and
/// every cue is `start` early for the item's whole length.
///
/// This is the deterministic twin of the two seeked-start external-subtitle
/// scenarios above, which need the video chain to lose the same race (measured
/// at 4/20 parallel-suite rounds) before they see it.
#[test]
fn seeked_start_puts_the_items_own_text_on_the_sought_timeline() {
    init();
    let start = gst::ClockTime::from_seconds(2);
    let main = ScenarioBuilder::new("seekstartinternal")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(20, gst::ClockTime::from_mseconds(400)))
        .duration(gst::ClockTime::from_seconds(10))
        .bytes_per_buffer(64)
        // Realtime rather than the suite's `PACING`: an unpaced ftestsrc pushes
        // the whole text schedule at once and it drains into the parking sink
        // before the selection links the branch (see the note in
        // `smoke_full_pipeline`). Sticky events are re-sent on the NEXT push,
        // so a text stream that finished before the link leaves the overlay's
        // subtitle_sink with no segment at all and this test measuring nothing.
        // Paced, cues keep arriving while the branch is being built.
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    harness.drain_events();
    harness.playbin.load_async(
        MediaInput::Uri(main.uri()),
        StartPoint::Seek {
            position: start,
            rate: 1.0,
        },
    );
    harness.wait_for("Loaded", |event| {
        matches!(event, PlaybinEvent::Loaded { .. })
    });
    harness.playbin.play().expect("play");
    harness.wait_for("settled PLAYING", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });

    // Text auto-select is off (see `selection.rs`), so the item's own text
    // only joins the overlay once asked for. Polled rather than read once:
    // `item_stream_id` answers off the LATEST collection, and a partial one
    // (urisourcebin's, before decodebin3's full set) can be the latest for a
    // moment.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let text_sid = loop {
        harness.drain_events();
        if let Some(sid) = harness.item_stream_id(gst::StreamType::TEXT) {
            break sid;
        }
        assert!(
            Instant::now() < deadline,
            "no collection ever advertised the item's text stream; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    };
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(text_sid.clone())));
    harness.playbin.pump_selection(harness.gate());
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(text_sid.clone())) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the item's own text selection never confirmed; log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.wait_subtitle_branch(EVENT_TIMEOUT, "the item's own text on a seeked start");

    let text_pad = harness.overlay_subtitle_pad();
    let video_pad = harness
        .overlay()
        .static_pad("video_sink")
        .expect("subtitleoverlay has a video_sink pad");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let text_origin = segment_origin(&text_pad);
        let video_origin = segment_origin(&video_pad);
        if text_origin == Some(start) && video_origin == Some(start) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the seeked start left the item's own streams on different timelines: \
             the origin at subtitleoverlay's video_sink is {video_origin:?} and at \
             its subtitle_sink {text_origin:?}, both expected {start:?}. A start seek \
             that only reaches the SINK-flagged children of the pipeline never gets \
             to the text branch at all. log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_worker_alive(&harness, "after a seeked start with the item's own text");
    harness.shutdown();
    main.unregister();
}
