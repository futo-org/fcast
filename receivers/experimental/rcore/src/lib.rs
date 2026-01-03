use anyhow::{Context, Result, bail};
use base64::Engine;
use common::Packet;
use fcast_protocol::{
    PlaybackState, SeekMessage, SetSpeedMessage, SetVolumeMessage,
    v2::{PlayMessage, PlaybackUpdateMessage, VolumeUpdateMessage},
};
use gst::prelude::*;
use tracing::{Instrument, debug, error, level_filters::LevelFilter};
use futures::StreamExt;
use session::{SessionDriver, SessionId};
use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::{Duration, Instant},
};

pub use slint;

use crate::session::Operation;

pub mod fcastwhepsrcbin;
// pub mod pipeline;
pub mod session;
pub mod video;

pub mod common {
    use std::sync::OnceLock;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::tcp::{ReadHalf, WriteHalf},
        runtime::Runtime,
    };

    pub const HEADER_BUFFER_SIZE: usize = 5;
    pub const MAX_BODY_SIZE: u32 = 32000 - 1;

    pub fn runtime() -> &'static Runtime {
        static RUNTIME: OnceLock<Runtime> = OnceLock::new();
        RUNTIME.get_or_init(|| Runtime::new().unwrap())
    }

    use anyhow::{Context, bail};

    use fcast_protocol::{
        Opcode, PlaybackErrorMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage,
        VersionMessage,
        v2::{PlayMessage, PlaybackUpdateMessage, VolumeUpdateMessage},
        v3::InitialReceiverMessage,
    };

    #[derive(Debug, PartialEq)]
    pub struct Header {
        pub size: u32,
        pub opcode: Opcode,
    }

    impl Header {
        pub fn new(opcode: Opcode, size: u32) -> Self {
            Self {
                size: size + 1,
                opcode,
            }
        }

        pub fn decode(buf: [u8; 5]) -> Self {
            Self {
                size: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) - 1,
                opcode: Opcode::try_from(buf[4]).unwrap(),
            }
        }

        pub fn encode(&self) -> [u8; 5] {
            let size_slice = u32::to_le_bytes(self.size);
            [
                size_slice[0],
                size_slice[1],
                size_slice[2],
                size_slice[3],
                self.opcode as u8,
            ]
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Packet {
        None,
        Play(PlayMessage),
        Pause,
        Resume,
        Stop,
        Seek(SeekMessage),
        PlaybackUpdate(PlaybackUpdateMessage),
        VolumeUpdate(VolumeUpdateMessage),
        SetVolume(SetVolumeMessage),
        PlaybackError(PlaybackErrorMessage),
        SetSpeed(SetSpeedMessage),
        Version(VersionMessage),
        Ping,
        Pong,
        Initial(InitialReceiverMessage),
    }

    impl From<&Packet> for Opcode {
        fn from(value: &Packet) -> Self {
            match value {
                Packet::None => Opcode::None,
                Packet::Play(_) => Opcode::Play,
                Packet::Pause => Opcode::Pause,
                Packet::Resume => Opcode::Resume,
                Packet::Stop => Opcode::Stop,
                Packet::Seek(_) => Opcode::Seek,
                Packet::PlaybackUpdate(_) => Opcode::PlaybackUpdate,
                Packet::VolumeUpdate(_) => Opcode::VolumeUpdate,
                Packet::SetVolume(_) => Opcode::SetVolume,
                Packet::PlaybackError(_) => Opcode::PlaybackError,
                Packet::SetSpeed(_) => Opcode::SetSpeed,
                Packet::Version(_) => Opcode::Version,
                Packet::Ping => Opcode::Ping,
                Packet::Pong => Opcode::Pong,
                Packet::Initial(_) => Opcode::Initial,
            }
        }
    }

    impl From<PlaybackErrorMessage> for Packet {
        fn from(value: PlaybackErrorMessage) -> Packet {
            Packet::PlaybackError(value)
        }
    }

    impl From<PlaybackUpdateMessage> for Packet {
        fn from(value: PlaybackUpdateMessage) -> Self {
            Self::PlaybackUpdate(value)
        }
    }

    impl From<VolumeUpdateMessage> for Packet {
        fn from(value: VolumeUpdateMessage) -> Self {
            Packet::VolumeUpdate(value)
        }
    }

    impl From<PlayMessage> for Packet {
        fn from(value: PlayMessage) -> Self {
            Packet::Play(value)
        }
    }

    impl Packet {
        pub fn decode(header: Header, body: &str) -> anyhow::Result<Self> {
            Ok(match header.opcode {
                Opcode::None => Self::None,
                Opcode::Play => Self::Play(serde_json::from_str(body).context("Play")?),
                Opcode::Pause => Self::Pause,
                Opcode::Resume => Self::Resume,
                Opcode::Stop => Self::Stop,
                Opcode::Seek => Self::Seek(serde_json::from_str(body)?),
                Opcode::PlaybackUpdate => {
                    Self::PlaybackUpdate(serde_json::from_str(body).context("PlaybackUpdate")?)
                }
                Opcode::VolumeUpdate => {
                    Self::VolumeUpdate(serde_json::from_str(body).context("VolumeUpdate")?)
                }
                Opcode::SetVolume => {
                    Self::SetVolume(serde_json::from_str(body).context("SetVolume")?)
                }
                Opcode::PlaybackError => {
                    Self::PlaybackError(serde_json::from_str(body).context("PlaybackError")?)
                }
                Opcode::SetSpeed => Self::SetSpeed(serde_json::from_str(body).context("SetSpeed")?),
                Opcode::Version => Self::Version(serde_json::from_str(body).context("Version")?),
                Opcode::Ping => Self::Ping,
                Opcode::Pong => Self::Pong,
                _ => bail!("Unsupported opcode: {:?}", header.opcode),
            })
        }

        pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
            let body = match self {
                Packet::Play(play_msg) => serde_json::to_string(&play_msg)?.into_bytes(),
                Packet::Seek(seek_msg) => serde_json::to_string(&seek_msg)?.into_bytes(),
                Packet::PlaybackUpdate(playback_update_msg) => {
                    serde_json::to_string(&playback_update_msg)?.into_bytes()
                }
                Packet::VolumeUpdate(volume_update_msg) => {
                    serde_json::to_string(&volume_update_msg)?.into_bytes()
                }
                Packet::SetVolume(set_volume_msg) => {
                    serde_json::to_string(&set_volume_msg)?.into_bytes()
                }
                Packet::PlaybackError(playback_error_msg) => {
                    serde_json::to_string(&playback_error_msg)?.into_bytes()
                }
                Packet::SetSpeed(set_speed_msg) => {
                    serde_json::to_string(&set_speed_msg)?.into_bytes()
                }
                Packet::Version(version_msg) => serde_json::to_string(&version_msg)?.into_bytes(),
                Packet::Initial(initial_msg) => serde_json::to_string(&initial_msg)?.into_bytes(),
                _ => Vec::new(),
            };

            assert!(body.len() < 32 * 1000);
            let header = Header::new(self.into(), body.len() as u32).encode();
            let mut pack = header.to_vec();
            pack.extend_from_slice(&body);
            Ok(pack)
        }
    }

    /// Attempt to read and decode FCast packet from `stream`.
    pub async fn read_packet(stream: &mut ReadHalf<'_>) -> anyhow::Result<Packet> {
        let mut header_buf: [u8; HEADER_BUFFER_SIZE] = [0; HEADER_BUFFER_SIZE];

        stream.read_exact(&mut header_buf).await?;

        let header = Header::decode(header_buf);

        let mut body_string = String::new();

        if header.size > 0 {
            let mut body_buf = vec![0; header.size as usize];
            stream.read_exact(&mut body_buf).await?;
            body_string = String::from_utf8(body_buf)?;
        }

        Packet::decode(header, &body_string)
    }

    pub async fn write_packet(stream: &mut WriteHalf<'_>, packet: Packet) -> anyhow::Result<()> {
        let bytes = packet.encode()?;
        stream.write_all(&bytes).await?;
        Ok(())
    }
}

