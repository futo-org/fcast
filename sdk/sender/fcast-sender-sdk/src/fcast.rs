use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use fcast_protocol::{
    v2,
    v3::{self, InitialReceiverMessage, MetadataObject, SetPlaylistItemMessage},
    Opcode, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, VolumeUpdateMessage,
};
use futures::StreamExt;
use log::{debug, error};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    runtime::Handle,
    sync::mpsc::{Receiver, Sender},
};

use crate::{
    device::{
        CastingDevice, CastingDeviceError, DeviceConnectionState, DeviceEventHandler,
        DeviceFeature, DeviceInfo, GenericEventSubscriptionGroup, GenericKeyEvent,
        GenericMediaEvent, Metadata, PlaybackState, Playlist, ProtocolType, Source,
    },
    utils, IpAddr,
};

const DEFAULT_SESSION_VERSION: u8 = 2;
const EVENT_SUB_MIN_PROTO_VERSION: u8 = 3;
const PLAYLIST_MIN_PROTO_VERSION: u8 = 3;
const V3_FEATURES_MIN_PROTO_VERSION: u8 = 3;

#[derive(Debug, PartialEq)]
enum LoadType {
    Url { url: String },
    Content { content: String, duration: f64 },
}

#[derive(Debug, PartialEq)]
enum Command {
    ChangeVolume(f64),
    ChangeSpeed(f64),
    Load {
        type_: LoadType,
        content_type: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    SeekVideo(f64),
    StopVideo,
    PauseVideo,
    ResumeVideo,
    Quit,
    SubscribeToAllMediaItemEvents,
    SubscribeToAllKeyEvents,
    UnsubscribeToAllMediaItemEvents,
    UnsubscribeToAllKeyEvents,
    SetPlaylistItemIndex(u32),
    JumpPlaylist(i32),
    LoadPlaylist(Playlist),
}

struct State {
    rt_handle: Handle,
    started: bool,
    command_tx: Option<Sender<Command>>,
    addresses: Vec<IpAddr>,
    name: String,
    port: u16,
}

impl State {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            rt_handle,
            started: false,
            command_tx: None,
            addresses: device_info.addresses,
            name: device_info.name,
            port: device_info.port,
        }
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct FCastDevice {
    state: Mutex<State>,
    session_version: FCastVersion,
}

impl FCastDevice {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Mutex::new(State::new(device_info, rt_handle)),
            session_version: FCastVersion::new(),
        }
    }
}

const HEADER_LENGTH: usize = 5;

struct FCastVersion(Arc<AtomicU8>);

impl FCastVersion {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(DEFAULT_SESSION_VERSION)))
    }

    pub fn get(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, value: u8) {
        self.0.store(value, Ordering::Relaxed)
    }
}

impl Clone for FCastVersion {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

fn meta_to_fcast_meta(meta: Option<Metadata>) -> Option<MetadataObject> {
    meta.map(|meta| MetadataObject::Generic {
        title: meta.title,
        thumbnail_url: meta.thumbnail_url,
        custom: None,
    })
}

struct InnerDevice {
    event_handler: Arc<dyn DeviceEventHandler>,
    writer: Option<tokio::net::tcp::OwnedWriteHalf>,
    session_version: FCastVersion,
}

impl InnerDevice {
    pub fn new(event_handler: Arc<dyn DeviceEventHandler>, session_version: FCastVersion) -> Self {
        Self {
            event_handler,
            writer: None,
            session_version,
        }
    }

    async fn send<T: Serialize>(&mut self, op: Opcode, msg: T) -> anyhow::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            bail!("`writer` is missing");
        };

        let json = serde_json::to_string(&msg)?;
        let data = json.as_bytes();
        let size = 1 + data.len();
        let mut header = vec![0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        let mut packet = header;
        packet.extend_from_slice(data);

        writer.write_all(&packet).await?;

        debug!(
            "Sent {} bytes with opcode: {op:?}, body: {json}",
            packet.len()
        );

        Ok(())
    }

