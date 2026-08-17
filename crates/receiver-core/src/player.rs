use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::Result;
use fcast_protocol::PlaybackState;
use gst::{glib::object::ObjectExt, prelude::*};
use tracing::{debug, error, info, instrument, warn};

use crate::MessageSender;
use fcastplaybin::state_machine::{
    BufferingStateResult, RunningState, Seek, StateChangeResult, StateMachine,
};

/// What plays. Re-exported from `fcastplaybin`: a URI, or a pre-built source
/// element. The APPLICATION builds the element (HTTP with per-load headers,
/// WHEP bin, fwebrtc, AirPlay mirror) rather than the playbin resolving a
/// URI scheme itself: those sources are receiver elements wired to receiver
/// state (signalling channels, mirror sessions, GStreamer contexts), which
/// fcastplaybin deliberately knows nothing about: no fake-URI dispatch, no
/// global config side channels.
pub use fcastplaybin::MediaInput;

/// Correlates missing-plugin element messages with decodebin's follow-up
/// "missing plugin" WARNING (posted right after, on the same thread) so the
/// user-facing warning can be dropped when the only undecodable streams were
/// non-media metadata that needs no decoder.
#[derive(Default)]
struct MissingPluginTracker {
    /// A real media stream had no decoder.
    saw_real: AtomicBool,
    /// Only a non-media metadata stream had no "decoder".
    saw_ignorable: AtomicBool,
}

/// Whether a missing-plugin element message is for a non-media metadata stream
/// (e.g. qtdemux's `meta/x-gst-fourcc-priv` for an unknown atom like `wide`),
/// which needs no decoder and so should not be reported as a missing codec.
fn missing_plugin_is_ignorable(msg: &gst::Message) -> bool {
    let Some(structure) = msg.structure() else {
        return false;
    };
    // Missing decoder/encoder messages carry the offending caps in `detail`; other
    // kinds (element/urisource/...) store a string there, so a failed caps read
    // means "treat as real".
    let Ok(caps) = structure.get::<gst::Caps>("detail") else {
        return false;
    };
    !caps.is_empty() && caps.iter().all(|s| s.name().as_str().starts_with("meta/"))
}

/// The debug text our adaptivedemux2 carry-patch posts when it discards a
/// buffer instead of pausing the output task for good
/// (`xtask/patches/adaptivedemux2-transient-flushing-no-permanent-pause.
/// patch`). Ours, so it is stable, and it appears nowhere else in GStreamer.
const TRANSIENT_FLUSHING_DISCARD: &str =
    "downstream returned FLUSHING while this element is not flushing";

/// Whether a bus WARNING is the carry-patch's transient-FLUSHING discard: the
/// ONE warning class that must not reach the user.
///
/// It reports a RECOVERED hiccup (the patch turned a permanent silent freeze
/// into one discarded buffer, playback continues), so the toast is pure noise,
/// and the race can fire on any transient flush. The match is on the debug
/// string because the patch posts a NULL user-facing text, so the message is
/// GStreamer's generic "GStreamer encountered a general stream error." for
/// STREAM/FAILED and cannot discriminate anything. Deliberately not a general
/// suppression list: every other warning still toasts exactly as before.
fn warning_is_transient_flushing_discard(debug: Option<&str>) -> bool {
    debug.is_some_and(|debug| debug.contains(TRANSIENT_FLUSHING_DISCARD))
}

/// The stream the discard names: `Discarding data on <stream>: downstream ...`.
///
/// The stream name is the whole of what makes a persistent discard actionable -
/// `subtitle_00` says the text branch is the stuck one, `video_00` says the
/// item is dead - and it is the only per-stream key the message carries.
fn discarded_stream_name(debug: &str) -> Option<&str> {
    let rest = debug.split("Discarding data on ").nth(1)?;
    let name = rest.split(':').next()?.trim();
    (!name.is_empty()).then_some(name)
}

/// Discards on ONE stream before the classifier stops calling it transient.
///
/// "Transient" is a claim, and `dash-embedded-still-broken.txt` is that claim
/// being wrong: one `Discarding data on subtitle_00` at PLAYING, logged as
/// "(recovered)", on a run where the subtitles never appeared at all. The
/// carry-patch's discard IS recoverable per buffer - it drops one buffer
/// instead of pausing the output loop for good - but nothing about it says the
/// downstream FLUSHING will ever clear, and a multiqueue slot's `srcresult`
/// latches until a FLUSH_STOP reaches it. So the count is the evidence: one
/// discard is a race, several on the same stream is a branch that is not
/// coming back.
///
/// Three, not one: a genuine transient flush can legitimately catch more than
/// one buffer in flight (the demuxer's output loop serves every track from one
/// thread and can be several buffers deep when a flush lands).
const FLUSHING_DISCARD_ESCALATION: u32 = 3;

/// How long a subtitle track may sit discarded with nothing delivered before
/// the receiver calls it dead rather than transient.
///
/// Generous, because a sparse track legitimately delivers nothing for a while:
/// this is a track that took a FLUSHING discard and then produced NO cue at
/// all for half a minute, not a track that is merely quiet.
const SUBTITLE_STALL_VERDICT: Duration = Duration::from_secs(30);

/// The subtitle path's liveness, shared between the consumer callback (which
/// counts what reaches the engine) and the bus hook (which sees the discards).
///
/// # Why the discard COUNT cannot be the signal
///
/// [`FLUSHING_DISCARD_ESCALATION`] waits for a third discard on one stream. It
/// will never arrive. The carry-patch sets `slot->warned_transient_flushing`
/// on the FIRST discard and clears it nowhere (`gstadaptivedemux.c:3700-3701`,
/// three references in the whole file: the declaration, this check and this
/// set), so a slot that has latched downstream FLUSHING for good discards
/// every subsequent buffer SILENTLY. The receiver therefore sees exactly ONE
/// warning for a permanently dead track, and a count-based threshold of three
/// is unreachable for the precise failure it was built to catch - which is the
/// field's `subtitle_00`, count=1, subtitles never appearing.
///
/// So the second signal is DELIVERY, not repetition: a discard followed by
/// [`SUBTITLE_STALL_VERDICT`] of nothing reaching the engine is a dead track,
/// however many warnings upstream chose to post. Sampled on the application's
/// existing tick (`Application::poll_freeze_watchdog`'s caller), never on a
/// timer of its own - the bus hook stays lock-free-ish and wakeup-free, which
/// is this file's standing discipline.
#[derive(Default, Clone)]
struct SubtitleFlow(std::sync::Arc<SubtitleFlowInner>);

#[derive(Default)]
struct SubtitleFlowInner {
    /// Subtitle items that reached the engine this load.
    delivered: AtomicU64,
    /// The first FLUSHING discard since the last load: the stream it named,
    /// the delivery count when it happened, and when. `None` until one lands.
    discard: std::sync::Mutex<Option<(String, u64, Instant)>>,
    /// The verdict has been reported; never report it twice for one load.
    reported: AtomicBool,
}

impl SubtitleFlow {
    fn delivered(&self) {
        self.0.delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// Count an item on its way to the engine and hand it back.
    ///
    /// A `Clear` is the ABSENCE of a cue, not one, so it does not count: a
    /// track that only ever clears has delivered nothing, which is exactly the
    /// state this verdict exists to name.
    fn tally(&self, item: fcastplaybin::SubtitleFeedItem) -> fcastplaybin::SubtitleFeedItem {
        if !matches!(item, fcastplaybin::SubtitleFeedItem::Clear) {
            self.delivered();
        }
        item
    }

    /// Record a discard, keeping the FIRST one: it is the one whose delivery
    /// mark tells us whether anything has flowed since the track broke.
    fn note_discard(&self, stream: &str) {
        let Ok(mut discard) = self.0.discard.lock() else {
            return;
        };
        if discard.is_none() {
            *discard = Some((
                stream.to_owned(),
                self.0.delivered.load(Ordering::Relaxed),
                Instant::now(),
            ));
        }
    }

    /// A new load: nothing discarded, nothing delivered, verdict re-armed.
    fn reset(&self) {
        self.0.delivered.store(0, Ordering::Relaxed);
        self.0.reported.store(false, Ordering::Relaxed);
        if let Ok(mut discard) = self.0.discard.lock() {
            *discard = None;
        }
    }

    /// The stream to report as dead, once, or `None`.
    fn stalled_stream(&self) -> Option<String> {
        let Ok(discard) = self.0.discard.lock() else {
            return None;
        };
        let (stream, delivered_then, at) = discard.as_ref()?;
        if self.0.delivered.load(Ordering::Relaxed) != *delivered_then
            || at.elapsed() < SUBTITLE_STALL_VERDICT
            || self.0.reported.swap(true, Ordering::SeqCst)
        {
            return None;
        }
        Some(stream.clone())
    }
}

/// Whether a teardown is in flight, shared with the bus hook.
///
/// # Why the escalation needs to know
///
/// A Stop on a live adaptive item ALWAYS produces this discard class, and
/// harmlessly: the teardown's flush pair reaches the demuxer while its output
/// loop still has buffers in hand, so every selected stream discards one. A
/// field log of a plain `Stop { target: Ready }` shows it on `video_00` AND
/// `audio_00` within ~1 ms of the job, alongside the crate's "restoring the
/// segment the flush pair removed" on `sink_0..2` - the ordinary shape of an
/// item being taken down, and the rig measures the same thing (the repeated
/// re-enable test's discards all land at 41.62 s against a shutdown at
/// 41.98 s).
///
/// Counting those against [`FLUSHING_DISCARD_ESCALATION`] spends 2-3 of a
/// 3-discard budget per Stop, so a receiver that has stopped a couple of items
/// escalates on the next transient race and tells the user a track is dead
/// when nothing is wrong. The budget exists for the MID-PLAY discard - the
/// stuck branch that never comes back - and that is the one it has to keep
/// its accuracy for.
///
/// An `AtomicBool` because the reader is the bus hook on a GStreamer streaming
/// thread, which this file's discipline keeps lock-free and wait-free.
#[derive(Default, Clone)]
struct TeardownFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl TeardownFlag {
    fn set(&self, tearing_down: bool) {
        self.0.store(tearing_down, Ordering::SeqCst);
    }

