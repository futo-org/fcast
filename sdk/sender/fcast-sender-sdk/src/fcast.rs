use std::{
    collections::HashMap,
    io::{Read, Seek},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
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
use log::{debug, error, warn};
use serde::Serialize;
use tokio::{
    runtime::Handle,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};
use tokio_rustls::{rustls, TlsConnector};

use crate::{
    device::{
        ApplicationInfo, CastingDevice, CastingDeviceError, CompanionSource,
        CompanionSourceDescriptor, DeviceConnectionState, DeviceEventHandler, DeviceFeature,
        DeviceInfo, EventSubscription, KeyEvent, KeyName, LoadRequest, MediaEvent, MediaItem,
        MediaItemEventType, Metadata, PlaybackState, PlaylistItem, ProtocolType, QueueItem,
        QueuePosition, Source,
    },
    utils, IpAddr,
};

const DEFAULT_SESSION_VERSION: u64 = 2;
const EVENT_SUB_MIN_PROTO_VERSION: u64 = 3;
const PLAYLIST_MIN_PROTO_VERSION: u64 = 3;
const V3_FEATURES_MIN_PROTO_VERSION: u64 = 3;

const CONNECTED_EVENT_DEADLINE_DURATION: Duration = Duration::from_secs(2);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(5);

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

#[derive(Debug, PartialEq)]
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
    LoadQueue {
        items: Vec<crate::device::QueueItem>,
        start_index: Option<u8>,
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
    QueueAdd {
        item: crate::device::QueueItem,
        position: QueuePosition,
    },
    QueueSelect {
        position: QueuePosition,
    },
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
    command_tx: Option<UnboundedSender<Command>>,
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
            addresses: device_info.addresses,
            name: device_info.name,
            port: device_info.port,
            txt_records: device_info.txt_records,
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
        companion_provider_id: Option<u16>,
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
    },
    Resource {
        request_id: u32,
        resource_id: u32,
        read_head: Option<(/* start */ u64, /* stop_inclusive */ u64)>,
    },
}

