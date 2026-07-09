use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use base64::Engine;
use fcast_protocol::{
    Opcode, PlaybackErrorMessage, PlaybackState,
    v3::{self, VolumeUpdateMessage},
    v4::{self, flat::ErrorKind},
};
use gst::{glib::object::Cast, prelude::*};
use parking_lot::Mutex;
use rcgen::PublicKeyData;
use slint::ToSharedString;
use smallvec::SmallVec;
use smol_str::SmolStr;
use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{self, UnboundedReceiver},
    },
};
use tracing::{debug, error, info, warn};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::message;
use crate::{
    AppState, FCAST_TCP_PORT, GCastUpdateSender, GuiPlaybackState, MediaItemId, MessageSender,
    SenderId, UiMediaTrack, UiMediaTrackType, UiPlayerVariant,
    fcast::{
        self, CompanionContext, InitialV4State, Operation, ReceiverToSenderMessage, SessionDriver,
        TranslatableMessage, WrappedPlayMessage,
    },
    fcompsrc, fwebrtcsrc, gcast,
    gui::{self, GuiController, ToastType},
    image,
    media_formats::SupportedFormats,
    message::{Mdns, Message, Raop, ReceiverToFCastSender},
    player::{self, PlayerState},
    raop, user_agent,
    utils::{current_time_millis, map_to_header_map},
};
#[cfg(not(target_os = "android"))]
use crate::{Settings, mdns};
#[cfg(feature = "airplay")]
use crate::{airplay, message::AirPlay};

const SENDER_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_TICK_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(any(target_os = "macos", target_os = "windows"))]
const UPDATER_BASE_URL: &str = "http://dl.fcast.org/receiver/desktop";

#[derive(PartialEq, Eq)]
enum PreservePlaylist {
    Yes,
    No,
}

#[derive(PartialEq, Eq)]
enum ContinueToPlay {
    Yes,
    No,
}

struct RaopServer {
    config: raop::Configuration,
}

#[cfg(feature = "airplay")]
struct AirPlayServer {
    config: airplay::Configuration,
}

#[derive(Clone, Debug)]
struct QueueItem {
    content_type: String,
    url: String,
    time: Option<f64>,
    volume: Option<f64>,
    speed: Option<f64>,
    show_duration: Option<f64>,
    headers: Option<HashMap<String, String>>,
    title: Option<String>,
    thumbnail_url: Option<String>,
}

impl QueueItem {
    fn from_flat(item: &v4::flat::QueueItem) -> Self {
        let media_item = item.media_item();
        let headers = media_item.headers().map(|headers| {
            headers
                .iter()
                .map(|h| (h.key().to_owned(), h.value().to_owned()))
                .collect()
        });
        Self {
            content_type: media_item.container().to_owned(),
            url: media_item.source_url().to_owned(),
            time: media_item
                .start_time()
                .map(|t| Duration::from_micros(t.micros()).as_secs_f64()),
            volume: media_item.volume().map(|v| v as f64),
            speed: media_item.speed().map(|s| s as f64),
            show_duration: item
                .playback_duration()
                .map(|t| Duration::from_micros(t.micros()).as_secs_f64()),
            headers,
            title: media_item.title().map(ToOwned::to_owned),
            thumbnail_url: media_item.thumbnail_url().map(ToOwned::to_owned),
        }
    }

    fn to_media_item(&self) -> v3::MediaItem {
        let metadata = if self.title.is_some() || self.thumbnail_url.is_some() {
            Some(v3::MetadataObject::Generic {
                title: self.title.clone(),
                thumbnail_url: self.thumbnail_url.clone(),
                custom: None,
            })
        } else {
            None
        };
        v3::MediaItem {
            container: self.content_type.clone(),
            url: Some(self.url.clone()),
            time: self.time,
            volume: self.volume,
            speed: self.speed,
            show_duration: self.show_duration,
            headers: self.headers.clone(),
            metadata,
            ..Default::default()
        }
    }
}

struct QueueState {
    items: Vec<QueueItem>,
    current_idx: u8,
}

fn image_download_error_kind(err: &image::DownloadImageError) -> ErrorKind {
    use image::DownloadImageError as E;
    match err {
        E::RequestFailed(_)
        | E::Unsuccessful(_)
        | E::InvalidUrl(_)
        | E::FailedToGetInfo
        | E::InvalidCompUrl
        | E::ProviderNotFound
        | E::CompRequestFailed
        | E::ResourceNotFound => ErrorKind::ResourceNotFound,
        E::MissingContentType
        | E::InvalidContentType
        | E::ContentTypeIsNotString
        | E::UnsupportedContentType(_)
        | E::DecodeImage(_) => ErrorKind::UnsupportedFormat,
    }
}

fn media_error_kind_to_error(kind: player::MediaErrorKind) -> ErrorKind {
    match kind {
        player::MediaErrorKind::NotFound | player::MediaErrorKind::NotAuthorized => {
            ErrorKind::ResourceNotFound
        }
        player::MediaErrorKind::UnsupportedFormat => ErrorKind::UnsupportedFormat,
        player::MediaErrorKind::Other => ErrorKind::Internal,
    }
}

#[derive(Debug, thiserror::Error)]
enum LoadMediaError {
    #[error("invalid content container ({0})")]
    InvalidContentContainer(String),
    #[error("item has no URL or content")]
    NoUrlOrContent,
    #[error("no current media item")]
    NoItem,
    #[error("playlist/queue index out of bounds")]
    IndexOutOfBounds,
}

fn load_media_error_kind(err: &LoadMediaError) -> ErrorKind {
    match err {
        LoadMediaError::NoUrlOrContent => ErrorKind::MalformedBody,
        LoadMediaError::InvalidContentContainer(_) => ErrorKind::UnsupportedFormat,
        LoadMediaError::IndexOutOfBounds | LoadMediaError::NoItem => ErrorKind::Internal,
    }
}

enum MediaSource {
    Single(Arc<fcast::WrappedPlayMessage>),
    Playlist {
        content: v3::PlaylistContent,
        index: usize,
    },
    Queue(QueueState),
    Raop,
    #[cfg_attr(not(feature = "airplay"), allow(dead_code))]
    AirPlayMirror {
        stream_connection_id: u64,
    },
}

#[derive(Debug, Copy, Clone)]
pub enum PacketOrigin {
    Gui,
    AutoPlay,
    FCast {
        sender_id: SenderId,
        packet_num: Option<u32>,
    },
    GCast {
        sender_id: SenderId,
    },
    Raop,
    #[cfg_attr(not(feature = "airplay"), allow(dead_code))]
    AirPlay,
}

impl PacketOrigin {
    pub(crate) fn fcast(sender_id: SenderId, packet_num: Option<u32>) -> Self {
        Self::FCast {
            sender_id,
            packet_num,
        }
    }

    pub(crate) fn gcast(sender_id: SenderId) -> Self {
        Self::GCast { sender_id }
    }
}

struct MediaSourceState {
    origin: PacketOrigin,
    source: MediaSource,
    image_id: Option<image::ImageId>,
    pending_thumbnail: Option<image::ImageId>,
    pending_thumbnail_download: Option<image::ImageDownloadId>,
}

impl MediaSourceState {
    fn new(origin: PacketOrigin, source: MediaSource) -> Self {
        Self {
            origin,
            source,
            image_id: None,
            pending_thumbnail: None,
            pending_thumbnail_download: None,
        }
    }
}

struct FCastSenderHandle {
    msg_tx: mpsc::UnboundedSender<ReceiverToFCastSender>,
    progress_interval: Duration,
    last_progress_update: Instant,
}

impl FCastSenderHandle {
    fn new(msg_tx: mpsc::UnboundedSender<ReceiverToFCastSender>) -> Self {
        Self {
            msg_tx,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            last_progress_update: Instant::now(),
        }
    }
}

struct OnMediaLoadedOperation {
    seek_position: gst::ClockTime,
    seek_rate: f32,
}

pub struct Application {
    #[cfg(target_os = "android")]
    android_app: slint::android::AndroidApp,
    msg_tx: MessageSender,
    updates_tx: broadcast::Sender<Arc<ReceiverToSenderMessage>>,
    #[cfg(not(target_os = "android"))]
    mdns: mdns_sd::ServiceDaemon,
    last_sent_update: Instant,
    debug_mode: bool,
    player: player::Player,
    current_duration: Option<gst::ClockTime>,
    on_playing_command: Option<OnMediaLoadedOperation>,
    pending_subtitle_refresh: bool,
    current_image_id: image::ImageId,
    current_image_download_id: image::ImageDownloadId,
    have_audio_track_cover: bool,
    current_media: Option<MediaSourceState>,
    have_media_info: bool,
    current_thumbnail_id: image::ImageId,
    current_addresses: HashSet<IpAddr>,
    have_media_title: bool,
    last_position_updated: f64,
    http_client: reqwest::Client,
    current_request_headers: Arc<Mutex<Option<HashMap<String, String>>>>,
    device_name: Option<String>,
    current_media_item_id: MediaItemId,
    is_loading_media: bool,
    raop_server: Option<RaopServer>,
    #[cfg(feature = "airplay")]
    airplay_server: Option<AirPlayServer>,
    gui: GuiController,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    update: Option<app_updater::Release>,
    gcast_tx: GCastUpdateSender,
    #[cfg(not(target_os = "android"))]
    settings: Settings,
    window_visible_before_playing: Option<bool>,
    window_fullscreen_before_playing: Option<bool>,
    image_downloader: image::Downloader,
    image_decoder: image::Decoder,
    screensaver_inhibitor: inhibit_screensaver::Inhibitor,
    tls_acceptor: tokio_rustls::TlsAcceptor,
    companion_ctx: CompanionContext,
    #[cfg(feature = "airplay")]
    airplay_context: airplay::AirPlayContext,
    signalling_channel: Arc<Mutex<Option<fwebrtcsrc::SignallingChannel>>>,
    receiver_info: Arc<crate::ReceiverInfo>,
    fcast_txt_records: HashMap<String, String>,
    fcast_senders: HashMap<SenderId, FCastSenderHandle>,
}