#[derive(Debug)]
enum PlayerEvent {
    UriLoaded,
    StateChanged(gst_play::PlayState),
    MediaInfoUpdated(gst_play::PlayMediaInfo),
    DurationChanged(gst::ClockTime),
    PositionChanged(gst::ClockTime),
    VolumeChanged(f64),
    Eos,
}

#[derive(Debug)]
pub enum Event {
    Stop,
    SetSpeed(SetSpeedMessage),
    SetVolume(SetVolumeMessage),
    Quit,
    PipelineEos,
    PipelineError,
    SessionFinished,
    ResumeOrPause,
    SeekPercent(f32),
    PipelineStateChanged(gst::State),
    ToggleDebug,
    Player(PlayerEvent),
    Op {
        session_id: SessionId,
        op: Operation,
    },
}

#[macro_export]
macro_rules! log_if_err {
    ($res:expr) => {
        if let Err(err) = $res {
            error!("{err}");
        }
    };
}

const FCAST_TCP_PORT: u16 = 46899;
const SENDER_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

slint::include_modules!();

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[derive(Debug)]
enum OnUriLoadedCommand {
    Seek(f64),
    Rate(f64),
    Volume(f64),
}

struct Application {
    // pipeline: Pipeline,
    event_tx: Sender<Event>,
    ui_weak: slint::Weak<MainWindow>,
    updates_tx: broadcast::Sender<Arc<Vec<u8>>>,
    mdns: mdns_sd::ServiceDaemon,
    last_sent_update: Instant,
    debug_mode: bool,
    player: gst_play::Play,
    player_state: gst_play::PlayState,
    current_media: Option<gst_play::PlayMediaInfo>,
    current_duration: Option<gst::ClockTime>,
    on_playing_command_queue: smallvec::SmallVec<[OnUriLoadedCommand; 6]>,
}

