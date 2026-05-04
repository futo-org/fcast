use std::{collections::HashMap, io, sync::Arc, time::Duration};

use crate::MessageSender;
use bitflags::bitflags;
use fcast_protocol::{
    Opcode, PlaybackErrorMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage,
    companion::{self, ResourceInfoResponse},
    v1,
    v2::{self, PlayMessage, PlaybackUpdateMessage, VolumeUpdateMessage},
    v3::{self, InitialReceiverMessage, ReceiverCapabilities},
    v4,
};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{
        broadcast::Receiver,
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tracing::{debug, error, instrument, trace};

pub type SessionId = u64;

const TICKS_BEFORE_PING: u32 = 3;

pub const MAX_BODY_SIZE: usize = 32000;

use anyhow::{Context, bail};

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
    CompanionHello(companion::HelloResponse),
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
            Packet::CompanionHello(_) => Opcode::CompanionHello,
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
            Packet::CompanionHello(hello_response) => hello_response.serialize().to_vec(),
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
    V4 { companion_provider_id: Option<u16> },
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
                SessionVersion::V4 { .. } => {
                    return None;
                }
            },
            TranslatableMessage::VolumeUpdate(msg) => match session_version {
                SessionVersion::V1 => ser!(v1::VolumeUpdateMessage { volume: msg.volume })?,
                SessionVersion::V2 | SessionVersion::V3 => ser!(msg)?,
                SessionVersion::V4 { .. } => return None,
            },
        })
    }
}

#[derive(Debug, PartialEq)]
pub enum V4Message {
    PositionUpdated(f64),
    VolumeChanged(f64),
    DurationChanged(f64),
    PlaybackStateChanged(v4::PlaybackState),
}

// #[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum ReceiverToSenderMessage {
    // Mandatory {
    //     op: Opcode,
    //     data: Vec<u8>,
    // },
    Error(PlaybackErrorMessage),
    LegacyTranslatable {
        op: Opcode,
        msg: TranslatableMessage,
    },
    PlayUpdate {
        msg: v3::PlayUpdateMessage,
    },
    Event {
        msg: v3::EventMessage,
    },
    V4(V4Message),
}

#[derive(Debug, thiserror::Error, PartialEq)]
enum StateError {
    #[error("body is not valid UTF-8")]
    BodyIsNotUtf8,
    #[error("invalid json")]
    InvalidJson,
    #[error("illegal version: {0}")]
    IllegalVersion(u64),
    #[error("illegal opcode: {0:?}")]
    IllegalOpcode(Opcode),
    #[error("missing body")]
    MissingBody,
    #[error("invalid body")]
    InvalidBody,
}

#[derive(Debug)]
enum DriverEvent<'a> {
    Tick,
    Packet {
        opcode: Opcode,
        // body: Option<&'a str>,
        body: Option<&'a [u8]>,
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
    SetVolumeNew(f64),
    SetSpeedNew(f64),
    SeekNew(f64),
}

#[derive(Debug, PartialEq)]
enum CompanionResponse {
    ResourceInfo(companion::ResourceInfoResponse),
    ResourceResponse(companion::ResourceResponse),
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
    StartTLS,
    RespondCompanionHello,
    Companion(CompanionResponse),
}

#[derive(Debug, PartialEq)]
enum StateVariant {
    WaitingForVersion,
    Active { version: SessionVersion },
    WaitingForStartTLS,
    UninitV4,
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

type CompanionMsgSender = UnboundedSender<CompanionMessage>;
type CompanionMsgReceiver = UnboundedReceiver<CompanionMessage>;

pub struct BlockingFeedbackChannel<T>(Arc<(Mutex<Option<T>>, Condvar)>);

impl<T> Clone for BlockingFeedbackChannel<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> BlockingFeedbackChannel<T> {
    pub fn new() -> Self {
        Self(Arc::new((Mutex::new(None::<T>), Condvar::new())))
    }

    fn send(&self, obj: T) {
        *self.0.0.lock() = Some(obj);
        self.0.1.notify_one();
    }

