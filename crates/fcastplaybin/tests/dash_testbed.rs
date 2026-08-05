//! The crate against a REAL adaptive demuxer: `dashdemux2` serving a local
//! DASH VOD over loopback HTTP.
//!
//! Every other suite here drives `ftestsrc`, which pushes each elementary
//! stream from its own task. An adaptive demuxer does not: `adaptivedemux2`
//! serves EVERY track from ONE output loop and pauses that loop for good on
//! any non-OK push, posting nothing. So one refused push on a track nobody is
//! watching freezes video and audio too, and the field signature is a
//! position that stops seconds in while the state stays Playing. That failure
//! mode cannot be built out of `ftestsrc`.
//!
//! `tests/regression_upstream_selection_extsub.rs` models an adaptive input
//! with `ScenarioBuilder::upstream_selection` and had to `#[ignore]` its
//! embedded-plus-external case: an `ftestsrc` answering SELECTABLE makes
//! decodebin3 drop its own parsebins' collections and abort, "a shape real
//! adaptive demuxers never produce". `dash_embedded_then_external_switch`
//! below is that scenario against the real thing.
//!
//! Two subtitle sources, and the difference matters:
//!
//! * EXTERNAL: `external/subs-a.vtt` and `external/subs-b.vtt`, named by no
//!   manifest and reached only because the caller called `attach_subtitle`.
//!   Each gets its own `urisourcebin` into a decodebin3 request pad.
//! * EMBEDDED: a `text/vtt` AdaptationSet in `vod/manifest-text.mpd`, so the
//!   text arrives out of `adaptivedemux2`'s output loop, which is the loop the
//!   freeze pauses.
//!
//! Cue payloads are prefixed `EXTA`/`EXTB`/`EMB` so the overlay tap proves
//! which source rendered rather than merely that something did.
//!
//! # What this suite does NOT exercise
//!
//! `Inner::flush_live_text_branches` (lever
//! `FCAST_NO_TEXT_FLUSH_FEEDER_HOLD`) is unreachable through a DASH input, so
//! nothing here covers it. An adaptive demuxer answers SELECTABLE, which makes
//! `upstream_owns_selection` true, and `pump_selection` then routes every
//! subtitle transition to the PARK arm. Measured over full runs of both switch
//! tests: zero flushes queued, one park per transition. Covering that flush
//! needs a NON-adaptive input, which the `ftestsrc` suites already have.
//!
//! The hold that does guard this path is the DROP probe in
//! `detach_text_parts`, lever `FCAST_NO_DETACH_DROP_PROBE`.
//!
//! Fixtures are generated on demand by `tests/support/gen-dash.sh` into the
//! gitignored `target/dash-fixtures`, and served by the std-only file server
//! in `tests/support/mod.rs`.

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks,
    StartPoint, TrackSlot, TrackTarget, state_machine::Seek,
};
use fcasttest::sink::{FTestSink, Recording};
use gst::prelude::*;

mod support;

/// Bound for anything the pipeline has to reach. Every segment is a real HTTP
/// round trip, so this is looser than the `ftestsrc` suites need.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Bound for the liveness assertions, which have to fetch and decode ~35 s of
/// media through synced sinks.
const LIVENESS_BOUND: Duration = Duration::from_secs(75);

const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// How long the rendered-frame count may stay flat before playback counts as
/// frozen. Everything is served from loopback, so nothing legitimate stalls
/// this long.
const FLAT_LIMIT: Duration = Duration::from_secs(25);

/// Frame rate of the generated fixture, see `tests/support/gen-dash.sh`.
const FPS: usize = 15;

/// How far playback must get before a test believes it is not frozen. The
/// field freezes park within the first few seconds, so 35 s of media is well
/// clear of them.
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

/// Every text payload crossing the overlay's subtitle input.
type TextTap = Arc<Mutex<Vec<String>>>;

fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> usize {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|payload| payload.trim_start().starts_with(prefix))
        .count()
}

