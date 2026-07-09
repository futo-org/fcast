use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use fcast_protocol::PlaybackState;
use gst::{glib::object::ObjectExt, prelude::*};
use tracing::{debug, debug_span, error, instrument, warn};

use crate::MessageSender;

mod subtitles;

pub use subtitles::{LoadKind, RestorePoint, SubtitlePhase, TextIntent, TimerAction, TimerEvent};

struct BoolLock(bool);

impl BoolLock {
    pub fn new() -> Self {
        Self(false)
    }

    pub fn acquire(&mut self) {
        self.0 = true;
    }

    pub fn release(&mut self) {
        self.0 = false;
    }

    pub fn is_locked(&self) -> bool {
        self.0
    }
}

struct PlaybinFlags {}

impl PlaybinFlags {
    const AUDIO_AND_VIDEO: &'static str = "buffering+soft-volume+text+audio+video";
    /// Used while (re)loading ANY media: with the text flag off, playbin3
    /// never routes a text stream into playsink mid-preroll — which can
    /// wedge the preroll (external subs reliably, embedded subs as a rare
    /// subtitleoverlay reconfigure livelock) — and the text branch is built
    /// exactly once, after the pipeline settles and `Job::EnableText`
    /// restores the full flags.
    const AUDIO_AND_VIDEO_NO_TEXT: &'static str = "buffering+soft-volume+audio+video";
    const AUDIO_ONLY: &'static str = "soft-volume+audio";
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PlayerState {
    Paused,
    Playing,
    Buffering,
    Stopped,
}

impl PlayerState {
    pub fn as_fcast_v4(&self) -> fcast_protocol::v4::PlaybackState {
        use fcast_protocol::v4;
        match self {
            PlayerState::Paused => v4::PlaybackState::Paused,
            PlayerState::Playing => v4::PlaybackState::Playing,
            PlayerState::Buffering => v4::PlaybackState::Buffering,
            PlayerState::Stopped => v4::PlaybackState::Idle,
        }
    }
}

type StreamId = String;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RunningState {
    Paused,
    Playing,
}

impl From<&mut RunningState> for gst::State {
    fn from(value: &mut RunningState) -> Self {
        match value {
            RunningState::Paused => gst::State::Paused,
            RunningState::Playing => gst::State::Playing,
        }
    }
}

impl From<RunningState> for gst::State {
    fn from(mut value: RunningState) -> Self {
        Self::from(&mut value)
    }
}

#[derive(Debug, PartialEq)]
enum PendingSeek {
    Async(Seek),
    Waiting,
}

#[derive(Debug, PartialEq)]
enum State {
    Stopped,
    PendingUriChange,
    Buffering {
        percent: i32,
        target_state: gst::State,
        pending_seek: Option<PendingSeek>,
    },
    Changing {
        target_state: gst::State,
        pending_seek: Option<Seek>,
    },
    SeekAsync {
        seek: Seek,
        target_state: gst::State,
    },
    Seeking {
        target_state: gst::State,
    },
    Running {
        state: RunningState,
    },
}

#[derive(Debug, PartialEq)]
pub enum StateChangeResult {
    NewPlaybackState(PlaybackState),
    Seek(Seek),
    Waiting,
    ChangeState(gst::State),
}

#[derive(Debug, PartialEq)]
enum BufferingStateResult {
    Started(gst::State),
    Buffering,
    /// Buffering finished with a pending async seek, but the pipeline has *already* settled at
    /// `Paused`, so the `Paused` edge `SeekAsync` waits for will never arrive again. The seek must
    /// be dispatched immediately (caller sends `Job::Seek`); the machine is left in `Seeking`.
    FinishedWithSeek(Seek),
    /// Buffering finished but a seek is still parked (`SeekAsync`, waiting for the pipeline to
    /// reach `Paused`) or already in flight (`Seeking`). Nothing to dispatch here.
    FinishedButWaitingSeek,
    Finished(Option<gst::State>),
}

#[derive(Debug)]
struct StateMachine {
    current_state: gst::State,
    pub state: State,
    pub is_live: bool,
    position: Option<gst::ClockTime>,
    pub rate: f64,
    pub seekable: bool,
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            current_state: gst::State::Ready,
            state: State::Stopped,
            is_live: false,
            position: None,
            rate: 1.0,
            seekable: false,
        }
    }

    fn queue_seek(&mut self, seek: Seek) {
        let target_state = match self.state {
            State::Buffering { target_state, .. }
            | State::Changing { target_state, .. }
            | State::SeekAsync { target_state, .. }
            | State::Seeking { target_state, .. } => target_state,
            State::Running { state } => state.into(),
            _ => gst::State::Paused,
        };

        self.state = State::SeekAsync { seek, target_state };
    }

    #[must_use]
    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    fn seek_internal(&mut self, mut seek: Seek, target_state: Option<gst::State>) -> Option<Seek> {
        if self.is_live {
            warn!("Cannot seek when source is live");
            return None;
        } else if self.state == State::Stopped {
            warn!("Cannot seek when not playing");
            return None;
        }

        debug!(?seek, state = ?self.state, current_state = ?self.current_state);

        if seek.rate.is_none() {
            seek.rate = Some(self.rate as f32);
        }

        let target_state = if let Some(ts) = target_state {
            ts
        } else {
            match self.state {
                State::Running {
                    state: RunningState::Playing,
                } => gst::State::Playing,
                State::Changing { target_state, .. }
                | State::SeekAsync { target_state, .. }
                | State::Buffering { target_state, .. } => target_state,
                _ => gst::State::Paused,
            }
        };

        match &mut self.state {
            State::SeekAsync {
                seek: prev_seek, ..
            } => {
                warn!("Cannot seek because a seek request is pending");
                prev_seek.position = seek.position;
                prev_seek.rate = seek.rate;
                None
            }
            State::Seeking { .. } => {
                warn!("Cannot seek because a seek request is pending");
                None
            }
            State::Buffering { pending_seek, .. } if pending_seek.is_some() => {
                if let Some(pending_seek) = pending_seek.as_mut()
                    && let PendingSeek::Async(pending_seek) = pending_seek
                {
                    pending_seek.position = seek.position;
                    pending_seek.rate = seek.rate;
                }
                None
            }
            _ => {
                self.state = State::Seeking { target_state };
                Some(seek)
            }
        }
    }

    fn is_seeking(&self) -> bool {
        matches!(
            self.state,
            State::SeekAsync { .. }
                | State::Seeking { .. }
                | State::Buffering {
                    pending_seek: Some(_),
                    ..
                }
        )
    }

    #[must_use]
    fn set_playback_state(&mut self, state: RunningState) -> Option<gst::State> {
        let next_state: gst::State = state.into();
        match &mut self.state {
            State::Stopped => {
                error!("Cannot set playback state when the player is stopped");
                return None;
            }
            State::PendingUriChange => (),
            State::Buffering { target_state, .. }
            | State::Changing { target_state, .. }
            | State::SeekAsync { target_state, .. }
            | State::Seeking { target_state, .. } => *target_state = next_state,
            State::Running {
                state: current_state,
            } => {
                if *current_state != state {
                    self.state = State::Changing {
                        target_state: next_state,
                        pending_seek: None,
                    };
                    return Some(next_state);
                }
            }
        }

        None
    }

    #[must_use]
    fn buffering(&mut self, new_percent: i32) -> BufferingStateResult {
        if new_percent >= 100 && !matches!(self.state, State::Buffering { .. }) {
            return BufferingStateResult::Buffering;
        }

        match &mut self.state {
            State::Stopped | State::PendingUriChange => {
                self.state = State::Buffering {
                    percent: new_percent,
                    target_state: gst::State::Playing,
                    pending_seek: None,
                };
            }
            State::SeekAsync {
                seek, target_state, ..
            } => {
                self.state = State::Buffering {
                    percent: new_percent,
                    target_state: *target_state,
                    pending_seek: Some(PendingSeek::Async(*seek)),
                };
            }
            State::Buffering {
                percent,
                target_state,
                pending_seek,
            } => {
                *percent = new_percent;
                if new_percent == 100 {
                    debug!("Buffering completed");
                    let target = *target_state;
                    if let Some(_seek) = pending_seek {
                        match _seek {
                            PendingSeek::Async(seek) => {
                                let seek = *seek;
                                // `SeekAsync` fires its seek on the next `Paused/VoidPending`
                                // state-change edge. If the pipeline has already reached `Paused`
                                // (buffering completed *after* the async state change settled),
                                // that edge is in the past and GStreamer won't repeat it — parking
                                // in `SeekAsync` would stall forever. Dispatch the seek now instead.
                                if self.current_state == gst::State::Paused {
                                    self.state = State::Seeking {
                                        target_state: target,
                                    };
                                    return BufferingStateResult::FinishedWithSeek(seek);
                                }
                                self.state = State::SeekAsync {
                                    target_state: target,
                                    seek,
                                };
                            }
                            PendingSeek::Waiting => {
                                self.state = State::Seeking {
                                    target_state: target,
                                };
                            }
                        }
                        return BufferingStateResult::FinishedButWaitingSeek;
                    }

                    if target != self.current_state {
                        self.state = State::Changing {
                            target_state: target,
                            pending_seek: None,
                        };
                        return BufferingStateResult::Finished(Some(target));
                    } else {
                        match target {
                            gst::State::VoidPending | gst::State::Null | gst::State::Ready => {
                                self.state = State::Stopped;
                            }
                            gst::State::Paused => {
                                self.state = State::Running {
                                    state: RunningState::Paused,
                                };
                            }
                            gst::State::Playing => {
                                self.state = State::Running {
                                    state: RunningState::Playing,
                                };
                            }
                        }
                        return BufferingStateResult::Finished(None);
                    }
                }
                return BufferingStateResult::Buffering;
            }
            State::Changing {
                target_state,
                pending_seek,
            } => {
                self.state = State::Buffering {
                    percent: new_percent,
                    target_state: *target_state,
                    pending_seek: pending_seek.as_mut().map(|seek| PendingSeek::Async(*seek)),
                };
            }
            State::Seeking { target_state } => {
                self.state = State::Buffering {
                    percent: new_percent,
                    target_state: *target_state,
                    pending_seek: Some(PendingSeek::Waiting),
                };
            }
            State::Running { state } => {
                self.state = State::Buffering {
                    percent: new_percent,
                    target_state: state.into(),
                    pending_seek: None,
                };
            }
        }

        BufferingStateResult::Started(gst::State::Paused)
    }

    #[must_use]
    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    pub fn state_changed(
        &mut self,
        _old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> StateChangeResult {
        debug!(?new, ?pending, state = ?self.state, "State changed");
        self.current_state = new;

        match &mut self.state {
            State::Stopped | State::PendingUriChange => {
                if matches!(pending, gst::State::Ready | gst::State::Null) {
                    return StateChangeResult::Waiting;
                }

                match new {
                    gst::State::Paused => {
                        self.state = State::Running {
                            state: RunningState::Paused,
                        };
                        StateChangeResult::NewPlaybackState(PlaybackState::Paused)
                    }
                    gst::State::Playing => {
                        self.state = State::Running {
                            state: RunningState::Playing,
                        };
                        StateChangeResult::NewPlaybackState(PlaybackState::Playing)
                    }
                    // TODO: idle?
                    _ => StateChangeResult::Waiting,
                }
            }
            State::Buffering { pending_seek, .. } => {
                if new == gst::State::Paused
                    && pending == gst::State::VoidPending
                    && matches!(pending_seek, Some(PendingSeek::Waiting))
                {
                    *pending_seek = None;
                }

                StateChangeResult::Waiting
            }
            State::Changing {
                target_state,
                pending_seek,
            } => {
                // Reaching Paused while the pipeline is still committed
                // upward to Playing is NOT arrival, even when Paused is the
                // target: a stale upward transition is in flight (e.g. the
                // load's original Playing commit arriving after a user Pause
                // retargeted this change). Settling into Running here let
                // the overshoot flip the machine to Playing, and the
                // deferred start seek then cemented it — un-pausing the
                // user. Wait for the overshoot instead; the `pending ==
                // VoidPending` branch below then issues the correction.
                let stale_upward = new == gst::State::Paused && pending == gst::State::Playing;
                if new == *target_state && !stale_upward {
                    if let Some(seek) = pending_seek.as_ref() {
                        let seek = *seek;
                        let target_state = *target_state;
                        if let Some(seek) = self.seek_internal(seek, Some(target_state)) {
                            return StateChangeResult::Seek(seek);
                        }
                    }
                    match new {
                        gst::State::VoidPending | gst::State::Null | gst::State::Ready => {
                            self.state = State::Stopped;
                            return StateChangeResult::NewPlaybackState(PlaybackState::Idle);
                        }
                        gst::State::Paused => {
                            self.state = State::Running {
                                state: RunningState::Paused,
                            };
                            return StateChangeResult::NewPlaybackState(PlaybackState::Paused);
                        }
                        gst::State::Playing => {
                            self.state = State::Running {
                                state: RunningState::Playing,
                            };
                            return StateChangeResult::NewPlaybackState(PlaybackState::Playing);
                        }
                    }
                } else if pending == gst::State::VoidPending {
                    return StateChangeResult::ChangeState(*target_state);
                }

                StateChangeResult::Waiting
            }
            State::SeekAsync { seek, target_state } => {
                if new == gst::State::Paused && pending == gst::State::VoidPending {
                    let seek = *seek;
                    self.state = State::Seeking {
                        target_state: *target_state,
                    };

                    return StateChangeResult::Seek(seek);
                }

                StateChangeResult::Waiting
            }
            State::Seeking { target_state, .. } => {
                if new == gst::State::Paused && pending == gst::State::VoidPending {
                    let target = *target_state;
                    if new != target {
                        self.state = State::Changing {
                            target_state: target,
                            pending_seek: None,
                        };
                        debug!(state = ?self.state, "Seek completed");
                        return StateChangeResult::ChangeState(target);
                    } else {
                        match new {
                            gst::State::Paused => {
                                self.state = State::Running {
                                    state: RunningState::Paused,
                                };
                                return StateChangeResult::NewPlaybackState(PlaybackState::Paused);
                            }
                            gst::State::Playing => {
                                self.state = State::Running {
                                    state: RunningState::Playing,
                                };
                                return StateChangeResult::NewPlaybackState(PlaybackState::Playing);
                            }
                            _ => {
                                self.state = State::Stopped;
                                return StateChangeResult::NewPlaybackState(PlaybackState::Idle);
                            }
                        }
                    }
                }

                StateChangeResult::Waiting
            }
            State::Running { .. } => match (new, pending) {
                (gst::State::VoidPending | gst::State::Null | gst::State::Ready, _)
                | (_, gst::State::Null) => {
                    self.state = State::Stopped;
                    StateChangeResult::NewPlaybackState(PlaybackState::Idle)
                }
                (gst::State::Paused, _) => {
                    self.state = State::Running {
                        state: RunningState::Paused,
                    };
                    StateChangeResult::NewPlaybackState(PlaybackState::Paused)
                }
                (gst::State::Playing, _) => {
                    self.state = State::Running {
                        state: RunningState::Playing,
                    };
                    StateChangeResult::NewPlaybackState(PlaybackState::Playing)
                }
            },
        }
    }

    fn seek_failed(&mut self) -> Option<gst::State> {
        match self.state {
            State::Seeking { target_state } => {
                if target_state == self.current_state {
                    self.state = match target_state {
                        gst::State::Playing => State::Running {
                            state: RunningState::Playing,
                        },
                        gst::State::Paused => State::Running {
                            state: RunningState::Paused,
                        },
                        _ => State::Stopped,
                    };
                    None
                } else {
                    self.state = State::Changing {
                        target_state,
                        pending_seek: None,
                    };
                    Some(target_state)
                }
            }
            _ => None,
        }
    }

    fn clear_state(&mut self) {
        self.state = State::Stopped;
        self.is_live = false;
        self.position = None;
        self.rate = 1.0;
        self.seekable = false;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Seek {
    pub position: Option<gst::ClockTime>,
    pub rate: Option<f32>,
}

impl Seek {
    pub fn new(position: Option<gst::ClockTime>, rate: Option<f32>) -> Self {
        Self { position, rate }
    }

    fn rate_is_safe(rate: f32) -> bool {
        rate.is_finite() && rate != 0.0
    }
}

/// Which stream slot a track-change request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// A full track selection: stream-list indices, `None` meaning disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSelection {
    pub video: Option<u32>,
    pub audio: Option<u32>,
    pub subtitle: Option<u32>,
}

/// What `TrackOps::pump` decided to dispatch next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackOpCommand {
    SelectStreams(TrackSelection),
    RefreshSeek,
}

