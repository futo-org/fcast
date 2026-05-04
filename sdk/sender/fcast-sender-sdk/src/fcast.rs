use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use fcast_protocol::{
    companion, v2,
    v3::{
        self, AVCapabilities, InitialReceiverMessage, LivestreamCapabilities, MetadataObject,
        ReceiverCapabilities, SetPlaylistItemMessage,
    },
    v4, Opcode, PlaybackErrorMessage, PlaybackState as FCastPlaybackState, SeekMessage,
    SetSpeedMessage, SetVolumeMessage, VersionMessage,
};
use log::{debug, error};
use rustls_pki_types::ServerName;
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};
use tokio_rustls::{client::TlsStream, TlsConnector};

use crate::{
    device::{
        ApplicationInfo, CastingDevice, CastingDeviceError, CompanionSource,
        CompanionSourceDescriptor, DeviceConnectionState, DeviceEventHandler, DeviceFeature,
        DeviceInfo, EventSubscription, KeyEvent, KeyName, LoadRequest, MediaEvent, MediaItem,
        MediaItemEventType, Metadata, PlaybackState, PlaylistItem, ProtocolType, Source,
    },
    utils, IpAddr,
};

const DEFAULT_SESSION_VERSION: u64 = 2;
const EVENT_SUB_MIN_PROTO_VERSION: u64 = 3;
const PLAYLIST_MIN_PROTO_VERSION: u64 = 3;
const V3_FEATURES_MIN_PROTO_VERSION: u64 = 3;

const CONNECTED_EVENT_DEADLINE_DURATION: Duration = Duration::from_secs(2);

