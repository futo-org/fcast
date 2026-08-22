//! The crate against a real adaptive demuxer: `dashdemux2` serving a local
//! DASH VOD over loopback HTTP.
//!
//! An adaptive demuxer serves every track from one output loop and pauses
//! that loop for good on any non-OK push, posting nothing. One refused push
//! on an unwatched track therefore freezes video and audio too, a failure
//! mode `ftestsrc` cannot model.
//!
//! Two subtitle sources, and the difference matters:
//!
//! * External: VTT files named by no manifest, reached only via
//!   `attach_subtitle`. Each gets its own `urisourcebin` into a decodebin3
//!   request pad.
//! * Embedded: a `text/vtt` AdaptationSet in the manifest, so the text arrives
//!   out of the demuxer's output loop, the loop the freeze pauses.
//!
//! Cue payloads are prefixed `EXTA`/`EXTB`/`EMB` so the overlay tap proves
//! which source rendered.
//!
//! `Inner::flush_live_text_branches` is unreachable through a DASH input: an
//! adaptive demuxer answers SELECTABLE, so every subtitle transition routes
//! to the park arm. The hold that does guard this path is the drop probe in
//! `detach_text_parts`.
//!
//! Fixtures are generated on demand by `tests/support/gen-dash.sh` into the
//! gitignored `target/dash-fixtures`, and served by the file server in
//! `tests/support/mod.rs`.

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

#[path = "support/census.rs"]
mod census;
#[path = "support/text_arm.rs"]
mod text_arm;

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
/// known freezes park within the first few seconds, so this is well clear of
/// them.
const WATERMARK: usize = 35 * FPS;

/// How far into the item a mid-play first subtitle select is staged. Far
/// enough that the playhead is unambiguously past the period start, early
/// enough to leave most cues still ahead.
const FIRST_SELECT_AT: usize = 5 * FPS;

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
type TextTap = text_arm::CueTap;

/// The media seconds of every `SEG NN` cue delivered so far, in delivery
/// order. The fixture names each cue after the second it covers, so the
/// payload is the only timeline evidence a tap needs.
fn seg_cue_seconds(tap: &TextTap) -> Vec<usize> {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter_map(|(text, _)| {
            text.trim()
                .strip_prefix("SEG")
                .and_then(|rest| rest.trim().parse::<usize>().ok())
        })
        .collect()
}

fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> usize {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|(payload, _)| payload.trim_start().starts_with(prefix))
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
    /// See [`Harness::poll_on_events_only`].
    event_polls_only: Cell<bool>,
    _audio: Arc<Mutex<Vec<Recording>>>,
    /// Checked when the harness drops: every counter whose healthy value is
    /// zero must have stayed flat for the life of this test. One guard per
    /// harness rather than one assertion per test, so a new silent-failure
    /// instrument is covered here the moment it is declared `zero`.
    _census: census::Census,
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
            event_polls_only: Cell::new(false),
            _audio: audio,
            _census: census::Census::arm("this dash harness"),
        }
    }

    /// Poll the text policy the way the shipped receiver does: on the crate's
    /// own edges only, never on a timer.
    ///
    /// [`Self::settle_pump`] normally re-drives the link policy every pump,
    /// which self-heals a whole class of defect the receiver never repairs
    /// because it polls only when something happens. A test reproducing a
    /// field shape must turn the timer polling off, or the harness repairs
    /// the defect before the crate can be asked to. `pump_selection` still
    /// runs, since the receiver drives that on every pump too.
    fn poll_on_events_only(&self) {
        self.event_polls_only.set(true);
    }

    /// Put back the transport the crate parked. A parked seek first, then the
    /// PLAYING target the pipeline dropped when a branch joined and lost
    /// state.
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

    /// The gate the receiver would report. This suite cannot hardcode
    /// `quiet: true`. A real adaptive source buffers, and claiming quiescence
    /// through a buffering dip invites the engine to dispatch a re-emit flush
    /// (a seek) into it.
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
        if !self.event_polls_only.get() {
            self.playbin.poll_text_policy();
        }
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

    /// Pause and wait for the pipeline to settle there. The demuxer keeps
    /// filling downstream buffers, so this is the cheap way to widen the
    /// distance between its output position and the playhead.
    fn pause(&self) {
        self.playbin.pause().expect("pause");
        self.paused.set(true);
        self.wants_playing.set(false);
        self.wait_for("settled PAUSED", |event| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Paused,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        });
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
    /// adaptive input advertises its tracks over several collections, and a
    /// later one may not carry the earlier tracks, so taking only the newest
    /// loses streams.
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

    /// What is wired into the text renderer right now, for a failure message.
    /// Separates "the track was never re-wired" from "it was wired and
    /// delivered nothing".
    fn text_tail_peers(&self) -> Vec<String> {
        text_arm::text_tail_pads(&self.playbin)
            .iter()
            .filter_map(|pad| pad.peer())
            .map(|peer| peer.name().to_string())
            .collect()
    }

    fn tap_overlay_text(&self) -> TextTap {
        text_arm::tap_cue_payloads(&self.playbin)
    }

    /// Every `text_%u` src pad decodebin3 has ever exposed, by name.
    ///
    /// The counter behind those names is monotonic and never reused for a
    /// still-requested stream, so a new name across a re-enable means the
    /// output was replaced and an unchanged list means it was re-used. The
    /// re-enable test branches on that discriminator.
    fn decodebin_text_pads(&self) -> Vec<String> {
        let Some(db3) = self.playbin.pipeline().by_name("fpb-decodebin") else {
            return Vec::new();
        };
        db3.src_pads()
            .iter()
            .map(|pad| pad.name().to_string())
            .filter(|name| name.starts_with("text_"))
            .collect()
    }

    /// Which decodebin3 text pad feeds the consumer tail right now, read off
    /// the live branch's queue, which is named after the pad it serves
    /// (`fpb-tqueue-<pad>`). `None` when no text branch is joined.
    fn seated_text_pad(&self) -> Option<String> {
        text_arm::text_tail_pads(&self.playbin)
            .iter()
            .filter_map(|pad| pad.peer())
            .filter_map(|peer| peer.parent_element())
            .filter_map(|element| {
                element
                    .name()
                    .as_str()
                    .strip_prefix("fpb-tqueue-")
                    .map(str::to_owned)
            })
            .next()
    }

    /// The multiqueue slot behind a decodebin3 text output pad.
    ///
    /// With no decoder to autoplug, decodebin3 ghosts a text output straight
    /// at the slot, so the ghost's target is the multiqueue src pad, whose
    /// `last_flow_result` records the value that becomes `sq->srcresult`. It
    /// is the closest thing to reading the latch from outside the element.
    fn text_slot_behind(&self, pad_name: &str) -> Option<gst::Pad> {
        let db3 = self.playbin.pipeline().by_name("fpb-decodebin")?;
        let ghost = db3
            .src_pads()
            .into_iter()
            .find(|pad| pad.name() == pad_name)?
            .downcast::<gst::GhostPad>()
            .ok()?;
        ghost.target()
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

    /// Ask for a subtitle target. Deliberately does not wait for a
    /// STREAMS_SELECTED, which decodebin3 never posts in upstream-selection
    /// mode. A rendered cue is the confirmation.
    fn request_subtitle(&self, target: TrackTarget) {
        self.playbin.request_track(TrackSlot::Subtitle, target);
        self.settle_pump();
    }

    /// How many times the crate has confirmed `sid` in the subtitle slot.
    fn confirmations_of(&self, sid: &str) -> usize {
        self.drain_events();
        let log = self.log.borrow();
        log.iter()
            .filter(|event| {
                matches!(
                    event,
                    PlaybinEvent::StreamsSelected { subtitle: Some(applied), .. }
                        if applied == sid
                )
            })
            .count()
    }

    /// Ask for a subtitle target and wait until the crate confirms it.
    ///
    /// [`Self::request_subtitle`] pumps the engine exactly once, and the
    /// engine refuses to dispatch through a buffering dip, so a leg staged
    /// with it alone can be skipped entirely, letting a later cue wait be
    /// satisfied by the previous track's stragglers. This is the only thing
    /// that says the switch actually happened.
    fn select_subtitle(&self, target: TrackTarget, sid: &str, what: &str) {
        let before = self.confirmations_of(sid);
        self.request_subtitle(target);
        self.wait_until(
            &format!("the crate to confirm {what}"),
            EVENT_TIMEOUT,
            || self.confirmations_of(sid) > before,
        );
    }

    fn video_buffers(&self) -> usize {
        self.video.buffer_count()
    }

    /// The liveness assertion this whole suite exists for. Playback reaches
    /// [`WATERMARK`] frames, or ends first. A frozen demuxer output loop
    /// posts nothing at all, so the count going flat is the signal.
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
            // frozen output loop, not slow I/O. Judging it early keeps a red
            // run cheap.
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

    /// Let playback run until `buffers` video frames have rendered, so a step
    /// can be staged at a known point mid-play.
    ///
    /// A "get me to t", not a liveness claim, so no flat-count judgement. It
    /// still fails loudly, because the caller's next assertion would
    /// otherwise be made at the wrong point in the stream.
    fn advance_past(&self, buffers: usize, what: &str) {
        self.wait_until(
            &format!("playback to reach {buffers} video buffers ({what})"),
            LIVENESS_BOUND,
            || self.video_buffers() >= buffers || self.saw_eos(),
        );
    }

    /// Record the video count at a named step, without waiting. A waiting
    /// per-phase liveness check would settle the pipeline between switches
    /// and relieve exactly the race this suite is here to catch. Marks are
    /// printed only by the failure path.
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
                "no {prefix} cue reached the renderer ({what}); text tail peers {:?}; \
                 video buffers {}; vtt fetches: embedded={} a={} b={}; log: {:#?}",
                self.text_tail_peers(),
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

    /// Wait for a cue about the present, one whose media second (in the
    /// payload as `SEG NN`) is at or after `from_second`.
    ///
    /// The number matters. A track that restarts its download at the period
    /// start still delivers cues, all for times the playhead has passed,
    /// which renders as no subtitles while looking healthy to a test that
    /// counts arrivals.
    fn wait_for_cue_at_or_after(
        &self,
        server: &support::FileServer,
        tap: &TextTap,
        from_second: usize,
        what: &str,
    ) -> usize {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.drain_events();
            let delivered = seg_cue_seconds(tap);
            if let Some(hit) = delivered.iter().copied().find(|n| *n >= from_second) {
                return hit;
            }
            assert!(
                Instant::now() < deadline,
                "no SEG cue at or after second {from_second} reached the renderer \
                 ({what}); the playhead is at ~{:.1} s and the cues delivered so far \
                 are for seconds {delivered:?}; text tail peers {:?}; vtt fetches: \
                 embedded={} a={} b={}; log: {:#?}",
                self.video_buffers() as f64 / FPS as f64,
                self.text_tail_peers(),
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
            !text_arm::text_branch_linked(&self.playbin)
        });
    }

    /// Tear the current item down and wait for it, so a following load starts
    /// against a pipeline that has finished stopping rather than one still
    /// flushing.
    fn stop(&self) {
        self.wants_playing.set(false);
        self.parked_seek.set(None);
        self.playbin.stop_async();
        self.wait_until("the pipeline to stop", EVENT_TIMEOUT, || {
            self.playbin.pipeline().current_state() <= gst::State::Ready
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

/// Probe an arbitrary dash URI on the default-selection path: load, play, and
/// never touch the subtitle slot.
///
/// A harness for field triage rather than a regression test, so it is ignored
/// and env-driven. It reports whether the default-selected embedded text
/// track ever renders a cue. No prefix matching, since a real stream's cues
/// are whatever language it ships.
#[test]
#[ignore = "field triage: set FCAST_PROBE_URI to a live DASH uri"]
fn probe_default_subtitle_on_a_live_uri() {
    let uri = std::env::var("FCAST_PROBE_URI").expect("set FCAST_PROBE_URI");
    init();
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let tap = harness.tap_overlay_text();
    let deadline = Instant::now() + Duration::from_secs(75);
    let mut best = 0usize;
    while Instant::now() < deadline {
        harness.settle_pump();
        harness.drain_events();
        let seen = tapped_with_prefix(&tap, "");
        if seen > best {
            best = seen;
            eprintln!(
                "PROBE cue #{best} at video={} :: {:?}",
                harness.video_buffers(),
                tap.lock().expect("tap").last().map(|(t, _)| t.clone())
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "PROBE RESULT cues={best} video_buffers={} text_tail_peers={:?} db3_text_pads={:?}",
        harness.video_buffers(),
        harness.text_tail_peers(),
        harness.decodebin_text_pads()
    );
    harness.shutdown();
    assert!(
        best > 0,
        "the default-selected text track rendered no cue at all"
    );
}

/// How long item A plays before item B replaces it. Long enough that A is
/// genuinely PLAYING with branches built and data flowing, so B's
/// construction races A's teardown rather than a still-prerolling pipeline.
const FIRST_ITEM_DWELL: usize = 3 * FPS;

/// Second-load staging of the field's shape: the whole-period-VTT item with
/// its text track default-selected, loaded after another item.
///
/// The suspicion under test is that item A's teardown or the gapless swap
/// touches machinery item B's text branch is still being built on, and one
/// FLUSHING return there latches B's multiqueue slot for the whole item.
///
/// The fixture is the unsegmented manifest: a bare `<BaseURL>` whole-period
/// WebVTT with `default="true"`, so the entire track arrives in one push
/// during exactly the window under suspicion. Lose that push and there is no
/// second chance.
///
/// Text is never requested. Default selection is the whole point.
fn second_load_renders_default_text(what: &str, between: impl FnOnce(&Harness, &str)) {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    // Item A, playing properly, with branches up.
    harness.load_and_play(&server.url("vod/manifest.mpd"));
    harness.advance_past(FIRST_ITEM_DWELL, "item A before the second load");
    harness.mark("item A playing");

    let tap = harness.tap_overlay_text();
    between(&harness, &server.url("vod/manifest-text.mpd"));

    harness.mark("item B loaded");
    harness.wait_for_cue(&server, &tap, "EMB", 0, what);
    harness.assert_reaches_watermark(what);
    harness.shutdown();
}

/// (a) A plain replacing load, which is what a sender does when the user casts
/// something new without stopping first.
#[test]
fn default_text_renders_on_a_replacing_second_load() {
    second_load_renders_default_text(
        "the default text track on a replacing second load",
        |h, uri| {
            h.load_and_play(uri);
        },
    );
}

/// (c) A stop between the two, so item A's teardown has fully run before item
/// B's load starts. If (a) reproduces and this does not, the window is the
/// overlap itself rather than anything B does.
#[test]
fn default_text_renders_on_a_second_load_after_a_stop() {
    second_load_renders_default_text("the default text track on a load after a stop", |h, uri| {
        h.stop();
        h.load_and_play(uri);
    });
}

/// (b) The gapless path. Item B is prepared while item A is still PLAYING,
/// then activated at A's boundary. No teardown is involved, so a failure here
/// and not in (a)/(c) would point at the swap rather than the flush.
///
/// A is driven to its end with a seek rather than played out, since only the
/// boundary matters.
///
/// A prepared adaptive input posts its own streams-selected the moment it is
/// PAUSED. Misread as decodebin3 switching items, the activation adopts the
/// next generation with nothing linked and the demuxer dies of not-linked.
/// The prepared input's self-report is dropped by the adaptive prepare hold;
/// without that hold this fails on the pipeline error before any cue.
#[test]
fn default_text_renders_on_a_gapless_prepared_second_item() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest.mpd"));
    harness.advance_past(FIRST_ITEM_DWELL, "item A before the prepare");

    let tap = harness.tap_overlay_text();
    harness
        .playbin
        .prepare_next_async(fcastplaybin::MediaInput::Uri(
            server.url("vod/manifest-text-seg.mpd"),
        ));
    harness.mark("item B prepared");

    // To the boundary. Land close enough that A ends on its own within the
    // wait below.
    harness.playbin.seek_async(fcastplaybin::Seek::new(
        Some(gst::ClockTime::from_seconds(88)),
        None,
    ));
    harness.wait_for("the gapless activation", |event| {
        matches!(event, PlaybinEvent::PreparedActivated)
    });
    harness.mark("item B activated");

    harness.wait_for_cue(
        &server,
        &tap,
        "SEG",
        0,
        "the default text track after a gapless swap",
    );
    harness.shutdown();
}

/// The control for `default_text_renders_on_a_gapless_prepared_second_item`:
/// the same prepare, with an item that has no text track at all. The defect
/// it guards is the adaptive prepare, not anything about text.
///
/// A stream-aware prepared input posts its own collection and
/// streams-selected at PAUSED. Misread as a switch, the activation adopts the
/// next generation with nothing linked, the playing input is removed, and the
/// demuxer dies of not-linked. This test is the prepare hold's bite proof.
/// Without the prepare hold item A stops dead right after the prepare (it
/// waits on item A's frames because the collateral is what makes the defect
/// unmissable); with it the prepared item sits blocked while A plays on.
#[test]
fn a_gapless_prepared_dash_item_is_linked_before_it_streams() {
    let (_server, root) = serve();
    let server = support::FileServer::serve(root);
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest.mpd"));
    harness.advance_past(FIRST_ITEM_DWELL, "item A before the prepare");
    harness
        .playbin
        .prepare_next_async(fcastplaybin::MediaInput::Uri(
            server.url("vod/manifest.mpd"),
        ));
    // No swap, no seek. Just prove neither item dies while the prepare sits
    // there. The wait is orders of magnitude past the defect's window.
    harness.advance_past(FIRST_ITEM_DWELL + 6 * FPS, "item A after the prepare");
    harness.shutdown();
}

/// The whole-period-VTT item with its embedded text default-selected, and an
/// external subtitle attached alongside it. The embedded track's branch is
/// built while a second input is materializing its own streams into the same
/// decodebin3.
#[test]
fn default_text_survives_an_external_input_arriving_beside_it() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();

    // The second urisourcebin, arriving while the embedded text branch is
    // being built. Never selected. The embedded default keeps the seat.
    harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    harness.mark("external attached beside the default embedded text");

    harness.wait_for_cue(
        &server,
        &tap,
        "EMB",
        0,
        "the default embedded text with an external input alongside",
    );
    harness.assert_reaches_watermark("default embedded text beside an external");
    harness.shutdown();
}

/// How long the staged join holds its branch at NULL. A sparse text track's
/// GAP tick is once a second, so this is comfortably more than one, and the
/// staging is only useful if something is guaranteed to cross the new link
/// while it is still inactive.
const STAGED_JOIN_HOLD: Duration = Duration::from_millis(4000);

/// How late the whole-period text Representation is served. Long enough that
/// its single push lands well after the join and well after PLAYING, so the
/// track's entire data rides on one push into a branch joined and healthy for
/// seconds.
const WHOLE_PERIOD_PUSH_AT: Duration = Duration::from_millis(1500);

/// A text branch joined while its own pads were still inactive, and the
/// multiqueue slot above it latched for the rest of the item.
///
/// On a whole-period text Representation the demuxer pushes the entire track
/// once, so the first push through a latched slot is also the last and the
/// track is gone.
///
/// Real: the fixture, one unsegmented `text/vtt` Representation over a bare
/// `<BaseURL>`, with `delay_path` scheduling its single push into the join
/// window so nothing crosses the slot between the join and the push.
///
/// Staged: the width of the join window. `stage_join_before_active` holds the
/// branch at NULL after its upstream link, widening a microsecond race to
/// something testable. The mechanism is untouched, since an unactivated pad
/// is FLUSHING by construction however long it stays inactive.
/// `multiqueue_slot_unlatch.rs` proves the latch and the repair against that
/// pad directly, with no timing at all.
///
/// Verification: green as shipped; without the slot unlatch it is red on the
/// repair never happening and no EMB cue following it.
///
/// The cue this waits for is one beyond whatever the text park replayed into
/// the join. A replayed cue never crossed the staged link and says nothing
/// about whether the slot recovered.
#[test]
fn a_whole_period_text_track_survives_a_join_that_raced_its_own_activation() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );
    // The one push, scheduled into the join window.
    server.delay_path("embedded.vtt", WHOLE_PERIOD_PUSH_AT);

    let harness = Harness::new();
    harness.playbin.stage_join_before_active(STAGED_JOIN_HOLD);

    let repairs_before = FcastPlaybin::global_stats().slot_unlatches;
    let races_before = FcastPlaybin::global_stats().joins_into_an_inactive_branch;

    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();

    // The join has to have raced, or this test is measuring an ordinary
    // playback and would pass with the repair deleted.
    harness.wait_until("the staged join to land", EVENT_TIMEOUT, || {
        FcastPlaybin::global_stats().joins_into_an_inactive_branch > races_before
    });

    // The latch has to have happened and been repaired, waited for rather
    // than asserted after the cue, because the cue is no longer proof.
    //
    // The park replays cues into the join on this fixture (the demuxer holds
    // the whole bring-up behind the delayed VTT), so waiting for "an EMB cue"
    // would return on a replayed one with the staged link still untouched.
    harness.wait_until(
        "the staged latch to be repaired (unrepaired, the slot is dead for the item and no \
         further EMB cue can follow)",
        EVENT_TIMEOUT,
        || FcastPlaybin::global_stats().slot_unlatches > repairs_before,
    );
    // Everything the park handed over, discounted. A cue beyond this mark can
    // only have come through the joined, and therefore repaired, slot.
    let replayed = tapped_with_prefix(&tap, "EMB");

    // The claim. The track's entire data is one push, it arrives seconds
    // after the join, and it must still render.
    harness.wait_for_cue(
        &server,
        &tap,
        "EMB",
        replayed,
        "the whole-period embedded track after a join that raced its own activation",
    );
    assert_eq!(
        FcastPlaybin::global_stats().slot_unlatch_failures,
        0,
        "a latched slot was found and the repair did not clear it"
    );

    // The discriminator, read straight off the graph rather than a counter.
    // Active and FLUSHING is the latch. Anything else is a slot that can
    // still deliver.
    for pad in harness.decodebin_text_pads() {
        let Some(slot) = harness.text_slot_behind(&pad) else {
            continue;
        };
        assert!(
            !(slot.is_active() && matches!(slot.last_flow_result(), Err(gst::FlowError::Flushing))),
            "the multiqueue slot behind {pad} is still latched after the item played through \
             ({:?})",
            slot.last_flow_result()
        );
    }

    harness.assert_reaches_watermark("after a staged join window");
    harness.shutdown();
}

/// How long after a load the whole-period track's FIRST cue may take to reach
/// the consumer.
///
/// It is the text branch's join that decides this, and the join waits for a
/// settled PAUSED, so the honest bound is "one preroll of a local DASH item".
/// Measured on this fixture at 0.17 s; five seconds is the flake margin over
/// it, and it is still a third of the interval the field reported.
const FIRST_CUE_BOUND: Duration = Duration::from_secs(5);

/// A whole-period text track renders its OPENING cues, not just the ones that
/// arrive after bring-up.
///
/// # The field defect
///
/// "The subtitles start after a few seconds instead of from the beginning."
/// Owner's DEBUG capture: load at 77.18, the collection at 77.236, the text
/// branch joined at 77.597, and the first cue at ~83.7, six and a half
/// seconds in, with everything covering the opening gone. Video and audio play
/// from the first frame throughout, which is what makes it a text-path defect
/// rather than a slow start.
///
/// # Attribution, measured rather than argued
///
/// `probe_first_cue_latency_on_a_whole_period_track` splits the interval and
/// the demuxer is not in it. Against `manifest-text.mpd`, `adaptivedemux2`
/// asks for the whole-period VTT 19 ms after the load, in the same
/// millisecond as the init segments, BEFORE the first media chunk, has the
/// file parsed into its track by 80 ms, and pushes the cue for second 0 at
/// 121 ms, `push returned ok`. Its scheduler has no presentation-time
/// ordering, no sparse-stream concept and no rule that defers a long segment
/// (gstadaptivedemux.c:2869-2889, downloads are submitted as idle callbacks
/// and run concurrently), and the buffering gate cannot fire on a first
/// fragment at all. There is no knob to turn there because there is no delay
/// there.
///
/// The interval is the CRATE's, and it is the text park. `route_db3_pad`
/// links a text pad to a parking sink the instant it appears, because an
/// adaptive demuxer serves every track from one output loop and an unconsumed
/// text pad pins it; the consumer branch is joined later by
/// `poll_text_policy`, which will not link before the pipeline settles at
/// PAUSED. Everything the demuxer pushes in between used to be thrown away.
/// Measured here before the repair: decodebin3's text pad carried pts 0, 1
/// and 2 s within 8 ms of each other, all three into the park, and the first
/// cue the consumer ever saw was the one for second 3.
///
/// The window is milliseconds of WALL clock and seconds of MEDIA, because the
/// demuxer's output position runs ahead of a playhead that has not started
/// yet. A segmented track survives it, the cues come round again. A
/// whole-period Representation is pushed exactly ONCE, so what the park ate
/// is gone for the item. That is why the field's shape is this shape.
///
/// # The claim
///
/// The first cue the consumer receives is the item's FIRST cue. Not "a cue
/// arrived", which was always true and is what let this hide: the fixture
/// names every cue after the media second it covers, so the payload is the
/// whole assertion.
///
/// # Verification
///
/// Green as shipped. With the discarding park back it is RED on the first cue
/// being `EMB 03` rather than `EMB 00`. That is the defect, reproduced.
#[test]
fn a_whole_period_text_track_renders_its_opening_cues() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );

    let harness = Harness::new();
    let replayed_before = FcastPlaybin::global_stats().parked_text_cues_replayed;
    let t0 = Instant::now();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();
    // The CLEAR-AWARE half, and it is not redundant with the payload tap: a
    // cue can be DELIVERED and then wiped by the join's own STREAM_START, and
    // a delivery log cannot tell that from a cue the viewer saw. This reads
    // the window the consumer actually keeps.
    let rendered = text_arm::TextTap::install(&harness.playbin);
    harness.wait_for_cue(&server, &tap, "EMB", 0, "the whole-period track's opening");

    let (first, at) = tap
        .lock()
        .expect("text tap")
        .first()
        .cloned()
        .expect("wait_for_cue returned, so the tap holds a cue");

    // THE CLAIM. `EMB 00` is the cue covering the item's first second; any
    // other payload here is an opening the viewer never saw.
    assert_eq!(
        first.trim(),
        "EMB 00",
        "the first cue delivered was {first:?}, so the whole-period track's opening was lost, \
         the cues before it went into the text park and a whole-period Representation has no \
         second copy of them (cues so far: {:?}, parked cues replayed: {})",
        tap.lock()
            .expect("text tap")
            .iter()
            .take(6)
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>(),
        FcastPlaybin::global_stats().parked_text_cues_replayed - replayed_before,
    );
    let latency = at.saturating_duration_since(t0);
    assert!(
        latency <= FIRST_CUE_BOUND,
        "the opening cue took {latency:?} to reach the consumer, over the {FIRST_CUE_BOUND:?} \
         bound"
    );

    // NOT VACUOUS. The repair only does anything when the demuxer put cues
    // across the pad before the branch joined, which is what the park is for.
    // A zero here means the join won that race and this run proved nothing
    // about the repair, so it is a staging failure and not a pass.
    assert!(
        FcastPlaybin::global_stats().parked_text_cues_replayed > replayed_before,
        "no parked cue was replayed, so the text branch joined before the demuxer pushed \
         anything and this test is passing vacuously, the whole-period fixture is supposed to \
         put its first cues across during bring-up"
    );

    // AND THE VIEWER SAW IT. Delivery is not display: the field's residual was
    // a replayed opening that the join's own STREAM_START cleared a
    // millisecond later, which every delivery-counting assertion above still
    // called a pass. Let the item run a little and require that the frames
    // going past have a cue on them.
    harness.wait_until("a few seconds of playback", LIVENESS_BOUND, || {
        harness.video_buffers() >= 5 * FPS
    });
    let frames = rendered.rendered();
    let covered = frames.iter().filter(|(_, has)| *has).count();
    assert!(
        !frames.is_empty(),
        "the video tap recorded nothing, so this assertion cannot speak, restage it"
    );
    assert!(
        covered * 2 >= frames.len(),
        "only {covered} of {} frames had a cue up; the fixture's cues cover 0.9 s in every 1.0 s, \
         so a track whose cues are delivered and then cleared looks exactly like this",
        frames.len()
    );

    harness.assert_reaches_watermark("after a whole-period text opening");
    harness.shutdown();
}

