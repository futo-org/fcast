//! Regression tests for `fcastplaybin::state_machine`, driven through the
//! PUBLIC API only (the unit tests inside the module can peek at `Phase`/
//! `SeekSlot`; these deliberately cannot, so every assertion is about
//! observable behavior the receiver depends on).
//!
//! Each test below reproduces a concrete wedge/lost-request found by audit.

use fcastplaybin::state_machine::{
    BufferingStateResult, PlaybackState, RunningState, Seek, StateChangeResult, StateMachine,
};

const TEN: gst::ClockTime = gst::ClockTime::from_seconds(10);
const THIRTY: gst::ClockTime = gst::ClockTime::from_seconds(30);

/// A machine settled in Running(Playing), reached through the API.
fn playing() -> StateMachine {
    let mut sm = StateMachine::new();
    assert_eq!(
        sm.state_changed(
            gst::State::Null,
            gst::State::Playing,
            gst::State::VoidPending
        ),
        StateChangeResult::NewPlaybackState(PlaybackState::Playing)
    );
    sm
}

/// A machine settled in Running(Paused) with the pipeline tracked at PAUSED.
fn paused() -> StateMachine {
    let mut sm = StateMachine::new();
    assert_eq!(
        sm.state_changed(
            gst::State::Null,
            gst::State::Paused,
            gst::State::VoidPending
        ),
        StateChangeResult::NewPlaybackState(PlaybackState::Paused)
    );
    assert_eq!(sm.current_state, gst::State::Paused);
    sm
}

/// The worker hands a seek back (`PlaybinEvent::QueueSeek`) whenever the
/// pipeline was not settled in PAUSED when `Job::Seek` ran, and drives to
/// PAUSED itself. That hand-back is asynchronous: the receiver's event loop
/// can process a NEWER user seek before the hand-back arrives. Parking the
/// stale hand-back must not discard the newer seek, or a scrub lands on an
/// old position ("latest wins" is the documented contract).
#[test]
fn handed_back_seek_must_not_clobber_a_newer_parked_seek() {
    let mut sm = playing();

    let first = Seek::new(Some(TEN), Some(1.0));
    let newest = Seek::new(Some(THIRTY), Some(1.0));

    // The user seeks: dispatched to the worker.
    assert_eq!(sm.seek_internal(first, None), Some(first));
    // The user scrubs again while the first is still out: parked behind it.
    assert_eq!(sm.seek_internal(newest, None), None);
    // ...and only NOW does the worker's hand-back for the FIRST seek land.
    sm.queue_seek(first);

    // The pipeline settles at PAUSED (the worker drove it there): the seek
    // that goes out must be the newest one the user asked for.
    assert_eq!(
        sm.state_changed(
            gst::State::Playing,
            gst::State::Paused,
            gst::State::VoidPending
        ),
        StateChangeResult::Seek(newest),
        "the stale handed-back seek overwrote the newer parked seek",
    );
}

/// `uri_loaded` commits the auto-play transport while the pipeline is still
/// climbing (the Loaded event is emitted from the load job, which only
/// *starts* the READY->PAUSED transition). A pause arriving in that window is
/// recorded by `set_playback_state` but the Loading phase then adopted every
/// state edge verbatim, including the stale upward overshoot to PLAYING, so
/// the user's pause was silently lost and the item kept playing.
///
/// `Phase::Changing` already guards this exact overshoot
/// (`pause_during_load_survives_stale_playing_overshoot`); Loading did not.
#[test]
fn pause_after_the_load_transport_commit_is_not_lost() {
    let mut sm = StateMachine::new();
    sm.begin_load();
    // Teardown echo of the load's READY reset.
    assert_eq!(
        sm.state_changed(gst::State::Null, gst::State::Ready, gst::State::VoidPending),
        StateChangeResult::Waiting
    );
    // `uri_loaded`: commit the auto-play transport. The load job still owns
    // the pipeline, so the machine records it and the caller drives PLAYING.
    assert_eq!(sm.set_playback_state(RunningState::Playing), None);
    // The user pauses while the climb is still in flight.
    assert_eq!(sm.set_playback_state(RunningState::Paused), None);

    // The climb reports PAUSED with the upward commit still pending...
    let reached_paused =
        sm.state_changed(gst::State::Ready, gst::State::Paused, gst::State::Playing);
    // ...and then overshoots into PLAYING.
    let overshoot = sm.state_changed(
        gst::State::Paused,
        gst::State::Playing,
        gst::State::VoidPending,
    );

    assert_ne!(
        sm.running(),
        Some(RunningState::Playing),
        "the recorded pause was lost to the load's stale upward overshoot \
         (reached_paused={reached_paused:?}, overshoot={overshoot:?})",
    );
    assert_eq!(
        overshoot,
        StateChangeResult::ChangeState(gst::State::Paused),
        "the machine must correct the overshoot back to the recorded pause",
    );
    // The correction lands and only then do we settle, paused.
    assert_eq!(
        sm.state_changed(
            gst::State::Playing,
            gst::State::Paused,
            gst::State::VoidPending
        ),
        StateChangeResult::NewPlaybackState(PlaybackState::Paused)
    );
    assert_eq!(sm.running(), Some(RunningState::Paused));
}

