//! Coverage for the diagnostics surface and the events receiver-core reads
//! but nothing tested: `stream_io_stats`, `source_summaries`, `dump_dot`,
//! `has_external_subtitles`, the buffering queries, the `Tags` event, the
//! seqnum on `StreamsSelected`, and the "call at most once" contract of
//! `set_subtitle_consumer`.
//!
//! Same harness shape as `tests/scenarios.rs`: real fcastplaybin over
//! `ftest://` media (or a real file where the surface needs one), pumped
//! waits, log-first matching.

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks,
    StartPoint, SubtitleFeedItem, TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

mod support;
#[path = "support/text_arm.rs"]
mod text_arm;

/// Generous bound for anything the pipeline has to reach, sized for a box
/// running the whole suite in parallel (see `tests/scenarios.rs`).
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Bound for a stop or shutdown.
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// A tagged real-media file (ID3): the `Tags` event needs one.
const SAMPLE_MP3: &str =
    "/home/merb/sub/Programming/fcast_root/fcast-sample-media/audio/Court_House_Blues_Take_1.mp3";

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        // The receiver's part of the pipeline, registered by the application.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// A unique temp path for generated fixtures.
fn temp_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fcastplaybin-diag-{}-{tag}-{n}",
        std::process::id()
    ))
}

/// Write a small WebVTT file with `count` cues 500 ms apart and return its
/// `file://` uri.
fn write_vtt(tag: &str, count: u32) -> String {
    let path = temp_path(tag).with_extension("vtt");
    let mut body = String::from("WEBVTT\n\n");
    for index in 0..count {
        let start_ms = 500 * u64::from(index) + 500;
        let end_ms = start_ms + 400;
        let fmt = |ms: u64| format!("00:{:02}.{:03}", ms / 1000, ms % 1000);
        body.push_str(&format!(
            "{} --> {}\nDIAG{index:02}\n\n",
            fmt(start_ms),
            fmt(end_ms)
        ));
    }
    std::fs::write(&path, body).expect("writing the vtt fixture");
    format!("file://{}", path.display())
}

/// Encode `seconds` of silent MP3, the `make_mp3_file` idiom from
/// `src/pipeline_tests.rs`.
fn make_mp3_file(path: &Path, seconds: f64) {
    let num_buffers = (seconds * 44100.0 / 1024.0).round() as i32;
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", false)
        .property_from_str("wave", "silence")
        .build()
        .unwrap();
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let enc = gst::ElementFactory::make("lamemp3enc").build().unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, &conv, &enc, &sink]).unwrap();
    gst::Element::link_many([&src, &conv, &enc, &sink]).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(10)) {
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(err) => panic!("mp3 encode failed: {err:?}"),
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// Dense cues for external text scenarios.
fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("CUE{index:02}"))
        })
        .collect()
}

/// The scenarios.rs harness, trimmed to this file's needs: no seeks are
/// driven here, so no transport redrive. The log keeps each event's
/// generation because the Tags test asserts on it.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<(PlaybinEvent, u64)>>,
    paused: Cell<bool>,
}

impl Harness {
    fn new() -> Self {
        Self::build(true)
    }

    /// Without the shared text-arm consumer, for the test whose subject IS
    /// `set_subtitle_consumer` (arming would install consumer number zero
    /// and shift the replacement under test by one).
    fn new_unarmed() -> Self {
        Self::build(false)
    }

