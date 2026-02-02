use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use gst::{
    glib::object::ObjectExt,
    prelude::{ElementExt, ElementExtManual},
};
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
    QueueSeek(f64),
    StreamsSelected {
        video: Option<StreamId>,
        audio: Option<StreamId>,
        subtitle: Option<StreamId>,
    },
    RateChanged(f64),
    Error(String),
    Warning(String),
}

#[derive(Debug)]
enum Job {
    SetState(gst::State),
    SetUri(String),
    SetRate(f64),
    Seek(f64),
    Quit,
}

#[derive(Debug)]
pub enum StateChangeResult {
    Changed,
    SeekPending,
    SeekCompleted,
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
    current_state: gst::State,
    is_buffering: bool,
    pub is_live: bool,
    pub player_state: PlayerState,
    position: Option<gst::ClockTime>,
    work_tx: std::sync::mpsc::Sender<Job>,
    pending_seek: Option<f64>,
    target_state: Option<gst::State>,
    pub video_streams: SmallVec<[gst::Stream; 3]>,
    pub audio_streams: SmallVec<[gst::Stream; 3]>,
    pub subtitle_streams: SmallVec<[gst::Stream; 3]>,
    pub rate: f64,
    pub current_video_stream: i32,
    pub current_audio_stream: i32,
    pub current_subtitle_stream: i32,
    pub seekable: bool,
}

