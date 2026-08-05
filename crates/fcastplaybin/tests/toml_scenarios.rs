//! Every `tests/scenario_files/*.toml` played end to end through the real
//! fcastplaybin.
//!
//! Adding a regression case is adding a file: no Rust, no registration, no test
//! attribute. The trade is depth. A file describes media and the smoke run asserts
//! what holds for ALL media (it loads, it plays, every sink sequence is legal, the
//! worker still answers). A scenario that needs an action schedule belongs in
//! `tests/scenarios.rs` where a timeline can drive it.
//!
//! Discovery is runtime, so one failing file names itself and the rest still run.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::{ScenarioHandle, check_all_named, toml, wait_quiescent},
    sink::{FTestSink, Recording, asserts, event_name},
};
use gst::prelude::*;

/// Generous bound for anything the pipeline has to reach. Scenario media plays in
/// real time (the sinks sync), so a busy box must not flake.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

/// No log growth for this long counts as quiescent for the invariant sweep.
const QUIESCENT_SETTLE: Duration = Duration::from_millis(200);

/// Bound for the shutdown barrier between files.
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

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

fn scenario_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenario_files")
}

/// Every `.toml` under [`scenario_dir`], name-sorted so a run is reproducible.
fn scenario_files() -> Vec<PathBuf> {
    let dir = scenario_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    files
}

/// The `tests/scenarios.rs` harness, trimmed to what a smoke run needs: recording
/// sinks, the pumped event wait, and the settle-point calls the receiver makes.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: Mutex<Vec<PlaybinEvent>>,
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
            log: Mutex::new(Vec::new()),
            video,
            audio,
        }
    }

    fn audio(&self) -> Option<Recording> {
        self.audio
            .lock()
            .expect("audio recording slot")
            .last()
            .cloned()
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
    }

    /// Wait until `pred` matches a newly received event, pumping between polls.
    /// Panics with the file name and the log on timeout or pipeline error.
    fn wait_for(&self, what: &str, file: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "{file}: timed out waiting for {what}; log: {:#?}",
                self.log.lock().expect("event log")
            );
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, _generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "{file}: pipeline error while waiting for {what}: {error} (log: {:#?})",
                            self.log.lock().expect("event log")
                        );
                    }
                    let hit = pred(&event);
                    self.log.lock().expect("event log").push(event);
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{file}: the event channel closed while waiting for {what}")
                }
            }
        }
    }

    /// Blocks until the worker tore the pipeline down, so the next file starts from
    /// a settled process.
    fn shutdown(&self, file: &str) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + TEARDOWN_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "{file}: shutdown never finished");
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{file}: the worker died during shutdown")
                }
            }
        }
    }
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

/// Loads one file's media, plays it to EOS and sweeps the sinks. Every panic names
/// `file`, which is the only thing that identifies the case.
fn play_to_eos(handle: &ScenarioHandle, file: &str) {
    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(handle.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_for("Loaded", file, |event| {
        matches!(event, PlaybinEvent::Loaded { .. })
    });
    harness.playbin.play().expect("play");
    harness.wait_for("settled PLAYING", file, |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    harness.wait_for("EndOfStream", file, |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });

    let audio = harness.audio();
    let mut recordings: Vec<(&str, &Recording)> = Vec::new();
    let declares = |want: gst::StreamType| {
        handle
            .spec()
            .streams
            .iter()
            .any(|stream| stream.kind.stream_type().contains(want))
    };
    let has_video = declares(gst::StreamType::VIDEO);
    let has_audio = declares(gst::StreamType::AUDIO);
    if has_video {
        assert!(
            harness.video.buffer_count() > 0,
            "{file}: the media has video but none reached the sink: {:?}",
            harness.video
        );
        assert_eq!(
            harness.video.event_count(event_name::EOS),
            1,
            "{file}: exactly one EOS per sink"
        );
        recordings.push(("video", &harness.video));
    }
    // Symmetric with the video half above, which it was not: `harness.audio()`
    // is None when the crate never built an audio sink for this load, and an
    // `if let Some(..)` there makes the whole audio half of the assertion
    // VANISH exactly when it matters. Every file but video_only.toml declares
    // audio, and for the seven that also carry video a silently missing audio
    // chain used to leave this test green on the video assertions alone.
    assert!(
        !has_audio || audio.is_some(),
        "{file}: the media declares an audio stream but the crate never built an \
         audio sink for it, so nothing here observed audio at all"
    );
    if let Some(audio) = &audio {
        assert!(
            audio.buffer_count() > 0,
            "{file}: no audio reached the sink: {audio:?}"
        );
        assert_eq!(
            audio.event_count(event_name::EOS),
            1,
            "{file}: exactly one EOS per sink (audio)"
        );
        recordings.push(("audio", audio));
    }
    assert!(
        !recordings.is_empty(),
        "{file}: the media produced no renderable stream"
    );

    // Only legal at a quiescent point: a snapshot taken mid-flushing-seek ends with
    // an unmatched FLUSH_START and would report a violation.
    assert!(
        wait_quiescent(&recordings, QUIESCENT_SETTLE, EVENT_TIMEOUT),
        "{file}: the sinks never went quiescent"
    );
    if let Err(violations) = check_all_named(&recordings) {
        panic!("{file}: {violations}");
    }
    // Not part of `asserts::all`, on purpose. The rule is measured for a stream a
    // sink watched OPEN and nothing here seeks mid-play or loads twice, so it is
    // run where the evidence is rather than armed for every suite. See
    // fcasttest::sink::asserts::first_buffer_is_discont.
    for (name, recording) in &recordings {
        if let Err(violation) = asserts::first_buffer_is_discont(&recording.snapshot()) {
            panic!("{file}: {name}: {violation}");
        }
    }
    assert_worker_alive(&harness, file);
    harness.shutdown(file);
}

/// Plays each file inside `catch_unwind` and collects what failed, so one bad
/// scenario names itself and every later file still runs. Aborting on the first
/// failure silently retired every file sorting after it, so a single red scenario
/// hid the rest for as long as it stayed red.
///
/// Returns `(file name, panic message)` per failure, in discovery order.
fn sweep(files: &[PathBuf]) -> Vec<(String, String)> {
    let mut failures: Vec<(String, String)> = Vec::new();
    for path in files {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a UTF-8 file name")
            .to_owned();
        let handle = toml::load_file(path).unwrap_or_else(|err| panic!("{err}"));

        let played = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            play_to_eos(&handle, &file);
        }));
        handle.unregister();

        if let Err(payload) = played {
            let why = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panicked with a non-string payload".to_owned());
            failures.push((file, why));
        }
    }
    failures
}