    fn build(arm: bool) -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        if arm {
            // Established before anything can flow, see support/text_arm.rs.
            text_arm::arm(&playbin);
        }
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin,
            events,
            log: RefCell::new(Vec::new()),
            paused: Cell::new(false),
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
        while let Ok(entry) = self.events.try_recv() {
            self.log.borrow_mut().push(entry);
        }
    }

    /// Wait until `pred` matches a newly received event, pumping between
    /// polls. Panics with the log on timeout or pipeline error.
    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent, u64) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}; log: {:#?}",
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
                    let hit = pred(&event, generation);
                    self.log.borrow_mut().push((event, generation));
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

    /// Wait until `pred` matches ANY logged event, past or new. For events
    /// that can arrive during an earlier wait (Tags rides the preroll).
    fn wait_logged(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent, u64) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.drain_events();
            {
                let log = self.log.borrow();
                if log
                    .iter()
                    .any(|(event, generation)| pred(event, *generation))
                {
                    return;
                }
                if let Some((PlaybinEvent::Error { error, .. }, _)) = log
                    .iter()
                    .find(|(event, _)| matches!(event, PlaybinEvent::Error { .. }))
                {
                    panic!("pipeline error while waiting for {what}: {error} (log: {log:#?})");
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Pump and drain for `span`, for observation windows and byte growth.
    fn pump_for(&self, span: Duration) {
        let end = Instant::now() + span;
        while Instant::now() < end {
            self.drain_events();
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Load `uri`, start playback, wait for the settled PLAYING. Returns the
    /// load's generation.
    fn load_and_play(&self, uri: &str) -> u64 {
        self.drain_events();
        let generation = self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event, _| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        self.playbin.play().expect("play");
        self.wait_for("settled PLAYING", |event, _| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        });
        generation
    }

    /// The latest `StreamsSelected` subtitle slot in the log, if any.
    fn last_selected_subtitle(&self) -> Option<Option<String>> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|(event, _)| match event {
                PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.clone()),
                _ => None,
            })
    }

    /// Attach `uri` and wait for its stream id to materialize.
    fn attach_and_materialize(&self, uri: &str) -> (ExternalSubId, String) {
        let id = self
            .playbin
            .attach_subtitle(uri)
            .expect("attaching the external input");
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let sid = loop {
            if let Some(sid) = self.playbin.subtitle_stream_ids(id).into_iter().next() {
                break sid;
            }
            assert!(
                Instant::now() < deadline,
                "the external stream never materialized; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        };
        (id, sid)
    }

    /// Select external `id` on the subtitle slot and pump until `sid`
    /// confirms.
    fn select_and_confirm(&self, id: ExternalSubId, sid: &str) {
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
        self.playbin.pump_selection(self.gate());
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.drain_events();
            if self.last_selected_subtitle() == Some(Some(sid.to_owned())) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the selection of {sid} never confirmed; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Blocks until the worker tore the pipeline down.
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

/// A barrier round-trip proves the worker is not wedged.
fn assert_worker_alive(harness: &Harness, what: &str) {
    let (tx, rx) = mpsc::channel();
    harness.playbin.barrier_async(Box::new(move || {
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

/// `stream_io_stats` must expose every live input stream with monotonic byte
/// counters, caps once data flowed, and the `external` field that lets the
/// inspector tell an attached subtitle's streams from the item's own. The
/// external attribution is the load-bearing bit nothing else checks.
#[test]
fn stream_io_stats_tags_externals() {
    init();
    let scenario = ScenarioBuilder::new("diagiostats")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(12))
        .bytes_per_buffer(64)
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());

    let sub_uri = write_vtt("iostats", 20);
    let (id, sid) = harness.attach_and_materialize(&sub_uri);
    harness.select_and_confirm(id, &sid);

    // The external's bytes flow within milliseconds of the attach (the
    // branch is unsynced), but not synchronously with the confirm.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let first = loop {
        let stats = harness.playbin.stream_io_stats();
        if stats.iter().any(|s| s.external == Some(id) && s.bytes > 0)
            && stats.iter().any(|s| s.external.is_none() && s.bytes > 0)
        {
            break stats;
        }
        assert!(
            Instant::now() < deadline,
            "stream_io_stats never showed flowing main and external streams: {stats:#?}"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    };

    // Realtime pacing keeps the main streams producing, so a second sample a
    // beat later exercises the monotonicity claim non-vacuously.
    harness.pump_for(Duration::from_millis(700));
    let second = harness.playbin.stream_io_stats();

    // The main item advertises video and audio, both untagged.
    let mains: Vec<_> = first.iter().filter(|s| s.external.is_none()).collect();
    assert!(
        mains.len() >= 2,
        "expected the item's video and audio taps, got {first:#?}"
    );
    // Exactly the attached input's entry carries its id, stamped with the
    // materialized stream id.
    let externals: Vec<_> = first.iter().filter(|s| s.external.is_some()).collect();
    assert_eq!(
        externals.len(),
        1,
        "expected exactly the vtt's tap tagged external: {first:#?}"
    );
    assert_eq!(externals[0].external, Some(id));
    assert_eq!(
        externals[0].stream_id.as_deref(),
        Some(sid.as_str()),
        "the external tap's stream id is not the materialized one"
    );

    for entry in &first {
        assert!(
            entry.bytes == 0 || entry.caps.is_some(),
            "a flowing stream must expose caps: {entry:#?}"
        );
    }
    for entry in &first {
        let Some(stream_id) = &entry.stream_id else {
            continue;
        };
        let later = second
            .iter()
            .find(|s| s.stream_id.as_ref() == Some(stream_id))
            .unwrap_or_else(|| panic!("stream {stream_id} vanished between samples: {second:#?}"));
        assert!(
            later.bytes >= entry.bytes,
            "bytes for {stream_id} went backwards: {} -> {}",
            entry.bytes,
            later.bytes
        );
        assert_eq!(
            later.external, entry.external,
            "the external tag flapped between samples for {stream_id}"
        );
    }

    assert_worker_alive(&harness, "after sampling stream_io_stats");
    harness.shutdown();
    scenario.unregister();
}

/// `source_summaries` must list every live input with its factory and uri,
/// and a detached input's entry must disappear. A stale entry means the
/// inspector lies about what is attached.
#[test]
fn source_summaries_tracks_attach_and_detach() {
    init();
    let scenario = ScenarioBuilder::new("diagsrcsum")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(20))
        .bytes_per_buffer(64)
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());

    let uri_a = write_vtt("srcsum-a", 10);
    let uri_b = write_vtt("srcsum-b", 10);
    let (id_a, _sid_a) = harness.attach_and_materialize(&uri_a);
    let (id_b, _sid_b) = harness.attach_and_materialize(&uri_b);

    let summaries = harness.playbin.source_summaries();
    assert_eq!(
        summaries.len(),
        3,
        "one main input and two externals: {summaries:#?}"
    );
    for entry in &summaries {
        assert_eq!(
            entry.factory, "urisourcebin",
            "every input here is a urisourcebin: {entry:#?}"
        );
    }
    let main = summaries
        .iter()
        .find(|s| s.external.is_none())
        .expect("the main input is listed");
    assert_eq!(main.uri.as_deref(), Some(scenario.uri().as_str()));
    for (id, uri) in [(id_a, &uri_a), (id_b, &uri_b)] {
        let entry = summaries
            .iter()
            .find(|s| s.external == Some(id))
            .unwrap_or_else(|| panic!("external {id:?} is not listed: {summaries:#?}"));
        assert_eq!(entry.uri.as_deref(), Some(uri.as_str()));
    }

    // The detach removes the input from routing synchronously, so the
    // summary must not show it again.
    harness.playbin.detach_subtitle(id_a).expect("detaching a");
    let summaries = harness.playbin.source_summaries();
    assert_eq!(
        summaries.len(),
        2,
        "the detached entry lingers: {summaries:#?}"
    );
    assert!(
        !summaries.iter().any(|s| s.external == Some(id_a)),
        "the inspector still lists detached {id_a:?}: {summaries:#?}"
    );
    assert!(
        summaries.iter().any(|s| s.external == Some(id_b)),
        "the surviving external {id_b:?} vanished with the other's detach: {summaries:#?}"
    );

    assert_worker_alive(&harness, "after the detach");
    harness.shutdown();
    scenario.unregister();
}

/// `dump_dot` must never panic, whatever the pipeline's phase: before any
/// load (no dynamic core exists yet), during playback, and after stop. With
/// a dump dir set it must also actually write the file each time.
#[test]
fn dump_dot_never_panics() {
    // Read by gst_init, so set before init(). Safe here because nextest
    // runs one process per test.
    let dot_dir = temp_path("dotdir");
    std::fs::create_dir_all(&dot_dir).expect("creating the dot dir");
    unsafe { std::env::set_var("GST_DEBUG_DUMP_DOT_DIR", &dot_dir) };
    init();

    let scenario = ScenarioBuilder::new("diagdot")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(10))
        .bytes_per_buffer(64)
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    harness.playbin.dump_dot("diag-precore");

    harness.load_and_play(&scenario.uri());
    harness.playbin.dump_dot("diag-midplay");

    harness.playbin.stop().expect("stop");
    harness.playbin.dump_dot("diag-poststop");

    let dumped = |needle: &str| {
        std::fs::read_dir(&dot_dir)
            .expect("reading the dot dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(needle))
    };
    for name in ["diag-precore", "diag-midplay", "diag-poststop"] {
        assert!(dumped(name), "no dot file was written for {name}");
    }

    assert_worker_alive(&harness, "after three dot dumps");
    harness.shutdown();
    scenario.unregister();
    let _ = std::fs::remove_dir_all(&dot_dir);
}

/// `has_external_subtitles` must answer true while an input is attached.
/// Every pre-existing assertion of this fn checks the false side only.
#[test]
fn has_external_subtitles_is_true_after_attach() {
    init();
    let scenario = ScenarioBuilder::new("diagextflag")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(15))
        .bytes_per_buffer(64)
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());
    assert!(
        !harness.playbin.has_external_subtitles(),
        "nothing is attached yet"
    );

    let sub_uri = write_vtt("extflag", 10);
    let (id, _sid) = harness.attach_and_materialize(&sub_uri);
    assert!(
        harness.playbin.has_external_subtitles(),
        "an attached external must flip the flag"
    );

    harness.playbin.detach_subtitle(id).expect("detaching");
    assert!(
        !harness.playbin.has_external_subtitles(),
        "the flag must drop with the detach"
    );

    assert_worker_alive(&harness, "after attach and detach");
    harness.shutdown();
    scenario.unregister();
}

/// The buffering surface over a real buffering element: `query_seekable` is
/// None before anything can answer and Some(true) once a seekable item
/// prerolled, `buffering_info` and `buffered_ranges` answer sane values, and
/// `buffered_ahead` reports a level from a non-appsrc arm (the graph here
/// has queue2 and multiqueue, and no appsrc at all).
#[test]
fn buffering_queries() {
    init();
    if gst::ElementFactory::find("lamemp3enc").is_none()
        || gst::ElementFactory::find("souphttpsrc").is_none()
    {
        eprintln!("skipping: lamemp3enc or souphttpsrc is not available");
        return;
    }

    let harness = Harness::new();
    // Nothing is loaded, so nothing in the pipeline can answer.
    assert_eq!(
        harness.playbin.query_seekable(),
        None,
        "the seeking query must be unanswerable before any load"
    );

    // An http source is what puts urisourcebin's queue2 (use-buffering) in
    // the graph; a file uri stays in pull mode with no buffering element.
    let media_dir = temp_path("bufmedia");
    std::fs::create_dir_all(&media_dir).expect("creating the media dir");
    make_mp3_file(&media_dir.join("media.mp3"), 30.0);
    let server = support::FileServer::serve(&media_dir);
    let url = server.url("media.mp3");

    harness.load_and_play(&url);

    // The seeking query only succeeds around preroll completion.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let seekable = loop {
        if let Some(seekable) = harness.playbin.query_seekable() {
            break seekable;
        }
        assert!(
            Instant::now() < deadline,
            "the seeking query never became answerable after preroll"
        );
        harness.pump_for(Duration::from_millis(20));
    };
    assert!(
        seekable,
        "a range-served mp3 must report seekable after preroll"
    );

    let deadline = Instant::now() + EVENT_TIMEOUT;
    let info = loop {
        if let Some(info) = harness.playbin.buffering_info() {
            break info;
        }
        assert!(
            Instant::now() < deadline,
            "buffering_info never became answerable with queue2 in the graph"
        );
        harness.pump_for(Duration::from_millis(20));
    };
    assert!(
        (0..=100).contains(&info.percent),
        "fill percent out of range: {info:#?}"
    );
    // The ranges live on `buffered_ranges`, which the scrubber polls on its
    // own cadence; `buffering_info` carries the fill state only.
    for range in harness.playbin.buffered_ranges() {
        assert!(
            0.0 <= range.start && range.start < range.stop && range.stop <= 1.0,
            "malformed buffered range: {range:?}"
        );
    }

    // No appsrc exists in this graph, so a level here proves the queue2 or
    // multiqueue arm (the appsrc arm has its own unit test).
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let ahead = loop {
        if let Some(ahead) = harness.playbin.buffered_ahead() {
            break ahead;
        }
        assert!(
            Instant::now() < deadline,
            "buffered_ahead never reported a queue level during playback"
        );
        harness.pump_for(Duration::from_millis(20));
    };
    assert!(ahead > gst::ClockTime::ZERO);

    assert_worker_alive(&harness, "after the buffering queries");
    harness.shutdown();
    let _ = std::fs::remove_dir_all(&media_dir);
}

/// A tagged real file must surface `PlaybinEvent::Tags` with a non-empty
/// taglist, stamped with the load's generation. Emitted at src/bus.rs and
/// read by receiver-core, observed by no other test.
#[test]
fn tags_event_reaches_the_caller() {
    init();
    if !Path::new(SAMPLE_MP3).exists() {
        eprintln!("skipping: sample media is not checked out at {SAMPLE_MP3}");
        return;
    }

    let harness = Harness::new();
    let generation = harness.load_and_play(&format!("file://{SAMPLE_MP3}"));

    // Tags ride the preroll, so they may already be in the log by the time
    // PLAYING settles. Scan rather than wait for a new arrival.
    let mut seen_wrong_generation = None;
    harness.wait_logged("a non-empty Tags event of this load", |event, event_gen| {
        let PlaybinEvent::Tags(tags) = event else {
            return false;
        };
        if tags.n_tags() == 0 {
            return false;
        }
        if event_gen != generation {
            seen_wrong_generation = Some(event_gen);
            return false;
        }
        true
    });
    assert_eq!(
        seen_wrong_generation, None,
        "a Tags event arrived stamped with a foreign generation (load was {generation})"
    );

    assert_worker_alive(&harness, "after receiving tags");
    harness.shutdown();
}

/// A caller-stamped seqnum on `select_streams` must come back on the
/// confirming `StreamsSelected`, which is the only way receiver-core can
/// attribute a confirmation to its request. Also pins what a selection
/// naming a stream id absent from every collection does.
#[test]
fn streams_selected_carries_the_request_seqnum() {
    init();
    // Two audio streams so the switch below CHANGES the selection.
    // decodebin3 does not confirm a selection identical to the current one.
    let scenario = ScenarioBuilder::new("diagseqnum")
        .video("video_0")
        .audio("audio_0")
        .audio("audio_1")
        .duration(gst::ClockTime::from_seconds(25))
        .bytes_per_buffer(64)
        .pacing(Pacing::Realtime)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&scenario.uri());

    // The initial selection tells which audio is on, so the request below
    // switches to the other one.
    let mut current_audio = None;
    harness.wait_logged("the initial StreamsSelected", |event, _| match event {
        PlaybinEvent::StreamsSelected { audio, .. } => {
            current_audio = audio.clone();
            true
        }
        _ => false,
    });
    let current_audio = current_audio.expect("the initial selection carries audio");
    // Captured from ONE matched collection: partial collections are posted
    // during the load, and reading video and audio off different ones races.
    let mut video_id = None;
    let mut other_audio = None;
    harness.wait_logged(
        "a collection with video and a second audio stream",
        |event, _| {
            let PlaybinEvent::StreamCollection(c) = event else {
                return false;
            };
            let video = c
                .iter()
                .find(|s| s.stream_type().contains(gst::StreamType::VIDEO))
                .and_then(|s| s.stream_id().map(|sid| sid.to_string()));
            let audio = c
                .iter()
                .filter(|s| s.stream_type().contains(gst::StreamType::AUDIO))
                .filter_map(|s| s.stream_id().map(|sid| sid.to_string()))
                .find(|sid| *sid != current_audio);
            match (video, audio) {
                (Some(video), Some(audio)) => {
                    video_id = Some(video);
                    other_audio = Some(audio);
                    true
                }
                _ => false,
            }
        },
    );
    let video_id = video_id.expect("the matched collection has video");
    let other_audio = other_audio.expect("the matched collection has a second audio");

    let seqnum = gst::Seqnum::next();
    harness
        .playbin
        .select_streams(&[&video_id, &other_audio], Some(seqnum))
        .expect("queueing the selection");
    harness.wait_for("the confirmation with our seqnum", |event, _| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected { seqnum: got, audio, .. }
                if *got == seqnum && audio.as_deref() == Some(other_audio.as_str())
        )
    });

    // The unknown-id case: refused up front with Err, nothing queued. It
    // used to be silently accepted (Ok, then no confirmation ever, a
    // seqnum-keyed caller starving forever), which this test caught.
    let unknown_seq = gst::Seqnum::next();
    let outcome = harness
        .playbin
        .select_streams(&["fcast-diag-no-such-stream"], Some(unknown_seq));
    assert!(
        outcome.is_err(),
        "an unknown id must be refused, not queued into silence: {outcome:?}"
    );
    harness.pump_for(Duration::from_secs(1));
    {
        let log = harness.log.borrow();
        let confirmed = log.iter().any(|(event, _)| {
            matches!(event, PlaybinEvent::StreamsSelected { seqnum: got, .. } if *got == unknown_seq)
        });
        assert!(!confirmed, "a refused selection must not confirm: {log:#?}");
        let errored = log
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::Error { .. }));
        assert!(
            !errored,
            "the refusal must not produce a pipeline error: {log:#?}"
        );
    }

    // Recovery: the select lane is not wedged, a valid request still lands.
    let recovery_seq = gst::Seqnum::next();
    harness
        .playbin
        .select_streams(&[&video_id, &current_audio], Some(recovery_seq))
        .expect("queueing the recovery selection");
    harness.wait_for("the recovery confirmation", |event, _| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected { seqnum: got, .. } if *got == recovery_seq
        )
    });

    assert_worker_alive(&harness, "after the unknown-id selection");
    harness.shutdown();
    scenario.unregister();
}

