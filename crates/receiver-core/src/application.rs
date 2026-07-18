use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    media_source,
    message::{Mdns, Message, Raop, ReceiverToFCastSender},
    player::{self, PlayerState},
    raop,
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

/// Track ids at or above this value denote external subtitles (see
/// `ExternalSubtitle::id`) rather than indices into `Player::streams`. Real
/// stream indices are small, so the high base namespace never collides.
const EXTERNAL_TRACK_ID_BASE: u32 = 0x1000_0000;

struct ExternalSubtitle {
    /// Stable id, advertised as this track's `MediaTrack.id`
    /// (>= `EXTERNAL_TRACK_ID_BASE`). Persists across reloads.
    id: u32,
    url: String,
    name: Option<SmolStr>,
    requested_by: PacketOrigin,
    /// fcast backend only: the live input attached for this entry (every
    /// catalog external is attached simultaneously there, selection is pure
    /// SELECT_STREAMS). `None` on the playbin3 backend, where only the
    /// active entry is realized, as the suburi. The handle is REPLACED when
    /// the input is re-armed (see `fail_or_rearm`).
    handle: Option<fcastplaybin::ExternalSubId>,
    /// fcast backend only: the entry's GStreamer stream id, learned when its
    /// stream first materializes in a collection. URI-derived, so it stays
    /// valid across input replacements. All id/index mapping goes through
    /// this, never through the live handle.
    stream_sid: Option<String>,
    /// fcast backend only: when this entry's input was last (re-)attached.
    /// debounces the error-triggered re-arm (an input can post several
    /// errors while dying).
    attached_at: Instant,
}

/// fcast backend: the subtitle end-state to enforce once a just-attached
/// external's stream materializes in a collection. decodebin3 re-runs its
/// default selection for the new collection and may auto-select the fresh
/// text stream, so even "attach but don't show" needs an explicit
/// correction, and select-on-add needs its explicit selection anyway.
struct FcastSubDesire {
    /// The catalog external whose stream is being waited for.
    ext_id: u32,
    target: FcastSubTarget,
}

enum FcastSubTarget {
    /// Select the awaited external itself.
    TheExternal,
    /// Keep what was showing before the attach: an embedded stream id
    /// (stable within a load), or `None` for no subtitle.
    Restore(Option<String>),
}

/// An `AddSubtitleSource` that arrived after the media was loaded but before the pipeline could
/// answer the seekability query.
struct PendingSubtitleAdd {
    url: String,
    select: bool,
    name: Option<SmolStr>,
    origin: PacketOrigin,
}

enum SubtitleTarget {
    /// A real advertised stream (an embedded track or an attached external's
    /// own stream) by stream id, or `None` to show no subtitle.
    Stream(Option<player::StreamId>),
    /// A catalog external whose stream has not materialized yet: the desired
    /// selection is parked until it appears.
    External(u32),
}

struct MediaSourceState {
    origin: PacketOrigin,
    source: MediaSource,
    image_id: Option<image::ImageId>,
    pending_thumbnail: Option<image::ImageId>,
    pending_thumbnail_download: Option<image::ImageDownloadId>,
    /// The external subtitle catalog for the current item.
    external_subtitles: Vec<ExternalSubtitle>,
    /// Enforcement parked until an attached external's stream materializes
    /// (see `FcastSubDesire`). Latest attach/selection wins.
    fcast_sub_desire: Option<FcastSubDesire>,
    /// Monotonic id source for `ExternalSubtitle::id` within this item.
    next_external_id: u32,
}

impl MediaSourceState {
    fn new(origin: PacketOrigin, source: MediaSource) -> Self {
        Self {
            origin,
            source,
            image_id: None,
            pending_thumbnail: None,
            pending_thumbnail_download: None,
            external_subtitles: Vec::new(),
            fcast_sub_desire: None,
            next_external_id: 0,
        }
    }

    /// Drop every external subtitle (external subtitles are per-item). The id counter keeps
    /// advancing so a stale id from the previous item can never alias a new one.
    fn clear_external_subtitles(&mut self) {
        self.external_subtitles.clear();
        self.fcast_sub_desire = None;
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
    pending_subtitle_adds: Vec<PendingSubtitleAdd>,
    pending_subtitle_add_epoch: u64,
    last_progress_broadcast: Option<Instant>,
    last_volume_cmd: Option<Instant>,
    pending_seek_op: Option<(PacketOrigin, gst::ClockTime)>,
    pending_seek_epoch: u64,
    /// DIAGNOSTIC (load-stall investigation): bumped per pipeline load so a
    /// stale `LoadStallCheck` watchdog no-ops.
    load_watchdog_epoch: u64,
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
    /// The fwebrtc signalling channel from the most recent
    /// `StartMirroringSession`, consumed when the fwebrtc source is built
    /// (`build_media_source`). The channel is a live object, so it is handed to
    /// `fwebrtcsrc` as a typed property, not smuggled through a fake URI.
    pending_fwebrtc_channel: Option<fwebrtcsrc::SignallingChannel>,
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
    receiver_info: Arc<crate::ReceiverInfo>,
    fcast_txt_records: HashMap<String, String>,
    fcast_senders: HashMap<SenderId, FCastSenderHandle>,
    inspector_bitrates: InspectorBitrates,
    /// Inspector: container format from the current item's tags.
    inspector_container: Option<String>,
    /// Inspector: format/size line for the current image item.
    inspector_image: String,
}

/// Bitrate sampling state for the inspector: the previous cumulative
/// parsed-byte totals of the selected video/audio streams and the rate
/// histories built from their deltas (kbit/s, oldest first).
#[derive(Default)]
struct InspectorBitrates {
    last_at: Option<Instant>,
    last_video: Option<(String, u64)>,
    last_audio: Option<(String, u64)>,
    video_kbps: VecDeque<f32>,
    audio_kbps: VecDeque<f32>,
}

impl InspectorBitrates {
    /// 500 ms ticks, so a minute of history.
    const WINDOW: usize = 120;

    /// Fold one cumulative (stream key, total bytes) sample into a slot's
    /// history. A changed key (track switch, new load) restarts the counter,
    /// that interval reports 0 rather than a bogus delta.
    fn push(
        history: &mut VecDeque<f32>,
        last: &mut Option<(String, u64)>,
        sample: Option<(String, u64)>,
        dt: f64,
    ) {
        let kbps = match (&last, &sample) {
            (Some((last_key, last_bytes)), Some((key, bytes))) if last_key == key && dt > 0.0 => {
                (bytes.saturating_sub(*last_bytes) as f64 * 8.0 / dt / 1000.0) as f32
            }
            _ => 0.0,
        };
        history.push_back(kbps);
        while history.len() > Self::WINDOW {
            history.pop_front();
        }
        *last = sample;
    }
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

        // Opt-in escape hatch (test/soak harness): force software (libav)
        // decode by disabling every VA element. The Intel VA dmabuf-export
        // path has a driver bug that leaks GPU state across receiver
        // restarts and eventually hangs the video sink in an async
        // Playing->Paused. Production keeps hardware decode, only the
        // stress harness sets FCAST_DISABLE_VA so long soaks stay clean.
        if std::env::var_os("FCAST_DISABLE_VA").is_some() {
            let mut disabled = 0;
            for va_feature in registry.features_by_plugin("va") {
                if let Some(elem) = va_feature.downcast_ref::<gst::ElementFactory>() {
                    elem.set_rank(gst::Rank::NONE);
                    disabled += 1;
                }
            }
            warn!("FCAST_DISABLE_VA: disabled {disabled} VA elements; using software decode");
        }

        #[cfg(target_os = "android")]
        if let Some(amcaudiodec) = registry.lookup_feature("amcaudiodec") {
            // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/4883
            amcaudiodec.set_rank(gst::Rank::NONE);
        }

