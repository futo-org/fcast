use crate::{
    casting_device::{
        CastConnectionState, CastProtocolType, CastingDevice, CastingDeviceError,
        CastingDeviceEventHandler, CastingDeviceInfo, GenericEventSubscriptionGroup, PlaybackState,
        Source,
    },
    IpAddr,
};
use anyhow::{anyhow, bail, Result};
use chromecast_protocol::{self as protocol, prost::Message, protos};
use chromecast_protocol::{namespaces, HEARTBEAT_NAMESPACE, MEDIA_NAMESPACE, RECEIVER_NAMESPACE};
use futures::StreamExt;
use log::{debug, error, info, warn};
use rustls_pki_types::ServerName;
use serde::Serialize;
use serde_json as json;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::mpsc::{Receiver, Sender},
};
use tokio_rustls::{
    client::TlsStream,
    rustls::{self, ClientConfig, RootCertStore},
    TlsConnector,
};

struct RequestId {
    inner: u64,
}

impl RequestId {
    pub fn new() -> Self {
        Self { inner: 0 }
    }

    pub fn inc(&mut self) -> u64 {
        self.inner += 1;
        self.inner - 1
    }
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
    pub fn new(
        device_info: CastingDeviceInfo,
        rt_handle: Handle,
        // ) -> Result<Self, AsyncRuntimeError> {
    ) -> Self {
        Self {
            // runtime: AsyncRuntime::new(Some(1), "chromecast-async-runtime")?,
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
        resume_position: Option<f64>,
        speed: Option<f64>,
    },
    ChangeVolume(f64),
    ChangeSpeed(f64),
    Seek(f64),
    Stop,
    PausePlayback,
    ResumePlayback,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct ChromecastCastingDevice {
    state: Mutex<State>,
}

// #[cfg_attr(feature = "uniffi", uniffi::export)]
impl ChromecastCastingDevice {
    // #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    // pub fn new(device_info: CastingDeviceInfo) -> Result<Self, AsyncRuntimeError> {
    pub fn new(device_info: CastingDeviceInfo, rt_handle: Handle) -> Self {
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

struct InnerDevice {
    write_buffer: Vec<u8>,
    cmd_rx: Receiver<Command>,
    event_handler: Arc<dyn CastingDeviceEventHandler>,
}

impl InnerDevice {
    pub fn new(
        cmd_rx: Receiver<Command>,
        event_handler: Arc<dyn CastingDeviceEventHandler>,
    ) -> Self {
        Self {
            write_buffer: vec![0u8; 1000 * 64],
            cmd_rx,
            event_handler,
        }
    }

    async fn send_channel_message<T>(
        &mut self,
        writer: &mut tokio::io::WriteHalf<TlsStream<TcpStream>>,
        source_id: impl ToString,
        destination_id: impl ToString,
        obj: T,
    ) -> anyhow::Result<()>
    where
        T: Serialize + namespaces::Namespace + std::fmt::Debug,
    {
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

    async fn inner_work(&mut self, addrs: Vec<SocketAddr>) -> anyhow::Result<()> {
        self.event_handler
            .connection_state_changed(CastConnectionState::Connecting);

        let Some(stream) =
            crate::try_connect_tcp(addrs, 5, &mut self.cmd_rx, |cmd| cmd == Command::Quit).await?
        else {
            debug!("Received Quit command in connect loop");
            self.event_handler
                .connection_state_changed(CastConnectionState::Disconnected);
            return Ok(());
        };

        let remote_addr = stream.peer_addr()?.ip();

        self.event_handler
            .connection_state_changed(CastConnectionState::Connected {
                used_remote_addr: remote_addr.into(),
                local_addr: stream.local_addr()?.ip().into(),
            });

        let mut root_cert_store = RootCertStore::empty();
        root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .dangerous()
            // TODO: hack or required?
            .with_custom_certificate_verifier(Arc::new(AllCertVerifier))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let dnsname = ServerName::from(remote_addr);
        let stream = connector.connect(dnsname, stream).await?;

        debug!("Connected to {remote_addr:?}");

        let (reader, mut writer) = tokio::io::split(stream);

        let mut request_id = RequestId::new();

        self.send_channel_message(
            &mut writer,
            "sender-0",
            "receiver-0",
            namespaces::Connection::Connect { conn_type: 0 },
        )
        .await?;

        self.send_channel_message(
            &mut writer,
            "sender-0",
            "receiver-0",
            namespaces::Receiver::GetStatus {
                request_id: request_id.inc(),
            },
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
                        debug!("Received packet, body: {body:?}");
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
        let mut is_running = false;
        // TODO: maybe Option<>s?
        let mut session_id = String::new();
        let mut transport_id = String::new();
        let mut media_session_id = 0u64;

        macro_rules! changed {
            ($param:ident, $new:expr, $fun:ident) => {
                if shared_state.$param != $new {
                    self.event_handler.$fun($new);
                    shared_state.$param = $new;
                }
            };
        }

        loop {
            tokio::select! {
                packet = packet_stream.next() => {
                    let packet = packet.ok_or(anyhow!("No more packets"))?;
                    if packet.payload_type() != protos::cast_message::PayloadType::String {
                        bail!("Payload type {:?} is not implemented", packet.payload_type());
                    }
                    let json_payload = packet.payload_utf8();
                    match packet.namespace.as_str() {
                        HEARTBEAT_NAMESPACE => {
                            let msg: namespaces::Heartbeat = json::from_str(json_payload)?;
                            match msg {
                                namespaces::Heartbeat::Ping => {
                                    self.send_channel_message(
                                        &mut writer,
                                        "sender-0",
                                        "receiver-0",
                                        namespaces::Heartbeat::Pong
                                    ).await?;
                                }
                                namespaces::Heartbeat::Pong => (),
                            }
                        }
                        RECEIVER_NAMESPACE => {
                            let msg: namespaces::Receiver = json::from_str(json_payload)?;
                            match msg {
                                namespaces::Receiver::Status { status, .. } => {
                                    let Some(applications) = status.applications else {
                                        debug!("Got ReceiverStatus with no `applications` field");
                                        continue;
                                    };
                                    let mut new_is_running = false;
                                    for application in applications {
                                        if &application.app_id == "CC1AD845" {
                                            new_is_running = true;
                                            if session_id.is_empty() {
                                                session_id = application.session_id;
                                                transport_id = application.transport_id;

                                                self.send_channel_message(
                                                    &mut writer,
                                                    "sender-0",
                                                    transport_id.clone(),
                                                    namespaces::Connection::Connect { conn_type: 0 }
                                                ).await?;

                                                debug!("Connected to media channel {transport_id}");

                                                self.send_channel_message(
                                                    &mut writer,
                                                    "sender-0",
                                                    transport_id.clone(),
                                                    namespaces::Media::GetStatus {
                                                        media_session_id: None,
                                                        request_id: request_id.inc()
                                                    }
                                                ).await?;
                                            }
                                        }
                                    }
                                    // Relaunch the app if it was terminated due to e.g. inactivity
                                    is_running = new_is_running;
                                    if !is_running {
                                        self.send_channel_message(
                                            &mut writer,
                                            "sender-0",
                                            "receiver-0",
                                            namespaces::Receiver::Launch {
                                                app_id: "CC1AD845".to_owned(),
                                                request_id: request_id.inc(),
                                            }).await?;
                                    }
                                }
                                _ => debug!("Ignored receiver message: {msg:?}"),
                            }
                        }
                        MEDIA_NAMESPACE => {
                            let msg = match json::from_str::<namespaces::Media>(json_payload) {
                                Ok(msg) => msg,
                                Err(err) => {
                                    error!("Failed to parse media message: {err}");
                                    continue;
                                }
                            };
                            #[allow(clippy::single_match)]
                            match msg {
                                namespaces::Media::Status { status, .. } => {
                                    for stat in status {
                                        media_session_id = stat.media_session_id;
                                        if let Some(media) = stat.media {
                                            if let Some(duration_update) = media.duration {
                                                changed!(duration, duration_update, duration_changed);
                                            }
                                            let new_source = Source::Url {
                                                url: media.content_id,
                                                content_type: media.content_type,
                                            };
                                            if shared_state.source != Some(new_source.clone()) {
                                                self.event_handler.source_changed(new_source.clone());
                                                shared_state.source = Some(new_source);
                                            }
                                        }
                                        debug!("New media_session_id: {media_session_id}");
                                        changed!(speed, stat.playback_rate, speed_changed);
                                        changed!(time, stat.current_time, time_changed);
                                        if let Some(level) = stat.volume.level {
                                            changed!(volume, level, volume_changed);
                                        }
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
                                    }
                                }
                                _ => (),
                            }
                        }
                        _ => warn!("Unsupported namespace: {}", packet.namespace),
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("Something"))?;
                    match cmd {
                        Command::Quit => break,
                        Command::LoadVideo {
                            content_type, content_id, speed, ..
                        } => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::Load {
                                    current_time: Some(0.0),
                                    media: protocol::MediaInformation {
                                        content_id,
                                        stream_type: protocol::StreamType::None,
                                        content_type,
                                        duration: None,
                                    },
                                    request_id: request_id.inc(),
                                    auto_play: None,
                                    playback_rate: speed,
                                }
                            ).await?;
                        }
                        Command::LoadUrl { content_type, url, resume_position, speed, .. } => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::Load {
                                    current_time: resume_position,
                                    media: protocol::MediaInformation {
                                        content_id: url,
                                        stream_type: protocol::StreamType::None,
                                        content_type,
                                        duration: None,
                                    },
                                    request_id: request_id.inc(),
                                    auto_play: None,
                                    playback_rate: speed,
                                }
                            ).await?;
                        }
                        Command::ChangeVolume(volume) => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                "receiver-0",
                                namespaces::Receiver::SetVolume {
                                    request_id: request_id.inc(),
                                    volume: protocol::Volume {
                                        level: Some(volume),
                                        muted: None,
                                    },
                                },
                            ).await?;
                        }
                        Command::ChangeSpeed(speed) => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::SetPlaybackRate {
                                    request_id: request_id.inc(),
                                    media_session_id,
                                    playback_rate: speed
                                },
                            ).await?;
                        }
                        Command::Seek(time_seconds) => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::Seek {
                                    media_session_id: media_session_id.to_string(),
                                    request_id: request_id.inc(),
                                    current_time: Some(time_seconds)
                                },
                            ).await?;
                        }
                        Command::Stop => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::Stop {
                                    media_session_id: media_session_id.to_string(),
                                    request_id: request_id.inc(),
                                }
                            ).await?;
                        }
                        Command::PausePlayback => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::Pause {
                                    media_session_id: media_session_id.to_string(),
                                    request_id: request_id.inc(),
                                }
                            ).await?;
                        }
                        Command::ResumePlayback => {
                            self.send_channel_message(
                                &mut writer,
                                "sender-0",
                                transport_id.clone(),
                                namespaces::Media::Resume {
                                    media_session_id: media_session_id.to_string(),
                                    request_id: request_id.inc(),
                                }
                            ).await?;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if !is_running {
                        debug!("Requesting receiver status");
                        self.send_channel_message(
                            &mut writer,
                            "sender-0",
                            "receiver-0",
                            namespaces::Receiver::GetStatus {
                                request_id: request_id.inc(),
                            },
                        )
                        .await?;
                    } else if media_session_id != 0 {
                        debug!("Requesting media status");
                        self.send_channel_message(
                            &mut writer,
                            "sender-0",
                            transport_id.clone(),
                            namespaces::Media::GetStatus {
                                request_id: request_id.inc(),
                                media_session_id: Some(media_session_id),
                            },
                        )
                        .await?;
                    }
                }
            }
        }

        info!("Shutting down...");

        writer.shutdown().await?;

        Ok(())
    }

    pub async fn work(mut self, addrs: Vec<SocketAddr>) {
        if let Err(err) = self.inner_work(addrs).await {
            error!("Inner work error: {err}");
        }

        self.event_handler
            .connection_state_changed(CastConnectionState::Disconnected);
    }
}

impl ChromecastCastingDevice {
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

// impl CastingDeviceExt for ChromecastCastingDevice {
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

//         Ok(Box::pin(InnerDevice::new(rx, event_handler).work(addrs)))
//     }
// }

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastingDevice for ChromecastCastingDevice {
    fn casting_protocol(&self) -> CastProtocolType {
        CastProtocolType::Chromecast
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
        false
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
        self.disconnect()
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

    fn load_url(
        &self,
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
    ) -> std::result::Result<(), CastingDeviceError> {
        self.send_command(Command::LoadUrl {
            content_type,
            url,
            resume_position,
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

    #[allow(unused_variables)]
    fn load_content(
        &self,
        content_type: String,
        content: String,
        resume_position: f64,
        duration: f64,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        todo!()
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
            .spawn(InnerDevice::new(rx, event_handler).work(addrs));

        Ok(())
    }

    fn get_device_info(&self) -> CastingDeviceInfo {
        let state = self.state.lock().unwrap();
        CastingDeviceInfo {
            name: state.name.clone(),
            r#type: CastProtocolType::Chromecast,
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
    fn subscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedSubscription)
    }

    #[allow(unused_variables)]
    fn unsubscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedSubscription)
    }
}
