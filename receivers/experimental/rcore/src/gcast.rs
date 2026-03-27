use std::sync::Arc;

use anyhow::{Result, bail};
use futures::StreamExt;
use google_cast_protocol::{
    Application, CONNECTION_NAMESPACE, HEARTBEAT_NAMESPACE, MEDIA_NAMESPACE, RECEIVER_NAMESPACE,
    VolumeStatus, namespaces, prost::Message, protos,
};
use parking_lot::RwLock;
use rcgen::{CertificateParams, DistinguishedName, KeyPair, date_time_ymd};
use serde_json as json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::UnboundedReceiver,
};
use tokio_rustls::{TlsAcceptor, rustls, server::TlsStream};
use tracing::{debug, error, instrument, warn};

use crate::EventSender;

const MAX_MSG_SIZE: usize = 1000 * 64;
const MEDIA_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

use google_cast_protocol::MediaStatus;

#[derive(Debug)]
pub enum StatusUpdate {
    Volume(f64),
    Position(f64),
    Duration(f64),
    PlayerState(crate::PlayerState),
}

struct State {
    pub event_tx: EventSender,
    pub has_launched: bool,
    pub media_status: Arc<RwLock<MediaStatus>>,
}

impl State {
    pub fn new(event_tx: EventSender, media_status: Arc<RwLock<MediaStatus>>) -> Self {
        Self {
            event_tx,
            has_launched: false,
            media_status,
        }
    }
}

// TODO: share with sender sdk
async fn write_channel_message<T>(
    writer: &mut tokio::io::WriteHalf<TlsStream<TcpStream>>,
    source_id: impl ToString,
    destination_id: impl ToString,
    obj: T,
) -> anyhow::Result<()>
where
    T: serde::Serialize + namespaces::Namespace + std::fmt::Debug,
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
    if encoded_len > MAX_MSG_SIZE {
        bail!("Message exceeded maximum length: {encoded_len} > {MAX_MSG_SIZE}");
    }
    // TODO: reuse buffers
    let mut write_buffer = vec![0u8; MAX_MSG_SIZE];
    cast_message.encode(&mut (&mut write_buffer[..encoded_len] as &mut [u8]))?;
    let serialized_size_be = (encoded_len as u32).to_be_bytes();
    let mut final_buf = serialized_size_be.to_vec();
    final_buf.extend_from_slice(&write_buffer[..encoded_len]);
    writer.write_all(&final_buf).await?;

    debug!("Sent {encoded_len} bytes, message: {cast_message:#?}");

    Ok(())
}

async fn send_status(
    writer: &mut tokio::io::WriteHalf<TlsStream<TcpStream>>,
    source_id: &str,
    destination_id: &str,
    request_id: u64,
) -> Result<()> {
    let status = google_cast_protocol::Status {
        applications: Some(vec![Application {
            app_id: "CC1AD845".to_owned(),
            app_type: Some("WEB".to_owned()),
            display_name: Some("Default Media Receiver".to_owned()),
            icon_url: Some("".to_owned()),
            is_idle_screen: Some(false),
            launched_from_cloud: Some(false),
            namespaces: Some(vec![google_cast_protocol::NamespaceMap {
                name: MEDIA_NAMESPACE.to_owned(),
            }]),
            session_id: MEDIA_ID.to_owned(),
            status_text: Some("Default Media Receiver".to_owned()),
            transport_id: MEDIA_ID.to_owned(),
            universal_app_id: Some("CC1AD845".to_owned()),
        }]),
        volume: VolumeStatus {
            control_type: "attenuation".to_owned(),
            level: 1.0,
            muted: false,
            step_interval: 0.01,
        },
    };

    write_channel_message(
        writer,
        source_id,
        destination_id,
        namespaces::Receiver::Status { request_id, status },
    )
    .await?;

    Ok(())
}

async fn send_empty_status(
    writer: &mut tokio::io::WriteHalf<TlsStream<TcpStream>>,
    sender_channel: &str,
    source_id: &str,
    request_id: u64,
) -> Result<()> {
    let status = google_cast_protocol::Status {
        applications: None,
        volume: VolumeStatus {
            control_type: "attenuation".to_owned(),
            level: 1.0,
            muted: false,
            step_interval: 0.01,
        },
    };

    write_channel_message(
        writer,
        source_id,
        sender_channel,
        namespaces::Receiver::Status { request_id, status },
    )
    .await?;

    Ok(())
}

