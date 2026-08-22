//! One FLUSHING push must not freeze a whole adaptive item.
//!
//! `adaptivedemux2` serves every track from one output task. Without
//! the fork's `adaptivedemux2-transient-flushing-no-permanent-pause` patch,
//! any non-OK `gst_pad_push` there pauses the task for good, and
//! `GST_FLOW_FLUSHING` posts no error at all, so one refused text push kills
//! video and audio output with the element still PLAYING and position pinned.
//!
//! The condition is injected exactly, not raced into existence. A buffer probe
//! on the demuxer's own subtitle pad returns `Handled` with the flow set to
//! `Flushing` for one buffer, which is what `gst_pad_push` hands back to the
//! output loop. No real flush event goes anywhere, so the demuxer's own
//! flushing flag stays false, precisely the case the patch recovers from.
//!
//! This is the demuxer half only. In the field the FLUSHING can come from a
//! downstream multiqueue that latches it independently, where the patch buys
//! "A/V keeps playing" but not the text track. Here the probe consumes the
//! buffer before any multiqueue sees it, so the text track must resume as
//! well and the test asserts that too.
//!
//! Gated on the patched plugin. `FCAST_PATCHED_ADAPTIVEDEMUX2` must name the
//! `.so` that `dashdemux2` actually resolves to, which
//! `cargo xtask patched-plugins` arranges. Against unpatched
//! GStreamer this test is red by design, so a plain `cargo test` skips it
//! instead of reporting an upstream bug as a local one.
//! `FCAST_ADAPTIVEDEMUX2_FLUSHING_TEST_FORCE=1` runs it anyway.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget, state_machine::Seek,
};
use fcasttest::sink::{FTestSink, Recording};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

mod support;

/// Bound for anything the pipeline has to reach. Every segment is a real HTTP
/// round trip, so this is looser than the `ftestsrc` suites need.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Bound for the liveness assertion, which has to fetch and decode ~35 s of
/// media through synced sinks.
const LIVENESS_BOUND: Duration = Duration::from_secs(75);

const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// How long the rendered-frame count may stay flat before playback counts as
/// frozen. Everything is served from loopback, so nothing legitimate stalls
/// this long.
const FLAT_LIMIT: Duration = Duration::from_secs(25);

/// Frame rate of the generated fixture, see `tests/support/gen-dash.sh`.
const FPS: usize = 15;

/// How far playback must get before a test believes it is not frozen.
const WATERMARK: usize = 35 * FPS;

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

/// The patched `.so`, but only if `dashdemux2` really comes from it.
///
/// A plugin that fails to load silently falls back to the system copy, which
/// would make this test red against upstream code while looking configured.
/// So compare what the registry resolved against what the env promised.
fn patched_adaptivedemux2() -> Result<PathBuf, String> {
    let want = std::env::var_os("FCAST_PATCHED_ADAPTIVEDEMUX2")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "FCAST_PATCHED_ADAPTIVEDEMUX2 is unset".to_owned())?;
    let factory = gst::ElementFactory::find("dashdemux2")
        .ok_or_else(|| "no dashdemux2 element factory at all".to_owned())?;
    let have = factory
        .plugin()
        .and_then(|plugin| plugin.filename())
        .ok_or_else(|| "dashdemux2 has no plugin file".to_owned())?;
    let canonical = |path: &PathBuf| path.canonicalize().unwrap_or_else(|_| path.clone());
    if canonical(&have) != canonical(&want) {
        return Err(format!(
            "dashdemux2 resolves to {} instead of {}",
            have.display(),
            want.display()
        ));
    }
    Ok(want)
}