/// One test over every file, so the discovery itself is what fails when the
/// directory is empty or a document is malformed.
#[test]
fn every_scenario_file_plays_to_eos() {
    init();
    let files = scenario_files();
    assert!(
        !files.is_empty(),
        "no .toml files in {}",
        scenario_dir().display()
    );

    let total = files.len();
    let failures = sweep(&files);
    assert!(
        failures.is_empty(),
        "{} of {total} scenario files failed:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|(file, why)| format!("  {file}: {why}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The sweep is only worth anything if a broken scenario actually turns it red,
/// and only readable if the FIRST broken one does not hide the second.
///
/// Both files here inject a source error, which the crate reports as a pipeline
/// error rather than a hang, so the case is fast and deterministic. They live in
/// `CARGO_TARGET_TMPDIR` rather than `scenario_files/`, because every document in
/// that directory has to pass.
#[test]
fn a_file_that_should_fail_does_and_does_not_hide_the_next_one() {
    init();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("must_fail_scenarios");
    std::fs::create_dir_all(&dir).expect("a temp directory for the failing documents");

    // Named 1 and 2 so the sort order is the order they are written here. The
    // point is that the FIRST failure does not retire the second file.
    let documents = [
        (
            "must_fail_1_audio_error.toml",
            r#"
key = "mustfailaudio"
duration_ms = 800
bytes_per_buffer = 64
pacing = "as_fast_as_possible"

[[stream]]
id = "video_0"
kind = "video"
fps = 25
keyframe_interval = 25

[[stream]]
id = "audio_0"
kind = "audio"

[[stream.fault]]
buffer_index = 2
kind = "error"
"#,
        ),
        (
            "must_fail_2_video_error.toml",
            r#"
key = "mustfailvideo"
duration_ms = 800
bytes_per_buffer = 64
pacing = "as_fast_as_possible"

[[stream]]
id = "video_0"
kind = "video"
fps = 25
keyframe_interval = 25

[[stream.fault]]
buffer_index = 3
kind = "error"

[[stream]]
id = "audio_0"
kind = "audio"
"#,
        ),
    ];

    let mut paths = Vec::new();
    for (name, document) in documents {
        let path = dir.join(name);
        std::fs::write(&path, document).expect("writing a failing document");
        paths.push(path);
    }

    let failures = sweep(&paths);
    let named: Vec<&str> = failures.iter().map(|(file, _)| file.as_str()).collect();
    assert_eq!(
        named,
        vec![
            "must_fail_1_audio_error.toml",
            "must_fail_2_video_error.toml"
        ],
        "the sweep has to report BOTH failing files, in order; it reported {failures:#?}"
    );
    for (file, why) in &failures {
        assert!(
            why.contains(file),
            "a failure has to name its own file: {why}"
        );
        // The crate reports a user-facing string, not the element's own text, so
        // the assertion is on the harness half of the message.
        assert!(
            why.contains("pipeline error"),
            "{file}: the failure has to say a pipeline error is what killed it: {why}"
        );
    }
}
