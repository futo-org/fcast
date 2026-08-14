//! The subtitle-delivery reconcile pass. Converged is a fixpoint, and the
//! overlay seat is read from the graph rather than remembered.
//!
//! The deferred-replay and deferred-verification lists were replaced by
//! `Inner::reconcile_subtitle_delivery`, which re-derives both from the
//! graph at every settled PLAYING. Deleting remembered work is only safe if
//! the replacement cannot itself become a source of work:
//!
//! * [`a_converged_pipeline_is_a_reconcile_fixpoint`] runs the pass far more
//!   often than production ever will against an aligned, delivering external
//!   and demands zero emissions. Without the desired-equals-observed and
//!   nothing-in-flight guards, an unconditional periodic trigger would be a
//!   replay generator.
//! * [`the_seat_occupant_is_observed_not_remembered`]:
//!   `observed_seat_occupant()` must agree with what is actually wired to the
//!   consumer tail and go to `None` the moment the branch leaves, whereas the
//!   `last_applied_subtitle` mirror may keep claiming whatever it last wrote.
//!
//! The complementary property, that the trigger stops firing when nothing is
//! left to remember, lives in `tests/tick_idle.rs` in its own binary.
//!
//! # Verification
//!
//! * Green: no env vars.
//! * Effectively serial by construction. Every test reads process-global
//!   counters, so `init()` hands each one the suite's single lock. Any
//!   `--test-threads` value is safe.
//! * `FCAST_NO_TEXT_RECONCILE=1`: the pass is off and the v1 slots are back.
//!   The fixpoint test then proves only that the v1 drains emit nothing.
//! * `FCAST_NO_TICK_RECONCILE_POKE=1`: removes the periodic trigger. Both tests
//!   still pass because they poll explicitly.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Far more passes than the 1 Hz tick would run in any realistic settle
/// window. If a converged graph emitted even occasionally, this many rounds
/// would find it.
const FIXPOINT_ROUNDS: usize = 50;

/// The suite asserts on process-global counters, so overlapping tests
/// inflate each other's numbers. `init()` hands every test the one lock, so
/// a parallel invocation is merely serial, not red. parking_lot, so a
/// panicking test cannot poison the rest.
fn init() -> parking_lot::MutexGuard<'static, ()> {
    static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
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
    SERIAL.lock()
}

fn cues(count: u32, step: gst::ClockTime, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{tag}{index:02}"))
        })
        .collect()
}

struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    /// Cue payloads reaching the renderer, from [`Harness::tap_text`].
    text: std::cell::RefCell<Option<text_arm::CueTap>>,
    /// Latched event facts.
    ///
    /// `pump()` drains the channel, so a test that also drains it in a
    /// `wait_for` predicate races its own pump for the one event it cares
    /// about. Latching in the one place that reads the channel removes the
    /// race.
    loaded: std::cell::Cell<bool>,
    activated: std::cell::Cell<bool>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        // The cue feed, before anything can flow. An unsynced external hands
        // its whole file over the moment its branch links.
        text_arm::arm(&playbin);
        Self {
            playbin: Arc::new(playbin),
            events,
            text: std::cell::RefCell::new(None),
            loaded: std::cell::Cell::new(false),
            activated: std::cell::Cell::new(false),
        }
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
        self.drain_events();
    }

    /// The only reader of the event channel. Latches what tests wait on.
    fn drain_events(&self) {
        while let Ok(event) = self.events.try_recv() {
            match &event {
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                PlaybinEvent::Loaded { .. } => self.loaded.set(true),
                PlaybinEvent::PreparedActivated => self.activated.set(true),
                _ => {}
            }
        }
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; seat observed {:?} mirror {:?}, \
                 reconcile emits {}",
                self.playbin.observed_seat_occupant(),
                self.playbin.mirrored_seat_occupant(),
                FcastPlaybin::reconcile_emits(),
            );
            self.pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Start reading cue payloads. Installed once the branch is linked.
    /// Nothing is lost by the wait because the tap backfills out of the feed
    /// armed at construction.
    fn tap_text(&self) {
        *self.text.borrow_mut() = Some(text_arm::tap_cue_payloads(&self.playbin));
    }

    fn cues_seen(&self, tag: &str) -> usize {
        self.text
            .borrow()
            .as_ref()
            .expect("the cue tap is installed")
            .lock()
            .expect("text tap")
            .iter()
            .filter(|(payload, _)| payload.trim_start().starts_with(tag))
            .count()
    }

    /// Drain events without poking the text policy. The caller cadence the
    /// reconcile pass is supposed to be independent of.
    fn pump_events_only(&self) {
        self.drain_events();
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// A replay seek held inside its send, on the thread that sent it.
///
/// The replay is a flushing seek pushed to the external input's own source
/// pads, so an upstream-event probe there sits on the exact thread the lane
/// uses, inside the exact call. Blocking in the callback is what holds it,
/// and the wait is timed so a test mistake ends the run instead of wedging
/// the binary.
struct ReplayPark {
    release: Option<mpsc::Sender<()>>,
    parked: Arc<Mutex<usize>>,
}

impl ReplayPark {
    /// How many replay seeks have entered the park.
    fn parked(&self) -> usize {
        *self.parked.lock().expect("parked")
    }

    /// Let every parked sender go.
    fn release(mut self) {
        drop(self.release.take());
    }
}

fn park_replay_seeks(harness: &Harness, uri: &str) -> ReplayPark {
    let external = harness
        .playbin
        .pipeline()
        .iterate_elements()
        .into_iter()
        .flatten()
        .find(|element| {
            // `find_property` first. Asking an element without a `uri`
            // property for one is a panic, not a `None`.
            element.find_property("uri").is_some()
                && element
                    .property::<Option<String>>("uri")
                    .is_some_and(|found| found == uri)
        })
        .expect("the external urisourcebin is in the pipeline");
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let parked = Arc::new(Mutex::new(0usize));
    for pad in external.src_pads() {
        let release_rx = release_rx.clone();
        let parked = parked.clone();
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && event.type_() == gst::EventType::Seek
            {
                *parked.lock().expect("parked") += 1;
                let _ = release_rx
                    .lock()
                    .expect("release")
                    .recv_timeout(EVENT_TIMEOUT);
            }
            gst::PadProbeReturn::Ok
        })
        .expect("installing the replay park probe");
    }
    ReplayPark {
        release: Some(release_tx),
        parked,
    }
}