fn get_all_available_addrs_ignore_v6_and_localhost() -> Result<Vec<Ipv4Addr>> {
    let mut ips: Vec<Ipv4Addr> = Vec::new();
    for iface in getifaddrs::getifaddrs()? {
        if let Some(ip_addr) = iface.address.ip_addr() {
            match ip_addr {
                std::net::IpAddr::V4(v4) if !v4.is_loopback() => ips.push(v4),
                std::net::IpAddr::V6(v6) if !v6.is_loopback() => {
                    debug!("Ignoring IPv6 address ({v6:?})")
                }
                _ => debug!("Ignoring loopback IP address ({ip_addr:?})"),
            }
        }
    }
    Ok(ips)
}

impl Application {
    pub async fn new(
        // win_handle_rx: std::sync::mpsc::Receiver<usize>,
        // win_handle_rx: std::sync::mpsc::Receiver<(RawDisplayHandle, usize)>,
        appsink: gst::Element,
        // TODO: should be a unbounded channel
        event_tx: Sender<Event>,
        ui_weak: slint::Weak<MainWindow>,
    ) -> Result<Self> {
        // let pipeline = Pipeline::new(appsink, event_tx.clone()).await?;

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        debug!("Finished sleeping");

        let video_renderer = gst_play::PlayVideoOverlayVideoRenderer::with_sink(&appsink);
        // let (handle, win) = win_handle_rx.recv().unwrap();
        // let video_renderer = unsafe { gst_play::PlayVideoOverlayVideoRenderer::new(win) };

        let player =
            gst_play::Play::new(Some(video_renderer.upcast::<gst_play::PlayVideoRenderer>()));

        let mut player_config = player.config();
        player_config.set_position_update_interval(250);
        player
            .set_config(player_config)
            .context("Failed to set gst player config")?;

        let player_playbin = player.pipeline();
        player_playbin.connect("element-setup", false, |vals| {
            let Ok(elem) = vals[1].get::<gst::Element>() else {
                return None;
            };

            if let Some(factory) = elem.factory()
                && factory.name() == "rtspsrc"
            {
                elem.set_property("latency", 25u32);
            }

            if let Some(factory) = elem.factory()
                && factory.name() == "webrtcbin"
            {
                elem.set_property("latency", 1u32);
            }

            None
        });

        // TODO: gifs should not be played by gstreamer (crashes)
        // TODO: images should not be played by gstreamer (overkill)

        tokio::spawn({
            let player_bus = player.message_bus();
            // let player_weak = player.downgrade();
            let event_tx = event_tx.clone();

            async move {
                let mut messages = player_bus.stream();

                while let Some(msg) = messages.next().await {
                    let Ok(play_message) = gst_play::PlayMessage::parse(&msg) else {
                        continue;
                    };

                    // debug!("Play message: {play_message:?}");

                    match play_message {
                        gst_play::PlayMessage::UriLoaded(loaded) => {
                            debug!("URI loaded uri={}", loaded.uri());
                            let _ = event_tx.send(Event::Player(PlayerEvent::UriLoaded)).await;
                            // if let Some(player) = player_weak.upgrade() {
                            //     player.play();
                            // }
                        }
                        gst_play::PlayMessage::PositionUpdated(update) => {
                            if let Some(position) = update.position() {
                                let _ = event_tx
                                    .send(Event::Player(PlayerEvent::PositionChanged(position)))
                                    .await;
                            }
                        }
                        gst_play::PlayMessage::DurationChanged(update) => {
                            if let Some(duration) = update.duration() {
                                let _ = event_tx
                                    .send(Event::Player(PlayerEvent::DurationChanged(duration)));
                            }
                        }
                        gst_play::PlayMessage::StateChanged(state) => {
                            let _ = event_tx
                                .send(Event::Player(PlayerEvent::StateChanged(state.state())))
                                .await;
                        }
                        // gst_play::PlayMessage::Buffering(buffering) => todo!(),
                        gst_play::PlayMessage::EndOfStream(_) => {
                            let _ = event_tx.send(Event::Player(PlayerEvent::Eos)).await;
                        }
                        // gst_play::PlayMessage::Error(error) => todo!(),
                        // gst_play::PlayMessage::Warning(warning) => todo!(),
                        // gst_play::PlayMessage::VideoDimensionsChanged(video_dimensions_changed) => todo!(),
                        gst_play::PlayMessage::MediaInfoUpdated(info) => {
                            let _ = event_tx
                                .send(Event::Player(PlayerEvent::MediaInfoUpdated(
                                    info.media_info().clone(),
                                )))
                                .await;
                        }
                        gst_play::PlayMessage::VolumeChanged(update) => {
                            let _ = event_tx
                                .send(Event::Player(PlayerEvent::VolumeChanged(update.volume())))
                                .await;
                        }
                        // gst_play::PlayMessage::MuteChanged(mute_changed) => todo!(),
                        // gst_play::PlayMessage::SeekDone(seek_done) => todo!(),
                        _ => (),
                    }
                }
            }
        });

        let (updates_tx, _) = broadcast::channel(10);

        // TODO: IPv6?
        // TODO: update addresses when they change on the device
        let mdns = {
            let daemon = mdns_sd::ServiceDaemon::new()?;

            let ips: Vec<IpAddr> = get_all_available_addrs_ignore_v6_and_localhost()?
                .into_iter()
                .map(|addr| IpAddr::V4(addr))
                .collect::<Vec<IpAddr>>();

            if ips.is_empty() {
                bail!("No addresses available to use for mDNS discovery");
            }

            let name = format!(
                "FCast-{}",
                gethostname::gethostname().to_string_lossy()
            );

            let service = mdns_sd::ServiceInfo::new(
                "_fcast._tcp.local.",
                &name,
                &format!("{name}.local."),
                ips.as_slice(),
                FCAST_TCP_PORT,
                None::<std::collections::HashMap<String, String>>,
            )?;

            daemon.register(service)?;

            daemon
        };

        Ok(Self {
            // pipeline,
            event_tx,
            ui_weak,
            updates_tx,
            mdns,
            last_sent_update: Instant::now() - SENDER_UPDATE_INTERVAL,
            debug_mode: false,
            player,
            player_state: gst_play::PlayState::Stopped,
            current_media: None,
            current_duration: None,
            on_playing_command_queue: smallvec::SmallVec::new(),
        })
    }

