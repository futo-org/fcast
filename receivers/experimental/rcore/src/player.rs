use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use fcast_protocol::PlaybackState;
use gst::{glib::object::ObjectExt, prelude::*};
// use gst_gl::prelude::*;
use smallvec::SmallVec;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, debug_span, error, warn};

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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PlayerState {
    Paused,
    Playing,
    Buffering,
    Stopped,
}

type StreamId = String;

#[derive(Debug, PartialEq, Eq)]
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
    state: State,
    pub is_live: bool,
    position: Option<gst::ClockTime>,
    pub rate: f64,
    pub seekable: bool,
    pub current_uri: Option<String>,
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
            current_uri: None,
        }
    }

    fn queue_seek(&mut self, seek: Seek) {
        // tracing::info!("<<TEST>> sm.queue_seek({seek:?});");

        let target_state = match self.state {
            State::Buffering { target_state, .. }
            | State::Changing { target_state, .. }
            | State::SeekAsync { target_state, .. }
            | State::Seeking { target_state, .. } => target_state,
            // TODO: playling variants?
            _ => gst::State::Paused,
        };

        self.state = State::SeekAsync { seek, target_state };
    }

    #[must_use]
    fn seek_internal(&mut self, mut seek: Seek, target_state: Option<gst::State>) -> Option<Seek> {
        // tracing::info!("<<TEST>> assert_eq!(sm.seek_internal({seek:?}, {target_state:?}), TODO);");

        if self.is_live {
            warn!("Cannot seek when source is live");
            return None;
        }

        debug!(?seek, state = ?self.state, current_state = ?self.current_state, "Seek internal called");

        if seek.rate.is_none() {
            seek.rate = Some(self.rate);
        }

        let target_state = if let Some(ts) = target_state {
            ts
        } else {
            match self.state {
                State::Running {
                    state: RunningState::Playing,
                } => gst::State::Playing,
                State::Changing { target_state, .. } => target_state,
                State::SeekAsync { target_state, .. } => target_state,
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

    #[must_use]
    fn set_playback_state(&mut self, state: RunningState) -> Option<gst::State> {
        // #[rustfmt::skip] tracing::info!("<<TEST>> assert_eq!(sm.set_playback_state(RunningState::{state:?}), TODO);");

        let next_state: gst::State = state.into();
        match &mut self.state {
            State::Stopped => {
                error!("Cannot set playback state when the player is stopped");
                return None;
            }
            State::Buffering { target_state, .. } => *target_state = next_state,
            State::Changing { target_state, .. } => if *target_state != next_state {},
            State::SeekAsync { target_state, .. } => *target_state = next_state,
            State::Seeking { target_state, .. } => *target_state = next_state,
            State::Running { .. } => {
                self.state = State::Changing {
                    target_state: next_state,
                    pending_seek: None,
                };
                return Some(next_state);
            }
        }

        None
    }

    #[must_use]
    fn buffering(&mut self, new_percent: i32) -> BufferingStateResult {
        // tracing::info!("<<TEST>> assert_eq!(sm.buffering({new_percent}), TODO);");

        match &mut self.state {
            State::Stopped => {
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
    pub fn state_changed(
        &mut self,
        _old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> StateChangeResult {
        debug!(?new, ?pending, state = ?self.state, "State changed");
        // #[rustfmt::skip] tracing::info!("<<TEST>> assert_eq!(sm.state_changed(gs!({_old:?}), gs!({new:?}), gs!({pending:?})), TODO);");

        self.current_state = new;

        match &mut self.state {
            State::Stopped => {
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
                if new == gst::State::Paused && pending == gst::State::VoidPending {
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
                        // TODO: handle seek...
                        let target_state = *target_state;
                        if let Some(seek) = self.seek_internal(seek, Some(target_state)) {
                            return StateChangeResult::Seek(seek);
                        } else {
                            todo!()
                        }
                    } else {
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
                    }
                } else if pending == gst::State::VoidPending {
                    return StateChangeResult::ChangeState(*target_state);
                }

                // TODO: check if next state is void pending and then send new state change?
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

    fn clear_state(&mut self) {
        self.state = State::Stopped;
        self.is_live = false;
        self.position = None;
        self.rate = 1.0;
        self.seekable = false;
        self.current_uri = None;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Seek {
    pub position: Option<f64>,
    pub rate: Option<f64>,
}

impl Seek {
    pub fn new(position: Option<f64>, rate: Option<f64>) -> Self {
        Self { position, rate }
    }
}

#[derive(Debug)]
pub enum PlayerEvent {
    EndOfStream,
    UriLoaded,
    /// User must call Player::get_duration()
    DurationChanged,
    Tags(gst::TagList),
    VolumeChanged(f64),
    /// User must call Player::handle_stream_collection()
    StreamCollection(gst::StreamCollection),
    AboutToFinish,
    Buffering(i32),
    IsLive,
    StateChanged {
        old: gst::State,
        current: gst::State,
        pending: gst::State,
    },
    QueueSeek(Seek),
    StreamsSelected {
        video: Option<StreamId>,
        audio: Option<StreamId>,
        subtitle: Option<StreamId>,
    },
    RateChanged(f64),
    Error(String),
    Warning(String),
    UriSet(String),
}

#[derive(Debug)]
enum Job {
    SetState(gst::State),
    SetUri(String),
    Seek(Seek),
    Quit,
    UriWasSet,
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
                res += &title[0..title.len().min(16)];
                if title.len() >= 16 {
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

pub struct Player {
    pub playbin: gst::Element,
    seek_lock: BoolLock,
    volume_lock: BoolLock,
    selection_lock: BoolLock,
    work_tx: std::sync::mpsc::Sender<Job>,
    pub video_streams: SmallVec<[gst::Stream; 3]>,
    pub audio_streams: SmallVec<[gst::Stream; 3]>,
    pub subtitle_streams: SmallVec<[gst::Stream; 3]>,
    pub current_video_stream: i32,
    pub current_audio_stream: i32,
    pub current_subtitle_stream: i32,
    state_machine: StateMachine,
}

impl Player {
    pub fn new(
        video_sink: gst::Element,
        event_tx: UnboundedSender<crate::Event>,
        // contexts: Arc<Mutex<Option<(gst_gl::GLDisplay, gst_gl::GLContext)>>>,
    ) -> Result<Self> {
        let scaletempo = gst::ElementFactory::make("scaletempo").build()?;
        let playbin = gst::ElementFactory::make("playbin3")
            .property("video-sink", video_sink)
            // .property("video-sink", gst::ElementFactory::make("fakesink").build()?)
            // .property("video-sink", gst::ElementFactory::make("glimagesink").build()?)
            // .property("video-sink", gst::ElementFactory::make("gtk4paintablesink").build()?)
            // .property("video-sink", gst::ElementFactory::make("waylandsink").build()?)
            .property("audio-filter", scaletempo)
            .property_from_str(
                "flags",
                "deinterlace+buffering+soft-volume+text+audio+video",
            )
            // .property("text-sink", gst::ElementFactory::make("fakesink").build()?) // debugging
            // .property("instant-uri", true)
            .build()?;

        playbin.connect_notify(Some("volume"), {
            let event_tx = event_tx.clone();
            move |playbin, _pspec| {
                let _ = event_tx.send(crate::Event::NewPlayerEvent(PlayerEvent::VolumeChanged(
                    playbin.property::<f64>("volume"),
                )));
            }
        });

        playbin.connect_notify(Some("uri"), {
            let event_tx = event_tx.clone();
            move |playbin, _pspec| {
                let new_uri = playbin.property::<String>("uri");
                debug!(new_uri, "URI changed");
                let _ = event_tx.send(crate::Event::NewPlayerEvent(PlayerEvent::UriSet(new_uri)));
            }
        });

        playbin.connect("about-to-finish", false, {
            let event_tx = event_tx.clone();
            move |_| {
                let _ = event_tx.send(crate::Event::NewPlayerEvent(PlayerEvent::AboutToFinish));
                None
            }
        });

        let bus = playbin.bus().ok_or(anyhow!("playbin is missing a bus"))?;
        let playbin_weak = playbin.downgrade();
        let event_tx_c = event_tx.clone();
        bus.set_sync_handler(move |_, msg| {
            // Self::handle_messsage(&playbin_weak, &event_tx_c, msg, &contexts);
            Self::handle_messsage(&playbin_weak, &event_tx_c, msg);
            gst::BusSyncReply::Drop
        });

        let (work_tx, work_rx) = std::sync::mpsc::channel();

        // Handle certain operations in a background thread to avoid blocking and potentially tokio runtime conflicts
        std::thread::spawn({
            // Strong ref
            let playbin = playbin.clone();
            let event_tx = event_tx.clone();
            move || {
                let span = debug_span!("player-work-thread");
                let _entered = span.enter();

                while let Ok(job) = work_rx.recv() {
                    debug!(?job, "Got job");

                    match job {
                        Job::SetState(state) => {
                            let _ = playbin.set_state(state);
                        }
                        Job::SetUri(uri) => {
                            playbin.set_state(gst::State::Ready).unwrap();

                            playbin.set_property("uri", uri);
                            playbin.set_property("suburi", None::<String>);
                        }
                        Job::UriWasSet => {
                            if let Ok(success) = playbin.set_state(gst::State::Paused)
                                && success == gst::StateChangeSuccess::NoPreroll
                            {
                                debug!("Pipeline is live");
                                let _ = event_tx
                                    .send(crate::Event::NewPlayerEvent(PlayerEvent::IsLive));
                            }

                            let _ =
                                event_tx.send(crate::Event::NewPlayerEvent(PlayerEvent::UriLoaded));
                        }
                        Job::Seek(seek) => {
                            let (_, state, _) = playbin.state(None);

                            if state != gst::State::Paused {
                                let _ = event_tx.send(crate::Event::NewPlayerEvent(
                                    // PlayerEvent::QueueSeek(seconds),
                                    PlayerEvent::QueueSeek(seek),
                                ));
                                let _ = playbin.set_state(gst::State::Paused);
                                continue;
                            }

                            let position = match seek.position {
                                Some(pos) => gst::ClockTime::from_seconds_f64(pos),
                                None => {
                                    let Some(pos) = playbin.query_position::<gst::ClockTime>()
                                    else {
                                        error!("Failed to query playback position");
                                        continue;
                                    };

                                    pos
                                }
                            };

                            let rate = seek.rate.unwrap_or(1.0);

                            debug!(rate, ?position);

                            if let Err(err) = if rate >= 0.0 {
                                playbin.seek(
                                    rate,
                                    gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                                    gst::SeekType::Set,
                                    position,
                                    gst::SeekType::None,
                                    gst::ClockTime::NONE,
                                )
                            } else {
                                playbin.seek(
                                    rate,
                                    gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                                    gst::SeekType::Set,
                                    gst::ClockTime::ZERO,
                                    gst::SeekType::End,
                                    position,
                                )
                            } {
                                error!(?err, "Failed to set rate");
                            } else {
                                let _ = event_tx.send(crate::Event::NewPlayerEvent(
                                    PlayerEvent::RateChanged(rate),
                                ));
                            }
                        }
                        Job::Quit => {
                            break;
                        }
                    }
                }

                debug!("Work thread finished");
            }
        });

        work_tx.send(Job::SetState(gst::State::Ready))?;

        Ok(Self {
            playbin,
            seek_lock: BoolLock::new(),
            volume_lock: BoolLock::new(),
            selection_lock: BoolLock::new(),
            work_tx,
            video_streams: SmallVec::new(),
            audio_streams: SmallVec::new(),
            subtitle_streams: SmallVec::new(),
            current_video_stream: -1,
            current_audio_stream: -1,
            current_subtitle_stream: -1,
            state_machine: StateMachine::new(),
        })
    }

    fn handle_messsage(
        playbin_weak: &gst::glib::WeakRef<gst::Element>,
        event_tx: &UnboundedSender<crate::Event>,
        msg: &gst::Message,
        // contexts: &Arc<Mutex<Option<(gst_gl::GLDisplay, gst_gl::GLContext)>>>,
    ) {
        use gst::MessageView;

        let event = match msg.view() {
            // MessageView::NeedContext(ctx) => {
            //     let typ = ctx.context_type();
            //     debug!(typ, "Need context");
            //     if typ == *gst_gl::GL_DISPLAY_CONTEXT_TYPE {
            //         let contexts = contexts.lock().unwrap();
            //         let Some(contexts) = contexts.as_ref() else {
            //             error!("Missing contexts");
            //             return;
            //         };

            //         if let Some(element) = msg
            //             .src()
            //             .and_then(|source| source.downcast_ref::<gst::Element>())
            //         {
            //             let display_ctx = gst::Context::new(typ, true);
            //             display_ctx.set_gl_display(&contexts.0);
            //             debug!(display_type = ?contexts.0.handle_type());
            //             element.set_context(&display_ctx);
            //         }
            //     } else if typ == "gst.gl.app_context" {
            //         let contexts = contexts.lock().unwrap();
            //         let Some(contexts) = contexts.as_ref() else {
            //             error!("Missing contexts");
            //             return;
            //         };

            //         if let Some(element) = msg
            //             .src()
            //             .and_then(|source| source.downcast_ref::<gst::Element>())
            //         {
            //             let mut app_ctx = gst::Context::new(typ, true);
            //             let app_ctx_mut = app_ctx.get_mut().unwrap();
            //             let structure = app_ctx_mut.structure_mut();
            //             debug!(app_context_display_type = ?contexts.1.display().handle_type());
            //             structure.set("context", &contexts.1);
            //             element.set_context(&app_ctx);
            //         }
            //     }

            //     return;
            // }
            MessageView::Eos(_) => PlayerEvent::EndOfStream,
            MessageView::Error(error) => PlayerEvent::Error(error.error().message().to_string()),
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
            MessageView::Element(_element) => {
                // TODO: handle redirects?
                return;
            }
            MessageView::DurationChanged(_) => PlayerEvent::DurationChanged,
            MessageView::RequestState(state) => {
                if let Some(playbin) = playbin_weak.upgrade() {
                    let state = state.requested_state();
                    debug!(?state, "State requested");

                    // TODO: safe to do this here?
                    if let Err(err) = playbin.set_state(state) {
                        error!(?err, "Failed to set requested state");
                    }
                }

                return;
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
            _ => return,
        };

        let _ = event_tx.send(crate::Event::NewPlayerEvent(event));
    }

    pub fn handle_stream_collection(&mut self, collection: gst::StreamCollection) {
        self.video_streams.clear();
        self.audio_streams.clear();
        self.subtitle_streams.clear();

        for stream in collection.iter() {
            let typ = stream.stream_type();

            if typ.contains(gst::StreamType::VIDEO) {
                self.video_streams.push(stream);
            } else if typ.contains(gst::StreamType::AUDIO) {
                self.audio_streams.push(stream);
            } else if typ.contains(gst::StreamType::TEXT) {
                self.subtitle_streams.push(stream);
            }
        }
    }

    pub fn get_duration(&self) -> Option<gst::ClockTime> {
        self.playbin.query_duration()
    }

    pub fn get_position(&self) -> Option<gst::ClockTime> {
        self.playbin.query_position()
    }

    fn clear_state(&mut self) {
        self.video_streams.clear();
        self.audio_streams.clear();
        self.subtitle_streams.clear();
        self.current_video_stream = -1;
        self.current_audio_stream = -1;
        self.current_subtitle_stream = -1;
        self.seek_lock.release();
        self.volume_lock.release();
    }

    pub fn set_uri(&mut self, uri: &str) {
        self.clear_state();
        let _ = self.work_tx.send(Job::SetUri(uri.to_string()));
    }

    fn seek_internal(&mut self, seek: Seek) {
        if let Some(seek) = self.state_machine.seek_internal(seek, None) {
            let _ = self.work_tx.send(Job::Seek(seek));
        }
    }

    pub fn seek(&mut self, seconds: f64) {
        if !seconds.is_sign_positive() || seconds.is_nan() {
            warn!(seconds, "Invalid seek timestamp");
            return;
        }

        self.seek_internal(Seek {
            position: Some(seconds),
            rate: None,
        });
    }

    pub fn queue_seek(&mut self, seek: Seek) {
        self.state_machine.queue_seek(seek);
    }

    pub fn set_volume(&mut self, volume: f64) {
        if self.volume_lock.is_locked() {
            warn!("Volume change is pending");
            return;
        }

        self.playbin.set_property("volume", volume.clamp(0.0, 1.0));

        self.volume_lock.acquire();
    }

    pub fn volume_changed(&mut self) {
        self.volume_lock.release();
    }

    pub fn set_rate(&mut self, rate: f64) {
        self.seek_internal(Seek {
            position: None,
            rate: Some(rate),
        });
    }

    fn set_state_async(&self, state: gst::State) {
        let _ = self.work_tx.send(Job::SetState(state));
    }

    pub fn play(&mut self) {
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Playing) {
            self.set_state_async(state);
        }
    }

    pub fn dump_graph(&self) {
        use std::io::Write;

        let Some(bin) = self.playbin.downcast_ref::<gst::Bin>() else {
            // Unreachable
            error!("Playbin is not a bin");
            return;
        };

        let graph = bin.debug_to_dot_data(gst::DebugGraphDetails::all());

        fn post(graph: &[u8]) -> anyhow::Result<()> {
            #[cfg(target_os = "android")]
            let sockaddr = option_env!("PIPELINE_DBG_HOST").unwrap_or("127.0.0.1:3000");
            #[cfg(not(target_os = "android"))]
            let sockaddr =
                std::env::var("PIPELINE_DBG_HOST").unwrap_or("127.0.0.1:3000".to_owned());

            let mut stream = std::net::TcpStream::connect(sockaddr)?;
            let len_buf = (graph.len() as u32).to_le_bytes();
            stream.write_all(&len_buf)?;
            stream.write_all(graph)?;
            stream.shutdown(std::net::Shutdown::Both)?;

            Ok(())
        }

        if let Err(err) = post(graph.as_bytes()) {
            error!(?err, "Failed to post graph data");
        }
    }

    pub fn pause(&mut self) {
        self.dump_graph();

        if let Some(state) = self.state_machine.set_playback_state(RunningState::Paused) {
            self.set_state_async(state);
        }
    }

    pub fn stop(&mut self) {
        if self.state_machine.current_state != gst::State::Ready
            && self.state_machine.current_state != gst::State::Null
        {
            debug!("Stopping playback");
            self.set_state_async(gst::State::Null);
            self.state_machine.clear_state();
            self.clear_state();
        }
    }

    fn select_streams(&self, video: i32, audio: i32, subtitle: i32) -> Result<()> {
        if self.selection_lock.is_locked() {
            bail!("Stream selection is pending");
        }

        fn stream_id_from_idx(idx: i32, streams: &[gst::Stream]) -> Option<gst::glib::GString> {
            if idx >= 0
                && let Some(stream) = streams.get(idx as usize)
            {
                return stream.stream_id();
            }

            None
        }

        let mut streams = vec![];
        if let Some(vid) = stream_id_from_idx(video, &self.video_streams) {
            streams.push(vid);
        }
        if let Some(aud) = stream_id_from_idx(audio, &self.audio_streams) {
            streams.push(aud);
        }
        if let Some(sub) = stream_id_from_idx(subtitle, &self.subtitle_streams) {
            streams.push(sub);
        }

        let event = gst::event::SelectStreams::new(streams.iter().map(|s| s.as_str()));
        self.playbin.send_event(event);

        Ok(())
    }

    pub fn select_video_stream(&mut self, sid: i32) -> Result<()> {
        self.select_streams(sid, self.current_audio_stream, self.current_subtitle_stream)
    }

    pub fn select_audio_stream(&mut self, sid: i32) -> Result<()> {
        self.select_streams(self.current_video_stream, sid, self.current_subtitle_stream)
    }

    pub fn select_subtitle_stream(&mut self, sid: i32) -> Result<()> {
        self.select_streams(self.current_video_stream, self.current_audio_stream, sid)
    }

    pub fn end_of_stream_reached(&mut self) {
        self.stop();
    }

    pub fn uri_set(&mut self, uri: String) {
        let _ = self.work_tx.send(Job::UriWasSet);
        self.state_machine.current_uri = Some(uri);
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

    fn stream_from_id_in<'a>(
        &'a self,
        id: &str,
        streams: &'a [gst::Stream],
    ) -> Option<&'a gst::Stream> {
        for stream in streams {
            if let Some(sid) = stream.stream_id()
                && sid == id
            {
                return Some(stream);
            }
        }

        None
    }

    pub fn get_stream_from_id(&self, id: &str) -> Option<&gst::Stream> {
        self.stream_from_id_in(id, &self.video_streams).or(self
            .stream_from_id_in(id, &self.audio_streams)
            .or(self.stream_from_id_in(id, &self.subtitle_streams)))
    }

    pub fn have_media_info(&self) -> bool {
        !self.video_streams.is_empty()
            || !self.audio_streams.is_empty()
            || !self.subtitle_streams.is_empty()
    }

    fn find_stream_idx(sid: &str, streams: &[gst::Stream]) -> i32 {
        for (idx, stream) in streams.iter().enumerate() {
            if let Some(this_id) = stream.stream_id()
                && this_id == sid
            {
                return idx as i32;
            }
        }

        -1
    }

    pub fn streams_selected(
        &mut self,
        video_sid: Option<&str>,
        audio_sid: Option<&str>,
        subtitle_sid: Option<&str>,
    ) -> (i32, i32, i32) {
        self.selection_lock.release();

        if let Some(video) = video_sid {
            self.current_video_stream = Self::find_stream_idx(video, &self.video_streams);
        }
        if let Some(audio) = audio_sid {
            self.current_audio_stream = Self::find_stream_idx(audio, &self.audio_streams);
        }
        if let Some(subtitle) = subtitle_sid {
            self.current_subtitle_stream = Self::find_stream_idx(subtitle, &self.subtitle_streams);
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
            State::Buffering { .. }
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

    pub fn set_rate_changed(&mut self, rate: f64) {
        self.state_machine.rate = rate;
    }

    pub fn current_uri(&self) -> Option<&str> {
        self.state_machine.current_uri.as_deref()
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

    #[test]
    #[rustfmt::skip]
    fn basic_playback() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(0.0), None), None), Some(Seek::new(Some(0.0), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(0.0), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(0.0), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)),);
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_2() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(0.0), None), None), Some(Seek::new(Some(0.0), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(0.0), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(0.0), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(0.0), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(0.0), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(Paused)), new_ps!(Playing),);

        // 2nd seek:
        assert_eq!(sm.seek_internal(Seek::new(Some(60.0), None), None), Some(Seek::new(Some(60.0), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(60.0), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(60.0), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(60.0), Some(1.0)));
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(60.0), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Paused)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending),), StateChangeResult::ChangeState(gs!(Playing)),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(Paused)), new_ps!(Playing),);
    }

    #[test]
    #[rustfmt::skip]
    fn basic_playback_3() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek {position: Some(0.0), rate: None}, None), Some(Seek::new(Some(0.0), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.buffering(1), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek);
        assert_eq!(sm.state_changed(gs!(Ready), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting);
        assert_eq!(sm.state_changed(gs!(Paused), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting);
        sm.queue_seek(Seek { position: Some(0.0), rate: Some(1.0) });
        assert_eq!(sm.state_changed(gs!(Playing), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(0.0), Some(1.0))));
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
        assert_eq!(sm.state_changed(gs!(Null), gs!(Ready), gs!(VoidPending)), StateChangeResult::Waiting);
        assert_eq!(sm.seek_internal(Seek::new(Some(0.0), None), None), Some(Seek::new(Some(0.0), Some(1.0))));
        assert_eq!(sm.set_playback_state(RunningState::Playing), None);
        assert_eq!(sm.state, State::Seeking {target_state: RunningState::Playing.into()});
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(Playing)), StateChangeResult::Waiting,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Playing), gs!(VoidPending)), StateChangeResult::Waiting,);
        sm.queue_seek(Seek::new(Some(0.0), Some(1.0)));
        assert!(matches!(sm.state, State::SeekAsync { seek: _, target_state: gs!(Playing) }));
        assert_eq!(sm.buffering(0), BufferingStateResult::Started(gs!(Paused)));
        assert!(matches!(sm.state, State::Buffering { .. }));
        assert_eq!(sm.seek_internal(Seek::new(Some(60.0), None), None), None);
        assert_eq!(sm.buffering(100), BufferingStateResult::FinishedButWaitingSeek,);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::Seek(Seek::new(Some(60.0), Some(1.0))),);
        assert_eq!(sm.state_changed(gs!(Null), gs!(Paused), gs!(VoidPending)), StateChangeResult::ChangeState(gs!(Playing)),);
    }
}
