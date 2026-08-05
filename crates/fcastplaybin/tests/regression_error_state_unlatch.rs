//! RED regression for the state-change FAILURE a consumed error latches onto
//! the pipeline, reduced from `fuzz_buffering` seed 400009 (i4 a14). Lever:
//! `FCAST_NO_ERROR_STATE_UNLATCH=1`.
//!
//! `gstbin.c` `gst_bin_handle_message_func` sets `GST_STATE_RETURN = FAILURE`
//! for ANY child error, and `bin_handle_async_done` then refuses every commit
//! until a fresh `set_state` clears it. The crate consumes some errors
//! deliberately (`decisions::external_error_action` calls an external's
//! transport race `Recover`), but the latch stays, so a flushing seek's lost
//! state (`bin_handle_async_start` writes `STATE = NEXT = PENDING = PAUSED`) is
//! never committed: sinks preroll, ASYNC_DONE arrives, the commit is refused,
//! nothing is posted, and the caller's `StateMachine` sits on
//! `SeekSlot::InFlight`. That is the fuzz `(Paused, Paused), unsettled []`
//! class. Determinism comes from releasing the error INSIDE the lost-state
//! window, held open by parking the main video on a sync point the seek's
//! restart runs into.

use std::{
    cell::{Cell, RefCell},
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, BufferingStateResult, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent,
    RunningState, Seek, SelectionGate, Sinks, StartPoint, StateChangeResult, StateMachine,
    TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{CueSpec, Fault, FlowStopReason, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

/// Bound for anything the pipeline has to reach on the healthy path.
const BOUND: Duration = Duration::from_secs(20);
/// Generous: the healthy path settles well under a second, the wedge never.
const SETTLE_BOUND: Duration = Duration::from_secs(15);
/// Longest the lost-state window is held open waiting for the consumed error.
const ERROR_BOUND: Duration = Duration::from_secs(3);
/// Gate the main video's post-seek restart parks on, which holds the
/// lost-state window open.
const MAIN_HOLD: &str = "erruncommitted";
/// Gate the external's first cue parks on, released to fire its transport
/// error at a moment of the test's choosing.
const EXT_GATE: &str = "erruntransport";
/// Far enough ahead that the first pass cannot reach the parked buffer before
/// the seek does (see `main_item`).
const SEEK_TARGET: gst::ClockTime = gst::ClockTime::from_seconds(20);
/// Video buffer index [`SEEK_TARGET`] restarts the schedule at (20 s at 25
/// fps): `start_index` takes the first item ending past the offset, and with a
/// keyframe interval of 1 it does not walk back.
const SEEK_TARGET_FRAME: u64 = 500;

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

/// The item the test plays. Two load-bearing properties:
///
/// * `Pacing::Realtime`, so the first pass cannot race ahead to
///   [`SEEK_TARGET_FRAME`]. Unpaced, ftestsrc fills decodebin3's multiqueue at
///   memory speed and the stall wedges the INITIAL preroll instead.
/// * the video stall at exactly the frame the post-seek restart begins at.
///   That park holds the lost-state window open (the sink cannot re-preroll,
///   no ASYNC_DONE, no commit attempt). Without it the window is one re-preroll
///   long (single-digit ms), the error lands after the commit, nothing is
///   stranded, and the test passes in BOTH arms.
fn main_item(key: &str) -> ScenarioHandle {
    let video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(25, 1),
            // Every frame a keyframe, so `start_index` lands on
            // SEEK_TARGET_FRAME itself and not on a frame the stall is not on.
            keyframe_interval: 1,
        },
    )
    .with_pacing(Pacing::Realtime)
    .with_fault(Fault::StallAt {
        buffer_index: SEEK_TARGET_FRAME,
        sync_point: MAIN_HOLD.to_owned(),
    });
    ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0").with_pacing(Pacing::Realtime))
        .duration(gst::ClockTime::from_seconds(60))
        .bytes_per_buffer(64)
        .register()
}

