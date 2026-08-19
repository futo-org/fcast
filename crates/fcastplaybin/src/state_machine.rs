//! Playback lifecycle state machine.
//!
//! Pure logic: it maps GStreamer state-change / buffering / async-done
//! transitions onto a coherent playback model and tells the caller what to do
//! next. It performs no side effects and never touches the pipeline itself,
//! so it is exhaustively unit-testable without a live pipeline.
//!
//! The model is three orthogonal fields instead of one flat enum:
//!
//! - `phase`: what the machine is settled in or waiting on (stopped, loading,
//!   buffering, a requested transition, running).
//! - `target`: the desired transport state (PAUSED/PLAYING) every transition
//!   converges toward. Stored once, not duplicated per variant.
//! - `slot`: the seek bookkeeping, at most one parked seek (latest wins) plus
//!   an in-flight marker. Seek edges take precedence over `phase` except during
//!   buffering, whose completion drives the transitions itself.

use tracing::{debug, error, warn};

/// A coherent, settled playback state. Deliberately NOT the FCast wire enum -
/// fcastplaybin is protocol-agnostic and the receiver maps this onto
/// `fcast_protocol::PlaybackState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Idle,
    Paused,
    Playing,
}

/// A seek request: absolute `position` (`None` = current) and `rate`
/// (`None` = keep current).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Seek {
    pub position: Option<gst::ClockTime>,
    pub rate: Option<f32>,
}

impl Seek {
    pub fn new(position: Option<gst::ClockTime>, rate: Option<f32>) -> Self {
        Self { position, rate }
    }

