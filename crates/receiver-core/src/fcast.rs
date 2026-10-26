use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    MessageSender, ReceiverInfo, SenderId, application::PacketOrigin,
    message::ReceiverToFCastSender, utils::current_time_millis,
};
use anyhow::bail;
use fcast_protocol::{
    Opcode, PlaybackErrorMessage, VersionMessage, companion,
    receiver::NetworkStream,
    session::{self, Action, CompanionResponse, Event, OpBudget, StateError},
    v2::VolumeUpdateMessage,
    v3::{self, ReceiverCapabilities},
    v4,
};
use serde::Serialize;
use tokio::{
    net::TcpStream,
    sync::{broadcast::Receiver, mpsc::UnboundedReceiver},
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, instrument, trace, warn};

// The sans-io session lives in fcast-protocol, re-exported under the old paths.
pub use fcast_protocol::session::{
    FlatLoadMessage, Header, InitialLegacyState, InitialV4State, LEGACY_MAX_BODY, MAX_RATE,
    MIN_RATE, Packet, PacketError, QueueInsertCell, ReceiverToSenderMessage, SessionSeed,
    SessionVersion, State, TICKS_BEFORE_PING, TranslatableMessage, V4Message, sanitize_rate,
};

const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct MirroringTx(pub tokio::sync::mpsc::UnboundedSender<InternalMessage>);

