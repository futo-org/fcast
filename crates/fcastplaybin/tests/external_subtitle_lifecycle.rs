//! Systematic coverage of the EXTERNAL subtitle lifecycle: attach, select,
//! switch, detach, and every degenerate source a caller can hand in.
//!
//! playbin3 supports one static `suburi`. This crate instead attaches each
//! external subtitle as its own `urisourcebin` into a decodebin3 request
//! pad, which buys any number of them at the price of a hand-rolled
//! lifecycle: a hold-until-selected data block, a replay seek that realigns
//! the input onto the item's timeline, a materialization watchdog and a
//! generation tag. Every one of those is a place where an attach can be
//! dropped on the floor, so the suite drives the same input through every
//! transport state rather than only the happy mid-play one.
//!
//! What the tests assert is deliberately OBSERVABLE: a stream materialized,
//! a cue crossed the overlay, the segment origin the cue rendered against,
//! the pipeline still moving afterwards. Internal routing state is not
//! consulted anywhere.
//!
//! `tests/scenarios.rs` owns the happy-path external scenarios and
//! `tests/subtitle_disable.rs` the enable/disable ones. This file covers
//! what neither does: the transport states other than mid-play, the
//! degenerate sources, and the duplicate attach.
//!
//! The whole suite passes against the current crate (on the PATCHED
//! playback plugin). Four defects were pinned red here before being fixed,
//! and their tests remain as the regression guards:
//!
//! * `attach_select_and_detach_while_paused_never_wedges_the_caller` detaching
//!   an external whose text branch was linked into subtitleoverlay, with the
//!   pipeline at rest in PAUSED, deadlocked in `Inner::remove_input` (fixed by
//!   deferring the removal).
//! * `attaching_the_same_uri_twice_is_refused_and_the_first_keeps_rendering`
//!   the same URI attached twice collided on one URI-derived stream id and the
//!   fallout was a subtitle path dead for the rest of the item. The duplicate
//!   attach is refused now, and the two re-attach tests
//!   (`reattaching_the_same_url_after_a_detach_renders_again`,
//!   `reattaching_the_same_url_while_paused_renders_after_resume`) pin the
//!   recovery mechanics the collision exposed, a data hold nothing lifted and
//!   an overlay seat held by a dead branch.
//! * `an_external_without_a_text_stream_is_reported_as_failed` a source
//!   carrying no text at all was never reported as failed.

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
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::{FTestSink, Recording},
    spec::{CueSpec, Fault, Pacing, StreamSpec},
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

/// Generous bound for anything the pipeline has to reach. Matches
/// `tests/scenarios.rs`: the suite runs many concurrent pipelines and a busy
/// box must not flake.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Bound for a teardown. The field failures these replace never returned.
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// Shortened materialization watchdog for the degenerate-source tests, so a
/// source that will never produce a stream is judged in seconds instead of
/// the production `EXTERNAL_SUB_TIMEOUT` of 5 s. Still comfortably longer
/// than a healthy attach takes here (measured well under 500 ms).
const SHORT_SUB_TIMEOUT: Duration = Duration::from_secs(2);

/// Unpaced everywhere: ftestsrc pushes as fast as the chain accepts and the
/// sinks do the syncing, exactly like `tests/scenarios.rs`.
const PACING: Pacing = Pacing::AsFastAsPossible;

/// Long enough that no item ends while a test is still working with it.
const LONG_CLIP: gst::ClockTime = gst::ClockTime::from_seconds(30);

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

/// Names a `StreamsSelected` that left the video or the audio slot empty on an
/// item that carries both, which means the crate decided from an INCOMPLETE
/// decodebin3 collection. `ftestsrc` exposes its elementary streams on separate
/// pads, so urisourcebin parses each one in its own parsebin and decodebin3
/// aggregates their collections one at a time. Which of them arrives first is a
/// race, and the incomplete-first case is the recurring signature ahead of the
/// load-time wedges this suite shows when several pipelines run at once.
///
/// Set `FCAST_TEST_TRACE_SELECT=1` (and pass `--nocapture` to see it on a
/// passing test) to turn it on. Off it costs one env read per event.
fn note_partial_selection(event: &PlaybinEvent) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !ON.get_or_init(|| std::env::var_os("FCAST_TEST_TRACE_SELECT").is_some()) {
        return;
    }
    let PlaybinEvent::StreamsSelected {
        video,
        audio,
        subtitle,
        ..
    } = event
    else {
        return;
    };
    if video.is_some() && audio.is_some() {
        return;
    }
    eprintln!("PARTIAL-SELECTION video={video:?} audio={audio:?} subtitle={subtitle:?}");
}

/// Cues with a distinguishing payload prefix, so the overlay tap can tell
/// WHICH external input a rendered cue came from.
fn prefixed_cues(prefix: &str, count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{prefix}{index:02}"))
        })
        .collect()
}

/// A plain video+audio item, the thing every test plays.
fn main_item(key: &str) -> fcasttest::scenario::ScenarioHandle {
    ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register()
}

/// A text-only side item, the thing every test attaches. Cues every 400 ms
/// so one is always close by whatever the video position.
fn sub_item(key: &str, prefix: &str) -> fcasttest::scenario::ScenarioHandle {
    ScenarioBuilder::new(key)
        .text(
            "text_0",
            prefixed_cues(prefix, 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .pacing(PACING)
        .register()
}

/// The timeline a pad renders against: the stream position whose running
/// time is zero, read off the pad's sticky segment exactly like the crate's
/// `overlay_timeline`. Cues sync against video iff both pads report the same
/// origin. Copied from `tests/scenarios.rs`, which is the model for reading
/// a pad's timeline.
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

/// Wait for the video sink's segment origin to reach `origin`. The timeout
/// panic dumps the pad's LIVE sticky segment: the rare loaded-run strand here
/// completes its flushing seek (RateChanged + AsyncDone posted) with the
/// origin still stale, and whether the pad then holds NO segment (a source
/// task dead on a FLUSHING latch) or the PRE-SEEK one (a stale sticky
/// re-push) is the discriminating fact the old panic never captured.
fn wait_video_origin(harness: &Harness, video_pad: &gst::Pad, origin: gst::ClockTime, what: &str) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if segment_origin(video_pad) == Some(origin) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "the video origin never moved to {origin} ({what}).\n\
                 video pad sticky segment: {:?}\n\
                 log: {:#?}",
                video_pad.sticky_event::<gst::event::Segment>(0),
                harness.log.borrow()
            );
        }
        harness.settle_pump();
        thread::sleep(Duration::from_millis(10));
    }
}

/// Every text payload crossing the overlay's subtitle input, with the origin
/// of the segment governing it at that instant.
type TextTap = text_arm::PositionTap;