/// `true` when the test should run. Prints loudly and returns `false`
/// otherwise, since an unpatched plugin makes this red by design.
fn gated_in() -> bool {
    init();
    match patched_adaptivedemux2() {
        Ok(path) => {
            println!(
                "running against the patched adaptivedemux2: {}",
                path.display()
            );
            true
        }
        Err(why) => {
            if std::env::var_os("FCAST_ADAPTIVEDEMUX2_FLUSHING_TEST_FORCE").is_some() {
                println!(
                    "!! FORCED: {why}. Expect a freeze, this is the unpatched \
                     half of the A/B."
                );
                return true;
            }
            println!(
                "\n\
                 ================================================================\n\
                 SKIPPING regression_adaptivedemux_flushing: {why}.\n\
                 This test needs the patched adaptivedemux2 plugin:\n\
                 \x20  eval \"$(cargo xtask patched-plugins --quiet)\"\n\
                 Unpatched GStreamer freezes here BY DESIGN, so the suite\n\
                 skips instead of reporting an upstream bug as a local one.\n\
                 ================================================================\n"
            );
            false
        }
    }
}

/// Every text payload crossing the overlay's subtitle input.
type TextTap = text_arm::CueTap;

fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> usize {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|(payload, _)| payload.trim_start().starts_with(prefix))
        .count()
}

/// State of the one-shot FLUSHING injection.
struct Injection {
    /// Buffers the demuxer pushed on its subtitle pad, ours included.
    seen: Arc<AtomicUsize>,
    /// The next buffer gets FLUSHING. Cleared by the probe that does it.
    armed: Arc<AtomicBool>,
    /// The output loop has been handed its FLUSHING.
    fired: Arc<AtomicBool>,
}

impl Injection {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    fn seen(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }
}

