use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io::{Read, Seek},
    net::SocketAddr,
    ops::RangeInclusive,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use fcast_protocol::{
    companion,
    sender::{CertVerifier, NetworkStream},
    v2,
    v3::{
        self, AVCapabilities, InitialReceiverMessage, LivestreamCapabilities, MetadataObject,
        ReceiverCapabilities, SetPlaylistItemMessage,
    },
    v4, Opcode, PlaybackErrorMessage, PlaybackState as FCastPlaybackState, SeekMessage,
    SetSpeedMessage, SetVolumeMessage, VersionMessage,
};
use futures::{
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
    StreamExt,
};
use log::{debug, error, warn};
use serde::Serialize;
use tokio::{
    runtime::Handle,
    sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use tokio_rustls::{rustls, TlsConnector};

use crate::{
    device::{
        ApplicationInfo, CastingDevice, CastingDeviceError, CompanionSource,
        CompanionSourceDescriptor, DeviceConnectionState, DeviceEventHandler, DeviceFeature,
        DeviceInfo, LoadRequest, MediaItem, MediaLocator, MediaTrack, MediaTrackType, Metadata,
        PlaybackState, PlaylistItem, ProtocolType, Queue, QueueEntry, QueueItem, QueuePosition,
        QueueState, ReceiverError, Source, SubtitleContent, SubtitleSource, TrackList,
    },
    utils, IpAddr,
};

const DEFAULT_SESSION_VERSION: u64 = 2;
const PLAYLIST_MIN_PROTO_VERSION: u64 = 3;
const V3_FEATURES_MIN_PROTO_VERSION: u64 = 3;

const CONNECTED_EVENT_DEADLINE_DURATION: Duration = Duration::from_secs(2);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(5);
const COMPANION_CALLBACK_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_COMPANION_CALLBACKS: usize = 8;

pub type CompanionResourceFuture<'a, T> =
    Pin<Box<dyn Future<Output = std::io::Result<T>> + Send + 'a>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompanionResourceRoute(String);

impl CompanionResourceRoute {
    pub fn new(route: impl Into<String>) -> Result<Self, companion::RouteError> {
        let route = route.into();
        companion::validate_route(&route)?;
        Ok(Self(route))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionResourceRequest {
    pub route: CompanionResourceRoute,
    pub range: Option<RangeInclusive<u64>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionResourceInfo {
    pub content_type: String,
    pub size: Option<u64>,
}

pub trait CompanionResource: std::fmt::Debug + Send + Sync + 'static {
    fn info(
        &self,
        route: CompanionResourceRoute,
    ) -> CompanionResourceFuture<'_, CompanionResourceInfo>;

    fn read(&self, request: CompanionResourceRequest) -> CompanionResourceFuture<'_, Vec<u8>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompanionResourceRegistrationError {
    Unsupported,
    Disconnected,
    Exhausted,
}

impl std::fmt::Display for CompanionResourceRegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                write!(f, "receiver does not support dynamic FCompanion resources")
            }
            Self::Disconnected => write!(f, "FCast worker is disconnected"),
            Self::Exhausted => write!(f, "FCompanion resource IDs are exhausted"),
        }
    }
}

impl std::error::Error for CompanionResourceRegistrationError {}

#[derive(Clone)]
pub struct CompanionResourceRegistrar {
    state: Weak<Mutex<State>>,
}

impl std::fmt::Debug for CompanionResourceRegistrar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanionResourceRegistrar")
            .finish_non_exhaustive()
    }
}

#[must_use = "dropping the registration unregisters its companion resource"]
#[derive(Debug)]
pub struct CompanionResourceRegistration {
    command_tx: UnboundedSender<Command>,
    generation: u64,
    provider_id: u16,
    resource_id: u32,
    url: String,
}

impl CompanionResourceRegistration {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn url_for(&self, route: &str) -> Result<String, companion::RouteError> {
        companion::create_routed_url(self.provider_id, self.resource_id, route)
    }
}

impl Drop for CompanionResourceRegistration {
    fn drop(&mut self) {
        let _ = self.command_tx.send(Command::UnregisterCompanionResource {
            generation: self.generation,
            resource_id: self.resource_id,
        });
    }
}

impl CompanionResourceRegistrar {
    pub async fn register(
        &self,
        resource: Arc<dyn CompanionResource>,
    ) -> Result<CompanionResourceRegistration, CompanionResourceRegistrationError> {
        let (reply, result) = oneshot::channel();
        let command = Command::RegisterCompanionResource { resource, reply };
        {
            let state = self
                .state
                .upgrade()
                .ok_or(CompanionResourceRegistrationError::Disconnected)?;
            let mut state = state.lock().unwrap();
            match state.command_tx.as_ref() {
                Some(tx) => {
                    tx.send(command)
                        .map_err(|_| CompanionResourceRegistrationError::Disconnected)?;
                }
                None if !state.ever_started => state.pending_commands.push_back(command),
                None => return Err(CompanionResourceRegistrationError::Disconnected),
            }
        }
        result
            .await
            .unwrap_or(Err(CompanionResourceRegistrationError::Disconnected))
    }
}

// #[derive(Debug, Clone, PartialEq)]
#[derive(Debug, PartialEq)]
enum LoadType {
    Url { url: String },
    Content { content: String },
    // CompanionResource { id: u32 },
    CompanionResource { source: CompanionSource },
}

#[derive(Debug)]
struct WrappedSignaller(Arc<dyn crate::device::FWRTCSignaller>);

impl PartialEq for WrappedSignaller {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}