        let companion_ctx = CompanionContext::new();
        #[cfg(feature = "airplay")]
        let airplay_context = airplay::AirPlayContext::new();
        let player = player::Player::new(
            video_sink,
            msg_tx.clone(),
            fcompsrc::imp::CompContext(companion_ctx.clone()),
            #[cfg(feature = "airplay")]
            airplay_context.clone(),
        )?;

        // Sources are built with their config baked in (`media_source`):
        // request headers via a per-source `deep-element-added` hook, the
        // fwebrtc signalling channel as a typed property. No global side
        // channels, no pipeline-wide element-setup hook.

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
            pending_subtitle_adds: Vec::new(),
            pending_subtitle_add_epoch: 0,
            last_progress_broadcast: None,
            last_volume_cmd: None,
            pending_seek_op: None,
            pending_seek_epoch: 0,
            load_watchdog_epoch: 0,
            current_image_id: 0,
            have_audio_track_cover: false,
            current_media: None,
            have_media_info: false,
            current_thumbnail_id: 0,
            current_image_download_id: 0,
            inspector_bitrates: InspectorBitrates::default(),
            inspector_container: None,
            inspector_image: String::new(),
            current_addresses: HashSet::new(),
            have_media_title: false,
            last_position_updated: -1.0,
            http_client,
            pending_fwebrtc_channel: None,
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
            receiver_info,
            fcast_txt_records,
            fcast_senders: HashMap::new(),
        })
    }

    fn should_broadcast(&self) -> bool {
        self.updates_tx.receiver_count() > 0
    }

    fn broadcast_volume(&mut self, volume: f32) {
        debug!(volume, "Broadcasting volume");
        if self.should_broadcast() {
            let update = VolumeUpdateMessage {
                generation_time: current_time_millis(),
                volume: volume as f64,
            };

            let msg = ReceiverToSenderMessage::LegacyTranslatable {
                op: Opcode::VolumeUpdate,
                msg: TranslatableMessage::VolumeUpdate(update),
            };
            let _ = self.updates_tx.send(Arc::new(msg));
            self.last_sent_update = Instant::now();

            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::VolumeChanged(volume),
            ));
        }

        self.gcast_tx
            .send(gcast::StatusUpdate::Volume(volume as f64));
    }

    /// Relay a playback rate to all senders (progress update + v4
    /// PlaybackRateChanged).
    fn broadcast_rate(&mut self, rate: f32) -> Result<()> {
        self.notify_updates(true)?;
        if self.updates_tx.strong_count() > 0 {
            let _ = self.updates_tx.send(Arc::new(ReceiverToSenderMessage::V4(
                fcast::V4Message::PlaybackRateChanged(rate),
            )));
        }
        Ok(())
    }

    /// Apply a volume command and confirm the accepted (clamped) value to
    /// senders immediately.
    fn set_volume_cmd(&mut self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        self.player.set_volume(clamped);
        self.gui.set_volume(clamped);
        self.last_volume_cmd = Some(Instant::now());
        self.broadcast_volume(clamped);
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

        // Discontinuity notification (seek/state edge): bypasses per-sender
        // intervals on purpose, but the start/seek dance produces bursts of
        // state edges (observed: 5 within 14ms), debounce so senders get
        // one prompt update per discontinuity, not the whole burst.
        let debounced = self
            .last_progress_broadcast
            .is_some_and(|at| at.elapsed() < Duration::from_millis(100));
        if self.should_broadcast() && !debounced {
            debug!("Broadcasting v4 progress (interval bypass)");
            self.last_progress_broadcast = Some(Instant::now());
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

        for (sender_id, handle) in self.fcast_senders.iter_mut() {
            if now.duration_since(handle.last_progress_update) < handle.progress_interval {
                continue;
            }
            debug!(sender_id, interval = ?handle.progress_interval, "per-sender progress");
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
        // Parked subtitle adds and seeks target the media that is going
        // away. (The player's own per-load state, the text-restore dance,
        // held seeks, parked deselects, is reset by `Player::stop` below.)
        self.reject_pending_subtitle_adds();
        self.drop_pending_seek();
        self.have_audio_track_cover = false;
        self.have_media_info = false;
        self.have_media_title = false;
        self.last_position_updated = -1.0;
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
        self.inspector_container = None;
        self.inspector_image = String::new();
        if let Err(err) = self.load_current_media_item() {
            error!(?err, "Failed to load media");
            if let Some(origin) = self.current_media.as_ref().map(|m| m.origin) {
                self.send_error(origin, load_media_error_kind(&err));
            }
        }
    }

    /// Build the source for a load: constructed directly with typed config,
    /// HTTP with per-load headers, WHEP, and fwebrtc, no fake-URI dispatch,
    /// no global header / signalling side channels. (AirPlay mirror is built
    /// at its own call site.)
    fn build_media_source(
        &mut self,
        container: &str,
        url: String,
        headers: Option<HashMap<String, String>>,
    ) -> player::MediaInput {
        let built = match container {
            "application/x-whep" => media_source::build_whep_source(&url),
            "application/x-fwebrtc" => match self.pending_fwebrtc_channel.take() {
                Some(chan) => media_source::build_fwebrtc_source(chan),
                None => Err(anyhow::anyhow!("fwebrtc load without a signalling channel")),
            },
            _ => media_source::build_uri_source(&url, headers),
        };
        match built {
            Ok(element) => player::MediaInput::Element(element),
            Err(err) => {
                error!(?err, container, "Failed to build the fcast source element");
                // Fall back to the URI path so the load still attempts (and
                // surfaces a real error) instead of silently doing nothing.
                player::MediaInput::Uri(url)
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
        let url = match item.url {
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
        if container == "application/x-whep" || container == "application/x-fwebrtc" {
            // The source is built directly with the real URL / typed channel
            // (`build_media_source`), no fake-URI dispatch.
            is_for_sure_live = true;
        }

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
            // External subtitles are LIVE inputs (attach/detach), never a
            // suburi ridden along a load, so every media load restores
            // embedded text via the plain text-restore sequence. Live sources
            // get no post-preroll start seek.
            let start = (!is_for_sure_live).then_some(player::RestorePoint {
                position: start_position,
                rate: playback_rate,
            });
            let source = self.build_media_source(&container, url, headers.clone());
            self.player.load(source, start);
            if let Some(volume) = volume {
                // Command path: stamp the echo window so the pipeline's
                // stale read-back notifies don't get relayed as external
                // changes (the confirm comes from the Load relay itself).
                self.player.set_volume(volume.clamp(0.0, 1.0));
                self.last_volume_cmd = Some(Instant::now());
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
        // Headers are applied at source construction (`build_media_source`).

        // DIAGNOSTIC (load-stall investigation): a pipeline load should reach a
        // steady PAUSED quickly, if this one has not by the timeout, dump why
        // (`Player::log_load_stall_diagnostics`). Images bypass the pipeline.
        if !is_image {
            self.load_watchdog_epoch += 1;
            let epoch = self.load_watchdog_epoch;
            let item = self.current_media_item_id;
            let msg_tx = self.msg_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Self::LOAD_STALL_TIMEOUT).await;
                msg_tx.send(Message::LoadStallCheck { item, epoch });
            });
        }

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

        // External subtitles are per-item, don't carry them over to the next.
        if let Some(media) = self.current_media.as_mut() {
            media.clear_external_subtitles();
        }

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
        // A pause landing mid-load is recorded as the player's desired
        // transport and committed when the load prerolls; no special casing.
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
            Operation::Stop => {
                self.stop_playback();
                // Let the other senders know playback was stopped (current item/queue cleared) by
                // this sender. The initiator is excluded as it already knows it issued the stop.
                self.relay_to_other_senders(
                    origin,
                    fcast_protocol::v4::MessageBuilder::new().stop_playback(),
                );
            }
            Operation::Seek(time) => {
                if self.is_playing() {
                    if !self.player.seekable_known {
                        // Tracks are advertised well before the pipeline can answer the seekability
                        // query, in that window the duration for the range check is unknown too,
                        // and the player would silently drop the seek. Park it (last seek wins) and
                        // apply it once the query resolves.
                        debug!(
                            ?time,
                            "Parking the seek until the seekability query resolves"
                        );
                        self.pending_seek_op = Some((origin, time));
                        self.pending_seek_epoch += 1;
                        let epoch = self.pending_seek_epoch;
                        let msg_tx = self.msg_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Self::PENDING_SEEK_TIMEOUT).await;
                            msg_tx.send(Message::PendingSeekCheck { epoch });
                        });
                    } else {
                        match self.current_duration {
                            Some(duration)
                                if duration > gst::ClockTime::ZERO && time > duration =>
                            {
                                self.send_error(origin, ErrorKind::SeekOutOfRange);
                                self.player.seek(duration);
                            }
                            _ => self.player.seek(time),
                        }
                    }
                }
            }
            Operation::SetSpeed(rate) => {
                // An idempotent speed set performs no rate-changing seek and thus emits no
                // RateChanged from the pipeline, but the sender still expects a
                // confirmation. Confirm directly, real changes are confirmed by the pipeline's
                // RateChanged.
                if (self.player.rate() - rate as f64).abs() < 1e-9 {
                    debug!(rate, "Speed unchanged; re-emitting the confirmation");
                    self.broadcast_rate(rate)?;
                } else {
                    self.player.set_rate(rate);
                }
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

                // External subtitles are per-item, drop on item change.
                if let Some(media) = self.current_media.as_mut() {
                    media.clear_external_subtitles();
                }

                self.load_media();
                self.gui.set_playlist_index(new_index as i32);
            }
            Operation::SetVolume(volume) => {
                self.set_volume_cmd(volume);
            }
            Operation::StartMirroringSession {
                tx: client_tx,
                offer_rx,
            } => {
                let chan = fwebrtcsrc::SignallingChannel {
                    tx: client_tx.0,
                    offer_rx,
                };
                // fwebrtcsrc is built directly with the channel as a typed
                // property (`build_media_source`), the channel is a live
                // object, so it cannot travel through a URI.
                self.pending_fwebrtc_channel = Some(chan);
                let play_message = v3::PlayMessage {
                    container: "application/x-fwebrtc".to_owned(),
                    // Placeholder: the fwebrtc source ignores the URL and uses
                    // `pending_fwebrtc_channel` instead.
                    url: Some("fwebrtc://placeholder".to_owned()),
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
                    self.pause();
                }
                fcast_protocol::v4::PlaybackState::Playing => {
                    self.resume();
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

                // Subtitles have their own path: ids can name an external
                // subtitle (a virtual track not present in `Player::streams`).
                if matches!(typ, v4::flat::MediaTrackType::Subtitle) {
                    self.change_subtitle_track(origin, id);
                    return Ok(false);
                }

                let stream_type = match typ {
                    v4::flat::MediaTrackType::Video => gst::StreamType::VIDEO,
                    v4::flat::MediaTrackType::Audio => gst::StreamType::AUDIO,
                    v4::flat::MediaTrackType::Subtitle => unreachable!(),
                    _ => {
                        error!(?typ, "Unknown track type");
                        self.send_error(origin, ErrorKind::MalformedBody);
                        return Ok(false);
                    }
                };

                // The wire speaks indices into the advertised stream list;
                // the pipeline speaks stream ids. Validate and convert here.
                let sid = match id {
                    None => None,
                    Some(id) => {
                        let sid = self
                            .player
                            .is_stream_of_type(id, stream_type)
                            .then(|| self.player.stream_id_of(id))
                            .flatten();
                        if sid.is_none() {
                            error!(id, ?typ, "Track id is not a track of the requested type");
                            self.send_error(origin, ErrorKind::MalformedBody);
                            return Ok(false);
                        }
                        sid
                    }
                };

                // Latest-wins and serialized against other track operations in
                // the player (see player::TrackOps), the subtitle re-emit
                // flush is scheduled there too.
                let kind = match typ {
                    v4::flat::MediaTrackType::Video => player::TrackKind::Video,
                    v4::flat::MediaTrackType::Audio => player::TrackKind::Audio,
                    _ => unreachable!(),
                };
                self.apply_track_change(kind, sid);
            }
            Operation::AddSubtitleSource { url, select, name } => {
                return self.add_subtitle_source(origin, url, select, name);
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
        // An `AddSubtitleSource` may be parked waiting for the seekability
        // query this update may have just resolved.
        self.maybe_apply_pending_subtitle_adds();

        // The start position/rate is applied inside `fcastplaybin::load`, here
        // we only replay a sender seek that raced the load once seekability
        // resolves.
        self.maybe_apply_pending_seek();
    }

    /// Map a selected subtitle stream id to the wire id senders should see:
    /// an external's STABLE id when its own stream is selected (so it
    /// matches `TracksAvailable`), otherwise the stream's advertised index.
    fn advertised_subtitle_id(&self, subtitle_sid: Option<&str>) -> Option<u32> {
        let sid = subtitle_sid?;
        if let Some(media) = self.current_media.as_ref() {
            for entry in &media.external_subtitles {
                if entry.stream_sid.as_deref() == Some(sid) {
                    return Some(entry.id);
                }
            }
        }
        self.player.stream_idx_by_id(sid)
    }

    /// How long a parked `AddSubtitleSource` may wait for the seekability
    /// query to resolve before it is rejected with `InvalidState`.
    const PENDING_SUBTITLE_ADD_TIMEOUT: Duration = Duration::from_secs(10);

    /// fcast backend: how long an attached external subtitle input may take
    /// to produce its stream before it is failed with `ResourceNotFound`
    /// (matches the playbin3 dance's `EXTERNAL_SUB_TIMEOUT`).
    const FCAST_EXTERNAL_SUB_TIMEOUT: Duration = Duration::from_secs(5);

    /// Handle `AddSubtitleSource`. If the media is loaded but the pipeline
    /// hasn't answered the seekability query yet (tracks are advertised off
    /// the first stream collection, well before the query can succeed at
    /// preroll completion, seconds apart on a slow preroll), the op is
    /// parked and replayed once seekability is known instead of being
    /// spuriously rejected.
    fn add_subtitle_source(
        &mut self,
        origin: PacketOrigin,
        url: String,
        select: bool,
        name: Option<SmolStr>,
    ) -> Result<bool> {
        debug!(url, select, ?name, "adding external subtitle source");

        // Preconditions: an active, non-live, seekable, fully loaded
        // media item. Selecting an external needs a reload+seek
        // (`suburi` only applies at load time), impossible on a
        // live/unseekable stream, and an in-flight load would be
        // raced.
        let src_supported = match self.current_media.as_ref().map(|m| &m.source) {
            Some(MediaSource::Single(_) | MediaSource::Playlist { .. } | MediaSource::Queue(_)) => {
                true
            }
            Some(MediaSource::Raop | MediaSource::AirPlayMirror { .. }) | None => false,
        };
        if !src_supported || self.is_loading_media {
            error!("Cannot add a subtitle source: no compatible media is loaded");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }
        if self.player.is_live() {
            error!("Cannot add a subtitle source to a live stream");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }
        if !self.player.seekable {
            if !self.player.seekable_known {
                // Not unseekable, just not answerable yet. Park the op.
                // `on_media_info_updated` replays it once the query
                // resolves, and the check timer bounds the wait.
                debug!("Parking the subtitle source until the seekability query resolves");
                self.pending_subtitle_adds.push(PendingSubtitleAdd {
                    url,
                    select,
                    name,
                    origin,
                });
                let epoch = self.pending_subtitle_add_epoch;
                let item = self.current_media_item_id;
                let msg_tx = self.msg_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Self::PENDING_SUBTITLE_ADD_TIMEOUT).await;
                    msg_tx.send(Message::PendingSubtitleAddCheck { item, epoch });
                });
                return Ok(false);
            }
            error!("Cannot add a subtitle source to an unseekable stream");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }

        // Every catalog external is a LIVE input, attached simultaneously
        // (decodebin3 request pads) so switching is pure stream selection,
        // no reload in either direction. The virtual track is advertised
        // immediately, the desired end state is enforced once the stream
        // materializes in a collection (see `pump_fcast_sub_desire`).
        let handle = self.player.attach_external_subtitle(&url);
        let Some(media) = self.current_media.as_mut() else {
            self.player.detach_external_subtitle(handle);
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        };
        let id = EXTERNAL_TRACK_ID_BASE + media.next_external_id;
        media.next_external_id += 1;
        media.external_subtitles.push(ExternalSubtitle {
            id,
            url,
            name,
            requested_by: origin,
            handle: Some(handle),
            stream_sid: None,
            attached_at: Instant::now(),
        });
        let target = if select {
            FcastSubTarget::TheExternal
        } else {
            // Keep what is showing now, decodebin3 may auto-select the
            // fresh text stream for the new collection otherwise.
            FcastSubTarget::Restore(self.player.current_subtitle_sid().map(str::to_string))
        };
        media.fcast_sub_desire = Some(FcastSubDesire { ext_id: id, target });
        self.arm_external_sub_watchdog(id);

        debug!(id, select, "Attached external subtitle input (live)");
        self.update_tracks(true);
        Ok(false)
    }

    /// Replay `AddSubtitleSource` ops parked while the seekability query was
    /// unresolved. No-op until it resolves, called whenever media info
    /// updates.
    fn maybe_apply_pending_subtitle_adds(&mut self) {
        if self.pending_subtitle_adds.is_empty() || !self.player.seekable_known {
            return;
        }
        self.pending_subtitle_add_epoch += 1;
        let adds = std::mem::take(&mut self.pending_subtitle_adds);
        for add in adds {
            debug!(url = add.url, "Applying a parked subtitle source");
            let _ = self.add_subtitle_source(add.origin, add.url, add.select, add.name);
        }
    }

    /// Drop parked subtitle adds, rejecting them to their senders: the media
    /// they targeted is being replaced or playback is stopping.
    fn reject_pending_subtitle_adds(&mut self) {
        if self.pending_subtitle_adds.is_empty() {
            return;
        }
        self.pending_subtitle_add_epoch += 1;
        for add in std::mem::take(&mut self.pending_subtitle_adds) {
            self.send_error(add.origin, ErrorKind::InvalidState);
        }
    }

    /// How long a parked `Seek` may wait for the seekability query to
    /// resolve before it is dropped.
    const PENDING_SEEK_TIMEOUT: Duration = Duration::from_secs(10);

    /// DIAGNOSTIC (load-stall investigation): how long after a pipeline load
    /// before, if it still has not reached a steady PAUSED, we dump why. Set
    /// below FAST's 16s confirm window so the stalled state is captured before
    /// the sender gives up and tears it down.
    const LOAD_STALL_TIMEOUT: Duration = Duration::from_secs(12);

    /// Apply a `Seek` parked while the seekability query was unresolved:
    /// now that duration and seekability are known, the range check gives
    /// the right answer (`SeekOutOfRange` for over-long seeks) instead of
    /// the seek being silently dropped.
    fn maybe_apply_pending_seek(&mut self) {
        if !self.player.seekable_known {
            return;
        }
        let Some((origin, time)) = self.pending_seek_op.take() else {
            return;
        };
        self.pending_seek_epoch += 1;
        debug!(?time, "Applying a parked seek");
        match self.current_duration {
            Some(duration) if duration > gst::ClockTime::ZERO && time > duration => {
                self.send_error(origin, ErrorKind::SeekOutOfRange);
                self.player.seek(duration);
            }
            _ => self.player.seek(time),
        }
    }

    /// Drop a parked seek without applying it (media going away or the
    /// query never resolved).
    fn drop_pending_seek(&mut self) {
        if self.pending_seek_op.take().is_some() {
            self.pending_seek_epoch += 1;
        }
    }

    /// Resolve a protocol/GUI subtitle track id into what the pipeline must
    /// do. Ids `>= EXTERNAL_TRACK_ID_BASE` name an external catalog entry,
    /// smaller ids are `Player::streams` indices (embedded tracks); `None`
    /// is "off". The wire speaks indices, the pipeline speaks stream ids;
    /// this is one of the edges where they convert.
    fn resolve_subtitle_target(&self, id: Option<u32>) -> Result<SubtitleTarget, ErrorKind> {
        let Some(id) = id else {
            return Ok(SubtitleTarget::Stream(None));
        };
        if id >= EXTERNAL_TRACK_ID_BASE {
            let entry_sid = match self
                .current_media
                .as_ref()
                .and_then(|m| m.external_subtitles.iter().find(|s| s.id == id))
            {
                Some(entry) => self.advertised_external_sid(entry),
                None => return Err(ErrorKind::MalformedBody),
            };
            // Every catalog external is a live input, once its stream is
            // advertised, selecting it is a plain stream selection. Before
            // that (attach still propagating) it stays an `External` target,
            // parked as the desired end state.
            if let Some(sid) = entry_sid {
                return Ok(SubtitleTarget::Stream(Some(sid)));
            }
            Ok(SubtitleTarget::External(id))
        } else {
            if !self.player.is_stream_of_type(id, gst::StreamType::TEXT) {
                return Err(ErrorKind::MalformedBody);
            }
            Ok(SubtitleTarget::Stream(self.player.stream_id_of(id)))
        }
    }

    /// Shared subtitle-change path for both the protocol `ChangeTrack` and the
    /// GUI `SelectTrack`. `origin` receives any error (a `Gui` origin swallows
    /// it). External-track selection parks the desired state until the stream
    /// materializes, everything else goes through the selection logic.
    fn change_subtitle_track(&mut self, origin: PacketOrigin, id: Option<u32>) {
        let target = match self.resolve_subtitle_target(id) {
            Ok(t) => t,
            Err(kind) => {
                error!(?id, "Invalid subtitle track id");
                self.send_error(origin, kind);
                return;
            }
        };

        // playsink cannot present a text stream without a video stream, so
        // selecting any subtitle while video is deselected would error the
        // pipeline or be silently dropped. Report it as unsatisfiable.
        let selecting_something = !matches!(target, SubtitleTarget::Stream(None)) && id.is_some();
        if selecting_something && self.player.current_video_sid().is_none() {
            error!("Cannot select a subtitle track while video is disabled");
            self.send_error(origin, ErrorKind::InvalidState);
            return;
        }

        match target {
            SubtitleTarget::External(ext_id) => {
                // Attached but its stream hasn't materialized yet: park it as
                // the desired end state, the collection pump applies it and the
                // eventual selection confirm relays the TracksSelected the
                // sender is waiting for.
                debug!(
                    ext_id,
                    "Parking the external selection until its stream appears"
                );
                if let Some(media) = self.current_media.as_mut() {
                    media.fcast_sub_desire = Some(FcastSubDesire {
                        ext_id,
                        target: FcastSubTarget::TheExternal,
                    });
                }
            }
            SubtitleTarget::Stream(stream_sid) => {
                // An explicit subtitle change supersedes a parked post-attach
                // desire (fcast backend): the newest intent wins, and a stale
                // desire enforcing itself later would stomp this change.
                if let Some(media) = self.current_media.as_mut() {
                    media.fcast_sub_desire = None;
                }
                // Apply immediately, paused or playing. A subtitle deselect
                // tears the overlay's text chain down, under playsink that
                // deadlocked while paused (the teardown needed flowing data),
                // so it used to be parked until resume. fcastplaybin tears text
                // down cleanly instead (`detach_text_from_overlay` flushes the
                // blocked push before unlinking) and decodebin3 posts the
                // deselect's STREAMS_SELECTED promptly while paused -- verified
                // by trace + a 199-case interleaved stress with no wedge.
                self.apply_track_change(player::TrackKind::Subtitle, stream_sid);
            }
        }
    }

    /// Apply a track change through TrackOps. While any external subtitle is
    /// attached the switch's re-emit flush is suppressed: a flush races the
    /// external inputs' reconfiguration and can freeze the item (and selecting
    /// an external needs no flush anyway, its input re-pushes the whole file).
    fn apply_track_change(&mut self, kind: player::TrackKind, sid: Option<player::StreamId>) {
        let externals_attached = self
            .current_media
            .as_ref()
            .is_some_and(|m| !m.external_subtitles.is_empty());
        let stale = if externals_attached {
            self.player.request_track_change_no_refresh(kind, sid)
        } else {
            self.player.request_track_change(kind, sid)
        };
        if stale {
            // The displayed cue belongs to the previous track. Clear it
            // immediately so the change registers visually, even while paused.
            self.gui.clear_video_overlays();
        }
    }

    /// A catalog external's stream id, once its stream is actually in the
    /// advertised collection (the remembered sid is stable across input
    /// replacements, but only counts once decodebin3 advertises it).
    fn advertised_external_sid(&self, entry: &ExternalSubtitle) -> Option<player::StreamId> {
        entry
            .stream_sid
            .clone()
            .filter(|sid| self.player.stream_idx_by_id(sid).is_some())
    }

    /// Learn the stream id of externals whose stream just materialized in the
    /// (new) collection. Runs before anything maps externals for that
    /// collection.
    fn refresh_external_stream_sids(&mut self) {
        let Some(media) = self.current_media.as_mut() else {
            return;
        };
        for entry in media.external_subtitles.iter_mut() {
            if entry.stream_sid.is_none()
                && let Some(handle) = entry.handle
                && let Some(sid) = self.player.external_stream_sid_of(handle)
            {
                debug!(id = entry.id, sid, "external subtitle stream materialized");
                entry.stream_sid = Some(sid);
            }
        }
    }

    /// fcast backend: enforce the desired subtitle end-state once the awaited
    /// external's stream has materialized in the advertised collection (run
    /// from the stream-collection handler). The enforcement goes out AFTER
    /// decodebin3 computed its own selection for that collection, so it
    /// supersedes a possible auto-select of the fresh text stream.
    fn pump_fcast_sub_desire(&mut self) {
        let Some(media) = self.current_media.as_ref() else {
            return;
        };
        let Some(desire) = media.fcast_sub_desire.as_ref() else {
            return;
        };
        let Some(entry) = media
            .external_subtitles
            .iter()
            .find(|s| s.id == desire.ext_id)
        else {
            // The awaited entry is gone (failed and removed), nothing left
            // to enforce.
            if let Some(media) = self.current_media.as_mut() {
                media.fcast_sub_desire = None;
            }
            return;
        };
        let Some(ext_sid) = self.advertised_external_sid(entry) else {
            // Stream not in the advertised collection yet, a later
            // collection re-runs this.
            return;
        };
        let target_sid = match &desire.target {
            FcastSubTarget::TheExternal => Some(ext_sid),
            FcastSubTarget::Restore(sid) => sid.clone(),
        };
        if let Some(media) = self.current_media.as_mut() {
            media.fcast_sub_desire = None;
        }
        debug!(?target_sid, "Enforcing the post-attach subtitle selection");
        self.apply_track_change(player::TrackKind::Subtitle, target_sid);
    }

    /// fcast backend: how soon after a (re-)attach an error may trigger
    /// another re-arm. A dying input posts several errors and only the first
    /// past this window may replace it.
    const FCAST_REARM_DEBOUNCE: Duration = Duration::from_secs(1);

    /// Bounded wait for an attached external's stream to materialize. A bad
    /// URL can fail without producing a stream or a bus error, so the check
    /// message fails the entry with `ResourceNotFound` if it is still
    /// stream-less when it fires. Armed on every attach, including re-arms.
    fn arm_external_sub_watchdog(&self, ext_id: u32) {
        let item = self.current_media_item_id;
        let msg_tx = self.msg_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Self::FCAST_EXTERNAL_SUB_TIMEOUT).await;
            msg_tx.send(Message::FcastExternalSubCheck { item, ext_id });
        });
    }

    /// fcast backend: decide what an external input's bus error means.
    ///
    /// An error from an input that is currently AWAITED (post-attach desire)
    /// or SHOWN is a genuine failure: the requester gets `ResourceNotFound`
    /// and the entry is dropped. An error from a deselected input is the
    /// known deselect race (switching away from a selected external races its
    /// in-flight push against decodebin3's slot deactivation and kills the
    /// source with not-linked) and must NOT fail the entry. Instead the input
    /// is RE-ARMED with a fresh one on the same URL so the track can be
    /// selected again. The stream id is URI-derived and stays the same, so
    /// all advertised ids remain valid.
    fn fail_or_rearm_fcast_external(&mut self, ext_id: u32) {
        let Some(media) = self.current_media.as_ref() else {
            return;
        };
        let awaited = media
            .fcast_sub_desire
            .as_ref()
            .is_some_and(|d| d.ext_id == ext_id);
        let Some(entry) = media.external_subtitles.iter().find(|s| s.id == ext_id) else {
            return;
        };
        let shown = entry.stream_sid.is_some()
            && entry.stream_sid.as_deref() == self.player.current_subtitle_sid();
        if awaited || shown {
            self.fail_fcast_external_subtitle(ext_id);
            return;
        }
        if entry.attached_at.elapsed() < Self::FCAST_REARM_DEBOUNCE {
            debug!(
                ext_id,
                "Ignoring error from an input that was just re-armed"
            );
            return;
        }
        let (url, old_handle) = (entry.url.clone(), entry.handle);
        debug!(ext_id, "Re-arming the deselected external subtitle input");
        if let Some(old) = old_handle {
            self.player.detach_external_subtitle(old);
        }
        let new_handle = Some(self.player.attach_external_subtitle(&url));
        if let Some(entry) = self
            .current_media
            .as_mut()
            .and_then(|m| m.external_subtitles.iter_mut().find(|s| s.id == ext_id))
        {
            entry.handle = new_handle;
            entry.stream_sid = None;
            entry.attached_at = Instant::now();
        }
        // The fresh input can fail as silently as the original: without a new
        // bounded check, a dead re-armed entry would stay selectable forever
        // and a selection parked on it would never resolve or error.
        self.arm_external_sub_watchdog(ext_id);
    }

    /// fcast backend: an attached external subtitle failed (a bus error from
    /// its input, or its stream never materialized). Detach the input, drop
    /// the catalog entry and tell the requester `ResourceNotFound`, the
    /// input is independent of the main item, so playback continues
    /// untouched (no reload).
    fn fail_fcast_external_subtitle(&mut self, ext_id: u32) {
        let Some(media) = self.current_media.as_mut() else {
            return;
        };
        let Some(pos) = media.external_subtitles.iter().position(|s| s.id == ext_id) else {
            return;
        };
        let failed = media.external_subtitles.remove(pos);
        if media
            .fcast_sub_desire
            .as_ref()
            .is_some_and(|d| d.ext_id == ext_id)
        {
            media.fcast_sub_desire = None;
        }
        warn!(url = failed.url, "External subtitle failed; removing it");
        if let Some(handle) = failed.handle {
            self.player.detach_external_subtitle(handle);
        }
        self.send_error(failed.requested_by, ErrorKind::ResourceNotFound);
        self.update_tracks(true);
    }

    fn update_tracks(&mut self, force_update: bool) {
        if !force_update && !self.player.update_stream_properties() {
            return;
        }

        // Every external subtitle is advertised as a subtitle track with its
        // STABLE id, in catalog order, AFTER the embedded tracks, regardless
        // of which one is currently realized as a stream. This keeps the
        // advertised order fixed as the selection changes. Externals that ARE
        // real GStreamer streams are skipped in the stream loops so they are
        // never advertised twice.
        let external_stream_idxs: Vec<u32> = self
            .current_media
            .as_ref()
            .map(|m| {
                m.external_subtitles
                    .iter()
                    .filter_map(|s| {
                        s.stream_sid
                            .as_deref()
                            .and_then(|sid| self.player.stream_idx_by_id(sid))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // (id, name) for every catalog external, in order.
        let externals: Vec<(u32, Option<SmolStr>)> = self
            .current_media
            .as_ref()
            .map(|m| {
                m.external_subtitles
                    .iter()
                    .map(|s| (s.id, s.name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        if self.should_broadcast() {
            let mut tracks: Vec<v4::MediaTrack> = self
                .player
                .streams
                .iter()
                .enumerate()
                .filter_map(|(idx, s)| {
                    // External streams are advertised below, by stable id.
                    if external_stream_idxs.contains(&(idx as u32)) {
                        return None;
                    }
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
                })
                .collect();

            // All externals, in stable catalog order.
            for (id, name) in &externals {
                tracks.push(v4::MediaTrack {
                    id: *id,
                    title: name.clone(),
                    iso_639: SmolStr::new("und"),
                    metadata: Some(v4::MediaTrackMetadata::Subtitle),
                });
            }

            let serialized_msg = v4::MessageBuilder::new().tracks_available(tracks.into_iter());
            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::TracksAvailable { serialized_msg },
            ));
        }

        let mut videos = Vec::new();
        let mut audios = Vec::new();
        let mut subtitles = Vec::new();
        for (idx, stream) in self.player.streams.iter().enumerate() {
            if external_stream_idxs.contains(&(idx as u32)) {
                continue;
            }
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
        for (id, name) in &externals {
            subtitles.push(UiMediaTrack {
                id: *id as i32,
                name: name
                    .as_ref()
                    .map(|n| n.to_shared_string())
                    .unwrap_or_else(|| SmolStr::new_inline("External").to_shared_string()),
            });
        }

        self.gui.set_tracks(videos, audios, subtitles);
    }

    /// Whether an event is scoped to a load, and must therefore be dropped
    /// when its generation is not the current one. The exceptions are not
    /// item-scoped at all (volume, sleep requests, tag-notify forwarding).
    ///
    /// `StateChanged` IS load-scoped: a superseded item's teardown edges
    /// used to walk the state machine right through a queued load
    /// (Loading -> stale-Paused settle -> Running -> stale-Ready -> Stopped,
    /// broadcasting a bogus Idle and losing a recorded mid-load pause to
    /// Stopped-buffering's Playing default). The machine no longer needs
    /// teardown echoes: stop and load reset it explicitly
    /// (`clear_state`/`begin_load`), and a load's own climb edges carry the
    /// NEW generation (it is adopted before the input is wired), so every
    /// edge the machine should see still arrives.
    fn player_event_is_load_scoped(event: &player::PlayerEvent) -> bool {
        !matches!(
            event,
            player::PlayerEvent::VolumeChanged(_)
                | player::PlayerEvent::RequestState(_)
                | player::PlayerEvent::ClockLost
                | player::PlayerEvent::StreamTagsUpdated
        )
    }

    fn handle_player_event(
        &mut self,
        event: player::PlayerEvent,
        generation: Option<u64>,
    ) -> Result<()> {
        // Exact supersession: every load-scoped event carries the generation
        // of the load it belongs to (stamped by fcastplaybin), so events
        // from a superseded or stopped load are dropped here in one place.
        // This replaces the per-event heuristics (have_media_info gates on
        // EOS/StreamsSelected, failed_uri matching on errors), which had
        // residual holes, e.g. a dying load's EOS processed after the new
        // load's first collection stopped the new item.
        if let Some(generation) = generation
            && Self::player_event_is_load_scoped(&event)
            && !self.player.is_event_current(generation)
        {
            debug!(generation, "Dropping player event from a superseded load");
            return Ok(());
        }
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
                if let Some(container) = tags.get::<gst::tags::ContainerFormat>() {
                    self.inspector_container = Some(container.get().to_string());
                }

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

                let echo_window = self
                    .last_volume_cmd
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
                debug!(volume, echo_window, "Player volume notify");
                if !echo_window {
                    self.broadcast_volume(volume as f32);
                }
            }
            player::PlayerEvent::StreamCollection(collection) => {
                self.player.handle_stream_collection(collection);
                // self.media_loaded_successfully();

                self.player.update_media_info();
                self.on_media_info_updated();

                self.gui.set_app_state(AppState::Playing);

                // self.current_duration = info.duration();
                // if info.number_of_video_streams() > 0 {
                //     self.video_stream_available()?;
                // }

                // NO transport driving here: `Player::uri_loaded` is the one
                // post-load transport driver (the collection-time auto-play
                // used to stomp a pause that landed mid-load, and un-paused
                // a paused pipeline when a live subtitle attach posted a
                // mid-playback collection).

                // Learn stream ids for externals that just materialized, then
                // advertise and enforce the parked desired selection (no-ops
                // otherwise).
                self.refresh_external_stream_sids();

                self.update_tracks(true);

                self.pump_fcast_sub_desire();

                if !self.have_media_info {
                    self.media_loaded_successfully();
                    self.have_media_info = true;
                }
            }
            player::PlayerEvent::AsyncDone => {
                // Settles an in-flight subtitle refresh (retrying it while
                // paused if no cue rendered) and dispatches track work queued
                // behind the async change.
                self.player.async_done();

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

                // Dispatch queued track work LAST: `on_media_info_updated`
                // above may just have launched the start seek, and a
                // selection dispatched before it would interleave with the
                // seek dance (its playsink reconfigure then runs outside
                // steady PLAYING, an observed permanent wedge). With the
                // seek already owning the state machine, the pump parks the
                // work until the dance commits.
                self.player.poll_track_ops();
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
            player::PlayerEvent::SubtitleRefreshFailed { seqnum } => {
                self.player.subtitle_refresh_failed(seqnum)
            }
            player::PlayerEvent::StreamsSelected {
                video,
                audio,
                subtitle,
                seqnum,
            } => {
                let selected = self.player.streams_selected(
                    video.as_deref(),
                    audio.as_deref(),
                    subtitle.as_deref(),
                    seqnum,
                );
                // The wire/GUI edge: map the applied stream ids to advertised
                // indices. Subtitles report an external's STABLE id when its
                // own stream is selected (matching TracksAvailable).
                let video_id = selected
                    .video
                    .as_deref()
                    .and_then(|sid| self.player.stream_idx_by_id(sid));
                let audio_id = selected
                    .audio
                    .as_deref()
                    .and_then(|sid| self.player.stream_idx_by_id(sid));
                let subtitle_id = self.advertised_subtitle_id(selected.subtitle.as_deref());
                self.gui.set_track_ids(
                    video_id.map(|i| i as i32).unwrap_or(-1),
                    audio_id.map(|i| i as i32).unwrap_or(-1),
                    subtitle_id.map(|i| i as i32).unwrap_or(-1),
                );

                if video.is_some() {
                    self.video_stream_available()?;
                } else {
                    self.video_stream_unavailable();
                }

                if self.updates_tx.strong_count() > 0 {
                    let msgs = vec![
                        v4::MessageBuilder::new()
                            .change_track(video_id, v4::flat::MediaTrackType::Video),
                        v4::MessageBuilder::new()
                            .change_track(audio_id, v4::flat::MediaTrackType::Audio),
                        v4::MessageBuilder::new()
                            .change_track(subtitle_id, v4::flat::MediaTrackType::Subtitle),
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
                self.broadcast_rate(new_rate as f32)?;
            }
            player::PlayerEvent::Error {
                origin: err_origin,
                kind,
                message,
                failed_uri,
            } => {
                #[cfg(debug_assertions)]
                self.player.dump_graph(remote_pipeline_dbg::Trigger::Error);
                // Attribution comes from fcastplaybin's generation-tagged
                // inputs (supersession is already handled by the generation
                // filter above); `failed_uri` is diagnostic only.
                match err_origin {
                    // A live subtitle input errored, never the main item, so
                    // playback keeps running. Whether the entry FAILS or
                    // merely re-arms is decided in
                    // `fail_or_rearm_fcast_external`.
                    fcastplaybin::ErrorOrigin::ExternalSubtitle(handle) => {
                        warn!(?failed_uri, message, "External subtitle input errored");
                        let ext_id = self.current_media.as_ref().and_then(|m| {
                            m.external_subtitles
                                .iter()
                                .find(|s| s.handle == Some(handle))
                                .map(|s| s.id)
                        });
                        match ext_id {
                            Some(ext_id) => self.fail_or_rearm_fcast_external(ext_id),
                            // The input was already detached (a re-arm or
                            // removal won the race); nothing to do.
                            None => debug!(?handle, "Error from an already-detached input"),
                        }
                    }
                    fcastplaybin::ErrorOrigin::Stale => {
                        debug!(?failed_uri, message, "Dropping error from a stale input");
                    }
                    fcastplaybin::ErrorOrigin::Main | fcastplaybin::ErrorOrigin::Unknown => {
                        self.player.stop();
                        if let Some(origin) = self.current_media.as_ref().map(|m| m.origin) {
                            self.send_error(origin, media_error_kind_to_error(kind));
                        }
                        self.media_error(message)?;
                    }
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
                // airplaysrc is built directly (encoded H.264/AAC ->
                // decodebin3, no fake-URI dispatch).
                let source = match media_source::build_airplay_mirror_source(&uri) {
                    Ok(element) => player::MediaInput::Element(element),
                    Err(err) => {
                        error!(?err, "Failed to build the AirPlay mirror source");
                        player::MediaInput::Uri(uri)
                    }
                };
                // No start seek: a mirror stream is live and has no text.
                self.player.load(source, None);
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
                    self.set_volume_cmd(volume);
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

                self.inspector_image = format!(
                    "{} {}x{}, {:?}",
                    img.format,
                    img.image.width(),
                    img.image.height(),
                    img.orientation
                );

                self.gui.set_image_preview(img);
                self.gui.set_app_state(AppState::Playing);

                self.media_loaded_successfully();
            }
            image::Event::DecodedAnimation { id, frames, format } => {
                if id != self.current_image_id {
                    warn!(id, "Ignoring old image decode result");
                    return Ok(false);
                }

                let size = frames
                    .first()
                    .map(|f| (f.image.width(), f.image.height()))
                    .unwrap_or((0, 0));
                self.inspector_image =
                    format!("{format} {}x{}, {} frames", size.0, size.1, frames.len());

                self.gui.set_animation(frames);
                self.gui.set_app_state(AppState::Playing);

                self.media_loaded_successfully();
            }
        }

        Ok(false)
    }

    fn refresh_inspector_graph(&self) {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };

        let Some(gui_tx) = self.gui.tx.clone() else {
            return;
        };

        let _ = gui_tx.send(gui::UpdateGuiCommand::SetInspectorDumping(true));

        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let timestamp = format!(
            "{:02}:{:02}:{:02} UTC",
            (secs / 3600) % 24,
            (secs / 60) % 60,
            secs % 60
        );

        // The dot walk runs on the player worker (serialized against loads
        // and teardowns, see `request_graph_dot_data`). The delivery callback
        // executes on that worker, so it only hands the layout work off to
        // the blocking pool.
        let runtime = tokio::runtime::Handle::current();
        self.player.request_graph_dot_data(move |dot| {
            runtime.spawn_blocking(move || {
                fn fail(gui_tx: &gui::UpdateGuiSender) {
                    let _ = gui_tx.send(gui::UpdateGuiCommand::SetInspectorDumping(false));
                }

                let child = Command::new("dot")
                    .arg("-Tjson")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn();
                let mut child = match child {
                    Ok(c) => c,
                    Err(err) => {
                        error!(?err, "Failed to spawn `dot` (is graphviz installed?)");
                        return fail(&gui_tx);
                    }
                };

                if let Some(mut stdin) = child.stdin.take()
                    && let Err(err) = stdin.write_all(dot.as_bytes())
                {
                    error!(?err, "Failed to write graph to graphviz");
                    return fail(&gui_tx);
                }

                let output = match child.wait_with_output() {
                    Ok(o) => o,
                    Err(err) => {
                        error!(?err, "graphviz layout failed");
                        return fail(&gui_tx);
                    }
                };

                match remote_pipeline_dbg::render::parse(&output.stdout) {
                    Ok(graph) => {
                        let _ = gui_tx.send(gui::UpdateGuiCommand::SetGraphDump(
                            gui::GraphDumpData {
                                trigger: "manual".to_string(),
                                timestamp,
                                graph,
                            }
                            .into(),
                        ));
                    }
                    Err(err) => {
                        error!(?err, "Failed to parse graphviz output");
                        fail(&gui_tx);
                    }
                }
            });
        });
    }

    /// One inspector sample: bitrate history (diffing the selected streams'
    /// cumulative parsed-byte counters against the previous tick), the track
    /// table, container, sink stats and player internals, pushed to the GUI
    /// as one command.
    fn inspector_tick(&mut self) {
        let stats = self.player.stream_io_stats();

        // The tapped input-side stream ids match the collection's ids for
        // parsed containers. When they don't (a single sid-less input), fall
        // back to the first tap of the right caps kind.
        let sample = |current_sid: Option<&str>, kind: &str| -> Option<(String, u64)> {
            let by_sid = stats
                .iter()
                .find(|s| s.stream_id.as_deref() == current_sid && current_sid.is_some());
            let by_kind = || {
                stats.iter().find(|s| {
                    s.external.is_none()
                        && s.caps
                            .as_ref()
                            .and_then(|c| c.structure(0))
                            .is_some_and(|structure| structure.name().as_str().starts_with(kind))
                })
            };
            by_sid.or_else(by_kind).map(|s| {
                (
                    s.stream_id.clone().unwrap_or_else(|| kind.to_string()),
                    s.bytes,
                )
            })
        };
        let video = sample(self.player.current_video_sid(), "video/");
        let audio = sample(self.player.current_audio_sid(), "audio/");

        let now = Instant::now();
        let dt = self
            .inspector_bitrates
            .last_at
            .map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        self.inspector_bitrates.last_at = Some(now);

        let probe = &mut self.inspector_bitrates;
        InspectorBitrates::push(&mut probe.video_kbps, &mut probe.last_video, video, dt);
        InspectorBitrates::push(&mut probe.audio_kbps, &mut probe.last_audio, audio, dt);

        self.gui.set_inspector_sample(gui::InspectorSample {
            video_kbps: probe.video_kbps.iter().copied().collect(),
            audio_kbps: probe.audio_kbps.iter().copied().collect(),
            tracks: self
                .player
                .stream_dbg_rows()
                .iter()
                .map(|(stream, selected)| Self::inspector_track_row(stream, *selected))
                .collect(),
            container: self.inspector_container.clone().unwrap_or_default(),
            sources: self.inspector_source_lines(),
            internals: self.inspector_internals(),
            sinks: self.inspector_sink_lines(),
            image: self.inspector_image.clone(),
        });
    }

    /// The sources card's lines: one per live input, showing the uri's
    /// protocol and hostname when the element has a uri, and the element
    /// factory either way.
    fn inspector_source_lines(&self) -> Vec<String> {
        self.player
            .dbg_sources()
            .into_iter()
            .map(|source| {
                let mut line = match source.uri.as_deref().map(url::Url::parse) {
                    Some(Ok(uri)) => {
                        let host = uri
                            .host_str()
                            .map(|host| format!("://{host}"))
                            .unwrap_or_default();
                        format!("{}{host} ({})", uri.scheme(), source.factory)
                    }
                    Some(Err(_)) => format!("unparseable uri ({})", source.factory),
                    None => source.factory,
                };
                if source.external.is_some() {
                    line = format!("subtitle: {line}");
                }
                line
            })
            .collect()
    }

    /// One track-table row from an advertised stream.
    fn inspector_track_row(stream: &gst::Stream, selected: bool) -> gui::InspectorTrackRow {
        let ty = stream.stream_type();
        let kind = if ty.contains(gst::StreamType::VIDEO) {
            "Video"
        } else if ty.contains(gst::StreamType::AUDIO) {
            "Audio"
        } else if ty.contains(gst::StreamType::TEXT) {
            "Text"
        } else {
            "Other"
        };

        let caps = stream.caps();
        let codec = caps
            .as_ref()
            .map(|c| gst_pbutils::pb_utils_get_codec_description(c).to_string())
            .unwrap_or_default();

        let mut detail = String::new();
        if let Some(s) = caps.as_ref().and_then(|c| c.structure(0)) {
            if let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) {
                detail = format!("{w}x{h}");
                if let Ok(fps) = s.get::<gst::Fraction>("framerate")
                    && fps.denom() != 0
                {
                    detail += &format!(" {:.3}fps", fps.numer() as f64 / fps.denom() as f64);
                }
            } else if let Ok(rate) = s.get::<i32>("rate") {
                detail = format!("{rate} Hz");
                if let Ok(ch) = s.get::<i32>("channels") {
                    detail += &format!(" {ch}ch");
                }
            }
        }

        let tags = stream.tags();
        let language = tags
            .as_ref()
            .and_then(|t| t.get::<gst::tags::LanguageCode>())
            .map(|v| v.get().to_string())
            .unwrap_or_default();
        if let Some(bitrate) = tags.as_ref().and_then(|t| t.get::<gst::tags::Bitrate>()) {
            let kbps = bitrate.get() / 1000;
            if kbps > 0 {
                if !detail.is_empty() {
                    detail += ", ";
                }
                detail += &format!("{kbps} kbit/s");
            }
        }

        gui::InspectorTrackRow {
            kind: kind.to_string(),
            codec,
            detail,
            language,
            selected,
        }
    }

    /// The internals card's lines: pipeline/player state, routing, external
    /// subtitle catalog, and any element stuck in a state transition.
    fn inspector_internals(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let (current, pending) = self.player.dbg_state_summary();
        lines.push(format!("pipeline: {current:?} -> {pending:?}"));
        lines.push(format!(
            "player: {:?}, rate {}, gen {}",
            self.player.player_state(),
            self.player.rate(),
            self.player
                .dbg_generation()
                .map_or_else(|| "-".to_string(), |g| g.to_string()),
        ));
        let routed = self.player.dbg_routed_summary();
        lines.push(format!(
            "routed: {}",
            if routed.is_empty() {
                "none".to_string()
            } else {
                routed.join(", ")
            }
        ));
        let unsettled = self.player.dbg_unsettled_elements();
        if !unsettled.is_empty() {
            lines.push(format!("unsettled: {}", unsettled.join(", ")));
        }
        if let Some(media) = self.current_media.as_ref() {
            for ext in &media.external_subtitles {
                lines.push(format!(
                    "external sub [{}] {}: {}",
                    ext.id,
                    ext.name.as_deref().unwrap_or("unnamed"),
                    if ext.stream_sid.is_some() {
                        "materialized"
                    } else {
                        "pending"
                    },
                ));
            }
        }
        lines
    }

    /// The sink card's lines: video QoS counters and the audio sink's
    /// negotiated format plus counters.
    fn inspector_sink_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(stats) = self.player.dbg_video_sink_stats() {
            lines.push(format!(
                "video: {} rendered, {} dropped",
                stats.get::<u64>("rendered").unwrap_or(0),
                stats.get::<u64>("dropped").unwrap_or(0),
            ));
        }
        match self.player.dbg_audio_sink_health() {
            Some((caps, stats)) => {
                let format = caps
                    .as_ref()
                    .and_then(|c| c.structure(0))
                    .map(|s| {
                        format!(
                            "{} {} Hz {}ch",
                            s.get::<&str>("format").unwrap_or("?"),
                            s.get::<i32>("rate").unwrap_or(0),
                            s.get::<i32>("channels").unwrap_or(0),
                        )
                    })
                    .unwrap_or_else(|| "not negotiated".to_string());
                lines.push(format!(
                    "audio: {format}, {} rendered, {} dropped",
                    stats.get::<u64>("rendered").unwrap_or(0),
                    stats.get::<u64>("dropped").unwrap_or(0),
                ));
            }
            None => lines.push("audio: no sink".to_string()),
        }
        lines
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

                let wire_id = if id >= 0 { Some(id as u32) } else { None };

                // Subtitles share the protocol ChangeTrack path (ids may name
                // a virtual external track not present in the stream list).
                if matches!(variant, UiMediaTrackType::Subtitle) {
                    self.change_subtitle_track(PacketOrigin::Gui, wire_id);
                    return Ok(false);
                }

                // GUI ids are indices into our own advertised list; a stale
                // one (list changed under the picker) resolves to None.
                let sid = wire_id.and_then(|i| self.player.stream_id_of(i));

                // Latest-wins and serialized against other track operations in
                // the player (see player::TrackOps): rapid picker changes
                // can't pile up overlapping playbin re-prerolls.
                let kind = match variant {
                    UiMediaTrackType::Video => player::TrackKind::Video,
                    UiMediaTrackType::Audio => player::TrackKind::Audio,
                    UiMediaTrackType::Subtitle => unreachable!(),
                };
                self.apply_track_change(kind, sid);
            }
            Message::NewPlayerEvent { event, generation } => {
                self.handle_player_event(event, generation)?;
            }
            Message::ShouldSetLoadingStatus(id) => {
                if id == self.current_media_item_id && self.is_loading_media {
                    self.gui.set_app_state(AppState::LoadingMedia);
                }
            }
            Message::PendingSubtitleAddCheck { item, epoch } => {
                // Epoch mismatch means the parked list was already drained
                // (applied or rejected) since this timer was armed.
                if epoch == self.pending_subtitle_add_epoch
                    && !self.pending_subtitle_adds.is_empty()
                {
                    warn!(
                        item,
                        "Seekability never resolved; rejecting parked subtitle source(s)"
                    );
                    self.reject_pending_subtitle_adds();
                }
            }
            Message::PendingSeekCheck { epoch } => {
                if epoch == self.pending_seek_epoch && self.pending_seek_op.is_some() {
                    warn!("Seekability never resolved; dropping the parked seek");
                    self.drop_pending_seek();
                }
            }
            Message::FcastExternalSubCheck { item, ext_id } => {
                // Fail only if the external is STILL attached and awaiting
                // its stream. A detached entry (switched away) legitimately
                // has no `stream_sid`, and an already-materialized one has
                // its `stream_sid` set, neither is a failure, so a stale
                // watchdog from an earlier attach must no-op.
                if item == self.current_media_item_id
                    && let Some(media) = self.current_media.as_ref()
                    && let Some(entry) = media.external_subtitles.iter().find(|s| s.id == ext_id)
                    && entry.handle.is_some()
                    && entry.stream_sid.is_none()
                {
                    warn!(ext_id, "External subtitle stream never materialized");
                    self.fail_fcast_external_subtitle(ext_id);
                }
            }
            Message::LoadStallCheck { item, epoch } => {
                // DIAGNOSTIC only. Fire iff this is still the load we armed for
                // (epoch + item) and the pipeline has NOT reached a steady
                // PAUSED. A slow-but-progressing preroll (extreme GPU
                // contention) can also trip this, the dumped collection-vs-
                // routed tells the two apart (a selected stream kind with no
                // routed pad = the genuine stall).
                if epoch == self.load_watchdog_epoch
                    && item == self.current_media_item_id
                    && !self.player.is_pipeline_stable()
                {
                    self.player
                        .log_load_stall_diagnostics(&format!("item{item}"));
                }
            }
            Message::Raop(event) => return self.handle_raop_event(event),
            #[cfg(feature = "airplay")]
            Message::AirPlay(event) => return self.handle_airplay_event(event),
            #[cfg(debug_assertions)]
            Message::DumpPipeline => {
                self.player.dump_graph(remote_pipeline_dbg::Trigger::Manual);
            }
            Message::InspectorRefresh => self.refresh_inspector_graph(),
            Message::InspectorBitrateTick => self.inspector_tick(),
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