    fn tearing_down(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Per-stream discard counts for [`FLUSHING_DISCARD_ESCALATION`].
///
/// COUNT-BASED, deliberately, and there is no timer anywhere near it. This is
/// read and written from the bus hook, which runs on GStreamer STREAMING
/// THREADS; arming or servicing a timer there is the one thing this file's
/// discipline forbids. A count needs no clock and no wakeup: the Nth discard
/// is itself the event.
///
/// Keyed by stream name so two stuck streams escalate independently, and never
/// reset - a stream that recovers simply stops arriving here, and re-arming on
/// a gap would need exactly the timer this avoids.
#[derive(Default)]
struct FlushingDiscards(std::sync::Mutex<std::collections::HashMap<String, u32>>);

impl FlushingDiscards {
    /// Record one discard on `stream` and return its new count.
    ///
    /// A poisoned lock is not worth a panic on a streaming thread: the counter
    /// is diagnostics, so a poisoned map degrades to "always transient" rather
    /// than taking the pipeline down with it.
    fn record(&self, stream: &str) -> u32 {
        let Ok(mut counts) = self.0.lock() else {
            return 1;
        };
        let count = counts.entry(stream.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }
}

/// Lever: `FCAST_NO_WARNING_FILTER` (set = old behavior, every warning toasts).
/// Read once, the hook runs on streaming threads.
fn toast_every_warning() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("FCAST_NO_WARNING_FILTER").is_some())
}

/// The driver's bitmap subtitle format as the engine names it.
///
/// The two enums are deliberate mirrors (the driver cannot depend on the
/// renderer) so the whole translation is this match, and this crate is the one
/// place that sees both sides. That is also why the agreement between their
/// implemented sets is asserted here (see the test of the same name below).
fn bitmap_format(format: fcastplaybin::BitmapSubFormat) -> fcast_video::subpic::BitmapFormat {
    match format {
        fcastplaybin::BitmapSubFormat::Pgs => fcast_video::subpic::BitmapFormat::Pgs,
        fcastplaybin::BitmapSubFormat::Vobsub => fcast_video::subpic::BitmapFormat::Vobsub,
        fcastplaybin::BitmapSubFormat::Dvb => fcast_video::subpic::BitmapFormat::Dvb,
    }
}

/// The playback snapshot a load returns to once it prerolls (the start
/// position/rate seek `fcastplaybin::load` applies in PAUSED).
#[derive(Debug, Clone, Copy)]
pub struct RestorePoint {
    pub position: gst::ClockTime,
    pub rate: f32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PlayerState {
    Paused,
    Playing,
    Buffering,
    Stopped,
}

impl PlayerState {
    pub fn as_fcast_v4(&self) -> fcast_protocol::v4::PlaybackState {
        use fcast_protocol::v4;
        match self {
            PlayerState::Paused => v4::PlaybackState::Paused,
            PlayerState::Playing => v4::PlaybackState::Playing,
            PlayerState::Buffering => v4::PlaybackState::Buffering,
            PlayerState::Stopped => v4::PlaybackState::Idle,
        }
    }
}

/// Project the player state onto the v3 wire [`PlaybackState`], which has no
/// Buffering variant (Idle/Playing/Paused only). Progress broadcasts run only
/// once media info exists, so a transient Buffering there is a mid-playback
/// rebuffer or gapless switch, NOT a stop: report the transport the pipeline
/// is resuming toward (`desired`) rather than a bogus Idle. Mapping it to Idle
/// makes senders read "playback ended" and advance/stop the queue in the
/// middle of a gapless handoff.
fn project_wire_state(state: PlayerState, desired: RunningState) -> PlaybackState {
    match state {
        PlayerState::Stopped => PlaybackState::Idle,
        PlayerState::Playing => PlaybackState::Playing,
        PlayerState::Paused => PlaybackState::Paused,
        PlayerState::Buffering => match desired {
            RunningState::Paused => PlaybackState::Paused,
            RunningState::Playing => PlaybackState::Playing,
        },
    }
}

pub type StreamId = String;

/// Which stream slot a track-change request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// A full track selection, keyed by GStreamer stream id (`None` = slot
/// disabled). Re-exported from `fcastplaybin`, whose selection engine owns
/// all dispatch/confirmation sequencing; indices exist only at the
/// protocol/GUI edge.
pub use fcastplaybin::TrackSelection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaErrorKind {
    NotFound,
    NotAuthorized,
    UnsupportedFormat,
    Other,
}

impl MediaErrorKind {
    fn from_glib_error(err: &gst::glib::Error) -> Self {
        if let Some(err) = err.kind::<gst::ResourceError>() {
            match err {
                gst::ResourceError::NotFound => Self::NotFound,
                gst::ResourceError::NotAuthorized => Self::NotAuthorized,
                _ => Self::Other,
            }
        } else if let Some(err) = err.kind::<gst::StreamError>() {
            match err {
                gst::StreamError::TypeNotFound
                | gst::StreamError::WrongType
                | gst::StreamError::CodecNotFound
                | gst::StreamError::Decode
                | gst::StreamError::Demux
                | gst::StreamError::Format => Self::UnsupportedFormat,
                _ => Self::Other,
            }
        } else {
            Self::Other
        }
    }
}

/// Receiver-facing playback events, forwarded into the application loop.
/// The raw GStreamer bus lives inside `fcastplaybin` now: it translates
/// messages into typed [`fcastplaybin::PlaybinEvent`]s on the posting
/// thread, and [`Player`] maps those onto this protocol-facing enum (see
/// `relay_event`).
#[derive(Debug)]
pub enum PlayerEvent {
    EndOfStream,
    UriLoaded,
    Tags(gst::TagList),
    VolumeChanged(f64),
    /// User must call Player::handle_stream_collection()
    StreamCollection(gst::StreamCollection),
    /// An async state change or (flushing) seek finished prerolling. Not
    /// attributable to a specific operation: `GstBin` posts its aggregated
    /// ASYNC_DONE with a fresh seqnum (fcastplaybin's selection engine
    /// relies on exclusivity instead).
    AsyncDone,
    /// The media's duration changed and any cached value is stale: re-query
    /// `Player::get_duration`. Carries no value on purpose (GStreamer's own
    /// `DURATION_CHANGED` does not either): only a fresh query is
    /// authoritative. A push-mode demuxer (`oggdemux` over the fcomp
    /// transport) announces an approximate duration up front and refines it
    /// while the item plays, which is what this event delivers.
    ///
    /// LOAD-SCOPED deliberately: a superseded load's refresh must be dropped
    /// by the generation filter, and fcastplaybin already suppresses the one
    /// case where the current generation would still answer for the wrong item
    /// (a performed gapless swap waiting to activate).
    DurationChanged,
    Buffering(i32),
    IsLive,
    StateChanged {
        old: gst::State,
        current: gst::State,
        pending: gst::State,
    },
    /// An element asked the application to change the pipeline state.
    RequestState(gst::State),
    QueueSeek(Seek),
    StreamsSelected {
        video: Option<StreamId>,
        audio: Option<StreamId>,
        subtitle: Option<StreamId>,
        /// Seqnum of the `SELECT_STREAMS` event this confirms (decodebin3
        /// stamps it onto the message).
        seqnum: gst::Seqnum,
    },
    /// A subtitle refresh seek could not be performed.
    SubtitleRefreshFailed {
        seqnum: gst::Seqnum,
    },
    RateChanged(f64),
    SeekFailed,
    /// The element providing the pipeline clock went away (e.g. the audio
    /// sink after the audio track was deselected). User must call
    /// `Player::recover_clock()`.
    ClockLost,
    Error {
        /// Which input the error came from (fcastplaybin's generation-tagged
        /// attribution). Never an external subtitle input: those errors are
        /// handled inside fcastplaybin (in-place recovery or
        /// `ExternalSubtitleFailed`).
        origin: fcastplaybin::ErrorOrigin,
        kind: MediaErrorKind,
        message: String,
        /// Diagnostic only (the failing source's URI, when it has one).
        failed_uri: Option<String>,
    },
    /// An attached external subtitle input failed for good and fcastplaybin
    /// already detached it (failed attach, bus error while shown, or its
    /// stream never materialized within the crate's watchdog). The
    /// application drops its catalog entry and reports `ResourceNotFound`.
    ExternalSubtitleFailed {
        id: fcastplaybin::ExternalSubId,
    },
    /// The selected subtitle track carries caps the cue renderer cannot
    /// render (bitmap subtitles, raw ASS/SSA), so nothing will be shown for
    /// it. Reported by fcastplaybin at most once per track per load; see
    /// `fcastplaybin::PlaybinEvent::SubtitleTrackUnsupported`.
    SubtitleTrackUnsupported {
        sid: String,
        /// The caps as a string, for the log and the toast.
        caps: String,
    },
    Warning(String),
    StreamTagsUpdated,
    /// fimagedec announced what the current load decodes to: the load is an
    /// image (still or animation) rendered through the video pipeline.
    ImageStream(ImageStreamInfo),
    /// The media source is throttled: its server directed a backoff of
    /// `remaining_ms` before the next request (sabrumpsrc, only while
    /// starved). 0 means the backoff ended. Shown as a "server busy"
    /// countdown in the GUI.
    SourceBackoff {
        remaining_ms: u64,
    },
    /// A pre-armed next item ([`Player::prepare_next`]) went live: the
    /// current item drained and the pipeline switched gaplessly. Stamped
    /// with the PREPARED generation; the application validates it against
    /// its pre-arm bookkeeping and adopts it
    /// ([`Player::adopt_gapless_generation`]). The new item's collection
    /// follows under the same generation.
    GaplessActivated,
    /// A pre-armed next item failed before activating; fcastplaybin already
    /// dropped it and the current item plays on. The application clears its
    /// pre-arm bookkeeping and the item loads through the ordinary
    /// end-of-stream advance instead.
    GaplessPrepareFailed {
        generation: u64,
    },
    /// A requested cancel ([`Player::cancel_prepared`]) took effect: nothing
    /// is pre-armed any more and no activation will follow. `generation` names
    /// the dropped pre-arm, `None` when there was nothing to cancel.
    GaplessCancelled {
        generation: Option<u64>,
    },
    /// A requested cancel lost the race against the swap: `generation` is
    /// activating regardless. The application must KEEP its pre-arm
    /// bookkeeping so the imminent [`PlayerEvent::GaplessActivated`] is
    /// adopted instead of resyncing with a reload of the finished item.
    GaplessCancelDeclined {
        generation: u64,
    },
}

/// Parsed form of fimagedec's "fcast-image-stream" announcement.
#[derive(Debug, Clone)]
pub struct ImageStreamInfo {
    /// Source format short name ("gif", "apng", "webp", ...).
    pub format: String,
    pub width: i32,
    pub height: i32,
    pub animated: bool,
}

pub fn stream_title(stream: &gst::Stream) -> String {
    let mut res = String::new();
    if let Some(tags) = stream.tags() {
        if let Some(language) = tags.get::<gst::tags::LanguageName>() {
            res += language.get();
        } else if let Some(language) = tags.get::<gst::tags::LanguageCode>() {
            let code = language.get();
            if let Some(lang) = gst_tag::language_codes::language_name(code) {
                res += lang;
            } else {
                res += code;
            }
        }
        if let Some(title) = tags.get::<gst::tags::Title>() {
            let title = title.get();
            if !title.is_empty() {
                if !res.is_empty() {
                    res += " - ";
                }
                res += title;
            }
        }
    }

    if res.is_empty() {
        res += "Unknown";
    }

    res
}

pub struct Stream {
    pub inner: gst::Stream,
    pub title: String,
}

/// Rebuild the stream list for a new collection with STABLE positions:
/// every stream of `previous` that is still advertised keeps its index
/// (adopting the collection's fresh `gst::Stream` object, whose tags may
/// have changed), streams that left are dropped in place, and newcomers
/// append in collection order. Positions are the protocol/GUI track ids,
/// which must not shift mid-item (see `handle_stream_collection`).
fn merge_streams_stable(previous: Vec<Stream>, collection: &gst::StreamCollection) -> Vec<Stream> {
    let fresh: Vec<gst::Stream> = collection.iter().collect();
    let sid_of = |s: &gst::Stream| s.stream_id().map(|id| id.to_string());

    let mut merged: Vec<Stream> = Vec::with_capacity(fresh.len());
    for old in previous {
        let old_sid = sid_of(&old.inner);
        if let Some(new) = fresh
            .iter()
            .find(|s| old_sid.is_some() && sid_of(s) == old_sid)
        {
            merged.push(Stream {
                title: stream_title(new),
                inner: new.clone(),
            });
        }
    }
    for new in &fresh {
        let new_sid = sid_of(new);
        let known = merged
            .iter()
            .any(|m| new_sid.is_some() && sid_of(&m.inner) == new_sid);
        if !known {
            merged.push(Stream {
                title: stream_title(new),
                inner: new.clone(),
            });
        }
    }
    merged
}

pub struct Player {
    /// The fcastplaybin playback orchestrator: the only pipeline handle.
    /// State changes, seeks, queries and events all go through its API.
    fcast: fcastplaybin::FcastPlaybin,
    /// A volume change was dispatched and its `VolumeChanged` confirmation
    /// has not arrived yet (see `set_volume`).
    volume_confirm_in_flight: bool,
    msg_tx: MessageSender,
    /// The transport state the user last asked for, committed by
    /// `uri_loaded` once a load prerolls. Requests landing mid-load are
    /// recorded here instead of being stomped by the load's own climb, so
    /// there is exactly ONE post-load transport driver.
    desired_transport: RunningState,
    /// The generation of the load this player currently expects events for
    /// (returned by `fcastplaybin::load_async`); `None` when stopped. The
    /// application drops load-scoped events from any other generation.
    expected_generation: Option<u64>,
    /// The generation of a pending gapless pre-arm
    /// ([`Player::prepare_next`]), adopted as `expected_generation` when
    /// its activation arrives.
    pending_gapless: Option<u64>,
    pub streams: Vec<Stream>,
    /// The applied (or optimistically in-flight) selection, keyed by stream
    /// id. Never index-based: indices exist only at the protocol/GUI edge.
    selected: TrackSelection,
    pub seekable: bool,
    /// Whether `seekable` reflects an actual answer from the pipeline. The
    /// seeking query only succeeds around preroll completion, well after
    /// tracks are first advertised. Until then `seekable == false` merely
    /// means "not known yet".
    pub seekable_known: bool,
    /// The newest volume requested while a previous change's confirmation
    /// was still in flight, applied when it arrives (see `set_volume`).
    pending_volume: Option<f32>,
    state_machine: StateMachine,
    stream_collection: Option<gst::StreamCollection>,
    stream_collection_notify: Option<gst::glib::SignalHandlerId>,
    /// Shared with the bus hook so the FLUSHING-discard escalation can tell a
    /// teardown's own flush from a branch that is genuinely stuck. See
    /// [`TeardownFlag`].
    teardown: TeardownFlag,
    /// Discards seen vs subtitle items delivered, the signal that catches a
    /// latched track the discard COUNT never can. See [`SubtitleFlow`].
    subtitle_flow: SubtitleFlow,
}

impl Player {
    pub fn new(
        video_sink: Option<gst::Element>,
        cue_engine: Option<fcast_video::cue::CueEngine>,
        msg_tx: MessageSender,
        fcomp_context: crate::fcompsrc::imp::CompContext,
        #[cfg(feature = "airplay")] airplay_context: crate::airplay::AirPlayContext,
    ) -> Result<Self> {
        // The fcastplaybin orchestrator owns the pipeline, its bus and its
        // worker thread, this constructor only wires the receiver-specific
        // pieces onto its API.
        //
        // Audio: the native PipeWire sink on Linux when a daemon is
        // reachable (see pwaudiosink.rs for why), autoaudiosink otherwise.
        // FCAST_NO_PW_AUDIO=1 forces the fallback for A/B comparisons.
        #[cfg(target_os = "linux")]
        let audio = if std::env::var("FCAST_NO_PW_AUDIO").is_ok_and(|v| v == "1")
            || !fcast_gst_elements::pwaudiosink::is_available()
        {
            info!("audio sink: autoaudiosink (PipeWire disabled or unreachable)");
            fcastplaybin::AudioSink::Auto
        } else {
            info!("audio sink: native PipeWire (fcastpwaudiosink)");
            fcastplaybin::AudioSink::Factory(Box::new(|| {
                use anyhow::Context;
                gst::ElementFactory::make("fcastpwaudiosink")
                    .build()
                    .context("creating fcastpwaudiosink")
            }))
        };
        #[cfg(not(target_os = "linux"))]
        let audio = fcastplaybin::AudioSink::Auto;

        let fcast = fcastplaybin::FcastPlaybin::new(fcastplaybin::Sinks {
            video: video_sink,
            audio,
        })?;

        // CUE-IR SELECTION. `rssubparse`/`rsssaparse` deliver styling one of
        // two ways, chosen by their `text-format` property: inline pango markup
        // in the buffer text (the default, and what the C subparse does), or
        // plain UTF-8 text with the structured cue attached as a `CueIrMeta`.
        // Only the second carries colors, per-cue positioning and karaoke, so
        // the receiver asks for it.
        //
        // A property, not a caps preference: cue-ir negotiates the very same
        // `text/x-raw, format=utf8` the default utf8 path does, so there is
        // nothing downstream could express a preference WITH. And it has to be
        // set on elements nobody here creates, since decodebin3 autoplugs them
        // by rank (see `gstreamer::init`'s rank swap), hence the hierarchy-wide
        // `deep-element-added` hook, the same trick vajpegdec and media_source
        // already use. It fires on `gst_bin_add`, before the child is brought
        // up to its parent's state, which is what the `mutable_ready` property
        // requires: the mode is latched when the src caps are chosen.
        //
        // Lever: `FCAST_NO_CUE_IR` (set = off) skips the hook entirely, so the
        // parsers stay in their default mode and negotiation is bit-for-bit
        // what it was before cue-IR existed. The driver reads the same single
        // answer and then ignores the meta as well.
        if fcastplaybin::cue_ir_enabled() {
            use gst::prelude::*;
            fcast
                .pipeline()
                .connect_deep_element_added(|_, _, element| {
                    let Some(factory) = element.factory() else {
                        return;
                    };
                    if matches!(factory.name().as_str(), "rssubparse" | "rsssaparse") {
                        element.set_property_from_str("text-format", "cue-ir");
                    }
                });
        }

        // SUBTITLE CUES: the driver routes them through this consumer, where
        // it used to composite them in subtitleoverlay, now deleted. This is
        // the whole of the receiver's side of that path: the
        // driver resolves each cue to running time, the sink's cue engine
        // decides which one covers the frame being shown and rasterizes it,
        // and `lib.rs` repaints when the engine says the overlay set moved.
        //
        // WITHOUT THIS NOTHING RENDERS. There is no compositor left in the
        // driver's pipeline to fall back to.
        //
        // THREADS: cues arrive on the text branch's streaming thread (the
        // appsink's new_sample/new_preroll), while a `Clear` may instead come
        // from that branch's pad probe on a flush or stream restart, or from
        // the caller's own thread when a load, a stop or a track switch
        // supersedes the item. The closure must not block on any of them
        // ([`fcastplaybin::FcastPlaybin::set_subtitle_consumer`]); every engine
        // method used here is non-blocking by construction (nothing rasterizes
        // inline, nothing waits on the raster worker) and none of them can
        // panic on a caller's cue text.
        let subtitle_flow = SubtitleFlow::default();
        if let Some(engine) = cue_engine {
            let flow = subtitle_flow.clone();
            // `tally` counts and hands the item straight back, so the delivery
            // signal costs this match neither an arm nor an indent level.
            fcast.set_subtitle_consumer(move |item| match flow.tally(item) {
                fcastplaybin::SubtitleFeedItem::Cue {
                    format,
                    text,
                    start_rt,
                    end_rt,
                    // The delivery timeline's origin; the engine takes bounds
                    // already resolved to running time and needs no timeline.
                    origin: _,
                } => engine.submit(fcast_video::cue::CueInput {
                    format: match format {
                        fcastplaybin::SubtitleTextFormat::Utf8 => {
                            fcast_video::cue::TextFormat::Utf8
                        }
                        fcastplaybin::SubtitleTextFormat::PangoMarkup => {
                            fcast_video::cue::TextFormat::PangoMarkup
                        }
                        // The IR is moved across, not copied: both sides hold
                        // the same `Arc`, and the engine's raster cache keys on
                        // it (pointer equality first).
                        fcastplaybin::SubtitleTextFormat::CueIr { ir, pts_start } => {
                            fcast_video::cue::TextFormat::CueIr { ir, pts_start }
                        }
                    },
                    text,
                    start_rt,
                    end_rt,
                }),
                // BITMAP SUBTITLES: bytes, not pictures. The driver forwards
                // one appsink sample untouched and the engine reassembles and
                // decodes them on its own worker, so this arm allocates
                // nothing, maps nothing and cannot block the streaming thread
                // it runs on. Live for all three formats, and the
                // driver's caps gate decides which caps get here (see
                // `fcastplaybin::bitmap_format_implemented`).
                fcastplaybin::SubtitleFeedItem::Bitmap {
                    format,
                    data,
                    codec_data,
                    rt,
                    duration,
                } => engine.submit_bitmap(fcast_video::subpic::BitmapPacket {
                    format: bitmap_format(format),
                    data,
                    codec_data,
                    rt,
                    duration,
                }),
                fcastplaybin::SubtitleFeedItem::Clear => engine.clear(),
            });
        }

        // Raw-message hook: bus traffic only the receiver understands
        // (context requests from its custom source elements, missing-plugin
        // reports). Runs on the posting (streaming) thread.
        let missing_plugins = MissingPluginTracker::default();
        let discards = FlushingDiscards::default();
        let teardown_flag = TeardownFlag::default();
        let teardown = teardown_flag.clone();
        let flow_hook = subtitle_flow.clone();
        let hook: fcastplaybin::MessageHook = Box::new(move |msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::NeedContext(ctx) => {
                    let typ = ctx.context_type();
                    debug!(typ, "Need context");
                    if let Some(element) = msg
                        .src()
                        .and_then(|source| source.downcast_ref::<gst::Element>())
                    {
                        if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                            let mut ctx = gst::Context::new(typ, true);
                            {
                                let ctx = ctx.get_mut().unwrap();
                                let s = ctx.structure_mut();
                                s.set("context", &fcomp_context);
                            }
                            element.set_context(&ctx);
                        }
                        #[cfg(feature = "airplay")]
                        if typ == crate::airplay::source::imp::AIRPLAY_CONTEXT {
                            let mut ctx = gst::Context::new(typ, true);
                            {
                                let ctx = ctx.get_mut().unwrap();
                                let s = ctx.structure_mut();
                                s.set(
                                    "context",
                                    crate::airplay::source::imp::BoxedAirPlayContext(
                                        airplay_context.clone(),
                                    ),
                                );
                            }
                            element.set_context(&ctx);
                        }
                    }
                    true
                }
                MessageView::Element(_) => {
                    // Consume ONLY missing-plugin reports. Other element
                    // messages (fcast-image-stream, sabrump-status) belong to
                    // fcastplaybin's translation and must fall through; a
                    // blanket `true` here silently ate them.
                    let Ok(mp) = gst_pbutils::MissingPluginMessage::parse(msg) else {
                        return false;
                    };
                    // qtdemux exposes non-media metadata streams (unknown atoms) as `meta/*`;
                    // decodebin then reports "no decoder" for them even though none is
                    // needed. Note it for the follow-up warning and don't cry wolf.
                    if missing_plugin_is_ignorable(msg) {
                        debug!(detail = %mp.installer_detail(), "Ignoring missing plugin for non-media stream");
                        missing_plugins.saw_ignorable.store(true, Ordering::SeqCst);
                    } else {
                        error!(detail = %mp.installer_detail(), desc = %mp.description(), "GStreamer missing plugin");
                        missing_plugins.saw_real.store(true, Ordering::SeqCst);
                    }
                    true
                }
                MessageView::Warning(warning) => {
                    let detail = warning.debug();
                    if warning_is_transient_flushing_discard(detail.as_deref()) {
                        // PERSISTENCE FIRST. The carry-patch recovers each
                        // individual buffer, so one discard really is a race
                        // worth swallowing - but the same message repeating on
                        // one stream means the downstream FLUSHING is not
                        // clearing, and a multiqueue slot's `srcresult` latches
                        // until a FLUSH_STOP reaches its sink pad
                        // (gstmultiqueue.c:2498 / :1466 / :2789). Past the
                        // threshold this stops being called "recovered", names
                        // the stuck stream, and is allowed through to the user
                        // EXACTLY ONCE (the count is strictly increasing, so
                        // the equality is a one-shot) - a toast per discarded
                        // buffer would be its own denial of service.
                        let stream = detail
                            .as_deref()
                            .and_then(discarded_stream_name)
                            .unwrap_or("?");
                        // EXPECTED, and off the budget. A teardown's own flush
                        // pair produces this on every selected stream (see
                        // [`TeardownFlag`]); it says nothing about whether a
                        // branch is stuck, because the branch is going away.
                        // Still logged - the A/B marker and the field forensics
                        // both want it - just not counted, and never escalated.
                        if teardown.tearing_down() {
                            let src = msg.src().map(|src| src.name().to_string());
                            debug!(
                                src = src.as_deref().unwrap_or("?"),
                                stream,
                                detail = detail.as_deref().unwrap_or(""),
                                "adaptivedemux2 discarded data during teardown (expected: the \
                                 stop's own flush caught the output loop mid-buffer)"
                            );
                            return !toast_every_warning();
                        }
                        // The delivery-based verdict's evidence, recorded
                        // whatever the count does (see [`SubtitleFlow`]: for a
                        // permanently latched slot the count never reaches the
                        // escalation threshold, because upstream warns once).
                        flow_hook.note_discard(stream);
                        let count = discards.record(stream);
                        if count >= FLUSHING_DISCARD_ESCALATION {
                            let src = msg.src().map(|src| src.name().to_string());
                            error!(
                                src = src.as_deref().unwrap_or("?"),
                                stream,
                                count,
                                detail = detail.as_deref().unwrap_or(""),
                                "adaptivedemux2 keeps discarding data on this stream: downstream \
                                 is persistently FLUSHING, so the branch is stuck and the track \
                                 will not play again by itself"
                            );
                            return count != FLUSHING_DISCARD_ESCALATION && !toast_every_warning();
                        }
                        // LOG-ONLY. Consuming the message here is what keeps the
                        // toast away: the crate emits no event for a consumed
                        // message, and `PlaybinEvent::Warning` is the only thing
                        // that reaches the GUI. Log it anyway (unconditionally,
                        // so the lever's A/B still has the marker), and with the
                        // detail the user-facing text lacks: the message the
                        // receiver used to print carried only GStreamer's
                        // generic STREAM/FAILED sentence, which named neither
                        // the element nor the pad.
                        let src = msg.src().map(|src| src.name().to_string());
                        warn!(
                            src = src.as_deref().unwrap_or("?"),
                            stream,
                            count,
                            detail = detail.as_deref().unwrap_or(""),
                            "adaptivedemux2 discarded data on a transient FLUSHING (recovered)"
                        );
                        return !toast_every_warning();
                    }
                    if warning.error().matches(gst::CoreError::MissingPlugin) {
                        let real = missing_plugins.saw_real.swap(false, Ordering::SeqCst);
                        let ignorable = missing_plugins.saw_ignorable.swap(false, Ordering::SeqCst);
                        ignorable && !real
                    } else {
                        false
                    }
                }
                _ => false,
            }
        });