/// Every buffer decodebin3's text output carried, as `(pts, when)`.
type Db3TextLog = Arc<Mutex<Vec<(Option<gst::ClockTime>, Instant)>>>;

/// ATTRIBUTION PROBE for "the subtitles start a few seconds in".
///
/// Prints, against the whole-period fixture, the four instants that split the
/// load-to-first-cue interval between its possible owners:
///
/// * `T0`: `load_async` returns from the caller's hand.
/// * `T_mpd`: the server is asked for the manifest. `T_mpd - T0` is the crate's
///   own bring-up: everything between the caller's load and a source that has
///   started fetching.
/// * `T_vtt`: the server is asked for `embedded.vtt`. `T_vtt - T_mpd` is the
///   DEMUXER's, and nobody else's: it has the manifest, it knows the text
///   Representation is there, and this is when it decided to go get it.
/// * `T_cue`: the first EMB payload crosses into the renderer. `T_cue - T_vtt`
///   is the crate's again: parse, route, and the sink's own scheduling.
///
/// Ignored because it is a measurement, not a claim. The claim it produced
/// lives in `the_first_cue_of_a_whole_period_track_is_not_gated_on_av_buffering`.
#[test]
#[ignore = "measurement probe; run explicitly"]
fn probe_first_cue_latency_on_a_whole_period_track() {
    let (server, root) = serve();
    assert!(support::has_embedded_text(&root));

    // `FCAST_PROBE_MANIFEST` points the probe at another manifest in the same
    // tree, a mirrored field one, for instance. The fixture's own is the
    // default and is what every assertion here uses.
    let manifest =
        std::env::var("FCAST_PROBE_MANIFEST").unwrap_or_else(|_| "manifest-text.mpd".to_owned());
    let vtt = std::env::var("FCAST_PROBE_VTT").unwrap_or_else(|_| "embedded.vtt".to_owned());

    // `FCAST_PROBE_VTT_DELAY_MS` holds the whole-period VTT's response, which
    // is the FIELD's ordering and not the fixture's. A local server answers in
    // microseconds, so the text data is always there before the branch joins
    // and the park is what loses it. A real sender is slower than its own
    // bring-up, the owner's proxying sender answered a cold request in 818 ms
    // so the data arrives AFTER the join, the park holds nothing, and
    // whatever happens then is a second mechanism the park cannot reach.
    if let Ok(ms) = std::env::var("FCAST_PROBE_VTT_DELAY_MS")
        && let Ok(ms) = ms.parse::<u64>()
    {
        server.delay_path(&vtt, Duration::from_millis(ms));
        eprintln!("PROBE holding the VTT response for {ms} ms");
    }

    let harness = Harness::new();

    // Every buffer that crosses decodebin3's text output, stamped. Installed
    // through `deep-element-added` because the pad is exposed DURING the load:
    // a probe added after `load_and_play` returns has already missed the
    // window this measurement is about.
    let db3_text: Db3TextLog = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = db3_text.clone();
        harness
            .playbin
            .pipeline()
            .connect("deep-element-added", false, move |values| {
                let element = values[2].get::<gst::Element>().ok()?;
                if element.name() != "fpb-decodebin" {
                    return None;
                }
                let seen = seen.clone();
                element.connect_pad_added(move |_db3, pad| {
                    if !pad.name().starts_with("text_") {
                        return;
                    }
                    let seen = seen.clone();
                    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                            seen.lock()
                                .expect("db3 text log")
                                .push((buffer.pts(), Instant::now()));
                        }
                        gst::PadProbeReturn::Ok
                    });
                });
                None
            });
    }

    let t0 = Instant::now();
    harness.load_and_play(&server.url(&format!("vod/{manifest}")));
    let tap = harness.tap_overlay_text();
    // The CLEAR-AWARE half. A cue that is delivered and then cleared is a cue
    // the viewer never saw, and the payload tap above cannot tell the two
    // apart: it logs deliveries, and a `Clear` is not one. This taps the same
    // window the consumer keeps, so `rendered` answers the viewer's question
    // ("was a cue up on this frame") rather than the transport's.
    let rendered = text_arm::TextTap::install(&harness.playbin);
    // No prefix: a mirrored field track's cues are whatever language it ships.
    harness.wait_for_cue(&server, &tap, "", 0, "the attribution probe");

    let since = |at: Instant| at.saturating_duration_since(t0).as_secs_f64();
    let t_mpd = server.first_fetch_at(&manifest).map(since);
    let t_vtt = server.first_fetch_at(&vtt).map(since);
    let t_cue = tap
        .lock()
        .expect("text tap")
        .iter()
        .find(|(text, _)| text.trim_start().starts_with("EMB"))
        .map(|(_, at)| since(*at));

    eprintln!("PROBE T0=0.000 T_mpd={t_mpd:?} T_vtt={t_vtt:?} T_cue={t_cue:?}");
    eprintln!("PROBE fetch timeline:");
    for (target, at) in server.timeline(t0) {
        eprintln!("  {at:>7.3}  {target}");
    }
    // Let the item run on, then report WHICH cues rendered. The fixture names
    // each cue after the media second it covers, so the first payload is the
    // whole answer to "do the opening seconds render or are they lost".
    harness.wait_until("some playback", Duration::from_secs(12), || {
        false_after(t0, 10.0)
    });
    eprintln!(
        "PROBE consumer saw {} cues; first payloads: {:?}",
        rendered.delivered_texts().len(),
        rendered
            .delivered_texts()
            .iter()
            .take(6)
            .map(|t| t.chars().take(28).collect::<String>())
            .collect::<Vec<_>>()
    );
    eprintln!("PROBE first cues (payload @ seconds since load):");
    for (text, at) in tap.lock().expect("text tap").iter().take(14) {
        eprintln!("  {:>7.3}  {text:?}", since(*at));
    }
    eprintln!("PROBE decodebin3 text output (pts @ seconds since load):");
    for (pts, at) in db3_text.lock().expect("db3 text log").iter().take(14) {
        eprintln!(
            "  {:>7.3}  pts={}",
            since(*at),
            pts.map_or("none".to_owned(), |pts| format!("{:.3}", pts.seconds_f64()))
        );
    }
    let frames = rendered.rendered();
    let with_text = frames.iter().filter(|(_, has)| *has).count();
    // An empty log is the tap, not the item: `TextTap::install` needs the
    // video chain to already be in the pipeline and says so by tapping
    // nothing.
    eprintln!(
        "PROBE frames with a cue up: {with_text}/{} (first covered frame at {:?}); consumer \
         clears={}{}",
        frames.len(),
        frames
            .iter()
            .find(|(_, has)| *has)
            .map(|(at, _)| format!("{:.3}", since(*at))),
        rendered.clears(),
        if frames.is_empty() {
            " -- NO VIDEO TAP, the coverage figures above say nothing"
        } else {
            ""
        },
    );
    eprintln!(
        "PROBE video={} frames (= {:.2} s of media) over {:.2} s of wall clock; \
         slot unlatches={} joins into an inactive branch={} parked cues replayed={}",
        harness.video_buffers(),
        harness.video_buffers() as f64 / FPS as f64,
        since(Instant::now()),
        FcastPlaybin::global_stats().slot_unlatches,
        FcastPlaybin::global_stats().joins_into_an_inactive_branch,
        FcastPlaybin::global_stats().parked_text_cues_replayed,
    );
    harness.shutdown();
}

