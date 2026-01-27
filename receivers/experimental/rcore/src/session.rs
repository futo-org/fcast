use std::{sync::Arc, time::Duration};

use crate::Event;
use bitflags::bitflags;
use fcast_protocol::{
    Opcode, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, v1, v2,
    v3::{self, ReceiverCapabilities},
};
use futures::stream::unfold;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{ReadHalf, WriteHalf},
    },
    sync::{broadcast::Receiver, mpsc::UnboundedSender},
};
use tokio_stream::StreamExt;
use tracing::{debug, error, trace};

pub type SessionId = u64;

const TICKS_BEFORE_PING: u32 = 3;

pub const HEADER_BUFFER_SIZE: usize = 5;
pub const MAX_BODY_SIZE: usize = 32000 - 1;

use anyhow::{Context, bail};

use fcast_protocol::{
    PlaybackErrorMessage,
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
            Opcode::SetVolume => Self::SetVolume(serde_json::from_str(body).context("SetVolume")?),
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
            Packet::SetSpeed(set_speed_msg) => serde_json::to_string(&set_speed_msg)?.into_bytes(),
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionVersion {
    V1,
    V2,
    V3,
}

/// Messages where higher versions can be translated to lower versions of the same message
#[derive(Debug, PartialEq)]
pub enum TranslatableMessage {
    PlaybackUpdate(v3::PlaybackUpdateMessage),
    VolumeUpdate(v2::VolumeUpdateMessage),
}

impl TranslatableMessage {
    fn translate_and_serialize(&self, session_version: SessionVersion) -> Option<Vec<u8>> {
        macro_rules! ser {
            ($obj:expr) => {
                serde_json::to_vec(&$obj).ok()
            };
        }

        Some(match self {
            TranslatableMessage::PlaybackUpdate(msg) => match session_version {
                SessionVersion::V1 => ser!(v1::PlaybackUpdateMessage {
                    time: msg.time?,
                    state: msg.state,
                })?,
                SessionVersion::V2 => ser!(v2::PlaybackUpdateMessage {
                    generation_time: msg.generation_time,
                    time: msg.time?,
                    duration: msg.duration?,
                    speed: msg.speed?,
                    state: msg.state,
                })?,
                SessionVersion::V3 => ser!(msg)?,
            },
            TranslatableMessage::VolumeUpdate(msg) => match session_version {
                SessionVersion::V1 => ser!(v1::VolumeUpdateMessage { volume: msg.volume })?,
                SessionVersion::V2 | SessionVersion::V3 => ser!(msg)?,
            },
        })
    }
}

// #[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum ReceiverToSenderMessage {
    // Mandatory {
    //     op: Opcode,
    //     data: Vec<u8>,
    // },
    Translatable {
        op: Opcode,
        msg: TranslatableMessage,
    },
    PlayUpdate {
        msg: v3::PlayUpdateMessage,
    },
    Event {
        msg: v3::EventMessage,
    },
}

#[derive(Debug, thiserror::Error, PartialEq)]
enum StateError {
    #[error("invalid json")]
    InvalidJson,
    #[error("illegal version: {0}")]
    IllegalVersion(u64),
    #[error("illegal opcode: {0:?}")]
    IllegalOpcode(Opcode),
    #[error("missing body")]
    MissingBody,
}