    // TODO: accept timeout
    pub fn recv(&self) -> Option<T> {
        let mut lock = self.0.0.lock();
        loop {
            debug!("Receiving object");
            if let Some(obj) = lock.take() {
                debug!("Received obj");
                return Some(obj);
            }
            debug!("Wating");
            self.0.1.wait(&mut lock);
        }
    }
}

pub enum FeedbackSender<T> {
    Oneshot(oneshot::Sender<T>),
    Blocking(BlockingFeedbackChannel<T>),
}

impl<T> FeedbackSender<T> {
    fn send(self, obj: T) {
        match self {
            FeedbackSender::Oneshot(sender) => {
                let _ = sender.send(obj);
            }
            FeedbackSender::Blocking(chan) => {
                chan.send(obj);
            }
        }
    }
}

pub enum CompanionMessage {
    GetResourceInfo {
        id: companion::ResourceId,
        feedback: FeedbackSender<companion::ResourceInfoResponse>,
    },
    GetResource {
        id: companion::ResourceId,
        read_head: companion::ReadHead,
        feedback: FeedbackSender<companion::ResourceResponse>,
    },
}

#[derive(Clone)]
pub struct CompanionProviderHandle {
    tx: CompanionMsgSender,
}

impl CompanionProviderHandle {
    pub fn get_resource_info_blocking(
        &self,
        resource_id: companion::ResourceId,
    ) -> BlockingFeedbackChannel<companion::ResourceInfoResponse> {
        let chan = BlockingFeedbackChannel::new();
        self.tx
            .send(CompanionMessage::GetResourceInfo {
                id: resource_id,
                feedback: FeedbackSender::Blocking(chan.clone()),
            })
            .unwrap(); // TODO: handle error
        chan
    }

    pub fn get_resoure_info(
        &self,
        resource_id: companion::ResourceId,
    ) -> anyhow::Result<oneshot::Receiver<ResourceInfoResponse>> {
        let (feedback, rx) = oneshot::channel();
        self.tx.send(CompanionMessage::GetResourceInfo {
            id: resource_id,
            feedback: FeedbackSender::Oneshot(feedback),
        })?;
        Ok(rx)
    }

    pub fn get_resource_blocking(
        &self,
        resource_id: companion::ResourceId,
        read_head: companion::ReadHead,
    ) -> BlockingFeedbackChannel<companion::ResourceResponse> {
        let chan = BlockingFeedbackChannel::new();
        self.tx
            .send(CompanionMessage::GetResource {
                id: resource_id,
                read_head,
                feedback: FeedbackSender::Blocking(chan.clone()),
            })
            .unwrap(); // TODO: handle error
        chan
    }

    pub async fn get_resouce(
        &self,
        resource_id: companion::ResourceId,
        read_head: companion::ReadHead,
    ) -> std::result::Result<companion::ResourceResponse, oneshot::error::RecvError> {
        let (feedback, rx) = oneshot::channel();
        self.tx
            .send(CompanionMessage::GetResource {
                id: resource_id,
                read_head,
                feedback: FeedbackSender::Oneshot(feedback),
            })
            .unwrap(); // TODO: handle error
        rx.await
    }
}

#[derive(Default)]
struct InnerCompanionContext {
    providers: HashMap<companion::ProviderId, CompanionProviderHandle>,
}

impl InnerCompanionContext {
    fn register_provider(&mut self, tx: CompanionMsgSender) -> companion::ProviderId {
        let mut id = 0;
        while self.providers.contains_key(&id) {
            id += 1;
        }

        let handle = CompanionProviderHandle { tx };
        self.providers.insert(id, handle);

        id
    }

    pub fn unregister_provider(&mut self, id: companion::ProviderId) {
        debug!(id, "Unregistering provider");
        self.providers.remove(&id);
    }
}

#[derive(Clone)]
pub struct CompanionContext(Arc<Mutex<InnerCompanionContext>>);

impl std::fmt::Debug for CompanionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CompanionContext").finish()
    }
}