        // Everything else arrives as typed events (bus translation and
        // worker feedback alike), mapped onto the protocol-facing
        // `PlayerEvent` and forwarded into the application loop.
        let event_tx = msg_tx.clone();
        fcast.set_event_handler(Some(hook), move |event, generation| {
            Self::relay_event(&event_tx, event, generation);
        });

        fcast.set_state_async(gst::State::Ready);

        Ok(Self {
            fcast,
            volume_confirm_in_flight: false,
            msg_tx,
            desired_transport: RunningState::Playing,
            expected_generation: None,
            pending_gapless: None,
            selected: TrackSelection::default(),
            seekable: false,
            seekable_known: false,
            pending_volume: None,
            state_machine: StateMachine::new(),
            stream_collection: None,
            stream_collection_notify: None,
            teardown: teardown_flag,
            subtitle_flow,
            streams: Vec::new(),
        })
    }

    /// Map a playbin event onto the protocol-facing [`PlayerEvent`] and
    /// forward it into the application loop with the load generation it
    /// belongs to. Runs on whatever thread emitted the event (a streaming
    /// thread or the playbin worker). It only sends.
    fn relay_event(msg_tx: &MessageSender, event: fcastplaybin::PlaybinEvent, generation: u64) {
        use fcastplaybin::PlaybinEvent as E;
        let event = match event {
            E::EndOfStream => PlayerEvent::EndOfStream,
            E::Loaded { live } => {
                if live {
                    msg_tx.player(PlayerEvent::IsLive, Some(generation));
                }
                PlayerEvent::UriLoaded
            }
            E::Tags(tags) => PlayerEvent::Tags(tags),
            E::VolumeChanged(volume) => PlayerEvent::VolumeChanged(volume),
            E::StreamCollection(collection) => PlayerEvent::StreamCollection(collection),
            E::AsyncDone => PlayerEvent::AsyncDone,
            E::DurationChanged => PlayerEvent::DurationChanged,
            E::Buffering(percent) => PlayerEvent::Buffering(percent),
            E::StateChanged {
                old,
                current,
                pending,
            } => PlayerEvent::StateChanged {
                old,
                current,
                pending,
            },
            E::RequestState(state) => PlayerEvent::RequestState(state),
            E::QueueSeek(seek) => PlayerEvent::QueueSeek(seek),
            E::StreamsSelected {
                video,
                audio,
                subtitle,
                seqnum,
            } => PlayerEvent::StreamsSelected {
                video,
                audio,
                subtitle,
                seqnum,
            },
            E::RefreshSeekFailed { seqnum } => PlayerEvent::SubtitleRefreshFailed { seqnum },
            E::RateChanged(rate) => PlayerEvent::RateChanged(rate),
            E::SeekFailed => PlayerEvent::SeekFailed,
            E::ClockLost => PlayerEvent::ClockLost,
            E::Error {
                origin,
                error,
                failed_uri,
            } => PlayerEvent::Error {
                origin,
                kind: MediaErrorKind::from_glib_error(&error),
                message: error.message().to_string(),
                failed_uri,
            },
            E::ExternalSubtitleFailed { id } => PlayerEvent::ExternalSubtitleFailed { id },
            E::SubtitleTrackUnsupported { sid, caps } => PlayerEvent::SubtitleTrackUnsupported {
                sid,
                caps: caps.to_string(),
            },
            E::ImageStream(s) => PlayerEvent::ImageStream(ImageStreamInfo {
                format: s.get::<&str>("format").unwrap_or("unknown").to_string(),
                width: s.get::<i32>("width").unwrap_or(0),
                height: s.get::<i32>("height").unwrap_or(0),
                animated: s.get::<bool>("animated").unwrap_or(false),
            }),
            E::SourceBackoff { remaining_ms } => PlayerEvent::SourceBackoff { remaining_ms },
            E::PreparedActivated => PlayerEvent::GaplessActivated,
            E::PreparedFailed { generation } => PlayerEvent::GaplessPrepareFailed { generation },
            E::PreparedCancelled { generation } => PlayerEvent::GaplessCancelled { generation },
            E::PreparedCancelDeclined { generation } => {
                PlayerEvent::GaplessCancelDeclined { generation }
            }
            E::Warning(message) => PlayerEvent::Warning(message),
        };
        msg_tx.player(event, Some(generation));
    }

    fn cleanup_stream_collection(&mut self) {
        if let Some(old_collection) = self.stream_collection.take()
            && let Some(sig_id) = self.stream_collection_notify.take()
        {
            old_collection.disconnect(sig_id);
        }
    }