impl Application {
    pub async fn new(
        gui: GuiController,
        video_sink: Option<gst::Element>,
        msg_tx: MessageSender,
        #[cfg(not(target_os = "android"))] settings: Settings,
    ) -> Result<Self> {
        let registry = gst::Registry::get();
        for nv_feature in registry.features_by_plugin("nvcodec") {
            if let Some(elem) = nv_feature.downcast_ref::<gst::ElementFactory>()
                && elem.has_type(gst::ElementFactoryType::DECODER)
            {
                debug!("Changing {}'s rank to MARGINAL", elem.name());
                elem.set_rank(gst::Rank::MARGINAL);
            }
        }

        #[cfg(target_os = "android")]
        if let Some(amcaudiodec) = registry.lookup_feature("amcaudiodec") {
            // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/4883
            amcaudiodec.set_rank(gst::Rank::NONE);
        }

        let companion_ctx = CompanionContext::new();
        #[cfg(feature = "airplay")]
        let airplay_context = airplay::AirPlayContext::new();
        let signalling_channel = Arc::new(Mutex::new(None::<fwebrtcsrc::SignallingChannel>));
        let player = player::Player::new(
            video_sink,
            msg_tx.clone(),
            fcompsrc::imp::CompContext(companion_ctx.clone()),
            #[cfg(feature = "airplay")]
            airplay_context.clone(),
            // Arc::clone(&signalling_channel),
        )?;

        let headers = Arc::new(Mutex::new(None::<HashMap<String, String>>));

        player.playbin.connect("element-setup", false, {
            let headers = Arc::clone(&headers);
            let signalling_channel = Arc::clone(&signalling_channel);
            move |vals| {
                let Ok(elem) = vals[1].get::<gst::Element>() else {
                    return None;
                };

                let name = elem.factory()?.name();
                // debug!(?name, "Setting up element");
                match name.as_str() {
                    "fcasthttpsrc" => {
                        let mut did_set_user_agent = false;
                        if let Some(ref headers) = *headers.lock() {
                            let mut extra_headers_builder =
                                gst::Structure::builder("reqwesthttpsrc-extra-headers");
                            for (k, v) in headers {
                                if k == "User-Agent" || k == "user-agent" {
                                    elem.set_property("user-agent", v);
                                    did_set_user_agent = true;
                                } else {
                                    extra_headers_builder = extra_headers_builder.field(k, v);
                                }
                            }
                            elem.set_property("extra-headers", extra_headers_builder.build());
                        }
                        if !did_set_user_agent {
                            elem.set_property(
                                "user-agent",
                                user_agent::random_browser_user_agent(None),
                            );
                        }
                    }
                    "fwebrtcsrc" => {
                        if let Some(chan) = signalling_channel.lock().clone() {
                            elem.set_property("signalling-channel", chan);
                            debug!("Set `signalling-channel` on `fwebrtcsrc`")
                        } else {
                            warn!("Missing signalling channel");
                        }
                    }
                    _ => (),
                }

                None
            }
        });

        let (updates_tx, _) = broadcast::channel(10);

        let (acceptor, fingerprint) = {
            use rcgen::{CertificateParams, DistinguishedName, KeyPair, date_time_ymd};
            use tokio_rustls::{TlsAcceptor, rustls};

            let mut params: CertificateParams = Default::default();
            params.not_before = date_time_ymd(1975, 1, 1);
            params.not_after = date_time_ymd(4096, 1, 1);
            params.distinguished_name = DistinguishedName::new();
            let key_pair = KeyPair::generate()?;
            let cert = params.self_signed(&key_pair)?;
            let spki = key_pair.subject_public_key_info();
            use sha2::Digest;
            let digest = sha2::Sha256::digest(&spki);
            let fingerprint = base64::engine::general_purpose::STANDARD.encode(digest);

            let config =
                rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                    .with_no_client_auth()
                    .with_single_cert(vec![cert.der().to_owned()], key_pair.into())?;
            (TlsAcceptor::from(Arc::new(config)), fingerprint)
        };

        let fcast_txt_records = HashMap::from([
            ("fp".to_owned(), fingerprint),
            ("v".to_owned(), "4".to_owned()),
        ]);
        #[cfg(not(target_os = "android"))]
        let mdns = mdns::start_daemon(&msg_tx, &settings, &fcast_txt_records)?;

        let run_gcast = if cfg!(not(target_os = "android")) {
            !settings.cli.no_google_cast
        } else {
            true
        };

        let gcast_tx = if run_gcast {
            let (gcast_tx, gcast_rx) = mpsc::unbounded_channel::<gcast::StatusUpdate>();
            tokio::spawn(gcast::run_server(msg_tx.clone(), gcast_rx));
            GCastUpdateSender(Some(gcast_tx))
        } else {
            GCastUpdateSender(None)
        };

        #[cfg(debug_assertions)]
        tokio::spawn({
            use tokio::io::AsyncReadExt;
            use tracing::{Instrument, debug_span};
            let msg_tx = msg_tx.clone();
            async move {
                let listener = tokio::net::TcpListener::bind("[::]:46897").await.unwrap();
                loop {
                    let (mut stream, addr) = listener.accept().await.unwrap();
                    debug!(?addr, "Got connection");

                    let mut buf = [0u8; 1];
                    if let Ok(_) = stream.read_exact(&mut buf).await
                        && buf[0] == 0xFF
                    {
                        msg_tx.send(Message::DumpPipeline);
                    }
                }
            }
            .instrument(debug_span!("pipeline-dbg-listener"))
        });

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        tokio::spawn({
            use tracing::Instrument;
            let msg_tx = msg_tx.clone();
            async move {
                match app_updater::check_for_update(UPDATER_BASE_URL, env!("CARGO_PKG_VERSION"))
                    .instrument(tracing::debug_span!("check_for_updates"))
                    .await
                {
                    Ok(release) => {
                        if let Some(release) = release {
                            msg_tx.app_update(message::AppUpdate::UpdateAvailable(release));
                        }
                    }
                    Err(err) => {
                        error!(?err, "Failed to check for update");
                    }
                }
            }
        });

        image::init_extra_decoders();
        let image_decoder = image::Decoder::new(msg_tx.clone())?;
        let http_client = reqwest::Client::new();
        let image_downloader =
            image::Downloader::new(msg_tx.clone(), http_client.clone(), companion_ctx.clone());

        let receiver_info = Arc::new(crate::ReceiverInfo {
            device_info: fcast_protocol::v4::DeviceInfo {
                display_name: None,
                app_name: Some("FCast Receiver Desktop".to_owned()),
                app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            },
            supported_formats: SupportedFormats::get_all(),
        });

        debug!("Receiver information: {receiver_info:?}");

        Ok(Self {
            #[cfg(target_os = "android")]
            android_app,
            msg_tx,
            updates_tx,
            #[cfg(not(target_os = "android"))]
            mdns,
            last_sent_update: Instant::now() - SENDER_UPDATE_INTERVAL,
            #[cfg(debug_assertions)]
            debug_mode: true,
            #[cfg(not(debug_assertions))]
            debug_mode: false,
            player,
            current_duration: None,
            on_playing_command: None,
            pending_subtitle_refresh: false,
            current_image_id: 0,
            have_audio_track_cover: false,
            current_media: None,
            have_media_info: false,
            current_thumbnail_id: 0,
            current_image_download_id: 0,
            current_addresses: HashSet::new(),
            have_media_title: false,
            last_position_updated: -1.0,
            http_client,
            current_request_headers: headers,
            device_name: None,
            current_media_item_id: 0,
            is_loading_media: false,
            raop_server: None,
            #[cfg(feature = "airplay")]
            airplay_server: None,
            gui,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            update: None,
            gcast_tx,
            #[cfg(not(target_os = "android"))]
            settings,
            window_visible_before_playing: None,
            window_fullscreen_before_playing: None,
            image_downloader,
            image_decoder,
            screensaver_inhibitor: inhibit_screensaver::Inhibitor::new(
                inhibit_screensaver::Options {
                    app_reverse_domain: "org.fcast.receiver".to_owned(),
                },
            ),
            tls_acceptor: acceptor,
            companion_ctx,
            #[cfg(feature = "airplay")]
            airplay_context,
            signalling_channel,
            receiver_info,
            fcast_txt_records,
            fcast_senders: HashMap::new(),
        })
    }

    fn should_broadcast(&self) -> bool {
        self.updates_tx.receiver_count() > 0
    }

    fn broadcast_update(&self, msg: ReceiverToSenderMessage) {
        let _ = self.updates_tx.send(Arc::new(msg)).is_err();
    }