/// `set_subtitle_consumer` is documented "call at most once"; a second call
/// REPLACES the first. Pinned behavior: after B is installed A receives
/// nothing further, and B's first item is a plain Cue with NO leading Clear,
/// so B silently inherits mid-window (it never learns whether a cue was on
/// screen at handover).
#[test]
fn replacing_the_subtitle_consumer_mid_stream() {
    init();
    let main = ScenarioBuilder::new("diagconsmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .bytes_per_buffer(64)
        .pacing(Pacing::Realtime)
        .register();
    // Realtime pacing on the text too, so cues keep arriving over the clip
    // instead of the one-burst handover an unpaced external produces.
    let subs = ScenarioBuilder::new("diagconstext")
        .text("text_0", cues(50, gst::ClockTime::from_mseconds(500)))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();

    // Unarmed: consumer A must be the FIRST consumer this pipeline sees.
    let harness = Harness::new_unarmed();

    #[derive(Debug, Clone, PartialEq)]
    enum Item {
        Cue(String),
        Clear,
    }
    let feed_of = |log: Arc<Mutex<Vec<Item>>>| {
        move |item: SubtitleFeedItem| {
            let entry = match item {
                SubtitleFeedItem::Cue { text, .. } => Item::Cue(text),
                SubtitleFeedItem::Clear => Item::Clear,
                SubtitleFeedItem::Bitmap { .. } => return,
            };
            log.lock().unwrap().push(entry);
        }
    };
    let cue_count = |log: &Arc<Mutex<Vec<Item>>>| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|item| matches!(item, Item::Cue(_)))
            .count()
    };

    let seen_a: Arc<Mutex<Vec<Item>>> = Default::default();
    harness
        .playbin
        .set_subtitle_consumer(feed_of(seen_a.clone()));

    harness.load_and_play(&main.uri());
    let (id, sid) = harness.attach_and_materialize(&subs.uri());
    harness.select_and_confirm(id, &sid);

    // A is live: cues flow to it.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while cue_count(&seen_a) < 2 {
        assert!(
            Instant::now() < deadline,
            "no cues ever reached consumer A; log: {:#?}",
            harness.log.borrow()
        );
        harness.pump_for(Duration::from_millis(10));
    }

    // The replacement under test.
    let seen_b: Arc<Mutex<Vec<Item>>> = Default::default();
    harness
        .playbin
        .set_subtitle_consumer(feed_of(seen_b.clone()));

    // A delivery already in flight at the swap still holds A's Arc, so the
    // baseline for "A receives nothing further" is taken once B provably
    // owns the feed.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while seen_b.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "consumer B never received anything after replacing A; log: {:#?}",
            harness.log.borrow()
        );
        harness.pump_for(Duration::from_millis(10));
    }
    let a_frozen_at = seen_a.lock().unwrap().len();

    let deadline = Instant::now() + EVENT_TIMEOUT;
    while cue_count(&seen_b) < 3 {
        assert!(
            Instant::now() < deadline,
            "cue flow to consumer B dried up; log: {:#?}",
            harness.log.borrow()
        );
        harness.pump_for(Duration::from_millis(10));
    }

    assert_eq!(
        seen_a.lock().unwrap().len(),
        a_frozen_at,
        "consumer A kept receiving after being replaced: {:#?}",
        seen_a.lock().unwrap()
    );
    // The handover pin: no synthetic Clear precedes B's first cue. B
    // inherits mid-window and cannot know what was on screen at the swap.
    let first_b = seen_b.lock().unwrap().first().cloned();
    assert!(
        matches!(first_b, Some(Item::Cue(_))),
        "expected B to inherit silently with a plain first cue, got {first_b:?}"
    );

    assert_worker_alive(&harness, "after replacing the consumer");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}