    fn notify_updates(&mut self, force: bool) -> Result<()> {
        // let pipeline_playback_state = match self.pipeline.get_playback_state() {
        //     Ok(s) => s,
        //     Err(err) => {
        //         error!("Failed to get playback state: {err}");
        //         return Ok(());
        //     }
        // };

        let Some(info) = self.current_media.as_ref() else {
            return Ok(());
        };

        let Some(position) = self.player.position() else {
            error!("No position");
            return Ok(());
        };
        let position = position.seconds_f64();
        // debug!("Getting current duration: {:?}", self.player.duration());
        let duration = self
            .current_duration
            .as_ref()
            .unwrap_or(&gst::ClockTime::default())
            .seconds_f64();
        // let Some(duration) = self.player.duration() else {
        // let Some(duration) = info.duration() else {
        //     error!("No duration");
        //     return Ok(());
        // };

        let progress_str = {
            //     let update = &pipeline_playback_state;
            //     let time_secs = update.time % 60.0;
            //     let time_mins = (update.time / 60.0) % 60.0;
            //     let time_hours = update.time / 60.0 / 60.0;

            //     let duration_secs = update.duration % 60.0;
            //     let duration_mins = (update.duration / 60.0) % 60.0;
            //     let duration_hours = update.duration / 60.0 / 60.0;

            let time_secs = position % 60.0;
            let time_mins = (position / 60.0) % 60.0;
            let time_hours = position / 60.0 / 60.0;

            let duration_secs = duration % 60.0;
            let duration_mins = (duration / 60.0) % 60.0;
            let duration_hours = duration / 60.0 / 60.0;

            format!(
                "{:02}:{:02}:{:02} / {:02}:{:02}:{:02}",
                time_hours as u32,
                time_mins as u32,
                time_secs as u32,
                duration_hours as u32,
                duration_mins as u32,
                duration_secs as u32,
            )
        };
        let progress_percent = (position / duration) as f32;
        // (pipeline_playback_state.time / pipeline_playback_state.duration * 100.0) as f32;
        let playback_state = {
            let is_live = info.is_live();

            // let is_live = {
            //     let Some(aaa) = self.player.media_info() else {
            //         return Ok(());
            //     };
            //     aaa.is_live()
            // };
            // let is_live = self.pipeline.is_live();
            // use fcast_lib::models::PlaybackState;
            // match pipeline_playback_state.state {
            // debug!("Player state: {:?}", self.player_state);
            match self.player_state {
                gst_play::PlayState::Stopped => GuiPlaybackState::Loading,
                gst_play::PlayState::Buffering => GuiPlaybackState::Loading,
                gst_play::PlayState::Playing | gst_play::PlayState::Paused if is_live => {
                    GuiPlaybackState::Live
                }
                gst_play::PlayState::Playing => GuiPlaybackState::Playing,
                gst_play::PlayState::Paused => GuiPlaybackState::Paused,
                _ => return Ok(()),
            }
            // PlaybackState::Playing | PlaybackState::Paused if is_live => GuiPlaybackState::Live,
            // PlaybackState::Playing => GuiPlaybackState::Playing,
            // PlaybackState::Paused => GuiPlaybackState::Paused,
            // PlaybackState::Idle => GuiPlaybackState::Loading,
        };

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_progress_label(progress_str.into());
            if !bridge.get_is_scrubbing_position() {
                bridge.set_playback_position(progress_percent);
            }
            bridge.set_playback_state(playback_state);
        })?;

        if self.updates_tx.receiver_count() > 0
            && (self.last_sent_update.elapsed() >= SENDER_UPDATE_INTERVAL || force)
        {
            let update = PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                // time: pipeline_playback_state.time,
                time: position,
                duration: duration,
                // state: pipeline_playback_state.state as u8,
                state: match playback_state {
                    GuiPlaybackState::Idle | GuiPlaybackState::Loading => PlaybackState::Idle,
                    GuiPlaybackState::Live | GuiPlaybackState::Playing => PlaybackState::Playing,
                    GuiPlaybackState::Paused => PlaybackState::Paused,
                },
                speed: self.player.rate(),
            };
            debug!("Sending update ({update:?})");
            self.updates_tx
                .send(Arc::new(Packet::from(update).encode()?))?;
            self.last_sent_update = Instant::now();
        }

        Ok(())
    }

    /// Returns `true` if the event loop should exit
    async fn handle_event(&mut self, event: Event) -> Result<bool> {
        // NOTE: all player actions are async (right?)
        match event {
            Event::SessionFinished => {
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>().invoke_device_disconnected();
                })?;
            }
            Event::ResumeOrPause => {
                // unreachable!("Legacy");
                match self.player_state {
                    gst_play::PlayState::Paused => self.player.play(),
                    gst_play::PlayState::Playing => self.player.pause(),
                    _ => error!(
                        "Cannot resume or pause in player current state: {:?}",
                        self.player_state
                    ),
                }
                // let Some(playing) = self.pipeline.is_playing() else {
                //     warn!("Pipeline is not in a state that can be resumed or paused");
                //     return Ok(false);
                // };
                // if let Err(err) = if playing {
                //     self.pipeline.pause()
                // } else {
                //     self.pipeline.play_or_resume()
                // } {
                //     error!("Failed to play or resume: {err}");
                // }
                // self.notify_updates()
                //     .context("failed to notify about updates")?;
            }
            Event::Stop => {
                self.player.stop();
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.invoke_playback_stopped();
                    ui.global::<Bridge>().set_app_state(AppState::Idle);
                })?;
            }
            Event::SetSpeed(set_speed_message) => {
                // unreachable!("Legacy");
                self.player.set_rate(set_speed_message.speed);
                // self
                // .pipeline
                // .set_speed(set_speed_message.speed)
                // .context("failed to set speed")?;
            }
            Event::SeekPercent(percent) => {
                debug!("SeekPercent({percent})");
                if let Some(duration) = self.current_duration {
                    // let seconds = percent / 100.0 * duration.seconds_f32();
                    let seconds = percent * duration.seconds_f32();
                    self.player.seek(gst::ClockTime::from_seconds_f32(seconds));
                }
                // let Some(duration) = self.pipeline.get_duration() else {
                //     error!("Failed to get playback duration");
                //     return Ok(false);
                // };
                // if duration.is_zero() {
                //     error!("Cannot seek when the duration is zero");
                //     return Ok(false);
                // }
                // let seek_to = duration.seconds_f64() * (percent as f64 / 100.0);
                // if let Err(err) = self.pipeline.seek(seek_to) {
                //     error!("Seek error: {err}");
                //     return Ok(false);
                // }
                // self.notify_updates()?;
            }
            Event::SetVolume(set_volume_message) => {
                self.player.set_volume(set_volume_message.volume);
                // self.pipeline.set_volume(set_volume_message.volume);
                // self.ui_weak.upgrade_in_event_loop(move |ui| {
                //     ui.global::<Bridge>()
                //         .set_volume(set_volume_message.volume as f32);
                // })?;
                // if self.updates_tx.receiver_count() > 0 {
                //     let update = VolumeUpdateMessage {
                //         generation_time: current_time_millis(),
                //         volume: set_volume_message.volume,
                //     };
                //     debug!("Sending update ({update:?})");
                //     self.updates_tx
                //         .send(Arc::new(Packet::from(update).encode()))?;
                //     self.last_sent_update = Instant::now();
                // }
            }
            Event::Quit => return Ok(true),
            Event::PipelineEos => {
                debug!("Pipeline reached EOS");
                // self.pipeline.stop()?;
                // self.ui_weak.upgrade_in_event_loop(|ui| {
                //     ui.invoke_playback_stopped();
                // })?;
            }
            Event::PipelineError => {
                // self.pipeline.stop().context("failed to stop pipeline")?;
                // // TODO: send error message to sessions
                // self.ui_weak.upgrade_in_event_loop(|ui| {
                //     ui.invoke_playback_stopped_with_error("Error unclear (todo)".into());
                // })?;
            }
            Event::PipelineStateChanged(state) => match state {
                gst::State::Paused | gst::State::Playing => self
                    .notify_updates(true)
                    .context("failed to notify about updates")?,
                _ => (),
            },
            Event::ToggleDebug => self.debug_mode = !self.debug_mode,
            Event::Player(event) => {
                // #############
                // self.ui_weak.upgrade_in_event_loop(|ui| {
                //     ui.invoke_playback_started();
                //     ui.global::<Bridge>().set_app_state(AppState::Playing);
                // })?;
                // ################

                match event {
                    PlayerEvent::UriLoaded => {
                        // self.player.pause();

                        debug!("Commands: {:?}", self.on_playing_command_queue);
                        // TODO: ignore just for testing webrtc streaming
                        for command in self.on_playing_command_queue.iter() {
                            match command {
                                OnUriLoadedCommand::Seek(time) => {
                                    self.player.seek(gst::ClockTime::from_seconds_f64(*time));
                                }
                                OnUriLoadedCommand::Rate(rate) => {
                                    self.player.set_rate(*rate);
                                }
                                OnUriLoadedCommand::Volume(volume) => {
                                    self.player.set_volume(*volume);
                                }
                            }
                        }

                        self.player.play();
                    }
                    PlayerEvent::StateChanged(state) => {
                        self.player_state = state;
                        match state {
                            // gst_play::PlayState::Stopped => todo!(),
                            // gst_play::PlayState::Buffering => todo!(),
                            gst_play::PlayState::Paused | gst_play::PlayState::Playing => {
                                self.ui_weak.upgrade_in_event_loop(|ui| {
                                    ui.invoke_playback_started();
                                    ui.global::<Bridge>().set_app_state(AppState::Playing);
                                })?;
                                self.notify_updates(true)
                                    .context("Failed to notify about updates")?;

                                // if state == gst_play::PlayState::Playing {
                                //     while let Some(command) = self.on_playing_command_queue.pop() {
                                //         match command {
                                //         }
                                //     }
                                // }
                            }
                            _ => (),
                        }
                    }
                    PlayerEvent::MediaInfoUpdated(info) => {
                        debug!("Media info updated: {info:?}");
                        debug!("New duration: {:?}", info.duration());
                        self.current_duration = info.duration();
                        self.current_media = Some(info);
                    }
                    PlayerEvent::DurationChanged(duration) => {
                        self.current_duration = Some(duration);
                    }
                    PlayerEvent::PositionChanged(_position) => {}
                    PlayerEvent::VolumeChanged(volume) => {
                        self.ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.global::<Bridge>().set_volume(volume as f32);
                        })?;
                        if self.updates_tx.receiver_count() > 0 {
                            let update = VolumeUpdateMessage {
                                generation_time: current_time_millis(),
                                volume,
                            };
                            debug!("Sending update ({update:?})");
                            self.updates_tx
                                .send(Arc::new(Packet::from(update).encode()?))?;
                            self.last_sent_update = Instant::now();
                        }
                    }
                    PlayerEvent::Eos => {}
                }
            }
            Event::Op { session_id: id, op } => {
                debug!(id, ?op, "Operation from sender");
                match op {
                    Operation::Pause => {
                        self.player.pause();
                    }
                    Operation::Resume => {
                        self.player.play();
                    }
                    Operation::Stop => {
                        self.player.stop();
                        self.ui_weak.upgrade_in_event_loop(|ui| {
                            ui.invoke_playback_stopped();
                            ui.global::<Bridge>().set_app_state(AppState::Idle);
                        })?;
                    }
                    Operation::Play(play_message) => {
                        let mut url = if let Some(url) = play_message.url {
                            url
                        } else {
                            let Some(content) = play_message.content else {
                                error!("Play message does not contain a URL or content");
                                return Ok(false);
                            };

                            let content_type = match play_message.container.as_str() {
                                "application/dash+xml" => "application/dash+xml",
                                "application/vnd.apple.mpegurl" | "audio/mpegurl" => {
                                    "application/x-hls"
                                }
                                _ => {
                                    error!("Invalid content type {}", play_message.container);
                                    return Ok(false);
                                }
                            };

                            let b64_content =
                                base64::engine::general_purpose::STANDARD.encode(content);

                            format!("data:{content_type};base64,{b64_content}")
                        };

                        let mut is_for_sure_live = false;
                        if play_message.container == "application/x-whep" {
                            url = url.replace("http://", "fcastwhep://");
                            is_for_sure_live = true;
                        }

                        self.on_playing_command_queue.clear();

                        self.player.set_uri(Some(&url));
                        if let Some(rate) = play_message.speed {
                            self.on_playing_command_queue
                                .push(OnUriLoadedCommand::Rate(rate));
                            // self.player.set_rate(rate);
                        }
                        if !is_for_sure_live && let Some(time) = play_message.time {
                            self.on_playing_command_queue
                                .push(OnUriLoadedCommand::Seek(time));
                            // self.player.seek(gst::ClockTime::from_seconds_f64(time));
                        }

                        // if let Err(err) = self.pipeline.set_playback_uri(&url) {
                        //     use pipeline::SetPlaybackUriError;
                        //     match err {
                        //         SetPlaybackUriError::PipelineStateChange(state_change_error) => {
                        //             return Err(state_change_error.into());
                        //         }
                        //         _ => {
                        //             error!("Failed to set playback URI: {err}");
                        //             return Ok(false);
                        //         }
                        //     }
                        // }
                        // if let Err(err) = self.pipeline.play_or_resume() {
                        //     error!("Failed to play_or_resume pipeline: {err}");
                        // } else {
                        //     self.ui_weak.upgrade_in_event_loop(|ui| {
                        //         ui.invoke_playback_started();
                        //         ui.global::<Bridge>().set_app_state(AppState::Playing);
                        //     })?;
                        //     self.notify_updates()
                        //         .context("failed to notify about updates")?;
                        // }
                    }
                    Operation::Seek(seek_message) => {
                        self.player
                            .seek(gst::ClockTime::from_seconds_f64(seek_message.time));
                    }
                    Operation::SetSpeed(set_speed_message) => {
                        self.player.set_rate(set_speed_message.speed);
                    }
                    Operation::SetPlaylistItem(_set_playlist_item_message) => (),
                    Operation::SetVolume(set_volume_msg) => {
                        self.player.set_volume(set_volume_msg.volume);
                    }
                }
            }
        }

        Ok(false)
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: Receiver<Event>,
        fin_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        let dispatch_listener = TcpListener::bind("0.0.0.0:46899").await?;

        let mut session_id: SessionId = 0;
        let mut update_interval = tokio::time::interval(Duration::from_millis(250));

        // let event_tx_cl = self.event_tx.clone();

        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    if let Some(event) = event {
                        debug!("Got event: {event:?}");
                        match self.handle_event(event).await {
                            Ok(true) => break,
                            Err(err) => error!("Handle event error: {err}"),
                            _ => (),
                        }
                    } else {
                        break;
                    }
                }
                _ = update_interval.tick() => {
                    // TODO: in what states can we omit updates?
                    self.notify_updates(false)?;
                }
                session = dispatch_listener.accept() => {
                    let (stream, _) = session?;

                    debug!("New connection id={session_id}");

                    tokio::spawn({
                        let id = session_id;
                        let event_tx = self.event_tx.clone();
                        let updates_rx = self.updates_tx.subscribe();
                        async move {
                            if let Err(err) =
                                SessionDriver::new(stream, id)
                                .run(updates_rx, &event_tx)
                                .instrument(tracing::debug_span!("session", id))
                                .await
                            {
                                error!("Session exited with error: {err}");
                            }

                            if let Err(err) = event_tx.send(Event::SessionFinished).await {
                                error!("Failed to send SessionFinished: {err}");
                            }
                        }
                    });

                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.invoke_device_connected();
                    })?;

                    session_id += 1;
                }
            }
        }

        self.player.stop();

        debug!("Quitting");

        if fin_tx.send(()).is_err() {
            bail!("Failed to send fin");
        }

        self.mdns.shutdown()?;

        Ok(())
    }
}

