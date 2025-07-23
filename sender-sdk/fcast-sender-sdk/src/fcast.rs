use std::{
    // future::Future,
    net::SocketAddr,
    // pin::Pin,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context};
use fcast_protocol::{
    v2,
    v3::{self, InitialReceiverMessage},
    Opcode, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, VolumeUpdateMessage,
};
use futures::StreamExt;
use log::{debug, error, info /* warn */};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::tcp::{ReadHalf, WriteHalf},
    runtime::Handle,
    sync::mpsc::{Receiver, Sender},
};

use crate::{
    casting_device::{
        CastConnectionState, CastProtocolType, CastingDevice, CastingDeviceError,
        CastingDeviceEventHandler, /* CastingDeviceExt, */ CastingDeviceInfo,
        GenericEventSubscriptionGroup, GenericKeyEvent, GenericMediaEvent, PlaybackState, Source,
    },
    /* AsyncRuntime, AsyncRuntimeError, */ IpAddr,
};

#[derive(Debug, PartialEq)]
enum Command {
    ChangeVolume(f64),
    ChangeSpeed(f64),
    #[allow(dead_code)]
    LoadVideo {
        stream_type: String,
        content_type: String,
        content_id: String,
        resume_position: f64,
        duration: f64,
        speed: Option<f64>,
    },
    LoadUrl {
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
    },
    LoadContent {
        content_type: String,
        content: String,
        resume_position: f64,
        duration: f64,
        speed: Option<f64>,
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
    // runtime: AsyncRuntime,
    rt_handle: Handle,
    started: bool,
    command_tx: Option<Sender<Command>>,
    addresses: Vec<IpAddr>,
    name: String,
    port: u16,
}

impl State {
    // pub fn new(device_info: CastingDeviceInfo) -> Result<Self, AsyncRuntimeError> {
    pub fn new(device_info: CastingDeviceInfo, rt_handle: Handle) -> Self {
        Self {
            // runtime: AsyncRuntime::new(Some(1), "fcast-async-runtime")?,
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
pub struct FCastCastingDevice {
    state: Mutex<State>,
}

// #[cfg_attr(feature = "uniffi", uniffi::export)]
impl FCastCastingDevice {
    // #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    // pub fn new(device_info: CastingDeviceInfo) -> Result<Self, AsyncRuntimeError> {
    pub fn new(device_info: CastingDeviceInfo, rt_handle: Handle) -> Self {
        // Ok(Self {
        //     state: Mutex::new(State::new(device_info)?),
        // })
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

struct InnerDevice {
    event_handler: Arc<dyn CastingDeviceEventHandler>,
}

impl InnerDevice {
    pub fn new(event_handler: Arc<dyn CastingDeviceEventHandler>) -> Self {
        Self { event_handler }
    }

    async fn send<T: Serialize>(
        &mut self,
        writer: &mut WriteHalf<'_>,
        op: Opcode,
        msg: T,
    ) -> anyhow::Result<()> {
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

    async fn send_empty(&mut self, writer: &mut WriteHalf<'_>, op: Opcode) -> anyhow::Result<()> {
        let mut header = [0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&1u32.to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        writer.write_all(&header).await?;

        debug!("Sent {} bytes with opcode: {op:?}", header.len());

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_play(
        &mut self,
        writer: &mut WriteHalf<'_>,
        version: &ProtocolVersion,
        content_type: String,
        url: Option<String>,
        content: Option<String>,
        resume_position: f64,
        speed: Option<f64>,
    ) -> anyhow::Result<()> {
        match version {
            ProtocolVersion::V2 => {
                let msg = v2::PlayMessage {
                    container: content_type,
                    url,
                    content,
                    time: Some(resume_position),
                    speed,
                    headers: None,
                };
                self.send(writer, Opcode::Play, msg).await?;
            }
            ProtocolVersion::V3 => {
                let msg = v3::PlayMessage {
                    container: content_type,
                    url,
                    content,
                    time: Some(resume_position),
                    speed,
                    headers: None,
                    volume: None,
                    metadata: None,
                };
                self.send(writer, Opcode::Play, msg).await?;
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
            .connection_state_changed(CastConnectionState::Connecting);

        let Some(mut stream) =
            crate::try_connect_tcp(addrs, 5, &mut cmd_rx, |cmd| cmd == Command::Quit).await?
        else {
            debug!("Received Quit command in connect loop");
            self.event_handler
                .connection_state_changed(CastConnectionState::Disconnected);
            return Ok(());
        };

        info!("Successfully connected");

        self.event_handler
            .connection_state_changed(CastConnectionState::Connected {
                used_remote_addr: stream.peer_addr()?.ip().into(),
                local_addr: stream.local_addr()?.ip().into(),
            });

        let (reader, mut writer) = stream.split();

        let packet_stream = futures::stream::unfold(
            (reader, vec![0u8; 1000 * 32 - 1]),
            |(mut reader, mut body_buf)| async move {
                async fn read_packet(
                    reader: &mut ReadHalf<'_>,
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
        let version = {
            self.send(&mut writer, Opcode::Version, VersionMessage { version: 3 })
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
                        &mut writer,
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

        info!("Using protocol {version:?}");

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
                            match version {
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
                        Opcode::Ping => self.send_empty(&mut writer, Opcode::Pong).await?,
                        Opcode::Event => {
                            if version != ProtocolVersion::V3 {
                                debug!("Received event message when not supposed to ({version:?}), ignoring");
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
                        _ => debug!("Packet ignored: {:?}", packet),
                    }
                }
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;

                    debug!("Received command: {cmd:?}");

                    match cmd {
                        Command::ChangeVolume(volume) => {
                            let msg = SetVolumeMessage { volume };
                            self.send(&mut writer, Opcode::SetVolume, msg).await?;
                        }
                        Command::ChangeSpeed(speed) => {
                            let msg = SetSpeedMessage { speed };
                            self.send(&mut writer, Opcode::SetSpeed, msg).await?;
                        }
                        Command::LoadVideo { content_type, content_id, speed, resume_position, .. } => {
                            self.send_play(
                                &mut writer,
                                &version,
                                content_type,
                                Some(content_id),
                                None,
                                resume_position,
                                speed,
                            ).await?;
                        }
                        Command::LoadUrl { content_type, url, resume_position, speed } => {
                            self.send_play(
                                &mut writer,
                                &version,
                                content_type,
                                Some(url),
                                None,
                                resume_position,
                                speed,
                            ).await?;
                        }
                        Command::LoadContent { content_type, content, resume_position, speed, .. } => {
                            self.send_play(
                                &mut writer,
                                &version,
                                content_type,
                                None,
                                Some(content),
                                resume_position,
                                speed,
                            ).await?;
                        }
                        Command::SeekVideo(time) => {
                            let msg = SeekMessage { time };
                            self.send(&mut writer, Opcode::Seek, msg).await?;
                        }
                        Command::StopVideo => {
                            self.send_empty(&mut writer, Opcode::Stop).await?;
                            self.event_handler.playback_state_changed(PlaybackState::Idle);
                        }
                        Command::PauseVideo => self.send_empty(&mut writer, Opcode::Pause).await?,
                        Command::ResumeVideo => self.send_empty(&mut writer, Opcode::Resume).await?,
                        Command::Quit => break,
                        Command::SubscribeToAllMediaItemEvents
                            | Command::SubscribeToAllKeyEvents
                            | Command::UnsubscribeToAllMediaItemEvents
                            | Command::UnsubscribeToAllKeyEvents => {
                            if version != ProtocolVersion::V3 {
                                error!(
                                    "Current protocol version ({version:?}) does not support event subscriptions, {:?} is required",
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
                                    &mut writer,
                                    op,
                                    v3::SubscribeEventMessage { event: obj }
                                ).await?;
                            }
                        }
                    }
                }
            }
        }

        info!("Shutting down...");

        writer.shutdown().await?;

        Ok(())
    }

    pub async fn work(mut self, addrs: Vec<SocketAddr>, cmd_rx: Receiver<Command>) {
        if let Err(err) = self.inner_work(addrs, cmd_rx).await {
            error!("Inner work error: {err}");
        }

        self.event_handler
            .connection_state_changed(CastConnectionState::Disconnected);
    }
}

impl FCastCastingDevice {
    fn send_command(&self, cmd: Command) -> Result<(), CastingDeviceError> {
        let state = self.state.lock().unwrap();
        let Some(tx) = &state.command_tx else {
            error!("Missing command tx");
            return Err(CastingDeviceError::FailedToSendCommand);
        };

        // TODO: `blocking_send()`? Would need to check for a runtime and use that if it exists.
        //        Can save clones when this function is called from sync environment.
        let tx = tx.clone();
        // state.runtime.spawn(async move { tx.send(cmd).await });
        state.rt_handle.spawn(async move { tx.send(cmd).await });

        Ok(())
    }
}

// impl CastingDeviceExt for FCastCastingDevice {
//     fn soft_start(
//         &self,
//         event_handler: Arc<dyn CastingDeviceEventHandler>,
//     ) -> Result<Pin<Box<dyn Future<Output = ()> + Send + 'static>>, CastingDeviceError> {
//         let mut state = self.state.lock().unwrap();
//         if state.started {
//             warn!("Failed to start: already started");
//             return Err(CastingDeviceError::DeviceAlreadyStarted);
//         }

//         let addrs = state
//             .addresses
//             .iter()
//             .map(|a| a.into())
//             .map(|a| SocketAddr::new(a, state.port))
//             .collect::<Vec<SocketAddr>>();

//         if addrs.is_empty() {
//             error!("Missing addresses");
//             return Err(CastingDeviceError::MissingAddresses);
//         }

//         state.started = true;
//         info!("Starting with address list: {addrs:?}...");

//         let (tx, rx) = tokio::sync::mpsc::channel::<Command>(50);
//         state.command_tx = Some(tx);

//         Ok(Box::pin(InnerDevice::new(event_handler).work(addrs, rx)))
//     }
// }

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastingDevice for FCastCastingDevice {
    fn casting_protocol(&self) -> CastProtocolType {
        CastProtocolType::FCast
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.addresses.is_empty() && state.port > 0 && !state.name.is_empty()
    }

    fn can_set_volume(&self) -> bool {
        true
    }

    fn can_set_speed(&self) -> bool {
        true
    }

    fn support_subscriptions(&self) -> bool {
        true
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
        info!("Stopping active device because stopCasting was called.");
        self.stop()
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
    ) -> Result<(), CastingDeviceError> {
        self.send_command(Command::LoadUrl {
            content_type,
            url,
            resume_position: resume_position.unwrap_or(0.0),
            speed,
        })
    }

    fn load_video(
        &self,
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        self.load_url(content_type, url, Some(resume_position), speed)
    }

    fn load_image(&self, content_type: String, url: String) -> Result<(), CastingDeviceError> {
        self.load_url(content_type, url, None, None)
    }

    fn load_content(
        &self,
        content_type: String,
        content: String,
        resume_position: f64,
        duration: f64,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        self.send_command(Command::LoadContent {
            content_type,
            content,
            resume_position,
            duration,
            speed,
        })
    }

    fn change_volume(&self, volume: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ChangeVolume(volume))
    }

    fn change_speed(&self, speed: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ChangeSpeed(speed))
    }

    fn stop(&self) -> Result<(), CastingDeviceError> {
        info!("Trying to stop worker...");
        if let Err(err) = self.send_command(Command::Quit) {
            error!("Failed to stop worker: {err}");
        }
        info!("Sent quit command");
        let mut state = self.state.lock().unwrap();
        state.command_tx = None;
        state.started = false;
        info!("Stopped OK");
        Ok(())
    }

    fn start(
        &self,
        event_handler: Arc<dyn CastingDeviceEventHandler>,
    ) -> Result<(), CastingDeviceError> {
        let mut state = self.state.lock().unwrap();
        if state.started {
            return Err(CastingDeviceError::DeviceAlreadyStarted);
        }

        let addrs = crate::casting_device::ips_to_socket_addrs(&state.addresses, state.port);
        if addrs.is_empty() {
            return Err(CastingDeviceError::MissingAddresses);
        }

        state.started = true;
        info!("Starting with address list: {addrs:?}...");

        let (tx, rx) = tokio::sync::mpsc::channel::<Command>(50);
        state.command_tx = Some(tx);

        state
            .rt_handle
            .spawn(InnerDevice::new(event_handler).work(addrs, rx));

        Ok(())
    }

    fn get_device_info(&self) -> CastingDeviceInfo {
        let state = self.state.lock().unwrap();
        CastingDeviceInfo {
            name: state.name.clone(),
            r#type: CastProtocolType::FCast,
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