#[derive(PartialEq, Eq)]
enum EndSession {
    Yes,
    No,
}

async fn handle_message(
    state: &mut State,
    writer: &mut tokio::io::WriteHalf<TlsStream<TcpStream>>,
    message: protos::CastMessage,
) -> Result<EndSession> {
    if message.payload_type() != protos::cast_message::PayloadType::String {
        bail!("Received message with unsupported payload type");
    }

    let json_payload = message.payload_utf8();
    match message.namespace.as_str() {
        HEARTBEAT_NAMESPACE => {}
        RECEIVER_NAMESPACE => match json::from_str::<namespaces::Receiver>(json_payload)? {
            namespaces::Receiver::SetVolume { volume, .. } => {
                state.event_tx.send(crate::Event::Op {
                    session_id: 0,
                    op: crate::Operation::SetVolume(fcast_protocol::SetVolumeMessage {
                        volume: volume.level.unwrap_or(
                            volume
                                .muted
                                .map(|mute| if mute { 0.0 } else { 1.0 })
                                .unwrap_or(0.0),
                        ),
                    }),
                })?;
            }
            namespaces::Receiver::StopSession { .. } => {
                state.event_tx.send(crate::Event::Op {
                    session_id: 0,
                    op: crate::Operation::Stop,
                })?;
                return Ok(EndSession::Yes);
            }
            namespaces::Receiver::Launch { app_id, request_id } => {
                if app_id == "CC1AD845" {
                    debug!("Launching default receiver");
                    send_status(writer, "receiver-0", &message.source_id, request_id).await?;
                    state.has_launched = true;
                } else {
                    todo!();
                }
            }
            namespaces::Receiver::GetStatus { request_id } => {
                if state.has_launched {
                    send_status(writer, "receiver-0", &message.source_id, request_id).await?;
                } else {
                    send_empty_status(writer, "receiver-0", &message.source_id, request_id).await?;
                }
            }
            _ => (),
        },
        MEDIA_NAMESPACE => {
            match json::from_str::<namespaces::Media>(json_payload)? {
                namespaces::Media::Load {
                    media,
                    current_time,
                    playback_rate,
                    ..
                } => {
                    state.event_tx.send(crate::Event::Op {
                        session_id: 0,
                        op: crate::Operation::Play(fcast_protocol::v3::PlayMessage {
                            container: media.content_type.clone(),
                            url: Some(media.content_id.clone()),
                            content: None,
                            time: current_time,
                            volume: None,
                            speed: playback_rate,
                            headers: None,
                            metadata: None,
                        }),
                    })?;
                    let mut status = state.media_status.write();
                    status.media = Some(media);
                    status.player_state = google_cast_protocol::PlayerState::Buffering;
                    status.idle_reason = None;
                }
                namespaces::Media::Seek { current_time, .. } => {
                    if let Some(time) = current_time {
                        state.event_tx.send(crate::Event::Op {
                            session_id: 0,
                            op: crate::Operation::Seek(fcast_protocol::SeekMessage { time }),
                        })?;
                    }
                }
                namespaces::Media::Resume { .. } => {
                    state.event_tx.send(crate::Event::Op {
                        session_id: 0,
                        op: crate::Operation::Resume,
                    })?;
                }
                namespaces::Media::Pause { .. } => {
                    state.event_tx.send(crate::Event::Op {
                        session_id: 0,
                        op: crate::Operation::Pause,
                    })?;
                }
                namespaces::Media::Stop { .. } => {
                    state.event_tx.send(crate::Event::Op {
                        session_id: 0,
                        op: crate::Operation::Stop,
                    })?;
                    let mut status = state.media_status.write();
                    status.media = None;
                }
                namespaces::Media::GetStatus { request_id, .. } => {
                    let status = state.media_status.read().clone();
                    write_channel_message(
                        writer,
                        MEDIA_ID,
                        "*",
                        namespaces::Media::Status {
                            request_id,
                            status: vec![status],
                        },
                    )
                    .await?;
                }
                namespaces::Media::SetPlaybackRate { playback_rate, .. } => {
                    state.event_tx.send(crate::Event::Op {
                        session_id: 0,
                        op: crate::Operation::SetSpeed(fcast_protocol::SetSpeedMessage {
                            speed: playback_rate,
                        }),
                    })?;
                }
                // TODO: implement support for these
                // namespaces::Media::QueueLoad { request_id, items, repeat_mode, start_index, queue_type } => todo!(),
                // namespaces::Media::QueueUpdate { request_id, media_session_id, jump } => todo!(),
                _ => (),
            }
        }
        CONNECTION_NAMESPACE => match json::from_str::<namespaces::Connection>(json_payload)? {
            namespaces::Connection::Connect { request_id, .. } => {
                send_status(
                    writer,
                    "receiver-0",
                    &message.source_id,
                    request_id.unwrap_or(0),
                )
                .await?;
            }
            namespaces::Connection::Close => (),
        },
        _ => warn!(
            namespace = message.namespace,
            "Received message with unsupported namespace"
        ),
    }

    Ok(EndSession::No)
}