// #[derive(Debug, Clone, PartialEq)]
#[derive(Debug, PartialEq)]
enum LoadType {
    Url { url: String },
    Content { content: String },
    // CompanionResource { id: u32 },
    CompanionResource { source: CompanionSource },
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
enum StateVariant {
    Connecting,
    V2,
    V3,
    V4WaitingForTLS,
    V4 { companion_provider_id: Option<u16> },
}

#[derive(Debug, PartialEq, Eq)]
enum QuitReason {
    InvalidBody,
    InvalidVersion,
    MissingBody,
    UnsupportedOpcode,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum VersionCode {
    V2,
    V3,
    V4,
}

#[derive(Debug, PartialEq)]
enum CompanionRequest {
    ResourceInfo(companion::ResourceInfoRequest),
    Resource(companion::ResourceRequest),
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
    StartTLS,
    UpgradeToTLS,
    PositionChanged(f64),
    DurationChanged(f64),
    VolumeChanged(f64),
    PlaybackStateChanged(v4::PlaybackState),
    Companion(CompanionRequest),
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
}

impl DeviceStateMachine {
    fn new() -> Self {
        Self {
            variant: StateVariant::Connecting,
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
                        self.variant = StateVariant::V4WaitingForTLS;
                        Action::StartTLS
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

    fn handle_packet_v4(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Action {
        match opcode {
            Opcode::None => todo!(),

            Opcode::Play
            | Opcode::Pause
            | Opcode::Resume
            | Opcode::Stop
            | Opcode::PlayUpdate
            | Opcode::SetPlaylistItem
            | Opcode::Version => Action::Quit(QuitReason::UnsupportedOpcode),

            Opcode::Seek => todo!(),
            Opcode::VolumeUpdate => {
                Action::VolumeChanged(json_from_body!(v4::UpdateVolumeMessage, body!(body)).volume)
            }
            Opcode::PlaybackError => todo!(),
            Opcode::SetSpeed => todo!(),
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            Opcode::Initial => {
                Action::Initial(json_from_body!(InitialReceiverMessage, body!(body)))
            }
            Opcode::SubscribeEvent => todo!(),
            Opcode::UnsubscribeEvent => todo!(),
            Opcode::Event => todo!(),
            Opcode::UpdatePlaybackState => Action::PlaybackStateChanged(
                json_from_body!(v4::UpdatePlaybackStateMessage, body!(body)).state,
            ),
            Opcode::PositionChanged => Action::PositionChanged(
                json_from_body!(v4::PositionChangedMessage, body!(body)).position,
            ),
            Opcode::DurationChanged => Action::DurationChanged(
                json_from_body!(v4::DurationChangedMessage, body!(body)).duration,
            ),
            Opcode::QueueInsert => todo!(),
            Opcode::QueueRemove => todo!(),
            Opcode::TracksAvailable => todo!(),
            Opcode::ChangeTrack => todo!(),
            Opcode::QueueItemSelected => todo!(),
            Opcode::AddSubtitleSource => todo!(),
            Opcode::SetStatusUpdateInterval => todo!(),
            Opcode::CompanionHello => {
                let Some(body) = body else {
                    return Action::Quit(QuitReason::MissingBody);
                };
                let response = match companion::HelloResponse::parse(body) {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!("Failed to parse companion hello response: {err}");
                        return Action::Quit(QuitReason::InvalidBody);
                    }
                };

                if let StateVariant::V4 {
                    companion_provider_id,
                } = &mut self.variant
                {
                    debug!("Got companion provider ID ({})", response.provider_id);
                    *companion_provider_id = Some(response.provider_id);
                }

                Action::None
            }
            Opcode::ResourceInfo => {
                let Some(body) = body else {
                    return Action::Quit(QuitReason::MissingBody);
                };
                let request = companion::ResourceInfoRequest::parse(body).unwrap(); // TODO: handle error
                Action::Companion(CompanionRequest::ResourceInfo(request))
            }
            Opcode::Resource => {
                let Some(body) = body else {
                    return Action::Quit(QuitReason::MissingBody);
                };
                let request = companion::ResourceRequest::parse(body).unwrap(); // TODO: handle error
                Action::Companion(CompanionRequest::Resource(request))
            }
            _ => todo!(),
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
            StateVariant::V4WaitingForTLS => match opcode {
                // Opcode::StartTLS => Action::UpgradeToTLS,
                Opcode::StartTLS => {
                    self.variant = StateVariant::V4 {
                        companion_provider_id: None,
                    };
                    Action::UpgradeToTLS
                }
                _ => todo!(),
            },
            StateVariant::V4 { .. } => self.handle_packet_v4(opcode, body),
        }
    }
}

#[derive(Debug)]
struct AllCertVerifier;

use tokio_rustls::rustls;

// TODO: accept spki hash thing
impl rustls::client::danger::ServerCertVerifier for AllCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA1,
            rustls::SignatureScheme::ECDSA_SHA1_Legacy,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::ED448,
        ]
    }
}

#[derive(Default)]
enum NetworkStream {
    #[default]
    None,
    Tcp {
        peer_addr: std::net::SocketAddr,
        rx: tokio::io::ReadHalf<TcpStream>,
        tx: tokio::io::BufWriter<tokio::io::WriteHalf<TcpStream>>,
    },
    Tls {
        tx: tokio::io::BufWriter<tokio::io::WriteHalf<TlsStream<TcpStream>>>,
        rx: tokio::io::ReadHalf<TlsStream<TcpStream>>,
    },
}

impl NetworkStream {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        let peer_addr = stream.peer_addr()?;

        let (rx, tx) = tokio::io::split(stream);
        let tx = tokio::io::BufWriter::new(tx);

        Ok(Self::Tcp { peer_addr, rx, tx })
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp { rx, .. } => rx.read(buf).await,
            Self::Tls { rx, .. } => rx.read(buf).await,
            Self::None => unreachable!(),
        }
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { tx, .. } => tx.write_all(buf).await,
            Self::Tls { tx, .. } => tx.write_all(buf).await,
            Self::None => unreachable!(),
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp { tx, .. } => tx.flush().await?,
            Self::Tls { tx, .. } => tx.flush().await?,
            _ => (),
        }

        Ok(())
    }

    pub async fn upgrade(
        &mut self,
        connector: &TlsConnector,
        server_name: ServerName<'static>,
    ) -> io::Result<()> {
        let old = std::mem::take(self);
        *self = match old {
            Self::Tcp { tx, rx, .. } => {
                let tx = tx.into_inner();
                let stream = rx.unsplit(tx);

                let tls_stream = connector.connect(server_name, stream).await?;
                let (rx, tx) = tokio::io::split(tls_stream);
                let tx = tokio::io::BufWriter::with_capacity(1024 * 16, tx);
                Self::Tls { tx, rx }
            }
            _ => old,
        };

        Ok(())
    }
}