/// The counterpart: an untouched load must still reach PLAYING through the
/// very same edges (the fix must not strand a normal startup).
#[test]
fn load_without_pause_still_reaches_playing_through_the_overshoot() {
    let mut sm = StateMachine::new();
    sm.begin_load();
    assert_eq!(
        sm.state_changed(gst::State::Null, gst::State::Ready, gst::State::VoidPending),
        StateChangeResult::Waiting
    );
    assert_eq!(sm.set_playback_state(RunningState::Playing), None);
    assert_eq!(
        sm.state_changed(gst::State::Ready, gst::State::Paused, gst::State::Playing),
        StateChangeResult::Waiting
    );
    assert_eq!(
        sm.state_changed(
            gst::State::Paused,
            gst::State::Playing,
            gst::State::VoidPending
        ),
        StateChangeResult::NewPlaybackState(PlaybackState::Playing)
    );
    assert_eq!(sm.running(), Some(RunningState::Playing));
}

/// A seek that fails while the source is rebuffering left the in-flight
/// marker set: `seek_failed` bails out on `Phase::Buffering` ("buffering
/// keeps its own recovery"), but buffering completion only re-derives the
/// transitions when the seek slot is CLEAR. With a stale in-flight marker it
/// returns `FinishedButWaitingSeek` and waits for a settle edge that will
/// never come (the pipeline is already settled where buffering put it), so
/// the machine never reports a running state again and the receiver is stuck
/// in "Buffering" for good.
#[test]
fn seek_failure_during_buffering_does_not_wedge_the_machine() {
    let mut sm = paused();

    // The user seeks: dispatched (the pipeline is settled in PAUSED).
    let seek = Seek::new(Some(TEN), Some(1.0));
    assert_eq!(sm.seek_internal(seek, None), Some(seek));

    // The source rebuffers while the seek is out. The caller re-commits
    // PAUSED, which the pipeline is already in: no state edge is posted.
    assert_eq!(
        sm.buffering(0),
        BufferingStateResult::Started(gst::State::Paused)
    );

    // The seek comes back as failed.
    assert_eq!(sm.seek_failed(), None);

    // Buffering completes. The pipeline is settled in PAUSED and will post
    // nothing further, so this is the machine's last chance to converge.
    let finished = sm.buffering(100);

    assert!(
        !sm.is_seeking(),
        "a failed seek left its in-flight marker set: {finished:?}",
    );
    assert_eq!(
        sm.running(),
        Some(RunningState::Paused),
        "the machine never settled again; the receiver reports Buffering \
         forever ({finished:?})",
    );
}

/// The same failure path must NOT discard a newer seek that superseded the
/// failed one mid-buffer (that one has not been dispatched yet, so it cannot
/// be what failed).
#[test]
fn seek_failure_during_buffering_keeps_a_superseding_seek() {
    let mut sm = playing();

    assert_eq!(
        sm.seek_internal(Seek::new(Some(TEN), None), None),
        Some(Seek::new(Some(TEN), Some(1.0)))
    );
    assert_eq!(
        sm.buffering(10),
        BufferingStateResult::Started(gst::State::Paused)
    );
    // A newer seek supersedes the in-flight one while buffering.
    assert_eq!(sm.seek_internal(Seek::new(Some(THIRTY), None), None), None);
    // The old, superseded seek reports failure.
    assert_eq!(sm.seek_failed(), None);
    // The pipeline settles at PAUSED during buffering, then buffering
    // completes: the superseding seek must still be dispatched.
    assert_eq!(
        sm.state_changed(
            gst::State::Playing,
            gst::State::Paused,
            gst::State::VoidPending
        ),
        StateChangeResult::Waiting
    );
    assert_eq!(
        sm.buffering(100),
        BufferingStateResult::FinishedWithSeek(Seek::new(Some(THIRTY), Some(1.0))),
    );
}
