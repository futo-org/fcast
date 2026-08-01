use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
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
    queue_cache, raop,
    utils::{current_time_millis, map_to_header_map},
};
#[cfg(not(target_os = "android"))]
use crate::{Settings, mdns};
#[cfg(feature = "airplay")]
use crate::{airplay, message::AirPlay};

const SENDER_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_TICK_INTERVAL: Duration = Duration::from_millis(100);
/// Buffered-range/nub polling is cheap but not free (the stream-mode nub walks
/// the pipeline), the buffered amount changes slowly, and the scrubber is
/// rarely on screen, so throttle it well below the 100 ms progress tick.
const BUFFERED_RANGES_INTERVAL: Duration = Duration::from_millis(1000);
/// How close the reported position must get to a GUI seek target before the optimistic thumb hold
/// is released. Seeks are ACCURATE so they land exactly, this only absorbs the tick's sampling
/// drift once playback resumes.
const SEEK_HOLD_TOLERANCE: f64 = 0.75;
/// Safety net: release the thumb hold even if the pipeline never reports the target (a
/// dropped/failed seek), so the thumb can't stay frozen forever.
const SEEK_HOLD_TIMEOUT: Duration = Duration::from_secs(12);
/// Pause after a failed `accept()` before taking the listener stream again.
///
/// A failing accept (`EMFILE`, `ECONNABORTED`) returns immediately, so without
/// this the select loop spins at full tilt until the condition clears. Short
/// enough that a transient failure is invisible, long enough to bound the spin.
pub(crate) const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

#[cfg(any(target_os = "macos", target_os = "windows"))]
const UPDATER_BASE_URL: &str = "http://dl.fcast.org/receiver/desktop";

/// State for an in-flight optimistic GUI seek (see `Application::gui_seek_hold`).
struct GuiSeekHold {
    /// Requested position, in seconds (clamped to the media duration).
    target: f64,
    /// When the hold was armed, for the `SEEK_HOLD_TIMEOUT` safety net.
    since: Instant,
}

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
    /// The spec'd Queue.autoplay flag: the receiver advances to the next
    /// item by itself when the current one finishes.
    autoplay: bool,
}

/// A gapless pre-arm in flight: the next autoplay queue item is prepared on
/// the live pipeline (`Player::prepare_next`) and activates at the current
/// item's drain with no teardown, preroll, or pipeline EOS in between.
struct GaplessPrearm {
    /// The generation the prepared item adopts at activation (validates the
    /// GaplessActivated event).
    generation: u64,
    /// The queue index the receiver advances to at activation.
    next_index: usize,
    /// The prepared item's URL, captured at arm time. Identifies the item
    /// when a declined cancel's activation has to be adopted after the
    /// queue moved underneath it (`next_index` is only a hint then).
    url: String,
    /// A cancel was requested but its outcome is not known yet. The pre-arm
    /// is KEPT in this state: the cancel races the pipeline's swap and a
    /// declined cancel activates anyway, so the bookkeeping has to survive
    /// long enough to adopt that activation instead of resync-reloading the
    /// item that just finished.
    cancelling: bool,
}

/// An operation held back until a gapless pre-arm's cancellation reports its
/// outcome (see [`Application::gapless_parked_op`]). Every payload here is
/// already validated, so a replay cannot produce a second error reply and
/// cannot re-derive a different answer than the original request did.
#[derive(Debug)]
enum GaplessParkedOp {
    /// A user seek, already range-clamped (its `SeekOutOfRange` reply, if
    /// any, went out when the operation arrived).
    Seek {
        origin: PacketOrigin,
        time: gst::ClockTime,
    },
    /// A real speed change. An idempotent one never parks: it performs no
    /// seek at all and is confirmed on the spot.
    SetSpeed { origin: PacketOrigin, rate: f32 },
    /// A validated audio/video selection, already resolved from the wire's
    /// track index to a stream id (`None` disables the slot).
    TrackChange {
        kind: player::TrackKind,
        sid: Option<player::StreamId>,
    },
    /// A validated subtitle selection: an advertised stream, "off", or an
    /// external whose own stream has not materialized yet.
    SubtitleChange {
        origin: PacketOrigin,
        target: SubtitleTarget,
    },
}

/// Which kind of operation is parked. The outcome policy below depends on the
/// operation's *shape*, not on its payload, so it is decided from this alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GaplessParkedOpKind {
    Seek,
    SetSpeed,
    TrackChange,
    SubtitleChange,
}

impl GaplessParkedOp {
    fn kind(&self) -> GaplessParkedOpKind {
        match self {
            Self::Seek { .. } => GaplessParkedOpKind::Seek,
            Self::SetSpeed { .. } => GaplessParkedOpKind::SetSpeed,
            Self::TrackChange { .. } => GaplessParkedOpKind::TrackChange,
            Self::SubtitleChange { .. } => GaplessParkedOpKind::SubtitleChange,
        }
    }
}

/// The pipeline state a parked operation is resolved against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GaplessOutcome {
    /// The prepare is gone and nothing will activate (`GaplessCancelled`,
    /// `GaplessPrepareFailed`). The playing item is the only linked input
    /// again, so the operation applies exactly as a fresh one would.
    PrepareGone,
    /// The swap already performed (`GaplessCancelDeclined`, or an activation
    /// that beat that report to the loop). The playing item's input is
    /// unlinked, so a flushing seek would be answered by the SUCCESSOR's
    /// source: whatever the pipeline still owes the user has to be reloaded
    /// rather than seeked.
    SwapPerformed,
    /// The item ended before any outcome arrived. The end-of-stream advance
    /// owns the transition and the operation has no item left to act on.
    ItemEnded,
}

/// What to do with a parked operation once the outcome is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParkedOpAction {
    /// Run it now, through the same code path a fresh operation takes.
    Replay,
    /// Reload the playing item at the operation's target instead of seeking
    /// into a pipeline whose only linked upstream is the next item.
    ReloadAtTarget,
    /// Give up on it, deliberately (see `parked_op_action`).
    Drop,
}

/// The whole park-until-outcome policy, in one pure function.
///
/// The one judgement call is `(TrackChange, SwapPerformed)`: it is dropped, not
/// reloaded. A switch flushed into the successor is corruption, so applying it
/// is not an option; and reloading is not one either, because the desired
/// selection is a stream id resolved against the RETIRING item's collection and
/// re-resolving it against the reloaded item's collection is the selection
/// engine's business, not this decision's. A seek and a speed change have no
/// such dependency: their target is a plain position and rate that a load can
/// carry directly.
///
/// The honest caveat: "the item is nearly over anyway" is only true for a long
/// item. The swap performs once the item's INPUT has drained, which for a short
/// or cached item happens near the start, so a dropped switch can land well
/// before the end. Losing a switch is still strictly better than corrupting
/// playback, and the follow-up belongs with the pre-arm margin (CLEANUP.md
/// already flags sub-40s items arming on their first tick).
fn parked_op_action(kind: GaplessParkedOpKind, outcome: GaplessOutcome) -> ParkedOpAction {
    use GaplessOutcome as O;
    use GaplessParkedOpKind as K;
    match (kind, outcome) {
        (_, O::ItemEnded) => ParkedOpAction::Drop,
        (_, O::PrepareGone) => ParkedOpAction::Replay,
        (K::Seek | K::SetSpeed, O::SwapPerformed) => ParkedOpAction::ReloadAtTarget,
        (K::TrackChange | K::SubtitleChange, O::SwapPerformed) => ParkedOpAction::Drop,
    }
}

/// Filter a duration query result down to what may be CACHED in
/// `current_duration`, i.e. a real answer about the current item.
///
/// A failed query and a zero duration are both "not known yet", and latching
/// either poisons everything downstream for the rest of the session: `dur: 0`
/// on the wire, a dead buffered-range nub, the seek clamp disabled,
/// `SeekPercent` mapping to 0, and `maybe_prearm_gapless`'s non-zero filter
/// suppressing every further gapless pre-arm. The cache stays `None` instead,
/// so the next progress tick simply asks again.
fn cacheable_duration(queried: Option<gst::ClockTime>) -> Option<gst::ClockTime> {
    queried.filter(|duration| !duration.is_zero())
}

/// Convert a wire `showDuration` (seconds, as a bare `f64`) into a timer delay.
fn show_duration_delay(show_duration: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(show_duration).ok()
}