/// A playbin whose sinks record, plus every event its callback produced.
/// The parts of `tests/dash_testbed.rs`'s harness this scenario needs, plus a
/// message hook that keeps the element warnings the patch posts.
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    buffering: Cell<i32>,
    loading: Cell<bool>,
    wants_playing: Cell<bool>,
    parked_seek: Cell<Option<Seek>>,
    video: Recording,
    /// `(source, debug)` of every WARNING that reached the bus.
    warnings: Arc<Mutex<Vec<(String, String)>>>,
    _audio: Arc<Mutex<Vec<Recording>>>,
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
        // The crate's Warning event carries only the generic GError message.
        // The patch identifies itself in the debug string, so read the raw
        // message.
        let warnings: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = warnings.clone();
        let hook = Box::new(move |msg: &gst::Message| {
            if let gst::MessageView::Warning(warning) = msg.view() {
                let source = msg
                    .src()
                    .map(|src| src.path_string().to_string())
                    .unwrap_or_default();
                recorder
                    .lock()
                    .expect("warning log")
                    .push((source, warning.debug().unwrap_or_default().to_string()));
            }
            false
        });
        playbin.set_event_handler(Some(hook), move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin: Arc::new(playbin),
            events,
            log: RefCell::new(Vec::new()),
            paused: Cell::new(false),
            buffering: Cell::new(100),
            loading: Cell::new(false),
            wants_playing: Cell::new(false),
            parked_seek: Cell::new(None),
            video,
            warnings,
            _audio: audio,
        }
    }

    /// Put back the transport the crate parked: a parked seek first, then the
    /// PLAYING target the pipeline dropped when a branch joined.
    fn redrive_transport(&self, event: &PlaybinEvent) {
        match event {
            PlaybinEvent::Buffering(percent) => self.buffering.set(*percent),
            PlaybinEvent::Loaded { .. } => self.loading.set(false),
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

    /// A real adaptive source buffers, so quiescence cannot be hardcoded.
    /// Claiming it through a buffering dip invites a re-emit flush into the
    /// very loop this test is about.
    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: self.buffering.get() >= 100 && !self.loading.get(),
            paused: self.paused.get(),
            seekable: self.seekable(),
        }
    }

    fn seekable(&self) -> bool {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        self.playbin.pipeline().query(&mut query) && query.result().0
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok((event, _generation)) = self.events.try_recv() {
            self.redrive_transport(&event);
            self.log.borrow_mut().push(event);
        }
    }

    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) {
        self.drain_events();
        for event in self.log.borrow().iter() {
            if pred(event) {
                return;
            }
        }
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what} within {EVENT_TIMEOUT:?}; log: {:#?}",
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
                    self.redrive_transport(&event);
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
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn load_and_play(&self, uri: &str) {
        self.drain_events();
        self.log.borrow_mut().clear();
        self.wants_playing.set(false);
        self.parked_seek.set(None);
        self.loading.set(true);
        self.buffering.set(100);
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        self.playbin.play().expect("play");
        self.paused.set(false);
        self.wants_playing.set(true);
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

    /// Stream ids of `kind` across every collection advertised so far. An
    /// adaptive input advertises its tracks over SEVERAL collections and a
    /// later one may not carry the earlier tracks.
    fn collection_ids(&self, kind: gst::StreamType) -> Vec<String> {
        self.drain_events();
        let mut ids = Vec::new();
        for event in self.log.borrow().iter() {
            let PlaybinEvent::StreamCollection(collection) = event else {
                continue;
            };
            for stream in collection.iter().filter(|s| s.stream_type() == kind) {
                if let Some(sid) = stream.stream_id() {
                    let sid = sid.to_string();
                    if !ids.contains(&sid) {
                        ids.push(sid);
                    }
                }
            }
        }
        ids
    }

    fn embedded_text_sid(&self) -> String {
        let mut found = None;
        self.wait_until(
            "the embedded text stream to be advertised",
            EVENT_TIMEOUT,
            || {
                found = self
                    .collection_ids(gst::StreamType::TEXT)
                    .into_iter()
                    .next();
                found.is_some()
            },
        );
        found.expect("set by the predicate above")
    }

    fn saw_eos(&self) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::EndOfStream))
    }

    fn tap_overlay_text(&self) -> TextTap {
        text_arm::tap_cue_payloads(&self.playbin)
    }

    /// Ask for a subtitle target. Deliberately does not wait for a
    /// STREAMS_SELECTED. decodebin3 never posts one in upstream-selection
    /// mode, which every adaptive input puts it in.
    fn request_subtitle(&self, target: TrackTarget) {
        self.playbin.request_track(TrackSlot::Subtitle, target);
        self.settle_pump();
    }

    fn video_buffers(&self) -> usize {
        self.video.buffer_count()
    }

    /// The demuxer's own text output pad, which IS the `OutputSlot` pad the
    /// output loop pushes on (`adaptivedemux2` names them `subtitle_%02u`).
    /// Probing anything further downstream would not reach the loop.
    fn demuxer_subtitle_pad(&self) -> gst::Pad {
        let mut found = None;
        self.wait_until("dashdemux2 to expose a subtitle pad", EVENT_TIMEOUT, || {
            let mut elements = self.playbin.pipeline().iterate_recurse();
            while let Ok(Some(element)) = elements.next() {
                if !element
                    .factory()
                    .is_some_and(|factory| factory.name() == "dashdemux2")
                {
                    continue;
                }
                let mut pads = element.iterate_src_pads();
                while let Ok(Some(pad)) = pads.next() {
                    if pad.name().starts_with("subtitle_") {
                        found = Some(pad);
                        return true;
                    }
                }
            }
            false
        });
        let pad = found.expect("set by the predicate above");
        if let Some(caps) = pad.current_caps() {
            let media = caps
                .structure(0)
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            assert!(
                media.starts_with("text/") || media.contains("subtitle"),
                "{} is not a text pad, its caps are {caps}",
                pad.name()
            );
        }
        pad
    }

    /// Arm one FLUSHING return into the output loop. `Handled` consumes the
    /// buffer and makes `gst_pad_push` return the flow the probe set, the
    /// target condition with no real flush anywhere.
    fn inject_one_flushing_text_push(&self) -> Injection {
        let injection = Injection {
            seen: Arc::new(AtomicUsize::new(0)),
            armed: Arc::new(AtomicBool::new(false)),
            fired: Arc::new(AtomicBool::new(false)),
        };
        let seen = injection.seen.clone();
        let armed = injection.armed.clone();
        let fired = injection.fired.clone();
        self.demuxer_subtitle_pad()
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                seen.fetch_add(1, Ordering::SeqCst);
                if armed.swap(false, Ordering::SeqCst) {
                    info.flow_res = Err(gst::FlowError::Flushing);
                    fired.store(true, Ordering::SeqCst);
                    return gst::PadProbeReturn::Handled;
                }
                gst::PadProbeReturn::Ok
            })
            .expect("probing the demuxer's subtitle pad");
        injection
    }

    fn flushing_warnings(&self) -> Vec<(String, String)> {
        self.warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|(_, debug)| debug.contains("FLUSHING"))
            .cloned()
            .collect()
    }

    /// The liveness assertion this whole file exists for. Playback must reach
    /// [`WATERMARK`] frames or end first. A frozen `adaptivedemux2` output
    /// loop posts nothing at all, so the count going flat IS the signal.
    fn assert_reaches_watermark(&self, what: &str) {
        let deadline = Instant::now() + LIVENESS_BOUND;
        let mut best = 0usize;
        let mut best_at = Instant::now();
        loop {
            self.drain_events();
            if self.video_buffers() >= WATERMARK || self.saw_eos() {
                return;
            }
            if self.video_buffers() > best {
                best = self.video_buffers();
                best_at = Instant::now();
            }
            let flat = best_at.elapsed();
            assert!(
                flat < FLAT_LIMIT && Instant::now() < deadline,
                "playback froze ({what}): {best} video buffers (~{:.1} s of media), \
                 flat for {:.1} s, watermark {WATERMARK}; warnings: {:?}; log: {:#?}",
                best as f64 / FPS as f64,
                flat.as_secs_f64(),
                self.warnings.lock().expect("warning log"),
                self.log.borrow()
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_cue(
        &self,
        server: &support::FileServer,
        tap: &TextTap,
        prefix: &str,
        already: usize,
        what: &str,
    ) -> usize {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.drain_events();
            let seen = tapped_with_prefix(tap, prefix);
            if seen > already {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "no {prefix} cue reached the renderer ({what}); text tail peers {:?}; \
                 video buffers {}; embedded.vtt fetches {}; log: {:#?}",
                text_arm::text_tail_pads(&self.playbin)
                    .iter()
                    .map(|pad| pad.peer().map(|peer| peer.name().to_string()))
                    .collect::<Vec<_>>(),
                self.video_buffers(),
                server.fetches("embedded.vtt"),
                self.log.borrow()
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(&self) {
        self.wants_playing.set(false);
        self.parked_seek.set(None);
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

/// The fixture tree plus a server rooted at it.
fn serve() -> (support::FileServer, std::path::PathBuf) {
    init();
    let root = support::fixtures();
    let server = support::FileServer::serve(root.clone());
    (server, root)
}

/// Select the EMBEDDED text track (the one that comes out of the output loop),
/// render a cue, then make exactly one of its pushes return FLUSHING. Video
/// must keep advancing to the watermark, and the element must have said so.
///
/// The text track is selected once and never cycled off. An embedded text
/// track may not come back after an off/on (a separate defect), which would
/// mask this one.
#[test]
fn one_flushing_text_push_does_not_freeze_the_adaptive_output_loop() {
    if !gated_in() {
        return;
    }
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );

    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    harness.request_subtitle(TrackTarget::Stream(Some(sid)));
    let cues = harness.wait_for_cue(&server, &tap, "EMB", 0, "the embedded text track");

    // A rendered cue proves the output loop is pushing text, so the injection
    // lands in steady playback rather than during setup.
    let injection = harness.inject_one_flushing_text_push();
    let before = harness.video_buffers();
    injection.arm();
    harness.wait_until("the injected FLUSHING push", EVENT_TIMEOUT, || {
        injection.fired()
    });

    harness.assert_reaches_watermark("after one FLUSHING text push");
    let warnings = harness.flushing_warnings();
    assert!(
        !warnings.is_empty(),
        "playback survived but the element posted no warning about the \
         discarded push, so the condition would still be invisible in the \
         field ({} text buffers seen, video {} -> {})",
        injection.seen(),
        before,
        harness.video_buffers()
    );
    // The probe consumed the buffer before any multiqueue saw it, so nothing
    // downstream is latched and text has to come back too.
    assert!(
        tapped_with_prefix(&tap, "EMB") > cues,
        "no further EMB cue after the discarded push ({cues} before, {} text \
         buffers seen on the demuxer pad)",
        injection.seen()
    );
    harness.shutdown();
}