struct WrappedCompanionSource {
    file: tokio::fs::File,
    content_type: String,
}

struct InnerDevice {
    event_handler: Arc<dyn DeviceEventHandler>,
    // writer: Option<tokio::net::tcp::OwnedWriteHalf>,
    // stream: Option<NetworkStream>,
    stream: NetworkStream,
    session_version: FCastVersion,
    app_info: Option<ApplicationInfo>,
    supports_whep: Arc<AtomicBool>,
    state_machine: DeviceStateMachine,
    // companion_sources: HashMap<u32, CompanionSource>,
    companion_sources: HashMap<u32, WrappedCompanionSource>,
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
            // writer: None,
            // stream: None,
            stream: NetworkStream::None,
            session_version,
            app_info,
            supports_whep,
            state_machine: DeviceStateMachine::new(),
            companion_sources: HashMap::new(),
        }
    }

    async fn add_source(&mut self, source: CompanionSource) -> std::io::Result<u32> {
        let file = match source.descriptor {
            CompanionSourceDescriptor::Path(ref path) => tokio::fs::File::open(path).await?,
        };

        let source = WrappedCompanionSource {
            file,
            content_type: source.content_type,
        };

        let mut id = 0;
        while self.companion_sources.contains_key(&id) {
            id += 1;
        }
        self.companion_sources.insert(id, source);
        Ok(id)
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

    async fn send_companion(&mut self, op: Opcode, body: &[u8]) -> anyhow::Result<()> {
        let size = 1 + body.len();
        let mut header = [0u8; HEADER_LENGTH];
        header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
        header[HEADER_LENGTH - 1] = op as u8;

        // if header.len() + body.len() < self.scratch_buffer.len() {
        //     self.scratch_buffer[0..header.len()].copy_from_slice(&header);
        //     self.scratch_buffer[header.len()..header.len() + body.len()].copy_from_slice(body);
        //     self.stream.write_all(&self.scratch_buffer[0..header.len() + body.len()]).await?;
        // } else {
        //     self.stream.write_all(&header).await?;
        //     self.stream.write_all(&body).await?;
        // }

        self.stream.write_all(&header).await?;
        self.stream.write_all(&body).await?;
        self.stream.flush().await?;

        // self.stream.write_all(&header).await?;
        // self.stream.write_all(body).await?;

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
                let msg = v4::PlayMessage::Single {
                    media_item: v4::MediaItem {
                        container: content_type.clone(),
                        source_url: match type_ {
                            LoadType::Url { url } => url.clone(),
                            LoadType::Content { .. } => unreachable!(),
                            LoadType::CompanionResource { source } => {
                                if let StateVariant::V4 {
                                    companion_provider_id,
                                } = self.state_machine.variant
                                {
                                    if let Ok(resource_id) = self.add_source(source).await {
                                        companion::create_url(
                                            companion_provider_id.unwrap(),
                                            resource_id,
                                        )
                                    } else {
                                        todo!()
                                    }
                                } else {
                                    todo!("Receiver does not support FCompanion");
                                }
                            }
                        },
                        start_time: Some(resume_position),
                        volume,
                        speed,
                        headers: None,
                        title: None,
                        thumbnail_url: None,
                        metadata: None,
                        extra_metadata: None,
                        status_update_interval: 500,
                    },
                };
                self.send(Opcode::Play, msg).await?;
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

    /// Returns `true` if the main loop should be quit.
    async fn handle_action(
        &mut self,
        shared_state: &mut SharedState,
        has_emitted_connected_event: &mut bool,
        current_playlist_item_index: &mut Option<usize>,
        used_remote_addr: &IpAddr,
        local_addr: &IpAddr,
        action: Action,
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
                VersionCode::V4 => todo!(),
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
            Action::StartTLS => {
                self.send_empty(Opcode::StartTLS).await?;
            }
            Action::UpgradeToTLS => {
                let config = rustls::ClientConfig::builder_with_protocol_versions(&[
                    &rustls::version::TLS13,
                ])
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AllCertVerifier))
                .with_no_client_auth();
                let connector = TlsConnector::from(Arc::new(config));
                // let dnsname = ServerName::from(remote_addr);
                let dnsname = rustls_pki_types::ServerName::from(match &self.stream {
                    // NetworkStream::Tcp(stream) => stream.peer_addr().unwrap().ip(),
                    NetworkStream::Tcp { peer_addr, .. } => peer_addr.ip(),
                    _ => unreachable!(),
                });
                debug!("Upgrading network stream to use TLS");
                self.stream.upgrade(&connector, dnsname).await?;
                debug!("Upgraded successfully");

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
                                concat!("FCast Sender SDK v", env!("CARGO_PKG_VERSION")).to_owned(),
                            ),
                            app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                        },
                    },
                )
                .await
                .context("Failed to send InitialSenderMessage")?;

                // TODO: here?
                self.session_version.set(4);

                self.send_empty(Opcode::CompanionHello).await?;
            }
            Action::PositionChanged(pos) => {
                self.event_handler.time_changed(pos);
                shared_state.time = pos;
            }
            Action::DurationChanged(dur) => {
                self.event_handler.duration_changed(dur);
                shared_state.duration = dur;
            }
            Action::VolumeChanged(vol) => {
                self.event_handler.volume_changed(vol);
                shared_state.volume = vol;
            }
            Action::PlaybackStateChanged(state) => {
                let state = match state {
                    v4::PlaybackState::Idle => PlaybackState::Idle,
                    v4::PlaybackState::Buffering => PlaybackState::Buffering,
                    v4::PlaybackState::Playing => PlaybackState::Playing,
                    v4::PlaybackState::Paused => PlaybackState::Paused,
                    v4::PlaybackState::Ended => PlaybackState::Idle, // TODO
                };
                self.event_handler.playback_state_changed(state);
            }
            Action::Companion(request) => {
                match request {
                    CompanionRequest::ResourceInfo(request) => {
                        if let Some(source) = self.companion_sources.get(&request.resource_id) {
                            if let Ok(meta) = source.file.metadata().await {
                                let len = meta.len();
                                let response = companion::ResourceInfoResponse {
                                    request_id: request.request_id,
                                    content_type: source.content_type.clone().into(),
                                    resource_size: companion::ResourceSize::Known(len.into()),
                                };
                                let body = response.serialize();
                                self.send_companion(Opcode::ResourceInfo, &body).await?;
                            }
                        }
                        // TODO: send failed?
                    }
                    CompanionRequest::Resource(request) => {
                        if let Some(source) = self.companion_sources.get_mut(&request.resource_id) {
                            let meta = source.file.metadata().await?;
                            let file_len = meta.len();
                            let (start, stop_inclusive): (u64, u64) = match request.read_head {
                                companion::ReadHead::Whole => (0, file_len - 1),
                                companion::ReadHead::Range {
                                    start,
                                    stop_inclusive,
                                } => {
                                    let stop_inclusive = file_len.min(*stop_inclusive);
                                    (*start, stop_inclusive)
                                }
                            };

                            source
                                .file
                                .seek(std::io::SeekFrom::Start(start))
                                .await?;

                            let mut bytes_to_read = stop_inclusive - start;

                            let response_header =
                                companion::ResourceResponse::header_success(request.request_id);
                            let size = 1 + response_header.len() + bytes_to_read as usize;
                            let mut header = [0u8; HEADER_LENGTH];
                            header[..HEADER_LENGTH - 1]
                                .copy_from_slice(&(size as u32).to_le_bytes());
                            header[HEADER_LENGTH - 1] = Opcode::Resource as u8;
                            self.stream.write_all(&header).await?;
                            self.stream.write_all(&response_header).await?;

                            while bytes_to_read > 0 {
                                let mut buf = [0u8; 1024 * 8];
                                let mut n_read = source.file.read(&mut buf).await?;
                                if n_read == 0 {
                                    break;
                                }
                                if n_read as u64 > bytes_to_read {
                                    n_read = bytes_to_read as usize;
                                }

                                self.stream.write_all(&buf[0..n_read]).await?;
                                if (n_read as u64) < bytes_to_read {
                                    bytes_to_read -= n_read as u64;
                                } else {
                                    break;
                                }
                            }

                            self.stream.flush().await?;
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    async fn set_playback_state(&mut self, state: v4::PlaybackState) -> anyhow::Result<()> {
        self.send(
            Opcode::UpdatePlaybackState,
            v4::UpdatePlaybackStateMessage { state },
        )
        .await
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

        tokio::spawn(async move {
            tokio::time::sleep(CONNECTED_EVENT_DEADLINE_DURATION).await;
            let _ = cmd_tx.send(Command::ConnectedEventDeadlineElapsed);
        });

        // let (mut reader, writer) = stream.into_split();
        // self.writer = Some(writer);
        // self.stream = NetworkStream::Tcp(stream);
        self.stream = NetworkStream::new(stream)?;
        let mut shared_state = SharedState::default();
        let mut playlist_length = None::<usize>;
        let mut current_playlist_item_index = None::<usize>;
        // let mut state_machine = DeviceStateMachine::new();
        self.state_machine = DeviceStateMachine::new();
        let mut read_buf = [0u8; 1024 * 8];
        const MAX_BODY_SIZE: usize = 1000 * 32;
        let mut packet_reader = fcast_protocol::PacketReader::new(MAX_BODY_SIZE);

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
                    packet_reader.push_data(&read_buf[..n_read]);
                    while let Some(packet) = packet_reader.get_packet() {
                        let (opcode, body) = match packet.len() {
                            0 => {
                                error!("Received empty packet");
                                continue;
                            }
                            1 => (packet[0], None),
                            _ => (packet[0], Some(&packet[1..])),
                        };

                        let opcode = Opcode::try_from(opcode).map_err(|e| anyhow!(e))?;

                        if let Some(body) = body.as_ref() {
                            if body.len() > MAX_BODY_SIZE {
                                return Err(anyhow!("Message exceeded maximum length ({} > {MAX_BODY_SIZE})", body.len()).into());
                            }
                        }

                        if opcode != Opcode::Ping {
                            // debug!("Received packet opcode={opcode:?} body={:?}", body.map(|b| str::from_utf8(b)));
                            debug!("Received packet opcode={opcode:?}");
                        }

                        // let action = state_machine.handle_packet(opcode, body);
                        let action = self.state_machine.handle_packet(opcode, body);
                        if self.handle_action(
                            &mut shared_state,
                            &mut has_emitted_connected_event,
                            &mut current_playlist_item_index,
                            &used_remote_addr,
                            &local_addr,
                            action
                        ).await? {
                            break 'main_loop;
                        }
                    }
                }
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;

                    debug!("Received command: {cmd:?}");

                    // TODO: move into own function
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
                            match self.state_machine.variant {
                                StateVariant::V2 | StateVariant::V3 => self.send_empty(Opcode::Stop).await?,
                                StateVariant::V4 { .. } => self.set_playback_state(v4::PlaybackState::Idle).await?,
                                _ => (),
                            }

                            self.event_handler.playback_state_changed(PlaybackState::Idle);
                            self.companion_sources.clear();
                        }
                        Command::PauseVideo => match self.state_machine.variant {
                            StateVariant::V2
                            | StateVariant::V3 => self.send_empty(Opcode::Pause).await?,
                            StateVariant::V4 { .. } => self.set_playback_state(v4::PlaybackState::Paused).await?,
                            _ => (),
                        },
                        Command::ResumeVideo => match self.state_machine.variant {
                            StateVariant::V2
                            | StateVariant::V3 => self.send_empty(Opcode::Resume).await?,
                            StateVariant::V4 { .. } => self.set_playback_state(v4::PlaybackState::Playing).await?,
                            _ => (),
                        },
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

        // TODO
        // if let Some(mut writer) = self.writer.take() {
        //     writer.shutdown().await?;
        // }

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
            DeviceFeature::FCompanion => session_version == 4,
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

        state.rt_handle.spawn(
            InnerDevice::new(
                app_info,
                event_handler,
                self.session_version.clone(),
                Arc::clone(&self.supports_whep),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_with_version(version: VersionCode) -> DeviceStateMachine {
        let mut state_machine = DeviceStateMachine::new();
        let body = match version {
            VersionCode::V2 => br#"{"version":2}"#,
            VersionCode::V3 => br#"{"version":3}"#,
            VersionCode::V4 => br#"{"version":4}"#,
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
                VersionCode::V4 => todo!(),
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
        let mut state_machine = DeviceStateMachine::new();
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
        let mut state_machine = DeviceStateMachine::new();
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
}