    pub fn handle_stream_collection(&mut self, collection: gst::StreamCollection) {
        self.cleanup_stream_collection();

        let msg_tx = self.msg_tx.clone();
        self.stream_collection_notify = Some(collection.connect_stream_notify(
            None,
            move |_collection, _stream, param| {
                if param.name() == "tags" {
                    msg_tx.player(PlayerEvent::StreamTagsUpdated, None);
                }
            },
        ));

        // STABLE ORDER across collections of one load: a stream keeps its
        // position for as long as it is advertised, newcomers append. The
        // list position is the protocol/GUI track id, and decodebin3 does
        // NOT keep collection order stable when it rebuilds the collection
        // (an external subtitle attach can flip video/audio). Ids shifting
        // mid-item desynchronize the senders' TracksAvailable/TracksSelected
        // view: a TracksSelected relayed before the flip can never match a
        // TracksAvailable advertised after it unless a further selection
        // change happens to re-relay, which is exactly the stuck
        // track-state settle FAST used to flake on.
        self.streams = merge_streams_stable(std::mem::take(&mut self.streams), &collection);

        // The selection is stream-id-keyed, so nothing needs remapping across
        // collections: drop slots whose stream left the collection and seed
        // still-unselected slots with playbin3's defaults (the first stream
        // of each type), so a track change arriving before the initial
        // `StreamsSelected` keeps the other streams selected instead of
        // dropping them. The real `StreamsSelected` corrects these the moment
        // it arrives.
        self.selected.video = self
            .selected
            .video
            .take()
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .or_else(|| self.first_sid_of(gst::StreamType::VIDEO));
        self.selected.audio = self
            .selected
            .audio
            .take()
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .or_else(|| self.first_sid_of(gst::StreamType::AUDIO));
        self.selected.subtitle = self
            .selected
            .subtitle
            .take()
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .or_else(|| self.first_sid_of(gst::StreamType::TEXT));

        self.stream_collection = Some(collection);

        // The crate's selection engine already reconciled against this
        // collection (and abandoned unconfirmable in-flight work) when it
        // translated the message; give it a pump now that the receiver's
        // own bookkeeping is consistent too.
        self.pump_selection();
    }

    fn first_sid_of(&self, ty: gst::StreamType) -> Option<StreamId> {
        self.streams
            .iter()
            .find(|s| s.inner.stream_type().contains(ty))
            .and_then(|s| s.inner.stream_id())
            .map(|sid| sid.to_string())
    }

    /// The applied (or optimistically in-flight) stream id per slot.
    pub fn current_video_sid(&self) -> Option<&str> {
        self.selected.video.as_deref()
    }

    pub fn current_audio_sid(&self) -> Option<&str> {
        self.selected.audio.as_deref()
    }

    pub fn current_subtitle_sid(&self) -> Option<&str> {
        self.selected.subtitle.as_deref()
    }

    pub fn get_duration(&self) -> Option<gst::ClockTime> {
        self.fcast.duration()
    }

    pub fn get_position(&self) -> Option<gst::ClockTime> {
        self.fcast.position()
    }

    /// Buffered regions of the current media as timeline fractions, for the
    /// scrubber's buffered indicator. Empty when the source can't answer a
    /// buffering query (local file, live/SABR, pre-preroll).
    pub fn buffered_ranges(&self) -> Vec<fcastplaybin::BufferedRange> {
        self.fcast.buffered_ranges()
    }

    /// Inspector: full buffering state (fill percent, mode, rates, ranges).
    pub fn dbg_buffering(&self) -> Option<fcastplaybin::BufferingInfo> {
        self.fcast.buffering_info()
    }

    /// "Buffered ahead of the playhead" duration, for the scrubber's buffered
    /// nub in STREAM mode (where the buffering query reports no ranges).
    pub fn buffered_ahead(&self) -> Option<gst::ClockTime> {
        self.fcast.buffered_ahead()
    }

    fn clear_state(&mut self) {
        self.streams.clear();
        self.selected = TrackSelection::default();
        self.seekable = false;
        self.seekable_known = false;
        self.volume_confirm_in_flight = false;
        self.expected_generation = None;
        // A load or stop supersedes any pending pre-arm (fcastplaybin drops
        // the prepared input in its own reset).
        self.pending_gapless = None;
        // A volume queued behind an in-flight confirmation must not be
        // stranded by the load (volume is not item-scoped): apply it now
        // that nothing is in flight.
        if let Some(volume) = self.pending_volume.take() {
            self.set_volume(volume);
        }
        // Track desires reset inside fcastplaybin (they are per-item and it
        // owns the engine): its load reset and teardown both clear them.
    }

    /// Whether an event stamped with `generation` belongs to the current
    /// load. Everything else is a superseded load's straggler.
    pub fn is_event_current(&self, generation: u64) -> bool {
        self.expected_generation == Some(generation)
    }

    /// Pre-arm the next item on the live pipeline for a gapless transition
    /// (see `fcastplaybin::prepare_next_async`). Returns the generation the
    /// item carries once it activates; the application keeps it to validate
    /// the `GaplessActivated` event.
    pub fn prepare_next(&mut self, source: MediaInput) -> u64 {
        let generation = self.fcast.prepare_next_async(source);
        self.pending_gapless = Some(generation);
        generation
    }

    /// Ask the pipeline to drop a pending pre-armed next item (seek away from
    /// the end, queue mutation, stop). A no-op when nothing is pending.
    ///
    /// `pending_gapless` deliberately SURVIVES the request: the cancel races
    /// the pipeline's swap and a cancel that loses activates anyway
    /// (`GaplessCancelDeclined`), and that activation is only adoptable while
    /// the generation is still recorded here. The outcome clears it, through
    /// [`clear_pending_gapless`](Self::clear_pending_gapless) on a confirmed
    /// cancel or [`adopt_gapless_generation`](Self::adopt_gapless_generation)
    /// on the activation.
    pub fn cancel_prepared(&mut self) {
        if self.pending_gapless.is_some() {
            self.fcast.cancel_prepared_async();
        }
    }

    /// Adopt a gapless activation: events from `generation` are the current
    /// item's from here on. Per-item view state resets like a load's
    /// (selection re-seeds from the incoming collection, seekability is
    /// re-queried); transport, volume, and the state machine carry over
    /// untouched, the pipeline never left steady playback. Returns false
    /// for an activation that does not match the pending pre-arm (stale).
    pub fn adopt_gapless_generation(&mut self, generation: u64) -> bool {
        if self.pending_gapless != Some(generation) {
            return false;
        }
        self.pending_gapless = None;
        self.expected_generation = Some(generation);
        self.selected = TrackSelection::default();
        self.seekable = false;
        self.seekable_known = false;
        true
    }

    /// Clear the pre-arm bookkeeping without touching the pipeline (the
    /// prepare already failed, was confirmed cancelled, or was consumed
    /// elsewhere).
    pub fn clear_pending_gapless(&mut self) {
        self.pending_gapless = None;
    }

    /// Load a new main source (the crate resets to READY and wires it into
    /// decodebin3 on its worker thread. Completion comes back as
    /// `UriLoaded`). External subtitles attach separately as live inputs
    /// (`attach_external_subtitle`). Callers go through `load`.
    fn set_source(&mut self, source: MediaInput, start: fcastplaybin::StartPoint) {
        // A new item is not the old one's teardown: discards from here on are
        // about THIS load and count normally again.
        self.teardown.set(false);
        self.subtitle_flow.reset();
        self.clear_state();
        self.state_machine.clear_state();
        self.expected_generation = Some(self.fcast.load_async(source, start));
        self.state_machine.begin_load();
    }

    /// Load a new main source. `start` is the post-preroll start seek
    /// (`None` for live sources, no seek at all). Embedded text auto-selects
    /// and links itself inside `fcastplaybin`, nothing to sequence here.
    pub fn load(&mut self, source: MediaInput, start: Option<RestorePoint>) {
        // A new load auto-plays unless a pause arrives while it is in flight.
        self.desired_transport = RunningState::Playing;
        // The start position/rate is applied inside `fcastplaybin::load`
        // while the pipeline is still in PAUSED, so a non-1.0 rate never
        // renders a 1.0x slice that a later seek flushes (the pop). `None`
        // marks a source with no start seek (live sources).
        let start = match start {
            Some(rp) => fcastplaybin::StartPoint::Seek {
                position: rp.position,
                rate: rp.rate as f64,
            },
            None => fcastplaybin::StartPoint::Live,
        };
        self.set_source(source, start);
    }

    fn seek_internal(&mut self, seek: Seek) {
        if let Some(rate) = seek.rate
            && !Seek::rate_is_safe(rate)
        {
            warn!(rate, "Ignoring invalid seek rate");
            return;
        }

        // An unresolved seekability query (`!seekable_known`) is not a
        // refusal: let the seek through. The state machine queues seeks that
        // land mid-preroll, so it runs once the pipeline settles. Only a
        // KNOWN unseekable stream drops the seek.
        if self.seekable || !self.seekable_known {
            // A user seek is itself a flushing seek and re-emits the current
            // subtitle cue, a separately queued refresh flush is redundant.
            self.fcast.cancel_selection_refresh();
            if let Some(seek) = self.state_machine.seek_internal(seek, None) {
                self.fcast.seek_async(seek);
            }
        } else {
            warn!(?seek, "Attempted to seek on a non seekable stream");
        }
    }

    pub fn seek(&mut self, position: gst::ClockTime) {
        self.seek_internal(Seek {
            position: Some(position),
            rate: None,
        });
    }

    /// The freeze watchdog's recovery seek: a FLUSHING, ACCURATE seek to the
    /// pipeline's current position at the current rate, performed IN PLACE
    /// (no transport change). Returns the fresh seqnum it is stamped with, so
    /// a refusal report can be attributed to it.
    ///
    /// Deliberately not [`Self::seek`]. That path refuses unless the pipeline
    /// is settled at PAUSED and drives it there first (`Job::Seek` in
    /// fcastplaybin), and a starved pipeline can NEVER complete a
    /// PLAYING->PAUSED transition: it needs a buffer to preroll with and none
    /// will arrive, so every sink returns ASYNC and stays there
    /// (`gstbasesink.c:5815-5834`, `needs_preroll` at `:3749`). The seek would
    /// park forever waiting for a settled-PAUSED edge, and a parked seek
    /// silences the very progress tick the watchdog escalates from. The
    /// crate's refresh seek is the one API that sends the flush in place
    /// (FREEZE-DIAGN.md section 6: the FLUSH flag is mandatory and the seqnum
    /// must be fresh or the demuxer drops the seek).
    pub fn freeze_recovery_seek(&self) -> gst::Seqnum {
        let seqnum = gst::Seqnum::next();
        self.fcast.refresh_seek_async(seqnum);
        seqnum
    }

    fn applied_track_selection(&self) -> TrackSelection {
        self.selected.clone()
    }

    /// Handle a track-change request. Sequencing (latest-wins composition,
    /// serialization against in-flight work, confirmation, re-assertion
    /// when decodebin3's auto-select stomps it, the switch's re-emit flush
    /// and its hazards) all lives in fcastplaybin's selection engine; this
    /// only states the desire and pumps. Returns whether the currently
    /// displayed subtitle cue became stale. The caller should clear the
    /// overlay so the change registers visually, even while paused.
    pub fn request_track_change(&mut self, kind: TrackKind, sid: Option<StreamId>) -> bool {
        let applied = self.applied_track_selection();
        let stale_cue =
            kind == TrackKind::Subtitle && applied.subtitle.is_some() && sid != applied.subtitle;
        let slot = match kind {
            TrackKind::Video => fcastplaybin::TrackSlot::Video,
            TrackKind::Audio => fcastplaybin::TrackSlot::Audio,
            TrackKind::Subtitle => fcastplaybin::TrackSlot::Subtitle,
        };
        self.fcast
            .request_track(slot, fcastplaybin::TrackTarget::Stream(sid));
        self.pump_selection();
        stale_cue
    }

    /// Ask for an attached external subtitle input's stream, before or
    /// after it materializes in a collection: the engine parks the desire
    /// until the stream is advertised, then selects it and re-asserts it
    /// against decodebin3's collection-default auto-select. Replaces the
    /// application's parked-desire enforcement.
    pub fn request_external_subtitle(&mut self, handle: fcastplaybin::ExternalSubId) {
        self.fcast.request_track(
            fcastplaybin::TrackSlot::Subtitle,
            fcastplaybin::TrackTarget::ExternalSubtitle(handle),
        );
        self.pump_selection();
    }

    /// Dispatch pending track work now that the pipeline may have settled.
    /// Called from the state-change handler (a re-preroll finishing is what
    /// unblocks work parked behind it). The pump is otherwise driven event-
    /// driven: a new request, `streams_selected`, `async_done`, buffering
    /// completion, collection changes and refresh failure, no periodic
    /// poll.
    pub fn poll_track_ops(&mut self) {
        self.pump_selection();
    }

    /// Let the selection engine act, under the transport gate only this
    /// side knows (see `fcastplaybin::SelectionGate`).
    fn pump_selection(&mut self) {
        // Ask the pipeline whether an async state change (re-preroll, seek
        // preroll) is in progress instead of predicting from the kind of
        // change, mispredictions are what used to wedge this logic.
        let async_busy = self.fcast.has_async_transition();
        let (running, paused) = match self.state_machine.running() {
            Some(state) => (true, state == RunningState::Paused),
            None => (false, false),
        };
        self.fcast.pump_selection(fcastplaybin::SelectionGate {
            quiet: running && !async_busy,
            paused,
            seekable: self.seekable,
        });
    }

    /// A top-level `ASYNC_DONE`: the pipeline has re-prerolled and settled.
    pub fn async_done(&mut self) {
        // A flush (e.g. the subtitle re-emit) has re-prerolled and the pipeline
        // is settled again. If a subtitle switch happened while paused, its new
        // text branch may still be parked (it routed mid-flush, when the
        // pipeline wasn't settled), link it now that we're steady so the
        // re-emit's cue actually composites onto the frozen frame.
        self.fcast.poll_text_policy();
        // The in-flight refresh seek, if any, was settled by the crate when
        // it translated this ASYNC_DONE; dispatch whatever was parked
        // behind it.
        self.pump_selection();
    }

    /// The refresh seek job could not perform its seek (already recorded by
    /// the crate; this is the pump trigger).
    pub fn subtitle_refresh_failed(&mut self, _seqnum: gst::Seqnum) {
        self.pump_selection();
    }

    pub fn is_seeking(&self) -> bool {
        self.state_machine.is_seeking()
    }

    pub fn queue_seek(&mut self, seek: Seek) {
        self.state_machine.queue_seek(seek);
    }