/// A `wait_until` predicate that is simply "N seconds have passed", so the
/// probe can let an item play on while the harness keeps pumping.
fn false_after(t0: Instant, secs: f64) -> bool {
    Instant::now().duration_since(t0).as_secs_f64() >= secs
}

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
/// text stream" ones. So the text-flush feeder hold is unreachable on DASH,
/// and the hold that matters on this path is the drop probe in
/// `detach_text_parts`.
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
        b = harness.wait_for_cue(
            &server,
            &tap,
            "EXTB",
            b,
            &format!("switch to b, cycle {cycle}"),
        );
        harness.mark(&format!("switched to b, cycle {cycle}"));
        harness.request_subtitle(TrackTarget::ExternalSubtitle(first));
        a = harness.wait_for_cue(
            &server,
            &tap,
            "EXTA",
            a,
            &format!("switch to a, cycle {cycle}"),
        );
        harness.mark(&format!("switched to a, cycle {cycle}"));
    }

    harness.assert_reaches_watermark("after the direct switches");
    harness.shutdown();
}

/// The EMBEDDED track: text out of `adaptivedemux2`'s own output loop rather
/// than a side input. ONE off/on of the embedded track, which is the whole of
/// `dash-reenable-freeze.txt` ("re enabling embedded dash sub freezes the vid
/// and doesn't show ever"), asserted in BOTH shapes decodebin3 can answer
/// the re-enable with.
///
/// # The two shapes, and why the test must take whichever it is dealt
///
/// A re-enable makes the demuxer expose a FRESH input pad either way; the
/// fork is on the OUTPUT side, and it turns on whether the previous input's
/// multiqueue slot has been released by the time the new input arrives:
///
/// * REPLACED, the old slot is still occupied, decodebin3 builds a new one and
///   a SECOND text output pad appears beside the dead first. The crate's seat
///   must move (the superseded/EOS-seat reclaims). This was the ONLY shape
///   before the outputless-slot patches, and the original version of this test
///   asserted it with a pad-count guard.
/// * REUSED, the old slot is free, decodebin3 takes it as the lowest-indexed
///   unused compatible slot (gstdecodebin3.c:3891) and RE-POINTS the ORIGINAL
///   output pad at it: no pad-added, no message, just a fresh
///   STREAM_START/SEGMENT and the re-fetched track through the pad the crate
///   already has routed (and parked). The patches keep outputs alive, so this
///   shape is live on this fixture (cycle 1 below deals it reliably) and COMMON
///   at field pace, and the old guard turned every such run into an abort.
///
/// # Why the old claim was vacuous too, measured
///
/// The original arrival wait carried the FIRST select's cue count forward,
/// and a whole-period track replays its entire file in one burst on
/// selection, so by the off/on the count had already moved: in 6/6 runs of
/// the old test the post-re-enable wait was satisfied 6-10 ms after the
/// DISABLE's own SELECT_STREAMS, before the re-enable had even dispatched,
/// which also means its guard compared pad lists from BEFORE the re-enable
/// could change them, so its "reuse" verdicts measured nothing. Stragglers,
/// not the re-enable. So this version (a) waits for the crate to CONFIRM the
/// re-select, (b) re-reads the baseline immediately before it, and (c) makes
/// the render claim with CUE IDENTITY, a frame in media second N, gone past
/// after the re-select, covered by a cue delivered after the re-select whose
/// payload is `EMB N` (the fixture names each cue after its second, see
/// `gen-dash.sh`). A boolean cover has been measured maskable twice in this
/// saga; the identity cannot be satisfied by another second's cue.
///
/// # The shape-specific claims
///
/// The render claim is the same in both shapes; what differs is where the
/// seat must end up, read off the live branch's queue (named after the
/// decodebin3 pad it serves):
///
/// * REPLACED: the consumer is fed by one of the NEW pads, the reclaim moved
///   the seat off the output decodebin3 abandoned (the original claim).
/// * REUSED: the consumer is fed by the ORIGINAL pad, the re-pointed pad was
///   re-admitted and re-seated, which is exactly task #30's acceptance: the
///   reuse-shape ask (the `saw_eos` alive-edge/first-buffer poke), the flow
///   reclaim's re-admission, and the keeping park's replay.
///
/// Neither shape aborts and neither passes vacuously; the shape taken is
/// printed per cycle so a batch can count them.
///
/// # Two cycles, two pacings, because the fork is a race the test can lean on
///
/// Which shape decodebin3 deals is decided by whether the previous input's
/// slot has been released when the re-add arrives, so the test runs the
/// off/on TWICE: cycle 0 re-enables immediately after the park (the old
/// input is still draining, replacement-favored), cycle 1 dwells a second
/// of playback after the off first (the drained slot is free, the
/// reuse-favored pacing, the same second of dwell the field-pace test
/// documents). Each cycle asserts the identity render and the seat claim of
/// whatever shape it was actually dealt.
#[test]
fn dash_embedded_text_rejoins_whether_decodebin3_replaces_or_reuses_its_pad() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let cue_window = text_arm::TextTap::install(&harness.playbin);
    // Every displayed frame: when it went past, its media position, and the
    // running time the renderer will place it at (the identity-test pattern).
    let frames: Arc<Mutex<Vec<(Instant, gst::ClockTime, gst::ClockTime)>>> = Default::default();
    {
        let seen = frames.clone();
        let pad = text_arm::video_tap_pad(&harness.playbin).expect("a video chain to tap");
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(buffer) = info.buffer()
                && let Some(pts) = buffer.pts()
                && let Some(rt) = pad
                    .sticky_event::<gst::event::Segment>(0)
                    .and_then(|event| {
                        event
                            .segment()
                            .downcast_ref::<gst::ClockTime>()?
                            .to_running_time(pts)
                    })
            {
                seen.lock()
                    .expect("frame log")
                    .push((Instant::now(), pts, rt));
            }
            gst::PadProbeReturn::Ok
        });
    }
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    harness.wait_for_cue(
        &server,
        &tap,
        "EMB",
        0,
        "the embedded track at first select",
    );

    for (cycle, dwell) in [(0, false), (1, true)] {
        let before = harness.decodebin_text_pads();
        harness.subtitle_off();
        if dwell {
            // The reuse-favored pacing: a second of playback for the drained
            // input's slot to be released, so the re-add finds it free and
            // decodebin3 re-points the existing output instead of building
            // a new one.
            harness.advance_past(
                harness.video_buffers() + FPS,
                "letting the deselected track's slot free",
            );
        }
        harness.mark(&format!("re-enabling the embedded track, cycle {cycle}"));
        let at = harness.video_buffers() / FPS;
        // Everything before this instant, earlier legs' bursts, their
        // stragglers, frames already shown, is out of the claim.
        let reselect = Instant::now();
        // Baseline read immediately before the switch (the strongest position
        // that is not a race, see the field-pace test), and the switch WAITS
        // for the crate's own confirmation so the wait after it is about this
        // leg.
        let base = tapped_with_prefix(&tap, "EMB");
        harness.select_subtitle(
            TrackTarget::Stream(Some(sid.clone())),
            &sid,
            &format!("the embedded track re-enabled, cycle {cycle}"),
        );

        // Arrival first, so "never delivered" and "mistimed" stay separable.
        harness.wait_for_cue(
            &server,
            &tap,
            "EMB",
            base,
            &format!("the embedded track after off/on cycle {cycle}"),
        );

        // THE CLAIM, with identity: a frame in media second N, beyond the
        // join transient, two seconds past the re-select playhead, goes past
        // covered by the cue FOR second N, both after the re-select.
        let beyond = (at + 2) as u64;
        harness.wait_until(
            &format!(
                "the playhead's own cue to cover a frame past second {beyond} after \
                 the cycle-{cycle} re-enable"
            ),
            EVENT_TIMEOUT,
            || {
                let cues = cue_window.window_cues();
                frames
                    .lock()
                    .expect("frame log")
                    .iter()
                    .any(|(when, pts, rt)| {
                        *when > reselect
                            && pts.seconds() >= beyond
                            && cues.iter().any(|(text, delivered, start_rt, end_rt)| {
                                *delivered > reselect
                                    && *start_rt <= *rt
                                    && end_rt.is_none_or(|end| end > *rt)
                                    && text.trim() == format!("EMB {:02}", pts.seconds())
                            })
                    })
            },
        );

        // THE SHAPE, taken as dealt, each with its own seat claim.
        let after = harness.decodebin_text_pads();
        let seated = harness.seated_text_pad();
        let replaced = after.iter().any(|pad| !before.contains(pad));
        eprintln!(
            "SHAPE[{cycle}]: {} ({before:?} -> {after:?}, seated {seated:?})",
            if replaced { "REPLACED" } else { "REUSED" }
        );
        if replaced {
            assert!(
                seated
                    .as_deref()
                    .is_some_and(|pad| !before.contains(&pad.to_string())),
                "decodebin3 replaced its text output ({before:?} -> {after:?}) but the \
                 consumer is fed by {seated:?}, the seat never moved off the abandoned pad"
            );
        } else {
            assert!(
                seated
                    .as_deref()
                    .is_some_and(|pad| before.contains(&pad.to_string())),
                "decodebin3 re-used its text output ({before:?} -> {after:?}) but the \
                 consumer is fed by {seated:?}, the re-pointed pad was never re-seated"
            );
        }
    }
    harness.assert_reaches_watermark("embedded text re-enable");
    harness.shutdown();
}

