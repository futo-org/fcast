use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use fcast_protocol::{
    v2,
    v3::{self, InitialReceiverMessage, MetadataObject},
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
}

impl FCastDevice {
    const SUPPORTED_FEATURES: [DeviceFeature; 6] = [
        DeviceFeature::SetVolume,
        DeviceFeature::SetSpeed,
        DeviceFeature::LoadContent,
        DeviceFeature::LoadUrl,
        DeviceFeature::KeyEventSubscription,
        DeviceFeature::MediaEventSubscription,
    ];

    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Mutex::new(State::new(device_info, rt_handle)),
        }
    }
}

const HEADER_LENGTH: usize = 5;

#[derive(Debug, PartialEq, Eq)]
enum ProtocolVersion {
    V2,
    V3,
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
}

impl InnerDevice {
    pub fn new(event_handler: Arc<dyn DeviceEventHandler>) -> Self {
        Self {
            event_handler,
            writer: None,
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
        version: &ProtocolVersion,
        type_: LoadType,
        content_type: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    ) -> anyhow::Result<()> {
        match version {
            ProtocolVersion::V2 => {
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
            ProtocolVersion::V3 => {
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

        // Negotiate version and potentially downgrade
        let session_version = {
            self.send(Opcode::Version, VersionMessage { version: 3 })
                .await?;

            let maybe_version_msg = packet_stream.next().await.ok_or(anyhow!(
                "Packet stream empty when waiting for version message"
            ))?;
            if maybe_version_msg.0 == Opcode::Version {
                let body = maybe_version_msg
                    .1
                    .ok_or(anyhow!("Received version message with no body"))?;
                let version_msg: VersionMessage = serde_json::from_str(&body)
                    .context("Failed to parse VersionMessage json body")?;
                if version_msg.version == 3 {
                    debug!("Receiver supports v3");

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

                    let maybe_initial_msg = packet_stream.next().await.ok_or(anyhow!(
                        "Packet stream empty when waiting for initial message"
                    ))?;
                    if maybe_initial_msg.0 != Opcode::Initial {
                        bail!("expected Initial message, got {:?}", maybe_initial_msg.0);
                    }
                    let body = maybe_initial_msg
                        .1
                        .ok_or(anyhow!("Received initial message with no body"))?;
                    let initial_msg: InitialReceiverMessage = serde_json::from_str(&body)
                        .context("Failed to parse InitialReceiverMessage json body")?;

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

                    ProtocolVersion::V3
                } else {
                    debug!("Receiver supports v2, downgrading");
                    ProtocolVersion::V2
                }
            } else {
                debug!(
                    "Expected to receive version message, got {:?}. Assuming receiver supports v2",
                    maybe_version_msg.0
                );
                // TODO: the received message gets dropped, should it?
                ProtocolVersion::V2
            }
        };

        debug!("Using protocol {session_version:?}");

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
                            match session_version {
                                ProtocolVersion::V2 => {
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
                                ProtocolVersion::V3 => {
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
                                    // TODO: item_index
                                }
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
                            if session_version != ProtocolVersion::V3 {
                                debug!("Received event message when not supposed to ({session_version:?}), ignoring");
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
                        _ => debug!("Packet ignored: {packet:?}"),
                    }
                }
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;

                    debug!("Received command: {cmd:?}");

                    match cmd {
                        Command::ChangeVolume(volume) => self.send(Opcode::SetVolume, SetVolumeMessage { volume }).await?,
                        Command::ChangeSpeed(speed) => self.send(Opcode::SetSpeed, SetSpeedMessage { speed }).await?,
                        Command::Load { type_, content_type, resume_position, speed, volume, metadata, request_headers, } =>
                            self.load(&session_version, type_, content_type, resume_position, speed, volume, metadata, request_headers).await?,
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
                            if session_version != ProtocolVersion::V3 {
                                error!(
                                    "Current protocol version ({session_version:?}) does not support event subscriptions, {:?} is required",
                                    ProtocolVersion::V3
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
        Self::SUPPORTED_FEATURES.contains(&feature)
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

        let playlist = v3::PlaylistContent {
            variant: v3::ContentType::Playlist,
            items,
            ..Default::default()
        };

        let json_paylaod = serde_json::to_string(&playlist)
            .map_err(|_| CastingDeviceError::FailedToSendCommand)?;

        self.load_content(
            "application/json".to_string(),
            json_paylaod,
            0.0,
            0.0,
            None,
            None,
            None,
            None,
        )
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
            .spawn(InnerDevice::new(event_handler).work(addrs, rx));

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
        self.send_command(match group {
            GenericEventSubscriptionGroup::Keys => Command::SubscribeToAllKeyEvents,
            GenericEventSubscriptionGroup::Media => Command::SubscribeToAllMediaItemEvents,
        })
    }

    fn unsubscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        self.send_command(match group {
            GenericEventSubscriptionGroup::Keys => Command::UnsubscribeToAllKeyEvents,
            GenericEventSubscriptionGroup::Media => Command::UnsubscribeToAllMediaItemEvents,
        })
    }
}
