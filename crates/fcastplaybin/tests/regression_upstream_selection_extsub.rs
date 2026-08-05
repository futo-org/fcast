//! RED regression for the field report `subtitle-regressions.txt`: an external
//! subtitle attached to an ADAPTIVE (DASH) main input.
//!
//! When any input answers the SELECTABLE query, decodebin3 defers ALL stream
//! selection upstream (`gstdecodebin3.c` "We force decodebin3 to upstream
//! selection mode if *any* of the inputs is. This means things might break if
//! there's a mix" — an external subtitle IS the mix). The demuxer rejects any
//! SELECT_STREAMS naming the external's foreign sid (`gstadaptivedemux.c:2494`
//! "Unrecognized stream_id", the WHOLE event refused), the external input
//! cannot handle the event either, so `send_event` returns FALSE and the crate
//! dropped the whole selection: "decodebin3 refused the SELECT_STREAMS event",
//! and the subtitle could never be turned back on. decodebin3 also never posts
//! STREAMS_SELECTED in this mode, so nothing confirms and nothing retries.
//!
//! `ftestsrc` models the demuxer via `ScenarioBuilder::upstream_selection`:
//! SELECTABLE = TRUE, adaptivedemux2's exact SELECT_STREAMS semantics, and a
//! STREAMS_SELECTED post only on an actual selection change.

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
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);
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

fn prefixed_cues(prefix: &str, count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{prefix}{index:02}"))
        })
        .collect()
}

/// Every text payload crossing the overlay's subtitle input.
type TextTap = Arc<Mutex<Vec<String>>>;

fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> Vec<String> {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|payload| payload.starts_with(prefix))
        .cloned()
        .collect()
}