#[instrument(skip_all, name = "gcast_session")]
async fn run_session(
    event_tx: EventSender,
    media_status: Arc<RwLock<MediaStatus>>,
    stream: TlsStream<TcpStream>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);

    let packet_stream = futures::stream::unfold(
        (reader, vec![0u8; MAX_MSG_SIZE]),
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

                let msg = protos::CastMessage::decode(&body_buf[..size])?;

                Ok(msg)
            }

            match read_packet(&mut reader, &mut body_buf).await {
                Ok(body) => Some((body, (reader, body_buf))),
                Err(err) => {
                    error!("Error occurred while reading packet: {err}");
                    None
                }
            }
        },
    );

    tokio::pin!(packet_stream);

    let mut state = State::new(event_tx, media_status);

    loop {
        tokio::select! {
            packet = packet_stream.next() => {
                let Some(message) = packet else {
                    break;
                };

                debug!(?message, "Received message");

                if handle_message(&mut state, &mut writer, message).await? == EndSession::Yes {
                    break;
                }
            }
        }
    }

    Ok(())
}

pub async fn run_server(
    event_tx: EventSender,
    mut status_rx: UnboundedReceiver<StatusUpdate>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("[::]:8009").await?;

    let mut params: CertificateParams = Default::default();
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    params.distinguished_name = DistinguishedName::new();
    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().to_owned()], key_pair.into())?;
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let media_status = Arc::new(RwLock::new(google_cast_protocol::MediaStatus {
        media_session_id: 0,
        media: None,
        playback_rate: 1.0,
        player_state: google_cast_protocol::PlayerState::Idle,
        idle_reason: None,
        current_time: 0.0,
        supported_media_commands: 0b001111,
        volume: google_cast_protocol::Volume {
            level: Some(1.0),
            muted: None,
        },
    }));

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, _addr) = res?;
                let acceptor = acceptor.clone();
                let event_tx = event_tx.clone();
                let media_status = Arc::clone(&media_status);
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(stream) => {
                            if let Err(err) = run_session(event_tx, media_status, stream).await {
                                debug!(?err, "Session ended with error");
                            }
                        }
                        Err(err) => debug!(?err, "TLS acceptor failed"),
                    }
                });
            }
            status_update = status_rx.recv() => {
                let Some(update) = status_update else {
                    break;
                };
                let mut status = media_status.write();
                match update {
                    StatusUpdate::Volume(vol) => status.volume.level = Some(vol),
                    StatusUpdate::Position(pos) => status.current_time = pos,
                    StatusUpdate::Duration(dur) => if let Some(info) = status.media.as_mut() {
                        info.duration = Some(dur);
                    },
                    StatusUpdate::PlayerState(state) => {
                        let (state, idle_reason) = match state {
                            crate::player::PlayerState::Paused => (google_cast_protocol::PlayerState::Paused, None),
                            crate::player::PlayerState::Playing => (google_cast_protocol::PlayerState::Playing, None),
                            crate::player::PlayerState::Buffering => (google_cast_protocol::PlayerState::Buffering, None),
                            crate::player::PlayerState::Stopped => (
                                google_cast_protocol::PlayerState::Idle,
                                Some(google_cast_protocol::IdleReason::Finished)
                            ),
                        };
                        status.player_state = state;
                        status.idle_reason = idle_reason;
                    }
                }
            }
        }
    }

    Ok(())
}