impl Player {
    pub fn new(video_sink: gst::Element, event_tx: UnboundedSender<crate::Event>) -> Result<Self> {
        let scaletempo = gst::ElementFactory::make("scaletempo").build()?;
        let playbin = gst::ElementFactory::make("playbin3")
            .property("video-sink", video_sink)
            .property("audio-filter", scaletempo)
            .build()?;

        playbin.connect_notify(Some("volume"), {
            let event_tx = event_tx.clone();
            move |playbin, _pspec| {
                let _ = event_tx.send(crate::Event::NewPlayerEvent(PlayerEvent::VolumeChanged(
                    playbin.property::<f64>("volume"),
                )));
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
        tokio::spawn({
            let playbin_weak = playbin.downgrade();
            let event_tx = event_tx.clone();
            async move {
                let mut messages = bus.stream();
                while let Some(msg) = messages.next().await {
                    Self::handle_messsage(&playbin_weak, &event_tx, msg);
                }
            }
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

                            let _ =
                                event_tx.send(crate::Event::NewPlayerEvent(PlayerEvent::UriLoaded));

                            if let Ok(success) = playbin.set_state(gst::State::Paused)
                                && success == gst::StateChangeSuccess::NoPreroll
                            {
                                debug!("Pipeline is live");
                                let _ = event_tx
                                    .send(crate::Event::NewPlayerEvent(PlayerEvent::IsLive));
                            }
                        }
                        Job::SetRate(rate) => {
                            let Some(position) = playbin.query_position::<gst::ClockTime>() else {
                                error!("Failed to query playback position");
                                continue;
                            };

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
                        Job::Seek(seconds) => {
                            let (_, state, _) = playbin.state(None);

                            if state != gst::State::Paused {
                                let _ = event_tx.send(crate::Event::NewPlayerEvent(
                                    PlayerEvent::QueueSeek(seconds),
                                ));
                                let _ = playbin.set_state(gst::State::Paused);
                                continue;
                            }

                            if let Err(err) = playbin.seek_simple(
                                gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                                gst::ClockTime::from_seconds_f64(seconds),
                            ) {
                                error!(seconds, ?err, "Failed to seek");
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
            current_state: gst::State::Ready,
            is_buffering: false,
            is_live: false,
            position: None,
            rate: 1.0,
            work_tx,
            pending_seek: None,
            target_state: None,
            video_streams: SmallVec::new(),
            audio_streams: SmallVec::new(),
            subtitle_streams: SmallVec::new(),
            player_state: PlayerState::Stopped,
            current_video_stream: -1,
            current_audio_stream: -1,
            current_subtitle_stream: -1,
            seekable: true,
        })
    }

    fn handle_messsage(
        playbin_weak: &gst::glib::WeakRef<gst::Element>,
        event_tx: &UnboundedSender<crate::Event>,
        msg: gst::Message,
    ) {
        use gst::MessageView;

        let event = match msg.view() {
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
        self.is_buffering = false;
        self.is_live = false;
        self.player_state = PlayerState::Stopped;
        self.video_streams.clear();
        self.audio_streams.clear();
        self.subtitle_streams.clear();
        self.current_video_stream = -1;
        self.current_audio_stream = -1;
        self.current_subtitle_stream = -1;
        self.position = None;
        self.pending_seek = None;
        self.target_state = None;
        self.seek_lock.release();
        self.volume_lock.release();
        self.seekable = true;
    }

    pub fn set_uri(&mut self, uri: &str) {
        self.clear_state();
        let _ = self.work_tx.send(Job::SetUri(uri.to_string()));
    }

    pub fn seek(&mut self, seconds: f64) {
        if self.seek_lock.is_locked() {
            warn!("Cannot seek because a seek request is pending");
            return;
        }

        if !seconds.is_sign_positive() || seconds.is_nan() {
            warn!(seconds, "Invalid seek timestamp");
            return;
        }

        let _ = self.work_tx.send(Job::Seek(seconds));

        self.seek_lock.acquire();
    }

    pub fn queue_seek(&mut self, seconds: f64) {
        if self.target_state.is_none() {
            self.target_state = Some(self.current_state);
        }
        self.pending_seek = Some(seconds);
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
        let _ = self.work_tx.send(Job::SetRate(rate));
    }

    fn set_state_async(&self, state: gst::State) {
        let _ = self.work_tx.send(Job::SetState(state));
    }

    pub fn play(&mut self) {
        if self.pending_seek.is_some() || self.seek_lock.is_locked() {
            self.target_state = Some(gst::State::Playing);
        } else {
            self.set_state_async(gst::State::Playing);
        }
    }

    pub fn pause(&mut self) {
        if self.pending_seek.is_some() || self.seek_lock.is_locked() {
            self.target_state = Some(gst::State::Paused);
        } else {
            self.set_state_async(gst::State::Paused);
        }
    }

    pub fn stop(&mut self) {
        self.set_state_async(gst::State::Ready);
        self.clear_state();
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

    pub fn uri_loaded(&mut self) {
        // self.set_state_async(gst::State::Paused);
        self.set_state_async(gst::State::Playing);
    }

    pub fn state_changed(
        &mut self,
        old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> StateChangeResult {
        debug!(?old, ?new, ?pending, "Changed state");

        self.current_state = new;

        let new_state = if new == gst::State::Paused && pending == gst::State::VoidPending {
            if let Some(seconds) = self.pending_seek.take() {
                // TODO: check if media is actually seekable
                self.seek_lock.release();
                self.seek(seconds);
                return StateChangeResult::SeekPending;
            } else if self.seek_lock.is_locked() {
                tracing::info!("Seek completed");
                if let Some(target_state) = self.target_state.take() {
                    debug!(
                        ?target_state,
                        "Setting state because we have a target state"
                    );
                    self.set_state_async(target_state);
                } else {
                    self.player_state = PlayerState::Paused;
                }
                self.seek_lock.release();
                return StateChangeResult::SeekCompleted;
            }

            PlayerState::Paused
        } else if new == gst::State::Paused && self.seek_lock.is_locked() {
            return StateChangeResult::SeekPending;
        } else if new == gst::State::Playing && pending == gst::State::VoidPending {
            let mut query = gst::query::Seeking::new(gst::Format::Time);
            if self.playbin.query(&mut query) {
                use gst::QueryView;
                if let QueryView::Seeking(seeking) = query.view() {
                    let (seekable, _, _) = seeking.result();
                    self.seekable = seekable;
                }
            }

            PlayerState::Playing
        } else if new == gst::State::Ready && old > gst::State::Ready {
            PlayerState::Stopped
        } else {
            PlayerState::Buffering
        };

        self.player_state = new_state;

        StateChangeResult::Changed
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
}

impl Drop for Player {
    fn drop(&mut self) {
        self.set_state_async(gst::State::Null);
        let _ = self.work_tx.send(Job::Quit);
    }
}