/// A playing item with an external subtitle attached and materialized, but
/// not selected.
///
/// The split matters for parking the join-time replay. It fires on
/// selection, so the probe has to be in place before that and after the
/// input exists.
fn attached(
    tag: &str,
) -> (
    Harness,
    fcasttest::scenario::ScenarioHandle,
    fcasttest::scenario::ScenarioHandle,
    fcastplaybin::ExternalSubId,
) {
    let media = ScenarioBuilder::new(&format!("{tag}main"))
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new(&format!("{tag}subs"))
        .text("text_0", cues(600, gst::ClockTime::from_mseconds(100), "R"))
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_for("the load to report Loaded", || harness.loaded.get());
    harness.playbin.play().expect("play");
    harness.wait_for("the pipeline to settle at PLAYING", || {
        harness.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
            && !harness.playbin.has_async_transition()
    });
    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching the external subtitle input");
    harness.wait_for("the external subtitle stream to materialize", || {
        !harness.playbin.subtitle_stream_ids(id).is_empty()
    });
    (harness, media, subs, id)
}

/// A playing item with one external subtitle selected and delivering cues.
fn converged(
    tag: &str,
) -> (
    Harness,
    fcasttest::scenario::ScenarioHandle,
    fcasttest::scenario::ScenarioHandle,
    fcastplaybin::ExternalSubId,
    String,
) {
    converged_for(tag, gst::ClockTime::from_seconds(120))
}