/// Pipeline conditions `TrackOps::pump` dispatches under (a snapshot taken by
/// `Player::pump_track_ops`).
#[derive(Debug, Clone, Copy)]
struct TrackOpCtx {
    /// No async state change in progress and the state machine is settled in
    /// `Running` (not buffering/seeking/changing).
    quiet: bool,
    /// Settled in `Running { Paused }`: the streaming threads are parked after
    /// preroll, so a dispatched selection won't apply (or confirm) until data
    /// flows again.
    paused: bool,
    /// The selection currently applied (or optimistically in flight).
    applied: TrackSelection,
}

/// How many extra flushing seeks a paused subtitle refresh may issue when the
/// previous one finished prerolling without a cue being rendered. Best-effort:
/// each flush races the paused subtitle branch's activation, and each retry is
/// another attempt that usually lands (see `Player::async_done`).
const SUBTITLE_REFRESH_RETRIES: u32 = 3;
/// Watchdog for an in-flight track operation whose completion signal never
/// arrives. Not load-bearing -- completions are seqnum-matched -- this only
/// stops a pathologically lost message from wedging the queue forever.
const TRACK_OP_WATCHDOG: Duration = Duration::from_secs(5);
/// How long the subtitle re-emit flush holds off when a bitmap subtitle
/// (`subpicture/*`) is involved in the switch: playsink's video-chain
/// rebuild for the bitmap renderer is signal-less, and a flush inside it
/// deadlocks the pipeline (observed errors landed within ~100ms of the
/// selection confirm; this is ~15x that, plus the poll tick on top).
const SUBPICTURE_REFRESH_DELAY: Duration = Duration::from_millis(1500);

/// Serialized track selection and subtitle refresh.
///
/// GStreamer confirms a `SELECT_STREAMS` with a `STREAMS_SELECTED` message
/// carrying the *event's* seqnum (decodebin3 stamps it), so selections are
/// settled by exact seqnum match. A (refresh) seek completes with a top-level
/// `ASYNC_DONE`, but that one CANNOT be seqnum-matched: `GstBin` builds its
/// aggregated ASYNC_DONE with a fresh seqnum (`bin_handle_async_done` never
/// copies one). The refresh is instead settled by exclusivity: this struct
/// keeps at most one async-causing operation in flight, so an ASYNC_DONE
/// arriving while the refresh is out is its completion (and a rare
/// mis-attribution, e.g. a racing user seek, only settles it early -- the
/// quiet check still gates the next dispatch). New work is held back until
/// the pipeline is quiet -- overlapping playsink re-prerolls deadlock the
/// pipeline, which is the failure mode all of this exists to prevent.
///
/// Requests are latest-wins: while an operation is in flight only the newest
/// composed selection is remembered (`pending`), dispatched once the pipeline
/// settles.
///
/// Paused is special (streaming threads are parked after preroll): a
/// dispatched selection won't confirm until data flows, so a parked selection
/// neither blocks a superseding one (there is no re-preroll to overlap with)
/// nor blocks the refresh flush -- that flush is exactly what makes data flow
/// and the selection apply.
#[derive(Debug)]
struct TrackOps {
    /// Latest desired selection not yet dispatched.
    pending: Option<TrackSelection>,
    /// In-flight `SELECT_STREAMS`: seqnum to match `STREAMS_SELECTED`, and the
    /// dispatch time for the watchdog.
    selecting: Option<(gst::Seqnum, Instant)>,
    /// In-flight subtitle refresh seek. Settled by the next `ASYNC_DONE`
    /// (attribution by exclusivity -- see the struct docs); the seqnum only
    /// matches the job's failure report and the logs.
    refreshing: Option<(gst::Seqnum, Instant)>,
    /// A subtitle re-emit flush is due once the pipeline settles: a sparse
    /// text track doesn't render its current cue after a switch until the next
    /// cue boundary, so a flushing seek to the current position re-emits it.
    refresh_wanted: bool,
    refresh_retries_left: u32,
    /// A subtitle overlay was rendered since the refresh was dispatched (fed
    /// by the sink via `PlayerEvent::SubtitleOverlayShown`); stops the paused
    /// retry loop.
    overlay_seen: bool,
    /// The latest subtitle request forbade its re-emit flush: while an
    /// external subtitle (`suburi`) is attached to the play item, ANY flush
    /// races the text-branch reconfiguration and errors the suburi source
    /// (pipeline error with the subtitle URL as its `failed_uri`), freezing
    /// the whole play item — the new track's cue appears at its next cue
    /// boundary instead. Re-decided by every subtitle request (see
    /// `suppress_refresh`), cleared by `reset`.
    refresh_suppressed: bool,
    /// The re-emit flush must wait this long past the first moment it could
    /// have dispatched (bitmap subtitle involved — see
    /// `Player::request_track_change_impl`); converted to
    /// `refresh_not_before` when the pump first reaches the refresh.
    refresh_delay: Option<Duration>,
    /// The deferred re-emit flush may not dispatch before this instant.
    refresh_not_before: Option<Instant>,
}