impl PartialEq for MirroringTx {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum WrappedPlayMessage {
    Legacy(v3::PlayMessage),
    V4(FlatLoadMessage),
    #[cfg(feature = "google-cast")]
    Chromecast(crate::gcast::ChromecastLoadItem),
}

impl From<session::WrappedPlayMessage> for WrappedPlayMessage {
    fn from(msg: session::WrappedPlayMessage) -> Self {
        match msg {
            session::WrappedPlayMessage::Legacy(m) => Self::Legacy(m),
            session::WrappedPlayMessage::V4(m) => Self::V4(m),
        }
    }
}

/// The session's [`session::Operation`] plus what only this receiver carries,
/// pipeline clock times and the mirroring channels.
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

impl From<session::Operation> for Operation {
    fn from(op: session::Operation) -> Self {
        use session::Operation as S;
        match op {
            S::Pause => Self::Pause,
            S::Resume => Self::Resume,
            S::Stop => Self::Stop,
            S::PlayNew(m) => Self::PlayNew(m.into()),
            // The session bounds a seek below u64::MAX nanoseconds
            S::Seek(t) => Self::Seek(gst::ClockTime::try_from(t).unwrap_or(gst::ClockTime::MAX)),
            S::SetSpeed(s) => Self::SetSpeed(s),
            S::SetPlaylistItem(m) => Self::SetPlaylistItem(m),
            S::SetVolume(v) => Self::SetVolume(v),
            S::SetPlaybackState(s) => Self::SetPlaybackState(s),
            S::ChangeTrack { id, typ } => Self::ChangeTrack { id, typ },
            S::AddSubtitleSource { url, select, name } => {
                Self::AddSubtitleSource { url, select, name }
            }
            S::SelectQueueItem(p) => Self::SelectQueueItem(p),
            S::RemoveQueueItem(p) => Self::RemoveQueueItem(p),
            S::InsertQueueItem(c) => Self::InsertQueueItem(c),
            S::ResumeOrPause => Self::ResumeOrPause,
            S::SetProgressUpdateInterval(d) => Self::SetProgressUpdateInterval(d),
        }
    }
}

impl From<session::SenderInfo> for crate::gui::SenderInfo {
    fn from(info: session::SenderInfo) -> Self {
        Self {
            display_name: info.display_name,
            app_name: info.app_name,
            app_version: info.app_version,
            protocol: match info.version {
                SessionVersion::V1 => fcast_bug_report::Protocol::FcastV1,
                SessionVersion::V2 => fcast_bug_report::Protocol::FcastV2,
                SessionVersion::V3 => fcast_bug_report::Protocol::FcastV3,
                SessionVersion::V4 { .. } => fcast_bug_report::Protocol::FcastV4,
            },
        }
    }
}

// The companion-resource contract moved to fcast-gst-elements alongside
// `fcompsrc`, its only in-pipeline consumer. Re-exported here so every existing
// call site (`crate::fcast::CompanionContext`, ...) keeps resolving.
pub use fcast_gst_elements::companion_ctx::{
    CompanionContext, CompanionMessage, FeedbackSender, ResourceInfoResponseCell,
};

use fcast_gst_elements::companion_ctx::{CompanionMsgReceiver, CompanionMsgSender};

enum CompanionQueueItem {
    GetResourceInfo(FeedbackSender<ResourceInfoResponseCell>),
    GetResource(FeedbackSender<companion::ResourceResponse>),
}

// Both live with the element that consumes them (fcast-webrtc); re-exported
// so `crate::fcast::InternalMessage` and friends still resolve.
pub use fcast_webrtc::{InternalMessage, MirroringOfferRx};

pub struct SessionDriver {
    op_budget: OpBudget,
    /// Transport commands draw on their own bucket, so a volume flood cannot
    /// starve a Stop and a Stop flood is still bounded.
    transport_budget: OpBudget,
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
    seed: SessionSeed,
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
        seed: SessionSeed,
    ) -> Self {
        Self {
            op_budget: OpBudget::new(std::time::Instant::now()),
            transport_budget: OpBudget::new(std::time::Instant::now()),
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
            seed,
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

    /// A v1-v3 message, skipped when over the packet limit: the sender would
    /// drop the connection on it (a play message carrying a big manifest).
    async fn send_legacy_msg(&mut self, opcode: Opcode, msg: impl Serialize) -> anyhow::Result<()> {
        let body = serde_json::to_vec(&msg)?;
        self.send_legacy_body(opcode, &body).await
    }

    async fn send_legacy_body(&mut self, opcode: Opcode, body: &[u8]) -> anyhow::Result<()> {
        if body.len() >= LEGACY_MAX_BODY {
            warn!(?opcode, len = body.len(), "Not sending a message over the v1-v3 packet limit");
            return Ok(());
        }
        self.send_bin_msg(opcode, body).await
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
                v4::flat::Time::new(pos.as_micros() as u64),
                v4::flat::Time::new(dur.as_micros() as u64),
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
            V4Message::Broadcast { serialized_msg } => {
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
        if let Some(SessionVersion::V4 { .. }) = self.state.version() {
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
            fmts.images.iter().map(|i| i.to_str()),
            true,
            true,
            0.01,
        );

        self.send_bin_msg(Opcode::Flatbuf, &msg).await?;

        // Volume seed: it only broadcasts on change, so a fresh sender would start with
        // a stale level.
        let volume_msg = v4::MessageBuilder::new().volume_changed(self.seed.volume);
        self.send_bin_msg(Opcode::Flatbuf, &volume_msg).await?;

        if let Some(initial) = self.seed.v4.take() {
            self.send_bin_msg(Opcode::Flatbuf, &initial.load).await?;
            let state_msg =
                v4::MessageBuilder::new().playback_state_changed(initial.playback_state);
            self.send_bin_msg(Opcode::Flatbuf, &state_msg).await?;
            if let Some((pos, dur)) = initial.progress {
                self.send_v4_message(&V4Message::ProgressUpdated { pos, dur })
                    .await?;
            }
            // a sender takes 1.0 until told otherwise
            if initial.rate != 1.0 {
                self.send_v4_message(&V4Message::PlaybackRateChanged(initial.rate))
                    .await?;
            }
            for msg in &initial.tracks {
                self.send_bin_msg(Opcode::Flatbuf, msg).await?;
            }
        }

        Ok(())
    }

    /// Seed a v1/v2/v3 session with the current volume (v4 gets it in
    /// `finish_tls_upgrade`); volume only broadcasts on change.
    async fn send_connect_volume(&mut self) -> anyhow::Result<()> {
        let Some(version) = self.state.version() else {
            return Ok(());
        };
        let msg = TranslatableMessage::VolumeUpdate(VolumeUpdateMessage {
            generation_time: current_time_millis(),
            volume: self.seed.volume as f64,
        });
        if let Some(body) = msg.translate_and_serialize(version) {
            self.send_bin_msg(Opcode::VolumeUpdate, &body).await?;
        }
        Ok(())
    }

    async fn send_connect_playback(
        &mut self,
        update: v3::PlaybackUpdateMessage,
    ) -> anyhow::Result<()> {
        let Some(version) = self.state.version() else {
            return Ok(());
        };
        let msg = TranslatableMessage::PlaybackUpdate(update);
        if let Some(body) = msg.translate_and_serialize(version) {
            self.send_bin_msg(Opcode::PlaybackUpdate, &body).await?;
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
        if let Some(transport) = res.as_ref().ok().and_then(session::budgeted_as_transport)
            && !{
                let budget = if transport { &mut self.transport_budget } else { &mut self.op_budget };
                budget.take(std::time::Instant::now())
            }
        {
            self.op_budget.dropped += 1;
            if self.op_budget.dropped.is_power_of_two() {
                warn!(id = self.id, dropped = self.op_budget.dropped, "Sender over its operation rate, dropping");
            }
            return Ok(false);
        }
        if let Ok(Action::Op(op)) = &res
            && session::names_foreign_companion(op, self.companion_provider_id())
        {
            warn!(id = self.id, "Refusing an operation naming another sender's companion resource");
            if let PacketOrigin::FCast { packet_num, .. } = origin {
                self.send_v4_error(packet_num, v4::flat::ErrorKind::ResourceNotFound).await?;
            }
            self.send_legacy_error("The receiver refused the media: ResourceNotFound".to_owned())
                .await?;
            return Ok(false);
        }
        match res {
            Ok(action) => match action {
                Action::None => (),
                Action::SenderIntroduced(info) => {
                    if let PacketOrigin::FCast { sender_id, .. } = origin {
                        msg_tx.send(crate::message::Message::SenderIntroduced {
                            sender_id,
                            info: info.into(),
                        });
                    }
                }
                Action::Ping => self.write_simple(Opcode::Ping).await?,
                Action::Pong => self.write_simple(Opcode::Pong).await?,
                Action::EndSession => return Ok(true),
                Action::Op(session::Operation::SetVolume(volume)) => {
                    let clamped = if volume.is_nan() {
                        0.0
                    } else {
                        volume.clamp(0.0, 1.0)
                    };
                    if clamped != volume {
                        if let PacketOrigin::FCast { packet_num, .. } = origin {
                            self.send_v4_error(packet_num, v4::flat::ErrorKind::VolumeOutOfRange)
                                .await?;
                        }
                    }
                    msg_tx.operation(origin, Operation::SetVolume(clamped));
                }
                Action::Op(session::Operation::SetSpeed(speed)) => {
                    let (rate, in_range) = sanitize_rate(speed);
                    if !in_range && let PacketOrigin::FCast { packet_num, .. } = origin {
                        self.send_v4_error(packet_num, v4::flat::ErrorKind::RateOutOfRange)
                            .await?;
                    }
                    msg_tx.operation(origin, Operation::SetSpeed(rate));
                }
                Action::Op(operation) => {
                    msg_tx.operation(origin, operation.into());
                }
                Action::SendInitial => {
                    let legacy = self.seed.legacy.take();
                    let mut initial = v3::InitialReceiverMessage {
                        display_name: self
                            .seed
                            .display_name
                            .clone()
                            .or_else(|| self.receiver_info.device_info.display_name.clone()),
                        app_name: self.receiver_info.device_info.app_name.clone(),
                        app_version: self.receiver_info.device_info.app_version.clone(),
                        play_data: legacy.as_ref().map(|l| l.play_data.clone()),
                        experimental_capabilities: Some(ReceiverCapabilities {
                            av: Some(v3::AVCapabilities {
                                livestream: Some(v3::LivestreamCapabilities { whep: Some(true) }),
                            }),
                        }),
                    };
                    session::fit_initial(&mut initial);
                    self.write_packet(Packet::Initial(initial)).await?;
                    self.send_connect_volume().await?;
                    // the state now, not at the next progress tick
                    if let Some(legacy) = legacy {
                        self.send_connect_playback(legacy.playback).await?;
                    }
                }
                Action::SendVolume => self.send_connect_volume().await?,
                Action::Forward {
                    session_version,
                    msg,
                } => match msg.as_ref() {
                    ReceiverToSenderMessage::Error(msg) => {
                        self.send_legacy_msg(Opcode::PlaybackError, msg).await?;
                    }
                    ReceiverToSenderMessage::LegacyTranslatable { op, msg } => {
                        let Some(session_version) = session_version else {
                            // Unreachable
                            error!("Missing session version");
                            return Ok(false);
                        };
                        if let Some(body) = msg.translate_and_serialize(session_version) {
                            self.send_legacy_body(*op, &body).await?;
                        } else {
                            error!("Could not translate message");
                        }
                    }
                    ReceiverToSenderMessage::Event { msg } => {
                        self.send_legacy_msg(Opcode::Event, msg).await?;
                    }
                    ReceiverToSenderMessage::PlayUpdate { msg } => {
                        self.send_legacy_msg(Opcode::PlayUpdate, msg).await?;
                    }
                    ReceiverToSenderMessage::V4(msg) => {
                        self.send_v4_message(msg).await?;
                    }
                },
                Action::UpgradeToTls => {
                    self.pending_tls_upgrade = true;
                }
                Action::RespondCompanionHello => {
                    // One provider per session, a repeated hello gets the same
                    // id, or a hello flood fills the registry.
                    let id = match self.companion_provider_id() {
                        Some(id) => id,
                        None => {
                            let Some(id) = self
                                .companion_ctx
                                .register_provider(self.internal_companion_tx.clone())
                            else {
                                error!("No free companion provider id");
                                return Ok(false);
                            };
                            debug!(id, "Registered companion provider");
                            self.state.set_companion_provider_id(id);
                            id
                        }
                    };
                    self.send_v4_message(&V4Message::CompanionHello(id)).await?;
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

    /// A PlaybackError for a v2/v3 session, nothing for v1 or v4.
    async fn send_legacy_error(&mut self, message: String) -> anyhow::Result<()> {
        if let Some(SessionVersion::V2 | SessionVersion::V3) = self.state.version() {
            self.send_legacy_msg(Opcode::PlaybackError, PlaybackErrorMessage { message })
                .await?;
        }
        Ok(())
    }

    async fn send_v4_error(
        &mut self,
        packet_num: Option<u32>,
        kind: v4::flat::ErrorKind,
    ) -> anyhow::Result<()> {
        if let Some(SessionVersion::V4 { .. }) = self.state.version() {
            let msg = v4::MessageBuilder::new().error(packet_num, kind);
            self.send_bin_msg(Opcode::Flatbuf, &msg).await?;
        }

        Ok(())
    }

    async fn handle_msg_from_receiver(&mut self, msg: ReceiverToFCastSender) -> anyhow::Result<()> {
        match msg {
            ReceiverToFCastSender::Error {
                kind,
                packet_num,
                legacy,
            } => {
                self.send_v4_error(packet_num, kind).await?;
                if let Some(message) = legacy {
                    self.send_legacy_error(message).await?;
                }
            }
            ReceiverToFCastSender::ProgressUpdate { pos, dur } => {
                if let Some(SessionVersion::V4 { .. }) = self.state.version() {
                    self.send_v4_message(&V4Message::ProgressUpdated {
                        pos: pos.into(),
                        dur: dur.into(),
                    })
                    .await?;
                }
            }
        }

        Ok(())
    }

    fn companion_provider_id(&self) -> Option<u16> {
        self.state.companion_provider_id()
    }

    #[cfg_attr(not(target_os = "android"), instrument(name = "session", skip_all, fields(id = self.id)))]
    pub async fn run(
        mut self,
        updates_rx: Receiver<Arc<ReceiverToSenderMessage>>,
        msg_tx: &MessageSender,
        comp_rx: CompanionMsgReceiver,
        msg_rx: UnboundedReceiver<ReceiverToFCastSender>,
    ) -> anyhow::Result<()> {
        let res = self.run_loop(updates_rx, msg_tx, comp_rx, msg_rx).await;
        debug!(state = ?self.state);
        // on every exit, an error included
        if let Some(id) = self.companion_provider_id() {
            self.companion_ctx.unregister_provider(id);
        }
        res
    }

    async fn run_loop(
        &mut self,
        mut updates_rx: Receiver<Arc<ReceiverToSenderMessage>>,
        msg_tx: &MessageSender,
        mut comp_rx: CompanionMsgReceiver,
        mut msg_rx: UnboundedReceiver<ReceiverToFCastSender>,
    ) -> anyhow::Result<()> {
        debug!("Session was started");

        const READ_HEADROOM: usize = 1024 * 8;
        let mut packet_reader =
            fcast_protocol::PacketReader::new(v4::MAX_PACKET_SIZE, READ_HEADROOM);
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
                            let res = self.state.advance(Event::InternalMirroringAnswer { sdp });
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
                res = self.stream.read(packet_reader.spare_capacity_mut()) => {
                    let Ok(n_read) = res else {
                        break;
                    };

                    if n_read == 0 {
                        break;
                    }

                    packet_reader.commit(n_read);

                    loop {
                        let packet = match packet_reader.get_packet() {
                            fcast_protocol::ReadResult::NeedData => break,
                            fcast_protocol::ReadResult::Read(packet) => packet,
                            fcast_protocol::ReadResult::PacketTooLarge(size) => bail!("Received too large packet size={size}"),
                        };

                        let (opcode, body) = match packet.len() {
                            0 => bail!("Received packet with no opcode (size=0)"),
                            1 => (packet[0], None),
                            // A view of the same bytes, not a copy of them.
                            _ => (packet[0], Some(packet.slice(1..))),
                        };

                        let Ok(opcode) = Opcode::try_from(opcode) else {
                            self.on_invalid_opcode(opcode, packet_num).await?;
                            continue;
                        };

                        trace!(?opcode, "Received packet");

                        let res = self.state.advance(Event::Packet { opcode, body });
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
                    let to_sender = match res {
                        Ok(to_sender) => to_sender,
                        // Missed events cannot be replayed, the sender resyncs
                        // on reconnect. Logged, a silent drop looked like a
                        // network fault.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                            warn!(missed, "Sender fell behind on updates, dropping it");
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };

                    let res = self.state.advance(Event::ToSender(to_sender));
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
                    let res = self.state.advance(Event::Tick);
                    if self.handle_state_result(origin, msg_tx, res, &internal_tx).await? {
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

    #[test]
    fn session_seek_maps_to_the_pipeline_clock() {
        let op = Operation::from(session::Operation::Seek(Duration::from_micros(5_000_000)));
        assert_eq!(
            op,
            Operation::Seek(gst::ClockTime::from_useconds(5_000_000))
        );
        // past the clock range saturates instead of panicking
        let op = Operation::from(session::Operation::Seek(Duration::MAX));
        assert_eq!(op, Operation::Seek(gst::ClockTime::MAX));
    }

    #[test]
    fn sender_info_keeps_the_protocol_it_introduced_itself_with() {
        let info = |version| session::SenderInfo {
            display_name: "d".into(),
            app_name: "a".into(),
            app_version: "1".into(),
            version,
        };
        let v3 = crate::gui::SenderInfo::from(info(SessionVersion::V3));
        assert_eq!(v3.protocol, fcast_bug_report::Protocol::FcastV3);
        assert_eq!(v3.display_name, "d");
        let v4 = crate::gui::SenderInfo::from(info(SessionVersion::V4 {
            companion_provider_id: Some(2),
            mirroring_session_id: None,
        }));
        assert_eq!(v4.protocol, fcast_bug_report::Protocol::FcastV4);
    }
}