/// A playbin whose sinks record, plus every event its callback produced.
/// Modelled on the harness in `tests/external_subtitle_lifecycle.rs`.
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    /// Last buffering percent the crate reported, 100 = nothing buffering.
    buffering: Cell<i32>,
    /// A load is in flight, so the transport is not settled running.
    loading: Cell<bool>,
    wants_playing: Cell<bool>,
    parked_seek: Cell<Option<Seek>>,
    video: Recording,
    /// (step, rendered video buffers), printed only by a failure.
    marks: RefCell<Vec<(String, usize)>>,
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
        playbin.set_event_handler(None, move |event, generation| {
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
            marks: RefCell::new(Vec::new()),
            _audio: audio,
        }
    }

    /// Put back the transport the crate parked. Same contract as the lifecycle
    /// suite's re-drive: a parked seek first, then the PLAYING target the
    /// pipeline dropped when a branch joined and lost state.
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

    /// The gate the receiver would report. Unlike the `ftestsrc` suites this
    /// one CANNOT hardcode `quiet: true`: a real adaptive source buffers, and
    /// claiming quiescence through a buffering dip invites the engine to
    /// dispatch a re-emit flush (a seek) into it. Buffering percent comes off
    /// the event stream, seekability from the pipeline itself.
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

    fn load(&self, uri: &str) {
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
    }

    fn load_and_play(&self, uri: &str) {
        self.load(uri);
        self.play();
    }

    fn play(&self) {
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
    /// adaptive input advertises its tracks over SEVERAL collections (the text
    /// AdaptationSet arrives after video and audio), and a later one may not
    /// carry the earlier tracks, so taking only the newest loses streams.
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

    /// Wait for the collection to advertise a text stream and return its id.
    /// The text AdaptationSet arrives in a later collection than video/audio.
    fn embedded_text_sid(&self) -> String {
        let mut found = None;
        self.wait_until("the embedded text stream to be advertised", EVENT_TIMEOUT, || {
            found = self.collection_ids(gst::StreamType::TEXT).into_iter().next();
            found.is_some()
        });
        found.expect("set by the predicate above")
    }

    fn saw_eos(&self) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::EndOfStream))
    }

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
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn overlay_subtitle_pad(&self) -> gst::Pad {
        self.overlay()
            .static_pad("subtitle_sink")
            .expect("subtitleoverlay has a subtitle_sink pad")
    }

    fn tap_overlay_text(&self) -> TextTap {
        let seen: TextTap = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        self.overlay_subtitle_pad()
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                    if let Ok(map) = buffer.map_readable() {
                        recorder
                            .lock()
                            .expect("text tap")
                            .push(String::from_utf8_lossy(map.as_slice()).into_owned());
                    }
                }
                gst::PadProbeReturn::Ok
            })
            .expect("tapping the overlay's subtitle input");
        seen
    }

    fn attach_and_materialize(&self, uri: &str) -> ExternalSubId {
        let id = self
            .playbin
            .attach_subtitle(uri)
            .expect("attaching the external input");
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if !self.playbin.subtitle_stream_ids(id).is_empty() {
                return id;
            }
            assert!(
                Instant::now() < deadline,
                "the external stream {id:?} never materialized; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Ask for a subtitle target. Deliberately does NOT wait for a
    /// STREAMS_SELECTED: decodebin3 never posts one in upstream-selection
    /// mode, which is the mode every adaptive input puts it in. A rendered cue
    /// is the confirmation, see `regression_upstream_selection_extsub.rs`.
    fn request_subtitle(&self, target: TrackTarget) {
        self.playbin.request_track(TrackSlot::Subtitle, target);
        self.settle_pump();
    }

    fn video_buffers(&self) -> usize {
        self.video.buffer_count()
    }

    /// The liveness assertion this whole suite exists for: playback reaches
    /// [`WATERMARK`] frames, or ends first. A frozen `adaptivedemux2` output
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
            // Everything is on loopback, so a count flat for FLAT_LIMIT is a
            // frozen output loop, not slow I/O. Judging it there rather than
            // at the full bound keeps a red run cheap.
            let flat = best_at.elapsed();
            assert!(
                flat < FLAT_LIMIT && Instant::now() < deadline,
                "playback froze ({what}): {best} video buffers (~{:.1} s of media), \
                 flat for {:.1} s, watermark {WATERMARK}; marks: {:?}; log: {:#?}",
                best as f64 / FPS as f64,
                flat.as_secs_f64(),
                self.marks.borrow(),
                self.log.borrow()
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Record the video count at a named step, WITHOUT waiting for anything.
    /// A waiting per-phase liveness check would settle the pipeline between
    /// switches and relieve exactly the race this suite is here to catch, so
    /// the marks are only printed by the failure path, where they show which
    /// step the count went flat at.
    fn mark(&self, what: &str) {
        self.marks
            .borrow_mut()
            .push((what.to_owned(), self.video_buffers()));
    }

    /// Wait for a cue whose payload starts with `prefix` beyond `already`.
    ///
    /// The failure path reports the subtitle files the server was actually
    /// asked for, which separates "nothing re-fetched the track" from
    /// "fetched and then lost somewhere downstream".
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
                "no {prefix} cue reached the overlay ({what}); subtitle_sink peer {:?}; \
                 video buffers {}; vtt fetches: embedded={} a={} b={}; log: {:#?}",
                self.overlay_subtitle_pad()
                    .peer()
                    .map(|peer| peer.name().to_string()),
                self.video_buffers(),
                server.fetches("embedded.vtt"),
                server.fetches("subs-a.vtt"),
                server.fetches("subs-b.vtt"),
                self.log.borrow()
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Turn the subtitle slot off and wait for the text branch to let go of
    /// the overlay seat, which is what a park does.
    fn subtitle_off(&self) {
        self.request_subtitle(TrackTarget::Stream(None));
        self.wait_until("the text branch to park", EVENT_TIMEOUT, || {
            self.overlay_subtitle_pad().peer().is_none()
        });
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

/// Plain DASH, no subtitles anywhere: the sanity floor. If this is red the
/// rest of the suite says nothing about the crate.
#[test]
fn dash_plays_past_the_watermark() {
    let (server, _root) = serve();
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest.mpd"));
    harness.assert_reaches_watermark("plain dash");
    harness.shutdown();
}

/// A genuinely external `.vtt` attached and selected as early as a caller can,
/// with cues from t=0. Both halves matter: the cues must render AND the item
/// must keep moving.
#[test]
fn dash_with_external_sub_selected_at_load_does_not_freeze() {
    let (server, _root) = serve();
    let harness = Harness::new();
    harness.load(&server.url("vod/manifest.mpd"));
    let tap = harness.tap_overlay_text();
    let id = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    harness.request_subtitle(TrackTarget::ExternalSubtitle(id));
    harness.play();
    harness.wait_for_cue(&server, &tap, "EXTA", 0, "external selected at load");
    harness.assert_reaches_watermark("external sub selected at load");
    harness.shutdown();
}

/// Five off/on cycles on the external. Every transition runs the crate's eager
/// text work against a branch that is still being fed, which is the race that
/// pauses `adaptivedemux2`'s output loop for good.
#[test]
fn dash_external_sub_switch_cycle() {
    let (server, _root) = serve();
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest.mpd"));
    let tap = harness.tap_overlay_text();
    let id = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    harness.request_subtitle(TrackTarget::ExternalSubtitle(id));
    let mut seen = harness.wait_for_cue(&server, &tap, "EXTA", 0, "the first selection");

    for cycle in 0..5 {
        harness.subtitle_off();
        harness.request_subtitle(TrackTarget::ExternalSubtitle(id));
        seen = harness.wait_for_cue(&server, &tap, "EXTA", seen, &format!("cycle {cycle}"));
    }

    harness.assert_reaches_watermark("after the switch cycles");
    harness.shutdown();
}

/// Text to text with NO "off" in between, the field sequence from
/// `regression_upstream_selection_extsub.rs`. Five round trips between two
/// attached externals, each one replacing a text branch that is still being
/// fed.
///
/// This does NOT reach `Inner::flush_live_text_branches`, and neither does any
/// other test here. Measured with `FCASTPLAYBIN_TEST_LOG=debug`: an adaptive
/// input answers SELECTABLE, so `upstream_owns_selection` is true, and
/// `pump_selection` sends every replace to the PARK arm instead of the flush
/// (`src/lib.rs`, the `replacing && upstream_owns_selection()` branch). Zero
/// "queueing the eager text-branch flush" lines over a whole run, ten "parked
/// text stream" ones. So `FCAST_NO_TEXT_FLUSH_FEEDER_HOLD` is inert on DASH,
/// and the hold that matters on this path is the one in `detach_text_parts`,
/// lever `FCAST_NO_DETACH_DROP_PROBE`.
#[test]
fn dash_external_to_external_direct_switch() {
    let (server, _root) = serve();
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest.mpd"));
    let tap = harness.tap_overlay_text();
    harness.mark("before attaching");
    let first = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    harness.mark("attached a");
    let second = harness.attach_and_materialize(&server.url("external/subs-b.vtt"));
    harness.mark("attached b");

    harness.request_subtitle(TrackTarget::ExternalSubtitle(first));
    let mut a = harness.wait_for_cue(&server, &tap, "EXTA", 0, "the first external");
    harness.mark("selected a");
    let mut b = 0;

    for cycle in 0..5 {
        harness.request_subtitle(TrackTarget::ExternalSubtitle(second));
        b = harness.wait_for_cue(&server, &tap, "EXTB", b, &format!("switch to b, cycle {cycle}"));
        harness.mark(&format!("switched to b, cycle {cycle}"));
        harness.request_subtitle(TrackTarget::ExternalSubtitle(first));
        a = harness.wait_for_cue(&server, &tap, "EXTA", a, &format!("switch to a, cycle {cycle}"));
        harness.mark(&format!("switched to a, cycle {cycle}"));
    }

    harness.assert_reaches_watermark("after the direct switches");
    harness.shutdown();
}

/// The EMBEDDED track: text out of `adaptivedemux2`'s own output loop rather
/// than a side input.
///
/// RED, reproducibly (3 runs, 3 failures). The track renders on the first
/// selection and never again after one off/on: no EMB cue reaches the overlay
/// on cycle 0, while video keeps advancing past 600 buffers and nothing errors.
/// It is not a track nobody re-requested either, the server counts THREE
/// `embedded.vtt` fetches, and `subtitle_sink` has a peer at the point of
/// failure. So the re-selected text is fetched and then lost between the
/// demuxer and subtitleoverlay.
///
/// Before filing this against the crate, re-check it with a SEGMENTED text
/// AdaptationSet. The fixture uses one unsegmented `text/vtt` Representation
/// (see `tests/support/gen-dash.sh` for why ffmpeg cannot produce the muxed
/// form), which is legal DASH but not the common shape, and a whole-period
/// text segment may re-activate differently from a segmented one.
#[test]
#[ignore = "RED: an embedded DASH text track never renders again after one \
            off/on, though the vtt is re-fetched. Needs triage against a \
            segmented text AdaptationSet first, see the doc comment."]
fn dash_embedded_text_track_plays() {
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
    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    let mut seen = harness.wait_for_cue(&server, &tap, "EMB", 0, "the embedded text track");

    for cycle in 0..5 {
        harness.subtitle_off();
        harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
        seen = harness.wait_for_cue(&server, &tap, "EMB", seen, &format!("embedded cycle {cycle}"));
    }

    harness.assert_reaches_watermark("embedded text track");
    harness.shutdown();
}

/// The FIELD shape from the receiver log: a DIRECT embedded-to-external
/// subtitle switch on an adaptive item, with no "off" in between (which is what
/// blocks `dash_embedded_then_external_switch`) and with a SECOND external
/// attached, so decodebin3 has another text output to prefer.
///
/// What the field log showed at the switch: the embedded input drained, the
/// crate kept the external sid out of the SELECT_STREAMS (upstream-selection
/// mode), and the selected external turned out to have NO decodebin3 output pad
/// at all ("no routed text pad carries the allowed sid", the other external
/// holding text_1). decodebin3 removes a multiqueue slot when its input drains
/// (`remove_slot_from_streaming_thread`, gstdecodebin3.c:3717) and only creates
/// one from a parsebin `pad-added`, so a drained external is SLOTLESS for good:
/// four replays died "reason not-linked" and the crate then gave up silently
/// while still reporting the track as selected.
///
/// `#[ignore]`d for two independent reasons, both honest: a fifth concurrent
/// real-time DASH pipeline destabilizes this suite (measured: one SIGABRT at
/// teardown and one `switch_cycle` failure over two runs, 4/4 clean without it),
/// and it MEASURABLY does not reproduce the field's slot REMOVAL precondition:
/// a debug run logs zero "no decodebin3 slot" and zero "reason not-linked"
/// lines, because the external's input stays connected here, so `slot->input`
/// is non-NULL and decodebin3 keeps the slot across the drain. Reaching the
/// field state needs the input DISCONNECTED as well as drained. Kept as
/// coverage for the direct switch itself, which nothing else here exercises.
/// Run it standalone:
/// `cargo test -p fcastplaybin --test dash_testbed -- --ignored --exact \
///  dash_embedded_then_external_direct_switch`
#[test]
#[ignore = "standalone only (a 5th parallel DASH pipeline destabilizes this \
            suite); models the field's direct embedded->external switch"]
fn dash_embedded_then_external_direct_switch() {
    let (server, root) = serve();
    assert!(support::has_embedded_text(&root), "no embedded-text manifest");
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    // Two externals, like the field item: the second gives decodebin3 another
    // text output to hold while the first is the one being switched to.
    let first = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    let _second = harness.attach_and_materialize(&server.url("external/subs-b.vtt"));

    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    harness.wait_for_cue(&server, &tap, "EMB", 0, "the embedded track first");
    harness.mark("embedded rendering");

    // The externals have long since drained (90 s of cues pushed as fast as the
    // source can), so this is a switch onto a spent input: the field's state.
    harness.request_subtitle(TrackTarget::ExternalSubtitle(first));
    harness.wait_for_cue(&server, &tap, "EXTA", 0, "the direct switch to the external");
    harness.assert_reaches_watermark("after the direct embedded to external switch");
    harness.shutdown();
}

/// Embedded and external on the same adaptive item, switched between: the
/// scenario `regression_upstream_selection_extsub.rs` had to `#[ignore]`
/// because `ftestsrc` cannot model an adaptive demuxer well enough to reach
/// it. Each switch replaces a text branch fed by a DIFFERENT input, which is
/// the widest version of the race.
///
/// RED for the same reason as `dash_embedded_text_track_plays`, and it fails at
/// the same step (the embedded track after its first off), so it is blocked
/// behind that triage rather than being a second finding.
#[test]
#[ignore = "RED behind dash_embedded_text_track_plays: the embedded track \
            never comes back after its first off, so the external leg is \
            never reached."]
fn dash_embedded_then_external_switch() {
    let (server, root) = serve();
    assert!(support::has_embedded_text(&root), "no embedded-text manifest");
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    let id = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));

    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    let mut embedded = harness.wait_for_cue(&server, &tap, "EMB", 0, "embedded first");
    let mut external = 0;

    for cycle in 0..3 {
        harness.subtitle_off();
        harness.request_subtitle(TrackTarget::ExternalSubtitle(id));
        external =
            harness.wait_for_cue(&server, &tap, "EXTA", external, &format!("external {cycle}"));
        harness.subtitle_off();
        harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
        embedded = harness.wait_for_cue(&server, &tap, "EMB", embedded, &format!("embedded {cycle}"));
    }

    harness.assert_reaches_watermark("after embedded/external switches");
    harness.shutdown();
}
