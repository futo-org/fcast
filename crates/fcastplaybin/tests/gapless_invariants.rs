//! Adversarial pins for the gapless invariants of CLEANUP.md that no other
//! test turns red (invariants 5, 6, 9, 10 and the R6 seek-while-armed hole).
//!
//! Every case here drives the REAL pipeline through the public API over
//! generated media, the configuration `tests/gapless.rs` established. What
//! makes this file different from that suite is what it aims at. The
//! machinery that keeps a user operation from deadlocking against a pending
//! prepare has no coverage at all, and its load-bearing lines look like dead
//! code:
//!
//! * `add_prepared_input`'s `set_locked_state(true)` is the ONLY thing that
//!   keeps a user `pause()` bounded while a prepare is armed. `Job::SetState`
//!   never aborts the swap gate.
//! * `swap_gate.abort()` on the downward paths (`load`, `teardown`,
//!   `Inner::drop`) is what lets a `stop()` join the prepared input's
//!   streaming threads, which are otherwise parked in the gate's condvar.
//! * The external-subtitle refusal in `Job::PrepareNext` (invariant 9) and
//!   the generation its `PreparedFailed` carries are read by nothing.
//! * `perform_gapless_swap`'s unlink-all-before-relink-any ordering
//!   (invariant 6) fails SILENTLY, and the exactly-one-STREAM_START contract
//!   at `fpb-aqueue` (invariant 5) anchors the held activation.
//!
//! A wedge here is not an error or an EOS, the calls simply never return, so
//! every blocking operation runs on its own thread under a bound.

use std::{
    cell::{Cell, RefCell},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
    state_machine::Seek,
};
use fcasttest::sink::FTestSink;
use gst::prelude::*;

/// Generous bound for anything the pipeline has to reach. The media plays in
/// real time and the suite runs several pipelines at once, so a busy box
/// must not flake while a wedge must still fail.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Bound for one blocking user call (`pause`, `stop`) issued while a prepare
/// is armed. The healthy calls return in well under a second.
const OP_BOUND: Duration = Duration::from_secs(15);

static TEE: AtomicBool = AtomicBool::new(false);

/// Sink for the tracing subscriber, teed to stderr when FCASTPLAYBIN_TEST_LOG
/// is set and swallowed otherwise.
///
/// It deliberately does NOT accumulate. The subscriber is installed once per
/// PROCESS, so any buffer behind it is shared by every test thread in the
/// binary and a substring assertion over it can be satisfied by another
/// test's pipeline. Assertions here read per-instance state instead (see
/// `an_arm_finding_the_boundary_already_crossed_releases_immediately`).
struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if TEE.load(Ordering::Relaxed) {
            let _ = std::io::stderr().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
        // Calls gst::init and registers ftestsink.
        fcasttest::register_for_tests();
        // Built by the constructor, registered by the application.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// Whether the plugins the media generator needs are present. Absent on
/// exotic environments; the tests skip rather than fail there, matching
/// `tests/gapless.rs`.
fn encoders_available() -> bool {
    [
        "vp8enc",
        "vorbisenc",
        "webmmux",
        "vp8dec",
        "vorbisdec",
        "matroskamux",
        "subparse",
    ]
    .iter()
    .all(|f| gst::ElementFactory::find(f).is_some())
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-ginv-{}-{}",
        std::process::id(),
        name
    ))
}