/// [`converged`] with the main item's duration chosen by the caller. A test
/// that needs the item to actually end asks for a short one. A gapless
/// boundary is only triggered by the outgoing item's EOS.
fn converged_for(
    tag: &str,
    duration: gst::ClockTime,
) -> (
    Harness,
    fcasttest::scenario::ScenarioHandle,
    fcasttest::scenario::ScenarioHandle,
    fcastplaybin::ExternalSubId,
    String,
) {
    let media = ScenarioBuilder::new(&format!("{tag}main"))
        .video("video_0")
        .audio("audio_0")
        .duration(duration)
        .pacing(Pacing::Realtime)
        .register();
    // The external's duration must track the main item's. The gapless swap
    // holds until the outgoing item's streams drain, so a longer external
    // would keep the item alive no matter how short the video is.
    let cue_step = gst::ClockTime::from_mseconds(100);
    let cue_count = (duration.nseconds() / cue_step.nseconds()) as u32;
    let subs = ScenarioBuilder::new(&format!("{tag}subs"))
        .text("text_0", cues(cue_count, cue_step, "R"))
        .duration(duration)
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_for("the load to report Loaded", || harness.loaded.get());
    harness.playbin.play().expect("play");
    harness.wait_for("the pipeline to settle at PLAYING", || {
        harness.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
            && !harness.playbin.has_async_transition()
    });

    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching the external subtitle input");
    harness.wait_for("the external subtitle stream to materialize", || {
        !harness.playbin.subtitle_stream_ids(id).is_empty()
    });
    let sid = harness
        .playbin
        .subtitle_stream_ids(id)
        .first()
        .cloned()
        .expect("the external advertised a stream");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.pump();
    harness.wait_for("the text branch to join the renderer", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    harness.tap_text();
    harness.wait_for("the first cue to reach the renderer", || {
        harness.cues_seen("R") > 0
    });
    (harness, media, subs, id, sid)
}

/// A converged graph emits nothing, however hard it is asked.
///
/// This is what makes the tick's unconditional trigger safe. The reconcile
/// pass runs whether or not anything is pending, so it must not create work.
/// Guard 1 is `aligned` (desired == observed) and guard 2 is
/// `replay_inflight` / `replay_checks_armed`.
#[test]
fn a_converged_pipeline_is_a_reconcile_fixpoint() {
    let _serial = init();
    let (harness, media, subs, id, sid) = converged("reconcilefix");

    // Let whatever the attach legitimately owed settle first. A join-time
    // replay is real work and not what this test is about.
    harness.wait_for("the attach's own replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });
    let seen = harness.cues_seen("R");
    harness.wait_for("the branch to keep delivering", || {
        harness.cues_seen("R") > seen
    });

    let emits_before = FcastPlaybin::reconcile_emits();
    for _ in 0..FIXPOINT_ROUNDS {
        // Every one of these queues a coalesced drain, whose tail IS the pass.
        harness.playbin.poll_text_policy();
        harness.pump();
        thread::sleep(Duration::from_millis(10));
    }
    // Give the queued passes room to have run before reading the counter.
    // There is no condition to wait on by construction. The subject is a
    // counter that must not move, so nothing's arrival could end the wait.
    // The pumping here is generous relative to what a coalesced job needs.
    for _ in 0..20 {
        harness.pump();
        thread::sleep(Duration::from_millis(10));
    }

    let emits_after = FcastPlaybin::reconcile_emits();
    assert_eq!(
        emits_after,
        emits_before,
        "{FIXPOINT_ROUNDS} reconcile passes over an aligned, delivering external emitted \
         {} replay(s). A converged graph must be a fixpoint, or the tick's 1 Hz trigger is \
         a replay generator",
        emits_after - emits_before
    );
    // And convergence is not "nothing happens". The branch is still live.
    // A consumer branch is unsynced and delivers its whole file within
    // milliseconds of linking, so "one more cue" could only time out. The
    // live branch is the one still wired, with the graph still naming it as
    // the renderer's occupant. The passes disposed of nothing, unlinked
    // nothing and misaligned nothing.
    assert!(
        text_arm::text_branch_linked(&harness.playbin),
        "the text branch left the renderer while {FIXPOINT_ROUNDS} reconcile passes ran \
         over a converged graph, having delivered {} cue(s)",
        harness.cues_seen("R")
    );
    assert_eq!(
        harness.playbin.observed_seat_occupant().as_deref(),
        Some(sid.as_str()),
        "the graph stopped naming {sid} as the renderer's occupant across the passes"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// The liveness the deleted lists used to provide: delivery is restored with
/// no caller polling at all.
///
/// With the deferred lists gone the only thing that re-asks is the tick's
/// unconditional poke, so this test stops polling entirely after the seek
/// and lets the tick do all of it. A pass that only ran on caller cadence
/// would leave the branch misaligned for as long as the caller stayed quiet.
///
/// The seek creates the divergence. It moves the video's running-time
/// origin, and a text branch still carrying the old one renders shifted,
/// which is `aligned == false`.
#[test]
fn a_misaligned_external_converges_without_any_caller_poll() {
    let _serial = init();
    let (harness, media, subs, id, _sid) = converged("reconcilelive");
    harness.wait_for("the attach's own replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });

    // Move the video timeline out from under the text branch.
    harness
        .playbin
        .seek(gst::ClockTime::from_seconds(30))
        .expect("seek");
    let seen = harness.cues_seen("R");

    // From here on, no `poll_text_policy` and no `pump_selection`. Only the
    // tick can drive the pass.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while harness.cues_seen("R") <= seen {
        assert!(
            Instant::now() < deadline,
            "the external never delivered again after the seek, with no caller poll to \
             carry it: seat observed {:?}, reconcile emits {}, replay in flight {}",
            harness.playbin.observed_seat_occupant(),
            FcastPlaybin::reconcile_emits(),
            harness.playbin.replay_inflight(id, 0),
        );
        harness.pump_events_only();
        thread::sleep(Duration::from_millis(20));
    }

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// Exactly one replay per divergence, proved by parking the lane mid-send.
///
/// The in-flight guard's job is the window between "a replay was handed to
/// the lane" and "its outcome came back". Inside that window the graph still
/// reads unaligned, so a pass that did not know a replay was outstanding
/// would emit rival replays, each fighting the last over the same input's
/// segment.
///
/// The window is manufactured. A probe on the external input's source pads
/// parks the sending thread inside the seek push and holds it there while
/// the test polls the pass repeatedly.
///
/// Reverting the insert at the top of `replay_subtitle` makes the assertions
/// below fail. Verified, not assumed.
#[test]
fn a_parked_replay_suppresses_every_further_emit_for_that_input() {
    let _serial = init();
    let (harness, media, subs, id) = attached("reconcileone");

    // The park, installed before the selection so the join-time replay walks
    // straight into it.
    let park = park_replay_seeks(&harness, &subs.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the join-time replay to reach the park", || {
        park.parked() > 0
    });

    let emits_before = FcastPlaybin::reconcile_emits();
    let seeks_before = FcastPlaybin::replay_seeks_sent();
    assert!(
        harness.playbin.replay_inflight(id, 0),
        "a replay is parked mid-send but the in-flight bit is clear; every reconcile pass \
         in the window below is free to emit a rival replay against the same input"
    );

    // Many passes against a graph that reads unaligned, with the replay that
    // would fix it stuck in the probe.
    for _ in 0..FIXPOINT_ROUNDS {
        harness.playbin.poll_text_policy();
        harness.pump();
        thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..20 {
        harness.pump();
        thread::sleep(Duration::from_millis(10));
    }

    let emits_after = FcastPlaybin::reconcile_emits();
    let seeks_after = FcastPlaybin::replay_seeks_sent();
    assert!(
        harness.playbin.replay_inflight(id, 0),
        "the in-flight bit cleared while the replay was still parked in the send; nothing \
         has reported an outcome yet"
    );
    assert_eq!(
        emits_after,
        emits_before,
        "{FIXPOINT_ROUNDS} passes emitted {} replay(s) while one was already parked \
         mid-send against the same (id, epoch)",
        emits_after - emits_before
    );
    assert!(
        seeks_after - seeks_before <= 1,
        "{} replay seeks reached the graph while one was parked mid-send; the in-flight \
         guard is not covering this emitter",
        seeks_after - seeks_before
    );

    // Release and let it converge, so the test also proves the park was the
    // only thing holding it.
    park.release();
    harness.wait_for("the parked replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// Two triggers, one seek: the second `ReplaySub` job is a logged no-op.
///
/// The in-flight bit cannot collapse these two. Every emitter sets
/// `replay_inflight` before it queues its job, so by the time
/// `replay_subtitle` runs the bit is always already set and says nothing
/// about whether somebody else's seek is out. Two near-simultaneous triggers
/// can both pass every guard and both be performed, giving two flushing
/// seeks on the same input and two whole-file redeliveries.
///
/// The park makes this deterministic. With the first replay's seek stuck in
/// the probe no outcome can arrive, so the second job runs in exactly the
/// window the guard has to cover.
///
/// Removing the `replay_seek_outstanding` check in `replay_subtitle` makes
/// `replay_seeks_sent` advance by 2 here. Verified, not assumed.
#[test]
fn a_second_replay_trigger_while_one_is_outstanding_sends_no_second_seek() {
    let _serial = init();
    let (harness, media, subs, id) = attached("replaytwice");

    // The first replay, parked mid-send and therefore unable to settle.
    let park = park_replay_seeks(&harness, &subs.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the join-time replay to reach the park", || {
        park.parked() > 0
    });
    assert!(
        harness.playbin.replay_inflight(id, 0),
        "the parked replay should hold the in-flight bit"
    );

    let seeks_before = FcastPlaybin::replay_seeks_sent();
    // The rival, queued exactly as an emitter queues it (bit first, then the
    // job), so it passes every guard before the choke point.
    assert!(
        harness.playbin.queue_replay_sub(id, 0),
        "queueing the rival replay job"
    );
    for _ in 0..30 {
        harness.pump();
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        FcastPlaybin::replay_seeks_sent(),
        seeks_before,
        "a second replay seek was handed off for an input whose first seek is still \
         travelling; the field saw both of them land and the file delivered twice"
    );

    // The park was the only thing holding it. Released, the input converges.
    park.release();
    harness.wait_for("the parked replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// The enqueue half of "one replay per input", which the choke point inside
/// `replay_subtitle` cannot cover.
///
/// If a second emitter queues a rival job and the first seek's outcome lands
/// before the rival runs, both the bit and `replay_seek_outstanding` are
/// clear by then, the choke point has nothing to collapse, and the branch is
/// flushed twice.
///
/// That ordering is a millisecond race no test can schedule, so the property
/// is asserted where it is decidable: with a replay demonstrably
/// outstanding, a join must queue no further `ReplaySub` job.
/// `replay_seeks_sent` cannot see this, because the parked seek keeps
/// `replay_seek_outstanding` set and the second job would be choked anyway.
/// That is why `replay_jobs_queued` exists.
///
/// Dropping the `replay_inflight` check from the join-time emitter makes
/// `replay_jobs_queued` advance by one here. Verified, not assumed.
#[test]
fn a_join_queues_no_second_replay_while_one_is_outstanding() {
    let _serial = init();
    let (harness, media, subs, id) = attached("replayenqueue");

    // The park has to exist before anything can send, and it is what keeps
    // the in-flight bit set for as long as the test needs it.
    let park = park_replay_seeks(&harness, &subs.uri());
    assert!(
        harness.playbin.queue_replay_sub(id, 0),
        "queueing the first replay the way an emitter does"
    );
    harness.wait_for("the first replay seek to reach the park", || {
        park.parked() > 0
    });
    assert!(
        harness.playbin.replay_inflight(id, 0),
        "the parked replay should hold the in-flight bit"
    );

    let queued_before = FcastPlaybin::replay_jobs_queued();
    // The rival: a real selection, whose join reaches the emitter at the
    // tail of `poll_text_policy`.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    for _ in 0..30 {
        harness.pump();
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        FcastPlaybin::replay_jobs_queued(),
        queued_before,
        "a second replay job was queued for an input whose replay is still outstanding; \
         in the field the first outcome landed before that job ran and the branch was \
         flushed twice"
    );

    park.release();
    harness.wait_for("the parked replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// A replay sent while the pipeline is PAUSED must not move the pipeline's
/// timeline.
///
/// A flushing seek on the external's source is answered with a resetting
/// FLUSH_STOP, whose RESET_TIME the pipeline turns into a start-time reset
/// to zero. While PAUSED the start time holds the running time playback
/// stopped at, so the next resume recomputes the base time from zero and
/// running time restarts, freezing video while the clock catches up to
/// frames the sink already has. PAUSED is not incidental. The start time is
/// NONE for the whole of PLAYING, so the reset writes nothing there.
///
/// `FCAST_NO_START_TIME_GUARD=1` fails both assertions. The position comes
/// back at about 0 and the restore counter never moves.
#[test]
fn a_replay_while_paused_does_not_restart_the_pipeline_timeline() {
    let _serial = init();
    let (harness, media, subs, id) = attached("replaypausedclock");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    // Far enough in that a restart to zero cannot be confused with sampling
    // noise. The item is realtime, so this is real elapsed playback.
    harness.wait_for("the position to pass 700 ms", || {
        harness
            .playbin
            .position()
            .is_some_and(|position| position >= gst::ClockTime::from_mseconds(700))
    });

    harness.playbin.pause().expect("pause");
    harness.wait_for("the pipeline to settle at PAUSED", || {
        harness.playbin.state_summary() == (gst::State::Paused, gst::State::VoidPending)
            && !harness.playbin.has_async_transition()
    });
    let before = harness
        .playbin
        .position()
        .expect("a paused pipeline answers a position");

    let restores_before = FcastPlaybin::start_time_restores();
    assert!(
        harness.playbin.queue_replay_sub(id, 0),
        "queueing the re-enable's replay the way an emitter does"
    );
    harness.wait_for("the replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });

    harness.playbin.play().expect("play");
    harness.wait_for("the pipeline to settle at PLAYING again", || {
        harness.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
            && !harness.playbin.has_async_transition()
    });
    let after = harness
        .playbin
        .position()
        .expect("a playing pipeline answers a position");

    // The claim first, so a broken guard fails on the symptom rather than on
    // the instrument that measures it.
    assert!(
        after >= before,
        "the pipeline's timeline restarted across a paused subtitle replay: position was \
         {before} before the replay and {after} after it. The field saw this as a video \
         frozen for the whole of the old position while the reported time climbed from 0."
    );
    // Then the instrument. With a working guard and no reset to repair, the
    // assertion above would hold vacuously. A run where the flush never
    // reached a sink proves nothing.
    assert!(
        FcastPlaybin::start_time_restores() > restores_before,
        "the replay's FLUSH_STOP never reset the pipeline's start time, so this test \
         did not exercise the defect it exists for (position {before} -> {after})"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// A forwarded seek an external refuses must not be recorded as if it
/// landed.
///
/// Recording `last_origin` before sending is worse than losing the seek. The
/// selection-time replay trigger compares the video's origin against it, so
/// a recorded-but-not-performed realignment makes the input look aligned and
/// silences the one trigger that would have re-sent it. The user-visible
/// result is cues that never realign after a seek for the rest of the item.
///
/// A refusal now leaves `last_origin` alone, is counted and logged at ERROR,
/// and hands the input to the replay machinery, the only path that realigns
/// an external.
///
/// The lever is the test's only way in. A real refusal comes from a source's
/// own seek handling underneath a parser chain and cannot be arranged from
/// outside the pipeline.
///
/// With the recovery removed, `replay_jobs_queued` does not move here.
#[test]
fn a_refused_forwarded_seek_is_counted_and_replayed() {
    let _serial = init();
    let (harness, media, subs, id) = attached("forwardseekrefused");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    // Let the join-time replay settle so what the seek below queues is the
    // only thing in flight for this input.
    harness.wait_for("the join-time replay to settle", || {
        !harness.playbin.replay_inflight(id, 0)
    });

    let refusals_before = FcastPlaybin::forward_seek_refusals();
    let queued_before = FcastPlaybin::replay_jobs_queued();
    // The forward, run directly. A transport seek would park behind this
    // harness's `seekable: false` gate and never reach `Job::Seek`.
    //
    // The lever is set around this call only and removed immediately. The
    // binary shares one process, and a lever left set would make every later
    // forwarded seek read as refused.
    // SAFETY: this binary is serial, so no other thread reads the
    // environment across these two statements.
    unsafe { std::env::set_var("FCAST_FORCE_FORWARD_SEEK_REFUSAL", "1") };
    harness
        .playbin
        .forward_seek_to_externals(1.0, gst::ClockTime::from_seconds(3));
    unsafe { std::env::remove_var("FCAST_FORCE_FORWARD_SEEK_REFUSAL") };
    harness.wait_for("the refused forward to be counted", || {
        FcastPlaybin::forward_seek_refusals() > refusals_before
    });
    harness.wait_for("the refusal to hand the input to a replay", || {
        FcastPlaybin::replay_jobs_queued() > queued_before
    });

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// A refused slot-seeding GAP must not be remembered as a seeding.
///
/// A held external stream is advertised but unselectable until it gets a
/// queue slot, and for a pre-parsed input the only thing that creates one is
/// the GAP `Inner::seed_slot_for_held_pad` pushes from the hold's block
/// probe. That push can be refused, because the realigning replay's flushing
/// seek travels the same pad and a push in flight when FLUSH_START lands
/// returns false.
///
/// If the refusal is remembered as a success, the stream stays slotless for
/// the life of the item. Later buffers return not-linked, the output pad
/// never carries a sticky CAPS, and the caps gate refuses the join forever
/// while the selection reads as confirmed. The seeding is therefore latched
/// only on success, and a FLUSH_STOP through the probe re-arms it.
///
/// # What this test does and does not pin
///
/// It pins that a refusal is counted and not fatal: once the lever is lifted
/// the input still selects, joins and renders. It deliberately does not
/// assert the retry. Under the lever the first buffer is still parked in the
/// hold probe, so no flush ever crosses the pad and no second push can be
/// observed from here.
#[test]
fn a_refused_slot_seeding_gap_is_counted_and_not_fatal() {
    let _serial = init();
    let refusals_before = FcastPlaybin::slot_seed_refusals();
    // The lever is the test's only way in. A real refusal needs FLUSH_START
    // to land while the GAP push is in flight, which no test can arrange
    // from outside the pipeline.
    // SAFETY: this binary is serial, so no other thread reads the
    // environment here.
    unsafe { std::env::set_var("FCAST_FORCE_SLOT_SEED_REFUSAL", "1") };
    let (harness, media, subs, id) = attached("slotseedrefused");
    harness.wait_for("the refused seeding to be counted", || {
        FcastPlaybin::slot_seed_refusals() > refusals_before
    });
    // Lifted the moment the refusal is on the board. The point is that the
    // input survives one, and a lever left set would refuse every retry too.
    unsafe { std::env::remove_var("FCAST_FORCE_SLOT_SEED_REFUSAL") };
    // Not fatal: the stream must still render after a refusal.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// The caps-gate escalation must not fire on a healthy item.
///
/// `Inner::capsless_text_since` turns the link loop's silent caps refusal
/// into one loud warn once the absence outlasts `CAPSLESS_TEXT_GRACE`. The
/// value of that line is that it is believed when it fires, so the thing
/// worth pinning is the negative: an ordinary attach-select-render, held
/// well past the grace period, must not produce one. A grace period too
/// short, or a watch keyed so a stream that later joins is still counted,
/// shows up here.
#[test]
fn the_capsless_text_escalation_stays_quiet_on_a_healthy_item() {
    let _serial = init();
    let stalls_before = FcastPlaybin::capsless_text_stalls();
    let (harness, media, subs, id) = attached("capslessquiet");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    // Past the grace period, with the poll running throughout.
    let until = Instant::now() + Duration::from_secs(7);
    while Instant::now() < until {
        harness.playbin.poll_text_policy();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        FcastPlaybin::capsless_text_stalls(),
        stalls_before,
        "the caps-gate escalation fired on an item whose text branch joined \
         and rendered"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// The text-seat stalemate break must not fire on an item that never
/// stalemates.
///
/// The break clears `superseded` and `evicted_dead`, the two latches that
/// stop same-sid entries trading the one consumer branch back and forth once
/// per poll. Clearing them is safe only when no entry holds the seat. A
/// break that fired while a branch was seated would hand back exactly the
/// thrash those latches stop.
///
/// So the guard is the negative: a plain attach, select and render, polled
/// hard throughout, must never need the break. The positive is pinned by
/// `external_subtitle_lifecycle`.
#[test]
fn the_seat_stalemate_break_stays_quiet_on_an_item_that_never_stalemates() {
    init();
    let breaks_before = FcastPlaybin::text_seat_stalemates();
    let (harness, media, subs, id) = attached("seatstalematequiet");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        harness.playbin.poll_text_policy();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        FcastPlaybin::text_seat_stalemates(),
        breaks_before,
        "the seat stalemate break cleared the contention latches on an item \
         whose text branch was seated and rendering"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// a text stream whose sticky CAPS the queue destroyed in flight must
/// still render.
///
/// # The defect
///
/// The seeding gap carries the pad's stickies into the fresh queue slot,
/// where they are queued. If the realigning replay's flushing seek lands
/// while the slot's loop thread is between popping and pushing the CAPS, the
/// popped event is unreffed outside the flush's sticky rescue and nothing
/// re-sends it, because sticky forwarding skips events already marked
/// received on the feeding pad. The slot's src pad then never carries a
/// CAPS, and the caps gate refuses to build a branch for the life of the
/// item: selected, confirmed, parked, silent.
///
/// # Why this is staged rather than raced
///
/// The trigger is a race between the input's streaming thread and the replay
/// lane inside GStreamer, which no test can win on demand. So
/// `stage_text_caps_loss` reproduces the state exactly (caps taken off the
/// ghost and the slot's src pad, left on the slot's sink pad) and what is
/// measured is the recovery, the part that ships.
///
/// Red with `FCAST_NO_TEXT_CAPS_RESCUE=1`: the caps never come back and the
/// wait for the branch to join times out.
#[test]
fn a_text_stream_whose_caps_the_multiqueue_lost_still_joins_and_renders() {
    let _serial = init();
    let rescues_before = FcastPlaybin::text_caps_rescues();
    let (harness, media, subs, id) = attached("capsrescue");
    // Armed before the selection, where the window is: the stream is routed
    // and parked with its caps, and the very next poll would have joined it.
    harness.playbin.stage_text_caps_loss();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    // The contract first, so this is what goes red. The restored caps have
    // to reach the ghost, open the gate and carry cues, not merely sit in
    // the slot's event store. Under the lever this wait times out with the
    // defect's own silhouette, seat observed `None` on a confirmed sid.
    harness.wait_for("the text branch to join its consumer tail", || {
        harness.playbin.poll_text_policy();
        text_arm::text_branch_linked(&harness.playbin)
    });
    let tap = text_arm::tap_cue_payloads(&harness.playbin);
    harness.wait_for("a cue to reach the renderer", || {
        !tap.lock().expect("cue tap").is_empty()
    });
    // And the staging really fired. The loss only lands on a parked text
    // stream that has caps, so a rescue proves the test met the defect.
    // Without this, a staging that quietly missed its window would read as a
    // green rendering test.
    assert!(
        FcastPlaybin::text_caps_rescues() > rescues_before,
        "no caps rescue ran, so the staged loss never landed and this run says \
         nothing about the caps loss"
    );
    // The repair either gets the caps back onto the slot or the track is dead,
    // so the failure counter is the invariant rather than a diagnostic.
    assert_eq!(
        FcastPlaybin::text_caps_rescue_failures(),
        0,
        "a caps rescue ran and the slot still had no caps afterwards"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// The caps rescue must not fire on a healthy item.
///
/// The repair writes a sticky event onto a queue slot's src pad behind the
/// decoder's back, defensible only when the slot's sink pad has a caps its
/// src pad does not, i.e. one really was destroyed. A rescue firing on the
/// caps gate's ordinary transient would be surgery on a stream about to join
/// by itself and would print upstream's misordering warning on every healthy
/// item. So the guard is the negative: an ordinary attach, select and
/// render, polled hard throughout, must never need it.
#[test]
fn the_caps_rescue_stays_quiet_on_a_healthy_item() {
    let _serial = init();
    let rescues_before = FcastPlaybin::text_caps_rescues();
    let (harness, media, subs, id) = attached("capsrescuequiet");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the text branch to join its consumer tail", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        harness.playbin.poll_text_policy();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        FcastPlaybin::text_caps_rescues(),
        rescues_before,
        "the caps rescue rewrote a multiqueue slot's sticky events on an item \
         whose text branch joined and rendered on its own"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// The leak invariant for the per-resource in-flight bit: no entry may
/// outlive the input it names.
///
/// `replay_inflight` suppresses the reconcile pass for an `(id, epoch)`
/// while a replay is outstanding. An entry never discharged does not fail
/// loudly. It silently switches the reconciler off for that resource forever
/// and grows the set without bound. Every path that ends a replay without an
/// outcome has to clear it. This asserts the consequence rather than the
/// paths: after each way an epoch can die, nothing in the set names an input
/// that is not attached.
///
/// The case that bit: a slotless hand-off to `RetrySub` returns without a
/// seek, and the retry's leave-it-be arm returns without bumping the epoch,
/// so a bit left behind there is permanent, on a live external.
#[test]
fn no_in_flight_replay_bit_outlives_its_input() {
    let _serial = init();
    // A detach while a replay is in flight. Detaching an idle external
    // proves nothing since its bit is already clear, so the join-time replay
    // is parked mid-send and the input pulled out from under it. No outcome
    // will ever be reported for an input that no longer exists, so
    // `remove_input` has to be the discharge.
    let (harness, media, subs, id) = attached("reconcileorphan");
    let park = park_replay_seeks(&harness, &subs.uri());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the join-time replay to reach the park", || {
        park.parked() > 0
    });
    assert!(
        harness.playbin.replay_inflight(id, 0),
        "the staging did not hold: no replay is in flight to be orphaned"
    );

    harness
        .playbin
        .detach_subtitle(id)
        .expect("detaching the external");
    harness.wait_for("the external to leave routing", || {
        !harness.playbin.has_external_subtitles()
    });
    assert_eq!(
        harness.playbin.replay_inflight_orphans(),
        0,
        "a detached external left its in-flight replay bit behind; the reconcile pass is \
         now permanently suppressed for that (id, epoch)"
    );
    park.release();

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

// The gapless-reap case is deliberately not staged here. It is covered by
// construction: the clear lives in `Inner::remove_input`, the single
// function every removal path funnels through, and the cases above exercise
// that exact line. Staging the reap needs a gapless boundary with an
// external attached, which no suite in this crate reliably produces. A test
// whose failure mode is "the staging did not happen" is worse than an honest
// note. `replay_inflight_orphans()` exists for whoever stages it.

/// The seat's occupant is read, not remembered.
///
/// `last_applied_subtitle` is a record of what this crate last decided.
/// `observed_seat_occupant` walks the graph from the renderer's peer back to
/// the routed entry and reads its StreamStart sticky. The difference is
/// visible the moment the branch leaves: the probe says `None` immediately,
/// and the mirror is under no obligation to.
#[test]
fn the_seat_occupant_is_observed_not_remembered() {
    let _serial = init();
    let (harness, media, subs, id, sid) = converged("reconcileseat");

    // Occupied: the probe names the stream that is actually linked.
    let observed = harness.playbin.observed_seat_occupant();
    assert_eq!(
        observed.as_deref(),
        Some(sid.as_str()),
        "the seat probe named {observed:?} while {sid} is the branch linked into \
         the renderer (mirror says {:?})",
        harness.playbin.mirrored_seat_occupant()
    );

    // Vacated: subtitles off unlinks the branch. The probe must follow the
    // graph immediately. That is the whole property.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.pump();
    harness.wait_for("the text branch to leave the renderer", || {
        !text_arm::text_branch_linked(&harness.playbin)
    });
    assert_eq!(
        harness.playbin.observed_seat_occupant(),
        None,
        "the seat probe still names an occupant although the branch's tail has no peer; it \
         is reading something remembered instead of the graph (mirror says {:?})",
        harness.playbin.mirrored_seat_occupant()
    );

    // Re-occupied: it comes back naming the same stream, without anyone
    // having written a mirror for it.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.wait_for("the branch to retake the seat", || {
        harness.playbin.observed_seat_occupant().is_some()
    });
    assert_eq!(
        harness.playbin.observed_seat_occupant().as_deref(),
        Some(sid.as_str()),
        "the re-linked branch is not the one the probe names"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}