    pub fn rate_is_safe(rate: f32) -> bool {
        rate.is_finite() && rate != 0.0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RunningState {
    Paused,
    Playing,
}

impl From<RunningState> for gst::State {
    fn from(value: RunningState) -> Self {
        match value {
            RunningState::Paused => gst::State::Paused,
            RunningState::Playing => gst::State::Playing,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum StateChangeResult {
    NewPlaybackState(PlaybackState),
    Seek(Seek),
    Waiting,
    ChangeState(gst::State),
}

#[derive(Debug, PartialEq)]
pub enum BufferingStateResult {
    Started(gst::State),
    Buffering,
    /// Buffering finished with a parked seek, but the pipeline has ALREADY
    /// settled at `Paused`, so the edge the parked seek waits for will never
    /// arrive again: dispatch it immediately (caller sends `Job::Seek`).
    FinishedWithSeek(Seek),
    /// Buffering finished but a seek is still parked (waiting for the
    /// pipeline to reach `Paused`) or already in flight. Nothing to dispatch.
    FinishedButWaitingSeek,
    Finished(Option<gst::State>),
}

/// What the machine is settled in or waiting on. Meaningful while no seek is
/// parked or in flight (`SeekSlot::None`), with one exception: `Buffering`
/// coexists with the seek slot, because buffering completion, not the state
/// edges, decides what happens next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Stopped,
    /// A load was queued, waiting for the pipeline's climb. `committed`
    /// records whether a transport has been requested for this load yet
    /// (`uri_loaded`, or a user Play/Pause landing mid-load). Until then
    /// `target` is only `begin_load`'s optimistic auto-play default and the
    /// climb's states are adopted verbatim; afterwards the climb is a
    /// transition toward `target` and is judged against it, exactly like
    /// `Changing`.
    Loading {
        committed: bool,
    },
    Buffering {
        percent: i32,
    },
    /// A transition toward `target` was requested (or a correction is owed).
    Changing,
    Running(RunningState),
}

/// The seek bookkeeping: at most one parked seek (latest wins) and an
/// in-flight marker. While non-`None` (outside buffering), the seek edges own
/// `state_changed`; `phase` is recomputed from `target` when the slot clears.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SeekSlot {
    None,
    /// Parked, waiting for a settled-Paused edge to dispatch.
    Parked(Seek),
    /// Dispatched to the pipeline, completion pending.
    InFlight,
    /// Dispatched, AND a newer seek parked behind it (latest wins).
    InFlightParked(Seek),
}

#[derive(Debug)]
pub struct StateMachine {
    pub current_state: gst::State,
    pub is_live: bool,
    pub rate: f64,
    phase: Phase,
    target: gst::State,
    slot: SeekSlot,
    /// Re-assert PAUSED when a tracked seek sees the pipeline settle at
    /// PLAYING. Lever: `FCAST_NO_PARKED_SEEK_RESCUE` (set = off).
    rescue: bool,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            current_state: gst::State::Ready,
            is_live: false,
            rate: 1.0,
            phase: Phase::Stopped,
            target: gst::State::Paused,
            slot: SeekSlot::None,
            rescue: std::env::var_os("FCAST_NO_PARKED_SEEK_RESCUE").is_none(),
        }
    }

    /// A new load was queued: wait for the pipeline's climb. Any seek
    /// bookkeeping belongs to the superseded item. `current_state` adopts the
    /// load's READY reset synthetically, because the caller drops the
    /// superseded generation's state edges (including the reset's own) and a
    /// leaked stale `Playing` makes buffering completion settle without
    /// dispatching, parking the load.
    pub fn begin_load(&mut self) {
        self.phase = Phase::Loading { committed: false };
        self.slot = SeekSlot::None;
        self.target = gst::State::Playing;
        self.current_state = gst::State::Ready;
    }

    /// Settled and idle (nothing loaded, no seek pending).
    pub fn is_stopped(&self) -> bool {
        self.phase == Phase::Stopped && self.slot == SeekSlot::None
    }

    /// The settled transport state, `None` while anything (load, buffering,
    /// transition, seek) is still in flight.
    pub fn running(&self) -> Option<RunningState> {
        match (self.phase, self.slot) {
            (Phase::Running(rs), SeekSlot::None) => Some(rs),
            _ => None,
        }
    }

    /// Park a seek to fire on the next settled-Paused edge (the pipeline
    /// handed it back because it could not perform it yet). Latest wins over
    /// anything already parked or in flight.
    pub fn queue_seek(&mut self, seek: Seek) {
        self.target = match (self.slot, self.phase) {
            (SeekSlot::None, Phase::Running(rs)) => rs.into(),
            (SeekSlot::None, Phase::Stopped | Phase::Loading { .. }) => gst::State::Paused,
            // Buffering/Changing, or a seek already tracked: keep the target.
            _ => self.target,
        };
        // Latest wins, and the hand-back is NOT necessarily the latest: it
        // names the seek the pipeline refused and travels back through the
        // event queue, so a newer user seek can already be tracked.
        self.slot = match self.slot {
            SeekSlot::Parked(newer) | SeekSlot::InFlightParked(newer) => SeekSlot::Parked(newer),
            SeekSlot::None | SeekSlot::InFlight => SeekSlot::Parked(seek),
        };
        // The parked seek's edges own the machine now. Buffering bookkeeping
        // (if any) is superseded.
        if matches!(self.phase, Phase::Buffering { .. }) {
            self.phase = Phase::Changing;
        }
    }

    #[must_use]
    pub fn seek_internal(
        &mut self,
        mut seek: Seek,
        target_state: Option<gst::State>,
    ) -> Option<Seek> {
        if self.is_live {
            warn!("Cannot seek when source is live");
            return None;
        } else if self.is_stopped() {
            warn!("Cannot seek when not playing");
            return None;
        }

        debug!(?seek, phase = ?self.phase, slot = ?self.slot, current_state = ?self.current_state);

        if seek.rate.is_none() {
            seek.rate = Some(self.rate as f32);
        }

        match self.slot {
            // Latest wins: replace whatever is parked.
            SeekSlot::Parked(_) => {
                debug!("Coalescing into the already-parked seek");
                self.slot = SeekSlot::Parked(seek);
                return None;
            }
            SeekSlot::InFlight | SeekSlot::InFlightParked(_) => {
                if matches!(self.phase, Phase::Buffering { .. }) {
                    // While buffering, a new seek supersedes the in-flight
                    // one entirely (both are flushing) and buffering
                    // completion dispatches it.
                    self.slot = SeekSlot::Parked(seek);
                } else {
                    // Park behind the in-flight seek; dispatched at its settle.
                    debug!("Parking a seek behind the in-flight one");
                    self.slot = SeekSlot::InFlightParked(seek);
                }
                return None;
            }
            SeekSlot::None => {}
        }

        // Nothing in flight: dispatch now.
        let target = target_state.unwrap_or(match self.phase {
            Phase::Running(RunningState::Playing) => gst::State::Playing,
            Phase::Buffering { .. } | Phase::Changing => self.target,
            _ => gst::State::Paused,
        });
        self.target = target;
        self.slot = SeekSlot::InFlight;
        if matches!(self.phase, Phase::Buffering { .. }) {
            self.phase = Phase::Changing;
        }
        Some(seek)
    }

    pub fn is_seeking(&self) -> bool {
        self.slot != SeekSlot::None
    }

    #[must_use]
    pub fn set_playback_state(&mut self, state: RunningState) -> Option<gst::State> {
        let next: gst::State = state.into();
        // Anything in flight (seek, buffering, transition) just retargets:
        // the in-flight operation's settle converges onto the new target.
        if self.slot != SeekSlot::None
            || matches!(self.phase, Phase::Buffering { .. } | Phase::Changing)
        {
            self.target = next;
            return None;
        }
        match self.phase {
            Phase::Stopped => {
                error!("Cannot set playback state when the player is stopped");
                None
            }
            // Record the target: nothing is dispatched while the load job
            // owns the pipeline, but the load's completion and any buffering
            // started mid-load converge on it.
            Phase::Loading { .. } => {
                self.target = next;
                // From here the climb is judged against `target`, so a Pause
                // landing after the load's PLAYING commit is honored instead
                // of being erased by the commit's own overshoot.
                self.phase = Phase::Loading { committed: true };
                None
            }
            Phase::Running(current) => {
                if current != state {
                    self.phase = Phase::Changing;
                    self.target = next;
                    Some(next)
                } else {
                    None
                }
            }
            Phase::Buffering { .. } | Phase::Changing => unreachable!("handled above"),
        }
    }

    #[must_use]
    pub fn buffering(&mut self, new_percent: i32) -> BufferingStateResult {
        // A lone completion report without a buffering start is a no-op.
        if new_percent >= 100 && !matches!(self.phase, Phase::Buffering { .. }) {
            return BufferingStateResult::Buffering;
        }

        if let Phase::Buffering { percent } = &mut self.phase {
            *percent = new_percent;
            if new_percent < 100 {
                return BufferingStateResult::Buffering;
            }
            debug!("Buffering completed");
            match self.slot {
                SeekSlot::Parked(seek) => {
                    self.phase = Phase::Changing;
                    // A parked seek fires on the next settled-Paused edge; if
                    // the pipeline already settled at Paused DURING buffering
                    // that edge is in the past and GStreamer will not repeat
                    // it, so parking would stall forever. Dispatch now.
                    if self.current_state == gst::State::Paused {
                        self.slot = SeekSlot::InFlight;
                        return BufferingStateResult::FinishedWithSeek(seek);
                    }
                    return BufferingStateResult::FinishedButWaitingSeek;
                }
                SeekSlot::InFlight | SeekSlot::InFlightParked(_) => {
                    self.phase = Phase::Changing;
                    return BufferingStateResult::FinishedButWaitingSeek;
                }
                SeekSlot::None => {}
            }
            // Buffering held the pipeline at PAUSED. Settle in place ONLY when
            // PAUSED is also the destination AND the pipeline already reached
            // it; otherwise redispatch toward the target. A PLAYING target
            // must always redispatch: the buffering PAUSE can still be in
            // flight and current_state can lag it (a fully prefetched gapless
            // item rebuffers and completes within one state round-trip, so
            // buffering(100) arrives while current_state still reads the
            // carried-over PLAYING - the gapless swap keeps the state machine
            // and never runs begin_load's reset to READY).
            if self.target != gst::State::Paused || self.current_state != gst::State::Paused {
                self.phase = Phase::Changing;
                return BufferingStateResult::Finished(Some(self.target));
            }
            self.phase = Phase::Running(RunningState::Paused);
            return BufferingStateResult::Finished(None);
        }

        // Buffering starts. Remember the transport to restore afterward (a
        // tracked seek keeps its own target).
        if self.slot == SeekSlot::None {
            self.target = match self.phase {
                Phase::Stopped => gst::State::Playing,
                // A load defaults to Playing (`begin_load`), but a transport
                // request that landed mid-load was recorded and must survive
                // the rebuffer.
                Phase::Loading { .. } | Phase::Changing => self.target,
                Phase::Running(rs) => rs.into(),
                Phase::Buffering { .. } => unreachable!("handled above"),
            };
        }
        // A seek parked behind an in-flight one supersedes it entirely (both
        // are flushing): within buffering only parked-or-in-flight exists.
        if let SeekSlot::InFlightParked(seek) = self.slot {
            self.slot = SeekSlot::Parked(seek);
        }
        self.phase = Phase::Buffering {
            percent: new_percent,
        };
        BufferingStateResult::Started(gst::State::Paused)
    }

    #[must_use]
    pub fn state_changed(
        &mut self,
        _old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> StateChangeResult {
        debug!(?new, ?pending, phase = ?self.phase, slot = ?self.slot, "State changed");
        self.current_state = new;

        let settled_paused = new == gst::State::Paused && pending == gst::State::VoidPending;

        // Buffering consumes every edge (its completion drives what happens
        // next). It only notes an in-flight seek settling mid-buffer.
        if matches!(self.phase, Phase::Buffering { .. }) {
            if settled_paused && self.slot == SeekSlot::InFlight {
                self.slot = SeekSlot::None;
            }
            return StateChangeResult::Waiting;
        }

        // Seek edges take precedence over the phase while a seek is tracked.
        //
        // A tracked seek waits for a settled-Paused EDGE, so a pipeline that
        // settles at PLAYING instead has provably skipped it (a retargeting
        // `set_state` makes `bin_handle_async_done` continue to TARGET, and
        // the intermediate Paused commit carries `pending = Playing`). Left
        // alone the slot never clears and `running()` reads `None` forever.
        // Re-asserting PAUSED manufactures the missing settle.
        let stranded =
            self.rescue && new == gst::State::Playing && pending == gst::State::VoidPending;
        if stranded && self.slot != SeekSlot::None {
            debug!(slot = ?self.slot, "the pipeline settled at PLAYING past a tracked seek; re-asserting PAUSED");
        }
        match self.slot {
            SeekSlot::Parked(seek) => {
                return if settled_paused {
                    self.slot = SeekSlot::InFlight;
                    StateChangeResult::Seek(seek)
                } else if stranded {
                    StateChangeResult::ChangeState(gst::State::Paused)
                } else {
                    StateChangeResult::Waiting
                };
            }
            SeekSlot::InFlightParked(seek) => {
                return if settled_paused {
                    // The parked seek goes out first. The target correction
                    // waits for its settle.
                    debug!(?seek, "Dispatching the parked seek");
                    self.slot = SeekSlot::InFlight;
                    StateChangeResult::Seek(seek)
                } else if stranded {
                    StateChangeResult::ChangeState(gst::State::Paused)
                } else {
                    StateChangeResult::Waiting
                };
            }
            SeekSlot::InFlight => {
                return if settled_paused {
                    self.slot = SeekSlot::None;
                    if self.target != gst::State::Paused {
                        debug!("Seek completed");
                        self.phase = Phase::Changing;
                        StateChangeResult::ChangeState(self.target)
                    } else {
                        self.phase = Phase::Running(RunningState::Paused);
                        StateChangeResult::NewPlaybackState(PlaybackState::Paused)
                    }
                } else if stranded {
                    StateChangeResult::ChangeState(gst::State::Paused)
                } else {
                    StateChangeResult::Waiting
                };
            }
            SeekSlot::None => {}
        }

        match self.phase {
            Phase::Stopped | Phase::Loading { .. } => {
                if matches!(pending, gst::State::Ready | gst::State::Null) {
                    return StateChangeResult::Waiting;
                }
                // Ready/Null/VoidPending arrivals here are teardown echoes,
                // not playback-state edges.
                if !matches!(new, gst::State::Paused | gst::State::Playing) {
                    return StateChangeResult::Waiting;
                }
                // Once a transport is committed for this load, its climb is a
                // transition toward `target` and carries the same hazard
                // `Phase::Changing` guards: the load's PLAYING commit is still
                // in flight when a user Pause retargets it, so both the
                // Paused-with-pending-Playing edge and the PLAYING overshoot
                // that follows must be corrected, not adopted.
                if matches!(self.phase, Phase::Loading { committed: true }) {
                    let stale_upward = new == gst::State::Paused && pending == gst::State::Playing;
                    if new != self.target || stale_upward {
                        return if pending == gst::State::VoidPending {
                            self.phase = Phase::Changing;
                            StateChangeResult::ChangeState(self.target)
                        } else {
                            StateChangeResult::Waiting
                        };
                    }
                }
                match new {
                    gst::State::Playing => {
                        self.phase = Phase::Running(RunningState::Playing);
                        StateChangeResult::NewPlaybackState(PlaybackState::Playing)
                    }
                    _ => {
                        self.phase = Phase::Running(RunningState::Paused);
                        StateChangeResult::NewPlaybackState(PlaybackState::Paused)
                    }
                }
            }
            Phase::Changing => {
                // Reaching Paused while the pipeline is still committed
                // upward to Playing is NOT arrival, even when Paused is the
                // target: a stale upward transition is in flight (the load's
                // original Playing commit arriving after a user Pause
                // retargeted this change), and settling here lets the
                // overshoot flip the machine to Playing. Wait for it; the
                // `pending == VoidPending` branch below issues the correction.
                let stale_upward = new == gst::State::Paused && pending == gst::State::Playing;
                if new == self.target && !stale_upward {
                    match new {
                        gst::State::Paused => {
                            self.phase = Phase::Running(RunningState::Paused);
                            StateChangeResult::NewPlaybackState(PlaybackState::Paused)
                        }
                        gst::State::Playing => {
                            self.phase = Phase::Running(RunningState::Playing);
                            StateChangeResult::NewPlaybackState(PlaybackState::Playing)
                        }
                        _ => {
                            self.phase = Phase::Stopped;
                            StateChangeResult::NewPlaybackState(PlaybackState::Idle)
                        }
                    }
                } else if pending == gst::State::VoidPending {
                    StateChangeResult::ChangeState(self.target)
                } else {
                    StateChangeResult::Waiting
                }
            }
            Phase::Running(running) => match (new, pending) {
                (gst::State::VoidPending | gst::State::Null | gst::State::Ready, _)
                | (_, gst::State::Null) => {
                    self.phase = Phase::Stopped;
                    StateChangeResult::NewPlaybackState(PlaybackState::Idle)
                }
                // A downward dip THROUGH Paused while an async transition is
                // in flight and the machine holds Playing is never a user
                // state (user pauses/seeks/buffering all leave Running before
                // their edges arrive): it is a bin-internal async re-preroll,
                // e.g. a fresh sink activating mid-load. Settling to
                // Running(Paused) here LOSES the Playing target for good.
                (gst::State::Paused, gst::State::Paused | gst::State::Playing)
                    if running == RunningState::Playing =>
                {
                    self.phase = Phase::Changing;
                    self.target = gst::State::Playing;
                    StateChangeResult::Waiting
                }
                (gst::State::Paused, _) => {
                    self.phase = Phase::Running(RunningState::Paused);
                    StateChangeResult::NewPlaybackState(PlaybackState::Paused)
                }
                (gst::State::Playing, _) => {
                    self.phase = Phase::Running(RunningState::Playing);
                    StateChangeResult::NewPlaybackState(PlaybackState::Playing)
                }
            },
            Phase::Buffering { .. } => unreachable!("handled above"),
        }
    }

    pub fn seek_failed(&mut self) -> Option<gst::State> {
        // Only an in-flight seek can fail. Buffering owns the pipeline while
        // it runs, so there is nothing to re-commit here and completion
        // re-derives the transitions - but only when the seek slot is CLEAR,
        // so retire the dead in-flight marker. Leaving it made completion
        // report `FinishedButWaitingSeek` and wait for a settle edge an
        // already-settled pipeline never posts again.
        if matches!(self.phase, Phase::Buffering { .. }) {
            if self.slot == SeekSlot::InFlight {
                self.slot = SeekSlot::None;
            }
            // A seek parked mid-buffer superseded the failed one and has not
            // been dispatched yet, so it is not what failed: keep it.
            return None;
        }
        match self.slot {
            SeekSlot::InFlight | SeekSlot::InFlightParked(_) => {
                if let SeekSlot::InFlightParked(dropped) = self.slot {
                    // The in-flight seek failed, so the parked one (same
                    // mechanics) would fail too. Drop it rather than loop.
                    warn!(?dropped, "Dropping the parked seek after a seek failure");
                }
                self.slot = SeekSlot::None;
                if self.target == self.current_state {
                    self.phase = match self.target {
                        gst::State::Playing => Phase::Running(RunningState::Playing),
                        gst::State::Paused => Phase::Running(RunningState::Paused),
                        _ => Phase::Stopped,
                    };
                    None
                } else {
                    self.phase = Phase::Changing;
                    Some(self.target)
                }
            }
            _ => None,
        }
    }

    pub fn clear_state(&mut self) {
        self.phase = Phase::Stopped;
        self.slot = SeekSlot::None;
        self.target = gst::State::Paused;
        self.is_live = false;
        self.rate = 1.0;
    }

    #[cfg(test)]
    fn probe(&self) -> (Phase, gst::State, SeekSlot) {
        (self.phase, self.target, self.slot)
    }

    /// The whole internal model in one string, for a test's failure message:
    /// `phase=Running(Playing) slot=InFlight current=Paused` names a
    /// dispatched seek whose settle edge can never arrive.
    pub fn debug_model(&self) -> String {
        format!(
            "phase={:?} target={:?} slot={:?} current={:?}",
            self.phase, self.target, self.slot, self.current_state
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! gs {
        ($state:ident) => {
            gst::State::$state
        };
    }

    macro_rules! rs {
        ($state:ident) => {
            RunningState::$state
        };
    }

    macro_rules! new_ps {
        ($state:ident) => {
            StateChangeResult::NewPlaybackState(PlaybackState::$state)
        };
    }

    const CTZ: gst::ClockTime = gst::ClockTime::ZERO;
    const FIVE: gst::ClockTime = gst::ClockTime::from_seconds(5);
    const TEN: gst::ClockTime = gst::ClockTime::from_seconds(10);
    const THIRTY: gst::ClockTime = gst::ClockTime::from_seconds(30);

    /// A machine settled in Running(Playing), reached through the API.
    fn playing() -> StateMachine {
        let mut sm = StateMachine::new();
        assert_eq!(
            sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)),
            new_ps!(Playing)
        );
        sm
    }

    /// A machine settled in Running(Paused), reached through the API.
    fn paused() -> StateMachine {
        let mut sm = StateMachine::new();
        assert_eq!(
            sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)),
            new_ps!(Paused)
        );
        sm
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(CTZ), None), None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        // The phase is a don't-care while a seek is tracked: target + slot
        // carry the behavior.
        assert_eq!(sm.probe().1, gs!(Playing));
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_2() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(CTZ), None), None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(Paused)), new_ps!(Playing));

        // 2nd seek:
        let sixty = gst::ClockTime::from_seconds(60);
        assert_eq!(sm.seek_internal(Seek::new(Some(sixty), None), None), Some(Seek::new(Some(sixty), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        sm.queue_seek(Seek::new(Some(sixty), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(sixty), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        sm.queue_seek(Seek::new(Some(sixty), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(sixty), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(Paused)), new_ps!(Playing));
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_3() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek {position: Some(CTZ), rate: None}, None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.buffering(1), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        sm.queue_seek(Seek { position: Some(CTZ), rate: Some(1.0) });
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(88), BufferingStateResult::Buffering);
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.buffering(89), BufferingStateResult::Buffering);
        assert_eq!(sm.buffering(100), BufferingStateResult::Finished(Some(gs!(Playing))));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.seek_internal(Seek { position: None, rate: Some(1.25) }, None), Some(Seek { position: None, rate: Some(1.25) }));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_4() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(CTZ), None), None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::Parked(Seek::new(Some(CTZ), Some(1.0)))));
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(sm.probe().0, Phase::Buffering { .. }));
        let sixty = gst::ClockTime::from_seconds(60);
        assert_eq!(sm.seek_internal(Seek::new(Some(sixty), None), None), None);
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(sixty), Some(1.0))));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn state_change() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.set_playback_state(RunningState::Playing), Some(gst::State::Playing));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);

        assert_eq!(sm.set_playback_state(RunningState::Paused), Some(gst::State::Paused));
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Paused), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.running(), Some(rs!(Paused)));
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
    }

    #[test]
    #[rustfmt::skip]
    fn changing_state_honors_latest_request() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Paused), SeekSlot::None));
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    fn lone_buffering_100_while_playing_must_not_strand() {
        let mut sm = playing();
        let r = sm.buffering(100);

        assert_ne!(
            r,
            BufferingStateResult::Started(gst::State::Paused),
            "buffering(100) while Playing requested a pause -> playback stalls",
        );
        assert!(
            !matches!(sm.probe().0, Phase::Buffering { .. }),
            "stuck in Buffering after a lone buffering(100)",
        );
    }

    #[test]
    fn lone_buffering_100_during_uri_change_must_not_strand() {
        let mut sm = StateMachine::new();
        sm.begin_load();

        let r = sm.buffering(100);
        assert_ne!(
            r,
            BufferingStateResult::Started(gst::State::Paused),
            "buffering(100) during URI change requested a pause",
        );

        let _ = sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending));
        let _ = sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending));

        assert!(
            !matches!(sm.probe().0, Phase::Buffering { .. }),
            "stuck in Buffering after startup despite pipeline reaching Playing",
        );
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn normal_rebuffer_while_playing_recovers() {
        let mut sm = playing();
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.buffering(40), BufferingStateResult::Buffering);
        assert_eq!(sm.buffering(100), BufferingStateResult::Finished(Some(gs!(Playing))));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn fast_rebuffer_before_paused_settles_recovers() {
        // Gapless queue handoff: a fully prefetched next item rebuffers right
        // after the swap and completes within one state round-trip, so
        // buffering(100) arrives BEFORE the buffering(0)'s Paused settles and
        // current_state still reads the previous item's Playing. Completion
        // must redispatch Playing or the in-flight Paused parks the item.
        let mut sm = playing();
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.current_state, gs!(Playing), "the buffering Paused has not settled yet");
        assert_eq!(sm.buffering(100), BufferingStateResult::Finished(Some(gs!(Playing))));
        // The in-flight Paused lands now, after completion already retargeted.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn rapid_play_pause_toggles_converge_to_last_request() {
        let mut sm = playing();
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.set_playback_state(rs!(Paused)), None);
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn changing_waits_through_async_transition_then_recovers() {
        let mut sm = paused();
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None),
            "fall-through must not mutate state while the transition is still in flight");
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)),
            "stranded in Changing after the async transition completed");
    }

    #[test]
    #[rustfmt::skip]
    fn changing_fallthrough_recovers_even_if_target_flipped_mid_transition() {
        let mut sm = paused();
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.set_playback_state(rs!(Paused)), None);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Paused), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Paused), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.running(), Some(rs!(Paused)));
    }

    #[test]
    #[rustfmt::skip]
    fn seek_is_rejected_when_stopped_or_live() {
        let mut sm = StateMachine::new();
        assert!(sm.is_stopped());
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), None);
        assert!(sm.is_stopped(), "rejected seek must not mutate state");
        let mut sm = playing();
        sm.is_live = true;
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), None);
        assert_eq!(sm.running(), Some(rs!(Playing)), "rejected live seek must not mutate state");
    }

    #[test]
    #[rustfmt::skip]
    fn seek_while_seeking_parks_latest_and_dispatches_on_settle() {
        // A seek arriving while one is in flight must park (latest wins) and
        // dispatch at the in-flight seek's settle, before the target-state
        // correction.
        let twenty = gst::ClockTime::from_seconds(20);
        let mut sm = playing();
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), Some(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.probe(), (Phase::Running(rs!(Playing)), gs!(Playing), SeekSlot::InFlight));
        // Two more seeks while in flight: only the latest is remembered.
        assert_eq!(sm.seek_internal(Seek::new(Some(FIVE), None), None), None);
        assert_eq!(sm.seek_internal(Seek::new(Some(twenty), None), None), None);
        assert_eq!(sm.probe().2, SeekSlot::InFlightParked(Seek::new(Some(twenty), Some(1.0))));
        // The first seek settles: the parked one goes out immediately.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(twenty), Some(1.0))));
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
        // The parked seek settles: normal recovery back toward Playing.
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn seek_during_buffering_with_inflight_seek_is_not_lost() {
        // A seek arriving while buffering holds an in-flight marker must
        // supersede it (latest wins) and dispatch after buffering completes.
        let mut sm = playing();
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), Some(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.buffering(10), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
        // A newer seek arrives mid-buffer: it supersedes the in-flight one.
        assert_eq!(sm.seek_internal(Seek::new(Some(THIRTY), None), None), None);
        assert!(matches!(sm.probe().2, SeekSlot::Parked(_)));
        // Pipeline settles at Paused during buffering, then buffering
        // completes: the parked seek must be dispatched, not dropped.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(
            sm.buffering(100),
            BufferingStateResult::FinishedWithSeek(Seek::new(Some(THIRTY), Some(1.0))),
        );
    }

    #[test]
    #[rustfmt::skip]
    fn parked_seek_survives_a_rebuffer() {
        let mut sm = playing();
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), Some(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.seek_internal(Seek::new(Some(THIRTY), None), None), None);
        assert_eq!(sm.buffering(10), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(sm.probe().2, SeekSlot::Parked(_)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(
            sm.buffering(100),
            BufferingStateResult::FinishedWithSeek(Seek::new(Some(THIRTY), Some(1.0))),
        );
    }

    #[test]
    #[rustfmt::skip]
    fn seek_async_stays_parked_until_pipeline_settles_at_paused() {
        // A parked seek is only dispatched once the pipeline actually settles
        // at Paused/VoidPending; interim transitions leave it parked. That is
        // also why the subtitle refresh (TrackOps) uses a direct flushing seek
        // via Job::RefreshSeek instead of driving this state machine.
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(TEN), Some(1.0)));
        assert_eq!(sm.probe(), (Phase::Running(rs!(Playing)), gs!(Playing), SeekSlot::Parked(Seek::new(Some(TEN), Some(1.0)))));
        // Reached Paused but still transitioning (pending != VoidPending): parked.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert!(matches!(sm.probe().2, SeekSlot::Parked(_)), "must stay parked while still transitioning");
        // Settled at PLAYING: stays parked, but the machine re-asserts PAUSED
        // (a settled pipeline posts no further edges to wait for).
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        assert!(matches!(sm.probe().2, SeekSlot::Parked(_)));
        // Settled at Paused/VoidPending: the queued seek is finally dispatched.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
    }

    #[test]
    #[rustfmt::skip]
    fn pause_during_load_survives_stale_playing_overshoot() {
        // A user Pause retargets the load's Changing-to-Playing to Paused
        // while the pipeline's original commit to Playing is still in flight.
        // Reaching Paused with pending=Playing must NOT count as arrival, or
        // the overshoot to Playing gets adopted and un-pauses the user.
        let mut sm = paused();
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        // The user pauses mid-climb: retargets the in-flight change.
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Paused), SeekSlot::None));
        // The stale upward commit still delivers Paused-with-pending-Playing...
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Paused), SeekSlot::None), "must not settle while the overshoot is in flight");
        // ...and overshoots to Playing. The machine must correct, not adopt.
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        // The correction lands and only then do we settle, paused.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.running(), Some(rs!(Paused)));
    }

    #[test]
    #[rustfmt::skip]
    fn queued_seek_while_playing_recovers_to_playing() {
        // The full happy path of a seek issued while playing: park ->
        // dispatch on settle -> re-preroll -> back to Playing.
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(TEN), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.probe(), (Phase::Running(rs!(Playing)), gs!(Playing), SeekSlot::InFlight));
        // Seek finished (pipeline re-prerolled to Paused). Target is Playing
        // so we must drive back up rather than stranding in Paused.
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    #[test]
    #[rustfmt::skip]
    fn buffering_completing_before_paused_settled_parks_in_seek_async() {
        // Buffering finishes while the pipeline is still transitioning (no
        // Paused settle yet): the seek stays parked and fires on the later
        // Paused/VoidPending edge.
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.current_state, gs!(Playing), "no Paused settle seen yet");
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.probe().2, SeekSlot::Parked(Seek::new(Some(CTZ), Some(1.0))));
        // The eventual Paused settle dispatches the parked seek.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
    }

    #[test]
    fn async_seek_not_dropped_when_pause_arrives_during_buffering() {
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(THIRTY), Some(1.0)));

        assert_eq!(sm.buffering(40), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(sm.probe().2, SeekSlot::Parked(_)));
        // Paused arrives DURING buffering: buffering consumes this edge and
        // it is the last Paused/VoidPending edge the pipeline will emit.
        assert_eq!(
            sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)),
            StateChangeResult::Waiting
        );
        assert_eq!(sm.current_state, gs!(Paused));
        // Completing buffering must therefore dispatch the seek itself:
        // parking would wait for a Paused edge that already passed.
        assert_eq!(
            sm.buffering(100),
            BufferingStateResult::FinishedWithSeek(Seek::new(Some(THIRTY), Some(1.0))),
            "queued async seek was silently dropped when Paused arrived mid-buffer",
        );
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
    }

    #[test]
    fn buffering_percent_above_100_completes() {
        // Completion is `>= 100`: an aggregated buffering message reporting
        // over 100 percent must not strand the machine in Buffering.
        let mut sm = playing();
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(
            sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)),
            StateChangeResult::Waiting
        );
        assert_eq!(
            sm.buffering(105),
            BufferingStateResult::Finished(Some(gs!(Playing)))
        );
        assert!(!matches!(sm.probe().0, Phase::Buffering { .. }));
    }

    #[test]
    #[rustfmt::skip]
    fn pause_during_load_survives_buffering() {
        // Regression (FAST cast_pause_during_load_v2): a Pause arriving right
        // after PlayNew, before the load prerolls. The Loading phase must
        // record the requested transport and buffering must converge on it,
        // not on a hardcoded Playing.
        let mut sm = StateMachine::new();
        sm.begin_load();
        // Teardown echo of the load's READY reset.
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        // The user pauses while the load is still in flight: recorded.
        assert_eq!(sm.set_playback_state(rs!(Paused)), None);
        assert_eq!(sm.probe().1, gs!(Paused));
        // use-buffering posts its cycle during the preroll.
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        // Completion must drive toward the RECORDED pause, not Playing.
        assert_eq!(sm.buffering(100), BufferingStateResult::Finished(Some(gs!(Paused))));
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.running(), Some(rs!(Paused)));
    }

    #[test]
    #[rustfmt::skip]
    fn begin_load_resets_the_tracked_pipeline_state() {
        // Regression (FAST cast_pause_during_load_v2, round 2): with the
        // superseded item's state edges generation-filtered, the machine
        // enters a load still holding the PREVIOUS item's last state. A stale
        // Playing makes buffering completion see target == current and settle
        // Running(Playing) WITHOUT dispatching, parking the load in PAUSED.
        let mut sm = playing();
        assert_eq!(sm.current_state, gs!(Playing), "stale state from the previous item");
        sm.begin_load();
        assert_eq!(sm.current_state, gs!(Ready), "must adopt the load's READY reset");
        // Buffering completes before any of the new load's edges arrive
        // (preroll is async): completion must DISPATCH toward the target.
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::Finished(Some(gs!(Playing))));
        assert_eq!(sm.running(), None, "nothing settled until the pipeline reports it");
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
    }

    #[test]
    #[rustfmt::skip]
    fn load_without_pause_still_buffers_toward_playing() {
        // The counterpart: an untouched load keeps auto-play semantics, so a
        // rebuffer mid-preroll still converges on Playing.
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.buffering(100), BufferingStateResult::Finished(Some(gs!(Playing))));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
    }

    #[test]
    fn seek_position_guard_matches_clocktime_panic_boundary() {
        for bad in [0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!Seek::rate_is_safe(bad), "rate {bad} should be rejected");
        }
        for ok in [1.0, 2.0, 0.5, -1.0] {
            assert!(Seek::rate_is_safe(ok), "rate {ok} should be accepted");
        }
    }

    #[test]
    fn queue_seek_preserves_playing_target() {
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(FIVE), Some(1.0)));
        assert_eq!(
            sm.probe().1,
            gst::State::Playing,
            "queue_seek while playing must keep target Playing",
        );
        let mut sm = paused();
        sm.queue_seek(Seek::new(Some(FIVE), Some(1.0)));
        assert_eq!(sm.probe().1, gst::State::Paused);
    }

    #[test]
    fn seek_failed_while_paused_must_not_hang() {
        let mut sm = paused();
        assert_eq!(
            sm.seek_internal(Seek::new(None, Some(1.5)), None),
            Some(Seek::new(None, Some(1.5)))
        );
        assert_eq!(sm.probe().2, SeekSlot::InFlight);
        let resume = sm.seek_failed();
        assert_eq!(
            sm.running(),
            Some(RunningState::Paused),
            "seek_failed left a state that can never complete: resume={resume:?}",
        );
    }

    /// `clear_state` post-conditions from a machine mid-seek: phase Stopped,
    /// seek slot empty, target back to Paused, live flag and rate reset. A
    /// cleared machine refuses the next seek like any stopped one.
    #[test]
    fn clear_state_from_mid_seek_resets_the_whole_model() {
        let mut sm = playing();
        assert_eq!(
            sm.seek_internal(Seek::new(Some(TEN), Some(2.0)), None),
            Some(Seek::new(Some(TEN), Some(2.0)))
        );
        sm.is_live = true;
        sm.rate = 2.0;
        assert_eq!(sm.probe().2, SeekSlot::InFlight);

        sm.clear_state();
        assert_eq!(sm.probe(), (Phase::Stopped, gs!(Paused), SeekSlot::None));
        assert!(sm.is_stopped());
        assert!(!sm.is_seeking());
        assert_eq!(sm.running(), None);
        assert!(!sm.is_live);
        assert_eq!(sm.rate, 1.0);
        assert_eq!(sm.seek_internal(Seek::new(Some(FIVE), None), None), None);
    }

    /// `clear_state` while buffering is latched: the latch is gone, so a
    /// later lone completion report is a no-op instead of a settle acting on
    /// dead bookkeeping.
    #[test]
    fn clear_state_drops_a_latched_buffering() {
        let mut sm = playing();
        assert_eq!(sm.buffering(37), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(sm.probe().0, Phase::Buffering { .. }));

        sm.clear_state();
        assert_eq!(sm.probe(), (Phase::Stopped, gs!(Paused), SeekSlot::None));
        assert_eq!(sm.buffering(100), BufferingStateResult::Buffering);
        assert!(sm.is_stopped(), "the stray completion settled nothing");
    }

    /// `clear_state` over a committed load: the Playing commit is forgotten
    /// with the load, not carried into the next item's target.
    #[test]
    fn clear_state_forgets_a_pending_load_commit() {
        let mut sm = StateMachine::new();
        sm.begin_load();
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.probe().0, Phase::Loading { committed: true });
        assert_eq!(sm.probe().1, gs!(Playing));

        sm.clear_state();
        assert_eq!(sm.probe(), (Phase::Stopped, gs!(Paused), SeekSlot::None));
    }

    /// A failed in-flight seek whose target the pipeline has NOT reached
    /// re-commits toward that target instead of settling in place.
    #[test]
    fn seek_failed_with_an_unreached_target_recommits_it() {
        let mut sm = paused();
        assert_eq!(
            sm.seek_internal(Seek::new(Some(TEN), None), Some(gs!(Playing))),
            Some(Seek::new(Some(TEN), Some(1.0)))
        );
        assert_eq!(sm.probe().1, gs!(Playing));
        assert_eq!(sm.probe().2, SeekSlot::InFlight);

        assert_eq!(sm.seek_failed(), Some(gs!(Playing)));
        assert_eq!(sm.probe(), (Phase::Changing, gs!(Playing), SeekSlot::None));
        // The re-commit settles like any transition.
        assert_eq!(
            sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)),
            new_ps!(Playing)
        );
    }

    /// A failure with a seek parked BEHIND the in-flight one (not buffering)
    /// drops the parked seek too: same mechanics, it would fail the same way.
    /// Nothing may dispatch it afterwards.
    #[test]
    fn seek_failed_drops_the_seek_parked_behind_it() {
        let mut sm = paused();
        assert_eq!(
            sm.seek_internal(Seek::new(Some(TEN), None), None),
            Some(Seek::new(Some(TEN), Some(1.0)))
        );
        assert_eq!(sm.seek_internal(Seek::new(Some(THIRTY), None), None), None);
        assert_eq!(
            sm.probe().2,
            SeekSlot::InFlightParked(Seek::new(Some(THIRTY), Some(1.0)))
        );

        assert_eq!(sm.seek_failed(), None);
        assert_eq!(sm.probe().2, SeekSlot::None);
        assert_eq!(sm.running(), Some(rs!(Paused)));
        // The settled-Paused edge that would have dispatched the parked seek
        // finds nothing to dispatch.
        assert_eq!(
            sm.state_changed(gs!(Paused), gs!(Paused), gs!(VoidPending)),
            new_ps!(Paused)
        );
    }

    /// While playing, a failed seek settles back to Running(Playing): the
    /// target and the current state agree, so nothing is re-committed.
    #[test]
    fn seek_failed_while_playing_settles_back_to_playing() {
        let mut sm = playing();
        assert_eq!(
            sm.seek_internal(Seek::new(Some(TEN), None), None),
            Some(Seek::new(Some(TEN), Some(1.0)))
        );
        assert_eq!(sm.seek_failed(), None);
        assert_eq!(sm.running(), Some(rs!(Playing)));
    }

    /// With no seek tracked at all, `seek_failed` is a no-op: a stale failure
    /// report must not disturb a settled machine.
    #[test]
    fn seek_failed_with_no_tracked_seek_changes_nothing() {
        let mut sm = paused();
        let before = sm.probe();
        assert_eq!(sm.seek_failed(), None);
        assert_eq!(sm.probe(), before);
    }
}