    async fn send_empty(&mut self, op: Opcode) -> anyhow::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            bail!("`writer` is missing");
        };

        let mut header = [0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&1u32.to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        writer.write_all(&header).await?;

        debug!("Sent {} bytes with opcode: {op:?}", header.len());

        Ok(())
    }

    async fn load(
        &mut self,
        type_: LoadType,
        content_type: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    ) -> anyhow::Result<()> {
        match self.session_version.get() {
            2 => {
                let mut msg = v2::PlayMessage {
                    container: content_type,
                    url: None,
                    content: None,
                    time: Some(resume_position),
                    speed,
                    headers: request_headers,
                };
                match type_ {
                    LoadType::Url { url } => {
                        msg.url = Some(url);
                    }
                    LoadType::Content { content, .. } => {
                        msg.content = Some(content);
                    }
                }
                self.send(Opcode::Play, msg).await?;
                if let Some(volume) = volume {
                    self.send(Opcode::SetVolume, SetVolumeMessage { volume })
                        .await?;
                }
            }
            3 => {
                let mut msg = v3::PlayMessage {
                    container: content_type,
                    url: None,
                    content: None,
                    time: Some(resume_position),
                    speed,
                    headers: request_headers,
                    volume,
                    metadata: meta_to_fcast_meta(metadata),
                };
                match type_ {
                    LoadType::Url { url } => {
                        msg.url = Some(url);
                    }
                    LoadType::Content { content, .. } => {
                        msg.content = Some(content);
                    }
                }
                self.send(Opcode::Play, msg).await?;
            }
            _ => bail!("Unspoorted session version {}", self.session_version.get()),
        }
        Ok(())
    }

    async fn inner_work(
        &mut self,
        addrs: Vec<SocketAddr>,
        mut cmd_rx: Receiver<Command>,
    ) -> anyhow::Result<()> {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        let Some(stream) =
            utils::try_connect_tcp(addrs, Duration::from_secs(5), &mut cmd_rx, |cmd| {
                cmd == Command::Quit
            })
            .await?
        else {
            debug!("Received Quit command in connect loop");
            self.event_handler
                .connection_state_changed(DeviceConnectionState::Disconnected);
            return Ok(());
        };

        debug!("Successfully connected");

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connected {
                used_remote_addr: stream.peer_addr()?.ip().into(),
                local_addr: stream.local_addr()?.ip().into(),
            });

        let (reader, writer) = stream.into_split();
        self.writer = Some(writer);

        let packet_stream = futures::stream::unfold(
            (reader, vec![0u8; 1000 * 32 - 1]),
            |(mut reader, mut body_buf)| async move {
                async fn read_packet(
                    reader: &mut tokio::net::tcp::OwnedReadHalf,
                    body_buf: &mut [u8],
                ) -> anyhow::Result<(Opcode, Option<String>)> {
                    let mut header_buf: [u8; HEADER_LENGTH] = [0; HEADER_LENGTH];

                    reader.read_exact(&mut header_buf).await?;

                    let opcode = Opcode::try_from(header_buf[4])?;
                    let body_length = u32::from_le_bytes([
                        header_buf[0],
                        header_buf[1],
                        header_buf[2],
                        header_buf[3],
                    ]) as usize
                        - 1;

                    if body_length > body_buf.len() {
                        bail!(
                            "Message exceeded maximum length: {body_length} > {}",
                            body_buf.len()
                        );
                    }

                    let json_body = if body_length > 0 {
                        reader.read_exact(body_buf[..body_length].as_mut()).await?;
                        Some(String::from_utf8(body_buf[..body_length].to_vec())?)
                    } else {
                        None
                    };

                    Ok((opcode, json_body))
                }

                match read_packet(&mut reader, &mut body_buf).await {
                    Ok((op, json)) => {
                        debug!("Received packet with opcode: {op:?}, body: {json:?}");
                        Some(((op, json), (reader, body_buf)))
                    }
                    Err(err) => {
                        error!("Error occurred while reading packet: {err}");
                        None
                    }
                }
            },
        );

        tokio::pin!(packet_stream);

        #[derive(Default)]
        struct SharedState {
            pub time: f64,
            pub duration: f64,
            pub volume: f64,
            pub speed: f64,
            pub playback_state: PlaybackState,
            pub source: Option<Source>,
        }

        let mut shared_state = SharedState::default();

        macro_rules! changed {
            ($param:ident, $new:expr, $cb:ident) => {
                if shared_state.$param != $new {
                    self.event_handler.$cb($new);
                    shared_state.$param = $new;
                }
            };
        }

        let mut playlist_length = None::<usize>;
        let mut current_playlist_item_index = None::<usize>;

        loop {
            tokio::select! {
                packet = packet_stream.next() => {
                    let packet = packet.ok_or(anyhow!("No more packets"))?;
                    match packet.0 {
                        Opcode::PlaybackUpdate => {
                            let Some(body) = packet.1 else {
                                error!("Missing body");
                                continue;
                            };
                            match self.session_version.get() {
                                2 => {
                                    let Ok(update) = serde_json::from_str::<v2::PlaybackUpdateMessage>(&body) else {
                                        error!("Malformed body: {body}");
                                        continue;
                                    };
                                    changed!(time, update.time, time_changed);
                                    changed!(duration, update.duration, duration_changed);
                                    changed!(speed, update.speed, speed_changed);
                                    changed!(
                                        playback_state,
                                        match update.state {
                                            1 => PlaybackState::Playing,
                                            2 => PlaybackState::Paused,
                                            _ => PlaybackState::Idle,
                                        },
                                        playback_state_changed
                                    );
                                }
                                3 => {
                                    let Ok(update) = serde_json::from_str::<v3::PlaybackUpdateMessage>(&body) else {
                                        error!("Malformed body: {body}");
                                        continue;
                                    };
                                    if let Some(time_update) = update.time {
                                        changed!(time, time_update, time_changed);
                                    }
                                    if let Some(duration_update) = update.duration {
                                        changed!(duration, duration_update, duration_changed);
                                    }
                                    if let Some(speed_update) = update.speed {
                                        changed!(speed, speed_update, speed_changed);
                                    }
                                    changed!(
                                        playback_state,
                                        match update.state {
                                            v3::PlaybackState::Playing => PlaybackState::Playing,
                                            v3::PlaybackState::Paused => PlaybackState::Paused,
                                            v3::PlaybackState::Idle => PlaybackState::Idle,
                                        },
                                        playback_state_changed
                                    );
                                    current_playlist_item_index = update.item_index.map(|idx| idx as usize);
                                }
                                _ => bail!("Unsupported session version {}", self.session_version.get()),
                            }
                        }
                        Opcode::VolumeUpdate => {
                            let Some(body) = packet.1 else {
                                error!("Received volume update message with no body");
                                continue;
                            };
                            let Ok(update) = serde_json::from_str::<VolumeUpdateMessage>(&body) else {
                                error!("Received volume update message with malformed body: {body}");
                                continue;
                            };
                            changed!(volume, update.volume, volume_changed);
                        }
                        Opcode::Ping => self.send_empty(Opcode::Pong).await?,
                        Opcode::Event => {
                            if self.session_version.get() != V3_FEATURES_MIN_PROTO_VERSION {
                                debug!("Received event message when not supposed to, ignoring");
                                continue;
                            }
                            let Some(body) = packet.1 else {
                                error!("Received event message with no body");
                                continue;
                            };
                            let Ok(msg) = serde_json::from_str::<v3::EventMessage>(&body) else {
                                error!("Received event message with malformed body: {body}");
                                continue;
                            };
                            match msg.event {
                                v3::EventObject::MediaItem { variant, .. } => {
                                    self.event_handler.media_event(
                                        match variant {
                                            v3::EventType::MediaItemStart => GenericMediaEvent::Started,
                                            v3::EventType::MediaItemEnd => GenericMediaEvent::Ended,
                                            v3::EventType::MediaItemChange => GenericMediaEvent::Changed,
                                            _ => {
                                                error!("Expected a MediaItem event, got {variant:?}");
                                                continue;
                                            }
                                        }
                                    );
                                }
                                v3::EventObject::Key { variant, key, repeat, handled } => {
                                    let event = GenericKeyEvent {
                                        released: match variant {
                                            v3::EventType::KeyDown => false,
                                            v3::EventType::KeyUp => true,
                                            _ => {
                                                error!("Expected Key event, got {variant:?}");
                                                continue;
                                            }
                                        },
                                        repeat,
                                        handled,
                                        name: key
                                    };
                                    self.event_handler.key_event(event);
                                }
                            }
                        }
                        Opcode::PlayUpdate => {
                            let Some(body) = packet.1 else {
                                error!("Missing body");
                                continue;
                            };
                            let Ok(play_update) = serde_json::from_str::<v3::PlayUpdateMessage>(&body) else {
                                error!("Malformed body: {body}");
                                continue;
                            };
                            let Some(play_data) = play_update.play_data else {
                                continue;
                            };
                            if let Some(url) = play_data.url {
                                let source = Source::Url {
                                    url,
                                    content_type: play_data.container,
                                };
                                self.event_handler.source_changed(source.clone());
                                self.event_handler.playback_state_changed(PlaybackState::Playing);
                                shared_state.source = Some(source);
                            } else if let Some(content) = play_data.content {
                                let source = Source::Content { content };
                                self.event_handler.source_changed(source.clone());
                                self.event_handler.playback_state_changed(PlaybackState::Playing);
                                shared_state.source = Some(source);
                            }
                        }
                        Opcode::Version => {
                            let Some(body) = packet.1 else {
                                error!("Version message is missing body");
                                continue;
                            };
                            let version_msg = match serde_json::from_str::<VersionMessage>(&body) {
                                Ok(msg) => msg,
                                Err(err) => {
                                    error!("Failed to parse VersionMessage json body: {err}");
                                    continue;
                                }
                            };
                            if version_msg.version >= V3_FEATURES_MIN_PROTO_VERSION {
                                debug!("Receiver supports v3");
                                self.send(
                                    Opcode::Version,
                                    VersionMessage { version: V3_FEATURES_MIN_PROTO_VERSION }
                                ).await?;

                                self.send(
                                    Opcode::Initial,
                                    v3::InitialSenderMessage {
                                        display_name: None,
                                        app_name: Some(
                                            concat!("FCast Sender SDK v", env!("CARGO_PKG_VERSION")).to_owned(),
                                        ),
                                        app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                                    },
                                )
                                .await
                                .context("Failed to send InitialSenderMessage")?;

                                self.session_version.set(V3_FEATURES_MIN_PROTO_VERSION);
                            }
                        }
                        Opcode::Initial => {
                            let Some(body) = packet.1 else {
                                error!("Received initial message with no body");
                                continue;
                            };
                            let initial_msg = match serde_json::from_str::<InitialReceiverMessage>(&body) {
                                Ok(msg) => msg,
                                Err(err) => {
                                    error!("Failed to parse InitialReceiverMessage json body: {err}");
                                    continue;
                                }
                            };

                            debug!("Received InitialReceiverMessage: {initial_msg:?}");

                            if let Some(play_msg) = initial_msg.play_data {
                                if let Some(url) = play_msg.url {
                                    let source = Source::Url {
                                        url,
                                        content_type: play_msg.container,
                                    };
                                    self.event_handler.source_changed(source.clone());
                                    self.event_handler
                                        .playback_state_changed(PlaybackState::Playing);
                                    shared_state.source = Some(source);
                                } else if let Some(content) = play_msg.content {
                                    let source = Source::Content { content };
                                    self.event_handler.source_changed(source.clone());
                                    self.event_handler
                                        .playback_state_changed(PlaybackState::Playing);
                                    shared_state.source = Some(source);
                                }
                            }
                        }
                        _ => debug!("Packet ignored: {packet:?}"),
                    }
                }
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;

                    debug!("Received command: {cmd:?}");

                    match cmd {
                        Command::ChangeVolume(volume) => self.send(Opcode::SetVolume, SetVolumeMessage { volume }).await?,
                        Command::ChangeSpeed(speed) => self.send(Opcode::SetSpeed, SetSpeedMessage { speed }).await?,
                        Command::Load { type_, content_type, resume_position, speed, volume, metadata, request_headers, } => {
                            self.load(type_, content_type, resume_position, speed, volume, metadata, request_headers).await?;
                            playlist_length = None;
                            current_playlist_item_index = None;
                        }
                        Command::LoadPlaylist(playlist) => {
                            let items = playlist
                                .items
                                .into_iter()
                                .map(|item| v3::MediaItem {
                                    container: item.content_type,
                                    url: Some(item.content_location),
                                    time: item.start_time,
                                    ..Default::default()
                                })
                                .collect::<Vec<v3::MediaItem>>();

                            playlist_length = Some(items.len());
                            current_playlist_item_index = Some(0);

                            let playlist = v3::PlaylistContent {
                                variant: v3::ContentType::Playlist,
                                items,
                                ..Default::default()
                            };

                            let Ok(json_paylaod) = serde_json::to_string(&playlist) else {
                                error!("Failed to serialize playlist to json");
                                continue;
                            };

                            self.load(
                                LoadType::Content { content: json_paylaod, duration: 0.0 },
                                "application/json".to_owned(),
                                0.0,
                                None,
                                None,
                                None,
                                None,
                            ).await?;
                        }
                        Command::SeekVideo(time) => self.send(Opcode::Seek, SeekMessage { time }).await?,
                        Command::StopVideo => {
                            self.send_empty(Opcode::Stop).await?;
                            self.event_handler.playback_state_changed(PlaybackState::Idle);
                        }
                        Command::PauseVideo => self.send_empty(Opcode::Pause).await?,
                        Command::ResumeVideo => self.send_empty(Opcode::Resume).await?,
                        Command::Quit => break,
                        Command::SubscribeToAllMediaItemEvents
                            | Command::SubscribeToAllKeyEvents
                            | Command::UnsubscribeToAllMediaItemEvents
                            | Command::UnsubscribeToAllKeyEvents => {
                            if self.session_version.get() != EVENT_SUB_MIN_PROTO_VERSION {
                                error!(
                                    "Current protocol version ({}) does not support event subscriptions, version >=3 is required",
                                    self.session_version.get(),
                                );
                                continue;
                            }
                                let op = if cmd == Command::SubscribeToAllMediaItemEvents
                                    || cmd == Command::SubscribeToAllKeyEvents {
                                Opcode::SubscribeEvent
                            } else {
                                Opcode::UnsubscribeEvent
                            };
                                let objs =  if cmd == Command::SubscribeToAllMediaItemEvents
                                    || cmd == Command::UnsubscribeToAllMediaItemEvents {
                                vec![
                                    v3::EventSubscribeObject::MediaItemStart,
                                    v3::EventSubscribeObject::MediaItemEnd,
                                    v3::EventSubscribeObject::MediaItemChanged,
                                ]
                            } else {
                                vec![
                                    v3::EventSubscribeObject::KeyDown { keys: v3::KeyNames::all() },
                                    v3::EventSubscribeObject::KeyUp { keys: v3::KeyNames::all() },
                                ]
                            };
                            for obj in objs {
                                self.send(
                                    op,
                                    v3::SubscribeEventMessage { event: obj }
                                ).await?;
                            }
                        }
                        Command::SetPlaylistItemIndex(item_index) =>
                            self.send(Opcode::SetPlaylistItem, SetPlaylistItemMessage { item_index: item_index as u64 }).await?,
                        Command::JumpPlaylist(jump) => {
                            let (Some(playlist_length), Some(current_playlist_item_index))
                                = (playlist_length, current_playlist_item_index.as_mut()) else {
                                error!("Cannot jump in playlist because a playlist is not currently playing");
                                continue;
                            };
                            if jump < 0 && *current_playlist_item_index == 0 {
                                *current_playlist_item_index = playlist_length - 1;
                            } else {
                                *current_playlist_item_index += jump as usize;
                                *current_playlist_item_index %= playlist_length;
                            }

                            self.send(
                                Opcode::SetPlaylistItem,
                                SetPlaylistItemMessage { item_index: *current_playlist_item_index as u64 }
                            ).await?;
                        }
                    }
                }
            }
        }

        debug!("Shutting down...");

        if let Some(mut writer) = self.writer.take() {
            writer.shutdown().await?;
        }

        Ok(())
    }

    pub async fn work(mut self, addrs: Vec<SocketAddr>, cmd_rx: Receiver<Command>) {
        if let Err(err) = self.inner_work(addrs, cmd_rx).await {
            error!("Inner work error: {err}");
        }

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Disconnected);
    }
}

