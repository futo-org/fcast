use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures::StreamExt;
use googlecast_protocol::prost::Message;
use googlecast_protocol::{
    self as protocol, namespaces, protos, MediaInformation, PlayerState, QueueItem,
    QueueRepeatMode, StreamType, CONNECTION_NAMESPACE, HEARTBEAT_NAMESPACE, MEDIA_NAMESPACE,
    RECEIVER_NAMESPACE,
};
use log::{debug, error, warn};
use rustls_pki_types::ServerName;
use serde::Serialize;
use serde_json as json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{self, ClientConfig};
use tokio_rustls::TlsConnector;

use crate::device::{
    ApplicationInfo, CastingDevice, CastingDeviceError, DeviceConnectionState, DeviceEventHandler,
    DeviceFeature, DeviceInfo, EventSubscription, LoadRequest, MediaItem, Metadata, PlaybackState,
    PlaylistItem, ProtocolType, Source,
};
use crate::{googlecast_protocol, utils, IpAddr};

const DEFAULT_GET_STATUS_DELAY: Duration = Duration::from_secs(1);
const RECEIVER_APP_ID: &str = "CC1AD845";
const MAX_LAUNCH_RETRIES: u8 = 15;

struct RequestId(u64);

impl RequestId {
    pub fn new() -> Self {
        Self(0)
    }