/// The external subtitle input whose death the crate consumes. Its first cue
/// parks on the gate, then dies as a transport race: [`Fault::FlowStoppedAt`]
/// posts `GST_ELEMENT_FLOW_ERROR`'s exact text, "streaming stopped, reason
/// not-linked (-1)". `decisions::external_error_action` classifies by that
/// text, and only "not-linked" and "flushing" are `Recover`.
///
/// Do NOT use `Fault::ErrorAt`: it classifies as `Fail`, which is SELF-HEALING
/// here (the detach's own `set_state` clears the latch and
/// `gst_bin_remove_func` resolves the async on removal), so the test would pass
/// in both arms and prove nothing.
fn sub_item(key: &str, gate: &str) -> ScenarioHandle {
    let cues: Vec<CueSpec> = (0..40u64)
        .map(|index| {
            let start = gst::ClockTime::from_mseconds(400) * (index + 1);
            CueSpec::new(
                start,
                start + gst::ClockTime::from_mseconds(200),
                format!("ERR{index:02}"),
            )
        })
        .collect();
    let text = StreamSpec::text("text_0", cues)
        .with_pacing(Pacing::AsFastAsPossible)
        // Parking on the FIRST cue keeps the death under the test's control,
        // and it survives the seek: an UNSELECTED external is never seeked
        // (`forward_seek_to_live_externals` skips it, and decodebin3 forwards
        // upstream seeks up the main input only).
        .with_fault(Fault::StallAt {
            buffer_index: 0,
            sync_point: gate.to_owned(),
        })
        .with_fault(Fault::FlowStoppedAt {
            buffer_index: 0,
            reason: FlowStopReason::NotLinked,
        });
    ScenarioBuilder::new(key)
        .stream(text)
        .duration(gst::ClockTime::from_seconds(30))
        .register()
}

