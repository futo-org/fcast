use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use fcast_protocol::{
    v2,
    v3::{
        self, AVCapabilities, InitialReceiverMessage, LivestreamCapabilities, MetadataObject,
        ReceiverCapabilities, SetPlaylistItemMessage,
    },
    Opcode, PlaybackErrorMessage, PlaybackState as FCastPlaybackState, SeekMessage,
    SetSpeedMessage, SetVolumeMessage, VersionMessage,
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
        ApplicationInfo, CastingDevice, CastingDeviceError, DeviceConnectionState,
        DeviceEventHandler, DeviceFeature, DeviceInfo, EventSubscription, KeyEvent, KeyName,
        LoadRequest, MediaEvent, MediaItem, MediaItemEventType, Metadata, PlaybackState,
        PlaylistItem, ProtocolType, Source,
    },
    utils, IpAddr,
};

const DEFAULT_SESSION_VERSION: u64 = 2;
const EVENT_SUB_MIN_PROTO_VERSION: u64 = 3;
const PLAYLIST_MIN_PROTO_VERSION: u64 = 3;
const V3_FEATURES_MIN_PROTO_VERSION: u64 = 3;

const CONNECTED_EVENT_DEADLINE_DURATION: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq)]
enum LoadType {
    Url { url: String },
    Content { content: String },
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
    Subscribe(EventSubscription),
    Unsubscribe(EventSubscription),
    SetPlaylistItemIndex(u32),
    JumpPlaylist(i32),
    LoadPlaylist(Vec<PlaylistItem>),
    ConnectedEventDeadlineElapsed,
}

fn key_names_to_string(keys: &[KeyName]) -> Vec<String> {
    keys.iter().map(|key| key.to_string()).collect()
}

fn event_sub_to_object(sub: &EventSubscription) -> v3::EventSubscribeObject {
    match sub {
        EventSubscription::MediaItemStart => v3::EventSubscribeObject::MediaItemStart,
        EventSubscription::MediaItemEnd => v3::EventSubscribeObject::MediaItemEnd,
        EventSubscription::MediaItemChange => v3::EventSubscribeObject::MediaItemChanged,
        EventSubscription::KeyDown { keys } => v3::EventSubscribeObject::KeyDown {
            keys: key_names_to_string(keys),
        },
        EventSubscription::KeyUp { keys } => v3::EventSubscribeObject::KeyDown {
            keys: key_names_to_string(keys),
        },
    }
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
    supports_whep: Arc<AtomicBool>,
}

impl FCastDevice {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Mutex::new(State::new(device_info, rt_handle)),
            session_version: FCastVersion::new(),
            supports_whep: Arc::new(AtomicBool::new(false)),
        }
    }
}

const HEADER_LENGTH: usize = 5;

struct FCastVersion(Arc<AtomicU64>);

impl FCastVersion {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(DEFAULT_SESSION_VERSION)))
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, value: u64) {
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
    app_info: Option<ApplicationInfo>,
    supports_whep: Arc<AtomicBool>,
}

