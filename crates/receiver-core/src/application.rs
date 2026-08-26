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
    FCAST_TCP_PORT, GCastUpdateSender, MediaItemId, MessageSender, SenderId,
    external_subtitles::{self, ExternalSubtitle, is_external_track_id},
    fcast::{
        self, CompanionContext, InitialV4State, Operation, ReceiverToSenderMessage, SessionDriver,
        TranslatableMessage, WrappedPlayMessage,
    },
    fcompsrc,
    freeze_watchdog::{self, FreezeAction, FreezeSample},
    fwebrtcsrc, gcast,
    gui::{self, GuiController},
    image,
    media_formats::SupportedFormats,
    media_source,
    message::{Mdns, Message, Raop, ReceiverToFCastSender},
    player::{self, PlayerState},
    queue_cache, raop,
    ui_types::{AppState, GuiPlaybackState, UiMediaTrack, UiPlayerVariant, UiToastKind},
    utils::{current_time_millis, map_to_header_map},
};
#[cfg(not(target_os = "android"))]
use crate::{Settings, mdns};
#[cfg(feature = "airplay")]
use crate::{airplay, message::AirPlay};

const SENDER_UPDATE_INTERVAL: Duration = Duration::from_millis(500);

/// How long a seek may stay unsettled before senders hear anything.
const SEEK_QUIET_DEBOUNCE: Duration = Duration::from_millis(500);
const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_TICK_INTERVAL: Duration = Duration::from_millis(100);
/// Deliberately far above the progress tick: the stream-mode nub walks the
/// pipeline.
const BUFFERED_RANGES_INTERVAL: Duration = Duration::from_millis(1000);
/// Tolerance for releasing the optimistic thumb hold; absorbs tick sampling
/// drift only.
const SEEK_HOLD_TOLERANCE: f64 = 0.75;
/// Safety net so a dropped/failed seek can't freeze the thumb forever.
const SEEK_HOLD_TIMEOUT: Duration = Duration::from_secs(12);

/// Cap on events held for a pending pre-arm (see
/// `Application::held_prearm_events`). The window is at most the aqueue
/// depth (~1s audio, ~30s video) at UI event rates, so the cap only bites a
/// runaway; overflow keeps the prefix and drops the newcomer so replay order
/// stays coherent.
const HELD_PREARM_EVENTS_MAX: usize = 256;
/// Pause after a failed `accept()`; a failing accept returns immediately, so
/// this bounds the spin.
pub(crate) const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

#[cfg(any(target_os = "macos", target_os = "windows"))]
const UPDATER_BASE_URL: &str = "http://dl.fcast.org/receiver/desktop";

/// State for an in-flight optimistic GUI seek.
struct GuiSeekHold {
    /// Requested position in seconds, clamped to the media duration.
    target: f64,
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
    /// Spec'd Queue.autoplay: the receiver advances by itself when an item
    /// finishes.
    autoplay: bool,
}

/// A gapless pre-arm in flight: the next queue item is prepared on the live
/// pipeline and activates at the current item's drain, with no pipeline EOS.
struct GaplessPrearm {
    /// Generation the prepared item adopts at activation; validates
    /// GaplessActivated.
    generation: u64,
    next_index: usize,
    /// Identifies the item when the queue moved under a declined cancel's
    /// activation.
    url: String,
    /// Cancel requested, outcome unknown. The pre-arm is KEPT: a declined
    /// cancel activates anyway and that activation must be adopted, not
    /// resync-reloaded.
    cancelling: bool,
}

/// An operation held back until a gapless pre-arm's cancellation reports its
/// outcome. Every payload is already validated, so a replay cannot emit a
/// second error reply.
#[derive(Debug)]
enum GaplessParkedOp {
    /// Already range-clamped; any `SeekOutOfRange` reply went out on arrival.
    Seek {
        origin: PacketOrigin,
        time: gst::ClockTime,
    },
    /// A real speed change; an idempotent one never parks.
    SetSpeed { origin: PacketOrigin, rate: f32 },
    /// Already resolved from wire track index to stream id (`None` disables the
    /// slot).
    TrackChange {
        kind: player::TrackKind,
        sid: Option<player::StreamId>,
    },
    SubtitleChange {
        origin: PacketOrigin,
        target: SubtitleTarget,
    },
}

/// The outcome policy depends on the operation's shape, not its payload.
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
    /// Nothing will activate; the playing item is the only linked input again.
    PrepareGone,
    /// The swap performed: the playing item's input is unlinked, so a flushing
    /// seek would be answered by the SUCCESSOR's source and the item must
    /// be reloaded.
    SwapPerformed,
    /// The end-of-stream advance owns the transition; nothing left to act on.
    ItemEnded,
}

/// What to do with a parked operation once the outcome is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParkedOpAction {
    Replay,
    ReloadAtTarget,
    Drop,
}

/// The park-until-outcome policy. `(TrackChange, SwapPerformed)` is dropped
/// rather than reloaded: flushing a switch into the successor corrupts
/// playback, and re-resolving a stream id from the retiring item's collection
/// is the selection engine's job.
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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum StaleEventAction {
    /// Describes the one real pipeline, not the item it is playing: only the
    /// attribution is future, so it applies now.
    Apply,
    /// The pending pre-arm's generation: the pipeline's future, not a
    /// straggler. Held for the adoption's drain.
    Hold,
    /// A superseded load's straggler.
    Drop,
}

/// What the generation filter does with a load-scoped event whose generation
/// is not the current one. The pipeline adopts the prepared generation at the
/// swap, up to a queue depth ahead of the activation the application adopts,
/// so dropping the pending generation lost real state (the new item's first
/// StreamsSelected, Tags, Buffering, StateChanged, genuine Errors).
///
/// Transport edges must not be held: the mirror state machine only converges
/// through them, and a held Playing->Paused leaves it mid-transition, where a
/// Resume merely retargets and dispatches nothing. The pipeline then stays
/// PAUSED, no audio crosses the boundary, the activation is never released and
/// the held edge never replays. Permanent wedge, so apply them.
fn stale_event_action(
    generation: u64,
    pending_prearm: Option<u64>,
    pipeline_scoped: bool,
) -> StaleEventAction {
    if pending_prearm != Some(generation) {
        StaleEventAction::Drop
    } else if pipeline_scoped {
        StaleEventAction::Apply
    } else {
        StaleEventAction::Hold
    }
}

/// How a confirmed cancel resolves against the pre-arm bookkeeping.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CancelReport {
    /// Nothing will activate: drop the bookkeeping and resolve what was parked
    /// against the still-playing item.
    PrepareGone,
    /// The prepared slot was already consumed by an activation the application
    /// has not adopted yet; handled exactly like an explicit decline.
    SwapPerformed,
    /// Not this application's cancel.
    Straggler,
}

/// What a `GaplessCancelled` means for the pre-arm. `generation: None` says
/// the crate found nothing prepared, and with a cancel of ours in flight the
/// only thing that empties that slot is the activation: the swap already
/// performed, the successor is the only linked upstream, and replaying a
/// parked flushing seek would hit IT (invariant 8). Report the swap instead,
/// which keeps the pre-arm for the imminent activation to adopt.
fn cancel_report(reported: Option<u64>, armed: Option<u64>, cancelling: bool) -> CancelReport {
    if armed.is_none() || !cancelling {
        return CancelReport::Straggler;
    }
    match reported {
        // A generation mismatch is bookkeeping drift, not a live prepare: the
        // caller warns and clears, since keeping a pre-arm nothing will
        // activate wedges gapless for the rest of the item.
        Some(_) => CancelReport::PrepareGone,
        None => CancelReport::SwapPerformed,
    }
}

/// When a held event may replay, relative to the adopted item's own events.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum HeldReplayPhase {
    /// Nothing else has to land first: replays in the adoption itself.
    Adoption,
    /// Resolves stream ids, so it waits for the collection that installs the
    /// adopted item's stream list.
    FirstCollection,
}

/// Where a held event belongs in the replay. The crate emits the activation
/// and the new collection as two messages, so the adoption's drain runs one
/// message BEFORE the collection: a selection replayed there resolves the new
/// item's sids against the retired item's stream list, resolves nothing, and
/// publishes "no track selected" for the rest of the item.
fn held_replay_phase(event: &player::PlayerEvent) -> HeldReplayPhase {
    match event {
        player::PlayerEvent::StreamsSelected { .. } => HeldReplayPhase::FirstCollection,
        _ => HeldReplayPhase::Adoption,
    }
}

/// A failed or zero duration means "not known yet"; latching it would poison
/// the wire duration, seek clamp and gapless pre-arm for the session, so stay
/// `None` and re-ask.
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
    use player::MediaErrorKind as K;
    match kind {
        K::NotFound | K::AccessDenied => ErrorKind::ResourceNotFound,
        K::UnsupportedFormat | K::MissingCodec | K::DecodeFailed | K::DrmProtected => {
            ErrorKind::UnsupportedFormat
        }
        K::NetworkFailure
        | K::OutputFailure
        | K::ImageDownloadFailed
        | K::Frozen
        | K::Unexpected => ErrorKind::Internal,
    }
}

/// Which UI surface a fatal error takes. `None` means the report-bug popup
/// (see `Application::show_bug_report`), otherwise the localized toast.
fn media_error_toast_kind(kind: player::MediaErrorKind) -> Option<UiToastKind> {
    use player::MediaErrorKind as K;
    match kind {
        K::NotFound => Some(UiToastKind::MediaNotFound),
        K::AccessDenied => Some(UiToastKind::AccessDenied),
        K::NetworkFailure => Some(UiToastKind::NetworkFailure),
        K::UnsupportedFormat => Some(UiToastKind::UnsupportedFormat),
        K::DecodeFailed => Some(UiToastKind::DecodeFailed),
        K::DrmProtected => Some(UiToastKind::DrmProtected),
        K::OutputFailure => Some(UiToastKind::OutputFailure),
        K::ImageDownloadFailed => Some(UiToastKind::ImageDownloadFailed),
        // A codec the receiver does not ship is a packaging gap worth a
        // report, not a user mistake.
        K::MissingCodec | K::Frozen | K::Unexpected => None,
    }
}

fn media_warning_toast_kind(kind: player::MediaWarningKind) -> UiToastKind {
    use player::MediaWarningKind as K;
    match kind {
        K::MissingCodecForTrack => UiToastKind::MissingCodecForTrack,
        K::StuckStream => UiToastKind::StuckStream,
        K::SubtitleFormatUnsupported => UiToastKind::SubtitleFormatUnsupported,
        K::Unknown => UiToastKind::GenericWarning,
    }
}

/// Query and fragment stripped, so tokens and signatures in media URLs
/// cannot end up in a bug-report screenshot.
fn strip_uri_query(uri: &str) -> &str {
    let end = uri.find(['?', '#']).unwrap_or(uri.len());
    &uri[..end]
}

/// Host of a URI, for toast detail. Userinfo and port dropped.
fn uri_host(uri: &str) -> Option<&str> {
    let rest = uri.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split_once(':').map_or(host, |(host, _)| host);
    (!host.is_empty()).then_some(host)
}

const ISSUE_TRACKER_URL: &str = "https://github.com/futo-org/fcast/issues";

/// Ring depth for the bug-report context and per-entry message cap, so one
/// debug dump cannot flood the diagnostic block.
const RECENT_WARNINGS_CAP: usize = 10;
const RECENT_WARNING_MSG_MAX: usize = 200;