impl TrackOps {
    fn new() -> Self {
        Self {
            pending: None,
            selecting: None,
            refreshing: None,
            refresh_wanted: false,
            refresh_retries_left: 0,
            overlay_seen: false,
            refresh_suppressed: false,
            refresh_delay: None,
            refresh_not_before: None,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    /// Compose a single-slot change onto the latest desired selection.
    fn request(&mut self, kind: TrackKind, id: Option<u32>, applied: TrackSelection) {
        let mut desired = self.pending.unwrap_or(applied);
        match kind {
            TrackKind::Video => desired.video = id,
            TrackKind::Audio => desired.audio = id,
            TrackKind::Subtitle => {
                desired.subtitle = id;
                // Each subtitle request re-decides whether its re-emit flush
                // is allowed and how long it must hold off; `suppress_refresh`
                // and `defer_refresh` re-apply after this call.
                self.refresh_suppressed = false;
                self.refresh_delay = None;
                self.refresh_not_before = None;
            }
        }
        self.pending = Some(desired);
    }

    /// Forbid the re-emit flush for the subtitle selection just composed by
    /// `request` (external suburi attached; see `refresh_suppressed`).
    fn suppress_refresh(&mut self) {
        self.refresh_suppressed = true;
    }

    /// Hold the re-emit flush back for `delay` past the first moment it
    /// could have dispatched (selection confirmed, pipeline quiet). Used
    /// when a bitmap subtitle is involved: the flush must not race
    /// playsink's video-chain rebuild, which is signal-less — time distance
    /// is the only available guard.
    fn defer_refresh(&mut self, delay: Duration) {
        self.refresh_delay = Some(delay);
    }

    /// Drop an in-flight operation whose completion never arrived. A parked
    /// paused selection is expected to wait indefinitely and is exempt.
    fn run_watchdog(&mut self, paused: bool) {
        if let Some((seqnum, at)) = self.selecting
            && !paused
            && at.elapsed() > TRACK_OP_WATCHDOG
        {
            warn!(?seqnum, "Track selection was never confirmed; dropping it");
            self.selecting = None;
        }
        if let Some((seqnum, at)) = self.refreshing
            && at.elapsed() > TRACK_OP_WATCHDOG
        {
            warn!(?seqnum, "Subtitle refresh seek never finished; dropping it");
            self.refreshing = None;
            // The overlay was already cleared for this switch; without the
            // re-emit the cue stays gone until the next boundary. Re-queue it
            // from the bounded retry budget rather than eating it.
            self.maybe_retry_refresh();
        }
    }

    /// Decide the next operation to dispatch, if the pipeline allows one.
    fn pump(&mut self, ctx: TrackOpCtx) -> Option<TrackOpCommand> {
        if !ctx.quiet {
            return None;
        }
        // A refresh flush is an async re-preroll; never dispatch on top of it.
        if self.refreshing.is_some() {
            return None;
        }
        // An unconfirmed selection blocks new work while data flows (its
        // playsink reconfigure may still be about to re-preroll). While paused
        // it is merely parked: superseding it, or flushing past it, is safe --
        // nothing is re-prerolling.
        if self.selecting.is_some() && !ctx.paused {
            return None;
        }

        if let Some(desired) = self.pending.take()
            && desired != ctx.applied
        {
            if desired.subtitle != ctx.applied.subtitle {
                if desired.subtitle.is_some() && !self.refresh_suppressed {
                    // Enable/switch: re-emit the new track's current cue once
                    // the selection settles.
                    self.refresh_wanted = true;
                    self.refresh_retries_left = SUBTITLE_REFRESH_RETRIES;
                } else {
                    // Suppressed (external suburi attached), or:
                    // Disable: no flush (flushing right after the text-branch
                    // teardown can fail allocation renegotiation, observed
                    // with vavp8dec: "DMABuf caps negotiated without
                    // VideoMeta" -> not-negotiated).
                    self.refresh_wanted = false;
                }
            }
            return Some(TrackOpCommand::SelectStreams(desired));
        }

        if self.refresh_wanted {
            // A deferred refresh (bitmap subtitle involved) starts its clock
            // at the first moment it could otherwise have dispatched —
            // selection confirmed and pipeline quiet — so the hold-off
            // always spans the rebuild that confirmation triggers, however
            // long the selection itself was parked.
            if let Some(delay) = self.refresh_delay.take() {
                self.refresh_not_before = Some(Instant::now() + delay);
                return None;
            }
            if let Some(not_before) = self.refresh_not_before {
                if Instant::now() < not_before {
                    return None;
                }
                self.refresh_not_before = None;
            }
            self.refresh_wanted = false;
            self.overlay_seen = false;
            return Some(TrackOpCommand::RefreshSeek);
        }

        None
    }

    fn selection_dispatched(&mut self, seqnum: gst::Seqnum) {
        self.selecting = Some((seqnum, Instant::now()));
    }

    fn refresh_dispatched(&mut self, seqnum: gst::Seqnum) {
        self.refreshing = Some((seqnum, Instant::now()));
    }

    /// A `STREAMS_SELECTED` arrived; settles the in-flight selection iff it is
    /// ours (decodebin3 stamps the message with the SELECT_STREAMS seqnum).
    fn streams_selected(&mut self, seqnum: gst::Seqnum) {
        if let Some((expected, _)) = self.selecting
            && expected == seqnum
        {
            self.selecting = None;
        }
    }

    /// A top-level `ASYNC_DONE` arrived; returns whether it finished our
    /// refresh seek. Attribution is by exclusivity, not seqnum: `GstBin` posts
    /// its aggregated ASYNC_DONE with a fresh seqnum, and this queue never has
    /// more than one async-causing operation out.
    fn refresh_done(&mut self) -> bool {
        self.refreshing.take().is_some()
    }

    fn refresh_failed(&mut self, seqnum: gst::Seqnum) {
        if let Some((expected, _)) = self.refreshing
            && expected == seqnum
        {
            self.refreshing = None;
        }
    }

    /// The completed refresh didn't render a cue; queue another flush if the
    /// budget allows. Returns whether a retry was queued.
    fn maybe_retry_refresh(&mut self) -> bool {
        if !self.overlay_seen && self.refresh_retries_left > 0 {
            self.refresh_retries_left -= 1;
            self.refresh_wanted = true;
            true
        } else {
            false
        }
    }

    /// The sink rendered a frame carrying subtitle overlays. Counts as *our*
    /// cue only while a refresh is in flight and the selection has been
    /// confirmed -- before confirmation, a not-yet-switched branch can still
    /// render the *old* track's cue.
    fn overlay_shown(&mut self) {
        if self.selecting.is_none() && self.refreshing.is_some() {
            self.overlay_seen = true;
        }
    }

    /// A user-initiated flushing seek re-emits the current cue by itself; a
    /// separately queued refresh flush would be redundant.
    fn cancel_refresh(&mut self) {
        self.refresh_wanted = false;
        self.refresh_delay = None;
        self.refresh_not_before = None;
    }

    fn has_dispatchable_work(&self) -> bool {
        self.pending.is_some() || self.refresh_wanted
    }

    /// Anything queued or still in flight (unconfirmed selection or refresh
    /// seek).
    fn is_busy(&self) -> bool {
        self.has_dispatchable_work() || self.selecting.is_some() || self.refreshing.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaErrorKind {
    NotFound,
    NotAuthorized,
    UnsupportedFormat,
    Other,
}

impl MediaErrorKind {
    fn from_glib_error(err: &gst::glib::Error) -> Self {
        if let Some(err) = err.kind::<gst::ResourceError>() {
            match err {
                gst::ResourceError::NotFound => Self::NotFound,
                gst::ResourceError::NotAuthorized => Self::NotAuthorized,
                _ => Self::Other,
            }
        } else if let Some(err) = err.kind::<gst::StreamError>() {
            match err {
                gst::StreamError::TypeNotFound
                | gst::StreamError::WrongType
                | gst::StreamError::CodecNotFound
                | gst::StreamError::Decode
                | gst::StreamError::Demux
                | gst::StreamError::Format => Self::UnsupportedFormat,
                _ => Self::Other,
            }
        } else {
            Self::Other
        }
    }
}

#[derive(Debug)]
pub enum PlayerEvent {
    EndOfStream,
    UriLoaded,
    Tags(gst::TagList),
    VolumeChanged(f64),
    /// User must call Player::handle_stream_collection()
    StreamCollection(gst::StreamCollection),
    AboutToFinish,
    /// An async state change or (flushing) seek finished prerolling. Not
    /// attributable to a specific operation: `GstBin` posts its aggregated
    /// ASYNC_DONE with a fresh seqnum (`TrackOps` relies on exclusivity
    /// instead).
    AsyncDone,
    Buffering(i32),
    IsLive,
    StateChanged {
        old: gst::State,
        current: gst::State,
        pending: gst::State,
    },
    /// An element asked the application to change the pipeline state.
    RequestState(gst::State),
    QueueSeek(Seek),
    StreamsSelected {
        video: Option<StreamId>,
        audio: Option<StreamId>,
        subtitle: Option<StreamId>,
        /// Seqnum of the `SELECT_STREAMS` event this confirms (decodebin3
        /// stamps it onto the message).
        seqnum: gst::Seqnum,
    },
    /// The video sink rendered a frame carrying subtitle overlays (posted on
    /// the empty -> non-empty edge, re-armed by every flush).
    SubtitleOverlayShown,
    /// A subtitle refresh seek could not be performed.
    SubtitleRefreshFailed {
        seqnum: gst::Seqnum,
    },
    RateChanged(f64),
    SeekFailed,
    /// The element providing the pipeline clock went away (e.g. the audio
    /// sink after the audio track was deselected). User must call
    /// `Player::recover_clock()`.
    ClockLost,
    Error {
        kind: MediaErrorKind,
        message: String,
        failed_uri: Option<String>,
    },
    Warning(String),
    StreamTagsUpdated,
}

#[derive(Debug)]
enum Job {
    SetState {
        target_state: gst::State,
        feedback: Option<oneshot::Sender<()>>,
    },
    SetUri {
        uri: String,
        suburi: Option<String>,
    },
    Seek(Seek),
    /// A flushing seek to the current position that keeps the pipeline in its
    /// current state. Used only to force a freshly selected sparse subtitle
    /// track to re-render; deliberately bypasses the seek state machine (which
    /// forces a Paused round-trip). Serialized against other track operations
    /// by `TrackOps`; `seqnum` is stamped on the seek event so its ASYNC_DONE
    /// can be attributed exactly.
    RefreshSeek {
        seqnum: gst::Seqnum,
    },
    /// Cycle the pipeline through Paused back to Playing so it elects a new
    /// clock after `ClockLost`. Without this every sink keeps waiting on the
    /// dead clock and playback stalls.
    RecoverClock,
    /// Restore the text playbin flag after a `SetUri` with a suburi disabled
    /// it for the duration of the preroll (see
    /// `PlaybinFlags::AUDIO_AND_VIDEO_NO_TEXT`).
    EnableText,
    Quit,
}

pub fn stream_title(stream: &gst::Stream) -> String {
    let mut res = String::new();
    if let Some(tags) = stream.tags() {
        if let Some(language) = tags.get::<gst::tags::LanguageName>() {
            res += language.get();
        } else if let Some(language) = tags.get::<gst::tags::LanguageCode>() {
            let code = language.get();
            if let Some(lang) = gst_tag::language_codes::language_name(code) {
                res += lang;
            } else {
                res += code;
            }
        }
        if let Some(title) = tags.get::<gst::tags::Title>() {
            if !res.is_empty() {
                res += " - ";
            }
            let title = title.get();
            if !title.is_empty() {
                let mut chars = title.chars();
                res.extend(chars.by_ref().take(16));
                if chars.next().is_some() {
                    res += "...";
                }
            }
        }
    }

    if res.is_empty() {
        res += "Unknown";
    }

    res
}

pub struct Stream {
    pub inner: gst::Stream,
    pub title: String,
}

pub struct Player {
    pub playbin: gst::Element,
    volume_lock: BoolLock,
    work_tx: std::sync::mpsc::Sender<Job>,
    msg_tx: MessageSender,
    /// The text-restore dance for the current load (see `subtitles`).
    subtitles: subtitles::SubtitleDance,
    pub streams: Vec<Stream>,
    pub current_video_stream: Option<u32>,
    pub current_audio_stream: Option<u32>,
    pub current_subtitle_stream: Option<u32>,
    pub seekable: bool,
    /// Whether `seekable` reflects an actual answer from the pipeline. The
    /// seeking query only succeeds once the pipeline can answer it (around
    /// preroll completion), which is well after tracks are first advertised
    /// — until then `seekable == false` merely means "not known yet".
    pub seekable_known: bool,
    /// The newest volume requested while a previous change's confirmation
    /// was still in flight; applied when it arrives (see `set_volume`).
    pending_volume: Option<f32>,
    state_machine: StateMachine,
    track_ops: TrackOps,
    stream_collection: Option<gst::StreamCollection>,
    stream_collection_notify: Option<gst::glib::SignalHandlerId>,
}

impl Player {
    pub fn new(
        video_sink: Option<gst::Element>,
        msg_tx: MessageSender,
        fcomp_context: crate::fcompsrc::imp::CompContext,
        #[cfg(feature = "airplay")] airplay_context: crate::airplay::AirPlayContext,
        // signalling_channel: std::sync::Arc<
        //     parking_lot::Mutex<Option<crate::fwebrtcsrc::SignallingChannel>>,
        // >,
    ) -> Result<Self> {
        let scaletempo = gst::ElementFactory::make("scaletempo").build()?;
        let has_video = video_sink.is_some();
        let playbin = {
            let mut builder =
                gst::ElementFactory::make("playbin3").property("audio-filter", scaletempo);
            if let Some(video_sink) = video_sink {
                builder = builder
                    .property("video-sink", video_sink)
                    .property_from_str("flags", PlaybinFlags::AUDIO_AND_VIDEO);
            } else {
                builder = builder.property_from_str("flags", PlaybinFlags::AUDIO_ONLY);
            }
            builder.build()?
        };

        // Whether decodebin3 may auto-select TEXT streams for the CURRENT
        // load (our `auto-select-text` decodebin3 patch). Plain loads keep
        // text out of the selection entirely until the post-settle restore
        // (any mid-preroll text handling — selected-but-unrouted, or a
        // deselect racing the initial wiring — can wedge the preroll).
        // Suburi loads need the auto-select: the brief selection is what
        // activates the external sub source's play item.
        let text_autoselect = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        // decodebin3 instances live inside uridecodebin3 and are reused
        // across loads; remember them so `Job::SetUri` can re-apply the
        // policy for each load (new instances pick it up on creation).
        let decodebins: std::sync::Arc<parking_lot::Mutex<Vec<gst::glib::WeakRef<gst::Element>>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        {
            fn track_decodebin3(
                element: &gst::Element,
                text_autoselect: &std::sync::atomic::AtomicBool,
                decodebins: &parking_lot::Mutex<Vec<gst::glib::WeakRef<gst::Element>>>,
            ) {
                let is_decodebin3 = element.factory().is_some_and(|f| f.name() == "decodebin3");
                if !is_decodebin3 {
                    return;
                }
                let allow = text_autoselect.load(std::sync::atomic::Ordering::SeqCst);
                debug!(
                    name = %element.name(),
                    allow,
                    "Applying the text auto-select policy to a decodebin3"
                );
                element.set_property("auto-select-text", allow);
                let mut list = decodebins.lock();
                list.retain(|w| w.upgrade().is_some());
                list.push(element.downgrade());
            }

            let bin = playbin
                .downcast_ref::<gst::Bin>()
                .expect("playbin3 is a bin");
            // Future instances (playbin3 normally reuses one, but be safe)…
            let text_autoselect_c = text_autoselect.clone();
            let decodebins_c = decodebins.clone();
            bin.connect_deep_element_added(move |_, _, element| {
                track_decodebin3(element, &text_autoselect_c, &decodebins_c);
            });
            // …and the one built during playbin3's own construction, which
            // predates the signal connection.
            for element in bin
                .iterate_all_by_element_factory_name("decodebin3")
                .into_iter()
                .flatten()
            {
                track_decodebin3(&element, &text_autoselect, &decodebins);
            }
        }

        playbin.connect_notify(Some("volume"), {
            let msg_tx = msg_tx.clone();
            move |playbin, _pspec| {
                msg_tx.player(PlayerEvent::VolumeChanged(
                    playbin.property::<f64>("volume"),
                ));
            }
        });

        playbin.connect("about-to-finish", false, {
            let msg_tx = msg_tx.clone();
            move |_| {
                msg_tx.player(PlayerEvent::AboutToFinish);
                None
            }
        });

        let bus = playbin.bus().ok_or(anyhow!("playbin is missing a bus"))?;
        let playbin_weak = playbin.downgrade();
        let msg_tx_c = msg_tx.clone();
        bus.set_sync_handler(move |_, msg| {
            Self::handle_messsage(
                &playbin_weak,
                &msg_tx_c,
                msg,
                &fcomp_context,
                #[cfg(feature = "airplay")]
                &airplay_context,
                // &signalling_channel,
            );
            gst::BusSyncReply::Drop
        });

        let (work_tx, work_rx) = std::sync::mpsc::channel();

        // Handle certain operations in a background thread to avoid blocking and potentially tokio runtime conflicts
        std::thread::Builder::new()
            .name("gst-player".to_owned())
            .spawn({
                // Strong ref
                let playbin = playbin.clone();
                let msg_tx = msg_tx.clone();
                let text_autoselect = text_autoselect.clone();
                let decodebins = decodebins.clone();
                move || {
                    let span = debug_span!("player");
                    let _entered = span.enter();

                    while let Ok(job) = work_rx.recv() {
                        debug!(?job, "Got job");

                        match job {
                            Job::SetState {
                                target_state,
                                feedback,
                            } => {
                                let _ = playbin.set_state(target_state);
                                if let Some(feedback) = feedback {
                                    debug!(res = ?feedback.send(()), "Sent state change feedback signal");
                                }
                            }
                            Job::SetUri { uri, suburi } => {
                                let _ = playbin.set_state(gst::State::Ready);

                                // Text auto-selection policy for this load
                                // (see the deep-element-added hook): applied
                                // to the decodebin3s that already exist —
                                // playbin3 reuses them across loads — before
                                // the new play item activates.
                                let allow = suburi.is_some();
                                text_autoselect
                                    .store(allow, std::sync::atomic::Ordering::SeqCst);
                                decodebins.lock().retain(|weak| {
                                    let Some(dbin) = weak.upgrade() else {
                                        return false;
                                    };
                                    dbin.set_property("auto-select-text", allow);
                                    true
                                });

                                if has_video {
                                    // Keep text streams out of playsink during
                                    // preroll — a text branch built while the
                                    // pipeline is not in steady PLAYING can
                                    // wedge it (subtitleoverlay's reconfigure
                                    // dance needs flowing data; embedded subs
                                    // hit this just like external ones). The
                                    // application restores the flag once the
                                    // pipeline settles (Job::EnableText).
                                    playbin.set_property_from_str(
                                        "flags",
                                        PlaybinFlags::AUDIO_AND_VIDEO_NO_TEXT,
                                    );
                                }

                                playbin.set_property("uri", uri);
                                // Must be set HERE, after `uri` and before
                                // `set_state(Paused)`: `suburi` binds to the next
                                // *inactive* play item and is silently lost once
                                // the item is activated by the Ready->Paused
                                // transition (uridecodebin3 FIXME). Setting it
                                // from any other thread races that activation.
                                playbin.set_property("suburi", suburi);

                                if let Ok(success) = playbin.set_state(gst::State::Paused)
                                    && success == gst::StateChangeSuccess::NoPreroll
                                {
                                    debug!("Pipeline is live");
                                    msg_tx.player(PlayerEvent::IsLive);
                                }

                                msg_tx.player(PlayerEvent::UriLoaded);
                            }
                            Job::Seek(seek) => {
                                // Non-blocking query: a zero timeout returns the
                                // in-flight transition instead of waiting for it.
                                // Waiting here (the old `state(None)`) wedged the
                                // whole worker when a seek arrived mid-preroll
                                // and the preroll stalled — every later job
                                // (Stop, SetUri, ...) queued behind it forever.
                                let (_, state, pending) = playbin.state(gst::ClockTime::ZERO);

                                if state != gst::State::Paused
                                    || pending != gst::State::VoidPending
                                {
                                    msg_tx.player(PlayerEvent::QueueSeek(seek));
                                    let _ = playbin.set_state(gst::State::Paused);
                                    continue;
                                }

                                let position = match seek.position {
                                    Some(pos) => pos,
                                    None => {
                                        let Some(pos) = playbin.query_position::<gst::ClockTime>()
                                        else {
                                            error!("Failed to query playback position");
                                            continue;
                                        };

                                        pos
                                    }
                                };

                                let rate = seek.rate.unwrap_or(1.0) as f64;

                                let mut flags = gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH;
                                if rate != 1.0 {
                                    flags |= gst::SeekFlags::TRICKMODE;
                                }

                                debug!(rate, ?position, "Performing seek");

                                let res = if rate >= 0.0 {
                                    playbin.seek(
                                        rate,
                                        flags,
                                        gst::SeekType::Set,
                                        position,
                                        gst::SeekType::None,
                                        gst::ClockTime::NONE,
                                    )
                                } else {
                                    playbin.seek(
                                        rate,
                                        flags,
                                        gst::SeekType::Set,
                                        gst::ClockTime::ZERO,
                                        gst::SeekType::End,
                                        position,
                                    )
                                };

                                if let Err(err) = res {
                                    error!(?err, "Failed to seek");
                                    msg_tx.player(PlayerEvent::SeekFailed);
                                } else {
                                    msg_tx.player(PlayerEvent::RateChanged(rate));
                                }
                            }
                            Job::RefreshSeek { seqnum } => {
                                let Some(position) =
                                    playbin.query_position::<gst::ClockTime>()
                                else {
                                    debug!("Skipping subtitle refresh: no position");
                                    msg_tx.player(PlayerEvent::SubtitleRefreshFailed { seqnum });
                                    continue;
                                };

                                // A flushing seek to the current position while
                                // staying in the current state: it re-emits the
                                // subtitle cue active *now* and flushes the stale
                                // one, without the Paused round-trip a normal
                                // seek performs. The stamped seqnum comes back on
                                // the resulting ASYNC_DONE.
                                debug!(?position, ?seqnum, "Refreshing subtitles via flushing seek");
                                let event = gst::event::Seek::builder(
                                    1.0,
                                    gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                                    gst::SeekType::Set,
                                    position,
                                    gst::SeekType::None,
                                    gst::ClockTime::NONE,
                                )
                                .seqnum(seqnum)
                                .build();
                                if !playbin.send_event(event) {
                                    warn!("Subtitle refresh seek failed");
                                    msg_tx.player(PlayerEvent::SubtitleRefreshFailed { seqnum });
                                }
                            }
                            Job::EnableText => {
                                if has_video {
                                    playbin.set_property_from_str(
                                        "flags",
                                        PlaybinFlags::AUDIO_AND_VIDEO,
                                    );
                                }
                            }
                            Job::RecoverClock => {
                                debug!("Recovering from clock loss");
                                if let Err(err) = playbin.set_state(gst::State::Paused) {
                                    warn!(?err, "Clock recovery: failed to reach Paused");
                                    continue;
                                }
                                if let Err(err) = playbin.set_state(gst::State::Playing) {
                                    warn!(?err, "Clock recovery: failed to reach Playing");
                                }
                            }
                            Job::Quit => {
                                break;
                            }
                        }
                    }

                    debug!("Player thread finished");
                }
            })?;

        work_tx.send(Job::SetState {
            target_state: gst::State::Ready,
            feedback: None,
        })?;

        Ok(Self {
            playbin,
            // TODO: are these "locks" needed?
            volume_lock: BoolLock::new(),
            work_tx,
            msg_tx,
            subtitles: subtitles::SubtitleDance::new(),
            current_video_stream: None,
            current_audio_stream: None,
            current_subtitle_stream: None,
            seekable: false,
            seekable_known: false,
            pending_volume: None,
            state_machine: StateMachine::new(),
            track_ops: TrackOps::new(),
            stream_collection: None,
            stream_collection_notify: None,
            streams: Vec::new(),
        })
    }

    fn handle_messsage(
        playbin_weak: &gst::glib::WeakRef<gst::Element>,
        msg_tx: &MessageSender,
        msg: &gst::Message,
        fcomp_context: &crate::fcompsrc::imp::CompContext,
        #[cfg(feature = "airplay")] airplay_context: &crate::airplay::AirPlayContext,
        // signalling_channel: &std::sync::Arc<
        //     parking_lot::Mutex<Option<crate::fwebrtcsrc::SignallingChannel>>,
        // >,
    ) {
        use gst::MessageView;

        let msg = match msg.view() {
            MessageView::NeedContext(ctx) => {
                let typ = ctx.context_type();
                debug!(typ, "Need context");
                if let Some(element) = msg
                    .src()
                    .and_then(|source| source.downcast_ref::<gst::Element>())
                {
                    debug!(typ, "Elem needs context");
                    if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                        let mut ctx = gst::Context::new(typ, true);
                        {
                            let ctx = ctx.get_mut().unwrap();
                            let s = ctx.structure_mut();
                            s.set("context", fcomp_context);
                        }
                        element.set_context(&ctx);
                        return;
                    }
                    #[cfg(feature = "airplay")]
                    if typ == crate::airplay::source::imp::AIRPLAY_CONTEXT {
                        let mut ctx = gst::Context::new(typ, true);
                        {
                            let ctx = ctx.get_mut().unwrap();
                            let s = ctx.structure_mut();
                            s.set(
                                "context",
                                crate::airplay::source::imp::BoxedAirPlayContext(
                                    airplay_context.clone(),
                                ),
                            );
                        }
                        element.set_context(&ctx);
                        return;
                    }
                    // } else if typ == crate::fwebrtcsrc::FSIG_CONTEXT {
                    //     let mut ctx = gst::Context::new(typ, true);
                    //     {
                    //         let ctx = ctx.get_mut().unwrap();
                    //         let s = ctx.structure_mut();
                    //         let sig_ctx =
                    //             crate::fwebrtcsrc::SigContext(signalling_channel.lock().clone());
                    //         s.set("context", sig_ctx);
                    //     }
                    //     element.set_context(&ctx);
                    //     return;
                    // }
                }

                return;
            }
            MessageView::Eos(_) => PlayerEvent::EndOfStream,
            MessageView::Error(error) => {
                if let Some(playbin) = playbin_weak.upgrade()
                    && let Some(src) = msg.src()
                    && !(src == &playbin)
                    && !src.has_as_ancestor(&playbin)
                {
                    debug!(
                        src = %src.name(),
                        "Dropping error from element no longer in the current pipeline"
                    );
                    return;
                }
                let failed_uri = msg
                    .src()
                    .and_then(|src| src.dynamic_cast_ref::<gst::URIHandler>())
                    .and_then(|handler| handler.uri())
                    .map(|uri| uri.to_string());
                let err = error.error();
                PlayerEvent::Error {
                    kind: MediaErrorKind::from_glib_error(&err),
                    message: err.message().to_string(),
                    failed_uri,
                }
            }
            MessageView::Warning(warning) => {
                PlayerEvent::Warning(warning.error().message().to_string())
            }
            MessageView::Tag(tag) => PlayerEvent::Tags(tag.tags()),
            MessageView::Buffering(buffering) => PlayerEvent::Buffering(buffering.percent()),
            MessageView::StateChanged(change) => {
                let Some(playbin) = playbin_weak.upgrade() else {
                    return;
                };

                if !msg.src().map(|s| s == &playbin).unwrap_or(false) {
                    return;
                }

                PlayerEvent::StateChanged {
                    old: change.old(),
                    current: change.current(),
                    pending: change.pending(),
                }
            }
            MessageView::RequestState(state) => {
                let state = state.requested_state();
                debug!(?state, "State requested");
                PlayerEvent::RequestState(state)
            }
            // MessageView::Toc(toc) TODO: is this something cool?
            MessageView::StreamCollection(collection) => {
                let collection = collection.stream_collection();
                PlayerEvent::StreamCollection(collection)
            }
            MessageView::StreamsSelected(streams) => {
                let mut video = None;
                let mut audio = None;
                let mut subtitle = None;

                for stream in streams.streams() {
                    let typ = stream.stream_type();

                    let id = stream.stream_id().map(|id| id.to_string());

                    if typ.contains(gst::StreamType::VIDEO) {
                        video = id;
                    } else if typ.contains(gst::StreamType::AUDIO) {
                        audio = id;
                    } else if typ.contains(gst::StreamType::TEXT) {
                        subtitle = id;
                    }
                }

                PlayerEvent::StreamsSelected {
                    video,
                    audio,
                    subtitle,
                    seqnum: msg.seqnum(),
                }
            }
            MessageView::ClockLost(_) => PlayerEvent::ClockLost,
            MessageView::AsyncDone(_) => {
                let Some(playbin) = playbin_weak.upgrade() else {
                    return;
                };

                if !msg.src().map(|s| s == &playbin).unwrap_or(false) {
                    return;
                }

                PlayerEvent::AsyncDone
            }
            MessageView::Element(_) => {
                if let Ok(msg) = gst_pbutils::MissingPluginMessage::parse(msg) {
                    error!(detail = %msg.installer_detail(), desc = %msg.description(), "GStreamer missing plugin");
                    return;
                }
                if msg
                    .structure()
                    .is_some_and(|s| s.name() == crate::video::SUBTITLE_OVERLAY_SHOWN_MESSAGE)
                {
                    PlayerEvent::SubtitleOverlayShown
                } else {
                    return;
                }
            }
            _ => return,
        };

        msg_tx.player(msg);
    }

    fn cleanup_stream_collection(&mut self) {
        if let Some(old_collection) = self.stream_collection.take()
            && let Some(sig_id) = self.stream_collection_notify.take()
        {
            old_collection.disconnect(sig_id);
        }
    }

    pub fn handle_stream_collection(&mut self, collection: gst::StreamCollection) {
        self.cleanup_stream_collection();

        let msg_tx = self.msg_tx.clone();
        self.stream_collection_notify = Some(collection.connect_stream_notify(
            None,
            move |_collection, _stream, param| {
                if param.name() == "tags" {
                    msg_tx.player(PlayerEvent::StreamTagsUpdated);
                }
            },
        ));

        // Indices into the outgoing stream list are meaningless in the new
        // one (media can advertise streams across several collections, each
        // reshuffling the indices), so carry the current selection over by
        // stream id before rebuilding.
        let video_sid = self.current_video_stream.and_then(|i| self.stream_id_of(i));
        let audio_sid = self.current_audio_stream.and_then(|i| self.stream_id_of(i));
        let subtitle_sid = self
            .current_subtitle_stream
            .and_then(|i| self.stream_id_of(i));

        self.streams.clear();

        for stream in collection.iter() {
            let title = stream_title(&stream);
            let stream = Stream {
                inner: stream,
                title,
            };

            self.streams.push(stream);
        }

        // Remap the carried-over selection onto the new collection, and seed
        // still-unselected slots with playbin3's defaults (the first stream
        // of each type) so a track change that arrives before the initial
        // `StreamsSelected` message still keeps the other streams selected
        // instead of dropping them. The real `StreamsSelected` corrects these
        // the moment it arrives. Remapping (rather than keeping the raw
        // index) means a later collection update (e.g. an added subtitle
        // source) never clobbers a selection the user already made.
        self.current_video_stream = video_sid
            .and_then(|sid| Self::find_stream_idx(&sid, &self.streams))
            .or_else(|| self.first_stream_of(gst::StreamType::VIDEO));
        self.current_audio_stream = audio_sid
            .and_then(|sid| Self::find_stream_idx(&sid, &self.streams))
            .or_else(|| self.first_stream_of(gst::StreamType::AUDIO));
        self.current_subtitle_stream = subtitle_sid
            .and_then(|sid| Self::find_stream_idx(&sid, &self.streams))
            .or_else(|| self.first_stream_of(gst::StreamType::TEXT));

        self.stream_collection = Some(collection);
    }

    fn first_stream_of(&self, ty: gst::StreamType) -> Option<u32> {
        self.streams
            .iter()
            .position(|s| s.inner.stream_type().contains(ty))
            .map(|idx| idx as u32)
    }

    pub fn get_duration(&self) -> Option<gst::ClockTime> {
        self.playbin.query_duration()
    }

    pub fn get_position(&self) -> Option<gst::ClockTime> {
        self.playbin.query_position()
    }

    fn clear_state(&mut self) {
        self.streams.clear();
        self.current_video_stream = None;
        self.current_audio_stream = None;
        self.current_subtitle_stream = None;
        self.seekable = false;
        self.seekable_known = false;
        self.volume_lock.release();
        // A volume queued behind an in-flight confirmation must not be
        // stranded by the load (volume is not item-scoped): apply it now
        // that the lock is free.
        if let Some(volume) = self.pending_volume.take() {
            self.set_volume(volume);
        }
        self.track_ops.reset();
    }

    /// Load a new main URI, optionally with an external subtitle URI.
    ///
    /// Both properties are applied together on the worker thread, in between
    /// the Ready and Paused state changes — the only window where playbin3
    /// reliably honors `suburi` (it binds to the next inactive play item and
    /// is silently ignored once that item is activated).
    ///
    /// Callers go through `load` (`subtitles` module), which also sets up
    /// the text-restore sequencing for the new item.
    fn set_uri(&mut self, uri: &str, suburi: Option<&str>) {
        self.clear_state();
        self.state_machine.clear_state();
        let _ = self.work_tx.send(Job::SetUri {
            uri: uri.to_string(),
            suburi: suburi.map(str::to_string),
        });
        self.state_machine.state = State::PendingUriChange;
    }

    fn seek_internal(&mut self, seek: Seek) {
        if let Some(rate) = seek.rate
            && !Seek::rate_is_safe(rate)
        {
            warn!(rate, "Ignoring invalid seek rate");
            return;
        }

        // An unresolved seekability query (`!seekable_known`) is not a
        // refusal: let the seek through — the state machine queues seeks
        // that land mid-preroll, so it runs once the pipeline settles. Only
        // a *known* unseekable stream drops the seek.
        if self.seekable || !self.seekable_known {
            // A user seek is itself a flushing seek and re-emits the current
            // subtitle cue; a separately queued refresh flush is redundant.
            self.track_ops.cancel_refresh();
            if let Some(seek) = self.state_machine.seek_internal(seek, None) {
                let _ = self.work_tx.send(Job::Seek(seek));
            }
        } else {
            warn!(?seek, "Attempted to seek on a non seekable stream");
        }
    }

    pub fn seek(&mut self, position: gst::ClockTime) {
        self.seek_internal(Seek {
            position: Some(position),
            rate: None,
        });
    }

    fn applied_track_selection(&self) -> TrackSelection {
        TrackSelection {
            video: self.current_video_stream,
            audio: self.current_audio_stream,
            subtitle: self.current_subtitle_stream,
        }
    }

    /// Handle a track-change request (latest-wins, serialized against other
    /// track operations; see `TrackOps`). Returns whether the currently
    /// displayed subtitle cue became stale -- the caller should clear the
    /// overlay so the change registers visually, even while paused.
    pub fn request_track_change(&mut self, kind: TrackKind, id: Option<u32>) -> bool {
        self.request_track_change_impl(kind, id, false)
    }

    /// A subtitle change that must not schedule the re-emit flush: while an
    /// external subtitle (`suburi`) is attached to the play item, ANY flush
    /// races the text-branch reconfiguration and errors the suburi source
    /// (pipeline error with the subtitle URL as its `failed_uri`), freezing
    /// the whole play item. The new track's cue appears at its next cue
    /// boundary instead. Same return as `request_track_change`.
    pub fn request_subtitle_change_no_refresh(&mut self, id: Option<u32>) -> bool {
        self.request_track_change_impl(TrackKind::Subtitle, id, true)
    }

    fn request_track_change_impl(
        &mut self,
        kind: TrackKind,
        id: Option<u32>,
        suppress_refresh: bool,
    ) -> bool {
        let applied = self.applied_track_selection();
        let stale_cue =
            kind == TrackKind::Subtitle && applied.subtitle.is_some() && id != applied.subtitle;
        // Bitmap subtitle tracks (`subpicture/*`: PGS, VOBSUB, DVB) are
        // composited INTO the video chain — subtitleoverlay splices
        // dvdspu/dvbsuboverlay into the video path — so switching one in or
        // out rebuilds the video chain itself, not just the text branch.
        // The re-emit flush racing that (unsignalled) rebuild has wedged
        // the pipeline for good: the flush broke the decoder's allocation
        // renegotiation ("DMABuf caps negotiated without VideoMeta"),
        // FLUSH_START died at a deactivating pad ("Failed to push event"
        // on vqueue) so the video sink never went flushing, and its
        // streaming thread kept clock-waiting on the lost-state-frozen
        // audio clock while holding the stream lock the seek's FLUSH_STOP
        // needs — deadlocking the worker and every load after it. The
        // rebuild posts no completion signal, so time distance is the only
        // guard: defer the flush (both directions) instead of racing it —
        // the bitmap cue appears ~2s after the switch rather than at its
        // next cue boundary (for DVB pages, potentially much later).
        let subpicture_involved = kind == TrackKind::Subtitle
            && [applied.subtitle, id]
                .into_iter()
                .flatten()
                .any(|idx| self.stream_is_subpicture(idx));
        self.track_ops.request(kind, id, applied);
        if suppress_refresh {
            self.track_ops.suppress_refresh();
        } else if subpicture_involved {
            self.track_ops.defer_refresh(SUBPICTURE_REFRESH_DELAY);
        }
        self.pump_track_ops();
        stale_cue
    }

    /// Whether the stream at `idx` is a bitmap subtitle (`subpicture/*`
    /// caps), rendered by splicing an overlay element into the video chain.
    fn stream_is_subpicture(&self, idx: u32) -> bool {
        self.streams
            .get(idx as usize)
            .and_then(|s| s.inner.caps())
            .and_then(|caps| {
                caps.structure(0)
                    .map(|st| st.name().starts_with("subpicture/"))
            })
            .unwrap_or(false)
    }

    /// Immediately send a text deselect, bypassing the serialized `TrackOps`
    /// queue (which parks work until the pipeline is quiet). Mid-preroll
    /// external-subtitle use ONLY: with the text playbin flag off, a text
    /// stream decodebin3 auto-selected mid-preroll wedges the preroll in well
    /// under 200 ms, so the deselect cannot wait for quiet — the preroll IS
    /// the noise. The confirming `STREAMS_SELECTED` carries this event's
    /// seqnum, which `TrackOps` never issued, so the queue ignores it; the
    /// caller runs its own verify/retry instead
    /// (see `Application::send_external_preroll_deselect`).
    pub fn deselect_text_mid_preroll(&mut self) -> Result<()> {
        let seqnum = gst::Seqnum::next();
        self.select_streams(
            self.current_video_stream,
            self.current_audio_stream,
            None,
            seqnum,
        )
        .map(drop)
    }

    /// Whether any serialized track operation is queued or still in flight
    /// (see `TrackOps`); the reload-restore re-pause waits for this to clear.
    pub fn has_pending_track_work(&self) -> bool {
        self.track_ops.is_busy()
    }

    /// Tick entry point: run the in-flight watchdog and dispatch pending track
    /// work once the pipeline has settled (the pump is also run from the
    /// state-change/ASYNC_DONE handlers; the tick covers completions that emit
    /// no further bus message, e.g. a plain stream switch).
    pub fn poll_track_ops(&mut self) {
        let paused = matches!(
            self.state_machine.state,
            State::Running {
                state: RunningState::Paused
            }
        );
        self.track_ops.run_watchdog(paused);
        self.pump_track_ops();
    }

    fn track_op_ctx(&self) -> TrackOpCtx {
        // Ask the pipeline whether an async state change (re-preroll, seek
        // preroll) is in progress instead of predicting from the kind of
        // change -- mispredictions are what used to wedge this logic.
        let (res, _, pending) = self.playbin.state(gst::ClockTime::ZERO);
        let async_busy =
            matches!(res, Ok(gst::StateChangeSuccess::Async)) || pending != gst::State::VoidPending;
        let (running, paused) = match self.state_machine.state {
            State::Running { state } => (true, state == RunningState::Paused),
            _ => (false, false),
        };
        TrackOpCtx {
            quiet: running && !async_busy,
            paused,
            applied: self.applied_track_selection(),
        }
    }

    fn pump_track_ops(&mut self) {
        while self.track_ops.has_dispatchable_work() {
            let ctx = self.track_op_ctx();
            let Some(cmd) = self.track_ops.pump(ctx) else {
                break;
            };
            match cmd {
                TrackOpCommand::SelectStreams(sel) => {
                    let seqnum = gst::Seqnum::next();
                    match self.select_streams(sel.video, sel.audio, sel.subtitle, seqnum) {
                        Ok(true) => self.track_ops.selection_dispatched(seqnum),
                        // Nothing was sent; there is no completion to wait for.
                        Ok(false) => (),
                        Err(err) => error!(?err, "Failed to apply track selection"),
                    }
                }
                TrackOpCommand::RefreshSeek => {
                    if !self.seekable {
                        debug!("Skipping subtitle refresh: stream is not seekable");
                        continue;
                    }
                    let seqnum = gst::Seqnum::next();
                    self.track_ops.refresh_dispatched(seqnum);
                    let _ = self.work_tx.send(Job::RefreshSeek { seqnum });
                }
            }
        }
    }

    /// A top-level `ASYNC_DONE`. Settles an in-flight subtitle refresh seek,
    /// retrying it while the pipeline stays paused and no cue was rendered:
    /// the flush races the paused subtitle branch's activation, which is why
    /// "switch away and back" used to work when a single flush didn't.
    pub fn async_done(&mut self) {
        if self.track_ops.refresh_done() {
            // A playing pipeline re-renders on the next flowing frame anyway,
            // so only retry when genuinely paused. The state machine can
            // transiently report Paused here (a re-preroll "loses" Playing),
            // so ask the pipeline for its target state instead.
            let (_, current, pending) = self.playbin.state(gst::ClockTime::ZERO);
            let paused_bound = current != gst::State::Playing && pending != gst::State::Playing;
            if paused_bound && self.track_ops.maybe_retry_refresh() {
                debug!("Subtitle refresh finished without rendering a cue; retrying");
            }
        }
        self.pump_track_ops();
    }

    /// The video sink rendered a frame carrying subtitle overlays.
    pub fn subtitle_overlay_shown(&mut self) {
        self.track_ops.overlay_shown();
    }

    /// The refresh seek job could not perform its seek.
    pub fn subtitle_refresh_failed(&mut self, seqnum: gst::Seqnum) {
        self.track_ops.refresh_failed(seqnum);
        self.pump_track_ops();
    }

    pub fn is_seeking(&self) -> bool {
        self.state_machine.is_seeking()
    }

    pub fn queue_seek(&mut self, seek: Seek) {
        self.state_machine.queue_seek(seek);
    }

    pub fn set_volume(&mut self, volume: f32) {
        if self.volume_lock.is_locked() {
            // A previous change's confirmation is still in flight. Don't
            // drop the request (the sender would wait forever for its
            // confirmation) — remember the latest and apply it on release.
            debug!(volume, "Volume change pending; queueing");
            self.pending_volume = Some(volume);
            return;
        }

        let target = (volume as f64).clamp(0.0, 1.0);
        let current: f64 = self.playbin.property("volume");
        if (current - target).abs() < 1e-9 {
            // Setting the property to its current value emits no notify,
            // so no confirmation would ever be relayed — but senders expect
            // one for an idempotent set too. Re-emit the notify manually;
            // it flows through the same VolumeChanged path as a real
            // change. No lock: nothing is in flight.
            debug!(volume, "Volume unchanged; re-emitting the confirmation");
            self.playbin.notify("volume");
            return;
        }

        self.playbin.set_property("volume", target);
        self.volume_lock.acquire();
    }

    pub fn volume_changed(&mut self) {
        self.volume_lock.release();
        // Apply the newest request that arrived while the confirmation was
        // in flight (last one wins).
        if let Some(volume) = self.pending_volume.take() {
            self.set_volume(volume);
        }
    }

    pub fn set_rate(&mut self, rate: f32) {
        self.seek_internal(Seek {
            position: None,
            rate: Some(rate),
        });
    }

    pub fn update_media_info(&mut self) {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        if self.playbin.query(query.query_mut()) {
            let (seekable, _, _) = query.result();
            let dur = self.get_duration();
            debug!(?dur, seekable, "Seek query returned");
            self.seekable = seekable && dur.is_some();
            self.seekable_known = true;
        }
    }

    pub fn seek_and_set_rate(&mut self, position: gst::ClockTime, rate: f32) {
        self.seek_internal(Seek {
            position: Some(position),
            rate: Some(rate),
        });
    }

    fn set_state_async(&self, target_state: gst::State) {
        let _ = self.work_tx.send(Job::SetState {
            target_state,
            feedback: None,
        });
    }

    fn set_state_async_with_feedback(
        &self,
        target_state: gst::State,
        feedback: oneshot::Sender<()>,
    ) {
        let _ = self.work_tx.send(Job::SetState {
            target_state,
            feedback: Some(feedback),
        });
    }

    pub fn play(&mut self) {
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Playing) {
            self.set_state_async(state);
        }
    }

    /// Honor a `RequestState` message from an element by dispatching the state
    /// change to the worker thread (off the streaming thread it arrived on).
    pub fn request_state(&self, state: gst::State) {
        self.set_state_async(state);
    }

    /// Re-enable the text playbin flag after a suburi load (it is disabled
    /// for the duration of the preroll; see `Job::SetUri`). playbin3 reacts
    /// by selecting a text stream on its own, confirmed via a
    /// `StreamsSelected`.
    pub fn enable_text_flag(&self) {
        let _ = self.work_tx.send(Job::EnableText);
    }

    /// Handle `ClockLost`: the element providing the pipeline clock went away
    /// (typically the audio sink after the audio track was deselected).
    pub fn recover_clock(&mut self) {
        if !matches!(self.player_state(), PlayerState::Playing) {
            debug!("Ignoring clock loss while not playing");
            return;
        }
        debug!("Pipeline clock lost; cycling through Paused to elect a new one");
        let _ = self.work_tx.send(Job::RecoverClock);
    }

    #[cfg(debug_assertions)]
    pub fn graph_dot_data(&self) -> Option<gst::glib::GString> {
        let bin = self.playbin.downcast_ref::<gst::Bin>()?;
        Some(bin.debug_to_dot_data(gst::DebugGraphDetails::all()))
    }

    #[cfg(debug_assertions)]
    pub fn dump_graph(&self, trigger: remote_pipeline_dbg::Trigger) {
        use remote_pipeline_dbg::{PipelineSource, post_graph};

        debug!(?trigger, "Dumping pipeline graph");

        let Some(bin) = self.playbin.downcast_ref::<gst::Bin>() else {
            // Unreachable
            error!("Playbin is not a bin");
            return;
        };

        let graph = bin.debug_to_dot_data(gst::DebugGraphDetails::all());

        if let Err(err) = post_graph(graph.as_bytes(), PipelineSource::MainPlayer, trigger) {
            error!(?err, "Failed to post graph data");
        }
    }

    pub fn pause(&mut self) {
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Paused) {
            self.set_state_async(state);
        }
    }

    fn go_to_stopped_state(&mut self, null: Option<oneshot::Sender<()>>) {
        // Unconditional (unlike the pipeline teardown below, which is
        // skipped when already stopped): the dance state of an aborted
        // early load must not leak into the next one, and bumping the
        // generation kills its armed timers.
        self.subtitles.reset();
        self.cleanup_stream_collection();

        let target = if null.is_some() {
            gst::State::Null
        } else {
            gst::State::Ready
        };

        if target == gst::State::Ready && self.state_machine.current_state == gst::State::Null {
            return;
        }

        if self.state_machine.current_state != target {
            match null {
                Some(feedback) => self.set_state_async_with_feedback(target, feedback),
                None => self.set_state_async(target),
            }
            self.state_machine.clear_state();
            self.clear_state();
        }
    }

    pub fn stop(&mut self) {
        debug!("Stopping playback");
        self.go_to_stopped_state(None)
    }

    pub fn shutdown(&mut self, feedback: oneshot::Sender<()>) {
        debug!("Shutting down player");
        self.go_to_stopped_state(Some(feedback));
    }

    /// Returns `true` if any stream has new properties.
    pub fn update_stream_properties(&mut self) -> bool {
        let mut did_change = false;

        for stream in &mut self.streams {
            let title = stream_title(&stream.inner);
            if title != stream.title {
                stream.title = title;
                did_change = true;
            }
        }

        did_change
    }

    /// Send a `SELECT_STREAMS` for the given selection, stamped with `seqnum`
    /// so the confirming `STREAMS_SELECTED` message can be attributed to it.
    /// Returns whether an event was actually sent.
    fn select_streams(
        &mut self,
        video: Option<u32>,
        audio: Option<u32>,
        mut subtitle: Option<u32>,
        seqnum: gst::Seqnum,
    ) -> Result<bool> {
        // playsink cannot build a text chain without a video chain (pipeline
        // error), so a selection without video must never carry a subtitle
        // stream. Deselecting video therefore implicitly deselects subtitles;
        // the relayed `TracksSelected` reports that to the senders.
        if video.is_none() && subtitle.is_some() {
            debug!("Dropping the subtitle stream from a selection without video");
            subtitle = None;
        }

        let mut streams = Vec::new();

        for idx in [video, audio, subtitle] {
            if let Some(idx) = idx
                && let Some(stream) = self.streams.get(idx as usize)
                && let Some(id) = stream.inner.stream_id()
            {
                streams.push(id);
            }
        }

        // An empty selection would trip a GStreamer assertion
        // (`gst_event_new_select_streams: streams != NULL`) and leave the
        // pipeline in an undefined state, so refuse to send one.
        if streams.is_empty() {
            debug!("Refusing to send an empty stream selection");
            return Ok(false);
        }

        let event = gst::event::SelectStreams::builder(streams.iter().map(|s| s.as_str()))
            .seqnum(seqnum)
            .build();
        if !self.playbin.send_event(event) {
            warn!("Pipeline refused the SELECT_STREAMS event");
            return Ok(false);
        }

        // Track the requested selection right away instead of waiting for
        // `StreamsSelected`: a second track change arriving before the first
        // one is confirmed must compose with it, not revert it (each change
        // rebuilds the full selection from `current_*`). `streams_selected`
        // overwrites these with whatever the pipeline actually applied.
        self.current_video_stream = video;
        self.current_audio_stream = audio;
        self.current_subtitle_stream = subtitle;

        Ok(true)
    }

    /// The index of the stream with this GStreamer stream id, if advertised.
    pub fn stream_idx_by_id(&self, sid: &str) -> Option<u32> {
        Self::find_stream_idx(sid, &self.streams)
    }

    /// The first advertised TEXT stream, if any — the same stream
    /// decodebin3's default selection would pick (one stream per type, in
    /// collection order). Used by the plain-load restore, which suppresses
    /// that default (`auto-select-text` off) and re-creates it after the
    /// pipeline settles.
    pub fn first_text_stream_idx(&self) -> Option<u32> {
        self.streams
            .iter()
            .position(|s| s.inner.stream_type().contains(gst::StreamType::TEXT))
            .map(|idx| idx as u32)
    }

    /// Whether the pipeline has no async state transition in progress
    /// (non-blocking query). Used to hold flushing operations off while
    /// playsink is still reconfiguring (e.g. building a text branch, which
    /// posts no bus signal of its own).
    pub fn is_pipeline_stable(&self) -> bool {
        let (res, _state, pending) = self.playbin.state(gst::ClockTime::ZERO);
        res.is_ok() && pending == gst::State::VoidPending
    }

    /// The GStreamer stream id of the `idx`th advertised stream.
    pub fn stream_id_of(&self, idx: u32) -> Option<String> {
        self.streams
            .get(idx as usize)?
            .inner
            .stream_id()
            .map(|id| id.to_string())
    }

    pub fn is_stream_of_type(&self, idx: u32, ty: gst::StreamType) -> bool {
        self.streams
            .get(idx as usize)
            .is_some_and(|s| s.inner.stream_type().contains(ty))
    }

    pub fn end_of_stream_reached(&mut self) {
        self.stop();
    }

    pub fn uri_loaded(&mut self) {
        // self.set_state_async(gst::State::Paused);
        self.set_state_async(gst::State::Playing);
    }

    /// Returns `true` if buffering completed
    pub fn buffering(&mut self, percent: i32) -> bool {
        let res = match self.state_machine.buffering(percent) {
            BufferingStateResult::Started(state) => {
                self.set_state_async(state);
                false
            }
            BufferingStateResult::Buffering => false,
            BufferingStateResult::FinishedWithSeek(seek) => {
                debug!(state = ?self.state_machine.state, "Buffering finished, dispatching seek");
                let _ = self.work_tx.send(Job::Seek(seek));
                true
            }
            BufferingStateResult::FinishedButWaitingSeek => {
                debug!(state = ?self.state_machine.state, "Buffering finished with seek");
                true
            }
            BufferingStateResult::Finished(state) => {
                debug!(state = ?self.state_machine.state, "Buffering finished");
                if let Some(state) = state {
                    self.set_state_async(state);
                }
                true
            }
        };

        // Buffering completion can settle the pipeline; dispatch queued track
        // work (no-op while still buffering: the machine is not `Running`).
        self.pump_track_ops();

        res
    }

    pub fn state_changed(
        &mut self,
        old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> Option<PlaybackState> {
        // Queued track work is deliberately NOT pumped from here: the
        // application runs this at the START of its StateChanged handling,
        // and a Playing commit's cascade may still launch the start/restore
        // seek (`on_media_info_updated` → `maybe_run_start_seek`) —
        // a selection dispatched into that one-instant-quiet window then
        // interleaves with the seek's Playing→Paused→seek→Playing dance and
        // its playsink reconfigure runs outside steady PLAYING (observed:
        // a parked video-disable dispatched at the commit wedged the
        // pipeline for good). The application pumps at the END of the
        // cascade instead, when the seek — if any — already owns the state
        // machine.
        match self.state_machine.state_changed(old, new, pending) {
            StateChangeResult::NewPlaybackState(new_state) => Some(new_state),
            StateChangeResult::Seek(seek) => {
                let _ = self.work_tx.send(Job::Seek(seek));
                None
            }
            StateChangeResult::Waiting => None,
            StateChangeResult::ChangeState(state) => {
                self.set_state_async(state);
                None
            }
        }
    }

    pub fn have_media_info(&self) -> bool {
        !self.streams.is_empty()
    }

    fn find_stream_idx(sid: &str, streams: &[Stream]) -> Option<u32> {
        for (idx, stream) in streams.iter().enumerate() {
            if let Some(this_id) = stream.inner.stream_id()
                && this_id == sid
            {
                return Some(idx as u32);
            }
        }

        None
    }

    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    pub fn streams_selected(
        &mut self,
        video_sid: Option<&str>,
        audio_sid: Option<&str>,
        subtitle_sid: Option<&str>,
        seqnum: gst::Seqnum,
    ) -> (Option<u32>, Option<u32>, Option<u32>) {
        debug!(?video_sid, ?audio_sid, ?subtitle_sid, ?seqnum);

        // Settles the in-flight selection when the seqnum matches. Queued work
        // is deliberately NOT dispatched from here: a confirmed selection can
        // still be about to re-preroll playsink (the async change may not have
        // started yet, so the pipeline still looks quiet). The next
        // state-change/ASYNC_DONE event -- or the 100 ms tick, when no
        // re-preroll follows -- runs the pump instead.
        self.track_ops.streams_selected(seqnum);

        self.current_video_stream = None;
        self.current_audio_stream = None;
        self.current_subtitle_stream = None;

        if let Some(video) = video_sid {
            self.current_video_stream = Self::find_stream_idx(video, &self.streams);
        }
        if let Some(audio) = audio_sid {
            self.current_audio_stream = Self::find_stream_idx(audio, &self.streams);
        }
        if let Some(subtitle) = subtitle_sid {
            self.current_subtitle_stream = Self::find_stream_idx(subtitle, &self.streams);
        }

        (
            self.current_video_stream,
            self.current_audio_stream,
            self.current_subtitle_stream,
        )
    }

    pub fn player_state(&self) -> PlayerState {
        match &self.state_machine.state {
            State::Stopped => PlayerState::Stopped,
            State::PendingUriChange
            | State::Buffering { .. }
            | State::Changing { .. }
            | State::SeekAsync { .. }
            | State::Seeking { .. } => PlayerState::Buffering, // TODO: ?
            State::Running { state } => match state {
                RunningState::Paused => PlayerState::Paused,
                RunningState::Playing => PlayerState::Playing,
            },
        }
    }

    pub fn is_live(&self) -> bool {
        self.state_machine.is_live
    }

    pub fn set_is_live(&mut self, live: bool) {
        self.state_machine.is_live = live;
    }

    pub fn rate(&self) -> f64 {
        self.state_machine.rate
    }

    #[instrument(skip_all)]
    pub fn seek_failed(&mut self) {
        if let Some(target_state) = self.state_machine.seek_failed() {
            debug!(?target_state);
            self.set_state_async(target_state);
        }
    }

    pub fn set_rate_changed(&mut self, rate: f64) {
        self.state_machine.rate = rate;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.set_state_async(gst::State::Null);
        let _ = self.work_tx.send(Job::Quit);
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
    const ONE: gst::ClockTime = gst::ClockTime::from_seconds(1);
    const FIVE: gst::ClockTime = gst::ClockTime::from_seconds(5);
    const TEN: gst::ClockTime = gst::ClockTime::from_seconds(10);
    // const TWENTY: gst::ClockTime = gst::ClockTime::from_seconds(10);
    const THIRTY: gst::ClockTime = gst::ClockTime::from_seconds(10);

    #[test]
    #[rustfmt::skip]
    fn basic_playback() {
        let mut sm = StateMachine::new();
        sm.state = State::PendingUriChange;
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(CTZ), None), None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)),);
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_2() {
        let mut sm = StateMachine::new();
        sm.state = State::PendingUriChange;
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(gst::ClockTime::from_seconds(0)), None), None), Some(Seek::new(Some(gst::ClockTime::from_seconds(0)), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(Paused)), new_ps!(Playing),);

        // 2nd seek:
        let sixty = gst::ClockTime::from_seconds(60);
        assert_eq!(sm.seek_internal(Seek::new(Some(sixty), None), None), Some(Seek::new(Some(sixty), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(sixty), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(sixty), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(sixty), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(sixty), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending),), StateChangeResult::ChangeState(gs!(Playing)),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(Paused)), new_ps!(Playing),);
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_3() {
        let mut sm = StateMachine::new();
        sm.state = State::PendingUriChange;
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek {position: Some(CTZ), rate: None}, None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.buffering(1), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting);
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
        sm.state = State::PendingUriChange;
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(CTZ), None), None), Some(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert!(matches!(sm.state, State::SeekAsync { seek: _, target_state: gs!(Playing) }));
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(sm.state, State::Buffering { .. }));
        let sixty = gst::ClockTime::from_seconds(60);
        assert_eq!(sm.seek_internal(Seek::new(Some(sixty), None), None), None);
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(sixty), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)),);
    }

    #[test]
    #[rustfmt::skip]
    fn state_change() {
        let mut sm = StateMachine::new();
        sm.state = State::PendingUriChange;
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)), StateChangeResult::NewPlaybackState(PlaybackState::Paused));
        assert_eq!(sm.set_playback_state(RunningState::Playing), Some(gst::State::Playing));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Changing { target_state: gst::State::Playing, pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::NewPlaybackState(PlaybackState::Playing));
        assert_eq!(sm.state, State::Running { state: RunningState::Playing });
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);

        assert_eq!(sm.set_playback_state(RunningState::Paused), Some(gst::State::Paused));
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.state, State::Changing { target_state: gst::State::Paused, pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::NewPlaybackState(PlaybackState::Paused));
        assert_eq!(sm.state, State::Running { state: RunningState::Paused });
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
    }

    #[test]
    #[rustfmt::skip]
    fn changing_state_honors_latest_request() {
        let mut sm = StateMachine::new();
        sm.state = State::PendingUriChange;
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.set_playback_state(rs!(Playing)), Some(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.state, State::Running { state: rs!(Playing) });
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.state, State::Changing { target_state: gs!(Paused), pending_seek: None });
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.state, State::Changing { target_state: gs!(Playing), pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state, State::Changing { target_state: gs!(Playing), pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.state, State::Running { state: rs!(Playing) });
    }

    fn playing() -> StateMachine {
        let mut sm = StateMachine::new();
        sm.current_state = gst::State::Playing;
        sm.state = State::Running {
            state: RunningState::Playing,
        };
        sm
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

        if let State::Buffering { percent, .. } = sm.state {
            let _ = sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending));
            panic!(
                "stuck in Buffering {{ percent: {percent} }} after a lone \
                 buffering(100); player_state() will report Buffering forever \
                 and playback is paused",
            );
        }
    }

    #[test]
    fn lone_buffering_100_during_uri_change_must_not_strand() {
        let mut sm = StateMachine::new();
        sm.state = State::PendingUriChange;

        let r = sm.buffering(100);
        assert_ne!(
            r,
            BufferingStateResult::Started(gst::State::Paused),
            "buffering(100) during URI change requested a pause",
        );

        let _ = sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending));
        let _ = sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending));

        assert!(
            !matches!(sm.state, State::Buffering { .. }),
            "stuck in Buffering after startup despite pipeline reaching Playing: {:?}",
            sm.state,
        );
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
        assert_eq!(sm.state, State::Running { state: rs!(Playing) });
    }

    #[test]
    #[rustfmt::skip]
    fn rapid_play_pause_toggles_converge_to_last_request() {
        let mut sm = playing();
        assert_eq!(sm.set_playback_state(rs!(Paused)), Some(gs!(Paused)));
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.set_playback_state(rs!(Paused)), None);
        assert_eq!(sm.set_playback_state(rs!(Playing)), None);
        assert_eq!(sm.state, State::Changing { target_state: gs!(Playing), pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.state, State::Running { state: rs!(Playing) });
    }

    #[test]
    #[rustfmt::skip]
    fn changing_waits_through_async_transition_then_recovers() {
        let mut sm = StateMachine::new();
        sm.current_state = gs!(Ready);
        sm.state = State::Changing { target_state: gs!(Playing), pending_seek: None };
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state, State::Changing { target_state: gs!(Playing), pending_seek: None },
            "fall-through must not mutate state while the transition is still in flight");
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.state, State::Running { state: rs!(Playing) },
            "stranded in Changing after the async transition completed");
    }

    #[test]
    #[rustfmt::skip]
    fn changing_fallthrough_recovers_even_if_target_flipped_mid_transition() {
        let mut sm = StateMachine::new();
        sm.current_state = gs!(Ready);
        sm.state = State::Changing { target_state: gs!(Playing), pending_seek: None };
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.set_playback_state(rs!(Paused)), None);
        assert_eq!(sm.state, State::Changing { target_state: gs!(Paused), pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        assert_eq!(sm.state, State::Changing { target_state: gs!(Paused), pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.state, State::Running { state: rs!(Paused) });
    }

    #[test]
    #[rustfmt::skip]
    fn seek_is_rejected_when_stopped_or_live() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.state, State::Stopped);
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), None);
        assert_eq!(sm.state, State::Stopped, "rejected seek must not mutate state");
        let mut sm = playing();
        sm.is_live = true;
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), None);
        assert_eq!(sm.state, State::Running { state: rs!(Playing) }, "rejected live seek must not mutate state");
    }

    #[test]
    #[rustfmt::skip]
    fn seek_while_seeking_coalesces() {
        let mut sm = playing();
        assert_eq!(sm.seek_internal(Seek::new(Some(TEN), None), None), Some(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.state, State::Seeking { target_state: gs!(Playing) });
        assert_eq!(sm.seek_internal(Seek::new(Some(gst::ClockTime::from_seconds(20)), None), None), None);
        assert_eq!(sm.state, State::Seeking { target_state: gs!(Playing) }, "still exactly one in-flight seek");
    }

    #[test]
    #[rustfmt::skip]
    fn seek_async_stays_parked_until_pipeline_settles_at_paused() {
        // A seek that has been queued (SeekAsync) is only dispatched once the
        // pipeline actually settles at Paused/VoidPending. Interim transitions
        // must leave it parked. If that settle never arrives -- e.g. a seek
        // issued while a subtitle stream selection is still reconfiguring the
        // pipeline -- the seek never completes and playback freezes. That is why
        // the subtitle refresh (TrackOps) uses a direct flushing seek via
        // Job::RefreshSeek instead of driving this state machine.
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(TEN), Some(1.0)));
        assert!(matches!(sm.state, State::SeekAsync { target_state: gs!(Playing), .. }));
        // Reached Paused but still transitioning (pending != VoidPending): parked.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert!(matches!(sm.state, State::SeekAsync { .. }), "must stay parked while still transitioning");
        // Settled at a non-Paused state: still parked (SeekAsync only fires on Paused).
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting);
        assert!(matches!(sm.state, State::SeekAsync { .. }));
        // Settled at Paused/VoidPending: the queued seek is finally dispatched.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.state, State::Seeking { target_state: gs!(Playing) });
    }

    #[test]
    #[rustfmt::skip]
    fn pause_during_load_survives_stale_playing_overshoot() {
        // A user Pause retargets the load's Changing{Playing} to Paused while
        // the pipeline's original commit to Playing is still in flight.
        // Reaching Paused with pending=Playing must NOT count as arrival: the
        // machine used to settle into Running{Paused}, adopt the overshoot to
        // Playing (Running follows the pipeline), and the deferred start seek
        // then cemented Playing — un-pausing the user.
        let mut sm = StateMachine::new();
        sm.state = State::Changing { target_state: gs!(Playing), pending_seek: None };
        // The user pauses mid-load: retargets the in-flight change.
        assert_eq!(sm.set_playback_state(RunningState::Paused), None);
        assert!(matches!(sm.state, State::Changing { target_state: gs!(Paused), .. }));
        // The stale upward commit still delivers Paused-with-pending-Playing…
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert!(matches!(sm.state, State::Changing { target_state: gs!(Paused), .. }), "must not settle while the overshoot is in flight");
        // …and overshoots to Playing; the machine must correct, not adopt.
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Paused)));
        // The correction lands and only then do we settle — paused.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), new_ps!(Paused));
        assert_eq!(sm.state, State::Running { state: rs!(Paused) });
    }

    #[test]
    #[rustfmt::skip]
    fn queued_seek_while_playing_recovers_to_playing() {
        // The full happy path of a seek issued while playing: park (SeekAsync) ->
        // dispatch on settle (Seeking) -> re-preroll (Changing) -> back to
        // Playing. Guards the ordinary seek path we still rely on for user seeks.
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(TEN), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(TEN), Some(1.0))));
        assert_eq!(sm.state, State::Seeking { target_state: gs!(Playing) });
        // Seek finished (pipeline re-prerolled to Paused); target is Playing so we
        // must drive back up rather than stranding in Paused.
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)));
        assert_eq!(sm.state, State::Changing { target_state: gs!(Playing), pending_seek: None });
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), new_ps!(Playing));
        assert_eq!(sm.state, State::Running { state: rs!(Playing) });
    }

    #[test]
    #[rustfmt::skip]
    fn buffering_completing_before_paused_settled_parks_in_seek_async() {
        // Opposite ordering: buffering finishes while the pipeline is still
        // transitioning (no Paused settle observed yet). The seek must still park
        // in SeekAsync and fire on the later Paused/VoidPending edge, exactly as
        // before the freeze fix.
        let mut sm = playing();
        sm.queue_seek(Seek::new(Some(CTZ), Some(1.0)));
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.current_state, gs!(Playing), "no Paused settle seen yet");
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert!(matches!(sm.state, State::SeekAsync { target_state: gs!(Playing), .. }));
        // The eventual Paused settle dispatches the parked seek.
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(CTZ), Some(1.0))));
        assert_eq!(sm.state, State::Seeking { target_state: gs!(Playing) });
    }

    #[test]
    fn changing_with_pending_seek_while_live() {
        let mut sm = StateMachine::new();
        sm.is_live = true;
        sm.current_state = gst::State::Paused;
        sm.state = State::Changing {
            target_state: gst::State::Paused,
            pending_seek: Some(Seek::new(Some(ONE), Some(1.0))),
        };
        let _ = sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending));
    }

    #[test]
    fn async_seek_not_dropped_when_pause_arrives_during_buffering() {
        let mut sm = StateMachine::new();
        sm.current_state = gst::State::Playing;
        sm.state = State::SeekAsync {
            seek: Seek::new(Some(THIRTY), Some(1.0)),
            target_state: gs!(Playing),
        };

        assert_eq!(sm.buffering(40), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(
            sm.state,
            State::Buffering {
                pending_seek: Some(PendingSeek::Async(_)),
                ..
            }
        ));
        // Paused arrives *during* buffering: the Buffering arm consumes this edge
        // (it only acts on a `Waiting` pending seek, not an `Async` one) and it is
        // the last Paused/VoidPending edge the pipeline will emit.
        assert_eq!(
            sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)),
            StateChangeResult::Waiting
        );
        assert_eq!(sm.current_state, gs!(Paused));
        // Completing buffering must therefore dispatch the seek itself. Parking in
        // SeekAsync (the previous behavior) waited for a Paused edge that already
        // passed and froze playback in "Buffering" forever.
        assert_eq!(
            sm.buffering(100),
            BufferingStateResult::FinishedWithSeek(Seek::new(Some(THIRTY), Some(1.0))),
            "queued async seek was silently dropped when Paused arrived mid-buffer",
        );
        assert_eq!(
            sm.state,
            State::Seeking {
                target_state: gs!(Playing)
            },
        );
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
        assert!(
            matches!(
                sm.state,
                State::SeekAsync {
                    target_state: gst::State::Playing,
                    ..
                }
            ),
            "queue_seek while playing must keep target Playing, got {:?}",
            sm.state,
        );
        let mut sm = StateMachine::new();
        sm.current_state = gst::State::Paused;
        sm.state = State::Running {
            state: RunningState::Paused,
        };
        sm.queue_seek(Seek::new(Some(FIVE), Some(1.0)));
        assert!(matches!(
            sm.state,
            State::SeekAsync {
                target_state: gst::State::Paused,
                ..
            }
        ));
    }

    #[test]
    fn seek_failed_while_paused_must_not_hang() {
        let mut sm = StateMachine::new();
        sm.current_state = gst::State::Paused;
        sm.state = State::Running {
            state: RunningState::Paused,
        };
        assert_eq!(
            sm.seek_internal(Seek::new(None, Some(1.5)), None),
            Some(Seek::new(None, Some(1.5)))
        );
        assert_eq!(
            sm.state,
            State::Seeking {
                target_state: gs!(Paused)
            }
        );
        let resume = sm.seek_failed();
        assert_eq!(
            sm.state,
            State::Running {
                state: RunningState::Paused
            },
            "seek_failed left a Changing state that can never complete \
             (target == current_state, no transition will occur): resume={resume:?}",
        );
    }

    // --- TrackOps -----------------------------------------------------------

    fn sel(video: Option<u32>, audio: Option<u32>, subtitle: Option<u32>) -> TrackSelection {
        TrackSelection {
            video,
            audio,
            subtitle,
        }
    }

    fn ctx(quiet: bool, paused: bool, applied: TrackSelection) -> TrackOpCtx {
        TrackOpCtx {
            quiet,
            paused,
            applied,
        }
    }

    #[test]
    fn selection_dispatches_immediately_when_quiet() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Audio, Some(2), applied);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(Some(0), Some(2), None)))
        );
    }

    #[test]
    fn selection_waits_until_quiet() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Audio, Some(2), applied);
        assert_eq!(ops.pump(ctx(false, false, applied)), None);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(Some(0), Some(2), None)))
        );
    }

    #[test]
    fn noop_selection_is_not_dispatched() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), Some(2));
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert_eq!(ops.pump(ctx(true, false, applied)), None);
        assert!(!ops.has_dispatchable_work());
    }

    #[test]
    fn playing_switch_serializes_and_coalesces_latest_wins() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), Some(2));
        ops.request(TrackKind::Subtitle, Some(3), applied);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(3)
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        // `select_streams` records the new selection optimistically.
        let applied = sel(Some(0), Some(1), Some(3));

        // Unconfirmed selection blocks everything while playing -- including
        // the refresh the subtitle switch scheduled.
        assert_eq!(ops.pump(ctx(true, false, applied)), None);

        // Spammed changes only remember the latest.
        ops.request(TrackKind::Subtitle, Some(4), applied);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert_eq!(ops.pump(ctx(true, false, applied)), None);

        // A foreign STREAMS_SELECTED (initial auto-selection, another op's
        // confirmation) must not settle ours.
        ops.streams_selected(gst::Seqnum::next());
        assert_eq!(ops.pump(ctx(true, false, applied)), None);

        // Ours settles it; the queued latest dispatches next pump.
        ops.streams_selected(sn);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(2)
            )))
        );
    }

    #[test]
    fn refresh_dispatches_after_selection_settles_and_pipeline_quiets() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        // Enabling a subtitle schedules a refresh.
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(2)
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));

        ops.streams_selected(sn);
        // Re-preroll in progress: refresh must hold.
        assert_eq!(ops.pump(ctx(false, false, applied)), None);
        // Settled: flush.
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        // One flush only.
        assert_eq!(ops.pump(ctx(true, false, applied)), None);
    }

    #[test]
    fn subtitle_disable_cancels_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(2)
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));
        ops.streams_selected(sn);

        // Disable before the refresh fired: no flush may follow (flushing
        // right after the text-branch teardown breaks renegotiation).
        ops.request(TrackKind::Subtitle, None, applied);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(Some(0), Some(1), None)))
        );
        let sn2 = gst::Seqnum::next();
        ops.selection_dispatched(sn2);
        let applied = sel(Some(0), Some(1), None);
        ops.streams_selected(sn2);
        assert_eq!(ops.pump(ctx(true, false, applied)), None);
    }

    #[test]
    fn suppressed_subtitle_switch_schedules_no_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        // External suburi attached: the app forbids the re-emit flush.
        ops.request(TrackKind::Subtitle, Some(2), applied);
        ops.suppress_refresh();
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(2)
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));
        ops.streams_selected(sn);
        // No flush may follow the confirmed selection.
        assert_eq!(ops.pump(ctx(true, false, applied)), None);
        assert!(!ops.is_busy());
    }

    #[test]
    fn each_subtitle_request_redecides_refresh_suppression() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        // A suppressed request parks (pipeline busy)...
        ops.request(TrackKind::Subtitle, Some(2), applied);
        ops.suppress_refresh();
        assert_eq!(ops.pump(ctx(false, false, applied)), None);
        // ...and is superseded by a plain one: its flush is allowed again.
        ops.request(TrackKind::Subtitle, Some(3), applied);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(3)
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(3));
        ops.streams_selected(sn);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
    }

    #[test]
    fn user_seek_cancels_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert!(ops.pump(ctx(true, false, applied)).is_some());
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));
        ops.streams_selected(sn);

        // The user's own flushing seek re-emits the cue already.
        ops.cancel_refresh();
        assert_eq!(ops.pump(ctx(true, false, applied)), None);
    }

    #[test]
    fn paused_selection_parks_and_refresh_flushes_past_it() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert_eq!(
            ops.pump(ctx(true, true, applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some(0),
                Some(1),
                Some(2)
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));

        // While paused the selection is parked (no STREAMS_SELECTED until data
        // flows); the refresh must dispatch anyway -- it is what wakes the
        // pipeline and makes the selection apply.
        assert_eq!(
            ops.pump(ctx(true, true, applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        let rn = gst::Seqnum::next();
        ops.refresh_dispatched(rn);

        // Flush in flight: nothing else dispatches even though paused.
        ops.request(TrackKind::Audio, Some(3), applied);
        assert_eq!(ops.pump(ctx(false, true, applied)), None);
    }

    #[test]
    fn paused_selection_can_be_superseded() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Audio, Some(2), applied);
        assert!(ops.pump(ctx(true, true, applied)).is_some());
        let sn1 = gst::Seqnum::next();
        ops.selection_dispatched(sn1);
        let applied = sel(Some(0), Some(2), None);

        // A parked selection has no re-preroll to overlap with; the next
        // request replaces it instead of queueing behind it forever.
        ops.request(TrackKind::Audio, Some(1), applied);
        assert_eq!(
            ops.pump(ctx(true, true, applied)),
            Some(TrackOpCommand::SelectStreams(sel(Some(0), Some(1), None)))
        );
        let sn2 = gst::Seqnum::next();
        ops.selection_dispatched(sn2);

        // The stale confirmation must not settle the superseding one.
        ops.streams_selected(sn1);
        assert!(ops.selecting.is_some());
        ops.streams_selected(sn2);
        assert!(ops.selecting.is_none());
    }

    #[test]
    fn paused_refresh_retries_until_overlay_seen() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert!(ops.pump(ctx(true, true, applied)).is_some());
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));
        assert_eq!(
            ops.pump(ctx(true, true, applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        let rn1 = gst::Seqnum::next();
        ops.refresh_dispatched(rn1);

        // Old track's cue rendered before the selection confirmed: must not
        // count as success.
        ops.overlay_shown();
        assert!(!ops.overlay_seen);

        // Flush lost the race: preroll finished, selection confirmed, no cue.
        ops.streams_selected(sn);
        assert!(ops.refresh_done());
        assert!(ops.maybe_retry_refresh());
        assert_eq!(
            ops.pump(ctx(true, true, applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        ops.refresh_dispatched(gst::Seqnum::next());

        // This one rendered the cue; no further retry.
        ops.overlay_shown();
        assert!(ops.refresh_done());
        assert!(!ops.maybe_retry_refresh());
        assert_eq!(ops.pump(ctx(true, true, applied)), None);
    }

    #[test]
    fn paused_refresh_retry_budget_is_bounded() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert!(ops.pump(ctx(true, true, applied)).is_some());
        ops.selection_dispatched(gst::Seqnum::next());
        let applied = sel(Some(0), Some(1), Some(2));

        let mut flushes = 0;
        while ops.pump(ctx(true, true, applied)) == Some(TrackOpCommand::RefreshSeek) {
            flushes += 1;
            ops.refresh_dispatched(gst::Seqnum::next());
            assert!(ops.refresh_done());
            ops.maybe_retry_refresh();
            assert!(flushes < 100, "retry loop must be bounded");
        }
        assert_eq!(flushes, 1 + SUBTITLE_REFRESH_RETRIES as usize);
    }

    #[test]
    fn async_done_settles_refresh_by_exclusivity() {
        let mut ops = TrackOps::new();
        // No refresh out: an unrelated ASYNC_DONE is not a refresh completion.
        assert!(!ops.refresh_done());
        // GstBin's aggregated ASYNC_DONE carries a fresh seqnum, so the next
        // one settles the in-flight refresh regardless of seqnums.
        ops.refresh_dispatched(gst::Seqnum::next());
        assert!(ops.refresh_done());
        assert!(ops.refreshing.is_none());
    }

    #[test]
    fn watchdog_requeues_dropped_refresh_from_budget() {
        let mut ops = TrackOps::new();
        let applied = sel(Some(0), Some(1), None);
        ops.request(TrackKind::Subtitle, Some(2), applied);
        assert!(ops.pump(ctx(true, false, applied)).is_some());
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn);
        let applied = sel(Some(0), Some(1), Some(2));
        ops.streams_selected(sn);
        assert_eq!(
            ops.pump(ctx(true, false, applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        let stale = Instant::now() - TRACK_OP_WATCHDOG - Duration::from_secs(1);
        ops.refreshing = Some((gst::Seqnum::next(), stale));

        // The overlay was cleared for this switch; a dropped refresh must be
        // re-queued (bounded by the retry budget) or the cue stays gone until
        // the next boundary.
        ops.run_watchdog(false);
        assert!(ops.refreshing.is_none());
        assert!(ops.refresh_wanted);
    }

    #[test]
    fn watchdog_drops_stuck_selection_only_when_not_parked() {
        let mut ops = TrackOps::new();
        let stale = Instant::now() - TRACK_OP_WATCHDOG - Duration::from_secs(1);
        ops.selecting = Some((gst::Seqnum::next(), stale));
        // Parked paused selections legitimately wait for data flow.
        ops.run_watchdog(true);
        assert!(ops.selecting.is_some());
        // While playing a confirmation should have arrived long ago.
        ops.run_watchdog(false);
        assert!(ops.selecting.is_none());

        ops.refreshing = Some((gst::Seqnum::next(), stale));
        ops.run_watchdog(true);
        assert!(ops.refreshing.is_none());
    }
}