/// A playbin whose sinks record, plus every event its callback produced.
/// Modelled on the harness in `tests/scenarios.rs`.
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    /// Whether a `play()` has been asked for and not yet withdrawn. What the
    /// re-drive in [`Harness::redrive_transport`] needs to tell a load's own
    /// preroll (settled PAUSED, nothing asked for yet) from a PLAYING target
    /// the pipeline dropped underneath us.
    wants_playing: Cell<bool>,
    /// A seek the crate REFUSED and parked, waiting to be re-driven from the
    /// next settled-PAUSED edge. See [`Harness::redrive_transport`].
    parked_seek: Cell<Option<Seek>>,
    video: Recording,
    /// One entry per load. Held so the sinks the factory builds stay
    /// reachable, never read: the video log alone answers "is the item still
    /// moving", and it survives every load.
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
        // The consumer arm's cue feed, established before anything can flow:
        // an unsynced text branch hands a whole external subtitle over in one
        // burst, and a tap armed later would see none of it (see
        // `support/text_arm.rs`).
        text_arm::arm(&playbin);
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
            video,
            _audio: audio,
        }
    }

    /// The one thing a real caller does that this harness used to skip: put
    /// back the transport the crate parked.
    ///
    /// The crate does NOT own the transport target. Two of its jobs hand work
    /// back to the caller instead of completing it, and both say so in an
    /// event:
    ///
    /// * `Job::Seek` (`src/lib.rs:4415`) refuses a seek that does not arrive at
    ///   a settled PAUSED, posts `QueueSeek` and commits PAUSED. The caller
    ///   re-issues the seek from the settle. `StateMachine`'s
    ///   `SeekSlot::Parked` arm (`src/state_machine.rs:420`) is that.
    /// * A pipeline that loses state (any element added while PLAYING has a
    ///   preroll to do: a late audio branch, an external subtitle branch) drops
    ///   ITSELF to PAUSED and, in `gst_element_lost_state`'s own words, "will
    ///   also not automatically go to PLAYING but let the parent/application
    ///   set us to PLAYING explicitly". The edge is `Paused`/pending `Paused`.
    ///   `StateMachine`'s `Phase::Running` dip arm (`src/state_machine.rs:541`)
    ///   keeps the PLAYING target across it and `Phase::Changing` re-commits it
    ///   once the dip settles.
    ///
    /// Neither had anything to re-drive them here, so a load that grew a branch
    /// after reaching PLAYING simply stopped, and a parked seek never happened.
    /// Both are timing races against the pipeline settling, which is why this
    /// only bit when several pipelines ran at once, and why it landed on a
    /// different test every time.
    ///
    /// Seek first, then the target, in `StateMachine`'s order: its seek slot
    /// takes precedence over the phase and the target correction waits for the
    /// seek's own settle.
    ///
    /// Lever: `FCAST_TEST_NO_TRANSPORT_REDRIVE`.
    fn redrive_transport(&self, event: &PlaybinEvent) {
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *OFF.get_or_init(|| std::env::var_os("FCAST_TEST_NO_TRANSPORT_REDRIVE").is_some()) {
            return;
        }
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
            note_partial_selection(&event);
            self.redrive_transport(&event);
            self.log.borrow_mut().push(event);
        }
    }

    /// Panics on any pipeline error already in the log.
    fn assert_no_error(&self, what: &str) {
        self.drain_events();
        if let Some(PlaybinEvent::Error { error, .. }) = self
            .log
            .borrow()
            .iter()
            .find(|event| matches!(event, PlaybinEvent::Error { .. }))
        {
            panic!("pipeline error {what}: {error}");
        }
    }

    fn wait_for(&self, what: &str, pred: impl FnMut(&PlaybinEvent) -> bool) {
        self.wait_for_within(what, EVENT_TIMEOUT, pred);
    }

    fn wait_for_within(
        &self,
        what: &str,
        bound: Duration,
        mut pred: impl FnMut(&PlaybinEvent) -> bool,
    ) {
        // Events can land before the wait that needs them (preroll posts
        // while the caller is still setting up), so the LOG is matched first
        // and the channel only for what has not arrived yet.
        self.drain_events();
        for event in self.log.borrow().iter() {
            if pred(event) {
                return;
            }
        }
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
                    note_partial_selection(&event);
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
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn load(&self, uri: &str) {
        self.load_at(uri, gst::ClockTime::ZERO);
    }

    fn load_at(&self, uri: &str, position: gst::ClockTime) {
        self.drain_events();
        self.log.borrow_mut().clear();
        // A load owns the transport until the test asks for one, and it
        // supersedes anything the previous item parked.
        self.wants_playing.set(false);
        self.parked_seek.set(None);
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position,
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

    /// The latest `StreamsSelected` subtitle slot in the log, if any.
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

    /// Every stream id in the latest advertised collection.
    fn collection_ids(&self) -> Vec<String> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => Some(
                    collection
                        .iter()
                        .filter_map(|stream| stream.stream_id().map(|id| id.to_string()))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Whether `ExternalSubtitleFailed` fired for `id`.
    fn failed(&self, id: ExternalSubId) -> bool {
        self.drain_events();
        self.log.borrow().iter().any(
            |event| matches!(event, PlaybinEvent::ExternalSubtitleFailed { id: got } if *got == id),
        )
    }

    /// The VIDEO anchor pad, waited for: the video chain is installed when the
    /// item's video ROUTES, which can trail the settled PLAYING.
    fn video_pad(&self) -> gst::Pad {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(pad) = text_arm::video_tap_pad(&self.playbin) {
                return pad;
            }
            assert!(
                Instant::now() < deadline,
                "the video chain never joined the pipeline; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// What is wired into the renderer right now, walked back to the
    /// decodebin3 pad feeding it: the overlay's subtitle input on the default
    /// arm, every per-stream appsink on the consumer arm. When a subtitle
    /// stops rendering this says WHICH branch is holding the renderer -- the
    /// seat's occupant on the arm that has a seat, and on the other arm the
    /// branch (or branches) the driver still believes in.
    fn text_renderer_occupants(&self) -> Vec<String> {
        let tails = text_arm::text_tail_pads(&self.playbin);
        if tails.is_empty() {
            return vec!["no renderer input exists yet".to_owned()];
        }
        tails
            .iter()
            .map(|tail| {
                let name = text_arm::text_tail_pad_name(tail);
                let Some(peer) = tail.peer() else {
                    return format!("{name}: unlinked");
                };
                let queue = peer
                    .parent_element()
                    .map(|element| element.name().to_string())
                    .unwrap_or_else(|| "?".to_owned());
                let upstream = peer
                    .parent_element()
                    .and_then(|element| element.static_pad("sink"))
                    .and_then(|sink| sink.peer());
                let feeder = upstream
                    .as_ref()
                    .map(|pad| {
                        format!(
                            "{}:{} stream_id={:?} segment_origin={:?}",
                            pad.parent_element()
                                .map(|element| element.name().to_string())
                                .unwrap_or_else(|| "?".to_owned()),
                            pad.name(),
                            pad.stream_id().map(|sid| sid.to_string()),
                            segment_origin(pad),
                        )
                    })
                    .unwrap_or_else(|| "nothing upstream".to_owned());
                format!("{name}: {queue} fed by {feeder}")
            })
            .collect()
    }

    /// Every decodebin3 TEXT output pad in the pipeline, with its stream id
    /// and whether it is linked onward.
    fn db3_text_pads(&self) -> Vec<String> {
        self.playbin
            .pipeline()
            .iterate_recurse()
            .into_iter()
            .flatten()
            .filter(|element| {
                element
                    .factory()
                    .is_some_and(|f| f.name().as_str() == "decodebin3")
            })
            .flat_map(|db3| {
                db3.src_pads()
                    .into_iter()
                    .map(|pad| {
                        format!(
                            "{}:{} stream_id={:?} origin={:?} -> {}",
                            db3.name(),
                            pad.name(),
                            pad.stream_id().map(|sid| sid.to_string()),
                            segment_origin(&pad),
                            pad.peer()
                                .map(|peer| format!(
                                    "{}:{}",
                                    peer.parent_element()
                                        .map(|e| e.name().to_string())
                                        .unwrap_or_else(|| "?".to_owned()),
                                    peer.name()
                                ))
                                .unwrap_or_else(|| "UNLINKED".to_owned())
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|line| line.contains("text"))
            .collect()
    }

    /// Records every text payload crossing the overlay's subtitle input
    /// together with the origin of the segment governing it at that instant.
    fn tap_overlay_text(&self) -> TextTap {
        text_arm::tap_cue_positions(&self.playbin)
    }

    /// Attach `uri` and wait for its first stream id to materialize.
    fn attach_and_materialize(&self, uri: &str) -> (ExternalSubId, String) {
        let id = self
            .playbin
            .attach_subtitle(uri)
            .expect("attaching the external input");
        let sid = self.materialized(id);
        (id, sid)
    }

    fn materialized(&self, id: ExternalSubId) -> String {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(sid) = self.playbin.subtitle_stream_ids(id).into_iter().next() {
                return sid;
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

    /// Request the subtitle slot onto `id` and pump until `sid` confirms.
    fn select_and_confirm(&self, id: ExternalSubId, sid: &str) {
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
        self.settle_pump();
        self.wait_until(
            &format!("the selection of {sid} to confirm"),
            EVENT_TIMEOUT,
            || self.last_selected_subtitle() == Some(Some(sid.to_owned())),
        );
    }

    fn attach_and_select(&self, uri: &str) -> (ExternalSubId, String) {
        let (id, sid) = self.attach_and_materialize(uri);
        self.select_and_confirm(id, &sid);
        (id, sid)
    }

    /// Wait until a cue with `prefix` beyond `already` recordings has crossed
    /// into the overlay, and return every recording with that prefix.
    fn wait_for_cue(
        &self,
        tap: &TextTap,
        prefix: &str,
        already: usize,
        what: &str,
    ) -> Vec<(String, Option<gst::ClockTime>)> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.drain_events();
            if tapped_with_prefix(tap, prefix).len() > already {
                return tapped_with_prefix(tap, prefix);
            }
            if Instant::now() >= deadline {
                // A cue that never arrives is almost always a text branch
                // that never linked, so report the seat and its holder
                // rather than only the event log.
                panic!(
                    "no {prefix} cue reached the renderer ({what}).\n\
                     renderer inputs: {:#?}\n\
                     decodebin3 text pads: {:#?}\n\
                     log: {:#?}",
                    self.text_renderer_occupants(),
                    self.db3_text_pads(),
                    self.log.borrow()
                );
            }
            self.settle_pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The cue count for `prefix`, sampled once delivery has gone QUIET.
    ///
    /// A baseline for "the next thing rendered" has to be taken before that
    /// thing happens, and it has to be stable. Both halves are transport
    /// properties, one per arm:
    ///
    /// * the consumer branch is unsynced, so an external subtitle is parsed and
    ///   handed over in ONE burst within milliseconds of the branch linking. A
    ///   baseline sampled after a re-attach has settled already contains
    ///   everything that input will ever deliver, and a wait for "one more cue"
    ///   can then only time out;
    /// * on the overlay arm a disposal lifts the outgoing queue's time cap to
    ///   wake a cue parked inside the renderer, and that push can cross the tap
    ///   microseconds after `detach_subtitle` has returned. Two samples a
    ///   quiescence apart put it on the baseline's side of the line rather than
    ///   letting it satisfy the assertion that follows.
    fn settled_cue_count(&self, tap: &TextTap, prefix: &str) -> usize {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut last = tapped_with_prefix(tap, prefix).len();
        loop {
            let quiet_until = Instant::now() + Duration::from_millis(250);
            while Instant::now() < quiet_until {
                self.settle_pump();
                thread::sleep(Duration::from_millis(10));
            }
            let now = tapped_with_prefix(tap, prefix).len();
            if now == last {
                return now;
            }
            assert!(
                Instant::now() < deadline,
                "{prefix} cues never stopped arriving, so no baseline can be \
                 taken: {last} -> {now}"
            );
            last = now;
        }
    }

    /// A graph-dump round-trip proves the worker is not wedged inside a
    /// previous job.
    fn assert_worker_alive(&self, what: &str) {
        let (tx, rx) = mpsc::channel();
        self.playbin.debug_graph_async(Box::new(move |_| {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the worker is wedged: {what}");
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died: {what}"),
            }
        }
    }

    /// Video buffers rendered so far. Used to prove the item is still
    /// MOVING, not merely that a call returned.
    fn video_buffers(&self) -> usize {
        self.video.buffer_count()
    }

    /// Assert video advances from here. The audio side is deliberately not
    /// checked: an audio sink is rebuilt per load and its recording rotates.
    fn assert_video_advances(&self, what: &str) {
        let before = self.video_buffers();
        self.wait_until(&format!("video to advance ({what})"), EVENT_TIMEOUT, || {
            self.video_buffers() > before + 2
        });
    }

    fn shutdown(&self) {
        // No transport survives a teardown, so the re-drive must not chase the
        // descent's edges back up.
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

fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> Vec<(String, Option<gst::ClockTime>)> {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|(payload, _)| payload.starts_with(prefix))
        .cloned()
        .collect()
}

/// Run `body` on its own thread and fail (rather than hang the harness) if it
/// has not returned within `bound`. On the failure path the playbin is
/// LEAKED: dropping it tears the pipeline down through the very branch the
/// wedged call is holding, which would hang the process instead of reporting
/// the failure. Same pattern as `tests/regression_paused_switch.rs`.
fn assert_returns_within(
    playbin: &Arc<FcastPlaybin>,
    bound: Duration,
    what: &str,
    body: impl FnOnce() + Send + 'static,
) {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("ext-sub-lifecycle".into())
        .spawn(move || {
            body();
            let _ = tx.send(());
        })
        .expect("spawning the bounded-call thread");
    match rx.recv_timeout(bound) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            std::mem::forget(playbin.clone());
            panic!("{what} did not return within {bound:?}");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} panicked on its own thread"),
    }
}

// ---------------------------------------------------------------------------
// 1. Attach and detach in every transport state
// ---------------------------------------------------------------------------

/// Attaching before ANY media is loaded. There is no decodebin3 to link into
/// yet, so this documents what the crate actually does with the request
/// rather than what a caller might hope: whatever the verdict, it must be a
/// verdict (not a silent hang) and it must not poison the load that follows.
#[test]
fn attach_before_any_load_is_resolved_and_the_next_load_still_plays() {
    init();
    let main = main_item("extlifestoppedmain");
    let subs = sub_item("extlifestoppedsubs", "STOP");

    let harness = Harness::new();
    harness.playbin.set_external_sub_timeout(SHORT_SUB_TIMEOUT);

    // No load has run: `inner.core` holds no decodebin3.
    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching with nothing loaded must not be an error");

    // The watchdog owns the verdict for an input that never materializes.
    harness.wait_until(
        "the stopped-state attach to be resolved either way",
        SHORT_SUB_TIMEOUT * 6,
        || harness.failed(id) || !harness.playbin.subtitle_stream_ids(id).is_empty(),
    );
    let survived = !harness.playbin.subtitle_stream_ids(id).is_empty();

    // Whatever the verdict, the pipeline must be usable afterwards.
    harness.load_and_play(&main.uri());
    harness.assert_video_advances("after an attach with nothing loaded");
    harness.assert_worker_alive("after an attach with nothing loaded");
    harness.assert_no_error("after an attach with nothing loaded");

    // A fresh attach on the now-loaded item must work regardless.
    let tap = harness.tap_overlay_text();
    let (_id2, _sid2) = harness.attach_and_select(&subs.uri());
    harness.wait_for_cue(&tap, "STOP", 0, "after re-attaching post-load");

    harness.shutdown();
    main.unregister();
    subs.unregister();
    assert!(
        !survived,
        "an attach with nothing loaded materialized a stream; if the crate \
         now defers such attaches onto the next load this assertion is the \
         thing to update, together with the doc on attach_subtitle_with_id"
    );
}

/// Attach WHILE the load job is still running (the receiver's
/// Load + AddSubtitleSource sequence), then detach it again before the load
/// finishes. The load must still complete and the detached input must leave
/// no trace in the advertised collection.
#[test]
fn attach_then_detach_mid_load_leaves_the_load_intact() {
    init();
    let main = main_item("extlifemidloadmain");
    let subs = sub_item("extlifemidloadsubs", "MIDL");

    let harness = Harness::new();
    harness.drain_events();
    harness.playbin.load_async(
        MediaInput::Uri(main.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    // The collection is announced inside the load job, before the start seek:
    // the earliest instant an attach can survive the load's input reset.
    harness.wait_for("the collection", |event| {
        matches!(event, PlaybinEvent::StreamCollection(_))
    });

    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching during the load");
    let sid = harness.materialized(id);
    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(10),
        "a mid-load detach",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(id).expect("detaching mid-load")
        },
    );

    harness.wait_for("Loaded", |event| {
        matches!(event, PlaybinEvent::Loaded { .. })
    });
    harness.play();
    harness.assert_video_advances("after a mid-load attach and detach");

    // The detached input must fall out of the advertised collection.
    harness.wait_until(
        "the detached external to leave the collection",
        EVENT_TIMEOUT,
        || !harness.collection_ids().contains(&sid),
    );
    assert!(!harness.playbin.has_external_subtitles());
    harness.assert_worker_alive("after a mid-load attach and detach");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// Mid-play attach, select, render, then detach WHILE it is the selected
/// track. The overlay loses its input under its feet, which is what happens
/// whenever an external that is being SHOWN goes away: a user removing it,
/// and (the field-reachable form) the crate's own `fail_subtitle` detaching
/// an input that errored while selected.
///
/// The item must keep playing, and, the part that regressed, the subtitle
/// path must still WORK afterwards. The follow-up external here is a
/// DIFFERENT URI on purpose, so a failure cannot be blamed on two inputs
/// sharing a stream-id (that confound has its own test below).
#[test]
fn a_second_external_renders_after_the_selected_one_was_detached() {
    init();
    let main = main_item("extlifedetselmain");
    let first = sub_item("extlifedetselsubs", "DSEL");
    let second = sub_item("extlifedetselnext", "NEXT");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();
    let (id, sid) = harness.attach_and_select(&first.uri());
    harness.wait_for_cue(&tap, "DSEL", 0, "before the detach");

    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(10),
        "detaching the selected external",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(id).expect("detaching the selected")
        },
    );

    harness.wait_until(
        "the detached external to leave the collection",
        EVENT_TIMEOUT,
        || !harness.collection_ids().contains(&sid),
    );
    harness.assert_video_advances("after detaching the selected external");
    harness.assert_worker_alive("after detaching the selected external");
    harness.assert_no_error("after detaching the selected external");

    // The one live text slot must be free again. A stale branch left
    // linked there makes the link policy skip EVERY later text stream
    // ("another text branch already feeds the consumer", and once
    // "subtitle_sink already linked"), which kills subtitles for the
    // rest of the item without any error being reported.
    let (_id2, _sid2) = harness.attach_and_select(&second.uri());
    harness.wait_for_cue(&tap, "NEXT", 0, "after detaching the selected external");

    harness.shutdown();
    main.unregister();
    first.unregister();
    second.unregister();
}

/// Attach, select and detach with the pipeline at rest in PAUSED.
///
/// The DETACH deadlocks, deterministically. Captured with gdb on the patched
/// playback plugin, the cycle closes across four threads:
///
/// ```text
/// ext-sub-lifecyc  detach_subtitle -> remove_input -> send_event(FLUSH_START)
///                  -> gst_multi_queue_sink_event
///                  -> gst_pad_pause_task        [waits for multiqueue1:src]
/// multiqueue1:src  gst_multi_queue_loop -> gst_pad_push
///                  -> gst_queue_chain_buffer_or_list -> g_cond_wait
///                                               [the text queue is full]
/// queue0:src       gst_subtitle_overlay_subtitle_sink_event
///                  (gstsubtitleoverlay.c:2246) -> do_probe_callbacks
///                  -> g_cond_wait               [parked in a BLOCK probe]
/// multiqueue1:src  gst_subtitle_overlay_video_sink_chain -> the video sink
///                  -> gst_base_sink_wait_preroll [waits for PLAYING]
/// ```
///
/// So it is the same shape as the switch deadlock
/// `tests/regression_paused_switch.rs` pins, reached through a different
/// door: that fix POSTPONES the eager flush inside `pump_selection`
/// (`Inner::run_deferred_text_work`, lever `FCAST_NO_TEXT_WORK_DEFERRAL`),
/// while `Inner::remove_input` flushes its decodebin3 sink pads
/// unconditionally and has no such deferral.
///
/// The same detach mid-PLAY returns fine, which
/// `a_second_external_renders_after_the_selected_one_was_detached` shows, and
/// detaching a NEVER-SELECTED external while paused returns fine too, which
/// `detaching_an_unselected_external_while_paused_returns` shows. So the
/// trigger is precisely: the detached input's text branch is LINKED into
/// subtitleoverlay and the pipeline is at rest in PAUSED.
///
/// In production the wedge is worse than a blocked caller: the receiver goes
/// through `detach_subtitle_async`, so `Job::DetachSub` parks the fcastplaybin
/// WORKER thread, and with it every later load, seek, stop and shutdown.
///
/// The detach runs on its own thread with the assertion on this one, so a
/// wedge fails the test instead of hanging the binary.
#[test]
fn attach_select_and_detach_while_paused_never_wedges_the_caller() {
    init();
    let main = main_item("extlifepausedmain");
    let subs = sub_item("extlifepausedsubs", "PAUS");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    // Let the item genuinely run before it is parked.
    harness.assert_video_advances("before pausing");
    harness.pause();

    let id = {
        let playbin = harness.playbin.clone();
        let uri = subs.uri();
        let slot: Arc<Mutex<Option<ExternalSubId>>> = Arc::new(Mutex::new(None));
        let out = slot.clone();
        assert_returns_within(
            &harness.playbin,
            Duration::from_secs(15),
            "attaching while paused",
            move || {
                let id = playbin.attach_subtitle(&uri).expect("attach while paused");
                *out.lock().expect("id slot") = Some(id);
            },
        );
        let id = slot.lock().expect("id slot").expect("the attach ran");
        id
    };
    let sid = harness.materialized(id);
    harness.select_and_confirm(id, &sid);

    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(15),
        "detaching while paused",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(id).expect("detach while paused")
        },
    );

    // Resuming must still work: the field symptom of this family is a
    // pipeline that never leaves PAUSED again.
    harness.play();
    harness.assert_video_advances("after a paused attach/select/detach cycle");
    harness.assert_worker_alive("after a paused attach/select/detach cycle");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// The control for the test above: same paused detach, but with the subtitle
/// slot held explicitly OFF so the external's text branch never reaches
/// subtitleoverlay. Nothing downstream is parked on the overlay, so the flush
/// inside `remove_input` has nothing to wait for and the detach returns.
///
/// Keeping this green is what makes the failing test above a statement about
/// the LINKED text branch rather than about PAUSED in general.
///
/// The slot has to be turned off EXPLICITLY. Simply not asking for the track
/// is not enough: decodebin3 auto-selects a fresh text stream, the link
/// policy joins it, and the branch is live with nobody having requested it
/// (the crate says as much in `decisions::external_error_action`). A first
/// draft of this control omitted the disable and deadlocked exactly like the
/// test it was meant to contrast with.
#[test]
fn detaching_an_unselected_external_while_paused_returns() {
    init();
    let main = main_item("extlifepausedunselmain");
    let subs = sub_item("extlifepausedunselsubs", "PUNS");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    harness.assert_video_advances("before pausing");

    // Subtitles OFF before the attach, and kept off: nothing may link.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.settle_pump();

    let (id, _sid) = harness.attach_and_materialize(&subs.uri());
    harness.wait_until(
        "the subtitle slot to be confirmed off",
        EVENT_TIMEOUT,
        || harness.last_selected_subtitle() == Some(None),
    );
    assert!(
        !text_arm::text_branch_linked(&harness.playbin),
        "the control needs an UNLINKED text branch, but one reached the \
         renderer anyway: {:#?}",
        harness.text_renderer_occupants()
    );
    harness.pause();

    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(15),
        "detaching an unselected external while paused",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(id).expect("detach while paused")
        },
    );

    harness.play();
    harness.assert_video_advances("after detaching an unselected external while paused");
    harness.assert_worker_alive("after detaching an unselected external while paused");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// Detaching an external that was attached but NEVER selected. Its data hold
/// is still armed and its branch never linked, so nothing downstream has ever
/// seen it: the detach must not wait on a branch that does not exist, and the
/// selection engine must not be left with a desire parked on a gone input.
#[test]
fn detaching_a_never_selected_external_is_clean() {
    init();
    let main = main_item("extlifeneverselmain");
    let never = sub_item("extlifeneverselnever", "NEVR");
    let other = sub_item("extlifeneverselother", "OTHR");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let (never_id, never_sid) = harness.attach_and_materialize(&never.uri());
    // Deliberately no request_track for `never_id`.
    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(10),
        "detaching a never-selected external",
        {
            let playbin = harness.playbin.clone();
            move || {
                playbin
                    .detach_subtitle(never_id)
                    .expect("detaching a never-selected external")
            }
        },
    );
    harness.wait_until(
        "the never-selected external to leave the collection",
        EVENT_TIMEOUT,
        || !harness.collection_ids().contains(&never_sid),
    );
    // A second detach of the same id is a caller error, not a crash.
    assert!(
        harness.playbin.detach_subtitle(never_id).is_err(),
        "detaching an already-detached id must report an error"
    );

    // The subtitle path must still work afterwards.
    let (_id, _sid) = harness.attach_and_select(&other.uri());
    harness.wait_for_cue(&tap, "OTHR", 0, "after detaching a never-selected external");
    harness.assert_worker_alive("after detaching a never-selected external");
    harness.shutdown();
    main.unregister();
    never.unregister();
    other.unregister();
}

/// Attaching a URI that is ALREADY attached is refused, on both entry
/// points, and the refusal leaves the original input untouched.
///
/// Two inputs on one URI cannot be told apart. With no upstream stream-id
/// to inherit, GStreamer's `gst_pad_create_stream_id` derives one by
/// querying the element's URI and hashing it, so two `urisourcebin`s on the
/// same URL report the SAME stream id, in production as much as here. And
/// everything downstream of `subtitle_stream_ids` is a stream-id lookup
/// that takes the first or any match (the selection engine's external
/// mapping, `unblock_selected_externals`, `poll_text_policy`'s join-time
/// replay, `verify_replay`'s still-selected check, and, in the receiver,
/// `Player::external_stream_sid_of`). Twins under one id made every one of
/// those answer about the wrong input, and the observed end state was a
/// subtitle path dead for the rest of the item. The crate now refuses to
/// create the second twin, which is the only place the ambiguity is still
/// representable.
///
/// The synchronous refusal is an `Err`. The receiver's asynchronous path
/// must see it as `ExternalSubtitleFailed` for the duplicate id, which its
/// catalog handling already turns into a dropped entry and a
/// ResourceNotFound verdict for the sender.
#[test]
fn attaching_the_same_uri_twice_is_refused_and_the_first_keeps_rendering() {
    init();
    let main = main_item("extlifesamesidmain");
    let subs = sub_item("extlifesamesidsubs", "SSID");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let (first, first_sid) = harness.attach_and_materialize(&subs.uri());

    // Synchronous entry point.
    assert!(
        harness.playbin.attach_subtitle(&subs.uri()).is_err(),
        "a second attach of an already-attached URI must be refused, the \
         duplicate would share the first input's URI-derived stream id"
    );

    // Asynchronous entry point, the receiver's path. The id is reserved up
    // front and the refusal has to come back as an event.
    let dup = harness.playbin.allocate_subtitle_id();
    harness.playbin.attach_subtitle_async(dup, subs.uri());
    harness.wait_until(
        "ExternalSubtitleFailed for the duplicate attach",
        EVENT_TIMEOUT,
        || harness.failed(dup),
    );

    // The refusal must not have disturbed the original input.
    harness.select_and_confirm(first, &first_sid);
    harness.wait_for_cue(&tap, "SSID", 0, "after the duplicate was refused");
    harness.assert_video_advances("after the duplicate was refused");
    harness.assert_worker_alive("after the duplicate was refused");
    harness.assert_no_error("after the duplicate was refused");

    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// Detach the rendering external, then immediately re-attach the SAME URL
/// and select it again. The re-attach must render.
///
/// This is the receiver's remove-then-add sequence for a subtitle the user
/// toggles, and it is the legitimate twin of the stream-id collision the
/// refusal test above pins. Stream ids are URI-derived, so the re-attach
/// materializes under the very sid the selection engine still has APPLIED,
/// and two defects used to leave that subtitle dead for the rest of the
/// item while the selection reported success:
///
/// * the engine dispatches nothing for a desire that equals the applied
///   selection, and the data hold (`hold_until_selected`) was only ever lifted
///   by a SELECT_STREAMS confirmation, so the re-attached input's buffers
///   stayed blocked at its source pads forever, and
/// * when decodebin3 exposes a fresh output pad for the re-attached stream
///   while the detached input's pad still holds subtitleoverlay's ONE subtitle
///   seat (its sticky segment wiped by `remove_input`'s flush, nothing upstream
///   ever feeding it again), the link policy answered every later text stream
///   with "subtitle_sink already linked; skipping extra text stream", forever.
///   Captured topology from a failing run, six seconds after such a detach:
///
/// ```text
/// subtitle_sink: queue0 fed by fpb-decodebin:text_0 stream_id=".. -text_0" origin=None
/// decodebin3 text pads:
///   fpb-decodebin:text_0 stream_id=".. -text_0" origin=None            -> queue0:sink
///   fpb-decodebin:text_1 stream_id=".. -text_0" origin=Some(0:00:00)   -> fakesink1:sink
/// ```
///
/// The hold is now lifted at the policy's settle points whenever the held
/// input's stream IS the applied subtitle, and a seat held by a branch
/// whose decodebin3 pad has no sticky segment is reclaimed for the stream
/// that can actually render.
#[test]
fn reattaching_the_same_url_after_a_detach_renders_again() {
    init();
    let main = main_item("extlifedupmain");
    let subs = sub_item("extlifedupsubs", "DUPE");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let (first, _first_sid) = harness.attach_and_select(&subs.uri());
    harness.wait_for_cue(&tap, "DUPE", 0, "before the detach");

    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(10),
        "detaching the rendering external",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(first).expect("detaching the first")
        },
    );

    // The baseline is taken HERE, between the detach and the re-attach, and
    // not after the re-attach has settled: see `settled_cue_count`. The
    // detached input's branch is gone, so nothing can be delivered until the
    // re-attached one links.
    let before = harness.settled_cue_count(&tap, "DUPE");

    // Re-attach STRAIGHT AWAY, no wait for the collection to drop the old
    // stream. The receiver's remove+add lands in exactly this window, and
    // it is the window where the old sid is still applied (and possibly
    // still advertised) when the new input materializes under it.
    let (second, second_sid) = harness.attach_and_materialize(&subs.uri());
    harness.select_and_confirm(second, &second_sid);
    harness.assert_video_advances("after re-attaching the same URL");
    harness.assert_worker_alive("after re-attaching the same URL");

    // The re-attach must render NEW cues. `wait_for_cue` reports every
    // renderer input and every decodebin3 text pad on failure, which is where
    // a dead branch still holding the renderer is visible.
    harness.wait_for_cue(&tap, "DUPE", before, "after re-attaching the same URL");

    harness.assert_no_error("after the detach and re-attach cycle");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// The PAUSED variant of the re-attach: detach the rendering external at
/// rest in PAUSED, re-attach the same URL still paused, select it, resume.
/// The re-attach must render after the resume.
///
/// This pins the data-hold defect on its own, deterministically. A detach
/// at rest in PAUSED DEFERS the input's removal (`remove_input_or_defer`,
/// the inline flush would wedge the caller), so the old input keeps its
/// decodebin3 pads and its stream id never leaves the collection, which
/// means the engine's applied subtitle survives the detach. The re-attach
/// then materializes under that very sid, the selection has nothing to
/// dispatch, and no SELECT_STREAMS confirmation ever runs the
/// confirmation-time hold release (`unblock_selected_externals`) for the
/// new input. Its buffers stayed blocked at its source pads forever while
/// the selection reported success. The mid-play re-attach test above does
/// not reach this defect because a mid-play detach releases the request
/// pads inline and decodebin3 posts the shrunk collection synchronously
/// inside the detach, so the engine always re-dispatches there.
///
/// The link policy now lifts the hold at its settle points whenever the
/// held input's stream IS the confirmed applied subtitle.
#[test]
fn reattaching_the_same_url_while_paused_renders_after_resume() {
    init();
    let main = main_item("extlifepausedredomain");
    let subs = sub_item("extlifepausedredosubs", "PRDO");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let (first, _first_sid) = harness.attach_and_select(&subs.uri());
    harness.wait_for_cue(&tap, "PRDO", 0, "before pausing");
    harness.pause();

    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(15),
        "detaching while paused",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(first).expect("detach while paused")
        },
    );

    // The baseline, before the re-attach opens the window in which a cue may
    // be delivered again (`settled_cue_count`). On the consumer arm the
    // re-attached input is handed over while the pipeline is still PAUSED --
    // the branch prerolls and the appsink is unsynced -- so "after the
    // resume" below is the LATEST the cue may appear, not the earliest. The
    // contract asserted is the one this test is named for either way: the
    // re-attached input renders, rather than sitting behind a data hold no
    // confirmation ever released.
    let before = harness.settled_cue_count(&tap, "PRDO");

    // Re-attach the same URL while still paused, on its own thread like
    // every other paused-state call here.
    let second = {
        let playbin = harness.playbin.clone();
        let uri = subs.uri();
        let slot: Arc<Mutex<Option<ExternalSubId>>> = Arc::new(Mutex::new(None));
        let out = slot.clone();
        assert_returns_within(
            &harness.playbin,
            Duration::from_secs(15),
            "re-attaching while paused",
            move || {
                let id = playbin
                    .attach_subtitle(&uri)
                    .expect("re-attach while paused");
                *out.lock().expect("id slot") = Some(id);
            },
        );
        let id = slot.lock().expect("id slot").expect("the attach ran");
        id
    };
    let second_sid = harness.materialized(second);
    harness.select_and_confirm(second, &second_sid);

    harness.play();
    harness.assert_video_advances("after resuming with the re-attached subtitle");

    harness.wait_for_cue(
        &tap,
        "PRDO",
        before,
        "after resuming with the re-attached subtitle",
    );

    harness.assert_worker_alive("after the paused re-attach cycle");
    harness.assert_no_error("after the paused re-attach cycle");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

// ---------------------------------------------------------------------------
// 2. Switching among several externals
// ---------------------------------------------------------------------------

/// Three externals, switched round-robin with NO wait between the requests,
/// so each `request_track` supersedes a selection still in flight. The engine
/// is latest-wins, so the observable contract is that the pipeline settles on
/// the LAST request and renders it, and that the superseded ones do not leave
/// the branch stuck.
#[test]
fn fast_switches_that_supersede_each_other_land_on_the_last_request() {
    init();
    let main = main_item("extlifefastmain");
    let a = sub_item("extlifefasta", "FSTA");
    let b = sub_item("extlifefastb", "FSTB");
    let c = sub_item("extlifefastc", "FSTC");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let (id_a, sid_a) = harness.attach_and_materialize(&a.uri());
    let (id_b, _sid_b) = harness.attach_and_materialize(&b.uri());
    let (id_c, sid_c) = harness.attach_and_materialize(&c.uri());

    // Two full round trips of superseding requests, ending on C.
    for _ in 0..2 {
        for id in [id_a, id_b, id_c] {
            harness
                .playbin
                .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
            // One pump per request: enough to dispatch, not enough to settle,
            // which is what makes the next request supersede an in-flight one.
            harness.playbin.pump_selection(harness.gate());
        }
    }
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_c));

    harness.wait_until("the selection to settle on C", EVENT_TIMEOUT, || {
        harness.last_selected_subtitle() == Some(Some(sid_c.clone()))
    });
    harness.wait_for_cue(&tap, "FSTC", 0, "after the superseding switches");
    harness.assert_video_advances("after the superseding switches");

    // And the engine is not stuck on C: a switch back must still take.
    harness.select_and_confirm(id_a, &sid_a);
    harness.wait_for_cue(&tap, "FSTA", 0, "switching back after the fast round-robin");

    harness.assert_worker_alive("after the superseding switches");
    harness.assert_no_error("after the superseding switches");
    harness.shutdown();
    main.unregister();
    a.unregister();
    b.unregister();
    c.unregister();
}

// ---------------------------------------------------------------------------
// 3. Degenerate sources
// ---------------------------------------------------------------------------

/// A URI nothing serves, through BOTH entry points.
///
/// `attach_subtitle` is synchronous and reports a source that refuses to
/// leave NULL as an `Err`. `attach_subtitle_async` is what the receiver
/// actually calls, and it must turn that same `Err` into an
/// `ExternalSubtitleFailed` event: a caller that only watches events would
/// otherwise wait for a subtitle that is never coming. Either way the item
/// must keep playing and nothing must stay attached.
#[test]
fn a_bad_uri_external_is_refused_and_reported_without_disturbing_playback() {
    init();
    let main = main_item("extlifebadurimain");
    let good = sub_item("extlifebadurigood", "BURI");
    const NOWHERE: &str = "ftest://extlifenosuchscenarioanywhere";

    let harness = Harness::new();
    harness.playbin.set_external_sub_timeout(SHORT_SUB_TIMEOUT);
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    // Synchronous: an unresolvable source cannot even reach the pipeline's
    // state, so the attach itself is the verdict.
    assert!(
        harness.playbin.attach_subtitle(NOWHERE).is_err(),
        "an unresolvable subtitle URI must be reported to the caller"
    );
    assert!(
        !harness.playbin.has_external_subtitles(),
        "a refused attach must roll itself back"
    );

    // Asynchronous: the receiver's path. The id is reserved up front and the
    // failure has to come back as an event.
    let id = harness.playbin.allocate_subtitle_id();
    harness
        .playbin
        .attach_subtitle_async(id, NOWHERE.to_owned());
    harness.wait_until(
        "ExternalSubtitleFailed for the bad URI",
        SHORT_SUB_TIMEOUT * 12,
        || harness.failed(id),
    );

    harness.assert_video_advances("after a bad-URI external");
    harness.assert_worker_alive("after a bad-URI external");
    assert!(
        !harness.playbin.has_external_subtitles(),
        "a failed external must be detached, not left attached"
    );

    // A good one attached afterwards must still work.
    let (_id, _sid) = harness.attach_and_select(&good.uri());
    harness.wait_for_cue(&tap, "BURI", 0, "after a bad-URI external");

    harness.shutdown();
    main.unregister();
    good.unregister();
}

/// A source that resolves fine but carries NO text stream (the user picked an
/// audio file as their subtitle). Nothing text-shaped ever materializes, so
/// the caller must get a verdict rather than an input that sits there forever.
#[test]
fn an_external_without_a_text_stream_is_reported_as_failed() {
    init();
    let main = main_item("extlifenotextmain");
    let audio_only = ScenarioBuilder::new("extlifenotextsubs")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.playbin.set_external_sub_timeout(SHORT_SUB_TIMEOUT);
    harness.load_and_play(&main.uri());

    let id = harness
        .playbin
        .attach_subtitle(&audio_only.uri())
        .expect("attaching an audio-only source as a subtitle");

    // The watchdog fires on "produced no stream". An audio-only source DOES
    // produce a stream, just not a text one, so this is exactly the hole the
    // test looks for: record which way it goes before asserting.
    let deadline = Instant::now() + SHORT_SUB_TIMEOUT * 6;
    let mut failed = false;
    while Instant::now() < deadline && !failed {
        failed = harness.failed(id);
        harness.settle_pump();
        thread::sleep(Duration::from_millis(20));
    }
    let ids = harness.playbin.subtitle_stream_ids(id);

    harness.assert_video_advances("after attaching a text-less external");
    harness.assert_worker_alive("after attaching a text-less external");
    harness.shutdown();
    main.unregister();
    audio_only.unregister();

    assert!(
        failed,
        "an external with no text stream was never reported as failed, and \
         `subtitle_stream_ids` hands the caller {ids:?} instead: a caller \
         following the documented contract would offer that AUDIO stream to \
         the user as a subtitle track and request it in the SUBTITLE slot. \
         The materialization watchdog only asks whether the input produced \
         ANY stream (`check_subtitle` -> `input.stream_ids().is_empty()`), \
         so a source with no text at all reads as a healthy subtitle"
    );
}

/// A whole container handed in as "the subtitle": video, audio AND text. Only
/// its TEXT stream may be usable as a subtitle track, and its video must not
/// displace the item's own. The interesting failure mode is the selection
/// engine's no-text-without-video rule seeing a second video stream.
#[test]
fn an_external_carrying_av_as_well_as_text_renders_only_its_text() {
    init();
    let main = main_item("extlifecontainermain");
    let container = ScenarioBuilder::new("extlifecontainersubs")
        .video("video_0")
        .audio("audio_0")
        .text(
            "text_0",
            prefixed_cues("CTNR", 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(PACING)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();
    let video_before = harness.video_buffers();

    let id = harness
        .playbin
        .attach_subtitle(&container.uri())
        .expect("attaching a full container as a subtitle");
    // Wait for the TEXT stream specifically, which is the one a subtitle
    // caller means. `subtitle_stream_ids` returns every stream the input
    // produced, so pick by suffix.
    harness.wait_until("the container's text stream", EVENT_TIMEOUT, || {
        harness
            .playbin
            .subtitle_stream_ids(id)
            .iter()
            .any(|sid| sid.ends_with("text_0"))
    });
    let text_sid = harness
        .playbin
        .subtitle_stream_ids(id)
        .into_iter()
        .find(|sid| sid.ends_with("text_0"))
        .expect("the container's text stream id");

    harness.select_and_confirm(id, &text_sid);
    harness.wait_for_cue(&tap, "CTNR", 0, "from a container handed in as a subtitle");

    // The item's own video must still be the one rendering.
    // Bounded rather than instantaneous: on the consumer arm the cue above
    // arrives within milliseconds of the branch linking (the appsink is
    // unsynced), which can be inside the cadence of a single video frame, so
    // an immediate comparison would be measuring frame pacing rather than the
    // property. The property is that the item's own video KEEPS rendering
    // with a video-bearing external selected.
    harness.wait_until(
        "the item's own video to render past the attach",
        EVENT_TIMEOUT,
        || harness.video_buffers() > video_before,
    );
    harness.assert_video_advances("with a video-bearing external selected");
    harness.assert_worker_alive("with a video-bearing external selected");
    harness.assert_no_error("with a video-bearing external selected");
    harness.shutdown();
    main.unregister();
    container.unregister();
}

/// A text stream with zero cues: the source schedules no buffers at all and
/// EOSes on its first push attempt. The stream still materializes, so this is
/// not the watchdog's case: selecting it must simply render nothing, without
/// taking the branch (or the item) down with it.
#[test]
fn an_empty_external_selects_without_breaking_the_item() {
    init();
    let main = main_item("extlifeemptymain");
    let empty = ScenarioBuilder::new("extlifeemptysubs")
        .text("text_0", Vec::new())
        .duration(LONG_CLIP)
        .pacing(PACING)
        .register();
    let good = sub_item("extlifeemptygood", "GOOD");

    let harness = Harness::new();
    harness.playbin.set_external_sub_timeout(SHORT_SUB_TIMEOUT);
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let id = harness
        .playbin
        .attach_subtitle(&empty.uri())
        .expect("attaching an empty subtitle source");
    // Either it materializes (and renders nothing) or the watchdog fails it.
    // Both are defensible; a hang is not.
    harness.wait_until(
        "a verdict on the empty external",
        SHORT_SUB_TIMEOUT * 6,
        || harness.failed(id) || !harness.playbin.subtitle_stream_ids(id).is_empty(),
    );
    if !harness.failed(id) {
        let sid = harness.materialized(id);
        harness.select_and_confirm(id, &sid);
    }

    harness.assert_video_advances("with an empty external attached");

    // The path must still be usable: a good external attached afterwards has
    // to render. This is the real risk of an empty source, an overlay branch
    // wedged on a stream that immediately EOSed.
    let (_good_id, _good_sid) = harness.attach_and_select(&good.uri());
    harness.wait_for_cue(&tap, "GOOD", 0, "after an empty external");

    harness.assert_worker_alive("after an empty external");
    harness.assert_no_error("after an empty external");
    harness.shutdown();
    main.unregister();
    empty.unregister();
    good.unregister();
}

/// A source that EOSes on its very first buffer. Unlike the empty one it does
/// announce a text stream with cues, then stops immediately, which is the
/// shape of a truncated download.
#[test]
fn an_external_that_eoses_immediately_does_not_wedge_the_branch() {
    init();
    let main = main_item("extlifeeosmain");
    let truncated = ScenarioBuilder::new("extlifeeossubs")
        .stream(
            StreamSpec::text(
                "text_0",
                prefixed_cues("TRUN", 70, gst::ClockTime::from_mseconds(400)),
            )
            .with_fault(Fault::EosAt { buffer_index: 0 }),
        )
        .duration(LONG_CLIP)
        .pacing(PACING)
        .register();
    let good = sub_item("extlifeeosgood", "AFTR");

    let harness = Harness::new();
    harness.playbin.set_external_sub_timeout(SHORT_SUB_TIMEOUT);
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let id = harness
        .playbin
        .attach_subtitle(&truncated.uri())
        .expect("attaching a truncated subtitle source");
    harness.wait_until(
        "a verdict on the truncated external",
        SHORT_SUB_TIMEOUT * 6,
        || harness.failed(id) || !harness.playbin.subtitle_stream_ids(id).is_empty(),
    );
    if !harness.failed(id) {
        let sid = harness.materialized(id);
        harness.select_and_confirm(id, &sid);
    }
    harness.assert_video_advances("with an immediately-EOSing external selected");

    // The branch must not be stuck on the dead stream.
    let (_good_id, _good_sid) = harness.attach_and_select(&good.uri());
    harness.wait_for_cue(&tap, "AFTR", 0, "after an immediately-EOSing external");

    harness.assert_worker_alive("after an immediately-EOSing external");
    harness.assert_no_error("after an immediately-EOSing external");
    harness.shutdown();
    main.unregister();
    truncated.unregister();
    good.unregister();
}

// ---------------------------------------------------------------------------
// 4 + 5. Seeks and timeline correctness
// ---------------------------------------------------------------------------

/// Move the video origin with a user seek FIRST, then attach. The external
/// joins a branch whose timeline already moved, so only the join-time replay
/// can put it on the same origin. Every cue it renders must carry the video's
/// origin.
#[test]
fn seek_then_attach_renders_on_the_sought_timeline() {
    init();
    let origin = gst::ClockTime::from_seconds(5);
    let main = main_item("extlifeseekattachmain");
    let subs = sub_item("extlifeseekattachsubs", "SKAT");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();
    seek_to(&harness, origin);

    let (_id, _sid) = harness.attach_and_select(&subs.uri());
    let cues = harness.wait_for_cue(&tap, "SKAT", 0, "after seek-then-attach");
    assert_origins(&cues, origin, "seek-then-attach");

    harness.assert_worker_alive("after seek-then-attach");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// The other order: attach and render FIRST, then seek. The pipeline seek
/// travels the sink chains and decodebin3 forwards it up the main input only,
/// so the crate has to forward it into the live external by hand
/// (`forward_seek_to_live_externals`). Cues rendered after the seek must
/// carry the new origin.
#[test]
fn attach_then_seek_keeps_the_external_on_the_video_timeline() {
    init();
    let origin = gst::ClockTime::from_seconds(6);
    let main = main_item("extlifeattachseekmain");
    let subs = sub_item("extlifeattachseeksubs", "ATSK");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let (_id, _sid) = harness.attach_and_select(&subs.uri());
    harness.wait_for_cue(&tap, "ATSK", 0, "before the seek");
    // SETTLED, not "the first cue to arrive". The consumer branch is UNSYNCED:
    // the attach hands the whole external over in one burst, and the burst is
    // concurrent with this thread. A baseline sampled the moment a cue first
    // appears therefore splits the burst -- and everything still to come in it
    // lands on the post-seek side of the assertion below, carrying the origin
    // it was correctly delivered against (0) because it was delivered BEFORE
    // the seek was requested. That is the whole of the long-running
    // attach-then-seek flake: measured 70 of 70 sampled on an idle machine but
    // 1..36 of 70 under 28-way parallel load, and no crate change can fix it,
    // since the crate cannot un-deliver a cue the caller had not yet asked to
    // move. `settled_cue_count` is the suite's existing answer to exactly this
    // (see its doc, and `re_attaching_*`/`preroll_*` which already use it);
    // this test simply never adopted it.
    // `attach_then_seek_baseline_survives_a_burst_still_in_flight` reproduces
    // the split deterministically.
    let already = harness.settled_cue_count(&tap, "ATSK");
    assert_origins(
        &tapped_with_prefix(&tap, "ATSK"),
        gst::ClockTime::ZERO,
        "before the seek",
    );

    seek_to(&harness, origin);
    let after = harness.wait_for_cue(&tap, "ATSK", already, "after the seek");
    assert_origins(&after[already..], origin, "after attach-then-seek");

    harness.assert_worker_alive("after attach-then-seek");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// The attach-then-seek flake, reproduced DETERMINISTICALLY instead of by
/// machine load -- and both halves of it stated as assertions.
///
/// [`attach_then_seek_keeps_the_external_on_the_video_timeline`] asks whether
/// the cues delivered AFTER a seek carry the sought origin, which it can only
/// ask against a baseline of "everything delivered before it". The consumer
/// branch is UNSYNCED, so `attach_and_select` hands the whole external over in
/// one burst that races the test thread: idle, the burst is finished before
/// the baseline is taken and the test passes; loaded, it is not, and the
/// burst's TAIL is scored against the post-seek origin it could not possibly
/// carry. Those cues are not misaligned -- they were delivered before the seek
/// was requested, against the origin that was correct then.
///
/// Throttling the consumer turns that window from a coin flip into a
/// parameter: at 4 ms a cue a 70-cue file cannot drain inside the quiescence
/// `settled_cue_count` waits for, so the burst is STILL RUNNING when the naive
/// baseline is taken, on any machine and at any load. The test then pins both
/// directions -- the naive baseline provably would have mis-scored the tail,
/// and the settled one provably holds -- so a revert to
/// `wait_for_cue(..).len()` fails here every time rather than one run in three.
#[test]
fn attach_then_seek_baseline_survives_a_burst_still_in_flight() {
    init();
    let origin = gst::ClockTime::from_seconds(6);
    let main = main_item("extlifeburstinflightmain");
    let subs = sub_item("extlifeburstinflightsubs", "BRST");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();
    text_arm::throttle_cue_delivery(&harness.playbin, Duration::from_millis(4));

    let (_id, _sid) = harness.attach_and_select(&subs.uri());
    // The baseline the flake used, and the one that replaced it.
    let naive = harness
        .wait_for_cue(&tap, "BRST", 0, "before the seek")
        .len();
    let already = harness.settled_cue_count(&tap, "BRST");
    assert!(
        naive < already,
        "the throttle failed to keep the attach burst in flight ({naive} of \
         {already} cues delivered when the naive baseline was taken), so this \
         reproduction proves nothing about the window it exists to hold open"
    );
    assert_origins(
        &tapped_with_prefix(&tap, "BRST"),
        gst::ClockTime::ZERO,
        "before the seek",
    );

    seek_to(&harness, origin);
    let after = harness.wait_for_cue(&tap, "BRST", already, "after the seek");
    // The contract, against the settled baseline: everything the seek is
    // responsible for is on the new timeline.
    assert_origins(&after[already..], origin, "after attach-then-seek");
    // And the naive baseline would have failed here, because the burst tail
    // between the two baselines is pre-seek material at the pre-seek origin.
    // This is the assertion that makes the reproduction bite: it fails if the
    // window ever stops being reproducible, rather than letting the test
    // quietly stop testing anything.
    assert!(
        after[naive..already]
            .iter()
            .any(|(_, seen)| *seen == Some(gst::ClockTime::ZERO)),
        "no origin-0 cue sits between the naive baseline ({naive}) and the \
         settled one ({already}), so the flake's material is absent and this \
         reproduction has gone vacuous"
    );

    harness.assert_worker_alive("after a burst-still-in-flight attach-then-seek");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// Attach DURING a seek: the attach lands between the seek request and the
/// pipeline settling on the new origin, so the input joins while the timeline
/// underneath it is still moving. Every cue must still carry the video's
/// final origin.
#[test]
fn attaching_during_a_seek_still_lands_on_the_sought_timeline() {
    init();
    let origin = gst::ClockTime::from_seconds(7);
    let main = main_item("extlifeduringseekmain");
    let subs = sub_item("extlifeduringseeksubs", "DRSK");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    // A playing-state seek parks waiting for the caller's state machine to
    // re-drive it, so do what that machine does: seek from a settled PAUSED.
    harness.pause();
    harness.playbin.seek_async(Seek {
        position: Some(origin),
        rate: None,
    });
    // No wait for the seek to land. The attach is issued straight after, so
    // it races the flush and the new segment.
    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching during a seek");

    let video_pad = harness.video_pad();
    wait_video_origin(
        &harness,
        &video_pad,
        origin,
        "after attaching during a seek",
    );
    harness.play();

    let sid = harness.materialized(id);
    harness.select_and_confirm(id, &sid);
    let cues = harness.wait_for_cue(&tap, "DRSK", 0, "after attaching during a seek");
    assert_origins(&cues, origin, "attach-during-seek");

    harness.assert_worker_alive("after attaching during a seek");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// Detach DURING a seek. The seek is in flight when the input's request pads
/// are released, which is the surgery-while-flushing case: the seek must
/// still complete and the item must still play.
#[test]
fn detaching_during_a_seek_completes_both() {
    init();
    let origin = gst::ClockTime::from_seconds(8);
    let main = main_item("extlifedetachseekmain");
    let subs = sub_item("extlifedetachseeksubs", "DTSK");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();
    let (id, _sid) = harness.attach_and_select(&subs.uri());
    harness.wait_for_cue(&tap, "DTSK", 0, "before the detach-during-seek");

    harness.pause();
    harness.playbin.seek_async(Seek {
        position: Some(origin),
        rate: None,
    });
    assert_returns_within(
        &harness.playbin,
        Duration::from_secs(15),
        "detaching during a seek",
        {
            let playbin = harness.playbin.clone();
            move || playbin.detach_subtitle(id).expect("detach during a seek")
        },
    );

    let video_pad = harness.video_pad();
    wait_video_origin(
        &harness,
        &video_pad,
        origin,
        "after detaching during a seek",
    );
    harness.play();
    harness.assert_video_advances("after detaching during a seek");
    harness.assert_worker_alive("after detaching during a seek");
    harness.assert_no_error("after detaching during a seek");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// Seek from a settled PAUSED (what the receiver's state machine does) and
/// wait until the overlay's video pad reports the new origin.
fn seek_to(harness: &Harness, position: gst::ClockTime) {
    harness.pause();
    harness.playbin.seek_async(Seek {
        position: Some(position),
        rate: None,
    });
    let video_pad = harness.video_pad();
    wait_video_origin(harness, &video_pad, position, "seek_to");
    harness.play();
}

/// Every recorded cue must have rendered against `expected`.
fn assert_origins(cues: &[(String, Option<gst::ClockTime>)], expected: gst::ClockTime, what: &str) {
    assert!(!cues.is_empty(), "no cue to check the origin of ({what})");
    let wrong: Vec<_> = cues
        .iter()
        .filter(|(_, origin)| *origin != Some(expected))
        .cloned()
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} cues rendered against a different origin than the video \
         (expected {expected}) in {what}: {:?}",
        wrong.len(),
        cues.len(),
        wrong
    );
}