#[derive(Debug, PartialEq)]
enum V4Load {
    Single(Source),
    Queue {
        items: Vec<QueueItem>,
        start_index: Option<u8>,
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
    Companion(CompanionRequest),
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
    },
    LoadedV4(V4Load),
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
                            companion_provider_id: None,
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
            v4::flat::Message::MirroringSessionDescription => {
                let msg = union!(packet.payload_as_mirroring_session_description());
                Action::HandleMirroringAnswer {
                    session_id: msg.session_id(),
                    sdp: msg.sdp().to_owned(),
                }
            }
            v4::flat::Message::CompanionHelloResponse => {
                let msg = union!(packet.payload_as_companion_hello_response());
                if let StateVariant::V4 {
                    companion_provider_id,
                    ..
                } = &mut self.variant
                {
                    debug!("Got companion provider ID ({})", msg.provider_id());
                    *companion_provider_id = Some(msg.provider_id());
                }

                Action::None
            }
            v4::flat::Message::CompanionResourceInfoRequest => {
                let msg = union!(packet.payload_as_companion_resource_info_request());
                Action::Companion(CompanionRequest::ResourceInfo {
                    request_id: msg.request_id(),
                    resource_id: msg.resource_id(),
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
                Action::None
            }
            v4::flat::Message::ReceiverIntroduction => {
                let msg = union!(packet.payload_as_receiver_introduction());
                debug!("Receiver introduction: {msg:?}");
                let mut supports_whep = false;
                if let Some(caps) = msg.capabilities() {
                    if let Some(media_caps) = caps.media() {
                        if let Some(protos) = media_caps.protocols() {
                            for proto in protos {
                                if proto == "whep" {
                                    supports_whep = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                Action::Introduction { supports_whep }
            }
            v4::flat::Message::CompanionResourceRequest => {
                let msg = union!(packet.payload_as_companion_resource_request());
                Action::Companion(CompanionRequest::Resource {
                    request_id: msg.request_id(),
                    resource_id: msg.resource_id(),
                    read_head: msg.read_head().map(|r| (r.start(), r.stop_inclusive())),
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
                        let items = queue
                            .items()
                            .iter()
                            .map(|qi| {
                                let media_item = qi.media_item();
                                QueueItem::Url {
                                    url: media_item.source_url().to_owned(),
                                    content_type: media_item.container().to_owned(),
                                    metadata: None,
                                    request_headers: None,
                                }
                            })
                            .collect();
                        V4Load::Queue {
                            items,
                            start_index: queue.start_index(),
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
            | Opcode::PlaybackError => Action::Quit(QuitReason::UnsupportedOpcode),
            Opcode::Flatbuf => self.handle_flat_packet_v4(body!(body)),
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            Opcode::Initial => {
                Action::Initial(json_from_body!(InitialReceiverMessage, body!(body)))
            }
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

struct WrappedCompanionSource {
    file: std::fs::File,
    content_type: String,
}

struct InnerDevice {
    event_handler: Arc<dyn DeviceEventHandler>,
    stream: NetworkStream,
    session_version: FCastVersion,
    app_info: Option<ApplicationInfo>,
    supports_whep: Arc<AtomicBool>,
    state_machine: DeviceStateMachine,
    companion_sources: HashMap<u32, WrappedCompanionSource>,
    receiver_fingerprint: Option<Vec<u8>>,
    signaller: Option<Arc<dyn crate::device::FWRTCSignaller>>,
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
            companion_sources: HashMap::new(),
            receiver_fingerprint,
            signaller: None,
        }
    }

    fn add_source(&mut self, source: &CompanionSource) -> std::io::Result<u32> {
        let file = match source.descriptor {
            CompanionSourceDescriptor::Path(ref path) => std::fs::File::open(path)?,
        };

        let source = WrappedCompanionSource {
            file,
            content_type: source.content_type.clone(),
        };

        let mut id = 0;
        while self.companion_sources.contains_key(&id) {
            id += 1;
        }
        self.companion_sources.insert(id, source);
        Ok(id)
    }

    fn companion_url(&mut self, source: &CompanionSource) -> anyhow::Result<String> {
        let StateVariant::V4 {
            companion_provider_id,
            ..
        } = self.state_machine.variant
        else {
            bail!("Receiver does not support FCompanion");
        };
        let Some(provider_id) = companion_provider_id else {
            bail!("No companion provider ID has been assigned");
        };
        let resource_id = self.add_source(source)?;
        Ok(companion::create_url(provider_id, resource_id))
    }

    async fn send<T: Serialize>(&mut self, op: Opcode, msg: T) -> anyhow::Result<()> {
        // let Some(writer) = self.writer.as_mut() else {
        // let Some(writer) = self.stream.as_mut() else {
        //     bail!("`writer` is missing");
        // };

        let json = serde_json::to_string(&msg)?;
        let data = json.as_bytes();
        let size = 1 + data.len();
        let mut header = vec![0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        // let mut packet = header;
        // packet.extend_from_slice(data);

        // writer.write_all(&packet).await?;
        self.stream.write_all(&header).await?;
        self.stream.write_all(&data).await?;
        self.stream.flush().await?;

        debug!("Sent opcode: {op:?}, body: {json}");

        Ok(())
    }

    async fn send_empty(&mut self, op: Opcode) -> anyhow::Result<()> {
        // let Some(writer) = self.writer.as_mut() else {
        // let Some(writer) = self.stream.as_mut() else {
        //     bail!("`writer` is missing");
        // };

        // TODO: use common header type with receiver
        let mut header = [0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&1u32.to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        // writer.write_all(&header).await?;
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
                    LoadType::Content { .. } => unreachable!(),
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
                // TODO: only emit this once it's actually changed on the receiver
                // self.event_handler.source_changed(Source::Url {
                //     url: match type_ {
                //         LoadType::Url { url } => url,
                //         LoadType::Content { .. } => todo!(),
                //     },
                //     content_type,
                // });
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

    async fn handle_resource_request(
        &mut self,
        request_id: u32,
        resource_id: u32,
        read_head: Option<(/* start */ u64, /* stop_inclusive */ u64)>,
    ) -> Result<(), utils::WorkError> {
        let Some(source) = self.companion_sources.get_mut(&resource_id) else {
            let body = companion::ResourceResponse {
                request_id,
                part: 0,
                total_parts: 1,
                result: companion::GetResourceResult::NotFound,
            }
            .serialize();

            let size = 1 + body.len();
            let mut header = [0u8; HEADER_LENGTH];
            header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
            header[HEADER_LENGTH - 1] = Opcode::Resource as u8;
            self.stream.write_all(&header).await?;
            self.stream.write_all(&body).await?;
            self.stream.flush().await?;
            return Ok(());
        };

        let meta = source.file.metadata()?;
        let file_len = meta.len();
        let (start, stop_inclusive): (u64, u64) = match read_head {
            Some((start, stop_inclusive)) => (start, stop_inclusive),
            None => (0, file_len.saturating_sub(1)),
        };

        source.file.seek(std::io::SeekFrom::Start(start))?;

        let max_packet_size = companion::MAX_RESOURCE_READ_SIZE;
        let mut bytes_to_read = resource_bytes_to_read(start, stop_inclusive, file_len);
        let total_packets = bytes_to_read.div_ceil(max_packet_size as u64);
        if total_packets > u8::MAX.into() {
            error!(
                "Companion resource request {request_id} for resource {resource_id} needs {total_packets} parts, exceeding the 256-part limit"
            );
            return Ok(());
        }

        let total_packets = total_packets as u8;
        let mut current_packet = 0;
        'outer: while bytes_to_read > 0 {
            let response_header = companion::ResourceResponse::header_success(
                request_id,
                current_packet,
                total_packets,
            );

            let mut packet_bytes_to_read = bytes_to_read.min(max_packet_size as u64);

            let size = 1 + response_header.len() + packet_bytes_to_read as usize;
            let mut header = [0u8; HEADER_LENGTH];
            header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
            header[HEADER_LENGTH - 1] = Opcode::Resource as u8;
            self.stream.write_all(&header).await?;
            self.stream.write_all(&response_header).await?;

            while packet_bytes_to_read > 0 {
                let mut buf = [0u8; 1024 * 8];
                let max_read = packet_bytes_to_read.min(buf.len() as u64) as usize;
                let mut n_read = source.file.read(&mut buf[0..max_read])?;
                if n_read == 0 {
                    return Err(utils::WorkError::Anyhow(anyhow!(
                        "Failed to read from local resource"
                    )));
                }

                n_read = (n_read as u64).min(bytes_to_read) as usize;

                self.stream.write_all(&buf[0..n_read]).await?;
                if (n_read as u64) < bytes_to_read {
                    packet_bytes_to_read -= n_read as u64;
                    bytes_to_read -= n_read as u64;
                } else {
                    break 'outer;
                }
            }

            current_packet += 1;
        }

        self.stream.flush().await?;

        Ok(())
    }

    /// Returns `true` if the main loop should be quit.
    async fn handle_action(
        &mut self,
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
                    self.emit_connected(*used_remote_addr, *local_addr);
                    *has_emitted_connected_event = true;
                    self.session_version.set(2);
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
                }
            },
            Action::VolumeUpdated(msg) => {
                changed!(volume, msg.volume, volume_changed);
            }
            Action::PlaybackError(error) => {
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
                    self.emit_connected(*used_remote_addr, *local_addr);
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
            Action::Event(msg) => match msg.event {
                v3::EventObject::MediaItem { variant, item } => {
                    let type_ = match variant {
                        v3::EventType::MediaItemStart => MediaItemEventType::Start,
                        v3::EventType::MediaItemEnd => MediaItemEventType::End,
                        v3::EventType::MediaItemChange => MediaItemEventType::Change,
                        _ => {
                            error!("Received event of type {variant:?} when a media event was expected");
                            return Ok(false);
                        }
                    };
                    self.event_handler.media_event(MediaEvent {
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
                                MetadataObject::Generic {
                                    title,
                                    thumbnail_url,
                                    ..
                                } => Metadata {
                                    title,
                                    thumbnail_url,
                                },
                            }),
                        },
                    });
                }
                v3::EventObject::Key {
                    variant,
                    key,
                    repeat,
                    handled,
                } => {
                    let event = KeyEvent {
                        released: match variant {
                            v3::EventType::KeyDown => false,
                            v3::EventType::KeyUp => true,
                            _ => {
                                error!("Expected Key event, got {variant:?}");
                                return Ok(false);
                            }
                        },
                        repeat,
                        handled,
                        name: key,
                    };
                    self.event_handler.key_event(event);
                }
            },
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
                let dnsname = rustls_pki_types::ServerName::from(match &self.stream {
                    NetworkStream::Tcp { peer_addr, .. } => peer_addr.ip(),
                    _ => unreachable!(),
                });
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

                let msg = v4::MessageBuilder::new().companion_hello_request();
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
                self.event_handler.playback_state_changed(state);
            }
            Action::Companion(request) => {
                match request {
                    CompanionRequest::ResourceInfo {
                        request_id,
                        resource_id,
                    } => {
                        if let Some(source) = self.companion_sources.get(&resource_id) {
                            if let Ok(meta) = source.file.metadata() {
                                let len = meta.len();
                                let msg = v4::MessageBuilder::new()
                                    .companion_resource_info_response(
                                        request_id,
                                        &source.content_type,
                                        Some(len),
                                    );
                                self.send_bytes(Opcode::Flatbuf, &msg).await?;
                            }
                        }
                        // TODO: send failed?
                    }
                    CompanionRequest::Resource {
                        request_id,
                        resource_id,
                        read_head,
                    } => {
                        self.handle_resource_request(request_id, resource_id, read_head)
                            .await?;
                    }
                }
            }
            Action::StartMirroringSession { id, signaller } => {
                self.start_mirroring_session(id, signaller, cmd_tx).await?;
            }
            Action::HandleMirroringAnswer { sdp, session_id: _ } => {
                if let Some(signaller) = self.signaller.clone() {
                    signaller.on_answer_received(sdp);
                }
            }
            Action::TracksAvailable(tracks) => self.event_handler.tracks_available(tracks),
            Action::ChangeTrack { id, typ } => self.event_handler.track_selected(id, typ),
            Action::PlaybackRateChanged(rate) => self.event_handler.speed_changed(rate as f64),
            Action::Introduction { supports_whep } => {
                self.supports_whep.store(supports_whep, Ordering::Relaxed);

                if !*has_emitted_connected_event {
                    self.emit_connected(*used_remote_addr, *local_addr);
                    *has_emitted_connected_event = true;
                }
            }
            Action::LoadedV4(load) => match load {
                V4Load::Single(source) => {
                    self.event_handler.source_changed(source.clone());
                    shared_state.source = Some(source);
                }
                V4Load::Queue { items, start_index } => {
                    // TODO: notify about the entire queue and not just one item
                    let index = start_index.unwrap_or(0) as usize;
                    if let Some(QueueItem::Url {
                        url, content_type, ..
                    }) = items.get(index)
                    {
                        let source = Source::Url {
                            url: url.clone(),
                            content_type: content_type.clone(),
                        };
                        self.event_handler.source_changed(source.clone());
                        shared_state.source = Some(source);
                    }
                }
            },
        }

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

    async fn stop_playback(&mut self) -> anyhow::Result<()> {
        match self.state_machine.variant {
            StateVariant::V2 | StateVariant::V3 => self.send_empty(Opcode::Stop).await?,
            StateVariant::V4 { .. } => {
                let msg = v4::MessageBuilder::new().stop_playback();
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            _ => (),
        }

        self.event_handler
            .playback_state_changed(PlaybackState::Idle);
        self.companion_sources.clear();

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
        shared_state: &mut SharedState,
        has_emitted_connected_event: &mut bool,
        current_playlist_item_index: &mut Option<usize>,
        used_remote_addr: &IpAddr,
        local_addr: &IpAddr,
        cmd_tx: &UnboundedSender<Command>,
        playlist_length: &mut Option<usize>,
        cmd: Command,
    ) -> anyhow::Result<bool> {
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
                self.load(
                    type_,
                    content_type,
                    resume_position,
                    speed,
                    volume,
                    metadata,
                    request_headers,
                )
                .await?;
                *playlist_length = None;
                *current_playlist_item_index = None;
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

                self.load(
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
                .await?;
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
            Command::Subscribe(ref event) | Command::Unsubscribe(ref event) => {
                if self.session_version.get() != EVENT_SUB_MIN_PROTO_VERSION {
                    error!(
                        "Current protocol version ({}) does not support event subscriptions, version >=3 is required",
                        self.session_version.get(),
                    );
                    return Ok(false);
                }
                let event = event_sub_to_object(event);
                let op = if matches!(cmd, Command::Subscribe(_)) {
                    Opcode::SubscribeEvent
                } else {
                    Opcode::UnsubscribeEvent
                };
                self.send(op, v3::SubscribeEventMessage { event }).await?;
            }
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
                if jump < 0 && *current_playlist_item_index == 0 {
                    *current_playlist_item_index = *playlist_length - 1;
                } else {
                    *current_playlist_item_index += jump as usize;
                    *current_playlist_item_index %= *playlist_length;
                }

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
                    self.emit_connected(*used_remote_addr, *local_addr);
                    *has_emitted_connected_event = true;
                }
            }
            Command::StartMirroringSession(signaller) => {
                let action = self.state_machine.start_mirroring_session(signaller);
                self.handle_action(
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
            Command::LoadQueue { items, start_index } => {
                let mut wrapped_items = Vec::new();
                for item in items {
                    let url = match &item {
                        crate::device::QueueItem::Url { url, .. } => url.clone(),
                        crate::device::QueueItem::Companion { source, .. } => {
                            self.companion_url(source)?
                        }
                    };

                    let wrapped = v4::MediaItem {
                        container: item.content_type().to_owned(),
                        source_url: url,
                        start_time: None,
                        volume: None,
                        speed: None,
                        headers: None,
                        title: None,
                        thumbnail_url: None,
                        metadata: None,
                        extra_metadata: None,
                    };
                    wrapped_items.push(wrapped);
                }

                let msg =
                    v4::MessageBuilder::new().load_queue(wrapped_items.into_iter(), start_index);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            // TODO: update the local queue to keep track of open companion files and close them when they're not needed
            Command::QueueRemove { position } => {
                let msg = v4::MessageBuilder::new().queue_remove(match position {
                    crate::device::QueuePosition::Front => v4::QueuePosition::Front,
                    crate::device::QueuePosition::Back => v4::QueuePosition::Back,
                    crate::device::QueuePosition::Index(idx) => v4::QueuePosition::Index(idx),
                });
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            Command::QueueAdd { item, position } => {
                let pos = match position {
                    crate::device::QueuePosition::Front => v4::QueuePosition::Front,
                    crate::device::QueuePosition::Back => v4::QueuePosition::Back,
                    crate::device::QueuePosition::Index(idx) => v4::QueuePosition::Index(idx),
                };
                let url = match &item {
                    crate::device::QueueItem::Url { url, .. } => url.clone(),
                    crate::device::QueueItem::Companion { source, .. } => {
                        self.companion_url(source)?
                    }
                };

                let wrapped = v4::MediaItem {
                    container: item.content_type().to_owned(),
                    source_url: url,
                    start_time: None,
                    volume: None,
                    speed: None,
                    headers: None,
                    title: None,
                    thumbnail_url: None,
                    metadata: None,
                    extra_metadata: None,
                };
                let msg = v4::MessageBuilder::new().queue_insert(wrapped, pos);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
            Command::QueueSelect { position } => {
                let pos = match position {
                    crate::device::QueuePosition::Front => v4::QueuePosition::Front,
                    crate::device::QueuePosition::Back => v4::QueuePosition::Back,
                    crate::device::QueuePosition::Index(idx) => v4::QueuePosition::Index(idx),
                };
                let msg = v4::MessageBuilder::new().queue_select(pos);
                self.send_bytes(Opcode::Flatbuf, &msg).await?;
            }
        }

        Ok(false)
    }

    async fn inner_work(
        &mut self,
        addrs: &[SocketAddr],
        cmd_rx: &mut UnboundedReceiver<Command>,
        cmd_tx: UnboundedSender<Command>,
        _txt_records: &HashMap<String, String>,
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

        tokio::spawn({
            let cmd_tx = cmd_tx.clone();
            async move {
                tokio::time::sleep(CONNECTED_EVENT_DEADLINE_DURATION).await;
                let _ = cmd_tx.send(Command::ConnectedEventDeadlineElapsed);
            }
        });

        // let (mut reader, writer) = stream.into_split();
        // self.writer = Some(writer);
        // self.stream = NetworkStream::Tcp(stream);
        self.stream = NetworkStream::new(stream)?;
        let mut shared_state = SharedState::default();
        let mut playlist_length = None::<usize>;
        let mut current_playlist_item_index = None::<usize>;
        self.state_machine = DeviceStateMachine::new(self.receiver_fingerprint.is_some());
        let mut read_buf = [0u8; 1024 * 8];
        let mut packet_reader =
            fcast_protocol::PacketReader::new(v4::MAX_PACKET_SIZE, read_buf.len());

        self.send(Opcode::Version, VersionMessage { version: 4 })
            .await?;

        'main_loop: loop {
            tokio::select! {
                // res = reader.read(&mut read_buf) => {
                res = self.stream.read(&mut read_buf) => {
                    let n_read = res?;
                    if n_read == 0 {
                        return Err(utils::WorkError::Disconnected);
                    }
                    packet_reader.push_data(&read_buf[..n_read]).map_err(|_| utils::WorkError::ReceivePacket)?;
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
                                error!("Received empty packet");
                                continue;
                            }
                            1 => (packet[0], None),
                            _ => (packet[0], Some(&packet[1..])),
                        };

                        let opcode = Opcode::try_from(opcode).map_err(|e| anyhow!(e))?;

                        let action = self.state_machine.handle_packet(opcode, body);
                        if self.handle_action(
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

                    if self.handle_command(
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
                }
            }
        }

        debug!("Shutting down...");

        // TODO: shutdown network stream?

        Ok(())
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

        crate::connection_loop!(
            reconnect_interval_millis,
            on_work = {
                self.inner_work(&addrs, &mut cmd_rx, cmd_tx.clone(), &txt_records)
                    .await
            },
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
        match state.command_tx.as_ref() {
            Some(cmd_tx) => {
                let _ = cmd_tx.send(cmd);
                Ok(())
            }
            None => {
                error!("Missing command tx");
                Err(CastingDeviceError::FailedToSendCommand)
            }
        }
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
            DeviceFeature::KeyEventSubscription
            | DeviceFeature::MediaEventSubscription
            | DeviceFeature::PlaylistNextAndPrevious
            | DeviceFeature::SetPlaylistItemIndex
            | DeviceFeature::LoadPlaylist => session_version == 3,
            DeviceFeature::WhepStreaming => self.supports_whep.load(Ordering::Relaxed),
            DeviceFeature::FCompanion
            | DeviceFeature::FWRTCSignalling
            | DeviceFeature::ChangeTrack
            | DeviceFeature::Queue => session_version == 4,
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

                self.send_command(Command::LoadQueue { items, start_index })
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

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        state.command_tx = Some(tx.clone());

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

        state.rt_handle.spawn(
            InnerDevice::new(
                app_info,
                event_handler,
                self.session_version.clone(),
                Arc::clone(&self.supports_whep),
                fingerprint,
            )
            .work(
                addrs,
                rx,
                tx,
                reconnect_interval_millis,
                state.txt_records.clone(),
            ),
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
            self.send_command(Command::QueueAdd { item, position })
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

#[cfg(test)]
mod tests {
    use super::*;

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
                companion_provider_id: None,
                mirroring_session: None,
                mirroring_session_id_gen: IdGenerator::new(),
            }
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
}
