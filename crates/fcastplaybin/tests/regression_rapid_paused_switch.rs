//! Regression coverage for the receiver parking in `Buffering` forever after
//! rapid subtitle track changes at a pipeline resting in PAUSED.
//!
//! Reported from the field. Roughly fifteen switches over five seconds at a
//! pipeline resting in PAUSED, each logging `postponing the eager text-branch
//! work ... work=Flush`, and then:
//!
//! ```text
//! 65.007  State changed new=Paused pending=VoidPending      <- the last one
//! 66.1..71.4  ~15 x "postponing the eager text-branch work"
//! 72.693  op=SetPlaybackState(Playing)                      <- no effect
//! 75.609  Cannot resume or pause in player current state: Buffering
//! ```
//!
//! The interlock, as reconstructed from the capture. The eager REPLACE flush
//! is postponed while the pipeline rests in PAUSED and drains at a settled
//! PLAYING. Undrained, the outgoing slots back up decodebin3's multiqueue,
//! the real source reports buffering, and the application state machine parks
//! in `Buffering`, holding the pipeline at PAUSED and refusing `ResumeOrPause`
//! with `InvalidState`. The 100 that would end the park needs the backlog
//! drained, the drain needs a settled PLAYING, and PLAYING needs the 100.
//!
//! Two tests, with different reach.
//!
//! `rapid_external_switches_while_parked_in_buffering_recover_on_completion`
//! is the field-shaped one, built on fcasttest's buffering knob. External
//! subtitle inputs as in the field, a receiver-shaped consumer driving the
//! real `StateMachine`, a genuine `Buffering` park raised by a real
//! `GST_MESSAGE_BUFFERING`, the run of switches dispatched at the parked
//! pipeline, the field's refused transport, and a deterministic release. It
//! asserts the property that matters. Dispatching a switch at the parked
//! pipeline must never block the caller, and once buffering completes the
//! machine must leave `Buffering`, the pipeline must reach PLAYING, and the
//! postponed work must drain far enough that the last selected track's text
//! actually reaches the overlay.
//!
//! IT REPRODUCES, AND A RED RUN IS A REAL BUG. On the build of 2026-08-05
//! this test fails most runs (11 of 12 measured at the settled constants,
//! 8 to 11 of 12 across nearby ones, the box drifts) with the last selected
//! track's text never reaching the overlay after the recovery, despite the
//! machine, the pipeline and video all recovering. Diagnosed from debug
//! captures, two cooperating defects in the external replay machinery.
//!
//! 1. `verify_replay` (lib.rs, "Worker side of the replay verification") reads
//!    the branch's sticky STREAM_START and sticky segment to decide "aligned
//!    delivery, no replay needed". While the pipeline is held below PLAYING
//!    nothing flows, so those stickies are leftovers of the input's PREVIOUS
//!    tenure, and a spent input passes the check. Captured as the final arming
//!    firing its check about 400 ms later, still parked, and returning silently
//!    through the aligned path. Checks that fire mid alternation instead log
//!    "selection moved on; not replaying" and also conclude nothing for the
//!    final selection.
//! 2. Nothing re-verifies after the resume. The arming is edge-triggered by the
//!    STREAMS_SELECTED confirmation, and that edge is either consumed while
//!    parked (feeding defect 1) or never posted at all when the backlog stalls
//!    decodebin3, in which case no arming ever exists for the final selection.
//!    The deferred-work drain re-attempts flushes, disposals, owed replays and
//!    input removals but never re-checks whether the settled selection is
//!    actually delivering. Both captured shapes end identically.
//!
//! So the run that parks, switches and recovers ends with the overlay
//! linked to a spent input that will never push another buffer, until the
//! user switches tracks yet again. The field's fifteen paused switches
//! between external subtitle files walk straight into this.
//!
//! Measured on one binary, interleaved, six rounds each.
//! `FCAST_INLINE_TEXT_FLUSH` off failed 4 of 6, on failed 3 of 6. The
//! defect is in the replay verification, which the dispatch rework did not
//! touch, so the lever does not gate it. Note the lever also cannot restore
//! the field's DRAIN behavior (the drain rearchitecture has no lever), so a
//! full old-drain A/B is not expressible on this binary.
//!
//! HONEST LIMIT. The knob's buffering percent is scripted. The one edge of
//! the field interlock no scripted source can model is the 100 DEPENDING on
//! the backed-up queues draining, which is the edge that closed the field
//! cycle. A percent derived from actual downstream consumption (a knob mode
//! where ftestsrc tracks delivery against consumption, or a real
//! queue2-fronted source) is what that last edge still needs. Every other
//! edge is exercised here, and the recovery property this asserts is the
//! one the field sequence violated.
//!
//! `rapid_subtitle_switching_while_paused_still_resumes` is the original
//! embedded-track test. It is ACTION COVERAGE ONLY. It exercises the user
//! action over embedded tracks on one input and passes against the build
//! that hung in the field, so a green run of it guarantees nothing about
//! this bug. It is kept for the paths the reproducer does not cross, the
//! REPLACE postponement and resume over embedded tracks of a single input.