#[derive(Debug)]
enum DriverEvent<'a> {
    Tick,
    Packet {
        opcode: Opcode,
        body: Option<&'a str>,
    },
    ToSender(Arc<ReceiverToSenderMessage>),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum Operation {
    Pause,
    Resume,
    Stop,
    Play(v3::PlayMessage),
    Seek(SeekMessage),
    SetSpeed(SetSpeedMessage),
    SetPlaylistItem(v3::SetPlaylistItemMessage),
    SetVolume(SetVolumeMessage),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
enum Action {
    None,
    Ping,
    Pong,
    EndSession,
    Op(Operation),
    SendInitial,
    Forward {
        session_version: Option<SessionVersion>,
        msg: Arc<ReceiverToSenderMessage>,
    },
}

#[derive(Debug, PartialEq)]
enum StateVariant {
    WaitingForVersion,
    Active { version: SessionVersion },
}

impl StateVariant {
    fn version(&self) -> Option<SessionVersion> {
        match self {
            StateVariant::Active { version } => Some(*version),
            _ => None,
        }
    }
}

bitflags! {
    #[derive(Debug)]
    struct MediaItemEventFlags: u8 {
        const Start = 1;
        const End = 1 << 1;
        const Changed = 1 << 2;
    }
}

bitflags! {
    #[derive(Debug)]
    struct KeyEventFlags: u8 {
        const ArrowLeft = 1;
        const ArrowRight = 1 << 1;
        const ArrowUp = 1 << 2;
        const ArrowDown = 1 << 3;
        const Enter = 1 << 4;
    }
}

impl KeyEventFlags {
    pub fn from_str(name: &str) -> Self {
        match name {
            "ArrowLeft" => Self::ArrowLeft,
            "ArrowRight" => Self::ArrowRight,
            "ArrowUp" => Self::ArrowUp,
            "ArrowDown" => Self::ArrowDown,
            "Enter" => Self::Enter,
            _ => Self::empty(),
        }
    }

    pub fn from_strings(names: &[String]) -> Self {
        let mut flags = Self::empty();
        for name in names {
            flags.insert(Self::from_str(name));
        }
        flags
    }
}

macro_rules! err_body {
    ($res:expr) => {
        $res.ok_or(StateError::MissingBody)?
    };
}

macro_rules! option_err_body {
    ($res:expr) => {
        $res.ok_or(StateError::MissingBody)
            .map_err(|err| Some(err))
            .ok()?
    };
}

macro_rules! err_json {
    ($t:ty, $body:expr) => {
        serde_json::from_str::<$t>($body).map_err(|_| StateError::InvalidJson)?
    };
}

macro_rules! option_err_json {
    ($t:ty, $body:expr) => {
        serde_json::from_str::<$t>($body)
            .map_err(|_| StateError::InvalidJson)
            .map_err(|err| Some(err))
            .ok()?
    };
}

#[derive(Debug)]
struct State {
    time: u32,
    last_packet_received: u32,
    waiting_for_pong: bool,
    variant: StateVariant,
    media_item_events: MediaItemEventFlags,
    key_name_events_down: KeyEventFlags,
    key_name_events_up: KeyEventFlags,
}

impl State {
    pub fn new() -> Self {
        Self {
            time: 0,
            last_packet_received: 0,
            waiting_for_pong: false,
            variant: StateVariant::WaitingForVersion,
            media_item_events: MediaItemEventFlags::empty(),
            key_name_events_down: KeyEventFlags::empty(),
            key_name_events_up: KeyEventFlags::empty(),
        }
    }

    fn handle_packet_uninit(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        Ok(match opcode {
            Opcode::None
            | Opcode::Play
            | Opcode::Pause
            | Opcode::Resume
            | Opcode::Stop
            | Opcode::Seek
            | Opcode::SetVolume => {
                self.variant = StateVariant::Active {
                    version: SessionVersion::V1,
                };
                return self.handle_packet_v1(opcode, body);
            }
            Opcode::Version => {
                let msg = err_json!(VersionMessage, err_body!(body));
                let version = match msg.version {
                    1 => SessionVersion::V1,
                    2 => SessionVersion::V2,
                    3 => SessionVersion::V3,
                    _ => return Err(StateError::IllegalVersion(msg.version)),
                };
                self.variant = StateVariant::Active { version };
                if version == SessionVersion::V3 {
                    Action::SendInitial
                } else {
                    Action::None
                }
            }
            // TODO: technically v2 doesn't need to accept VersionMessage before starting the session
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    /// Handle those packets that are common for v{1, 2, 3}
    // TODO: should it return option or error variant like Unsupported?
    fn handle_packet_common(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Option<Result<Action, StateError>> {
        Some(Ok(match opcode {
            Opcode::None => todo!(),
            Opcode::Play => {
                let msg = option_err_json!(v3::PlayMessage, option_err_body!(body));
                Action::Op(Operation::Play(msg))
            }
            Opcode::Pause => Action::Op(Operation::Pause),
            Opcode::Resume => Action::Op(Operation::Resume),
            Opcode::Stop => Action::Op(Operation::Stop),
            Opcode::Seek => {
                let msg = option_err_json!(SeekMessage, option_err_body!(body));
                Action::Op(Operation::Seek(msg))
            }
            Opcode::SetVolume => {
                let msg = option_err_json!(SetVolumeMessage, option_err_body!(body));
                Action::Op(Operation::SetVolume(msg))
            }
            // Ignore
            Opcode::PlaybackUpdate
            | Opcode::VolumeUpdate
            | Opcode::PlayUpdate
            | Opcode::PlaybackError => Action::None,
            _ => return None,
        }))
    }

    fn handle_packet_v1(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        if let Some(res) = self.handle_packet_common(opcode, body) {
            return res;
        };

        Err(StateError::IllegalOpcode(opcode))
    }

    fn handle_packet_v2(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        if let Some(res) = self.handle_packet_common(opcode, body) {
            return res;
        };

        Ok(match opcode {
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            Opcode::SetSpeed => {
                let msg = err_json!(SetSpeedMessage, err_body!(body));
                Action::Op(Operation::SetSpeed(msg))
            }
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    fn handle_packet_v3(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        if let Some(res) = self.handle_packet_common(opcode, body) {
            return res;
        };

        let v2_res = self.handle_packet_v2(opcode, body);
        if !matches!(v2_res, Err(StateError::IllegalOpcode(_))) {
            return v2_res;
        }

        debug!(?opcode);

        Ok(match opcode {
            Opcode::Initial => {
                let _msg = err_json!(v3::InitialSenderMessage, err_body!(body));
                Action::None
            }
            Opcode::SetPlaylistItem => {
                let msg = err_json!(v3::SetPlaylistItemMessage, err_body!(body));
                Action::Op(Operation::SetPlaylistItem(msg))
            }
            Opcode::SubscribeEvent => {
                let msg = err_json!(v3::SubscribeEventMessage, err_body!(body));
                match msg.event {
                    v3::EventSubscribeObject::MediaItemStart => {
                        self.media_item_events.insert(MediaItemEventFlags::Start)
                    }
                    v3::EventSubscribeObject::MediaItemEnd => {
                        self.media_item_events.insert(MediaItemEventFlags::End)
                    }
                    v3::EventSubscribeObject::MediaItemChanged => {
                        self.media_item_events.insert(MediaItemEventFlags::Changed)
                    }
                    v3::EventSubscribeObject::KeyDown { keys } => self
                        .key_name_events_down
                        .insert(KeyEventFlags::from_strings(&keys)),
                    v3::EventSubscribeObject::KeyUp { keys } => self
                        .key_name_events_up
                        .insert(KeyEventFlags::from_strings(&keys)),
                }
                Action::None
            }
            Opcode::UnsubscribeEvent => {
                let msg = err_json!(v3::UnsubscribeEventMessage, err_body!(body));
                match msg.event {
                    v3::EventSubscribeObject::MediaItemStart => {
                        self.media_item_events.remove(MediaItemEventFlags::Start)
                    }
                    v3::EventSubscribeObject::MediaItemEnd => {
                        self.media_item_events.remove(MediaItemEventFlags::End)
                    }
                    v3::EventSubscribeObject::MediaItemChanged => {
                        self.media_item_events.remove(MediaItemEventFlags::Changed)
                    }
                    v3::EventSubscribeObject::KeyDown { keys } => self
                        .key_name_events_down
                        .remove(KeyEventFlags::from_strings(&keys)),
                    v3::EventSubscribeObject::KeyUp { keys } => self
                        .key_name_events_up
                        .remove(KeyEventFlags::from_strings(&keys)),
                }
                Action::None
            }
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    pub fn advance(&mut self, event: DriverEvent) -> Result<Action, StateError> {
        Ok(match event {
            DriverEvent::Tick => {
                self.time += 1;
                let diff = self.time - self.last_packet_received;
                if diff >= TICKS_BEFORE_PING {
                    if self.waiting_for_pong && diff >= TICKS_BEFORE_PING * 2 {
                        Action::EndSession
                    } else if !self.waiting_for_pong {
                        self.waiting_for_pong = true;
                        Action::Ping
                    } else {
                        Action::None
                    }
                } else {
                    Action::None
                }
            }
            DriverEvent::Packet { opcode, body } => {
                self.last_packet_received = self.time;
                self.waiting_for_pong = false;

                match &self.variant {
                    StateVariant::WaitingForVersion => {
                        return self.handle_packet_uninit(opcode, body);
                    }
                    StateVariant::Active { version } => {
                        return match version {
                            SessionVersion::V1 => self.handle_packet_v1(opcode, body),
                            SessionVersion::V2 => self.handle_packet_v2(opcode, body),
                            SessionVersion::V3 => self.handle_packet_v3(opcode, body),
                        };
                    }
                }
            }
            DriverEvent::ToSender(msg) => match msg.as_ref() {
                // ReceiverToSenderMessage::Mandatory { .. }
                // | ReceiverToSenderMessage::Translatable { .. } => Action::Forward {
                ReceiverToSenderMessage::Translatable { .. } => Action::Forward {
                    session_version: self.variant.version(),
                    msg,
                },
                ReceiverToSenderMessage::Event { msg: event_msg } => {
                    match &event_msg.event {
                        v3::EventObject::MediaItem { variant, .. } => {
                            let flag = match &variant {
                                v3::EventType::MediaItemStart => MediaItemEventFlags::Start,
                                v3::EventType::MediaItemEnd => MediaItemEventFlags::End,
                                v3::EventType::MediaItemChange => MediaItemEventFlags::Changed,
                                _ => return Ok(Action::None), // Unreachable
                            };

                            if self.media_item_events.contains(flag) {
                                Action::Forward {
                                    session_version: self.variant.version(),
                                    msg,
                                }
                            } else {
                                Action::None
                            }
                        }
                        v3::EventObject::Key { variant, key, .. } => {
                            let flag = KeyEventFlags::from_str(key);
                            if flag.is_empty() {
                                return Ok(Action::None);
                            }
                            let session_version = self.variant.version();
                            match &variant {
                                v3::EventType::KeyDown => {
                                    if self.key_name_events_down.contains(flag) {
                                        Action::Forward {
                                            session_version,
                                            msg,
                                        }
                                    } else {
                                        Action::None
                                    }
                                }
                                v3::EventType::KeyUp => {
                                    if self.key_name_events_down.contains(flag) {
                                        Action::Forward {
                                            session_version,
                                            msg,
                                        }
                                    } else {
                                        Action::None
                                    }
                                }
                                _ => Action::None, // Unreachable
                            }
                        }
                    }
                }
                ReceiverToSenderMessage::PlayUpdate { .. } => {
                    if matches!(
                        self.variant,
                        StateVariant::Active {
                            version: SessionVersion::V3
                        }
                    ) {
                        Action::Forward {
                            session_version: None,
                            msg,
                        }
                    } else {
                        Action::None
                    }
                }
            },
        })
    }
}

pub struct SessionDriver {
    stream: TcpStream,
    id: SessionId,
    state: State,
}

impl SessionDriver {
    pub fn new(stream: TcpStream, id: SessionId) -> Self {
        Self {
            stream,
            id,
            state: State::new(),
        }
    }

    async fn write_simple(tcp_stream_tx: &mut WriteHalf<'_>, opcode: Opcode) -> anyhow::Result<()> {
        let header = Header::new(opcode, 0).encode();
        tcp_stream_tx.write_all(&header).await?;
        trace!(?opcode, "Sent simple packet");
        Ok(())
    }

    async fn write_packet(stream: &mut WriteHalf<'_>, packet: Packet) -> anyhow::Result<()> {
        let bytes = packet.encode()?;
        stream.write_all(&bytes).await?;
        Ok(())
    }

    /// Returns true if the session should end.
    async fn handle_state_result(
        id: SessionId,
        tcp_stream_tx: &mut WriteHalf<'_>,
        event_tx: &UnboundedSender<Event>,
        res: Result<Action, StateError>,
    ) -> anyhow::Result<bool> {
        match res {
            Ok(action) => match action {
                Action::None => (),
                Action::Ping => Self::write_simple(tcp_stream_tx, Opcode::Ping).await?,
                // Action::Pong => write_packet(tcp_stream_tx, Packet::Pong).await?,
                Action::Pong => Self::write_simple(tcp_stream_tx, Opcode::Pong).await?,
                Action::EndSession => return Ok(true),
                Action::Op(operation) => {
                    event_tx.send(Event::Op {
                        session_id: id,
                        op: operation,
                    })?;
                }
                Action::SendInitial => {
                    Self::write_packet(
                        tcp_stream_tx,
                        Packet::Initial(v3::InitialReceiverMessage {
                            display_name: None,
                            app_name: Some("FCast Receiver Desktop".to_owned()),
                            app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                            play_data: None,
                            experimental_capabilities: Some(ReceiverCapabilities {
                                av: Some(v3::AVCapabilities {
                                    livestream: Some(v3::LivestreamCapabilities {
                                        whep: Some(true),
                                    }),
                                }),
                            }),
                        }),
                    )
                    .await?
                }
                Action::Forward {
                    session_version,
                    msg,
                } => match msg.as_ref() {
                    ReceiverToSenderMessage::Translatable { op, msg } => {
                        let Some(session_version) = session_version else {
                            // Unreachable
                            error!("Missing session version");
                            return Ok(false);
                        };
                        if let Some(body) = msg.translate_and_serialize(session_version) {
                            let header = Header::new(*op, body.len() as u32).encode();
                            tcp_stream_tx.write_all(&header).await?;
                            tcp_stream_tx.write_all(&body).await?;
                        } else {
                            error!("Could not translate message");
                        }
                    }
                    ReceiverToSenderMessage::Event { msg } => {
                        let body = serde_json::to_vec(&msg)?;
                        let header = Header::new(Opcode::Event, body.len() as u32).encode();
                        tcp_stream_tx.write_all(&header).await?;
                        tcp_stream_tx.write_all(&body).await?;
                    }
                    ReceiverToSenderMessage::PlayUpdate { msg } => {
                        let body = serde_json::to_vec(&msg)?;
                        let header = Header::new(Opcode::PlayUpdate, body.len() as u32).encode();
                        tcp_stream_tx.write_all(&header).await?;
                        tcp_stream_tx.write_all(&body).await?;
                    }
                },
            },
            Err(err) => {
                error!(?err, "Error occured when advancing state");
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn read_packet(
        stream: &mut ReadHalf<'_>,
        read_buf: &mut [u8; MAX_BODY_SIZE],
    ) -> anyhow::Result<(Opcode, Option<String>)> {
        let mut header_buf: [u8; HEADER_BUFFER_SIZE] = [0; HEADER_BUFFER_SIZE];

        stream.read_exact(&mut header_buf).await?;

        let header = Header::decode(header_buf);

        let mut body_string = None;

        if header.size as usize > read_buf.len() {
            bail!("Too big body size {}", header.size);
        }

        if header.size > 0 {
            // let mut body_buf = vec![0; header.size as usize];
            // stream.read_exact(&mut body_buf).await?;
            stream
                .read_exact(&mut read_buf[0..header.size as usize])
                .await?;
            // body_string = Some(String::from_utf8(read_buf[0..header.size as usize])?);
            body_string = Some(str::from_utf8(&read_buf[0..header.size as usize])?.to_owned());
        }

        Ok((header.opcode, body_string))
    }

    // TODO: instrument this in caller with the id etc.
    pub async fn run(
        mut self,
        // TODO: this should contain events that are subscribable
        // updates_rx: Receiver<Arc<Vec<u8>>>,
        mut updates_rx: Receiver<Arc<ReceiverToSenderMessage>>,
        event_tx: &UnboundedSender<Event>,
    ) -> anyhow::Result<()> {
        debug!("id={} Session was started", self.id);

        let (tcp_stream_rx, mut tcp_stream_tx) = self.stream.split();

        let packets_stream = unfold(
            (tcp_stream_rx, Box::new([0; MAX_BODY_SIZE])),
            |(mut tcp_stream, mut body_buf)| async move {
                match Self::read_packet(&mut tcp_stream, &mut body_buf).await {
                    Ok(p) => Some((p, (tcp_stream, body_buf))),
                    Err(err) => {
                        error!("Failed to receive packet: {err}");
                        None
                    }
                }
            },
        );

        tokio::pin!(packets_stream);

        let mut tick_interval = tokio::time::interval(Duration::from_secs(1));

        Self::write_packet(
            &mut tcp_stream_tx,
            Packet::Version(VersionMessage { version: 3 }),
        )
        .await?;

        loop {
            tokio::select! {
                r = packets_stream.next() => {
                    let Some(packet) = r else {
                        break;
                    };

                    trace!("id={} Got packet: {packet:?}", self.id);

                    let opcode = packet.0;
                    let body = packet.1.as_ref();
                    let res = self.state.advance(DriverEvent::Packet { opcode, body: body.map(|b| b.as_str()) });
                    if Self::handle_state_result(self.id, &mut tcp_stream_tx, event_tx, res).await? {
                        break;
                    }
                }
                res = updates_rx.recv() => {
                    let Ok(to_sender) = res else {
                        break;
                    };

                    let res = self.state.advance(DriverEvent::ToSender(to_sender));
                    if Self::handle_state_result(self.id, &mut tcp_stream_tx, event_tx, res).await? {
                        break;
                    }
                }
                _ = tick_interval.tick() => {
                    let res = self.state.advance(DriverEvent::Tick);
                    if Self::handle_state_result(self.id, &mut tcp_stream_tx, event_tx, res).await? {
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_advancements(state: &mut State, events: Vec<(DriverEvent, Result<Action, StateError>)>) {
        for (event, res) in events.into_iter() {
            assert_eq!(state.advance(event), res);
        }
    }

    #[test]
    fn timeout() {
        let mut state = State::new();
        let mut events = Vec::new();
        for _ in 0..TICKS_BEFORE_PING - 1 {
            events.push((DriverEvent::Tick, Ok(Action::None)));
        }
        events.push((DriverEvent::Tick, Ok(Action::Ping)));
        for _ in 0..TICKS_BEFORE_PING - 1 {
            events.push((DriverEvent::Tick, Ok(Action::None)));
        }
        events.push((DriverEvent::Tick, Ok(Action::EndSession)));
        run_advancements(&mut state, events);
    }

    #[test]
    fn uninit_to_active() {
        let v2_json = serde_json::to_string(&VersionMessage { version: 2 }).unwrap();
        let v3_json = serde_json::to_string(&VersionMessage { version: 3 }).unwrap();
        let sessions = [
            (
                Opcode::Resume,
                None,
                Action::Op(Operation::Resume),
                SessionVersion::V1,
            ),
            (
                Opcode::Version,
                Some(v2_json.as_str()),
                Action::None,
                SessionVersion::V2,
            ),
            (
                Opcode::Version,
                Some(v3_json.as_str()),
                Action::SendInitial,
                SessionVersion::V3,
            ),
        ];

        for (opcode, body, res, version) in sessions {
            let mut state = State::new();
            run_advancements(
                &mut state,
                vec![(DriverEvent::Packet { opcode, body }, Ok(res))],
            );
            assert_eq!(state.variant, StateVariant::Active { version });
        }
    }

    #[test]
    fn invalid_json() {
        let mut state = State::new();
        run_advancements(
            &mut state,
            vec![(
                DriverEvent::Packet {
                    opcode: Opcode::Version,
                    body: Some("{"),
                },
                Err(StateError::InvalidJson),
            )],
        );
    }

    #[test]
    fn illegal_opcode() {
        let v2_json = serde_json::to_string(&VersionMessage { version: 2 }).unwrap();
        let mut state = State::new();
        run_advancements(
            &mut state,
            vec![
                (
                    DriverEvent::Packet {
                        opcode: Opcode::Version,
                        body: Some(v2_json.as_str()),
                    },
                    Ok(Action::None),
                ),
                (
                    DriverEvent::Packet {
                        opcode: Opcode::Initial,
                        body: None,
                    },
                    Err(StateError::IllegalOpcode(Opcode::Initial)),
                ),
            ],
        );
    }

    // TODO: test more of v3
}