#[derive(Debug)]
enum Command {
    ChangeVolume(f64),
    ChangeSpeed(f64),
    Load {
        type_: LoadType,
        content_type: String,
        // TODO: should be optional
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    SetProgressUpdateInterval(u64),
    SeekVideo(f64),
    StopVideo,
    PauseVideo,
    ResumeVideo,
    Quit,
    SetPlaylistItemIndex(u32),
    JumpPlaylist(i32),
    LoadPlaylist(Vec<PlaylistItem>),
    LoadQueue(Queue),
    AddSubtitleSource {
        source: SubtitleCommandSource,
        select: bool,
        name: Option<String>,
    },
    ConnectedEventDeadlineElapsed,
    StartMirroringSession(WrappedSignaller),
    MirroringOffer {
        session_id: u16,
        sdp: String,
    },
    ChangeTrack {
        id: Option<u32>,
        track_type: crate::device::MediaTrackType,
    },
    QueueRemove {
        position: QueuePosition,
    },
    QueueInsert {
        item: MediaItem,
        playback_duration: Option<f64>,
        position: QueuePosition,
    },
    QueueSelect {
        position: QueuePosition,
    },
    RegisterCompanionResource {
        resource: Arc<dyn CompanionResource>,
        reply: oneshot::Sender<
            Result<CompanionResourceRegistration, CompanionResourceRegistrationError>,
        >,
    },
    UnregisterCompanionResource {
        generation: u64,
        resource_id: u32,
    },
}

struct State {
    rt_handle: Handle,
    started: bool,
    command_tx: Option<UnboundedSender<Command>>,
    pending_commands: VecDeque<Command>,
    ever_started: bool,
    worker_id: u64,
    addresses: Vec<IpAddr>,
    name: String,
    port: u16,
    txt_records: HashMap<String, String>,
}

impl State {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            rt_handle,
            started: false,
            command_tx: None,
            pending_commands: VecDeque::new(),
            ever_started: false,
            worker_id: 0,
            addresses: device_info.addresses,
            name: device_info.name,
            port: device_info.port,
            txt_records: device_info.txt_records,
        }
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct FCastDevice {
    state: Arc<Mutex<State>>,
    session_version: FCastVersion,
    supports_whep: Arc<AtomicBool>,
}

impl FCastDevice {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::new(device_info, rt_handle))),
            session_version: FCastVersion::new(),
            supports_whep: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn companion_resource_registrar(&self) -> CompanionResourceRegistrar {
        CompanionResourceRegistrar {
            state: Arc::downgrade(&self.state),
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

#[derive(Debug, PartialEq, Eq)]
struct IdGenerator(u16);

impl IdGenerator {
    fn new() -> Self {
        Self(0)
    }

    fn next(&mut self) -> u16 {
        self.0 += 1;
        self.0 - 1
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StateVariant {
    Connecting,
    V2,
    V3,
    V4 {
        companion_provider: Option<(u16, u16)>,
        mirroring_session: Option<u16>,
        mirroring_session_id_gen: IdGenerator,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum QuitReason {
    InvalidBody,
    InvalidVersion,
    MissingBody,
    UnsupportedOpcode,
    InvalidUnionValue,
    InvalidPacket,
    InsecureDowngrade,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum VersionCode {
    V2,
    V3,
}

#[derive(Debug, PartialEq)]
enum CompanionRequest {
    ResourceInfo {
        request_id: u32,
        resource_id: u32,
        route: String,
    },
    Resource {
        request_id: u32,
        resource_id: u32,
        read_head: Option<(/* start */ u64, /* stop_inclusive */ u64)>,
        route: String,
    },
}

#[derive(Debug, PartialEq)]
enum V4Load {
    Single(Source),
    Queue {
        entries: Vec<QueueEntry>,
        start_index: Option<u8>,
        autoplay: bool,
    },
}

#[derive(Debug, PartialEq)]
enum Action {
    None,
    Pong,
    Quit(QuitReason),
    Connected(VersionCode),
    VolumeUpdated(v2::VolumeUpdateMessage),
    PlaybackError(PlaybackErrorMessage),
    PlaybackUpdateV2(v2::PlaybackUpdateMessage),
    PlaybackUpdateV3(v3::PlaybackUpdateMessage),
    Initial(v3::InitialReceiverMessage),
    PlayUpdate(v3::PlayUpdateMessage),
    Event(v3::EventMessage),
    UpgradeToTls,
    ProgressChanged {
        pos: f64,
        dur: f64,
    },
    VolumeChanged(f64),
    PlaybackStateChanged(v4::fcast_flatbuffers::fcast::v4::PlaybackState),
    PlaybackStopped,
    Companion(CompanionRequest),
    CompanionHello {
        provider_id: u16,
        protocol_version: u16,
    },
    StartMirroringSession {
        id: u16,
        signaller: WrappedSignaller,
    },
    HandleMirroringAnswer {
        session_id: u16,
        sdp: String,
    },
    TracksAvailable(Vec<crate::device::MediaTrack>),
    ChangeTrack {
        id: Option<u32>,
        typ: crate::device::MediaTrackType,
    },
    PlaybackRateChanged(f32),
    Introduction {
        supports_whep: bool,
        capabilities: Option<crate::device::ReceiverCapabilities>,
    },
    /// The receiver assigned the companion provider ID.
    CompanionReady,
    LoadedV4(V4Load),
    QueueInserted {
        entry: QueueEntry,
        position: QueuePosition,
    },
    QueueRemoved {
        position: QueuePosition,
    },
    QueueItemSelected {
        position: QueuePosition,
    },
    ReceiverError(ReceiverError),
}

/// Convert the v4 `ReceiverCapabilities` flatbuffer into the public
/// [`crate::device::ReceiverCapabilities`] struct forwarded to callers.
fn map_receiver_capabilities(
    caps: v4::flat::ReceiverCapabilities<'_>,
) -> crate::device::ReceiverCapabilities {
    fn strings<'a>(it: Option<impl Iterator<Item = &'a str>>) -> Vec<String> {
        it.map(|i| i.map(|s| s.to_owned()).collect())
            .unwrap_or_default()
    }

    crate::device::ReceiverCapabilities {
        media: caps.media().map(|m| crate::device::MediaCapabilities {
            protocols: strings(m.protocols().map(|v| v.iter())),
            containers: strings(m.containers().map(|v| v.iter())),
            video_formats: strings(m.video_formats().map(|v| v.iter())),
            audio_formats: strings(m.audio_formats().map(|v| v.iter())),
            subtitle_formats: strings(m.subtitle_formats().map(|v| v.iter())),
            hdr_formats: strings(m.hdr_formats().map(|v| v.iter())),
            image_formats: strings(m.image_formats().map(|v| v.iter())),
            external_subtitles: m.external_subtitles(),
            mirroring: m.mirroring(),
        }),
        display: caps.display().map(|d| crate::device::DisplayCapabilities {
            resolution: d.resolution().map(|r| crate::device::VideoResolution {
                width: r.width(),
                height: r.height(),
            }),
        }),
        audio: caps.audio().map(|a| crate::device::AudioCapabilities {
            volume_step_interval: a.volume_step_interval(),
        }),
    }
}

#[derive(Default)]
struct SharedState {
    pub time: f64,
    pub duration: f64,
    pub volume: f64,
    pub speed: f64,
    pub playback_state: PlaybackState,
    pub source: Option<Source>,
}

macro_rules! body {
    ($maybe_body:expr) => {
        match $maybe_body {
            Some(b) => b,
            None => return Action::Quit(QuitReason::MissingBody),
        }
    };
    (return_option, $maybe_body:expr) => {
        match $maybe_body {
            Some(b) => b,
            None => return Some(Action::Quit(QuitReason::MissingBody)),
        }
    };
}

macro_rules! json_from_body {
    ($type:ty, $body:expr) => {
        match str::from_utf8($body) {
            Ok(s) => match serde_json::from_str::<$type>(s) {
                Ok(obj) => obj,
                Err(_) => return Action::Quit(QuitReason::InvalidBody),
            },
            Err(_) => return Action::Quit(QuitReason::InvalidBody),
        }
    };
    (return_option, $type:ty, $body:expr) => {
        match str::from_utf8($body) {
            Ok(s) => match serde_json::from_str::<$type>(s) {
                Ok(obj) => obj,
                Err(_) => return Some(Action::Quit(QuitReason::InvalidBody)),
            },
            Err(_) => return Some(Action::Quit(QuitReason::InvalidBody)),
        }
    };
}

/// Read a `QueuePosition` off any flatbuffer message that carries one
/// (`QueueInsert`, `QueueRemove`, `QueueItemSelected`). They share the same
/// `position_type()` / `position_as_index()` accessors.
macro_rules! read_queue_position {
    ($msg:expr) => {
        match $msg.position_type() {
            v4::flat::QueuePosition::Index => $msg
                .position_as_index()
                .map(|i| QueuePosition::Index(i.index())),
            v4::flat::QueuePosition::Front => Some(QueuePosition::Front),
            v4::flat::QueuePosition::Back => Some(QueuePosition::Back),
            _ => None,
        }
    };
}

struct DeviceStateMachine {
    variant: StateVariant,
    require_v4: bool,
}

impl DeviceStateMachine {
    fn new(require_v4: bool) -> Self {
        Self {
            variant: StateVariant::Connecting,
            require_v4,
        }
    }

    fn handle_opcode_common(&mut self, opcode: Opcode) -> Option<Action> {
        match opcode {
            Opcode::Ping => Some(Action::Pong),
            Opcode::None | Opcode::Pong => Some(Action::None),
            Opcode::Play
            | Opcode::Pause
            | Opcode::Resume
            | Opcode::Stop
            | Opcode::Seek
            | Opcode::SetVolume
            | _ => None,
        }
    }

    fn handle_packet_in_connecting_state(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Action {
        match opcode {
            Opcode::Version => {
                let msg = json_from_body!(VersionMessage, body!(body));
                if self.require_v4 && msg.version < 4 {
                    warn!(
                        "Receiver is known to support v4 but offered v{}, refusing insecure downgrade",
                        msg.version
                    );
                    return Action::Quit(QuitReason::InsecureDowngrade);
                }
                match msg.version {
                    2 => {
                        self.variant = StateVariant::V2;
                        Action::Connected(VersionCode::V2)
                    }
                    3 => {
                        debug!("Receiver supports v3");
                        self.variant = StateVariant::V3;
                        Action::Connected(VersionCode::V3)
                    }
                    4 => {
                        self.variant = StateVariant::V4 {
                            companion_provider: None,
                            mirroring_session: None,
                            mirroring_session_id_gen: IdGenerator::new(),
                        };
                        Action::UpgradeToTls
                    }
                    _ => Action::Quit(QuitReason::InvalidVersion),
                }
            }
            _ => Action::Quit(QuitReason::UnsupportedOpcode),
        }
    }

    fn handle_packet_common_v2_v3(
        &mut self,
        opcode: Opcode,
        body: Option<&[u8]>,
    ) -> Option<Action> {
        match opcode {
            Opcode::VolumeUpdate => Some(Action::VolumeUpdated(json_from_body!(
                return_option,
                v2::VolumeUpdateMessage,
                body!(return_option, body)
            ))),
            Opcode::PlaybackError => Some(Action::PlaybackError(json_from_body!(
                return_option,
                PlaybackErrorMessage,
                body!(return_option, body)
            ))),
            _ => None,
        }
    }

    fn handle_packet_v2(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Action {
        if let Some(action) = self.handle_packet_common_v2_v3(opcode, body) {
            return action;
        }

        match opcode {
            Opcode::PlaybackUpdate => {
                Action::PlaybackUpdateV2(json_from_body!(v2::PlaybackUpdateMessage, body!(body)))
            }
            _ => Action::Quit(QuitReason::UnsupportedOpcode),
        }
    }

    fn handle_packet_v3(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Action {
        if let Some(action) = self.handle_packet_common_v2_v3(opcode, body) {
            return action;
        }

        match opcode {
            Opcode::PlaybackUpdate => {
                Action::PlaybackUpdateV3(json_from_body!(v3::PlaybackUpdateMessage, body!(body)))
            }
            Opcode::Initial => {
                Action::Initial(json_from_body!(InitialReceiverMessage, body!(body)))
            }
            Opcode::PlayUpdate => {
                Action::PlayUpdate(json_from_body!(v3::PlayUpdateMessage, body!(body)))
            }
            Opcode::Event => Action::Event(json_from_body!(v3::EventMessage, body!(body))),
            _ => Action::Quit(QuitReason::UnsupportedOpcode),
        }
    }

    fn handle_flat_packet_v4(&mut self, body: &[u8]) -> Action {
        macro_rules! union {
            ($val:expr) => {
                match $val {
                    Some(v) => v,
                    None => return Action::Quit(QuitReason::InvalidUnionValue),
                }
            };
        }

        let Ok(packet) = v4::flat::root_as_packet(body) else {
            return Action::Quit(QuitReason::InvalidPacket);
        };
        match packet.payload_type() {
            v4::flat::Message::ProgressChanged => {
                let progress = union!(packet.payload_as_progress_changed());
                Action::ProgressChanged {
                    pos: progress
                        .position()
                        .map(|t| Duration::from_micros(t.micros()).as_secs_f64())
                        .unwrap_or(0.0),
                    dur: progress
                        .duration()
                        .map(|t| Duration::from_micros(t.micros()).as_secs_f64())
                        .unwrap_or(0.0),
                }
            }
            v4::flat::Message::VolumeChanged => {
                Action::VolumeChanged(union!(packet.payload_as_volume_changed()).volume() as f64)
            }
            v4::flat::Message::PlaybackStateChanged => {
                let msg = union!(packet.payload_as_playback_state_changed()).state();
                Action::PlaybackStateChanged(msg)
            }
            v4::flat::Message::StopPlayback => Action::PlaybackStopped,
            v4::flat::Message::MirroringSessionDescription => {
                let msg = union!(packet.payload_as_mirroring_session_description());
                let active = match &self.variant {
                    StateVariant::V4 {
                        mirroring_session, ..
                    } => *mirroring_session,
                    _ => None,
                };
                if active == Some(msg.session_id()) {
                    Action::HandleMirroringAnswer {
                        session_id: msg.session_id(),
                        sdp: msg.sdp().to_owned(),
                    }
                } else {
                    warn!(
                        "Ignoring MirroringSessionDescription for session_id={} (active session={active:?})",
                        msg.session_id()
                    );
                    Action::None
                }
            }
            v4::flat::Message::CompanionHelloResponse => {
                let msg = union!(packet.payload_as_companion_hello_response());
                if let StateVariant::V4 {
                    companion_provider, ..
                } = &mut self.variant
                {
                    debug!(
                        "Got companion provider ID ({}) with protocol version {}",
                        msg.provider_id(),
                        msg.protocol_version()
                    );
                    *companion_provider = Some((msg.provider_id(), msg.protocol_version()));
                }
                Action::CompanionHello {
                    provider_id: msg.provider_id(),
                    protocol_version: msg.protocol_version(),
                }
            }
            v4::flat::Message::CompanionResourceInfoRequest => {
                let msg = union!(packet.payload_as_companion_resource_info_request());
                Action::Companion(CompanionRequest::ResourceInfo {
                    request_id: msg.request_id(),
                    resource_id: msg.resource_id(),
                    route: msg.route().unwrap_or_default().to_owned(),
                })
            }
            v4::flat::Message::TracksAvailable => {
                let msg = union!(packet.payload_as_tracks_available());
                if let Some(new_tracks) = msg.tracks() {
                    let mut tracks = Vec::new();
                    for track in new_tracks {
                        let typ = match track.metadata_type() {
                            v4::flat::MediaTrackMetadata::Video => {
                                crate::device::MediaTrackType::Video
                            }
                            v4::flat::MediaTrackMetadata::Audio => {
                                crate::device::MediaTrackType::Audio
                            }
                            v4::flat::MediaTrackMetadata::Subtitle => {
                                crate::device::MediaTrackType::Subtitle
                            }
                            _ => continue,
                        };
                        tracks.push(crate::device::MediaTrack {
                            id: track.id(),
                            title: track.title().map(String::from),
                            language: track.iso_639().to_owned(),
                            typ,
                        });
                    }
                    Action::TracksAvailable(tracks)
                } else {
                    Action::None
                }
            }
            v4::flat::Message::ChangeTrack => {
                let msg = union!(packet.payload_as_change_track());
                let id = msg.id();
                let typ = match msg.track_type() {
                    v4::flat::MediaTrackType::Video => crate::device::MediaTrackType::Video,
                    v4::flat::MediaTrackType::Audio => crate::device::MediaTrackType::Audio,
                    v4::flat::MediaTrackType::Subtitle => crate::device::MediaTrackType::Subtitle,
                    _ => {
                        warn!(
                            "Got invalid track type in ChangeTrack message (type={:?})",
                            msg.track_type()
                        );
                        return Action::None;
                    }
                };
                Action::ChangeTrack { id, typ }
            }
            v4::flat::Message::SpeedChanged => {
                let msg = union!(packet.payload_as_speed_changed());
                Action::PlaybackRateChanged(msg.speed())
            }
            v4::flat::Message::Error => {
                let msg = union!(packet.payload_as_error());
                warn!("Got error: {msg:?}");
                Action::ReceiverError(receiver_error_from_flat(msg.kind()))
            }
            v4::flat::Message::QueueInsert => {
                let msg = union!(packet.payload_as_queue_insert());
                match read_queue_position!(msg) {
                    Some(position) => Action::QueueInserted {
                        entry: queue_entry_from_flat(&msg.item()),
                        position,
                    },
                    None => Action::None,
                }
            }
            v4::flat::Message::QueueRemove => {
                let msg = union!(packet.payload_as_queue_remove());
                match read_queue_position!(msg) {
                    Some(position) => Action::QueueRemoved { position },
                    None => Action::None,
                }
            }
            v4::flat::Message::QueueItemSelected => {
                let msg = union!(packet.payload_as_queue_item_selected());
                match read_queue_position!(msg) {
                    Some(position) => Action::QueueItemSelected { position },
                    None => Action::None,
                }
            }
            v4::flat::Message::ReceiverIntroduction => {
                let msg = union!(packet.payload_as_receiver_introduction());
                debug!("Receiver introduction: {msg:?}");

                let capabilities = msg.capabilities().map(map_receiver_capabilities);

                let supports_whep = capabilities
                    .as_ref()
                    .and_then(|c| c.media.as_ref())
                    .map(|m| m.protocols.iter().any(|p| p == "whep"))
                    .unwrap_or(false);

                Action::Introduction {
                    supports_whep,
                    capabilities,
                }
            }
            v4::flat::Message::CompanionResourceRequest => {
                let msg = union!(packet.payload_as_companion_resource_request());
                Action::Companion(CompanionRequest::Resource {
                    request_id: msg.request_id(),
                    resource_id: msg.resource_id(),
                    read_head: msg.read_head().map(|r| (r.start(), r.stop_inclusive())),
                    route: msg.route().unwrap_or_default().to_owned(),
                })
            }
            v4::flat::Message::Load => {
                let msg = union!(packet.payload_as_load());
                let load = match msg.source_type() {
                    v4::flat::MediaSource::Single => {
                        let item = union!(msg.source_as_single());
                        V4Load::Single(Source::Url {
                            url: item.source_url().to_owned(),
                            content_type: item.container().to_owned(),
                        })
                    }
                    v4::flat::MediaSource::Queue => {
                        let queue = union!(msg.source_as_queue());
                        let entries = queue
                            .items()
                            .iter()
                            .map(|qi| queue_entry_from_flat(&qi))
                            .collect();
                        V4Load::Queue {
                            entries,
                            start_index: queue.start_index(),
                            autoplay: queue.autoplay(),
                        }
                    }
                    _ => return Action::None,
                };
                Action::LoadedV4(load)
            }
            _ => {
                warn!(
                    "Received unhandled flatbuf message payload_type={:?}",
                    packet.payload_type()
                );
                Action::None
            }
        }
    }

    fn handle_packet_v4(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Action {
        match opcode {
            Opcode::None => Action::None,
            Opcode::Play
            | Opcode::Pause
            | Opcode::Resume
            | Opcode::Stop
            | Opcode::PlayUpdate
            | Opcode::SetPlaylistItem
            | Opcode::Version
            | Opcode::Seek
            | Opcode::Initial
            | Opcode::PlaybackError => Action::Quit(QuitReason::UnsupportedOpcode),
            Opcode::Flatbuf => self.handle_flat_packet_v4(body!(body)),
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            _ => Action::Quit(QuitReason::UnsupportedOpcode),
        }
    }

    fn handle_packet(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Action {
        if let Some(action) = self.handle_opcode_common(opcode) {
            return action;
        }

        match self.variant {
            StateVariant::Connecting => self.handle_packet_in_connecting_state(opcode, body),
            StateVariant::V2 => self.handle_packet_v2(opcode, body),
            StateVariant::V3 => self.handle_packet_v3(opcode, body),
            StateVariant::V4 { .. } => self.handle_packet_v4(opcode, body),
        }
    }

    fn start_mirroring_session(&mut self, signaller: WrappedSignaller) -> Action {
        match &mut self.variant {
            StateVariant::V4 {
                mirroring_session,
                mirroring_session_id_gen,
                ..
            } => {
                let id = mirroring_session_id_gen.next();
                *mirroring_session = Some(id);
                Action::StartMirroringSession { id, signaller }
            }
            _ => Action::None,
        }
    }
}

/// The SDK's mirror of the receiver's queue, reconstructed from the `Load`,
/// `QueueInsert`, `QueueRemove`, and `QueueItemSelected` broadcasts (and
/// updated optimistically for this sender's own mutations, since the receiver
/// only relays those to *other* senders). The mutation methods replicate the
/// receiver's accept/reject rules (`application.rs`) and report whether the
/// mirror changed, so a command the receiver refuses (an out-of-range position,
/// removing the playing item, inserting into an empty or full queue) leaves the
/// mirror in agreement with the receiver instead of desyncing it. Best-effort:
/// a fresh `Load` resyncs it.
#[derive(Default)]
struct QueueMirror {
    active: bool,
    items: Vec<QueueEntry>,
    current_index: Option<u32>,
    autoplay: bool,
}

impl QueueMirror {
    fn snapshot(&self) -> Option<QueueState> {
        self.active.then(|| QueueState {
            items: self.items.clone(),
            current_index: self.current_index,
            autoplay: self.autoplay,
        })
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn set(&mut self, items: Vec<QueueEntry>, start_index: Option<u32>, autoplay: bool) {
        let len = items.len();
        self.active = true;
        self.items = items;
        self.autoplay = autoplay;
        self.current_index = (len > 0).then(|| start_index.unwrap_or(0).min(len as u32 - 1));
    }

    fn resolve(&self, position: &QueuePosition) -> usize {
        match position {
            QueuePosition::Front => 0,
            QueuePosition::Back => self.items.len().saturating_sub(1),
            QueuePosition::Index(i) => *i as usize,
        }
    }

    fn insert(&mut self, entry: QueueEntry, position: &QueuePosition) -> bool {
        if !self.active {
            return false;
        }
        // The receiver refuses inserts into an empty or full queue (capped at 256
        // items) and positions past the end (`Back` appends).
        if self.items.is_empty() || self.items.len() > u8::MAX as usize {
            return false;
        }
        let idx = match position {
            QueuePosition::Front => 0,
            QueuePosition::Back => self.items.len(),
            QueuePosition::Index(i) => *i as usize,
        };
        if idx > self.items.len() {
            return false;
        }
        self.items.insert(idx, entry);
        // Mirror the receiver's index bookkeeping (application.rs).
        if let Some(cur) = self.current_index.as_mut() {
            if idx as u32 <= *cur {
                *cur += 1;
            }
        }
        true
    }

    fn remove(&mut self, position: &QueuePosition) -> bool {
        if !self.active {
            return false;
        }
        let idx = self.resolve(position);
        // The receiver refuses out-of-range positions and removal of the currently
        // playing item, so the mirror must keep them too.
        if idx >= self.items.len() || Some(idx as u32) == self.current_index {
            return false;
        }
        self.items.remove(idx);
        if let Some(cur) = self.current_index.as_mut() {
            if (idx as u32) < *cur {
                *cur -= 1;
            }
        }
        true
    }

    fn select(&mut self, position: &QueuePosition) -> bool {
        if !self.active {
            return false;
        }
        let idx = self.resolve(position);
        // The receiver refuses out-of-range selects rather than clamping. A same-index
        // select is accepted receiver-side (it restarts the item) but leaves
        // the snapshot unchanged, so it isn't re-emitted.
        if idx >= self.items.len() || Some(idx as u32) == self.current_index {
            return false;
        }
        self.current_index = Some(idx as u32);
        true
    }
}

/// The SDK's mirror of the available tracks and current selection, built from
/// `TracksAvailable` and the per-type `ChangeTrack` relays.
#[derive(Default)]
struct TrackMirror {
    tracks: Vec<MediaTrack>,
    selected_video: Option<u32>,
    selected_audio: Option<u32>,
    selected_subtitle: Option<u32>,
}

impl TrackMirror {
    fn snapshot(&self) -> TrackList {
        TrackList {
            tracks: self.tracks.clone(),
            selected_video: self.selected_video,
            selected_audio: self.selected_audio,
            selected_subtitle: self.selected_subtitle,
        }
    }

    fn set_selected(&mut self, id: Option<u32>, typ: &MediaTrackType) {
        match typ {
            MediaTrackType::Video => self.selected_video = id,
            MediaTrackType::Audio => self.selected_audio = id,
            MediaTrackType::Subtitle => self.selected_subtitle = id,
        }
    }
}

fn to_v4_queue_position(position: QueuePosition) -> v4::QueuePosition {
    match position {
        QueuePosition::Front => v4::QueuePosition::Front,
        QueuePosition::Back => v4::QueuePosition::Back,
        QueuePosition::Index(idx) => v4::QueuePosition::Index(idx),
    }
}

/// Convert a received flatbuffer `MediaItem` into the public [`MediaItem`].
/// Received items are always URL-sourced (companion items arrive as companion
/// URLs), and headers / typed metadata are stripped by the receiver relay.
fn media_item_from_flat(item: &v4::flat::MediaItem<'_>) -> MediaItem {
    MediaItem {
        content_type: item.container().to_owned(),
        source: MediaLocator::Url {
            url: item.source_url().to_owned(),
        },
        start_time: item
            .start_time()
            .map(|t| Duration::from_micros(t.micros()).as_secs_f64()),
        volume: item.volume().map(|v| v as f64),
        speed: item.speed().map(|s| s as f64),
        request_headers: None,
        title: item.title().map(|s| s.to_owned()),
        thumbnail_url: item.thumbnail_url().map(|s| s.to_owned()),
    }
}

fn queue_entry_from_flat(item: &v4::flat::QueueItem<'_>) -> QueueEntry {
    QueueEntry {
        item: media_item_from_flat(&item.media_item()),
        playback_duration: item
            .playback_duration()
            .map(|t| Duration::from_micros(t.micros()).as_secs_f64()),
    }
}

fn receiver_error_from_flat(kind: v4::flat::ErrorKind) -> ReceiverError {
    use v4::flat::ErrorKind as K;
    match kind {
        K::InvalidOpcode => ReceiverError::InvalidOpcode,
        K::ResourceNotFound => ReceiverError::ResourceNotFound,
        K::SeekOutOfRange => ReceiverError::SeekOutOfRange,
        K::VolumeOutOfRange => ReceiverError::VolumeOutOfRange,
        K::RateOutOfRange => ReceiverError::RateOutOfRange,
        K::UnsupportedFormat => ReceiverError::UnsupportedFormat,
        K::MalformedBody => ReceiverError::MalformedBody,
        K::InvalidState => ReceiverError::InvalidState,
        K::QueuePositionOutOfRange => ReceiverError::QueuePositionOutOfRange,
        K::QueueRemovePlayingItem => ReceiverError::QueueRemovePlayingItem,
        K::QueueFull => ReceiverError::QueueFull,
        K::InvalidPayloadType => ReceiverError::InvalidPayloadType,
        K::Internal => ReceiverError::Internal,
        _ => ReceiverError::Unknown,
    }
}

/// Convert the legacy lossy [`QueueItem`] into a [`QueueEntry`], carrying its
/// title/thumbnail through so the deprecated queue API still populates the
/// richer wire fields.
fn queue_item_to_entry(item: QueueItem) -> QueueEntry {
    let (content_type, source, request_headers, metadata) = match item {
        QueueItem::Url {
            url,
            content_type,
            metadata,
            request_headers,
        } => (
            content_type,
            MediaLocator::Url { url },
            request_headers,
            metadata,
        ),
        QueueItem::FCompanion {
            content_type,
            source,
            metadata,
        } => (
            content_type,
            MediaLocator::FCompanion { source },
            None,
            metadata,
        ),
    };
    let (title, thumbnail_url) = match metadata {
        Some(m) => (m.title, m.thumbnail_url),
        None => (None, None),
    };
    QueueEntry {
        item: MediaItem {
            content_type,
            source,
            start_time: None,
            volume: None,
            speed: None,
            request_headers,
            title,
            thumbnail_url,
        },
        playback_duration: None,
    }
}

#[derive(Debug)]
struct StaticCompanionResource {
    file: Arc<Mutex<std::fs::File>>,
    content_type: String,
    /// Cached at registration. Companion resources are treated as immutable for
    /// the session.
    len: u64,
}

impl CompanionResource for StaticCompanionResource {
    fn info(
        &self,
        _route: CompanionResourceRoute,
    ) -> CompanionResourceFuture<'_, CompanionResourceInfo> {
        let file = Arc::clone(&self.file);
        let content_type = self.content_type.clone();
        Box::pin(async move {
            let size = tokio::task::spawn_blocking(move || file.lock().unwrap().metadata())
                .await
                .map_err(std::io::Error::other)??
                .len();
            Ok(CompanionResourceInfo {
                content_type,
                size: Some(size),
            })
        })
    }

    fn read(&self, request: CompanionResourceRequest) -> CompanionResourceFuture<'_, Vec<u8>> {
        let file = Arc::clone(&self.file);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut file = file.lock().unwrap();
                let file_len = file.metadata()?.len();
                let Some((start, stop)) = request
                    .range
                    .map(|range| (*range.start(), *range.end()))
                    .or_else(|| (file_len > 0).then_some((0, file_len - 1)))
                else {
                    return Ok(Vec::new());
                };
                if start > stop {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "invalid inclusive companion range",
                    ));
                }
                let len = resource_bytes_to_read(start, stop, file_len);
                let max_len = companion::MAX_RESOURCE_READ_SIZE * u8::MAX as usize;
                if len > max_len as u64 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "companion response exceeds multipart limit",
                    ));
                }
                let mut data = vec![0; len as usize];
                file.seek(std::io::SeekFrom::Start(start))?;
                file.read_exact(&mut data)?;
                Ok(data)
            })
            .await
            .map_err(std::io::Error::other)?
        })
    }
}

#[derive(Debug)]
struct RegisteredCompanionResource {
    resource: Arc<dyn CompanionResource>,
    playback_owned: bool,
}

#[derive(Debug)]
struct PendingCompanionRegistration {
    resource: Arc<dyn CompanionResource>,
    reply:
        oneshot::Sender<Result<CompanionResourceRegistration, CompanionResourceRegistrationError>>,
}

enum CompanionCallbackKind {
    Info(CompanionResourceRoute),
    Read(CompanionResourceRequest),
}

struct CompanionCallbackJob {
    resource_id: u32,
    request_id: u32,
    resource: Arc<dyn CompanionResource>,
    kind: CompanionCallbackKind,
}

enum CompanionCallbackValue {
    Info(std::io::Result<CompanionResourceInfo>),
    Read {
        requested_range: Option<RangeInclusive<u64>>,
        result: std::io::Result<Vec<u8>>,
    },
}

struct CompanionCallbackResult {
    request_id: u32,
    value: CompanionCallbackValue,
}

type ActiveCompanionCallback =
    Pin<Box<dyn Future<Output = (u64, Option<CompanionCallbackResult>)> + Send + 'static>>;

#[derive(Default)]
struct CompanionCallbacks {
    queued: VecDeque<CompanionCallbackJob>,
    active: FuturesUnordered<ActiveCompanionCallback>,
    aborts: HashMap<u64, (u32, AbortHandle)>,
    next_id: u64,
}

impl CompanionCallbacks {
    fn push(&mut self, job: CompanionCallbackJob) {
        self.queued.push_back(job);
        self.fill();
    }

    fn fill(&mut self) {
        while self.active.len() < MAX_COMPANION_CALLBACKS {
            let Some(job) = self.queued.pop_front() else {
                break;
            };
            let callback_id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let (abort, registration) = AbortHandle::new_pair();
            self.aborts.insert(callback_id, (job.resource_id, abort));
            let callback = async move {
                let value = match job.kind {
                    CompanionCallbackKind::Info(route) => CompanionCallbackValue::Info(
                        match tokio::time::timeout(
                            COMPANION_CALLBACK_TIMEOUT,
                            job.resource.info(route),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "companion metadata callback timed out",
                            )),
                        },
                    ),
                    CompanionCallbackKind::Read(request) => {
                        let requested_range = request.range.clone();
                        CompanionCallbackValue::Read {
                            requested_range,
                            result: match tokio::time::timeout(
                                COMPANION_CALLBACK_TIMEOUT,
                                job.resource.read(request),
                            )
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => Err(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "companion read callback timed out",
                                )),
                            },
                        }
                    }
                };
                CompanionCallbackResult {
                    request_id: job.request_id,
                    value,
                }
            };
            self.active.push(Box::pin(async move {
                (
                    callback_id,
                    Abortable::new(callback, registration).await.ok(),
                )
            }));
        }
    }

    async fn next(&mut self) -> Option<CompanionCallbackResult> {
        loop {
            let (id, result) = self.active.next().await?;
            self.aborts.remove(&id);
            self.fill();
            if result.is_some() {
                return result;
            }
        }
    }

    fn cancel_missing(&mut self, resources: &HashMap<u32, RegisteredCompanionResource>) {
        self.queued
            .retain(|job| resources.contains_key(&job.resource_id));
        let removed: Vec<_> = self
            .aborts
            .iter()
            .filter_map(|(id, (resource_id, _))| {
                (!resources.contains_key(resource_id)).then_some(*id)
            })
            .collect();
        for id in removed {
            if let Some((_, abort)) = self.aborts.remove(&id) {
                abort.abort();
            }
        }
    }
}