type RecentWarnings = VecDeque<(Instant, &'static str, String)>;

fn push_recent_warning(ring: &mut RecentWarnings, at: Instant, code: &'static str, message: &str) {
    let mut message = message.to_owned();
    if message.len() > RECENT_WARNING_MSG_MAX {
        let cut = (0..=RECENT_WARNING_MSG_MAX)
            .rev()
            .find(|i| message.is_char_boundary(*i))
            .unwrap_or(0);
        message.truncate(cut);
        message.push_str("...");
    }
    if ring.len() == RECENT_WARNINGS_CAP {
        ring.pop_front();
    }
    ring.push_back((at, code, message));
}

/// The warnings preceding a fatal error are usually the actual story (a
/// stuck stream, a discard streak), so the bug-report block carries them.
fn format_recent_warnings(ring: &RecentWarnings, now: Instant) -> String {
    if ring.is_empty() {
        return "no recent warnings".to_owned();
    }
    let mut out = String::from("recent warnings");
    for (at, code, message) in ring {
        let secs = now.saturating_duration_since(*at).as_secs();
        out.push_str(&format!("\n{secs}s ago {code} {message}"));
    }
    out
}

fn issue_tracker_qr() -> Option<crate::ui_types::QrCode> {
    let qrcode = fast_qr::QRBuilder::new(ISSUE_TRACKER_URL.as_bytes())
        .build()
        .ok()?;
    let dims = qrcode.size as u32;
    let module_count = (dims * dims) as usize;
    let dark = qrcode.data[0..module_count]
        .iter()
        .map(|module| *module != fast_qr::Module::LIGHT)
        .collect();
    Some(crate::ui_types::QrCode { size: dims, dark })
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

/// An `AddSubtitleSource` that arrived before the receiver could act on it.
struct PendingSubtitleAdd {
    url: String,
    select: bool,
    name: Option<SmolStr>,
    origin: PacketOrigin,
}

#[derive(Debug)]
enum SubtitleTarget {
    /// An advertised stream by id, or `None` to show no subtitle.
    Stream(Option<player::StreamId>),
    /// A catalog external whose stream has not materialized yet.
    External(u32),
}

struct MediaSourceState {
    origin: PacketOrigin,
    source: MediaSource,
    image_id: Option<image::ImageId>,
    pending_thumbnail: Option<image::ImageId>,
    pending_thumbnail_download: Option<image::ImageDownloadId>,
    /// Owns the STABLE advertised track ids for the current item's externals.
    externals: external_subtitles::Catalog,
}

impl MediaSourceState {
    fn new(origin: PacketOrigin, source: MediaSource) -> Self {
        Self {
            origin,
            source,
            image_id: None,
            pending_thumbnail: None,
            pending_thumbnail_download: None,
            externals: external_subtitles::Catalog::default(),
        }
    }

    /// Drop every external subtitle; the id counter keeps advancing so stale
    /// ids can't alias.
    fn clear_external_subtitles(&mut self) {
        self.externals.clear();
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
    android_app: android_activity::AndroidApp,
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
    /// Silences playback-state broadcasts mid-seek: transient Idle/Buffering
    /// read as "playback ended" to senders. On timeout v4 gets Buffering,
    /// v1-v3 gets nothing.
    seek_quiet: bool,
    seek_quiet_epoch: u64,
    /// Pins the slider thumb at the seek target so a stale position tick can't
    /// spring it back.
    gui_seek_hold: Option<GuiSeekHold>,
    /// Bumped per pipeline load so a stale `LoadStallCheck` watchdog no-ops.
    load_watchdog_epoch: u64,
    /// Active server-directed source backoff (deadline, total ms), shown by
    /// the GUI as a "server busy" countdown.
    source_backoff: Option<(Instant, u64)>,
    /// Bumped per backoff change/clear so a stale `SourceBackoffTick` no-ops.
    source_backoff_epoch: u64,
    /// Detects a silently wedged pipeline and drives recovery. Lever:
    /// `FCAST_NO_FREEZE_WATCHDOG`.
    freeze_watchdog: freeze_watchdog::FreezeWatchdog,
    current_image_id: image::ImageId,
    current_image_download_id: image::ImageDownloadId,
    /// True while the load is an image routed through the player pipeline
    /// (fimagedec): progress traffic is suppressed and the image view is
    /// painted transparent.
    image_via_player: bool,
    have_audio_track_cover: bool,
    current_media: Option<MediaSourceState>,
    have_media_info: bool,
    current_thumbnail_id: image::ImageId,
    current_addresses: HashSet<IpAddr>,
    /// The port actually bound (the user may relocate it on a conflict); the QR
    /// code and network config advertise this so discovery stays correct.
    fcast_port: u16,
    /// False until the port is bound; the QR/IP panel must not be published
    /// before then.
    port_committed: bool,
    have_media_title: bool,
    // GStreamer re-emits the artist tag many times per item and gapless has no
    // queue-metadata artist to gate on, so dedup by value rather than a seen-flag.
    last_artist_name: Option<String>,
    last_position_updated: f64,
    http_client: reqwest::Client,
    /// Signalling channel from the last `StartMirroringSession`; a live object,
    /// so it is handed to `fwebrtcsrc` as a typed property rather than
    /// through a URI.
    pending_fwebrtc_channel: Option<fwebrtcsrc::SignallingChannel>,
    device_name: Option<String>,
    current_media_item_id: MediaItemId,
    /// Which item already showed the report-bug popup, one per item max.
    bug_report_shown_for: Option<MediaItemId>,
    /// Last few classified warnings of the current item, bug-report context.
    recent_warnings: RecentWarnings,
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
    /// Prefetched bytes for queue items around the current index.
    queue_cache: queue_cache::Cache,
    queue_prefetcher: queue_cache::Prefetcher,
    gapless_prearm: Option<GaplessPrearm>,
    /// Invariant: a parked operation only ever exists alongside a `cancelling`
    /// pre-arm, so every site that clears `gapless_prearm` must also decide
    /// this one's fate. Untimed by design: flapjack reports exactly one
    /// outcome per cancel.
    gapless_parked_op: Option<GaplessParkedOp>,
    /// Events stamped with the PENDING pre-arm's generation, held until the
    /// activation is adopted. The pipeline adopts the prepared generation at
    /// the swap while the user-facing activation is held to the audio
    /// boundary, so everything it emits in that window (the new item's first
    /// StreamsSelected, Tags, Buffering, StateChanged, genuine Errors) is
    /// ahead of `expected_generation`. Dropping them lost real state, the
    /// worst being a Buffering(100) whose absence latched the mirror machine
    /// in Buffering for the session. Drained through `handle_player_event` at
    /// adoption, where each event re-passes the generation filter, so a
    /// straggler held for a pre-arm that died is dropped there. Cleared with
    /// the pre-arm bookkeeping at every site that clears `pending_gapless`.
    held_prearm_events: Vec<(player::PlayerEvent, Option<u64>)>,
    /// Start position/rate overriding the next load's own `time`/`speed`, for
    /// when a load stands in for an operation the pipeline can no longer
    /// serve. Consumed unconditionally by the next load so it can never
    /// leak into a later one.
    load_start_override: Option<player::RestorePoint>,
    /// Item whose pre-arm FAILED; blocks re-arming so each tick can't retry the
    /// same failure.
    gapless_blocked_item: Option<MediaItemId>,
    /// Kill switch: FCAST_NO_GAPLESS=1 forces the ordinary EOS-then-load path.
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
    /// Gates all inspector work so nothing is computed or sent while it is
    /// closed.
    inspector_active: bool,
    inspector_container: Option<String>,
    inspector_image: String,
}

/// Inspector bitrate sampling: previous cumulative parsed-byte totals plus rate
/// histories in kbit/s, oldest first.
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

    /// Fold a cumulative sample into a slot; a changed key restarts the counter
    /// at 0 rather than reporting a bogus delta.
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
        // The video sink's subtitle cue state; `None` when headless.
        cue_engine: Option<fcast_video::cue::CueEngine>,
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

        // Soak-harness escape hatch: the Intel VA dmabuf-export path leaks GPU state
        // across restarts and eventually hangs the sink in an async Playing->Paused.
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
            cue_engine,
            msg_tx.clone(),
            fcompsrc::imp::CompContext(companion_ctx.clone()),
            #[cfg(feature = "airplay")]
            airplay_context.clone(),
        )?;

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
                    // A failed bind must not take the process down; log and skip gcast.
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
            seek_quiet: false,
            seek_quiet_epoch: 0,
            gui_seek_hold: None,
            load_watchdog_epoch: 0,
            source_backoff: None,
            source_backoff_epoch: 0,
            freeze_watchdog: freeze_watchdog::FreezeWatchdog::new(),
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
            bug_report_shown_for: None,
            recent_warnings: RecentWarnings::new(),
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
            held_prearm_events: Vec::new(),
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

    /// Relay a playback rate to all senders.
    fn broadcast_rate(&mut self, rate: f32) -> Result<()> {
        self.notify_updates(true)?;
        if self.updates_tx.strong_count() > 0 {
            let _ = self.updates_tx.send(Arc::new(ReceiverToSenderMessage::V4(
                fcast::V4Message::PlaybackRateChanged(rate),
            )));
        }
        Ok(())
    }

    /// Apply a volume command and confirm the clamped value to senders
    /// immediately.
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
            PacketOrigin::FCast { sender_id, .. } => Some(sender_id),
            _ => None,
        };

        if let Some(sender_id) = sender_id
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

    /// Push the scrubber's buffered indicator, throttled to
    /// [`BUFFERED_RANGES_INTERVAL`]. Real timeline ranges when available,
    /// else a stream-mode buffered-ahead nub.
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

        let nub = self.buffered_ahead_range();
        self.gui.set_buffered_ranges(nub.into_iter().collect());
    }

    /// The buffered-ahead nub as a `(start, stop)` timeline fraction.
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

        // A seek's settle while paused reaches the thumb here, not via the tick.
        self.release_seek_hold_if_landed(position.seconds_f64());
        self.gui
            .update_playback_progress(position.seconds_f64() as f32, duration.seconds_f64() as f32);
        self.push_buffered_ranges();

        // Bypasses per-sender intervals on purpose; debounced because the start/seek
        // dance emits bursts of state edges (observed: 5 within 14ms).
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
        // A pipeline image has no meaningful progress.
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

    /// Arm the seek broadcast debounce.
    fn arm_seek_quiet(&mut self) {
        self.seek_quiet = true;
        self.seek_quiet_epoch += 1;
        let epoch = self.seek_quiet_epoch;
        let msg_tx = self.msg_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SEEK_QUIET_DEBOUNCE).await;
            msg_tx.send(Message::SeekQuietTimeout { epoch });
        });
    }

    fn playback_state_changed(&mut self, state: fcast_protocol::v4::PlaybackState) {
        use fcast_protocol::v4::PlaybackState as S;
        // Seek debounce: transients stay quiet, a settled state ends the window.
        match state {
            S::Idle | S::Buffering if self.seek_quiet => return,
            S::Playing | S::Paused | S::Ended => self.seek_quiet = false,
            _ => {}
        }
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
    /// Release the optimistic GUI seek hold once the pipeline lands (or the
    /// timeout elapses).
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
        // A pipeline image loops forever: no progress traffic, other broadcasts
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
        // Must precede the progress write below, which is suppressed while the hold is
        // active.
        self.release_seek_hold_if_landed(position);
        self.last_position_updated = position;
        // Deliberately ONE-SHOT per item: a re-query mid-item can be answered by the
        // NEXT item once a gapless swap has performed, latching the successor's
        // duration.
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
            && !self.seek_quiet
            && (self.last_sent_update.elapsed() >= SENDER_UPDATE_INTERVAL || force)
        {
            let update = v3::PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: Some(position),
                duration: Some(duration),
                // NOT the GUI Loading state: it collapses Buffering into Idle, which reads
                // as "playback ended" on the wire and advances senders' queues mid-handoff.
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
        // Playback is stopping or being replaced: a real Idle must go out.
        self.seek_quiet = false;
        self.gapless_prearm = None;
        self.player.clear_pending_gapless();
        self.held_prearm_events.clear();
        self.gapless_parked_op = None;
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
        self.image_via_player = false;
        self.gui.set_image_via_player(false);
        self.player.stop();
        self.is_loading_media = false;
        if let Some(current_media) = self.current_media.as_mut() {
            current_media.image_id = None;
            current_media.pending_thumbnail = None;
            current_media.pending_thumbnail_download = None;
        }

        self.current_thumbnail_id += 1;
        self.current_image_id += 1;
        self.current_image_download_id += 1;
        self.clear_source_backoff();

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

    /// Arm the show-duration timer for `id`. `show_duration` is a bare wire
    /// `f64`, so a negative/NaN/huge value is rejected here rather than
    /// panicking in `Duration`.
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
                // Spec'd playback_duration marks the item finished. Images never post EOS,
                // so a slideshow only advances through this timer.
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

    /// `diagnostic` is the raw technical text. It goes to senders and the
    /// log verbatim; the user sees only the kind's localized wording (or the
    /// report-bug popup, where the diagnostic block IS the point).
    fn media_error(
        &mut self,
        kind: player::MediaErrorKind,
        detail: Option<String>,
        diagnostic: String,
    ) -> Result<()> {
        if !self.is_playing() {
            return Ok(());
        }

        error!(?kind, msg = diagnostic, "Media error");

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
                message: diagnostic.clone(),
            }))
        }

        match media_error_toast_kind(kind) {
            Some(toast) => self.gui.show_toast(toast, detail, kind.code()),
            None => self.show_bug_report(kind, detail.as_deref(), &diagnostic),
        }

        Ok(())
    }

    /// At most one popup per loaded item (the id bumps on every load), so an
    /// error storm cannot stack dialogs. `detail` is the kind's interpolable
    /// scrap (the codec description for MissingCodec), shown on the first
    /// line of the block.
    fn show_bug_report(
        &mut self,
        kind: player::MediaErrorKind,
        detail: Option<&str>,
        diagnostic: &str,
    ) {
        if self.bug_report_shown_for == Some(self.current_media_item_id) {
            return;
        }
        self.bug_report_shown_for = Some(self.current_media_item_id);
        let head = match detail {
            Some(detail) => format!("{} {:?} ({detail})", kind.code(), kind),
            None => format!("{} {:?}", kind.code(), kind),
        };
        let block = format!(
            "{}\nreceiver {}\n{}\n{}\n{}",
            head,
            env!("CARGO_PKG_VERSION"),
            gst::version_string(),
            diagnostic,
            format_recent_warnings(&self.recent_warnings, Instant::now()),
        );
        self.gui
            .show_bug_report(block, kind.code(), issue_tracker_qr());
    }

    fn media_warning(
        &mut self,
        kind: player::MediaWarningKind,
        detail: Option<String>,
        message: String,
    ) -> Result<()> {
        // Ignore false positives from the video sink before its GL contexts are set.
        if !self.is_playing() {
            return Ok(());
        }

        warn!(?kind, msg = message, "Media warning");

        push_recent_warning(
            &mut self.recent_warnings,
            Instant::now(),
            kind.code(),
            &message,
        );
        self.gui
            .show_toast(media_warning_toast_kind(kind), detail, kind.code());

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

        // An autoplay queue with a next item is exempt: the receiver-side advance must
        // keep working after the last sender disconnects.
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
        // The popup belongs to the failed item, a new cast replaces it.
        self.gui.hide_bug_report();
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

    /// Cached prefetch entry usable for this load. Queue-sourced cacheable
    /// containers only: Single loads must not serve stale bytes and
    /// HLS/DASH demuxers need the upstream URI.
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

    /// Build the source for a load with typed config. AirPlay mirror is built
    /// elsewhere.
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
                // Fall back to the URI path so the load surfaces a real error.
                player::MediaInput::Uri(url)
            }
        }
    }

    fn load_current_media_item(&mut self) -> std::result::Result<(), LoadMediaError> {
        // Taken unconditionally: surviving one of the early exits would relocate a
        // LATER load.
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
                .ok_or(LoadMediaError::IndexOutOfBounds)?
                .clone(),
            MediaSource::Queue(queue) => queue
                .items
                .get(queue.current_idx as usize)
                .ok_or(LoadMediaError::IndexOutOfBounds)?
                .to_media_item(),
            MediaSource::Raop => {
                warn!("Cannot load RAOP source");
                return Ok(());
            }
            MediaSource::AirPlayMirror { .. } => {
                // The mirror URI is set in the MirrorStarted handler, not here.
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
        // An override stands for the operation this load replaces, so it wins over the
        // item's own start point.
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
            is_for_sure_live = true;
        }

        // Image containers fimagedec decodes in the player pipeline; anything it cannot
        // decode stays on the legacy in-GUI path.
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
            // Legacy still images keep the previous frame up while the next one downloads.
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

        self.image_via_player = pipeline_image;
        self.gui.set_image_via_player(pipeline_image);

        let mut is_image = false;
        if container.starts_with("image/") && !pipeline_image {
            is_image = true;
            if let Some(item) = self
                .queue_cache_entry(&url, &container)
                .filter(|item| item.complete)
            {
                // Only complete entries decode; a partial head is not a decodable image.
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
            // Live sources get no post-preroll start seek.
            let start = (!is_for_sure_live).then_some(player::RestorePoint {
                position: start_position,
                rate: playback_rate,
            });
            let source = self.build_media_source(&container, url, headers.clone());
            self.player.load(source, start);
            if let Some(volume) = volume {
                // Stamp the echo window so stale read-back notifies aren't relayed as
                // external changes; the confirm comes from the Load relay itself.
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
        self.recent_warnings.clear();

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
        self.clear_source_backoff();

        // A pipeline load should reach a steady PAUSED quickly; dump diagnostics if
        // not.
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

        // A pipeline image exposes a raw video stream, but the UI stays on the Image
        // variant.
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

    /// `relay` forwards the selection as `QueueItemSelected`; must be `false`
    /// for implicit selections, whose triggering `Load` is relayed on its
    /// own.
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

        self.cancel_gapless_prearm(player::AfterCancel::Nothing);

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

    /// The index the receiver advances to by itself. Also gates the
    /// `media_ended` teardown, so an unattended autoplay queue is not wiped
    /// between items.
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

        if self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(fcast::V4Message::Broadcast {
                serialized_msg: fcast_protocol::v4::MessageBuilder::new()
                    .queue_select(v4::QueuePosition::Index(next as u8)),
            }));
        }
    }

    /// Must exceed the audio queue's 30s of DECODED audio: an audio stream's
    /// EOS passes decodebin3's outputs that far before the item audibly
    /// ends, and the pre-arm must beat it there or the handoff is missed.
    const GAPLESS_PREARM_MARGIN: gst::ClockTime = gst::ClockTime::from_seconds(40);

    /// Plain progressive A/V only: start/speed/volume overrides need a real
    /// load, images never post EOS, and adaptive/live containers cannot
    /// ride a prepared input.
    fn gapless_eligible(item: &QueueItem) -> bool {
        item.time.is_none()
            && item.speed.is_none()
            && item.volume.is_none()
            && !item.content_type.starts_with("image/")
            && queue_cache::cacheable_container(&item.content_type)
    }

    /// Pre-arm the next autoplay queue item near the current item's end; the
    /// advance itself is handled by the GaplessActivated event.
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
        // External subtitles are side inputs on the live core; a swap would carry them
        // into the next item's collections, so those items take the ordinary EOS load.
        if self
            .current_media
            .as_ref()
            .is_some_and(|m| !m.externals.is_empty())
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
        // A playback_duration item advances through its timer; a pre-arm would fight
        // it.
        if current_show_duration.is_some() {
            return;
        }
        if !Self::gapless_eligible(&next_item) {
            return;
        }
        let Some(position) = self.player.get_position() else {
            return;
        };
        let Some(duration) = self.current_duration.filter(|d| !d.is_zero()) else {
            return;
        };
        // Items shorter than the pre-arm margin take the gapped path. Such
        // an item pre-arms on its FIRST tick, mid-bring-up, and its input
        // drains decode-paced within milliseconds, so the swap lands seconds
        // before the audible boundary and decodebin3 then holds the old
        // item's drained state next to the new item's arriving one for the
        // item's whole remaining playtime. That coexistence is where every
        // R1 boundary wedge lived (slots removed under the successor,
        // outputless selected slots, unused-slot EOS churn); the
        // carry-patches cover the shapes we caught, upstream has not touched
        // decodebin3 in over a year, and a measured 15s item still wedged,
        // so the safe set is exactly the items whose pre-arm fires in steady
        // state. A short item's gap is imperceptible next to a wedged
        // session.
        if duration < Self::GAPLESS_PREARM_MARGIN {
            debug!(
                next,
                "Gapless: current item shorter than the pre-arm margin; gapped advance"
            );
            return;
        }
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

    /// One tick of the freeze watchdog. Must run on EVERY progress tick,
    /// including while paused/loading/stopped: the detector owns every
    /// exclusion, and skipping ticks would let excluded time count as
    /// pinned playback on the first resumed tick.
    fn poll_freeze_watchdog(&mut self) -> Result<()> {
        // A DEAD SUBTITLE TRACK, which the discard escalation cannot see.
        // Sampled here because this is the tick that already exists; the
        // verdict needs elapsed time and the bus hook has none (see
        // `player::SubtitleFlow`). Reported at most once per load, and only
        // logged: the track is gone for the item either way, and a toast for
        // something the user cannot act on is the noise the warning filter
        // exists to prevent.
        if let Some(stream) = self.player.stalled_subtitle_stream() {
            error!(
                stream,
                "the subtitle track took a FLUSHING discard and has delivered nothing since: \
                 its multiqueue slot is latched and the track will not play again for this item"
            );
        }

        if !self.freeze_watchdog.enabled() {
            return Ok(());
        }

        let playing = self.player.player_state() == PlayerState::Playing;
        let sample = FreezeSample {
            now: Instant::now(),
            item: self.current_media_item_id,
            playing,
            // Asked of the pipeline, not predicted: a flushing seek re-prerolls with
            // `pending` still VoidPending, so only the async query sees it.
            pipeline_settled: playing && !self.player.has_async_transition(),
            have_media_info: self.player.have_media_info(),
            loading: self.is_loading_media,
            image: self.image_via_player,
            live: self.player.is_live(),
            seekable: self.player.seekable,
            // Every shape of "a seek is outstanding".
            seek_pending: self.player.is_seeking()
                || self.gui_seek_hold.is_some()
                || self.pending_seek_op.is_some()
                || self.gapless_parked_op.is_some(),
            rate: self.player.rate(),
            position: playing.then(|| self.player.get_position()).flatten(),
            duration: self.current_duration,
        };

        let action = self.freeze_watchdog.poll(&sample);
        let pinned_for = match action {
            FreezeAction::None => return Ok(()),
            FreezeAction::Seek { pinned_for }
            | FreezeAction::Reload { pinned_for }
            | FreezeAction::GiveUp { pinned_for } => pinned_for,
        };
        let position = sample.position.unwrap_or(gst::ClockTime::ZERO);
        self.log_freeze_diagnostics(action, position, pinned_for);

        match action {
            FreezeAction::None => (),
            FreezeAction::Seek { .. } => {
                let seqnum = self.player.freeze_recovery_seek();
                self.freeze_watchdog.note_recovery_seek(seqnum);
            }
            FreezeAction::Reload { .. } => {
                let rate = self.player.rate() as f32;
                self.reload_current_item_at(position, rate);
                // Adopt the reload's new item id WITHOUT resetting the per-item cap, or a
                // permanently wedging stream would alternate seek and reload forever.
                self.freeze_watchdog
                    .note_recovery_reload(self.current_media_item_id);
            }
            FreezeAction::GiveUp { .. } => {
                self.media_error(
                    player::MediaErrorKind::Frozen,
                    None,
                    "Playback froze and could not be recovered".to_owned(),
                )?;
            }
        }

        Ok(())
    }

    /// One self-contained diagnostic line. The `.dot` dump only writes when
    /// `GST_DEBUG_DUMP_DOT_DIR` is set.
    fn log_freeze_diagnostics(
        &self,
        action: FreezeAction,
        position: gst::ClockTime,
        pinned_for: Duration,
    ) {
        let (current, pending) = self.player.dbg_state_summary();
        let buffering = self.player.dbg_buffering();
        warn!(
            recovery = ?action,
            stage = self.freeze_watchdog.stage_name(),
            item = self.current_media_item_id,
            source = self.current_source_kind(),
            image = self.image_via_player,
            pinned_for_ms = pinned_for.as_millis(),
            position_s = position.seconds_f64(),
            duration_s = self.current_duration.map(|d| d.seconds_f64()),
            rate = self.player.rate(),
            player_state = ?self.player.player_state(),
            gst_state = ?current,
            gst_pending = ?pending,
            seekable = self.player.seekable,
            seekable_known = self.player.seekable_known,
            live = self.player.is_live(),
            buffering_percent = buffering.as_ref().map(|b| b.percent),
            buffering_busy = buffering.as_ref().map(|b| b.busy),
            buffering_mode = ?buffering.as_ref().map(|b| b.mode),
            // Queues drained empty while downloads park at the input watermark is the
            // wedge's signature.
            buffered_ahead_ms = self.player.buffered_ahead().map(|t| t.mseconds()),
            routed = ?self.player.dbg_routed_summary(),
            unsettled = ?self.player.dbg_unsettled_elements(),
            sources = ?self.player.dbg_sources(),
            video_sink = ?self.player.dbg_video_sink_stats(),
            "FREEZE WATCHDOG: playback position pinned while the receiver believes it is playing"
        );
        self.player
            .dump_dot(&format!("freeze-item{}", self.current_media_item_id));
    }

    /// Coarse identity of what is playing, for diagnostics.
    fn current_source_kind(&self) -> &'static str {
        match self.current_media.as_ref().map(|m| &m.source) {
            Some(MediaSource::Single(_)) => "single",
            Some(MediaSource::Playlist { .. }) => "playlist",
            Some(MediaSource::Queue(_)) => "queue",
            Some(MediaSource::Raop) => "raop",
            Some(MediaSource::AirPlayMirror { .. }) => "airplay-mirror",
            None => "none",
        }
    }

    /// Gapless variant of [`build_media_source`](Self::build_media_source): a
    /// cached item still goes through urisourcebin with its bytes as a
    /// preloaded head, because a prepared input's pads sit
    /// unlinked-and-blocked until the swap and the appsrc bytes source dies
    /// not-negotiated against them.
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

    /// Invalidate a pending gapless pre-arm; a no-op when nothing is pre-armed.
    ///
    /// The bookkeeping is only MARKED here, never dropped: the cancel races the
    /// pipeline's swap and a declined cancel activates regardless, so
    /// dropping it up front leaves that activation unmatched and audibly
    /// replays the finished track.
    ///
    /// `after` tells the crate whether a flushing seek follows. The gapless
    /// output hold has usually already eaten the item's EOS by cancel time
    /// (it crosses decodebin3 up to a video queue depth before the item is
    /// audibly over), and the crate synthesizes that end unless something is
    /// about to regenerate it. A parked seek IS that something, and a
    /// synthesized end there advances the queue instead, turning the user's
    /// scrub-back into a track skip.
    fn cancel_gapless_prearm(&mut self, after: player::AfterCancel) {
        let Some(prearm) = self.gapless_prearm.as_mut() else {
            return;
        };
        // The first cancel's outcome decides for both.
        if prearm.cancelling {
            return;
        }
        prearm.cancelling = true;
        let generation = prearm.generation;
        debug!(generation, ?after, "Cancelling the gapless pre-arm");
        self.player.cancel_prepared(after);
    }

    /// Run an operation that reaches the pipeline as a flushing seek, or park
    /// it until a pending pre-arm's cancellation reports its outcome.
    ///
    /// Invariant: a pending prepare must be cancelled AND the cancellation
    /// confirmed before a flushing seek reaches the pipeline. After the
    /// swap the prepared input is the only linked upstream, so the seek is
    /// answered by the SUCCESSOR's source.
    fn park_or_apply_gapless_op(&mut self, op: GaplessParkedOp) {
        if self.gapless_prearm.is_none() {
            self.apply_gapless_op(op);
            return;
        }
        // Latest intent wins, across kinds too. One slot, never a queue, so a burst of
        // scrubbing cannot pile up work for the outcome.
        let kind = op.kind();
        if let Some(previous) = self.gapless_parked_op.replace(op) {
            debug!(
                replaced = ?previous.kind(),
                ?kind,
                "Replacing the operation parked on the gapless cancel outcome"
            );
        } else {
            debug!(
                ?kind,
                "Parking the operation until the gapless cancel resolves"
            );
        }
        // A seek or a speed change reaches the pipeline AS a flushing seek
        // once the outcome replays it, and that seek regenerates the item's
        // end. A track or subtitle change does not, so its cancel still owes
        // the caller the consumed end.
        let after = match kind {
            GaplessParkedOpKind::Seek | GaplessParkedOpKind::SetSpeed => {
                player::AfterCancel::FlushingSeek
            }
            GaplessParkedOpKind::TrackChange | GaplessParkedOpKind::SubtitleChange => {
                player::AfterCancel::Nothing
            }
        };
        // Idempotent: an already in-flight cancel's outcome resolves this too.
        self.cancel_gapless_prearm(after);
    }

    /// The apply half of
    /// [`park_or_apply_gapless_op`](Self::park_or_apply_gapless_op), shared
    /// with the replay path so the two cannot drift.
    fn apply_gapless_op(&mut self, op: GaplessParkedOp) {
        match op {
            GaplessParkedOp::Seek { origin, time } => self.apply_seek(origin, time),
            // Confirmed by the pipeline's own `RateChanged`.
            GaplessParkedOp::SetSpeed { rate, .. } => self.player.set_rate(rate),
            GaplessParkedOp::TrackChange { kind, sid } => self.apply_track_change(kind, sid),
            GaplessParkedOp::SubtitleChange { origin, target } => {
                self.apply_subtitle_target(origin, target)
            }
        }
    }

    /// Report and clamp a seek target against the known duration, before the
    /// gapless park so the sender's error reply is never delayed by a
    /// cancel round-trip. Known caveat: after a swap `current_duration` can
    /// be the next item's.
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
    /// seekability query resolves; the player would silently drop a seek
    /// issued in that window.
    fn apply_seek(&mut self, origin: PacketOrigin, time: gst::ClockTime) {
        if self.player.seekable_known {
            self.player.seek(time);
            return;
        }
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
                // Rate carries over: a reload resets the pipeline's rate.
                let rate = self.player.rate() as f32;
                self.reload_current_item_at(time, rate);
                true
            }
            (ParkedOpAction::ReloadAtTarget, GaplessParkedOp::SetSpeed { origin, rate }) => {
                // Best effort: the position is the outgoing item's, so this resumes
                // roughly where the speed change was asked for.
                let position = self.player.get_position().unwrap_or(gst::ClockTime::ZERO);
                info!(
                    ?origin,
                    rate,
                    ?position,
                    "Gapless: the swap already performed, reloading the item at the new speed"
                );
                self.reload_current_item_at(position, rate);
                // The load applies the rate as its start seek, which emits no `RateChanged`,
                // so confirm here or the sender waits for an ack that never comes.
                if let Err(err) = self.broadcast_rate(rate) {
                    warn!(?err, "Failed to relay the rate after a gapless reload");
                }
                true
            }
            // `parked_op_action` never pairs `ReloadAtTarget` with a track change, so
            // folding the two arms keeps this total without a panicking arm.
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

    /// Reload the current item at `position`/`rate`, standing in for an
    /// operation the pipeline can no longer serve after a gapless swap
    /// unlinked this item's input.
    fn reload_current_item_at(&mut self, position: gst::ClockTime, rate: f32) {
        // The load supersedes the in-flight activation. The queue index is left alone:
        // the advance only happens on adoption, so "current" is still the playing item.
        self.gapless_prearm = None;
        self.player.clear_pending_gapless();
        self.held_prearm_events.clear();
        // Carried across `cleanup_playback_data`, which would otherwise drop the hold
        // and clear the GUI's `seek-pending` flag even though this load lands
        // on the target.
        let seek_hold = self.gui_seek_hold.take();
        // The start point rides the load (applied in PAUSED) rather than being seeked
        // in afterwards, which would render a 1.0x slice the seek then flushes.
        self.load_start_override = Some(player::RestorePoint { position, rate });
        self.load_media();
        self.gui_seek_hold = seek_hold;
        // `Player::load` resets the tracked rate to 1.0 and the start seek emits no
        // `RateChanged`, so restate it or later speed requests compare against the
        // wrong value.
        self.player.set_rate_changed(rate as f64);
    }

    /// Where the pre-armed item sits now: a queue mutation may have shifted or
    /// dropped it, so the armed index is only a hint and the URL is the
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

    /// Roll the application onto a gapless activation: everything
    /// `play_queue_item` does except the load. The index is a parameter,
    /// not read from the pre-arm, because a declined cancel's activation
    /// can arrive after the queue moved.
    fn adopt_gapless_activation(&mut self, generation: u64, next_index: usize) {
        // Consumed either way: an activation the player refuses never comes back.
        self.gapless_prearm = None;
        // A parked operation belongs to the item that just retired; applying it to the
        // new one is a real bug class, so drop it even though callers resolve
        // it first.
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

        // Per-item view state rolls like a fresh load; the new item's collection
        // follows and re-runs media_loaded_successfully through the
        // have_media_info gate.
        self.current_media_item_id += 1;
        self.recent_warnings.clear();
        self.have_media_info = false;
        self.current_duration = None;
        self.inspector_container = None;
        self.inspector_image = String::new();
        self.have_media_title = title.is_some();
        // The gapless path skips cleanup_playback_data, so the labels roll
        // here too: a titleless item must not keep the retired item's title,
        // and the artist only ever comes from Tags, so it clears either way
        // and refreshes if the new item carries any.
        self.gui.set_media_title(title.unwrap_or_default());
        self.last_artist_name = None;
        self.gui.set_artist_name(String::new());

        // The gapless path bypasses the normal load, so refresh the audio cover here or
        // the previous track's thumbnail lingers and the Tags handler ignores new art.
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
            self.gui.clear_audio_covers();
        }

        if self.should_broadcast() {
            self.broadcast_update(ReceiverToSenderMessage::V4(fcast::V4Message::Broadcast {
                serialized_msg: fcast_protocol::v4::MessageBuilder::new()
                    .queue_select(v4::QueuePosition::Index(next_index as u8)),
            }));
        }
        self.sync_queue_cache();

        // Replay what the new generation emitted inside the held window, in
        // arrival order, now that the filter accepts it. Each event re-passes
        // the filter, so anything held for a generation that is no longer
        // current drops there. Selections stay parked for the collection that
        // follows this activation (see `held_replay_phase`); re-parking them
        // BEFORE the replay keeps them reachable to a held collection's own
        // drain.
        let held = std::mem::take(&mut self.held_prearm_events);
        let (deferred, now): (Vec<_>, Vec<_>) = held
            .into_iter()
            .partition(|(event, _)| held_replay_phase(event) == HeldReplayPhase::FirstCollection);
        self.held_prearm_events = deferred;
        self.replay_held_prearm_events(now);
    }

    /// Feed held events back through the filter, in arrival order.
    fn replay_held_prearm_events(&mut self, events: Vec<(player::PlayerEvent, Option<u64>)>) {
        for (event, generation) in events {
            if let Err(err) = self.handle_player_event(event, generation) {
                warn!(?err, "replaying a held pre-arm event failed");
            }
        }
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
                        // Adaptive manifests must stream live: never prefetch them.
                        .filter(|item| queue_cache::cacheable_container(&item.content_type))
                        .map(|item| queue_cache::PrefetchSpec {
                            url: item.url.clone(),
                            headers: item.headers.clone(),
                        })
                        .collect();
                // Retained but never fetched, so flipping to a neighbor and back does not
                // re-download it.
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
        self.cancel_gapless_prearm(player::AfterCancel::Nothing);

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
        self.cancel_gapless_prearm(player::AfterCancel::Nothing);

        if let Some(relay_msg) =
            fcast_protocol::v4::MessageBuilder::new().from_queue_insert_stripped(insert)
        {
            self.relay_to_other_senders(origin, relay_msg);
        }

        self.sync_queue_cache();
    }

    fn pause(&mut self) {
        // A pause landing mid-load is recorded as desired transport and committed at
        // preroll.
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
                        // The spec caps a queue at 256 items: wire positions are ubytes,
                        // and the u8 bookkeeping would wrap back to item 0.
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
                self.relay_to_other_senders(
                    origin,
                    fcast_protocol::v4::MessageBuilder::new().stop_playback(),
                );
            }
            Operation::Seek(time) => {
                if self.is_playing() {
                    // Range-check first so a park cannot delay the sender's error reply.
                    let time = self.clamp_seek_target(origin, time);
                    self.arm_seek_quiet();
                    // A flushing seek must not reach the pipeline before a pre-arm
                    // cancellation is confirmed.
                    self.park_or_apply_gapless_op(GaplessParkedOp::Seek { origin, time });
                }
            }
            Operation::SetSpeed(rate) => {
                // An idempotent set emits no RateChanged, but the sender still expects a
                // confirmation, so confirm it directly here.
                if (self.player.rate() - rate as f64).abs() < 1e-9 {
                    debug!(rate, "Speed unchanged; re-emitting the confirmation");
                    self.broadcast_rate(rate)?;
                } else {
                    // A real rate change IS a flushing seek: same park as `Seek` above.
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
                // A live object, so it cannot travel through a URI.
                self.pending_fwebrtc_channel = Some(chan);
                let play_message = v3::PlayMessage {
                    container: "application/x-fwebrtc".to_owned(),
                    // Placeholder: the fwebrtc source ignores the URL.
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

                // Subtitles have their own path: an id can name an external subtitle,
                // a virtual track absent from `Player::streams`.
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

                // The wire speaks indices into the advertised stream list, the pipeline
                // speaks stream ids.
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

                let kind = match typ {
                    v4::flat::MediaTrackType::Video => player::TrackKind::Video,
                    v4::flat::MediaTrackType::Audio => player::TrackKind::Audio,
                    _ => unreachable!(),
                };
                // An audio switch is a flushing seek in the selection engine, so it carries
                // the same post-swap hazard as `Seek`; video/subtitle ride along because a
                // selection resolved against the retired item is wrong for the successor.
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
    /// bound port.
    fn update_connection_details(&mut self) -> Result<()> {
        if !self.port_committed {
            // Never advertise a QR for a port that is not bound yet.
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
            let module_count = (dims * dims) as usize;
            let dark = qrcode.data[0..module_count]
                .iter()
                .map(|module| *module != fast_qr::Module::LIGHT)
                .collect();

            self.gui
                .set_connection_details(crate::ui_types::QrCode { size: dims, dark }, ips_string);
        }

        Ok(())
    }

    fn on_media_info_updated(&mut self) {
        self.maybe_apply_pending_subtitle_adds();

        // Only replays a sender seek that raced the load; the start position/rate is
        // applied inside `flapjack::load`.
        self.maybe_apply_pending_seek();
    }

    /// Map a selected subtitle stream id to the wire id senders should see: an
    /// external's STABLE catalog id, otherwise the stream's advertised
    /// index.
    fn advertised_subtitle_id(&self, subtitle_sid: Option<&str>) -> Option<u32> {
        let sid = subtitle_sid?;
        // The catalog comes first: an external is never advertised under its list
        // position, so a relayed selection would otherwise name an id no
        // TracksAvailable carried.
        if let Some(id) = self
            .current_media
            .as_ref()
            .and_then(|m| m.externals.id_of_stream(sid))
        {
            return Some(id);
        }
        self.player.stream_idx_by_id(sid)
    }

    /// Bounds the combined wait of an in-flight load completing and the
    /// seekability query resolving, each of which can take over 10s on a
    /// slow preroll.
    const PENDING_SUBTITLE_ADD_TIMEOUT: Duration = Duration::from_secs(20);

    /// Handle `AddSubtitleSource`, parking it rather than rejecting it while
    /// the load is in flight or the seekability query is unresolved.
    fn add_subtitle_source(
        &mut self,
        origin: PacketOrigin,
        url: String,
        select: bool,
        name: Option<SmolStr>,
    ) -> Result<bool> {
        debug!(url, select, ?name, "adding external subtitle source");

        // Requires an active, non-live, seekable, fully loaded item. Only an
        // incompatible source is a genuine rejection; the rest is parked until
        // answerable.
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
            // Deliberately before the liveness/seekability checks and the pre-arm cancel:
            // mid-load neither property is known, and the replay evaluates all of it.
            debug!("Parking the subtitle source until the in-flight load completes");
            self.park_pending_subtitle_add(url, select, name, origin);
            return Ok(false);
        }
        if self.player.is_live() {
            error!("Cannot add a subtitle source to a live stream");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }
        // An external subtitle makes the item gapless-ineligible: a swap would leak
        // this item's sub into the next item's collections.
        self.cancel_gapless_prearm(player::AfterCancel::Nothing);
        if !self.player.seekable {
            if !self.player.seekable_known {
                // Not unseekable, just not answerable yet.
                debug!("Parking the subtitle source until the seekability query resolves");
                self.park_pending_subtitle_add(url, select, name, origin);
                return Ok(false);
            }
            error!("Cannot add a subtitle source to an unseekable stream");
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        }

        // The sender's URL is attached as-is; `rssubparse` decides the charset from
        // whole-stream evidence, so no transcoding pre-pass is needed.
        let source_url = url.clone();

        // Every catalog external is a LIVE input attached simultaneously, so switching
        // is pure stream selection. A genuine failure arrives as
        // `ExternalSubtitleFailed`.
        let handle = self.player.attach_external_subtitle(&url);
        let Some(media) = self.current_media.as_mut() else {
            self.player.detach_external_subtitle(handle);
            self.send_error(origin, ErrorKind::InvalidState);
            return Ok(false);
        };
        // One assignment site, one id per entry, for the life of the item.
        let id = media.externals.attach(source_url, name, origin, handle);
        if select {
            self.player.request_external_subtitle(handle);
        } else {
            // Pin what is showing NOW as the explicit desire: decodebin3 may auto-select
            // the fresh text stream, and an unset desire would adopt it.
            let current = self.player.current_subtitle_sid().map(str::to_string);
            self.apply_track_change(player::TrackKind::Subtitle, current);
        }

        debug!(id, select, "Attached external subtitle input (live)");
        self.update_tracks(true);
        Ok(false)
    }

    /// Park an `AddSubtitleSource` and arm the timer bounding the wait. The
    /// timer carries the current epoch and item, so a drained list makes
    /// the check a no-op.
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

    /// Replay parked `AddSubtitleSource` ops; a no-op until the load and the
    /// seekability query have settled, or the replay would re-enter
    /// mid-load and just park again.
    fn maybe_apply_pending_subtitle_adds(&mut self) {
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

    /// Drop parked subtitle adds, rejecting them to their senders.
    fn reject_pending_subtitle_adds(&mut self) {
        if self.pending_subtitle_adds.is_empty() {
            return;
        }
        self.pending_subtitle_add_epoch += 1;
        for add in std::mem::take(&mut self.pending_subtitle_adds) {
            self.send_error(add.origin, ErrorKind::InvalidState);
        }
    }

    /// How long a parked `Seek` waits for the seekability query before it is
    /// dropped.
    const PENDING_SEEK_TIMEOUT: Duration = Duration::from_secs(10);

    /// Kept below FAST's 16s confirm window so a stalled load is captured
    /// before the sender gives up and tears it down.
    const LOAD_STALL_TIMEOUT: Duration = Duration::from_secs(12);

    /// Apply a `Seek` parked while the seekability query was unresolved.
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

    /// Drop a parked seek without applying it.
    fn drop_pending_seek(&mut self) {
        if self.pending_seek_op.take().is_some() {
            self.pending_seek_epoch += 1;
        }
    }

    /// Drop any "server busy" countdown (new load, stop, or the load
    /// prerolled). Safe to call blindly, repeated clears are swallowed.
    fn clear_source_backoff(&mut self) {
        self.source_backoff_epoch += 1;
        self.source_backoff = None;
        self.gui.set_source_backoff(0, 0);
    }

    /// Whether a server-directed backoff deserves the "server busy" countdown:
    /// only when the pipeline is low on buffered media, so the wait may
    /// actually interrupt playback. Live sources at their edge are directed to
    /// back off routinely while holding many seconds of runway; those waits
    /// are pacing, not trouble. When no element can report a level, err on
    /// showing.
    fn source_backoff_worth_showing(&self) -> bool {
        const RUNWAY_THRESHOLD: gst::ClockTime = gst::ClockTime::from_seconds(3);
        match self.player.buffered_ahead() {
            Some(ahead) => ahead < RUNWAY_THRESHOLD,
            None => true,
        }
    }

    /// Resolve a wire subtitle track id: ids `>= EXTERNAL_TRACK_ID_BASE` name a
    /// catalog entry, smaller ids are `Player::streams` indices, `None` is
    /// "off".
    fn resolve_subtitle_target(&self, id: Option<u32>) -> Result<SubtitleTarget, ErrorKind> {
        let Some(id) = id else {
            return Ok(SubtitleTarget::Stream(None));
        };
        if is_external_track_id(id) {
            let entry_sid = match self
                .current_media
                .as_ref()
                .and_then(|m| m.externals.by_id(id))
            {
                Some(entry) => self.advertised_external_sid(entry),
                None => return Err(ErrorKind::MalformedBody),
            };
            // Once advertised it is a plain stream selection; before that it stays an
            // `External` target parked as the desired end state.
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

    /// Shared subtitle-change path for the protocol `ChangeTrack` and the GUI
    /// `SelectTrack`. Validation happens here; enacting goes through the
    /// gapless gate.
    fn change_subtitle_track(&mut self, origin: PacketOrigin, id: Option<u32>) {
        let target = match self.resolve_subtitle_target(id) {
            Ok(t) => t,
            Err(kind) => {
                error!(?id, "Invalid subtitle track id");
                self.send_error(origin, kind);
                return;
            }
        };

        // playsink cannot present text without video: selecting a subtitle while video
        // is deselected would error the pipeline or be dropped, so report it
        // unsatisfiable.
        let selecting_something = !matches!(target, SubtitleTarget::Stream(None)) && id.is_some();
        if selecting_something && self.player.current_video_sid().is_none() {
            error!("Cannot select a subtitle track while video is disabled");
            self.send_error(origin, ErrorKind::InvalidState);
            return;
        }

        // `target` is fully validated, so it is safe to hold across a cancel
        // round-trip.
        self.park_or_apply_gapless_op(GaplessParkedOp::SubtitleChange { origin, target });
    }

    /// Enact a validated subtitle target, shared with the gapless replay path.
    fn apply_subtitle_target(&mut self, origin: PacketOrigin, target: SubtitleTarget) {
        match target {
            SubtitleTarget::External(ext_id) => {
                // Not materialized yet: the engine parks the desire and applies it when
                // the stream appears, and the selection confirm relays TracksSelected.
                let handle = self
                    .current_media
                    .as_ref()
                    .and_then(|m| m.externals.by_id(ext_id))
                    .map(|s| s.handle);
                match handle {
                    Some(handle) => {
                        debug!(ext_id, "Requesting the external subtitle from the engine");
                        self.player.request_external_subtitle(handle);
                    }
                    // Only a racing removal gets here.
                    None => self.send_error(origin, ErrorKind::MalformedBody),
                }
            }
            SubtitleTarget::Stream(stream_sid) => {
                // Safe to apply while paused: flapjack flushes the blocked push before
                // unlinking text, so the deselect cannot deadlock waiting for data.
                self.apply_track_change(player::TrackKind::Subtitle, stream_sid);
            }
        }
    }

    /// Apply a track change through TrackOps. Whether the switch's re-emit
    /// flush is safe is decided inside the player's pump, off the
    /// pipeline's own input state.
    fn apply_track_change(&mut self, kind: player::TrackKind, sid: Option<player::StreamId>) {
        if self.player.request_track_change(kind, sid) {
            // The displayed cue belongs to the previous track; clear it even while paused.
            self.gui.clear_video_overlays();
        }
    }

    /// A catalog external's stream id, but only once decodebin3 advertises that
    /// stream.
    fn advertised_external_sid(&self, entry: &ExternalSubtitle) -> Option<player::StreamId> {
        entry
            .stream_sid
            .clone()
            .filter(|sid| self.player.stream_idx_by_id(sid).is_some())
    }

    /// Learn the stream ids of newly materialized externals; must run before
    /// anything maps externals for that collection.
    fn refresh_external_stream_sids(&mut self) {
        let Some(media) = self.current_media.as_mut() else {
            return;
        };
        let learned = media
            .externals
            .learn_stream_sids(|handle| self.player.external_stream_sid_of(handle));
        for (id, sid) in learned {
            debug!(id, sid, "external subtitle stream materialized");
        }
    }

    /// Retire an external subtitle flapjack already detached; playback is
    /// untouched.
    fn fail_fcast_external_subtitle(&mut self, ext_id: u32) {
        let Some(media) = self.current_media.as_mut() else {
            return;
        };
        // Survivors keep their advertised ids; this one stays retired.
        let Some(failed) = media.externals.remove(ext_id) else {
            return;
        };
        warn!(url = failed.url, "External subtitle failed; removing it");
        self.send_error(failed.requested_by, ErrorKind::ResourceNotFound);
        self.update_tracks(true);
    }

    fn update_tracks(&mut self, force_update: bool) {
        if !force_update && !self.player.update_stream_properties() {
            return;
        }

        // Externals are advertised by STABLE id, in catalog order, after the embedded
        // tracks, so the advertised order is fixed as the selection changes.
        // Materialized ones are skipped in the stream loops so they are never
        // advertised twice.
        let external_stream_idxs: Vec<u32> = self
            .current_media
            .as_ref()
            .map(|m| {
                m.externals
                    .materialized_sids()
                    .filter_map(|sid| self.player.stream_idx_by_id(sid))
                    .collect()
            })
            .unwrap_or_default();
        let externals: Vec<(u32, Option<SmolStr>)> = self
            .current_media
            .as_ref()
            .map(|m| m.externals.iter().map(|s| (s.id, s.name.clone())).collect())
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
                    name: stream.title.to_string(),
                });
            }
        }
        for (id, name) in &externals {
            subtitles.push(UiMediaTrack {
                id: *id as i32,
                name: name
                    .as_ref()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| SmolStr::new_inline("External").to_string()),
            });
        }

        self.gui.set_tracks(videos, audios, subtitles);
    }

    /// Whether an event must be dropped when its generation is not the current
    /// one. The exceptions are not item-scoped at all. `StateChanged` IS
    /// load-scoped: a superseded item's teardown edges would otherwise walk
    /// the state machine through a queued load.
    fn player_event_is_load_scoped(event: &player::PlayerEvent) -> bool {
        !matches!(
            event,
            player::PlayerEvent::VolumeChanged(_)
                | player::PlayerEvent::RequestState(_)
                | player::PlayerEvent::ClockLost
                | player::PlayerEvent::StreamTagsUpdated
                // Carry the PREPARED (future) generation and are validated against the
                // pre-arm bookkeeping, not against the current load.
                | player::PlayerEvent::GaplessActivated
                | player::PlayerEvent::GaplessCancelled { .. }
                | player::PlayerEvent::GaplessCancelDeclined { .. }
        )
    }

    /// Whether an event describes the pipeline rather than the item playing
    /// out of it. Both transport edges are pipeline-wide, and there is exactly
    /// one pipeline across a gapless boundary, so the pending pre-arm's
    /// generation on them is a future ATTRIBUTION, not a future event.
    fn player_event_is_pipeline_scoped(event: &player::PlayerEvent) -> bool {
        matches!(
            event,
            player::PlayerEvent::StateChanged { .. } | player::PlayerEvent::AsyncDone
        )
    }

    fn handle_player_event(
        &mut self,
        event: player::PlayerEvent,
        generation: Option<u64>,
    ) -> Result<()> {
        // Exact supersession: every load-scoped event carries its load's generation, so
        // events from a superseded or stopped load are dropped here in one place.
        if let Some(generation) = generation
            && Self::player_event_is_load_scoped(&event)
            && !self.player.is_event_current(generation)
        {
            match stale_event_action(
                generation,
                self.player.pending_gapless_generation(),
                Self::player_event_is_pipeline_scoped(&event),
            ) {
                // Falls through to the handler below: per-item misattribution here
                // is transient and the adoption re-seeds duration/seekability.
                StaleEventAction::Apply => {
                    debug!(
                        generation,
                        "Applying a pipeline-scoped event from the pending pre-arm"
                    );
                }
                StaleEventAction::Hold => {
                    if self.held_prearm_events.len() >= HELD_PREARM_EVENTS_MAX {
                        warn!(generation, "held pre-arm buffer full; dropping the event");
                    } else {
                        self.held_prearm_events.push((event, Some(generation)));
                    }
                    return Ok(());
                }
                StaleEventAction::Drop => {
                    debug!(generation, "Dropping player event from a superseded load");
                    return Ok(());
                }
            }
        }
        match event {
            player::PlayerEvent::EndOfStream => {
                // Resolved before the cancel so a parked operation cannot survive into the
                // next item; the advance below owns the transition.
                self.resolve_parked_gapless_op(GaplessOutcome::ItemEnded);
                self.cancel_gapless_prearm(player::AfterCancel::Nothing);

                self.player.end_of_stream_reached();

                debug!("Player reached EOS");

                // A seek to the end lands here, not on a transport settle.
                self.seek_quiet = false;

                self.media_ended();

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

                self.player.update_media_info();
                self.on_media_info_updated();

                self.gui.set_app_state(AppState::Playing);

                // NO transport driving here: `Player::uri_loaded` is the one post-load
                // transport driver, or a mid-load pause gets stomped and a live subtitle
                // attach's collection un-pauses a paused pipeline.

                self.refresh_external_stream_sids();

                self.update_tracks(true);

                if !self.have_media_info {
                    self.media_loaded_successfully();
                    self.have_media_info = true;
                }

                // Retried now that `is_loading_media` is clear; covers the order where
                // seekability resolved before this collection.
                self.maybe_apply_pending_subtitle_adds();

                // The stream list is installed, so anything a gapless adoption parked
                // for it can resolve now. A collection arriving while the pre-arm is
                // still pending re-parks them through the filter, in order, so an
                // early drain is a no-op.
                if !self.held_prearm_events.is_empty() {
                    let deferred = std::mem::take(&mut self.held_prearm_events);
                    self.replay_held_prearm_events(deferred);
                }
            }
            player::PlayerEvent::AsyncDone => {
                self.player.async_done();

                if self.player.have_media_info()
                    && self.player.player_state() != PlayerState::Playing
                {
                    self.playback_progress_changed();
                }
            }
            player::PlayerEvent::DurationChanged => {
                // A push-mode demuxer announces an approximate duration up front and
                // refines it as it plays, so the cache has to be refreshed here.
                // A pipeline image produces no progress traffic and must not start now.
                if self.image_via_player {
                    return Ok(());
                }
                match cacheable_duration(self.player.get_duration()) {
                    Some(duration) => {
                        debug!(?duration, "Duration refined mid-item");
                        self.current_duration = Some(duration);
                        // Held back mid-load/mid-seek: the position read would be transient
                        // and would fight the seek hold. The next tick reports the cache.
                        if self.player.have_media_info() && !self.player.is_seeking() {
                            self.playback_progress_changed();
                        }
                    }
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
                // The only duration writer that deliberately overwrites: preroll/resume is
                // where the pipeline first has a real answer. A gapless swap produces
                // neither edge, hence the `DurationChanged` handler and the activation reset.
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

                // Track work dispatches LAST: a selection interleaved with the start seek
                // reconfigures playsink outside steady PLAYING, an observed permanent wedge.
                self.player.poll_track_ops();
            }
            player::PlayerEvent::UriLoaded => {
                if !self.is_playing() {
                    debug!("Ignoring stale UriLoaded (nothing is loaded)");
                    return Ok(());
                }
                self.player.uri_loaded();
                // Preroll finished, so data arrived: any countdown is stale.
                self.clear_source_backoff();
            }
            player::PlayerEvent::RequestState(state) => self.player.request_state(state),
            player::PlayerEvent::QueueSeek(seek) => self.player.queue_seek(seek),
            player::PlayerEvent::SubtitleRefreshFailed { seqnum } => {
                // The freeze watchdog's recovery seek rides the same job; a refusal means
                // only the escalation can recover.
                if self.freeze_watchdog.is_recovery_seek(seqnum) {
                    warn!("FREEZE WATCHDOG: the recovery seek was refused by the pipeline");
                }
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
                // Covers engine-initiated switches too, whose dispatch the application
                // never sees and which leave a stale cue on screen.
                if selected.subtitle.as_deref() != prev_subtitle.as_deref() {
                    self.gui.clear_video_overlays();
                }
                // The wire/GUI edge: applied stream ids map back to advertised indices.
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
                detail: codec_detail,
                message,
                failed_uri,
            } => {
                // Attribution comes from flapjack's generation-tagged inputs.
                // `failed_uri` is not just diagnostic: `kind` was classified from
                // its presence, and its host is the network-failure detail below.
                // External subtitles never error here.
                match err_origin {
                    flapjack::ErrorOrigin::Stale => {
                        debug!(?failed_uri, message, "Dropping error from a stale input");
                    }
                    flapjack::ErrorOrigin::Main | flapjack::ErrorOrigin::Unknown => {
                        self.player.stop();
                        if let Some(origin) = self.current_media.as_ref().map(|m| m.origin) {
                            self.send_error(origin, media_error_kind_to_error(kind));
                        }
                        let detail = match kind {
                            player::MediaErrorKind::NetworkFailure => {
                                failed_uri.as_deref().and_then(uri_host).map(str::to_owned)
                            }
                            player::MediaErrorKind::MissingCodec => codec_detail,
                            _ => None,
                        };
                        let mut diagnostic = message;
                        if let Some(uri) = &failed_uri {
                            diagnostic.push_str(&format!(" (uri {})", strip_uri_query(uri)));
                        }
                        self.media_error(kind, detail, diagnostic)?;
                    }
                }
            }
            player::PlayerEvent::ExternalSubtitleFailed { id } => {
                // flapjack already detached the input; only the protocol side is left.
                let ext_id = self
                    .current_media
                    .as_ref()
                    .and_then(|m| m.externals.id_of_handle(id));
                match ext_id {
                    Some(ext_id) => self.fail_fcast_external_subtitle(ext_id),
                    None => debug!(?id, "Failure report for an unknown external subtitle"),
                }
            }
            player::PlayerEvent::SubtitleTrackUnsupported { sid, caps } => {
                // Deliberately at error level: a selected-but-unrenderable track is a
                // capability gap, and the field log has to name the caps.
                error!(
                    sid,
                    caps, "The selected subtitle track cannot be rendered; showing nothing"
                );
                // Best effort: `media_warning` suppresses itself while not playing.
                let message = format!("Unsupported subtitle format: {caps}");
                self.media_warning(
                    player::MediaWarningKind::SubtitleFormatUnsupported,
                    Some(caps),
                    message,
                )?;
            }
            player::PlayerEvent::Warning {
                kind,
                detail,
                message,
            } => {
                self.media_warning(kind, detail, message)?;
            }
            player::PlayerEvent::StreamTagsUpdated => {
                self.update_tracks(false);
            }
            player::PlayerEvent::SourceBackoff { remaining_ms } => {
                self.source_backoff_epoch += 1;
                if remaining_ms == 0 {
                    self.source_backoff = None;
                    self.gui.set_source_backoff(0, 0);
                } else {
                    debug!(remaining_ms, "Source entered a server-directed backoff");
                    let deadline = Instant::now() + Duration::from_millis(remaining_ms);
                    self.source_backoff = Some((deadline, remaining_ms));
                    // Only shown while low on buffered media: a live source at
                    // its edge gets routine pacing backoffs while holding many
                    // seconds of runway, and showing those reads as trouble
                    // where there is none. The ticks below re-check, so a
                    // backoff that outlives the buffer surfaces the moment
                    // the runway runs low.
                    if self.source_backoff_worth_showing() {
                        self.gui.set_source_backoff(remaining_ms, remaining_ms);
                    }
                    // Drive the countdown; a stale epoch makes the ticks no-ops.
                    let epoch = self.source_backoff_epoch;
                    let msg_tx = self.msg_tx.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            msg_tx.send(Message::SourceBackoffTick { epoch });
                            if Instant::now() >= deadline {
                                break;
                            }
                        }
                    });
                }
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
                    // A generation AHEAD of the player means the pipeline switched while
                    // the application had no pre-arm: reload so the two agree again.
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
                        self.held_prearm_events.clear();
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

                // The activation can beat the decline report to the loop (different
                // threads), so a parked operation is resolved here against the same state.
                if self.resolve_parked_gapless_op(GaplessOutcome::SwapPerformed) {
                    return Ok(());
                }

                // The cancel lost the race and the prepared item IS playing: adopt it
                // rather than resync-reloading the item that just finished. A queue
                // mutation may have moved it, so re-locate it first.
                match self.prepared_queue_index(next_index, &url) {
                    Some(index) => {
                        debug!(
                            generation,
                            index, "Gapless: adopting the activation of a declined cancel"
                        );
                        self.adopt_gapless_activation(generation, index);
                    }
                    None => {
                        // The activated item was removed from the queue, so there is no
                        // slot to advance to: hand the boundary to the end-of-stream path.
                        warn!(
                            generation,
                            url, "Gapless: the activated item is gone from the queue"
                        );
                        // Keep the player's view honest (the generation IS live) first.
                        self.player.adopt_gapless_generation(generation);
                        self.gapless_prearm = None;
                        // The item is being treated as ended, not adopted for
                        // playback, so its held events describe nothing.
                        self.held_prearm_events.clear();
                        self.player.end_of_stream_reached();
                        self.media_ended();
                        // Senders see the same shape as an ordinary end-of-stream advance.
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
                // Also the exit for a prepare failing while a cancel is in flight: nothing
                // will activate, so a `cancelling` pre-arm must be dropped here too.
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
                    self.held_prearm_events.clear();
                    self.resolve_parked_gapless_op(GaplessOutcome::PrepareGone);
                }
            }
            player::PlayerEvent::GaplessCancelled { generation } => {
                let armed = self.gapless_prearm.as_ref().map(|prearm| prearm.generation);
                let cancelling = self
                    .gapless_prearm
                    .as_ref()
                    .is_some_and(|prearm| prearm.cancelling);
                match cancel_report(generation, armed, cancelling) {
                    // The prepare is gone and no activation follows, so the bookkeeping
                    // the cancel kept alive can finally be dropped.
                    CancelReport::PrepareGone => {
                        if generation != armed {
                            warn!(
                                ?generation,
                                ?armed,
                                "Gapless cancellation confirmed for an unexpected generation"
                            );
                        } else {
                            debug!(?generation, "Gapless pre-arm cancellation confirmed");
                        }
                        self.gapless_prearm = None;
                        self.player.clear_pending_gapless();
                        self.held_prearm_events.clear();
                        self.resolve_parked_gapless_op(GaplessOutcome::PrepareGone);
                    }
                    // Same shape as a declined cancel: the pre-arm and its held events
                    // stay for the activation handler to adopt.
                    CancelReport::SwapPerformed => {
                        info!(
                            ?armed,
                            "Gapless cancellation confirmed after the swap; the activation stands"
                        );
                        self.resolve_parked_gapless_op(GaplessOutcome::SwapPerformed);
                    }
                    // A pre-arm with no cancel in flight is not this cancel's, and
                    // nothing can be parked without a `cancelling` pre-arm.
                    CancelReport::Straggler => debug!(
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
                    // The prepared item goes live regardless. With nothing parked the
                    // pre-arm is kept for the activation handler to adopt; a parked seek or
                    // speed change instead reloads, since this item's input is unlinked.
                    info!(
                        generation,
                        "Gapless cancellation declined (the swap already performed)"
                    );
                    self.resolve_parked_gapless_op(GaplessOutcome::SwapPerformed);
                } else {
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
                                Ok((stream, _)) => msg_tx.airplay(AirPlay::SenderConnected(stream)),
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
                let source = match media_source::build_airplay_mirror_source(&uri) {
                    Ok(element) => player::MediaInput::Element(element),
                    Err(err) => {
                        error!(?err, "Failed to build the AirPlay mirror source");
                        player::MediaInput::Uri(uri)
                    }
                };
                // No start seek: a mirror stream is live.
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
                    .set_updater_state(crate::ui_types::UiUpdaterState::ShowingDialog);
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
                                let error_msg = err.to_string();
                                let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                                    crate::ui_types::UiUpdaterState::DownloadFailed,
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
                            Box::new({
                                let gui_tx = gui_tx.clone();
                                move |closure| {
                                    gui_tx
                                        .send(gui::UpdateGuiCommand::RunOnMainThread(
                                            closure.into(),
                                        ))
                                        .is_err()
                                }
                            }),
                        )
                        .await
                        {
                            error!(?err, "Failed to install update");
                            let error_msg = err.to_string();
                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                                crate::ui_types::UiUpdaterState::InstallFailed,
                            ));
                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdaterError(error_msg));
                            return;
                        }

                        debug!(?update, "Successfully updated");

                        let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                            crate::ui_types::UiUpdaterState::InstallSuccessful,
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
                        self.media_error(
                            player::MediaErrorKind::ImageDownloadFailed,
                            None,
                            format!("Image download failed: {err:?}"),
                        )?;
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

        // The callback runs on the player worker, so it only hands layout off to the
        // blocking pool.
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

    /// One inspector sample: bitrates, tracks, container, sinks and internals,
    /// in one command.
    fn inspector_tick(&mut self) {
        if !self.inspector_active {
            return;
        }
        let stats = self.player.stream_io_stats();

        // Tapped stream ids match the collection's for parsed containers; a sid-less
        // input falls back to the first tap of the right caps kind.
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

    /// The buffering card's data; `None` when the source can't answer a
    /// buffering query.
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

    /// The sources card's lines: one per live input.
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

    /// The internals card's lines: state, routing, externals, unsettled
    /// elements.
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
            for ext in media.externals.iter() {
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

    /// The sink card's lines: video QoS counters and audio format/counters.
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
            // Only meaningful while `resolve_listen_port` awaits a choice.
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

                // A queue item's playback_duration elapsed: the spec's autoplay trigger.
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

                // Subtitles share the protocol ChangeTrack path (ids may name an external).
                if matches!(variant, player::TrackKind::Subtitle) {
                    self.change_subtitle_track(PacketOrigin::Gui, wire_id);
                    return Ok(false);
                }

                // GUI ids index our own advertised list; a stale one resolves to None.
                let sid = wire_id.and_then(|i| self.player.stream_id_of(i));

                let kind = variant;
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
                // Epoch mismatch means the parked list was already drained.
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
            Message::SeekQuietTimeout { epoch } => {
                // Still unsettled after the debounce: v4 gets Buffering,
                // v1-v3 have no such state and stay silent.
                if epoch == self.seek_quiet_epoch && self.seek_quiet && self.should_broadcast() {
                    self.broadcast_update(ReceiverToSenderMessage::V4(
                        fcast::V4Message::PlaybackStateChanged(
                            fcast_protocol::v4::PlaybackState::Buffering,
                        ),
                    ));
                }
            }
            Message::LoadStallCheck { item, epoch } => {
                // Diagnostic only; a slow-but-progressing preroll can also trip it, and
                // the dumped collection-vs-routed tells the two apart.
                if epoch == self.load_watchdog_epoch
                    && item == self.current_media_item_id
                    && !self.player.is_pipeline_stable()
                {
                    self.player
                        .log_load_stall_diagnostics(&format!("item{item}"));
                }
            }
            Message::SourceBackoffTick { epoch } => {
                if epoch == self.source_backoff_epoch
                    && let Some((deadline, total_ms)) = self.source_backoff
                {
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u64;
                    // Once shown, keep counting down (recovering runway must
                    // not freeze the bar); otherwise show only when the
                    // runway has run low. A clear is always delivered (and
                    // swallowed by the GUI when nothing is shown).
                    if remaining == 0
                        || self.gui.source_backoff_shown()
                        || self.source_backoff_worth_showing()
                    {
                        self.gui.set_source_backoff(remaining, total_ms);
                    }
                    if remaining == 0 {
                        self.source_backoff = None;
                    }
                }
            }
            Message::Raop(event) => return self.handle_raop_event(event),
            #[cfg(feature = "airplay")]
            Message::AirPlay(event) => return self.handle_airplay_event(event),
            Message::InspectorActive(active) => {
                self.inspector_active = active;
                if !active {
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
            let initial_volume = self.player.volume();
            async move {
                if let Err(err) = SessionDriver::new(
                    stream,
                    id,
                    tls_acceptor,
                    companion_ctx,
                    comp_tx,
                    receiver_info,
                    initial_v4_state,
                    initial_volume,
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

    /// Bind the FCast listening socket(s); `port == 0` requests an ephemeral
    /// port. Later address families are pinned to the first's port so one
    /// number can be advertised.
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

    /// Acquire the FCast listening socket(s); `None` if the user quit before
    /// one was bound.
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

    /// Surface the port-conflict dialog and act on the user's choice. Only
    /// fcast relocates; gcast/raop/airplay keep their fixed ports and skip
    /// if theirs is taken.
    #[cfg(not(target_os = "android"))]
    async fn handle_port_conflict(
        &mut self,
        event_rx: &mut UnboundedReceiver<Message>,
    ) -> Result<Option<Vec<TcpListener>>> {
        // Headless can neither show the dialog nor receive a choice, so fail fast.
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
                Some(Message::PortConflictChoice(
                    crate::message::PortConflictChoice::UseDifferentPort,
                )) => {
                    let listeners = Self::bind_fcast_listeners(0).await?;
                    let port = listeners[0].local_addr()?.port();
                    info!(port, "Starting FCast on a different port");
                    self.fcast_port = port;
                    break Some(listeners);
                }
                // The Quit button ends the Slint loop, which drives `Message::Quit`.
                Some(Message::Quit) | None => break None,
                // Keep dispatching ordinary events (notably mDNS updates emitted before
                // we got here) so the idle UI shows network info rather than "not connected".
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

        // `None` means the user quit before anything was bound. When FCast is disabled
        // we commit with no listeners so the loop still serves
        // chromecast/airplay/raop; the empty listener stream stays pending and
        // never fires.
        let listeners = if self.settings.fcast_enabled() {
            self.resolve_listen_port(&mut event_rx).await?
        } else {
            info!("FCast receiver disabled by settings, not binding or advertising it");
            Some(Vec::new())
        };
        if let Some(listeners) = listeners {
            self.port_committed = true;
            // Advertise only now, at the port actually bound, so a second instance never
            // publishes a duplicate record.
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
                        // Deliberately outside the Playing gate: the detector must see the
                        // excluded ticks or they count as pinned playback.
                        if let Err(err) = self.poll_freeze_watchdog() {
                            error!(?err, "Freeze watchdog recovery failed");
                        }
                    }
                    session = listener_stream.select_next_some() => {
                        match session {
                            Ok((stream, _)) => {
                                self.handle_new_fcast_session(stream, session_id);
                                session_id += 1;
                            }
                            // A failed accept is per-connection: propagating it would end
                            // the event loop while the UI and mDNS keep running.
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

        self.gui.quit_loop();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two-masters window: a stale-looking generation matching the
    /// pending pre-arm is the pipeline's future and must be held, never
    /// dropped. Everything else stays a dropped straggler, including a stale
    /// generation while a DIFFERENT prepare is pending.
    #[test]
    fn only_the_pending_prearm_generation_is_held() {
        assert_eq!(
            stale_event_action(7, Some(7), false),
            StaleEventAction::Hold
        );
        assert_eq!(
            stale_event_action(7, Some(8), false),
            StaleEventAction::Drop
        );
        assert_eq!(stale_event_action(7, None, false), StaleEventAction::Drop);
    }

    /// Holding a transport edge wedges the mirror machine mid-transition, and
    /// a Resume out of that only retargets: the pipeline stays PAUSED, so the
    /// activation never releases and the edge never replays.
    #[test]
    fn transport_edges_of_the_pending_prearm_apply_immediately() {
        assert_eq!(
            stale_event_action(7, Some(7), true),
            StaleEventAction::Apply
        );
        // A pipeline-scoped edge from a SUPERSEDED load is still a straggler:
        // that pipeline is gone.
        assert_eq!(stale_event_action(7, Some(8), true), StaleEventAction::Drop);
        assert_eq!(stale_event_action(7, None, true), StaleEventAction::Drop);
    }

    /// Only the two transport edges describe the pipeline instead of the item.
    #[test]
    fn only_transport_edges_are_pipeline_scoped() {
        assert!(Application::player_event_is_pipeline_scoped(
            &player::PlayerEvent::AsyncDone
        ));
        assert!(Application::player_event_is_pipeline_scoped(
            &player::PlayerEvent::StateChanged {
                old: gst::State::Playing,
                current: gst::State::Paused,
                pending: gst::State::VoidPending,
            }
        ));
        for event in [
            player::PlayerEvent::EndOfStream,
            player::PlayerEvent::DurationChanged,
            player::PlayerEvent::Buffering(50),
            player::PlayerEvent::StreamsSelected {
                video: None,
                audio: None,
                subtitle: None,
                seqnum: gst::Seqnum::next(),
            },
        ] {
            assert!(
                !Application::player_event_is_pipeline_scoped(&event),
                "{event:?}"
            );
        }
    }

    /// A selection resolves stream ids against the list the collection
    /// installs, and the crate emits the collection one message AFTER the
    /// activation: replaying it in the adoption clears every track id.
    #[test]
    fn a_held_selection_waits_for_the_adopted_collection() {
        assert_eq!(
            held_replay_phase(&player::PlayerEvent::StreamsSelected {
                video: None,
                audio: None,
                subtitle: None,
                seqnum: gst::Seqnum::next(),
            }),
            HeldReplayPhase::FirstCollection
        );
        for event in [
            player::PlayerEvent::DurationChanged,
            player::PlayerEvent::Buffering(100),
            player::PlayerEvent::AsyncDone,
        ] {
            assert_eq!(
                held_replay_phase(&event),
                HeldReplayPhase::Adoption,
                "{event:?}"
            );
        }
    }

    /// The post-activation window: the crate reports a generation-less cancel
    /// because the activation already took the prepared slot. Reading that as
    /// "prepare gone" replays a flushing seek into the successor.
    #[test]
    fn a_generationless_cancel_reports_the_performed_swap() {
        assert_eq!(
            cancel_report(None, Some(7), true),
            CancelReport::SwapPerformed
        );
        assert_eq!(
            cancel_report(Some(7), Some(7), true),
            CancelReport::PrepareGone
        );
        // Drift still clears: a pre-arm nothing will activate wedges gapless.
        assert_eq!(
            cancel_report(Some(9), Some(7), true),
            CancelReport::PrepareGone
        );
    }

    /// Nothing can be parked without a cancel of ours in flight, so anything
    /// else is another cancel's echo.
    #[test]
    fn a_cancel_without_one_in_flight_is_a_straggler() {
        for reported in [None, Some(7)] {
            assert_eq!(
                cancel_report(reported, Some(7), false),
                CancelReport::Straggler
            );
            assert_eq!(
                cancel_report(reported, None, false),
                CancelReport::Straggler
            );
            assert_eq!(cancel_report(reported, None, true), CancelReport::Straggler);
        }
    }

    #[test]
    fn valid_show_durations_become_delays() {
        assert_eq!(show_duration_delay(0.0), Some(Duration::ZERO));
        assert_eq!(show_duration_delay(1.5), Some(Duration::from_millis(1500)));
        assert_eq!(show_duration_delay(30.0), Some(Duration::from_secs(30)));
    }

    /// A won cancel leaves the playing item as the only linked input.
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

    /// After a swap a flushing seek would be answered by the NEXT item's
    /// source.
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

    /// Deliberate policy: a track switch is not worth restarting the playing
    /// item.
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

    /// A parked operation must never leak into the next item.
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

    /// Only a reload reports "playback replaced" and skips the caller's adopt.
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

    /// A latched zero would survive the whole session, since the lazy read is
    /// one-shot.
    #[test]
    fn a_failed_or_zero_duration_query_is_never_cached() {
        assert_eq!(cacheable_duration(None), None);
        assert_eq!(cacheable_duration(Some(gst::ClockTime::ZERO)), None);
        let real = gst::ClockTime::from_seconds(42);
        assert_eq!(cacheable_duration(Some(real)), Some(real));
        // Sub-second durations are real durations.
        let tiny = gst::ClockTime::from_nseconds(1);
        assert_eq!(cacheable_duration(Some(tiny)), Some(tiny));
    }

    #[test]
    fn unusable_show_durations_are_rejected_not_panicked() {
        // `showDuration` is an unvalidated `f64` on the v3 wire.
        assert_eq!(show_duration_delay(-1.0), None);
        assert_eq!(show_duration_delay(f64::NAN), None);
        assert_eq!(show_duration_delay(f64::INFINITY), None);
        assert_eq!(show_duration_delay(f64::NEG_INFINITY), None);
        assert_eq!(show_duration_delay(f64::MAX), None);
    }

    #[test]
    fn bug_report_uri_keeps_no_secrets() {
        // DASH/HLS URLs carry tokens and signatures in the query; the
        // diagnostic block must not.
        assert_eq!(
            strip_uri_query("https://cdn.example.com/v/main.mpd?token=SECRET&sig=x"),
            "https://cdn.example.com/v/main.mpd"
        );
        assert_eq!(
            strip_uri_query("https://cdn.example.com/v/main.mpd#t=10"),
            "https://cdn.example.com/v/main.mpd"
        );
        assert_eq!(strip_uri_query("file:///a/b.mkv"), "file:///a/b.mkv");
    }

    #[test]
    fn toast_detail_host_extraction() {
        assert_eq!(
            uri_host("https://user:pw@cdn.example.com:8443/v/x.mpd?sig=1"),
            Some("cdn.example.com")
        );
        assert_eq!(uri_host("http://10.0.0.4/x.mkv"), Some("10.0.0.4"));
        assert_eq!(uri_host("not a uri"), None);
        assert_eq!(uri_host("file:///a/b.mkv"), None);
    }

    #[test]
    fn every_error_kind_has_exactly_one_surface() {
        use player::MediaErrorKind as K;
        for kind in [
            K::NotFound,
            K::AccessDenied,
            K::NetworkFailure,
            K::UnsupportedFormat,
            K::MissingCodec,
            K::DecodeFailed,
            K::DrmProtected,
            K::OutputFailure,
            K::ImageDownloadFailed,
            K::Frozen,
            K::Unexpected,
        ] {
            let popup = matches!(kind, K::Frozen | K::Unexpected | K::MissingCodec);
            assert_eq!(
                media_error_toast_kind(kind).is_none(),
                popup,
                "{kind:?} must {} the report-bug popup",
                if popup { "take" } else { "not take" }
            );
        }
    }

    #[test]
    fn warning_ring_caps_and_truncates() {
        let mut ring = RecentWarnings::new();
        let t0 = Instant::now();
        // One long message must not flood the block, and the cut must land
        // on a char boundary (the ø straddles the 200-byte mark).
        let long = "ø".repeat(150);
        push_recent_warning(&mut ring, t0, "FC-W99", &long);
        let stored = &ring[0].2;
        assert!(stored.len() <= RECENT_WARNING_MSG_MAX + 3);
        assert!(stored.ends_with("..."));
        // The ring holds the LAST cap entries.
        for i in 0..RECENT_WARNINGS_CAP + 5 {
            push_recent_warning(&mut ring, t0, "FC-W02", &format!("m{i}"));
        }
        assert_eq!(ring.len(), RECENT_WARNINGS_CAP);
        assert_eq!(
            ring.back().unwrap().2,
            format!("m{}", RECENT_WARNINGS_CAP + 4)
        );

        let formatted = format_recent_warnings(&ring, t0 + Duration::from_secs(7));
        assert!(formatted.starts_with("recent warnings"));
        assert!(formatted.contains("7s ago FC-W02"));
        assert_eq!(
            format_recent_warnings(&RecentWarnings::new(), t0),
            "no recent warnings"
        );
    }

    #[test]
    fn issue_tracker_qr_builds() {
        let qr = issue_tracker_qr().expect("static URL must encode");
        assert!(qr.size > 0);
        assert_eq!(qr.dark.len(), (qr.size * qr.size) as usize);
    }
}