/// The same re-enable, with an EXTERNAL taking the seat in between,
/// `selecting-embedded-doesnt-show.txt`, and the shape the off/on test above
/// cannot reach.
///
/// RED on 313a8663 (4 runs, 4 failures): no EMB cue for the whole 40 s
/// `EVENT_TIMEOUT` after a re-select the crate reports as joined, while every
/// switch back to the external renders inside 150 ms. `#[ignore]`d because no
/// fix has landed yet, not because it is unreliable, see the mechanism below
/// for why the two obvious repairs are both refused.
///
/// # The mechanism, from `GST_DEBUG=decodebin3:6`
///
/// An adaptive input answers SELECTABLE, so `dbin->upstream_handles_selection`
/// is 1 and decodebin3 never runs `handle_stream_switch` at all
/// (gstdecodebin3.c:3663, `if (!dbin->upstream_handles_selection && ...)`).
/// The ONLY thing that attaches an output pad to a slot in that mode is
/// `mq_slot_check_reconfiguration`, and `multiqueue_src_probe` calls it on a
/// CAPS event and nothing else (gstdecodebin3.c:3690).
///
/// So a re-select goes upstream, the demuxer answers by exposing a FRESH pad,
/// the crate links it into a fresh decodebin3 sink, and decodebin3:
///
/// * `create_new_slot`, "Created new slot 4 (multiqueue1:src_4)", and
/// * `remove_input_stream` on the OLD input, "slot 0x… cleared",
///   `<multiqueue1:sink_2> Sending EOS to unused slot`.
///
/// Slot 2 is the one output pad `text_0` is ghosted onto. From that instant
/// `text_0` can never carry another buffer, and slot 4, which has the stream
/// has no output pad and will only get one if a CAPS event crosses it.
///
/// Captured here: the deselect lands ~11 ms after that fresh pad appears, the
/// demuxer drains the pad it had just built, and the re-select 0.7 s later is
/// swallowed while the track is still draining. No CAPS, no buffer, no output
/// pad; the pad EOSes 1.35 s later and unlinks. The crate meanwhile joins
/// `text_0` and logs "text stream joined its consumer tail pad=text_0
/// segment=true", a seat on a slot decodebin3 has ended, and
/// `consumer_branch_live` then refuses everything else for the life of
/// the selection.
///
/// # What fixes it, and how the two candidate repairs were told apart
///
/// A crate-side census enumerated decodebin3's multiqueue directly whenever the
/// selected text stream had no LIVE routed pad, and on this capture it settled
/// the question (the census is retired; see the note below):
///
/// ```text
/// src_4[caps=None sticky=StreamStart+Segment+StreamCollection eos=false ...]
///   <- sink_4[caps=None sticky=StreamStart+Segment+StreamCollection sid=…/text-2]
/// ```
///
/// The slot's SINK pad has no CAPS either. Nothing was destroyed inside
/// decodebin3 (that is the lost-caps shape, whose repair needs a caps on the
/// sink to put back). Nothing ever ARRIVED. The demuxer exposed the pad, pushed
/// its opening events, was deselected 3.4 ms later and drained it, and the
/// re-select that followed 0.7 s later landed 1.36 s BEFORE that drain
/// finished and was swallowed.
///
/// So the repair is on the SEND side: `Inner::await_text_input_drain` holds a
/// text re-select on the select lane until the demuxer's previous pad for that
/// stream carries its EOS. Waiting rather than re-sending is what keeps it
/// clear of `dispatch.rs` (5), "re-sending a `SELECT_STREAMS` at an adaptive
/// demuxer mid-drain is the `g_assert(track->draining && !track->selected)`
/// abort", because the event still goes out exactly once, after the state
/// that would have swallowed it is over.
///
/// The EOS tap on routed text outputs (`RoutedStream::saw_eos`) is the other
/// half and is what the FIELD capture needs, where a live replacement output
/// `text_4` sat routed beside the dead `text_0`. It does not repaint THIS
/// capture, where decodebin3 built no replacement output at all, which is
/// exactly why both had to land.
///
/// # The knob that localises it
///
/// One second of dwell per leg (`advance_past(video_buffers() + FPS)` after
/// each cue wait) turned this GREEN before any fix, which was the whole
/// diagnosis in one line: the loss is a deselect arriving while the demuxer is
/// still bringing the previous re-select's pad up. The legs here are
/// deliberately NOT dwelled, so the race is still run on every execution.
#[test]
fn dash_embedded_text_rejoins_after_a_round_trip_through_an_external() {
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

    // STAGED MID-PLAY, and every leg settled before the next one starts. The
    // attach, the engine's own pick-up of the new external and this test's
    // switches all land on the same slot, and legs that overlap bring-up
    // collapse into one coalesced switch, which is the shape this test is NOT
    // about. Letting the item run first is what separates them.
    let mut emb = harness.wait_for_cue(&server, &tap, "EMB", 0, "the default embedded selection");
    harness.advance_past(FIRST_SELECT_AT, "before attaching the external");
    harness.mark("embedded rendered from the default selection");

    let ext = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    let ext_sid = harness
        .playbin
        .subtitle_stream_ids(ext)
        .into_iter()
        .next()
        .expect("the materialized external has a stream id");
    let mut exta = 0;

    for round in 0..2 {
        // Out to the external. This is the leg that leaves a replacement text
        // output behind for the NEXT re-select's join to choose wrongly from.
        harness.select_subtitle(
            TrackTarget::ExternalSubtitle(ext),
            &ext_sid,
            &format!("the external on round trip {round}"),
        );
        exta = harness.wait_for_cue(
            &server,
            &tap,
            "EXTA",
            exta,
            &format!("the external on round trip {round}"),
        );
        harness.mark(&format!("external rendered, round {round}"));

        // THE CLAIM, made once per round trip. Re-selecting the embedded track
        // renders again. The field reports this as "nothing ever shows",
        // indefinitely, with the external still rendering every time it is
        // picked.
        harness.mark(&format!("re-selecting the embedded track, round {round}"));
        harness.select_subtitle(
            TrackTarget::Stream(Some(sid.clone())),
            &sid,
            &format!("the embedded track on round trip {round}"),
        );
        emb = harness.wait_for_cue(
            &server,
            &tap,
            "EMB",
            emb,
            &format!("the embedded track after round trip {round} through an external"),
        );
        harness.mark(&format!("embedded rendered, round {round}"));
    }
    assert!(emb > 0, "the cue waits returned without a cue");

    harness.assert_reaches_watermark("embedded text after an external round trip");
    harness.shutdown();
}

