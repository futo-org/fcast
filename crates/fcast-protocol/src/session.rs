//! Sans-io FCast receiver session state machine.
//!
//! The driver feeds [`Event`]s into [`State::advance`] and carries out the
//! returned [`Action`]. No io, no clock and no runtime live in here.

use std::{sync::Arc, time::Duration};

use bitflags::bitflags;
use bytes::Bytes;

use crate::{
    companion, v1,
    v2::{self, PlayMessage, PlaybackUpdateMessage, VolumeUpdateMessage},
    v3::{self, InitialReceiverMessage},
    v4, Opcode, PlaybackErrorMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage,
    VersionMessage,
};

pub use crate::v4::ResourceInfoResponseCell;

#[cfg(test)]
mod tests;

// Logs through tracing when the feature is on, nothing otherwise
macro_rules! log_at {
    ($lvl:ident, $($arg:tt)+) => {{
        #[cfg(feature = "tracing")]
        tracing::$lvl!($($arg)+);
    }};
}

/// Ticks without a packet before the receiver pings, twice that ends the
/// session.
pub const TICKS_BEFORE_PING: u32 = 3;

pub use crate::{Header, LEGACY_MAX_BODY};

/// Id of a sender session.
pub type SenderId = u32;

#[derive(Debug)]
pub enum PacketError {
    Json {
        opcode: Opcode,
        source: serde_json::Error,
    },
    UnsupportedOpcode(Opcode),
    /// Body length over [`LEGACY_MAX_BODY`].
    BodyTooLarge(usize),
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketError::Json { opcode, source } => write!(f, "{opcode:?}: {source}"),
            PacketError::UnsupportedOpcode(opcode) => write!(f, "Unsupported opcode: {opcode:?}"),
            PacketError::BodyTooLarge(len) => {
                write!(f, "{len} byte body over the v1-v3 packet limit")
            }
        }
    }
}