/// Harness modelled on `tests/external_subtitle_lifecycle.rs`, reduced to what
/// this file needs (see there for the transport re-drive rationale).
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    wants_playing: Cell<bool>,
    parked_seek: Cell<Option<Seek>>,
    video: Recording,
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
                audio_slot.lock().expect("audio slot").push(sink.recording());
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
            wants_playing: Cell::new(false),
            parked_seek: Cell::new(None),
            video,
            _audio: audio,
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
                } else if self.wants_playing.get() {
                    let _ = self.playbin.play();
                }
            }
            _ => {}
        }
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
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
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, _generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!("pipeline error while waiting for {what}: {error}");
                    }
                    self.redrive_transport(&event);
                    let hit = pred(&event);
                    self.log.borrow_mut().push(event);
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("event channel closed while waiting for {what}")
                }
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
        self.wants_playing.set(false);
        self.parked_seek.set(None);
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

    fn overlay_subtitle_pad(&self) -> gst::Pad {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(overlay) = self.playbin.pipeline().by_name("fpb-suboverlay") {
                return overlay
                    .static_pad("subtitle_sink")
                    .expect("subtitleoverlay has a subtitle_sink pad");
            }
            assert!(
                Instant::now() < deadline,
                "subtitleoverlay never joined the pipeline"
            );
            self.settle_pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn tap_overlay_text(&self) -> TextTap {
        let seen: TextTap = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        self.overlay_subtitle_pad()
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                    let payload = buffer
                        .map_readable()
                        .map(|map| String::from_utf8_lossy(map.as_slice()).into_owned())
                        .unwrap_or_default();
                    recorder.lock().expect("text tap").push(payload);
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
        self.wait_until(
            &format!("the external stream {id:?} to materialize"),
            EVENT_TIMEOUT,
            || !self.playbin.subtitle_stream_ids(id).is_empty(),
        );
        id
    }

    fn wait_for_cue(&self, tap: &TextTap, prefix: &str, already: usize, what: &str) {
        self.wait_until(
            &format!("a {prefix} cue past {already} to reach the overlay ({what})"),
            EVENT_TIMEOUT,
            || tapped_with_prefix(tap, prefix).len() > already,
        );
    }

    fn video_buffers(&self) -> usize {
        self.video.buffer_count()
    }

    fn assert_video_advances(&self, what: &str) {
        let before = self.video_buffers();
        self.wait_until(&format!("video to advance ({what})"), EVENT_TIMEOUT, || {
            self.video_buffers() > before + 2
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

/// Selecting a subtitle while PAUSED must confirm to the caller without a
/// resume.
///
/// The field choreography: an upstream-selection item sits at a settled PAUSED
/// and the user attaches an external with select. The subtitle side works (the
/// branch joins, the replay runs, the verdict is correctly held below PLAYING),
/// but NOTHING ever confirms the selection: no `StreamsSelected` reaches the
/// caller, so the receiver never sends SetTrackIds and the UI keeps showing the
/// previous track for as long as the item stays paused.
///
/// The split sends only a CHANGED upstream part, because an adaptive demuxer
/// confirms activation edges and a no-op send has none. Here the part read as
/// changed (the embedded text leaves the upstream selection when the external
/// takes over) and the confirmation still never came: at a settled PAUSED
/// nothing flows, so an upstream deactivation that needs its pad idle cannot
/// complete. `Inner::arm_upstream_confirm_fallback` bounds that: if the engine
/// still awaits the seqnum after `UPSTREAM_CONFIRM_FALLBACK`, the crate posts
/// the confirmation itself.
///
/// The resume at the end is free coverage for the held-verdict path.
///
/// # The mechanism, and why it is engine-side
///
/// Measured with `FCASTPLAYBIN_TEST_LOG=debug`: there is no
/// `sent SELECT_STREAMS` line at all. `SelectionEngine::pump` resolves the
/// request to a target that EQUALS `applied`, because `collection_changed` had
/// already seeded the text slot with the external's stream (it is the only text
/// stream in the collection) and decodebin3's own auto-select had joined it.
/// Nothing is dispatched, so nothing was owed a confirmation, and in
/// upstream-selection mode decodebin3 never posts one either. `ConfirmApplied`
/// answers exactly this shape. Lever: `FCAST_NO_CONFIRM_APPLIED` (set = the old
/// unanswered behaviour, which makes this test red again).
///
/// The synthetic confirmation is scoped away from a gapless activation window,
/// where `applied` already names the incoming item's streams and a report naming
/// them would reach the activation trigger itself. It is NOT scoped away from
/// adaptive inputs: `dash_testbed` measures the same 1-in-8 pre-existing flake
/// (`dash_external_sub_switch_cycle`, a startup freeze) with the confirmation on
/// and off, so an earlier suspicion that it regressed adaptive playback was a
/// load artifact, not the mechanism.
#[test]
fn a_paused_subtitle_selection_confirms_without_a_resume() {
    init();
    // NO embedded text on the main input: an `ftestsrc` that answers
    // SELECTABLE *and* carries text makes decodebin3 abort on
    // `mq_slot_handle_stream_start: assertion failed: (collection)`, the shape
    // that `#[ignore]`s `embedded_then_external_switch_on_an_adaptive_main_input`
    // above. Dropping it costs nothing here: with video+audio only, the
    // dispatch's upstream part names exactly what is already active, which IS
    // the field's unconfirmable no-op send.
    let media = ScenarioBuilder::new("pausedconfirmmain")
        .video("video_0")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .pacing(Pacing::Realtime)
        .upstream_selection()
        .register();
    let subs = ScenarioBuilder::new("pausedconfirmsubs")
        .text("text_0", prefixed_cues("EXT", 300, gst::ClockTime::from_mseconds(100)))
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&media.uri());
    // Real playback first, so the pause below rests on a prerolled pipeline.
    harness.assert_video_advances("before pausing");

    // The harness re-drives PLAYING on every settled-Paused edge while this is
    // set (see `redrive_transport`), which would un-pause the test.
    harness.wants_playing.set(false);
    harness.playbin.pause().expect("pause");
    harness.wait_until("a settled PAUSED", EVENT_TIMEOUT, || {
        harness.playbin.state_summary() == (gst::State::Paused, gst::State::VoidPending)
    });
    harness.log.borrow_mut().clear();

    // The field's attach-with-select, at rest in PAUSED.
    let id = harness.attach_and_materialize(&subs.uri());
    let sid = {
        let sids = harness.playbin.subtitle_stream_ids(id);
        sids.first().expect("the external advertised a stream").clone()
    };
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.settle_pump();

    // THE ASSERTION: the caller is told, while still paused. This is the event
    // the receiver relays and turns into SetTrackIds.
    harness.wait_until(
        "the paused selection to confirm to the caller",
        Duration::from_secs(10),
        || {
            harness.drain_events();
            harness.log.borrow().iter().any(|event| {
                matches!(
                    event,
                    PlaybinEvent::StreamsSelected { subtitle: Some(got), .. } if *got == sid
                )
            })
        },
    );
    assert_eq!(
        harness.playbin.state_summary(),
        (gst::State::Paused, gst::State::VoidPending),
        "the confirmation must not have needed a resume"
    );

    // Resuming must then render the external: the held replay verdict completes.
    harness.playbin.play().expect("resume");
    harness.wants_playing.set(true);
    let tap = harness.tap_overlay_text();
    harness.wait_for_cue(&tap, "EXT", 0, "the external after the resume");

    subs.release_all();
    media.release_all();
    harness.shutdown();
    subs.unregister();
    media.unregister();
}

/// The confirmation must answer REQUESTS ONLY.
///
/// `dirty` is set by plenty of things that nobody asked for: a collection
/// change, a foreign report, engine-internal reseeding. If `ConfirmApplied`
/// fired for those, the receiver would relay unsolicited track changes and
/// SetTrackIds storms. Attaching a second external WITHOUT selecting it churns
/// the collection exactly that way, so no `StreamsSelected` may reach the caller
/// from it.
#[test]
fn collection_churn_without_a_request_confirms_nothing() {
    init();
    let media = ScenarioBuilder::new("nochurnconfirmmain")
        .video("video_0")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .pacing(Pacing::Realtime)
        .upstream_selection()
        .register();
    let first = ScenarioBuilder::new("nochurnconfirmsubs0")
        .text("text_0", prefixed_cues("SA", 300, gst::ClockTime::from_mseconds(100)))
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let second = ScenarioBuilder::new("nochurnconfirmsubs1")
        .text("text_0", prefixed_cues("SB", 300, gst::ClockTime::from_mseconds(100)))
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&media.uri());
    let tap = harness.tap_overlay_text();
    let id = harness.attach_and_materialize(&first.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.settle_pump();
    harness.wait_for_cue(&tap, "SA", 0, "the requested external");

    // From here nobody requests anything. The second attach churns the
    // collection and marks the engine dirty.
    harness.log.borrow_mut().clear();
    let _unselected = harness.attach_and_materialize(&second.uri());
    let quiet_until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < quiet_until {
        harness.settle_pump();
        harness.drain_events();
        thread::sleep(Duration::from_millis(20));
    }
    let unsolicited = harness
        .log
        .borrow()
        .iter()
        .filter(|event| matches!(event, PlaybinEvent::StreamsSelected { .. }))
        .count();
    assert_eq!(
        unsolicited, 0,
        "collection churn produced {unsolicited} unsolicited confirmation(s); log: {:#?}",
        harness.log.borrow()
    );

    first.release_all();
    second.release_all();
    media.release_all();
    harness.shutdown();
    first.unregister();
    second.unregister();
    media.unregister();
}

/// A SLOW CALLER must not cost the user a subtitle track.
///
/// The join that puts a switched-to external's branch into subtitleoverlay runs
/// from `poll_text_policy`, i.e. on the CALLER's cadence, while the replay
/// verification that decides whether the switch took runs on the worker's 400 ms
/// timer. Exhausting `REPLAY_ATTEMPTS` therefore means "the caller has not
/// polled yet" at least as often as it means "this input is bad", and the first
/// version of the give-up escalation could not tell the two apart: it ran
/// `Job::FailSub`, which DETACHES the input and emits
/// `ExternalSubtitleFailed`, so the receiver dropped a perfectly servable track
/// from the user's list and the sender's next SetTrack answered
/// ResourceNotFound. Field-confirmed on a three-external item.
///
/// The choreography reproduces exactly that: dispatch the switch with a single
/// pump, then go quiet for longer than the whole replay chain takes, and only
/// then resume polling. The switched-to external must still render, and must
/// still be attached.
#[test]
fn a_slow_caller_does_not_cost_a_switched_to_external() {
    init();
    let media = ScenarioBuilder::new("slowpumpmain")
        .video("video_0")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .pacing(Pacing::Realtime)
        .upstream_selection()
        .register();
    let subs: Vec<_> = ["SA", "SB", "SC"]
        .iter()
        .enumerate()
        .map(|(index, prefix)| {
            ScenarioBuilder::new(format!("slowpumpsubs{index}"))
                .text(
                    "text_0",
                    prefixed_cues(prefix, 300, gst::ClockTime::from_mseconds(100)),
                )
                .duration(LONG_CLIP)
                .pacing(Pacing::AsFastAsPossible)
                .register()
        })
        .collect();

    let harness = Harness::new();
    harness.load_and_play(&media.uri());
    let tap = harness.tap_overlay_text();
    let ids: Vec<ExternalSubId> = subs
        .iter()
        .map(|scenario| harness.attach_and_materialize(&scenario.uri()))
        .collect();

    // The first external renders normally, so the switch below is a
    // text-to-text replace with a live outgoing branch.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ids[0]));
    harness.settle_pump();
    harness.wait_for_cue(&tap, "SA", 0, "the first external");

    // THE SWITCH, dispatched and then left alone. One pump sends it; after that
    // the caller goes quiet for longer than REPLAY_ATTEMPTS * 400 ms, so the
    // worker's verification chain runs out while no poll can perform the join.
    // Events are still drained, so a detach would be recorded.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ids[2]));
    harness.settle_pump();
    let quiet_until = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < quiet_until {
        harness.drain_events();
        thread::sleep(Duration::from_millis(20));
    }

    // The input must have survived the quiet window.
    assert!(
        !harness
            .log
            .borrow()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::ExternalSubtitleFailed { .. })),
        "a servable external was failed while the caller was not polling; log: {:#?}",
        harness.log.borrow()
    );
    assert!(
        !harness.playbin.subtitle_stream_ids(ids[2]).is_empty(),
        "the switched-to external was detached while the caller was not polling"
    );

    // And the caller polling again must still get the track it asked for.
    harness.wait_for_cue(&tap, "SC", 0, "the switched-to external after the quiet window");
    harness.assert_video_advances("after the slow-caller switch");

    for scenario in &subs {
        scenario.release_all();
    }
    media.release_all();
    harness.shutdown();
    for scenario in &subs {
        scenario.unregister();
    }
    media.unregister();
}

/// A text-to-text SWITCH on an adaptive main input (field report
/// `ext-subtitle-regression-2.txt`): the dispatch's eager work runs only a
/// Flush for a replace and relies on decodebin3's pad swap to free the
/// overlay seat, but no SELECT_STREAMS ever reaches decodebin3 in
/// upstream-selection mode, so the outgoing branch held `subtitle_sink`
/// forever ("subtitle_sink already linked; skipping extra text stream") and
/// the replay verifier looped, re-pushing the new input into a parked branch.
#[test]
fn external_subtitle_switch_on_an_adaptive_main_input() {
    init();
    let main = ScenarioBuilder::new("upsel2main")
        .video("video_0")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .upstream_selection()
        .register();
    let first = ScenarioBuilder::new("upsel2a")
        .text(
            "text_0",
            prefixed_cues("SWA", 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let second = ScenarioBuilder::new("upsel2b")
        .text(
            "text_0",
            prefixed_cues("SWB", 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let first_id = harness.attach_and_materialize(&first.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(first_id));
    harness.wait_for_cue(&tap, "SWA", 0, "first external");

    // The switch: no off in between, exactly the field sequence.
    let second_id = harness.attach_and_materialize(&second.uri());
    harness.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::ExternalSubtitle(second_id),
    );
    harness.wait_for_cue(&tap, "SWB", 0, "switched-to external");

    // And BACK (field round 3): the first external's input drained (EOS into
    // decodebin3) long ago, and if its multiqueue slot was reclaimed for the
    // second stream, no output pad can carry its sid again: the branch sits
    // parked on its fakesink while subtitleoverlay drops to passthrough.
    let first_before = tapped_with_prefix(&tap, "SWA").len();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(first_id));
    harness.wait_for_cue(&tap, "SWA", first_before, "switched-back external");

    harness.assert_video_advances("after the switch");
    harness.shutdown();
    main.unregister();
    first.unregister();
    second.unregister();
}

/// An adaptive item with an EMBEDDED text track plus an external (field
/// round 3): external renders, disable, select the embedded track (a REAL
/// upstream change dashdemux2 activates and confirms), then back to the
/// external. The field log showed every bookkeeping step succeed on that
/// last switch and the external's branch still sitting on its parking
/// fakesink.
#[test]
#[ignore = "SIGABRT in decodebin3 (g_assert(collection), mq_slot_handle_stream_start): an \
            ftestsrc answering SELECTABLE makes db3 drop its own parsebins' collection \
            messages, a shape real adaptive demuxers never produce (their input arrives \
            parsed, collections as events). Needs a fake adaptive demuxer element in \
            fcasttest (typefound as adaptive media, per-track pad add/remove) before this \
            can run."]
fn embedded_then_external_switch_on_an_adaptive_main_input() {
    init();
    let main = ScenarioBuilder::new("upsel3main")
        .video("video_0")
        .audio("audio_0")
        .text(
            "text_0",
            prefixed_cues("EMB", 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .upstream_selection()
        .register();
    let subs = ScenarioBuilder::new("upsel3subs")
        .text(
            "text_0",
            prefixed_cues("EXT", 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let embedded_sid = main.stream_id("text_0");

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let id = harness.attach_and_materialize(&subs.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for_cue(&tap, "EXT", 0, "external first");

    // Disable, exactly the field's off step.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.wait_until("the text branch to park", EVENT_TIMEOUT, || {
        harness.overlay_subtitle_pad().peer().is_none()
    });

    // The embedded track: a REAL upstream change (the demuxer activates it
    // and posts the confirmation).
    harness.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some(embedded_sid.clone())),
    );
    harness.wait_for_cue(&tap, "EMB", 0, "embedded track");

    // Back to the external: the field failure.
    let ext_before = tapped_with_prefix(&tap, "EXT").len();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for_cue(&tap, "EXT", ext_before, "external again");

    harness.assert_video_advances("after the final switch");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// The full field cycle on an adaptive main input: select renders, deselect
/// stops, RE-select renders again. The re-select is what the field lost.
#[test]
fn external_subtitle_cycle_on_an_adaptive_main_input() {
    init();
    let main = ScenarioBuilder::new("upsel1main")
        .video("video_0")
        .audio("audio_0")
        .duration(LONG_CLIP)
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .upstream_selection()
        .register();
    let subs = ScenarioBuilder::new("upsel1subs")
        .text(
            "text_0",
            prefixed_cues("UPS", 70, gst::ClockTime::from_mseconds(400)),
        )
        .duration(LONG_CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = harness.tap_overlay_text();

    let id = harness.attach_and_materialize(&subs.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for_cue(&tap, "UPS", 0, "initial select");

    // Deselect. The park is crate-local, so this half worked in the field too.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.wait_until("the text branch to park", EVENT_TIMEOUT, || {
        harness.overlay_subtitle_pad().peer().is_none()
    });
    let after_off = tapped_with_prefix(&tap, "UPS").len();

    // Re-select: the field failure. Nothing may depend on decodebin3
    // confirming, because in upstream-selection mode it never does.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for_cue(&tap, "UPS", after_off, "re-select");

    harness.assert_video_advances("after the re-select");
    harness.shutdown();
    main.unregister();
    subs.unregister();
}