    /// The current volume: the queued pending request when one exists (it
    /// is the receiver's newest intent), otherwise the playbin's live
    /// value. For seeding a newly connected sender's state.
    pub fn volume(&self) -> f32 {
        match self.pending_volume {
            Some(volume) => volume.clamp(0.0, 1.0),
            None => self.fcast.volume() as f32,
        }
    }

    /// Set the volume. The value itself lives in the playbin
    /// (`FcastPlaybin::set_volume`). What stays here is the receiver's
    /// confirmation protocol: senders expect exactly one `VolumeChanged`
    /// per request, so overlapping requests are queued (latest wins) and an
    /// idempotent set re-emits its confirmation.
    pub fn set_volume(&mut self, volume: f32) {
        if self.volume_confirm_in_flight {
            // A previous change's confirmation is still in flight. Don't
            // drop the request (the sender would wait forever for its
            // confirmation). Remember the latest and apply it once the
            // confirmation arrives.
            debug!(volume, "Volume change pending; queueing");
            self.pending_volume = Some(volume);
            return;
        }

        let target = (volume as f64).clamp(0.0, 1.0);
        if (self.fcast.volume() - target).abs() < 1e-9 {
            // Setting the property to its current value emits no notify,
            // but senders expect a confirmation for an idempotent set too.
            // Re-emit it manually through the same VolumeChanged path.
            debug!(volume, "Volume unchanged; re-emitting the confirmation");
            self.fcast.renotify_volume();
            return;
        }

        self.fcast.set_volume(target);
        self.volume_confirm_in_flight = true;
    }

    pub fn volume_changed(&mut self) {
        self.volume_confirm_in_flight = false;
        // Apply the newest request that arrived while the confirmation was
        // in flight (last one wins).
        if let Some(volume) = self.pending_volume.take() {
            self.set_volume(volume);
        }
    }

    pub fn set_rate(&mut self, rate: f32) {
        self.seek_internal(Seek {
            position: None,
            rate: Some(rate),
        });
    }

    pub fn update_media_info(&mut self) {
        if let Some(seekable) = self.fcast.query_seekable() {
            let dur = self.get_duration();
            debug!(?dur, seekable, "Seek query returned");
            self.seekable = seekable && dur.is_some();
            self.seekable_known = true;
        }
    }

    fn set_state_async(&self, target_state: gst::State) {
        self.fcast.set_state_async(target_state);
    }