impl FCastDevice {
    fn send_command(&self, cmd: Command) -> Result<(), CastingDeviceError> {
        let state = self.state.lock().unwrap();
        let Some(tx) = &state.command_tx else {
            error!("Missing command tx");
            return Err(CastingDeviceError::FailedToSendCommand);
        };

        let tx = tx.clone();
        state.rt_handle.spawn(async move { tx.send(cmd).await });

        Ok(())
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastingDevice for FCastDevice {
    fn casting_protocol(&self) -> ProtocolType {
        ProtocolType::FCast
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.addresses.is_empty() && state.port > 0 && !state.name.is_empty()
    }

    fn supports_feature(&self, feature: DeviceFeature) -> bool {
        match feature {
            DeviceFeature::SetVolume
            | DeviceFeature::SetSpeed
            | DeviceFeature::LoadContent
            | DeviceFeature::LoadUrl => true,
            DeviceFeature::KeyEventSubscription
            | DeviceFeature::MediaEventSubscription
            | DeviceFeature::LoadImage
            | DeviceFeature::PlaylistNextAndPrevious
            | DeviceFeature::SetPlaylistItemIndex
            | DeviceFeature::LoadPlaylist => {
                self.session_version.get() >= V3_FEATURES_MIN_PROTO_VERSION
            }
        }
    }

    fn name(&self) -> String {
        let state = self.state.lock().unwrap();
        state.name.clone()
    }

    fn set_name(&self, name: String) {
        let mut state = self.state.lock().unwrap();
        state.name = name;
    }

    fn stop_casting(&self) -> Result<(), CastingDeviceError> {
        if let Err(err) = self.stop_playback() {
            error!("Failed to stop playback: {err}");
        }
        debug!("Stopping active device because stopCasting was called.");
        self.disconnect()
    }

    fn seek(&self, time_seconds: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::SeekVideo(time_seconds))
    }

    fn stop_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::StopVideo)
    }

    fn pause_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::PauseVideo)
    }

    fn resume_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ResumeVideo)
    }

    fn load_url(
        &self,
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    ) -> Result<(), CastingDeviceError> {
        self.send_command(Command::Load {
            content_type,
            type_: LoadType::Url { url },
            resume_position: resume_position.unwrap_or(0.0),
            speed,
            volume,
            metadata,
            request_headers,
        })
    }

    fn load_video(
        &self,
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    ) -> Result<(), CastingDeviceError> {
        self.load_url(
            content_type,
            url,
            Some(resume_position),
            speed,
            volume,
            metadata,
            request_headers,
        )
    }

    fn load_image(
        &self,
        content_type: String,
        url: String,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    ) -> Result<(), CastingDeviceError> {
        if self.session_version.get() < PLAYLIST_MIN_PROTO_VERSION {
            return Err(CastingDeviceError::UnsupportedFeature);
        }

        self.load_url(
            content_type,
            url,
            None,
            None,
            None,
            metadata,
            request_headers,
        )
    }

    fn load_playlist(&self, playlist: Playlist) -> Result<(), CastingDeviceError> {
        if self.session_version.get() < PLAYLIST_MIN_PROTO_VERSION {
            return Err(CastingDeviceError::UnsupportedFeature);
        }

        self.send_command(Command::LoadPlaylist(playlist))
    }

    fn playlist_item_next(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::JumpPlaylist(1))
    }

    fn playlist_item_previous(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::JumpPlaylist(-1))
    }

    fn set_playlist_item_index(&self, index: u32) -> Result<(), CastingDeviceError> {
        if self.session_version.get() >= PLAYLIST_MIN_PROTO_VERSION {
            self.send_command(Command::SetPlaylistItemIndex(index))
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn load_content(
        &self,
        content_type: String,
        content: String,
        resume_position: f64,
        duration: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    ) -> Result<(), CastingDeviceError> {
        self.send_command(Command::Load {
            type_: LoadType::Content { content, duration },
            content_type,
            resume_position,
            speed,
            volume,
            metadata,
            request_headers,
        })
    }

    fn change_volume(&self, volume: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ChangeVolume(volume))
    }

    fn change_speed(&self, speed: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ChangeSpeed(speed))
    }

    fn disconnect(&self) -> Result<(), CastingDeviceError> {
        debug!("Trying to stop worker...");
        if let Err(err) = self.send_command(Command::Quit) {
            error!("Failed to stop worker: {err}");
        }
        debug!("Sent quit command");
        let mut state = self.state.lock().unwrap();
        state.command_tx = None;
        state.started = false;
        debug!("Stopped OK");
        Ok(())
    }

    fn connect(
        &self,
        event_handler: Arc<dyn DeviceEventHandler>,
    ) -> Result<(), CastingDeviceError> {
        let mut state = self.state.lock().unwrap();
        if state.started {
            return Err(CastingDeviceError::DeviceAlreadyStarted);
        }

        let addrs = crate::device::ips_to_socket_addrs(&state.addresses, state.port);
        if addrs.is_empty() {
            return Err(CastingDeviceError::MissingAddresses);
        }

        state.started = true;
        debug!("Starting with address list: {addrs:?}...");

        let (tx, rx) = tokio::sync::mpsc::channel::<Command>(50);
        state.command_tx = Some(tx);

        state
            .rt_handle
            .spawn(InnerDevice::new(event_handler, self.session_version.clone()).work(addrs, rx));

        Ok(())
    }

    fn get_device_info(&self) -> DeviceInfo {
        let state = self.state.lock().unwrap();
        DeviceInfo {
            name: state.name.clone(),
            r#type: ProtocolType::FCast,
            addresses: state.addresses.clone(),
            port: state.port,
        }
    }

    fn get_addresses(&self) -> Vec<IpAddr> {
        let state = self.state.lock().unwrap();
        state.addresses.clone()
    }

    fn set_addresses(&self, addrs: Vec<IpAddr>) {
        let mut state = self.state.lock().unwrap();
        state.addresses = addrs;
    }

    fn get_port(&self) -> u16 {
        let state = self.state.lock().unwrap();
        state.port
    }

    fn set_port(&self, port: u16) {
        let mut state = self.state.lock().unwrap();
        state.port = port;
    }

    fn subscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        if self.session_version.get() >= EVENT_SUB_MIN_PROTO_VERSION {
            self.send_command(match group {
                GenericEventSubscriptionGroup::Keys => Command::SubscribeToAllKeyEvents,
                GenericEventSubscriptionGroup::Media => Command::SubscribeToAllMediaItemEvents,
            })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn unsubscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        if self.session_version.get() >= EVENT_SUB_MIN_PROTO_VERSION {
            self.send_command(match group {
                GenericEventSubscriptionGroup::Keys => Command::UnsubscribeToAllKeyEvents,
                GenericEventSubscriptionGroup::Media => Command::UnsubscribeToAllMediaItemEvents,
            })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }
}
