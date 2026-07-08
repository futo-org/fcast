use anyhow::{Result, anyhow, bail};
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
                                self.state = State::SeekAsync {
                                    target_state: target,
                                    seek: *seek,
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
    /// An async state change or (flushing) seek finished prerolling.
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
    },
    RateChanged(f64),
    SeekFailed,
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
    selection_lock: BoolLock,
    work_tx: std::sync::mpsc::Sender<Job>,
    pub streams: Vec<Stream>,
    pub current_video_stream: Option<u32>,
    pub current_audio_stream: Option<u32>,
    pub current_subtitle_stream: Option<u32>,
    pub seekable: bool,
    state_machine: StateMachine,
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
            selection_lock: BoolLock::new(),
            work_tx,
            current_video_stream: None,
            current_audio_stream: None,
            current_subtitle_stream: None,
            seekable: false,
            state_machine: StateMachine::new(),
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
                }
            }
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
                }
                return;
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

        self.stream_collection = Some(collection);
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

    fn select_streams(
        &self,
        video: Option<u32>,
        audio: Option<u32>,
        subtitle: Option<u32>,
    ) -> Result<()> {
        if self.selection_lock.is_locked() {
            bail!("Stream selection is pending");
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

        let event = gst::event::SelectStreams::new(streams.iter().map(|s| s.as_str()));
        self.playbin.send_event(event);

        Ok(())
    }

    pub fn select_video_stream(&mut self, sid: Option<u32>) -> Result<()> {
        self.select_streams(sid, self.current_audio_stream, self.current_subtitle_stream)
    }

    pub fn select_audio_stream(&mut self, sid: Option<u32>) -> Result<()> {
        self.select_streams(self.current_video_stream, sid, self.current_subtitle_stream)
    }

    pub fn select_subtitle_stream(&mut self, sid: Option<u32>) -> Result<()> {
        self.select_streams(self.current_video_stream, self.current_audio_stream, sid)
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
        match self.state_machine.buffering(percent) {
            BufferingStateResult::Started(state) => {
                self.set_state_async(state);
                false
            }
            BufferingStateResult::Buffering => false,
            // BufferingStateResult::FinishedWithSeek(seek) => {
            BufferingStateResult::FinishedButWaitingSeek => {
                // let _ = self.work_tx.send(Job::Seek(seek));
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
        }
    }

    pub fn state_changed(
        &mut self,
        old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> Option<PlaybackState> {
        match self.state_machine.state_changed(old, new, pending) {
            StateChangeResult::NewPlaybackState(new_state) => return Some(new_state),
            StateChangeResult::Seek(seek) => {
                let _ = self.work_tx.send(Job::Seek(seek));
            }
            StateChangeResult::Waiting => (),
            StateChangeResult::ChangeState(state) => {
                self.set_state_async(state);
            }
        }

        None
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
    ) -> (Option<u32>, Option<u32>, Option<u32>) {
        self.selection_lock.release();

        debug!(?video_sid, ?audio_sid, ?subtitle_sid);

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
        assert_eq!(
            sm.state_changed(gs!(Ready), gs!(Paused), gs!(VoidPending)),
            StateChangeResult::Waiting
        );
        assert_eq!(
            sm.buffering(100),
            BufferingStateResult::FinishedButWaitingSeek,
            "queued async seek was silently dropped when Paused arrived mid-buffer",
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
}