/// The OWNER'S gesture at the OWNER'S pace, with the harness's timer poll
/// switched off, the field shape rather than the race above.
///
/// # What this covers that the reproduction above does not
///
/// The reproduction runs its legs back to back, so the deselect lands ~3 ms
/// after the demuxer exposes the pad and decodebin3 never builds an output for
/// the fresh slot at all. `selecting-embedded-doesnt-show.txt` is the other
/// side of that: 0.54 s, 0.9 s and 1.3 s between selections, which is long
/// enough for decodebin3 to build the replacement output, the capture has a
/// LIVE `text_4` sitting routed beside the dead `text_0` the crate had seated.
/// Dwelling a second per leg here reproduces that pacing.
///
/// # Why the poll has to be off
///
/// See [`Harness::poll_on_events_only`]. With this suite's 10 ms pump the
/// existing `superseded` reclaim heals a wrong seat within one tick, which is
/// exactly why the field never healed and the tests never showed it: the
/// field's receiver polls on events, and the capture has NO poll at all in the
/// 6.5 s after the bad seat. Off, the crate has to ask for its own re-check,
/// which is what the link loop's follow-up poll exists to do.
///
/// # Why the cue baselines are re-read immediately before every switch
///
/// An external replays its whole file on selection and a whole-period embedded
/// track re-fetches its whole VTT, so each leg delivers a burst and then goes
/// quiet. A CARRIED counter (updated only when a wait succeeds) is left behind
/// by the rest of that burst and by the dwell after it, so the next wait for
/// the same prefix is satisfied by cues the previous leg delivered. Re-reading
/// the count immediately before each switch folds all of those into the
/// baseline, which makes every wait a claim about the leg under it.
///
/// Reading it AFTER the switch confirms would be stronger still and is WRONG:
/// measured, the external's replay is queued by the join and its whole file
/// lands within ~40 ms of the confirmation, so the read races the delivery it
/// is supposed to baseline and the leg fails with the track working perfectly.
/// Before the switch is the strongest position that is not a race: nothing can
/// be delivered for a track that is not selected yet.
///
/// # The UPSTREAM defect this reached first (historical; repaired since)
///
/// RED, and NOT for want of the repairs in that commit: it was red with them,
/// without them, and with this suite's timer poll left ON (`embedded=4` VTT
/// fetches, i.e. the demuxer re-downloading the track four times, and still no
/// cue). So it is not poll starvation and not a seat the crate holds wrongly.
///
/// `GST_DEBUG=decodebin3:6` says what it is, in two layers.
///
/// The FIRST is that the demuxer has nothing left to send. A whole-period text
/// Representation is ONE segment covering the period; once it has been
/// downloaded and pushed, a re-select gets a fresh pad and an empty track,
/// captured as stream-start, segment and stream-collection across the new slot
/// and then no caps and no buffer for forty seconds while the track is
/// selected. Only a flushing seek re-downloads it, which is why the field
/// report says the subtitles "appear after you seek", and the engine cancels
/// exactly that seek whenever an external subtitle is attached
/// (`SelectionEngine::pump`, "any flush races the external inputs'
/// reconfiguration and can freeze the item").
///
/// The SECOND is what happens if you send it anyway. Measured in a throwaway
/// build that seeks the MAIN INPUT ELEMENT only: the demuxer does re-download,
/// the CAPS does cross the fresh slot, decodebin3 does run
/// `mq_slot_check_reconfiguration`, and then:
///
/// ```text
/// mq_slot_get_or_create_output:<multiqueue1:src_4> Reassigning to output …:text_1
/// mq_slot_reassign:<multiqueue1:src_3> Unlinking from previous output
/// mq_slot_reassign:<multiqueue1:src_3> Attempting to re-assing output stream
/// mq_slot_reassign:<multiqueue1:src_3> No target slot, removing output
/// db_output_stream_free:<fpb-decodebin:text_1> Freeing
/// ```
///
/// decodebin3 picks the just-freed output of the DESELECTED EXTERNAL to
/// recycle for the new slot, and the reassign meant to detach that output from
/// its old slot DESTROYS it instead, because the old slot's own stream has
/// nowhere to go. The slot it was chosen for keeps the stream and the caps and
/// never gets an output, and decodebin3 never revisits the decision. Every
/// crate-side rule below decodebin3 reasons about routed pads, and there is no
/// pad.
///
/// And that seek is refused for the reason the engine already gave: 8 runs of
/// the reproduction above, 8 failures, video flat at 76 buffers with no text
/// branch at all, against 8/8 without it. Confining the seek to one ELEMENT
/// does not confine its FLUSH.
///
/// So the crate half of this shape was a NAME, not a repair: a multiqueue
/// census that logged the slot holding the selected stream with no output,
/// which is the one thing a field capture could not previously say. Fixing it
/// is an upstream change, and the patches below are that fix, which is why the
/// census was retired (zero escalations across the whole reselect battery on
/// the patched build). NOTE that this is NOT the field capture's shape either:
/// `selecting-embedded-doesnt-show.txt` has a LIVE `text_4` routed beside the
/// dead `text_0`, i.e. decodebin3 DID build the replacement output there. This
/// is a third shape, found on the way.
///
/// # What the upstream patches changed, and what is still red
///
/// The fork's `decodebin3-outputless-slot-keeps-its-output` and
/// `adaptivedemux2-track-flush-keeps-its-caps` patches repair the
/// upstream half. Three defects, each pinned by its own capture:
///
/// * with upstream handling the selection, `handle_stream_switch` never runs
///   (both its call sites are gated on `!upstream_handles_selection`), so a
///   CAPS event crossing the slot is the ONLY thing that ever gives a slot an
///   output. On this gesture no CAPS crosses and the slot is stranded;
/// * `mq_slot_reassign` looks for the recycled output's new home only in
///   `to_activate`, which `handle_stream_switch` fills in and which is
///   therefore always empty in this mode, so it DESTROYS the output instead;
/// * `find_free_compatible_output` calls an output free because its stream is
///   not in `requested_selection`, a list that decides nothing in this mode;
/// * adaptivedemux2's `gst_adaptive_demux_track_flush` drops the track's CAPS
///   sticky, and nothing upstream re-sends it, so the re-selected track is
///   pushed out with a segment, buffers and no caps at all.
///
/// Measured against the baseline `.so`: round trip 0 goes from red to green,
/// and the failure signature changes at every one of those steps.
///
/// # The CRATE half: the REUSED pad, and the ask that did not exist
///
/// With decodebin3 no longer destroying outputs, round trip 1's re-select
/// REUSES a cleared same-stream-id slot that already has an output
/// (gstdecodebin3.c:3891): no pad-added, just a fresh STREAM_START and the
/// re-fetched track through a pad the crate already had routed. Measured at
/// 0/8 here, every run identical: the re-select found both same-sid pads
/// EOS-dead, seated one corpse, latched the other `superseded` off a DEAD
/// rival, 0.5 s before decodebin3 re-pointed that very pad, and then
/// nothing ever asked the link policy again (the corpse-watch excludes
/// superseded pads, and with `poll_on_events_only` there is no other ask).
/// The re-fetched burst meanwhile drained into a DISCARDING park.
///
/// What repaired it, and what this test now pins at 8/8:
///
/// * the reuse-shape ASK, the `saw_eos` probe's alive edge (the re-point's own
///   STREAM_START) and the first buffer after it each request a coalesced poll,
///   so the flow reclaim gets both an occasion and its evidence;
/// * the corpse-watch admits superseded-but-alive pads (the walk-back in
///   progress), as the 1 Hz belt behind the probe;
/// * the superseded latch needs a LIVE rival to justify it (a second corpse is
///   not a replacement);
/// * every text park keeps what it consumes (`park_text_stream`), so the heal's
///   join replays the burst the re-point pushed before the seat could move.
#[test]
fn dash_embedded_text_rejoins_at_field_pace_with_no_timer_poll() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    // BEFORE the load, so not one pump in this test's life polls on a timer.
    harness.poll_on_events_only();
    harness.load_and_play(&server.url("vod/manifest-text.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();

    harness.wait_for_cue(&server, &tap, "EMB", 0, "the default embedded selection");
    harness.advance_past(FIRST_SELECT_AT, "before attaching the external");
    harness.mark("embedded rendered from the default selection");

    let ext = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    let ext_sid = harness
        .playbin
        .subtitle_stream_ids(ext)
        .into_iter()
        .next()
        .expect("the materialized external has a stream id");

    for round in 0..2 {
        let base = tapped_with_prefix(&tap, "EXTA");
        harness.select_subtitle(
            TrackTarget::ExternalSubtitle(ext),
            &ext_sid,
            &format!("the external on round trip {round}"),
        );
        harness.wait_for_cue(
            &server,
            &tap,
            "EXTA",
            base,
            &format!("the external on round trip {round}"),
        );
        harness.mark(&format!("external rendered, round {round}"));
        // THE FIELD'S PACING. A whole second of playback between the legs, so
        // the demuxer finishes exposing the pad it was asked for and
        // decodebin3 builds an output for it, which is what leaves a LIVE
        // replacement beside the dead pad for the next re-select to choose
        // wrongly from.
        harness.advance_past(
            harness.video_buffers() + FPS,
            &format!("dwelling on the external, round {round}"),
        );

        harness.mark(&format!("re-selecting the embedded track, round {round}"));
        let base = tapped_with_prefix(&tap, "EMB");
        harness.select_subtitle(
            TrackTarget::Stream(Some(sid.clone())),
            &sid,
            &format!("the embedded track on round trip {round}"),
        );
        harness.wait_for_cue(
            &server,
            &tap,
            "EMB",
            base,
            &format!("the embedded track after round trip {round} at field pace"),
        );
        harness.mark(&format!("embedded rendered, round {round}"));
        harness.advance_past(
            harness.video_buffers() + FPS,
            &format!("dwelling on the embedded track, round {round}"),
        );
    }

    harness.assert_reaches_watermark("embedded text at field pace with no timer poll");
    harness.shutdown();
}

/// The SEGMENTED-text twin of the re-enable test, and the one that can see the
/// field's failure at all.
///
/// `dash-embedded-still-broken.txt` is "Discarding data on subtitle_00:
/// downstream returned FLUSHING while this element is not flushing" at
/// PLAYING, with the subtitles never appearing. Reproducing it needs the
/// demuxer to be PUSHING subtitle data while the crate does its text-branch
/// surgery, and against the unsegmented fixture it never is: measured with
/// `GST_DEBUG=adaptivedemux2:5`, a full 30 s window after a re-enable contains
/// nothing but "track track-text-2-period0 push returned ok" gap ticks, no
/// download and no buffer. This variant keeps the demuxer downloading and
/// pushing for the whole item.
///
/// The cue assertion is therefore a real delivery claim on BOTH sides of the
/// toggle: unlike the whole-period fixture, a re-select here has cues still
/// ahead of the playhead to deliver.
#[test]
fn dash_segmented_embedded_text_survives_a_re_enable() {
    let (server, root) = serve();
    assert!(
        support::has_segmented_text(&root),
        "no segmented-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text-seg.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    let mut seen = harness.wait_for_cue(
        &server,
        &tap,
        "SEG",
        0,
        "the segmented track at first select",
    );

    harness.subtitle_off();
    harness.request_subtitle(TrackTarget::Stream(Some(sid)));
    seen = harness.wait_for_cue(
        &server,
        &tap,
        "SEG",
        seen,
        "the segmented track after one off/on",
    );
    assert!(seen > 0, "the cue wait returned without a cue");

    harness.assert_reaches_watermark("segmented embedded text re-enable");
    harness.shutdown();
}

/// The FIRST select, taken MID-PLAY, with no seek anywhere in its path.
///
/// # The coverage gap this closes
///
/// Every other embedded-text test in this file starts with the track already
/// selected and toggles it off/on, so every one of them has a flushing
/// `Job::RefreshSeek` in its path (the re-enable schedules one) and none of
/// them exercises the shape the field reports: start with subtitles off, watch
/// for a while, then turn them on. That path has NO seek in it at all, which
/// is exactly what the field says is the difference - "they never show, but
/// they appear after you seek".
///
/// Staged at [`FIRST_SELECT_AT`] frames rather than at the start because the
/// distance between the playhead and the period start is the whole question:
/// a track that begins downloading from the period start when it is selected
/// mid-play has to catch up sequentially before it can produce a cue the
/// playhead has not already passed.
#[test]
fn dash_segmented_embedded_text_shows_on_a_first_mid_play_select() {
    let (server, root) = serve();
    assert!(
        support::has_segmented_text(&root),
        "no segmented-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text-seg.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();

    // Subtitles OFF, explicitly, before any of them can be auto-selected: the
    // premise of this test is that the mid-play select below is the FIRST one.
    harness.request_subtitle(TrackTarget::Stream(None));
    harness.advance_past(FIRST_SELECT_AT, "before the first subtitle select");
    assert_eq!(
        tapped_with_prefix(&tap, "SEG"),
        0,
        "a SEG cue rendered before the track was ever selected, so this test is \
         not staging a FIRST select and proves nothing about one"
    );

    harness.mark("first mid-play subtitle select");
    let at = harness.video_buffers() / FPS;
    harness.request_subtitle(TrackTarget::Stream(Some(sid)));

    // THE CLAIM, and it is about the timeline, not about arrival. A cue for a
    // second the playhead has already passed is not this track working; it is
    // the track reading itself out from the period start while the viewer sees
    // nothing. So the cue that satisfies this must be for the moment the
    // select happened or later.
    harness.wait_for_cue_at_or_after(&server, &tap, at, "the first mid-play select, with no seek");

    // The delivered set is the diagnosis if this ever goes red: measured
    // [8, 8, 6, 7, 8, 9, ... 36] here, i.e. every cue from the select onward,
    // with only the two seconds already behind the playhead missing.
    harness.assert_reaches_watermark("segmented embedded text first mid-play select");
    harness.shutdown();
}

/// The same first select, taken while PAUSED - "pause, turn subtitles on,
/// carry on watching", and the shape that makes the demuxer's restart rule
/// bite.
///
/// # Why pausing is the load-bearing part
///
/// A stream selected mid-play is restarted by adaptivedemux2 at
/// `demux->priv->global_output_position` (gstadaptivedemux.c:2540-2561), which
/// is the demuxer's OUTPUT position - how far it has pushed downstream - not
/// the playhead. It then snaps to the fragment CONTAINING that position, so
/// the track only covers the viewer's "now" while
///
///     global_output_position - playhead <= text fragment duration
///
/// Playing, that distance is whatever is buffered downstream (0.93 s here,
/// under the fixture's 2 s text fragments, which is why
/// `dash_segmented_embedded_text_shows_on_a_first_mid_play_select` is green).
/// PAUSED, the playhead stops while the demuxer keeps filling, so the distance
/// grows to the buffers' depth - and the field's players, buffering seconds
/// rather than a fraction of one, sit permanently on the far side of that
/// inequality. That is the reported "subtitles never show, but they appear
/// after you seek": a seek is the one operation that puts the playhead and the
/// download position back on the same instant.
///
/// # What it actually caught, which is NOT the download position
///
/// RED, ~1 in 3 (4 of 6 on one sweep), and the traced failure says the text
/// stream is never selected UPSTREAM AT ALL:
///
/// ```text
/// WARN SELECT_STREAMS event refused target=urisourcebin0
/// WARN a selection was refused id=3 seqnum=Seqnum(743)
/// ```
///
/// with no later `sent SELECT_STREAMS` anywhere in the run. The re-enable's
/// own flushing `Job::RefreshSeek` (seqnum 744, dispatched one job behind the
/// selection, whose send runs on the select lane) races the send and the event
/// is refused. `SelectionEngine::selection_dispatched` has already set
/// `applied` to the target optimistically, so the engine believes the track is
/// on, `Inner::poll_text_policy` builds and links the branch off
/// `subtitle_sid()` - hence the linked, permanently silent tail this test
/// reports - and desired == applied means nothing ever asks again.
///
/// # The fix this now pins
///
/// `SelectionEngine::pump` had the guard already - "an unconfirmed selection
/// blocks new work" - but exempted the PAUSED case, on the reasoning that a
/// parked selection is safe to supersede or flush past. Superseding it is
/// safe. Flushing past it is not, and that exemption is exactly why this
/// staging had to pause to reproduce. The refresh is now DEFERRED (not
/// dropped) while `selecting` is some, so it dispatches on the first pump
/// after the confirmation, the refusal, or the deadline advisory's timeout.
/// The rollback in `dispatch_failed` is the other half: a refusal that does
/// slip through no longer leaves the engine converged on a selection that
/// never left.
///
/// WITHOUT `FCASTPLAYBIN_TEST_LOG` when checking this: crate logging slows the
/// send enough that the race stops happening (5 of 5 green with it on, even
/// unfixed).
#[test]
fn dash_segmented_embedded_text_shows_on_a_first_select_while_paused() {
    let (server, root) = serve();
    assert!(
        support::has_segmented_text(&root),
        "no segmented-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text-seg.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();

    harness.request_subtitle(TrackTarget::Stream(None));
    harness.advance_past(FIRST_SELECT_AT, "before the paused subtitle select");
    let at = harness.video_buffers() / FPS;

    harness.pause();
    harness.mark("first subtitle select, paused");
    harness.request_subtitle(TrackTarget::Stream(Some(sid)));
    harness.play();

    harness.wait_for_cue_at_or_after(
        &server,
        &tap,
        at,
        "the first select taken while paused, with no seek",
    );

    harness.assert_reaches_watermark("segmented embedded text first paused select");
    harness.shutdown();
}

/// The same staging REPEATED, which is where it broke: the walk-back, and the
/// pin on the rule that the text seat follows the DATA rather than routed
/// order.
///
/// # What it was NOT
///
/// Not the field's `Discarding data on subtitle_00` - every discard in
/// this rig is still at TEARDOWN, and the receiver-core escalation remains the
/// net for that unreproduced one. And not, as the first read of the trace
/// concluded, a PAUSED multiqueue slot task with nothing left to restart it.
/// That hypothesis was measured and is false: with
/// `GST_DEBUG=2,adaptivedemux2:5,multiqueue:5,decodebin3:6` the slot's task is
/// RUNNING for the whole starvation window, `gst_single_queue_push_one` on
/// `multiqueue1:queue_2` pushing one cue per second for 40 s. Nothing was ever
/// paused and nothing was ever held.
///
/// # What it was
///
/// The cues were being delivered the whole time - to the crate's own parking
/// fakesink, because the crate was holding the WRONG PAD.
///
/// decodebin3 recycles text outputs in BOTH directions.
/// `gst_decodebin_get_slot_for_input_stream_locked` takes the LOWEST-INDEXED
/// unused compatible slot (gstdecodebin3.c:3874-3886) and
/// `db_output_stream_reconfigure` (:4229) re-points an EXISTING ghost pad at
/// it, emitting no pad-added. Which direction a re-enable takes depends only
/// on whether the previous input's slot has been released yet:
///
/// * cycle 0 - the old input is still on slot 2, so decodebin3 builds slot 3
///   and a NEW pad `text_1`. The crate follows it and marks `text_0`
///   superseded;
/// * cycle 1 - the old input drained at 34.953 and was released BEFORE the
///   re-enable's input was added at 35.020, so slot 2 is free: "Re-using
///   existing unused slot 2", and `text_0` is re-pointed at it.
///
/// The crate, holding `text_1` (whose slot 3 now has no input at all) with
/// `text_0` marked PERMANENTLY superseded, then refused the only pad still
/// carrying cues for the rest of the item. Every symptom follows: the branch
/// is linked, adaptivedemux2's pushes return `ok` (they land in slot 2 fine),
/// and `fpb-tqueue-text_1` sees nothing for 40 s.
///
/// The five cycles are load-bearing: decodebin3 ping-pongs `text_0` <->
/// `text_1` once per cycle, so a fix that only handles one direction fails on
/// the next.
#[test]
fn dash_segmented_embedded_text_survives_repeated_re_enables() {
    let (server, root) = serve();
    assert!(
        support::has_segmented_text(&root),
        "no segmented-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text-seg.mpd"));
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    let mut seen = harness.wait_for_cue(
        &server,
        &tap,
        "SEG",
        0,
        "the segmented track at first select",
    );

    for cycle in 0..5 {
        harness.subtitle_off();
        harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
        seen = harness.wait_for_cue(
            &server,
            &tap,
            "SEG",
            seen,
            &format!("the segmented track after off/on cycle {cycle}"),
        );
    }

    harness.assert_reaches_watermark("segmented embedded text repeated re-enable");
    harness.shutdown();
}

/// THE TIMING HALF of the re-select, which every arrival-counting test above
/// is blind to: after a round trip through an external, the re-added embedded
/// pad's cues must COVER THE PLAYHEAD, reach the consumer with a running
/// time that overlaps the frames actually going past, not merely arrive.
///
/// # The mechanism this pins (`reselect-alignment-field.txt`)
///
/// A mid-stream re-add makes `dashdemux2` restart the track at its global
/// output position and emit `start ≈ base ≈ that position`: a segment already
/// ON the pipeline's running-time line. `Inner::sync_text_running_time` used
/// to apply `video_base - text_base` alone, the right offset only when both
/// segments carry the same start, so it read that base as drift and shifted
/// every cue a playhead's worth into the past (the field's `offset=-22.066s`
/// on `text_3`, every cue expiring on arrival, including the park's two
/// replays). The full origin formula computes 0 for this shape.
///
/// # Why the staging is what it is
///
/// * The DWELL is load-bearing: a re-select at media ~0 has the demuxer re-add
///   with `start ≈ base ≈ 0`, the wrong formula also computes ~0, and the bug
///   is invisible. [`RESELECT_DWELL`] puts the playhead ≥ 5 s in, so the broken
///   offset is seconds wide and no delivered cue can cover a frame.
/// * The SEGMENTED fixture is load-bearing: a whole-period track is pushed once
///   and a re-select has nothing left to deliver at the present (see
///   `gen-dash.sh`); the SegmentTemplate track keeps producing cues for the
///   playhead's own seconds.
/// * The external leg in between is the field's gesture, and it is what makes
///   the re-select a decodebin3 re-ADD (fresh input pad, fresh output, EOS-seat
///   reclaim) rather than a same-pad relink.
///
/// # The claim carries the cue's IDENTITY, and a boolean cover cannot
///
/// The first cut of this test asked `text_arm::TextTap`'s question, "does
/// SOME delivered cue's `[start_rt, end_rt)` cover this frame's running
/// time", and it PASSED on the broken build. The broken offset shifts every
/// cue window by the same amount, so the cue for media `t + 6.67s` lands
/// exactly on the playhead's running time; the text branch is unsynced and
/// delivers the demuxer's readahead immediately, so that future cue is
/// reliably in the window to cover for the one that expired. The viewer sees
/// the WRONG cue (or the field's nothing, when the stream is paced and the
/// readahead never spans the offset); a boolean cover can't tell.
///
/// The fixture names each cue after the second it covers (`SEG NN` spans
/// `[NN, NN.9)`, see `gen-dash.sh`), so the claim here is the renderer's rule
/// WITH identity: a frame in media second N, tapped after the re-select, is
/// covered by a cue delivered after the re-select whose payload is `SEG N`.
/// Under the mistimed offset no such pair can exist at any readahead, the
/// covering cue's second is always `N + offset`, so the red does not depend
/// on delivery pacing or on which leg's leftovers sit in the window. The
/// arrival half (`wait_for_cue_at_or_after`) still runs first so a red
/// separates "mistimed" from "never delivered".
///
/// # And it must outlive the join transient
///
/// The frame it demands sits TWO seconds past the re-select playhead. Between
/// the old dead pad re-joining and the EOS-seat reclaim moving the seat off it
/// (~50 ms here), the demuxer's first re-add pushes cross that OLD branch,
/// whose pad offset is still 0, so the playhead second's own cue goes out
/// correctly timed even on the broken build (measured: `SEG 06` delivered at
/// `start_rt=6.667` while `SEG 07`/`SEG 08`, through the fresh branch, went
/// out at `0.33`/`1.33`). A claim satisfied by that one transient second
/// would pass while every cue after the reclaim is dead; two seconds out, only
/// the fresh branch is still delivering and the identity cover exists only if
/// its offset is right.
///
/// RED on 9d0a80fc (the arrival wait passes, then no identity-covered frame
/// beyond the transient for the whole bound); GREEN with the
/// origin-difference alignment.
#[test]
fn dash_reselected_embedded_text_covers_the_playhead() {
    /// Media to play through before the re-select, so the demuxer's re-add
    /// segment start/base are unambiguously nonzero (≥ 5 s once the external
    /// leg's own waits are behind it).
    const RESELECT_DWELL: usize = 6 * FPS;
    let (server, root) = serve();
    assert!(
        support::has_segmented_text(&root),
        "no segmented-text manifest under {}",
        root.display()
    );
    let harness = Harness::new();
    harness.load_and_play(&server.url("vod/manifest-text-seg.mpd"));
    let cue_window = text_arm::TextTap::install(&harness.playbin);
    // Every displayed frame: when it went past, where it sits in the media,
    // and the running time the renderer will place it at. The suite's own
    // boolean tap can't carry the identity claim below, so the frames are
    // logged raw and joined against the cue window at assertion time.
    let frames: Arc<Mutex<Vec<(Instant, gst::ClockTime, gst::ClockTime)>>> = Default::default();
    {
        let seen = frames.clone();
        let pad = text_arm::video_tap_pad(&harness.playbin).expect("a video chain to tap");
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(buffer) = info.buffer()
                && let Some(pts) = buffer.pts()
                && let Some(rt) = pad
                    .sticky_event::<gst::event::Segment>(0)
                    .and_then(|event| {
                        event
                            .segment()
                            .downcast_ref::<gst::ClockTime>()?
                            .to_running_time(pts)
                    })
            {
                seen.lock()
                    .expect("frame log")
                    .push((Instant::now(), pts, rt));
            }
            gst::PadProbeReturn::Ok
        });
    }
    let tap = harness.tap_overlay_text();
    let sid = harness.embedded_text_sid();
    harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
    harness.wait_for_cue(
        &server,
        &tap,
        "SEG",
        0,
        "the segmented track at first select",
    );

    let ext = harness.attach_and_materialize(&server.url("external/subs-a.vtt"));
    let ext_sid = harness
        .playbin
        .subtitle_stream_ids(ext)
        .into_iter()
        .next()
        .expect("the materialized external has a stream id");
    harness.select_subtitle(
        TrackTarget::ExternalSubtitle(ext),
        &ext_sid,
        "the external before the re-select",
    );
    harness.wait_for_cue(
        &server,
        &tap,
        "EXTA",
        0,
        "the external before the re-select",
    );
    harness.advance_past(RESELECT_DWELL, "the dwell that makes the re-add mid-stream");

    harness.mark("re-selecting the embedded track mid-stream");
    let at = harness.video_buffers() / FPS;
    // Everything before this instant, earlier legs' cues, earlier frames,
    // is out of the claim: the re-select must produce its own covered frame.
    let reselect = Instant::now();
    harness.select_subtitle(
        TrackTarget::Stream(Some(sid.clone())),
        &sid,
        "the embedded track after the external",
    );

    // Arrival first, so the two failure modes stay distinguishable. This
    // passes on the mistimed build too: the cue for the present ARRIVES, its
    // running time is simply a playhead's worth in the past.
    let hit = harness.wait_for_cue_at_or_after(
        &server,
        &tap,
        at,
        "the re-selected embedded track's arrival",
    );

    // THE CLAIM: a frame in media second N, beyond the join transient (see
    // the doc above), goes past covered by the cue FOR second N, both after
    // the re-select. On the broken alignment the cue covering any frame is
    // the one `offset` seconds in the future, its payload never matches the
    // frame's own second, and this times out.
    let beyond = (at + 2) as u64;
    harness.wait_until(
        &format!(
            "the playhead's own cue to cover a frame past second {beyond} after the re-select \
             (arrival hit SEG {hit:02})"
        ),
        EVENT_TIMEOUT,
        || {
            let cues = cue_window.window_cues();
            frames
                .lock()
                .expect("frame log")
                .iter()
                .any(|(when, pts, rt)| {
                    *when > reselect
                        && pts.seconds() >= beyond
                        && cues.iter().any(|(text, delivered, start_rt, end_rt)| {
                            *delivered > reselect
                                && *start_rt <= *rt
                                && end_rt.is_none_or(|end| end > *rt)
                                && text.trim() == format!("SEG {:02}", pts.seconds())
                        })
                })
        },
    );
    harness.shutdown();
}