struct InnerDevice {
    event_handler: Arc<dyn DeviceEventHandler>,
    stream: NetworkStream,
    session_version: FCastVersion,
    app_info: Option<ApplicationInfo>,
    supports_whep: Arc<AtomicBool>,
    state_machine: DeviceStateMachine,
    companion_resources: HashMap<u32, RegisteredCompanionResource>,
    pending_companion_registrations: VecDeque<PendingCompanionRegistration>,
    next_companion_resource_id: u64,
    connection_generation: u64,
    receiver_fingerprint: Option<Vec<u8>>,
    signaller: Option<Arc<dyn crate::device::FWRTCSignaller>>,
    queue_mirror: QueueMirror,
    track_mirror: TrackMirror,
    /// Set when this sender dispatches a load and cleared by the first
    /// Buffering/Playing update that follows it. While set, an inbound
    /// `StopPlayback` relay is ambiguous: the receiver may have relayed
    /// another sender's stop of the *previous* media before processing our
    /// load, so playback-scoped state (in particular the companion sources
    /// the new load needs) must not be dropped for it.
    load_in_flight: bool,
    /// Companion-source commands that arrived before the receiver assigned the
    /// companion provider ID. The `Connected` event precedes the
    /// `CompanionHelloResponse` carrying the ID, so a load issued right at
    /// connect time lands in this window. Replayed in order once the ID
    /// arrives.
    pending_companion_cmds: Vec<Command>,
}

impl InnerDevice {
    pub fn new(
        app_info: Option<ApplicationInfo>,
        event_handler: Arc<dyn DeviceEventHandler>,
        session_version: FCastVersion,
        supports_whep: Arc<AtomicBool>,
        receiver_fingerprint: Option<Vec<u8>>,
    ) -> Self {
        Self {
            event_handler,
            stream: NetworkStream::None,
            session_version,
            app_info,
            supports_whep,
            state_machine: DeviceStateMachine::new(receiver_fingerprint.is_some()),
            companion_resources: HashMap::new(),
            pending_companion_registrations: VecDeque::new(),
            next_companion_resource_id: 0,
            connection_generation: 0,
            receiver_fingerprint,
            signaller: None,
            queue_mirror: QueueMirror::default(),
            track_mirror: TrackMirror::default(),
            load_in_flight: false,
            pending_companion_cmds: Vec::new(),
        }
    }

    /// Reconstruct the full queue snapshot from the mirror and, when a queue is
    /// active, forward it to the event handler. The SDK keeps no queue
    /// state of its own beyond the transient mirror needed to assemble this
    /// snapshot.
    fn emit_queue_changed(&mut self) {
        if let Some(snapshot) = self.queue_mirror.snapshot() {
            self.event_handler.queue_changed(snapshot);
        }
    }

    /// Deactivate the queue mirror and, if a queue was being tracked, emit one
    /// final empty snapshot so apps holding a previous [`QueueState`] learn
    /// the queue is gone. A stop or single-item load ends it with no relay
    /// to the originator, so this local emission is the only signal.
    fn clear_queue_mirror(&mut self) {
        if self.queue_mirror.active {
            self.queue_mirror.clear();
            self.event_handler.queue_changed(QueueState::default());
        }
    }

    /// Assemble the aggregated track list from the mirror and forward it to the
    /// event handler.
    fn emit_tracks_changed(&mut self) {
        self.event_handler
            .tracks_changed(self.track_mirror.snapshot());
    }

    /// Resolve a media item's locator to a plain URL, registering a companion
    /// source (and taking ownership of any transferred fd) when needed.
    ///
    /// The returned item always carries a [`MediaLocator::Url`], so it is safe
    /// to retain in the queue mirror and re-emit to the app: no consumed
    /// file descriptor lingers in it.
    fn resolve_media_item(&mut self, item: MediaItem) -> anyhow::Result<MediaItem> {
        let source = match item.source {
            MediaLocator::Url { url } => MediaLocator::Url { url },
            MediaLocator::FCompanion { source } => MediaLocator::Url {
                url: self.companion_url(&source)?,
            },
        };
        Ok(MediaItem { source, ..item })
    }

    /// Build a v4 wire item. `item` is expected to already be resolved to a URL
    /// locator (see [`Self::resolve_media_item`]). The companion arm
    /// resolves the source itself in case a caller skips that step.
    fn build_v4_media_item(&mut self, item: MediaItem) -> anyhow::Result<v4::MediaItem> {
        let source_url = match item.source {
            MediaLocator::Url { url } => url,
            MediaLocator::FCompanion { source } => self.companion_url(&source)?,
        };
        Ok(v4::MediaItem {
            container: item.content_type,
            source_url,
            start_time: item.start_time,
            volume: item.volume.map(|v| v as f32),
            speed: item.speed.map(|s| s as f32),
            headers: item.request_headers,
            title: item.title,
            thumbnail_url: item.thumbnail_url,
            metadata: None,
            extra_metadata: None,
        })
    }

    async fn load_rich_queue(&mut self, queue: Queue) -> anyhow::Result<()> {
        let autoplay = queue.autoplay;
        // The wire index is a u8 and the receiver refuses out-of-range start indexes,
        // so clamp to the last item instead of letting `as u8` wrap to an
        // arbitrary in-range value. The mirror below reuses the clamped index,
        // keeping both views on the same item.
        let start_index = queue.start_index.map(|i| {
            i.min(queue.items.len().saturating_sub(1) as u32)
                .min(u8::MAX as u32) as u8
        });
        let mut wire_items = Vec::with_capacity(queue.items.len());
        let mut entries = Vec::with_capacity(queue.items.len());
        for entry in queue.items {
            // Resolve companion sources up front so the fd is consumed exactly once and
            // only the resulting URL is kept in the mirror.
            let resolved = self.resolve_media_item(entry.item)?;
            let wire_item = self.build_v4_media_item(resolved.clone())?;
            wire_items.push((wire_item, entry.playback_duration));
            entries.push(QueueEntry {
                item: resolved,
                playback_duration: entry.playback_duration,
            });
        }
        let msg =
            v4::MessageBuilder::new().load_queue(wire_items.into_iter(), start_index, autoplay);
        self.send_bytes(Opcode::Flatbuf, &msg).await?;
        self.queue_mirror
            .set(entries, start_index.map(|i| i as u32), autoplay);
        self.emit_queue_changed();
        Ok(())
    }

