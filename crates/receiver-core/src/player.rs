use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use fcast_protocol::PlaybackState;
use gst::{glib::object::ObjectExt, prelude::*};
use tracing::{debug, debug_span, error, instrument, warn};

use crate::MessageSender;

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
                if new == *target_state {
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
            TrackKind::Subtitle => desired.subtitle = id,
        }
        self.pending = Some(desired);
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
                if desired.subtitle.is_some() {
                    // Enable/switch: re-emit the new track's current cue once
                    // the selection settles.
                    self.refresh_wanted = true;
                    self.refresh_retries_left = SUBTITLE_REFRESH_RETRIES;
                } else {
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
    }

    fn has_dispatchable_work(&self) -> bool {
        self.pending.is_some() || self.refresh_wanted
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
    SetUri(String),
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
    pub streams: Vec<Stream>,
    pub current_video_stream: Option<u32>,
    pub current_audio_stream: Option<u32>,
    pub current_subtitle_stream: Option<u32>,
    pub seekable: bool,
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
                            Job::SetUri(uri) => {
                                let _ = playbin.set_state(gst::State::Ready);

                                playbin.set_property("uri", uri);
                                playbin.set_property("suburi", None::<String>);

                                if let Ok(success) = playbin.set_state(gst::State::Paused)
                                    && success == gst::StateChangeSuccess::NoPreroll
                                {
                                    debug!("Pipeline is live");
                                    msg_tx.player(PlayerEvent::IsLive);
                                }

                                msg_tx.player(PlayerEvent::UriLoaded);
                            }
                            Job::Seek(seek) => {
                                let (_, state, _) = playbin.state(None);

                                if state != gst::State::Paused {
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
            current_video_stream: None,
            current_audio_stream: None,
            current_subtitle_stream: None,
            seekable: false,
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

    pub fn handle_stream_collection(
        &mut self,
        collection: gst::StreamCollection,
        msg_tx: MessageSender,
    ) {
        self.cleanup_stream_collection();

        self.stream_collection_notify = Some(collection.connect_stream_notify(
            None,
            move |_collection, _stream, param| {
                if param.name() == "tags" {
                    msg_tx.player(PlayerEvent::StreamTagsUpdated);
                }
            },
        ));

        self.streams.clear();

        for stream in collection.iter() {
            let title = stream_title(&stream);
            let stream = Stream {
                inner: stream,
                title,
            };

            self.streams.push(stream);
        }

        // Seed the current selection with playbin3's defaults (the first stream
        // of each type) so a track change that arrives before the initial
        // `StreamsSelected` message still keeps the other streams selected
        // instead of dropping them. The real `StreamsSelected` corrects these
        // the moment it arrives. Only seed slots that are still unset so a
        // later collection update (e.g. an added subtitle source) never
        // clobbers a selection the user already made.
        self.current_video_stream = self
            .current_video_stream
            .or_else(|| self.first_stream_of(gst::StreamType::VIDEO));
        self.current_audio_stream = self
            .current_audio_stream
            .or_else(|| self.first_stream_of(gst::StreamType::AUDIO));
        self.current_subtitle_stream = self
            .current_subtitle_stream
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
        self.volume_lock.release();
        self.track_ops.reset();
    }

    pub fn set_uri(&mut self, uri: &str) {
        self.clear_state();
        self.state_machine.clear_state();
        let _ = self.work_tx.send(Job::SetUri(uri.to_string()));
        self.state_machine.state = State::PendingUriChange;
    }

    fn seek_internal(&mut self, seek: Seek) {
        if let Some(rate) = seek.rate
            && !Seek::rate_is_safe(rate)
        {
            warn!(rate, "Ignoring invalid seek rate");
            return;
        }

        if self.seekable {
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
        let applied = self.applied_track_selection();
        let stale_cue =
            kind == TrackKind::Subtitle && applied.subtitle.is_some() && id != applied.subtitle;
        self.track_ops.request(kind, id, applied);
        self.pump_track_ops();
        stale_cue
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
            warn!("Volume change is pending");
            return;
        }

        self.playbin
            .set_property("volume", (volume as f64).clamp(0.0, 1.0));

        self.volume_lock.acquire();
    }

    pub fn volume_changed(&mut self) {
        self.volume_lock.release();
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
        let res = match self.state_machine.state_changed(old, new, pending) {
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
        };

        // The pipeline may just have settled; dispatch queued track work.
        self.pump_track_ops();

        res
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