impl std::error::Error for PacketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PacketError::Json { source, .. } => Some(source),
            _ => None,
        }
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
    pub fn decode(header: Header, body: &str) -> Result<Self, PacketError> {
        let opcode = header.opcode;
        macro_rules! json {
            () => {
                serde_json::from_str(body).map_err(|source| PacketError::Json { opcode, source })?
            };
        }
        Ok(match opcode {
            Opcode::None => Self::None,
            Opcode::Play => Self::Play(json!()),
            Opcode::Pause => Self::Pause,
            Opcode::Resume => Self::Resume,
            Opcode::Stop => Self::Stop,
            Opcode::Seek => Self::Seek(json!()),
            Opcode::PlaybackUpdate => Self::PlaybackUpdate(json!()),
            Opcode::VolumeUpdate => Self::VolumeUpdate(json!()),
            Opcode::SetVolume => Self::SetVolume(json!()),
            Opcode::PlaybackError => Self::PlaybackError(json!()),
            Opcode::SetSpeed => Self::SetSpeed(json!()),
            Opcode::Version => Self::Version(json!()),
            Opcode::Ping => Self::Ping,
            Opcode::Pong => Self::Pong,
            _ => return Err(PacketError::UnsupportedOpcode(opcode)),
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, PacketError> {
        let opcode = Opcode::from(self);
        let ser = |res: serde_json::Result<Vec<u8>>| {
            res.map_err(|source| PacketError::Json { opcode, source })
        };
        let body = match self {
            Packet::Play(msg) => ser(serde_json::to_vec(msg))?,
            Packet::Seek(msg) => ser(serde_json::to_vec(msg))?,
            Packet::PlaybackUpdate(msg) => ser(serde_json::to_vec(msg))?,
            Packet::VolumeUpdate(msg) => ser(serde_json::to_vec(msg))?,
            Packet::SetVolume(msg) => ser(serde_json::to_vec(msg))?,
            Packet::PlaybackError(msg) => ser(serde_json::to_vec(msg))?,
            Packet::SetSpeed(msg) => ser(serde_json::to_vec(msg))?,
            Packet::Version(msg) => ser(serde_json::to_vec(msg))?,
            Packet::Initial(msg) => ser(serde_json::to_vec(msg))?,
            _ => Vec::new(),
        };

        if body.len() >= LEGACY_MAX_BODY {
            return Err(PacketError::BodyTooLarge(body.len()));
        }
        let header = Header::new(opcode, body.len() as u32).encode();
        let mut pack = header.to_vec();
        pack.extend_from_slice(&body);
        Ok(pack)
    }
}

/// Makes Initial fit the v3 limit. The play data is the first to give: a
/// fetched playlist or a manifest sent as content keeps only its url, and
/// without one the item is left out.
pub fn fit_initial(initial: &mut v3::InitialReceiverMessage) {
    let fits = |m: &v3::InitialReceiverMessage| {
        serde_json::to_vec(m).is_ok_and(|b| b.len() < LEGACY_MAX_BODY)
    };
    if fits(initial) {
        return;
    }
    initial.play_data = initial
        .play_data
        .take()
        .filter(|p| p.url.is_some())
        .map(|p| v3::PlayMessage { content: None, ..p });
    if !fits(initial) {
        initial.play_data = None;
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

/// Messages where higher versions can be translated to lower versions of the
/// same message
#[derive(Debug, PartialEq)]
pub enum TranslatableMessage {
    PlaybackUpdate(v3::PlaybackUpdateMessage),
    VolumeUpdate(v2::VolumeUpdateMessage),
}

impl TranslatableMessage {
    /// The body for a sender of `session_version`, `None` for v4 or when the
    /// message lacks a field the older version requires.
    pub fn translate_and_serialize(&self, session_version: SessionVersion) -> Option<Vec<u8>> {
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
        pos: Duration,
        dur: Duration,
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
        serialized_msg: v4::ConstructedMessage<'static>,
    },
    RelayToOtherSenders {
        initiator_session_id: SenderId,
        serialized_msg: v4::ConstructedMessage<'static>,
    },
    /// A receiver-initiated message for every v4 sender (no initiator to
    /// exclude).
    Broadcast {
        serialized_msg: v4::ConstructedMessage<'static>,
    },
    TracksAvailable {
        serialized_msg: v4::ConstructedMessage<'static>,
    },
    TracksSelected(Vec<v4::ConstructedMessage<'static>>),
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

#[derive(Debug, PartialEq)]
pub enum StateError {
    BodyIsNotUtf8,
    InvalidJson,
    IllegalVersion(u64),
    IllegalOpcode(Opcode),
    MissingBody,
    InvalidBody,
    InvalidFlatbuffer(v4::flatbuffers::InvalidFlatbuffer),
    InvalidUnionType,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::BodyIsNotUtf8 => write!(f, "body is not valid UTF-8"),
            StateError::InvalidJson => write!(f, "invalid json"),
            StateError::IllegalVersion(v) => write!(f, "illegal version: {v}"),
            StateError::IllegalOpcode(op) => write!(f, "illegal opcode: {op:?}"),
            StateError::MissingBody => write!(f, "missing body"),
            StateError::InvalidBody => write!(f, "invalid body"),
            StateError::InvalidFlatbuffer(err) => write!(f, "invalid flatbuffer: {err}"),
            StateError::InvalidUnionType => write!(f, "invalid union type"),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StateError::InvalidFlatbuffer(err) => Some(err),
            _ => None,
        }
    }
}

impl From<v4::flatbuffers::InvalidFlatbuffer> for StateError {
    fn from(err: v4::flatbuffers::InvalidFlatbuffer) -> Self {
        StateError::InvalidFlatbuffer(err)
    }
}

/// Input to [`State::advance`].
pub enum Event {
    /// One second passed.
    Tick,
    Packet {
        opcode: Opcode,
        /// The packet body, owned as a refcounted view of the read window.
        ///
        /// Owned rather than borrowed so `Opcode::Resource` can carry its
        /// payload out to the companion channel without copying it;
        /// every other opcode just derefs to `&[u8]`.
        body: Option<Bytes>,
    },
    ToSender(Arc<ReceiverToSenderMessage>),
    /// The local mirroring pipeline produced its SDP answer.
    InternalMirroringAnswer {
        sdp: String,
    },
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

use v4::flat::QueueInsert as FlatQueueInsert;

self_cell::self_cell!(
    pub struct QueueInsertCell {
        owner: Vec<u8>,
        #[covariant]
        dependent: FlatQueueInsert,
    }

    impl {Debug, PartialEq}
);

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum WrappedPlayMessage {
    Legacy(v3::PlayMessage),
    V4(FlatLoadMessage),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum Operation {
    Pause,
    Resume,
    Stop,
    PlayNew(WrappedPlayMessage),
    Seek(Duration),
    SetSpeed(f32),
    SetPlaylistItem(v3::SetPlaylistItemMessage),
    SetVolume(f32),
    SetPlaybackState(v4::PlaybackState),
    ChangeTrack {
        id: Option<u32>,
        typ: v4::flat::MediaTrackType,
    },
    AddSubtitleSource {
        url: String,
        select: bool,
        name: Option<smol_str::SmolStr>,
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

/// A v3 seek time as a duration, `None` when negative, non-finite or past
/// the nanosecond range of a u64 (`u64::MAX` itself included).
fn seek_duration(secs: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(secs)
        .ok()
        .filter(|d| d.as_nanos() < u64::MAX as u128)
}

#[derive(Debug, PartialEq)]
pub enum CompanionResponse {
    ResourceInfo(ResourceInfoResponseCell),
    ResourceResponse(companion::ResourceResponse),
}

/// What a sender said about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct SenderInfo {
    pub display_name: String,
    pub app_name: String,
    pub app_version: String,
    /// The protocol the session spoke when it introduced itself.
    pub version: SessionVersion,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    /// The sender introduced itself.
    SenderIntroduced(SenderInfo),
    Ping,
    Pong,
    EndSession,
    Op(Operation),
    SendInitial,
    /// Seed a newly active session with the receiver's current volume.
    SendVolume,
    Forward {
        session_version: Option<SessionVersion>,
        msg: Arc<ReceiverToSenderMessage>,
    },
    /// Upgrade the stream to TLS, then call [`State::tls_success`].
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
    fn from_key(name: &str) -> Self {
        match name {
            "ArrowLeft" => Self::ArrowLeft,
            "ArrowRight" => Self::ArrowRight,
            "ArrowUp" => Self::ArrowUp,
            "ArrowDown" => Self::ArrowDown,
            "Enter" => Self::Enter,
            _ => Self::empty(),
        }
    }

    fn from_keys(names: &[String]) -> Self {
        let mut flags = Self::empty();
        for name in names {
            flags.insert(Self::from_key(name));
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
        match serde_json::from_str::<$t>($body) {
            Ok(obj) => obj,
            Err(_err) => {
                log_at!(error, err = ?_err, "Failed to decode json object");
                return Some(Err(StateError::InvalidJson));
            }
        }
    };
}

macro_rules! utf8_body {
    ($bytes:expr) => {
        match $bytes {
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(_) => return Err(StateError::BodyIsNotUtf8),
            },
            None => None,
        }
    };
}

/// One sender session.
#[derive(Debug)]
pub struct State {
    time: u32,
    last_packet_received: u32,
    waiting_for_pong: bool,
    variant: StateVariant,
    media_item_events: MediaItemEventFlags,
    key_name_events_down: KeyEventFlags,
    key_name_events_up: KeyEventFlags,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
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

    /// The negotiated version, `None` before the session is active.
    pub fn version(&self) -> Option<SessionVersion> {
        self.variant.version()
    }

    /// The companion provider registered for a v4 session.
    pub fn companion_provider_id(&self) -> Option<u16> {
        match self.variant {
            StateVariant::Active {
                version:
                    SessionVersion::V4 {
                        companion_provider_id,
                        ..
                    },
            } => companion_provider_id,
            _ => None,
        }
    }

    /// Records the provider registered for this session, a no-op below v4.
    pub fn set_companion_provider_id(&mut self, id: u16) {
        if let StateVariant::Active {
            version:
                SessionVersion::V4 {
                    companion_provider_id,
                    ..
                },
        } = &mut self.variant
        {
            *companion_provider_id = Some(id);
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
                    sender_version @ 4.. => {
                        if sender_version > 4 {
                            log_at!(
                                debug,
                                sender_version,
                                "Sender is higher version than we implement, using v4"
                            );
                        }
                        self.variant = StateVariant::UninitV4;
                        return Ok(Action::UpgradeToTls);
                    }
                };
                self.variant = StateVariant::Active { version };
                if version == SessionVersion::V3 {
                    Action::SendInitial
                } else {
                    // v1/v2 have no Initial message, so seed the volume (it only
                    // broadcasts on change).
                    Action::SendVolume
                }
            }
            // TODO: technically v2 doesn't need to accept VersionMessage before starting the
            // session
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    /// Handle those packets that are common for v{1, 2, 3}
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
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
                match seek_duration(msg.time) {
                    Some(t) => Action::Op(Operation::Seek(t)),
                    None => {
                        log_at!(error, time = msg.time, "Got invalid time in seek message");
                        Action::None
                    }
                }
            }
            Opcode::SetVolume => {
                let msg = option_err_json!(SetVolumeMessage, option_err_body!(body));
                Action::Op(Operation::SetVolume(msg.volume as f32))
            }
            Opcode::PlaybackUpdate
            | Opcode::VolumeUpdate
            | Opcode::PlayUpdate
            | Opcode::PlaybackError => Action::None,
            _ => return None,
        }))
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
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

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
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

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
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
                let msg = err_json!(v3::InitialSenderMessage, err_body!(body));
                Action::SenderIntroduced(SenderInfo {
                    display_name: msg.display_name.unwrap_or_default(),
                    app_name: msg.app_name.unwrap_or_default(),
                    app_version: msg.app_version.unwrap_or_default(),
                    version: SessionVersion::V3,
                })
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
                        .insert(KeyEventFlags::from_keys(&keys)),
                    v3::EventSubscribeObject::KeyUp { keys } => self
                        .key_name_events_up
                        .insert(KeyEventFlags::from_keys(&keys)),
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
                        .remove(KeyEventFlags::from_keys(&keys)),
                    v3::EventSubscribeObject::KeyUp { keys } => self
                        .key_name_events_up
                        .remove(KeyEventFlags::from_keys(&keys)),
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
                    // The position must fit u64 nanoseconds, as the pipeline clock does.
                    match pos.micros().checked_mul(1000) {
                        Some(_) => Action::Op(Operation::Seek(Duration::from_micros(pos.micros()))),
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
                let info = union!(packet.payload_as_sender_introduction()).device_info();
                Action::SenderIntroduced(SenderInfo {
                    display_name: info.display_name().unwrap_or_default().to_owned(),
                    app_name: info.app_name().unwrap_or_default().to_owned(),
                    app_version: info.app_version().unwrap_or_default().to_owned(),
                    version: self.variant.version().unwrap_or(SessionVersion::V4 {
                        companion_provider_id: None,
                        mirroring_session_id: None,
                    }),
                })
            }
            v4::flat::Message::Load => Action::Op(Operation::PlayNew(WrappedPlayMessage::V4(
                FlatLoadMessage::try_new(body.to_owned(), |buf| {
                    let packet = v4::flat::root_as_packet(buf)?;
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
                        let packet = v4::flat::root_as_packet(buf)?;
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
            v4::flat::Message::AddSubtitleSource => {
                let msg = union!(packet.payload_as_add_subtitle_source());
                let url = msg.url();
                if url.is_empty() {
                    // The spec gives an empty URL no meaning.
                    Action::Error {
                        kind: v4::flat::ErrorKind::MalformedBody,
                    }
                } else {
                    Action::Op(Operation::AddSubtitleSource {
                        url: url.to_owned(),
                        select: msg.select(),
                        name: msg.name().map(smol_str::SmolStr::new),
                    })
                }
            }
            v4::flat::Message::QueueItemSelected => {
                let msg = union!(packet.payload_as_queue_item_selected());
                let position = get_queue_position!(msg);
                Action::Op(Operation::SelectQueueItem(position))
            }
            v4::flat::Message::QueueInsert => {
                let insert = QueueInsertCell::try_new(body.to_owned(), |buf| {
                    let packet = v4::flat::root_as_packet(buf)?;
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
                log_at!(warn, payload_type = ?packet.payload_type(), "Received invalid payload type");
                Action::Error {
                    kind: v4::flat::ErrorKind::InvalidPayloadType,
                }
            }
        };

        Ok(action)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn handle_packet_v4(
        &mut self,
        opcode: Opcode,
        body: Option<Bytes>,
    ) -> Result<Action, StateError> {
        Ok(match opcode {
            Opcode::Flatbuf => self.handle_flat_packet_v4(&body.ok_or(StateError::MissingBody)?)?,
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

    /// The TLS upgrade asked for by [`Action::UpgradeToTls`] finished, the
    /// session is now v4.
    pub fn tls_success(&mut self) {
        self.variant = StateVariant::Active {
            version: SessionVersion::V4 {
                companion_provider_id: None,
                mirroring_session_id: None,
            },
        };
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn advance(&mut self, event: Event) -> Result<Action, StateError> {
        Ok(match event {
            Event::Tick => {
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
            Event::Packet { opcode, body } => {
                self.last_packet_received = self.time;
                self.waiting_for_pong = false;

                match &self.variant {
                    StateVariant::WaitingForVersion => {
                        return self.handle_packet_uninit(opcode, utf8_body!(body.as_deref()));
                    }
                    StateVariant::Active { version } => {
                        return match version {
                            SessionVersion::V1 => {
                                self.handle_packet_v1(opcode, utf8_body!(body.as_deref()))
                            }
                            SessionVersion::V2 => {
                                self.handle_packet_v2(opcode, utf8_body!(body.as_deref()))
                            }
                            SessionVersion::V3 => {
                                self.handle_packet_v3(opcode, utf8_body!(body.as_deref()))
                            }
                            SessionVersion::V4 { .. } => self.handle_packet_v4(opcode, body),
                        };
                    }
                    StateVariant::UninitV4 => Action::InvalidOpcode(opcode),
                }
            }
            Event::ToSender(msg) => match msg.as_ref() {
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
                ReceiverToSenderMessage::Event { msg: event_msg } => match &event_msg.event {
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
                        let flag = KeyEventFlags::from_key(key);
                        if flag.is_empty() {
                            return Ok(Action::None);
                        }
                        let session_version = self.variant.version();
                        let subscribed = match &variant {
                            v3::EventType::KeyDown => self.key_name_events_down.contains(flag),
                            v3::EventType::KeyUp => self.key_name_events_up.contains(flag),
                            _ => false, // Unreachable
                        };
                        if subscribed {
                            Action::Forward {
                                session_version,
                                msg,
                            }
                        } else {
                            Action::None
                        }
                    }
                },
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
            Event::InternalMirroringAnswer { sdp } => {
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

/// What a v4 sender connecting mid-cast is sent to catch up, as the
/// receiver stood at accept time.
pub struct InitialV4State {
    /// The current item or queue as a `Load`, request headers left out.
    pub load: v4::ConstructedMessage<'static>,
    pub playback_state: v4::PlaybackState,
    /// Position and duration. Progress only ticks while playing, so a
    /// paused item would never say where it is.
    pub progress: Option<(Duration, Duration)>,
    pub rate: f32,
    /// `TracksAvailable` and the selection per kind, broadcasts the
    /// newcomer was not there for.
    pub tracks: Vec<v4::ConstructedMessage<'static>>,
}

/// What a v3 sender gets in `Initial`: the item a legacy sender cast, with
/// the current time, volume and speed, then the playback state.
pub struct InitialLegacyState {
    pub play_data: v3::PlayMessage,
    pub playback: v3::PlaybackUpdateMessage,
}

/// The receiver as it stood at accept time, sent once the session activates.
// TODO: not refreshed between accept and handshake, so an update in that window
// is missed
pub struct SessionSeed {
    pub volume: f32,
    pub display_name: Option<String>,
    pub v4: Option<InitialV4State>,
    pub legacy: Option<InitialLegacyState>,
}

/// Playback rates the pipeline is driven at. A negative rate would play
/// backwards and a huge one overflows the tempo element.
pub const MIN_RATE: f32 = 1.0 / 16.0;
pub const MAX_RATE: f32 = 16.0;

/// The rate to apply and whether the requested one was in range. Zero,
/// negative and non-finite fall back to normal speed, the rest is clamped.
pub fn sanitize_rate(speed: f32) -> (f32, bool) {
    if !speed.is_finite() || speed <= 0.0 {
        (1.0, false)
    } else {
        let rate = speed.clamp(MIN_RATE, MAX_RATE);
        (rate, rate == speed)
    }
}

/// Whether an action draws on the transport budget (true) or the general
/// one (false), `None` for the actions that queue no work on the application.
/// Transport has its own so a flood of other ops cannot starve a stop.
pub fn budgeted_as_transport(action: &Action) -> Option<bool> {
    match action {
        Action::Op(op) => Some(matches!(
            op,
            Operation::Stop
                | Operation::Pause
                | Operation::Resume
                | Operation::ResumeOrPause
                | Operation::SetPlaybackState(_)
        )),
        // a full load on the application, as a Play is
        Action::StartMirroringSession { .. } => Some(false),
        _ => None,
    }
}

/// Token bucket over a session's operations. Slider drags send tens a
/// second, a flood sends them as fast as TCP delivers and would queue
/// without bound on the way to the app.
#[derive(Debug, Clone, Copy)]
pub struct OpBudget {
    tokens: f32,
    last: std::time::Instant,
    /// Ops dropped since the last warning.
    pub dropped: u32,
}

impl OpBudget {
    pub const RATE: f32 = 100.0;
    pub const BURST: f32 = 200.0;

    pub fn new(now: std::time::Instant) -> Self {
        Self {
            tokens: Self::BURST,
            last: now,
            dropped: 0,
        }
    }

    /// Takes one token, false when the bucket is dry.
    pub fn take(&mut self, now: std::time::Instant) -> bool {
        let refill = now.saturating_duration_since(self.last).as_secs_f32() * Self::RATE;
        self.tokens = (self.tokens + refill).min(Self::BURST);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Whether `op` names a companion resource of a provider other than `own`,
/// the session's. Providers are per sender, one must not serve another's.
pub fn names_foreign_companion(op: &Operation, own: Option<u16>) -> bool {
    let foreign = |url: &str| {
        url::Url::parse(url)
            .ok()
            .and_then(|u| companion::FCompUrl::new(&u))
            .is_some_and(|c| Some(c.provider_id) != own)
    };
    let item =
        |m: v4::flat::MediaItem| foreign(m.source_url()) || m.thumbnail_url().is_some_and(foreign);
    match op {
        Operation::PlayNew(WrappedPlayMessage::Legacy(msg)) => {
            let thumb = match &msg.metadata {
                Some(v3::MetadataObject::Generic { thumbnail_url, .. }) => thumbnail_url.as_deref(),
                _ => None,
            };
            msg.url.as_deref().is_some_and(foreign) || thumb.is_some_and(foreign)
        }
        Operation::PlayNew(WrappedPlayMessage::V4(load)) => {
            let play = load.borrow_dependent();
            match play.source_type() {
                v4::flat::MediaSource::Single => play.source_as_single().is_some_and(item),
                v4::flat::MediaSource::Queue => play
                    .source_as_queue()
                    .is_some_and(|q| q.items().iter().any(|i| item(i.media_item()))),
                _ => false,
            }
        }
        Operation::InsertQueueItem(insert) => item(insert.borrow_dependent().item().media_item()),
        Operation::AddSubtitleSource { url, .. } => foreign(url),
        _ => false,
    }
}