    fn add_source(&mut self, source: &CompanionSource) -> std::io::Result<u32> {
        let file = match &source.descriptor {
            CompanionSourceDescriptor::Path(ref path) => std::fs::File::open(path)?,
            #[cfg(unix)]
            CompanionSourceDescriptor::Fd(fd) => std::fs::File::from(fd.take()?),
            #[cfg(not(unix))]
            CompanionSourceDescriptor::Fd(..) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "file-descriptor companion sources are only supported on Unix targets",
                ));
            }
            CompanionSourceDescriptor::Bytes(ref data) => {
                CompanionData::Bytes(std::io::Cursor::new(data.clone()))
            }
        };

        file.metadata()?;
        let source = StaticCompanionResource {
            file: Arc::new(Mutex::new(file)),
            content_type: source.content_type.clone(),
            len,
        };
        let id = self
            .next_resource_id()
            .map_err(|_| std::io::Error::other("companion resource IDs are exhausted"))?;
        self.companion_resources.insert(
            id,
            RegisteredCompanionResource {
                resource: Arc::new(source),
                playback_owned: true,
            },
        );
        Ok(id)
    }

    fn next_resource_id(&mut self) -> Result<u32, CompanionResourceRegistrationError> {
        let id = u32::try_from(self.next_companion_resource_id)
            .map_err(|_| CompanionResourceRegistrationError::Exhausted)?;
        self.next_companion_resource_id += 1;
        Ok(id)
    }

    /// A v4 session is up but the receiver has not assigned the companion
    /// provider ID yet (the window between its introduction and its
    /// `CompanionHelloResponse`).
    fn awaiting_companion_provider_id(&self) -> bool {
        matches!(
            self.state_machine.variant,
            StateVariant::V4 {
                companion_provider_id: None,
                ..
            }
        )
    }

    /// Whether executing this command requires the companion provider ID.
    fn command_awaits_companion(cmd: &Command) -> bool {
        match cmd {
            Command::Load {
                type_: LoadType::CompanionResource { .. },
                ..
            } => true,
            Command::LoadQueue(queue) => queue
                .items
                .iter()
                .any(|e| matches!(e.item.source, MediaLocator::FCompanion { .. })),
            Command::QueueInsert { item, .. } => {
                matches!(item.source, MediaLocator::FCompanion { .. })
            }
            Command::AddSubtitleSource {
                source: SubtitleCommandSource::Companion(_),
                ..
            } => true,
            _ => false,
        }
    }

    /// Release the transferred file descriptors of a command that will never
    /// execute (see [`Self::discard_companion_descriptor`]).
    fn discard_command_descriptors(&self, cmd: &Command) {
        match cmd {
            Command::Load {
                type_: LoadType::CompanionResource { source },
                ..
            } => self.discard_companion_descriptor(&source.descriptor),
            Command::LoadQueue(queue) => {
                for entry in &queue.items {
                    if let MediaLocator::FCompanion { source } = &entry.item.source {
                        self.discard_companion_descriptor(&source.descriptor);
                    }
                }
            }
            Command::QueueInsert { item, .. } => {
                if let MediaLocator::FCompanion { source } = &item.source {
                    self.discard_companion_descriptor(&source.descriptor);
                }
            }
            Command::AddSubtitleSource {
                source: SubtitleCommandSource::Companion(source),
                ..
            } => self.discard_companion_descriptor(&source.descriptor),
            _ => {}
        }
    }

    fn companion_url(&mut self, source: &CompanionSource) -> anyhow::Result<String> {
        let StateVariant::V4 {
            companion_provider, ..
        } = self.state_machine.variant
        else {
            bail!("Receiver does not support FCompanion");
        };
        let Some((provider_id, _)) = companion_provider else {
            bail!("No companion provider ID has been assigned");
        };
        let resource_id = self.add_source(source)?;
        Ok(companion::create_url(provider_id, resource_id))
    }

    async fn send<T: Serialize>(&mut self, op: Opcode, msg: T) -> anyhow::Result<()> {
        let json = serde_json::to_string(&msg)?;
        let data = json.as_bytes();
        let size = 1 + data.len();
        let mut header = vec![0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        self.stream.write_all(&header).await?;
        self.stream.write_all(&data).await?;
        self.stream.flush().await?;

        debug!("Sent opcode: {op:?}, body: {json}");

        Ok(())
    }

    async fn send_empty(&mut self, op: Opcode) -> anyhow::Result<()> {
        // TODO: use common header type with receiver
        let mut header = [0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&1u32.to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        self.stream.write_all(&header).await?;
        self.stream.flush().await?;

        if op != Opcode::Pong {
            debug!("Sent {} bytes with opcode: {op:?}", header.len());
        }

        Ok(())
    }

    async fn send_bytes(&mut self, op: Opcode, body: &[u8]) -> anyhow::Result<()> {
        let size = 1 + body.len();
        let mut header = [0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        self.stream.write_all(&header).await?;
        self.stream.write_all(&body).await?;
        self.stream.flush().await?;

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
                    _ => bail!("Unsupported load type"),
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
                    _ => bail!("Unsupported load type"),
                }
                self.send(Opcode::Play, msg).await?;
            }
            4 => {
                let url = match type_ {
                    LoadType::Url { url } => url,
                    LoadType::Content { .. } => bail!("Unsupported load type"),
                    LoadType::CompanionResource { source } => self.companion_url(&source)?,
                };

                let item = v4::MediaItem {
                    container: content_type,
                    source_url: url,
                    start_time: Some(resume_position),
                    volume: volume.map(|v| v as f32),
                    speed: speed.map(|s| s as f32),
                    headers: None,
                    title: None,
                    thumbnail_url: None,
                    metadata: None,
                    extra_metadata: None,
                };

                let msg = v4::MessageBuilder::new().load_single(item);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
                // TODO: only emit this once it's actually changed on the
                // receiver self.event_handler.
                // source_changed(Source::Url {     url: match
                // type_ {         LoadType::Url { url } => url,
                //         LoadType::Content { .. } => todo!(),
                //     },
                //     content_type,
                // });
            }
            _ => bail!("Unspoorted session version {}", self.session_version.get()),
        }
        Ok(())
    }

    fn emit_connected(
        &self,
        used_remote_addr: IpAddr,
        local_addr: IpAddr,
        capabilities: Option<crate::device::ReceiverCapabilities>,
    ) {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connected {
                used_remote_addr,
                local_addr,
                capabilities,
            });
    }

    async fn start_mirroring_session(
        &mut self,
        id: u16,
        signaller: WrappedSignaller,
        cmd_tx: &UnboundedSender<Command>,
    ) -> anyhow::Result<()> {
        let msg = v4::MessageBuilder::new().start_mirroring_session(id);
        self.send_bytes(Opcode::Flatbuf, &msg).await?;

        let (offer_tx, mut offer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        signaller
            .0
            .set_offer_sink(std::sync::Arc::new(crate::device::MirroringOfferSink::new(
                offer_tx,
            )));

        let cmd_tx_clone = cmd_tx.clone();
        tokio::spawn(async move {
            while let Some(sdp) = offer_rx.recv().await {
                let _ = cmd_tx_clone.send(Command::MirroringOffer {
                    session_id: id,
                    sdp,
                });
            }
        });

        self.signaller = Some(signaller.0);
        Ok(())
    }

    async fn send_resource_result(
        &mut self,
        request_id: u32,
        result: companion::GetResourceResult,
    ) -> anyhow::Result<()> {
        let body = companion::ResourceResponse {
            request_id,
            part: 0,
            total_parts: 1,
            result,
        }
        .serialize();
        self.send_bytes(Opcode::Resource, &body).await
    }

    async fn queue_companion_request(
        &mut self,
        callbacks: &mut CompanionCallbacks,
        request: CompanionRequest,
    ) -> anyhow::Result<()> {
        let (request_id, resource_id, route, kind) = match request {
            CompanionRequest::ResourceInfo {
                request_id,
                resource_id,
                route,
            } => (request_id, resource_id, route, None),
            CompanionRequest::Resource {
                request_id,
                resource_id,
                read_head,
                route,
            } => (request_id, resource_id, route, Some(read_head)),
        };
        let Some(resource) = self.companion_resources.get(&resource_id) else {
            if kind.is_some() {
                return self
                    .send_resource_result(request_id, companion::GetResourceResult::NotFound)
                    .await;
            }
            let msg = v4::MessageBuilder::new().companion_resource_info_response_with_status(
                request_id,
                "application/octet-stream",
                None,
                v4::flat::CompanionResourceStatus::NotFound,
            );
            return self.send_bytes(Opcode::Flatbuf, &msg).await;
        };
        let route = match CompanionResourceRoute::new(route) {
            Ok(route) => route,
            Err(_) if kind.is_some() => {
                return self
                    .send_resource_result(request_id, companion::GetResourceResult::Failed)
                    .await
            }
            Err(_) => {
                let msg = v4::MessageBuilder::new().companion_resource_info_response_with_status(
                    request_id,
                    "application/octet-stream",
                    None,
                    v4::flat::CompanionResourceStatus::Failed,
                );
                return self.send_bytes(Opcode::Flatbuf, &msg).await;
            }
        };
        let kind = match kind {
            None => CompanionCallbackKind::Info(route),
            Some(read_head) => {
                let range = read_head.map(|(start, stop)| start..=stop);
                if range
                    .as_ref()
                    .is_some_and(|range| range.start() > range.end())
                {
                    return self
                        .send_resource_result(
                            request_id,
                            companion::GetResourceResult::InvalidRange,
                        )
                        .await;
                }
                CompanionCallbackKind::Read(CompanionResourceRequest { route, range })
            }
        };
        callbacks.push(CompanionCallbackJob {
            resource_id,
            request_id,
            resource: Arc::clone(&resource.resource),
            kind,
        });
        Ok(())
    }

    async fn handle_companion_callback(
        &mut self,
        callback: CompanionCallbackResult,
    ) -> anyhow::Result<()> {
        match callback.value {
            CompanionCallbackValue::Info(Ok(info)) => {
                let msg = v4::MessageBuilder::new().companion_resource_info_response(
                    callback.request_id,
                    &info.content_type,
                    info.size,
                );
                self.send_bytes(Opcode::Flatbuf, &msg).await
            }
            CompanionCallbackValue::Info(Err(err)) => {
                let msg = v4::MessageBuilder::new().companion_resource_info_response_with_status(
                    callback.request_id,
                    "application/octet-stream",
                    None,
                    companion_info_status(err.kind()),
                );
                self.send_bytes(Opcode::Flatbuf, &msg).await
            }
            CompanionCallbackValue::Read {
                requested_range,
                result: Ok(data),
            } => {
                if requested_range.as_ref().is_some_and(|range| {
                    range
                        .end()
                        .checked_sub(*range.start())
                        .and_then(|len| len.checked_add(1))
                        .is_none_or(|len| data.len() as u64 > len)
                }) {
                    return self
                        .send_resource_result(
                            callback.request_id,
                            companion::GetResourceResult::InvalidRange,
                        )
                        .await;
                }
                let Some(total_parts) = companion::resource_part_count(data.len()) else {
                    return self
                        .send_resource_result(
                            callback.request_id,
                            companion::GetResourceResult::Failed,
                        )
                        .await;
                };
                if data.is_empty() {
                    return self
                        .send_resource_result(
                            callback.request_id,
                            companion::GetResourceResult::Success(Vec::new()),
                        )
                        .await;
                }
                for (part, chunk) in data.chunks(companion::MAX_RESOURCE_READ_SIZE).enumerate() {
                    let body = companion::ResourceResponse {
                        request_id: callback.request_id,
                        part: part as u8,
                        total_parts,
                        result: companion::GetResourceResult::Success(chunk.to_vec()),
                    }
                    .serialize();
                    self.send_bytes(Opcode::Resource, &body).await?;
                }
                Ok(())
            }
            CompanionCallbackValue::Read {
                result: Err(err), ..
            } => {
                self.send_resource_result(callback.request_id, companion_read_result(err.kind()))
                    .await
            }
        }
    }

    fn register_companion_resource(
        &mut self,
        pending: PendingCompanionRegistration,
        cmd_tx: &UnboundedSender<Command>,
    ) {
        let StateVariant::V4 {
            companion_provider, ..
        } = self.state_machine.variant
        else {
            if matches!(self.state_machine.variant, StateVariant::Connecting) {
                self.pending_companion_registrations.push_back(pending);
            } else {
                let _ = pending
                    .reply
                    .send(Err(CompanionResourceRegistrationError::Unsupported));
            }
            return;
        };
        let Some((provider_id, version)) = companion_provider else {
            self.pending_companion_registrations.push_back(pending);
            return;
        };
        if version != companion::FCOMPANION_PROTOCOL_VERSION {
            let _ = pending
                .reply
                .send(Err(CompanionResourceRegistrationError::Unsupported));
            return;
        }
        let resource_id = match self.next_resource_id() {
            Ok(id) => id,
            Err(err) => {
                let _ = pending.reply.send(Err(err));
                return;
            }
        };
        self.companion_resources.insert(
            resource_id,
            RegisteredCompanionResource {
                resource: pending.resource,
                playback_owned: false,
            },
        );
        let _ = pending.reply.send(Ok(CompanionResourceRegistration {
            command_tx: cmd_tx.clone(),
            generation: self.connection_generation,
            provider_id,
            resource_id,
            url: companion::create_url(provider_id, resource_id),
        }));
    }

    fn finish_pending_companion_registrations(&mut self, cmd_tx: &UnboundedSender<Command>) {
        let pending = std::mem::take(&mut self.pending_companion_registrations);
        for registration in pending {
            self.register_companion_resource(registration, cmd_tx);
        }
    }

    fn unregister_companion_resource(&mut self, generation: u64, resource_id: u32) {
        if generation == self.connection_generation
            && self
                .companion_resources
                .get(&resource_id)
                .is_some_and(|resource| !resource.playback_owned)
        {
            self.companion_resources.remove(&resource_id);
        }
    }

    /// Returns `true` if the main loop should be quit.
    async fn handle_action(
        &mut self,
        callbacks: &mut CompanionCallbacks,
        shared_state: &mut SharedState,
        has_emitted_connected_event: &mut bool,
        current_playlist_item_index: &mut Option<usize>,
        used_remote_addr: &IpAddr,
        local_addr: &IpAddr,
        action: Action,
        cmd_tx: &UnboundedSender<Command>,
    ) -> Result<bool, utils::WorkError> {
        macro_rules! changed {
            ($param:ident, $new:expr, $cb:ident) => {
                if shared_state.$param != $new {
                    self.event_handler.$cb($new);
                    shared_state.$param = $new;
                }
            };
        }

        match action {
            Action::None => (),
            Action::Pong => {
                self.send_empty(Opcode::Pong).await?;
            }
            Action::Quit(reason) => {
                debug!("Quitting reason: {reason:?}");
                return Ok(true);
            }
            Action::Connected(version_code) => match version_code {
                VersionCode::V2 => {
                    self.emit_connected(*used_remote_addr, *local_addr, None);
                    *has_emitted_connected_event = true;
                    self.session_version.set(2);
                    self.finish_pending_companion_registrations(cmd_tx);
                }
                VersionCode::V3 => {
                    self.send(
                        Opcode::Initial,
                        match self.app_info.as_ref() {
                            Some(info) => v3::InitialSenderMessage {
                                display_name: Some(info.display_name.clone()),
                                app_name: Some(info.name.clone()),
                                app_version: Some(info.version.clone()),
                            },
                            None => v3::InitialSenderMessage {
                                display_name: None,
                                app_name: Some(
                                    concat!("FCast Sender SDK v", env!("CARGO_PKG_VERSION"))
                                        .to_owned(),
                                ),
                                app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                            },
                        },
                    )
                    .await
                    .context("Failed to send InitialSenderMessage")?;

                    self.session_version.set(V3_FEATURES_MIN_PROTO_VERSION);

                    self.send(
                        Opcode::SubscribeEvent,
                        v3::SubscribeEventMessage {
                            event: v3::EventSubscribeObject::MediaItemEnd,
                        },
                    )
                    .await
                    .context("Failed to subscribe to MediaItemEnd")?;
                    self.finish_pending_companion_registrations(cmd_tx);
                }
            },
            Action::VolumeUpdated(msg) => {
                changed!(volume, msg.volume, volume_changed);
            }
            Action::PlaybackError(error) => {
                if self.load_in_flight {
                    self.load_in_flight = false;
                    self.clear_playback_scoped_state();
                }
                self.event_handler.playback_error(error.message);
            }
            Action::PlaybackUpdateV2(update) => {
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
            Action::PlaybackUpdateV3(update) => {
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
                *current_playlist_item_index = update.item_index.map(|idx| idx as usize);
            }
            Action::Initial(initial_msg) => {
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
                    av:
                        Some(AVCapabilities {
                            livestream:
                                Some(LivestreamCapabilities {
                                    whep: Some(supports_whep),
                                }),
                        }),
                }) = initial_msg.experimental_capabilities
                {
                    self.supports_whep.store(supports_whep, Ordering::Relaxed);
                }

                if !*has_emitted_connected_event {
                    self.emit_connected(*used_remote_addr, *local_addr, None);
                    *has_emitted_connected_event = true;
                }
            }
            Action::PlayUpdate(msg) => {
                let Some(play_data) = msg.play_data else {
                    return Ok(false);
                };
                if let Some(url) = play_data.url {
                    let source = Source::Url {
                        url,
                        content_type: play_data.container,
                    };
                    self.event_handler.source_changed(source.clone());
                    self.event_handler
                        .playback_state_changed(PlaybackState::Playing);
                    shared_state.source = Some(source);
                } else if let Some(content) = play_data.content {
                    let source = Source::Content { content };
                    self.event_handler.source_changed(source.clone());
                    self.event_handler
                        .playback_state_changed(PlaybackState::Playing);
                    shared_state.source = Some(source);
                }
            }
            Action::Event(msg) => {
                if let v3::EventObject::MediaItem {
                    variant: v3::EventType::MediaItemEnd,
                    ..
                } = msg.event
                {
                    changed!(playback_state, PlaybackState::Ended, playback_state_changed);
                }
            }
            Action::UpgradeToTls => {
                let Some(fingerprint) = self.receiver_fingerprint.clone() else {
                    error!("Missing fingerprint for TLS upgrade");
                    return Err(utils::WorkError::Disconnected);
                };

                let provider = rustls::crypto::CryptoProvider::get_default()
                    .expect("a default crypto provider should be installed")
                    .clone();
                let config = rustls::ClientConfig::builder_with_protocol_versions(&[
                    &rustls::version::TLS13,
                ])
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(CertVerifier::new(
                    fingerprint,
                    provider,
                )))
                .with_no_client_auth();
                let connector = TlsConnector::from(Arc::new(config));
                let NetworkStream::Tcp { peer_addr, .. } = &self.stream else {
                    error!("TLS upgrade requested on a non-TCP stream");
                    return Err(utils::WorkError::Disconnected);
                };
                let dnsname = rustls_pki_types::ServerName::from(peer_addr.ip());
                debug!("Upgrading network stream to use TLS");
                self.stream
                    .upgrade(&connector, dnsname, TLS_UPGRADE_TIMEOUT)
                    .await?;
                debug!("Upgraded successfully");

                let info = if let Some(info) = self.app_info.as_ref() {
                    v4::DeviceInfo {
                        display_name: Some(info.display_name.clone()),
                        app_name: Some(info.name.clone()),
                        app_version: Some(info.version.clone()),
                    }
                } else {
                    v4::DeviceInfo {
                        display_name: None,
                        app_name: Some(
                            concat!("FCast Sender SDK v", env!("CARGO_PKG_VERSION")).to_owned(),
                        ),
                        app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                    }
                };

                let msg = v4::MessageBuilder::new().sender_introduction(&info);
                self.send_bytes(Opcode::Flatbuf, &msg)
                    .await
                    .context("Failed to send InitialSenderMessage")?;

                self.session_version.set(4);

                let msg = v4::MessageBuilder::new()
                    .companion_hello_request_with_version(companion::FCOMPANION_PROTOCOL_VERSION);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            Action::ProgressChanged { pos, dur } => {
                self.event_handler.time_changed(pos);
                if shared_state.duration != dur {
                    self.event_handler.duration_changed(dur);
                }
                shared_state.time = pos;
                shared_state.duration = dur;
            }
            Action::VolumeChanged(vol) => {
                self.event_handler.volume_changed(vol);
                shared_state.volume = vol;
            }
            Action::PlaybackStateChanged(state) => {
                let state = match state {
                    v4::fcast_flatbuffers::fcast::v4::PlaybackState::Idle => PlaybackState::Idle,
                    v4::fcast_flatbuffers::fcast::v4::PlaybackState::Buffering => {
                        PlaybackState::Buffering
                    }
                    v4::fcast_flatbuffers::fcast::v4::PlaybackState::Playing => {
                        PlaybackState::Playing
                    }
                    v4::fcast_flatbuffers::fcast::v4::PlaybackState::Paused => {
                        PlaybackState::Paused
                    }
                    v4::fcast_flatbuffers::fcast::v4::PlaybackState::Ended => PlaybackState::Ended,
                    other => {
                        warn!("Received unknown playback state: {other:?}");
                        return Ok(false);
                    }
                };
                if matches!(state, PlaybackState::Buffering | PlaybackState::Playing) {
                    // Playback moving forward is the receiver's first observable response to a
                    // dispatched load. From here an inbound stop relay is unambiguous again (the
                    // receiver serializes commands, so a stop of *our* load is always relayed after
                    // this update).
                    self.load_in_flight = false;
                }
                if state == PlaybackState::Ended {
                    self.load_in_flight = false;
                    self.clear_playback_scoped_state();
                }
                self.event_handler.playback_state_changed(state);
            }
            Action::PlaybackStopped => {
                if self.load_in_flight {
                    // This relay may be another sender's stop of the *previous* media that the
                    // receiver processed before our in-flight load: dropping the companion sources
                    // registered for the new load would break it right as the receiver starts
                    // requesting them. Skip the cleanup once. A stop of the new load itself is
                    // always preceded by its Buffering/Playing update, which clears this flag.
                    self.load_in_flight = false;
                } else {
                    // Another sender (or the receiver) stopped playback: release any companion
                    // sources this sender left open, closing their fds, and retract the queue.
                    self.clear_playback_scoped_state();
                }
                self.event_handler.playback_stopped();
            }
            Action::Companion(request) => self.queue_companion_request(callbacks, request).await?,
            Action::CompanionHello { .. } => self.finish_pending_companion_registrations(cmd_tx),
            Action::StartMirroringSession { id, signaller } => {
                self.start_mirroring_session(id, signaller, cmd_tx).await?;
            }
            Action::HandleMirroringAnswer { sdp, session_id: _ } => {
                if let Some(signaller) = self.signaller.clone() {
                    signaller.on_answer_received(sdp);
                }
            }
            Action::TracksAvailable(tracks) => {
                self.track_mirror.tracks = tracks.clone();
                self.event_handler.tracks_available(tracks);
                self.emit_tracks_changed();
            }
            Action::ChangeTrack { id, typ } => {
                self.track_mirror.set_selected(id, &typ);
                self.event_handler.track_selected(id, typ);
                self.emit_tracks_changed();
            }
            Action::PlaybackRateChanged(rate) => self.event_handler.speed_changed(rate as f64),
            Action::Introduction {
                supports_whep,
                capabilities,
            } => {
                self.supports_whep.store(supports_whep, Ordering::Relaxed);

                if !*has_emitted_connected_event {
                    self.emit_connected(*used_remote_addr, *local_addr, capabilities);
                    *has_emitted_connected_event = true;
                }
            }
            Action::LoadedV4(load) => {
                self.companion_resources
                    .retain(|_, resource| !resource.playback_owned);
                match load {
                    V4Load::Single(source) => {
                        // Switching to a single item ends any active queue.
                        self.clear_queue_mirror();
                        self.event_handler.source_changed(source.clone());
                        shared_state.source = Some(source);
                    }
                    V4Load::Queue {
                        entries,
                        start_index,
                        autoplay,
                    } => {
                        let index = start_index.unwrap_or(0) as usize;
                        if let Some(entry) = entries.get(index) {
                            if let MediaLocator::Url { url } = &entry.item.source {
                                let source = Source::Url {
                                    url: url.clone(),
                                    content_type: entry.item.content_type.clone(),
                                };
                                self.event_handler.source_changed(source.clone());
                                shared_state.source = Some(source);
                            }
                        }
                        self.queue_mirror
                            .set(entries, start_index.map(|i| i as u32), autoplay);
                        self.emit_queue_changed();
                    }
                }
            }
            Action::QueueInserted { entry, position } => {
                if self.queue_mirror.insert(entry, &position) {
                    self.emit_queue_changed();
                }
            }
            Action::QueueRemoved { position } => {
                if self.queue_mirror.remove(&position) {
                    self.emit_queue_changed();
                }
            }
            Action::QueueItemSelected { position } => {
                if self.queue_mirror.select(&position) {
                    self.emit_queue_changed();
                }
            }
            Action::ReceiverError(error) => {
                self.event_handler.command_error(error);
            }
        }

        callbacks.cancel_missing(&self.companion_resources);
        Ok(false)
    }

    async fn set_playback_state(&mut self, state: v4::PlaybackState) -> anyhow::Result<()> {
        let msg = v4::MessageBuilder::new().playback_state_changed(state);
        self.send_bytes(Opcode::Flatbuf, &msg).await
    }

    async fn change_volume(&mut self, volume: f64) -> anyhow::Result<()> {
        match self.state_machine.variant {
            StateVariant::V2 | StateVariant::V3 => {
                self.send(Opcode::SetVolume, SetVolumeMessage { volume })
                    .await?;
            }
            StateVariant::V4 { .. } => {
                let builder = v4::MessageBuilder::new();
                let msg = builder.volume_changed(volume as f32);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            StateVariant::Connecting => (), // TODO: log or error out?
        }

        Ok(())
    }

    async fn seek(&mut self, time: std::time::Duration) -> anyhow::Result<()> {
        match self.state_machine.variant {
            StateVariant::V2 | StateVariant::V3 => {
                let time = time.as_secs_f64();
                self.send(Opcode::Seek, SeekMessage { time }).await?;
            }
            StateVariant::V4 { .. } => {
                let builder = v4::MessageBuilder::new();
                let time_micros = time.as_micros() as u64;
                let msg =
                    builder.progress_changed_raw(Some(&v4::flat::Time::new(time_micros)), None);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            StateVariant::Connecting => (), // TODO: log or error out?
        }

        Ok(())
    }

    /// Drop all playback-scoped state: registered companion sources (which
    /// closes the file descriptors / files they own) and the queue mirror
    /// (emitting its final empty snapshot).
    ///
    /// Called whenever playback stops, whether initiated locally
    /// ([`Self::stop_playback`]) or by another sender (an unambiguous
    /// inbound `StopPlayback` relay, [`Action::PlaybackStopped`]). A
    /// stop clears the receiver's current item and queue, so no companion
    /// resource can still be requested afterwards and nothing may be left
    /// open.
    fn clear_playback_scoped_state(&mut self) {
        self.companion_resources
            .retain(|_, resource| !resource.playback_owned);
        self.clear_queue_mirror();
    }

    fn finish_replacement_load(&mut self, first_new_id: u64, succeeded: bool) {
        self.companion_resources.retain(|id, resource| {
            !resource.playback_owned || (u64::from(*id) >= first_new_id) == succeeded
        });
    }

    /// Ask a v4 receiver to report playback progress every `interval_millis` milliseconds (floored
    /// to 100 ms, the receiver's granularity). Older receivers have no such message, so the request
    /// is skipped there.
    async fn send_progress_update_interval(&mut self, interval_millis: u64) -> anyhow::Result<()> {
        match self.state_machine.variant {
            StateVariant::V4 { .. } => {
                let interval = crate::device::sanitize_progress_interval(interval_millis);
                let micros = u64::try_from(interval.as_micros()).unwrap_or(u64::MAX);
                let msg = v4::MessageBuilder::new()
                    .set_progress_update_interval(v4::flat::Time::new(micros));
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            _ => debug!("Receiver does not support SetProgressUpdateInterval"),
        }

        Ok(())
    }

    async fn stop_playback(&mut self) -> anyhow::Result<()> {
        match self.state_machine.variant {
            StateVariant::V2 | StateVariant::V3 => self.send_empty(Opcode::Stop).await?,
            StateVariant::V4 { .. } => {
                let msg = v4::MessageBuilder::new().stop_playback();
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            _ => (),
        }

        self.event_handler.playback_stopped();
        self.event_handler
            .playback_state_changed(PlaybackState::Idle);
        // Our own stop is ordered after any load we dispatched, so there is no
        // ambiguity to preserve.
        self.load_in_flight = false;
        self.clear_playback_scoped_state();

        Ok(())
    }

    async fn change_speed(&mut self, speed: f64) -> anyhow::Result<()> {
        match self.state_machine.variant {
            StateVariant::V2 | StateVariant::V3 => {
                self.send(Opcode::SetSpeed, SetSpeedMessage { speed })
                    .await?
            }
            StateVariant::V4 { .. } => {
                let msg = v4::MessageBuilder::new().speed_changed(speed as f32);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            _ => (),
        }

        Ok(())
    }

    /// Returns `true` if the event loops should quit;
    async fn handle_command(
        &mut self,
        callbacks: &mut CompanionCallbacks,
        shared_state: &mut SharedState,
        has_emitted_connected_event: &mut bool,
        current_playlist_item_index: &mut Option<usize>,
        used_remote_addr: &IpAddr,
        local_addr: &IpAddr,
        cmd_tx: &UnboundedSender<Command>,
        playlist_length: &mut Option<usize>,
        cmd: Command,
    ) -> anyhow::Result<bool> {
        // Executing a companion-source command without the provider ID would
        // fail the whole session. Park it until the ID arrives.
        if Self::command_awaits_companion(&cmd) && self.awaiting_companion_provider_id() {
            debug!("Parking companion command until the provider ID is assigned");
            self.pending_companion_cmds.push(cmd);
            return Ok(false);
        }
        match cmd {
            Command::ChangeVolume(volume) => self.change_volume(volume).await?,
            Command::ChangeSpeed(speed) => self.change_speed(speed).await?,
            Command::Load {
                type_,
                content_type,
                resume_position,
                speed,
                volume,
                metadata,
                request_headers,
            } => {
                let first_new_id = self.next_companion_resource_id;
                let result = self
                    .load(
                        type_,
                        content_type,
                        resume_position,
                        speed,
                        volume,
                        metadata,
                        request_headers,
                    )
                    .await;
                self.finish_replacement_load(first_new_id, result.is_ok());
                result?;
                self.load_in_flight = true;
                *playlist_length = None;
                *current_playlist_item_index = None;
                // Loading a single item clears any active queue on the receiver. The originator
                // isn't sent a relay, so reflect it locally.
                self.clear_queue_mirror();
            }
            Command::LoadPlaylist(items) => {
                let first_new_id = self.next_companion_resource_id;
                let items = items
                    .into_iter()
                    .map(|item| v3::MediaItem {
                        container: item.content_type,
                        url: Some(item.content_location),
                        time: item.start_time,
                        ..Default::default()
                    })
                    .collect::<Vec<v3::MediaItem>>();

                *playlist_length = Some(items.len());
                *current_playlist_item_index = Some(0);

                let playlist = v3::PlaylistContent {
                    variant: v3::ContentType::Playlist,
                    items,
                    ..Default::default()
                };

                let Ok(json_paylaod) = serde_json::to_string(&playlist) else {
                    error!("Failed to serialize playlist to json");
                    return Ok(false);
                };

                let result = self
                    .load(
                        LoadType::Content {
                            content: json_paylaod,
                        },
                        "application/json".to_owned(),
                        0.0,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await;
                self.finish_replacement_load(first_new_id, result.is_ok());
                result?;
                self.load_in_flight = true;
            }
            Command::SetProgressUpdateInterval(interval_millis) => {
                self.send_progress_update_interval(interval_millis).await?
            }
            Command::SeekVideo(time) => self.seek(Duration::from_secs_f64(time)).await?,
            Command::StopVideo => self.stop_playback().await?,
            Command::PauseVideo => match self.state_machine.variant {
                StateVariant::V2 | StateVariant::V3 => self.send_empty(Opcode::Pause).await?,
                StateVariant::V4 { .. } => {
                    self.set_playback_state(v4::PlaybackState::Paused).await?
                }
                _ => (),
            },
            Command::ResumeVideo => match self.state_machine.variant {
                StateVariant::V2 | StateVariant::V3 => self.send_empty(Opcode::Resume).await?,
                StateVariant::V4 { .. } => {
                    self.set_playback_state(v4::PlaybackState::Playing).await?
                }
                _ => (),
            },
            Command::Quit => return Ok(true),
            Command::SetPlaylistItemIndex(item_index) => {
                self.send(
                    Opcode::SetPlaylistItem,
                    SetPlaylistItemMessage {
                        item_index: item_index as u64,
                    },
                )
                .await?
            }
            Command::JumpPlaylist(jump) => {
                let (Some(playlist_length), Some(current_playlist_item_index)) =
                    (playlist_length, current_playlist_item_index.as_mut())
                else {
                    error!("Cannot jump in playlist because a playlist is not currently playing");
                    return Ok(false);
                };
                let Some(next_index) =
                    wrapped_playlist_index(*current_playlist_item_index, jump, *playlist_length)
                else {
                    error!("Cannot jump in an empty playlist");
                    return Ok(false);
                };
                *current_playlist_item_index = next_index;

                self.send(
                    Opcode::SetPlaylistItem,
                    SetPlaylistItemMessage {
                        item_index: *current_playlist_item_index as u64,
                    },
                )
                .await?;
            }
            Command::ConnectedEventDeadlineElapsed => {
                if !*has_emitted_connected_event {
                    self.emit_connected(*used_remote_addr, *local_addr, None);
                    *has_emitted_connected_event = true;
                }
            }
            Command::StartMirroringSession(signaller) => {
                let action = self.state_machine.start_mirroring_session(signaller);
                self.handle_action(
                    callbacks,
                    shared_state,
                    has_emitted_connected_event,
                    current_playlist_item_index,
                    &used_remote_addr,
                    &local_addr,
                    action,
                    &cmd_tx,
                )
                .await?;
            }
            Command::MirroringOffer { session_id, sdp } => {
                let msg = v4::MessageBuilder::new().mirroring_session_description(session_id, &sdp);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            Command::ChangeTrack { id, track_type } => {
                let msg = v4::MessageBuilder::new().change_track(
                    id,
                    match track_type {
                        crate::device::MediaTrackType::Video => v4::flat::MediaTrackType::Video,
                        crate::device::MediaTrackType::Audio => v4::flat::MediaTrackType::Audio,
                        crate::device::MediaTrackType::Subtitle => {
                            v4::flat::MediaTrackType::Subtitle
                        }
                    },
                );
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            Command::LoadQueue(queue) => {
                let first_new_id = self.next_companion_resource_id;
                let result = self.load_rich_queue(queue).await;
                self.finish_replacement_load(first_new_id, result.is_ok());
                result?;
                self.load_in_flight = true;
                *playlist_length = None;
                *current_playlist_item_index = None;
            }
            Command::AddSubtitleSource {
                source,
                select,
                name,
            } => {
                // `command_awaits_companion` guarantees the provider ID is set by now, so a
                // companion source resolves to its `fcomp://` URL here.
                let url = match source {
                    SubtitleCommandSource::Url(url) => url,
                    SubtitleCommandSource::Companion(source) => self.companion_url(&source)?,
                };
                let msg =
                    v4::MessageBuilder::new().add_subtitle_source(&url, select, name.as_deref());
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            // TODO: update the local queue to keep track of open companion files and close them
            // when they're not needed
            Command::QueueRemove { position } => {
                let msg = v4::MessageBuilder::new().queue_remove(to_v4_queue_position(position));
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
                // The receiver relays queue mutations only to *other* senders, so mirror our
                // own change locally. The mirror applies the receiver's
                // accept/reject rules, so a mutation the receiver will refuse
                // (reported via `command_error`) is not reflected.
                if self.queue_mirror.remove(&position) {
                    self.emit_queue_changed();
                }
            }
            Command::QueueInsert {
                item,
                playback_duration,
                position,
            } => {
                let first_new_id = self.next_companion_resource_id;
                // Resolve companion sources up front so the fd is consumed once and only the
                // resulting URL is retained in the mirror.
                let resolved = self.resolve_media_item(item)?;
                let wire_item = self.build_v4_media_item(resolved.clone())?;
                let msg = v4::MessageBuilder::new().queue_insert(
                    wire_item,
                    playback_duration,
                    to_v4_queue_position(position),
                );
                if let Err(err) = self.send_bytes(Opcode::Flatbuf, &msg).await {
                    self.finish_replacement_load(first_new_id, false);
                    return Err(err);
                }
                if self.queue_mirror.insert(
                    QueueEntry {
                        item: resolved,
                        playback_duration,
                    },
                    &position,
                ) {
                    self.emit_queue_changed();
                }
            }
            Command::QueueSelect { position } => {
                let msg = v4::MessageBuilder::new().queue_select(to_v4_queue_position(position));
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
                if self.queue_mirror.select(&position) {
                    self.emit_queue_changed();
                }
            }
            Command::RegisterCompanionResource { resource, reply } => {
                self.register_companion_resource(
                    PendingCompanionRegistration { resource, reply },
                    cmd_tx,
                );
            }
            Command::UnregisterCompanionResource {
                generation,
                resource_id,
            } => self.unregister_companion_resource(generation, resource_id),
        }

        callbacks.cancel_missing(&self.companion_resources);
        Ok(false)
    }

    async fn inner_work(
        &mut self,
        addrs: &[SocketAddr],
        cmd_rx: &mut UnboundedReceiver<Command>,
        cmd_tx: UnboundedSender<Command>,
        queued_commands: &mut VecDeque<Command>,
        _txt_records: &HashMap<String, String>,
    ) -> Result<(), utils::WorkError> {
        let Some(stream) = utils::try_connect_tcp(addrs, Duration::from_secs(5), cmd_rx, |cmd| {
            if matches!(cmd, Command::Quit) {
                true
            } else {
                queued_commands.push_back(cmd);
                false
            }
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

        tokio::spawn({
            let cmd_tx = cmd_tx.clone();
            async move {
                tokio::time::sleep(CONNECTED_EVENT_DEADLINE_DURATION).await;
                let _ = cmd_tx.send(Command::ConnectedEventDeadlineElapsed);
            }
        });

        self.stream = NetworkStream::new(stream)?;
        let mut shared_state = SharedState::default();
        let mut playlist_length = None::<usize>;
        let mut current_playlist_item_index = None::<usize>;
        self.state_machine = DeviceStateMachine::new(self.receiver_fingerprint.is_some());
        self.queue_mirror = QueueMirror::default();
        self.track_mirror = TrackMirror::default();
        self.load_in_flight = false;
        self.connection_generation = self
            .connection_generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("connection generations are exhausted"))?;
        self.next_companion_resource_id = 0;
        self.companion_resources.clear();
        let mut callbacks = CompanionCallbacks::default();
        let mut deferred_commands = std::mem::take(queued_commands);
        const READ_HEADROOM: usize = 1024 * 8;
        let mut packet_reader =
            fcast_protocol::PacketReader::new(v4::MAX_PACKET_SIZE, READ_HEADROOM);

        self.send(Opcode::Version, VersionMessage { version: 4 })
            .await?;

        'main_loop: loop {
            for _ in 0..deferred_commands.len() {
                let cmd = deferred_commands.pop_front().unwrap();
                if self.command_is_ready(&cmd) {
                    if self
                        .handle_command(
                            &mut callbacks,
                            &mut shared_state,
                            &mut has_emitted_connected_event,
                            &mut current_playlist_item_index,
                            &used_remote_addr,
                            &local_addr,
                            &cmd_tx,
                            &mut playlist_length,
                            cmd,
                        )
                        .await?
                    {
                        break 'main_loop;
                    }
                } else {
                    deferred_commands.push_back(cmd);
                }
            }
            tokio::select! {
                res = self.stream.read(packet_reader.spare_capacity_mut()) => {
                    let n_read = res?;
                    if n_read == 0 {
                        return Err(utils::WorkError::Disconnected);
                    }
                    packet_reader.commit(n_read);
                    loop {
                        let packet = match packet_reader.get_packet() {
                            fcast_protocol::ReadResult::NeedData => break,
                            fcast_protocol::ReadResult::Read(packet) => packet,
                            fcast_protocol::ReadResult::PacketTooLarge(size) => {
                                error!("Received too large packet: size={size}");
                                return Err(utils::WorkError::ReceivePacket);
                            }
                        };

                        let (opcode, body) = match packet.len() {
                            0 => {
                                error!("Received packet with no opcode (size=0), disconnecting");
                                return Err(utils::WorkError::ReceivePacket);
                            }
                            1 => (packet[0], None),
                            _ => (packet[0], Some(&packet[1..])),
                        };

                        let opcode = Opcode::try_from(opcode).map_err(|e| anyhow!(e))?;

                        let action = self.state_machine.handle_packet(opcode, body);
                        if self.handle_action(
                            &mut callbacks,
                            &mut shared_state,
                            &mut has_emitted_connected_event,
                            &mut current_playlist_item_index,
                            &used_remote_addr,
                            &local_addr,
                            action,
                            &cmd_tx,
                        ).await? {
                            break 'main_loop;
                        }
                    }
                }
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;

                    debug!("Received command: {cmd:?}");
                    if self.command_is_ready(&cmd) {
                        if self.handle_command(
                            &mut callbacks,
                            &mut shared_state,
                            &mut has_emitted_connected_event,
                            &mut current_playlist_item_index,
                            &used_remote_addr,
                            &local_addr,
                            &cmd_tx,
                            &mut playlist_length,
                            cmd
                        ).await? {
                            break;
                        }
                    } else {
                        deferred_commands.push_back(cmd);
                    }
                }
                callback = callbacks.next(), if !callbacks.active.is_empty() => {
                    if let Some(callback) = callback {
                        self.handle_companion_callback(callback).await?;
                    }
                }
            }
        }

        debug!("Shutting down...");

        // TODO: shutdown network stream?

        Ok(())
    }

    fn command_is_ready(&self, command: &Command) -> bool {
        match command {
            Command::Quit
            | Command::ConnectedEventDeadlineElapsed
            | Command::RegisterCompanionResource { .. }
            | Command::UnregisterCompanionResource { .. } => true,
            Command::Load {
                type_: LoadType::CompanionResource { .. },
                ..
            }
            | Command::LoadQueue(_)
            | Command::QueueInsert { .. } => matches!(
                self.state_machine.variant,
                StateVariant::V4 {
                    companion_provider: Some(_),
                    ..
                }
            ),
            _ => !matches!(self.state_machine.variant, StateVariant::Connecting),
        }
    }

    fn clear_connection_companion_state(&mut self) {
        self.companion_resources.clear();
        for registration in self.pending_companion_registrations.drain(..) {
            let _ = registration
                .reply
                .send(Err(CompanionResourceRegistrationError::Disconnected));
        }
    }

    pub async fn work(
        mut self,
        addrs: Vec<SocketAddr>,
        mut cmd_rx: UnboundedReceiver<Command>,
        cmd_tx: UnboundedSender<Command>,
        reconnect_interval_millis: u64,
        txt_records: HashMap<String, String>,
    ) {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        let mut queued_commands = VecDeque::new();
        'worker: loop {
            let result = self
                .inner_work(
                    &addrs,
                    &mut cmd_rx,
                    cmd_tx.clone(),
                    &mut queued_commands,
                    &txt_records,
                )
                .await;
            self.clear_connection_companion_state();
            match result {
                Ok(()) => break,
                Err(err) => {
                    error!("Inner work error: {err}");
                    if reconnect_interval_millis == 0 {
                        break;
                    }
                    if !matches!(err, utils::WorkError::DidNotConnect(_)) {
                        self.event_handler
                            .connection_state_changed(DeviceConnectionState::Reconnecting);
                    }
                }
            }

            let sleep = tokio::time::sleep(Duration::from_millis(reconnect_interval_millis));
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    command = cmd_rx.recv() => match command {
                        Some(Command::Quit) | None => break 'worker,
                        Some(command) => queued_commands.push_back(command),
                    }
                }
            }
        }

        self.clear_connection_companion_state();
        while let Some(command) = queued_commands.pop_front() {
            fail_pending_registration(command);
        }
        while let Ok(command) = cmd_rx.try_recv() {
            fail_pending_registration(command);
        }
        self.session_version.set(DEFAULT_SESSION_VERSION);

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Disconnected);
    }
}

impl FCastDevice {
    fn send_command(&self, cmd: Command) -> Result<(), CastingDeviceError> {
        let mut state = self.state.lock().unwrap();
        let Some(cmd_tx) = state.command_tx.as_ref() else {
            error!("Missing command tx");
            return Err(CastingDeviceError::FailedToSendCommand);
        };
        if cmd_tx.send(cmd).is_err() {
            state.command_tx = None;
            state.started = false;
            return Err(CastingDeviceError::FailedToSendCommand);
        }
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
        let session_version = self.session_version.get();
        match feature {
            DeviceFeature::SetVolume | DeviceFeature::SetSpeed | DeviceFeature::LoadUrl => true,
            DeviceFeature::LoadImage => session_version > 2,
            DeviceFeature::LoadContent => session_version < 4,
            DeviceFeature::PlaylistNextAndPrevious
            | DeviceFeature::SetPlaylistItemIndex
            | DeviceFeature::LoadPlaylist => session_version == 3,
            DeviceFeature::WhepStreaming => self.supports_whep.load(Ordering::Relaxed),
            DeviceFeature::FCompanion
            | DeviceFeature::FWRTCSignalling
            | DeviceFeature::ChangeTrack
            | DeviceFeature::Queue
            | DeviceFeature::SetProgressUpdateInterval => session_version == 4,
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

    fn load(
        &self,
        request: LoadRequest,
        progress_update_interval_millis: Option<u64>,
    ) -> Result<(), CastingDeviceError> {
        let result = match request {
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
            LoadRequest::CompanionResource {
                content_type,
                source,
                resume_position,
                speed,
                volume,
                metadata,
            } => {
                if self.session_version.get() < 4 {
                    return Err(CastingDeviceError::UnsupportedFeature);
                }

                self.send_command(Command::Load {
                    type_: LoadType::CompanionResource { source },
                    content_type,
                    resume_position: resume_position.unwrap_or(0.0),
                    speed,
                    volume,
                    metadata,
                    request_headers: None,
                })
            }
            LoadRequest::Queue { items, start_index } => {
                if self.session_version.get() < 4 {
                    return Err(CastingDeviceError::UnsupportedFeature);
                }

                // Route the legacy lossy queue-load through the rich path.
                let queue = Queue {
                    items: items.into_iter().map(queue_item_to_entry).collect(),
                    start_index: start_index.map(|i| i as u32),
                    autoplay: false,
                };
                self.send_command(Command::LoadQueue(queue))
            }
        };
        if result.is_ok() {
            // Queued after the load command, so the receiver applies the interval right
            // after it processes the load. Skipped by the worker on receivers
            // without the message (v2/v3).
            if let Some(interval_millis) = progress_update_interval_millis {
                self.send_command(Command::SetProgressUpdateInterval(interval_millis))?;
            }
        }
        result
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
        self.send_command(Command::Quit)?;
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
        state.ever_started = true;
        state.worker_id = state.worker_id.wrapping_add(1);
        let worker_id = state.worker_id;
        debug!("Starting with address list: {addrs:?}...");

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        state.command_tx = Some(tx.clone());
        for command in state.pending_commands.drain(..) {
            if let Err(err) = tx.send(command) {
                fail_pending_registration(err.0);
            }
        }

        let fingerprint = state.txt_records.get("fp").and_then(|fp| {
            let mut fingerprint = Vec::new();
            match base64::engine::general_purpose::STANDARD.decode_vec(fp, &mut fingerprint) {
                Ok(_) => Some(fingerprint),
                Err(err) => {
                    warn!("Failed to decode `fp` TXT record as base64: {err:?}");
                    None
                }
            }
        });

        let worker_state = Arc::clone(&self.state);
        let session_version = self.session_version.clone();
        let supports_whep = Arc::clone(&self.supports_whep);
        let txt_records = state.txt_records.clone();
        let rt_handle = state.rt_handle.clone();
        drop(state);
        rt_handle.spawn(async move {
            InnerDevice::new(
                app_info,
                event_handler,
                session_version,
                supports_whep,
                fingerprint,
            )
            .work(addrs, rx, tx, reconnect_interval_millis, txt_records)
            .await;
            let mut state = worker_state.lock().unwrap();
            if state.worker_id == worker_id {
                state.command_tx = None;
                state.started = false;
            }
        });

        Ok(())
    }

    fn get_device_info(&self) -> DeviceInfo {
        let state = self.state.lock().unwrap();
        DeviceInfo {
            name: state.name.clone(),
            protocol: ProtocolType::FCast,
            addresses: state.addresses.clone(),
            port: state.port,
            txt_records: HashMap::new(), // TODO
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

    fn start_mirroring_session(
        &self,
        signaller: Arc<dyn crate::device::FWRTCSignaller>,
    ) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::FWRTCSignalling) {
            self.send_command(Command::StartMirroringSession(WrappedSignaller(signaller)))
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn change_track(
        &self,
        id: Option<u32>,
        track_type: crate::device::MediaTrackType,
    ) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::ChangeTrack) {
            self.send_command(Command::ChangeTrack { id, track_type })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn queue_remove(&self, position: QueuePosition) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::Queue) {
            self.send_command(Command::QueueRemove { position })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn queue_add(
        &self,
        item: crate::device::QueueItem,
        position: QueuePosition,
    ) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::Queue) {
            let entry = queue_item_to_entry(item);
            self.send_command(Command::QueueInsert {
                item: entry.item,
                playback_duration: entry.playback_duration,
                position,
            })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn queue_select(&self, position: QueuePosition) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::Queue) {
            self.send_command(Command::QueueSelect { position })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn load_queue(&self, queue: Queue) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::Queue) {
            self.send_command(Command::LoadQueue(queue))
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn queue_insert(
        &self,
        item: MediaItem,
        playback_duration: Option<f64>,
        position: QueuePosition,
    ) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::Queue) {
            self.send_command(Command::QueueInsert {
                item,
                playback_duration,
                position,
            })
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn set_progress_update_interval(&self, interval_millis: u64) -> Result<(), CastingDeviceError> {
        if self.supports_feature(DeviceFeature::SetProgressUpdateInterval) {
            self.send_command(Command::SetProgressUpdateInterval(interval_millis))
        } else {
            Err(CastingDeviceError::UnsupportedFeature)
        }
    }

    fn add_subtitle_source(&self, subtitle: SubtitleSource) -> Result<(), CastingDeviceError> {
        // External subtitles are a v4 feature (`AddSubtitleSource`).
        if self.session_version.get() < 4 {
            return Err(CastingDeviceError::UnsupportedFeature);
        }
        let source = match subtitle.content {
            SubtitleContent::Url { url } => SubtitleCommandSource::Url(url),
            // Data rides the companion channel. Wrap the bytes in an in-memory companion source,
            // which the command handler registers and turns into an `fcomp://` URL at send time.
            SubtitleContent::Data { data, content_type } => {
                SubtitleCommandSource::Companion(CompanionSource {
                    descriptor: CompanionSourceDescriptor::Bytes(data),
                    content_type,
                })
            }
        };
        self.send_command(Command::AddSubtitleSource {
            source,
            select: subtitle.select,
            name: subtitle.name,
        })
    }
}

fn fail_pending_registration(command: Command) {
    if let Command::RegisterCompanionResource { reply, .. } = command {
        let _ = reply.send(Err(CompanionResourceRegistrationError::Disconnected));
    }
}

fn companion_info_status(kind: std::io::ErrorKind) -> v4::flat::CompanionResourceStatus {
    match kind {
        std::io::ErrorKind::NotFound => v4::flat::CompanionResourceStatus::NotFound,
        std::io::ErrorKind::InvalidInput => v4::flat::CompanionResourceStatus::InvalidRange,
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::TimedOut => {
            v4::flat::CompanionResourceStatus::Cancelled
        }
        std::io::ErrorKind::UnexpectedEof => v4::flat::CompanionResourceStatus::EndOfStream,
        _ => v4::flat::CompanionResourceStatus::Failed,
    }
}

fn companion_read_result(kind: std::io::ErrorKind) -> companion::GetResourceResult {
    match kind {
        std::io::ErrorKind::NotFound => companion::GetResourceResult::NotFound,
        std::io::ErrorKind::InvalidInput => companion::GetResourceResult::InvalidRange,
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::TimedOut => {
            companion::GetResourceResult::Cancelled
        }
        std::io::ErrorKind::UnexpectedEof => companion::GetResourceResult::EndOfStream,
        _ => companion::GetResourceResult::Failed,
    }
}

fn resource_bytes_to_read(start: u64, stop_inclusive: u64, file_len: u64) -> u64 {
    if file_len == 0 || start >= file_len {
        return 0;
    }
    let stop_inclusive = stop_inclusive.min(file_len - 1);
    if start > stop_inclusive {
        return 0;
    }
    stop_inclusive - start + 1
}

/// Compute the playlist index after jumping `jump` positions with wraparound.
fn wrapped_playlist_index(current: usize, jump: i32, length: usize) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let next = (current as i64 + jump as i64).rem_euclid(length as i64);
    Some(next as usize)
}

#[cfg(test)]
mod tests {
    use fcast_protocol::bytes::Bytes;

    use super::*;

    #[derive(Debug)]
    struct NoopHandler;

    impl DeviceEventHandler for NoopHandler {
        fn connection_state_changed(&self, _: DeviceConnectionState) {}
        fn volume_changed(&self, _: f64) {}
        fn time_changed(&self, _: f64) {}
        fn playback_state_changed(&self, _: PlaybackState) {}
        fn duration_changed(&self, _: f64) {}
        fn speed_changed(&self, _: f64) {}
        fn source_changed(&self, _: Source) {}
        fn playback_stopped(&self) {}
        fn playback_error(&self, _: String) {}
        fn tracks_available(&self, _: Vec<MediaTrack>) {}
        fn track_selected(&self, _: Option<u32>, _: MediaTrackType) {}
        fn tracks_changed(&self, _: TrackList) {}
        fn queue_changed(&self, _: QueueState) {}
        fn command_error(&self, _: ReceiverError) {}
    }

    #[derive(Debug)]
    struct TestResource {
        data: Vec<u8>,
        error: Option<std::io::ErrorKind>,
        requests: Arc<Mutex<Vec<CompanionResourceRequest>>>,
    }

    impl TestResource {
        fn new(data: impl Into<Vec<u8>>) -> Self {
            Self {
                data: data.into(),
                error: None,
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl CompanionResource for TestResource {
        fn info(
            &self,
            _route: CompanionResourceRoute,
        ) -> CompanionResourceFuture<'_, CompanionResourceInfo> {
            Box::pin(async move {
                if let Some(kind) = self.error {
                    return Err(std::io::Error::new(kind, "test error"));
                }
                Ok(CompanionResourceInfo {
                    content_type: "video/test".to_owned(),
                    size: Some(self.data.len() as u64),
                })
            })
        }

        fn read(&self, request: CompanionResourceRequest) -> CompanionResourceFuture<'_, Vec<u8>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                if let Some(kind) = self.error {
                    Err(std::io::Error::new(kind, "test error"))
                } else {
                    Ok(self.data.clone())
                }
            })
        }
    }

    fn test_inner() -> InnerDevice {
        InnerDevice::new(
            None,
            Arc::new(NoopHandler),
            FCastVersion::new(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
    }

    fn set_companion(inner: &mut InnerDevice, provider_id: u16, version: u16) {
        inner.state_machine.variant = StateVariant::V4 {
            companion_provider: Some((provider_id, version)),
            mirroring_session: None,
            mirroring_session_id_gen: IdGenerator::new(),
        };
        inner.connection_generation = 1;
    }

    #[test]
    fn companion_resource_is_object_safe() {
        fn accepts(_: Arc<dyn CompanionResource>) {}
        accepts(Arc::new(TestResource::new([])));
    }

    #[test]
    fn companion_hello_accepts_old_and_new_responses() {
        for version in [0, companion::FCOMPANION_PROTOCOL_VERSION] {
            let mut state = DeviceStateMachine::new(false);
            state.variant = StateVariant::V4 {
                companion_provider: None,
                mirroring_session: None,
                mirroring_session_id_gen: IdGenerator::new(),
            };
            let packet =
                v4::MessageBuilder::new().companion_hello_response_with_version(17, version);
            let action = state.handle_packet(Opcode::Flatbuf, Some(&packet));
            assert_eq!(
                action,
                Action::CompanionHello {
                    provider_id: 17,
                    protocol_version: version,
                }
            );
            assert!(matches!(
                state.variant,
                StateVariant::V4 {
                    companion_provider: Some((17, v)),
                    ..
                } if v == version
            ));
        }
    }

    #[tokio::test]
    async fn pending_registration_waits_for_v1_and_url_is_stable() {
        let mut inner = test_inner();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        inner.register_companion_resource(
            PendingCompanionRegistration {
                resource: Arc::new(TestResource::new([])),
                reply,
            },
            &cmd_tx,
        );
        assert_eq!(inner.pending_companion_registrations.len(), 1);
        set_companion(&mut inner, 12, 1);
        inner.finish_pending_companion_registrations(&cmd_tx);
        let registration = result.await.unwrap().unwrap();
        assert_eq!(registration.url(), "fcomp://12.fcast/0");
        assert_eq!(
            registration.url_for("/manifest.mpd?x=1").unwrap(),
            "fcomp://12.fcast/0/manifest.mpd?x=1"
        );
        assert!(registration.url_for("//authority").is_err());
    }

    #[tokio::test]
    async fn registration_before_connect_is_retained() {
        let state = Arc::new(Mutex::new(State::new(
            DeviceInfo::fcast("test".to_owned(), vec![], 1, HashMap::new()),
            Handle::current(),
        )));
        let registrar = CompanionResourceRegistrar {
            state: Arc::downgrade(&state),
        };
        let task =
            tokio::spawn(async move { registrar.register(Arc::new(TestResource::new([]))).await });
        tokio::task::yield_now().await;
        let command = state.lock().unwrap().pending_commands.pop_front().unwrap();
        assert!(matches!(
            &command,
            Command::RegisterCompanionResource { .. }
        ));
        fail_pending_registration(command);
        assert!(matches!(
            task.await.unwrap(),
            Err(CompanionResourceRegistrationError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn companion_resource_ids_exhaust_without_reuse() {
        let mut inner = test_inner();
        set_companion(&mut inner, 12, 1);
        inner.next_companion_resource_id = u64::from(u32::MAX) + 1;
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        inner.register_companion_resource(
            PendingCompanionRegistration {
                resource: Arc::new(TestResource::new([])),
                reply,
            },
            &cmd_tx,
        );
        assert!(matches!(
            result.await.unwrap(),
            Err(CompanionResourceRegistrationError::Exhausted)
        ));
        assert!(inner.companion_resources.is_empty());
    }

    #[tokio::test]
    async fn v0_registration_is_unsupported_but_static_source_is_allowed() {
        let mut inner = test_inner();
        set_companion(&mut inner, 4, 0);
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        inner.register_companion_resource(
            PendingCompanionRegistration {
                resource: Arc::new(TestResource::new([])),
                reply,
            },
            &cmd_tx,
        );
        assert!(matches!(
            result.await.unwrap(),
            Err(CompanionResourceRegistrationError::Unsupported)
        ));

        let path =
            std::env::temp_dir().join(format!("fcast-static-companion-{}", std::process::id()));
        std::fs::write(&path, b"static").unwrap();
        let source = CompanionSource::from_path(path.to_string_lossy(), "video/test");
        assert_eq!(inner.companion_url(&source).unwrap(), "fcomp://4.fcast/0");
        std::fs::remove_file(path).unwrap();
    }

    #[derive(Debug)]
    struct NestedResource {
        registrar: CompanionResourceRegistrar,
    }

    impl CompanionResource for NestedResource {
        fn info(
            &self,
            _route: CompanionResourceRoute,
        ) -> CompanionResourceFuture<'_, CompanionResourceInfo> {
            Box::pin(async {
                Ok(CompanionResourceInfo {
                    content_type: "video/test".to_owned(),
                    size: None,
                })
            })
        }

        fn read(&self, _request: CompanionResourceRequest) -> CompanionResourceFuture<'_, Vec<u8>> {
            Box::pin(async move {
                let _nested = self
                    .registrar
                    .register(Arc::new(TestResource::new([])))
                    .await
                    .map_err(std::io::Error::other)?;
                Ok(vec![1])
            })
        }
    }

    #[tokio::test]
    async fn nested_registration_does_not_deadlock_callback_polling() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(State::new(
            DeviceInfo::fcast("test".to_owned(), vec![], 1, HashMap::new()),
            Handle::current(),
        )));
        {
            let mut state = state.lock().unwrap();
            state.command_tx = Some(cmd_tx.clone());
            state.ever_started = true;
        }
        let registrar = CompanionResourceRegistrar {
            state: Arc::downgrade(&state),
        };
        let mut inner = test_inner();
        set_companion(&mut inner, 9, 1);
        let mut callbacks = CompanionCallbacks::default();
        callbacks.push(CompanionCallbackJob {
            resource_id: 0,
            request_id: 1,
            resource: Arc::new(NestedResource { registrar }),
            kind: CompanionCallbackKind::Read(CompanionResourceRequest {
                route: CompanionResourceRoute::default(),
                range: Some(0..=0),
            }),
        });
        let mut callback = Box::pin(callbacks.next());
        let command = tokio::select! {
            command = cmd_rx.recv() => command.unwrap(),
            _ = &mut callback => panic!("callback completed before nested registration"),
        };
        let Command::RegisterCompanionResource { resource, reply } = command else {
            panic!("unexpected command");
        };
        inner
            .register_companion_resource(PendingCompanionRegistration { resource, reply }, &cmd_tx);
        let result = tokio::time::timeout(Duration::from_millis(100), callback)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result.value,
            CompanionCallbackValue::Read {
                result: Ok(ref data), ..
            } if data == &[1]
        ));
    }

    #[tokio::test]
    async fn callback_forwards_route_range_metadata_and_empty_success() {
        let resource = Arc::new(TestResource::new([]));
        let requests = Arc::clone(&resource.requests);
        let mut callbacks = CompanionCallbacks::default();
        callbacks.push(CompanionCallbackJob {
            resource_id: 0,
            request_id: 1,
            resource: resource.clone(),
            kind: CompanionCallbackKind::Info(CompanionResourceRoute::new("/meta").unwrap()),
        });
        assert!(matches!(
            callbacks.next().await.unwrap().value,
            CompanionCallbackValue::Info(Ok(CompanionResourceInfo {
                content_type,
                size: Some(0),
            })) if content_type == "video/test"
        ));
        callbacks.push(CompanionCallbackJob {
            resource_id: 0,
            request_id: 2,
            resource,
            kind: CompanionCallbackKind::Read(CompanionResourceRequest {
                route: CompanionResourceRoute::new("/segment").unwrap(),
                range: Some(4..=7),
            }),
        });
        assert!(matches!(
            callbacks.next().await.unwrap().value,
            CompanionCallbackValue::Read { result: Ok(data), .. } if data.is_empty()
        ));
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[CompanionResourceRequest {
                route: CompanionResourceRoute::new("/segment").unwrap(),
                range: Some(4..=7),
            }]
        );
        assert_eq!(companion::resource_part_count(0), Some(1));
    }

    #[test]
    fn callback_errors_map_without_disconnect_statuses() {
        use std::io::ErrorKind::*;
        assert_eq!(
            companion_read_result(NotFound),
            companion::GetResourceResult::NotFound
        );
        assert_eq!(
            companion_read_result(InvalidInput),
            companion::GetResourceResult::InvalidRange
        );
        assert_eq!(
            companion_read_result(TimedOut),
            companion::GetResourceResult::Cancelled
        );
        assert_eq!(
            companion_read_result(UnexpectedEof),
            companion::GetResourceResult::EndOfStream
        );
        assert_eq!(
            companion_info_status(PermissionDenied),
            v4::flat::CompanionResourceStatus::Failed
        );
    }

    #[derive(Debug)]
    struct BlockingResource {
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl CompanionResource for BlockingResource {
        fn info(
            &self,
            _route: CompanionResourceRoute,
        ) -> CompanionResourceFuture<'_, CompanionResourceInfo> {
            Box::pin(std::future::pending())
        }

        fn read(&self, _request: CompanionResourceRequest) -> CompanionResourceFuture<'_, Vec<u8>> {
            Box::pin(async move {
                let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(current, Ordering::SeqCst);
                let _guard = ActiveGuard(Arc::clone(&self.active));
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn callbacks_are_limited_and_cancelled_on_drop() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resource = Arc::new(BlockingResource {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        });
        let mut callbacks = CompanionCallbacks::default();
        for request_id in 0..9 {
            callbacks.push(CompanionCallbackJob {
                resource_id: 0,
                request_id,
                resource: resource.clone(),
                kind: CompanionCallbackKind::Read(CompanionResourceRequest {
                    route: CompanionResourceRoute::default(),
                    range: None,
                }),
            });
        }
        let task = tokio::spawn(async move { callbacks.next().await });
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), MAX_COMPANION_CALLBACKS);
        assert_eq!(peak.load(Ordering::SeqCst), MAX_COMPANION_CALLBACKS);
        task.abort();
        let _ = task.await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn callback_timeout_is_cancelled() {
        let resource = Arc::new(BlockingResource {
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let mut callbacks = CompanionCallbacks::default();
        callbacks.push(CompanionCallbackJob {
            resource_id: 0,
            request_id: 1,
            resource,
            kind: CompanionCallbackKind::Read(CompanionResourceRequest {
                route: CompanionResourceRoute::default(),
                range: None,
            }),
        });
        let result = callbacks.next().await.unwrap();
        assert!(matches!(
            result.value,
            CompanionCallbackValue::Read {
                result: Err(ref err), ..
            } if err.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[tokio::test]
    async fn generation_and_connection_cleanup_are_safe() {
        let mut inner = test_inner();
        set_companion(&mut inner, 3, 1);
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        inner.register_companion_resource(
            PendingCompanionRegistration {
                resource: Arc::new(TestResource::new([])),
                reply,
            },
            &cmd_tx,
        );
        let registration = result.await.unwrap().unwrap();
        assert_eq!(inner.companion_resources.len(), 1);
        inner.connection_generation = 2;
        inner.unregister_companion_resource(1, 0);
        assert_eq!(inner.companion_resources.len(), 1);
        inner.unregister_companion_resource(2, 0);
        assert!(inner.companion_resources.is_empty());
        drop(registration);

        let (reply, result) = oneshot::channel();
        inner
            .pending_companion_registrations
            .push_back(PendingCompanionRegistration {
                resource: Arc::new(TestResource::new([])),
                reply,
            });
        inner.clear_connection_companion_state();
        assert!(matches!(
            result.await.unwrap(),
            Err(CompanionResourceRegistrationError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn dropped_registration_unregisters_resource() {
        let mut inner = test_inner();
        set_companion(&mut inner, 3, 1);
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply, result) = oneshot::channel();
        inner.register_companion_resource(
            PendingCompanionRegistration {
                resource: Arc::new(TestResource::new([])),
                reply,
            },
            &cmd_tx,
        );
        drop(result.await.unwrap().unwrap());
        let Command::UnregisterCompanionResource {
            generation,
            resource_id,
        } = cmd_rx.recv().await.unwrap()
        else {
            panic!("unexpected command");
        };
        inner.unregister_companion_resource(generation, resource_id);
        assert!(inner.companion_resources.is_empty());
    }

    #[tokio::test]
    async fn failed_command_send_is_reported_and_resets_worker_state() {
        let device = FCastDevice::new(
            DeviceInfo::fcast(
                "test".to_owned(),
                vec![IpAddr::v4(127, 0, 0, 1)],
                1,
                HashMap::new(),
            ),
            Handle::current(),
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        {
            let mut state = device.state.lock().unwrap();
            state.command_tx = Some(tx);
            state.started = true;
            state.ever_started = true;
        }
        assert!(matches!(
            device.seek(1.0),
            Err(CastingDeviceError::FailedToSendCommand)
        ));
        let state = device.state.lock().unwrap();
        assert!(!state.started);
        assert!(state.command_tx.is_none());
    }

    #[tokio::test]
    async fn failed_and_partial_loads_release_static_resources() {
        let path =
            std::env::temp_dir().join(format!("fcast-partial-companion-{}", std::process::id()));
        std::fs::write(&path, b"static").unwrap();
        let mut inner = test_inner();
        set_companion(&mut inner, 5, 1);
        let first_new_id = inner.next_companion_resource_id;
        let result = inner
            .load(
                LoadType::CompanionResource {
                    source: CompanionSource::from_path(path.to_string_lossy(), "video/test"),
                },
                "video/test".to_owned(),
                0.0,
                None,
                None,
                None,
                None,
            )
            .await;
        inner.finish_replacement_load(first_new_id, result.is_ok());
        assert!(result.is_err());
        assert!(inner.companion_resources.is_empty());

        let queue = Queue {
            items: vec![
                QueueEntry {
                    item: MediaItem {
                        content_type: "video/test".to_owned(),
                        source: MediaLocator::FCompanion {
                            source: CompanionSource::from_path(
                                path.to_string_lossy(),
                                "video/test",
                            ),
                        },
                        start_time: None,
                        volume: None,
                        speed: None,
                        request_headers: None,
                        title: None,
                        thumbnail_url: None,
                    },
                    playback_duration: None,
                },
                QueueEntry {
                    item: MediaItem {
                        source: MediaLocator::FCompanion {
                            source: CompanionSource::from_path(
                                path.with_extension("missing").to_string_lossy(),
                                "video/test",
                            ),
                        },
                        ..test_entry(2).item
                    },
                    playback_duration: None,
                },
            ],
            start_index: None,
            autoplay: true,
        };
        let first_new_id = inner.next_companion_resource_id;
        let result = inner.load_rich_queue(queue).await;
        inner.finish_replacement_load(first_new_id, result.is_ok());
        assert!(result.is_err());
        assert!(inner.companion_resources.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn owned_fd_is_consumed_and_closed_exactly_once() {
        use std::os::{fd::OwnedFd, unix::net::UnixStream};

        let (owned, mut peer) = UnixStream::pair().unwrap();
        let source = CompanionSource::from_fd(OwnedFd::from(owned), "video/test");
        let CompanionSourceDescriptor::Fd(owner) = &source.descriptor else {
            unreachable!();
        };
        let mut inner = test_inner();
        set_companion(&mut inner, 6, 1);
        inner.add_source(&source).unwrap();
        assert!(owner.take().is_err());
        inner.clear_playback_scoped_state();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        drop(source);
    }

    fn test_entry(n: u32) -> QueueEntry {
        QueueEntry {
            item: MediaItem {
                content_type: "video/mp4".to_owned(),
                source: MediaLocator::Url {
                    url: format!("http://example.test/{n}.mp4"),
                },
                start_time: None,
                volume: None,
                speed: None,
                request_headers: None,
                title: None,
                thumbnail_url: None,
            },
            playback_duration: None,
        }
    }

    fn mirror_with(n: usize, start_index: Option<u32>) -> QueueMirror {
        let mut mirror = QueueMirror::default();
        mirror.set((0..n as u32).map(test_entry).collect(), start_index, true);
        mirror
    }

    #[test]
    fn queue_mirror_refuses_removing_playing_item() {
        // The receiver rejects these with QueueRemovePlayingItem. The mirror
        // must not drift ahead of it.
        let mut mirror = mirror_with(3, Some(2));
        assert!(!mirror.remove(&QueuePosition::Back));
        assert_eq!(mirror.items.len(), 3);
        assert_eq!(mirror.current_index, Some(2));

        // Removing behind the playing item shifts it down.
        assert!(mirror.remove(&QueuePosition::Front));
        assert_eq!(mirror.items.len(), 2);
        assert_eq!(mirror.current_index, Some(1));
    }

    #[test]
    fn queue_mirror_refuses_out_of_range_ops() {
        let mut mirror = mirror_with(2, Some(0));
        assert!(!mirror.remove(&QueuePosition::Index(2)));
        assert!(!mirror.select(&QueuePosition::Index(2)));
        assert!(!mirror.insert(test_entry(9), &QueuePosition::Index(3)));
        assert_eq!(mirror.items.len(), 2);
        assert_eq!(mirror.current_index, Some(0));
    }

    #[test]
    fn queue_mirror_refuses_insert_into_empty_or_full_queue() {
        let mut mirror = mirror_with(0, None);
        assert!(!mirror.insert(test_entry(0), &QueuePosition::Front));

        let mut mirror = mirror_with(u8::MAX as usize + 1, Some(0));
        assert!(!mirror.insert(test_entry(0), &QueuePosition::Back));
        assert_eq!(mirror.items.len(), u8::MAX as usize + 1);
    }

    #[test]
    fn queue_mirror_insert_shifts_current_index() {
        let mut mirror = mirror_with(2, Some(1));
        assert!(mirror.insert(test_entry(9), &QueuePosition::Front));
        assert_eq!(mirror.current_index, Some(2));
        assert!(mirror.insert(test_entry(10), &QueuePosition::Back));
        assert_eq!(mirror.current_index, Some(2));
    }

    #[test]
    fn queue_mirror_select_is_exact_and_deduplicated() {
        let mut mirror = mirror_with(3, Some(0));
        assert!(mirror.select(&QueuePosition::Back));
        assert_eq!(mirror.current_index, Some(2));
        // Re-selecting the same item leaves the snapshot unchanged.
        assert!(!mirror.select(&QueuePosition::Index(2)));
    }

    #[test]
    fn queue_mirror_inactive_ignores_everything() {
        let mut mirror = QueueMirror::default();
        assert!(mirror.snapshot().is_none());
        assert!(!mirror.insert(test_entry(0), &QueuePosition::Front));
        assert!(!mirror.remove(&QueuePosition::Front));
        assert!(!mirror.select(&QueuePosition::Front));
    }

    fn init_with_version(version: VersionCode) -> DeviceStateMachine {
        let mut state_machine = DeviceStateMachine::new(false);
        let body = match version {
            VersionCode::V2 => br#"{"version":2}"#,
            VersionCode::V3 => br#"{"version":3}"#,
        };
        assert_eq!(
            state_machine.handle_packet(Opcode::Version, Some(body)),
            Action::Connected(version)
        );
        assert_eq!(
            state_machine.variant,
            match version {
                VersionCode::V2 => StateVariant::V2,
                VersionCode::V3 => StateVariant::V3,
            },
        );
        state_machine
    }

    #[test]
    fn start_version_v2() {
        init_with_version(VersionCode::V2);
    }

    #[test]
    fn start_version_v3() {
        init_with_version(VersionCode::V3);
    }

    #[test]
    fn unversioned_init() {
        let mut state_machine = DeviceStateMachine::new(false);
        assert_eq!(
            state_machine.handle_packet(Opcode::Ping, None),
            Action::Pong
        );
        assert_eq!(
            state_machine.handle_packet(Opcode::Pong, None),
            Action::None
        );
    }

    #[test]
    fn no_update_in_unversioned() {
        let mut state_machine = DeviceStateMachine::new(false);
        assert_eq!(
            state_machine.handle_packet(Opcode::VolumeUpdate, Some(br#"{"volume":0.0}"#)),
            Action::Quit(QuitReason::UnsupportedOpcode)
        );
    }

    #[test]
    fn invalid_body() {
        let mut state_machine = init_with_version(VersionCode::V3);
        assert_eq!(
            state_machine.handle_packet(Opcode::VolumeUpdate, Some(br#"{"volume":0.0"#)),
            Action::Quit(QuitReason::InvalidBody)
        );
    }

    #[test]
    fn start_version_v4_upgrades_to_tls() {
        let mut state_machine = DeviceStateMachine::new(false);
        assert_eq!(
            state_machine.handle_packet(Opcode::Version, Some(br#"{"version":4}"#)),
            Action::UpgradeToTls
        );
        assert_eq!(
            state_machine.variant,
            StateVariant::V4 {
                companion_provider: None,
                mirroring_session: None,
                mirroring_session_id_gen: IdGenerator::new(),
            }
        );
    }

    fn init_v4() -> DeviceStateMachine {
        let mut state_machine = DeviceStateMachine::new(false);
        assert_eq!(
            state_machine.handle_packet(Opcode::Version, Some(br#"{"version":4}"#)),
            Action::UpgradeToTls
        );
        state_machine
    }

    /// The exact bytes receiver-core's `send_v4_message` puts on the wire for
    /// `V4Message::ProgressUpdated` (crates/receiver-core/src/fcast.rs:1380)
    /// must decode into a `ProgressChanged` action carrying position and
    /// duration in seconds.
    #[test]
    fn v4_progress_changed_decodes_position_and_duration() {
        let mut state_machine = init_v4();
        let msg = v4::MessageBuilder::new().progress_changed(
            v4::flat::Time::new(12_500_000),
            v4::flat::Time::new(90_000_000),
        );
        assert_eq!(
            state_machine.handle_packet(Opcode::Flatbuf, Some(&msg)),
            Action::ProgressChanged {
                pos: 12.5,
                dur: 90.0
            }
        );
    }

    /// An absent position/duration field is reported as zero rather than
    /// failing the packet.
    #[test]
    fn v4_progress_changed_defaults_absent_fields_to_zero() {
        let mut state_machine = init_v4();
        let msg = v4::MessageBuilder::new().progress_changed_raw(None, None);
        assert_eq!(
            state_machine.handle_packet(Opcode::Flatbuf, Some(&msg)),
            Action::ProgressChanged { pos: 0.0, dur: 0.0 }
        );
    }

    /// Track events and progress ride the *same* v4 flatbuf path
    /// (`Opcode::Flatbuf` -> `handle_flat_packet_v4`), and v3 carries no track
    /// messages at all. So a session that surfaces "Tracks available" is
    /// necessarily a v4 session whose flatbuf decoding works, which means
    /// progress decodes too. This pins that diagnostic invariant: the
    /// two cannot regress independently without failing here.
    #[test]
    fn v4_tracks_and_progress_decode_on_the_same_session() {
        let mut state_machine = init_v4();

        let tracks = v4::MessageBuilder::new().tracks_available(
            [v4::MediaTrack {
                id: 7,
                title: Some("English".into()),
                iso_639: "eng".into(),
                metadata: Some(v4::MediaTrackMetadata::Subtitle),
            }]
            .into_iter(),
        );
        assert_eq!(
            state_machine.handle_packet(Opcode::Flatbuf, Some(&tracks)),
            Action::TracksAvailable(vec![crate::device::MediaTrack {
                id: 7,
                title: Some("English".to_owned()),
                language: "eng".to_owned(),
                typ: crate::device::MediaTrackType::Subtitle,
            }])
        );

        let progress = v4::MessageBuilder::new().progress_changed(
            v4::flat::Time::new(1_000_000),
            v4::flat::Time::new(2_000_000),
        );
        assert_eq!(
            state_machine.handle_packet(Opcode::Flatbuf, Some(&progress)),
            Action::ProgressChanged { pos: 1.0, dur: 2.0 }
        );
    }

    /// v3 has no track messages, so tracks can only ever have come from a v4
    /// session.
    #[test]
    fn v3_has_no_track_messages() {
        let mut state_machine = init_with_version(VersionCode::V3);
        assert_eq!(
            state_machine.handle_packet(Opcode::Flatbuf, Some(&[])),
            Action::Quit(QuitReason::UnsupportedOpcode)
        );
    }

    #[test]
    fn require_v4_refuses_insecure_downgrade() {
        for body in [
            br#"{"version":2}"#.as_slice(),
            br#"{"version":3}"#.as_slice(),
        ] {
            let mut state_machine = DeviceStateMachine::new(true);
            assert_eq!(
                state_machine.handle_packet(Opcode::Version, Some(body)),
                Action::Quit(QuitReason::InsecureDowngrade)
            );
            assert_eq!(state_machine.variant, StateVariant::Connecting);
        }
    }

    #[test]
    fn require_v4_still_upgrades_on_v4() {
        let mut state_machine = DeviceStateMachine::new(true);
        assert_eq!(
            state_machine.handle_packet(Opcode::Version, Some(br#"{"version":4}"#)),
            Action::UpgradeToTls
        );
    }

    #[test]
    fn unsupported_version_quits_without_transition() {
        for body in [
            br#"{"version":1}"#.as_slice(),
            br#"{"version":99}"#.as_slice(),
        ] {
            let mut state_machine = DeviceStateMachine::new(false);
            assert_eq!(
                state_machine.handle_packet(Opcode::Version, Some(body)),
                Action::Quit(QuitReason::InvalidVersion)
            );
            assert_eq!(state_machine.variant, StateVariant::Connecting);
        }
    }

    #[test]
    fn version_missing_body_quits() {
        let mut state_machine = DeviceStateMachine::new(false);
        assert_eq!(
            state_machine.handle_packet(Opcode::Version, None),
            Action::Quit(QuitReason::MissingBody)
        );
        assert_eq!(state_machine.variant, StateVariant::Connecting);
    }

    #[test]
    fn version_invalid_body_quits() {
        let mut state_machine = DeviceStateMachine::new(false);
        assert_eq!(
            state_machine.handle_packet(Opcode::Version, Some(b"not json")),
            Action::Quit(QuitReason::InvalidBody)
        );
        assert_eq!(state_machine.variant, StateVariant::Connecting);
    }

    #[test]
    fn connecting_rejects_data_opcodes() {
        for opcode in [Opcode::Initial, Opcode::PlaybackUpdate, Opcode::PlayUpdate] {
            let mut state_machine = DeviceStateMachine::new(false);
            assert_eq!(
                state_machine.handle_packet(opcode, Some(b"{}")),
                Action::Quit(QuitReason::UnsupportedOpcode)
            );
            assert_eq!(state_machine.variant, StateVariant::Connecting);
        }
    }

    #[test]
    fn ping_pong_handled_in_every_state() {
        let mut connecting = DeviceStateMachine::new(false);
        assert_eq!(connecting.handle_packet(Opcode::Ping, None), Action::Pong);

        for version in [VersionCode::V2, VersionCode::V3] {
            let mut state_machine = init_with_version(version);
            assert_eq!(
                state_machine.handle_packet(Opcode::Ping, None),
                Action::Pong
            );
            assert_eq!(
                state_machine.handle_packet(Opcode::Pong, None),
                Action::None
            );
        }
    }

    #[test]
    fn version_not_renegotiated_after_connecting() {
        for version in [VersionCode::V2, VersionCode::V3] {
            let mut state_machine = init_with_version(version);
            assert_eq!(
                state_machine.handle_packet(Opcode::Version, Some(br#"{"version":3}"#)),
                Action::Quit(QuitReason::UnsupportedOpcode)
            );
        }
    }

    #[test]
    fn v3_initial_handshake_after_version() {
        let mut state_machine = init_with_version(VersionCode::V3);
        assert!(matches!(
            state_machine.handle_packet(Opcode::Initial, Some(b"{}")),
            Action::Initial(_)
        ));
    }

    #[test]
    fn resource_read_is_inclusive() {
        assert_eq!(resource_bytes_to_read(0, 0, 100), 1);
        assert_eq!(resource_bytes_to_read(0, 9, 100), 10);
        assert_eq!(resource_bytes_to_read(10, 19, 100), 10);
    }

    #[test]
    fn resource_read_whole_file() {
        assert_eq!(resource_bytes_to_read(0, 99, 100), 100);
        assert_eq!(resource_bytes_to_read(0, u64::MAX, 100), 100);
    }

    #[test]
    fn resource_read_clamps_to_eof() {
        assert_eq!(resource_bytes_to_read(50, 999, 100), 50);
        assert_eq!(resource_bytes_to_read(99, 999, 100), 1);
    }

    #[test]
    fn resource_read_exact_chunk_boundary() {
        let max = companion::MAX_RESOURCE_READ_SIZE as u64;
        let file_len = max * 3;
        let bytes = resource_bytes_to_read(0, file_len - 1, file_len);
        assert_eq!(bytes, file_len);
        assert_eq!(bytes.div_ceil(max), 3);

        let last_chunk = resource_bytes_to_read(max * 2, file_len - 1, file_len);
        assert_eq!(last_chunk, max);
        assert_eq!(last_chunk.div_ceil(max), 1);
    }

    #[test]
    fn resource_read_empty_and_invalid() {
        assert_eq!(resource_bytes_to_read(0, 0, 0), 0);
        assert_eq!(resource_bytes_to_read(0, 100, 0), 0);
        assert_eq!(resource_bytes_to_read(100, 200, 100), 0);
        assert_eq!(resource_bytes_to_read(50, 10, 100), 0);
    }

    #[test]
    fn wrapped_playlist_index_empty_playlist_is_none() {
        // Regression: `load(Playlist([]))` then `playlist_item_next()` reached `current
        // %= 0` (divide-by-zero) and `playlist_item_previous()` reached `0usize
        // - 1` (underflow). Both must now be a safe no-op, not a panic.
        assert_eq!(wrapped_playlist_index(0, 0, 0), None);
        assert_eq!(wrapped_playlist_index(0, 1, 0), None);
        assert_eq!(wrapped_playlist_index(0, -1, 0), None);
    }

    #[test]
    fn wrapped_playlist_index_forward_wraps() {
        assert_eq!(wrapped_playlist_index(0, 1, 5), Some(1));
        assert_eq!(wrapped_playlist_index(4, 1, 5), Some(0)); // past the end
        assert_eq!(wrapped_playlist_index(3, 4, 5), Some(2)); // multi-step past
                                                              // end
    }

    #[test]
    fn wrapped_playlist_index_backward_wraps() {
        assert_eq!(wrapped_playlist_index(2, -1, 5), Some(1));
        assert_eq!(wrapped_playlist_index(0, -1, 5), Some(4)); // past the start
                                                               // Previously buggy: the `jump < 0 && current == 0` special case ignored
                                                               // the jump magnitude, and negative `jump as usize` corrupted other cases.
        assert_eq!(wrapped_playlist_index(0, -3, 5), Some(2));
        assert_eq!(wrapped_playlist_index(1, -3, 5), Some(3));
    }

    #[test]
    fn wrapped_playlist_index_single_item() {
        assert_eq!(wrapped_playlist_index(0, 1, 1), Some(0));
        assert_eq!(wrapped_playlist_index(0, -1, 1), Some(0));
    }
}
