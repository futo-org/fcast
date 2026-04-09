use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    MessageSender, ReceiverInfo, SenderId, application::PacketOrigin,
    message::ReceiverToFCastSender,
};
use anyhow::{Context, bail};
use bitflags::bitflags;
use fcast_protocol::{
    Opcode, PlaybackErrorMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage,
    companion,
    receiver::NetworkStream,
    v1,
    v2::{self, PlayMessage, PlaybackUpdateMessage, VolumeUpdateMessage},
    v3::{self, InitialReceiverMessage, ReceiverCapabilities},
    v4,
};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::{
    net::TcpStream,
    sync::{
        broadcast::Receiver,
        mpsc::{UnboundedReceiver, UnboundedSender},
    },
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, instrument, trace, warn};

const TICKS_BEFORE_PING: u32 = 3;

const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(5);

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
    // CompanionHello(companion::HelloResponse),
    // CompanionHello {
    //     provider_id: u16,
    // },
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
            // Packet::CompanionHello(_) => Opcode::CompanionHello,
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
            // Packet::CompanionHello(hello_response) => hello_response.serialize().to_vec(),
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
    V4 {
        companion_provider_id: Option<u16>,
        mirroring_session_id: Option<u16>,
    },
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
    ProgressUpdated {
        pos: gst::ClockTime,
        dur: gst::ClockTime,
    },
    VolumeChanged(f32),
    PlaybackStateChanged(v4::PlaybackState),
    PlaybackRateChanged(f32),
    CompanionHello(u16),
    CompanionGetResourceInfo {
        request_id: u32,
        resource_id: u32,
    },
    Play {
        initiator_session_id: SenderId,
        serialized_msg: fcast_protocol::v4::ConstructedMessage<'static>,
    },
    RelayToOtherSenders {
        initiator_session_id: SenderId,
        serialized_msg: fcast_protocol::v4::ConstructedMessage<'static>,
    },
    TracksAvailable {
        serialized_msg: fcast_protocol::v4::ConstructedMessage<'static>,
    },
    TracksSelected(Vec<fcast_protocol::v4::ConstructedMessage<'static>>),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum ReceiverToSenderMessage {
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
    #[error("invalid flatbuffer: {0}")]
    InvalidFlatbuffer(#[from] v4::flatbuffers::InvalidFlatbuffer),
    #[error("invlaid union type")]
    InvalidUnionType,
}

enum DriverEvent<'a> {
    Tick,
    Packet {
        opcode: Opcode,
        body: Option<&'a [u8]>,
    },
    ToSender(Arc<ReceiverToSenderMessage>),
    InternalMirroringAnswer {
        sdp: String,
    },
}

#[derive(Debug)]
pub struct MirroringTx(pub tokio::sync::mpsc::UnboundedSender<InternalMessage>);