    pub fn inc(&mut self) -> u64 {
        self.0 += 1;
        self.0 - 1
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

#[derive(Debug, PartialEq)]
enum Command {
    Quit,
    LoadUrl {
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    LoadPlaylist(Vec<PlaylistItem>),
    ChangeVolume(f64),
    ChangeSpeed(f64),
    Seek(f64),
    Stop,
    PausePlayback,
    ResumePlayback,
    JumpPlaylist(i32),
    Subscribe(EventSubscription),
    Unsubscribe(EventSubscription),
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct ChromecastDevice {
    state: Mutex<State>,
}

impl ChromecastDevice {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Mutex::new(State::new(device_info, rt_handle)),
        }
    }
}

#[derive(Debug)]
struct AllCertVerifier;

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

fn meta_to_gcast_meta(meta: Option<Metadata>) -> Option<protocol::Metadata> {
    meta.map(|meta| protocol::Metadata::Generic {
        title: meta.title,
        subtitle: None,
        images: meta.thumbnail_url.map(|url| vec![protocol::Image { url }]),
        release_date: None,
    })
}

struct SharedReceiverState {
    pub time: f64,
    pub duration: f64,
    pub volume: f64,
    pub speed: f64,
    pub playback_state: PlaybackState,
    pub source: Option<Source>,
    pub is_running: bool,
    pub remote_sockaddr: std::net::SocketAddr,
    pub stream_local_sockaddr: std::net::SocketAddr,
    pub media_item: Option<MediaItem>,
}

struct InnerDevice {
    write_buffer: Vec<u8>,
    cmd_rx: Receiver<Command>,
    event_handler: Arc<dyn DeviceEventHandler>,
    transport_id: Option<String>,
    writer: Option<tokio::io::WriteHalf<TlsStream<TcpStream>>>,
    request_id: RequestId,
    media_session_id: u64,
    current_player_state: PlayerState,
    session_id: String,
    launch_retries: u8,
    subscriptions: HashSet<EventSubscription>,
}

impl InnerDevice {
    pub fn new(cmd_rx: Receiver<Command>, event_handler: Arc<dyn DeviceEventHandler>) -> Self {
        Self {
            write_buffer: vec![0u8; 1000 * 64],
            cmd_rx,
            event_handler,
            transport_id: None,
            writer: None,
            request_id: RequestId::new(),
            media_session_id: 0,
            current_player_state: PlayerState::Idle,
            session_id: String::new(),
            launch_retries: 0,
            subscriptions: HashSet::new(),
        }
    }

    async fn send_channel_message<T>(
        &mut self,
        source_id: impl ToString,
        destination_id: impl ToString,
        obj: T,
    ) -> anyhow::Result<()>
    where
        T: Serialize + namespaces::Namespace + std::fmt::Debug,
    {
        let Some(writer) = self.writer.as_mut() else {
            bail!("`writer` is missing");
        };

        let cast_message = protos::CastMessage {
            protocol_version: protos::cast_message::ProtocolVersion::Castv210.into(),
            source_id: source_id.to_string(),
            destination_id: destination_id.to_string(),
            namespace: obj.name().to_owned(),
            payload_type: protos::cast_message::PayloadType::String.into(),
            payload_utf8: Some(json::to_string(&obj)?),
            payload_binary: None,
        };

        let encoded_len = cast_message.encoded_len();
        if encoded_len > self.write_buffer.len() {
            bail!(
                "Message exceeded maximum length: {encoded_len} > {}",
                self.write_buffer.len()
            );
        }
        cast_message.encode(&mut self.write_buffer[..encoded_len].as_mut())?;
        let serialized_size_be = (encoded_len as u32).to_be_bytes();
        writer.write_all(&serialized_size_be).await?;
        writer.write_all(&self.write_buffer[..encoded_len]).await?;

        debug!("Sent {encoded_len} bytes, payload: {obj:?}");

        Ok(())
    }

    async fn send_media_channel_message<T>(&mut self, obj: T) -> anyhow::Result<()>
    where
        T: Serialize + namespaces::Namespace + std::fmt::Debug,
    {
        match self.transport_id.as_ref() {
            Some(transport_id) => {
                self.send_channel_message("sender-0", transport_id.clone(), obj)
                    .await
            }
            None => {
                bail!("`transport_id` is missing")
            }
        }
    }

    async fn stop_playback(&mut self) -> anyhow::Result<()> {
        let request_id = self.request_id.inc();
        self.send_media_channel_message(namespaces::Media::Stop {
            media_session_id: self.media_session_id.to_string(),
            request_id,
        })
        .await
    }

    async fn stop_session(&mut self) -> anyhow::Result<()> {
        let request_id = self.request_id.inc();
        self.send_channel_message(
            "sender-0",
            "receiver-0",
            namespaces::Receiver::StopSession {
                session_id: self.session_id.clone(),
                request_id,
            },
        )
        .await
    }

    async fn launch_app(&mut self) -> anyhow::Result<()> {
        if self.launch_retries < MAX_LAUNCH_RETRIES {
            debug!("Trying to launch app ({})", self.launch_retries);
            self.launch_retries += 1;
            let request_id = self.request_id.inc();
            self.send_channel_message(
                "sender-0",
                "receiver-0",
                namespaces::Receiver::Launch {
                    app_id: RECEIVER_APP_ID.to_owned(),
                    request_id,
                },
            )
            .await
        } else {
            bail!("Launch retries exceeded MAX_LAUNCH_RETRIES ({MAX_LAUNCH_RETRIES})")
        }
    }

    async fn change_volume(&mut self, volume: f64) -> anyhow::Result<()> {
        let request_id = self.request_id.inc();
        self.send_channel_message(
            "sender-0",
            "receiver-0",
            namespaces::Receiver::SetVolume {
                request_id,
                volume: protocol::Volume {
                    level: Some(volume),
                    muted: None,
                },
            },
        )
        .await
    }

    /// Returns `true` if the device should quit.
    async fn handle_command(&mut self, cmd: Command) -> anyhow::Result<bool> {
        match cmd {
            Command::Quit => return Ok(true),
            Command::LoadUrl {
                content_type,
                url,
                resume_position,
                speed,
                metadata,
                volume,
                ..
            } => {
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::Load {
                    current_time: resume_position,
                    media: protocol::MediaInformation {
                        content_id: url,
                        stream_type: protocol::StreamType::None,
                        content_type,
                        duration: None,
                        metadata: meta_to_gcast_meta(metadata),
                    },
                    request_id,
                    auto_play: None,
                    playback_rate: speed,
                })
                .await?;
                if let Some(volume) = volume {
                    self.change_volume(volume).await?;
                }
            }
            Command::LoadPlaylist(items) => {
                let queue_items = items
                    .into_iter()
                    .map(|item| QueueItem {
                        autoplay: true,
                        media: MediaInformation {
                            content_id: item.content_location,
                            stream_type: StreamType::None,
                            content_type: item.content_type,
                            duration: None,
                            metadata: None,
                        },
                        playback_duration: i32::MAX,
                        start_time: 0.0,
                    })
                    .collect::<Vec<QueueItem>>();
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::QueueLoad {
                    request_id,
                    items: queue_items,
                    repeat_mode: QueueRepeatMode::All,
                    start_index: 0,
                    queue_type: Some("PLAYLIST".to_string()),
                })
                .await?;
            }
            Command::ChangeVolume(volume) => self.change_volume(volume).await?,
            Command::ChangeSpeed(speed) => {
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::SetPlaybackRate {
                    request_id,
                    media_session_id: self.media_session_id,
                    playback_rate: speed,
                })
                .await?;
            }
            Command::Seek(time_seconds) => {
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::Seek {
                    media_session_id: self.media_session_id.to_string(),
                    request_id,
                    current_time: Some(time_seconds),
                })
                .await?;
            }
            Command::Stop => self.stop_playback().await?,
            Command::PausePlayback => {
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::Pause {
                    media_session_id: self.media_session_id.to_string(),
                    request_id,
                })
                .await?;
            }
            Command::ResumePlayback => {
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::Resume {
                    media_session_id: self.media_session_id.to_string(),
                    request_id,
                })
                .await?;
            }
            Command::JumpPlaylist(jump) => {
                let request_id = self.request_id.inc();
                self.send_media_channel_message(namespaces::Media::QueueUpdate {
                    media_session_id: self.media_session_id.to_string(),
                    request_id,
                    jump: Some(jump),
                })
                .await?;
            }
            Command::Subscribe(subscription) => {
                let _ = self.subscriptions.insert(subscription);
            }
            Command::Unsubscribe(subscription) => {
                let _ = self.subscriptions.remove(&subscription);
            }
        }

        Ok(false)
    }

    /// Returns true if session was closed
    async fn handle_message(
        &mut self,
        shared_state: &mut SharedReceiverState,
        message: protos::CastMessage,
    ) -> Result<bool> {
        macro_rules! changed {
            ($param:ident, $new:expr, $fun:ident) => {
                if shared_state.$param != $new {
                    self.event_handler.$fun($new);
                    shared_state.$param = $new;
                }
            };
        }

        if message.payload_type() != protos::cast_message::PayloadType::String {
            return Err(anyhow!(
                "Payload type {:?} is not implemented",
                message.payload_type()
            )
            .into());
        }
        let json_payload = message.payload_utf8();
        match message.namespace.as_str() {
            HEARTBEAT_NAMESPACE => {
                let msg: namespaces::Heartbeat = json::from_str(json_payload)?;
                match msg {
                    namespaces::Heartbeat::Ping => {
                        self.send_channel_message(
                            "sender-0",
                            "receiver-0",
                            namespaces::Heartbeat::Pong,
                        )
                        .await?;
                    }
                    namespaces::Heartbeat::Pong => (),
                }
            }
            RECEIVER_NAMESPACE => {
                let msg: namespaces::Receiver = json::from_str(json_payload)?;
                match msg {
                    namespaces::Receiver::Status { status, .. } => {
                        debug!("Receiver status: {status:#?}");
                        let Some(applications) = status.applications else {
                            debug!("Got ReceiverStatus with no `applications` field");
                            if !shared_state.is_running {
                                self.launch_app().await?;
                            }
                            return Ok(false);
                        };
                        let mut new_is_running = false;
                        for application in applications {
                            if application.app_id == RECEIVER_APP_ID {
                                new_is_running = true;
                                if self.session_id.is_empty() {
                                    self.session_id = application.session_id;
                                    self.transport_id = Some(application.transport_id);

                                    self.send_media_channel_message(
                                        namespaces::Connection::Connect { conn_type: 0 },
                                    )
                                    .await?;

                                    debug!("Connected to media channel {:?}", self.transport_id);

                                    let request_id = self.request_id.inc();
                                    self.send_media_channel_message(namespaces::Media::GetStatus {
                                        media_session_id: None,
                                        request_id,
                                    })
                                    .await?;

                                    if !shared_state.is_running {
                                        self.event_handler.connection_state_changed(
                                            DeviceConnectionState::Connected {
                                                used_remote_addr: shared_state.remote_sockaddr.into(),
                                                local_addr: shared_state.stream_local_sockaddr.into(),
                                            },
                                        );
                                    }
                                }
                            }
                        }
                        shared_state.is_running = new_is_running;
                        if shared_state.is_running {
                            changed!(volume, status.volume.level, volume_changed);
                        } else {
                            self.launch_app().await?;
                        }
                    }
                    _ => debug!("Ignored receiver message: {msg:#?}"),
                }
            }
            MEDIA_NAMESPACE => {
                let msg = match json::from_str::<namespaces::Media>(json_payload) {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!("Failed to parse media message: {err}");
                        return Ok(false);
                    }
                };
                #[allow(clippy::single_match)]
                match msg {
                    namespaces::Media::Status { status, .. } => {
                        for stat in status {
                            self.media_session_id = stat.media_session_id;
                            if let Some(media) = stat.media {
                                if let Some(duration_update) = media.duration {
                                    changed!(duration, duration_update, duration_changed);
                                }
                                let new_source = Source::Url {
                                    url: media.content_id.clone(),
                                    content_type: media.content_type.clone(),
                                };
                                shared_state.media_item = Some(MediaItem {
                                    content_type: media.content_type,
                                    url: Some(media.content_id),
                                    content: None,
                                    time: None,
                                    volume: None,
                                    speed: None,
                                    show_duration: media.duration,
                                    metadata: media.metadata.map(|meta| match meta {
                                        googlecast_protocol::Metadata::Generic {
                                            title,
                                            images,
                                            ..
                                        } => crate::device::Metadata {
                                            title,
                                            thumbnail_url: images
                                                .map(|imgs| imgs.get(0).map(|img| img.url.clone()))
                                                .flatten(),
                                        },
                                    }),
                                });
                                if shared_state.source != Some(new_source.clone()) {
                                    self.event_handler.source_changed(new_source.clone());
                                    shared_state.source = Some(new_source);
                                }
                            }
                            debug!("New media_session_id: {}", self.media_session_id);
                            changed!(speed, stat.playback_rate, speed_changed);
                            changed!(time, stat.current_time, time_changed);
                            changed!(
                                playback_state,
                                match stat.player_state {
                                    protocol::PlayerState::Idle => PlaybackState::Idle,
                                    protocol::PlayerState::Buffering => PlaybackState::Buffering,
                                    protocol::PlayerState::Playing => PlaybackState::Playing,
                                    protocol::PlayerState::Paused => PlaybackState::Paused,
                                },
                                playback_state_changed
                            );
                            self.current_player_state = stat.player_state;
                            if let Some(idle_reason) = stat.idle_reason {
                                match idle_reason {
                                    googlecast_protocol::IdleReason::Finished
                                        if self
                                            .subscriptions
                                            .contains(&EventSubscription::MediaItemEnd) =>
                                    {
                                        if let Some(media_item) = shared_state.media_item.take() {
                                            self.event_handler.media_event(
                                                crate::device::MediaEvent {
                                                    type_: crate::device::MediaItemEventType::End,
                                                    item: media_item,
                                                },
                                            );
                                        }
                                    }
                                    _ => (),
                                }
                            }
                        }
                    }
                    namespaces::Media::Error {
                        reason: Some(error_reason),
                        ..
                    } => {
                        self.event_handler.playback_error(error_reason);
                    }
                    _ => (),
                }
            }
            CONNECTION_NAMESPACE => {
                let msg = match json::from_str::<namespaces::Connection>(json_payload) {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!("Failed to parse media message: {err}");
                        return Ok(false);
                    }
                };

                debug!("Connection message: {msg:#?}");

                if matches!(msg, namespaces::Connection::Close) {
                    debug!("Session closed");
                    return Ok(true);
                }
            }
            _ => warn!("Unsupported namespace: {}", message.namespace),
        }

        Ok(false)
    }

    async fn inner_work(&mut self, addrs: &[SocketAddr]) -> Result<(), utils::WorkError> {
        self.session_id.clear();
        self.media_session_id = 0;
        self.transport_id = None;
        self.current_player_state = PlayerState::Idle;
        self.launch_retries = 0;
        self.writer = None;
        self.request_id = RequestId::new();

        let Some(stream) =
            utils::try_connect_tcp(addrs, Duration::from_secs(5), &mut self.cmd_rx, |cmd| {
                cmd == Command::Quit
            })
            .await
            .map_err(|err| utils::WorkError::DidNotConnect(err.to_string()))?
        else {
            debug!("Received Quit command in connect loop");
            return Ok(());
        };

        let remote_sockaddr = stream.peer_addr()?;
        let stream_local_sockaddr = stream.local_addr()?;
        let remote_addr = remote_sockaddr.ip();

        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AllCertVerifier))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let dnsname = ServerName::from(remote_addr);
        let stream = connector.connect(dnsname, stream).await?;

        debug!("Connected to {remote_addr:?}");

        let (reader, writer) = tokio::io::split(stream);
        self.writer = Some(writer);

        self.send_channel_message(
            "sender-0",
            "receiver-0",
            namespaces::Connection::Connect { conn_type: 0 },
        )
        .await?;

        let packet_stream = futures::stream::unfold(
            (reader, vec![0u8; 1000 * 64]),
            |(mut reader, mut body_buf)| async move {
                async fn read_packet(
                    reader: &mut tokio::io::ReadHalf<TlsStream<TcpStream>>,
                    body_buf: &mut [u8],
                ) -> anyhow::Result<protos::CastMessage> {
                    let mut size_buf = [0u8; 4];
                    reader.read_exact(&mut size_buf).await?;
                    let size = u32::from_be_bytes(size_buf) as usize;

                    if size > body_buf.len() {
                        bail!(
                            "Packet size ({size}) exceeded the maximum ({})",
                            body_buf.len()
                        );
                    }

                    reader.read_exact(&mut body_buf[..size]).await?;

                    debug!("Received {size} bytes");

                    let msg = protos::CastMessage::decode(&body_buf[..size])?;

                    Ok(msg)
                }

                match read_packet(&mut reader, &mut body_buf).await {
                    Ok(body) => {
                        debug!("Received packet, body: {body:#?}");
                        Some((body, (reader, body_buf)))
                    }
                    Err(err) => {
                        error!("Error occurred while reading packet: {err}");
                        None
                    }
                }
            },
        );

        tokio::pin!(packet_stream);

        let mut shared_state = SharedReceiverState {
            time: 0.0,
            duration: 0.0,
            volume: 0.0,
            speed: 0.0,
            playback_state: PlaybackState::Idle,
            source: None,
            is_running: false,
            remote_sockaddr,
            stream_local_sockaddr,
            media_item: None,
        };

        let mut get_status_interval = tokio::time::interval(DEFAULT_GET_STATUS_DELAY);

        loop {
            tokio::select! {
                packet = packet_stream.next() => {
                    let message = packet.ok_or(anyhow!("No more packets"))?;
                    if self.handle_message(&mut shared_state, message).await? {
                        break;
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("Failed to receive command"))?;
                    if self.handle_command(cmd).await? {
                        break;
                    }
                }
                _ = get_status_interval.tick() => {
                    if !shared_state.is_running {
                        debug!("Requesting receiver status");
                        let request_id = self.request_id.inc();
                        self.send_channel_message(
                            "sender-0",
                            "receiver-0",
                            namespaces::Receiver::GetStatus {
                                request_id,
                            },
                        )
                        .await?;
                    } else if self.media_session_id != 0 && self.current_player_state == PlayerState::Playing {
                        debug!("Requesting media status");
                        let request_id = self.request_id.inc();
                        self.send_media_channel_message(
                            namespaces::Media::GetStatus {
                                request_id,
                                media_session_id: Some(self.media_session_id),
                            },
                        )
                        .await?;
                    }
                }
            }
        }

        debug!("Shutting down...");

        self.stop_session().await?;

        if let Some(mut writer) = self.writer.take() {
            writer.shutdown().await?;
        }

        Ok(())
    }

    pub async fn work(mut self, addrs: Vec<SocketAddr>, reconnect_interval_millis: u64) {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        crate::connection_loop!(
            reconnect_interval_millis,
            on_work = { self.inner_work(&addrs).await },
            on_reconnect_started = {
                self.event_handler
                    .connection_state_changed(DeviceConnectionState::Reconnecting);
            }
        );

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Disconnected);
    }
}