impl CompanionContext {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(InnerCompanionContext::default())))
    }

    pub fn register_provider(&self, tx: CompanionMsgSender) -> companion::ProviderId {
        self.0.lock().register_provider(tx)
    }

    pub fn unregister_provider(&self, id: companion::ProviderId) {
        self.0.lock().unregister_provider(id)
    }

    pub fn get_provider(&self, id: companion::ProviderId) -> Option<CompanionProviderHandle> {
        self.0.lock().providers.get(&id).cloned()
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
        match serde_json::from_str::<$t>($body) {
            Ok(obj) => obj,
            Err(err) => {
                error!(?err, "Failed to decode json object");
                return Some(Err(StateError::InvalidJson));
            }
        }
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

macro_rules! stringify {
    ($bytes:expr) => {
        match $bytes {
            Some(bytes) => match str::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(_) => return Err(StateError::BodyIsNotUtf8),
            },
            None => None,
        }
    };
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
                    4 => {
                        self.variant = StateVariant::WaitingForStartTLS;
                        // self.variant = StateVariant::UninitV4;
                        return Ok(Action::None);
                    }
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
    #[instrument(skip_all)]
    fn handle_packet_common(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Option<Result<Action, StateError>> {
        Some(Ok(match opcode {
            Opcode::None => Action::None,
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

    #[instrument(skip_all)]
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

    #[instrument(skip_all)]
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

    #[instrument(skip_all)]
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

    #[instrument(skip_all)]
    fn handle_packet_v4(
        &mut self,
        opcode: Opcode,
        body: Option<&[u8]>,
    ) -> Result<Action, StateError> {
        Ok(match opcode {
            Opcode::Play => {
                let msg = err_json!(v4::PlayMessage, err_body!(stringify!(body)));
                match msg {
                    v4::PlayMessage::Single { media_item: item } => {
                        Action::Op(Operation::Play(v3::PlayMessage {
                            container: item.container,
                            url: Some(item.source_url),
                            content: None,
                            time: None,
                            volume: None,
                            speed: None,
                            headers: None,
                            metadata: None,
                        }))
                    }
                    v4::PlayMessage::Queue { items, start_index } => todo!(),
                }
            }
            Opcode::Seek => Action::Op(Operation::SeekNew(
                err_json!(v4::SeekMessage, err_body!(stringify!(body))).time,
            )),
            Opcode::SetVolume => Action::Op(Operation::SetVolumeNew(
                err_json!(v4::UpdateVolumeMessage, err_body!(stringify!(body))).volume,
            )),
            Opcode::PlaybackError => todo!(),
            Opcode::SetSpeed => Action::Op(Operation::SetSpeedNew(
                err_json!(v4::SetSpeedMessage, err_body!(stringify!(body))).speed,
            )),
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            Opcode::Initial => {
                let msg = err_json!(v4::InitialSenderMessage, err_body!(stringify!(body)));
                debug!(?msg, "Got inital sender message");
                Action::None
            }
            Opcode::UpdatePlaybackState => {
                let msg = err_json!(v4::UpdatePlaybackStateMessage, err_body!(stringify!(body)));
                debug!(?msg);
                match msg.state {
                    v4::PlaybackState::Idle => Action::Op(Operation::Stop),
                    v4::PlaybackState::Playing => Action::Op(Operation::Resume),
                    v4::PlaybackState::Paused => Action::Op(Operation::Pause),
                    v4::PlaybackState::Buffering | v4::PlaybackState::Ended => todo!(),
                }
            }

            // Only sent from receiver to sender
            Opcode::PositionChanged | Opcode::DurationChanged | Opcode::TracksAvailable => {
                return Err(StateError::IllegalOpcode(opcode));
            }

            Opcode::QueueInsert => todo!(),
            Opcode::QueueRemove => todo!(),
            Opcode::ChangeTrack => todo!(),
            Opcode::QueueItemSelected => todo!(),
            Opcode::AddSubtitleSource => todo!(),
            Opcode::SetStatusUpdateInterval => todo!(),
            Opcode::CompanionHello => Action::RespondCompanionHello,
            Opcode::ResourceInfo => {
                let Some(body) = body else {
                    return Err(StateError::MissingBody);
                };
                let resp = companion::ResourceInfoResponse::parse(body)
                    .map_err(|_| StateError::InvalidBody)?;
                Action::Companion(CompanionResponse::ResourceInfo(resp))
            }
            Opcode::Resource => {
                let Some(body) = body else {
                    return Err(StateError::MissingBody);
                };
                let resp = companion::ResourceResponse::parse(body)
                    .map_err(|_| StateError::InvalidBody)?;
                Action::Companion(CompanionResponse::ResourceResponse(resp))
            }
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    #[instrument(skip_all)]
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
                        return self.handle_packet_uninit(opcode, stringify!(body));
                    }
                    StateVariant::Active { version } => {
                        return match version {
                            SessionVersion::V1 => self.handle_packet_v1(opcode, stringify!(body)),
                            SessionVersion::V2 => self.handle_packet_v2(opcode, stringify!(body)),
                            SessionVersion::V3 => self.handle_packet_v3(opcode, stringify!(body)),
                            SessionVersion::V4 { .. } => self.handle_packet_v4(opcode, body),
                        };
                    }
                    StateVariant::WaitingForStartTLS => match opcode {
                        Opcode::StartTLS => {
                            self.variant = StateVariant::UninitV4;
                            Action::StartTLS
                        }
                        _ => return Err(StateError::IllegalOpcode(opcode)),
                    },
                    StateVariant::UninitV4 => match opcode {
                        Opcode::Initial => {
                            self.variant = StateVariant::Active {
                                version: SessionVersion::V4 {
                                    companion_provider_id: None,
                                },
                            };
                            Action::None
                        }
                        _ => return Err(StateError::IllegalOpcode(opcode)),
                    },
                }
            }
            // DriverEvent::ToSender(msg) => match msg.as_ref() {
            DriverEvent::ToSender(msg) => match msg.as_ref() {
                ReceiverToSenderMessage::Error(_)
                | ReceiverToSenderMessage::LegacyTranslatable { .. } => match self.variant {
                    StateVariant::Active { version } => match version {
                        SessionVersion::V1 | SessionVersion::V2 | SessionVersion::V3 => {
                            Action::Forward {
                                session_version: self.variant.version(),
                                msg,
                            }
                        }
                        SessionVersion::V4 { .. } => Action::None,
                    },
                    _ => Action::None,
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
                ReceiverToSenderMessage::V4(_) => {
                    if matches!(
                        self.variant,
                        StateVariant::Active {
                            version: SessionVersion::V4 { .. },
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

#[derive(Default)]
enum NetworkStream {
    #[default]
    None,
    Tcp {
        rx: tokio::io::ReadHalf<TcpStream>,
        tx: tokio::io::BufWriter<tokio::io::WriteHalf<TcpStream>>,
    },
    Tls {
        tx: tokio::io::BufWriter<tokio::io::WriteHalf<TlsStream<TcpStream>>>,
        rx: tokio::io::ReadHalf<TlsStream<TcpStream>>,
    },
}

impl NetworkStream {
    pub fn new(stream: TcpStream) -> Self {
        let (rx, tx) = tokio::io::split(stream);
        let tx = tokio::io::BufWriter::new(tx);

        Self::Tcp { rx, tx }
    }

    #[instrument(skip_all)]
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp { rx, .. } => rx.read(buf).await,
            Self::Tls { rx, .. } => rx.read(buf).await,
            Self::None => unreachable!(),
        }
    }

    #[instrument(skip_all)]
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { tx, .. } => tx.write_all(buf).await,
            Self::Tls { tx, .. } => tx.write_all(buf).await,
            Self::None => unreachable!(),
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        match self {
            NetworkStream::Tcp { tx, .. } => tx.flush().await?,
            NetworkStream::Tls { tx, .. } => tx.flush().await?,
            _ => (),
        }

        Ok(())
    }

    async fn upgrade(&mut self, acceptor: &TlsAcceptor) -> io::Result<()> {
        let old = std::mem::take(self);
        *self = match old {
            NetworkStream::Tcp { rx, tx } => {
                let tx = tx.into_inner();
                let stream = rx.unsplit(tx);

                let stream = acceptor.accept(stream).await?;
                let (rx, tx) = tokio::io::split(stream);
                let tx = tokio::io::BufWriter::new(tx);
                Self::Tls { tx, rx }
            }
            _ => old,
        };

        Ok(())
    }
}

enum CompanionQueueItem {
    GetResourceInfo(FeedbackSender<companion::ResourceInfoResponse>),
    GetResource(FeedbackSender<companion::ResourceResponse>),
}

pub struct SessionDriver {
    stream: NetworkStream,
    id: SessionId,
    state: State,
    tls_acceptor: TlsAcceptor,
    companion_ctx: CompanionContext,
    internal_companion_tx: CompanionMsgSender,
    companion_queue: HashMap<companion::ResourceId, CompanionQueueItem>,
    req_id_gen: companion::RequestIdGenerator,
}

impl SessionDriver {
    pub fn new(
        stream: TcpStream,
        id: SessionId,
        tls_acceptor: TlsAcceptor,
        companion_ctx: CompanionContext,
        internal_companion_tx: CompanionMsgSender,
    ) -> Self {
        Self {
            stream: NetworkStream::new(stream),
            id,
            state: State::new(),
            tls_acceptor,
            companion_ctx,
            internal_companion_tx,
            companion_queue: HashMap::new(),
            req_id_gen: companion::RequestIdGenerator::new(),
        }
    }

    #[instrument(skip_all)]
    async fn write_simple(&mut self, opcode: Opcode) -> anyhow::Result<()> {
        let header = Header::new(opcode, 0).encode();
        self.stream.write_all(&header).await?;
        self.stream.flush().await?;
        trace!(?opcode, "Sent simple packet");
        Ok(())
    }

    #[instrument(skip_all)]
    async fn write_packet(&mut self, packet: Packet) -> anyhow::Result<()> {
        let bytes = packet.encode()?;
        self.stream.write_all(&bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn send_msg(&mut self, opcode: Opcode, msg: impl Serialize) -> anyhow::Result<()> {
        let body = serde_json::to_vec(&msg)?;
        let header = Header::new(opcode, body.len() as u32).encode();
        self.stream.write_all(&header).await?;
        self.stream.write_all(&body).await?;
        self.stream.flush().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn send_companion_msg(&mut self, opcode: Opcode, msg: &[u8]) -> anyhow::Result<()> {
        let header = Header::new(opcode, msg.len() as u32).encode();
        self.stream.write_all(&header).await?;
        self.stream.write_all(&msg).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Returns true if the session should end.
    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    async fn handle_state_result(
        &mut self,
        id: SessionId,
        msg_tx: &MessageSender,
        res: Result<Action, StateError>,
    ) -> anyhow::Result<bool> {
        match res {
            Ok(action) => match action {
                Action::None => (),
                Action::Ping => self.write_simple(Opcode::Ping).await?,
                Action::Pong => self.write_simple(Opcode::Pong).await?,
                Action::EndSession => return Ok(true),
                Action::Op(operation) => {
                    msg_tx.operation(id, operation);
                }
                Action::SendInitial => {
                    self.write_packet(Packet::Initial(v3::InitialReceiverMessage {
                        display_name: None,
                        app_name: Some("FCast Receiver Desktop".to_owned()),
                        app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                        play_data: None,
                        experimental_capabilities: Some(ReceiverCapabilities {
                            av: Some(v3::AVCapabilities {
                                livestream: Some(v3::LivestreamCapabilities { whep: Some(true) }),
                            }),
                        }),
                    }))
                    .await?
                }
                Action::Forward {
                    session_version,
                    msg,
                } => match msg.as_ref() {
                    ReceiverToSenderMessage::Error(msg) => {
                        let body = serde_json::to_vec(&msg)?;
                        let header = Header::new(Opcode::PlaybackError, body.len() as u32).encode();
                        self.stream.write_all(&header).await?;
                        self.stream.write_all(&body).await?;
                    }
                    ReceiverToSenderMessage::LegacyTranslatable { op, msg } => {
                        let Some(session_version) = session_version else {
                            // Unreachable
                            error!("Missing session version");
                            return Ok(false);
                        };
                        if let Some(body) = msg.translate_and_serialize(session_version) {
                            let header = Header::new(*op, body.len() as u32).encode();
                            self.stream.write_all(&header).await?;
                            self.stream.write_all(&body).await?;
                        } else {
                            error!("Could not translate message");
                        }
                    }
                    ReceiverToSenderMessage::Event { msg } => {
                        self.send_msg(Opcode::Event, msg).await?;
                        // let body = serde_json::to_vec(&msg)?;
                        // let header = Header::new(Opcode::Event, body.len() as u32).encode();
                        // self.stream.write_all(&header).await?;
                        // self.stream.write_all(&body).await?;
                    }
                    ReceiverToSenderMessage::PlayUpdate { msg } => {
                        self.send_msg(Opcode::PlayUpdate, msg).await?;
                        // let body = serde_json::to_vec(&msg)?;
                        // let header = Header::new(Opcode::PlayUpdate, body.len() as u32).encode();
                        // self.stream.write_all(&header).await?;
                        // self.stream.write_all(&body).await?;
                    }
                    ReceiverToSenderMessage::V4(msg) => match msg {
                        V4Message::PositionUpdated(position) => {
                            let msg = v4::PositionChangedMessage {
                                position: *position,
                            };
                            self.send_msg(Opcode::PositionChanged, msg).await?;
                        }
                        V4Message::VolumeChanged(volume) => {
                            let msg = v4::UpdateVolumeMessage { volume: *volume };
                            self.send_msg(Opcode::VolumeUpdate, msg).await?;
                        }
                        V4Message::DurationChanged(duration) => {
                            let msg = v4::DurationChangedMessage {
                                duration: *duration,
                            };
                            self.send_msg(Opcode::DurationChanged, msg).await?;
                        }
                        V4Message::PlaybackStateChanged(state) => {
                            let msg = v4::UpdatePlaybackStateMessage { state: *state };
                            self.send_msg(Opcode::UpdatePlaybackState, msg).await?;
                        }
                    },
                },
                Action::StartTLS => {
                    self.write_simple(Opcode::StartTLS).await?;
                    debug!("Upgrading network stream to use TLS");
                    self.stream.upgrade(&self.tls_acceptor).await?;
                    debug!("Upgraded successfully");

                    // TODO: v4
                    self.write_packet(Packet::Initial(v3::InitialReceiverMessage {
                        display_name: None,
                        app_name: Some("FCast Receiver Desktop".to_owned()),
                        app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                        play_data: None,
                        experimental_capabilities: Some(ReceiverCapabilities {
                            av: Some(v3::AVCapabilities {
                                livestream: Some(v3::LivestreamCapabilities { whep: Some(true) }),
                            }),
                        }),
                    }))
                    .await?
                }
                Action::RespondCompanionHello => {
                    // let (id, mut rx) = self.companion_ctx.register_provider(self.internal_companion_tx.clone());
                    let id = self
                        .companion_ctx
                        .register_provider(self.internal_companion_tx.clone());
                    debug!(id, "Registered companion provider");
                    tokio::spawn(async move {});

                    self.write_packet(Packet::CompanionHello(companion::HelloResponse {
                        provider_id: id,
                    }))
                    .await?;

                    if let StateVariant::Active {
                        version:
                            SessionVersion::V4 {
                                companion_provider_id,
                            },
                    } = &mut self.state.variant
                    {
                        *companion_provider_id = Some(id);
                    }

                    // let tx = self.internal_companion_tx.clone();
                    // tokio::spawn(async move {
                    //     loop {
                    //         let res = rx.recv().await.unwrap();
                    //         tx.send(res).unwrap();
                    //     }
                    // });
                }
                Action::Companion(resp) => match resp {
                    CompanionResponse::ResourceInfo(resource_info) => {
                        if let Some(req) = self.companion_queue.remove(&resource_info.request_id) {
                            if let CompanionQueueItem::GetResourceInfo(feedback) = req {
                                feedback.send(resource_info);
                            }
                        } else {
                            error!(
                                request_id = resource_info.request_id,
                                "Could not find request from response request ID"
                            );
                        }
                    }
                    CompanionResponse::ResourceResponse(resource) => {
                        if let Some(req) = self.companion_queue.remove(&resource.request_id) {
                            if let CompanionQueueItem::GetResource(feedback) = req {
                                debug!("Sending resource for req_id={}", resource.request_id);
                                feedback.send(resource);
                                debug!("Sent OK");
                            }
                        } else {
                            error!(
                                request_id = resource.request_id,
                                "Could not find request from response request ID"
                            );
                        }
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

    #[cfg_attr(not(target_os = "android"), instrument(name = "session", skip_all, fields(id = self.id)))]
    pub async fn run(
        mut self,
        mut updates_rx: Receiver<Arc<ReceiverToSenderMessage>>,
        msg_tx: &MessageSender,
        mut comp_rx: CompanionMsgReceiver,
    ) -> anyhow::Result<()> {
        debug!("Session was started");

        let mut packet_reader = fcast_protocol::PacketReader::new(MAX_BODY_SIZE);
        let mut tick_interval = tokio::time::interval(Duration::from_secs(1));

        self.write_packet(Packet::Version(VersionMessage { version: 4 }))
            .await?;

        let mut read_buf = [0u8; 1024 * 8];
        'main_loop: loop {
            tokio::select! {
                comp = comp_rx.recv() => {
                    let Some(comp) = comp else {
                        continue;
                    };

                    let request_id = self.req_id_gen.next();
                    match comp {
                        CompanionMessage::GetResourceInfo { id, feedback } => {
                            let request = companion::ResourceInfoRequest { request_id, resource_id: id };
                            self.send_companion_msg(Opcode::ResourceInfo, &request.serialize()).await?;
                            self.companion_queue.insert(request_id, CompanionQueueItem::GetResourceInfo(feedback));
                        }
                        CompanionMessage::GetResource { id, read_head, feedback } => {
                            let request = companion::ResourceRequest { request_id, resource_id: id, read_head };
                            self.send_companion_msg(Opcode::Resource, &request.serialize()).await?;
                            self.companion_queue.insert(request_id, CompanionQueueItem::GetResource(feedback));
                        }
                    }
                }
                res = self.stream.read(&mut read_buf) => {
                    let Ok(n_read) = res else {
                        break;
                    };

                    if n_read == 0 {
                        break;
                    }

                    packet_reader.push_data(&read_buf[0..n_read]);

                    // let mut start = std::time::Instant::now();

                    while let Some(packet) = packet_reader.get_packet() {
                        // let end = start.elapsed();
                        // debug!("Read packet in {end:?}");
                        let (opcode, body) = match packet.len() {
                            0 => {
                                error!("Received empty packet");
                                continue;
                            }
                            1 => (packet[0], None),
                            _ => (packet[0], Some(&packet[1..])),
                        };

                        let opcode = Opcode::try_from(opcode)?;

                        let body = match body {
                            Some(body) => {
                                // if body.len() > MAX_BODY_SIZE {
                                //     bail!("Message exceeded maximum length ({} > {MAX_BODY_SIZE})", body.len())
                                // }
                                Some(body)
                            }
                            None => None
                        };

                        // if opcode != Opcode::Ping && opcode != Opcode::Pong && opcode != Opcode::Resource {
                        if opcode != Opcode::Ping && opcode != Opcode::Pong {
                            // debug!(?opcode, "Received packet opcode={opcode:?} body={:?}", body);
                            debug!(?opcode, "Received packet");
                        }

                        let res = self.state.advance(DriverEvent::Packet { opcode, body });
                        if self.handle_state_result(self.id, msg_tx, res).await? {
                            break 'main_loop;
                        }

                        // start = std::time::Instant::now();
                    }
                }
                res = updates_rx.recv() => {
                    let Ok(to_sender) = res else {
                        break;
                    };

                    let res = self.state.advance(DriverEvent::ToSender(to_sender));
                    if self.handle_state_result(self.id, msg_tx, res).await? {
                        break;
                    }
                }
                _ = tick_interval.tick() => {
                    let res = self.state.advance(DriverEvent::Tick);
                    if self.handle_state_result(self.id, msg_tx, res).await? {
                        break;
                    }
                }
            }
        }

        debug!(state = ?self.state.variant);

        if let StateVariant::Active {
            version:
                SessionVersion::V4 {
                    companion_provider_id: Some(id),
                },
        } = self.state.variant
        {
            self.companion_ctx.unregister_provider(id);
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
                Some(v2_json.as_bytes()),
                Action::None,
                SessionVersion::V2,
            ),
            (
                Opcode::Version,
                Some(v3_json.as_bytes()),
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
                    body: Some(b"{"),
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
                        body: Some(v2_json.as_bytes()),
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