fn image_download_error_kind(err: &image::DownloadImageError) -> ErrorKind {
    use image::DownloadImageError as E;
    match err {
        E::RequestFailed(_)
        | E::Unsuccessful(_)
        | E::InvalidUrl(_)
        | E::UnsupportedScheme(_)
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
    /// The live input attached for this entry (every catalog external is
    /// attached simultaneously, selection is pure SELECT_STREAMS). The id
    /// is stable for the entry's whole life: fcastplaybin re-arms a dead
    /// deselected input internally under the same id, and a genuine
    /// failure comes back as `PlayerEvent::ExternalSubtitleFailed`, which
    /// removes the entry.
    handle: fcastplaybin::ExternalSubId,
    /// The entry's GStreamer stream id, learned when its stream first
    /// materializes in a collection. URI-derived, so it stays valid across
    /// fcastplaybin's internal input replacements. All id/index mapping
    /// goes through this, never through the live handle.
    stream_sid: Option<String>,
}

/// An `AddSubtitleSource` that arrived before the receiver could act on it: either while the load
/// it targets is still in flight, or after the load but before the pipeline could answer the
/// seekability query.
struct PendingSubtitleAdd {
    url: String,
    select: bool,
    name: Option<SmolStr>,
    origin: PacketOrigin,
}

#[derive(Debug)]
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
            next_external_id: 0,
        }
    }

    /// Drop every external subtitle (external subtitles are per-item). The id counter keeps
    /// advancing so a stale id from the previous item can never alias a new one.
    fn clear_external_subtitles(&mut self) {
        self.external_subtitles.clear();
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
    last_buffered_push: Option<Instant>,
    last_volume_cmd: Option<Instant>,
    pending_seek_op: Option<(PacketOrigin, gst::ClockTime)>,
    pending_seek_epoch: u64,
    /// Active optimistic hold for a GUI-originated seek: the slider thumb stays
    /// pinned at `target` (and the GUI's `seek-pending` flag stays set) until the
    /// pipeline reports it has landed there, so a stale position tick can't
    /// spring the thumb back to the pre-seek position.
    gui_seek_hold: Option<GuiSeekHold>,
    /// DIAGNOSTIC (load-stall investigation): bumped per pipeline load so a
    /// stale `LoadStallCheck` watchdog no-ops.
    load_watchdog_epoch: u64,
    current_image_id: image::ImageId,
    current_image_download_id: image::ImageDownloadId,
    /// True while the current load is an image routed through the player
    /// pipeline (fimagedec) rather than the legacy in-GUI image downloader.
    /// Progress traffic is suppressed for these loads and the image view is
    /// painted transparent so the video sink shows through.
    image_via_player: bool,
    have_audio_track_cover: bool,
    current_media: Option<MediaSourceState>,
    have_media_info: bool,
    current_thumbnail_id: image::ImageId,
    current_addresses: HashSet<IpAddr>,
    /// The port the FCast TCP listener actually bound. Normally
    /// `FCAST_TCP_PORT`, but the user may relocate it on a port conflict. The
    /// QR code / network config advertise this so discovery stays correct.
    fcast_port: u16,
    /// False until the FCast port is actually bound. While false (e.g. during
    /// the port-conflict dialog) we don't publish the connection QR/IP panel,
    /// since we aren't listening on any port yet.
    port_committed: bool,
    have_media_title: bool,
    // Last artist pushed to the UI. GStreamer re-emits the artist tag many
    // times per item and (unlike the title) gapless has no queue-metadata artist
    // to gate on, so we dedup by value instead of a "seen it" boolean.
    last_artist_name: Option<String>,
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
    /// Prefetched bytes for the queue items around the current index, so
    /// selecting a neighbor serves from memory (see `queue_cache`).
    queue_cache: queue_cache::Cache,
    queue_prefetcher: queue_cache::Prefetcher,
    /// A gapless pre-arm in flight (see [`GaplessPrearm`]). Armed from the
    /// progress tick near the current item's end, consumed by
    /// GaplessActivated, cancelled by seeks, speed changes, queue
    /// mutations, and anything that replaces playback.
    gapless_prearm: Option<GaplessPrearm>,
    /// The one operation held back until a pending pre-arm's cancellation
    /// reports its outcome (see [`GaplessParkedOp`]).
    ///
    /// This is NOT [`Self::pending_seek_op`]: that one parks a seek until the
    /// pipeline can answer the seekability query, on its own 10s timer, and
    /// only ever holds a seek. This park is scoped to a single cancel
    /// round-trip instead, has no timer at all (fcastplaybin reports exactly
    /// one outcome per cancel, and a failed prepare or a fresh load cover the
    /// rest), and holds any of the operations that reach the pipeline as a
    /// flushing seek. The two compose: a replayed seek can still end up parked
    /// in `pending_seek_op` afterwards.
    ///
    /// Invariant: a parked operation only ever exists alongside a
    /// `cancelling` pre-arm, because parking and requesting the cancel happen
    /// together. Every site that clears `gapless_prearm` therefore also has to
    /// decide what happens to this.
    gapless_parked_op: Option<GaplessParkedOp>,
    /// Start position/rate for the NEXT `load_current_media_item`, overriding
    /// the item's own `time`/`speed`. Set when a load stands in for an
    /// operation the pipeline can no longer serve (a seek or speed change
    /// whose gapless cancel was declined), so the item resumes where the user
    /// asked instead of at its start. Consumed unconditionally by the next
    /// load so it can never leak into a later one.
    load_start_override: Option<player::RestorePoint>,
    /// The media item id whose pre-arm FAILED: no re-arm for the same item
    /// (each progress tick would otherwise retry into the same failure).
    /// The ordinary end-of-stream advance owns that transition instead.
    gapless_blocked_item: Option<MediaItemId>,
    /// Kill switch: FCAST_NO_GAPLESS=1 turns the pre-arm off and every
    /// autoplay advance goes through the ordinary EOS-then-load path.
    gapless_enabled: bool,
    screensaver_inhibitor: inhibit_screensaver::Inhibitor,
    tls_acceptor: tokio_rustls::TlsAcceptor,
    companion_ctx: CompanionContext,
    #[cfg(feature = "airplay")]
    airplay_context: airplay::AirPlayContext,
    receiver_info: Arc<crate::ReceiverInfo>,
    fcast_txt_records: HashMap<String, String>,
    fcast_senders: HashMap<SenderId, FCastSenderHandle>,
    inspector_bitrates: InspectorBitrates,
    /// Whether the inspector is currently open. Gates all inspector work so
    /// nothing is computed or sent while it's closed.
    inspector_active: bool,
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
        let mdns = mdns::start_daemon(&msg_tx, &settings)?;

        let run_gcast = if cfg!(not(target_os = "android")) {
            settings.google_cast_enabled()
        } else {
            true
        };

        let gcast_tx = if run_gcast {
            let (gcast_tx, gcast_rx) = mpsc::unbounded_channel::<gcast::StatusUpdate>();
            tokio::spawn({
                let msg_tx = msg_tx.clone();
                async move {
                    // A failed bind (e.g. port 8009 held by another receiver)
                    // shouldn't take the process down, so just log and skip gcast.
                    if let Err(err) = gcast::run_server(msg_tx, gcast_rx).await {
                        warn!(?err, "Google Cast server stopped (port 8009 may be in use)");
                    }
                }
            });
            GCastUpdateSender(Some(gcast_tx))
        } else {
            GCastUpdateSender(None)
        };

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
        let queue_prefetcher = queue_cache::Prefetcher::new(
            msg_tx.clone(),
            http_client.clone(),
            companion_ctx.clone(),
        );

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
            last_buffered_push: None,
            last_volume_cmd: None,
            pending_seek_op: None,
            pending_seek_epoch: 0,
            gui_seek_hold: None,
            load_watchdog_epoch: 0,
            current_image_id: 0,
            image_via_player: false,
            have_audio_track_cover: false,
            current_media: None,
            have_media_info: false,
            current_thumbnail_id: 0,
            current_image_download_id: 0,
            inspector_bitrates: InspectorBitrates::default(),
            inspector_active: false,
            inspector_container: None,
            inspector_image: String::new(),
            current_addresses: HashSet::new(),
            fcast_port: FCAST_TCP_PORT,
            port_committed: false,
            have_media_title: false,
            last_artist_name: None,
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
            queue_cache: queue_cache::Cache::new(),
            queue_prefetcher,
            gapless_prearm: None,
            gapless_parked_op: None,
            load_start_override: None,
            gapless_blocked_item: None,
            gapless_enabled: !std::env::var("FCAST_NO_GAPLESS").is_ok_and(|v| v == "1"),
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
        let sender_id = match origin {
            PacketOrigin::Gui => Some(0),
            PacketOrigin::FCast {sender_id, ..} => Some(sender_id),
            _ => None,
        };

        if let Some(sender_id) = sender_id && self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(
                fcast::V4Message::RelayToOtherSenders {
                    initiator_session_id: sender_id,
                    serialized_msg,
                },
            ));
        }
    }

    /// Push the scrubber's buffered indicator. Prefers real timeline ranges (download/timeshift
    /// buffering); in the receiver's STREAM mode those are empty, so it falls back to a single
    /// "buffered ahead of the playhead" nub sized from the queue depth. Empty (no bar) when neither
    /// is known.
    ///
    /// Throttled to [`BUFFERED_RANGES_INTERVAL`]: it is driven from the 100 ms progress tick and
    /// from state edges, but the buffered amount changes slowly and the stream-mode nub walks the
    /// pipeline, so polling every tick is wasteful.
    fn push_buffered_ranges(&mut self) {
        if self
            .last_buffered_push
            .is_some_and(|at| at.elapsed() < BUFFERED_RANGES_INTERVAL)
        {
            return;
        }
        self.last_buffered_push = Some(Instant::now());

        let ranges = self.player.buffered_ranges();
        if !ranges.is_empty() {
            let ranges = ranges
                .into_iter()
                .map(|r| (r.start as f32, r.stop as f32))
                .collect();
            self.gui.set_buffered_ranges(ranges);
            return;
        }

        // STREAM mode: draw [position, position + buffered-ahead] as one nub.
        let nub = self.buffered_ahead_range();
        self.gui.set_buffered_ranges(nub.into_iter().collect());
    }

    /// The buffered-ahead nub as a single `(start, stop)` timeline fraction, or `None` when the
    /// ahead duration or total duration is unknown.
    fn buffered_ahead_range(&self) -> Option<(f32, f32)> {
        let duration = self.current_duration?.seconds_f64();
        if duration <= 0.0 {
            return None;
        }
        let ahead = self.player.buffered_ahead()?.seconds_f64();
        let position = self.player.get_position()?.seconds_f64();
        let start = (position / duration).clamp(0.0, 1.0);
        let stop = ((position + ahead) / duration).clamp(0.0, 1.0);
        (stop > start).then_some((start as f32, stop as f32))
    }

    fn playback_progress_changed(&mut self) {
        let position = self.player.get_position().unwrap_or(gst::ClockTime::ZERO);
        let duration = self.current_duration.unwrap_or(gst::ClockTime::ZERO);

        // A seek's own settle (ASYNC_DONE while paused) reaches the thumb through here, not the
        // tick, so release the hold on this path too.
        self.release_seek_hold_if_landed(position.seconds_f64());
        self.gui
            .update_playback_progress(position.seconds_f64() as f32, duration.seconds_f64() as f32);
        self.push_buffered_ranges();

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
        // A pipeline image has no meaningful progress, so send nothing (see
        // `notify_updates`).
        if self.image_via_player {
            return;
        }
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
    /// Release the optimistic GUI seek hold once the pipeline has actually landed on the requested
    /// position (or a safety timeout elapses), so the slider thumb stops being pinned to the seek
    /// target and resumes following playback.
    fn release_seek_hold_if_landed(&mut self, position: f64) {
        let Some(hold) = self.gui_seek_hold.as_ref() else {
            return;
        };
        let landed = (position - hold.target).abs() <= SEEK_HOLD_TOLERANCE;
        let expired = hold.since.elapsed() >= SEEK_HOLD_TIMEOUT;
        if !landed && !expired {
            return;
        }
        if expired && !landed {
            debug!(
                target = hold.target,
                position, "GUI seek hold timed out before the pipeline reported the target"
            );
        }
        self.gui_seek_hold = None;
        self.gui.set_seek_pending(false);
    }

    fn notify_updates(&mut self, force: bool) -> Result<()> {
        // A pipeline image loops forever and has no meaningful position or
        // duration, so it produces no progress traffic (matching the legacy
        // in-GUI image path). Other broadcasts (playback state and so on) are
        // unaffected.
        if self.image_via_player {
            return Ok(());
        }
        if !self.player.have_media_info() || self.player.is_seeking() {
            return Ok(());
        }

        let Some(position) = self.player.get_position() else {
            error!("player does not have a playback position");
            return Ok(());
        };
        let position = position.seconds_f64();
        // Once the pipeline has caught up to a GUI seek, let the thumb follow playback again (must
        // precede the progress write below, which is suppressed while the hold is active).
        self.release_seek_hold_if_landed(position);
        self.last_position_updated = position;
        // The lazy duration read, and deliberately ONE-SHOT per item: it only
        // runs while the cache is empty. A re-query mid-item could be answered
        // by the NEXT item once a gapless swap has performed (up to ~30s before
        // the app learns of it), and would then latch the successor's duration
        // onto the item still playing. fcastplaybin drops its own
        // `DurationChanged` inside that window as the primary guard. This is
        // the second one.
        //
        // Only a real answer is cached (see `cacheable_duration`). A failed or
        // zero query reports 0 for THIS tick and leaves the cache empty, so the
        // next tick retries, instead of latching a zero that would kill the seek
        // clamp and every remaining gapless pre-arm.
        let duration = match self.current_duration {
            Some(dur) => dur,
            None => {
                let queried = self.player.get_duration();
                self.current_duration = cacheable_duration(queried);
                queried.unwrap_or_default()
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
        self.push_buffered_ranges();

        if self.should_broadcast()
            && (self.last_sent_update.elapsed() >= SENDER_UPDATE_INTERVAL || force)
        {
            let update = v3::PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: Some(position),
                duration: Some(duration),
                // NOT derived from the GUI Loading state, which collapses a
                // mid-playback Buffering into Idle: that reads as "playback
                // ended" on the wire and makes senders advance the queue
                // during a gapless handoff (see `project_wire_state`).
                state: self.player.wire_playback_state(),
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
        // Playback is being replaced: any pending gapless pre-arm targets
        // media that is going away (the player's load reset drops the
        // prepared input; this clears the bookkeeping).
        self.gapless_prearm = None;
        self.player.clear_pending_gapless();
        // An operation parked on that pre-arm's outcome targets the media
        // going away too, and a fresh load supersedes it outright.
        self.gapless_parked_op = None;
        // Parked subtitle adds and seeks target the media that is going
        // away. (The player's own per-load state, the text-restore dance,
        // held seeks, parked deselects, is reset by `Player::stop` below.)
        self.reject_pending_subtitle_adds();
        self.drop_pending_seek();
        if self.gui_seek_hold.take().is_some() {
            self.gui.set_seek_pending(false);
        }
        self.have_audio_track_cover = false;
        self.have_media_info = false;
        self.have_media_title = false;
        self.last_artist_name = None;
        self.last_position_updated = -1.0;
        // The next load re-arms this if it is another pipeline image.
        self.image_via_player = false;
        self.gui.set_image_via_player(false);
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

    /// Arm the show-duration timer for `id`.
    ///
    /// `show_duration` arrives as a bare `f64` on the v3 wire, so a negative,
    /// NaN or absurdly large value would panic inside `Duration::from_secs_f64`.
    /// Reject it here instead: an item without a usable duration plays until
    /// EOS, which is already what an item with no duration at all does.
    fn arm_show_duration(&self, show_duration: f64, id: MediaItemId) {
        let Some(after) = show_duration_delay(show_duration) else {
            warn!(show_duration, "Ignoring invalid showDuration");
            return;
        };

        let msg_tx = self.msg_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            msg_tx.send(Message::MediaItemFinish(id));
        });
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
                    self.arm_show_duration(show_duration, self.current_media_item_id);
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
            MediaSource::Queue(queue) => {
                // The spec'd playback_duration: when it elapses the item is
                // "finished", which is what autoplay advances on. Images and
                // animations never post EOS (fimagedec parks stills and loops
                // animations), so a photo slideshow only ever advances through
                // this timer. Items without a duration keep playing until EOS
                // (or, for images, until a sender selects something else).
                if queue.autoplay
                    && let Some(show_duration) = queue
                        .items
                        .get(queue.current_idx as usize)
                        .and_then(|item| item.show_duration)
                {
                    self.arm_show_duration(show_duration, self.current_media_item_id);
                }
            }
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
        self.queue_cache.clear();

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

        // Special case for when there's a google cast sender connected.
        // An autoplay queue with a next item is exempt: the receiver-side
        // advance (`maybe_autoplay_advance`, which runs after this in the
        // EOS path) must keep working after the last sender disconnects,
        // that is the fire-and-forget use case autoplay exists for.
        if self.updates_tx.receiver_count() == 0 && self.autoplay_next_index().is_none() {
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
        self.sync_queue_cache();
    }

    /// Cached prefetch entry usable for this load. Consulted only for
    /// queue-sourced loads of cacheable containers: a Single load of a url
    /// lingering in the cache must not serve possibly stale bytes, and
    /// adaptive-streaming manifests (HLS, DASH) must never play from memory
    /// (their demuxers need the upstream URI context for relative fragment
    /// references and playlist reloads, which the in-memory source cannot
    /// answer; see `queue_cache::cacheable_container`).
    fn queue_cache_entry(&self, url: &str, container: &str) -> Option<queue_cache::CachedItem> {
        if !matches!(
            self.current_media.as_ref().map(|m| &m.source),
            Some(MediaSource::Queue(_))
        ) {
            return None;
        }
        if !queue_cache::cacheable_container(container) {
            return None;
        }
        self.queue_cache.get(url)
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
            _ => match self.queue_cache_entry(&url, container) {
                Some(item) if item.complete => {
                    debug!(
                        url,
                        len = item.bytes.len(),
                        "Serving the load from the queue prefetch cache"
                    );
                    media_source::build_bytes_source(item.bytes)
                }
                Some(item) => {
                    debug!(
                        url,
                        len = item.bytes.len(),
                        total = item.total,
                        "Starting the load from a prefetched head"
                    );
                    media_source::build_uri_source_with_head(
                        &url,
                        headers,
                        Some(media_source::PreloadedHead {
                            bytes: item.bytes,
                            total: item.total,
                        }),
                    )
                }
                None => media_source::build_uri_source(&url, headers),
            },
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
        // Taken up front, unconditionally: this function has several early
        // exits (image containers, RAOP, malformed bodies) and an override
        // surviving one of them would silently relocate a LATER load.
        let start_override = self.load_start_override.take();
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
        // An override stands for an operation this load is replacing, so it
        // wins over the item's own start point (`gapless_eligible` refuses to
        // pre-arm an item that has either, so the two never really compete).
        let start_position = match start_override {
            Some(start) => start.position,
            None => item
                .time
                .and_then(|s| gst::ClockTime::try_from_seconds_f64(s).ok())
                .unwrap_or(gst::ClockTime::ZERO),
        };
        let playback_rate = match start_override {
            Some(start) => start.rate,
            None => item.speed.unwrap_or(1.0) as f32,
        };
        let headers = item.headers;

        self.have_audio_track_cover = false;
        let mut is_for_sure_live = false;
        if container == "application/x-whep" || container == "application/x-fwebrtc" {
            // The source is built directly with the real URL / typed channel
            // (`build_media_source`), no fake-URI dispatch.
            is_for_sure_live = true;
        }

        // Image containers decoded by fimagedec inside the normal player
        // pipeline (animations loop forever and never post EOS, stills hold
        // their frame). JPEG rides this path too, via the private
        // image/x-fcast-jpeg caps so it never disturbs MJPEG video (see
        // `imagedec::player_mime_types`). Only an unrecognized image mime,
        // which fimagedec cannot decode, stays on the legacy in-GUI path.
        let pipeline_image = crate::imagedec::player_mime_types().contains(&container.as_str());

        let player_variant = if container.starts_with("image/") {
            UiPlayerVariant::Image
        } else if container.starts_with("audio/")
            // Video streams are audio only until proven otherwise
            || container.starts_with("video/")
            || container == "application/x-whep"
            || container == "application/dash+xml"
            || container == "application/vnd.apple.mpegurl"
            || container == "application/x-fwebrtc"
            || container == "application/x-sabr-ump"
        {
            UiPlayerVariant::Audio
        } else {
            UiPlayerVariant::Unknown
        };

        match player_variant {
            // Legacy still images keep the previous frame up (ContinueToPlay::Yes)
            // while the next one downloads. A pipeline image reloads the player
            // like any other media, so it tears down the previous playback.
            UiPlayerVariant::Image if !pipeline_image => {
                self.cleanup_playback_data(ContinueToPlay::Yes, PreservePlaylist::Yes)
            }
            UiPlayerVariant::Image
            | UiPlayerVariant::Unknown
            | UiPlayerVariant::Audio
            | UiPlayerVariant::Video => {
                self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::Yes)
            }
            UiPlayerVariant::Raop => (),
        }

        self.window_visible_before_playing = Some(self.gui.set_window_visibility(true));
        #[cfg(not(target_os = "android"))]
        if !self.settings.no_fullscreen_player() {
            // If the window was hidden, it takes some time before it can be fullscreened.
            self.gui.wait_for_is_visible();
            self.window_fullscreen_before_playing = Some(self.gui.set_fullscreen(true));
        }

        let mut media_title = None;
        if !self.settings.headless()
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

        // A pipeline image follows the media load branch below (it is decoded
        // by fimagedec in the player), so track it so progress traffic is
        // suppressed and the image view is painted transparent.
        self.image_via_player = pipeline_image;
        self.gui.set_image_via_player(pipeline_image);

        let mut is_image = false;
        if container.starts_with("image/") && !pipeline_image {
            is_image = true;
            if let Some(item) = self
                .queue_cache_entry(&url, &container)
                .filter(|item| item.complete)
            {
                // A prefetched queue photo: decode straight from the cached
                // bytes instead of re-downloading (mirrors the DownloadResult
                // success arm of handle_image_event). Only complete entries
                // decode, a partial head is not a decodable image.
                debug!(
                    url,
                    len = item.bytes.len(),
                    "Decoding image from the queue prefetch cache"
                );
                self.current_image_id += 1;
                let id = self.current_image_id;
                self.image_decoder.queue_job(
                    id,
                    image::ImageDecodeJob::new_no_format(
                        item.bytes,
                        image::ImageDecodeJobType::Regular,
                    ),
                );
            } else {
                self.current_image_download_id += 1;
                let id = self.current_image_download_id;
                self.image_downloader
                    .queue_download(id, url.clone(), headers.clone());
            }
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
        // (`Player::log_load_stall_diagnostics`). Legacy images bypass the
        // pipeline (pipeline images go through it and are covered).
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

        // A pipeline image exposes a raw video stream (fimagedec output), but
        // the UI stays on the Image variant so the image view is shown.
        if self.image_via_player {
            return Ok(());
        }

        debug!("Video stream available");

        self.gui.set_player_type(UiPlayerVariant::Video);

        Ok(())
    }

    fn video_stream_unavailable(&self) {
        if !self.is_playing() {
            debug!("Ignoring old video stream unavailable event");
            return;
        };

        if self.image_via_player {
            return;
        }

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
            self.queue_cache.clear();
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

        // An explicit selection replaces any pending gapless pre-arm.
        self.cancel_gapless_prearm();

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

    /// The spec'd Queue.autoplay behavior: when the current item of an
    /// autoplay queue finishes, the receiver advances to the next item by
    /// itself instead of waiting for a sender's QueueItemSelected round-trip
    /// (with the prefetched head already resident, the next item starts
    /// immediately). Senders are told through a broadcast QueueItemSelected
    /// so their UIs follow. Runs from the EOS handler, after the Ended
    /// broadcast.
    /// The index the receiver should advance to by itself: only meaningful
    /// for an autoplay queue whose current item has a successor. Also gates
    /// the `media_ended` teardown, so an unattended autoplay queue is not
    /// wiped between items.
    fn autoplay_next_index(&self) -> Option<usize> {
        let media = self.current_media.as_ref()?;
        let MediaSource::Queue(queue) = &media.source else {
            return None;
        };
        if !queue.autoplay {
            return None;
        }
        let next = queue.current_idx as usize + 1;
        (next < queue.items.len()).then_some(next)
    }

    fn maybe_autoplay_advance(&mut self) {
        let Some(origin) = self.current_media.as_ref().map(|m| m.origin) else {
            return;
        };
        let Some(next) = self.autoplay_next_index() else {
            return;
        };

        info!(next, "Autoplay: advancing to the next queue item");
        self.play_queue_item(origin, v4::QueuePosition::Index(next as u8), false);

        // Receiver-initiated: every sender hears about the selection.
        if self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(fcast::V4Message::Broadcast {
                serialized_msg: fcast_protocol::v4::MessageBuilder::new()
                    .queue_select(v4::QueuePosition::Index(next as u8)),
            }));
        }
    }

    /// Time before the current item's end at which the next autoplay item
    /// is pre-armed on the live pipeline. The bound that matters: the
    /// pipeline's audio queue holds up to 30s of DECODED audio, so an audio
    /// stream's end-of-stream passes decodebin3's outputs ~30s before the
    /// item audibly ends, and the pre-arm must beat it there or the handoff
    /// is missed (for an audio-only item the escaped EOS ends the pipeline
    /// between the items, every time). Also comfortably past the next
    /// item's parse time, and early enough that short clips pre-arm on
    /// their first progress tick.
    const GAPLESS_PREARM_MARGIN: gst::ClockTime = gst::ClockTime::from_seconds(40);

    /// Whether a queue item can be the target of a gapless pre-arm: plain
    /// progressive A/V only. Per-item start/speed/volume overrides need a
    /// real load (they apply in PAUSED), images never post EOS (fimagedec
    /// parks stills and loops animations), adaptive and live containers
    /// cannot ride a prepared input.
    fn gapless_eligible(item: &QueueItem) -> bool {
        item.time.is_none()
            && item.speed.is_none()
            && item.volume.is_none()
            && !item.content_type.starts_with("image/")
            && queue_cache::cacheable_container(&item.content_type)
    }

    /// Pre-arm the next autoplay queue item near the current item's end
    /// (runs from the progress tick): build its source (cache-aware, same
    /// path a load takes) and hand it to the player, which links it into
    /// the live pipeline and switches at the drain. The advance itself is
    /// handled by the GaplessActivated event.
    fn maybe_prearm_gapless(&mut self) {
        if !self.gapless_enabled || self.gapless_prearm.is_some() {
            return;
        }
        if self.gapless_blocked_item == Some(self.current_media_item_id) {
            return;
        }
        if self.image_via_player || self.is_loading_media || !self.have_media_info {
            return;
        }
        // External subtitles are side inputs on the live core; a swap would
        // carry them into the next item's collections (fcastplaybin refuses
        // such a prepare too). Those items advance through the ordinary
        // end-of-stream load.
        if self
            .current_media
            .as_ref()
            .is_some_and(|m| !m.external_subtitles.is_empty())
        {
            return;
        }
        let Some(next) = self.autoplay_next_index() else {
            return;
        };
        let (current_show_duration, next_item) = {
            let Some(MediaSource::Queue(queue)) = self.current_media.as_ref().map(|m| &m.source)
            else {
                return;
            };
            let Some(current) = queue.items.get(queue.current_idx as usize) else {
                return;
            };
            let Some(next_item) = queue.items.get(next) else {
                return;
            };
            (current.show_duration, next_item.clone())
        };
        // A playback_duration item advances through its timer with a normal
        // load cutting the media mid-stream; a pre-arm would fight it.
        if current_show_duration.is_some() {
            return;
        }
        if !Self::gapless_eligible(&next_item) {
            return;
        }
        // Only near the end of a finite item.
        let Some(position) = self.player.get_position() else {
            return;
        };
        let Some(duration) = self.current_duration.filter(|d| !d.is_zero()) else {
            return;
        };
        if position + Self::GAPLESS_PREARM_MARGIN < duration {
            return;
        }

        info!(next, "Gapless: pre-arming the next queue item");
        let input = self.build_gapless_source(
            &next_item.content_type,
            next_item.url.clone(),
            next_item.headers.clone(),
        );
        let generation = self.player.prepare_next(input);
        self.gapless_prearm = Some(GaplessPrearm {
            generation,
            next_index: next,
            url: next_item.url,
            cancelling: false,
        });
    }

    /// Like [`build_media_source`](Self::build_media_source) but for a
    /// gapless pre-arm: a fully cached item still goes through urisourcebin
    /// with its bytes injected as a preloaded head covering the WHOLE
    /// resource, instead of the bare appsrc bytes source. A prepared
    /// input's pads sit unlinked-and-blocked until the swap, the topology
    /// uridecodebin3's own gapless uses, which the urisourcebin path
    /// handles and the appsrc bytes source does not (its chain dies
    /// not-negotiated against the blocked pads). The head covers the full
    /// resource, so playback still never touches the network.
    fn build_gapless_source(
        &mut self,
        container: &str,
        url: String,
        headers: Option<HashMap<String, String>>,
    ) -> player::MediaInput {
        let head =
            self.queue_cache_entry(&url, container)
                .map(|item| media_source::PreloadedHead {
                    bytes: item.bytes,
                    total: item.total,
                });
        match media_source::build_uri_source_with_head(&url, headers, head) {
            Ok(element) => player::MediaInput::Element(element),
            Err(err) => {
                error!(
                    ?err,
                    container, "Failed to build the gapless source element"
                );
                player::MediaInput::Uri(url)
            }
        }
    }

    /// Invalidate a pending gapless pre-arm (seek, speed change, queue
    /// mutation, anything that breaks "the current item plays to its end and
    /// the next one follows"). A no-op when nothing is pre-armed.
    ///
    /// The bookkeeping is only marked here, not dropped: the cancel races
    /// the pipeline's swap and commonly loses (the swap performs at pre-arm
    /// time for a small or cached item), and a declined cancel activates
    /// regardless. Dropping the pre-arm up front left that activation
    /// unmatched, and the resync branch then reloaded (audibly replayed) the
    /// track that had just finished. The pre-arm now clears on the outcome:
    /// `GaplessCancelled`, an adoption, `GaplessPrepareFailed`, or the next
    /// load's `cleanup_playback_data`.
    fn cancel_gapless_prearm(&mut self) {
        let Some(prearm) = self.gapless_prearm.as_mut() else {
            return;
        };
        // A second cancel has nothing left to ask for: the first one's
        // outcome decides for both.
        if prearm.cancelling {
            return;
        }
        prearm.cancelling = true;
        let generation = prearm.generation;
        debug!(generation, "Cancelling the gapless pre-arm");
        self.player.cancel_prepared();
    }

    /// Run an operation that reaches the pipeline as a flushing seek, or park
    /// it until a pending pre-arm's cancellation reports its outcome.
    ///
    /// Contract invariant 8 (`fcastplaybin/CLEANUP.md`): a pending prepare must
    /// be cancelled AND the cancellation confirmed before a flushing seek
    /// reaches the pipeline. Once the swap has performed, the prepared input is
    /// the only linked upstream, so a `FLUSH|ACCURATE` seek flushes away the
    /// playing item's buffered tail and is answered by the SUCCESSOR's source:
    /// the user seeks inside track N and hears track N+1. Requesting the cancel
    /// and forwarding the operation in the same breath (what this used to do)
    /// loses that race whenever the swap performed early, which is the normal
    /// case for a small or cached item.
    fn park_or_apply_gapless_op(&mut self, op: GaplessParkedOp) {
        if self.gapless_prearm.is_none() {
            self.apply_gapless_op(op);
            return;
        }
        // Latest intent wins, across kinds as well as within one: a seek
        // arriving while a track change waits replaces it, the same way the
        // pipeline's own seek parking is latest-wins. One slot, never a queue,
        // so a burst of scrubbing cannot pile up work for the outcome.
        let kind = op.kind();
        if let Some(previous) = self.gapless_parked_op.replace(op) {
            debug!(
                replaced = ?previous.kind(),
                ?kind,
                "Replacing the operation parked on the gapless cancel outcome"
            );
        } else {
            debug!(?kind, "Parking the operation until the gapless cancel resolves");
        }
        // Idempotent when a cancel is already in flight (e.g. one a queue
        // mutation asked for): that cancel's outcome resolves this too.
        self.cancel_gapless_prearm();
    }

    /// The apply half of [`park_or_apply_gapless_op`](Self::park_or_apply_gapless_op),
    /// shared by the immediate path and by a replay after a confirmed
    /// cancellation so the two cannot drift.
    fn apply_gapless_op(&mut self, op: GaplessParkedOp) {
        match op {
            GaplessParkedOp::Seek { origin, time } => self.apply_seek(origin, time),
            // Confirmed by the pipeline's own `RateChanged`, like any other
            // real speed change.
            GaplessParkedOp::SetSpeed { rate, .. } => self.player.set_rate(rate),
            GaplessParkedOp::TrackChange { kind, sid } => self.apply_track_change(kind, sid),
            GaplessParkedOp::SubtitleChange { origin, target } => {
                self.apply_subtitle_target(origin, target)
            }
        }
    }

    /// Report and clamp a seek target against the known duration. Done before
    /// the gapless park so the sender's error reply is never delayed by a
    /// cancel round-trip.
    ///
    /// `current_duration` can be stale in exactly this window: it is a
    /// pipeline query, and once the swap has performed the PREPARED input
    /// answers it, so a late seek can be clamped against the next item's
    /// duration. Accepted, and not made worse by parking: the check reads the
    /// same field the old inline one did. CLEANUP.md's ranked defect 3 owns
    /// that readout.
    fn clamp_seek_target(&mut self, origin: PacketOrigin, time: gst::ClockTime) -> gst::ClockTime {
        match self.current_duration {
            Some(duration) if duration > gst::ClockTime::ZERO && time > duration => {
                self.send_error(origin, ErrorKind::SeekOutOfRange);
                duration
            }
            _ => time,
        }
    }

    /// Send an already-clamped seek to the pipeline, or park it until the
    /// seekability query resolves (see [`Self::pending_seek_op`]).
    fn apply_seek(&mut self, origin: PacketOrigin, time: gst::ClockTime) {
        if self.player.seekable_known {
            self.player.seek(time);
            return;
        }
        // Tracks are advertised well before the pipeline can answer the
        // seekability query, and the player would silently drop the seek in
        // that window. Park it (last seek wins) and apply it once the query
        // resolves.
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
    }

    /// Resolve the operation parked on a gapless cancel, now that `outcome` is
    /// known. Returns whether playback was replaced, in which case the caller
    /// must not go on to adopt an activation (the reload superseded it).
    fn resolve_parked_gapless_op(&mut self, outcome: GaplessOutcome) -> bool {
        let Some(op) = self.gapless_parked_op.take() else {
            return false;
        };
        let action = parked_op_action(op.kind(), outcome);
        debug!(kind = ?op.kind(), ?outcome, ?action, "Resolving the parked operation");
        match (action, op) {
            (ParkedOpAction::Replay, op) => {
                self.apply_gapless_op(op);
                false
            }
            (ParkedOpAction::ReloadAtTarget, GaplessParkedOp::Seek { origin, time }) => {
                info!(
                    ?origin,
                    ?time,
                    "Gapless: the swap already performed, reloading the item at the seek target"
                );
                // Rate carries over: a reload resets the pipeline's rate, and
                // the user did not ask to change speed.
                let rate = self.player.rate() as f32;
                self.reload_current_item_at(time, rate);
                true
            }
            (ParkedOpAction::ReloadAtTarget, GaplessParkedOp::SetSpeed { origin, rate }) => {
                // Best effort: the pipeline's position is the outgoing item's
                // (its tail is still what the sink renders), so this resumes
                // roughly where the speed change was asked for.
                let position = self.player.get_position().unwrap_or(gst::ClockTime::ZERO);
                info!(
                    ?origin,
                    rate,
                    ?position,
                    "Gapless: the swap already performed, reloading the item at the new speed"
                );
                self.reload_current_item_at(position, rate);
                // A real speed change is normally confirmed by the pipeline's
                // `RateChanged`, but the load applies the rate as its start
                // seek (`apply_start_seek`), which emits none. Confirm from
                // here so the sender is not left waiting for an ack that will
                // never come.
                if let Err(err) = self.broadcast_rate(rate) {
                    warn!(?err, "Failed to relay the rate after a gapless reload");
                }
                true
            }
            // `parked_op_action` never pairs `ReloadAtTarget` with a track
            // change (there is no position to reload at), so folding the two
            // together keeps this total without a panicking arm.
            (ParkedOpAction::Drop | ParkedOpAction::ReloadAtTarget, op) => {
                info!(
                    kind = ?op.kind(),
                    ?outcome,
                    "Gapless: dropping the parked operation"
                );
                false
            }
        }
    }

    /// Reload the item the application considers current, starting at
    /// `position` and `rate`. Stands in for an operation the pipeline can no
    /// longer serve because the gapless swap already performed and unlinked
    /// this item's input.
    fn reload_current_item_at(&mut self, position: gst::ClockTime, rate: f32) {
        // The load supersedes the in-flight activation (fcastplaybin's jobs
        // are latest-wins and its load resets the prepared input), so clear
        // the pre-arm bookkeeping first, exactly like the unmatched-activation
        // resync branch. The queue index is left alone: the advance only
        // happens on adoption, so "current" is still the playing item.
        self.gapless_prearm = None;
        self.player.clear_pending_gapless();
        // `cleanup_playback_data` drops the optimistic GUI seek hold. Carry it
        // across: this load lands exactly on the hold's target, so the thumb
        // should stay pinned until then instead of springing back for a tick.
        // Taking it here also keeps the cleanup from clearing the GUI's own
        // `seek-pending` flag.
        let seek_hold = self.gui_seek_hold.take();
        // The start point rides the load (applied in PAUSED inside
        // fcastplaybin) rather than being seeked in afterwards: that is the
        // same mechanism a per-item `time`/`speed` uses, and it avoids
        // rendering a 1.0x slice that a later seek would flush (the pop).
        self.load_start_override = Some(player::RestorePoint { position, rate });
        self.load_media();
        self.gui_seek_hold = seek_hold;
        // `Player::load` resets the tracked rate to 1.0 and the crate's start
        // seek emits no `RateChanged`, so restate it or the application would
        // compare later speed requests against the wrong value.
        self.player.set_rate_changed(rate as f64);
    }

    /// Where the pre-armed item sits in the queue now. Only needed when a
    /// declined cancel's activation has to be adopted: the queue mutation
    /// that triggered the cancel may have shifted the item (insert/remove) or
    /// dropped it entirely, so the armed index is a hint, the URL is the
    /// identity.
    fn prepared_queue_index(&self, armed_index: usize, url: &str) -> Option<usize> {
        let Some(MediaSource::Queue(queue)) = self.current_media.as_ref().map(|m| &m.source) else {
            return None;
        };
        if queue
            .items
            .get(armed_index)
            .is_some_and(|item| item.url == url)
        {
            return Some(armed_index);
        }
        queue.items.iter().position(|item| item.url == url)
    }

    /// Roll the application onto a gapless activation: `generation` is live in
    /// the pipeline and `next_index` is the queue item it plays. Everything
    /// `play_queue_item` does except the load, since the pipeline already
    /// switched.
    ///
    /// The index is a parameter instead of being read from the pre-arm because
    /// a declined cancel's activation can arrive after the queue moved (see
    /// the `GaplessActivated` handler).
    fn adopt_gapless_activation(&mut self, generation: u64, next_index: usize) {
        // Consumed either way: an activation the player refuses never comes
        // back, and keeping the pre-arm would only block gapless for the rest
        // of the item.
        self.gapless_prearm = None;
        // An adoption is an item boundary, and a parked operation belongs to
        // the item that just retired. Callers resolve it before getting here,
        // so this only ever catches bookkeeping drift, but applying an old
        // item's operation to the new one is the exact class of bug the reset
        // table in CLEANUP.md is about.
        if let Some(op) = self.gapless_parked_op.take() {
            debug!(
                kind = ?op.kind(),
                "Dropping an operation still parked at a gapless boundary"
            );
        }
        if !self.player.adopt_gapless_generation(generation) {
            warn!(generation, "Ignoring a stale gapless activation");
            return;
        }

        info!(
            index = next_index,
            "Gapless: the next queue item is playing"
        );

        // The queue advances exactly like play_queue_item, minus the load
        // (the pipeline already switched).
        if let Some(media) = self.current_media.as_mut() {
            media.clear_external_subtitles();
        }
        let (title, thumbnail_url, headers) = match self.queue_mut() {
            Some(queue) => {
                queue.current_idx = next_index as u8;
                match queue.items.get(next_index) {
                    Some(item) => (
                        item.title.clone(),
                        item.thumbnail_url.clone(),
                        item.headers.clone(),
                    ),
                    None => (None, None, None),
                }
            }
            None => (None, None, None),
        };

        // Per-item view state rolls like a fresh load. The new item's
        // collection follows this event and re-runs
        // media_loaded_successfully through the usual have_media_info gate
        // (arming its playback_duration timer among other things).
        self.current_media_item_id += 1;
        self.have_media_info = false;
        self.current_duration = None;
        self.inspector_container = None;
        self.inspector_image = String::new();
        self.have_media_title = title.is_some();
        if let Some(title) = title {
            self.gui.set_media_title(title);
        }

        // The gapless path bypasses the normal load, so refresh the audio
        // cover ourselves. Otherwise the previous track's thumbnail
        // lingers: the metadata thumbnail is never fetched, and
        // `have_audio_track_cover` staying set makes the Tags handler
        // ignore an embedded image tag too.
        self.have_audio_track_cover = false;
        if let Some(media) = self.current_media.as_mut() {
            media.pending_thumbnail = None;
            media.pending_thumbnail_download = None;
        }
        if !self.settings.headless()
            && let Some(thumbnail_url) = thumbnail_url
        {
            self.have_audio_track_cover = true;
            self.current_image_download_id += 1;
            let this_id = self.current_image_download_id;
            if let Some(media) = self.current_media.as_mut() {
                media.pending_thumbnail_download = Some(this_id);
            }
            self.image_downloader
                .queue_download(this_id, thumbnail_url, headers);
        } else {
            // No metadata thumbnail for the new item: drop the old cover
            // so it doesn't linger (an embedded image tag, if any, is
            // still picked up by the Tags handler now that the flag and
            // pending state are reset).
            self.gui.clear_audio_covers();
        }

        // Receiver-initiated selection: every sender hears about it
        // (the same broadcast the EOS advance sends).
        if self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(fcast::V4Message::Broadcast {
                serialized_msg: fcast_protocol::v4::MessageBuilder::new()
                    .queue_select(v4::QueuePosition::Index(next_index as u8)),
            }));
        }
        self.sync_queue_cache();
    }

    /// Reconcile the prefetch cache with the current queue window. Runs after
    /// every queue mutation (select, insert, remove, initial queue load).
    fn sync_queue_cache(&mut self) {
        let (desired, retain) = match self.current_media.as_ref().map(|m| &m.source) {
            Some(MediaSource::Queue(queue)) => {
                let desired =
                    queue_cache::window_indices(queue.items.len(), queue.current_idx as usize)
                        .into_iter()
                        .filter_map(|idx| queue.items.get(idx))
                        // Adaptive manifests (HLS, DASH) must stream live and a
                        // cached head would be useless anyway: never prefetch them.
                        .filter(|item| queue_cache::cacheable_container(&item.content_type))
                        .map(|item| queue_cache::PrefetchSpec {
                            url: item.url.clone(),
                            headers: item.headers.clone(),
                        })
                        .collect();
                // The current item is retained but never fetched: its bytes
                // are already playing, but flipping back to a neighbor and
                // returning must not re-download it.
                let retain = queue
                    .items
                    .get(queue.current_idx as usize)
                    .map(|item| item.url.clone())
                    .into_iter()
                    .collect::<Vec<_>>();
                (desired, retain)
            }
            _ => (Vec::new(), Vec::new()),
        };
        let prefetcher = &self.queue_prefetcher;
        self.queue_cache.sync(desired, &retain, |spec, epoch| {
            prefetcher.fetch(spec, epoch)
        });
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

        // Indices shifted (and the pre-armed item itself may be gone).
        self.cancel_gapless_prearm();

        self.relay_to_other_senders(
            origin,
            fcast_protocol::v4::MessageBuilder::new().queue_remove(position),
        );

        self.sync_queue_cache();
    }

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

        // Indices shifted; the pre-armed item may no longer be the next.
        self.cancel_gapless_prearm();

        if let Some(relay_msg) =
            fcast_protocol::v4::MessageBuilder::new().from_queue_insert_stripped(insert)
        {
            self.relay_to_other_senders(origin, relay_msg);
        }

        self.sync_queue_cache();
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
                        // The spec caps a queue at 2^8 items (every queue
                        // position on the wire is a ubyte). Reject oversized
                        // queues outright: indices past 255 are unaddressable
                        // and the u8 bookkeeping (e.g. the autoplay advance)
                        // would wrap back to item 0.
                        if items.len() > u8::MAX as usize + 1 {
                            error!(len = items.len(), "Queue exceeds the spec's 256 item cap");
                            self.send_error(origin, ErrorKind::MalformedBody);
                            return;
                        }
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
                                autoplay: queue.autoplay(),
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
                    // Range-check first so a park below cannot delay the
                    // sender's error reply. This used to run only on the
                    // seekable-known branch, with `maybe_apply_pending_seek`
                    // repeating it for a seek parked before the query
                    // resolved; the check is idempotent (it clamps to the
                    // duration, so the second pass finds nothing to report)
                    // and running it once, up front, is the same answer unless
                    // the item's duration GROWS between PAUSED and PLAYING,
                    // which would now clamp where it previously would not.
                    let time = self.clamp_seek_target(origin, time);
                    // Seeking away from the end invalidates "plays to its end,
                    // next item follows", and a flushing seek must not reach
                    // the pipeline before the cancellation is confirmed.
                    self.park_or_apply_gapless_op(GaplessParkedOp::Seek { origin, time });
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
                    // A real rate change IS a flushing seek: same hazard and
                    // same park as `Seek` above.
                    self.park_or_apply_gapless_op(GaplessParkedOp::SetSpeed { origin, rate });
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
                // An audio switch is a flushing seek in the selection engine
                // (`Job::RefreshSeek`), so it carries the same post-swap hazard
                // as `Seek`: park it until a pending pre-arm's cancellation is
                // confirmed. Video and subtitle switches ride along because a
                // selection resolved against the retired item's stream list is
                // wrong for the successor either way.
                self.park_or_apply_gapless_op(GaplessParkedOp::TrackChange { kind, sid });
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

        self.update_connection_details()
    }

    /// Rebuild the idle-screen QR code / IP list from the current addresses and
    /// the actually-bound FCast port. Called on every mDNS update and after a
    /// port relocation.
    fn update_connection_details(&mut self) -> Result<()> {
        if !self.port_committed {
            // Not listening yet (e.g. resolving a port conflict), so don't
            // advertise a QR for a port we haven't bound.
            return Ok(());
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
                    port: self.fcast_port,
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
        // An `AddSubtitleSource` may be parked waiting for the load to
        // complete, or for the seekability query this update may have just
        // resolved.
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

    /// How long a parked `AddSubtitleSource` may wait before it is rejected
    /// with `InvalidState`. This bounds the combined wait: the in-flight load
    /// completing, then the seekability query resolving. A slow preroll under
    /// load contention can take well over 10 seconds on its own, so the two
    /// waits back to back need the headroom.
    const PENDING_SUBTITLE_ADD_TIMEOUT: Duration = Duration::from_secs(20);

    /// Handle `AddSubtitleSource`. The op is parked and replayed instead of
    /// being spuriously rejected in two windows:
    ///
    /// - The load it targets is still in flight. A sender may send `Load` and
    ///   `AddSubtitleSource` back to back, and everything the preconditions
    ///   below ask about (liveness, seekability) only becomes answerable once
    ///   the pipeline has something to answer with.
    /// - The media is loaded but the pipeline hasn't answered the seekability
    ///   query yet. Tracks are advertised off the first stream collection,
    ///   well before the query can succeed at preroll completion, seconds
    ///   apart on a slow preroll.
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
        // live/unseekable stream, and acting on an in-flight load would
        // race it. Only an incompatible source is a genuine rejection
        // here: liveness and seekability are answerable once the load has
        // settled, so an op that arrives before then is parked.
        let src_supported = match self.current_media.as_ref().map(|m| &m.source) {
            Some(MediaSource::Single(_) | MediaSource::Playlist { .. } | MediaSource::Queue(_)) => {
                true
            }
            Some(MediaSource::Raop | MediaSource::AirPlayMirror { .. }) | None => false,
        };
        if !src_supported {
            error!("Cannot add a subtitle source: no compatible media is loaded");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }
        if self.is_loading_media {
            // The media the op targets is the one currently loading, so it is
            // not a mistake by the sender, just early. This whole window
            // (between the `Load` op and its first stream collection) used to
            // be a hard `InvalidState`, which forced senders to guess when the
            // receiver was ready. Park instead and let the load's completion
            // replay it.
            //
            // Parking happens BEFORE the liveness and seekability checks and
            // before `cancel_gapless_prearm` on purpose: mid-load neither
            // property is known yet, and a fresh load has no pre-arm to
            // cancel. All of that is evaluated on the replay, where the
            // answers mean something.
            debug!("Parking the subtitle source until the in-flight load completes");
            self.park_pending_subtitle_add(url, select, name, origin);
            return Ok(false);
        }
        if self.player.is_live() {
            error!("Cannot add a subtitle source to a live stream");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }
        // A gapless swap does not carry external subtitles across items (it
        // would leak this item's sub into the next item's collections), so
        // an external subtitle on the current item makes it ineligible. A
        // pre-arm already in flight when the subtitle is added must be
        // dropped; the item then advances through the ordinary
        // end-of-stream load.
        self.cancel_gapless_prearm();
        if !self.player.seekable {
            if !self.player.seekable_known {
                // Not unseekable, just not answerable yet. Park the op.
                // `on_media_info_updated` replays it once the query
                // resolves, and the check timer bounds the wait.
                debug!("Parking the subtitle source until the seekability query resolves");
                self.park_pending_subtitle_add(url, select, name, origin);
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
        // fcastplaybin babysits the input itself (materialization watchdog,
        // deselect-race re-arm); a genuine failure arrives as
        // `PlayerEvent::ExternalSubtitleFailed`.
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
            handle,
            stream_sid: None,
        });
        if select {
            // The engine parks the desire until the input's stream
            // materializes, then selects it and re-asserts it against
            // decodebin3's collection-default auto-select.
            self.player.request_external_subtitle(handle);
        } else {
            // Pin what is showing NOW as the explicit desire (the old
            // Restore enforcement, declaratively): decodebin3 may
            // auto-select the fresh text stream for the new collection,
            // and a never-requested (unset) desire would simply adopt
            // that, showing a subtitle nobody asked for.
            let current = self.player.current_subtitle_sid().map(str::to_string);
            self.apply_track_change(player::TrackKind::Subtitle, current);
        }

        debug!(id, select, "Attached external subtitle input (live)");
        self.update_tracks(true);
        Ok(false)
    }

    /// Park an `AddSubtitleSource` for a later replay and arm the timer that
    /// bounds the wait. Shared by both park sites (in-flight load, unresolved
    /// seekability). The timer is stamped with the current epoch and media
    /// item, so if the list is drained (applied or rejected) before it fires,
    /// the check is a no-op.
    fn park_pending_subtitle_add(
        &mut self,
        url: String,
        select: bool,
        name: Option<SmolStr>,
        origin: PacketOrigin,
    ) {
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
    }

    /// Replay `AddSubtitleSource` ops parked while the load was in flight or
    /// the seekability query was unresolved. No-op until both have settled,
    /// called whenever media info updates.
    fn maybe_apply_pending_subtitle_adds(&mut self) {
        // `is_loading_media` matters here because the first stream collection
        // calls `on_media_info_updated` before `media_loaded_successfully`
        // clears the flag: replaying at that point would re-enter
        // `add_subtitle_source` mid-load and just park again. The
        // StreamCollection arm calls back here once the flag is clear.
        if self.pending_subtitle_adds.is_empty()
            || self.is_loading_media
            || !self.player.seekable_known
        {
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
    ///
    /// Validation happens here; enacting the result goes through the gapless
    /// gate, so it can be held back until a pending pre-arm's cancellation is
    /// confirmed (see [`park_or_apply_gapless_op`](Self::park_or_apply_gapless_op)).
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

        // `target` is fully validated from here on, so it is safe to hold
        // across a gapless cancel round-trip if one is in flight.
        self.park_or_apply_gapless_op(GaplessParkedOp::SubtitleChange { origin, target });
    }

    /// Enact a validated subtitle target. Split out of
    /// [`change_subtitle_track`](Self::change_subtitle_track) so a replay after
    /// a gapless cancel runs the identical code the original request would.
    fn apply_subtitle_target(&mut self, origin: PacketOrigin, target: SubtitleTarget) {
        match target {
            SubtitleTarget::External(ext_id) => {
                // Attached but its stream hasn't materialized yet: the
                // engine parks the desire and applies it when the stream
                // appears; the eventual selection confirm relays the
                // TracksSelected the sender is waiting for. Latest-wins
                // composition in the engine replaces the old parked-desire
                // supersede bookkeeping.
                let handle = self
                    .current_media
                    .as_ref()
                    .and_then(|m| m.external_subtitles.iter().find(|s| s.id == ext_id))
                    .map(|s| s.handle);
                match handle {
                    Some(handle) => {
                        debug!(ext_id, "Requesting the external subtitle from the engine");
                        self.player.request_external_subtitle(handle);
                    }
                    // resolve_subtitle_target just found it, so only a
                    // racing removal gets here.
                    None => self.send_error(origin, ErrorKind::MalformedBody),
                }
            }
            SubtitleTarget::Stream(stream_sid) => {
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

    /// Apply a track change through TrackOps. Whether the switch's re-emit
    /// flush is safe (it races an attached external input's reconfiguration
    /// and can freeze the item) is decided inside the player's pump, off the
    /// pipeline's own input state, not from the catalog here.
    fn apply_track_change(&mut self, kind: player::TrackKind, sid: Option<player::StreamId>) {
        if self.player.request_track_change(kind, sid) {
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
                && let Some(sid) = self.player.external_stream_sid_of(entry.handle)
            {
                debug!(id = entry.id, sid, "external subtitle stream materialized");
                entry.stream_sid = Some(sid);
            }
        }
    }

    /// An attached external subtitle failed for good: fcastplaybin reported
    /// `ExternalSubtitleFailed` with the input ALREADY detached (it owns the
    /// materialization watchdog, the deselect-race re-arm, and dropping any
    /// selection desire parked on the input). Drop the catalog entry and
    /// tell the requester `ResourceNotFound`; the input is independent of
    /// the main item, so playback continues untouched.
    fn fail_fcast_external_subtitle(&mut self, ext_id: u32) {
        let Some(media) = self.current_media.as_mut() else {
            return;
        };
        let Some(pos) = media.external_subtitles.iter().position(|s| s.id == ext_id) else {
            return;
        };
        let failed = media.external_subtitles.remove(pos);
        warn!(url = failed.url, "External subtitle failed; removing it");
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
                // Stamped with the PREPARED (future) generation by design;
                // its handler validates it against the pre-arm bookkeeping.
                | player::PlayerEvent::GaplessActivated
                // Cancel outcomes are control-plane too: they carry the
                // prepared generation and are validated against the pre-arm
                // bookkeeping, not against the current load.
                | player::PlayerEvent::GaplessCancelled { .. }
                | player::PlayerEvent::GaplessCancelDeclined { .. }
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
                // The item ended before the cancel outcome arrived, so an
                // operation parked on it has no item left to act on: the
                // advance below owns the transition. Resolved before the
                // cancel so the parked operation cannot survive into the next
                // item on this path.
                self.resolve_parked_gapless_op(GaplessOutcome::ItemEnded);
                // A pipeline EOS while pre-armed means the gapless handoff
                // missed (or was cancelled after the drain): the ordinary
                // advance below owns the transition, drop the pre-arm.
                self.cancel_gapless_prearm();

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

                self.maybe_autoplay_advance();
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

                if !self.settings.headless()
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

                if let Some(artist) = tags.get::<gst::tags::Artist>()
                    && self.last_artist_name.as_deref() != Some(artist.get())
                {
                    let name = artist.get().to_owned();
                    self.gui.set_artist_name(name.clone());
                    self.last_artist_name = Some(name);
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

                // Learn stream ids for externals that just materialized and
                // advertise them. Selection enforcement is the engine's job
                // now: `Player::handle_stream_collection` pumped it above,
                // and a desire parked on a just-materialized external
                // resolved there.
                self.refresh_external_stream_sids();

                self.update_tracks(true);

                if !self.have_media_info {
                    self.media_loaded_successfully();
                    self.have_media_info = true;
                }

                // A parked `AddSubtitleSource` may have been waiting on the
                // load itself, and this collection's `on_media_info_updated`
                // above ran while `is_loading_media` was still set. Retry the
                // release now that it is clear. In the common order
                // (collection first, seekability at preroll completion) the
                // later `on_media_info_updated` does the release, this covers
                // the reverse order where seekability resolved first.
                self.maybe_apply_pending_subtitle_adds();
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
            player::PlayerEvent::DurationChanged => {
                // The refresh edge `current_duration` used to lack entirely.
                // A push-mode demuxer announces an approximate duration up
                // front and refines it as it plays (oggdemux over the fcomp
                // transport does exactly this), so without re-querying here an
                // opus track reports a duration a few seconds short for its
                // whole length.
                //
                // A pipeline image has no meaningful timeline and produces no
                // progress traffic at all (see `notify_updates`), so it must
                // not start doing so here.
                if self.image_via_player {
                    return Ok(());
                }
                match cacheable_duration(self.player.get_duration()) {
                    Some(duration) => {
                        debug!(?duration, "Duration refined mid-item");
                        self.current_duration = Some(duration);
                        // Out to the GUI and the senders now rather than at the
                        // next tick, on the same interval bypass a seek settle
                        // uses. Held back mid-load or mid-seek for the reason
                        // `notify_updates` holds back there: the position read
                        // would be transient and would fight the seek hold. The
                        // cached value is what matters, and the next tick
                        // reports it.
                        if self.player.have_media_info() && !self.player.is_seeking() {
                            self.playback_progress_changed();
                        }
                    }
                    // Nothing usable came back: keep what is cached (or keep
                    // the cache empty for the lazy read to retry).
                    None => debug!("Ignoring a duration change the pipeline cannot answer"),
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
                // Duration writer #1 of three, and the only one that
                // deliberately overwrites: a preroll or a resume is the edge
                // where the pipeline first has a real answer, so it always
                // wins. The other two never fight it: the lazy read in
                // `notify_updates` only fills an EMPTY cache, and
                // `DurationChanged` only ever refines upward from the source
                // itself. All three go through `cacheable_duration`, so a
                // failed or zero query leaves the cache empty for the lazy read
                // to retry rather than latching a zero (which would disable the
                // seek clamp and suppress every further gapless pre-arm).
                //
                // A gapless swap produces neither of these edges by design,
                // which is why the boundary needs the `DurationChanged` handler
                // and the activation's own `current_duration = None` reset.
                if first_paused || started_playing {
                    self.current_duration = cacheable_duration(self.player.get_duration());
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
                let prev_subtitle = self.player.current_subtitle_sid().map(str::to_string);
                let selected = self.player.streams_selected(
                    video.as_deref(),
                    audio.as_deref(),
                    subtitle.as_deref(),
                    seqnum,
                );
                // A confirmed subtitle switch makes the displayed cue stale.
                // Requests placed through `apply_track_change` cleared it
                // optimistically already; this also covers engine-initiated
                // switches (an external materializing, a re-assertion), whose
                // dispatch the application never sees.
                if selected.subtitle.as_deref() != prev_subtitle.as_deref() {
                    self.gui.clear_video_overlays();
                }
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
                // Attribution comes from fcastplaybin's generation-tagged
                // inputs (supersession is already handled by the generation
                // filter above); `failed_uri` is diagnostic only. External
                // subtitle inputs never error here: fcastplaybin handles
                // them itself and reports `ExternalSubtitleFailed` when one
                // is beyond saving.
                match err_origin {
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
            player::PlayerEvent::ExternalSubtitleFailed { id } => {
                // fcastplaybin already detached the input (failed attach,
                // bus error while its stream was shown, or no stream within
                // its watchdog); what is left is the protocol side: drop the
                // catalog entry and report ResourceNotFound.
                let ext_id = self.current_media.as_ref().and_then(|m| {
                    m.external_subtitles
                        .iter()
                        .find(|s| s.handle == id)
                        .map(|s| s.id)
                });
                match ext_id {
                    Some(ext_id) => self.fail_fcast_external_subtitle(ext_id),
                    // The catalog entry is already gone (the item was
                    // replaced, or the entry was removed by its sender).
                    None => debug!(?id, "Failure report for an unknown external subtitle"),
                }
            }
            player::PlayerEvent::Warning(msg) => {
                self.media_warning(msg)?;
            }
            player::PlayerEvent::StreamTagsUpdated => {
                self.update_tracks(false);
            }
            player::PlayerEvent::ImageStream(info) => {
                debug!(?info, "Image stream announced by fimagedec");
                self.inspector_image = format!(
                    "{} {}x{}{}",
                    info.format,
                    info.width,
                    info.height,
                    if info.animated { ", animated" } else { "" }
                );
            }
            player::PlayerEvent::GaplessActivated => {
                // Stamped with the PREPARED generation (excluded from the
                // load-scoped guard above); validate against the pre-arm.
                let Some(generation) = generation else {
                    return Ok(());
                };
                let armed = self
                    .gapless_prearm
                    .as_ref()
                    .filter(|prearm| prearm.generation == generation)
                    .map(|prearm| (prearm.next_index, prearm.url.clone(), prearm.cancelling));
                let Some((next_index, url, cancelling)) = armed else {
                    // A stale activation (superseded by a later load) is a
                    // harmless straggler. One AHEAD of the player means the
                    // pipeline switched while the application had dropped
                    // the pre-arm: reload the item the application believes
                    // is current so pipeline and state agree again. This is
                    // the last resort now that a cancel losing the race
                    // against the swap keeps its pre-arm (below).
                    if self
                        .player
                        .dbg_generation()
                        .is_some_and(|expected| generation > expected)
                    {
                        warn!(
                            generation,
                            "Gapless activation without a matching pre-arm: reloading to resync"
                        );
                        self.gapless_prearm = None;
                        self.player.clear_pending_gapless();
                        // No parked operation can be here (it only exists
                        // alongside the pre-arm this branch failed to match),
                        // and the reload's cleanup would drop one anyway.
                        self.load_media();
                    } else {
                        debug!(generation, "Ignoring a stale gapless activation");
                    }
                    return Ok(());
                };
                if !cancelling {
                    self.adopt_gapless_activation(generation, next_index);
                    return Ok(());
                }

                // The activation can beat the decline report to the loop (they
                // are emitted from different threads), so an operation parked
                // on that cancel is resolved HERE too, against the same
                // swap-already-performed state. A seek or speed change takes
                // over with a reload (it cannot be served by a pipeline whose
                // only linked upstream is the next item); a track change is
                // dropped and the activation is adopted normally below.
                if self.resolve_parked_gapless_op(GaplessOutcome::SwapPerformed) {
                    return Ok(());
                }

                // A cancel was requested and lost the race against the swap
                // (the decline report may still be behind this event in the
                // channel, the two are emitted from different threads): the
                // prepared item IS playing, so adopt it instead of falling
                // to the resync reload, which would restart the item that
                // just finished. The reason for the cancel may have been a
                // queue mutation though, so re-locate the item first.
                match self.prepared_queue_index(next_index, &url) {
                    Some(index) => {
                        debug!(
                            generation,
                            index, "Gapless: adopting the activation of a declined cancel"
                        );
                        self.adopt_gapless_activation(generation, index);
                    }
                    None => {
                        // The item that just went live was removed from the
                        // queue while its swap had already performed, so
                        // there is no slot to advance to. Hand the boundary
                        // back to the ordinary end-of-stream advance: it
                        // either loads the queue's next item (superseding
                        // the pipeline's phantom) or ends playback.
                        warn!(
                            generation,
                            url, "Gapless: the activated item is gone from the queue"
                        );
                        // Keep the player's view of the pipeline honest
                        // (the generation IS live) before tearing it down.
                        self.player.adopt_gapless_generation(generation);
                        // The parked operation, if any, was already resolved
                        // above (a seek/speed change reloaded and returned, a
                        // track change was dropped), so nothing survives here.
                        self.gapless_prearm = None;
                        self.player.end_of_stream_reached();
                        self.media_ended();
                        // The outgoing item did end, and this fallback is the
                        // gapped path: senders see the same shape they see
                        // for an ordinary end-of-stream advance.
                        if self.should_broadcast() {
                            self.broadcast_update(ReceiverToSenderMessage::V4(
                                fcast::V4Message::PlaybackStateChanged(
                                    fcast_protocol::v4::PlaybackState::Ended,
                                ),
                            ));
                        }
                        self.maybe_autoplay_advance();
                    }
                }
            }
            player::PlayerEvent::GaplessPrepareFailed { generation } => {
                // Also the exit for a prepare that fails while a cancel is in
                // flight: nothing will activate, so the `cancelling` pre-arm
                // must go here too or it would block gapless for the rest of
                // the item (the cancel's own outcome then finds no pre-arm
                // and is ignored).
                if self
                    .gapless_prearm
                    .as_ref()
                    .is_some_and(|prearm| prearm.generation == generation)
                {
                    debug!(
                        generation,
                        "Gapless pre-arm failed; the end-of-stream advance loads the item normally"
                    );
                    self.gapless_prearm = None;
                    self.gapless_blocked_item = Some(self.current_media_item_id);
                    self.player.clear_pending_gapless();
                    // Nothing was ever swapped in, so the playing item is the
                    // only input and a parked operation applies as if fresh.
                    self.resolve_parked_gapless_op(GaplessOutcome::PrepareGone);
                }
            }
            player::PlayerEvent::GaplessCancelled { generation } => {
                // The prepare really is gone and no activation follows, so the
                // bookkeeping the cancel kept alive can finally be dropped.
                // Re-arming the same item later is allowed again from here.
                match self.gapless_prearm.as_ref() {
                    Some(prearm) if prearm.cancelling => {
                        if let Some(generation) = generation
                            && generation != prearm.generation
                        {
                            // The application only ever has one pre-arm, so
                            // this is bookkeeping drift. Clear it anyway:
                            // keeping a pre-arm nothing will ever activate
                            // wedges gapless for the rest of the item.
                            warn!(
                                generation,
                                armed = prearm.generation,
                                "Gapless cancellation confirmed for an unexpected generation"
                            );
                        } else {
                            debug!(?generation, "Gapless pre-arm cancellation confirmed");
                        }
                        self.gapless_prearm = None;
                        self.player.clear_pending_gapless();
                        // The cancel won: the pipeline is back to a single
                        // linked input, so an operation parked on this outcome
                        // applies exactly as a fresh one would.
                        self.resolve_parked_gapless_op(GaplessOutcome::PrepareGone);
                    }
                    // Either a no-op cancel (nothing was prepared), or a
                    // fresh load already cleaned up and re-armed. Both are
                    // stragglers with nothing left to clear, and a pre-arm
                    // with no cancel in flight is NOT this cancel's. Nothing
                    // can be parked either (parking always marks the pre-arm
                    // `cancelling`, and every path that clears a pre-arm
                    // resolves the parked operation with it).
                    _ => debug!(
                        ?generation,
                        "Gapless cancellation confirmed with no pre-arm"
                    ),
                }
            }
            player::PlayerEvent::GaplessCancelDeclined { generation } => {
                if self
                    .gapless_prearm
                    .as_ref()
                    .is_some_and(|prearm| prearm.generation == generation)
                {
                    // The swap had already performed when the cancel reached
                    // the worker, so the prepared item goes live regardless.
                    //
                    // With nothing parked, the pre-arm is kept exactly as it
                    // is and the activation handler adopts it, which is what
                    // keeps the finished item from being reloaded. With a seek
                    // or speed change parked, the playing item's input is
                    // already unlinked and can never serve it, so that
                    // operation takes over: it reloads the item at its target,
                    // superseding the activation. A parked track change is
                    // dropped and the adoption proceeds as normal.
                    info!(
                        generation,
                        "Gapless cancellation declined (the swap already performed)"
                    );
                    self.resolve_parked_gapless_op(GaplessOutcome::SwapPerformed);
                } else {
                    // The activation beat this report to the loop (they are
                    // emitted from different threads) and was already
                    // adopted, or a load cleaned the pre-arm up first.
                    debug!(
                        generation,
                        "Gapless cancellation declined for a pre-arm that is already gone"
                    );
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn handle_raop_event(&mut self, event: Raop) -> Result<bool> {
        match event {
            Raop::ConfigAvailable(config) => {
                let run_raop = if cfg!(not(target_os = "android")) {
                    self.settings.raop_enabled()
                } else {
                    true
                };

                if run_raop && self.raop_server.is_none() {
                    info!(?config, "Starting raop server");

                    let msg_tx = self.msg_tx.clone();
                    tokio::spawn(async move {
                        // IpV4 only
                        let listener = match tokio::net::TcpListener::bind("0.0.0.0:33505").await {
                            Ok(listener) => listener,
                            Err(err) => {
                                warn!(
                                    ?err,
                                    "RAOP port 33505 unavailable, RAOP disabled for this instance"
                                );
                                return;
                            }
                        };

                        loop {
                            match listener.accept().await {
                                Ok((stream, _)) => msg_tx.raop(Raop::SenderConnected(stream)),
                                Err(err) => {
                                    warn!(?err, "RAOP listener accept failed; stopping");
                                    return;
                                }
                            }
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
                    self.settings.airplay_enabled()
                } else {
                    true
                };

                if run_airplay && self.airplay_server.is_none() {
                    info!(?config, "Starting airplay server");

                    let msg_tx = self.msg_tx.clone();
                    tokio::spawn(async move {
                        // IpV4 only
                        let listener = match tokio::net::TcpListener::bind((
                            "0.0.0.0",
                            airplay::AIRPLAY_TCP_PORT,
                        ))
                        .await
                        {
                            Ok(listener) => listener,
                            Err(err) => {
                                warn!(
                                    ?err,
                                    port = airplay::AIRPLAY_TCP_PORT,
                                    "AirPlay port unavailable, AirPlay disabled for this instance"
                                );
                                return;
                            }
                        };

                        loop {
                            match listener.accept().await {
                                Ok((stream, _)) => {
                                    msg_tx.airplay(AirPlay::SenderConnected(stream))
                                }
                                Err(err) => {
                                    warn!(?err, "AirPlay listener accept failed; stopping");
                                    return;
                                }
                            }
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
        }

        Ok(false)
    }

    fn refresh_inspector_graph(&self) {
        if !self.inspector_active {
            return;
        }
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

        // The graph walk runs on the player worker (serialized against loads
        // and teardowns, see `request_graph_snapshot`). The delivery callback
        // executes on that worker, so it only hands the layout work off to
        // the blocking pool.
        let runtime = tokio::runtime::Handle::current();
        self.player.request_graph_snapshot(move |snapshot| {
            runtime.spawn_blocking(move || {
                let scene = crate::inspector_graph::layout(&snapshot);
                let _ = gui_tx.send(gui::UpdateGuiCommand::SetGraphDump(
                    gui::GraphDumpData {
                        trigger: "manual".to_string(),
                        timestamp,
                        scene,
                    }
                    .into(),
                ));
            });
        });
    }

    /// One inspector sample: bitrate history (diffing the selected streams'
    /// cumulative parsed-byte counters against the previous tick), the track
    /// table, container, sink stats and player internals, pushed to the GUI
    /// as one command.
    fn inspector_tick(&mut self) {
        if !self.inspector_active {
            return;
        }
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
            buffering: self.inspector_buffering(),
        });
    }

    /// The buffering card's data: a summary of the current buffering state. `None` when the source
    /// can't answer a buffering query.
    fn inspector_buffering(&self) -> Option<gui::InspectorBuffering> {
        let info = self.player.dbg_buffering()?;

        Some(gui::InspectorBuffering {
            fill_fraction: (info.percent as f32 / 100.0).clamp(0.0, 1.0),
            fill_label: format!(
                "{}%{}",
                info.percent,
                if info.busy { " (busy)" } else { "" }
            ),
            ahead_label: self
                .player
                .buffered_ahead()
                .map(|ahead| format!("{:.1} s", ahead.seconds_f64()))
                .unwrap_or_default(),
            mode_label: format!("{:?}", info.mode).to_lowercase(),
            eta_label: info
                .buffering_left
                .filter(|left| *left > gst::ClockTime::ZERO)
                .map(|left| format!("full in {:.1} s", left.seconds_f64()))
                .unwrap_or_default(),
        })
    }

    /// The sources card's lines: one per live input, showing the uri's protocol and hostname when
    /// the element has a uri, and the element factory either way.
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
                let counts = match stats {
                    Some(stats) => format!(
                        "{} rendered, {} dropped",
                        stats.get::<u64>("rendered").unwrap_or(0),
                        stats.get::<u64>("dropped").unwrap_or(0),
                    ),
                    None => "stats n/a".to_string(),
                };
                lines.push(format!("audio: {format}, {counts}"));
            }
            None => lines.push("audio: no sink".to_string()),
        }
        lines
    }

    /// Returns `true` if the event loop should exit
    async fn handle_event(&mut self, event: Message) -> Result<bool> {
        match event {
            // Only meaningful while `resolve_listen_port` is awaiting a choice;
            // by the time the main loop runs the dialog is gone, so ignore it.
            Message::PortConflictChoice(_) => {}
            Message::SessionFinished => {
                self.gui.device_disconnected();
            }
            Message::SeekPercent(percent) => {
                debug!("SeekPercent({percent})");
                if let Some(duration) = self.current_duration {
                    if let Ok(pos) = gst::ClockTime::try_from_seconds_f64(
                        percent as f64 * duration.seconds_f64(),
                    ) {
                        self.gui_seek_hold = Some(GuiSeekHold {
                            target: pos.min(duration).seconds_f64(),
                            since: Instant::now(),
                        });
                        return self.handle_operation(Operation::Seek(pos), PacketOrigin::Gui);
                    }
                }
                self.gui_seek_hold = None;
                self.gui.set_seek_pending(false);
            }
            Message::Quit => return Ok(true),
            Message::ToggleDebug => self.debug_mode = !self.debug_mode,
            Message::Op { origin, op } => {
                debug!(?origin, ?op, "Operation from sender");
                return self.handle_operation(op, origin);
            }
            Message::Image(event) => return self.handle_image_event(event),
            Message::QueueCache(event) => self.queue_cache.on_event(event),
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

                if id != self.current_media_item_id {
                    debug!(id, "Ignoring media item finish event");
                    return Ok(false);
                }

                // A queue item's playback_duration elapsed: the item is
                // finished (the spec's autoplay trigger; the timer is only
                // armed for autoplay queues, see media_loaded_successfully).
                if matches!(media.source, MediaSource::Queue(_)) {
                    self.maybe_autoplay_advance();
                    return Ok(false);
                }

                let MediaSource::Playlist { content, index } = &media.source else {
                    debug!(id, "Ignoring media item finish event");
                    return Ok(false);
                };

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
                // Same gapless park as the protocol ChangeTrack above.
                self.park_or_apply_gapless_op(GaplessParkedOp::TrackChange { kind, sid });
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
                        "Load or seekability never resolved, rejecting parked subtitle source(s)"
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
            Message::InspectorActive(active) => {
                self.inspector_active = active;
                if !active {
                    // Reset per-session sampling so a reopen starts fresh.
                    self.inspector_bitrates = InspectorBitrates::default();
                }
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
            Message::SetConfigBool { key, value } => {
                #[cfg(not(target_os = "android"))]
                {
                    let mut known = false;
                    let res = self
                        .settings
                        .config
                        .update(|config| known = config.set_bool(&key, value));
                    self.report_config_change(&key, known, res);
                }
                #[cfg(target_os = "android")]
                let _ = (key, value);
            }
            Message::SetConfigString { key, value } => {
                #[cfg(not(target_os = "android"))]
                {
                    let mut known = false;
                    let res = self
                        .settings
                        .config
                        .update(|config| known = config.set_string(&key, &value));
                    self.report_config_change(&key, known, res);
                }
                #[cfg(target_os = "android")]
                let _ = (key, value);
            }
        }

        Ok(false)
    }

    /// Push the current persisted config into the settings drawer's bindings.
    #[cfg(not(target_os = "android"))]
    fn push_settings_to_ui(&self) {
        let path = self
            .settings
            .config
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.gui.init_settings(
            self.settings.config.get().clone(),
            path,
            cfg!(feature = "airplay"),
        );
    }

    /// Log the outcome of an autosaved settings change from the UI.
    #[cfg(not(target_os = "android"))]
    fn report_config_change(&self, key: &str, known: bool, result: std::io::Result<()>) {
        if !known {
            warn!(key, "Ignoring unknown setting from the settings UI");
        } else if let Err(err) = result {
            error!(?err, key, "Failed to persist settings change");
        } else {
            debug!(key, "Persisted settings change (applies on restart)");
        }
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

    /// Bind the FCast listening socket(s). `port == 0` requests an ephemeral
    /// port. When more than one address family is used the later families are
    /// pinned to the port the first was assigned so a single port number can be
    /// advertised.
    async fn bind_fcast_listeners(port: u16) -> std::io::Result<Vec<TcpListener>> {
        use std::net::{IpAddr, Ipv6Addr, SocketAddr};

        #[cfg(target_os = "windows")]
        let addrs: &[IpAddr] = &[
            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ];
        #[cfg(not(target_os = "windows"))]
        let addrs: &[IpAddr] = &[IpAddr::V6(Ipv6Addr::UNSPECIFIED)];

        let mut listeners = Vec::with_capacity(addrs.len());
        let mut chosen = port;
        for addr in addrs {
            let listener = TcpListener::bind(SocketAddr::new(*addr, chosen)).await?;
            if chosen == 0 {
                chosen = listener.local_addr()?.port();
            }
            listeners.push(listener);
        }
        Ok(listeners)
    }

    /// Acquire the FCast listening socket(s), prompting the user if the default
    /// port is already in use. Returns `None` if the user chose to quit before
    /// a port could be bound.
    async fn resolve_listen_port(
        &mut self,
        event_rx: &mut UnboundedReceiver<Message>,
    ) -> Result<Option<Vec<TcpListener>>> {
        match Self::bind_fcast_listeners(FCAST_TCP_PORT).await {
            Ok(listeners) => Ok(Some(listeners)),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                self.handle_port_conflict(event_rx).await
            }
            Err(err) => Err(err.into()),
        }
    }

    /// The default FCast port is taken. Surface the dialog and act on the
    /// user's choice. Only fcast relocates on "different port". gcast/raop/
    /// airplay stay on their fixed ports and simply skip if theirs is taken.
    #[cfg(not(target_os = "android"))]
    async fn handle_port_conflict(
        &mut self,
        event_rx: &mut UnboundedReceiver<Message>,
    ) -> Result<Option<Vec<TcpListener>>> {
        // Headless has no window to show the dialog in and no way to receive a
        // choice, so fail fast instead of blocking forever on a decision that
        // can never come.
        if self.settings.headless() {
            anyhow::bail!(
                "FCast port {FCAST_TCP_PORT} is already in use (another receiver may be running). \
                 Cannot prompt for an alternative in --headless mode"
            );
        }

        warn!(
            port = FCAST_TCP_PORT,
            "FCast port already in use; prompting user"
        );
        self.gui.show_port_conflict(FCAST_TCP_PORT);

        let outcome = loop {
            match event_rx.recv().await {
                Some(Message::PortConflictChoice(crate::message::PortConflictChoice::Retry)) => {
                    match Self::bind_fcast_listeners(FCAST_TCP_PORT).await {
                        Ok(listeners) => break Some(listeners),
                        // Still taken, leave the dialog up for another try.
                        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
                        Err(err) => return Err(err.into()),
                    }
                }
                Some(Message::PortConflictChoice(crate::message::PortConflictChoice::UseDifferentPort)) => {
                    let listeners = Self::bind_fcast_listeners(0).await?;
                    let port = listeners[0].local_addr()?.port();
                    info!(port, "Starting FCast on a different port");
                    self.fcast_port = port;
                    break Some(listeners);
                }
                // The Quit button ends the Slint loop, which drives `Message::Quit`.
                Some(Message::Quit) | None => break None,
                // Keep dispatching ordinary events while the dialog is up, in
                // particular the mDNS address/name updates that `start_daemon`
                // emitted before we got here, so the idle UI shows the network
                // info instead of "not connected". Mirror the main loop's
                // error handling.
                Some(other) => match self.handle_event(other).await {
                    Ok(true) => break None,
                    Ok(false) => {}
                    Err(err) => error!("Handle event error during port conflict: {err}"),
                },
            }
        };

        if self.settings.no_main_window() {
            // The dialog forced the window open, so restore the hidden state.
            self.gui.set_window_visibility(false);
        }
        Ok(outcome)
    }

    #[cfg(target_os = "android")]
    async fn handle_port_conflict(
        &mut self,
        _event_rx: &mut UnboundedReceiver<Message>,
    ) -> Result<Option<Vec<TcpListener>>> {
        anyhow::bail!("FCast port {FCAST_TCP_PORT} is already in use");
    }

        pub async fn run_event_loop(
            mut self,
            mut event_rx: UnboundedReceiver<Message>,
            fin_tx: tokio::sync::oneshot::Sender<()>,
        ) -> Result<()> {
            // Seed the settings drawer with the current persisted config.
            #[cfg(not(target_os = "android"))]
            self.push_settings_to_ui();

            // Acquire the FCast listening socket(s). If the default port is taken
            // this prompts the user (retry / different port / quit). `None` means
            // the user quit before anything was bound, so we skip serving and fall
            // straight through to the shutdown tail below.
            // When FCast is disabled we skip binding and advertising it entirely and
            // commit with no listeners, so the event loop still serves
            // chromecast/airplay/raop. `select_next_some` on the resulting empty,
            // terminated listener stream stays pending, so it simply never fires.
            let listeners = if self.settings.fcast_enabled() {
                self.resolve_listen_port(&mut event_rx).await?
            } else {
                info!("FCast receiver disabled by settings, not binding or advertising it");
                Some(Vec::new())
            };
            if let Some(listeners) = listeners {
                // The port is ours. Commit to running, publish the connection
                // QR/IP panel (addresses may have arrived while the conflict dialog
                // was up) and reveal the system tray.
                self.port_committed = true;
                // Advertise the fcast service now, at the port we actually bound,
                // so a second instance never publishes a duplicate record.
                #[cfg(not(target_os = "android"))]
                if self.settings.fcast_enabled() {
                    mdns::register_fcast(
                        &self.mdns,
                        &self.settings.fcast_name(),
                        self.fcast_port,
                        &self.fcast_txt_records,
                    )?;
                }
                self.update_connection_details()?;
                self.gui.show_system_tray();
                // Fade the startup screen out now that we're actually listening.
                self.gui.set_starting_up(false);

                let accept_streams = listeners.into_iter().map(|listener| {
                    // `Box::pin` so the `Unfold` streams are `Unpin`, as `select_all` requires.
                    Box::pin(futures::stream::unfold(listener, |listener| async move {
                        Some((listener.accept().await, listener))
                    }))
                });
                let mut listener_stream = futures::stream::select_all(accept_streams);

                #[cfg(not(target_os = "android"))]
                if self.settings.fullscreen() {
                    self.gui.set_fullscreen(true);
                }

                let mut update_interval = tokio::time::interval(PROGRESS_TICK_INTERVAL);

                use futures::stream::StreamExt;

                // Start at 1, let 0 be an anonymous session (e.g. the GUI)
                let mut session_id: SenderId = 1;
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
                                if let Err(err) = self.notify_updates(false) {
                                    error!(?err, "Failed to push a progress update");
                                }
                                self.send_v4_progress_updates();
                                self.maybe_prearm_gapless();
                            }
                        }
                        session = listener_stream.select_next_some() => {
                            match session {
                                Ok((stream, _)) => {
                                    self.handle_new_fcast_session(stream, session_id);
                                    session_id += 1;
                                }
                                // A failed accept is per-connection and usually
                                // transient. Propagating it would end the event loop, leaving the
                                // UI running with no protocol handling, no pipeline teardown, and
                                // mDNS still advertising a receiver that answers nothing.
                                Err(err) => {
                                    warn!(?err, "Failed to accept an FCast connection");
                                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                                }
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_show_durations_become_delays() {
        assert_eq!(show_duration_delay(0.0), Some(Duration::ZERO));
        assert_eq!(show_duration_delay(1.5), Some(Duration::from_millis(1500)));
        assert_eq!(show_duration_delay(30.0), Some(Duration::from_secs(30)));
    }

    /// A confirmed cancellation (or a failed prepare) leaves the playing item
    /// as the only linked input, so every parked operation runs as if fresh.
    #[test]
    fn a_won_cancel_replays_every_parked_operation() {
        for kind in [
            GaplessParkedOpKind::Seek,
            GaplessParkedOpKind::SetSpeed,
            GaplessParkedOpKind::TrackChange,
            GaplessParkedOpKind::SubtitleChange,
        ] {
            assert_eq!(
                parked_op_action(kind, GaplessOutcome::PrepareGone),
                ParkedOpAction::Replay,
                "{kind:?}"
            );
        }
    }

    /// The defect this whole park exists for: once the swap has performed, the
    /// prepared input is the only linked upstream, so a flushing seek would be
    /// answered by the NEXT item's source. Never replay one, reload instead.
    #[test]
    fn a_performed_swap_reloads_instead_of_seeking() {
        assert_eq!(
            parked_op_action(GaplessParkedOpKind::Seek, GaplessOutcome::SwapPerformed),
            ParkedOpAction::ReloadAtTarget
        );
        assert_eq!(
            parked_op_action(GaplessParkedOpKind::SetSpeed, GaplessOutcome::SwapPerformed),
            ParkedOpAction::ReloadAtTarget
        );
    }

    /// Deliberate policy: a track switch is not worth restarting the item the
    /// user is listening to, and the item is in its final stretch anyway.
    #[test]
    fn a_performed_swap_drops_a_parked_track_change() {
        assert_eq!(
            parked_op_action(
                GaplessParkedOpKind::TrackChange,
                GaplessOutcome::SwapPerformed
            ),
            ParkedOpAction::Drop
        );
        assert_eq!(
            parked_op_action(
                GaplessParkedOpKind::SubtitleChange,
                GaplessOutcome::SwapPerformed
            ),
            ParkedOpAction::Drop
        );
    }

    /// A parked operation must never leak into the next item: if the item ends
    /// before the outcome arrives, the advance owns the transition.
    #[test]
    fn an_ended_item_drops_every_parked_operation() {
        for kind in [
            GaplessParkedOpKind::Seek,
            GaplessParkedOpKind::SetSpeed,
            GaplessParkedOpKind::TrackChange,
            GaplessParkedOpKind::SubtitleChange,
        ] {
            assert_eq!(
                parked_op_action(kind, GaplessOutcome::ItemEnded),
                ParkedOpAction::Drop,
                "{kind:?}"
            );
        }
    }

    /// `resolve_parked_gapless_op` only reports "playback replaced" (and so
    /// skips the caller's adopt) for the actions that actually reload.
    #[test]
    fn only_a_reload_supersedes_a_pending_activation() {
        for kind in [
            GaplessParkedOpKind::Seek,
            GaplessParkedOpKind::SetSpeed,
            GaplessParkedOpKind::TrackChange,
            GaplessParkedOpKind::SubtitleChange,
        ] {
            for outcome in [
                GaplessOutcome::PrepareGone,
                GaplessOutcome::SwapPerformed,
                GaplessOutcome::ItemEnded,
            ] {
                let action = parked_op_action(kind, outcome);
                let reloads = action == ParkedOpAction::ReloadAtTarget;
                // Only a performed swap can strand an operation this way.
                assert_eq!(
                    reloads,
                    outcome == GaplessOutcome::SwapPerformed
                        && matches!(
                            kind,
                            GaplessParkedOpKind::Seek | GaplessParkedOpKind::SetSpeed
                        ),
                    "{kind:?} x {outcome:?} -> {action:?}"
                );
            }
        }
    }

    /// The `Some(0)` latch: a failed or zero duration query must NOT be
    /// cached. Latching one used to survive for the rest of the session (the
    /// lazy read is one-shot per item), putting `dur: 0` on the wire, disabling
    /// the seek clamp, mapping `SeekPercent` to 0 and suppressing every further
    /// gapless pre-arm.
    #[test]
    fn a_failed_or_zero_duration_query_is_never_cached() {
        assert_eq!(cacheable_duration(None), None);
        assert_eq!(cacheable_duration(Some(gst::ClockTime::ZERO)), None);
        let real = gst::ClockTime::from_seconds(42);
        assert_eq!(cacheable_duration(Some(real)), Some(real));
        // Sub-second durations are real durations (a short image/animation).
        let tiny = gst::ClockTime::from_nseconds(1);
        assert_eq!(cacheable_duration(Some(tiny)), Some(tiny));
    }

    #[test]
    fn unusable_show_durations_are_rejected_not_panicked() {
        // `showDuration` is an unvalidated `f64` on the v3 wire. Each of these
        // used to panic inside `Duration::from_secs_f64` on the app task.
        assert_eq!(show_duration_delay(-1.0), None);
        assert_eq!(show_duration_delay(f64::NAN), None);
        assert_eq!(show_duration_delay(f64::INFINITY), None);
        assert_eq!(show_duration_delay(f64::NEG_INFINITY), None);
        assert_eq!(show_duration_delay(f64::MAX), None);
    }
}