    pub fn play(&mut self) {
        self.desired_transport = RunningState::Playing;
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Playing) {
            self.set_state_async(state);
        }
    }

    /// Honor a `RequestState` message from an element by dispatching the state
    /// change to the worker thread (off the streaming thread it arrived on).
    pub fn request_state(&self, state: gst::State) {
        self.set_state_async(state);
    }

    /// Handle `ClockLost`: the element providing the pipeline clock went away
    /// (typically the audio sink after the audio track was deselected).
    pub fn recover_clock(&mut self) {
        if !matches!(self.player_state(), PlayerState::Playing) {
            debug!("Ignoring clock loss while not playing");
            return;
        }
        debug!("Pipeline clock lost; cycling through Paused to elect a new one");
        self.fcast.recover_clock_async();
    }

    /// Produce a graph snapshot of the pipeline for the inspector, delivered
    /// via `done`. Runs on the fcastplaybin worker so the graph walk is
    /// serialized against loads and teardowns (the walk reads every
    /// element's properties, and racing the per-load audio sink's finalize
    /// double-freed in the sink back when this was a dot dump). `done` is
    /// invoked on the worker thread: hand the work off, do not block in it.
    pub fn request_graph_snapshot(
        &self,
        done: impl FnOnce(fcastplaybin::graph::GraphSnapshot) + Send + 'static,
    ) {
        self.fcast.debug_graph_async(Box::new(done));
    }

    pub fn pause(&mut self) {
        self.desired_transport = RunningState::Paused;
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Paused) {
            self.set_state_async(state);
        }
    }

    fn go_to_stopped_state(&mut self, null: Option<oneshot::Sender<()>>) {
        self.desired_transport = RunningState::Playing;
        // BEFORE the teardown is dispatched, not after: the discards this
        // suppresses land within ~1 ms of the job, on the streaming threads,
        // while this function is still running. Cleared by the next `load`.
        self.teardown.set(true);
        self.cleanup_stream_collection();

        // A full teardown either way (pipeline down, inputs and the per-load
        // audio sink removed), so a Stop releases the item's network/audio
        // resources NOW rather than at the next load. Queued on the worker,
        // it also aborts an in-flight load cleanly (jobs are ordered).
        match null {
            Some(feedback) => self.fcast.shutdown_async(Box::new(move || {
                debug!(res = ?feedback.send(()), "Sent shutdown feedback signal");
            })),
            None => {
                // Don't raise an already shut-down pipeline back to READY.
                if self.state_machine.current_state != gst::State::Null {
                    self.fcast.stop_async();
                }
            }
        }

        // Unconditional: even when the pipeline needed no state change (a
        // stop landing mid-load, with the pipeline still at READY), the
        // machine and the per-item state must reset or the aborted load's
        // leftovers leak into the next one.
        self.state_machine.clear_state();
        self.clear_state();
    }

    /// The subtitle track that took a FLUSHING discard and has delivered
    /// nothing since, reported at most once per load. `None` normally.
    ///
    /// Polled from the application's tick rather than pushed from the bus
    /// hook, because the verdict needs elapsed time and the hook may not have
    /// any (see [`SubtitleFlow`]).
    pub fn stalled_subtitle_stream(&self) -> Option<String> {
        self.subtitle_flow.stalled_stream()
    }

    pub fn stop(&mut self) {
        debug!("Stopping playback");
        self.go_to_stopped_state(None)
    }

    pub fn shutdown(&mut self, feedback: oneshot::Sender<()>) {
        debug!("Shutting down player");
        self.go_to_stopped_state(Some(feedback));
    }

    /// Returns `true` if any stream has new properties.
    pub fn update_stream_properties(&mut self) -> bool {
        let mut did_change = false;

        for stream in &mut self.streams {
            let title = stream_title(&stream.inner);
            if title != stream.title {
                stream.title = title;
                did_change = true;
            }
        }

        did_change
    }

    /// The index of the stream with this GStreamer stream id, if advertised.
    pub fn stream_idx_by_id(&self, sid: &str) -> Option<u32> {
        Self::find_stream_idx(sid, &self.streams)
    }

    /// Cumulative parsed-byte counters per live input stream, for the
    /// inspector's bitrate sampling (poll and diff; see
    /// `fcastplaybin::StreamIoStats`). All of the item's streams are counted,
    /// selected or not; correlate with `streams`/`current_*_sid` for kind and
    /// selection.
    pub fn stream_io_stats(&self) -> Vec<fcastplaybin::StreamIoStats> {
        self.fcast.stream_io_stats()
    }

    /// Inspector: every advertised stream plus whether it is currently
    /// selected, for the track table (`gst::Stream` clones are refcounted).
    pub fn stream_dbg_rows(&self) -> Vec<(gst::Stream, bool)> {
        self.streams
            .iter()
            .map(|s| {
                let sid = s.inner.stream_id().map(|id| id.to_string());
                let selected = sid.is_some()
                    && [
                        &self.selected.video,
                        &self.selected.audio,
                        &self.selected.subtitle,
                    ]
                    .into_iter()
                    .any(|sel| *sel == sid);
                (s.inner.clone(), selected)
            })
            .collect()
    }

    /// Inspector: pipeline current + pending state.
    pub fn dbg_state_summary(&self) -> (gst::State, gst::State) {
        self.fcast.state_summary()
    }

    /// Inspector: "kind:pad" for every routed decodebin3 stream.
    pub fn dbg_routed_summary(&self) -> Vec<String> {
        self.fcast.routed_summary()
    }

    /// Inspector: every live input's factory and uri.
    pub fn dbg_sources(&self) -> Vec<fcastplaybin::SourceDbg> {
        self.fcast.source_summaries()
    }

    /// Inspector: elements with an unfinished state transition.
    pub fn dbg_unsettled_elements(&self) -> Vec<String> {
        self.fcast.unsettled_elements()
    }

    /// Inspector: the video sink's rendered/dropped buffer counts.
    pub fn dbg_video_sink_stats(&self) -> Option<gst::Structure> {
        self.fcast.video_sink_stats()
    }

    /// Inspector: the audio sink's negotiated caps and rendered/dropped
    /// counts, while a per-load sink exists.
    pub fn dbg_audio_sink_health(&self) -> Option<(Option<gst::Caps>, Option<gst::Structure>)> {
        self.fcast.audio_sink_health()
    }

    /// Inspector: the generation the player currently accepts events from.
    pub fn dbg_generation(&self) -> Option<u64> {
        self.expected_generation
    }

    /// Whether the pipeline is settled, meaning no async state transition
    /// is in progress (non-blocking query). Used to hold flushing operations
    /// off while a reconfiguration that posts no bus signal of its own is
    /// still in flight.
    pub fn is_pipeline_stable(&self) -> bool {
        self.fcast.is_settled()
    }

    /// Whether an async state change is in progress (a re-preroll, a flushing
    /// seek's preroll). NOT the complement of
    /// [`is_pipeline_stable`](Self::is_pipeline_stable): a flushing seek in
    /// PLAYING re-prerolls with `pending` still VoidPending, so only this one
    /// sees it.
    pub fn has_async_transition(&self) -> bool {
        self.fcast.has_async_transition()
    }

    /// Diagnostic (load-stall investigation): explain why a load has not
    /// reached a steady PAUSED. Logs the pipeline's current+pending state, the
    /// media's stream collection kinds vs the decodebin3 pads actually routed
    /// (a selected stream kind with no matching routed pad is the stall), and
    /// dumps a pipeline `.dot` (needs `GST_DEBUG_DUMP_DOT_DIR`).
    pub fn log_load_stall_diagnostics(&self, tag: &str) {
        let (current, pending) = self.fcast.state_summary();
        let collection: Vec<&'static str> = self
            .streams
            .iter()
            .map(|s| {
                let t = s.inner.stream_type();
                if t.contains(gst::StreamType::VIDEO) {
                    "video"
                } else if t.contains(gst::StreamType::AUDIO) {
                    "audio"
                } else if t.contains(gst::StreamType::TEXT) {
                    "text"
                } else {
                    "other"
                }
            })
            .collect();
        let routed = self.fcast.routed_summary();
        let elements = self.fcast.element_states();
        warn!(
            tag,
            ?current,
            ?pending,
            collection = ?collection,
            routed = ?routed,
            elements = ?elements,
            "LOAD STALL DIAGNOSTIC: pipeline not steady"
        );
        self.fcast.dump_dot(&format!("load-stall-{tag}"));
    }

    /// Dump the pipeline as a `.dot` graph. A no-op unless
    /// `GST_DEBUG_DUMP_DOT_DIR` is set.
    pub fn dump_dot(&self, name: &str) {
        self.fcast.dump_dot(name);
    }

    /// The GStreamer stream id of the `idx`th advertised stream.
    pub fn stream_id_of(&self, idx: u32) -> Option<String> {
        self.streams
            .get(idx as usize)?
            .inner
            .stream_id()
            .map(|id| id.to_string())
    }

    pub fn is_stream_of_type(&self, idx: u32, ty: gst::StreamType) -> bool {
        self.streams
            .get(idx as usize)
            .is_some_and(|s| s.inner.stream_type().contains(ty))
    }

    pub fn end_of_stream_reached(&mut self) {
        self.stop();
    }

    pub fn uri_loaded(&mut self) {
        // The load is wired (and usually still prerolling). Commit the
        // transport the user last asked for: Playing unless a pause landed
        // while the load was in flight. This is the ONE post-load transport
        // driver. A load whose user already paused never blips through
        // Playing at all.
        let desired = self.desired_transport;
        if let Some(state) = self.state_machine.set_playback_state(desired) {
            self.set_state_async(state);
        } else if self.state_machine.running() != Some(desired) {
            // The machine could not act on it, typically because the load's
            // preroll has not settled yet (Loaded arrives when the load job
            // returns, before the async climb finishes). Drive the pipeline
            // directly; the machine follows the state edges as always.
            self.set_state_async(desired.into());
        }
    }

    /// Returns `true` if buffering completed
    pub fn buffering(&mut self, percent: i32) -> bool {
        let res = match self.state_machine.buffering(percent) {
            BufferingStateResult::Started(state) => {
                self.set_state_async(state);
                false
            }
            BufferingStateResult::Buffering => false,
            BufferingStateResult::FinishedWithSeek(seek) => {
                debug!("Buffering finished, dispatching seek");
                self.fcast.seek_async(seek);
                true
            }
            BufferingStateResult::FinishedButWaitingSeek => {
                debug!("Buffering finished with seek");
                true
            }
            BufferingStateResult::Finished(state) => {
                debug!("Buffering finished");
                if let Some(state) = state {
                    self.set_state_async(state);
                }
                true
            }
        };

        // Buffering completion can settle the pipeline, dispatch queued track
        // work (no-op while still buffering: the machine is not `Running`).
        self.pump_selection();

        res
    }

    /// Live-attach an external subtitle input to the running pipeline.
    /// Returns the reserved id immediately, the attach itself runs on the
    /// playbin's worker thread (the source's `start()` blocks). The stream
    /// becomes selectable once decodebin3 announces the updated collection
    /// (always a later collection, mapped back with
    /// `external_stream_sid_of`). fcastplaybin babysits the input from
    /// here: deselect-race deaths recover in place under the same id, and
    /// a genuine failure (failed attach, error while shown, or no stream
    /// within its watchdog) comes back as
    /// `PlayerEvent::ExternalSubtitleFailed` with the input already
    /// detached.
    pub fn attach_external_subtitle(&mut self, url: &str) -> fcastplaybin::ExternalSubId {
        let id = self.fcast.allocate_subtitle_id();
        self.fcast.attach_subtitle_async(id, url.to_string());
        id
    }

    /// Detach a live external subtitle input (failed URL, or its catalog
    /// entry going away). Best effort, on the playbin's worker thread. The
    /// input is leaving regardless.
    pub fn detach_external_subtitle(&mut self, id: fcastplaybin::ExternalSubId) {
        self.fcast.detach_subtitle_async(id);
    }

    /// The GStreamer stream id of an attached external subtitle input, once
    /// its stream has appeared in the advertised collection. The id is
    /// URI-derived and therefore STABLE for the input's lifetime, so
    /// callers should remember it rather than re-query.
    pub fn external_stream_sid_of(&self, id: fcastplaybin::ExternalSubId) -> Option<String> {
        let sids = self.fcast.subtitle_stream_ids(id);
        let sid = sids
            .into_iter()
            .find(|sid| Self::find_stream_idx(sid, &self.streams).is_some());
        debug!(?id, ?sid, "external subtitle stream lookup");
        sid
    }

    pub fn state_changed(
        &mut self,
        old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> Option<PlaybackState> {
        // A state change is the settle event for the crate's text link
        // policy: parked text may join its renderer only once the
        // pipeline is SETTLED >= PAUSED, and this callback fires exactly
        // when that can newly hold (the crate re-checks, cheap no-op
        // otherwise).
        self.fcast.poll_text_policy();
        // Queued track work is deliberately NOT pumped from here: the
        // application runs this at the START of its StateChanged handling,
        // and a Playing commit's cascade may still launch a restore seek. A
        // selection dispatched into that one-instant-quiet window
        // interleaves with the seek's Playing->Paused->seek->Playing dance
        // and its reconfigure runs outside steady PLAYING (a parked
        // video-disable dispatched at the commit once wedged the pipeline
        // for good). The application pumps at the END of the cascade
        // instead, when the seek, if any, already owns the state machine.
        match self.state_machine.state_changed(old, new, pending) {
            // Map the backend-native playback state onto the FCast wire enum
            // (fcastplaybin is protocol-agnostic, this is the only seam).
            StateChangeResult::NewPlaybackState(new_state) => {
                use fcastplaybin::state_machine::PlaybackState as SmState;
                Some(match new_state {
                    SmState::Idle => PlaybackState::Idle,
                    SmState::Paused => PlaybackState::Paused,
                    SmState::Playing => PlaybackState::Playing,
                })
            }
            StateChangeResult::Seek(seek) => {
                self.fcast.seek_async(seek);
                None
            }
            StateChangeResult::Waiting => None,
            StateChangeResult::ChangeState(state) => {
                self.set_state_async(state);
                None
            }
        }
    }

    pub fn have_media_info(&self) -> bool {
        !self.streams.is_empty()
    }

    fn find_stream_idx(sid: &str, streams: &[Stream]) -> Option<u32> {
        for (idx, stream) in streams.iter().enumerate() {
            if let Some(this_id) = stream.inner.stream_id()
                && this_id == sid
            {
                return Some(idx as u32);
            }
        }

        None
    }

    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    pub fn streams_selected(
        &mut self,
        video_sid: Option<&str>,
        audio_sid: Option<&str>,
        subtitle_sid: Option<&str>,
        seqnum: gst::Seqnum,
    ) -> TrackSelection {
        debug!(?video_sid, ?audio_sid, ?subtitle_sid, ?seqnum);

        self.fcast.poll_text_policy();

        // Adopt what the pipeline reports as applied, verbatim (stream ids
        // need no index mapping). The engine's confirmation/overtake logic
        // already ran when the crate translated this message; this mirror
        // only serves the protocol/GUI reads.
        self.selected = TrackSelection {
            video: video_sid.map(str::to_string),
            audio: audio_sid.map(str::to_string),
            subtitle: subtitle_sid.map(str::to_string),
        };

        // Dispatch the next queued operation now that this one confirmed. A
        // plain switch (subtitle, or an audio/video switch between already-
        // decoded streams) applies with no re-preroll and so posts no further
        // bus message, this is the event that advances the queue for it. If
        // the switch DID trigger a re-preroll, the pump's quiet gate (it
        // queries the pipeline's async state) holds the next op back until
        // the ASYNC_DONE/state-change handler pumps again, so this never
        // dispatches into a re-preroll.
        self.pump_selection();

        self.selected.clone()
    }

    pub fn player_state(&self) -> PlayerState {
        if self.state_machine.is_stopped() {
            return PlayerState::Stopped;
        }
        match self.state_machine.running() {
            Some(RunningState::Paused) => PlayerState::Paused,
            Some(RunningState::Playing) => PlayerState::Playing,
            // The wire protocol has no loading/seeking state. Buffering is
            // the honest "not rendering, working on it" for everything in
            // transition.
            None => PlayerState::Buffering,
        }
    }

    /// The player state projected onto the v3 wire enum (Idle/Playing/Paused,
    /// no Buffering variant), for progress broadcasts. See
    /// [`project_wire_state`] for why a transient Buffering must not become
    /// Idle.
    pub fn wire_playback_state(&self) -> PlaybackState {
        project_wire_state(self.player_state(), self.desired_transport)
    }

    pub fn is_live(&self) -> bool {
        self.state_machine.is_live
    }

    pub fn set_is_live(&mut self, live: bool) {
        self.state_machine.is_live = live;
    }

    pub fn rate(&self) -> f64 {
        self.state_machine.rate
    }

    #[instrument(skip_all)]
    pub fn seek_failed(&mut self) {
        if let Some(target_state) = self.state_machine.seek_failed() {
            debug!(?target_state);
            self.set_state_async(target_state);
        }
    }

    pub fn set_rate_changed(&mut self, rate: f64) {
        self.state_machine.rate = rate;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The playbin's worker exits on its own once the last handle drops.
        // Queue the final teardown (usually a no-op, `shutdown` already
        // drove the pipeline to Null and waited).
        self.set_state_async(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Put a test's registry where the shipped binary's is on the subtitle
    /// path, which `init_for_tests` does not.
    ///
    /// `init_and_load_plugins` (gstreamer.rs:76-95) registers the Rust
    /// subtitle parsers and swaps ranks so decodebin3 autoplugs them over the
    /// C ones - and only `rssubparse`/`rsssaparse` put the driver on its
    /// cue-IR path (`Player::new`'s deep-element hook matches nothing else). A
    /// probe without this exercises a different parser from the one every
    /// field report comes from. Done here rather than in `init_for_tests`
    /// because a global rank swap would change parser selection for every
    /// other test in the crate.
    fn register_shipped_subtitle_parsers() {
        gstrssubparse::plugin_register_static().expect("registering the rust subtitle parsers");
        use gst::prelude::PluginFeatureExtManual;
        let registry = gst::Registry::get();
        for c_name in ["subparse", "ssaparse"] {
            if let Some(feature) = registry.lookup_feature(c_name) {
                feature.set_rank(gst::Rank::NONE);
            }
        }
        for rs_name in ["rssubparse", "rsssaparse"] {
            if let Some(feature) = registry.lookup_feature(rs_name) {
                feature.set_rank(gst::Rank::PRIMARY);
            }
        }
        // The audio sink the receiver picks on Linux, chosen inside
        // `Player::new` and instantiated per load.
        #[cfg(target_os = "linux")]
        let _ = fcast_gst_elements::pwaudiosink::plugin_init();
    }

    /// FULL-STACK field triage: the same default-selected-subtitle probe the
    /// driver suite runs, but through `Player` - its selection policy, its
    /// consumer install order, the REAL `FSink`, and the REAL `CueEngine` the
    /// sink owns. Everything the shipped receiver has except the GUI: no
    /// window, no GL, no `frame-available`/`overlays-changed` signal
    /// consumers, and no `render-delay` feedback (that lives in `receiver-ui`
    /// and needs a repaint loop to produce a cost to feed back).
    ///
    /// The point is the DELTA against `dash_testbed`'s
    /// `probe_default_subtitle_on_a_live_uri`, which drives the driver alone
    /// with test sinks and does NOT reproduce the field's dead track. If the
    /// full stack reproduces where the driver did not, the difference is in
    /// this file or in the sink's preroll, and the bisect starts here.
    ///
    /// The event pump replicates the minimum the application does to make a
    /// load progress (`application.rs`: `UriLoaded` -> `uri_loaded`,
    /// `RequestState` -> `request_state`, `QueueSeek` -> `queue_seek`,
    /// `Buffering` -> `buffering`, plus `poll_track_ops` per tick). A missing
    /// one shows up as a load that never reaches PLAYING, not as a subtle
    /// difference, so this is self-checking: the assertion below requires
    /// video to be moving before it says anything about subtitles.
    #[test]
    #[ignore = "field triage: set FCAST_PROBE_URI to a live DASH uri"]
    fn probe_default_subtitle_through_the_player() {
        let uri = std::env::var("FCAST_PROBE_URI").expect("set FCAST_PROBE_URI");
        crate::gstreamer::init_for_tests();

        // MATCH THE SHIPPED AUTOPLUG, or this probe tests a pipeline the
        // receiver never builds. `init_for_tests` registers one element;
        // `init_and_load_plugins` (gstreamer.rs:76-95) additionally registers
        // the Rust subtitle parsers and swaps ranks so decodebin3 picks
        // `rssubparse` over the C `subparse` - which is what puts the driver
        // on its cue-IR path (`Player::new`'s deep-element hook only matches
        // `rssubparse`/`rsssaparse`). Without this the probe autoplugs the C
        // parser and exercises a different subtitle path from the one the
        // field report comes from. Done here rather than in `init_for_tests`
        // because a global rank swap would change parser selection for every
        // other test in the crate.
        register_shipped_subtitle_parsers();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = fcast_video::video::FSink::new();
        let engine = sink.cue_engine();
        engine.set_canvas(1280, 720);
        let mut player = Player::new(
            Some(sink.clone().upcast()),
            Some(engine.clone()),
            MessageSender::new(tx),
            crate::fcompsrc::imp::CompContext(crate::fcast::CompanionContext::new()),
        )
        .expect("building the player");

        player.load(MediaInput::Uri(uri), None);

        let deadline = Instant::now() + Duration::from_secs(75);
        let mut overlays_seen = 0usize;
        let mut had_overlay = false;
        let mut events = 0usize;
        while Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                let crate::message::Message::NewPlayerEvent { event, .. } = msg else {
                    continue;
                };
                events += 1;
                match event {
                    PlayerEvent::UriLoaded => player.uri_loaded(),
                    PlayerEvent::RequestState(state) => player.request_state(state),
                    PlayerEvent::QueueSeek(seek) => player.queue_seek(seek),
                    PlayerEvent::Buffering(percent) => {
                        player.buffering(percent);
                    }
                    _ => {}
                }
            }
            player.poll_track_ops();
            // A cue is on screen when the engine has an overlay for now. Edges
            // are counted, not polls: one cue held across many ticks is one
            // cue.
            let now_overlay = !engine.current_overlays().is_empty();
            if now_overlay && !had_overlay {
                overlays_seen += 1;
            }
            had_overlay = now_overlay;
            if let Some(stream) = player.stalled_subtitle_stream() {
                eprintln!("PLAYER PROBE stalled subtitle stream: {stream}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let position = player.get_position();
        eprintln!(
            "PLAYER PROBE RESULT overlays={overlays_seen} position={position:?} \
             state={:?} events={events}",
            player.player_state()
        );
        player.stop();
        assert!(
            position.is_some_and(|p| p > gst::ClockTime::from_seconds(10)),
            "playback never got going ({position:?}), so this says nothing about subtitles"
        );
        assert!(
            overlays_seen > 0,
            "the default-selected text track put no cue on screen"
        );
    }

    /// THE OBSTRUCTION STAGING, driven at the mechanism rather than at the
    /// correlate.
    ///
    /// The field correlates the subtitle discard with a video-obstruction
    /// transition (2 for 2, ~400 ms and ~900 ms after `obstructed=true`). The
    /// handler itself cannot be the cause - it terminates in
    /// `wl_subsurface.place_above`/`place_below` plus a Wayland connection
    /// flush and touches no GStreamer object (`fcast-video/src/wayland_sink.rs
    /// :1362`, and the trait default at `video_sink.rs:34` is an empty body).
    /// Its SECOND-ORDER path does reach the pipeline: a restack flips
    /// `self_clocked()`, that changes which render path runs and therefore the
    /// measured render cost, and `note_render_cost` pushes the new cost to the
    /// sink and posts a LATENCY message (`receiver-ui/src/lib.rs:160-166`),
    /// which makes the whole pipeline recalculate latency. The field log shows
    /// `RecalculateLatency` tracking the obstruction transitions.
    ///
    /// So this drives THAT, exactly as `note_render_cost` does, at controlled
    /// offsets into the item's life - including mid-bring-up, while the text
    /// branch is still being constructed. Calling `set_video_obstructed` here
    /// would be theatre: headless there is no subsurface and the default body
    /// does nothing.
    ///
    /// Env-driven so the matrix runs without recompiling:
    ///   `FCAST_PROBE_URI`            the item (required)
    ///   `FCAST_PROBE_EXTERNAL`       attach this external subtitle FIRST, so
    ///                                the item under test is the second
    ///                                urisourcebin, as in the field
    ///   `FCAST_PROBE_LATENCY_AT_MS`  comma-separated offsets from PLAYING at
    ///                                which to fire the latency feedback
    ///   `FCAST_NO_RENDER_DELAY_FEEDBACK` the A/B: suppresses the firing
    #[test]
    #[ignore = "field triage: set FCAST_PROBE_URI (see the doc comment for the matrix)"]
    fn probe_obstruction_latency_against_a_live_uri() {
        let uri = std::env::var("FCAST_PROBE_URI").expect("set FCAST_PROBE_URI");
        let offsets: Vec<u64> = std::env::var("FCAST_PROBE_LATENCY_AT_MS")
            .unwrap_or_else(|_| "500,1500,3000,6000".to_owned())
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        let feedback_on = std::env::var_os("FCAST_NO_RENDER_DELAY_FEEDBACK").is_none();
        crate::gstreamer::init_for_tests();
        register_shipped_subtitle_parsers();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = fcast_video::video::FSink::new();
        let engine = sink.cue_engine();
        engine.set_canvas(1280, 720);
        let mut player = Player::new(
            Some(sink.clone().upcast()),
            Some(engine.clone()),
            MessageSender::new(tx),
            crate::fcompsrc::imp::CompContext(crate::fcast::CompanionContext::new()),
        )
        .expect("building the player");

        // The external FIRST, so the item under test gets the second
        // urisourcebin exactly as the field session did.
        if let Ok(external) = std::env::var("FCAST_PROBE_EXTERNAL") {
            player.attach_external_subtitle(&external);
        }
        player.load(MediaInput::Uri(uri), None);

        let start = Instant::now();
        let deadline = start + Duration::from_secs(60);
        let mut fired = 0usize;
        let mut overlays_seen = 0usize;
        let mut had_overlay = false;
        while Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                let crate::message::Message::NewPlayerEvent { event, .. } = msg else {
                    continue;
                };
                match event {
                    PlayerEvent::UriLoaded => player.uri_loaded(),
                    PlayerEvent::RequestState(state) => player.request_state(state),
                    PlayerEvent::QueueSeek(seek) => player.queue_seek(seek),
                    PlayerEvent::Buffering(percent) => {
                        player.buffering(percent);
                    }
                    _ => {}
                }
            }
            player.poll_track_ops();

            // The obstruction's second-order consequence, at its offset.
            let elapsed = start.elapsed().as_millis() as u64;
            if feedback_on && fired < offsets.len() && elapsed >= offsets[fired] {
                use gst_video::prelude::BaseSinkExt;
                // A plausible render cost swing: the two render paths differ by
                // milliseconds, and it is the RECALCULATION this is probing,
                // not the number.
                let delay = if fired % 2 == 0 { 12 } else { 2 };
                sink.set_render_delay(gst::ClockTime::from_mseconds(delay));
                let _ = sink.post_message(gst::message::Latency::builder().src(&sink).build());
                eprintln!(
                    "OBSTRUCTION PROBE fired latency feedback #{fired} at {elapsed}ms (delay {delay}ms)"
                );
                fired += 1;
            }

            let now_overlay = !engine.current_overlays().is_empty();
            if now_overlay && !had_overlay {
                overlays_seen += 1;
            }
            had_overlay = now_overlay;
            if let Some(stream) = player.stalled_subtitle_stream() {
                eprintln!("OBSTRUCTION PROBE stalled subtitle stream: {stream}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let position = player.get_position();
        eprintln!(
            "OBSTRUCTION PROBE RESULT overlays={overlays_seen} fired={fired} \
             feedback_on={feedback_on} position={position:?}"
        );
        player.stop();
        assert!(
            position.is_some_and(|p| p > gst::ClockTime::from_seconds(10)),
            "playback never got going ({position:?}), so this says nothing about subtitles"
        );
        assert!(
            overlays_seen > 0,
            "the default-selected text track put no cue on screen"
        );
    }

    /// The REAL video sink runs headless, which is what makes a full-stack
    /// field-triage harness possible at all: `FSink` owns the cue engine
    /// (`receiver-ui` takes the engine OUT of the sink and hands both to the
    /// player, `receiver-ui/src/lib.rs:756-782`), and its `show_frame` only
    /// stores the newest frame behind a mutex, so nothing here needs a window,
    /// a GL context or placebo. The delta against the shipped binary is the UI
    /// render path and its two signal connections, not the sink itself.
    #[test]
    fn the_real_video_sink_and_its_cue_engine_build_headless() {
        crate::gstreamer::init_for_tests();
        let sink = fcast_video::video::FSink::new();
        let engine = sink.cue_engine();
        engine.set_canvas(1280, 720);
        let element: gst::Element = sink.upcast();
        assert_eq!(element.current_state(), gst::State::Null);
    }

    /// The verdict needs BOTH halves: a discard, and nothing delivered since.
    /// A discard alone is the transient the escalation was always willing to
    /// forgive.
    #[test]
    fn a_discard_followed_by_a_delivery_is_not_a_stalled_track() {
        let flow = SubtitleFlow::default();
        flow.note_discard("subtitle_00");
        // Backdate past the verdict window so only the delivery decides.
        {
            let mut d = flow.0.discard.lock().expect("discard");
            let (stream, mark, _) = d.take().expect("noted");
            *d = Some((stream, mark, Instant::now() - SUBTITLE_STALL_VERDICT * 2));
        }
        flow.delivered();
        assert_eq!(flow.stalled_stream(), None);
    }

    #[test]
    fn a_discard_with_nothing_delivered_since_is_reported_once() {
        let flow = SubtitleFlow::default();
        flow.delivered();
        flow.note_discard("subtitle_00");
        // Not yet: the window has not elapsed, so a sparse track is safe.
        assert_eq!(flow.stalled_stream(), None);
        {
            let mut d = flow.0.discard.lock().expect("discard");
            let (stream, mark, _) = d.take().expect("noted");
            *d = Some((stream, mark, Instant::now() - SUBTITLE_STALL_VERDICT * 2));
        }
        assert_eq!(flow.stalled_stream(), Some("subtitle_00".to_owned()));
        // ONCE: a per-tick repeat would be a log flood for a track that is
        // already gone for the item.
        assert_eq!(flow.stalled_stream(), None);
    }

    /// The FIRST discard is the one kept: its delivery mark is what "nothing
    /// since the track broke" is measured against, so a later discard must not
    /// move the goalposts forward.
    #[test]
    fn a_later_discard_does_not_reset_the_verdict_clock() {
        let flow = SubtitleFlow::default();
        flow.note_discard("subtitle_00");
        flow.note_discard("subtitle_01");
        let held = flow.0.discard.lock().expect("discard").clone();
        assert_eq!(held.map(|(s, _, _)| s), Some("subtitle_00".to_owned()));
    }

    #[test]
    fn a_load_rearms_the_verdict() {
        let flow = SubtitleFlow::default();
        flow.note_discard("subtitle_00");
        flow.reset();
        assert!(flow.0.discard.lock().expect("discard").is_none());
        assert_eq!(flow.0.delivered.load(Ordering::Relaxed), 0);
    }

    /// The driver's caps gate and the engine's decoder table each write down
    /// which bitmap subtitle formats are implemented, and they cannot import
    /// each other, since the dependency runs one way. This crate depends on
    /// both, so this is the only place the two claims can be compared, and a
    /// format that lands on one side alone dies here rather than in the field.
    ///
    /// Disagreement is not cosmetic in either direction: gate-yes/engine-no
    /// feeds packets to an engine that answers every one with a counted decode
    /// error and a blank screen, and gate-no/engine-yes leaves a format that
    /// can be drawn stuck behind `SubtitleTrackUnsupported`.
    #[test]
    fn the_drivers_implemented_set_and_the_engines_agree() {
        for format in fcastplaybin::BitmapSubFormat::ALL {
            assert_eq!(
                fcastplaybin::bitmap_format_implemented(format),
                fcast_video::subpic::implemented(bitmap_format(format)),
                "{format:?}: the driver's gate and the engine's decoder table disagree"
            );
        }
    }

    /// The bytes the driver's tests push are bytes a decoder can actually read.
    ///
    /// `fcasttest`'s bitmap fixtures are DELIBERATELY not the decoders' own:
    /// they are written from the specifications a second time, so that a
    /// transport test cannot pass because both ends share one author's
    /// misreading. The cost of that is a fixture nobody ever decodes: every
    /// driver-side test counts samples and never looks inside them, so a
    /// fixture that was malformed would prove the carriage works while
    /// proving nothing about what it carries. This is the join, and this
    /// crate is the only place it can be made, since `fcastplaybin` cannot
    /// depend on `fcast-video`.
    #[test]
    fn the_drivers_bitmap_fixtures_decode() {
        gst::init().unwrap();

        for (format, bytes, codec_data) in [
            (
                fcast_video::subpic::BitmapFormat::Pgs,
                fcasttest::pgs::display_set(0),
                None,
            ),
            (
                fcast_video::subpic::BitmapFormat::Vobsub,
                fcasttest::vobsub::subpicture_unit(0),
                Some(fcasttest::vobsub::SAMPLE_IDX.to_vec()),
            ),
            (
                fcast_video::subpic::BitmapFormat::Dvb,
                fcasttest::dvb::display_set(0),
                None,
            ),
        ] {
            let mut decoder = fcast_video::subpic::decoder_for(format)
                .unwrap_or_else(|| panic!("{format:?} has no decoder"));
            decoder.set_video_size(1920, 1080);
            if let Some(codec_data) = codec_data {
                decoder.set_codec_data(&codec_data);
            }
            let updates = decoder.push(&fcast_video::subpic::BitmapPacket {
                format,
                data: gst::Buffer::from_slice(bytes),
                codec_data: None,
                rt: gst::ClockTime::from_seconds(1),
                duration: None,
            });
            assert_eq!(
                decoder.take_decode_errors(),
                0,
                "{format:?}: the driver's fixture is malformed"
            );
            let update = updates
                .first()
                .unwrap_or_else(|| panic!("{format:?}: the driver's fixture drew nothing"));
            let region = update
                .regions
                .first()
                .unwrap_or_else(|| panic!("{format:?}: the driver's fixture drew no region"));
            assert_eq!(
                region.pixels.len(),
                region.width as usize * region.height as usize * 4,
                "{format:?}: the fixture decoded into a region that does not match its size"
            );
            assert!(
                region.pixels.chunks_exact(4).any(|pixel| pixel[3] != 0),
                "{format:?}: the fixture decoded into a fully transparent picture, which a                  sample-counting transport test would never have noticed"
            );
        }
    }

    /// THE FIELD DEFECT, END TO END: an embedded text track whose cues ABUT is
    /// carried by the real driver into the real engine, and no frame between
    /// the first cue's start and the last one's end comes out blank.
    ///
    /// This crate is the only place the join can be made (`fcastplaybin`
    /// cannot depend on `fcast-video`) and the join is the point. The
    /// engine's own `contiguous_cues_hand_over_without_a_blank_frame` pins
    /// the seam against cues handed straight to `submit`; this one makes
    /// the DRIVER produce them, through decodebin3, the stock
    /// `fpb-tqueue-*` and the consumer appsink, so a regression in how cues
    /// are timed or delivered fails here too.
    ///
    /// # The pacing, which is not symmetric and should not be
    ///
    /// Video is `Realtime`, because the wall-clock spacing between frames is
    /// half the mechanism: it is the ~33 ms per frame that gives the raster
    /// worker its chance, and a free-running video stream would evaluate the
    /// whole timeline inside one raster latency and blank on every frame.
    ///
    /// Text is deliberately NOT `Realtime`. `ftestsrc` paces BEFORE it pushes
    /// (`src_bin.rs`, `pace(duration)` ahead of the push), so a realtime-paced
    /// cue leaves the source at its own END time and reaches the engine already
    /// expired, which is a delivery failure and a different test from this
    /// one. Left
    /// free-running, the cues are throttled instead by the text queue's stock
    /// 1 s `max-size-time`, which counts the dead air between sparse cues; that
    /// is the same throttle the field file meets, where the text branch was
    /// measured running 1.2 s ahead of the clock through decodebin3.
    #[test]
    fn contiguous_cues_from_the_driver_never_blank_a_frame() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize},
                mpsc,
            },
            time::{Duration, Instant},
        };

        use fcast_video::cue::{CueEngine, CueInput, TextFormat};
        use fcastplaybin::{
            AudioSink, FcastPlaybin, PlaybinEvent, SelectionGate, Sinks, StartPoint,
            SubtitleFeedItem, TrackSlot, TrackTarget,
        };
        use fcasttest::{
            scenario::ScenarioBuilder,
            sink::FTestSink,
            spec::{CueSpec, Pacing, StreamSpec},
        };
        use parking_lot::Mutex;

        gst::init().unwrap();
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            fcasttest::register_for_tests();
            fcast_gst_elements::fcastaudiostretch::plugin_init()
                .expect("registering fcastaudiostretch");
        });

        // CONTIGUOUS, as measured on a real file: cue N ends on
        // the nanosecond cue N+1 starts, so there is no gap for a frame to fall
        // into and every seam is a handover.
        // The run starts at FIRST_CUE, late enough that the track is selected and
        // its branch built well before the first seam. The walk below is about
        // boundaries, not about how fast a selection lands.
        const CUE: u64 = 500;
        const FIRST_CUE: u64 = 4_000;
        const CUES: u64 = 16;
        let cues: Vec<CueSpec> = (0..CUES)
            .map(|index| {
                CueSpec::new(
                    gst::ClockTime::from_mseconds(FIRST_CUE + CUE * index),
                    gst::ClockTime::from_mseconds(FIRST_CUE + CUE * (index + 1)),
                    format!("ALT {}m", 120 + index),
                )
            })
            .collect();

        let scenario = ScenarioBuilder::new("contiguous_text_no_blank_frame")
            .stream(StreamSpec::video("video_0").with_pacing(Pacing::Realtime))
            // A FIXED hold per item, which is neither of the other two pacings
            // and is the only one that reproduces the field. `Realtime` paces by
            // the item's own duration and `ftestsrc` holds BEFORE it pushes, so a
            // realtime cue leaves the source at its own END and reaches the
            // engine already expired. Free-running, the whole track is pushed and
            // EOS'd into the parking sink before a selection can land, and the
            // consumer sees nothing but `Clear`. A fixed 100 ms step keeps the
            // source alive across the selection while every cue still LEADS its
            // own running time by seconds, which is what a real file does,
            // where the text branch was measured 1.2 s ahead of the clock.
            .stream(
                StreamSpec::text("text_0", cues).with_pacing(Pacing::Jitter {
                    base_ms: 100,
                    jitter_ms: 0,
                }),
            )
            .duration(gst::ClockTime::from_seconds(30))
            .register();

        let video_sink = FTestSink::new();
        let frames = video_sink.recording();
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(video_sink.upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );

        // The REAL engine, wired the way `set_subtitle_consumer` wires it above.
        let engine = Arc::new(CueEngine::new());
        engine.set_canvas(1280, 720);
        let sink = engine.clone();
        // The windows the driver actually delivered. The claim below is made
        // against THESE, not against the spec: a cue the selection was too late
        // to catch is not a cue the engine can be asked to draw.
        let fed: Arc<Mutex<Vec<(gst::ClockTime, gst::ClockTime)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let feed_log = fed.clone();
        // A `Clear` resets the engine, so a walk that straddled one would be
        // asserting across a deliberate wipe. Counted, and the walk is required
        // not to contain any.
        let clears = Arc::new(AtomicUsize::new(0));
        let clear_count = clears.clone();
        playbin.set_subtitle_consumer(move |item| match item {
            SubtitleFeedItem::Cue {
                text,
                start_rt,
                end_rt,
                ..
            } => {
                if let Some(end) = end_rt {
                    feed_log.lock().push((start_rt, end));
                }
                sink.submit(CueInput {
                    format: TextFormat::Utf8,
                    text,
                    start_rt,
                    end_rt,
                });
            }
            SubtitleFeedItem::Clear => {
                feed_log.lock().clear();
                clear_count.fetch_add(1, Ordering::Release);
                sink.clear();
            }
            _ => {}
        });

        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let loaded = Arc::new(AtomicBool::new(false));
        let text_sids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        let flag = loaded.clone();
        let sids = text_sids.clone();
        playbin.set_event_handler(None, move |event, _| match event {
            PlaybinEvent::Error { error, .. } => sink.lock().push(error.to_string()),
            PlaybinEvent::Loaded { .. } => {
                flag.store(true, Ordering::Release);
            }
            PlaybinEvent::StreamCollection(collection) => {
                *sids.lock() = collection
                    .iter()
                    .filter(|stream| stream.stream_type().contains(gst::StreamType::TEXT))
                    .filter_map(|stream| stream.stream_id().map(|s| s.to_string()))
                    .collect();
            }
            _ => {}
        });
        let gate = SelectionGate {
            quiet: true,
            paused: false,
            seekable: true,
        };
        let pump = || {
            playbin.poll_text_policy();
            playbin.pump_selection(gate);
            assert!(
                errors.lock().is_empty(),
                "pipeline error: {:?}",
                errors.lock()
            );
        };

        playbin.load_async(
            MediaInput::Uri(scenario.uri()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        let load_deadline = Instant::now() + Duration::from_secs(30);
        while !loaded.load(Ordering::Acquire) {
            assert!(Instant::now() < load_deadline, "the load never finished");
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        playbin.play().expect("play");

        // The subtitle track is off until something asks for it, exactly as in
        // the app, and the request only takes once the pipeline is running.
        let sid_deadline = Instant::now() + Duration::from_secs(30);
        while text_sids.lock().is_empty() {
            assert!(
                Instant::now() < sid_deadline,
                "the text stream was never advertised"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let sid = text_sids.lock()[0].clone();
        playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
        // The branch has to actually carry a cue before the walk can claim
        // anything: a run where selection never landed would show a clean sweep
        // of blank frames and call it a pass.
        let cue_deadline = Instant::now() + Duration::from_secs(30);
        while fed.lock().len() < 2 {
            assert!(
                Instant::now() < cue_deadline,
                "no cue ever reached the consumer"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }

        // Every frame the sink shows, evaluated ONCE against the engine at its
        // own running time, which is exactly what `VideoSink` does per frame,
        // and once is the point: a frame is drawn one time and a retry would let
        // the raster land and hide the very flash this is looking for. Reading
        // the recording as it fills keeps the wall-clock spacing between
        // evaluations equal to the real frame period.
        let last = gst::ClockTime::from_mseconds(FIRST_CUE + CUE * CUES);
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut seen = 0usize;
        // Each frame carries the clear-epoch it was drawn in. A `Clear` empties
        // the engine on purpose, so frames from an earlier epoch are describing a
        // schedule that no longer exists and only the LAST epoch can be judged,
        // which also keeps a stray re-dispatch from being read as the defect.
        let mut walk: Vec<(gst::ClockTime, bool, usize)> = Vec::new();
        while Instant::now() < deadline {
            pump();
            let log = frames.snapshot();
            for entry in log[seen.min(log.len())..].iter() {
                let Some(pts) = entry.pts().filter(|_| entry.is_buffer()) else {
                    continue;
                };
                let epoch = clears.load(Ordering::Acquire);
                walk.push((pts, engine.overlays_for(Some(pts)).is_empty(), epoch));
            }
            seen = log.len();
            if walk.last().is_some_and(|(pts, ..)| *pts >= last) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // BEFORE the shutdown: tearing the pipeline down flushes the branch, and
        // the flush is itself a `Clear` that would wipe the record being read.
        let delivered = fed.lock().clone();
        let epoch = clears.load(Ordering::Acquire);

        let (tx, rx) = mpsc::channel();
        playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let _ = rx.recv_timeout(Duration::from_secs(30));

        // A frame is owed an overlay exactly when a DELIVERED cue covers it. The
        // earliest delivered cue is exempt for its first 100 ms: it is the one
        // cue with nothing ahead of it, so its raster can still be in flight.
        assert!(
            delivered.len() >= 4,
            "only {} cues reached the consumer; too few boundaries to claim anything",
            delivered.len()
        );
        let grace = delivered
            .iter()
            .map(|(start, _)| *start)
            .min()
            .expect("checked non-empty")
            + gst::ClockTime::from_mseconds(100);

        let mut ordered = delivered.clone();
        ordered.sort_by_key(|(start, _)| *start);

        let mut covered = 0usize;
        let mut seams = 0usize;
        let mut previous: Option<usize> = None;
        let mut blanks: Vec<gst::ClockTime> = Vec::new();
        for (pts, empty, drawn_in) in &walk {
            if *drawn_in != epoch || *pts < grace {
                continue;
            }
            let Some(index) = ordered.iter().position(|(s, e)| pts >= s && pts < e) else {
                continue;
            };
            covered += 1;
            // A SEAM CROSSED: this frame is covered by a different cue than the
            // previous covered frame was. That, not a frame count, is what the
            // claim is made of, and it is the honest sufficiency bar, because a
            // loaded machine drops frames and a raw count then fails a run that
            // actually crossed every boundary cleanly. Dropping frames cannot
            // hide the defect either: the blank is the FIRST frame at or after a
            // seam, and however sparse the frames, one of them is still first.
            if previous.is_some_and(|last| last != index) {
                seams += 1;
            }
            previous = Some(index);
            if *empty {
                blanks.push(*pts);
            }
        }

        assert!(
            seams >= 6,
            "only {seams} cue boundaries were crossed ({covered} covered frames of {} walked, {} \
             cues delivered); the run is too thin to claim anything",
            walk.len(),
            delivered.len()
        );
        // NOT `blanks.is_empty()`, and the slack is measured rather than
        // guessed. The defect blanks EVERY boundary (15 of 15 seams on this
        // scenario with the prefetch disabled) so anything that regresses it
        // fails this by an order of magnitude. What the slack absorbs is a
        // different thing: on a host running the rest of this suite in parallel
        // the pipeline drops to roughly one frame per cue and the raster worker
        // can miss its window between two of them, which is host scheduling, not
        // the driver or the engine. The strict zero-blank claim is the engine's
        // own `contiguous_cues_hand_over_without_a_blank_frame`, which owns its
        // clock and cannot be starved.
        assert!(
            blanks.len() <= 1 && blanks.len() * 4 < seams,
            "{} of {covered} frames covered by a contiguous cue carried no overlay, across \
             {seams} boundaries, at {blanks:?}: the screen flashes at every cue boundary",
            blanks.len()
        );
    }

    #[test]
    fn buffering_projects_onto_the_resuming_transport_not_idle() {
        // Regression: a gapless switch / rebuffer briefly leaves the state
        // machine in a transient (running() == None -> Buffering). The v3
        // wire enum has no Buffering, and the old mapping collapsed it to
        // Idle, broadcasting "playback ended" mid-handoff (senders then
        // advanced or stopped the queue). Buffering must project onto the
        // transport being resumed instead.
        assert_eq!(
            project_wire_state(PlayerState::Buffering, RunningState::Playing),
            PlaybackState::Playing,
        );
        assert_eq!(
            project_wire_state(PlayerState::Buffering, RunningState::Paused),
            PlaybackState::Paused,
        );
        // Steady states pass straight through; a genuine stop is still Idle.
        assert_eq!(
            project_wire_state(PlayerState::Playing, RunningState::Playing),
            PlaybackState::Playing,
        );
        assert_eq!(
            project_wire_state(PlayerState::Paused, RunningState::Paused),
            PlaybackState::Paused,
        );
        assert_eq!(
            project_wire_state(PlayerState::Stopped, RunningState::Playing),
            PlaybackState::Idle,
        );
    }

    /// The field shape: our carry-patch's discard warning, as GStreamer
    /// formats a debug string (source location and object path around it), must
    /// be recognized so it stays out of the toast.
    #[test]
    fn the_carry_patchs_discard_warning_is_recognized() {
        let debug = "gstadaptivedemux.c(3705): gst_adaptive_demux_output_loop (): \
                     /GstPipeline:fcastplaybin/GstURISourceBin:fpb-src-0/GstDashDemux2:dashdemux2:\n\
                     Discarding data on subtitle_00: downstream returned FLUSHING while this \
                     element is not flushing";
        assert!(warning_is_transient_flushing_discard(Some(debug)));
        // Any pad name, and the bare message on its own.
        assert!(warning_is_transient_flushing_discard(Some(
            "Discarding data on video_01: downstream returned FLUSHING while this element is not flushing"
        )));
    }

    /// Narrow on purpose: this is not a general warning-suppression list, so
    /// everything else must keep reaching the user exactly as before.
    #[test]
    fn other_warnings_are_never_filtered() {
        assert!(!warning_is_transient_flushing_discard(None));
        assert!(!warning_is_transient_flushing_discard(Some("")));
        assert!(!warning_is_transient_flushing_discard(Some(
            "gsturidecodebin.c(1234): no decoder available for type 'video/x-h265'"
        )));
        // Superficially similar but a different condition: only the patch's
        // full sentence counts.
        assert!(!warning_is_transient_flushing_discard(Some(
            "gstqueue.c(1393): pushing on pad src returned FLUSHING"
        )));
        assert!(!warning_is_transient_flushing_discard(Some(
            "Discarding data on subtitle_00: downstream returned NOT_LINKED"
        )));
    }

    /// The stream name is what makes a persistent discard actionable, so it
    /// has to survive GStreamer's full debug formatting, not just the bare
    /// sentence.
    #[test]
    fn the_discarded_stream_is_named() {
        let debug = "gstadaptivedemux.c(3705): gst_adaptive_demux_output_loop (): \
                     /GstPipeline:fcastplaybin/GstURISourceBin:urisourcebin0/GstDashDemux2:dashdemux2-0:\n\
                     Discarding data on subtitle_00: downstream returned FLUSHING while this \
                     element is not flushing";
        assert_eq!(discarded_stream_name(debug), Some("subtitle_00"));
        assert_eq!(
            discarded_stream_name(
                "Discarding data on video_01: downstream returned FLUSHING while this element is not flushing"
            ),
            Some("video_01")
        );
        // Nothing to name is not a panic and not an empty name.
        assert_eq!(discarded_stream_name("some other warning entirely"), None);
        assert_eq!(discarded_stream_name("Discarding data on : x"), None);
    }

    /// Counting is PER STREAM and escalates only on the stream that keeps
    /// failing. `dash-embedded-still-broken.txt` is the case that matters: the
    /// text branch is stuck while video and audio are fine, so a global
    /// counter would either escalate on healthy streams or need several
    /// unrelated discards before it noticed the one that is wedged.
    #[test]
    fn repeated_discards_escalate_per_stream() {
        let discards = FlushingDiscards::default();
        // Below the threshold, still "transient".
        for expected in 1..FLUSHING_DISCARD_ESCALATION {
            assert_eq!(discards.record("subtitle_00"), expected);
        }
        // A different stream has its own count and does not push the first one
        // over.
        assert_eq!(discards.record("video_00"), 1);
        assert_eq!(discards.record("subtitle_00"), FLUSHING_DISCARD_ESCALATION);
        // Strictly increasing past the threshold, which is what makes the
        // "let it through exactly once" equality in the hook a one-shot.
        assert_eq!(
            discards.record("subtitle_00"),
            FLUSHING_DISCARD_ESCALATION + 1
        );
    }

    #[test]
    fn missing_plugin_ignorable_only_for_metadata_streams() {
        crate::gstreamer::init_for_tests();
        // gst_missing_decoder_message_new requires a non-null src element.
        let src = gst::ElementFactory::make("identity").build().unwrap();

        // qtdemux's non-media metadata stream: no decoder is needed, so it must not be
        // reported as a missing codec.
        let meta = gst::Caps::builder("meta/x-gst-fourcc-priv").build();
        let msg = gst_pbutils::MissingPluginMessage::builder_for_decoder(&meta)
            .src(&src)
            .build();
        assert!(missing_plugin_is_ignorable(&msg));

        // A real codec with no decoder must still be reported.
        let video = gst::Caps::builder("video/x-h264").build();
        let msg = gst_pbutils::MissingPluginMessage::builder_for_decoder(&video)
            .src(&src)
            .build();
        assert!(!missing_plugin_is_ignorable(&msg));
    }

    fn stream(sid: &str, ty: gst::StreamType) -> gst::Stream {
        gst::Stream::new(Some(sid), None, ty, gst::StreamFlags::empty())
    }

    fn collection(streams: &[gst::Stream]) -> gst::StreamCollection {
        let mut builder = gst::StreamCollection::builder(None);
        for s in streams {
            builder = builder.stream(s.clone());
        }
        builder.build()
    }

    fn sids(streams: &[Stream]) -> Vec<String> {
        streams
            .iter()
            .filter_map(|s| s.inner.stream_id().map(|id| id.to_string()))
            .collect()
    }

    #[test]
    fn stream_positions_stay_stable_across_collections() {
        crate::gstreamer::init_for_tests();
        let audio = stream("a0", gst::StreamType::AUDIO);
        let video = stream("v0", gst::StreamType::VIDEO);
        let text = stream("t0", gst::StreamType::TEXT);

        // Initial collection: [audio, video].
        let first = merge_streams_stable(Vec::new(), &collection(&[audio.clone(), video.clone()]));
        assert_eq!(sids(&first), ["a0", "v0"]);

        // decodebin3 rebuilds the collection in a DIFFERENT order and with a
        // new text stream (an external subtitle attach). Positions of the
        // known streams must not move (they are the advertised track ids);
        // the newcomer appends.
        let second = merge_streams_stable(
            first,
            &collection(&[video.clone(), audio.clone(), text.clone()]),
        );
        assert_eq!(sids(&second), ["a0", "v0", "t0"]);

        // A stream leaving (external detached) drops in place; the rest
        // keep their positions.
        let third = merge_streams_stable(second, &collection(&[video, audio]));
        assert_eq!(sids(&third), ["a0", "v0"]);
    }
}