    fn relay_to_other_senders(
        &self,
        origin: PacketOrigin,
        serialized_msg: fcast_protocol::v4::ConstructedMessage<'static>,
    ) {
        if let PacketOrigin::FCast { sender_id, .. } = origin
            && self.should_broadcast()
        {
            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::RelayToOtherSenders {
                    initiator_session_id: sender_id,
                    serialized_msg,
                },
            ));
        }
    }

    fn playback_progress_changed(&mut self) {
        let position = self.player.get_position().unwrap_or(gst::ClockTime::ZERO);
        let duration = self.current_duration.unwrap_or(gst::ClockTime::ZERO);

        self.gui
            .update_playback_progress(position.seconds_f64() as f32, duration.seconds_f64() as f32);

        if self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::ProgressUpdated {
                    pos: position,
                    dur: duration,
                },
            ));
        }
    }

    fn send_v4_progress_updates(&mut self) {
        if self.fcast_senders.is_empty() {
            return;
        }

        let pos = self.player.get_position().unwrap_or(gst::ClockTime::ZERO);
        let dur = self.current_duration.unwrap_or(gst::ClockTime::ZERO);
        let now = Instant::now();

        for handle in self.fcast_senders.values_mut() {
            if now.duration_since(handle.last_progress_update) < handle.progress_interval {
                continue;
            }
            handle.last_progress_update = now;
            let _ = handle
                .msg_tx
                .send(ReceiverToFCastSender::ProgressUpdate { pos, dur });
        }
    }

    fn playback_state_changed(&mut self, state: fcast_protocol::v4::PlaybackState) {
        if self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::PlaybackStateChanged(state),
            ));
        }
    }

    fn send_error(&self, origin: PacketOrigin, error: fcast_protocol::v4::flat::ErrorKind) {
        error!(?origin, ?error, "An error occured");

        match origin {
            PacketOrigin::Gui
            | PacketOrigin::AutoPlay
            | PacketOrigin::Raop
            | PacketOrigin::AirPlay => (),
            PacketOrigin::FCast {
                sender_id,
                packet_num,
            } => {
                if let Some(sender_handle) = self.fcast_senders.get(&sender_id) {
                    let _ = sender_handle.msg_tx.send(ReceiverToFCastSender::Error {
                        kind: error,
                        packet_num,
                    });
                }
            }
            PacketOrigin::GCast { .. } => (),
        }
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all))]
    fn notify_updates(&mut self, force: bool) -> Result<()> {
        if !self.player.have_media_info() || self.player.is_seeking() {
            return Ok(());
        }

        let Some(position) = self.player.get_position() else {
            error!("player does not have a playback position");
            return Ok(());
        };
        let position = position.seconds_f64();
        self.last_position_updated = position;
        let duration = match self.current_duration {
            Some(dur) => dur,
            None => {
                let dur = self.player.get_duration().unwrap_or_default();
                self.current_duration = Some(dur);
                dur
            }
        };
        let duration = duration.seconds_f64();

        self.gcast_tx.send(gcast::StatusUpdate::Duration(duration));
        self.gcast_tx.send(gcast::StatusUpdate::Position(position));

        let is_live = self.player.is_live();
        let playback_state = {
            match self.player.player_state() {
                PlayerState::Stopped | PlayerState::Buffering => GuiPlaybackState::Loading,
                PlayerState::Playing => GuiPlaybackState::Playing,
                PlayerState::Paused => GuiPlaybackState::Paused,
            }
        };
        let playback_rate = self.player.rate();

        self.gui.set_playback_state(playback_state);
        self.gui.set_is_live(is_live);
        self.gui.set_playback_rate(playback_rate as f32);
        self.gui
            .update_playback_progress(position as f32, duration as f32);

        if self.should_broadcast()
            && (self.last_sent_update.elapsed() >= SENDER_UPDATE_INTERVAL || force)
        {
            let update = v3::PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: Some(position),
                duration: Some(duration),
                state: match playback_state {
                    GuiPlaybackState::Idle | GuiPlaybackState::Loading => PlaybackState::Idle,
                    GuiPlaybackState::Playing => PlaybackState::Playing,
                    GuiPlaybackState::Paused => PlaybackState::Paused,
                },
                speed: Some(playback_rate),
                item_index: None,
            };

            debug!("Sending update ({update:?})");

            self.broadcast_update(ReceiverToSenderMessage::LegacyTranslatable {
                op: Opcode::PlaybackUpdate,
                msg: TranslatableMessage::PlaybackUpdate(update),
            });
            self.last_sent_update = Instant::now();
        }

        Ok(())
    }

    fn cleanup_playback_data(
        &mut self,
        continue_to_play: ContinueToPlay,
        preserve_playlist: PreservePlaylist,
    ) {
        self.current_duration = None;
        self.on_playing_command = None;
        self.have_audio_track_cover = false;
        self.have_media_info = false;
        self.have_media_title = false;
        self.last_position_updated = -1.0;
        *self.current_request_headers.lock() = None;
        self.player.stop();
        self.is_loading_media = false;
        if let Some(current_media) = self.current_media.as_mut() {
            // TODO: is this right?
            current_media.image_id = None;
            current_media.pending_thumbnail = None;
            current_media.pending_thumbnail_download = None;
        }

        self.current_thumbnail_id += 1;
        self.current_image_id += 1;
        self.current_image_download_id += 1;

        if continue_to_play == ContinueToPlay::No {
            self.gui.set_media_title("".to_owned());
            self.gui.set_artist_name("".to_owned());
            self.gui.clear_images();
            self.gui.update_playback_progress(0.0, 0.0);
            self.gui.set_app_state(AppState::Idle);
            self.gui.set_playback_state(GuiPlaybackState::Idle);
            self.gui.clear_tracks();
            self.gui.set_track_ids(-1, -1, -1);
            self.gui.clear_common_playback_state();

            if preserve_playlist == PreservePlaylist::No {
                self.gui.update_playlist(0, 0);
            }

            if let Some(fullscreen) = self.window_fullscreen_before_playing.take() {
                self.gui.set_fullscreen(fullscreen);
                // https://github.com/slint-ui/slint/issues/11267
                std::thread::sleep(std::time::Duration::from_millis(75));
            }

            if let Some(visible) = self.window_visible_before_playing.take() {
                self.gui.set_window_visibility(visible);
            }
        }
    }

    fn is_playing(&self) -> bool {
        self.current_media.is_some()
    }

    fn media_loaded_successfully(&mut self) {
        self.is_loading_media = false;

        if !self.is_playing() {
            debug!("Ignoring old media loaded succesfully event");
            return;
        };

        // TODO: needs debouncing since seeks will trigger this too, or maybe not?
        info!("Media loaded successfully");

        #[cfg(target_os = "android")]
        {
            let android_app = self.android_app.clone();
            tokio::task::spawn_blocking(move || {
                android_app.set_window_flags(
                    WindowManagerFlags::KEEP_SCREEN_ON,
                    WindowManagerFlags::empty(),
                );
            });
        }

        let Some(current_media) = self.current_media.as_ref() else {
            return;
        };

        match &current_media.source {
            MediaSource::Single(play_msg) => {
                if self.should_broadcast()
                    && let fcast::WrappedPlayMessage::Legacy(msg) = play_msg.as_ref()
                {
                    let event = v3::EventObject::MediaItem {
                        variant: v3::EventType::MediaItemStart,
                        item: msg.clone().into(),
                    };
                    let msg = v3::EventMessage {
                        generation_time: current_time_millis(),
                        event,
                    };
                    self.broadcast_update(ReceiverToSenderMessage::Event { msg });
                }
            }
            MediaSource::Playlist { content, index } => {
                let Some(item) = content.items.get(*index).cloned() else {
                    return;
                };

                if let Some(show_duration) = item.show_duration {
                    let msg_tx = self.msg_tx.clone();
                    let id = self.current_media_item_id;
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(show_duration)).await;
                        msg_tx.send(Message::MediaItemFinish(id));
                    });
                }

                if self.should_broadcast() {
                    let event = v3::EventObject::MediaItem {
                        variant: v3::EventType::MediaItemChange,
                        item,
                    };
                    let msg = v3::EventMessage {
                        generation_time: current_time_millis(),
                        event,
                    };
                    self.broadcast_update(ReceiverToSenderMessage::Event { msg });
                }
            }
            MediaSource::Queue(_) => (),
            MediaSource::Raop | MediaSource::AirPlayMirror { .. } => (),
        }
    }

    fn current_item_uri(&self) -> Option<&str> {
        match &self.current_media.as_ref()?.source {
            MediaSource::Single(play_msg) => match play_msg.as_ref() {
                fcast::WrappedPlayMessage::Legacy(msg) => msg.url.as_deref(),
                fcast::WrappedPlayMessage::V4(msg) => {
                    let msg = msg.borrow_dependent();
                    match msg.source_type() {
                        fcast_protocol::v4::flat::MediaSource::Single => {
                            Some(msg.source_as_single()?.source_url())
                        }
                        _ => None,
                    }
                }
                fcast::WrappedPlayMessage::Chromecast(item) => Some(&item.url),
            },
            MediaSource::Playlist { content, index } => content.items.get(*index)?.url.as_deref(),
            MediaSource::Queue(queue) => Some(&queue.items.get(queue.current_idx as usize)?.url),
            MediaSource::Raop | MediaSource::AirPlayMirror { .. } => None,
        }
    }

    fn media_error(&mut self, message: String) -> Result<()> {
        if !self.is_playing() {
            return Ok(());
        }

        error!(msg = message, "Media error");

        self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::No);
        self.current_media = None;

        if self.should_broadcast() {
            let update = v3::PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: None,
                duration: None,
                state: PlaybackState::Idle,
                speed: None,
                item_index: None,
            };
            self.broadcast_update(ReceiverToSenderMessage::LegacyTranslatable {
                op: Opcode::PlaybackUpdate,
                msg: TranslatableMessage::PlaybackUpdate(update),
            });
            self.broadcast_update(ReceiverToSenderMessage::Error(PlaybackErrorMessage {
                message: message.clone(),
            }))
        }

        self.gui.show_toast(ToastType::Error, message);

        Ok(())
    }

    fn media_warning(&mut self, message: String) -> Result<()> {
        // Ignore false positives because of the video sink not being ready until it has GL contexts set
        if !self.is_playing() {
            return Ok(());
        }

        warn!(msg = message, "Media warning");

        self.gui.show_toast(ToastType::Warning, message);

        Ok(())
    }

    fn media_ended(&mut self) {
        info!("Media finished");

        #[cfg(target_os = "android")]
        {
            let android_app = self.android_app.clone();
            tokio::task::spawn_blocking(move || {
                android_app.set_window_flags(
                    WindowManagerFlags::empty(),
                    WindowManagerFlags::KEEP_SCREEN_ON,
                );
            });
        }

        // Special case for when there's a google cast sender connected
        if self.updates_tx.receiver_count() == 0 {
            self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::Yes);
            self.current_media = None;
        }

        self.screensaver_inhibitor.un_inhibit();
    }

    fn queue_mut(&mut self) -> Option<&mut QueueState> {
        match &mut self.current_media.as_mut()?.source {
            MediaSource::Queue(queue) => Some(queue),
            _ => None,
        }
    }

    fn load_media(&mut self) {
        if let Err(err) = self.load_current_media_item() {
            error!(?err, "Failed to load media");
            if let Some(origin) = self.current_media.as_ref().map(|m| m.origin) {
                self.send_error(origin, load_media_error_kind(&err));
            }
        }
    }

    fn load_current_media_item(&mut self) -> std::result::Result<(), LoadMediaError> {
        let current_media = self.current_media.as_ref().ok_or(LoadMediaError::NoItem)?;
        // TODO: this shouldn't be v3 item
        let item = match &current_media.source {
            MediaSource::Single(play_data) => match play_data.as_ref() {
                fcast::WrappedPlayMessage::Legacy(msg) => msg.clone().into(),
                fcast::WrappedPlayMessage::V4(packet) => {
                    let Some(single) = packet.borrow_dependent().source_as_single() else {
                        error!("Body is not a valid single source");
                        self.send_error(current_media.origin, ErrorKind::MalformedBody);
                        return Ok(());
                    };
                    v3::MediaItem {
                        container: single.container().to_owned(),
                        url: Some(single.source_url().to_owned()),
                        time: single
                            .start_time()
                            .map(|t| Duration::from_micros(t.micros()).as_secs_f64()),
                        volume: single.volume().map(|v| v as f64),
                        speed: single.speed().map(|s| s as f64),
                        ..Default::default()
                    }
                }
                fcast::WrappedPlayMessage::Chromecast(cast) => v3::MediaItem {
                    container: cast.container.clone(),
                    url: Some(cast.url.clone()),
                    time: cast.time,
                    speed: cast.speed,
                    ..Default::default()
                },
            },
            MediaSource::Playlist { content, index } => content
                .items
                .get(*index)
                // Caller checks the index so this shouldn't be reached
                .ok_or(LoadMediaError::IndexOutOfBounds)?
                .clone(),
            MediaSource::Queue(queue) => queue
                .items
                .get(queue.current_idx as usize)
                // Caller checks the index so this shouldn't be reached
                .ok_or(LoadMediaError::IndexOutOfBounds)?
                .to_media_item(),
            MediaSource::Raop => {
                warn!("Cannot load RAOP source");
                return Ok(());
            }
            MediaSource::AirPlayMirror { .. } => {
                // The mirror URI is set directly in the MirrorStarted handler,
                // not through the media-item load path.
                warn!("Cannot load AirPlay mirror source as a media item");
                return Ok(());
            }
        };

        let container = item.container;
        let mut url = match item.url {
            Some(url) => url,
            None => {
                let Some(content) = item.content else {
                    return Err(LoadMediaError::NoUrlOrContent);
                };
                let content_type = match container.as_str() {
                    "application/dash+xml" => "application/dash+xml",
                    "application/vnd.apple.mpegurl" | "audio/mpegurl" => "application/x-hls",
                    other => {
                        return Err(LoadMediaError::InvalidContentContainer(other.to_owned()));
                    }
                };
                let b64_content = base64::engine::general_purpose::STANDARD.encode(content);
                format!("data:{content_type};base64,{b64_content}")
            }
        };
        let volume = item.volume.map(|v| v as f32);
        let start_position = item
            .time
            .and_then(|s| gst::ClockTime::try_from_seconds_f64(s).ok())
            .unwrap_or(gst::ClockTime::ZERO);
        let playback_rate = item.speed.unwrap_or(1.0) as f32;
        let headers = item.headers;

        self.have_audio_track_cover = false;
        let mut is_for_sure_live = false;
        if container == "application/x-whep" {
            url = url.replace("http://", "fcastwhep://");
            is_for_sure_live = true;
        } else if container == "application/x-fwebrtc" {
            is_for_sure_live = true;
        }

        self.on_playing_command = None;

        let player_variant = if container.starts_with("image/") {
            UiPlayerVariant::Image
        } else if container.starts_with("audio/")
            // Video streams are audio only until proven otherwise
            || container.starts_with("video/")
            || container == "application/x-whep"
            || container == "application/dash+xml"
            || container == "application/vnd.apple.mpegurl"
            || container == "application/x-fwebrtc"
        {
            UiPlayerVariant::Audio
        } else {
            UiPlayerVariant::Unknown
        };

        match player_variant {
            UiPlayerVariant::Image => {
                self.cleanup_playback_data(ContinueToPlay::Yes, PreservePlaylist::Yes)
            }
            UiPlayerVariant::Unknown | UiPlayerVariant::Audio | UiPlayerVariant::Video => {
                self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::Yes)
            }
            UiPlayerVariant::Raop => (),
        }

        self.window_visible_before_playing = Some(self.gui.set_window_visibility(true));
        #[cfg(not(target_os = "android"))]
        if !self.settings.cli.no_fullscreen_player {
            // If the window was hidden, it takes some time before it can be fullscreened.
            self.gui.wait_for_is_visible();
            self.window_fullscreen_before_playing = Some(self.gui.set_fullscreen(true));
        }

        let mut media_title = None;
        if !self.settings.cli.headless
            && let Some(v3::MetadataObject::Generic {
                title,
                thumbnail_url: Some(thumbnail_url),
                ..
            }) = item.metadata
        {
            media_title = title;
            self.have_audio_track_cover = true;
            self.current_image_download_id += 1;
            let this_id = self.current_image_download_id;
            self.current_media
                .as_mut()
                .ok_or(LoadMediaError::NoItem)?
                .pending_thumbnail_download = Some(this_id);
            self.image_downloader
                .queue_download(this_id, thumbnail_url, headers.clone());
        }

        let mut is_image = false;
        if container.starts_with("image/") {
            self.current_image_download_id += 1;
            let id = self.current_image_download_id;
            is_image = true;
            self.image_downloader
                .queue_download(id, url.clone(), headers.clone());
        } else {
            self.player.set_uri(&url);
            if let Some(volume) = volume {
                self.player.set_volume(volume);
            }

            if !is_for_sure_live {
                self.on_playing_command = Some(OnMediaLoadedOperation {
                    seek_position: start_position,
                    seek_rate: playback_rate,
                });
            }
        }

        self.have_media_title = media_title.is_some();

        self.gui.set_player_type(player_variant);
        if !is_image {
            self.gui.set_app_state(AppState::LoadingMedia);
        }
        if let Some(title) = media_title {
            self.gui.set_media_title(title);
        }

        self.current_media_item_id += 1;

        if is_image {
            tokio::spawn({
                let id = self.current_media_item_id;
                let msg_tx = self.msg_tx.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    msg_tx.send(Message::ShouldSetLoadingStatus(id));
                }
            });
        }
        self.is_loading_media = true;
        *self.current_request_headers.lock() = headers;

        self.screensaver_inhibitor.inhibit("Media playback");

        Ok(())
    }

    fn handle_playlist_play_request(&mut self, play_message: &v3::PlayMessage) {
        if let Some(url) = play_message.url.as_ref() {
            let url = url.clone();
            let mut play_message = play_message.clone();
            let msg_tx = self.msg_tx.clone();
            let client = self.http_client.clone();
            tokio::spawn(async move {
                let mut request = client.get(url);
                if let Some(headers) = play_message.headers.as_ref() {
                    request = request.headers(map_to_header_map(headers));
                }
                let mut result = None;
                match request.send().await {
                    Ok(resp) => match resp.text().await {
                        Ok(json) => {
                            play_message.content = Some(json);
                            result = Some(play_message);
                        }
                        Err(err) => {
                            error!(?err, "Failed to convert response to text");
                        }
                    },
                    Err(err) => {
                        error!(?err, "Failed to download playlist json data");
                    }
                }

                msg_tx.send(Message::PlaylistDataResult {
                    play_message: result,
                });
            });
        } else if play_message.content.is_some() {
            self.msg_tx.send(Message::PlaylistDataResult {
                play_message: Some(play_message.clone()),
            });
        } else {
            error!("Cannot load playlist since there's no URL or content");
        }
    }

    fn video_stream_available(&self) -> Result<()> {
        if !self.is_playing() {
            debug!("Ignoring old video stream available event");
            return Ok(());
        };

        debug!("Video stream available");

        self.gui.set_player_type(UiPlayerVariant::Video);

        Ok(())
    }

    fn video_stream_unavailable(&self) {
        if !self.is_playing() {
            debug!("Ignoring old video stream unavailable event");
            return;
        };

        debug!("Video stream unavailable");

        self.gui.set_player_type(UiPlayerVariant::Audio);
    }

    fn stop_playback(&mut self) {
        tracing::info!(is_playing = self.is_playing());
        if self.is_playing() {
            self.player.stop();
            self.gui.set_app_state(AppState::Idle);
            self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::No);
            self.current_media = None;
            self.screensaver_inhibitor.un_inhibit();
        }
    }

    /// `relay` controls whether a successful selection is forwarded to the
    /// other senders as a `QueueItemSelected`. It must be `false` for implicit
    /// selections (e.g. the initial item of a freshly loaded queue), since the
    /// triggering `Load` is relayed on its own.
    fn play_queue_item(&mut self, origin: PacketOrigin, position: v4::QueuePosition, relay: bool) {
        let Some(queue) = self.queue_mut() else {
            error!("Cannot play a queue item when there's no active queue");
            self.send_error(origin, ErrorKind::InvalidState);
            return;
        };

        let index = match position {
            v4::QueuePosition::Index(idx) => idx,
            v4::QueuePosition::Front => 0,
            v4::QueuePosition::Back => queue.items.len().saturating_sub(1) as u8,
        };

        if queue.items.is_empty() || index as usize >= queue.items.len() {
            error!(index, "Requested queue item index does not exist");
            self.send_error(origin, ErrorKind::QueuePositionOutOfRange);
            return;
        }

        debug!(?index, "Selecting queue item");
        queue.current_idx = index;

        self.load_media();

        if relay {
            self.relay_to_other_senders(
                origin,
                fcast_protocol::v4::MessageBuilder::new().queue_select(position),
            );
        }
    }

    #[tracing::instrument(skip_all)]
    fn remove_queue_item(&mut self, origin: PacketOrigin, position: v4::QueuePosition) {
        let Some(queue) = self.queue_mut() else {
            error!("Cannot play a queue item when there's no active queue");
            self.send_error(origin, ErrorKind::InvalidState);
            return;
        };

        let idx = match position {
            v4::QueuePosition::Index(idx) => idx as usize,
            v4::QueuePosition::Front => 0,
            v4::QueuePosition::Back => queue.items.len().saturating_sub(1),
        };

        if queue.items.is_empty() || idx >= queue.items.len() {
            error!(idx, "Invalid index");
            self.send_error(origin, ErrorKind::QueuePositionOutOfRange);
            return;
        }

        if idx == queue.current_idx as usize {
            error!(idx, "Cannot remove the currently playing item");
            self.send_error(origin, ErrorKind::QueueRemovePlayingItem);
            return;
        }

        if idx <= queue.current_idx as usize {
            queue.current_idx = queue.current_idx.saturating_sub(1);
        }

        queue.items.remove(idx);

        self.relay_to_other_senders(
            origin,
            fcast_protocol::v4::MessageBuilder::new().queue_remove(position),
        );
    }

    // TODO: caching
    #[tracing::instrument(skip_all)]
    fn insert_queue_item(&mut self, origin: PacketOrigin, insert: fcast::QueueInsertCell) {
        let Some(queue) = self.queue_mut() else {
            error!("Cannot play a queue item when there's no active queue");
            self.send_error(origin, ErrorKind::InvalidState);
            return;
        };

        if queue.items.len() >= u8::MAX as usize + 1 {
            error!("Cannot insert into the queue because it's full");
            self.send_error(origin, ErrorKind::QueueFull);
            return;
        }

        let insert = insert.borrow_dependent();
        let idx = match insert.position_type() {
            v4::flat::QueuePosition::Back => queue.items.len(),
            v4::flat::QueuePosition::Front => 0,
            v4::flat::QueuePosition::Index => {
                let Some(idx) = insert.position_as_index() else {
                    error!("Queue insert position is missing its index");
                    self.send_error(origin, ErrorKind::MalformedBody);
                    return;
                };

                idx.index() as usize
            }
            _ => {
                error!(position = ?insert.position_type(), "Invalid queue position");
                self.send_error(origin, ErrorKind::MalformedBody);
                return;
            }
        };

        if queue.items.is_empty() || idx > queue.items.len() {
            error!(idx, "Invalid index");
            self.send_error(origin, ErrorKind::QueuePositionOutOfRange);
            return;
        }

        if idx <= queue.current_idx as usize {
            queue.current_idx += 1;
        }

        queue
            .items
            .insert(idx, QueueItem::from_flat(&insert.item()));

        if let Some(relay_msg) =
            fcast_protocol::v4::MessageBuilder::new().from_queue_insert_stripped(insert)
        {
            self.relay_to_other_senders(origin, relay_msg);
        }
    }

    fn pause(&mut self) {
        if self.is_playing() {
            self.player.pause();
        }
    }

    fn resume(&mut self) {
        if self.is_playing() {
            self.player.play();
        }
    }

    fn handle_play_message(&mut self, msg: WrappedPlayMessage, origin: PacketOrigin) {
        let play_data = Arc::new(msg);
        match play_data.as_ref() {
            fcast::WrappedPlayMessage::Legacy(msg) => {
                if msg.container == "application/json" {
                    self.handle_playlist_play_request(msg);
                } else {
                    self.current_media = Some(MediaSourceState::new(
                        origin,
                        MediaSource::Single(Arc::clone(&play_data)),
                    ));
                    self.load_media();
                }

                if self.should_broadcast() {
                    let msg = v3::PlayUpdateMessage {
                        generation_time: Some(current_time_millis()),
                        play_data: Some(msg.clone()),
                    };
                    self.broadcast_update(ReceiverToSenderMessage::PlayUpdate { msg })
                }
            }
            fcast::WrappedPlayMessage::V4(inner) => {
                let play = inner.borrow_dependent();
                match play.source_type() {
                    v4::flat::MediaSource::Single => {
                        self.current_media = Some(MediaSourceState::new(
                            origin,
                            MediaSource::Single(Arc::clone(&play_data)),
                        ));
                        self.load_media();
                    }
                    v4::flat::MediaSource::Queue => {
                        let Some(queue) = play.source_as_queue() else {
                            self.send_error(origin, ErrorKind::MalformedBody);
                            return;
                        };
                        let items = queue.items();
                        let mut queue_items = Vec::new();
                        for item in items {
                            queue_items.push(QueueItem::from_flat(&item));
                        }
                        let idx = queue.start_index().unwrap_or(0);
                        self.current_media = Some(MediaSourceState::new(
                            origin,
                            MediaSource::Queue(QueueState {
                                items: queue_items,
                                current_idx: idx,
                            }),
                        ));
                        self.play_queue_item(origin, v4::QueuePosition::Index(idx), false);
                    }
                    _ => {
                        error!(source_type = ?play.source_type(), "Got play message with invalid source type");
                        self.send_error(origin, ErrorKind::MalformedBody);
                    }
                }

                match origin {
                    PacketOrigin::FCast {
                        sender_id,
                        packet_num: _,
                    } => {
                        if self.should_broadcast()
                            && let Some(stripped) =
                                fcast_protocol::v4::MessageBuilder::new().from_play_stripped(play)
                        {
                            debug!("Sending play message to active sesssions");
                            self.broadcast_update(ReceiverToSenderMessage::V4(
                                fcast::V4Message::Play {
                                    initiator_session_id: sender_id,
                                    serialized_msg: stripped,
                                },
                            ));
                        }
                    }
                    _ => (),
                }
            }
            fcast::WrappedPlayMessage::Chromecast(_) => {
                self.current_media = Some(MediaSourceState::new(
                    origin,
                    MediaSource::Single(Arc::clone(&play_data)),
                ));
                self.load_media();
            }
        }
    }

    fn handle_operation(&mut self, op: Operation, origin: PacketOrigin) -> Result<bool> {
        match op {
            Operation::Pause => self.pause(),
            Operation::Resume => self.resume(),
            Operation::Stop => self.stop_playback(),
            Operation::Seek(time) => {
                if self.is_playing() {
                    match self.current_duration {
                        Some(duration) if duration > gst::ClockTime::ZERO && time > duration => {
                            self.send_error(origin, ErrorKind::SeekOutOfRange);
                            self.player.seek(duration);
                        }
                        _ => self.player.seek(time),
                    }
                }
            }
            Operation::SetSpeed(rate) => {
                self.player.set_rate(rate);
            }
            Operation::SetPlaylistItem(msg) => {
                debug!(?msg, "Set playlist item");
                let new_index = msg.item_index as usize;
                if let Some(current_media) = self.current_media.as_mut()
                    && let MediaSource::Playlist { content, index } = &mut current_media.source
                {
                    if new_index >= content.items.len() {
                        error!(new_index, "Playlist item not found");
                        return Ok(false);
                    }
                    *index = new_index;
                } else {
                    error!("Cannot set playlist item when no playlist is loaded");
                    return Ok(false);
                }

                self.load_media();
                self.gui.set_playlist_index(new_index as i32);
            }
            Operation::SetVolume(volume) => {
                self.player.set_volume(volume);
                self.gui.set_volume(volume);
            }
            Operation::StartMirroringSession {
                tx: client_tx,
                offer_rx,
            } => {
                let chan = fwebrtcsrc::SignallingChannel {
                    tx: client_tx.0,
                    offer_rx,
                };
                *self.signalling_channel.lock() = Some(chan);
                let play_message = v3::PlayMessage {
                    container: "application/x-fwebrtc".to_owned(),
                    url: Some("fwebrtc://fake.host".to_owned()),
                    content: None,
                    time: None,
                    volume: None,
                    speed: None,
                    headers: None,
                    metadata: None,
                };
                self.current_media = Some(MediaSourceState::new(
                    origin,
                    MediaSource::Single(Arc::new(fcast::WrappedPlayMessage::Legacy(play_message))),
                ));
                self.load_media();
            }
            Operation::SetPlaybackState(state) => match state {
                fcast_protocol::v4::PlaybackState::Paused => {
                    if self.is_playing() {
                        self.player.pause();
                    }
                }
                fcast_protocol::v4::PlaybackState::Playing => {
                    if self.is_playing() {
                        self.player.play();
                    }
                }
                fcast_protocol::v4::PlaybackState::Idle
                | fcast_protocol::v4::PlaybackState::Ended => {
                    self.stop_playback();
                }
                _ => (),
            },
            Operation::PlayNew(msg) => {
                self.handle_play_message(msg, origin);
            }
            Operation::ChangeTrack { id, typ } => {
                debug!(id, ?typ, "changing track");

                let stream_type = match typ {
                    v4::flat::MediaTrackType::Video => gst::StreamType::VIDEO,
                    v4::flat::MediaTrackType::Audio => gst::StreamType::AUDIO,
                    v4::flat::MediaTrackType::Subtitle => gst::StreamType::TEXT,
                    _ => {
                        error!(?typ, "Unknown track type");
                        self.send_error(origin, ErrorKind::MalformedBody);
                        return Ok(false);
                    }
                };

                if let Some(id) = id
                    && !self.player.is_stream_of_type(id, stream_type)
                {
                    error!(id, ?typ, "Track id is not a track of the requested type");
                    self.send_error(origin, ErrorKind::MalformedBody);
                    return Ok(false);
                }

                // playsink cannot present a text stream without a video
                // stream, so selecting a subtitle while video is deselected
                // would either error the pipeline or be silently dropped.
                // Report it as unsatisfiable instead.
                if matches!(typ, v4::flat::MediaTrackType::Subtitle)
                    && id.is_some()
                    && self.player.current_video_stream.is_none()
                {
                    error!("Cannot select a subtitle track while video is disabled");
                    self.send_error(origin, ErrorKind::InvalidState);
                    return Ok(false);
                }

                let res = match typ {
                    v4::flat::MediaTrackType::Video => self.player.select_video_stream(id),
                    v4::flat::MediaTrackType::Audio => self.player.select_audio_stream(id),
                    v4::flat::MediaTrackType::Subtitle => self.player.select_subtitle_stream(id),
                    _ => unreachable!(),
                };

                if let Err(err) = res {
                    error!(?err, "Failed to change track");
                    self.send_error(origin, ErrorKind::Internal);
                } else if matches!(typ, v4::flat::MediaTrackType::Subtitle) {
                    // Refresh once the selection is actually applied (see the
                    // StreamsSelected handler), so the sparse subtitle change
                    // takes effect immediately. Deferring to StreamsSelected
                    // avoids racing the flush against the still-in-flight stream
                    // reconfiguration.
                    self.pending_subtitle_refresh = true;
                }
            }
            Operation::SelectQueueItem(position) => {
                self.play_queue_item(origin, position, true);
            }
            Operation::RemoveQueueItem(position) => {
                self.remove_queue_item(origin, position);
            }
            Operation::InsertQueueItem(insert) => {
                self.insert_queue_item(origin, insert);
            }
            Operation::SetProgressUpdateInterval(interval) => {
                if let PacketOrigin::FCast { sender_id, .. } = origin
                    && let Some(handle) = self.fcast_senders.get_mut(&sender_id)
                {
                    debug!(?interval, sender_id, "Updating progress update interval");
                    handle.progress_interval = interval;
                    handle.last_progress_update = Instant::now();
                }
            }
            Operation::ResumeOrPause => match self.player.player_state() {
                PlayerState::Paused => self.resume(),
                PlayerState::Playing => self.pause(),
                _ => {
                    error!(
                        "Cannot resume or pause in player current state: {:?}",
                        self.player.player_state(),
                    );
                    self.send_error(origin, ErrorKind::InvalidState);
                    return Ok(false);
                }
            },
        }

        Ok(false)
    }

    fn handle_mdns_event(&mut self, event: Mdns) -> Result<()> {
        match event {
            Mdns::NameSet(device_name) => {
                self.device_name = Some(device_name.clone());
                self.gui.set_local_device_name(device_name);
            }
            Mdns::IpAdded(addr) => {
                let _ = self.current_addresses.insert(addr);
            }
            Mdns::IpRemoved(addr) => {
                let _ = self.current_addresses.remove(&addr);
            }
            Mdns::SetIps(addrs) => {
                self.current_addresses.clear();
                for addr in addrs {
                    let _ = self.current_addresses.insert(addr);
                }
            }
        }

        let addrs = self
            .current_addresses
            .iter()
            .filter(|addr| {
                !addr.is_loopback() && {
                    match *addr {
                        IpAddr::V4(_) => true,
                        IpAddr::V6(v6) => !v6.is_unicast_link_local(),
                    }
                }
            })
            .map(|addr| addr.to_string())
            .collect::<SmallVec<[String; 5]>>();

        if addrs.is_empty() {
            // TODO: Reset QR
        } else if let Some(device_name) = self.device_name.clone() {
            let ips_string = addrs.join(", ");
            let net_config = fcast_protocol::FCastNetworkConfig {
                name: device_name,
                addresses: addrs.to_vec(),
                services: vec![fcast_protocol::FCastService {
                    port: FCAST_TCP_PORT,
                    r#type: 0,
                }],
                txt: Some(self.fcast_txt_records.clone()),
            };
            debug!(?net_config, "Network config for QR code created");
            let device_url = net_config.to_url()?;
            let qrcode = fast_qr::QRBuilder::new(device_url.as_bytes()).build()?;
            let dims = qrcode.size as u32;
            let mut pixbuf: gui::QrCodeImage = slint::SharedPixelBuffer::new(dims, dims);
            let pixbuf_pixels = pixbuf.make_mut_slice();
            for (idx, module) in qrcode.data[0..pixbuf_pixels.len()].iter().enumerate() {
                if *module == fast_qr::Module::LIGHT {
                    pixbuf_pixels[idx] = slint::Rgb8Pixel::new(0xFF, 0xFF, 0xFF);
                } else {
                    pixbuf_pixels[idx] = slint::Rgb8Pixel::new(0x00, 0x00, 0x00);
                }
            }

            self.gui.set_connection_details(pixbuf, ips_string);
        }

        Ok(())
    }

    fn on_media_info_updated(&mut self) {
        if self.player.seekable
            && let Some(cmd) = self.on_playing_command.take()
        {
            self.player
                .seek_and_set_rate(cmd.seek_position, cmd.seek_rate);
        }
    }

    fn update_tracks(&mut self, force_update: bool) {
        if !force_update && !self.player.update_stream_properties() {
            return;
        }

        if self.should_broadcast() {
            let serialized_msg = v4::MessageBuilder::new().tracks_available(
                self.player
                    .streams
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, s)| {
                        let typ = s.inner.stream_type();

                        let metadata = if typ.contains(gst::StreamType::VIDEO) {
                            Some(v4::MediaTrackMetadata::Video)
                        } else if typ.contains(gst::StreamType::AUDIO) {
                            Some(v4::MediaTrackMetadata::Audio)
                        } else if typ.contains(gst::StreamType::TEXT) {
                            Some(v4::MediaTrackMetadata::Subtitle)
                        } else {
                            return None;
                        };

                        let (title, iso_639) = if let Some(tags) = s.inner.tags() {
                            (
                                tags.get::<gst::tags::Title>()
                                    .map(|t| smol_str::SmolStr::new(t.get())),
                                tags.get::<gst::tags::LanguageCode>()
                                    .map(|t| SmolStr::new(t.get())),
                            )
                        } else {
                            (None, None)
                        };

                        Some(v4::MediaTrack {
                            id: idx as u32,
                            title,
                            iso_639: iso_639.unwrap_or(SmolStr::new("und")),
                            metadata,
                        })
                    }),
            );
            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::TracksAvailable { serialized_msg },
            ));
        }

        let mut videos = Vec::new();
        let mut audios = Vec::new();
        let mut subtitles = Vec::new();
        for (idx, stream) in self.player.streams.iter().enumerate() {
            let typ = stream.inner.stream_type();
            let dst = if typ.contains(gst::StreamType::VIDEO) {
                Some(&mut videos)
            } else if typ.contains(gst::StreamType::AUDIO) {
                Some(&mut audios)
            } else if typ.contains(gst::StreamType::TEXT) {
                Some(&mut subtitles)
            } else {
                None
            };

            if let Some(dst) = dst {
                dst.push(UiMediaTrack {
                    id: idx as i32,
                    name: stream.title.to_shared_string(),
                });
            }
        }

        self.gui.set_tracks(videos, audios, subtitles);
    }

    fn handle_player_event(&mut self, event: player::PlayerEvent) -> Result<()> {
        match event {
            player::PlayerEvent::EndOfStream => {
                self.player.end_of_stream_reached();

                debug!("Player reached EOS");

                self.media_ended();

                // TODO: this should be the last message sent regarding the media currently being played
                if self.should_broadcast()
                    && let Some(current_media) = self.current_media.as_ref()
                {
                    match &current_media.source {
                        MediaSource::Single(play_msg) => match play_msg.as_ref() {
                            fcast::WrappedPlayMessage::Legacy(msg) => {
                                let event = v3::EventMessage {
                                    generation_time: current_time_millis(),
                                    event: v3::EventObject::MediaItem {
                                        variant: v3::EventType::MediaItemEnd,
                                        item: msg.clone().into(),
                                    },
                                };
                                self.broadcast_update(ReceiverToSenderMessage::Event {
                                    msg: event,
                                });
                            }
                            fcast::WrappedPlayMessage::V4(_) => {
                                self.broadcast_update(ReceiverToSenderMessage::V4(
                                    fcast::V4Message::PlaybackStateChanged(
                                        fcast_protocol::v4::PlaybackState::Ended,
                                    ),
                                ));
                            }
                            fcast::WrappedPlayMessage::Chromecast(_) => (),
                        },
                        MediaSource::Queue(_) => {
                            self.broadcast_update(ReceiverToSenderMessage::V4(
                                fcast::V4Message::PlaybackStateChanged(
                                    fcast_protocol::v4::PlaybackState::Ended,
                                ),
                            ));
                        }
                        MediaSource::Playlist { .. }
                        | MediaSource::Raop
                        | MediaSource::AirPlayMirror { .. } => (),
                    }
                }
            }
            player::PlayerEvent::Tags(tags) => {
                let Some(has_pending_thumbnail) = self
                    .current_media
                    .as_ref()
                    .map(|m| m.pending_thumbnail.is_some())
                else {
                    error!("Received tags from player when no media is loaded");
                    return Ok(());
                };

                if !self.settings.cli.headless
                    && !self.have_audio_track_cover
                    && let Some(cover) = tags.get::<gst::tags::Image>()
                    && let Some(buffer) = cover.get().buffer()
                    && let Ok(buffer) = buffer.map_readable()
                    && !has_pending_thumbnail
                {
                    self.current_thumbnail_id += 1;
                    let this_id = self.current_thumbnail_id;
                    self.image_decoder.queue_job(
                        this_id,
                        image::ImageDecodeJob::new_no_format(
                            buffer.to_vec(),
                            image::ImageDecodeJobType::AudioThumbnail,
                        ),
                    );
                    if let Some(current_media) = self.current_media.as_mut() {
                        current_media.pending_thumbnail = Some(this_id);
                    }
                }

                if !self.have_media_title
                    && let Some(title) = tags.get::<gst::tags::Title>()
                {
                    self.have_media_title = true;
                    self.gui.set_media_title(title.get().to_owned());
                }

                if let Some(artist) = tags.get::<gst::tags::Artist>() {
                    self.gui.set_artist_name(artist.get().to_owned());
                }
            }
            player::PlayerEvent::VolumeChanged(volume) => {
                self.player.volume_changed();

                if self.should_broadcast() {
                    let update = VolumeUpdateMessage {
                        generation_time: current_time_millis(),
                        volume,
                    };

                    let msg = ReceiverToSenderMessage::LegacyTranslatable {
                        op: Opcode::VolumeUpdate,
                        msg: TranslatableMessage::VolumeUpdate(update),
                    };
                    self.updates_tx.send(Arc::new(msg))?;
                    self.last_sent_update = Instant::now();

                    self.broadcast_update(ReceiverToSenderMessage::V4(
                        fcast::V4Message::VolumeChanged(volume as f32),
                    ));
                }

                self.gcast_tx.send(gcast::StatusUpdate::Volume(volume));
            }
            player::PlayerEvent::StreamCollection(collection) => {
                self.player
                    .handle_stream_collection(collection, self.msg_tx.clone());
                // self.media_loaded_successfully();

                self.player.update_media_info();
                self.on_media_info_updated();

                self.gui.set_app_state(AppState::Playing);

                // self.current_duration = info.duration();
                // if info.number_of_video_streams() > 0 {
                //     self.video_stream_available()?;
                // }

                self.player.play();

                self.update_tracks(true);

                if !self.have_media_info {
                    self.media_loaded_successfully();
                    self.have_media_info = true;
                }
            }
            player::PlayerEvent::AboutToFinish => {}
            player::PlayerEvent::AsyncDone => {
                if self.player.have_media_info()
                    && self.player.player_state() != PlayerState::Playing
                {
                    self.playback_progress_changed();
                }
            }
            player::PlayerEvent::Buffering(percent) => {
                if self.player.buffering(percent) {
                    self.notify_updates(true)?;
                    self.playback_state_changed(fcast_protocol::v4::PlaybackState::Buffering);
                }
            }
            player::PlayerEvent::IsLive => {
                self.player.set_is_live(true);
            }
            player::PlayerEvent::StateChanged {
                old,
                current,
                pending,
            } => {
                if self.player.state_changed(old, current, pending).is_some() {
                    self.notify_updates(true)?;
                    let v4_state = match self.player.player_state() {
                        PlayerState::Paused => fcast_protocol::v4::PlaybackState::Paused,
                        PlayerState::Playing => fcast_protocol::v4::PlaybackState::Playing,
                        PlayerState::Buffering => fcast_protocol::v4::PlaybackState::Buffering,
                        PlayerState::Stopped => fcast_protocol::v4::PlaybackState::Idle,
                    };
                    self.playback_state_changed(v4_state);
                }

                let first_paused = old == gst::State::Ready
                    && current == gst::State::Paused
                    && pending == gst::State::VoidPending;
                let started_playing =
                    current == gst::State::Playing && pending == gst::State::VoidPending;
                // Try to get duration
                if first_paused || started_playing {
                    self.current_duration = self.player.get_duration();
                    if self.current_duration.is_some() && self.should_broadcast() {
                        self.playback_progress_changed();
                    }
                }

                self.gcast_tx
                    .send(gcast::StatusUpdate::PlayerState(self.player.player_state()));

                if (old == gst::State::Ready
                    && current == gst::State::Paused
                    && pending == gst::State::VoidPending)
                    || (old == gst::State::Paused
                        && current == gst::State::Playing
                        && pending == gst::State::VoidPending)
                {
                    // pre-rolled
                    self.player.update_media_info();
                    self.on_media_info_updated();
                }
            }
            player::PlayerEvent::UriLoaded => {
                if !self.is_playing() {
                    debug!("Ignoring stale UriLoaded (nothing is loaded)");
                    return Ok(());
                }
                self.player.uri_loaded();
            }
            player::PlayerEvent::RequestState(state) => self.player.request_state(state),
            player::PlayerEvent::QueueSeek(seek) => self.player.queue_seek(seek),
            player::PlayerEvent::StreamsSelected {
                video,
                audio,
                subtitle,
            } => {
                let (video_sid, audio_sid, subtitle_sid) = self.player.streams_selected(
                    video.as_deref(),
                    audio.as_deref(),
                    subtitle.as_deref(),
                );
                self.gui.set_track_ids(
                    video_sid.map(|i| i as i32).unwrap_or(-1),
                    audio_sid.map(|i| i as i32).unwrap_or(-1),
                    subtitle_sid.map(|i| i as i32).unwrap_or(-1),
                );

                // The selection is now applied, so it is safe to act without
                // racing the reconfiguration.
                // - Selecting/switching a track: flush at the current position so
                //   the sparse subtitle track renders its current cue right away.
                // - Disabling: never seek. Flushing right after the text branch
                //   teardown can fail allocation renegotiation (observed with
                //   vavp8dec: "DMABuf caps negotiated without VideoMeta" ->
                //   not-negotiated -> pipeline error).
                if std::mem::take(&mut self.pending_subtitle_refresh) {
                    if subtitle_sid.is_some() {
                        self.player.refresh_position();
                    } else {
                        self.gui.clear_video_overlays();
                    }
                }

                if video.is_some() {
                    self.video_stream_available()?;
                } else {
                    self.video_stream_unavailable();
                }

                if self.updates_tx.strong_count() > 0 {
                    let msgs = vec![
                        v4::MessageBuilder::new()
                            .change_track(video_sid, v4::flat::MediaTrackType::Video),
                        v4::MessageBuilder::new()
                            .change_track(audio_sid, v4::flat::MediaTrackType::Audio),
                        v4::MessageBuilder::new()
                            .change_track(subtitle_sid, v4::flat::MediaTrackType::Subtitle),
                    ];
                    let _ = self.updates_tx.send(Arc::new(ReceiverToSenderMessage::V4(
                        fcast::V4Message::TracksSelected(msgs),
                    )));
                }
            }
            player::PlayerEvent::SeekFailed => {
                self.player.seek_failed();
            }
            player::PlayerEvent::ClockLost => {
                self.player.recover_clock();
            }
            player::PlayerEvent::RateChanged(new_rate) => {
                self.player.set_rate_changed(new_rate);
                self.notify_updates(true)?;
                if self.updates_tx.strong_count() > 0 {
                    let _ = self.updates_tx.send(Arc::new(ReceiverToSenderMessage::V4(
                        fcast::V4Message::PlaybackRateChanged(new_rate as f32),
                    )));
                }
            }
            player::PlayerEvent::Error {
                kind,
                message,
                failed_uri,
            } => {
                #[cfg(debug_assertions)]
                self.player.dump_graph(remote_pipeline_dbg::Trigger::Error);
                if let Some(failed_uri) = &failed_uri
                    && self.current_item_uri() != Some(failed_uri.as_str())
                {
                    debug!(failed_uri, "Dropping error from a superseded load");
                } else {
                    self.player.stop();
                    if let Some(origin) = self.current_media.as_ref().map(|m| m.origin) {
                        self.send_error(origin, media_error_kind_to_error(kind));
                    }
                    self.media_error(message)?;
                }
            }
            player::PlayerEvent::Warning(msg) => {
                #[cfg(debug_assertions)]
                self.player
                    .dump_graph(remote_pipeline_dbg::Trigger::Warning);
                self.media_warning(msg)?;
            }
            player::PlayerEvent::StreamTagsUpdated => {
                self.update_tracks(false);
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn handle_raop_event(&mut self, event: Raop) -> Result<bool> {
        match event {
            Raop::ConfigAvailable(config) => {
                let run_raop = if cfg!(not(target_os = "android")) {
                    !self.settings.cli.no_raop
                } else {
                    true
                };

                if run_raop && self.raop_server.is_none() {
                    info!(?config, "Starting raop server");

                    let msg_tx = self.msg_tx.clone();
                    tokio::spawn(async move {
                        // IpV4 only
                        let listener = tokio::net::TcpListener::bind("0.0.0.0:33505")
                            .await
                            .unwrap();

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            msg_tx.raop(Raop::SenderConnected(stream));
                        }
                    });
                    self.raop_server = Some(RaopServer { config });
                }
            }
            Raop::SenderConnected(stream) => {
                if self.current_media.is_some() {
                    warn!("Rejecting RAOP sender because media is already loaded");
                    return Ok(false);
                }

                let Some(server) = self.raop_server.as_ref() else {
                    error!("No server is running");
                    return Ok(false);
                };

                let config = server.config.clone();
                let msg_tx = self.msg_tx.clone();
                tokio::spawn(async move {
                    raop::handle_sender(stream, config, msg_tx.clone()).await;
                    msg_tx.raop(Raop::SenderDisconnected);
                });

                debug!("Session started");
                self.current_media =
                    Some(MediaSourceState::new(PacketOrigin::Raop, MediaSource::Raop));

                self.gui.set_app_state(AppState::Playing);
                self.gui.set_player_type(UiPlayerVariant::Raop);
            }
            Raop::SenderDisconnected => {
                debug!("Session ended");
                self.current_media = None;
                self.gui.set_app_state(AppState::Idle);
                self.gui.set_player_type(UiPlayerVariant::Unknown);
                self.gui.clear_common_playback_state();
            }
            Raop::CoverArtSet(data) => {
                self.current_thumbnail_id += 1;
                let this_id = self.current_thumbnail_id;
                self.image_decoder.queue_job(
                    this_id,
                    image::ImageDecodeJob::new_no_format(
                        data,
                        image::ImageDecodeJobType::AudioThumbnail,
                    ),
                );
                match self.current_media.as_mut() {
                    Some(current_media) => {
                        current_media.pending_thumbnail = Some(this_id);
                    }
                    None => error!("Got CoverArtSet event but no media is currently loaded"),
                }
            }
            Raop::CoverArtRemoved => self.gui.clear_audio_covers(),
            Raop::MetadataSet(metadata) => {
                if let Some(title) = metadata.title {
                    self.gui.set_media_title(title);
                }
                if let Some(name) = metadata.artist {
                    self.gui.set_artist_name(name);
                }
            }
            Raop::ProgressUpdate {
                position_sec,
                duration_sec,
            } => self
                .gui
                .update_playback_progress(position_sec as f32, duration_sec as f32),
        }

        Ok(false)
    }

    #[cfg(feature = "airplay")]
    fn is_current_airplay_mirror(&self, stream_connection_id: u64) -> bool {
        matches!(
            self.current_media.as_ref().map(|m| &m.source),
            Some(MediaSource::AirPlayMirror { stream_connection_id: id })
                if *id == stream_connection_id
        )
    }

    #[cfg(feature = "airplay")]
    fn handle_airplay_event(&mut self, event: AirPlay) -> Result<bool> {
        match event {
            AirPlay::ConfigAvailable(config) => {
                let run_airplay = if cfg!(not(target_os = "android")) {
                    !self.settings.cli.no_airplay
                } else {
                    true
                };

                if run_airplay && self.airplay_server.is_none() {
                    info!(?config, "Starting airplay server");

                    let msg_tx = self.msg_tx.clone();
                    tokio::spawn(async move {
                        // IpV4 only
                        let listener =
                            tokio::net::TcpListener::bind(("0.0.0.0", airplay::AIRPLAY_TCP_PORT))
                                .await
                                .unwrap();

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            msg_tx.airplay(AirPlay::SenderConnected(stream));
                        }
                    });
                    self.airplay_server = Some(AirPlayServer { config });
                }
            }
            AirPlay::SenderConnected(stream) => {
                let Some(server) = self.airplay_server.as_ref() else {
                    error!("No airplay server is running");
                    return Ok(false);
                };

                let config = server.config.clone();
                let msg_tx = self.msg_tx.clone();
                let airplay_context = self.airplay_context.clone();
                tokio::spawn(async move {
                    airplay::handle_sender(stream, config, msg_tx, airplay_context).await;
                });
            }
            AirPlay::MirrorStarted {
                stream_connection_id,
            } => {
                let busy_with_other = self
                    .current_media
                    .as_ref()
                    .is_some_and(|m| !matches!(m.source, MediaSource::AirPlayMirror { .. }));
                if busy_with_other {
                    warn!(
                        stream_connection_id,
                        "Refusing AirPlay mirror: other media is already playing"
                    );
                    self.airplay_context.end_session(stream_connection_id);
                    return Ok(false);
                }

                let uri = airplay::source::mirror_uri(stream_connection_id);
                debug!(%uri, "Starting AirPlay mirror playback");
                self.player.set_uri(&uri);
                self.player.play();
                self.current_media = Some(MediaSourceState::new(
                    PacketOrigin::AirPlay,
                    MediaSource::AirPlayMirror {
                        stream_connection_id,
                    },
                ));
                self.gui.set_app_state(AppState::Playing);
                self.gui.set_player_type(UiPlayerVariant::Video);
            }
            AirPlay::MirrorPaused {
                stream_connection_id,
            } => {
                if self.is_current_airplay_mirror(stream_connection_id) {
                    debug!(stream_connection_id, "Pausing AirPlay mirror playback");
                    self.player.pause();
                }
            }
            AirPlay::MirrorResumed {
                stream_connection_id,
            } => {
                if self.is_current_airplay_mirror(stream_connection_id) {
                    debug!(stream_connection_id, "Resuming AirPlay mirror playback");
                    self.player.play();
                }
            }
            AirPlay::VolumeChanged {
                stream_connection_id,
                volume,
            } => {
                if self.is_current_airplay_mirror(stream_connection_id) {
                    debug!(
                        stream_connection_id,
                        volume, "Setting AirPlay mirror volume"
                    );
                    self.player.set_volume(volume);
                    self.gui.set_volume(volume);
                }
            }
            AirPlay::MirrorStopped {
                stream_connection_id,
            } => {
                if self.is_current_airplay_mirror(stream_connection_id) {
                    debug!(stream_connection_id, "Stopping AirPlay mirror playback");
                    self.stop_playback();
                    self.gui.set_player_type(UiPlayerVariant::Unknown);
                }
            }
        }

        Ok(false)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn handle_app_update_event(&mut self, event: message::AppUpdate) -> Result<bool> {
        match event {
            message::AppUpdate::UpdateAvailable(release) => {
                self.update = Some(release);
                self.gui
                    .set_updater_state(crate::UiUpdaterState::ShowingDialog);
            }
            message::AppUpdate::UpdateApplication => {
                let Some(update) = self.update.take() else {
                    error!("User want's to update but no updates available");
                    return Ok(false);
                };

                if let Some(gui_tx) = self.gui.tx.clone() {
                    tokio::spawn(async move {
                        let res = app_updater::download_update(UPDATER_BASE_URL, &update, {
                            let gui_tx = gui_tx.clone();
                            move |progress, total| {
                                let progress_percent = if total == 0 {
                                    0.0
                                } else {
                                    progress as f64 / total as f64
                                } * 100.0;

                                let _ =
                                    gui_tx.send(gui::UpdateGuiCommand::SetUpdateDownloadProgress(
                                        progress_percent as i32,
                                    ));
                            }
                        })
                        .await;

                        let update_file = match res {
                            Ok(update) => update,
                            Err(err) => {
                                let error_msg = err.to_shared_string();
                                let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                                    crate::UiUpdaterState::DownloadFailed,
                                ));
                                let _ =
                                    gui_tx.send(gui::UpdateGuiCommand::SetUpdaterError(error_msg));
                                return;
                            }
                        };

                        if let Err(err) = app_updater::install_update(
                            #[cfg(target_os = "macos")]
                            "FCast Receiver.app",
                            update_file,
                            Box::new(|closure| {
                                slint::invoke_from_event_loop(move || {
                                    (closure)();
                                })
                                .is_err()
                            }),
                        )
                        .await
                        {
                            error!(?err, "Failed to install update");
                            let error_msg = err.to_shared_string();
                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                                crate::UiUpdaterState::InstallFailed,
                            ));
                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdaterError(error_msg));
                            return;
                        }

                        debug!(?update, "Successfully updated");

                        let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                            crate::UiUpdaterState::InstallSuccessful,
                        ));
                    });
                }
            }
            message::AppUpdate::RestartApp => {
                debug!("Restarting app...");
                app_updater::restart_application();
            }
        }

        Ok(false)
    }

    fn handle_image_event(&mut self, event: image::Event) -> Result<bool> {
        match event {
            image::Event::DownloadResult { id, res } => {
                debug!(id, "Got image download result");

                let pending_thumbnail_download = self
                    .current_media
                    .as_ref()
                    .map(|m| m.pending_thumbnail_download)
                    .flatten();
                if Some(id) == pending_thumbnail_download {
                    match res {
                        Ok((encoded_image, format)) => {
                            self.current_thumbnail_id += 1;
                            let this_id = self.current_thumbnail_id;
                            if let Some(current_media) = self.current_media.as_mut() {
                                current_media.pending_thumbnail_download = None;
                                current_media.pending_thumbnail = Some(this_id);
                            }
                            self.image_decoder.queue_job(
                                this_id,
                                image::ImageDecodeJob::new(
                                    encoded_image,
                                    format,
                                    image::ImageDecodeJobType::AudioThumbnail,
                                ),
                            );
                        }
                        Err(err) => {
                            error!(%err, "Thumbnail image download failed");
                        }
                    }
                    return Ok(false);
                }

                if id != self.current_image_download_id {
                    warn!(id, "Ignoring old image download result");
                    return Ok(false);
                }

                match res {
                    Ok((encoded_image, format)) => {
                        self.current_image_id += 1;
                        let this_id = self.current_image_id;
                        self.image_decoder.queue_job(
                            this_id,
                            image::ImageDecodeJob::new(
                                encoded_image,
                                format,
                                image::ImageDecodeJobType::Regular,
                            ),
                        );
                    }
                    Err(err) => {
                        if let Some(origin) = self.current_media.as_ref().map(|m| m.origin) {
                            self.send_error(origin, image_download_error_kind(&err));
                        }
                        self.media_error(format!("Image download failed: {err:?}"))?;
                    }
                }
            }
            image::Event::AudioThumbnailAvailable(img) => {
                if let Some(current_media) = self.current_media.as_ref()
                    && let Some(pending_thumbnail) = current_media.pending_thumbnail
                    && pending_thumbnail == img.id
                {
                    self.gui.set_audio_track_cover(img);
                }
            }
            image::Event::Decoded(img) => {
                if img.id != self.current_image_id {
                    warn!(img.id, "Ignoring old image decode result");
                    return Ok(false);
                }

                self.gui.set_image_preview(img);
                self.gui.set_app_state(AppState::Playing);

                self.media_loaded_successfully();
            }
            image::Event::DecodedAnimation { id, frames } => {
                if id != self.current_image_id {
                    warn!(id, "Ignoring old image decode result");
                    return Ok(false);
                }

                self.gui.set_animation(frames);
                self.gui.set_app_state(AppState::Playing);

                self.media_loaded_successfully();
            }
        }

        Ok(false)
    }

    /// Returns `true` if the event loop should exit
    async fn handle_event(&mut self, event: Message) -> Result<bool> {
        match event {
            Message::SessionFinished => {
                self.gui.device_disconnected();
            }
            Message::SeekPercent(percent) => {
                debug!("SeekPercent({percent})");
                if let Some(duration) = self.current_duration {
                    if let Ok(pos) = gst::ClockTime::try_from_seconds_f64(
                        percent as f64 * duration.seconds_f64(),
                    ) {
                        return self.handle_operation(Operation::Seek(pos), PacketOrigin::Gui);
                    }
                }
            }
            Message::Quit => return Ok(true),
            Message::ToggleDebug => self.debug_mode = !self.debug_mode,
            Message::Op { origin, op } => {
                debug!(?origin, ?op, "Operation from sender");
                return self.handle_operation(op, origin);
            }
            Message::Image(event) => return self.handle_image_event(event),
            Message::Mdns(event) => {
                debug!(?event, "mDNS event");
                self.handle_mdns_event(event)?;
            }
            Message::PlaylistDataResult { play_message } => {
                let Some(play_message) = play_message else {
                    error!("Playlist failed to laod");
                    return Ok(false);
                };

                let Some(content) = play_message.content else {
                    // Unreachable
                    error!("Playlist play message is missing content");
                    return Ok(false);
                };

                let playlist = serde_json::from_str::<v3::PlaylistContent>(&content)?;

                let start_idx = match playlist.offset {
                    Some(idx) => idx as usize,
                    None => 0,
                };
                let length = playlist.items.len();

                if start_idx >= playlist.items.len() {
                    error!(
                        start_idx,
                        ?playlist,
                        "Playlist's start index is out of bounds"
                    );
                    return Ok(false);
                }

                self.current_media = Some(MediaSourceState::new(
                    PacketOrigin::Gui,
                    MediaSource::Playlist {
                        content: playlist,
                        index: start_idx,
                    },
                ));
                self.load_media();

                self.gui.update_playlist(start_idx as i32, length as i32);
            }
            Message::MediaItemFinish(id) => {
                let Some(media) = &self.current_media else {
                    return Ok(false);
                };
                let MediaSource::Playlist { content, index } = &media.source else {
                    debug!(id, "Ignoring media item finish event");
                    return Ok(false);
                };

                if id != self.current_media_item_id {
                    debug!(id, "Ignoring media item finish event");
                    return Ok(false);
                }

                let next_idx = index + 1;
                if next_idx < content.items.len() {
                    self.handle_operation(
                        Operation::SetPlaylistItem(v3::SetPlaylistItemMessage {
                            item_index: next_idx as u64,
                        }),
                        PacketOrigin::AutoPlay,
                    )?;
                } else {
                    info!("Playlist ended");
                }
            }
            Message::SelectTrack { id, variant } => {
                debug!(id, ?variant, "Selecting track");

                let sid = if id >= 0 { Some(id as u32) } else { None };

                let res = match variant {
                    UiMediaTrackType::Video => self.player.select_video_stream(sid),
                    UiMediaTrackType::Audio => self.player.select_audio_stream(sid),
                    UiMediaTrackType::Subtitle => self.player.select_subtitle_stream(sid),
                };

                if let Err(err) = res {
                    error!(?err, id, ?variant, "Failed to select track");
                } else if matches!(variant, UiMediaTrackType::Subtitle) {
                    // See the ChangeTrack handler: refresh once the selection is
                    // applied (on StreamsSelected), not now, to avoid racing the
                    // flush against the reconfiguration.
                    self.pending_subtitle_refresh = true;
                }
            }
            Message::NewPlayerEvent(event) => {
                self.handle_player_event(event)?;
            }
            Message::ShouldSetLoadingStatus(id) => {
                if id == self.current_media_item_id && self.is_loading_media {
                    self.gui.set_app_state(AppState::LoadingMedia);
                }
            }
            Message::Raop(event) => return self.handle_raop_event(event),
            #[cfg(feature = "airplay")]
            Message::AirPlay(event) => return self.handle_airplay_event(event),
            #[cfg(debug_assertions)]
            Message::DumpPipeline => {
                self.player.dump_graph(remote_pipeline_dbg::Trigger::Manual);
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Message::AppUpdate(event) => return self.handle_app_update_event(event),
            Message::GuiWindowClosed(feedback) => {
                self.player.shutdown(feedback);
            }
            Message::FCastSenderDisconnect(id) => {
                self.fcast_senders.remove(&id);
            }
        }

        Ok(false)
    }

    fn handle_new_fcast_session(&mut self, stream: tokio::net::TcpStream, session_id: SenderId) {
        debug!("New connection id={session_id}");

        let (recv_to_f_tx, recv_to_f_rx) = mpsc::unbounded_channel();
        let _ = self
            .fcast_senders
            .insert(session_id, FCastSenderHandle::new(recv_to_f_tx));
        tokio::spawn({
            let id = session_id;
            let msg_tx = self.msg_tx.clone();
            let updates_rx = self.updates_tx.subscribe();
            let tls_acceptor = self.tls_acceptor.clone();
            let companion_ctx = self.companion_ctx.clone();
            let (comp_tx, comp_rx) = mpsc::unbounded_channel();
            let receiver_info = Arc::clone(&self.receiver_info);
            let initial_v4_state = if let Some(current_media) = self.current_media.as_ref()
                && let MediaSource::Single(play_data) = &current_media.source
                && matches!(play_data.as_ref(), fcast::WrappedPlayMessage::V4(_))
            {
                Some(InitialV4State {
                    play_data: Arc::clone(play_data),
                    playback_state: self.player.player_state().as_fcast_v4(),
                })
            } else {
                None
            };
            async move {
                if let Err(err) = SessionDriver::new(
                    stream,
                    id,
                    tls_acceptor,
                    companion_ctx,
                    comp_tx,
                    receiver_info,
                    initial_v4_state,
                )
                .run(updates_rx, &msg_tx, comp_rx, recv_to_f_rx)
                .await
                {
                    error!("Session exited with error: {err}");
                }

                msg_tx.send(Message::FCastSenderDisconnect(id));
            }
        });

        self.gui.device_connected();
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: UnboundedReceiver<Message>,
        fin_tx: tokio::sync::oneshot::Sender<()>,
    ) -> Result<()> {
        macro_rules! listener_stream {
            ($addr:expr) => {
                futures::stream::unfold(
                    TcpListener::bind(SocketAddr::new($addr, FCAST_TCP_PORT)).await?,
                    |listener| async move { Some((listener.accept().await, listener)) },
                )
            };
        }

        #[cfg(target_os = "windows")]
        let ipv4_stream = listener_stream!(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        let ipv6_stream = listener_stream!(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));

        #[cfg(target_os = "windows")]
        tokio::pin!(ipv4_stream);
        tokio::pin!(ipv6_stream);

        #[cfg(target_os = "windows")]
        let listener_stream = futures::stream::select(ipv4_stream, ipv6_stream);
        #[cfg(not(target_os = "windows"))]
        let mut listener_stream = ipv6_stream;

        #[cfg(target_os = "windows")]
        tokio::pin!(listener_stream);

        #[cfg(not(target_os = "android"))]
        if self.settings.cli.fullscreen {
            self.gui.set_fullscreen(true);
        }

        let mut update_interval = tokio::time::interval(PROGRESS_TICK_INTERVAL);

        use futures::stream::StreamExt;

        let mut session_id: SenderId = 0;
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    if let Some(event) = event {
                        match self.handle_event(event).await {
                            Ok(true) => break,
                            Err(err) => error!("Handle event error: {err}"),
                            _ => (),
                        }
                    } else {
                        break;
                    }
                }
                _ = update_interval.tick() => {
                    if self.player.player_state() == player::PlayerState::Playing {
                        self.notify_updates(false)?;
                        self.send_v4_progress_updates();
                    }
                }
                session = listener_stream.select_next_some() => {
                    let (stream, _) = session?;
                    self.handle_new_fcast_session(stream, session_id);
                    session_id += 1;
                }
            }
        }

        debug!("Quitting");

        self.player.stop();
        self.gui.quit_loop();

        if fin_tx.send(()).is_err() {
            bail!("Failed to send fin");
        }

        #[cfg(not(target_os = "android"))]
        {
            'outer: loop {
                let shutdown_rx = self.mdns.shutdown();
                match shutdown_rx {
                    Ok(rx) => loop {
                        match rx.recv_async().await {
                            Ok(status) => {
                                if status == mdns_sd::DaemonStatus::Shutdown {
                                    debug!("mDNS daemon shutdown");
                                    break 'outer;
                                }
                            }
                            Err(err) => {
                                error!(?err, "Failed to shutdown mDNS daemon");
                                break 'outer;
                            }
                        }
                    },
                    Err(mdns_sd::Error::Again) => continue,
                    Err(_) => break,
                }
            }
        }

        let _ = slint::quit_event_loop();

        Ok(())
    }
}