#[test]
#[ignore = "SUPERSEDED. Cycle 0 was the superseded-pad reclaim and is now \
            green (pinned un-ignored by dash_embedded_text_rejoins_whether_\
            decodebin3_replaces_or_reuses_its_pad). Cycle 1 cannot mean anything against \
            THIS fixture - one unsegmented whole-period vtt, so the demuxer \
            emits nothing but gap ticks after the first push and a re-select \
            has nothing left to deliver. The segmented AdaptationSet this \
            asked for now exists, and the repeated-re-enable defect is \
            reproduced honestly against it by \
            dash_segmented_embedded_text_survives_repeated_re_enables. Keep \
            this one ignored; that is where the work is."]
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
        seen = harness.wait_for_cue(
            &server,
            &tap,
            "EMB",
            seen,
            &format!("embedded cycle {cycle}"),
        );
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
/// teardown and one `switch_cycle` failure over two runs, 4/4 clean without
/// it), and it MEASURABLY does not reproduce the field's slot REMOVAL
/// precondition: a debug run logs zero "no decodebin3 slot" and zero "reason
/// not-linked" lines, because the external's input stays connected here, so
/// `slot->input` is non-NULL and decodebin3 keeps the slot across the drain.
/// Reaching the field state needs the input DISCONNECTED as well as drained.
/// Kept as coverage for the direct switch itself, which nothing else here
/// exercises. Run it standalone:
/// `cargo test -p fcastplaybin --test dash_testbed -- --ignored --exact \
///  dash_embedded_then_external_direct_switch`
#[test]
#[ignore = "standalone only (a 5th parallel DASH pipeline destabilizes this \
            suite); models the field's direct embedded->external switch"]
fn dash_embedded_then_external_direct_switch() {
    let (server, root) = serve();
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest"
    );
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
    harness.wait_for_cue(
        &server,
        &tap,
        "EXTA",
        0,
        "the direct switch to the external",
    );
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
    assert!(
        support::has_embedded_text(&root),
        "no embedded-text manifest"
    );
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
        external = harness.wait_for_cue(
            &server,
            &tap,
            "EXTA",
            external,
            &format!("external {cycle}"),
        );
        harness.subtitle_off();
        harness.request_subtitle(TrackTarget::Stream(Some(sid.clone())));
        embedded =
            harness.wait_for_cue(&server, &tap, "EMB", embedded, &format!("embedded {cycle}"));
    }

    harness.assert_reaches_watermark("after embedded/external switches");
    harness.shutdown();
}