fn run_to_eos(desc: &str) {
    let pipeline = gst::parse::launch(desc).expect("encode pipeline parses");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("encode finishes");
    if let gst::MessageView::Error(err) = msg.view() {
        panic!("encode pipeline failed: {}", err.error());
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// Encode an A/V webm clip (64x64 vp8 + vorbis tone) of `seconds` and return
/// its file:// URI. `pattern`/`freq` make each clip distinct.
fn encode_av_clip(name: &str, pattern: &str, freq: u32, seconds: u32) -> String {
    let path = tmp_path(&format!("{name}.webm"));
    let desc = format!(
        "videotestsrc num-buffers={} pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers={} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         webmmux name=mux ! filesink location={}",
        seconds * 30,
        seconds * 44,
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// Encode an audio-only webm clip, `num_buffers` x 1024 samples at 44.1kHz.
/// The single-request-pad topology, and the shape whose input drains latest
/// relative to its length (nothing throttles decode but the shallow audio
/// queue), which is what gives the armed-not-yet-swapped window the pause,
/// stop and seek cases need.
fn encode_audio_clip(name: &str, freq: u32, num_buffers: u32) -> String {
    let path = tmp_path(&format!("{name}.webm"));
    let desc = format!(
        "audiotestsrc num-buffers={num_buffers} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc \
           ! webmmux ! filesink location={}",
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// Dense cues covering `seconds`, one every 400ms, so the external input is
/// live whatever the playhead. Returned as a file:// URI.
fn write_srt(name: &str, seconds: u32) -> String {
    let path = tmp_path(name);
    let mut srt = String::new();
    for i in 0..(seconds * 1000 / 400) {
        let start = i * 400;
        let end = start + 380;
        let stamp = |ms: u32| {
            format!(
                "{:02}:{:02}:{:02},{:03}",
                ms / 3_600_000,
                (ms / 60_000) % 60,
                (ms / 1000) % 60,
                ms % 1000
            )
        };
        srt.push_str(&format!(
            "{}\n{} --> {}\nCUE{i:02}\n\n",
            i + 1,
            stamp(start),
            stamp(end)
        ));
    }
    std::fs::write(&path, srt).expect("writing the srt file");
    format!("file://{}", path.display())
}

/// A prepared input built the way the receiver builds one, `urisourcebin`
/// with parsed streams out (a prepared input must be urisourcebin-rooted,
/// invariant 11). Handing the element in instead of a URI lets a test keep
/// the handle and observe the swap.
fn uri_source(uri: &str) -> gst::Element {
    gst::ElementFactory::make("urisourcebin")
        .property("uri", uri)
        .property("parse-streams", true)
        .property("use-buffering", true)
        .build()
        .expect("building a urisourcebin prepared input")
}

/// A playbin under test plus the ordered `(event, generation)` log its
/// callback produced. Waits pump the receiver's settle points
/// (`poll_text_policy` + `pump_selection`) and re-drive the transport the
/// crate hands back (`QueueSeek`, a lost-state dip to PAUSED), exactly like
/// the harnesses in `tests/regression_gapless.rs` and
/// `tests/external_subtitle_lifecycle.rs`.
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<(PlaybinEvent, u64)>>,
    paused: Cell<bool>,
    wants_playing: Cell<bool>,
    parked_seek: Cell<Option<Seek>>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin: Arc::new(playbin),
            events,
            log: RefCell::new(Vec::new()),
            paused: Cell::new(false),
            wants_playing: Cell::new(false),
            parked_seek: Cell::new(None),
        }
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

    /// Put back the transport the crate parked. A refused seek comes back as
    /// `QueueSeek` and is re-issued from the next settled PAUSED; a pipeline
    /// that lost state (a branch added while PLAYING) is re-driven to the
    /// PLAYING the test still wants.
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
                } else if self.wants_playing.get() {
                    let _ = self.playbin.play();
                }
            }
            _ => {}
        }
    }

    fn drain_events(&self) {
        while let Ok((event, generation)) = self.events.try_recv() {
            self.redrive_transport(&event);
            self.log.borrow_mut().push((event, generation));
        }
    }

    /// The log position after draining what already arrived. Waits scoped to
    /// a mark can see events that landed between the triggering call and the
    /// wait, without matching stale ones from before it.
    fn mark(&self) -> usize {
        self.drain_events();
        self.log.borrow().len()
    }

    /// Wait until `pred` matches an event at or past `mark`, pumping between
    /// polls. Panics with the log on timeout or pipeline error.
    fn wait_for_since(
        &self,
        mark: usize,
        what: &str,
        mut pred: impl FnMut(&PlaybinEvent, u64) -> bool,
    ) {
        self.drain_events();
        if self.log.borrow()[mark..]
            .iter()
            .any(|(event, generation)| pred(event, *generation))
        {
            return;
        }
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
                            "pipeline error while waiting for {what}, {error} (log: {:#?})",
                            self.log.borrow()
                        );
                    }
                    self.redrive_transport(&event);
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

    /// Pump until `done`, or panic at `bound`.
    fn wait_until(&self, what: &str, bound: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + bound;
        loop {
            self.drain_events();
            if done() {
                return;
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

    /// Pump for `settle` so late events can land, then drain.
    fn drain_after(&self, settle: Duration) {
        let deadline = Instant::now() + settle;
        while Instant::now() < deadline {
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        self.drain_events();
    }

    /// Load `uri`, start playback and wait for the settled PLAYING. Returns
    /// the load's generation.
    fn load_and_play(&self, uri: &str) -> u64 {
        let mark = self.mark();
        self.wants_playing.set(false);
        self.parked_seek.set(None);
        let generation = self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for_since(mark, "Loaded", |event, seen| {
            matches!(event, PlaybinEvent::Loaded { .. }) && seen == generation
        });
        self.play();
        generation
    }

    fn play(&self) {
        let mark = self.mark();
        self.playbin.play().expect("play");
        self.paused.set(false);
        self.wants_playing.set(true);
        self.wait_for_since(mark, "settled PLAYING", |event, _| {
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

    /// Run a blocking playbin call on its own thread and require it to
    /// return within `bound`, pumping while waiting. The wedge detector.
    fn bounded_op<T: Send + 'static>(
        &self,
        what: &str,
        bound: Duration,
        op: impl FnOnce(&FcastPlaybin) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::channel();
        let playbin = self.playbin.clone();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(op(&playbin));
        });
        let deadline = Instant::now() + bound;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(value) => {
                    let _ = handle.join();
                    return value;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "{what} did not return within {bound:?}, the pipeline is wedged; \
                         log: {:#?}",
                        self.log.borrow()
                    );
                    self.drain_events();
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the {what} thread died without reporting")
                }
            }
        }
    }

    /// The worker must answer a queued job within a bound. A wedged worker
    /// or streaming thread shows up here as a job that never completes.
    fn assert_worker_alive(&self, what: &str) {
        let (tx, rx) = mpsc::channel();
        self.playbin.debug_graph_async(Box::new(move |_| {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the worker is wedged, {what}");
                    self.drain_events();
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died, {what}"),
            }
        }
    }

    /// Queue-order barrier plus shape assertions for an armed prepare. The
    /// worker runs jobs in order, so a completed debug-graph round trip
    /// queued after the prepare proves `Job::PrepareNext` ran. Armed means
    /// the prepared element joined the pipeline; not-yet-swapped means no
    /// source pad is linked into decodebin3. The trailing wait gives the
    /// prepared input's streaming threads time to reach the swap gate's
    /// condvar, which is the parked state the pause and stop cases are
    /// about.
    fn wait_prepare_armed(&self, prepared: &gst::Element) {
        self.assert_worker_alive("arming the prepare");
        assert!(
            prepared.parent().is_some(),
            "the prepared input never joined the pipeline (the prepare failed?); \
             log: {:#?}",
            self.log.borrow()
        );
        self.wait_until("the prepared input's parsed pads", EVENT_TIMEOUT, || {
            !prepared.src_pads().is_empty()
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !prepared.src_pads().iter().any(|pad| pad.is_linked()),
            "the swap already performed before the test could act inside the armed \
             window. The first item must be long enough that its input has not \
             drained yet, lengthen it instead of relaxing anything"
        );
    }

    fn log_has(&self, mut pred: impl FnMut(&PlaybinEvent, u64) -> bool) -> bool {
        self.drain_events();
        self.log
            .borrow()
            .iter()
            .any(|(event, generation)| pred(event, *generation))
    }

    fn assert_no_activation(&self, context: &str) {
        assert!(
            !self.log_has(|event, _| matches!(event, PlaybinEvent::PreparedActivated)),
            "PreparedActivated fired {context}; log: {:#?}",
            self.log.borrow()
        );
    }

    fn assert_no_eos_since(&self, mark: usize, context: &str) {
        self.drain_events();
        let log = self.log.borrow();
        assert!(
            !log[mark..]
                .iter()
                .any(|(event, _)| matches!(event, PlaybinEvent::EndOfStream)),
            "unexpected pipeline EOS {context}; log since the mark: {:#?}",
            &log[mark..]
        );
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + OP_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "shutdown never completed");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// INVARIANT 10, the pause half. `Job::SetState` never calls
/// `swap_gate.abort()`, so with a prepare armed (its streaming threads
/// parked in the gate's condvar, its element in the pipeline) the ONLY thing
/// keeping a user `pause()` from deadlocking against the prepared input is
/// `add_prepared_input`'s `set_locked_state(true)`, which makes the bin's
/// state machinery skip that child. Nothing else covers that line, and
/// deleting it looks like dead code.
///
/// The pin: pause from a caller thread must return within a bound, the
/// worker must still answer afterwards, and the armed prepare must survive
/// the pause untouched, activating normally after resume.
#[test]
fn pause_while_a_prepare_is_armed_is_bounded() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    // ~8s (352 x 1024 @ 44.1kHz). The input drains only ~1.3s before the
    // end on the audio-only topology, so the armed window is wide open when
    // the pause lands ~1s in.
    let first = encode_audio_clip("pause-a", 440, 352);
    let second = encode_audio_clip("pause-b", 880, 87);

    let h = Harness::new();
    let _first_generation = h.load_and_play(&first);
    let prepared_element = uri_source(&second);
    let mark = h.mark();
    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Element(prepared_element.clone()));
    h.wait_prepare_armed(&prepared_element);

    // The user pause, from its own thread under a bound.
    h.paused.set(true);
    h.wants_playing.set(false);
    let pause_mark = h.mark();
    h.bounded_op("pause() with a prepare armed", OP_BOUND, |playbin| {
        playbin.pause()
    })
    .expect("pause");
    h.wait_for_since(pause_mark, "settled PAUSED", |event, _| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Paused,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    h.assert_worker_alive("after a pause with a prepare armed");

    // The pause touched neither the prepare nor the item's end.
    h.assert_no_activation("during the pause");
    assert!(
        prepared_element.parent().is_some(),
        "the pause tore the armed prepare out of the pipeline"
    );
    h.assert_no_eos_since(mark, "across the pause");

    // Resume and let the swap complete normally.
    h.play();
    h.wait_for_since(mark, "PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    h.assert_worker_alive("after the post-pause activation");
    let eos_generation = Cell::new(None);
    h.wait_for_since(mark, "the final EndOfStream", |event, generation| {
        let hit = matches!(event, PlaybinEvent::EndOfStream);
        if hit {
            eos_generation.set(Some(generation));
        }
        hit
    });
    assert_eq!(
        eos_generation.get(),
        Some(prepared_generation),
        "the final EOS belongs to the activated item"
    );
    h.shutdown();
}

/// INVARIANT 10, the stop half. The downward paths (`teardown`, the load
/// reset, `Inner::drop`) call `swap_gate.abort()` BEFORE the state change,
/// because tearing down NULLs every input including the prepared one, and
/// NULLing an element joins its task threads. With a prepare armed those
/// threads are parked in the gate's condvar, so without the abort the join
/// waits forever and `stop()` never returns.
///
/// The pin: stop with a prepare armed returns Ok within a bound, the worker
/// answers afterwards, no activation ever fires, and a fresh load on the
/// same instance plays to its own end.
#[test]
fn stop_while_a_prepare_is_armed_is_bounded() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_audio_clip("stop-a", 440, 352);
    let second = encode_audio_clip("stop-b", 880, 87);
    let third = encode_audio_clip("stop-c", 660, 87);

    let h = Harness::new();
    let _first_generation = h.load_and_play(&first);
    let prepared_element = uri_source(&second);
    let _prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Element(prepared_element.clone()));
    h.wait_prepare_armed(&prepared_element);

    h.wants_playing.set(false);
    h.bounded_op("stop() with a prepare armed", OP_BOUND, |playbin| {
        playbin.stop()
    })
    .expect("stop");
    h.assert_worker_alive("after a stop with a prepare armed");
    h.assert_no_activation("after the stop dropped the prepare");

    // The instance is reusable, a fresh load plays to its own end.
    let third_generation = h.load_and_play(&third);
    let eos_generation = Cell::new(None);
    let mark = h.mark();
    h.wait_for_since(mark, "the fresh load's EndOfStream", |event, generation| {
        let hit = matches!(event, PlaybinEvent::EndOfStream);
        if hit {
            eos_generation.set(Some(generation));
        }
        hit
    });
    assert_eq!(
        eos_generation.get(),
        Some(third_generation),
        "the EOS belongs to the fresh load"
    );
    h.assert_no_activation("after the fresh load");
    h.shutdown();
}

/// INVARIANT 9. A prepare while an external subtitle input is attached must
/// be refused, because the swap would carry the side input across and its
/// stream would corrupt the next item's collections. The crate-side guard
/// sits at the top of `Job::PrepareNext`, and its `PreparedFailed` carries
/// the refused generation, a payload nothing else reads.
///
/// The pin: with an attached, materialized external subtitle, a prepare
/// reports `PreparedFailed` whose generation field equals what
/// `prepare_next_async` returned, no activation ever follows, and the
/// current item still ends through the ordinary path under its own
/// generation.
#[test]
fn prepare_is_refused_while_an_external_subtitle_is_attached() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("refuse-a", "smpte", 440, 6);
    let second = encode_av_clip("refuse-b", "ball", 880, 2);
    let subs = write_srt("refuse.srt", 7);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let id = h
        .playbin
        .attach_subtitle(&subs)
        .expect("attaching the external subtitle");
    h.wait_until("the external stream to materialize", EVENT_TIMEOUT, || {
        !h.playbin.subtitle_stream_ids(id).is_empty()
    });

    let mark = h.mark();
    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    h.wait_for_since(mark, "PreparedFailed for the refusal", |event, _| {
        matches!(
            event,
            PlaybinEvent::PreparedFailed { generation }
                if *generation == prepared_generation
        )
    });

    // The refusal left the ordinary ending untouched.
    let eos_generation = Cell::new(None);
    h.wait_for_since(mark, "EndOfStream after the refusal", |event, generation| {
        let hit = matches!(event, PlaybinEvent::EndOfStream);
        if hit {
            eos_generation.set(Some(generation));
        }
        hit
    });
    assert_eq!(
        eos_generation.get(),
        Some(first_generation),
        "a refused prepare must leave the current item's ending untouched"
    );
    h.drain_after(Duration::from_millis(700));
    h.assert_no_activation("after a refused prepare");
    h.shutdown();
}