use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, BufferingStateResult, FcastPlaybin, MediaInput, PlaybinEvent, RunningState,
    SelectionGate, Sinks, StartPoint, StateChangeResult, StateMachine, TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::{FTestSink, Recording},
    spec::{BufferingDip, BufferingRecovery, BufferingSpec, CueSpec, Pacing},
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Matches the field report. About fifteen changes over a few seconds.
const SWITCHES: usize = 15;

/// The field capture spaced them 200 to 400 ms apart. Within that range the
/// spacing also sets how often decodebin3 confirms a switch while still
/// parked, which is what arms the mis-verification a red run diagnoses.
/// Measured failure rates over 10 to 12 runs per configuration were 8/10 at
/// 250 ms against 5/10 at 120 ms and 4/10 at 350 ms.
const BETWEEN_SWITCHES: Duration = Duration::from_millis(250);

/// Resuming is a state change, not a rebuild. If it has not happened in this
/// long the pipeline is not coming back.
const RESUME_BOUND: Duration = Duration::from_secs(20);

/// The percent every dip's low post carries.
const LOW_PERCENT: i32 = 10;

/// The dip's anchor. At 25 fps realtime this is eight seconds in, which
/// leaves the load, the external attaches and the first track's text flow
/// comfortably behind before the low percent can arrive.
const DIP_AT_VIDEO_BUFFER: u64 = 200;

/// How long one switch dispatch at the parked pipeline may take. A dispatch
/// there only records the flush intent and pokes the worker, so a healthy
/// build returns in microseconds. The previous inline dispatch could block
/// the caller behind the text branch, and with the buffering hold keeping
/// the pipeline below PLAYING that block would last until the release this
/// test deliberately withholds.
const PUMP_BOUND: Duration = Duration::from_secs(10);

/// How long the last selected track's text may take to reach the overlay
/// after the recovery. This spans the deferred flush drain, the replay of a
/// spent external input (a restarted urisourcebin) and decodebin3 completing
/// the coalesced REPLACE, so it is deliberately generous.
const FINAL_TEXT_BOUND: Duration = Duration::from_secs(25);

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

fn cues(count: u32, step: gst::ClockTime, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{tag}{index:02}"))
        })
        .collect()
}

fn gate(paused: bool) -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused,
        seekable: false,
    }
}

/// The receiver's view of the player, derived exactly like receiver-core's
/// `player_state()`. `Buffering` is the state `Operation::ResumeOrPause`
/// refuses with `InvalidState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverState {
    Stopped,
    Paused,
    Playing,
    Buffering,
}

/// The receiver-shaped consumer, the same machinery receiver-core's player
/// runs. Every event feeds the real `StateMachine` and every state the
/// machine dispatches is applied to the pipeline, so the `Buffering` this
/// test parks in is the very state application.rs refuses transport in.
struct Receiver {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    sm: StateMachine,
    /// Every buffering percent the consumer saw, in order.
    saw_buffering: Vec<i32>,
}