impl PartialEq for MirroringTx {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

use v4::flat::Load as FlatLoad;

self_cell::self_cell!(
    pub struct FlatLoadMessage {
        owner: Vec<u8>,
        #[covariant]
        dependent: FlatLoad,
    }

    impl {PartialEq}
);

impl std::fmt::Debug for FlatLoadMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatLoadMessage")
            .field("load", self.borrow_dependent())
            .finish()
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum WrappedPlayMessage {
    Legacy(v3::PlayMessage),
    V4(FlatLoadMessage),
    Chromecast(crate::gcast::ChromecastLoadItem),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum Operation {
    Pause,
    Resume,
    Stop,
    PlayNew(WrappedPlayMessage),
    Seek(gst::ClockTime),
    SetSpeed(f32),
    SetPlaylistItem(v3::SetPlaylistItemMessage),
    SetVolume(f32),
    StartMirroringSession {
        tx: MirroringTx,
        offer_rx: MirroringOfferRx,
    },
    SetPlaybackState(fcast_protocol::v4::fcast_flatbuffers::fcast::v4::PlaybackState),
    ChangeTrack {
        id: Option<u32>,
        typ: fcast_protocol::v4::flat::MediaTrackType,
    },
    SelectQueueItem(v4::QueuePosition),
    RemoveQueueItem(v4::QueuePosition),
    InsertQueueItem(QueueInsertCell),
    ResumeOrPause,
    SetProgressUpdateInterval(Duration),
}

fn round_progress_interval(micros: u64) -> Duration {
    const STEP_MICROS: u64 = 100_000;
    let steps = ((micros + STEP_MICROS / 2) / STEP_MICROS).max(1);
    Duration::from_micros(steps * STEP_MICROS)
}

use v4::flat::{CompanionResourceInfoResponse, QueueInsert as FlatQueueInsert};

self_cell::self_cell!(
    pub struct ResourceInfoResponseCell {
        owner: Vec<u8>,
        #[covariant]
        dependent: CompanionResourceInfoResponse,
    }

    impl {Debug, PartialEq}
);

self_cell::self_cell!(
    pub struct QueueInsertCell {
        owner: Vec<u8>,
        #[covariant]
        dependent: FlatQueueInsert,
    }

    impl {Debug, PartialEq}
);

#[derive(Debug, PartialEq)]
enum CompanionResponse {
    ResourceInfo(ResourceInfoResponseCell),
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
    UpgradeToTls,
    RespondCompanionHello,
    Companion(CompanionResponse),
    StartMirroringSession {
        session_id: u16,
    },
    GotOffer {
        sdp: String,
    },
    SendAnswer {
        session_id: u16,
        sdp: String,
    },
    InvalidOpcode(Opcode),
    Error {
        kind: v4::flat::ErrorKind,
    },
}

#[derive(Debug, PartialEq)]
enum StateVariant {
    WaitingForVersion,
    Active { version: SessionVersion },
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

pub enum FeedbackSender<T> {
    // TODO: is this the most efficient solution?
    Channel(tokio::sync::mpsc::UnboundedSender<T>),
}

impl<T> FeedbackSender<T> {
    fn send(&self, obj: T) {
        match self {
            FeedbackSender::Channel(sender) => {
                let _ = sender.send(obj);
            }
        }
    }
}

pub enum CompanionMessage {
    GetResourceInfo {
        id: companion::ResourceId,
        feedback: FeedbackSender<ResourceInfoResponseCell>,
    },
    GetResource {
        id: companion::ResourceId,
        read_head: Option<v4::flat::ResourceReadHead>,
        feedback: FeedbackSender<companion::ResourceResponse>,
    },
}

#[derive(Clone)]
pub struct CompanionProviderHandle {
    tx: CompanionMsgSender,
}

impl CompanionProviderHandle {
    pub fn get_resource_info(
        &self,
        resource_id: companion::ResourceId,
    ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<ResourceInfoResponseCell>> {
        let (feedback, rx) = tokio::sync::mpsc::unbounded_channel();
        self.tx.send(CompanionMessage::GetResourceInfo {
            id: resource_id,
            feedback: FeedbackSender::Channel(feedback),
        })?;
        Ok(rx)
    }

    pub fn get_resource(
        &self,
        resource_id: companion::ResourceId,
        read_head: Option<v4::flat::ResourceReadHead>,
    ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<companion::ResourceResponse>> {
        let (feedback, rx) = tokio::sync::mpsc::unbounded_channel();
        self.tx.send(CompanionMessage::GetResource {
            id: resource_id,
            read_head,
            feedback: FeedbackSender::Channel(feedback),
        })?;
        Ok(rx)
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
                    0 => return Err(StateError::IllegalVersion(msg.version)),
                    1 => SessionVersion::V1,
                    2 => SessionVersion::V2,
                    3 => SessionVersion::V3,
                    4 => {
                        self.variant = StateVariant::UninitV4;
                        return Ok(Action::UpgradeToTls);
                    }
                    sender_version => {
                        debug!(
                            sender_version,
                            "Sender is higher version than we implement, downgrading"
                        );
                        SessionVersion::V3
                    }
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
                Action::Op(Operation::PlayNew(WrappedPlayMessage::Legacy(msg)))
            }
            Opcode::Pause => Action::Op(Operation::Pause),
            Opcode::Resume => Action::Op(Operation::Resume),
            Opcode::Stop => Action::Op(Operation::Stop),
            Opcode::Seek => {
                let msg = option_err_json!(SeekMessage, option_err_body!(body));
                match gst::ClockTime::try_from_seconds_f64(msg.time) {
                    Ok(t) => Action::Op(Operation::Seek(t)),
                    Err(err) => {
                        error!(time = msg.time, ?err, "Got invalid time in seek message");
                        Action::None
                    }
                }
            }
            Opcode::SetVolume => {
                let msg = option_err_json!(SetVolumeMessage, option_err_body!(body));
                Action::Op(Operation::SetVolume(msg.volume as f32))
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
                Action::Op(Operation::SetSpeed(msg.speed as f32))
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

    fn handle_flat_packet_v4(&mut self, body: &[u8]) -> Result<Action, StateError> {
        macro_rules! union {
            ($res:expr) => {
                ($res).ok_or(StateError::InvalidUnionType)?
            };
        }

        macro_rules! get_queue_position {
            ($msg:expr) => {
                match $msg.position_type() {
                    v4::flat::QueuePosition::Back => v4::QueuePosition::Back,
                    v4::flat::QueuePosition::Front => v4::QueuePosition::Front,
                    v4::flat::QueuePosition::Index => {
                        let pos = union!($msg.position_as_index());
                        v4::QueuePosition::Index(pos.index())
                    }
                    _ => return Err(StateError::InvalidBody),
                }
            };
        }

        let packet = v4::flat::root_as_packet(body)?;
        let action = match packet.payload_type() {
            v4::flat::Message::ProgressChanged => {
                if let Some(pos) = union!(packet.payload_as_progress_changed()).position() {
                    // `gst::ClockTime::from_useconds` multiplies by 1000 and panics on overflow.
                    match pos.micros().checked_mul(1000) {
                        Some(nanos) => {
                            Action::Op(Operation::Seek(gst::ClockTime::from_nseconds(nanos)))
                        }
                        None => Action::Error {
                            kind: v4::flat::ErrorKind::MalformedBody,
                        },
                    }
                } else {
                    Action::Error {
                        kind: v4::flat::ErrorKind::MalformedBody,
                    }
                }
            }
            v4::flat::Message::VolumeChanged => Action::Op(Operation::SetVolume(
                union!(packet.payload_as_volume_changed()).volume(),
            )),
            v4::flat::Message::PlaybackStateChanged => Action::Op(Operation::SetPlaybackState(
                union!(packet.payload_as_playback_state_changed()).state(),
            )),
            v4::flat::Message::SpeedChanged => Action::Op(Operation::SetSpeed(
                union!(packet.payload_as_speed_changed()).speed(),
            )),
            v4::flat::Message::SenderIntroduction => {
                // TODO: do something with the payload
                debug!("Got inital sender message");
                Action::None
            }
            v4::flat::Message::Load => Action::Op(Operation::PlayNew(WrappedPlayMessage::V4(
                FlatLoadMessage::try_new(body.to_owned(), |buf| {
                    let packet = v4::flat::root_as_packet(&buf)?;
                    match packet.payload_as_load() {
                        Some(p) => Ok(p),
                        None => Err(StateError::InvalidUnionType),
                    }
                })?,
            ))),
            v4::flat::Message::StartMirroringSession => {
                let msg = union!(packet.payload_as_start_mirroring_session());
                let StateVariant::Active {
                    version:
                        SessionVersion::V4 {
                            mirroring_session_id,
                            ..
                        },
                } = &mut self.variant
                else {
                    return Err(StateError::IllegalOpcode(Opcode::Flatbuf));
                };
                *mirroring_session_id = Some(msg.session_id());
                Action::StartMirroringSession {
                    session_id: msg.session_id(),
                }
            }
            v4::flat::Message::MirroringSessionDescription => {
                let msg = union!(packet.payload_as_mirroring_session_description());
                if let StateVariant::Active {
                    version:
                        SessionVersion::V4 {
                            mirroring_session_id,
                            ..
                        },
                } = &self.variant
                {
                    if *mirroring_session_id == Some(msg.session_id()) {
                        Action::GotOffer {
                            sdp: msg.sdp().to_owned(),
                        }
                    } else {
                        Action::Error {
                            kind: v4::flat::ErrorKind::InvalidState,
                        }
                    }
                } else {
                    return Err(StateError::IllegalOpcode(Opcode::Flatbuf));
                }
            }
            v4::flat::Message::StopPlayback => Action::Op(Operation::Stop),
            v4::flat::Message::CompanionHelloRequest => Action::RespondCompanionHello,
            v4::flat::Message::CompanionResourceInfoResponse => {
                Action::Companion(CompanionResponse::ResourceInfo(
                    ResourceInfoResponseCell::try_new(body.to_owned(), |buf| {
                        let packet = v4::flat::root_as_packet(&buf)?;
                        match packet.payload_as_companion_resource_info_response() {
                            Some(p) => Ok(p),
                            None => Err(StateError::InvalidUnionType),
                        }
                    })?,
                ))
            }
            v4::flat::Message::ChangeTrack => {
                let msg = union!(packet.payload_as_change_track());
                let id = msg.id();
                let typ = msg.track_type();
                Action::Op(Operation::ChangeTrack { id, typ })
            }
            v4::flat::Message::QueueItemSelected => {
                let msg = union!(packet.payload_as_queue_item_selected());
                let position = get_queue_position!(msg);
                Action::Op(Operation::SelectQueueItem(position))
            }
            v4::flat::Message::QueueInsert => {
                // let msg = union!(packet.payload_as_queue_insert());
                // let position = get_queue_position!(msg);
                let insert = QueueInsertCell::try_new(body.to_owned(), |buf| {
                    let packet = v4::flat::root_as_packet(&buf)?;
                    match packet.payload_as_queue_insert() {
                        Some(p) => Ok(p),
                        None => Err(StateError::InvalidUnionType),
                    }
                })?;
                Action::Op(Operation::InsertQueueItem(insert))
            }
            v4::flat::Message::QueueRemove => {
                let msg = union!(packet.payload_as_queue_remove());
                let position = get_queue_position!(msg);
                Action::Op(Operation::RemoveQueueItem(position))
            }
            v4::flat::Message::SetProgressUpdateInterval => {
                match union!(packet.payload_as_set_progress_update_interval()).interval() {
                    Some(interval) => Action::Op(Operation::SetProgressUpdateInterval(
                        round_progress_interval(interval.micros()),
                    )),
                    None => Action::Error {
                        kind: v4::flat::ErrorKind::MalformedBody,
                    },
                }
            }
            _ => {
                warn!(payload_type = ?packet.payload_type(), "Received invalid payload type");
                Action::Error {
                    kind: v4::flat::ErrorKind::InvalidPayloadType,
                }
            }
        };

        Ok(action)
    }

    #[instrument(skip_all)]
    fn handle_packet_v4(
        &mut self,
        opcode: Opcode,
        body: Option<&[u8]>,
    ) -> Result<Action, StateError> {
        Ok(match opcode {
            Opcode::Flatbuf => self.handle_flat_packet_v4(body.ok_or(StateError::MissingBody)?)?,
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            Opcode::Resource => {
                let Some(body) = body else {
                    return Err(StateError::MissingBody);
                };
                let resp = companion::ResourceResponse::parse(body)
                    .map_err(|_| StateError::InvalidBody)?;
                Action::Companion(CompanionResponse::ResourceResponse(resp))
            }
            _ => Action::InvalidOpcode(opcode),
        })
    }

    pub fn tls_success(&mut self) {
        self.variant = StateVariant::Active {
            version: SessionVersion::V4 {
                companion_provider_id: None,
                mirroring_session_id: None,
            },
        };
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
                    StateVariant::UninitV4 => Action::InvalidOpcode(opcode),
                }
            }
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
                                    if self.key_name_events_up.contains(flag) {
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
            DriverEvent::InternalMirroringAnswer { sdp } => {
                if let StateVariant::Active {
                    version:
                        SessionVersion::V4 {
                            mirroring_session_id: Some(session_id),
                            ..
                        },
                } = &self.variant
                {
                    Action::SendAnswer {
                        sdp,
                        session_id: *session_id,
                    }
                } else {
                    Action::None
                }
            }
        })
    }
}

enum CompanionQueueItem {
    GetResourceInfo(FeedbackSender<ResourceInfoResponseCell>),
    GetResource(FeedbackSender<companion::ResourceResponse>),
}

pub enum InternalMessage {
    Answer { sdp: String },
}

pub struct MirroringOfferRx(
    pub std::sync::Arc<parking_lot::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>>,
);

impl Clone for MirroringOfferRx {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for MirroringOfferRx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MirroringOfferRx").finish()
    }
}

impl PartialEq for MirroringOfferRx {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

pub struct InitialV4State {
    pub play_data: Arc<WrappedPlayMessage>,
    pub playback_state: v4::PlaybackState,
}

pub struct SessionDriver {
    stream: NetworkStream,
    id: SenderId,
    state: State,
    tls_acceptor: TlsAcceptor,
    companion_ctx: CompanionContext,
    internal_companion_tx: CompanionMsgSender,
    companion_queue: HashMap<companion::ResourceId, CompanionQueueItem>,
    req_id_gen: companion::RequestIdGenerator,
    mirroring_offer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    receiver_info: Arc<ReceiverInfo>,
    // TODO: this should be updated in the time between new updates happen and when the state is sent out
    initial_v4_state: Option<InitialV4State>,
    pending_tls_upgrade: bool,
}

impl SessionDriver {
    pub fn new(
        stream: TcpStream,
        id: SenderId,
        tls_acceptor: TlsAcceptor,
        companion_ctx: CompanionContext,
        internal_companion_tx: CompanionMsgSender,
        receiver_info: Arc<ReceiverInfo>,
        initial_v4_state: Option<InitialV4State>,
    ) -> Self {
        Self {
            stream: NetworkStream::new(stream),
            id,
            state: State::new(),
            tls_acceptor,
            companion_ctx,
            internal_companion_tx,
            companion_queue: HashMap::new(),
            req_id_gen: companion::RequestIdGenerator::default(),
            mirroring_offer_tx: None,
            receiver_info,
            initial_v4_state,
            pending_tls_upgrade: false,
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
    async fn send_bin_msg(&mut self, opcode: Opcode, msg: &[u8]) -> anyhow::Result<()> {
        let header = Header::new(opcode, msg.len() as u32).encode();
        self.stream.write_all(&header).await?;
        self.stream.write_all(&msg).await?;
        self.stream.flush().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn send_v4_message(&mut self, msg: &V4Message) -> anyhow::Result<()> {
        let builder = v4::MessageBuilder::new();
        let msg = match msg {
            V4Message::ProgressUpdated { pos, dur } => builder.progress_changed(
                v4::flat::Time::new(pos.useconds()),
                v4::flat::Time::new(dur.useconds()),
            ),
            V4Message::VolumeChanged(vol) => builder.volume_changed(*vol),
            V4Message::PlaybackStateChanged(state) => builder.playback_state_changed(*state),
            V4Message::PlaybackRateChanged(rate) => builder.speed_changed(*rate),
            V4Message::CompanionHello(id) => builder.companion_hello_response(*id),
            V4Message::CompanionGetResourceInfo {
                request_id,
                resource_id,
            } => builder.companion_resource_info_request(*request_id, *resource_id),
            V4Message::Play {
                initiator_session_id,
                serialized_msg,
            } => {
                if *initiator_session_id != self.id {
                    return self.send_bin_msg(Opcode::Flatbuf, serialized_msg).await;
                } else {
                    return Ok(());
                }
            }
            V4Message::RelayToOtherSenders {
                initiator_session_id,
                serialized_msg,
            } => {
                if *initiator_session_id != self.id {
                    return self.send_bin_msg(Opcode::Flatbuf, serialized_msg).await;
                } else {
                    return Ok(());
                }
            }
            V4Message::TracksAvailable { serialized_msg } => {
                return self.send_bin_msg(Opcode::Flatbuf, serialized_msg).await;
            }
            V4Message::TracksSelected(messages) => {
                for msg in messages {
                    self.send_bin_msg(Opcode::Flatbuf, msg).await?;
                }

                return Ok(());
            }
        };
        self.send_bin_msg(Opcode::Flatbuf, &msg).await
    }

    async fn on_invalid_opcode(&mut self, opcode: u8, packet_num: u32) -> anyhow::Result<()> {
        warn!(opcode, packet_num, "Received invalid opcode");
        if let StateVariant::Active {
            version: SessionVersion::V4 { .. },
        } = &self.state.variant
        {
            let msg = v4::MessageBuilder::new()
                .error(Some(packet_num), v4::flat::ErrorKind::InvalidOpcode);
            return self.send_bin_msg(Opcode::Flatbuf, &msg).await;
        } else {
            // Terminate session on <V4
            bail!("Received invalid opcode ({opcode})");
        }
    }

    async fn finish_tls_upgrade(&mut self) -> anyhow::Result<()> {
        self.state.tls_success();

        let fmts = &self.receiver_info.supported_formats;
        let msg = v4::MessageBuilder::new().receiver_introduction(
            &self.receiver_info.device_info,
            fmts.protocols.iter().map(|p| p.to_str()),
            fmts.containers.iter().map(|c| c.to_str()),
            fmts.videos.iter().map(|v| v.to_str()),
            fmts.audios.iter().map(|a| a.to_str()),
            fmts.subtitles.iter().map(|s| s.to_str()),
            fmts.hdrs.iter().map(|s| s.to_str()),
            fmts.images.iter().map(|i| i.mime_type()),
            false,
            0.01,
        );

        self.send_bin_msg(Opcode::Flatbuf, &msg).await?;

        if let Some(initial) = self.initial_v4_state.take()
            && let WrappedPlayMessage::V4(play_msg) = initial.play_data.as_ref()
        {
            let load = play_msg.borrow_dependent();
            if let Some(load_msg) = v4::MessageBuilder::new().from_play_stripped(&load) {
                self.send_bin_msg(Opcode::Flatbuf, &load_msg).await?;
                let state_msg =
                    v4::MessageBuilder::new().playback_state_changed(initial.playback_state);
                self.send_bin_msg(Opcode::Flatbuf, &state_msg).await?;
            }
        }

        Ok(())
    }

    /// Returns true if the session should end.
    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    async fn handle_state_result(
        &mut self,
        origin: PacketOrigin,
        msg_tx: &MessageSender,
        res: Result<Action, StateError>,
        internal_msg_tx: &tokio::sync::mpsc::UnboundedSender<InternalMessage>,
    ) -> anyhow::Result<bool> {
        match res {
            Ok(action) => match action {
                Action::None => (),
                Action::Ping => self.write_simple(Opcode::Ping).await?,
                Action::Pong => self.write_simple(Opcode::Pong).await?,
                Action::EndSession => return Ok(true),
                Action::Op(operation) => {
                    msg_tx.operation(origin, operation);
                }
                Action::SendInitial => {
                    self.write_packet(Packet::Initial(v3::InitialReceiverMessage {
                        display_name: self.receiver_info.device_info.display_name.clone(),
                        app_name: self.receiver_info.device_info.app_name.clone(),
                        app_version: self.receiver_info.device_info.app_version.clone(),
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
                        self.send_msg(Opcode::PlaybackError, msg).await?;
                    }
                    ReceiverToSenderMessage::LegacyTranslatable { op, msg } => {
                        let Some(session_version) = session_version else {
                            // Unreachable
                            error!("Missing session version");
                            return Ok(false);
                        };
                        if let Some(body) = msg.translate_and_serialize(session_version) {
                            self.send_bin_msg(*op, &body).await?;
                        } else {
                            error!("Could not translate message");
                        }
                    }
                    ReceiverToSenderMessage::Event { msg } => {
                        self.send_msg(Opcode::Event, msg).await?;
                    }
                    ReceiverToSenderMessage::PlayUpdate { msg } => {
                        self.send_msg(Opcode::PlayUpdate, msg).await?;
                    }
                    ReceiverToSenderMessage::V4(msg) => {
                        self.send_v4_message(msg).await?;
                    }
                },
                Action::UpgradeToTls => {
                    self.pending_tls_upgrade = true;
                }
                Action::RespondCompanionHello => {
                    // let (id, mut rx) = self.companion_ctx.register_provider(self.internal_companion_tx.clone());
                    let id = self
                        .companion_ctx
                        .register_provider(self.internal_companion_tx.clone());
                    debug!(id, "Registered companion provider");
                    tokio::spawn(async move {});

                    self.send_v4_message(&V4Message::CompanionHello(id)).await?;
                    // self.write_packet(Packet::CompanionHello {
                    //     provider_id: id,
                    // })
                    // .await?;

                    if let StateVariant::Active {
                        version:
                            SessionVersion::V4 {
                                companion_provider_id,
                                ..
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
                        let request_id = resource_info.borrow_dependent().request_id();
                        if let Some(req) = self.companion_queue.remove(&request_id) {
                            if let CompanionQueueItem::GetResourceInfo(feedback) = req {
                                feedback.send(resource_info);
                            }
                        } else {
                            error!(
                                request_id = request_id,
                                "Could not find request from response request ID"
                            );
                        }
                    }
                    CompanionResponse::ResourceResponse(resource) => {
                        // if let Some(req) = self.companion_queue.remove(&resource.request_id) {
                        let request_id = resource.request_id;
                        if let Some(req) = self.companion_queue.get(&request_id) {
                            if let CompanionQueueItem::GetResource(feedback) = req {
                                let was_last_part =
                                    resource.part == resource.total_parts.saturating_sub(1);
                                feedback.send(resource);
                                if was_last_part {
                                    self.companion_queue.remove(&request_id);
                                }
                            }
                        } else {
                            error!(
                                request_id = resource.request_id,
                                "Could not find request from response request ID"
                            );
                        }
                    }
                },
                Action::StartMirroringSession { session_id: _ } => {
                    let (offer_tx, offer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    self.mirroring_offer_tx = Some(offer_tx);
                    msg_tx.operation(
                        origin,
                        Operation::StartMirroringSession {
                            tx: MirroringTx(internal_msg_tx.clone()),
                            offer_rx: MirroringOfferRx(std::sync::Arc::new(
                                parking_lot::Mutex::new(Some(offer_rx)),
                            )),
                        },
                    );
                }
                Action::GotOffer { sdp } => {
                    if let Some(tx) = self.mirroring_offer_tx.take() {
                        let _ = tx.send(sdp);
                    }
                }
                Action::SendAnswer { session_id, sdp } => {
                    debug!(session_id, "Sending answer to sender");
                    let msg =
                        v4::MessageBuilder::new().mirroring_session_description(session_id, &sdp);
                    self.send_bin_msg(Opcode::Flatbuf, &msg).await?;
                }
                Action::InvalidOpcode(opcode) => {
                    if let PacketOrigin::FCast {
                        packet_num: Some(packet_num),
                        ..
                    } = &origin
                    {
                        self.on_invalid_opcode(opcode as u8, *packet_num).await?;
                    }
                }
                Action::Error { kind } => {
                    if let PacketOrigin::FCast { packet_num, .. } = origin {
                        self.send_v4_error(packet_num, kind).await?;
                    }
                }
            },
            Err(err) => {
                error!(?err, "Error occured when advancing state");
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn send_v4_error(
        &mut self,
        packet_num: Option<u32>,
        kind: v4::flat::ErrorKind,
    ) -> anyhow::Result<()> {
        if let StateVariant::Active {
            version: SessionVersion::V4 { .. },
        } = &self.state.variant
        {
            let msg = v4::MessageBuilder::new().error(packet_num, kind);
            self.send_bin_msg(Opcode::Flatbuf, &msg).await?;
        }

        Ok(())
    }

    async fn handle_msg_from_receiver(&mut self, msg: ReceiverToFCastSender) -> anyhow::Result<()> {
        match msg {
            ReceiverToFCastSender::Error { kind, packet_num } => {
                self.send_v4_error(packet_num, kind).await?;
            }
            ReceiverToFCastSender::ProgressUpdate { pos, dur } => {
                if let StateVariant::Active {
                    version: SessionVersion::V4 { .. },
                } = &self.state.variant
                {
                    self.send_v4_message(&V4Message::ProgressUpdated { pos, dur })
                        .await?;
                }
            }
        }

        Ok(())
    }

    #[cfg_attr(not(target_os = "android"), instrument(name = "session", skip_all, fields(id = self.id)))]
    pub async fn run(
        mut self,
        mut updates_rx: Receiver<Arc<ReceiverToSenderMessage>>,
        msg_tx: &MessageSender,
        mut comp_rx: CompanionMsgReceiver,
        mut msg_rx: UnboundedReceiver<ReceiverToFCastSender>,
    ) -> anyhow::Result<()> {
        debug!("Session was started");

        let mut read_buf = [0u8; 1024 * 8];
        let mut packet_reader =
            fcast_protocol::PacketReader::new(v4::MAX_PACKET_SIZE, read_buf.len());
        let mut tick_interval = tokio::time::interval(Duration::from_secs(1));

        let (internal_tx, mut internal_rx) =
            tokio::sync::mpsc::unbounded_channel::<InternalMessage>();

        self.write_packet(Packet::Version(VersionMessage { version: 4 }))
            .await?;

        let mut packet_num = 0;
        'main_loop: loop {
            let origin = PacketOrigin::fcast(self.id, None);
            tokio::select! {
                msg = internal_rx.recv() => {
                    let Some(msg) = msg else {
                        break;
                    };

                    match msg {
                        InternalMessage::Answer { sdp } => {
                            let res = self.state.advance(DriverEvent::InternalMirroringAnswer { sdp });
                            self.handle_state_result(origin, msg_tx, res, &internal_tx).await?;
                        }
                    }
                }
                comp = comp_rx.recv() => {
                    let Some(comp) = comp else {
                        break;
                    };

                    let request_id = self.req_id_gen.next();
                    match comp {
                        CompanionMessage::GetResourceInfo { id, feedback } => {
                            self.send_v4_message(&V4Message::CompanionGetResourceInfo { request_id, resource_id: id }).await?;
                            self.companion_queue.insert(request_id, CompanionQueueItem::GetResourceInfo(feedback));
                        }
                        CompanionMessage::GetResource { id, read_head, feedback } => {
                            let builder = v4::MessageBuilder::new();
                            let msg = builder.companion_resource_request(request_id, id, read_head);
                            self.send_bin_msg(Opcode::Flatbuf, &msg).await?;
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

                    packet_reader.push_data(&read_buf[0..n_read]).map_err(|_| anyhow::anyhow!("Invalid packet (buffer overrun)"))?;

                    loop {
                        let packet = match packet_reader.get_packet() {
                            fcast_protocol::ReadResult::NeedData => break,
                            fcast_protocol::ReadResult::Read(packet) => packet,
                            fcast_protocol::ReadResult::PacketTooLarge(size) => bail!("Received too large packet size={size}"),
                        };

                        let (opcode, body) = match packet.len() {
                            0 => {
                                error!("Received empty packet");
                                continue;
                            }
                            1 => (packet[0], None),
                            _ => (packet[0], Some(&packet[1..])),
                        };

                        let Ok(opcode) = Opcode::try_from(opcode) else {
                            self.on_invalid_opcode(opcode, packet_num).await?;
                            continue;
                        };

                        trace!(?opcode, "Received packet");

                        let res = self.state.advance(DriverEvent::Packet { opcode, body });
                        if self.handle_state_result(
                            PacketOrigin::fcast(self.id, Some(packet_num)),
                            msg_tx,
                            res,
                            &internal_tx
                        ).await?
                        {
                            break 'main_loop;
                        }

                        packet_num += 1;

                        if self.pending_tls_upgrade {
                            self.pending_tls_upgrade = false;
                            // Any bytes read past the `Version` packet belong to the TLS handshake.
                            let prefix = packet_reader.drain_unparsed();
                            debug!("Upgrading network stream to use TLS");
                            self.stream
                                .upgrade_with_prefix(&self.tls_acceptor, &prefix, TLS_UPGRADE_TIMEOUT)
                                .await?;
                            debug!("Upgraded successfully");
                            self.finish_tls_upgrade().await?;
                            break;
                        }
                    }
                }
                res = updates_rx.recv() => {
                    let Ok(to_sender) = res else {
                        break;
                    };

                    let res = self.state.advance(DriverEvent::ToSender(to_sender));
                    if self.handle_state_result(origin, msg_tx, res, &internal_tx).await? {
                        break;
                    }
                }
                msg = msg_rx.recv() => {
                    let Some(msg) = msg else {
                        break;
                    };

                    self.handle_msg_from_receiver(msg).await?;
                }
                _ = tick_interval.tick() => {
                    let res = self.state.advance(DriverEvent::Tick);
                    if self.handle_state_result(origin, msg_tx, res, &internal_tx).await? {
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
                    ..
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

    #[test]
    fn progress_interval_rounding() {
        let ms = |m: u64| Duration::from_millis(m);
        // Sub-minimum requests clamp up to 100ms.
        assert_eq!(round_progress_interval(0), ms(100));
        assert_eq!(round_progress_interval(1), ms(100));
        assert_eq!(round_progress_interval(49_000), ms(100));
        // Rounds to the nearest 100ms.
        assert_eq!(round_progress_interval(50_000), ms(100));
        assert_eq!(round_progress_interval(99_000), ms(100));
        assert_eq!(round_progress_interval(100_000), ms(100));
        assert_eq!(round_progress_interval(120_000), ms(100));
        assert_eq!(round_progress_interval(149_000), ms(100));
        assert_eq!(round_progress_interval(150_000), ms(200));
        assert_eq!(round_progress_interval(180_000), ms(200));
        assert_eq!(round_progress_interval(249_000), ms(200));
        assert_eq!(round_progress_interval(250_000), ms(300));
        assert_eq!(round_progress_interval(500_000), ms(500));
        assert_eq!(round_progress_interval(549_000), ms(500));
        assert_eq!(round_progress_interval(550_000), ms(600));
    }

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

    fn v4_state() -> State {
        let mut state = State::new();
        state.tls_success();
        assert!(matches!(
            state.variant,
            StateVariant::Active {
                version: SessionVersion::V4 { .. }
            }
        ));
        state
    }

    fn advance_flatbuf(state: &mut State, body: &[u8]) -> Result<Action, StateError> {
        state.advance(DriverEvent::Packet {
            opcode: Opcode::Flatbuf,
            body: Some(body),
        })
    }

    #[test]
    fn v4_progress_changed_huge_micros_does_not_panic() {
        let mut state = v4_state();
        let msg = v4::MessageBuilder::new()
            .progress_changed_raw(Some(&v4::flat::Time::new(u64::MAX)), None);
        let res = advance_flatbuf(&mut state, &msg);
        assert_eq!(
            res,
            Ok(Action::Error {
                kind: v4::flat::ErrorKind::MalformedBody
            }),
            "expected a graceful error, not a panic"
        );
    }

    fn key_event(variant: v3::EventType, key: &str) -> Arc<ReceiverToSenderMessage> {
        Arc::new(ReceiverToSenderMessage::Event {
            msg: v3::EventMessage {
                generation_time: 0,
                event: v3::EventObject::Key {
                    variant,
                    key: key.to_owned(),
                    repeat: false,
                    handled: false,
                },
            },
        })
    }

    #[test]
    fn v3_keyup_subscription_forwards_keyup() {
        let mut state = State::new();
        state.variant = StateVariant::Active {
            version: SessionVersion::V3,
        };
        state.key_name_events_up = KeyEventFlags::Enter;

        let res = state.advance(DriverEvent::ToSender(key_event(
            v3::EventType::KeyUp,
            "Enter",
        )));

        assert!(
            matches!(res, Ok(Action::Forward { .. })),
            "KeyUp subscriber should receive KeyUp events, got {res:?}"
        );
    }

    #[test]
    fn v3_keydown_only_does_not_forward_keyup() {
        let mut state = State::new();
        state.variant = StateVariant::Active {
            version: SessionVersion::V3,
        };
        state.key_name_events_down = KeyEventFlags::Enter;

        let res = state.advance(DriverEvent::ToSender(key_event(
            v3::EventType::KeyUp,
            "Enter",
        )));

        assert_eq!(
            res,
            Ok(Action::None),
            "KeyDown-only subscriber should not receive KeyUp events"
        );
    }

    #[test]
    fn v4_progress_changed_normal_micros_seeks() {
        let mut state = v4_state();

        let micros = 5_000_000u64;
        let msg = v4::MessageBuilder::new()
            .progress_changed_raw(Some(&v4::flat::Time::new(micros)), None);

        let res = advance_flatbuf(&mut state, &msg);

        assert_eq!(
            res,
            Ok(Action::Op(Operation::Seek(gst::ClockTime::from_useconds(
                micros
            ))))
        );
    }
}