/// INVARIANT 5. Exactly one STREAM_START crosses `fpb-aqueue` per audio
/// item. The held activation's release is anchored on that crossing with no
/// group-id bookkeeping at all (streamsynchronizer may rewrite group ids),
/// so the whole scheme rests on the crossing being exactly one per item, a
/// duplicate would release a hold that was armed for the NEXT boundary and
/// a missing one would hold the activation forever.
///
/// The pin: over an A/V-to-A/V gapless swap the queue's src pad sees exactly
/// two STREAM_STARTs, one per item with distinct stream ids, the second one
/// already recorded when `PreparedActivated` reaches the caller, and no
/// third ever follows.
#[test]
fn exactly_one_stream_start_crosses_the_audio_queue_per_item() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("count-a", "smpte", 440, 2);
    let second = encode_av_clip("count-b", "ball", 880, 2);

    let h = Harness::new();
    // The decoupling queue is a static element, installed at construction
    // and surviving every load, so the probe goes in before anything flows.
    let aqueue = h
        .playbin
        .pipeline()
        .by_name("fpb-aqueue")
        .expect("fpb-aqueue exists");
    let src = aqueue.static_pad("src").expect("fpb-aqueue src pad");
    let starts: Arc<Mutex<Vec<String>>> = Arc::default();
    let rec = starts.clone();
    src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && let gst::EventView::StreamStart(stream_start) = event.view()
        {
            rec.lock().unwrap().push(stream_start.stream_id().to_string());
        }
        gst::PadProbeReturn::Ok
    });

    let _first_generation = h.load_and_play(&first);
    h.wait_until("item A's STREAM_START at the queue", EVENT_TIMEOUT, || {
        !starts.lock().unwrap().is_empty()
    });
    h.drain_after(Duration::from_millis(300));
    assert_eq!(
        starts.lock().unwrap().len(),
        1,
        "item A pushed more than one STREAM_START through fpb-aqueue; \
         seen: {:?}",
        starts.lock().unwrap()
    );

    let mark = h.mark();
    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    h.wait_for_since(mark, "PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    // The release is EMITTED from the crossing's own probe, so by the time
    // the event reaches this thread the second crossing must be recorded.
    // The bounded poll only absorbs probe ordering on the pad itself.
    h.wait_until("item B's STREAM_START at the queue", Duration::from_secs(2), || {
        starts.lock().unwrap().len() >= 2
    });
    {
        let seen = starts.lock().unwrap();
        assert_eq!(
            seen.len(),
            2,
            "the boundary pushed more than one STREAM_START per item through \
             fpb-aqueue; seen: {seen:?}"
        );
        assert_ne!(
            seen[0], seen[1],
            "the second crossing repeats item A's stream id, so item B's audio \
             never announced itself; seen: {seen:?}"
        );
    }

    h.wait_for_since(mark, "the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    h.drain_after(Duration::from_millis(300));
    assert_eq!(
        starts.lock().unwrap().len(),
        2,
        "a straggler STREAM_START crossed fpb-aqueue after the boundary; \
         seen: {:?}",
        starts.lock().unwrap()
    );
    h.shutdown();
}

/// INVARIANT 6, whose failure mode is documented as SILENT.
/// `perform_gapless_swap` must unlink EVERY reused decodebin3 sink pad
/// before relinking ANY of them, because only the unlink invalidates an
/// input's group id, and one sibling still holding the old id makes
/// decodebin3 rewrite the new item's group id to the old one, after which
/// streamsynchronizer never rebases and nothing errors.
///
/// The pin sits at the seam itself: every existing `sink_%u` pad's
/// "unlinked" and "linked" GObject signals record into one ordered log, and
/// across an A/V-to-A/V swap (both request pads reused) every unlink must
/// precede the first relink of any reused pad, exactly once each.
#[test]
fn swap_unlinks_every_reused_pad_before_relinking_any() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("order-a", "smpte", 440, 2);
    let second = encode_av_clip("order-b", "ball", 880, 2);

    let h = Harness::new();
    let _first_generation = h.load_and_play(&first);

    let db3 = h
        .playbin
        .pipeline()
        .by_name("fpb-decodebin")
        .expect("fpb-decodebin exists");
    let sink_pads: Vec<gst::Pad> = db3
        .sink_pads()
        .into_iter()
        .filter(|pad| pad.name().starts_with("sink_"))
        .collect();
    assert_eq!(
        sink_pads.len(),
        2,
        "an A/V item requests one decodebin3 sink pad per elementary stream"
    );
    // (kind, pad name) in signal order. One mutex makes the vec order the
    // sequence counter.
    let recording: Arc<Mutex<Vec<(&'static str, String)>>> = Arc::default();
    for pad in &sink_pads {
        let rec = recording.clone();
        pad.connect_unlinked(move |pad, _peer| {
            rec.lock().unwrap().push(("unlink", pad.name().to_string()));
        });
        let rec = recording.clone();
        pad.connect_linked(move |pad, _peer| {
            rec.lock().unwrap().push(("link", pad.name().to_string()));
        });
    }

    let mark = h.mark();
    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    h.wait_for_since(mark, "PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });

    // The surgery ran strictly before the activation (the swap performs at
    // input drain, the activation only once the new streams reached the
    // output), so the recording is complete here and untouched by teardown.
    let snapshot = recording.lock().unwrap().clone();
    for pad in &sink_pads {
        let name = pad.name();
        let unlinks = snapshot
            .iter()
            .filter(|(kind, pad)| *kind == "unlink" && *pad == name)
            .count();
        let links = snapshot
            .iter()
            .filter(|(kind, pad)| *kind == "link" && *pad == name)
            .count();
        assert_eq!(
            (unlinks, links),
            (1, 1),
            "the swap must unlink and relink the reused pad {name} exactly once; \
             recording: {snapshot:?}"
        );
    }
    let last_unlink = snapshot
        .iter()
        .rposition(|(kind, _)| *kind == "unlink")
        .expect("asserted above");
    let first_link = snapshot
        .iter()
        .position(|(kind, _)| *kind == "link")
        .expect("asserted above");
    assert!(
        last_unlink < first_link,
        "the swap relinked a reused decodebin3 sink pad before every reused pad \
         was unlinked. A sibling still linked pins the old group id inside \
         decodebin3 and the new item's timeline is silently never rebased; \
         recording: {snapshot:?}"
    );

    h.wait_for_since(mark, "the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    h.shutdown();
}

/// The R6 coverage hole, a seek issued while a prepare is armed and
/// uncancelled. The receiver's contract (invariant 8) is to cancel first
/// through the parked-op slot, so this path is off the app's happy road, but
/// the crate must not WEDGE on a caller that seeks anyway.
///
/// The pin is wedge detection under bounds, not an ordering the crate does
/// not promise: the refused seek comes back as `QueueSeek` and its re-issue
/// from the settled PAUSED completes (`RateChanged`), the worker keeps
/// answering, and the session still terminates cleanly, either the swap
/// activates and the prepared item ends under its generation, or the
/// prepare fails/cancels cleanly and the current item ends under its own.
/// An EOS with the prepare still silently armed is the failure this exists
/// to catch.
#[test]
fn seek_while_a_prepare_is_armed_does_not_wedge() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_audio_clip("seek-a", 440, 352);
    let second = encode_audio_clip("seek-b", 880, 87);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let prepared_element = uri_source(&second);
    let mark = h.mark();
    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Element(prepared_element.clone()));
    h.wait_prepare_armed(&prepared_element);

    // The seek. At PLAYING it is handed back as QueueSeek and the harness
    // re-issues it from the settled PAUSED, the receiver's own loop shape.
    h.playbin.seek_async(Seek {
        position: Some(gst::ClockTime::from_mseconds(500)),
        rate: None,
    });
    let outcome = Cell::new(None);
    h.wait_for_since(mark, "the seek outcome", |event, _| match event {
        PlaybinEvent::RateChanged(_) => {
            outcome.set(Some("performed"));
            true
        }
        PlaybinEvent::SeekFailed => {
            outcome.set(Some("failed"));
            true
        }
        _ => false,
    });
    assert_eq!(
        outcome.get(),
        Some("performed"),
        "the seek on a seekable file at settled PAUSED must perform; log: {:#?}",
        h.log.borrow()
    );
    h.assert_worker_alive("after a seek with a prepare armed");

    // Terminal progress under a bound. Whatever the crate decided about the
    // armed prepare, the session must END, and the ending must be coherent.
    let eos_generation = Cell::new(None);
    h.wait_for_since(mark, "an EndOfStream after the seek", |event, generation| {
        let hit = matches!(event, PlaybinEvent::EndOfStream);
        if hit {
            eos_generation.set(Some(generation));
        }
        hit
    });
    h.assert_worker_alive("after the post-seek ending");
    let activated = h.log_has(|event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    let resolved = h.log_has(|event, _| {
        matches!(
            event,
            PlaybinEvent::PreparedFailed { generation }
                | PlaybinEvent::PreparedCancelled {
                    generation: Some(generation)
                } if *generation == prepared_generation
        )
    });
    if activated {
        assert_eq!(
            eos_generation.get(),
            Some(prepared_generation),
            "the swap activated, so the final EOS belongs to the prepared item; \
             log: {:#?}",
            h.log.borrow()
        );
    } else {
        assert!(
            resolved,
            "the item ended with the prepare still silently armed, neither \
             activated nor failed nor cancelled; log: {:#?}",
            h.log.borrow()
        );
        assert_eq!(
            eos_generation.get(),
            Some(first_generation),
            "the prepare resolved without activating, so the EOS belongs to the \
             current item; log: {:#?}",
            h.log.borrow()
        );
    }
    h.shutdown();
}

/// Encode an A/V matroska clip with an embedded SubRip track. A swap into a
/// text-bearing item is what makes the activation trigger able to fire on a
/// TEXT pad's thread: item A has no text, so item B's text is a fresh
/// decodebin3 slot whose whole track parses instantly at the swap, and its
/// output STREAM_START can reach the routed-pad probe long before the A/V
/// slots drain. Text bypasses streamsynchronizer, so an activation running
/// on the text thread holds back neither audio nor video, the shape the R1
/// race needs (queue_autoplay's second item carries two subtitle tracks).
fn encode_av_text_clip(name: &str, pattern: &str, seconds: u32) -> String {
    let srt = write_srt(&format!("{name}-cues"), seconds);
    let srt_path = srt.strip_prefix("file://").expect("srt uri").to_owned();
    let path = tmp_path(&format!("{name}.mkv"));
    let desc = format!(
        "videotestsrc num-buffers={} pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers={} freq=880 \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         filesrc location={srt_path} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        seconds * 30,
        seconds * 44,
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}


/// R1, the rare queue_autoplay boundary wedge (tracks never advertised),
/// mechanism pin. The activation can run on a thread with no ordering
/// against the audio data plane (a group-id rewrite disarms both the audio
/// pad's own trigger and the ssync barrier, and the video flip races the
/// aqueue crossing at millisecond scale), so arming `held_activation` after
/// the item's only STREAM_START already crossed `fpb-aqueue` held the
/// activation forever. The fix reads the aqueue src sticky under the hold's
/// lock at arm time and emits instead of arming when the edge is spent.
///
/// The spent-edge branch is made deterministic here with the one shape that
/// reaches it every run: two queue items from the SAME file. Their stream
/// ids are identical, so at arm the sticky (item A's own STREAM_START)
/// already names the prepared item's audio sid and the check must fire,
/// which is also the documented identical-sid degradation (the seam emits a
/// queue depth early rather than ever wedging). The boundary must then
/// complete, the prepared item ending under its own generation.
///
/// The branch is asserted from the instance's OWN counter, not from the
/// captured tracing. `CAPTURED` is process-global and every test in this
/// binary writes into it, so a substring search could be satisfied by
/// `a_late_activation_never_strands_the_held_events` reaching the same
/// branch in its own pipeline during one of its multi-second rounds: under
/// plain `cargo test` (threads in one process) the pin would pass with the
/// fix's branch never firing here. The generation is no discriminator
/// either, both instances count from the same base.
#[test]
fn an_arm_finding_the_boundary_already_crossed_releases_immediately() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let uri = encode_av_clip("arm-spent-edge", "smpte", 440, 3);

    let h = Harness::new();
    let _first_generation = h.load_and_play(&uri);
    let mark = h.mark();
    let before = h.playbin.arm_time_activation_releases();
    let prepared_generation = h.playbin.prepare_next_async(MediaInput::Uri(uri.clone()));

    h.wait_for_since(mark, "PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_eq!(
        h.playbin.arm_time_activation_releases(),
        before + 1,
        "identical stream ids must make the arm-time check see the edge as \
         spent and release right there; log: {:#?}",
        h.log.borrow()
    );

    h.wait_for_since(mark, "the prepared item's EndOfStream", |event, generation| {
        matches!(event, PlaybinEvent::EndOfStream) && generation == prepared_generation
    });
    h.shutdown();
}

/// R1's wedge detector. A staged delay at the top of `activate_prepared_now`
/// (`stage_activation_delay`, the per-instance staging pattern of
/// `stage_join_before_active`) widens the trigger-to-arm window to seconds,
/// so ANY interleaving in which the boundary's audio crossing is not held
/// back by the activation thread spends the release edge inside the window.
/// Whatever the interleaving does, the activation must reach the caller and
/// the prepared item must play out, three boundaries in a row.
///
/// Item B carries an embedded text track (the queue_autoplay field shape,
/// whose second item has two), giving decodebin3 a fresh slot next to the
/// reused ones. When a run does stage the race (crossing recorded well
/// before the activation's arrival) it is reported, but not required, the
/// common progressive-file interleavings are self-ordering by construction
/// and the deterministic branch pin lives in the test above.
#[test]
fn a_late_activation_never_strands_the_held_events() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("late-arm-a", "smpte", 440, 2);
    let second = encode_av_text_clip("late-arm-b", "ball", 8);

    for round in 0..3 {
        let h = Harness::new();
        h.playbin.stage_activation_delay(Duration::from_secs(3));

        let aqueue = h
            .playbin
            .pipeline()
            .by_name("fpb-aqueue")
            .expect("fpb-aqueue exists");
        let src = aqueue.static_pad("src").expect("fpb-aqueue src pad");
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let rec = starts.clone();
        src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && matches!(event.view(), gst::EventView::StreamStart(_))
            {
                rec.lock().unwrap().push(Instant::now());
            }
            gst::PadProbeReturn::Ok
        });

        let _first_generation = h.load_and_play(&first);
        let mark = h.mark();
        let prepared_generation = h
            .playbin
            .prepare_next_async(MediaInput::Uri(second.clone()));

        h.wait_for_since(mark, "PreparedActivated", |event, generation| {
            matches!(event, PlaybinEvent::PreparedActivated)
                && generation == prepared_generation
        });
        let staged = {
            let seen = starts.lock().unwrap();
            seen.len() >= 2
                && Instant::now().duration_since(seen[1]) > Duration::from_millis(700)
        };
        if staged {
            eprintln!("round {round}: the boundary crossing spent the edge inside the window");
        }

        h.wait_for_since(mark, "the prepared item's EndOfStream", |event, generation| {
            matches!(event, PlaybinEvent::EndOfStream) && generation == prepared_generation
        });
        h.shutdown();
    }
}