impl Receiver {
    fn build() -> (Self, Recording) {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(video_sink.upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        // Before any cue can be delivered (see `support/text_arm.rs`).
        text_arm::arm(&playbin);
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        (
            Self {
                playbin,
                events,
                sm: StateMachine::new(),
                saw_buffering: Vec::new(),
            },
            video,
        )
    }

    fn state(&self) -> ReceiverState {
        if self.sm.is_stopped() {
            return ReceiverState::Stopped;
        }
        match self.sm.running() {
            Some(RunningState::Paused) => ReceiverState::Paused,
            Some(RunningState::Playing) => ReceiverState::Playing,
            None => ReceiverState::Buffering,
        }
    }

    fn load(&mut self, uri: String) {
        self.sm.begin_load();
        self.playbin.load_async(
            MediaInput::Uri(uri),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
    }

    fn apply(&self, state: gst::State) {
        match state {
            gst::State::Playing => self.playbin.play().expect("play"),
            gst::State::Paused => self.playbin.pause().expect("pause"),
            other => panic!("the machine never dispatches {other:?} in this test"),
        }
    }

    fn handle(&mut self, event: PlaybinEvent) {
        match event {
            PlaybinEvent::Loaded { .. } => {
                // uri_loaded's transport commit, with Playing as the desired
                // transport. The fallback drive mirrors the receiver's too.
                if let Some(state) = self.sm.set_playback_state(RunningState::Playing) {
                    self.apply(state);
                } else if self.sm.running() != Some(RunningState::Playing) {
                    self.playbin.play().expect("play");
                }
            }
            PlaybinEvent::Buffering(percent) => {
                self.saw_buffering.push(percent);
                match self.sm.buffering(percent) {
                    BufferingStateResult::Started(state) => self.apply(state),
                    BufferingStateResult::Finished(Some(state)) => self.apply(state),
                    BufferingStateResult::Finished(None)
                    | BufferingStateResult::Buffering
                    | BufferingStateResult::FinishedButWaitingSeek => {}
                    BufferingStateResult::FinishedWithSeek(seek) => {
                        panic!("no seek was ever issued, the machine invented {seek:?}")
                    }
                }
            }
            PlaybinEvent::StateChanged {
                old,
                current,
                pending,
            } => match self.sm.state_changed(old, current, pending) {
                StateChangeResult::ChangeState(state) => self.apply(state),
                StateChangeResult::NewPlaybackState(_) | StateChangeResult::Waiting => {}
                StateChangeResult::Seek(seek) => {
                    panic!("no seek was ever parked, the machine dispatched {seek:?}")
                }
            },
            PlaybinEvent::ExternalSubtitleFailed { id, .. } => {
                panic!("external subtitle input {id:?} failed for good")
            }
            PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
            _ => {}
        }
    }

    fn pump(&mut self, paused: bool) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(gate(paused));
        while let Ok(event) = self.events.try_recv() {
            self.handle(event);
        }
    }

    fn wait_until_for(
        &mut self,
        what: &str,
        bound: Duration,
        paused: bool,
        mut done: impl FnMut(&Self) -> bool,
    ) {
        let deadline = Instant::now() + bound;
        loop {
            self.pump(paused);
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} (receiver state {:?}, buffering posts {:?})",
                self.state(),
                self.saw_buffering
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until(&mut self, what: &str, paused: bool, done: impl FnMut(&Self) -> bool) {
        self.wait_until_for(what, EVENT_TIMEOUT, paused, done);
    }

    fn shutdown(self) {
        let (done_tx, done_rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = done_tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match done_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.playbin.poll_text_policy();
                    self.playbin.pump_selection(gate(false));
                    while self.events.try_recv().is_ok() {}
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// The field-shaped reproduction. External subtitle inputs, a real Buffering
/// park, the run of switches dispatched at the parked pipeline, the refused
/// transport, and the recovery once buffering completes.
///
/// The field arrived at this resting state through its own backlog (switches
/// first, buffering second). Here buffering is raised first and held, then
/// the switches are dispatched. The crate cannot tell the two orders apart.
/// It sees the same settled-PAUSED pipeline, the same gate and the same run
/// of REPLACE dispatches with the flush intent pending, and the application
/// machine sits in the same `Buffering` phase with the same refused
/// transport. The scripted order is deterministic where the field's is a
/// wall-clock race.
#[test]
fn rapid_external_switches_while_parked_in_buffering_recover_on_completion() {
    init();
    let media = ScenarioBuilder::new("rapidbufmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::Realtime)
        .buffering(BufferingSpec::new(LOW_PERCENT).with_dip(BufferingDip {
            stream: "video_0".to_owned(),
            buffer_index: DIP_AT_VIDEO_BUFFER,
            recovery: BufferingRecovery::OnSyncPoint("refill".to_owned()),
        }))
        .register();
    // The subtitle sources run AS FAST AS POSSIBLE against the realtime main
    // item, as regression_paused_switch does. The text branches run far
    // ahead of the video clock, so the outgoing branch of every REPLACE
    // carries a real backlog, which is what the postponed flush exists to
    // clear. It also spends the inputs quickly, so re-selecting one takes
    // the replay path, exactly like re-selecting a finished subtitle file in
    // the field.
    let subs_a = ScenarioBuilder::new("rapidbufsubsa")
        .text("text_0", cues(550, gst::ClockTime::from_mseconds(100), "A"))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let subs_b = ScenarioBuilder::new("rapidbufsubsb")
        .text("text_0", cues(550, gst::ClockTime::from_mseconds(100), "B"))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let (mut receiver, video) = Receiver::build();
    receiver.load(media.uri());
    receiver.wait_until("the receiver to settle Playing", false, |receiver| {
        receiver.state() == ReceiverState::Playing
    });

    let id_a = receiver
        .playbin
        .attach_subtitle(&subs_a.uri())
        .expect("attach A");
    let id_b = receiver
        .playbin
        .attach_subtitle(&subs_b.uri())
        .expect("attach B");
    receiver.wait_until(
        "both external subtitle streams to materialize",
        false,
        |receiver| {
            !receiver.playbin.subtitle_stream_ids(id_a).is_empty()
                && !receiver.playbin.subtitle_stream_ids(id_b).is_empty()
        },
    );

    // Text reaching the renderer, observed with its payload so the recovery
    // can be pinned to the LAST selected track rather than to any stale
    // backlog. The video chain is what carries the item's frames, so its
    // appearance is waited out rather than assumed.
    receiver.wait_until("the video chain to join the pipeline", false, |r| {
        text_arm::video_tap_pad(&r.playbin).is_some()
    });
    // Cue payloads as the renderer receives them.
    let texts = text_arm::tap_cue_payloads(&receiver.playbin);

    receiver
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_a));
    {
        let texts = texts.clone();
        receiver.wait_until("the first external's text to flow", false, move |_| {
            texts.lock().expect("texts").len() >= 2
        });
    }
    assert!(
        receiver.saw_buffering.is_empty(),
        "the dip fired during setup (posts {:?}), so the parked phase below would not start \
         from a flowing pipeline. Raise DIP_AT_VIDEO_BUFFER",
        receiver.saw_buffering
    );

    // The dip. The machine dispatches the buffering PAUSE, the pipeline
    // comes to rest, and the receiver-visible state is the field's parked
    // `Buffering`.
    receiver.wait_until(
        "the receiver to park in Buffering at a resting pipeline",
        true,
        |receiver| {
            let (_, current, pending) = receiver.playbin.pipeline().state(gst::ClockTime::ZERO);
            receiver.state() == ReceiverState::Buffering
                && current == gst::State::Paused
                && pending == gst::State::VoidPending
        },
    );
    assert_eq!(receiver.saw_buffering, vec![LOW_PERCENT]);

    // The run of switches, dispatched AT the parked pipeline. The dispatch
    // loop runs on its own thread purely so a wedged dispatch FAILS the test
    // instead of hanging the binary. The thread still dispatches one switch
    // at a time with the receiver's own cadence, exactly like the field's
    // single-threaded receiver loop.
    let (beat_tx, beat_rx) = mpsc::channel();
    let (back_tx, back_rx) = mpsc::channel();
    let switcher = thread::Builder::new()
        .name("parked-switches".into())
        .spawn(move || {
            for i in 0..SWITCHES {
                let target = if i % 2 == 0 { id_b } else { id_a };
                receiver
                    .playbin
                    .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(target));
                receiver.pump(true);
                let _ = beat_tx.send(i);
                thread::sleep(BETWEEN_SWITCHES);
            }
            let _ = back_tx.send(receiver);
        })
        .expect("spawning the switch thread");
    let mut last_beat = Instant::now();
    let mut receiver = loop {
        match back_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(receiver) => break receiver,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                while beat_rx.try_recv().is_ok() {
                    last_beat = Instant::now();
                }
                assert!(
                    last_beat.elapsed() < PUMP_BOUND,
                    "a subtitle switch dispatched at the pipeline parked in Buffering did not \
                     return within {PUMP_BOUND:?}. The dispatch is blocking the caller behind \
                     the text branch, and with the buffering hold keeping the pipeline below \
                     PLAYING that block never releases, which is the receiver freezing in the \
                     field"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => match switcher.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => panic!("the switch thread died without returning the receiver"),
            },
        }
    };
    switcher.join().expect("joining the switch thread");

    // The field symptom made measurable. Still parked, no 100 has arrived,
    // and a transport request into the machine is a bare retarget that
    // dispatches nothing, which application.rs surfaces as the InvalidState
    // refusal in the field log.
    assert_eq!(receiver.state(), ReceiverState::Buffering);
    assert_eq!(
        receiver.saw_buffering,
        vec![LOW_PERCENT],
        "something other than the held dip posted buffering"
    );
    assert_eq!(
        receiver.sm.set_playback_state(RunningState::Playing),
        None,
        "a transport request while parked in Buffering must be a bare retarget"
    );
    assert_eq!(receiver.state(), ReceiverState::Buffering);

    // The hold. The machine must stay parked for exactly as long as the
    // buffering gate is held, switches or no switches. The hold is long
    // enough for decodebin3's confirmation of the LAST switch to land and be
    // consumed while still parked, the way the field's fifteen switches all
    // completed their bookkeeping at the resting pipeline. Measured over 12
    // runs per configuration, 11/12 failed at 3 s against 5/10 at 2 s. A run
    // where the confirmation stalls past the resume takes the healthy
    // post-resume arming path instead and passes, which is the residual
    // nondeterminism.
    let parked_at = Instant::now();
    while parked_at.elapsed() < Duration::from_millis(3000) {
        receiver.pump(true);
        assert_eq!(
            receiver.state(),
            ReceiverState::Buffering,
            "left Buffering without any 100 having been posted"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // The release, and the property that matters. Buffering completion must
    // lead back out. The machine redispatches the remembered PLAYING, the
    // pipeline must actually get there, and the work postponed during the
    // parked switches must drain far enough that the LAST selected track's
    // text reaches the overlay. On the field build the postponed flushes
    // never drained and nothing past this point happened.
    let frames_before_release = video.buffer_count();
    let texts_before_release = texts.lock().expect("texts").len();
    media.release("refill");
    receiver.wait_until_for(
        "the receiver to leave Buffering after the recovery",
        RESUME_BOUND,
        false,
        |receiver| receiver.state() == ReceiverState::Playing,
    );
    assert_eq!(receiver.saw_buffering, vec![LOW_PERCENT, 100]);
    receiver.wait_until_for(
        "the pipeline to settle at PLAYING after the recovery",
        RESUME_BOUND,
        false,
        |receiver| {
            let (_, current, pending) = receiver.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Playing && pending == gst::State::VoidPending
        },
    );
    receiver.wait_until("video to flow again after the recovery", false, |_| {
        video.buffer_count() > frames_before_release + 3
    });
    {
        let deadline = Instant::now() + FINAL_TEXT_BOUND;
        loop {
            receiver.pump(false);
            {
                let texts = texts.lock().expect("texts");
                if texts[texts_before_release..]
                    .iter()
                    .any(|(text, _)| text.starts_with('B'))
                {
                    break;
                }
                if Instant::now() >= deadline {
                    let after: Vec<&String> = texts[texts_before_release..]
                        .iter()
                        .map(|(t, _)| t)
                        .collect();
                    let tail: Vec<&&String> = after.iter().rev().take(8).collect();
                    panic!(
                        "the last selected track's text never reached the overlay within \
                         {FINAL_TEXT_BOUND:?} of the recovery (receiver {:?}, overlay \
                         branch linked={}, post-release cues={} tail={tail:?}). \
                         This is the diagnosed replay-verification bug, see the file header: \
                         verify_replay concluded 'aligned delivery' off stickies left over \
                         from the input's previous tenure while the pipeline was parked, so \
                         the spent input was never replayed and nothing after the resume \
                         re-checks the settled selection",
                        receiver.state(),
                        text_arm::text_branch_linked(&receiver.playbin),
                        after.len(),
                    );
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    receiver.shutdown();
    media.unregister();
    subs_a.unregister();
    subs_b.unregister();
}

/// ACTION COVERAGE ONLY, see the file header. Embedded tracks on one input,
/// a plain user pause, the run of switches and the resume. This passes
/// against the build that hung in the field, so it guards the embedded
/// REPLACE postponement and the resume path, nothing more.
#[test]
fn rapid_subtitle_switching_while_paused_still_resumes() {
    init();
    // Four text tracks, so the switches are genuine REPLACEs rather than a
    // re-assertion of the same stream.
    let media = ScenarioBuilder::new("rapidpausedmain")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(100), "A"))
        .text("text_1", cues(200, gst::ClockTime::from_mseconds(100), "B"))
        .text("text_2", cues(200, gst::ClockTime::from_mseconds(100), "C"))
        .text("text_3", cues(200, gst::ClockTime::from_mseconds(100), "D"))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::Realtime)
        .register();

    let playbin = Arc::new(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    );
    let (tx, events) = mpsc::channel();
    let collection = Arc::new(std::sync::Mutex::new(None::<gst::StreamCollection>));
    let sink = collection.clone();
    playbin.set_event_handler(None, move |event, _generation| {
        if let PlaybinEvent::StreamCollection(c) = &event {
            *sink.lock().expect("collection") = Some(c.clone());
        }
        let _ = tx.send(event);
    });

    let drain = || {
        playbin.poll_text_policy();
        playbin.pump_selection(gate(false));
        while events.try_recv().is_ok() {}
    };
    let wait_for = |what: &str, mut done: Box<dyn FnMut() -> bool>| {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            drain();
            thread::sleep(Duration::from_millis(10));
        }
    };

    playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    {
        // Tracked separately. The Loaded event is consumed once, so folding
        // the collection into the same flag would clear a latch that can
        // never be set again.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut got_loaded = false;
        loop {
            // decodebin3 GROWS its merged collection as each input reports,
            // so the first one to arrive can be video and audio only. Wait
            // for the one that carries all four text tracks.
            let got_collection = collection
                .lock()
                .expect("collection")
                .as_ref()
                .is_some_and(|c| {
                    c.iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::TEXT))
                        .count()
                        == 4
                });
            if got_loaded && got_collection {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the load never finished (loaded={got_loaded} collection={got_collection})"
            );
            playbin.poll_text_policy();
            playbin.pump_selection(gate(false));
            while let Ok(event) = events.try_recv() {
                got_loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    playbin.play().expect("play");

    let text_sids: Vec<String> = collection
        .lock()
        .expect("collection")
        .clone()
        .expect("a collection arrived")
        .iter()
        .filter(|s| s.stream_type().contains(gst::StreamType::TEXT))
        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
        .collect();
    assert_eq!(
        text_sids.len(),
        4,
        "expected four text tracks: {text_sids:?}"
    );

    // One track live in the overlay, so every switch below is a REPLACE.
    playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some(text_sids[0].clone())),
    );
    {
        let probe = playbin.clone();
        wait_for(
            "the first subtitle track to reach its renderer",
            Box::new(move || text_arm::text_branch_linked(&probe)),
        );
    }

    playbin.pause().expect("pause");
    {
        let probe = playbin.clone();
        wait_for(
            "the pipeline to settle at PAUSED",
            Box::new(move || {
                let (_, current, pending) = probe.pipeline().state(gst::ClockTime::ZERO);
                current == gst::State::Paused && pending == gst::State::VoidPending
            }),
        );
    }

    // The run of switches, at rest in PAUSED, exactly as reported.
    for i in 0..SWITCHES {
        let sid = &text_sids[(i + 1) % text_sids.len()];
        playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid.clone())));
        playbin.poll_text_policy();
        playbin.pump_selection(gate(true));
        while events.try_recv().is_ok() {}
        thread::sleep(BETWEEN_SWITCHES);
    }