#[derive(clap::Parser)]
#[command(version)]
struct CliArgs {
    // Disable animated background. Reduces resource usage
    // #[arg(short = 'b', long, default_value_t = false)]
    // no_background: bool,
}

fn log_level() -> LevelFilter {
    match std::env::var("FCAST_LOG") {
        Ok(level) => match level.to_ascii_lowercase().as_str() {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            "trace" => LevelFilter::TRACE,
            _ => LevelFilter::OFF,
        },
        #[cfg(debug_assertions)]
        Err(_) => LevelFilter::DEBUG,
        #[cfg(not(debug_assertions))]
        Err(_) => LevelFilter::OFF,
    }
}

/// Run the main app.
///
/// Slint and friends are assumed to be initialized by the platform specific target.
pub fn run() -> Result<()> {
    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(log_level());
    let prev_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        tracing_panic::panic_hook(panic_info);
        prev_panic_hook(panic_info);
    }));
    tracing_gstreamer::integrate_events();
    gst::log::remove_default_log_function();
    gst::log::set_default_threshold(gst::DebugLevel::Warning);

    tracing_subscriber::registry().with(fmt_layer).init();

    gst::init()?;

    fcastwhepsrcbin::plugin_init()?;

    let ips: Vec<Ipv4Addr> = get_all_available_addrs_ignore_v6_and_localhost()?;

    use base64::Engine as _;
    let device_url = format!(
        "fcast://r/{}",
        base64::engine::general_purpose::URL_SAFE
            .encode(
                format!(
                    r#"{{"name":"Test","addresses":[{}],"services":[{{"port":46899,"type":0}}]}}"#,
                    ips.iter()
                        .map(|addr| format!("\"{}\"", addr))
                        .collect::<Vec<String>>()
                        .join(","),
                )
                .as_bytes()
            )
            .as_str(),
    );
    debug!("url: {device_url}");

    let qr_code = qrcode::QrCode::new(device_url)?;
    let qr_code_dims = qr_code.width() as u32;
    let qr_code_colors = qr_code.into_colors();
    let mut qr_code_pixels =
        slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(qr_code_dims, qr_code_dims);
    qr_code_pixels.make_mut_slice().copy_from_slice(
        &qr_code_colors
            .into_iter()
            .map(|px| match px {
                qrcode::Color::Light => slint::Rgb8Pixel::new(0xFF, 0xFF, 0xFF),
                qrcode::Color::Dark => slint::Rgb8Pixel::new(0x0, 0x0, 0x0),
            })
            .collect::<Vec<slint::Rgb8Pixel>>(),
    );
    let qr_code_image = slint::Image::from_rgb8(qr_code_pixels);

    let (event_tx, event_rx) = mpsc::channel::<Event>(100);
    let (fin_tx, fin_rx) = oneshot::channel::<()>();

    let mut slint_sink = video::SlintOpenGLSink::new()?;
    let slint_appsink = slint_sink.video_sink();

    // let imagesink = gst::ElementFactory::make("glimagesink").build()?;
    // let video_overlay = imagesink.clone().dynamic_cast::<gst_video::VideoOverlay>().unwrap();
    // let (win_handle_tx, win_handle_rx) = std::sync::mpsc::channel();

    let ui = MainWindow::new()?;

    ui.global::<Bridge>().set_qr_code(qr_code_image);

    #[cfg(debug_assertions)]
    ui.global::<Bridge>().set_is_debugging(true);

    // let window_handle = ui.window().window_handle().window_handle().unwrap().as_raw();
    // debug!(?window_handle, "Got window handle");

    // use slint::winit_030::WinitWindowAccessor;
    // let ui_weak = ui.as_weak();
    // slint::spawn_local(async move {
    //     let ui = ui_weak.upgrade().unwrap();
    //     let win = ui.window().winit_window().await.unwrap();
    //     debug!(?win, "Window handle");
    //     // win.raw_window_handle();
    //     let display_handle = win.display_handle().unwrap();
    //     let raw_display_handle = display_handle.as_raw();
    //     let win_handle = win.window_handle().unwrap();
    //     let raw_handle = win_handle.as_raw();
    //     debug!(?raw_handle, "Raw window handle");
    //     match raw_handle {
    //         slint::winit_030::winit::raw_window_handle::RawWindowHandle::Wayland(wayland_window_handle) => {
    //             unsafe {
    //                 // video_overlay.set_window_handle(wayland_window_handle.surface.read() as usize);
    //                 win_handle_tx.send((raw_display_handle, wayland_window_handle.surface.read() as usize)).unwrap();
    //             }
    //         }
    //         // slint::winit_030::winit::raw_window_handle::RawWindowHandle::Drm(drm_window_handle) => todo!(),
    //         // slint::winit_030::winit::raw_window_handle::RawWindowHandle::Gbm(gbm_window_handle) => todo!(),
    //         _ => todo!(),
    //     }
    // }).unwrap();
    // common::runtime().spawn(async move {
    //     let win = win.await;
    // });

    ui.window().set_rendering_notifier({
        let ui_weak = ui.as_weak();

        move |state, graphics_api| {
            if let slint::RenderingState::RenderingSetup = state {
                debug!("Got graphics API: {graphics_api:?}");
                let ui_weak = ui_weak.clone();

                slint_sink
                    .connect(graphics_api, move || {
                        ui_weak
                            .upgrade_in_event_loop(move |ui| {
                                ui.window().request_redraw();
                            })
                            .unwrap();
                    })
                    .unwrap();
            } else if let slint::RenderingState::BeforeRendering = state {
                let Some(ui) = ui_weak.upgrade() else {
                    error!("Failed to upgrade ui");
                    return;
                };

                // TODO: don't render the video when the frame is from the old source (i.e. playback was
                //       stopped, then new source was set and for a brief moment the last displayed frame
                //       of the old source becomes visible.)
                if ui.global::<Bridge>().get_playing() {
                    let Some((texture_id, size)) = slint_sink.fetch_next_frame_as_texture() else {
                        return;
                    };
                    let frame = unsafe {
                        slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                            texture_id,
                            size.into(),
                        )
                        .build()
                    };
                    ui.global::<Bridge>().set_video_frame(frame);
                }
            }
        }
    })?;

    common::runtime().spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        async move {
            Application::new(slint_appsink, event_tx, ui_weak)
                // Application::new(imagesink, event_tx, ui_weak)
                // Application::new(win_handle_rx, event_tx, ui_weak)
                .await
                .unwrap()
                .run_event_loop(event_rx, fin_tx)
                .await
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_resume_or_pause({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.blocking_send(Event::ResumeOrPause));
        }
    });

    ui.global::<Bridge>().on_seek_to_percent({
        let event_tx = event_tx.clone();
        move |percent| {
            log_if_err!(event_tx.blocking_send(Event::SeekPercent(percent)));
        }
    });

    ui.global::<Bridge>().on_toggle_fullscreen({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks always get called from the event loop");
            ui.window().set_fullscreen(!ui.window().is_fullscreen());
        }
    });

    ui.global::<Bridge>().on_set_volume({
        let event_tx = event_tx.clone();
        move |volume| {
            log_if_err!(event_tx.blocking_send(Event::SetVolume(SetVolumeMessage {
                volume: volume as f64,
            })));
        }
    });

    ui.global::<Bridge>().on_force_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    ui.global::<Bridge>().on_debug_toggled({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.blocking_send(Event::ToggleDebug));
        }
    });

    ui.global::<Bridge>().set_label(format!("{ips:?}").into());

    ui.run()?;

    debug!("Shutting down...");

    common::runtime().block_on(async move {
        event_tx.send(Event::Quit).await.unwrap();
        fin_rx.await.unwrap();
    });

    Ok(())
}