impl InnerDevice {
    pub fn new(
        app_info: Option<ApplicationInfo>,
        event_handler: Arc<dyn DeviceEventHandler>,
        session_version: FCastVersion,
        supports_whep: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_handler,
            writer: None,
            session_version,
            app_info,
            supports_whep,
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
                    LoadType::Content { content } => {
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

    fn emit_connected(&self, used_remote_addr: IpAddr, local_addr: IpAddr) {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connected {
                used_remote_addr,
                local_addr,
            });
    }

    async fn inner_work(
        &mut self,
        addrs: &[SocketAddr],
        cmd_rx: &mut Receiver<Command>,
        cmd_tx: Sender<Command>,
    ) -> Result<(), utils::WorkError> {
        let Some(stream) = utils::try_connect_tcp(addrs, Duration::from_secs(5), cmd_rx, |cmd| {
            cmd == Command::Quit
        })
        .await
        .map_err(|err| utils::WorkError::DidNotConnect(err.to_string()))?
        else {
            debug!("Received Quit command in connect loop");
            return Ok(());
        };

        debug!("Successfully connected");

        let used_remote_addr: IpAddr = stream.peer_addr()?.into();
        let local_addr: IpAddr = stream.local_addr()?.into();
        let mut has_emitted_connected_event = false;

        tokio::spawn(async move {
            tokio::time::sleep(CONNECTED_EVENT_DEADLINE_DURATION).await;
            let _ = cmd_tx.send(Command::ConnectedEventDeadlineElapsed).await;
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

        self.send(
            Opcode::Version,
            VersionMessage {
                version: V3_FEATURES_MIN_PROTO_VERSION,
            },
        )
        .await?;

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
                                            FCastPlaybackState::Idle => PlaybackState::Idle,
                                            FCastPlaybackState::Playing => PlaybackState::Playing,
                                            FCastPlaybackState::Paused => PlaybackState::Paused,
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
                                            FCastPlaybackState::Playing => PlaybackState::Playing,
                                            FCastPlaybackState::Paused => PlaybackState::Paused,
                                            FCastPlaybackState::Idle => PlaybackState::Idle,
                                        },
                                        playback_state_changed
                                    );
                                    current_playlist_item_index = update.item_index.map(|idx| idx as usize);
                                }
                                _ => return Err(anyhow!("Unsupported session version {}", self.session_version.get()).into()),
                            }
                        }
                        Opcode::VolumeUpdate => {
                            let Some(body) = packet.1 else {
                                error!("Received volume update message with no body");
                                continue;
                            };
                            let Ok(update) = serde_json::from_str::<v2::VolumeUpdateMessage>(&body) else {
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
                                v3::EventObject::MediaItem { variant, item } => {
                                    let type_ = match variant {
                                        v3::EventType::MediaItemStart => MediaItemEventType::Start,
                                        v3::EventType::MediaItemEnd => MediaItemEventType::End,
                                        v3::EventType::MediaItemChange => MediaItemEventType::Change,
                                        _ => {
                                            error!("Received event of type {variant:?} when a media event was expected");
                                            continue;
                                        }
                                    };
                                    self.event_handler.media_event(
                                        MediaEvent {
                                            type_,
                                            item: MediaItem {
                                                content_type: item.container,
                                                url: item.url,
                                                content: item.content,
                                                time: item.time,
                                                volume: item.volume,
                                                speed: item.speed,
                                                show_duration: item.show_duration,
                                                metadata: item.metadata.map(|m| match m {
                                                    MetadataObject::Generic {title, thumbnail_url, ..} =>
                                                        Metadata { title, thumbnail_url },
                                                }),
                                            }
                                        }
                                    );
                                }
                                v3::EventObject::Key { variant, key, repeat, handled } => {
                                    let event = KeyEvent {
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
                                    Opcode::Initial,
                                    match self.app_info.as_ref() {
                                        Some(info) => {
                                            v3::InitialSenderMessage {
                                                display_name: Some(info.display_name.clone()),
                                                app_name: Some(info.name.clone()),
                                                app_version: Some(info.version.clone()),
                                            }
                                        }
                                        None => {
                                            v3::InitialSenderMessage {
                                                display_name: None,
                                                app_name: Some(
                                                    concat!("FCast Sender SDK v", env!("CARGO_PKG_VERSION")).to_owned(),
                                                ),
                                                app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                                            }
                                        }
                                    }
                                )
                                .await
                                .context("Failed to send InitialSenderMessage")?;

                                self.session_version.set(V3_FEATURES_MIN_PROTO_VERSION);
                            } else {
                                // Since the user of the SDK should be able to tell what features are available after the connected
                                // event has occured, we emit that event after the receiver has anounced version 2, for version 3
                                // we have to wait until the InitialReceiverMessage is received
                                self.emit_connected(used_remote_addr, local_addr);
                                has_emitted_connected_event = true;
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
                                if let Some(volume) = play_msg.volume {
                                    self.event_handler.volume_changed(volume);
                                }
                                if let Some(time) = play_msg.time {
                                    self.event_handler.time_changed(time);
                                }
                                if let Some(speed) = play_msg.speed {
                                    self.event_handler.speed_changed(speed);
                                }
                            }

                            if let Some(ReceiverCapabilities {
                                av: Some(AVCapabilities {
                                    livestream: Some(LivestreamCapabilities {
                                        whep: Some(supports_whep)
                                    })
                                })
                            }) = initial_msg.experimental_capabilities {
                                self.supports_whep.store(supports_whep, Ordering::Relaxed);
                            }

                            if !has_emitted_connected_event {
                                self.emit_connected(used_remote_addr, local_addr);
                                has_emitted_connected_event = true;
                            }
                        }
                        Opcode::PlaybackError => {
                            let Some(body) = packet.1 else {
                                error!("Received playback error with no body");
                                continue;
                            };
                            let error = match serde_json::from_str::<PlaybackErrorMessage>(&body) {
                                Ok(msg) => msg,
                                Err(err) => {
                                    error!("Failed to parse PlaybackErrorMessage json body: {err}");
                                    continue;
                                }
                            };
                            self.event_handler.playback_error(error.message);
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
                        Command::LoadPlaylist(items) => {
                            let items = items
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
                                LoadType::Content { content: json_paylaod },
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
                        Command::Subscribe(ref event) | Command::Unsubscribe(ref event) => {
                            if self.session_version.get() != EVENT_SUB_MIN_PROTO_VERSION {
                                error!(
                                    "Current protocol version ({}) does not support event subscriptions, version >=3 is required",
                                    self.session_version.get(),
                                );
                                continue;
                            }
                            let event = event_sub_to_object(event);
                            let op = if matches!(cmd, Command::Subscribe(_)) {
                                Opcode::SubscribeEvent
                            } else {
                                Opcode::UnsubscribeEvent
                            };
                            self.send(op, v3::SubscribeEventMessage { event }).await?;
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
                        Command::ConnectedEventDeadlineElapsed => {
                            if !has_emitted_connected_event {
                                self.emit_connected(used_remote_addr, local_addr);
                                has_emitted_connected_event = true;
                            }
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

    pub async fn work(
        mut self,
        addrs: Vec<SocketAddr>,
        mut cmd_rx: Receiver<Command>,
        cmd_tx: Sender<Command>,
        reconnect_interval_millis: u64,
    ) {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        crate::connection_loop!(
            reconnect_interval_millis,
            on_work = { self.inner_work(&addrs, &mut cmd_rx, cmd_tx.clone()).await },
            on_reconnect_started = {
                self.event_handler
                    .connection_state_changed(DeviceConnectionState::Reconnecting);
            }
        );

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
            DeviceFeature::WhepStreaming => self.supports_whep.load(Ordering::Relaxed),
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

    fn load(&self, request: LoadRequest) -> Result<(), CastingDeviceError> {
        match request {
            LoadRequest::Url {
                content_type,
                url,
                resume_position,
                speed,
                volume,
                metadata,
                request_headers,
            } => self.send_command(Command::Load {
                content_type,
                type_: LoadType::Url { url },
                resume_position: resume_position.unwrap_or(0.0),
                speed,
                volume,
                metadata,
                request_headers,
            }),
            LoadRequest::Content {
                content_type,
                content,
                resume_position,
                speed,
                volume,
                metadata,
                request_headers,
            } => self.send_command(Command::Load {
                type_: LoadType::Content { content },
                content_type,
                resume_position,
                speed,
                volume,
                metadata,
                request_headers,
            }),
            LoadRequest::Video {
                content_type,
                url,
                resume_position,
                speed,
                volume,
                metadata,
                request_headers,
            } => self.load_url(
                content_type,
                url,
                Some(resume_position),
                speed,
                volume,
                metadata,
                request_headers,
            ),
            LoadRequest::Image {
                content_type,
                url,
                metadata,
                request_headers,
            } => {
                if self.session_version.get() < V3_FEATURES_MIN_PROTO_VERSION {
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
            LoadRequest::Playlist { items } => {
                if self.session_version.get() < PLAYLIST_MIN_PROTO_VERSION {
                    return Err(CastingDeviceError::UnsupportedFeature);
                }

                self.send_command(Command::LoadPlaylist(items))
            }
        }
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
        app_info: Option<ApplicationInfo>,
        event_handler: Arc<dyn DeviceEventHandler>,
        reconnect_interval_millis: u64,
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
        state.command_tx = Some(tx.clone());

        state.rt_handle.spawn(
            InnerDevice::new(
                app_info,
                event_handler,
                self.session_version.clone(),
                Arc::clone(&self.supports_whep),
            )
            .work(addrs, rx, tx, reconnect_interval_millis),
        );

        Ok(())
    }

    fn get_device_info(&self) -> DeviceInfo {
        let state = self.state.lock().unwrap();
        DeviceInfo {
            name: state.name.clone(),
            protocol: ProtocolType::FCast,
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

    fn subscribe_event(&self, subscription: EventSubscription) -> Result<(), CastingDeviceError> {
        if self.session_version.get() >= EVENT_SUB_MIN_PROTO_VERSION {
            self.send_command(Command::Subscribe(subscription))
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn unsubscribe_event(&self, subscription: EventSubscription) -> Result<(), CastingDeviceError> {
        if self.session_version.get() >= EVENT_SUB_MIN_PROTO_VERSION {
            self.send_command(Command::Unsubscribe(subscription))
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }
}