    // The pipeline must still come back.
    playbin.play().expect("resume");
    let started = Instant::now();
    let deadline = started + RESUME_BOUND;
    loop {
        let (_, current, pending) = playbin.pipeline().state(gst::ClockTime::ZERO);
        if current == gst::State::Playing && pending == gst::State::VoidPending {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pipeline never reached PLAYING again after {SWITCHES} paused subtitle switches \
             (stuck at current={current:?} pending={pending:?} after {:?}), which is the field \
             report: every switch postponed its text-branch work, the slots never drained, and \
             the pipeline parked in buffering with its own drain condition out of reach",
            started.elapsed()
        );
        drain();
        thread::sleep(Duration::from_millis(20));
    }

    // And it must actually be running, not merely claiming PLAYING.
    let video = text_arm::video_tap_pad(&playbin).expect("the video sink's sink pad");
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = seen.clone();
    video
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting video after the resume");
    {
        let seen = seen.clone();
        wait_for(
            "video to flow again after the resume",
            Box::new(move || seen.load(std::sync::atomic::Ordering::SeqCst) >= 2),
        );
    }

    let (done_tx, done_rx) = mpsc::channel();
    playbin.shutdown_async(Box::new(move || {
        let _ = done_tx.send(());
    }));
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match done_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the shutdown never finished");
                playbin.pump_selection(gate(false));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
        }
    }
    media.unregister();
}