impl ChromecastDevice {
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
    ) -> std::result::Result<(), CastingDeviceError> {
        self.send_command(Command::LoadUrl {
            content_type,
            url,
            resume_position,
            speed,
            volume,
            metadata,
            request_headers,
        })
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastingDevice for ChromecastDevice {
    fn casting_protocol(&self) -> ProtocolType {
        ProtocolType::Chromecast
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.addresses.is_empty() && state.port > 0 && !state.name.is_empty()
    }

    fn supports_feature(&self, feature: DeviceFeature) -> bool {
        match feature {
            DeviceFeature::SetVolume
            | DeviceFeature::SetSpeed
            | DeviceFeature::LoadUrl
            | DeviceFeature::LoadImage
            | DeviceFeature::LoadPlaylist
            | DeviceFeature::PlaylistNextAndPrevious
            | DeviceFeature::MediaEventSubscription => true,
            _ => false,
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
        self.send_command(Command::Seek(time_seconds))
    }

    fn stop_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::Stop)
    }

    fn pause_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::PausePlayback)
    }

    fn resume_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ResumePlayback)
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
            } => self.send_command(Command::LoadUrl {
                content_type,
                url,
                resume_position,
                speed,
                volume,
                metadata,
                request_headers,
            }),
            LoadRequest::Content { .. } => Err(CastingDeviceError::UnsupportedFeature),
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
            } => self.load_url(
                content_type,
                url,
                None,
                None,
                None,
                metadata,
                request_headers,
            ),
            LoadRequest::Playlist { items } => self.send_command(Command::LoadPlaylist(items)),
        }
    }

    fn playlist_item_next(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::JumpPlaylist(1))
    }

    fn playlist_item_previous(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::JumpPlaylist(-1))
    }

    fn set_playlist_item_index(&self, _index: u32) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedFeature)
    }

    fn change_volume(&self, volume: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ChangeVolume(volume))
    }

    fn change_speed(&self, speed: f64) -> Result<(), CastingDeviceError> {
        // It seems this is the valid range for playback speed
        let speed = speed.clamp(0.5, 2.0);
        self.send_command(Command::ChangeSpeed(speed))
    }

    fn disconnect(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::Quit)?;
        let mut state = self.state.lock().unwrap();
        state.command_tx = None;
        state.started = false;
        Ok(())
    }

    fn connect(
        &self,
        _app_info: Option<ApplicationInfo>,
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
        state.command_tx = Some(tx);

        state
            .rt_handle
            .spawn(InnerDevice::new(rx, event_handler).work(addrs, reconnect_interval_millis));

        Ok(())
    }

    fn get_device_info(&self) -> DeviceInfo {
        let state = self.state.lock().unwrap();
        DeviceInfo {
            name: state.name.clone(),
            protocol: ProtocolType::Chromecast,
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

    #[allow(unused_variables)]
    fn subscribe_event(&self, group: EventSubscription) -> Result<(), CastingDeviceError> {
        match group {
            EventSubscription::MediaItemEnd => self.send_command(Command::Subscribe(group)),
            _ => Err(CastingDeviceError::UnsupportedSubscription),
        }
    }

    #[allow(unused_variables)]
    fn unsubscribe_event(&self, group: EventSubscription) -> Result<(), CastingDeviceError> {
        self.send_command(Command::Unsubscribe(group))
    }
}