/// The receiver's machinery, driven the way `receiver-core/src/player.rs`
/// drives it. A hand-rolled waiter would not do: the fix re-commits the state
/// the bin already decided on (PAUSED) and reports the edge that re-commit
/// suppresses, and getting back to PLAYING from there is the state machine's
/// job. Only a real [`StateMachine`] can tell "the settle was announced" from
/// "the pipeline happens to be at PAUSED".
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    sm: RefCell<StateMachine>,
    desired: Cell<RunningState>,
    video_sink: gst::Element,
    loaded: Cell<bool>,
    /// Last CONFIRMED subtitle selection, so the test can insist the external
    /// is off rather than assume it.
    selected_subtitle: RefCell<Option<Option<String>>>,
    problem: RefCell<Option<String>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.clone().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        Self {
            playbin,
            events,
            sm: RefCell::new(StateMachine::new()),
            desired: Cell::new(RunningState::Playing),
            video_sink: video_sink.upcast(),
            loaded: Cell::new(false),
            selected_subtitle: RefCell::new(None),
            problem: RefCell::new(None),
        }
    }

    fn note(&self, message: String) {
        let mut slot = self.problem.borrow_mut();
        if slot.is_none() {
            *slot = Some(message);
        }
    }

    /// `player.rs` `pump_selection`, gate derived the same way.
    fn pump_selection(&self) {
        let async_busy = self.playbin.has_async_transition();
        let (running, paused) = match self.sm.borrow().running() {
            Some(state) => (true, state == RunningState::Paused),
            None => (false, false),
        };
        self.playbin.pump_selection(SelectionGate {
            quiet: running && !async_busy,
            paused,
            seekable: true,
        });
    }

    fn apply(&self, state: gst::State) {
        self.playbin.set_state_async(state);
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.pump_selection();
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlaybinEvent::Loaded { .. } => {
                    self.loaded.set(true);
                    let desired = self.desired.get();
                    let dispatched = self.sm.borrow_mut().set_playback_state(desired);
                    if let Some(state) = dispatched {
                        self.apply(state);
                    } else if self.sm.borrow().running() != Some(desired) {
                        self.apply(desired.into());
                    }
                }
                PlaybinEvent::StateChanged {
                    old,
                    current,
                    pending,
                } => {
                    self.playbin.poll_text_policy();
                    let result = self.sm.borrow_mut().state_changed(old, current, pending);
                    match result {
                        StateChangeResult::NewPlaybackState(_) | StateChangeResult::Waiting => {}
                        StateChangeResult::Seek(seek) => self.playbin.seek_async(seek),
                        StateChangeResult::ChangeState(state) => self.apply(state),
                    }
                    self.pump_selection();
                }
                PlaybinEvent::QueueSeek(seek) => self.sm.borrow_mut().queue_seek(seek),
                PlaybinEvent::SeekFailed => {
                    // Not expected (seekable media, seek from a settled
                    // PAUSED). Recorded so the failure names itself.
                    self.note("the pipeline reported SeekFailed".to_owned());
                    let dispatched = self.sm.borrow_mut().seek_failed();
                    if let Some(state) = dispatched {
                        self.apply(state);
                    }
                }
                // No BufferingSpec on either scenario, so this is the real
                // elements' own flow control. Fed in anyway, as `player.rs`
                // does.
                PlaybinEvent::Buffering(percent) => {
                    let result = self.sm.borrow_mut().buffering(percent);
                    match result {
                        BufferingStateResult::Started(state) => self.apply(state),
                        BufferingStateResult::FinishedWithSeek(seek) => {
                            self.playbin.seek_async(seek)
                        }
                        BufferingStateResult::Finished(Some(state)) => self.apply(state),
                        BufferingStateResult::Buffering
                        | BufferingStateResult::FinishedButWaitingSeek
                        | BufferingStateResult::Finished(None) => {}
                    }
                    self.pump_selection();
                }
                PlaybinEvent::AsyncDone => {
                    self.playbin.poll_text_policy();
                    self.pump_selection();
                }
                PlaybinEvent::RequestState(state) => self.apply(state),
                PlaybinEvent::StreamsSelected { subtitle, .. } => {
                    *self.selected_subtitle.borrow_mut() = Some(subtitle)
                }
                PlaybinEvent::Error { error, origin, .. } => {
                    self.note(format!("the pipeline posted an error: {error} ({origin:?})"))
                }
                // The premise: the external's death must be CONSUMED. If it is
                // reported, the injected error classified `Fail` and this is
                // no longer a control (see `sub_item`).
                PlaybinEvent::ExternalSubtitleFailed { id } => self.note(format!(
                    "external subtitle {id:?} was reported as failed; its error was \
                     classified Fail, not Recover, so this test would pass in both arms"
                )),
                PlaybinEvent::EndOfStream => {
                    self.note("the item reached EOS before the test was done".to_owned())
                }
                _ => {}
            }
        }
    }

    fn running(&self) -> Option<RunningState> {
        self.sm.borrow().running()
    }

    fn wait_until(&self, what: &str, bound: Duration, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + bound;
        loop {
            self.pump();
            if let Some(problem) = self.problem.borrow().clone() {
                panic!("{problem} (while waiting for {what})");
            }
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\n  pipeline {:?}, unsettled {:?}\n  machine {}\n  \
                 elements: {:?}",
                self.playbin.state_summary(),
                self.playbin.unsettled_elements(),
                self.sm.borrow().debug_model(),
                self.playbin.element_states(),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Pumps for `bound` or until `done`, without failing on the timeout.
    /// Returns whether `done` was reached.
    fn pump_until(&self, bound: Duration, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn rendered(&self) -> usize {
        self.video_sink
            .downcast_ref::<FTestSink>()
            .expect("the video sink is an FTestSink")
            .recording()
            .buffer_count()
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
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

/// Pins the coupling this file rests on: fcasttest's injected text must be the
/// text the crate recovers on. `lib.rs`'s `external_error_action` unit tests
/// pin the other end, so drift is a failing test rather than a regression test
/// that quietly stops discriminating.
#[test]
fn the_injected_transport_error_carries_the_text_the_crate_recovers_on() {
    assert_eq!(
        FlowStopReason::NotLinked.debug_text(),
        "streaming stopped, reason not-linked (-1)"
    );
    assert_eq!(
        FlowStopReason::Flushing.debug_text(),
        "streaming stopped, reason flushing (-2)"
    );
}

/// Setup both tests share. Each step is load-bearing:
///
/// 1. play to a settled PLAYING. From PAUSED this is a non-control: ftestsrc
///    parks at "refused as flushing, parking until FLUSH_STOP" and the pipeline
///    never re-prerolls in either arm.
/// 2. attach the erroring externals UNSELECTED. Selected, the seek is forwarded
///    into them and `handle_seek`'s `pad.stream_lock()` blocks on the very task
///    the stall gate holds, wedging the worker in both arms.
/// 3. seek. The crate answers `QueueSeek` and drops to PAUSED, the machine
///    dispatches the real flushing seek on the settled-Paused edge, and that
///    flush loses the state.
/// 4. wait for the lost state with the main video parked at the restart frame.
///
/// Returns the rendered-frame count just before the seek.
fn drive_into_the_lost_state_window(
    harness: &Harness,
    main: &ScenarioHandle,
    externals: &[(&ScenarioHandle, &str)],
) -> usize {
    harness.sm.borrow_mut().begin_load();
    harness.playbin.load_async(
        MediaInput::Uri(main.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_until("the load to report Loaded", BOUND, |h| h.loaded.get());
    harness.wait_until("a settled PLAYING", BOUND, |h| {
        h.running() == Some(RunningState::Playing)
            && h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });
    // Real playback first, so the seek lands mid-stream, not on a preroll.
    harness.wait_until("the first video frames to render", BOUND, |h| {
        h.rendered() >= 3
    });

    // Attached and deliberately left off. `Stream(None)` is asserted, not
    // assumed: decodebin3's auto-select can route a fresh external before
    // anyone asked, and the crate corrects that asynchronously.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    let ids: Vec<ExternalSubId> = externals
        .iter()
        .map(|(item, _)| {
            let id: ExternalSubId = harness.playbin.allocate_subtitle_id();
            harness.playbin.attach_subtitle_async(id, item.uri());
            id
        })
        .collect();
    for id in ids {
        harness.wait_until("every external to expose its text stream", BOUND, move |h| {
            !h.playbin.subtitle_stream_ids(id).is_empty()
        });
    }
    // Each task must be parked ON its gate before the seek, or a release below
    // races a source that has not reached the fault yet.
    for (item, gate) in externals {
        assert!(
            item.sync_point(gate).wait_for_arrival(BOUND),
            "external {} never reached its stall gate, so its error cannot be timed",
            item.key()
        );
    }
    harness.wait_until("the subtitle slot to be confirmed off", BOUND, |h| {
        matches!(&*h.selected_subtitle.borrow(), Some(None))
    });
    // Each attach puts an ASYNC_START/ASYNC_DONE pair on the bus. Seeking with
    // one in flight let the machine consume ITS settled-Paused edge as the
    // seek's settle, so the slot cleared before the flush went out and the
    // test measured nothing (observed, with two externals).
    harness.wait_until("the attaches to go quiet", BOUND, |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
            && !h.playbin.has_async_transition()
    });
    let _ = harness.pump_until(Duration::from_millis(400), |_| false);
    assert_eq!(
        harness.playbin.state_summary(),
        (gst::State::Playing, gst::State::VoidPending),
        "the pipeline has to be quiet before the seek"
    );

    let before = harness.rendered();
    // Arrivals rather than `waiting`: `park` registers the arrival once and
    // then busy-polls `is_released`, so `waiting()` reads zero throughout.
    // Counted from before the seek so a first-pass arrival cannot be mistaken
    // for the restart's (Realtime pacing cannot reach frame 500 by then).
    let holds_before = main.sync_point(MAIN_HOLD).arrivals();
    harness.playbin.cancel_selection_refresh();
    let dispatched = harness.sm.borrow_mut().seek_internal(
        Seek {
            position: Some(SEEK_TARGET),
            rate: None,
        },
        None,
    );
    if let Some(seek) = dispatched {
        harness.playbin.seek_async(seek);
    }

    // Both halves are needed: the parked restart proves the flush went out and
    // the sink cannot re-preroll (window stays open), and `(Paused, Paused)` is
    // the lost state itself. The park alone would also match the earlier
    // PLAYING->PAUSED dip `Job::Seek` performs before the real seek.
    harness.wait_until("the seek's flush to lose the pipeline state", BOUND, |h| {
        main.sync_point(MAIN_HOLD).arrivals() > holds_before
            && h.playbin.state_summary() == (gst::State::Paused, gst::State::Paused)
    });
    before
}

/// An error the crate consumes must not cost the caller its seek settle. The
/// error is released inside the lost-state window, latching FAILURE there. Then
/// the window reopens: sinks preroll and post ASYNC_DONE, the commit is refused,
/// and the receiver never leaves the seek. Lever:
/// `FCAST_NO_ERROR_STATE_UNLATCH=1`.
#[test]
fn a_consumed_error_still_lets_a_seek_settle() {
    init();
    let main = main_item("erruncommit1");
    let subs = sub_item("erruncommit1subs", EXT_GATE);
    let harness = Harness::new();
    let before = drive_into_the_lost_state_window(&harness, &main, &[(&subs, EXT_GATE)]);

    subs.release(EXT_GATE);
    // Hold the window open until the consumed error has been seen. Fixed: the
    // re-commit reports the settle and this returns early. Levered: nothing is
    // reported and it runs its bound, which is what puts the error inside the
    // window rather than after the commit.
    let cleared_in_window = harness.pump_until(ERROR_BOUND, |h| !h.sm.borrow().is_seeking());
    main.release(MAIN_HOLD);

    harness.wait_until("the receiver to come back to Playing", SETTLE_BOUND, |h| {
        h.running() == Some(RunningState::Playing)
    });
    harness.wait_until("the pipeline to settle at PLAYING", SETTLE_BOUND, |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
            && !h.playbin.has_async_transition()
    });
    // Settled is not enough: prove the item is actually moving again.
    harness.wait_until("video to render past the seek", SETTLE_BOUND, |h| {
        h.rendered() > before
    });
    // Diagnostic only: whether the settle was announced promptly or arrived
    // later by some other route.
    eprintln!("the seek slot cleared inside the window: {cleared_in_window}");

    main.release_all();
    subs.release_all();
    harness.shutdown();
    main.unregister();
    subs.unregister();
}

/// A consumed error caught while the pipeline is committed to a DIFFERENT state
/// than it holds must re-commit THAT state, not the current one. Lever:
/// `FCAST_UNLATCH_RECOMMIT_CURRENT=1`.
///
/// The first error is the shape above: its re-commit frees the seek, the machine
/// re-asserts PLAYING, and with the main video still parked that climb is async,
/// so the pipeline reads `(Paused, Playing)`. The second error latches FAILURE
/// on exactly that. Re-committing `current` (PAUSED) there sets
/// `GST_STATE_TARGET` back to PAUSED and announces nothing: measured
/// `pipeline (Paused, VoidPending), unsettled []` with
/// `phase=Changing target=Playing`.
#[test]
fn a_consumed_error_on_a_pending_climb_keeps_the_climb() {
    init();
    let main = main_item("erruncommit2");
    let first = sub_item("erruncommit2a", "erruncommit2gatea");
    let second = sub_item("erruncommit2b", "erruncommit2gateb");
    let harness = Harness::new();
    let before = drive_into_the_lost_state_window(
        &harness,
        &main,
        &[
            (&first, "erruncommit2gatea"),
            (&second, "erruncommit2gateb"),
        ],
    );

    first.release("erruncommit2gatea");
    assert!(
        harness.pump_until(ERROR_BOUND, |h| !h.sm.borrow().is_seeking()),
        "the first error's shape was not handled, so the pending climb this \
         test needs never happens (that is `a_consumed_error_still_lets_a_seek_settle`)"
    );
    // The shape under test, asserted: without it the second error lands on
    // something else and the test measures nothing.
    assert!(
        harness.pump_until(ERROR_BOUND, |h| {
            h.playbin.state_summary() == (gst::State::Paused, gst::State::Playing)
        }),
        "the re-asserted PLAYING never showed up as a pending climb (pipeline \
         {:?}, machine {})",
        harness.playbin.state_summary(),
        harness.sm.borrow().debug_model(),
    );

    second.release("erruncommit2gateb");
    // Nothing to wait FOR: the second error's whole effect is on the pending
    // state, which nothing announces. A bounded pump gives it time to land
    // before the sink is allowed to preroll.
    let _ = harness.pump_until(Duration::from_millis(600), |_| false);
    main.release(MAIN_HOLD);

    harness.wait_until("the receiver to come back to Playing", SETTLE_BOUND, |h| {
        h.running() == Some(RunningState::Playing)
    });
    harness.wait_until("the pipeline to settle at PLAYING", SETTLE_BOUND, |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
            && !h.playbin.has_async_transition()
    });
    harness.wait_until("video to render past the seek", SETTLE_BOUND, |h| {
        h.rendered() > before
    });

    main.release_all();
    first.release_all();
    second.release_all();
    harness.shutdown();
    main.unregister();
    first.unregister();
    second.unregister();
}

/// Scrubbing DURING the lost-state window must not lose the seek. Field report
/// `seek-leads-to-freeze.txt`: `slot=Parked` forever, receiver stuck reporting
/// Buffering, every crate thread idle (`running()` needs `SeekSlot::None`).
///
/// The measured chain (`FCASTPLAYBIN_TEST_LOG=debug` on the levered arm): two
/// consumed external errors run `Job::ClearStateFailure` back-to-back, and each
/// emits its settle report while the pipeline is still async. The machine takes
/// the first as "dispatch the parked scrub" and the second as that seek's
/// completion (it never ran), re-asserts PLAYING, and the real `Job::Seek` is
/// refused and re-parked. `bin_handle_async_done` then continues to the
/// retargeted PLAYING, so the settled-Paused edge the parked seek waits for is
/// never posted. Levers: `FCAST_NO_PARKED_SEEK_RESCUE` (the fix this test
/// discriminates) and `FCAST_NO_SEEK_REFUSAL_EDGE`.
#[test]
fn scrubbing_inside_the_lost_state_window_keeps_the_seek() {
    init();
    let main = main_item("seekfreezemain");
    let first = sub_item("seekfreezea", EXT_GATE);
    let second = sub_item("seekfreezeb", EXT_GATE);
    let harness = Harness::new();

    let before =
        drive_into_the_lost_state_window(&harness, &main, &[(&first, EXT_GATE), (&second, EXT_GATE)]);

    // The scrub. Every one of these lands while the pipeline reads
    // `(Paused, Paused)`, so `Job::Seek` refuses each and hands it back.
    for pct in [0.34_f64, 0.50, 0.66, 0.17] {
        let target = gst::ClockTime::from_seconds((60.0 * pct) as u64);
        harness.playbin.cancel_selection_refresh();
        let dispatched = harness.sm.borrow_mut().seek_internal(
            Seek {
                position: Some(target),
                rate: None,
            },
            None,
        );
        if let Some(seek) = dispatched {
            harness.playbin.seek_async(seek);
        }
        let _ = harness.pump_until(Duration::from_millis(120), |_| false);
    }

    // NO VERDICT guards: a pass only counts if the scrubs landed inside the
    // window and are still tracked when it closes. Nothing here can be closed
    // by the fix (the window is held by MAIN_HOLD, which the fix never
    // touches), so these hold in both arms or the rig itself broke.
    assert_eq!(
        harness.playbin.state_summary(),
        (gst::State::Paused, gst::State::Paused),
        "NO VERDICT: the lost-state window closed before the scrub finished"
    );
    assert!(
        harness.sm.borrow().is_seeking(),
        "NO VERDICT: no seek is tracked after the scrub"
    );

    // The externals die INSIDE the window, so both consumed errors land on a
    // held pipeline (the double-settle detonator), deterministically. Then the
    // window closes and the receiver must converge on its own.
    first.release_all();
    second.release_all();
    let _ = harness.pump_until(Duration::from_millis(400), |_| false);
    main.release(MAIN_HOLD);

    harness.wait_until(
        "the receiver to come back to Playing after the scrub",
        SETTLE_BOUND,
        |h| h.running() == Some(RunningState::Playing),
    );
    harness.wait_until("video to render past the scrub", SETTLE_BOUND, |h| {
        h.rendered() > before
    });

    main.release_all();
    harness.shutdown();
    main.unregister();
    first.unregister();
    second.unregister();
}
