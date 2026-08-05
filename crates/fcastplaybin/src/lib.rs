//! fcastplaybin: a receiver-owned replacement for playbin3/playsink.
//!
//! Topology (playsink's, minus its hidden reconfiguration state machine):
//!
//! ```text
//! input (urisourcebin | element)   -> decodebin3 -> streamsynchronizer -> chains
//! external subtitle (urisourcebin) -> decodebin3 (request pads, live attach/detach)
//!
//! video chain: ssync -> subtitleoverlay.video_sink -> video sink
//! text  path : decodebin3 -> queue -> subtitleoverlay.subtitle_sink (policy-gated)
//! audio chain: ssync -> queue -> audioconvert -> audioresample -> scaletempo
//!              -> volume -> audio sink
//! ```
//!
//! The mechanism layer (urisourcebin/decodebin3/streamsynchronizer/
//! subtitleoverlay and the decoders) stays stock. This crate owns policy:
//! when chains link, when text may join, how inputs attach and detach, and
//! how errors are attributed (every input carries a generation tag).
//!
//! The crate also owns the bus ([`FcastPlaybin::set_event_handler`] delivers
//! typed [`PlaybinEvent`]s) and a worker thread for the blocking operations
//! (the `_async` methods), so callers never touch raw GStreamer state
//! changes, seeks or bus messages.

use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use parking_lot::{Condvar, Mutex};
use tracing::{debug, debug_span, error, info, warn};

pub mod graph;
pub mod selection;
pub mod state_machine;

pub use selection::{SelectionGate, TrackSelection, TrackSlot, TrackTarget};
pub use state_machine::{
    BufferingStateResult, PlaybackState, RunningState, Seek, StateChangeResult, StateMachine,
};

/// What plays: a URI (http/file/DASH/HLS/`data:`), or a pre-built source
/// element configured in typed Rust by the caller (no fake-URI dispatch, no
/// property side channels).
#[derive(Debug)]
pub enum MediaInput {
    Uri(String),
    /// A pre-configured source element (WHEP bin, fwebrtcsrc, AirPlay mirror
    /// source). Must expose (possibly dynamic) source pads carrying parsed
    /// or decodable streams.
    Element(gst::Element),
}

/// Where a load should begin. Applied by [`FcastPlaybin::load`] while the
/// pipeline is still in PAUSED: applying a non-1.0 rate after PLAYING renders
/// a slice of 1.0x audio that the flushing seek then discards, an audible pop.
#[derive(Debug, Clone, Copy)]
pub enum StartPoint {
    /// Seekable source: after preroll, one flushing ACCURATE seek to
    /// `position` at `rate`. The 1.0x start-of-stream no-op is skipped, so a
    /// plain load never blocks on the seek.
    Seek { position: gst::ClockTime, rate: f64 },
    /// Live source (WHEP/fwebrtc/mirror): preroll only, never seek.
    Live,
}

/// What [`FcastPlaybin::load`] learned while prerolling.
#[derive(Debug, Clone, Copy)]
pub struct StartOutcome {
    /// The pipeline prerolled with no data (`NoPreroll`): a live source.
    pub live: bool,
    /// The load's generation (every event carries one, see
    /// [`FcastPlaybin::load_async`]).
    pub generation: u64,
}

/// Bounded wait for a load's (re-)preroll. Bounded on purpose: an unbounded
/// `get_state(None)` here would wedge the caller's worker if preroll stalled.
const PREROLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(10);

/// Identifies one attached external subtitle input for later detach. The id is STABLE across
/// in-place recoveries (see `Inner::handle_external_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalSubId(u64);

/// How long an attached external subtitle input may take to produce its stream before it is
/// failed.
const EXTERNAL_SUB_TIMEOUT: Duration = Duration::from_secs(5);
/// How long after a replay its verification fires, and how many replays a
/// single trigger may issue (see `Job::VerifyReplay`).
const REPLAY_VERIFY_AFTER: Duration = Duration::from_millis(400);
const REPLAY_ATTEMPTS: u32 = 3;
/// How long a SENT upstream selection may go unconfirmed before the crate
/// confirms it locally (see `Inner::arm_upstream_confirm_fallback`). Longer than
/// any demuxer edge takes, short enough that a user waiting for the track list
/// to update does not notice.
const UPSTREAM_CONFIRM_FALLBACK: Duration = Duration::from_millis(700);

/// How many times an external input that died before anything of it reached
/// decodebin3 is re-attached (see [`Job::RetrySub`]). A genuinely bad URL
/// exhausts these near-instantly and the materialization watchdog delivers
/// the verdict.
const MAX_ATTACH_RETRIES: u32 = 3;

/// A cumulative byte counter for one input stream's PARSED (compressed) data, for bitrate
/// inspection (see [`FcastPlaybin::stream_io_stats`]). Counters are per-load by construction (they
/// live and die with the input). Callers sample periodically and derive rates from deltas.
#[derive(Debug, Clone)]
pub struct StreamIoStats {
    /// The GStreamer stream id, for correlating with the stream collection (`None` until the pad
    /// has carried its stream-start).
    pub stream_id: Option<String>,
    /// Set when the stream belongs to an external subtitle input.
    pub external: Option<ExternalSubId>,
    /// Compressed bytes that have passed into decodebin3 so far.
    pub bytes: u64,
    /// The stream's current caps (codec, dimensions, rate, ...).
    pub caps: Option<gst::Caps>,
}

/// A buffered region of the current media, expressed as fractions `[0.0, 1.0]` of the whole
/// timeline. Derived from a `GST_QUERY_BUFFERING` in `PERCENT` format, so the values map directly
/// onto a scrubber. There can be several disjoint ranges (e.g. after a seek into an unbuffered
/// region).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BufferedRange {
    pub start: f64,
    pub stop: f64,
}

/// Overall buffering state plus the buffered ranges, for the inspector. See
/// [`FcastPlaybin::buffering_info`].
#[derive(Debug, Clone)]
pub struct BufferingInfo {
    /// Fill level of the buffer that gates playback, `0..=100`.
    pub percent: i32,
    /// Whether the pipeline is actively (re)filling and would stall if the
    /// buffer drained now.
    pub busy: bool,
    /// How the source buffers (stream, on-disk download, timeshift, live).
    pub mode: gst::BufferingMode,
    /// Estimated time until the buffer is full, when known.
    pub buffering_left: Option<gst::ClockTime>,
    /// Buffered regions of the media (may be empty even when the query
    /// otherwise succeeds).
    pub ranges: Vec<BufferedRange>,
}

/// `PERCENT`-format buffering values run `0..=GST_FORMAT_PERCENT_MAX`.
const GST_FORMAT_PERCENT_MAX: f64 = 1_000_000.0;

/// Convert a `PERCENT`-format buffering bound to a `[0.0, 1.0]` fraction.
fn percent_fraction(v: gst::GenericFormattedValue) -> Option<f64> {
    (v.format() == gst::Format::Percent)
        .then(|| (v.value() as f64 / GST_FORMAT_PERCENT_MAX).clamp(0.0, 1.0))
}

/// Extract the buffered ranges from an answered `PERCENT`-format buffering query, dropping any
/// empty or malformed range.
fn buffered_ranges_from(query: &gst::query::Buffering) -> Vec<BufferedRange> {
    query
        .ranges()
        .filter_map(|(start, stop)| {
            let start = percent_fraction(start)?;
            let stop = percent_fraction(stop)?;
            (stop > start).then_some(BufferedRange { start, stop })
        })
        .collect()
}

/// One live input, for the inspector's source listing (see
/// [`FcastPlaybin::source_summaries`]).
#[derive(Debug, Clone)]
pub struct SourceDbg {
    /// Set when this is an external subtitle input.
    pub external: Option<ExternalSubId>,
    /// The input element's factory name (`urisourcebin`, `fwebrtcsrc`, ...).
    pub factory: String,
    /// The element's `uri` property, when it has one.
    pub uri: Option<String>,
}

/// Where a bus error originated, derived from the generation-tagged inputs. This replaces
/// playbin3's contextless `failed_uri` guessing. Errors from live external subtitle inputs never
/// surface here: the crate handles them internally (in-place recovery, or
/// [`PlaybinEvent::ExternalSubtitleFailed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorOrigin {
    /// The main input of the CURRENT load.
    Main,
    /// An element of a previous, already-replaced load whose teardown died noisily. Safe to ignore.
    Stale,
    /// Not attributable to a specific input (sinks, decoders, ...): treat as the current load's
    /// problem.
    Unknown,
}

/// A bus error's source input, classified by the generation-tagged inputs. The internal superset of
/// [`ErrorOrigin`]: external-input errors are consumed by the crate and need their id for the
/// fail/re-arm decision.
enum ErrorSource {
    Main,
    External(ExternalSubId),
    Stale,
    /// A pre-armed next input that has not activated yet (its generation is
    /// AHEAD of the current one). Consumed internally: the prepare is
    /// abandoned and reported as [`PlaybinEvent::PreparedFailed`].
    Prepared(u64),
    Unknown,
}

/// A typed pipeline event, delivered through the callback installed by
/// [`FcastPlaybin::set_event_handler`]. Bus messages are translated on the posting (streaming)
/// thread, and worker feedback (load completion, seek outcomes) arrives through the same callback:
/// one ordered event source instead of a raw bus plus side channels.
#[derive(Debug)]
pub enum PlaybinEvent {
    EndOfStream,
    /// An async load ([`FcastPlaybin::load_async`]) finished wiring and
    /// prerolling its input. `live` mirrors [`StartOutcome::live`].
    Loaded {
        live: bool,
    },
    Tags(gst::TagList),
    /// The volume changed: a deterministic `notify::volume` from the
    /// dedicated volume element (see [`FcastPlaybin::set_volume`]). Also
    /// re-emitted on demand by [`FcastPlaybin::renotify_volume`].
    VolumeChanged(f64),
    /// A stream collection for the caller's stream list. Partial collections
    /// posted by external subtitle inputs are already filtered out so they
    /// cannot clobber the main collection.
    StreamCollection(gst::StreamCollection),
    /// An async state change or flushing seek finished prerolling. Not
    /// attributable to a specific operation: `GstBin` posts its aggregated
    /// ASYNC_DONE with a fresh seqnum.
    AsyncDone,
    /// The media's duration changed and any value the caller cached is stale:
    /// re-query [`duration`](FcastPlaybin::duration). No payload, mirroring
    /// GStreamer's own `DURATION_CHANGED` contract (the message carries no
    /// duration either, precisely because only a fresh query is authoritative).
    ///
    /// Real sources need this: a push-mode `oggdemux` (the fcomp/companion
    /// transport) reports an APPROXIMATE duration up front and refines it as it
    /// plays, so a caller that never re-queries reports the whole item a few
    /// seconds short.
    ///
    /// NOT emitted for anything describing the NEXT item: a refinement posted
    /// BY a prefetching prepared input, and anything at all while a performed
    /// gapless swap waits to activate (upstream answers for the successor item
    /// there, so a refresh would poison the caller's view of the item still
    /// playing). See `Inner::translate_message`.
    DurationChanged,
    Buffering(i32),
    /// A state change of the pipeline itself (per-element state changes are
    /// filtered out).
    StateChanged {
        old: gst::State,
        current: gst::State,
        pending: gst::State,
    },
    /// An element asked for a pipeline state change (e.g. a sink handling a
    /// system sleep).
    RequestState(gst::State),
    /// A seek arrived while the pipeline couldn't perform it. The worker is
    /// driving to PAUSED and hands the seek back for the caller (who owns
    /// the seek state machine) to re-queue.
    QueueSeek(Seek),
    /// decodebin3 confirmed a stream selection. One stream id per slot, and
    /// `seqnum` is the one stamped on the `SELECT_STREAMS` event this
    /// confirms (see [`FcastPlaybin::select_streams`]).
    StreamsSelected {
        video: Option<String>,
        audio: Option<String>,
        subtitle: Option<String>,
        seqnum: gst::Seqnum,
    },
    /// A refresh seek ([`FcastPlaybin::refresh_seek_async`]) could not be
    /// performed. `seqnum` is the one the caller stamped on it.
    RefreshSeekFailed {
        seqnum: gst::Seqnum,
    },
    RateChanged(f64),
    SeekFailed,
    /// The element providing the pipeline clock went away (e.g. the audio
    /// sink after audio was deselected). Call
    /// [`FcastPlaybin::recover_clock_async`] to elect a new clock.
    ClockLost,
    Error {
        /// Which input the error came from (generation-tagged attribution).
        origin: ErrorOrigin,
        error: gst::glib::Error,
        /// URI of the failing source element, when the source is one.
        failed_uri: Option<String>,
    },
    /// An attached external subtitle input failed for good and has already been DETACHED by the
    /// crate: its attach failed outright, a bus error arrived while its stream was selected (or
    /// before it ever produced one), or it produced no stream within the materialization timeout.
    /// Deselect-race errors recover in place and never surface (see
    /// `Inner::handle_external_error`). The caller drops its bookkeeping for the id and reports the
    /// failure.
    ExternalSubtitleFailed {
        id: ExternalSubId,
    },
    /// fimagedec's announcement of an image load: the "fcast-image-stream"
    /// structure with format (str), width/height (i32) and animated (bool).
    /// The caller uses it to classify the load as an image and feed its
    /// inspector; animations otherwise look like ordinary video streams.
    ImageStream(gst::Structure),
    /// A prepared next input ([`FcastPlaybin::prepare_next_async`]) went
    /// live: the current item drained and decodebin3 switched to the
    /// prepared item's streams without any state change. Stamped with the
    /// PREPARED generation (the one `prepare_next_async` returned), which is
    /// the pipeline's current generation from this event on. Followed by the
    /// new item's `StreamCollection` and `StreamsSelected`, mirroring a
    /// fresh load's event order.
    PreparedActivated,
    /// A prepared next input failed before it could activate (its element
    /// errored, or the prepare itself failed). The input is already being
    /// removed; the caller drops its pre-arm bookkeeping and the item loads
    /// through the normal end-of-stream advance instead. `generation` names
    /// the failed prepare (the event itself is stamped with the still
    /// current item's generation).
    PreparedFailed {
        generation: u64,
    },
    /// A caller-requested cancel ([`FcastPlaybin::cancel_prepared_async`])
    /// took effect: the prepared input is gone and NO activation will fire.
    /// `generation` names the prepare that was dropped, and is `None` when
    /// there was nothing to cancel. The caller drops its pre-arm bookkeeping
    /// here.
    PreparedCancelled {
        generation: Option<u64>,
    },
    /// A caller-requested cancel arrived after the swap had already performed:
    /// the activation is imminent and was left to finish, so `generation` WILL
    /// activate.
    ///
    /// The caller must KEEP its pre-arm bookkeeping so the coming
    /// [`PreparedActivated`](Self::PreparedActivated) is adopted instead of
    /// treated as unmatched. An unmatched activation makes the caller reload
    /// the item it still believes is current, i.e. the track that just
    /// finished replays from 0.
    PreparedCancelDeclined {
        generation: u64,
    },
    Warning(String),
}

/// First look at every raw bus message, invoked on the posting (streaming) thread, for
/// caller-specific messages the crate does not understand (`NeedContext` for custom source
/// elements, missing-plugin reports).  Return `true` to consume the message. No event is emitted
/// for it.
pub type MessageHook = Box<dyn Fn(&gst::Message) -> bool + Send + Sync>;

/// The caller's event sink. The second argument is the generation of the
/// load the event belongs to (see [`FcastPlaybin::load_async`]).
type EventCallback = Arc<dyn Fn(PlaybinEvent, u64) + Send + Sync>;

/// Work executed on the crate's worker thread (the `_async` methods). A
/// dedicated thread because these calls can block (a state change waits on
/// streaming threads, an attach's `start()` may perform I/O) and must not
/// run on the caller's event loop. A single queue keeps them ordered.
enum Job {
    SetState {
        target: gst::State,
    },
    /// Full teardown to `target` (see [`FcastPlaybin::stop_async`]).
    Stop {
        target: gst::State,
        done: Option<Box<dyn FnOnce() + Send>>,
    },
    Load {
        input: MediaInput,
        start: StartPoint,
        generation: u64,
    },
    Seek(Seek),
    RefreshSeek {
        seqnum: gst::Seqnum,
    },
    RecoverClock,
    /// Re-run the pipeline's latency query and redistribute (answers a
    /// `GST_MESSAGE_LATENCY`, e.g. after the video sink's render-delay changed).
    /// On the worker thread: it queries upstream and pushes a latency event, so
    /// it must not run inline on the bus (streaming) thread.
    RecalculateLatency,
    AttachSub {
        id: ExternalSubId,
        url: String,
    },
    DetachSub {
        id: ExternalSubId,
    },
    /// Fail an external subtitle input: detach it and report
    /// [`PlaybinEvent::ExternalSubtitleFailed`]. Queued by the bus error
    /// handler (the detach must not run on the posting streaming thread).
    /// `epoch` guards against acting on an input that was re-armed or
    /// replaced after the job was queued.
    FailSub {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Bounded materialization check, armed per (re-)attach: an input still
    /// without streams when this fires is dead (bad URL that never errors)
    /// and is failed.
    CheckSub {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Re-attach an external input that died before anything of it
    /// reached decodebin3 (see `FcastPlaybin::retry_subtitle`).
    RetrySub {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Unlock a materialized external input and join it to the pipeline
    /// state. Attach leaves externals STATE-LOCKED (see `Inner::add_input`),
    /// so a pipeline state change cannot recurse into an input whose
    /// typefind/parsebin machinery is still plugging (that recursion is an
    /// ABBA against the plugging thread's sync_state_with_parent and wedged
    /// the caller in the field). Materialized means the plugging finished,
    /// which makes this join safe.
    AdoptSubState {
        id: ExternalSubId,
        epoch: u32,
    },
    /// Did a replay actually take? A replay racing decodebin3's own slot
    /// swap (the selection that triggered it is still reconfiguring) can
    /// pour its one-shot re-delivery into a slot the swap then drains, so
    /// each replay arms one bounded re-check: if the input's stream has not
    /// reached its decodebin3 output pad, replay again.
    VerifyReplay {
        id: ExternalSubId,
        epoch: u32,
        attempt: u32,
    },
    /// Replay an external input whose stream just joined the overlay: a
    /// flushing zero-seek into the input (see `Inner::poll_text_policy`,
    /// which queues one on EVERY join).
    ReplaySub {
        id: ExternalSubId,
        /// Which retry this is, see [`Job::VerifyReplay`].
        attempt: u32,
        epoch: u32,
    },
    /// Snapshot the pipeline graph for the inspector. On the worker so the
    /// element walk cannot race a load's sink teardown.
    DumpGraph {
        done: Box<dyn FnOnce(graph::GraphSnapshot) + Send>,
    },
    /// Pre-arm the next item on the live core (gapless transition). See
    /// [`FcastPlaybin::prepare_next_async`].
    PrepareNext {
        input: MediaInput,
        generation: u64,
    },
    /// Drop a prepared next input that will not be needed (seek away from
    /// the end, queue mutation, autoplay turned off).
    CancelPrepared {
        /// Report the outcome ([`PlaybinEvent::PreparedCancelled`] /
        /// [`PlaybinEvent::PreparedCancelDeclined`]). Only a CALLER's cancel
        /// wants that: the crate's own self-cancels follow a
        /// [`PlaybinEvent::PreparedFailed`] that already told the caller its
        /// prepare is gone.
        notify: bool,
    },
    /// Post-activation cleanup: remove every input older than the newly
    /// activated generation (the drained main input and the previous item's
    /// external subtitles). Queued by the activation detection, which runs
    /// on a posting (streaming) thread where pipeline surgery is forbidden.
    FinishActivation,
    /// Re-align the text branches' running time with the A/V branches' (see
    /// [`Inner::sync_text_running_time`]). Queued by the SEGMENT probe on
    /// subtitleoverlay's `video_sink`, which runs on the VIDEO streaming
    /// thread: the probe itself must take no lock, so it only posts this and
    /// the worker does the routing-lock work, same reason as
    /// [`Job::FinishActivation`].
    SyncTextRunningTime,
    /// Drain whatever text-branch work was postponed at a moment the
    /// pipeline could not carry it out (see [`Inner::run_deferred_text_work`]).
    ///
    /// Queued by the bus translation on every PIPELINE state edge, so the
    /// drain can never depend on the caller's poll cadence. Postponed work
    /// used to drain only through [`FcastPlaybin::poll_text_policy`], and a
    /// caller that stopped polling (or a state machine parked waiting for the
    /// very work that was postponed) left it pending forever. The job itself
    /// re-checks what the pipeline currently allows, so a drain queued on an
    /// edge that still cannot complete simply leaves the work pending for the
    /// next edge.
    DrainTextWork,
    /// The item's video stream left decodebin3: park any overlay-linked text
    /// and take the video chain out of the pipeline (see
    /// [`Inner::unroute_db3_pad`]).
    ///
    /// Queued rather than run inline for the reason
    /// [`Job::FinishActivation`] gives. `unroute_db3_pad` runs on
    /// decodebin3's `pad-removed` callback, which is a STREAMING thread, and
    /// both halves of this are pipeline surgery. Running them there produced
    /// a four-way wedge under the fuzz driver: the streaming thread sat in
    /// `remove_video_chain`'s `set_state` waiting for a state lock, the
    /// caller sat in `play`'s `set_state`, the select sender sat in
    /// `park_video_chain_for_deselect`'s, and the worker sat in
    /// `gst_pad_pause_task` waiting for that same streaming thread.
    VideoChainGone,
    /// Re-commit a pipeline whose state changes GStreamer has latched off.
    ///
    /// ANY child error makes `gst_bin_handle_message_func` set
    /// `GST_STATE_RETURN = FAILURE`, after which `bin_handle_async_done` refuses
    /// every commit, so a pipeline that lost state keeps `pending` forever even
    /// once all sinks have prerolled, and posts no further `state-changed`.
    /// Nothing is unsettled, only uncommitted, hence `unsettled []`. Only a
    /// fresh `set_state` clears it. So an error this crate CONSUMES rather than
    /// surfaces has to re-commit, or the caller waits for a settle that can no
    /// longer happen.
    ///
    /// Re-commit `GST_STATE_PENDING`, the transition actually refused, not
    /// `current`: on a pipeline caught mid-climb the two differ and re-committing
    /// `current` cancels the climb.
    ///
    /// Levers: `FCAST_NO_ERROR_STATE_UNLATCH`, `FCAST_UNLATCH_RECOMMIT_CURRENT`.
    ClearStateFailure,
}

/// The overlay's ONE subtitle seat, as a lock rather than a fact read off the
/// pad.
///
/// Two threads decide about `subtitle_sink` and they shared no lock:
/// [`Inner::poll_text_policy`] links a fresh text queue into it on the caller
/// thread, while [`Inner::dispose_text_branch_on`] checks `is_linked()` and, if
/// free, sends it a flush pair from the worker. Lose that race and the pair
/// lands on the branch that just joined: its queue's `srcresult` latches
/// FLUSHING (`gstqueue.c` `out_flushing`), every later multiqueue push into it
/// returns FLUSHING, the multiqueue latches too, and adaptivedemux2's single
/// output task pauses for good with nothing posted at all (FREEZE-DIAGN.md
/// section 8.2, top freeze candidate).
///
/// Lock order: `routing` then this, never the other way round. The linking side
/// holds `routing`; the disposal side deliberately holds no routing lock (see
/// [`Inner::live_text_downstream_pads`] for the inversion that forbids it), so
/// there is no cycle.
///
/// Lever: `FCAST_NO_TEXT_SEAT_LOCK` (set = the old unsynchronized check).
#[derive(Default)]
struct TextSeat {
    lock: Mutex<()>,
    /// How often the two critical sections actually OVERLAPPED. The TOCTOU
    /// needs exactly that overlap, and it is the only evidence a test can
    /// gather that the window is real rather than argued from source.
    contentions: AtomicU64,
}

/// How long the LINKING side may wait for the seat. Long enough to cover a
/// disposal's ordinary flush (microseconds), short enough that the caller
/// never notices.
const TEXT_SEAT_LINK_WAIT: Duration = Duration::from_millis(5);

impl TextSeat {
    /// The DISPOSAL side, which may block: it runs on the worker or a streaming
    /// thread, never on the caller. `None` means the lever turned the lock off,
    /// not that the seat is busy.
    fn hold(&self) -> Option<parking_lot::MutexGuard<'_, ()>> {
        if std::env::var_os("FCAST_NO_TEXT_SEAT_LOCK").is_some() {
            return None;
        }
        Some(match self.lock.try_lock() {
            Some(guard) => guard,
            None => {
                self.contentions.fetch_add(1, Ordering::Relaxed);
                self.lock.lock()
            }
        })
    }

    /// The LINKING side, which must NOT block: `poll_text_policy` runs on the
    /// CALLER thread and a disposal holds the seat across a flush that waits
    /// for the overlay's stream lock, which a parked cue push can hold for as
    /// long as it likes (`tests/caller_nonblocking.rs` holds it for 12 s on
    /// purpose). A seat still busy after [`TEXT_SEAT_LINK_WAIT`] means a
    /// disposal is deciding about this very pad, so the link waits for the next
    /// poll instead of for the flush. `Err` = busy.
    fn try_hold(&self) -> Result<Option<parking_lot::MutexGuard<'_, ()>>, ()> {
        if std::env::var_os("FCAST_NO_TEXT_SEAT_LOCK").is_some() {
            return Ok(None);
        }
        if let Some(guard) = self.lock.try_lock() {
            return Ok(Some(guard));
        }
        self.contentions.fetch_add(1, Ordering::Relaxed);
        self.lock
            .try_lock_for(TEXT_SEAT_LINK_WAIT)
            .map(Some)
            .ok_or(())
    }
}

/// A text branch already taken out of the graph, waiting for its blocking
/// teardown. See [`Inner::detach_text_parts`].
struct TextDisposal {
    /// The text queue's sink pad, which the flush pair goes to.
    downstream: gst::Pad,
    /// The per-stream queue, to be NULLed and dropped from the pipeline.
    tqueue: Option<gst::Element>,
}

/// The eager text-branch work `pump_selection` does before dispatching a
/// selection, when it has to be postponed. See there and
/// [`Inner::run_deferred_text_work`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferredTextWork {
    /// The selection turns subtitles off: park the live text branches.
    Park,
    /// The selection replaces one text track with another: flush the
    /// outgoing branch so the switch is not queued behind its backlog.
    Flush,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Job::SetState { target } => f.debug_struct("SetState").field("target", target).finish(),
            Job::Stop { target, done } => f
                .debug_struct("Stop")
                .field("target", target)
                .field("feedback", &done.is_some())
                .finish(),
            Job::Load {
                input,
                start,
                generation,
            } => f
                .debug_struct("Load")
                .field("input", input)
                .field("start", start)
                .field("generation", generation)
                .finish(),
            Job::Seek(seek) => f.debug_tuple("Seek").field(seek).finish(),
            Job::RefreshSeek { seqnum } => f
                .debug_struct("RefreshSeek")
                .field("seqnum", seqnum)
                .finish(),
            Job::RecoverClock => write!(f, "RecoverClock"),
            Job::RecalculateLatency => write!(f, "RecalculateLatency"),
            Job::AttachSub { id, url } => f
                .debug_struct("AttachSub")
                .field("id", id)
                .field("url", url)
                .finish(),
            Job::DetachSub { id } => f.debug_struct("DetachSub").field("id", id).finish(),
            Job::FailSub { id, epoch } => f
                .debug_struct("FailSub")
                .field("id", id)
                .field("epoch", epoch)
                .finish(),
            Job::CheckSub { id, epoch } => f
                .debug_struct("CheckSub")
                .field("id", id)
                .field("epoch", epoch)
                .finish(),
            Job::RetrySub { id, epoch } => f
                .debug_struct("RetrySub")
                .field("id", id)
                .field("epoch", epoch)
                .finish(),
            Job::AdoptSubState { id, epoch } => f
                .debug_struct("AdoptSubState")
                .field("id", id)
                .field("epoch", epoch)
                .finish(),
            Job::ReplaySub { id, epoch, attempt } => f
                .debug_struct("ReplaySub")
                .field("id", id)
                .field("epoch", epoch)
                .field("attempt", attempt)
                .finish(),
            Job::VerifyReplay { id, epoch, attempt } => f
                .debug_struct("VerifyReplay")
                .field("id", id)
                .field("epoch", epoch)
                .field("attempt", attempt)
                .finish(),
            Job::DumpGraph { .. } => write!(f, "DumpGraph"),
            Job::PrepareNext { input, generation } => f
                .debug_struct("PrepareNext")
                .field("input", input)
                .field("generation", generation)
                .finish(),
            Job::CancelPrepared { notify } => f
                .debug_struct("CancelPrepared")
                .field("notify", notify)
                .finish(),
            Job::FinishActivation => write!(f, "FinishActivation"),
            Job::SyncTextRunningTime => write!(f, "SyncTextRunningTime"),
            Job::DrainTextWork => write!(f, "DrainTextWork"),
            Job::VideoChainGone => write!(f, "VideoChainGone"),
            Job::ClearStateFailure => write!(f, "ClearStateFailure"),
        }
    }
}

/// A queued `SELECT_STREAMS` (see [`FcastPlaybin::select_streams`]). Sent
/// from a dedicated thread, NOT the crate worker: a wedged send must not
/// block the queued Stop/Load whose flush is what releases such a wedge.
struct SelectJob {
    /// The decodebin3 the selection was built against. The sender skips the
    /// job if a core swap superseded it (the selection could never confirm).
    db3: gst::Element,
    /// Where the event goes: decodebin3 normally; the MAIN input when
    /// upstream owns selection (an adaptive demuxer rejects any event that
    /// names a foreign stream, and an external input's pads poison
    /// `gst_bin_send_event`'s AND-fold through decodebin3).
    target: gst::Element,
    event: gst::Event,
    /// The selected ids, kept for the video-deselect check after the send.
    stream_ids: Vec<String>,
}

/// A queued external-subtitle replay seek (see
/// [`FcastPlaybin::replay_subtitle`]). Sent from a dedicated thread for the
/// same reason as [`SelectJob`], and captured with gdb doing the damage. A
/// FLUSHING seek is performed INLINE by the source on the sending thread,
/// which pushes `FLUSH_START` down the whole live graph from there. When that
/// reaches a `queue` whose src task is parked behind a sink in
/// `gst_base_sink_wait_preroll`, the queue's own handler calls
/// `gst_pad_pause_task` and the sender waits on that task's stream lock. The
/// only thing that ends such a preroll wait is the pipeline reaching PLAYING,
/// and the only thread that can carry [`Job::SetState`] is the worker, so
/// sending from the worker closes a cycle through the worker itself.
struct ReplayJob {
    /// The input's source pads, resolved on the worker before the handover
    /// so the send never re-enters the routing lock.
    pads: Vec<gst::Pad>,
    seek: gst::Event,
    id: ExternalSubId,
    epoch: u32,
    attempt: u32,
    /// Log-only, so the replay's line reads exactly as it did when the send
    /// was inline on the worker.
    origin: gst::ClockTime,
    rate: f64,
}

/// A queued chain join: the blocking half of [`Inner::route_db3_pad`] (see
/// [`Inner::run_chain_join`]).
///
/// Sent from a dedicated thread for the same reason as [`SelectJob`] and
/// [`ReplayJob`], and with one extra constraint that rules the crate worker
/// out entirely: `Inner::apply_start_seek` waits for the preroll on the
/// worker (`pipeline.state(PREROLL_TIMEOUT)`), and the preroll cannot finish
/// until this join has run. Queuing it behind that wait would be the
/// postponed-work-blocked-by-its-own-drain shape a seventh time.
struct ChainJoinJob {
    /// The decodebin3 the route was made against. A core swap makes the join
    /// stale (see `Inner::run_chain_join`).
    db3: gst::Element,
    /// The routed decodebin3 source pad, to re-check that the stream is still
    /// routed by the time the join runs.
    pad: gst::Pad,
    kind: StreamKind,
    /// The blocking probe holding this stream at the streamsynchronizer src
    /// pad until the chain is up. Released by the join, whatever its outcome.
    hold: Option<(gst::Pad, gst::PadProbeId)>,
}

/// A byte counter on one input stream's parsed data, for bitrate
/// inspection (see [`FcastPlaybin::stream_io_stats`]). The probe lives on
/// the input's source pad and is removed with the input.
struct StreamTap {
    /// The input element's source pad (one parsed elementary stream).
    pad: gst::Pad,
    bytes: Arc<AtomicU64>,
    probe: Option<gst::PadProbeId>,
    /// Drain watch: whether this pad has pushed EOS into decodebin3 (reset
    /// by SEGMENT/STREAM_START, i.e. by seeks and item switches). The
    /// gapless swap fires when every main-input pad is drained.
    saw_eos: Arc<AtomicBool>,
    /// The EVENT_DOWNSTREAM probe maintaining `saw_eos`.
    event_probe: Option<gst::PadProbeId>,
}

/// External-subtitle bookkeeping for an [`Input`] (`None` for the main
/// input).
struct ExternalInput {
    id: ExternalSubId,
    /// The subtitle URI, kept for the never-linked attach retry (see
    /// `FcastPlaybin::retry_subtitle`).
    uri: String,
    /// Bumped per attach retry. Queued fail/check/retry/replay jobs carry
    /// the epoch they were decided against and no-op on a mismatch, so a
    /// stale job can never act on a different incarnation of the id.
    epoch: u32,
    /// The input's source task died deselected (its push hit the unlinked
    /// slot). Nothing more will EVER arrive from it, so a selection moving
    /// back onto it must replay eagerly: stored stickies drain from the
    /// slot and can look like delivery, but no cue follows. Set by the
    /// error classification's recover path, cleared by every replay.
    task_dead: bool,
    /// The timeline origin the input last had a seek applied for: ZERO at
    /// attach (a file plays from its start), updated by every replay and
    /// forwarded seek. The selection-time replay compares it against the
    /// overlay's origin: a mismatch means the switched-to cues WILL render
    /// shifted, so only then is the destructive eager flush justified.
    last_origin: gst::ClockTime,
    /// Block this input's buffers at its source pads until a selection
    /// naming its stream applies.
    ///
    /// An external input's buffers may only flow once decodebin3 has given
    /// its stream a multiqueue slot WITH an output, i.e. once the stream is
    /// selected. Pushing earlier dies not-linked (the slot's source pad has
    /// nothing behind it and multiqueue relays that upstream), taking the
    /// whole input down. Nothing about attaching makes selection win that
    /// race: decodebin3 only learns the stream from the events the first
    /// push carries, so the first push is inherently too early whenever the
    /// stream is not auto-selected (another text stream already is) or the
    /// caller attached it unselected.
    ///
    /// Held, the input survives indefinitely: its sticky events reach
    /// decodebin3, one seeded GAP gets the stream slotted (see
    /// [`Inner::seed_slot_for_held_pad`]) and therefore selectable, and the
    /// buffers follow once `STREAMS_SELECTED` confirms the selection (see
    /// [`Inner::unblock_selected_externals`]).
    hold_until_selected: bool,
    /// The selection that lifted `hold_until_selected` also owes this input a
    /// realigning replay, so its block probes are still installed and only
    /// [`FcastPlaybin::run_replay_seek`] may remove them. See
    /// [`Inner::release_owed_hold`].
    hold_release_owed: bool,
}

/// One live input: an element (urisourcebin or caller-provided) whose source
/// pads are linked into decodebin3 request sink pads.
struct Input {
    element: gst::Element,
    /// Which load (or attach) this input belongs to. A bumped generation
    /// makes this input's errors [`ErrorOrigin::Stale`].
    generation: u64,
    /// External-subtitle bookkeeping, `None` for the main input.
    external: Option<ExternalInput>,
    /// decodebin3 request sink pads we hold for this input.
    db3_sink_pads: Vec<gst::Pad>,
    /// Per-stream byte counters (see [`StreamTap`]).
    taps: Vec<StreamTap>,
    /// Signal handlers to disconnect on removal.
    pad_added_sig: Option<gst::glib::SignalHandlerId>,
    /// Prepared (gapless) inputs only: the per-pad block probes holding
    /// buffers back until the swap. Cleared by the swap itself; removed
    /// here when a still-pending prepare is cancelled.
    block_probes: Vec<(gst::Pad, gst::PadProbeId)>,
}

impl Input {
    /// The stream ids this input's source pads have produced so far. Empty
    /// until the pads exist and carry their stream-start events (guaranteed
    /// by the time decodebin3 posts the collection containing the streams).
    fn stream_ids(&self) -> Vec<String> {
        self.element
            .src_pads()
            .iter()
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
    }

    /// The input's TEXT stream ids only.
    ///
    /// The subtitle slot may only ever hold a text stream, and nothing stops a
    /// caller handing in an audio file, or a container, as "the subtitle".
    /// `stream_ids` answers "every stream this input has", which is what the
    /// seek forwarding and the routing bookkeeping want. Anything speaking for
    /// the SUBTITLE slot has to ask this instead, or an audio-only external
    /// looks healthy and its audio stream gets advertised to the caller as a
    /// subtitle track.
    ///
    /// A pad whose kind is not classifiable yet is excluded rather than
    /// guessed at, so a caller polling this waits instead of advertising a
    /// stream it cannot classify. See [`Self::has_unclassified_stream`] for
    /// the other half of that distinction.
    fn text_stream_ids(&self) -> Vec<String> {
        self.element
            .src_pads()
            .iter()
            .filter(|pad| Inner::stream_kind_of(pad) == Some(StreamKind::Text))
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
    }

    /// Whether any of the input's pads has no classifiable kind yet, i.e. no
    /// `GstStream` on its sticky stream-start and no caps. Distinguishes "this
    /// input carries no text" from "it has not said yet", which the watchdog
    /// must not confuse.
    fn has_unclassified_stream(&self) -> bool {
        self.element
            .src_pads()
            .iter()
            .any(|pad| Inner::stream_kind_of(pad).is_none())
    }

    fn is_external(&self, id: ExternalSubId) -> bool {
        self.external.as_ref().is_some_and(|e| e.id == id)
    }
}

/// A decodebin3 output stream routed into a chain.
///
/// Audio and video pass through streamsynchronizer (`ssync_*` are `Some`).
/// TEXT deliberately BYPASSES it (`ssync_*` are `None`) and links from
/// `db3_src_pad` directly: streamsynchronizer syncs ALL its sink pads, so a
/// sparse text stream through it stalls video/audio on every flushing seek's
/// re-preroll (no text buffer at the seek target to advance the sync) and
/// the pipeline hangs ASYNC. subtitleoverlay timestamps text against video
/// itself, so text needs no ssync synchronization.
struct RoutedStream {
    db3_src_pad: gst::Pad,
    /// A/V only: the streamsynchronizer request sink pad (released on
    /// unroute) and its paired src pad feeding the chain. `None` for text.
    ssync_sink: Option<gst::Pad>,
    ssync_src: Option<gst::Pad>,
    /// The live chain entry this stream is linked to: the A/V chain head, or
    /// the text queue feeding subtitleoverlay.
    downstream: Option<gst::Pad>,
    /// Text only: the parking sink's pad while the stream is parked. Parked
    /// text must be CONSUMED, not left unlinked: decodebin3 cannot finish a
    /// deselected sparse stream's drain into an unlinked pad, and a blocked
    /// drain holds up the whole selection reconfiguration. Exactly one of
    /// `downstream`/`park_pad` is Some for text streams.
    park_pad: Option<gst::Pad>,
    /// Text only: the per-stream parking `fakesink` behind `park_pad`. Must
    /// exist only WHILE its stream does: GstBin EOS aggregation requires an
    /// EOS from every sink child regardless of state, so a permanent parking
    /// sink that sees no data would swallow the pipeline's EOS forever. A
    /// per-stream sink receives its stream's drain, EOS included.
    park_sink: Option<gst::Element>,
    /// Text only: the per-stream `queue` in front of subtitleoverlay while
    /// the stream is live. Load-bearing twice over: (a) textoverlay
    /// prefetch-blocks the next cue's push until video reaches its
    /// timestamp, and without a queue absorbing that the decodebin3 text
    /// slot's src pad is permanently mid-push, stalling slot (de)activation
    /// for the media's cue spacing, and (b) it must NOT outlive the stream, or
    /// subtitleoverlay's subtitle input stays wired across loads with stale
    /// caps/renderer state and the next preroll wedges.
    tqueue: Option<gst::Element>,
    /// The group id of the last STREAM_START this pad carried. An EOS on a
    /// pad whose group is BEHIND the pipeline's active group belongs to a
    /// previous item draining out during a gapless switch and is dropped
    /// (uridecodebin3 keeps its gapless EOS drop open until every output
    /// pad has flipped to the new group, this is the per-pad equivalent).
    group: Option<gst::GroupId>,
    /// Text only. The link policy's seat reclaim evicted this branch from
    /// subtitleoverlay because its pad carried no sticky segment with
    /// nothing left upstream to send one. While that stays true the entry
    /// must not relink (it would win the seat back from the stream that can
    /// actually render, forever, since routed order is stable). A segment
    /// appearing on the pad is proof of life and clears the verdict.
    seat_evicted: bool,
    kind: StreamKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Video,
    Audio,
    Text,
}

/// The mutable, per-load state of the dynamic pad graph: the live [`Input`]s,
/// the decodebin3 output streams routed into the fixed chains, and the
/// generation / external-id bookkeeping. Guarded by one mutex. Distinct from
/// [`Core`] (the decodebin3 + streamsynchronizer elements themselves).
#[derive(Default)]
struct RoutingState {
    inputs: Vec<Input>,
    /// Routed streams. Text entries with `downstream: None` are parked
    /// awaiting the link policy (`poll_text_policy`).
    routed: Vec<RoutedStream>,
    /// Stream ids of the VIDEO streams in the latest advertised collection
    /// (cached by the bus translation, cleared per load). Lets
    /// [`FcastPlaybin::select_streams`] tell a selection that DROPS video
    /// entirely (video-chain deactivation needed) from a video-to-video
    /// switch, whose new id is not routed yet and would otherwise look like
    /// "no video".
    collection_video_ids: Vec<String>,
    next_external_id: u64,
}

/// A pre-armed next item (see [`FcastPlaybin::prepare_next_async`]). Its
/// input element is ALSO registered in `RoutingState::inputs` (under its
/// future generation), so the ordinary input machinery covers linking,
/// bitrate taps, and removal. This record carries what the gapless
/// transition additionally needs: the activation identity (which element,
/// which generation) and the held-back collection.
struct PreparedNext {
    element: gst::Element,
    /// The generation the item adopts when it activates (returned by
    /// `prepare_next_async` so the caller can correlate).
    generation: u64,
    /// The next item's stream collection, held back until activation so the
    /// caller sees it AFTER [`PlaybinEvent::PreparedActivated`], stamped
    /// with the new generation: the same collection-then-selected order a
    /// fresh load produces.
    pending_collection: Option<gst::StreamCollection>,
}

impl PreparedNext {
    /// The stream ids the prepared input has produced so far (empty until
    /// its pads exist, guaranteed populated by the time decodebin3 selects
    /// them).
    fn stream_ids(&self) -> Vec<String> {
        self.element
            .src_pads()
            .iter()
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
    }
}

/// Coordination between the current input's drain watches and the prepared
/// input's blocked streaming threads (the uridecodebin3 recipe: the
/// prepared input's first buffer blocks in a pad probe until the current
/// input has pushed EOS into decodebin3 on every pad, then that same probe
/// performs the relink).
#[derive(Default)]
struct SwapState {
    /// The generation of the pending prepared input. `None`: no gapless
    /// swap pending (never armed, cancelled, or already activated).
    pending: Option<u64>,
    /// Every main-input pad has pushed its EOS into decodebin3.
    drained: bool,
    /// The relink surgery ran; remaining block probes just remove
    /// themselves and let their data flow.
    swapped: bool,
    /// The output-side hold dropped an EOS while this swap was pending: the
    /// current item's end has been consumed for good, so a cancel must
    /// synthesize it or the caller never learns the item ended. An input
    /// pushing EOS into decodebin3 (`drained`) is NOT that: until the EOS
    /// re-emerges at the outputs it still flows normally once the hold
    /// disarms.
    dropped_eos: bool,
}

impl SwapState {
    /// The generation of a swap that has already PERFORMED and is still
    /// waiting to activate, if any. In that window the prepared input is the
    /// only linked upstream, so anything asking upstream (a timeline query, a
    /// cancel's surgery) is describing the SUCCESSOR item, not the one still
    /// coming out of the sinks.
    ///
    /// `swapped` alone is not the window: a long-completed activation leaves
    /// `swapped` set with `pending` cleared.
    fn activation_pending(&self) -> Option<u64> {
        self.pending.filter(|_| self.swapped)
    }
}

#[derive(Default)]
struct SwapGate {
    state: Mutex<SwapState>,
    cond: Condvar,
}

impl SwapGate {
    /// Abort any pending swap and wake every thread parked on the gate.
    /// MUST run before any downward pipeline transition while a prepare may
    /// be pending: a state change joins streaming threads, and a prepared
    /// pad's thread parked in the gate's condvar would deadlock it.
    /// Returns the aborted state for the caller's cleanup decisions.
    fn abort(&self) -> SwapState {
        let mut state = self.state.lock();
        let aborted = std::mem::take(&mut *state);
        self.cond.notify_all();
        aborted
    }
}

/// What a `cancel_prepared` did. The distinction is load-bearing for the
/// caller's pre-arm bookkeeping (see [`PlaybinEvent::PreparedCancelDeclined`])
/// and for a `Job::PrepareNext` deciding whether it may arm over the slot.
enum CancelOutcome {
    /// Nothing is prepared any more and no activation will fire.
    /// `generation` names the dropped prepare, `None` for a no-op cancel
    /// (nothing was prepared).
    Cancelled { generation: Option<u64> },
    /// A performed swap made cancellation impossible: the relink is live, the
    /// activation of `generation` is imminent and was left to finish.
    Declined { generation: u64 },
}

/// The per-load dynamic core: decodebin3 + streamsynchronizer. Rebuilt FRESH
/// for every load. These are the only elements that accumulate per-media
/// state across items (decodebin3's multiqueue keeps its interleave-tuned
/// slot sizing, collections and selection bookkeeping), and that
/// accumulation wedges later prerolls: after a run of audio-only items, a
/// reused multiqueue's audio slot filled and blocked the demuxer before the
/// first video buffer, holding an A/V preroll below PAUSED forever. A fresh
/// pair per load makes every load independent of instance history.
struct Core {
    db3: gst::Element,
    ssync: gst::Element,
    pad_added_sig: gst::glib::SignalHandlerId,
    pad_removed_sig: gst::glib::SignalHandlerId,
}

/// The user-facing half of a gapless activation, held back from decodebin3's
/// output until the new item's audio actually reaches the sink. See
/// [`Inner::held_activation`].
struct HeldActivation {
    /// The new item's stream collection, re-emitted right after
    /// [`PlaybinEvent::PreparedActivated`] (the fresh-load ordering the caller
    /// relies on).
    collection: Option<gst::StreamCollection>,
}

struct Inner {
    pipeline: gst::Pipeline,
    /// See [`Core`]. `None` only during construction.
    core: Mutex<Option<Core>>,
    /// The preroll token: a permanent `appsrc ! fakesink(sync=false)` branch
    /// whose only job is keeping every load's READY->PAUSED honestly ASYNC.
    /// At load time NO output chain is in the pipeline yet (both join at
    /// route time), so without the token the transition completes instantly:
    /// running time starts before any media exists, chains then join a
    /// committed pipeline late against a stale base_time (the QoS drop-storm
    /// class), and the caller's state machine commits off bogus settles.
    /// The token fakesink returns ASYNC like any dataless sink. Once the
    /// first real chain joins, `finish_preroll_token` feeds the appsrc one
    /// buffer + EOS, prerolling the token out of the equation (the EOSed
    /// sink also satisfies the bin's EOS aggregation, so the token never
    /// blocks the real end-of-stream). READY resets both ends for the next
    /// load. Forged ASYNC_START messages do NOT work instead: gstbin
    /// ignores them while its target is at or below READY.
    token_src: gst::Element,
    /// Held across DOWNWARD pipeline transitions (stop, the load reset).
    /// `route_db3_pad` try-locks it and refuses pads while a teardown is in
    /// flight. A polling state-query gate alone is TOCTOU-racy: a pad
    /// exposed microseconds before a Stop's READY descent routed anyway and
    /// its chain activation deadlocked against the descending set_state.
    /// Always held through [`RouteGate`], whose release re-attempts
    /// `deferred_pads`.
    route_gate: Mutex<()>,
    /// The chain-join half of `route_gate`, held by `fpb-join` across a chain
    /// activation and by every downward transition (which takes BOTH, in that
    /// order). DELIBERATELY NOT the same lock as `route_gate`: a join can
    /// block for as long as a sink's transition takes, and a route that finds
    /// `route_gate` taken DEFERS rather than waits, so joining under the route
    /// gate made every multi-stream load defer its second stream. A deferred
    /// pad is unlinked while it waits, so decodebin3 pushes its first buffer
    /// into nothing and multiqueue drops it (measured: the audio sink's first
    /// buffer became pts=20ms with no DISCONT in 5 of 6 `toml_scenarios`
    /// runs). Two locks make joins exclusive with descents, which is what they
    /// need, without ever making a route wait.
    ///
    /// Lock order where both are taken: `route_gate` then `join_gate`, and
    /// [`Inner::gate`] is the only place that does it.
    join_gate: Mutex<()>,
    /// The pending eager text-branch flush, as a COALESCING intent (latest
    /// wins, so a run of rapid switches leaves one flush, not a backlog).
    /// Written by `pump_selection`'s dispatch, executed only by the worker
    /// through [`Inner::run_deferred_text_work`], which every pipeline
    /// state edge re-attempts. Cleared by teardown and the load reset, the
    /// intent is per-item selection state.
    deferred_text_work: Mutex<Option<DeferredTextWork>>,
    /// See [`TextSeat`]. Shared with [`Teardown`], which disposes of the
    /// branches this crate could not.
    text_seat: Arc<TextSeat>,
    /// Text branches unlinked but not yet torn down, because the pipeline was
    /// at rest in PAUSED when they were detached. Drained by
    /// [`Inner::run_deferred_text_work`].
    deferred_text_disposal: Mutex<Vec<TextDisposal>>,
    /// Inputs a user DETACH took out of the routing state but could not tear
    /// down yet, because the pipeline was at rest in PAUSED. Drained by
    /// [`Inner::run_deferred_text_work`]. Teardown paths never use this, see
    /// [`Inner::remove_input_or_defer`].
    deferred_input_removal: Mutex<Vec<Input>>,
    /// Inputs with a replay verification already armed, so a second arming
    /// cannot start a rival chain. See [`Inner::arm_replay_verification`].
    replay_checks_armed: Mutex<std::collections::HashSet<(ExternalSubId, u32)>>,
    /// Replays owed once the pipeline is playing again, because a flushing
    /// seek cannot be delivered to one at rest in PAUSED. See
    /// [`FcastPlaybin::replay_subtitle`].
    deferred_replays: Mutex<Vec<(ExternalSubId, u32, u32)>>,
    /// Replay verdicts held because the check fired at a pipeline below a
    /// settled PLAYING, where nothing flows and the branch stickies it would
    /// read are leftovers of the input's previous tenure. Drained by
    /// [`Inner::run_deferred_text_work`], which re-arms each held check so
    /// it fires against a pipeline that can actually deliver. See
    /// [`FcastPlaybin::verify_replay`].
    deferred_verifications: Mutex<Vec<(ExternalSubId, u32, u32)>>,
    /// Whether the LAST [`Job::DrainTextWork`] the worker ran was a no-op
    /// (postponed work pending, pipeline below a settled PLAYING). While
    /// set, [`Inner::poll_text_policy`] suppresses its per-poll re-poke,
    /// because a drain already ran against this exact situation and decided
    /// it cannot proceed, so re-running it on every 5ms poll is a busy loop
    /// (measured at one worker job per poll for the whole time a pipeline
    /// sat parked in Buffering). Cleared by every event that could change
    /// that verdict, which keeps the poke live exactly when it can matter.
    /// The recording of any new postponed item clears it, and the drain
    /// itself clears it when it proceeds. Pipeline state edges do not need
    /// to clear it because the bus translation queues the drain on every
    /// edge unconditionally, and that queued run refreshes the verdict.
    /// Suppression is disabled by the FCAST_NO_DRAIN_POKE_SUPPRESS lever,
    /// which restores the poke-on-every-poll behavior. The flag is the only
    /// thing the lever's branch reads, so the lever covers the whole
    /// change.
    drain_poke_parked: AtomicBool,
    /// Diagnostic count of [`Job::DrainTextWork`] jobs the worker received.
    /// Read through [`FcastPlaybin::drain_text_job_count`] by the busy-loop
    /// regression test. Not behavior.
    drain_jobs_seen: AtomicU64,
    /// Whether the LAST selection this crate dispatched to decodebin3 turned
    /// video off entirely (a video-bearing collection with no video id in the
    /// selection). Written on the select-sender thread BEFORE the send, so a
    /// pad decodebin3 exposes inside the send already reads the intent that
    /// selection carries. Read by `route_db3_pad`, whose Video arm must not
    /// rebuild the video chain for a stream decodebin3's collection-default
    /// auto-select resurrected over an explicit deselect. The selection
    /// engine owns the real desire and self-corrects the divergence. This
    /// mirror exists only because the route decision runs on a streaming
    /// thread at pad-exposure time, where the engine's answer arrives too
    /// late (see the Video arm). Reset per load and at teardown, carried
    /// across gapless exactly like the engine's carried video-off desire.
    ///
    /// Deliberately an atomic rather than an accessor on the selection
    /// engine, and both halves of that are load-bearing. Reading the engine
    /// would take a mutex on a streaming thread, which is the lock-ordering
    /// hazard that wedged the pipeline against pad stream locks before the
    /// routing lock learned to collect and then act. It would also answer
    /// the wrong question, reporting the CURRENT desire when a pad exposed
    /// inline inside `send_event` needs the intent of the selection being
    /// dispatched right then.
    video_deselected: AtomicBool,
    /// A VIDEO stream was unrouted at least once since the load. The
    /// drained-resurrect park in `route_db3_pad` only applies to a
    /// RE-route. At the initial route of a fast-paced item the input can
    /// already be at EOS while decodebin3's multiqueue still holds the
    /// whole stream, and parking that first route would silence video for
    /// the item. Reset per load.
    video_unrouted_once: AtomicBool,
    /// decodebin3 source pads from the CURRENT core that `route_db3_pad`
    /// refused because `route_gate` was momentarily held by a concurrent
    /// downward transition. Dropping them for good stalled the active load
    /// (audio routed but video lost -> never prerolls, the load-stall race).
    /// Every [`RouteGate`] release re-attempts them, and the routing guards
    /// re-reject any that are genuinely stale.
    deferred_pads: Mutex<Vec<gst::Pad>>,
    /// The generation of the CURRENT load: stamped on every emitted event
    /// and on every attached input. Callers compare against the value
    /// returned by [`FcastPlaybin::load_async`] to drop events from
    /// superseded loads exactly, and inputs whose generation is behind it
    /// classify as [`ErrorOrigin::Stale`].
    generation: AtomicU64,
    /// Allocator for `generation`: bumped when a load is REQUESTED (so the
    /// caller knows the tag up front), adopted by the load at its reset.
    next_generation: AtomicU64,
    overlay: gst::Element,
    /// Head of the audio chain (the decoupling queue's sink pad).
    audio_entry: gst::Element,
    volume: gst::Element,
    /// The video output chain (subtitleoverlay + video sink). It lives in
    /// the pipeline ONLY while the item has a routed video stream
    /// (`ensure_video_chain`/`remove_video_chain`), exactly like the
    /// per-load audio sink: an absent chain cannot hang a video-less
    /// preroll and never counts in the bin's EOS/STREAM_START aggregation,
    /// by construction. The preroll token (see `token_src`) keeps a load
    /// ASYNC while no chain has joined yet. The video sink is caller-owned
    /// and GL/window-bound, so it parks at READY when out of the pipeline
    /// and is never NULLed (playbin3's own treatment of it).
    video_chain: Vec<gst::Element>,
    /// How the audio sink is built: once per load, when audio routes, and
    /// the previous sink is dropped at the load reset. Reusing one sink
    /// across a session degrades: pulsesink holds its `pa_context` open at
    /// READY and a context carried across dozens of loads eventually returns
    /// "Disconnected: Bad state" on the READY->PAUSED that starts a load.
    /// A fresh element per load gives a fresh context, playsink's own
    /// behavior.
    audio: AudioSink,
    /// The audio sink built for the current load, linked `volume ! sink`.
    /// `None` between the load reset and the first audio route, or for a
    /// video-only item.
    audio_sink: Mutex<Option<gst::Element>>,
    /// The caller's event handler (see [`FcastPlaybin::set_event_handler`]).
    /// Events are silently dropped until one is installed.
    events: Mutex<Option<EventCallback>>,
    /// Feeds the worker thread (see [`Job`]). The worker owns the receiver
    /// and exits when this sender is dropped with `Inner`.
    work_tx: mpsc::Sender<Job>,
    /// Feeds the SELECT_STREAMS sender thread (see [`SelectJob`]). Same
    /// lifetime discipline as `work_tx`.
    select_tx: mpsc::Sender<SelectJob>,
    /// Feeds the replay-seek sender thread (see [`ReplayJob`]). Same
    /// lifetime discipline as `work_tx`.
    replay_tx: mpsc::Sender<ReplayJob>,
    /// Feeds the chain-join thread (see [`ChainJoinJob`]). Same lifetime
    /// discipline as `work_tx`.
    join_tx: mpsc::Sender<ChainJoinJob>,
    routing: Mutex<RoutingState>,
    /// The declarative track-selection engine (see the [`selection`] module
    /// docs). Recording happens at bus-translate time, dispatch only in
    /// [`FcastPlaybin::pump_selection`]. Lock order: `routing` before
    /// `selection`, never the reverse.
    selection: Mutex<selection::SelectionEngine>,
    /// The subtitle sid of the last APPLIED selection (StreamsSelected as
    /// reported, never the engine's optimistic in-flight state). Only the
    /// selection-time external replay reads it: the engine's `applied` is
    /// already the new target by the time the confirmation arrives, so it
    /// cannot say whether the slot MOVED. Reset wherever the engine resets.
    last_applied_subtitle: Mutex<Option<String>>,
    /// Whether the MAIN input answers the SELECTABLE query (an adaptive
    /// demuxer): decodebin3 then defers ALL selection upstream and never
    /// posts STREAMS_SELECTED (`is_selection_done` returns early). `None`
    /// until first asked, reset wherever the engine resets. See
    /// [`Inner::upstream_owns_selection`].
    upstream_selection: Mutex<Option<bool>>,
    /// The upstream-owned id set last dispatched while upstream owns
    /// selection: an adaptive demuxer only confirms an activation EDGE, so a
    /// no-op re-send would never confirm and must be settled locally instead.
    last_upstream_ids: Mutex<Vec<String>>,
    /// The timeline the current item is MEANT to render against: rate and
    /// the position whose running time is zero, recorded when a load's
    /// start seek or a user seek is issued. `overlay_timeline` falls back to
    /// it while the overlay has no sticky segment yet, so an external
    /// replay that runs inside that window still lands on the right
    /// timeline instead of zero.
    intended_timeline: Mutex<(f64, gst::ClockTime)>,
    /// Serializes decodebin3 sink-pad requests. Concurrent
    /// `request_pad_simple("sink_%u")` calls (an input's pad-added streaming
    /// threads racing an inline `attach_subtitle`) can both draw the same
    /// name inside decodebin3; the second add fails ("Padname sink_0 is not
    /// unique") and the broken pad object panics the requesting thread in
    /// the bindings, which killed streaming threads mid-lock in the field.
    db3_pad_request: Mutex<()>,
    /// The external-subtitle materialization timeout, normally
    /// [`EXTERNAL_SUB_TIMEOUT`]. Mutable only so tests can shorten it
    /// ([`FcastPlaybin::set_external_sub_timeout`]).
    sub_timeout: Mutex<Duration>,
    /// The pre-armed next item, if any (see [`PreparedNext`]). Lock order:
    /// take and RELEASE this before `routing`/`selection`, never hold it
    /// across them.
    prepared: Mutex<Option<PreparedNext>>,
    /// See [`SwapGate`].
    swap_gate: SwapGate,
    /// The group id of the item currently flowing OUT of decodebin3 (from
    /// STREAM_START on its output pads; reset per load). A change while a
    /// prepared item is pending IS the gapless activation: decodebin3
    /// posts no new streams-selected for a same-slot continuation, so the
    /// data plane's group id is the reliable switch signal (uridecodebin3
    /// tracks output activation the same way).
    active_group: Mutex<Option<gst::GroupId>>,
    /// The group id the last gapless activation RETIRED (the previous
    /// item's). Output pads still carrying it have their EOS dropped even
    /// after the activation cleared the swap gate: the selection-side
    /// activation trigger can fire while the old item's tail is still
    /// draining out of decodebin3, and an old EOS reaching the sinks there
    /// can end the pipeline between items. Reset per load.
    retired_group: Mutex<Option<gst::GroupId>>,
    /// The group whose EOS the output gate committed to LETTING THROUGH
    /// into streamsynchronizer. A short item's fastest stream (audio
    /// decodes a whole 2s clip in milliseconds) can push its EOS past the
    /// output gate BEFORE a pre-arm arms it. streamsynchronizer then parks
    /// that stream's pushing thread (the multiqueue slot task!) until the
    /// whole group is EOS, and the parked task can never deliver the next
    /// item's stream-start queued behind it. Dropping the group's REMAINING
    /// EOS at the output gate would leave the group forever incomplete and
    /// wedge the switch, so the gate is all-or-nothing per group: once one
    /// EOS of a group passed, its siblings pass too, streamsynchronizer
    /// completes the group and re-emits EOS on its src pads, where the
    /// post-ssync gate consumes them before they reach the sinks. Reset per
    /// load.
    passing_eos_group: Mutex<Option<gst::GroupId>>,
    /// Stream ids whose INPUT-side stream has delivered EOS into decodebin3
    /// and has not been restarted by a flush since (recorded by a probe on
    /// every input pad linked into decodebin3, see `Inner::link_input_pad`).
    /// A deselected stream's end never reaches the output probes (its slot
    /// is gone), so `passing_eos_group` cannot see it, and re-routing such
    /// a stream builds a chain that can never preroll. The
    /// drained-resurrect park in `route_db3_pad` consults this next to the
    /// group mirror. Reset per load. Maintained only while the park's lever
    /// is unset (`FCAST_NO_DRAINED_RESURRECT_PARK`).
    input_eos_sids: Mutex<std::collections::HashSet<String>>,
    /// A gapless activation's user-facing events (PreparedActivated + the new
    /// item's collection), held back from decodebin3's output until the new
    /// item's audio crosses the decoupling queue to the sink. The switch is
    /// detected at decodebin3's output, one decoupling-queue ahead of the
    /// speakers, so emitting the title/duration there flips the UI before the
    /// sound. The `fpb-aqueue` src STREAM_START probe releases this when the
    /// item's audio actually reaches the sink, matching the sink-anchored
    /// playback position. Only set for items with audio (the release is
    /// anchored on the audio queue); audio-less items emit immediately. At
    /// most one is ever held: real media never runs two swaps within one
    /// queue depth, and a superseding activation flushes any prior hold.
    /// Cleared per load.
    held_activation: Mutex<Option<HeldActivation>>,
    /// The DROP probe a mid-item video deselect leaves on the pad feeding the
    /// video chain (see `park_video_chain_for_deselect`). Removed when the
    /// chain rejoins or leaves the pipeline, so a re-selected video stream is
    /// never dropped. The pad is kept alongside the id because the probe has
    /// to be removed from the same pad it was added to, and by then the
    /// overlay may already be unlinked from it.
    video_park_probe: Mutex<Option<(gst::Pad, gst::PadProbeId)>>,
    /// Serialises the video chain's MEMBERSHIP change (see
    /// [`Inner::attach_video_chain`]). A leaf lock: nothing is taken under it.
    video_chain_membership: Mutex<()>,
}

/// An RAII hold on [`Inner::route_gate`], [`Inner::join_gate`] or both.
/// Dropping it releases what it holds FIRST and then re-attempts
/// `deferred_pads`, so the invariant is simply "every gate release drains": a
/// pad deferred while any holder had the gate is re-routed the moment that
/// holder finishes, with no polling thread.
struct RouteGate<'a> {
    inner: &'a Arc<Inner>,
    guard: Option<parking_lot::MutexGuard<'a, ()>>,
    join: Option<parking_lot::MutexGuard<'a, ()>>,
}

impl Drop for RouteGate<'_> {
    fn drop(&mut self) {
        // Release the mutexes before draining: the drain re-enters
        // `route_db3_pad`, which must be able to take the gate itself.
        self.join.take();
        self.guard.take();
        Inner::drain_deferred_pads(self.inner);
    }
}

impl Inner {
    /// Take BOTH gates (blocking): a downward transition excludes routes and
    /// chain joins alike. See [`RouteGate`] and [`Inner::join_gate`].
    fn gate(inner: &Arc<Inner>) -> RouteGate<'_> {
        let guard = inner.route_gate.lock();
        let join = inner.join_gate.lock();
        RouteGate {
            inner,
            guard: Some(guard),
            join: Some(join),
        }
    }

    /// Take the route gate without blocking. See [`RouteGate`].
    fn try_gate(inner: &Arc<Inner>) -> Option<RouteGate<'_>> {
        inner.route_gate.try_lock().map(|guard| RouteGate {
            inner,
            guard: Some(guard),
            join: None,
        })
    }

    /// Take the JOIN gate only (blocking), which excludes downward transitions
    /// without making a concurrent route defer. See [`Inner::join_gate`].
    fn join_hold(inner: &Arc<Inner>) -> RouteGate<'_> {
        RouteGate {
            inner,
            guard: None,
            join: Some(inner.join_gate.lock()),
        }
    }
}

/// Builds a fresh audio sink. See [`AudioSink::Factory`].
pub type AudioSinkFactory = Box<dyn Fn() -> Result<gst::Element> + Send + Sync>;

/// How the audio sink is built. Whatever the choice, the sink is built FRESH
/// for every load and dropped at the next load's reset (see [`Inner::audio`]
/// for why reuse degrades pulsesink).
pub enum AudioSink {
    /// `autoaudiosink` per load.
    Auto,
    /// Caller-provided factory, invoked once per load.
    Factory(AudioSinkFactory),
}

/// The playback orchestrator. `Clone` is a cheap handle onto the same
/// pipeline. Internal callbacks run on GStreamer streaming threads and only
/// touch `RoutingState` under its lock.
///
/// # Threading
///
/// Every method is callable from any thread EXCEPT a GStreamer streaming
/// thread or the event callback: the state-changing calls
/// ([`play`](Self::play)/[`pause`](Self::pause)/[`stop`](Self::stop)/
/// [`load`](Self::load)/[`set_pipeline_state`](Self::set_pipeline_state))
/// wrap `gst_element_set_state`, which is MT-safe but may wait on the very
/// streaming threads it reconfigures (the standard GStreamer self-deadlock).
/// From event loops, bus callbacks, or anywhere blocking is unacceptable,
/// use the `_async` variants: they queue onto the crate's worker thread,
/// which also keeps the operations ordered. Downward transitions take the
/// internal route gate (`stop`, `set_pipeline_state`, the worker's jobs).
/// `play`/`pause` are upward and need none.
#[derive(Clone)]
pub struct FcastPlaybin {
    inner: Arc<Inner>,
}

/// Sink configuration.
pub struct Sinks {
    /// The video sink. `None` picks a throwaway synced fake sink
    /// (spike/tests). In the pipeline only while the item has video, parked
    /// at READY otherwise, never NULLed (caller-owned, GL/window-bound).
    pub video: Option<gst::Element>,
    /// How the per-load audio sink is built (see [`AudioSink`]).
    pub audio: AudioSink,
}

/// Audio decoupling queue depth (`fpb-aqueue` `max-size-time`). Chosen purely
/// for decoupling headroom: it absorbs sink scheduling jitter so a busy CPU
/// never starves the audio sink. 1s is generous (audio decodes far faster than
/// realtime and upstream use-buffering handles the source). It must stay below
/// the gapless pre-arm lead so the EOS-hold at decodebin3's output still
/// catches a late pre-arm's outgoing EOS (the pre-arm arms seconds before the
/// end; 1s of race-ahead is well inside that). It does NOT set the gapless
/// transition seam any more: the user-facing item switch is HELD and released
/// when the new item's audio actually crosses this queue to the sink (see
/// [`Inner::held_activation`]), so the title/duration flip with the sound
/// regardless of this depth. Deep only while a video chain is present, to
/// absorb audio during a video re-preroll (the A/V mid-load deadlock the queue
/// exists for). See `aqueue` in `new`.
const AQUEUE_AUDIO_TIME_NS: u64 = 1_000_000_000;
const AQUEUE_VIDEO_TIME_NS: u64 = 30 * 1_000_000_000;

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("creating {factory} ({name})"))
}

/// Opt-in (`FCAST_FORCE_SYSTEM_CLOCK=1`): pin the pipeline to the monotonic
/// system clock instead of electing the audio sink's.
///
/// Every captured player wedge shares one keystone: a video-branch thread
/// parked in `gst_base_sink_wait_clock` on the AUDIO SINK's clock after that
/// clock stopped advancing (switch backpressure, an audio deselect releasing
/// the ring buffer, or a stuck pulse stream). The parked thread holds the
/// sink's stream lock and back-pressures the single demuxer thread into a
/// cycle nothing internal can break. A monotonic clock's waits always
/// complete, so the cycles cannot close (validated under stress).
///
/// NOT the default yet: through the PulseAudio shim the audio sink must
/// SLAVE to the external clock and both slaving modes audibly regress
/// (`skew` pops on jittery-latency corrections, `resample` broke near-EOS
/// draining). The native PipeWire sink shares the monotonic clock domain,
/// so once it is everywhere this becomes the default.
fn force_system_clock() -> bool {
    std::env::var("FCAST_FORCE_SYSTEM_CLOCK").is_ok_and(|v| v == "1")
}

/// Seek flags for a `rate`. TRICKMODE lets decoders drop frames to keep up:
/// right for fast-scrub, wrong for pitch-corrected speed playback where
/// scaletempo wants every frame. Only high forward rates and reverse (which
/// can't be decoded frame-complete anyway) enable it, so a 1.25x/1.5x/2x
/// "watch faster" stays full quality.
fn seek_flags_for(rate: f64) -> gst::SeekFlags {
    let mut flags = gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH;
    if rate < 0.0 || rate > 2.0 {
        flags |= gst::SeekFlags::TRICKMODE;
    }
    flags
}

/// The flushing ACCURATE seek event [`send_rate_seek`] sends, so every other
/// issuer of "the same seek" (the refresh flush, the external-input
/// forwarding) lands on exactly the same timeline instead of re-deriving it.
/// `seqnum` stamps the event for callers that need to attribute the answer.
fn rate_seek_event(
    rate: f64,
    position: gst::ClockTime,
    seqnum: Option<gst::Seqnum>,
) -> gst::Event {
    let flags = seek_flags_for(rate);
    let builder = if rate >= 0.0 {
        gst::event::Seek::builder(
            rate,
            flags,
            gst::SeekType::Set,
            position,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        )
    } else {
        gst::event::Seek::builder(
            rate,
            flags,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::End,
            position,
        )
    };
    match seqnum {
        Some(seqnum) => builder.seqnum(seqnum).build(),
        None => builder.build(),
    }
}

/// A flushing ACCURATE seek to `position` at `rate`, handling reverse rates
/// (seek from the end).
fn send_rate_seek(
    pipeline: &gst::Pipeline,
    rate: f64,
    position: gst::ClockTime,
) -> std::result::Result<(), gst::glib::error::BoolError> {
    let flags = seek_flags_for(rate);
    if rate >= 0.0 {
        pipeline.seek(
            rate,
            flags,
            gst::SeekType::Set,
            position,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        )
    } else {
        pipeline.seek(
            rate,
            flags,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::End,
            position,
        )
    }
}

impl FcastPlaybin {
    pub fn new(sinks: Sinks) -> Result<Self> {
        let pipeline = gst::Pipeline::builder().name("fcastplaybin").build();

        // Opt-in until the native PipeWire sink is everywhere (see
        // `force_system_clock`).
        if force_system_clock() {
            pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        }

        let overlay = make("subtitleoverlay", "fpb-suboverlay")?;
        // The fake sink is a test/spike convenience, not a headless mode:
        // video still fully decodes into it. Callers that want to skip video
        // work deselect the video stream instead.
        let video_sink = match sinks.video {
            Some(sink) => {
                // playsink parity: QoS events from the video sink drive
                // decoder frame-skipping and fimagedec's schedule rebase.
                // Without them a slow decode path silently drops at the sink
                // with no way for upstream to adapt.
                if sink.has_property_with_type("qos", bool::static_type()) {
                    sink.set_property("qos", true);
                }
                sink
            }
            None => {
                let sink = make("fakesink", "fpb-fake-vsink")?;
                sink.set_property("sync", true);
                sink
            }
        };

        let aconv = make("audioconvert", "fpb-aconv")?;
        let aresample = make("audioresample", "fpb-aresample")?;
        // Pitch-preserving rate change. `fcastaudiostretch` (PICOLA) replaced `scaletempo`
        // (SOLA), which buzzed on speech at non-1.0 rates and needed
        // `scaletempo-s16-overlap-overflow.patch` for a click on the first buffer after
        // engaging. Both consume the segment rate identically, so this is a drop-in swap;
        // FCAST_SCALETEMPO=1 restores the old element for an A/B without a rebuild.
        let scaletempo = if std::env::var_os("FCAST_SCALETEMPO").is_some() {
            make("scaletempo", "fpb-scaletempo")?
        } else {
            make("fcastaudiostretch", "fpb-audiostretch")?
        };
        let volume = make("volume", "fpb-volume")?;
        // Decoupling queue at the head of the audio branch. Without it, a
        // paused audio sink (parked in wait_preroll during a mid-load
        // re-preroll) backpressures through streamsynchronizer into
        // decodebin3's multiqueue and stalls the single demuxer thread,
        // which then can't feed VIDEO, so the video sink never re-prerolls
        // and the whole pipeline deadlocks. The queue absorbs the audio that
        // piles up during the video re-preroll window. Bounded by TIME (the
        // default 1s cap is what bites, VIDEO depth caps memory to a few MB
        // of PCM) with no min-threshold, so it adds no playback latency.
        //
        // The depth is video-conditional (see `AQUEUE_*_TIME_NS`): the deep
        // buffer is ONLY needed while a video chain can re-preroll. For
        // audio-only playback a deep queue is actively harmful to gapless: it
        // carries the outgoing item's decoded EOS past the gapless EOS-hold
        // (at decodebin3's output, upstream of this queue) tens of seconds
        // before the speakers reach it, so a pre-arm that lands later (the
        // field: the sender queues the next track seconds before the end) is
        // too late and the item is skipped. A shallow audio-only queue keeps
        // the hold near the sink boundary. `ensure_video_chain` deepens it.
        let aqueue = make("queue", "fpb-aqueue")?;
        aqueue.set_property("max-size-time", AQUEUE_AUDIO_TIME_NS);
        aqueue.set_property("max-size-bytes", 0u32);
        aqueue.set_property("max-size-buffers", 0u32);

        let token_src = make("appsrc", "fpb-token-src")?;
        token_src.set_property_from_str("format", "time");
        let token_sink = make("fakesink", "fpb-token-sink")?;
        token_sink.set_property("sync", false);
        token_sink.set_property("enable-last-sample", false);
        // The token must carry the load's ASYNC (a message-level mechanism)
        // but stay invisible to everything GstBin routes through SINK-flagged
        // children: seeking queries (appsrc answers "not seekable" and
        // poisons the pipeline's seekability), seek events (a flushing seek
        // would flush the token's preroll away and hang waiting for it), and
        // EOS aggregation.
        token_sink.unset_element_flags(gst::ElementFlags::SINK);

        // The video chain (subtitleoverlay + video sink) is NOT added here:
        // it lives in the pipeline only while the item has a routed video
        // stream (`ensure_video_chain`/`remove_video_chain`), exactly like
        // the per-load audio sink. An absent chain cannot hang a video-less
        // preroll and cannot swallow the bin's EOS/STREAM_START aggregation,
        // by construction (this replaces the old locked-state + SINK-flag
        // deactivation games).
        pipeline.add_many([
            &aqueue,
            &aconv,
            &aresample,
            &scaletempo,
            &volume,
            &token_src,
            &token_sink,
        ])?;
        token_src.link(&token_sink)?;

        // Static links. Everything upstream of these is dynamic. The video
        // sink links DIRECTLY to subtitleoverlay, no converter in between:
        // the receiver's sink negotiates DMA-BUF/zero-copy caps that a
        // videoconvert would reject, and accepts plain raw video too.
        // Callers with a pickier sink wrap it in a bin with a converter.
        // The audio sink is built and linked per load (`ensure_audio_sink`);
        // the overlay-to-video-sink link is made when the chain first joins
        // the pipeline and persists across its membership changes.
        gst::Element::link_many([&aqueue, &aconv, &aresample, &scaletempo, &volume])?;

        let (work_tx, work_rx) = mpsc::channel();
        let (select_tx, select_rx) = mpsc::channel();
        let (replay_tx, replay_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();

        let inner = Arc::new(Inner {
            video_chain: vec![overlay.clone(), video_sink.clone()],
            audio: sinks.audio,
            audio_sink: Mutex::new(None),
            pipeline,
            core: Mutex::new(None),
            token_src,
            route_gate: Mutex::new(()),
            join_gate: Mutex::new(()),
            deferred_pads: Mutex::new(Vec::new()),
            deferred_text_work: Mutex::new(None),
            text_seat: Arc::new(TextSeat::default()),
            deferred_text_disposal: Mutex::new(Vec::new()),
            deferred_input_removal: Mutex::new(Vec::new()),
            replay_checks_armed: Mutex::new(std::collections::HashSet::new()),
            deferred_replays: Mutex::new(Vec::new()),
            deferred_verifications: Mutex::new(Vec::new()),
            drain_poke_parked: AtomicBool::new(false),
            drain_jobs_seen: AtomicU64::new(0),
            video_deselected: AtomicBool::new(false),
            video_unrouted_once: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            next_generation: AtomicU64::new(0),
            overlay,
            // The audio branch's head is the decoupling queue. ssync links here.
            audio_entry: aqueue,
            volume,
            events: Mutex::new(None),
            work_tx,
            select_tx,
            replay_tx,
            join_tx,
            routing: Mutex::new(RoutingState::default()),
            selection: Mutex::new(selection::SelectionEngine::new()),
            last_applied_subtitle: Mutex::new(None),
            upstream_selection: Mutex::new(None),
            last_upstream_ids: Mutex::new(Vec::new()),
            intended_timeline: Mutex::new((1.0, gst::ClockTime::ZERO)),
            db3_pad_request: Mutex::new(()),
            sub_timeout: Mutex::new(EXTERNAL_SUB_TIMEOUT),
            prepared: Mutex::new(None),
            swap_gate: SwapGate::default(),
            active_group: Mutex::new(None),
            retired_group: Mutex::new(None),
            passing_eos_group: Mutex::new(None),
            input_eos_sids: Mutex::new(std::collections::HashSet::new()),
            held_activation: Mutex::new(None),
            video_park_probe: Mutex::new(None),
            video_chain_membership: Mutex::new(()),
        });

        Inner::install_core(&inner)?;

        // The sink-boundary release for a held gapless activation (see
        // `Inner::held_activation`). A STREAM_START leaving the decoupling
        // queue means the item behind it has drained and the NEXT item's audio
        // is now reaching the sink: exactly one STREAM_START crosses per audio
        // item, and a hold is only ever armed at a gapless boundary, so the
        // first STREAM_START after an arm is that boundary. No group-id
        // bookkeeping needed (and none possible: streamsynchronizer may rewrite
        // group ids downstream). The initial load's STREAM_START finds no hold
        // and is a no-op.
        if let Some(src) = inner.audio_entry.static_pad("src") {
            let weak = Arc::downgrade(&inner);
            src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::StreamStart(_))
                    && let Some(inner) = weak.upgrade()
                {
                    inner.release_held_activation();
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Trigger #2 for the text running-time alignment (see
        // `Inner::sync_text_running_time`): a SEGMENT reaching the overlay's
        // video input is the ONLY event that changes what the alignment should
        // be, and it is the only one a REUSED text slot produces at a gapless
        // boundary (nothing re-links, so `poll_text_policy` is never re-entered,
        // and receiver-core's `async_done` never fires). Over a full run this
        // sees exactly two: the load's (base 0, a no-op) and the swap's.
        //
        // The probe runs on the VIDEO streaming thread and therefore takes NO
        // lock: it only posts the job, and the worker does the work.
        if let Some(sink) = inner.overlay.static_pad("video_sink") {
            let weak = Arc::downgrade(&inner);
            sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::Segment(_))
                    && let Some(inner) = weak.upgrade()
                {
                    let _ = inner.work_tx.send(Job::SyncTextRunningTime);
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Volume notifies become events. The dedicated element makes them
        // deterministic (see `set_volume`).
        inner.volume.connect_notify(Some("volume"), {
            let weak = Arc::downgrade(&inner);
            move |volume, _pspec| {
                if let Some(inner) = weak.upgrade() {
                    inner.emit(PlaybinEvent::VolumeChanged(
                        volume.property::<f64>("volume"),
                    ));
                }
            }
        });

        // The worker holds only a Weak: it never keeps the pipeline alive,
        // and it exits when the last handle drops (the channel closes).
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("fcastplaybin".to_owned())
            .spawn(move || Inner::worker_loop(weak, work_rx))
            .context("spawning the fcastplaybin worker")?;

        // The SELECT_STREAMS sender (see `FcastPlaybin::select_streams`),
        // same Weak/channel lifetime as the worker.
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("fpb-select".to_owned())
            .spawn(move || Inner::select_sender_loop(weak, select_rx))
            .context("spawning the fcastplaybin select sender")?;

        // The replay-seek sender (see `ReplayJob`), same Weak/channel
        // lifetime as the worker.
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("fpb-replay".to_owned())
            .spawn(move || Inner::replay_sender_loop(weak, replay_rx))
            .context("spawning the fcastplaybin replay sender")?;

        // The chain-join thread (see `ChainJoinJob`), same Weak/channel
        // lifetime as the worker.
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("fpb-join".to_owned())
            .spawn(move || Inner::chain_join_loop(weak, join_rx))
            .context("spawning the fcastplaybin chain joiner")?;

        Ok(Self { inner })
    }

    /// The bus, for callers that self-serve their messages (the spike).
    /// Unusable once [`set_event_handler`](Self::set_event_handler) installs
    /// its sync handler: every message is consumed there.
    pub fn bus(&self) -> gst::Bus {
        self.inner
            .pipeline
            .bus()
            .expect("pipeline always has a bus")
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.inner.pipeline
    }

    /// Own the bus and deliver typed [`PlaybinEvent`]s through `events`
    /// instead. Call at most once, before driving playback. Translation runs
    /// as a bus SYNC handler on the posting (streaming) thread, so the
    /// callback must be cheap and non-blocking (forward into a channel).
    /// Worker feedback ([`PlaybinEvent::Loaded`], seek outcomes) arrives
    /// through the same callback. The callback's second argument is the
    /// generation of the load the event belongs to (see
    /// [`load_async`](Self::load_async)).
    ///
    /// `hook`, when given, gets first look at every raw message (also on the
    /// posting thread) for caller-specific traffic like `NeedContext`.
    /// Returning `true` consumes the message.
    pub fn set_event_handler(
        &self,
        hook: Option<MessageHook>,
        events: impl Fn(PlaybinEvent, u64) + Send + Sync + 'static,
    ) {
        *self.inner.events.lock() = Some(Arc::new(events));
        // Weak: a strong clone here would cycle pipeline -> bus -> handler.
        let weak = Arc::downgrade(&self.inner);
        self.bus().set_sync_handler(move |_, msg| {
            // This upgrade may turn out to be the LAST strong reference, and
            // it dies on the posting (streaming) thread. `Inner::drop` is
            // written for exactly that; see the comment above `impl Drop for
            // Inner`.
            if let Some(inner) = weak.upgrade() {
                if let Some(hook) = &hook
                    && hook(msg)
                {
                    return gst::BusSyncReply::Drop;
                }
                if let Some(event) = inner.translate_message(msg) {
                    inner.emit(event);
                }
            }
            gst::BusSyncReply::Drop
        });
    }

    /// Load a new media input, replacing the previous one (and any attached
    /// external subtitles). The pipeline ends in READY with the new input
    /// wired. Call [`play`]/[`pause`] to start. The returned outcome carries
    /// the load's generation (see [`load_async`](Self::load_async)).
    pub fn load(&self, input: MediaInput, start: StartPoint) -> Result<StartOutcome> {
        let generation = self.inner.allocate_generation();
        self.load_with_generation(input, start, generation)
    }

    fn load_with_generation(
        &self,
        input: MediaInput,
        start: StartPoint,
        generation: u64,
    ) -> Result<StartOutcome> {
        let inner = &self.inner;
        {
            // No routes during the reset (see `Inner::route_gate`).
            let _gate = Inner::gate(inner);
            // A pending prepared next input is superseded by this load; its
            // element is in `inputs` and leaves with the rest. Wake any of
            // its threads parked on the swap gate BEFORE the state change,
            // which joins streaming threads.
            inner.swap_gate.abort();
            *inner.prepared.lock() = None;
            inner
                .pipeline
                .set_state(gst::State::Ready)
                .context("pipeline to READY for load")?;
            Inner::remove_all_inputs(inner);

            // Fresh dynamic core per load (see `Core`).
            Inner::teardown_core(inner);
            Inner::install_core(inner)?;

            // Drop the previous load's audio sink at this quiescent point
            // (pipeline at READY, under the gate) so the next audio route
            // builds a fresh one (see `Inner::audio`).
            inner.remove_audio_sink();

            // The video chain leaves the pipeline between items. Routing
            // re-adds it iff the item has video (see `Inner::video_chain`).
            inner.remove_video_chain();
        }

        // Everything after this point belongs to the new load: events emitted
        // earlier (teardown stragglers) still carry the previous generation.
        inner.generation.store(generation, Ordering::SeqCst);
        // The previous item's collection is gone with its core.
        inner.routing.lock().collection_video_ids.clear();
        // A fresh core outputs a fresh group; the first STREAM_START
        // records it (see `Inner::active_group`).
        *inner.active_group.lock() = None;
        *inner.retired_group.lock() = None;
        *inner.passing_eos_group.lock() = None;
        inner.input_eos_sids.lock().clear();
        // A fresh load supersedes any gapless activation still held for the
        // sink boundary; its events belong to a play item this load replaces.
        *inner.held_activation.lock() = None;
        // Track desires are per-item: the new load starts on the pipeline's
        // own defaults. A pending eager flush was the previous item's too,
        // and left in the slot it would fire at this load's first settled
        // PLAYING and flush the fresh text branch's first cues away.
        inner.selection.lock().reset();
        *inner.last_applied_subtitle.lock() = None;
        *inner.upstream_selection.lock() = None;
        inner.last_upstream_ids.lock().clear();
        *inner.deferred_text_work.lock() = None;
        *inner.intended_timeline.lock() = (1.0, gst::ClockTime::ZERO);
        inner.video_deselected.store(false, Ordering::SeqCst);
        inner.video_unrouted_once.store(false, Ordering::SeqCst);

        let element = match input {
            MediaInput::Uri(uri) => Inner::make_urisourcebin(&uri, true)?,
            MediaInput::Element(element) => element,
        };
        Inner::add_input(inner, element, generation, None)?;

        // Drive to PAUSED here to (a) detect a live source and (b) apply the
        // start position/rate seek while still PAUSED. The caller then just
        // plays and the first audio out is already at the target rate, so
        // there is no 1.0x-to-Nx seam.
        let change = inner
            .pipeline
            .set_state(gst::State::Paused)
            .context("pipeline to PAUSED for load")?;
        if change == gst::StateChangeSuccess::NoPreroll {
            return Ok(StartOutcome {
                live: true,
                generation,
            });
        }

        // A plain load (start-of-stream at 1.0x) needs no seek, so only a
        // real position/rate start pays the preroll wait.
        if let StartPoint::Seek { position, rate } = start
            && (rate != 1.0 || position != gst::ClockTime::ZERO)
        {
            // Recorded BEFORE the seek: an external subtitle attached during
            // this load can join (and replay) at any point of the start
            // dance, and the replay must aim at the timeline this item is
            // heading for even when the overlay has no segment yet.
            *inner.intended_timeline.lock() = (rate, position);
            Self::apply_start_seek(inner, position, rate);
        }
        Ok(StartOutcome {
            live: false,
            generation,
        })
    }

    /// Apply the start position/rate as a single flushing seek in PAUSED.
    /// Waits for preroll before seeking and for the flush's re-preroll after,
    /// both bounded so a stalled source degrades to "played at 1.0x" instead
    /// of a wedged worker. A non-seekable source is left as-is.
    ///
    /// The start seek is delivered at the INPUT, not through the pipeline,
    /// because at this point in a load the output graph is still being built.
    /// A `pipeline.seek()` is an upstream event, so `GstBin` hands it to the
    /// sink children in the pipeline AT THAT INSTANT and to nothing else. The
    /// load's chains join one at a time (a chain per routed stream) and the
    /// preroll token retires on the FIRST of them, so the pipeline reports a
    /// finished preroll while later streams are still being exposed; with
    /// `urisourcebin parse-streams=true` every elementary stream is its own
    /// decodebin3 input, so a seek that travels up one branch never reaches
    /// the others. A video chain joining after such a seek renders the item
    /// from the PRE-SEEK segment, and since `Inner::overlay_timeline` reads
    /// that very pad, everything aligned off it (external subtitles above
    /// all) is shifted by the start offset for the item's whole length.
    /// Measured under a parallel-suite soak, that lost 4/20 rounds; the
    /// failing ones logged `routed=[Audio] video_chain_in_pipeline=false`.
    ///
    /// `Inner::seek_main_input` has no such dependency on the output side, so
    /// that is the primary delivery and the pipeline broadcast is only the
    /// fallback for an input with no source pads to seek.
    fn apply_start_seek(inner: &Arc<Inner>, position: gst::ClockTime, rate: f64) {
        let (res, _, _) = inner.pipeline.state(PREROLL_TIMEOUT);
        if res.is_err() {
            return;
        }
        let mut q = gst::query::Seeking::new(gst::Format::Time);
        if !inner.pipeline.query(q.query_mut()) || !q.result().0 {
            return;
        }
        // One lock at a time: nothing else pairs these two, so do not be the
        // first to establish an order between them.
        let routed: Vec<StreamKind> = inner.routing.lock().routed.iter().map(|r| r.kind).collect();
        let audio_sink = inner.audio_sink.lock().is_some();
        debug!(
            ?routed,
            video_chain_in_pipeline = inner.overlay.parent().is_some(),
            audio_sink,
            "start seek: chains present at seek time"
        );
        let event = rate_seek_event(rate, position, None);
        let sought = inner.seek_main_input(&event)
            || send_rate_seek(&inner.pipeline, rate, position).is_ok();
        if sought {
            // An external subtitle that joined during the load sits on the
            // pre-seek timeline: the seek travels the MAIN input only, so
            // align the side inputs exactly like a user seek does.
            inner.forward_seek_to_live_externals(rate, position);
            let _ = inner.pipeline.state(PREROLL_TIMEOUT);
        }
        debug!(
            origin = ?inner.overlay_timeline().1,
            sought,
            "start seek: the overlay timeline after the seek"
        );
    }

    /// Reserve an [`ExternalSubId`] without touching the pipeline. Lets a
    /// caller do its bookkeeping on one thread and run the actual attach
    /// ([`attach_subtitle_with_id`]) on another: attaching drives the input
    /// element to the pipeline's state, and a source's `start()` may block
    /// on I/O, which must not run on an async event loop.
    pub fn allocate_subtitle_id(&self) -> ExternalSubId {
        let mut routing = self.inner.routing.lock();
        let id = ExternalSubId(routing.next_external_id);
        routing.next_external_id += 1;
        id
    }

    /// Live-attach an external subtitle by URI (file/http) under a
    /// pre-reserved id. Works in any pipeline state. The stream becomes
    /// selectable once decodebin3 announces the updated collection. The
    /// crate babysits the input from here: an input whose task dies in the
    /// deselect race recovers in place through the join-time replay, and
    /// one that fails for good (or never produces a stream within the
    /// bounded wait) is detached and reported as
    /// [`PlaybinEvent::ExternalSubtitleFailed`].
    ///
    /// A URI that is ALREADY attached is refused. With nothing upstream to
    /// inherit an id from, GStreamer derives a source pad's stream id by
    /// hashing the element's URI, so a second input on the same URI would
    /// report the first input's stream id, and every stream-id lookup in
    /// the crate and its callers resolves first match. Twins under one id
    /// answered about the wrong input everywhere at once (the hold release,
    /// the join and replay machinery, the selection engine's external map),
    /// and the observed end state was a subtitle path dead for the rest of
    /// the item. Detaching the existing input first makes the URI
    /// attachable again.
    pub fn attach_subtitle_with_id(&self, id: ExternalSubId, uri: &str) -> Result<()> {
        {
            let routing = self.inner.routing.lock();
            let twin = routing.inputs.iter().find_map(|input| {
                input
                    .external
                    .as_ref()
                    .filter(|external| external.uri == uri)
                    .map(|external| external.id)
            });
            if let Some(twin) = twin {
                return Err(anyhow!(
                    "subtitle URI is already attached as {twin:?}, and two inputs \
                     on one URI would share a URI-derived stream id"
                ));
            }
        }
        let generation = self.inner.current_generation();
        // NO buffering on subtitle side-inputs (uridecodebin3 also buffers
        // only the main item): a fresh input's own queue2 levels would drive
        // the caller's buffering state machine and wedge a paused pipeline
        // in "Buffering".
        let element = Inner::make_urisourcebin(uri, false)?;
        let external = ExternalInput {
            id,
            uri: uri.to_string(),
            epoch: 0,
            // Held from the very first attach: an unselected stream's first
            // push is fatal (see `ExternalInput::hold_until_selected`).
            hold_until_selected: true,
            hold_release_owed: false,
            task_dead: false,
            last_origin: gst::ClockTime::ZERO,
        };
        Inner::add_input(&self.inner, element, generation, Some(external))?;
        info!(?id, uri, "attached external subtitle input");
        self.arm_sub_watchdog(id, 0);
        Ok(())
    }

    /// Arm the bounded materialization check for a just (re-)attached
    /// external input: a sleeping thread queues [`Job::CheckSub`] after the
    /// timeout. The job no-ops if the input produced streams, was detached,
    /// or was re-armed (epoch mismatch) in the meantime.
    fn arm_sub_watchdog(&self, id: ExternalSubId, epoch: u32) {
        let work_tx = self.inner.work_tx.clone();
        let timeout = *self.inner.sub_timeout.lock();
        let spawned = std::thread::Builder::new()
            .name("fpb-sub-watchdog".into())
            .spawn(move || {
                std::thread::sleep(timeout);
                let _ = work_tx.send(Job::CheckSub { id, epoch });
            });
        if let Err(err) = spawned {
            warn!(?err, ?id, "failed to arm the subtitle watchdog");
        }
    }

    /// Shorten the external-subtitle materialization timeout. For tests
    /// only: production callers keep [`EXTERNAL_SUB_TIMEOUT`].
    #[doc(hidden)]
    pub fn set_external_sub_timeout(&self, timeout: Duration) {
        *self.inner.sub_timeout.lock() = timeout;
    }

    /// How often a text-branch disposal and the overlay-seat link actually ran
    /// at the same time (see [`TextSeat`]). For tests: a non-zero count is the
    /// evidence that the unsynchronized `is_linked()` check this lock replaced
    /// had a real window, not an argued one.
    #[doc(hidden)]
    pub fn text_seat_contentions(&self) -> u64 {
        self.inner.text_seat.contentions.load(Ordering::Relaxed)
    }

    /// Whether any external subtitle input is currently attached. Callers
    /// gate flushing operations on this: a flush races the external inputs'
    /// reconfiguration and can freeze the play item.
    pub fn has_external_subtitles(&self) -> bool {
        self.inner
            .routing
            .lock()
            .inputs
            .iter()
            .any(|i| i.external.is_some())
    }

    /// [`allocate_subtitle_id`] + [`attach_subtitle_with_id`] in one call,
    /// for callers without threading constraints.
    pub fn attach_subtitle(&self, uri: &str) -> Result<ExternalSubId> {
        let id = self.allocate_subtitle_id();
        self.attach_subtitle_with_id(id, uri)?;
        Ok(id)
    }

    /// Detach a live external subtitle input: stop it, unlink it from
    /// decodebin3 and release the request pads. Deliberately flush-based,
    /// with no draining of queued sparse data (uridecodebin3's drain is a
    /// known deactivation stall).
    pub fn detach_subtitle(&self, id: ExternalSubId) -> Result<()> {
        let inner = &self.inner;
        let mut routing = inner.routing.lock();
        let idx = routing
            .inputs
            .iter()
            .position(|i| i.is_external(id))
            .ok_or_else(|| anyhow!("no attached subtitle {id:?}"))?;
        let input = routing.inputs.remove(idx);
        drop(routing);
        Inner::remove_input_or_defer(inner, input);
        // A selection desire parked on this input must not park forever.
        inner.selection.lock().external_gone(id);
        info!(?id, "detached external subtitle input");
        Ok(())
    }

    /// The GStreamer stream ids produced by an attached external subtitle
    /// input. Empty until the input's source pads have appeared and carry
    /// their stream-start events, which is guaranteed by the time decodebin3
    /// posts the collection containing the streams, so collection handlers
    /// can rely on it to map an external input to its stream(s).
    pub fn subtitle_stream_ids(&self, id: ExternalSubId) -> Vec<String> {
        let routing = self.inner.routing.lock();
        let Some(input) = routing.inputs.iter().find(|i| i.is_external(id)) else {
            return Vec::new();
        };
        input.text_stream_ids()
    }

    /// State a slot's desired track (latest wins): a stream id from the
    /// advertised collection, `None` for "slot off", or (subtitle only) an
    /// attached external input's stream once it materializes. The engine
    /// dispatches, confirms and re-asserts on its own. Call
    /// [`pump_selection`](Self::pump_selection) afterwards (and at every
    /// transport settle point) to let it act.
    pub fn request_track(&self, slot: TrackSlot, target: TrackTarget) {
        self.inner.selection.lock().request(slot, target);
    }

    /// A user-initiated flushing seek re-emits the current subtitle cue by
    /// itself, making a scheduled switch-refresh flush redundant.
    pub fn cancel_selection_refresh(&self) {
        self.inner.selection.lock().cancel_refresh();
    }

    /// Let the selection engine dispatch whatever the transport now allows:
    /// a composed `SELECT_STREAMS` (stamped with a fresh seqnum, confirmed
    /// by the matching `StreamsSelected` event) or the switch's re-emit
    /// flush. The caller owns the transport state machine and calls this at
    /// its settle points. The gate deliberately comes from there, not from
    /// a pipeline query: only the transport machine knows about queued
    /// seeks and the mid-cascade quiet instants that wedge a dispatched
    /// reconfigure (see the [`selection`] module docs).
    pub fn pump_selection(&self, gate: SelectionGate) {
        loop {
            enum Dispatch {
                Select(TrackSelection, gst::Seqnum),
                Refresh(gst::Seqnum),
                ConfirmApplied(TrackSelection, gst::Seqnum),
            }

            let (externals_attached, externals) = {
                let routing = self.inner.routing.lock();
                let externals: Vec<(ExternalSubId, Vec<String>)> = routing
                    .inputs
                    .iter()
                    .filter_map(|i| i.external.as_ref().map(|e| (e.id, i.stream_ids())))
                    .collect();
                (!externals.is_empty(), externals)
            };
            // Read BEFORE the engine lock: the query takes the routing lock,
            // and the order is routing then selection (see `Inner::routing`).
            // The lever makes the engine behave as if nothing upstream owned
            // selection, so no request is ever answered locally.
            // NOT during a gapless activation. `applied` there already names
            // the INCOMING item's streams, and a synthetic report naming them
            // reaches `Inner::try_activate_prepared`, which IS the activation
            // trigger: measured, confirming through that window took
            // `gapless_switch_into_a_text_bearing_item` from its documented
            // ~1-in-3 flake to 3 failures in 4 ("playback did not advance ... a
            // parked streaming thread"). An activation posts a real collection
            // and selection of its own, which answers anything waiting behind
            // it, so nothing is owed here.
            // Lever: `FCAST_NO_CONFIRM_APPLIED`.
            let activating = self.inner.swap_gate.state.lock().activation_pending().is_some();
            let upstream_owns = self.inner.upstream_owns_selection()
                && !activating
                && std::env::var_os("FCAST_NO_CONFIRM_APPLIED").is_none();
            let dispatch = {
                let ctx = selection::PumpCtx {
                    gate,
                    externals_attached,
                    externals: externals.clone(),
                    upstream_owns,
                };
                let mut engine = self.inner.selection.lock();
                match engine.pump(&ctx) {
                    None => break,
                    Some(selection::Command::SelectStreams(target)) => {
                        let seqnum = gst::Seqnum::next();
                        engine.selection_dispatched(seqnum, target.clone());
                        Dispatch::Select(target, seqnum)
                    }
                    Some(selection::Command::RefreshSeek) => {
                        let seqnum = gst::Seqnum::next();
                        engine.refresh_dispatched(seqnum);
                        Dispatch::Refresh(seqnum)
                    }
                    // Recorded as a dispatch so the confirmation below settles
                    // it through the ONE seqnum-keyed path every other
                    // confirmation uses: nothing downstream can tell this from
                    // a no-op dispatch's local confirm, which is the point.
                    Some(selection::Command::ConfirmApplied(target)) => {
                        let seqnum = gst::Seqnum::next();
                        engine.selection_dispatched(seqnum, target.clone());
                        Dispatch::ConfirmApplied(target, seqnum)
                    }
                }
            };

            // Execute outside the engine lock: `select_streams` touches the
            // core, and the recorders (translate-time) take the engine lock
            // on streaming threads.
            match dispatch {
                Dispatch::Select(target, seqnum) => {
                    // The subtitle slot MOVES (off, on, or to another track):
                    // detach text from the overlay now, before the send.
                    //
                    // Two reasons, and they are the same reason. Waiting for
                    // decodebin3's pad removal queues behind the overlay's
                    // blocked next-cue push, so the on-screen cue would linger
                    // until its line ends. And the OUTGOING text slot cannot
                    // even release its multiqueue src pad while that push is in
                    // flight: `RoutedStream::tqueue` is a plain `queue`, so its
                    // default `max-size-time` of 1s counts the DEAD AIR between
                    // sparse cues (`gst_queue_apply_gap` advances the time level
                    // off GAP events), and it reports itself full holding zero
                    // buffers and zero bytes. decodebin3's switch then waits out
                    // the outgoing track's cue cadence: measured 1.6s at a 2s cue
                    // period and 4.6s at 4s, with the whole latency sitting in
                    // that one blocked push. Parking flushes the branch, wakes
                    // the push and moves the pad to an unsynced fakesink, which
                    // is why turning subtitles OFF was always instant; a REPLACE
                    // has the identical cause and now takes the identical path.
                    //
                    // A REPLACE gets the flush without the re-parenting: it is
                    // the flush that wakes the push (and drops the outgoing
                    // backlog, so the new track's first cue is not queued behind
                    // it), and moving the pad to a parking sink on top of that
                    // regressed the gapless text-to-text switch (3 of 6 runs of
                    // `gapless_switch_between_text_bearing_items` failed under
                    // CPU load with the park, 0 of 6 without). Skipped for a
                    // same-track re-assertion, which must not blink the cue that
                    // is on screen, and for a slot not confirmed on anything (a
                    // fresh load, and every gapless activation, which re-seeds
                    // it): there is no outgoing push to wake there.
                    // `last_applied_subtitle` is the CONFIRMED slot; the
                    // engine's own `applied` is optimistic and already names the
                    // new target by now.
                    //
                    // Both halves flush the text branch, and a flush blocks
                    // until that branch's streaming thread can be paused.
                    // Whether it can be paused NOW is not reliably decidable
                    // from pipeline state (a pipeline resting in PAUSED never
                    // releases it, one held below PLAYING by buffering does
                    // not either, and three state-based guards over-matched
                    // in a row), so the flush NEVER runs on this thread. See
                    // `Inner::submit_text_flush`.
                    //
                    // A replace normally rides decodebin3's pad swap to free
                    // the overlay seat. In upstream-selection mode no
                    // SELECT_STREAMS ever reaches decodebin3, no swap comes,
                    // and the outgoing branch holds `subtitle_sink` forever
                    // ("skipping extra text stream",
                    // `ext-subtitle-regression-2.txt`), so the park does the
                    // handover. The flush-not-park measurement that shaped the
                    // normal path was db3-owned gapless, which this mode never
                    // is. Lever: `FCAST_NO_UPSTREAM_SELECTION_SPLIT` (via the
                    // mode detection).
                    let replacing = {
                        let applied = self.inner.last_applied_subtitle.lock();
                        applied.is_some() && *applied != target.subtitle
                    };
                    // The mode is a TRI-STATE and the third state is not
                    // "false": a dispatch before the main input's decodebin3
                    // sink pads link has nobody to send the SELECTABLE query
                    // to. FREEZE-DIAGN.md section 3 shows that state cannot
                    // co-occur with a replace (both `last_applied_subtitle`
                    // setters need the very link that populates the pads), so
                    // this arm is defense in depth: park rather than flush a
                    // branch whose feeder might be an adaptive demuxer, whose
                    // ONE output loop pauses for good on a FLUSHING return.
                    // Lever: `FCAST_EAGER_FLUSH_ON_UNKNOWN_MODE`.
                    let mode = self.inner.upstream_selection_mode();
                    let work = decisions::eager_text_work(
                        target.subtitle.is_none(),
                        replacing,
                        mode,
                        std::env::var_os("FCAST_EAGER_FLUSH_ON_UNKNOWN_MODE").is_some(),
                    );
                    if replacing && mode.is_none() {
                        debug!(
                            ?work,
                            "the upstream-selection mode is not decidable yet for a text replace"
                        );
                    }
                    match work {
                        // Only the FLUSH leaves the caller, never the PARK.
                        //
                        // The park moves a deselected text stream onto its
                        // parking sink, and leaving it linked into the overlay
                        // for longer is exactly what stops decodebin3
                        // reconfiguring (see `park_text_streams`). Measured
                        // when the park was postponed as well,
                        // `regression_gapless` went to 15 failures in 22 runs
                        // against 11 in 22 for the unchanged code, with
                        // `subtitle_disable_survives_a_gapless_transition`
                        // twice as likely to fail. The park's own blocking
                        // half (the branch disposal) already routes through
                        // the deferred-work drain, so what runs inline here is
                        // the non-blocking surgery.
                        Some(DeferredTextWork::Park) => {
                            Inner::run_text_work(&self.inner, DeferredTextWork::Park)
                        }
                        Some(DeferredTextWork::Flush) => Inner::submit_text_flush(&self.inner),
                        None => {}
                    }
                    let ids: Vec<&str> = [&target.video, &target.audio, &target.subtitle]
                        .into_iter()
                        .filter_map(|sid| sid.as_deref())
                        .collect();
                    // An adaptive main input owns selection (SELECTABLE), and
                    // decodebin3 defers to it wholesale. That demuxer rejects
                    // any event naming a stream it does not carry
                    // ("Unrecognized stream_id", the WHOLE event refused),
                    // never posts about streams it cannot know, and decodebin3
                    // posts nothing at all in this mode. So external-input
                    // streams stay out of the event (the crate routes them
                    // itself), only a CHANGED upstream-owned part is sent (a
                    // no-op has no activation edge to confirm it), and an
                    // unchanged one is confirmed locally.
                    // Lever: `FCAST_NO_UPSTREAM_SELECTION_SPLIT`.
                    if self.inner.upstream_owns_selection() {
                        let external_sids: Vec<&str> = externals
                            .iter()
                            .flat_map(|(_, sids)| sids.iter().map(String::as_str))
                            .collect();
                        let upstream_ids: Vec<&str> = ids
                            .iter()
                            .copied()
                            .filter(|sid| !external_sids.contains(sid))
                            .collect();
                        let changed = {
                            let mut sorted: Vec<String> =
                                upstream_ids.iter().map(|s| s.to_string()).collect();
                            sorted.sort();
                            let mut last = self.inner.last_upstream_ids.lock();
                            if *last == sorted {
                                false
                            } else {
                                *last = sorted;
                                true
                            }
                        };
                        if changed && !upstream_ids.is_empty() {
                            let main_input = {
                                let routing = self.inner.routing.lock();
                                routing
                                    .inputs
                                    .iter()
                                    .find(|input| input.external.is_none())
                                    .map(|input| input.element.clone())
                            };
                            if let Err(err) =
                                self.select_streams_to(main_input, &upstream_ids, Some(seqnum))
                            {
                                warn!(?err, "selection dispatch refused");
                                self.inner.selection.lock().dispatch_failed(seqnum);
                                break;
                            }
                            // Confirmation arrives from the demuxer with this
                            // seqnum; the translate arm keeps the crate-owned
                            // subtitle slot (see MessageView::StreamsSelected).
                            // Unless it does not, which is what the field
                            // showed: bounded fallback below.
                            Inner::arm_upstream_confirm_fallback(
                                &self.inner,
                                target.clone(),
                                seqnum,
                            );
                        } else {
                            self.inner.post_synthetic_streams_selected(&target, seqnum);
                        }
                    } else if let Err(err) = self.select_streams(&ids, Some(seqnum)) {
                        warn!(?err, "selection dispatch refused");
                        self.inner.selection.lock().dispatch_failed(seqnum);
                        break;
                    }
                }
                Dispatch::Refresh(seqnum) => self.queue_job(Job::RefreshSeek { seqnum }),
                // No event goes anywhere: the request was already satisfied, so
                // the pipeline needs nothing and only the CALLER is owed an
                // answer.
                Dispatch::ConfirmApplied(target, seqnum) => {
                    self.inner.post_synthetic_streams_selected(&target, seqnum)
                }
            }
        }
    }

    /// Queue a stream selection (ids from the current stream collection).
    /// `seqnum` is stamped on the event so the confirming `StreamsSelected`
    /// message can be attributed to this request (`None` for a fresh one).
    /// Sent to decodebin3 directly, no detour through the sinks.
    ///
    /// The send happens on a dedicated thread, NOT inline: decodebin3
    /// handles `SELECT_STREAMS` on the sending thread, and its stream-switch
    /// machinery takes slot pad object locks that a live-spinning slot
    /// streaming thread can starve for seconds to forever (the sticky-event
    /// re-push livelock, which zombified the app's event loop mid switch).
    /// The single queue keeps back-to-back selections ordered. `Ok` means
    /// queued, not applied: confirmation arrives as the `StreamsSelected`
    /// bus message, and a selection superseded by a core swap before it
    /// sends is silently dropped (it could never confirm anyway).
    pub fn select_streams(&self, stream_ids: &[&str], seqnum: Option<gst::Seqnum>) -> Result<()> {
        self.select_streams_to(None, stream_ids, seqnum)
    }

    /// [`Self::select_streams`] with an explicit send target. `None` sends
    /// to decodebin3; the upstream-selection split passes the main input.
    fn select_streams_to(
        &self,
        target: Option<gst::Element>,
        stream_ids: &[&str],
        seqnum: Option<gst::Seqnum>,
    ) -> Result<()> {
        if stream_ids.is_empty() {
            return Err(anyhow!("refusing an empty stream selection"));
        }
        let mut builder = gst::event::SelectStreams::builder(stream_ids.iter().copied());
        if let Some(seqnum) = seqnum {
            builder = builder.seqnum(seqnum);
        }
        let Some(db3) = self.inner.core.lock().as_ref().map(|c| c.db3.clone()) else {
            return Err(anyhow!("no dynamic core"));
        };
        self.inner
            .select_tx
            .send(SelectJob {
                target: target.unwrap_or_else(|| db3.clone()),
                db3,
                event: builder.build(),
                stream_ids: stream_ids.iter().map(|s| s.to_string()).collect(),
            })
            .map_err(|_| anyhow!("the select sender thread is gone"))
    }

    // Blocking state entry points (see the struct-level Threading docs:
    // MT-safe, but not from streaming threads or the event callback).

    pub fn play(&self) -> Result<()> {
        self.inner
            .pipeline
            .set_state(gst::State::Playing)
            .context("pipeline to PLAYING")?;
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.inner
            .pipeline
            .set_state(gst::State::Paused)
            .context("pipeline to PAUSED")?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.teardown(gst::State::Null)
    }

    /// Full teardown to `target` (READY or NULL): drop the pipeline, remove
    /// every input (releasing its network/file resources NOW rather than at
    /// the next load) and drop the per-load audio sink. The video chain, if
    /// present, follows the pipeline down and is removed by the pad-removed
    /// unroutes or the next load's reset.
    ///
    /// # OPEN, AND IT BLOCKS THE CALLER TOO. The highest-value thing left here.
    ///
    /// `fuzz_buffering` seeds 1600039 and 1800015 wedge the WORKER (through
    /// [`Job::Stop`]), deterministic 3 of 3. `fuzz_scenarios` seed 2700007
    /// (ITERS=4 ACTIONS=22) wedges the CALLER, 3 of 3, on `stop() never
    /// returned`: `stop` is this function called synchronously, so it strands
    /// the application thread rather than the worker. All three are ONE call.
    /// The proof is the A/B on attempt 1 below, which cleared 2700007 3 of 3
    /// (it falls through to the pre-existing cue-misalignment class) and both
    /// buffering seeds 3 of 3.
    ///
    /// The stacks, captured with gdb. The blocked thread parks in the wake
    /// below, on a pad stream lock held by a task parked (through a queue) in
    /// `gst_base_sink_wait_preroll`:
    ///
    /// * seed 1600039: `drain_disposals_for_teardown` -> `dispose_text_branch`
    ///   -> `send_event(FLUSH_STOP)` on the overlay's `subtitle_sink`, which is
    ///   SERIALIZED, so `gst_pad_send_event_unchecked` waits for that pad's
    ///   stream lock;
    /// * seed 1800015: `flush_parked_text_pushes` -> `flush_pads` ->
    ///   `send_event(FLUSH_START)` into a queue sink pad ->
    ///   `gst_queue_handle_sink_event` -> `gst_pad_pause_task`.
    ///
    /// Below PLAYING that preroll ends only when the SINKS move, which is what
    /// the `set_state(target)` below does. The worker is the only thread that
    /// runs [`Job::SetState`], so this is a self-deadlock through the job
    /// queue, the same shape as the replay seek (see [`ReplayJob`]) and the
    /// same shape as `Inner::remove_input_or_defer` guards against.
    ///
    /// # Four fixes were implemented and MEASURED WRONG. Start past them.
    ///
    /// What makes this hard is a SECOND obstacle that releases on the opposite
    /// thing: a push parked on something a state change does not signal (a
    /// blocking pad probe, the geometry `tests/deferred_drain.rs` manufactures)
    /// releases only on a FLUSH_START, and that FLUSH_START has to arrive
    /// BEFORE the pipeline descends. The descent's `pad-removed` unroutes empty
    /// `routed`, after which `live_text_downstream_pads` is empty and the flush
    /// has nothing left to travel down. Any fix has to clear both.
    ///
    /// 1. *Move `set_state(target)` ahead of the wake.* Fixed 1600039 and
    ///    1800015 3 of 3 and broke
    ///    `deferred_drain::stopping_after_a_paused_subtitle_off_does_not_wedge`
    ///    (15 s bound, no flush ever sent) and
    ///    `regression_deadlock::the_teardown_flush_does_not_hold_the_routing_lock`
    ///    (40 s waiting for a flush that never came), for exactly that reason.
    /// 2. *Run the wake on a helper thread, joined after the state change.* The
    ///    gates pass and the race is only sometimes won: 1600039 went to 3
    ///    failures in 7, twice reporting `pipeline (Paused, Ready)`, a descent
    ///    that could not finish. `dispose_text_branch_on`'s FLUSH_STOP HOLDS
    ///    the overlay's `subtitle_sink` stream lock while it forwards, so the
    ///    helper can hold the very lock the descent needs while waiting for the
    ///    preroll the descent would release. Concurrency buys a new cycle.
    /// 3. *Take the crate's own sinks (video chain, audio sink) to READY first,
    ///    leaving the order otherwise untouched.* No effect: 1600039 and
    ///    1800015 back to 4 of 4, and 1600058 moved its wedge EARLIER, from
    ///    `Inner::drop` to a stop-and-reload. So the parked push is not held by
    ///    the two sinks this crate owns, or the READY itself does not get past
    ///    them. That is the next thing to establish, with a stack.
    /// 4. *Attempt 1 plus collecting the wake's TARGETS (the disposals and the
    ///    pads) before the descent*, on the theory that the post-descent wake
    ///    failed the two gates only because it had nothing left to send to.
    ///    Both gates still fail identically, 1 of 1 each, so an empty target
    ///    list is not the whole reason. `regression_deadlock`'s probe still
    ///    never fires, which points at the SEND being short-circuited on a pad
    ///    the descent has already deactivated rather than at the lookup.
    ///
    /// # FIXED, by moving the DESCENT rather than the wake
    ///
    /// The recorded shape for a fifth attempt was "FLUSH_START before the
    /// descent from a thread holding no stream lock, the matching FLUSH_STOP
    /// after". A gdb capture of seed 1800015 says that shape cannot be reached
    /// by splitting the flush, and says why:
    ///
    /// * the worker parks in `gst_pad_pause_task` on the text queue's src
    ///   stream lock, inside the FLUSH_**START**, not the stop,
    /// * `queue1:src` holds that lock in `gst_queue_loop`, pushing a STICKY
    ///   event through `gst_subtitle_overlay_subtitle_sink_event`
    ///   (gstsubtitleoverlay.c:2259) and blocked acquiring a further stream
    ///   lock down that chain,
    /// * `multiqueue7:src` and `fpb-aqueue:src` hold the locks it wants, both
    ///   parked in `gst_base_sink_wait_preroll`.
    ///
    /// So dropping the FLUSH_STOP buys nothing here: it is the FLUSH_START
    /// that parks, and a pad set flushing does not release a thread already
    /// blocked on a mutex behind it. The wake and the descent have to run
    /// CONCURRENTLY, and the only question is which of the two moves.
    ///
    /// Moving the wake is attempt 2, and it races: the wake's serialized
    /// FLUSH_STOP takes `subtitle_sink`'s stream lock and HOLDS it while it
    /// forwards, so the descent can end up behind the wake. Moving the
    /// DESCENT does not have that failure mode. When the wake is parked it
    /// holds nothing at all (it is *acquiring* a stream lock in both captured
    /// stacks), and `gst_bin_iterate_sorted` takes a bin down "from the most
    /// downstream elements (sinks) to the sources", so the descent releases
    /// `wait_preroll` before it needs anything the wake is queued behind.
    ///
    /// [`WakeRescue`] is that thread. It is ARMED, not unconditional: it does
    /// nothing at all unless the wake is still running after
    /// [`TEARDOWN_WAKE_BUDGET`], so a healthy teardown, and every gate written
    /// against the inline ordering, keeps byte-for-byte the old behaviour:
    /// same thread, same order, flush PAIRS intact and issued before the
    /// descent. Only a teardown that would otherwise have wedged forever sees
    /// a descent from another thread.
    ///
    /// That also answers the open question the fourth attempt left, which was
    /// whether a post-descent FLUSH_STOP still reaches the sink. It does not:
    /// `gst_pad_send_event_unchecked` discards a FLUSH_STOP on an INACTIVE pad
    /// outright (gstpad.c:6258, "we can't accept flush-stop on inactive pads"),
    /// and a descent to READY or NULL deactivates every pad. Splitting a pair
    /// around the descent would therefore strand the FLUSH_START at the sink,
    /// which is exactly the `flush_pairs_matched` violation that
    /// [`Inner::flush_parked_text_pushes`] records. Keeping the pair on one
    /// thread keeps it matched; the rescue only ever moves the descent
    /// underneath a pair that is already stuck.
    ///
    /// Lever: `FCAST_NO_TEARDOWN_RESCUE` restores the unrescued teardown.
    ///
    /// # RESIDUAL, measured, not closed
    ///
    /// A rescued teardown can still report `pipeline (Paused, Ready)`, the
    /// attempt-2 signature of a descent that could not finish. Measured on
    /// `fuzz_buffering` seed 1600039, ITERS=4 ACTIONS=22: 0 failures in 15
    /// runs with only this fix in the tree, 2 in 18 with the `poll_text_policy`
    /// stomped-subtitle guard in as well (which changes what is linked into
    /// the overlay at teardown, hence what the wake is parked on), against 3
    /// of 3 failing before either. The window is the one interval in which the
    /// wake HOLDS rather than waits for a stream lock: `dispose_text_branch_on`
    /// acquires `subtitle_sink`'s and keeps it while the FLUSH_STOP forwards,
    /// and a descent that reaches subtitleoverlay inside that interval queues
    /// behind it. It is bounded, not a wedge (the failing runs report on the
    /// driver's 15 s worker bound and then unwind normally, in the same total
    /// runtime as a passing one), so what is left is a rescue budget that eats
    /// 5 s of that 15 s rather than a new deadlock. Closing it needs the wake
    /// to hold no serialized event at all, which is the shape the FLUSH_START
    /// split could not reach for the reason recorded above.
    fn teardown(&self, target: gst::State) -> Result<()> {
        // A chain parked by a mid-item deselect is state-locked. Unlock it
        // so it follows the pipeline down.
        for element in &self.inner.video_chain {
            element.set_locked_state(false);
        }
        // Wake any prepared-input thread parked on the swap gate BEFORE
        // the state change, which joins streaming threads.
        self.inner.swap_gate.abort();
        *self.inner.prepared.lock() = None;
        // Both calls below can park forever on a push that only the descent
        // releases, and on the worker the descent is a job only this thread
        // runs. Arm the rescue across them (see the docs above).
        let rescue = WakeRescue::arm(&self.inner, target);
        // Postponed branch disposals first. The parked-push flush below
        // cannot see them, and it wedges on a multiqueue task blocked
        // mid-push into an undisposed queue (see
        // `Inner::drain_disposals_for_teardown`).
        self.inner.drain_disposals_for_teardown();
        // And wake every parked text push, or the downward change
        // deadlocks on the pad locks those pushes hold (see
        // `Inner::flush_parked_text_pushes`).
        self.inner.flush_parked_text_pushes();
        // Disarmed (and joined) BEFORE the gate below, so the two threads
        // never contend for it.
        rescue.disarm();
        {
            let _gate = Inner::gate(&self.inner);
            self.inner
                .pipeline
                .set_state(target)
                .with_context(|| format!("pipeline to {target:?} for teardown"))?;
        }
        Inner::remove_all_inputs(&self.inner);
        self.inner.remove_audio_sink();
        // Everything desired/applied/in-flight belonged to the torn-down
        // item, and so did a still-pending eager flush (running it against
        // the next item's fresh branch would drop that branch's first
        // cues). Branch disposals, input removals and replays stay pending.
        // Their targets outlive the item and their drains no-op when stale.
        self.inner.selection.lock().reset();
        *self.inner.last_applied_subtitle.lock() = None;
        *self.inner.upstream_selection.lock() = None;
        self.inner.last_upstream_ids.lock().clear();
        *self.inner.deferred_text_work.lock() = None;
        *self.inner.intended_timeline.lock() = (1.0, gst::ClockTime::ZERO);
        self.inner.video_deselected.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Change the pipeline state. Callers driving the pipeline through a
    /// caller-owned handle must use this instead of `set_state` on the
    /// pipeline element: DOWNWARD transitions take the route gate so no
    /// stream gets routed (and no chain activated) into the descending
    /// pipeline (see `Inner::route_gate`).
    pub fn set_pipeline_state(
        &self,
        state: gst::State,
    ) -> std::result::Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let _gate = (state < gst::State::Paused).then(|| Inner::gate(&self.inner));
        self.inner.pipeline.set_state(state)
    }

    // Worker-thread entry points: queued, ordered, safe from any thread
    // (including the event callback and async executors).

    /// Queue a pipeline state change on the worker thread. Downward
    /// transitions take the route gate there, exactly like
    /// [`set_pipeline_state`](Self::set_pipeline_state).
    pub fn set_state_async(&self, state: gst::State) {
        self.queue_job(Job::SetState { target: state });
    }

    /// Queue a [`load`](Self::load) on the worker thread. Completion is
    /// reported as [`PlaybinEvent::Loaded`]. A failed load only logs: any
    /// user-visible failure arrives through the pipeline error path.
    ///
    /// Returns the load's GENERATION. Every event is delivered together with
    /// the generation it belongs to, so the caller can drop events from
    /// superseded loads by comparing against this value: events posted by
    /// the previous item (even ones still queued when this load is
    /// requested) carry an older generation.
    pub fn load_async(&self, input: MediaInput, start: StartPoint) -> u64 {
        let generation = self.inner.allocate_generation();
        self.queue_job(Job::Load {
            input,
            start,
            generation,
        });
        generation
    }

    /// Pre-arm the next media input on the LIVE core for a gapless
    /// transition. The input element is created, added to the running
    /// pipeline, and its parsed streams link into decodebin3 alongside the
    /// current item's; when the current item drains, decodebin3 switches to
    /// the prepared streams with no state change, no flush, and no
    /// pipeline EOS in between. The switch surfaces as
    /// [`PlaybinEvent::PreparedActivated`] followed by the new item's
    /// collection and selection, all stamped with the returned generation.
    ///
    /// Constraints the caller upholds: the pipeline is in steady playback of
    /// a finite (non-live) item, and the prepared input is a plain A/V item
    /// (no start seek, images and live sources go through a normal load).
    /// A prepare while another is pending replaces it (latest wins). A
    /// normal `load`/`stop` drops any pending prepare. If the prepared
    /// input fails before activating, [`PlaybinEvent::PreparedFailed`] is
    /// emitted and playback of the current item is unaffected: its
    /// end-of-stream then arrives normally and the caller advances through
    /// its ordinary path.
    ///
    /// The same failure path also covers prepares the crate REFUSES or
    /// demotes on its own: external subtitles attached to the current item
    /// (a swap would carry them into the next item's collections), a
    /// prepare arriving while a performed swap is mid-activation, and a
    /// prepared item that lacks a stream type the current item is playing
    /// (the abandoned sink would block the next end-of-stream forever).
    ///
    /// Returns the generation the prepared item will carry once active.
    pub fn prepare_next_async(&self, input: MediaInput) -> u64 {
        let generation = self.inner.allocate_generation();
        self.queue_job(Job::PrepareNext { input, generation });
        generation
    }

    /// Drop a pending prepared next input (see
    /// [`prepare_next_async`](Self::prepare_next_async)): the caller seeked
    /// away from the end, the queue changed, or autoplay was turned off.
    /// A no-op when nothing is prepared or it already activated.
    ///
    /// The outcome comes back as exactly one event, because a cancel RACES the
    /// swap and commonly loses (the swap performs at pre-arm time for a small
    /// or cached item): [`PlaybinEvent::PreparedCancelled`] means the prepare
    /// is gone, [`PlaybinEvent::PreparedCancelDeclined`] means it is
    /// activating anyway. On the latter the caller MUST keep its pre-arm
    /// bookkeeping so the imminent [`PlaybinEvent::PreparedActivated`] is
    /// adopted rather than treated as unmatched.
    pub fn cancel_prepared_async(&self) {
        self.queue_job(Job::CancelPrepared { notify: true });
    }

    /// Queue a full stop on the worker thread: pipeline to READY, every
    /// input removed (its network/file resources released now, not at the
    /// next load) and the per-load audio sink dropped.
    pub fn stop_async(&self) {
        self.queue_job(Job::Stop {
            target: gst::State::Ready,
            done: None,
        });
    }

    /// Like [`stop_async`](Self::stop_async) but to NULL, invoking `done`
    /// once the teardown finished (a shutdown barrier).
    pub fn shutdown_async(&self, done: Box<dyn FnOnce() + Send>) {
        self.queue_job(Job::Stop {
            target: gst::State::Null,
            done: Some(done),
        });
    }

    /// Queue a [`graph::snapshot`] of the pipeline graph, delivered to `done`
    /// ON THE WORKER THREAD (hand it off, do not block). Queued so the
    /// element walk cannot race a concurrent load or teardown.
    pub fn debug_graph_async(&self, done: Box<dyn FnOnce(graph::GraphSnapshot) + Send>) {
        self.queue_job(Job::DumpGraph { done });
    }

    /// Queue a position/rate seek. If the pipeline is not settled in PAUSED
    /// the seek is handed back via [`PlaybinEvent::QueueSeek`] while the
    /// worker drives to PAUSED (the caller owns the seek queue and re-issues
    /// it once settled). Outcomes are [`PlaybinEvent::RateChanged`] and
    /// [`PlaybinEvent::SeekFailed`].
    pub fn seek_async(&self, seek: Seek) {
        self.queue_job(Job::Seek(seek));
    }

    /// Queue a flushing seek to the CURRENT position that keeps the pipeline
    /// in its current state, stamped with `seqnum` (failures come back as
    /// [`PlaybinEvent::RefreshSeekFailed`] with that seqnum). Used to force a
    /// freshly selected sparse subtitle track to re-emit its active cue. It
    /// deliberately bypasses any Paused round-trip a normal seek performs.
    pub fn refresh_seek_async(&self, seqnum: gst::Seqnum) {
        self.queue_job(Job::RefreshSeek { seqnum });
    }

    /// Queue a Paused->Playing cycle so the pipeline elects a new clock after
    /// [`PlaybinEvent::ClockLost`]. Without it every sink keeps waiting on
    /// the dead clock and playback stalls.
    pub fn recover_clock_async(&self) {
        self.queue_job(Job::RecoverClock);
    }

    /// Queue a live external-subtitle attach under a pre-reserved id
    /// ([`allocate_subtitle_id`](Self::allocate_subtitle_id)) on the worker
    /// thread: attaching drives the source to the pipeline's state, and a
    /// source's `start()` may block on I/O. An attach that fails never
    /// produces a stream and emits no event (a caller-side watchdog is the
    /// deterministic detector for that).
    pub fn attach_subtitle_async(&self, id: ExternalSubId, url: String) {
        self.queue_job(Job::AttachSub { id, url });
    }

    /// Queue a live external-subtitle detach. Best effort: the input is
    /// leaving regardless, and detaching an attach that already failed is
    /// harmless.
    pub fn detach_subtitle_async(&self, id: ExternalSubId) {
        self.queue_job(Job::DetachSub { id });
    }

    fn queue_job(&self, job: Job) {
        // Send can only fail if the worker died (it holds the receiver for
        // as long as it runs), and the pipeline is unusable then anyway.
        if self.inner.work_tx.send(job).is_err() {
            error!("fcastplaybin worker is gone; dropping the job");
        }
    }

    /// Set the volume (clamped to `0.0..=1.0`). Confirmation arrives as
    /// [`PlaybinEvent::VolumeChanged`]. GObject semantics apply: setting the
    /// current value again emits no notify (see
    /// [`renotify_volume`](Self::renotify_volume)).
    ///
    /// Volume lives on a dedicated `volume` element, NOT the audio sink: the
    /// sink is rebuilt per load, many resolved sinks expose no volume
    /// property at all, and sink-proxied volume notifies
    /// non-deterministically. playsink ships a dedicated volume element for
    /// the same reasons.
    pub fn set_volume(&self, volume: f64) {
        self.inner
            .volume
            .set_property("volume", volume.clamp(0.0, 1.0));
    }

    /// The current volume (`0.0..=1.0`).
    pub fn volume(&self) -> f64 {
        self.inner.volume.property("volume")
    }

    /// Re-emit [`PlaybinEvent::VolumeChanged`] at the current value, for
    /// callers whose protocol expects a confirmation even for an idempotent
    /// set.
    pub fn renotify_volume(&self) {
        self.inner.volume.notify("volume");
    }

    /// The current item's own stream time, asked of the OUTPUT sink.
    ///
    /// A correct gapless swap resets the segment, so the sink already reports
    /// the current item's own stream time (no cross-item rebase needed). This
    /// is the sink-anchored source of truth: the user-facing item switch
    /// (title/duration) is held until the new item's audio reaches the sink
    /// (see `Inner::held_activation`), so it lands in step with this position
    /// flipping to the new item's 0-based time, not a decoupling-queue ahead
    /// of it.
    ///
    /// Asking the PIPELINE instead would break that: `GstBin` folds POSITION
    /// with MAX over every SINK-flagged child, so an unsynced text parking
    /// sink racing toward the item's end can dominate the answer, and a `-1`
    /// DURATION from any folded sink poisons `duration()` outright.
    pub fn position(&self) -> Option<gst::ClockTime> {
        self.query_timeline(|element| element.query_position::<gst::ClockTime>())
    }

    /// The current item's duration, asked of the same sink `position()` is
    /// anchored on (so the two answers describe one item).
    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.query_timeline(|element| element.query_duration::<gst::ClockTime>())
    }

    /// Run a timeline query against the authoritative element: the per-load
    /// audio sink (the pipeline clock and the held-activation anchor), else
    /// the video sink, else the pipeline as a whole. One helper so
    /// `position()` and `duration()` cannot drift onto different items.
    ///
    /// A candidate outside the pipeline is skipped (`ensure_audio_sink` has
    /// not built one yet, or `remove_video_chain` took the chain out for an
    /// audio-only item), as is one that cannot answer.
    fn query_timeline(
        &self,
        query: impl Fn(&gst::Element) -> Option<gst::ClockTime>,
    ) -> Option<gst::ClockTime> {
        // Cloned out first: a query takes element and pad locks and can block
        // behind a streaming thread, and holding `audio_sink` across that
        // would stall every load's sink teardown.
        let audio_sink = self.inner.audio_sink.lock().clone();
        let video_sink = self.inner.video_chain.last().cloned();
        for candidate in [audio_sink, video_sink].into_iter().flatten() {
            if candidate.parent().is_none() {
                continue;
            }
            if let Some(value) = query(&candidate) {
                return Some(value);
            }
        }
        query(self.inner.pipeline.upcast_ref::<gst::Element>())
    }

    /// Whether the pipeline is settled: the last state change succeeded and
    /// no transition is pending (non-blocking query). NOT the complement of
    /// [`has_async_transition`](Self::has_async_transition), since a FAILED
    /// last change is neither settled nor async.
    pub fn is_settled(&self) -> bool {
        let (res, _, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
        res.is_ok() && pending == gst::State::VoidPending
    }

    /// Whether an async state change (re-preroll, a flushing seek's preroll)
    /// is in progress (non-blocking query). Asking the pipeline beats
    /// predicting from the kind of operation: mispredictions are what used
    /// to wedge callers' serialization logic.
    pub fn has_async_transition(&self) -> bool {
        let (res, _, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
        matches!(res, Ok(gst::StateChangeSuccess::Async)) || pending != gst::State::VoidPending
    }

    /// Ask the pipeline whether the current media is seekable. `None` while
    /// it cannot answer (the seeking query only succeeds around preroll
    /// completion, well after streams are first advertised).
    pub fn query_seekable(&self) -> Option<bool> {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        if self.inner.pipeline.query(query.query_mut()) {
            Some(query.result().0)
        } else {
            None
        }
    }

    /// Buffered regions of the current media as timeline fractions, from a
    /// `GST_QUERY_BUFFERING` in `PERCENT` format. Cheap and non-blocking, so
    /// callers can poll it to drive a buffered indicator on the scrubber.
    /// Empty when nothing in the pipeline can answer (a local file with no
    /// buffering element, a live/SABR source, or before preroll).
    pub fn buffered_ranges(&self) -> Vec<BufferedRange> {
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        if !self.inner.pipeline.query(query.query_mut()) {
            return Vec::new();
        }
        buffered_ranges_from(&query)
    }

    /// Full buffering state (fill percent, mode, rates, ranges) for the
    /// inspector, from a single `GST_QUERY_BUFFERING`. `None` when nothing in
    /// the pipeline can answer the query.
    pub fn buffering_info(&self) -> Option<BufferingInfo> {
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        if !self.inner.pipeline.query(query.query_mut()) {
            return None;
        }
        let (busy, percent) = query.percent();
        let (mode, _avg_in, _avg_out, buffering_left_ms) = query.stats();
        Some(BufferingInfo {
            percent,
            busy,
            mode,
            buffering_left: (buffering_left_ms > 0)
                .then(|| gst::ClockTime::from_mseconds(buffering_left_ms as u64)),
            ranges: buffered_ranges_from(&query),
        })
    }

    /// Best-effort "buffered ahead of the playhead" duration. In STREAM mode
    /// (the receiver's default) the buffering query exposes no ranges, but the
    /// queue elements still track how much media is queued: queue2,
    /// downloadbuffer and queue expose it element-wide, multiqueue per sink
    /// pad. Returns the deepest level found (the network-side buffer),
    /// `None` if nothing reports one. Poll it to size a buffered-ahead nub on
    /// the scrubber.
    pub fn buffered_ahead(&self) -> Option<gst::ClockTime> {
        let mut best: Option<u64> = None;
        let mut it = self.inner.pipeline.iterate_recurse();
        while let Ok(Some(elem)) = it.next() {
            let level_ns = match elem.factory().map(|f| f.name()).as_deref() {
                Some("queue2" | "downloadbuffer" | "queue") => elem
                    .find_property("current-level-time")
                    .map(|_| elem.property::<u64>("current-level-time")),
                Some("multiqueue") => elem
                    .sink_pads()
                    .iter()
                    .filter_map(|pad| {
                        pad.find_property("current-level-time")
                            .map(|_| pad.property::<u64>("current-level-time"))
                    })
                    .max(),
                _ => None,
            };
            if let Some(ns) = level_ns {
                best = Some(best.map_or(ns, |b| b.max(ns)));
            }
        }
        best.filter(|ns| *ns > 0).map(gst::ClockTime::from_nseconds)
    }

    pub fn seek(&self, position: gst::ClockTime) -> Result<()> {
        self.inner
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position)
            .context("seek")?;
        Ok(())
    }

    /// Re-drive the text link policy (link routed text into subtitleoverlay
    /// when a video stream is present). The crate re-checks on its own
    /// events, so this is a belt-and-suspenders hook for a caller's
    /// state-change handler and a no-op when nothing is pending.
    pub fn poll_text_policy(&self) {
        Inner::poll_text_policy(&self.inner);
    }

    /// How many [`Job::DrainTextWork`] jobs the worker has received so far.
    /// A diagnostic counter for the busy-loop regression test, which pins
    /// that a caller polling at a pipeline parked below PLAYING does not
    /// re-queue the drain on every poll. Not part of the public API.
    #[doc(hidden)]
    pub fn drain_text_job_count(&self) -> u64 {
        self.inner.drain_jobs_seen.load(Ordering::SeqCst)
    }

    pub fn dump_dot(&self, name: &str) {
        self.inner
            .pipeline
            .debug_to_dot_file_with_ts(gst::DebugGraphDetails::ALL, name);
    }

    /// Cumulative parsed-byte counters for every live input stream (all
    /// streams, selected or not: decodebin3 keeps consuming deselected
    /// inputs). Poll and diff to plot per-stream bitrate, and correlate
    /// with the stream collection via `stream_id` and with track selection
    /// via the caller's selected ids.
    pub fn stream_io_stats(&self) -> Vec<StreamIoStats> {
        let routing = self.inner.routing.lock();
        routing
            .inputs
            .iter()
            .flat_map(|input| {
                input.taps.iter().map(|tap| StreamIoStats {
                    stream_id: tap.pad.stream_id().map(|sid| sid.to_string()),
                    external: input.external.as_ref().map(|e| e.id),
                    bytes: tap.bytes.load(Ordering::Relaxed),
                    caps: tap.pad.current_caps(),
                })
            })
            .collect()
    }

    /// Inspector: every live input's element factory, its `uri` property
    /// when the element has one, and whether it is an external subtitle
    /// input.
    pub fn source_summaries(&self) -> Vec<SourceDbg> {
        self.inner
            .routing
            .lock()
            .inputs
            .iter()
            .map(|input| {
                let factory = input.element.factory().map(|f| f.name().to_string());
                SourceDbg {
                    external: input.external.as_ref().map(|e| e.id),
                    // A directly-constructed wrapper bin's factory is just
                    // "bin", its NAME carries the source kind
                    // (fcast-whep-source, fcast-fwebrtc-source, ...).
                    factory: match factory.as_deref() {
                        None | Some("bin") => input.element.name().to_string(),
                        Some(name) => name.to_string(),
                    },
                    uri: input
                        .element
                        .find_property("uri")
                        .and_then(|_| input.element.property::<Option<String>>("uri")),
                }
            })
            .collect()
    }

    /// Diagnostic: "kind:pad-name" for every currently-routed decodebin3
    /// stream. Compare against the media's stream collection to spot a
    /// selected stream whose pad never got routed.
    pub fn routed_summary(&self) -> Vec<String> {
        self.inner
            .routing
            .lock()
            .routed
            .iter()
            .map(|r| format!("{:?}:{}", r.kind, r.db3_src_pad.name()))
            .collect()
    }

    /// Diagnostic: the pipeline's current + pending state (a stalled load
    /// sits with an unfinished async transition, `pending != VoidPending`).
    pub fn state_summary(&self) -> (gst::State, gst::State) {
        let (_, current, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
        (current, pending)
    }

    /// Diagnostic: every pipeline element's `name (current -> pending)`, to
    /// spot which element is stuck below the pipeline's target at a stall.
    pub fn element_states(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut iter = self.inner.pipeline.iterate_recurse();
        while let Ok(Some(elem)) = iter.next() {
            let (ret, cur, pend) = elem.state(gst::ClockTime::ZERO);
            out.push(format!("{}({:?}->{:?} {:?})", elem.name(), cur, pend, ret));
        }
        out
    }

    /// Diagnostic: elements with an unfinished state transition (`pending !=
    /// VoidPending`). Normally empty, the interesting subset of
    /// [`element_states`](Self::element_states) at inspector poll rates.
    pub fn unsettled_elements(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut iter = self.inner.pipeline.iterate_recurse();
        while let Ok(Some(elem)) = iter.next() {
            let (_, cur, pend) = elem.state(gst::ClockTime::ZERO);
            if pend != gst::State::VoidPending {
                out.push(format!("{}({cur:?}->{pend:?})", elem.name()));
            }
        }
        out
    }

    /// The caller video sink's base-sink `stats` structure (rendered/dropped
    /// buffer counts), when a video sink is configured. `stats` is a
    /// `GstBaseSink` property, so it is absent on a bin sink (autovideosink);
    /// `None` then rather than panicking.
    pub fn video_sink_stats(&self) -> Option<gst::Structure> {
        let sink = self.inner.video_chain.last()?;
        sink.find_property("stats")
            .map(|_| sink.property::<gst::Structure>("stats"))
    }

    /// The per-load audio sink's negotiated caps and base-sink `stats`
    /// structure, while one exists. `stats` is a `GstBaseSink` property, so it
    /// is `None` on a bin sink (autoaudiosink) that lacks it.
    pub fn audio_sink_health(&self) -> Option<(Option<gst::Caps>, Option<gst::Structure>)> {
        let slot = self.inner.audio_sink.lock();
        let sink = slot.as_ref()?;
        let caps = sink.static_pad("sink").and_then(|pad| pad.current_caps());
        let stats = sink
            .find_property("stats")
            .map(|_| sink.property::<gst::Structure>("stats"));
        Some((caps, stats))
    }
}

/// How long [`FcastPlaybin::teardown`]'s pre-descent wake gets before
/// [`WakeRescue`] performs the pipeline's state change on its behalf.
///
/// Both bounds are load-bearing. It is ABOVE the three seconds
/// `tests/regression_deadlock.rs` deliberately pins the teardown flush for,
/// so a wake that is merely slow is never rescued and every gate written
/// against the inline ordering keeps exactly that ordering. It is well BELOW
/// the fifteen seconds the fuzz drivers give a stop or a graph dump, so a
/// rescued teardown still answers inside their bound.
const TEARDOWN_WAKE_BUDGET: Duration = Duration::from_secs(5);

/// Performs the teardown's pipeline descent from a second thread when the
/// wake ahead of it has parked. See [`FcastPlaybin::teardown`] for the
/// deadlock and for why the DESCENT is what moves.
///
/// Armed, never unconditional. The rescue thread waits on a hangup channel
/// for [`TEARDOWN_WAKE_BUDGET`]; [`WakeRescue::disarm`] hangs the channel up,
/// so a wake that completes normally cancels it before it can act and the
/// teardown is bit-for-bit the one that ran before this existed.
struct WakeRescue {
    /// Hanging this up is the cancellation. Dropped by `disarm`.
    cancel: Option<mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WakeRescue {
    fn arm(inner: &Arc<Inner>, target: gst::State) -> Self {
        if std::env::var_os("FCAST_NO_TEARDOWN_RESCUE").is_some() {
            return WakeRescue {
                cancel: None,
                handle: None,
            };
        }
        let (cancel, cancelled) = mpsc::channel();
        let inner = Arc::clone(inner);
        let handle = std::thread::Builder::new()
            .name("fpb-tdrescue".into())
            .spawn(move || {
                // Anything but a timeout means the wake finished (or the
                // teardown thread went away): nothing to rescue.
                if cancelled.recv_timeout(TEARDOWN_WAKE_BUDGET)
                    != Err(mpsc::RecvTimeoutError::Timeout)
                {
                    return;
                }
                warn!(
                    ?target,
                    "the teardown wake is still parked after {TEARDOWN_WAKE_BUDGET:?}, \
                     taking the pipeline down from the rescue thread"
                );
                let _gate = Inner::gate(&inner);
                let _ = inner.pipeline.set_state(target);
                debug!(?target, "the rescue descent finished");
            })
            .ok();
        WakeRescue {
            cancel: Some(cancel),
            handle,
        }
    }

    /// Cancel the rescue and JOIN it, so the teardown never runs on past a
    /// descent that is still in flight.
    fn disarm(mut self) {
        drop(self.cancel.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Everything the blocking half of the teardown needs, in OWNED form. Every
/// field is a refcounted GStreamer handle, so this keeps the graph alive by
/// itself once `Inner`'s memory is gone, which is what lets the descent run
/// somewhere other than the thread that dropped the last reference. See
/// [`Teardown::run`] and the comment above `impl Drop for Inner`.
struct Teardown {
    pipeline: gst::Pipeline,
    overlay: gst::Element,
    video_chain: Vec<gst::Element>,
    inputs: Vec<gst::Element>,
    disposals: Vec<TextDisposal>,
    /// Pads of live text branches, plus the decodebin3 sink pads of every
    /// input. Collected under the routing lock while `Inner` still existed
    /// (see [`Inner::flush_parked_text_pushes`], which this replaces at the
    /// teardown boundary).
    text_pads: Vec<gst::Pad>,
    db3_sink_pads: Vec<gst::Pad>,
    /// Carried so the disposals below take the same seat lock the live crate
    /// does (see [`TextSeat`]). Uncontended by this point, `Inner` is gone and
    /// nothing can link the seat any more.
    text_seat: Arc<TextSeat>,
}

impl Teardown {
    /// The former body of `Inner::drop` from the disposals onward. Every call
    /// here can block on a pad stream lock, which is precisely why it is
    /// separated from the reference count that triggered it.
    ///
    /// # The PIPELINE goes down before the inputs do
    ///
    /// The input NULL loop used to run first, and wedged the whole process on
    /// `fuzz_buffering` seed 1600058, deterministic 3 of 3 at the full 600 s
    /// timeout. Captured with gdb:
    ///
    /// * this thread in `element.set_state(Null)` ->
    ///   `gst_uri_source_bin_change_state` -> `gst_bin_src_pads_activate` ->
    ///   `activate_mode_internal`, waiting for a pad STREAM LOCK,
    /// * the input's own `ftestsrc:text` task holding it, blocked in
    ///   `gst_multi_queue_sink_event` -> `gst_data_queue_push` on decodebin3's
    ///   FULL multiqueue,
    /// * that multiqueue's src task parked in `gst_base_sink_wait_preroll`.
    ///
    /// This is the campaign-5 wedge (`tests/regression_teardown_flush.rs`,
    /// seeds 100014, 200030, 300046, 200057). Its window is the flush PAIR:
    /// the FLUSH_STOP re-arms the pad and an as-fast-as-possible source refills
    /// the slot and blocks on a serialized event again before the loop reaches
    /// its NULL. Sending FLUSH_START alone was implemented and MEASURED WRONG,
    /// so the pairing stays and the input NULLs move BEHIND the pipeline's own
    /// descent instead. That descent takes the sinks out of `wait_preroll`, the
    /// multiqueue drains, and the input's push returns before anything asks for
    /// its stream lock. The explicit NULLs stay, for an input the descent
    /// cannot reach (one already out of the pipeline).
    ///
    /// The wake still runs FIRST here, unlike in [`FcastPlaybin::teardown`].
    /// It is not what wedged, and a push parked on something the descent does
    /// not signal still needs it (see that function for the two-obstacle
    /// argument).
    ///
    /// `FCAST_TEARDOWN_INPUTS_BEFORE_PIPELINE` restores the old order.
    fn run(self) {
        let inputs_first =
            std::env::var_os("FCAST_TEARDOWN_INPUTS_BEFORE_PIPELINE").is_some();
        let Teardown {
            pipeline,
            overlay,
            video_chain,
            inputs,
            disposals,
            text_pads,
            db3_sink_pads,
            text_seat,
        } = self;

        // Postponed branch disposals first, the parked-push flush cannot
        // see them (see `Inner::drain_disposals_for_teardown`). Then every
        // parked text push, or the NULLs below deadlock on the pad locks
        // those pushes hold (see `Inner::flush_parked_text_pushes`).
        for disposal in disposals {
            debug!("disposing of a postponed text branch at teardown");
            Inner::dispose_text_branch_on(&overlay, &pipeline, disposal, &text_seat);
        }
        Inner::flush_pads(&text_pads);
        Inner::flush_db3_sink_pads(&db3_sink_pads);
        debug!("drop: parked pushes flushed");

        // A state-locked prepared input does not follow the pipeline down, and
        // its unref at PLAYING trips a CRITICAL. Unlocked BEFORE the descent so
        // the descent carries it.
        for element in &inputs {
            element.set_locked_state(false);
        }
        if !inputs_first {
            let _ = pipeline.set_state(gst::State::Null);
        }
        for element in &inputs {
            let _ = element.set_state(gst::State::Null);
        }
        debug!("drop: inputs down");
        if inputs_first {
            let _ = pipeline.set_state(gst::State::Null);
        }
        debug!("drop: pipeline down");
        // Between video items the caller sink parks at READY OUTSIDE the
        // pipeline (`remove_video_chain`), so the NULL above never reaches
        // it and the final unref would trip GStreamer's dispose-in-READY
        // CRITICAL. Down any orphaned chain element explicitly.
        for element in &video_chain {
            if element.parent().is_none() {
                let _ = element.set_state(gst::State::Null);
            }
        }
    }
}

// Teardown lives on `Inner`, NOT on the cloneable handle: a `Drop` on
// `FcastPlaybin` fires for EVERY dropped clone, including the worker's
// per-job temporaries. A handle-level Drop once NULLed the pipeline from a
// streaming thread mid-post and deadlocked a concurrent load's state change.
//
// # Which thread this runs on is NOT the caller's choice
//
// Every internal callback holds a `Weak` and upgrades for the duration of its
// work, so ANY of them is the last strong reference whenever the caller drops
// its final handle inside that window: the bus sync handler, decodebin3's
// pad-added, an EOS probe. The pipeline's descent to NULL then runs on
// whatever thread that was, and on a streaming thread it CANNOT work. Captured
// (`toml_scenarios`, measured here at 2 runs in 42 ending in signal 11):
//
//     gst_multi_queue_loop -> sticky push into decodebin3's sink
//       -> gst_decodebin_input_setup_identity -> gst_element_sync_state_with_parent
//       -> state-changed -> gst_bus_post -> the crate's bus sync handler
//       -> Arc<Inner>::drop_slow -> Inner::drop -> set_state(Null)
//
// The descent tries to deactivate the pad whose task IS the calling thread
// ("Trying to join task ... from its thread would deadlock", "Failed to
// deactivate pad multiqueueN:src_0, very bad"), gives up half-way with the
// pipeline at READY, and the dispose cascade over the still-live elements
// segfaults.
//
// So the blocking half is COLLECTED into an owned [`Teardown`] and, on a
// thread this process did not create, handed to one it did. Fixing the
// callbacks instead was tried FIRST and is not enough: retiring the bus sync
// handler's reference through a dedicated thread left the crash rate
// unchanged (1 run in 36 against 1 in 38), because the terminal reference
// simply lands on the next callback instead, and converting every
// `Weak::upgrade` in the crate still left the warnings at 1 run in 42. There
// is no finite set of call sites to fix; the guarantee belongs here, where the
// thread is known.
//
// # This is NOT the whole class, and the numbers say so
//
// `tests/regression_teardown_thread.rs` manufactures the race and goes from 3
// failures in 3 to 3 passes in 3, so the path it walks is closed. The
// `toml_scenarios` soak did NOT follow: 42 runs per arm, interleaved on one
// binary, gave 2 signal-11 runs and 2 warning-emitting runs with
// `FCAST_TEARDOWN_ON_ANY_THREAD=1` against 1 and 1 without it. At that base
// rate 2 against 1 is noise, so a second route to the same symptom is still
// open and none of this should be read as closing it.
//
// The likeliest remaining one, UNPROVEN and stated so nobody has to rediscover
// the shape of the search: `Inner`'s OWN fields drop on the calling thread
// after this function returns, concurrently with the handed-off descent. An
// element whose last reference happens to be one of them is then disposed
// from there, and GStreamer's dispose deactivates its pads exactly as the
// descent would have. `Teardown` holds the pipeline, the video chain and the
// inputs; it does not hold every element `Inner` owns.
//
// `FCAST_TEARDOWN_ON_ANY_THREAD` runs the descent in place again, whatever
// the thread, for an A/B without a rebuild. It gates the whole change.
impl Drop for Inner {
    fn drop(&mut self) {
        debug!("dropping the playbin core");
        // A chain parked by a mid-item deselect is state-locked. Unlock it
        // so it follows the pipeline down.
        for element in &self.video_chain {
            element.set_locked_state(false);
        }
        // Wake any prepared-input thread parked on the swap gate before the
        // state change joins streaming threads.
        self.swap_gate.abort();

        // Collected BEFORE anything blocks, and each under the routing lock
        // on its own: a downward state change joins streaming threads, and
        // those run pad probes that take that lock (the inversion
        // `Inner::live_text_downstream_pads` documents).
        let teardown = Teardown {
            pipeline: self.pipeline.clone(),
            overlay: self.overlay.clone(),
            video_chain: self.video_chain.clone(),
            disposals: std::mem::take(&mut *self.deferred_text_disposal.lock()),
            text_seat: Arc::clone(&self.text_seat),
            text_pads: self.live_text_downstream_pads(),
            db3_sink_pads: {
                let routing = self.routing.lock();
                routing
                    .inputs
                    .iter()
                    .flat_map(|input| input.db3_sink_pads.iter().cloned())
                    .collect()
            },
            inputs: self
                .routing
                .lock()
                .inputs
                .iter()
                .map(|input| input.element.clone())
                .collect(),
        };

        // A thread with no Rust name is one this process never spawned, i.e.
        // a GStreamer task thread. Every thread that may legitimately carry
        // the descent has one: the caller's `main` or its test thread, and
        // the crate's own `fcastplaybin`, `fpb-select`, `fpb-replay`. An
        // unnamed `std::thread::spawn` in a caller reads as foreign too,
        // which only costs it an asynchronous teardown.
        let foreign = std::thread::current().name().is_none()
            && std::env::var_os("FCAST_TEARDOWN_ON_ANY_THREAD").is_none();
        if !foreign {
            teardown.run();
            return;
        }
        debug!("handing the teardown off a thread the crate does not own");
        // NOT joined. Joining would wait for the very descent that is about
        // to join THIS thread's task, which is the deadlock in a new place.
        // The handles above keep the graph alive without `Inner`, so letting
        // it run on is safe.
        if let Err(err) = std::thread::Builder::new()
            .name("fpb-teardown".to_owned())
            .spawn(move || teardown.run())
        {
            error!(
                ?err,
                "could not hand off the teardown; the pipeline stays up"
            );
        }
    }
}

impl Inner {
    /// Deliver an event to the caller's handler, a no-op until
    /// [`FcastPlaybin::set_event_handler`] installs one. Stamped with the
    /// current load generation.
    fn emit(&self, event: PlaybinEvent) {
        let callback = self.events.lock().clone();
        if let Some(callback) = callback {
            callback(event, self.current_generation());
        }
    }

    /// The generation the NEXT load will run under (see
    /// [`FcastPlaybin::load_async`]).
    fn allocate_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The generation of the current load (adopted at its reset point).
    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The worker thread (see [`Job`]). Holds only a `Weak` between jobs so
    /// it never keeps the pipeline alive, and exits when every handle is
    /// gone (the channel closes). If a job's temporary upgrade turns out to
    /// be the LAST strong ref, `Inner::drop` (pipeline to NULL) simply runs
    /// here after the job. THIS thread is a safe one for it, because nothing
    /// GStreamer owns is waiting on it. That is a property of this thread and
    /// NOT of temporary upgrades in general: the same reasoning applied to the
    /// bus sync handler and was false, because that one runs on whichever
    /// STREAMING thread posted the message, and NULLing the pipeline there
    /// deadlocks the descent against the caller's own task. See
    /// [`Inner::retire`], which is how every callback that can run on a
    /// GStreamer-owned thread must release its reference.
    fn worker_loop(weak: Weak<Inner>, work_rx: mpsc::Receiver<Job>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        while let Ok(job) = work_rx.recv() {
            let Some(inner) = weak.upgrade() else { break };
            debug!(?job, "Got job");
            FcastPlaybin { inner }.run_job(job);
        }

        debug!("fcastplaybin worker finished");
    }

    /// The replay-seek sender thread (see [`ReplayJob`] for why the send is
    /// not on the worker). Same lifetime discipline as `worker_loop`: holds
    /// only a `Weak` between jobs, exits when the channel closes.
    fn replay_sender_loop(weak: Weak<Inner>, replay_rx: mpsc::Receiver<ReplayJob>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        while let Ok(job) = replay_rx.recv() {
            let Some(inner) = weak.upgrade() else { break };
            FcastPlaybin::run_replay_seek(&inner, job);
        }

        debug!("fcastplaybin replay sender finished");
    }

    /// The chain-join thread (see [`ChainJoinJob`]). Same lifetime discipline
    /// as `worker_loop`: holds only a `Weak` between jobs, exits when the
    /// channel closes.
    fn chain_join_loop(weak: Weak<Inner>, join_rx: mpsc::Receiver<ChainJoinJob>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        while let Ok(job) = join_rx.recv() {
            let Some(inner) = weak.upgrade() else { break };
            Inner::run_chain_join(&inner, job);
        }

        debug!("fcastplaybin chain joiner finished");
    }

    /// The SELECT_STREAMS sender thread (see [`SelectJob`] and
    /// [`FcastPlaybin::select_streams`] for why the send is not inline).
    /// Same lifetime discipline as `worker_loop`: holds only a `Weak`
    /// between jobs, exits when the channel closes.
    fn select_sender_loop(weak: Weak<Inner>, select_rx: mpsc::Receiver<SelectJob>) {
        let span = debug_span!("fcastplaybin");
        let _entered = span.enter();

        while let Ok(job) = select_rx.recv() {
            let Some(inner) = weak.upgrade() else { break };

            // A selection built against a superseded core can never confirm.
            // Don't run decodebin3's inline switch machinery on a dying
            // instance for nothing.
            let stale = inner.core.lock().as_ref().map(|c| &c.db3) != Some(&job.db3);
            if stale {
                debug!("dropping a stream selection for a superseded core");
                continue;
            }

            // Record the selection's video intent BEFORE the send.
            // decodebin3 can expose pads inline inside send_event, and the
            // route decision reading this mirror must see the intent THIS
            // selection carries, not the previous one. Pure intent on
            // purpose, unlike the park decision after the send, which also
            // wants a linked chain. An empty collection_video_ids means
            // kinds are unknowable and never counts as off, matching
            // `decisions::deselects_video`.
            {
                let routing = inner.routing.lock();
                let video_off = !routing.collection_video_ids.is_empty()
                    && !routing
                        .collection_video_ids
                        .iter()
                        .any(|vid| job.stream_ids.contains(vid));
                inner.video_deselected.store(video_off, Ordering::SeqCst);
            }

            // send_event runs decodebin3's selection handling inline on THIS
            // thread. It may stall behind streaming threads, which is the
            // point of this thread (see `select_streams`).
            let seqnum = job.event.seqnum();
            if !job.target.send_event(job.event) {
                warn!(target = %job.target.name(), "SELECT_STREAMS event refused");
                // A refused dispatch never confirms; leaving it in flight
                // starves every later change (`pump` refuses to dispatch over
                // an unconfirmed selection while playing). Field shape:
                // `subtitle-regressions.txt`.
                // Lever: `FCAST_NO_SELECT_REFUSAL_FEEDBACK`.
                if std::env::var_os("FCAST_NO_SELECT_REFUSAL_FEEDBACK").is_none() {
                    inner.selection.lock().dispatch_failed(seqnum);
                }
                continue;
            }
            debug!(?seqnum, ids = ?job.stream_ids, "sent SELECT_STREAMS");

            // A selection that drops video ENTIRELY must not leave the video
            // branch able to block on the pipeline clock (see
            // `park_video_chain_for_deselect`). Not on a video-to-video
            // switch: decodebin3 reuses the routed pad for those (no
            // pad-removed/added), so a parked chain would never re-join.
            // Hence the check against the collection's video ids, not just
            // the routed pad's. Running after `send_event` lets decodebin3's
            // armed slot deactivation complete rather than racing it.
            let deselects_video = {
                let routing = inner.routing.lock();
                let video_linked = routing
                    .routed
                    .iter()
                    .any(|r| r.kind == StreamKind::Video && r.downstream.is_some());
                decisions::deselects_video(
                    video_linked,
                    &routing.collection_video_ids,
                    &job.stream_ids,
                )
            };
            if deselects_video {
                inner.park_video_chain_for_deselect();
            }
        }

        debug!("fcastplaybin select sender finished");
    }

    /// Whether the MAIN input's upstream answers the SELECTABLE query, i.e.
    /// an adaptive demuxer owns stream selection. Mirrors decodebin3, which
    /// flips into upstream-selection mode when ANY input answers TRUE (its
    /// own FIXME: "things might break if there's a mix" — an external
    /// subtitle IS the mix). Cached per load: the answer cannot change
    /// mid-item. Lever: `FCAST_NO_UPSTREAM_SELECTION_SPLIT` (set = pretend
    /// no upstream ever owns selection, the old behaviour).
    ///
    /// Reads the tri-state below as "no upstream owner" when the query cannot
    /// be answered yet, which is what every caller here needs. The one place
    /// that must tell ignorance from a definite no is `pump_selection`'s eager
    /// text work, which calls [`Inner::upstream_selection_mode`] instead.
    fn upstream_owns_selection(&self) -> bool {
        self.upstream_selection_mode().unwrap_or(false)
    }

    /// [`Inner::upstream_owns_selection`] as a TRI-STATE: `None` while the
    /// main input has no decodebin3 sink pad linked, so there is nobody to ask
    /// and the answer is genuinely unknown rather than false. A definite
    /// answer is cached (the mode cannot change mid-item), ignorance is not.
    fn upstream_selection_mode(&self) -> Option<bool> {
        if std::env::var_os("FCAST_NO_UPSTREAM_SELECTION_SPLIT").is_some() {
            // A definite answer and not ignorance: the lever exists to make
            // the crate behave as if no upstream ever owned selection.
            return Some(false);
        }
        if let Some(known) = *self.upstream_selection.lock() {
            return Some(known);
        }
        let pads: Vec<gst::Pad> = {
            let routing = self.routing.lock();
            routing
                .inputs
                .iter()
                .filter(|input| input.external.is_none())
                .flat_map(|input| input.db3_sink_pads.iter().cloned())
                .collect()
        };
        if pads.is_empty() {
            // Not linked yet: don't cache ignorance.
            return None;
        }
        // An UNHANDLED query is a definite no, not ignorance: SELECTABLE is
        // answered by adaptive demuxers and forwarded upstream by everything
        // else, so a plain source leaves it unhandled and `peer_query` returns
        // false. decodebin3 reads it exactly this way. Treating the refusal as
        // "not decidable yet" was measured WRONG: every non-adaptive replace
        // then took the PARK arm, which runs inline, and
        // `caller_nonblocking.rs` caught the caller blocking behind the text
        // branch. Only "no pad to ask at all" is ignorance.
        let owns = pads.iter().any(|pad| {
            let mut query = gst::query::Selectable::new();
            pad.peer_query(&mut query) && query.selectable()
        });
        debug!(owns, "queried whether upstream owns stream selection");
        *self.upstream_selection.lock() = Some(owns);
        Some(owns)
    }

    /// Post the STREAMS_SELECTED that decodebin3 never posts in
    /// upstream-selection mode (`is_selection_done` returns early there), for
    /// a dispatch whose upstream-owned part did not change (an adaptive
    /// demuxer only confirms an activation edge). Everything downstream of
    /// the bus arm (engine settle, external hold release, replay) runs off
    /// the post, unchanged.
    /// A SENT upstream selection whose confirmation never arrives is confirmed
    /// locally after a bounded wait.
    ///
    /// The split sends only a CHANGED upstream part precisely because an
    /// adaptive demuxer confirms activation EDGES and a no-op send has none. A
    /// changed part is supposed to produce one, and the field showed two ways it
    /// does not:
    ///
    /// * The change can be real to the crate and a no-op to the demuxer. The
    ///   record it compares against (`last_upstream_ids`) is seeded from
    ///   observed reports (see the `StreamsSelected` arm) AND from what was
    ///   sent, so an item that never got an initial report compares against an
    ///   empty set and reads its first dispatch as changed whatever it names.
    /// * The edge can be real and unable to complete. At a settled PAUSED
    ///   nothing flows, and a track deactivation that needs its pad to go idle
    ///   waits for a push parked in a sink's preroll (the same mechanism as
    ///   `Inner::lift_deselected_video_sink`, one element further upstream where
    ///   this crate owns nothing to lift).
    ///
    /// Either way the caller is left waiting on a seqnum nothing will answer:
    /// the receiver never relays, never sends SetTrackIds, and the UI keeps
    /// showing the previous track while the new one is audibly/visibly active.
    /// So: after [`UPSTREAM_CONFIRM_FALLBACK`], if the engine still awaits this
    /// exact seqnum, post the confirmation the crate already knows how to build.
    /// Keyed on the seqnum, so a real confirmation that arrives first (or a
    /// selection that moved on) makes this a no-op.
    ///
    /// One-shot sleeper thread, the pattern `Inner::arm_sub_watchdog` and
    /// `Inner::arm_replay_verification` already use. Lever:
    /// `FCAST_NO_UPSTREAM_CONFIRM_FALLBACK`.
    fn arm_upstream_confirm_fallback(
        inner: &Arc<Inner>,
        target: selection::TrackSelection,
        seqnum: gst::Seqnum,
    ) {
        if std::env::var_os("FCAST_NO_UPSTREAM_CONFIRM_FALLBACK").is_some() {
            return;
        }
        let weak = Arc::downgrade(inner);
        let spawned = std::thread::Builder::new()
            .name("fpb-confirm-fallback".into())
            .spawn(move || {
                std::thread::sleep(UPSTREAM_CONFIRM_FALLBACK);
                let Some(inner) = weak.upgrade() else { return };
                if !inner.selection.lock().selection_in_flight(seqnum) {
                    return;
                }
                warn!(
                    ?seqnum,
                    "the upstream selection was never confirmed; confirming it locally"
                );
                inner.post_synthetic_streams_selected(&target, seqnum);
            });
        if let Err(err) = spawned {
            warn!(?err, "failed to arm the upstream confirmation fallback");
        }
    }

    fn post_synthetic_streams_selected(
        &self,
        target: &selection::TrackSelection,
        seqnum: gst::Seqnum,
    ) {
        let mut collection = gst::StreamCollection::builder(None);
        let mut selected = Vec::new();
        for (sid, kind) in [
            (&target.video, gst::StreamType::VIDEO),
            (&target.audio, gst::StreamType::AUDIO),
            (&target.subtitle, gst::StreamType::TEXT),
        ] {
            if let Some(sid) = sid {
                let stream = gst::Stream::new(Some(sid), None, kind, gst::StreamFlags::empty());
                selected.push(stream.clone());
                collection = collection.stream(stream);
            }
        }
        debug!(
            ?seqnum,
            "confirming an upstream-owned no-op selection locally"
        );
        let message = gst::message::StreamsSelected::builder(&collection.build())
            .streams(selected)
            .src(&self.pipeline)
            .seqnum(seqnum)
            .build();
        if self.pipeline.post_message(message).is_err() {
            warn!("failed to post the local selection confirmation");
        }
    }

    /// Queue [`Job::ClearStateFailure`] for an error message this crate
    /// consumes instead of surfacing. Called from the bus translation, i.e. a
    /// streaming thread, so it only queues: reading and re-committing the
    /// pipeline's state belongs on the worker.
    ///
    /// Errors that DO reach the caller are deliberately left alone. The caller
    /// decides what a real error means (a teardown, usually), and unlatching
    /// under it would hide a pipeline that genuinely cannot run.
    fn queue_state_unlatch(&self) {
        if std::env::var_os("FCAST_NO_ERROR_STATE_UNLATCH").is_some() {
            return;
        }
        let _ = self.work_tx.send(Job::ClearStateFailure);
    }

    /// Classify a bus message source by the generation-tagged inputs.
    fn classify_error_src(&self, src: Option<&gst::Object>) -> ErrorSource {
        let Some(src) = src else {
            return ErrorSource::Unknown;
        };
        let generation = self.current_generation();
        let routing = self.routing.lock();
        for input in &routing.inputs {
            let is_from_input = src == input.element.upcast_ref::<gst::Object>()
                || src.has_as_ancestor(&input.element);
            if !is_from_input {
                continue;
            }
            if input.generation > generation {
                return ErrorSource::Prepared(input.generation);
            }
            if input.generation != generation {
                return ErrorSource::Stale;
            }
            return match &input.external {
                Some(external) => ErrorSource::External(external.id),
                None => ErrorSource::Main,
            };
        }
        ErrorSource::Unknown
    }

    /// Decide what a bus error from a live external subtitle input means,
    /// by its cause (see [`decisions::external_error_action`]). A
    /// transport-race death recovers in place: the join-time replay
    /// restarts the task, or the never-linked retry re-attaches. Anything
    /// else is detached and reported as
    /// [`PlaybinEvent::ExternalSubtitleFailed`]. Runs on the posting
    /// (streaming) thread, so it only queues worker follow-up.
    fn handle_external_error(
        &self,
        id: ExternalSubId,
        error: &gst::glib::Error,
        debug_info: Option<gst::glib::GString>,
    ) {
        let (epoch, never_linked) = {
            let routing = self.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| i.is_external(id)) else {
                debug!(?id, "error from an already-detached external input");
                return;
            };
            (
                input.external.as_ref().expect("external input").epoch,
                input.stream_ids().is_empty() && input.db3_sink_pads.is_empty(),
            )
        };

        use decisions::ExternalErrorAction as Action;
        match decisions::external_error_action(debug_info.as_deref()) {
            Action::Fail => {
                warn!(?id, %error, ?debug_info, "external subtitle input failed");
                let _ = self.work_tx.send(Job::FailSub { id, epoch });
            }
            // A linked input recovers through the join-time replay. One
            // that died before anything of it reached decodebin3 has
            // nothing to select, so the replay can never run: re-attach
            // it (safe exactly here, see `retry_subtitle`), a bounded
            // number of times.
            Action::Recover if never_linked && epoch < MAX_ATTACH_RETRIES => {
                info!(?id, %error, epoch, "the input died before reaching decodebin3; retrying the attach");
                let _ = self.work_tx.send(Job::RetrySub { id, epoch });
            }
            Action::Recover if never_linked => {
                debug!(?id, %error, "the input keeps dying unlinked; the watchdog owns the verdict");
            }
            Action::Recover => {
                debug!(?id, %error, "a transport race killed the input's task; the next replay restarts it");
                let mut routing = self.routing.lock();
                if let Some(external) = routing
                    .inputs
                    .iter_mut()
                    .filter_map(|input| input.external.as_mut())
                    .find(|external| external.id == id && external.epoch == epoch)
                {
                    external.task_dead = true;
                }
            }
        }
    }

    /// Whether a bus message originates inside an external subtitle input.
    /// Such inputs post their own PARTIAL stream collections straight to the
    /// bus (they are siblings of decodebin3, nothing aggregates them), and
    /// those must not be mistaken for the pipeline-wide collection. NOT
    /// applied to the main input: some media (plain mp3) only ever gets a
    /// collection message from the main input's parsebin, never decodebin3.
    fn message_from_external_input(&self, msg: &gst::Message) -> bool {
        let Some(src) = msg.src() else {
            return false;
        };
        let routing = self.routing.lock();
        routing
            .inputs
            .iter()
            .filter(|i| i.external.is_some())
            .any(|i| {
                src == i.element.upcast_ref::<gst::Object>() || src.has_as_ancestor(&i.element)
            })
    }

    /// Whether a bus message originates inside the prepared next input.
    /// Its buffering messages must not drive the caller's buffering state
    /// machine while the CURRENT item plays, its own (parsebin-posted) stream
    /// collection belongs to the next item, and so does any duration it
    /// refines.
    fn message_from_prepared_input(&self, msg: &gst::Message) -> bool {
        let Some(src) = msg.src() else {
            return false;
        };
        let prepared = self.prepared.lock();
        prepared.as_ref().is_some_and(|p| {
            src == p.element.upcast_ref::<gst::Object>() || src.has_as_ancestor(&p.element)
        })
    }

    /// Whether a stream collection consists purely of the prepared input's
    /// streams. Catches the decodebin3-posted form of the next item's
    /// collection (whose message src is decodebin3, not the input).
    fn collection_is_prepared(&self, collection: &gst::StreamCollection) -> bool {
        let prepared = self.prepared.lock();
        let Some(prepared) = prepared.as_ref() else {
            return false;
        };
        let ids = prepared.stream_ids();
        if ids.is_empty() {
            return false;
        }
        let mut any = false;
        for stream in collection.iter() {
            let Some(sid) = stream.stream_id() else {
                continue;
            };
            if ids.iter().any(|id| *id == sid) {
                any = true;
            } else {
                // A stream from elsewhere: a combined or current-item
                // collection, not the prepared item's.
                return false;
            }
        }
        any
    }

    /// Refresh the recorded output groups from each routed pad's sticky
    /// stream-start. Route-time recording is best-effort (the first
    /// stream-start can pass while the pad is being routed, before the
    /// probe exists); by prepare time the stickies are authoritative.
    fn refresh_output_groups(inner: &Arc<Inner>) {
        let mut routing = inner.routing.lock();
        let mut seen = None;
        for routed in routing.routed.iter_mut() {
            if let Some(group) = routed
                .db3_src_pad
                .sticky_event::<gst::event::StreamStart>(0)
                .and_then(|event| event.group_id())
            {
                routed.group = Some(group);
                seen = Some(group);
            }
        }
        drop(routing);
        if let Some(group) = seen {
            let mut active = inner.active_group.lock();
            if active.is_none() {
                *active = Some(group);
            }
        }
    }

    /// Output-side activation trigger: a STREAM_START on a decodebin3
    /// output pad carries a new group id. When a prepared item is pending,
    /// the group change IS the switch (a same-slot continuation posts no
    /// new streams-selected, so the data plane is the reliable signal).
    fn note_output_stream_start(&self, group: Option<gst::GroupId>) {
        let Some(group) = group else { return };
        let retired = {
            let mut active = self.active_group.lock();
            if *active == Some(group) {
                return;
            }
            let first_of_load = active.is_none();
            let retired = *active;
            *active = Some(group);
            if first_of_load {
                return;
            }
            retired
        };
        let prepared = self.prepared.lock().take();
        if let Some(prepared) = prepared {
            info!(
                generation = prepared.generation,
                "gapless activation: the prepared item's group reached the output"
            );
            self.activate_prepared_now(prepared, retired);
        }
    }

    /// The gapless EOS-hold decision, shared by the output gate and the
    /// post-streamsynchronizer gate: an EOS on a pad whose stream group is
    /// `pad_group` must be dropped while a swap is pending (committed to a
    /// next item, nothing may end the pipeline) or while the pad still
    /// carries a non-active group, either lagging the active one or
    /// positively the RETIRED one (old-item drainage, see
    /// [`Inner::retired_group`]). Unknowns on either side never drop: only
    /// a positively known group mismatch is old-item drainage. A pending
    /// drop is recorded for the cancel synthesis (see
    /// [`SwapState::dropped_eos`]). Returns (drop, pending, behind).
    fn gapless_eos_check_and_mark(&self, pad_group: Option<gst::GroupId>) -> (bool, bool, bool) {
        let active_group = *self.active_group.lock();
        let retired_group = *self.retired_group.lock();
        let behind = match (pad_group, active_group) {
            (Some(pad_group), Some(active)) => pad_group != active,
            _ => false,
        } || (pad_group.is_some() && pad_group == retired_group);
        // One lock hold for the check AND the drop record: a cancel between
        // them would zero the state and the mark would pollute the next
        // prepare's gate.
        let mut state = self.swap_gate.state.lock();
        let pending = state.pending.is_some();
        if pending {
            // The item's end is consumed for good; a cancelled swap must
            // synthesize it (see `SwapState::dropped_eos`).
            state.dropped_eos = true;
        }
        (pending || behind, pending, behind)
    }

    /// A full-pipeline flushing seek went out. It reset
    /// streamsynchronizer's per-pad EOS bookkeeping and restarted every
    /// branch it reached, so the passing-EOS mirror is stale FOR THE LIVE
    /// BRANCHES. It is cleared only when a live video branch received the
    /// flush. A video stream deselected at seek time is not restarted by
    /// the seek, so its drained state stands and the drained-resurrect park
    /// in `route_db3_pad` must still see the mirror. Gated on that park's
    /// lever, `FCAST_NO_DRAINED_RESURRECT_PARK` (without the park the
    /// mirror has no route-time reader and the pre-existing behavior never
    /// cleared it).
    fn clear_passing_eos_after_flush(&self) {
        if std::env::var_os("FCAST_NO_DRAINED_RESURRECT_PARK").is_some() {
            return;
        }
        let video_live = self
            .routing
            .lock()
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Video && r.downstream.is_some());
        if video_live {
            *self.passing_eos_group.lock() = None;
        }
    }

    /// Selection-side activation trigger, run against every
    /// STREAMS_SELECTED: when the selection names (only) the prepared
    /// input's streams, decodebin3 has switched to the next item. Some
    /// switches post this (fresh slots), same-slot continuations do not
    /// (see [`Self::note_output_stream_start`], the other trigger).
    fn try_activate_prepared(&self, selected_ids: &[String]) {
        let prepared = {
            let mut slot = self.prepared.lock();
            let Some(prepared) = slot.as_ref() else {
                return;
            };
            let ids = prepared.stream_ids();
            if selected_ids.is_empty()
                || !selected_ids
                    .iter()
                    .all(|sel| ids.iter().any(|id| id == sel))
            {
                return;
            }
            slot.take().expect("checked above")
        };
        info!(
            generation = prepared.generation,
            "gapless activation: the prepared item's streams are selected"
        );
        // This trigger fires input-side, BEFORE the output pads flip to the
        // new group: the still-active group is the one being retired.
        let retired = *self.active_group.lock();
        self.activate_prepared_now(prepared, retired);
    }

    /// The activation itself: adopt the prepared generation NOW, on the
    /// calling posting/streaming thread, so everything after it is stamped
    /// with the new one; re-seed the per-item state exactly like a load's
    /// reset; and queue the input surgery for the worker. The user-facing
    /// [`PlaybinEvent::PreparedActivated`] and the held-back collection are
    /// NOT emitted here for an item with audio: they are held and released
    /// when the item's audio reaches the sink (see [`Inner::held_activation`]),
    /// so the title/duration switch lands with the sound. Audio-less items
    /// emit them here, keeping the activation-then-collection order.
    fn activate_prepared_now(&self, prepared: PreparedNext, retired: Option<gst::GroupId>) {
        // A prior hold still waiting on its sink boundary (unreachable with
        // real media, two swaps within one queue depth) is flushed under the
        // outgoing generation, before we adopt this one, to keep order and
        // stamps correct.
        self.release_held_activation();

        self.generation.store(prepared.generation, Ordering::SeqCst);

        // Output pads still carrying the previous item's group keep their
        // EOS dropped until they flip or die (see `Inner::retired_group`):
        // the selection trigger can run this while the old tail is still
        // draining.
        *self.retired_group.lock() = retired;

        // The swap window closes: the output EOS hold disarms (the new
        // item's own end must flow) while `swapped` stays set so straggler
        // block probes on the now-live input self-remove. The next prepare
        // re-seeds the whole gate. Guarded by generation: a concurrent
        // prepare for the item after this one may have re-armed the gate
        // already, and ITS pending must survive this activation.
        {
            let mut state = self.swap_gate.state.lock();
            if state.pending == Some(prepared.generation) {
                state.pending = None;
                state.dropped_eos = false;
            }
        }

        // Per-item state rolls like a load's reset, EXCEPT the user's own track
        // intent: see `SelectionEngine::reset_across_gapless`. A plain `reset()`
        // here discarded a subtitle-off and the boundary relinked the text
        // branch the user had turned off
        // (`regression_gapless::subtitle_disable_survives_a_gapless_transition`).
        self.selection.lock().reset_across_gapless();
        *self.last_applied_subtitle.lock() = None;
        *self.upstream_selection.lock() = None;
        self.last_upstream_ids.lock().clear();
        *self.intended_timeline.lock() = (1.0, gst::ClockTime::ZERO);
        {
            let mut routing = self.routing.lock();
            routing.collection_video_ids = prepared
                .pending_collection
                .as_ref()
                .map(|collection| {
                    collection
                        .iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::VIDEO))
                        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                        .collect()
                })
                .unwrap_or_default();
        }

        // The selection engine is pipeline truth and updates NOW (a track
        // command must act on the item that is actually decoding), even though
        // the caller-facing collection event is held with the switch below.
        let has_audio = prepared
            .pending_collection
            .as_ref()
            .is_some_and(|collection| {
                collection
                    .iter()
                    .any(|s| s.stream_type().contains(gst::StreamType::AUDIO))
            });
        if let Some(collection) = prepared.pending_collection.as_ref() {
            let streams = collection
                .iter()
                .filter_map(|s| {
                    let sid = s.stream_id()?.to_string();
                    let typ = s.stream_type();
                    let kind = if typ.contains(gst::StreamType::VIDEO) {
                        StreamKind::Video
                    } else if typ.contains(gst::StreamType::AUDIO) {
                        StreamKind::Audio
                    } else if typ.contains(gst::StreamType::TEXT) {
                        StreamKind::Text
                    } else {
                        return None;
                    };
                    Some(selection::CollectionStream { sid, kind })
                })
                .collect();
            self.selection.lock().collection_changed(streams);
        }

        // The user-facing switch: hold it for the sink boundary if the item
        // has audio to anchor the release on, else emit it now (an audio-less
        // item never crosses the audio queue, so there is nothing to wait for).
        let held = HeldActivation {
            collection: prepared.pending_collection,
        };
        if has_audio {
            *self.held_activation.lock() = Some(held);
        } else {
            self.emit_held(held);
        }

        // Input surgery (removing the drained previous inputs) must not run
        // on this streaming thread.
        let _ = self.work_tx.send(Job::FinishActivation);
    }

    /// Emit a gapless activation's user-facing events in the canonical
    /// fresh-load order: [`PlaybinEvent::PreparedActivated`] (which the caller
    /// uses to adopt the new generation, letting the collection past its
    /// supersession guard) followed by the new item's collection.
    fn emit_held(&self, held: HeldActivation) {
        self.emit(PlaybinEvent::PreparedActivated);
        if let Some(collection) = held.collection {
            self.emit(PlaybinEvent::StreamCollection(collection));
        }
    }

    /// Release a held gapless activation, if one is waiting. Called from the
    /// `fpb-aqueue` src STREAM_START probe when the item's audio reaches the
    /// sink, and as a flush before a superseding activation. A no-op when
    /// nothing is held (the common case, and every non-boundary STREAM_START).
    fn release_held_activation(&self) {
        let held = self.held_activation.lock().take();
        if let Some(held) = held {
            self.emit_held(held);
        }
    }

    /// Translate a bus message into its typed event, applying the crate's
    /// filters: per-element state changes and foreign ASYNC_DONEs are
    /// dropped, external-input collections are swallowed, and errors from
    /// elements no longer in the pipeline (a superseded load's teardown
    /// dying noisily) are discarded.
    fn translate_message(&self, msg: &gst::Message) -> Option<PlaybinEvent> {
        use gst::MessageView;

        let pipeline_obj = self.pipeline.upcast_ref::<gst::Object>();
        let event = match msg.view() {
            MessageView::Eos(_) => {
                // With a prepared next input linked, decodebin3 switches at
                // drain and no pipeline EOS should exist between the items.
                // One arriving anyway means the gapless handoff missed
                // (e.g. the prepared input produced no streams in time);
                // surface it so the caller's ordinary end-of-stream advance
                // takes over. The next load's reset cleans the input up.
                if self.prepared.lock().is_some() {
                    warn!("pipeline EOS with a prepared next input: gapless handoff missed");
                }
                PlaybinEvent::EndOfStream
            }
            MessageView::Error(error) => {
                if let Some(src) = msg.src()
                    && src != pipeline_obj
                    && !src.has_as_ancestor(&self.pipeline)
                {
                    debug!(
                        src = %src.name(),
                        "Dropping error from element no longer in the current pipeline"
                    );
                    // It was still a child when it posted, so the pipeline
                    // carries the latch even though the element has left.
                    self.queue_state_unlatch();
                    return None;
                }
                // Live external subtitle inputs are the crate's own to
                // babysit: their errors are consumed here (re-arm or a
                // typed failure event), never surfaced as pipeline errors.
                let origin = match self.classify_error_src(msg.src()) {
                    ErrorSource::External(id) => {
                        self.handle_external_error(id, &error.error(), error.debug());
                        self.queue_state_unlatch();
                        return None;
                    }
                    ErrorSource::Prepared(generation) => {
                        // The pre-armed next input died before activating
                        // (e.g. the resource moved since the prefetch). The
                        // CURRENT item is unaffected: drop the prepare, tell
                        // the caller, and let its ordinary end-of-stream
                        // advance load the item normally (surfacing a real
                        // error then, if it is still broken).
                        warn!(
                            generation,
                            error = %error.error(),
                            debug = ?error.debug(),
                            "prepared next input failed before activation"
                        );
                        let _ = self.work_tx.send(Job::CancelPrepared { notify: false });
                        self.queue_state_unlatch();
                        self.emit(PlaybinEvent::PreparedFailed { generation });
                        return None;
                    }
                    ErrorSource::Main => ErrorOrigin::Main,
                    ErrorSource::Stale => ErrorOrigin::Stale,
                    ErrorSource::Unknown => ErrorOrigin::Unknown,
                };
                // Diagnostic only: supersession is decided by the event's
                // generation and attribution by `origin`, not by this URI.
                let failed_uri = msg
                    .src()
                    .and_then(|src| src.dynamic_cast_ref::<gst::URIHandler>())
                    .and_then(|handler| handler.uri())
                    .map(|uri| uri.to_string());
                PlaybinEvent::Error {
                    origin,
                    error: error.error(),
                    failed_uri,
                }
            }
            MessageView::Warning(warning) => {
                PlaybinEvent::Warning(warning.error().message().to_string())
            }
            MessageView::Tag(tag) => PlaybinEvent::Tags(tag.tags()),
            MessageView::Buffering(buffering) => {
                // The prepared next input buffers ahead while the CURRENT
                // item plays; its levels must not drive the caller's
                // buffering state machine. Once it activates it is the main
                // input and its messages flow normally.
                if self.message_from_prepared_input(msg) {
                    debug!(
                        percent = buffering.percent(),
                        "dropping buffering from the prepared next input"
                    );
                    return None;
                }
                PlaybinEvent::Buffering(buffering.percent())
            }
            MessageView::StateChanged(change) => {
                if !msg.src().map(|s| s == pipeline_obj).unwrap_or(false) {
                    return None;
                }
                // Every pipeline state edge re-attempts the postponed
                // text-branch work (see [`Job::DrainTextWork`]). On the bus
                // this only queues. The worker does the blocking part.
                if self.has_deferred_text_work() {
                    let _ = self.work_tx.send(Job::DrainTextWork);
                }
                PlaybinEvent::StateChanged {
                    old: change.old(),
                    current: change.current(),
                    pending: change.pending(),
                }
            }
            MessageView::RequestState(state) => {
                let state = state.requested_state();
                debug!(?state, "State requested");
                PlaybinEvent::RequestState(state)
            }
            MessageView::StreamCollection(collection) => {
                if self.message_from_external_input(msg) {
                    debug!(
                        src = ?msg.src().map(|s| s.name()),
                        "Ignoring a partial stream collection from an external subtitle input"
                    );
                    return None;
                }
                let collection = collection.stream_collection();
                // The prepared next input's collection belongs to the NEXT
                // item: hold it back and deliver it at activation, after
                // PreparedActivated and stamped with the new generation
                // (the input-posted form is caught by ancestry, the
                // decodebin3-posted form by its stream ids).
                //
                // This has to stay AHEAD of the decodebin3 filter below. The
                // gapless handoff needs the next item's collection as early as
                // the prepared input can post it, and dropping the
                // input-posted form left `gapless_switch_from_text_bearing_
                // item_to_one_without` waiting for a `PreparedActivated` that
                // never came.
                if self.message_from_prepared_input(msg) || self.collection_is_prepared(&collection)
                {
                    debug!("holding the prepared next input's stream collection");
                    if let Some(prepared) = self.prepared.lock().as_mut() {
                        prepared.pending_collection = Some(collection);
                    }
                    return None;
                }
                // Only decodebin3's collection is the MERGED one, and it is
                // the only one whose stream ids a `SELECT_STREAMS` sent to
                // decodebin3 may name. Three elements post collections onto
                // this bus (gsturisourcebin.c:3084, gstdecodebin3.c:2814,
                // gstparsebin.c:3659 and :4219), and with a per-stream source
                // every input pad gets its own parsebin, so the partial ones
                // arrive interleaved with the merged ones and each names a
                // single stream.
                //
                // Feeding those to the selection engine makes the collection
                // appear to SHRINK. `collection_changed` then reconciles the
                // applied selection against a collection with no video in it,
                // `SelectionEngine::resolve` reads the empty video slot as
                // "video off", the no-text-without-video rule strips the
                // subtitle too, and the composed event actively deselects
                // both. Observed as `sent SELECT_STREAMS ids=["...audio_0"]`
                // followed by `selection drops video, parking the video chain
                // at READY` in the middle of a load that asked for neither.
                //
                // Matching the CURRENT core also drops a collection from a
                // decodebin3 the load already superseded.
                // A/B lever for bisecting regressions without a rebuild, like
                // FCAST_NO_SELECTION_REPLAY.
                let from_db3 = std::env::var_os("FCAST_NO_DB3_COLLECTION_FILTER").is_some() || {
                    let core = self.core.lock();
                    match (core.as_ref(), msg.src()) {
                        (Some(core), Some(src)) => src == core.db3.upcast_ref::<gst::Object>(),
                        _ => false,
                    }
                };
                if !from_db3 {
                    debug!(
                        src = ?msg.src().map(|s| s.name()),
                        "ignoring a partial stream collection that is not decodebin3's merged one"
                    );
                    return None;
                }
                // Cache the collection's video ids BEFORE the caller can
                // react to the event: `select_streams` classifies a
                // no-video selection with them (see there).
                {
                    let mut routing = self.routing.lock();
                    routing.collection_video_ids = collection
                        .iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::VIDEO))
                        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                        .collect();
                }
                // The selection engine reconciles against the new collection
                // BEFORE the caller can react to the event (its next pump
                // resolves desires against it).
                let streams = collection
                    .iter()
                    .filter_map(|s| {
                        let sid = s.stream_id()?.to_string();
                        let typ = s.stream_type();
                        let kind = if typ.contains(gst::StreamType::VIDEO) {
                            StreamKind::Video
                        } else if typ.contains(gst::StreamType::AUDIO) {
                            StreamKind::Audio
                        } else if typ.contains(gst::StreamType::TEXT) {
                            StreamKind::Text
                        } else {
                            return None;
                        };
                        Some(selection::CollectionStream { sid, kind })
                    })
                    .collect();
                self.selection.lock().collection_changed(streams);
                PlaybinEvent::StreamCollection(collection)
            }
            MessageView::StreamsSelected(streams) => {
                let mut video = None;
                let mut audio = None;
                let mut subtitle = None;
                let mut all_ids = Vec::new();

                for stream in streams.streams() {
                    let typ = stream.stream_type();
                    let id = stream.stream_id().map(|id| id.to_string());
                    if let Some(id) = &id {
                        all_ids.push(id.clone());
                    }

                    if typ.contains(gst::StreamType::VIDEO) {
                        video = id;
                    } else if typ.contains(gst::StreamType::AUDIO) {
                        audio = id;
                    } else if typ.contains(gst::StreamType::TEXT) {
                        subtitle = id;
                    }
                }

                // An upstream-selection demuxer reports only its OWN streams,
                // so its report says nothing about an external input's text:
                // absence there must not deselect one. Merge the crate-owned
                // slot back in (cache-only read, this runs on a streaming
                // thread). Same lever as the dispatch split.
                if subtitle.is_none() && *self.upstream_selection.lock() == Some(true) {
                    let kept = self.selection.lock().subtitle_sid();
                    if let Some(sid) = kept {
                        let is_external = self.routing.lock().inputs.iter().any(|input| {
                            input.external.is_some() && input.stream_ids().contains(&sid)
                        });
                        if is_external {
                            debug!(%sid, "keeping the external subtitle an upstream report cannot speak about");
                            all_ids.push(sid.clone());
                            subtitle = Some(sid);
                        }
                    }
                }

                // Track the upstream-owned active set (every report, minus
                // external-input sids) so the dispatch split can tell a real
                // upstream change from a no-op: an adaptive demuxer only ever
                // confirms an activation EDGE, so a no-op send would leave the
                // engine awaiting a confirmation that cannot come.
                {
                    let mut upstream_ids: Vec<String> = {
                        let routing = self.routing.lock();
                        all_ids
                            .iter()
                            .filter(|sid| {
                                !routing.inputs.iter().any(|input| {
                                    input.external.is_some()
                                        && input.stream_ids().contains(sid)
                                })
                            })
                            .cloned()
                            .collect()
                    };
                    upstream_ids.sort();
                    *self.last_upstream_ids.lock() = upstream_ids;
                }

                // A selection naming the prepared input's streams IS the
                // gapless switch: adopt the next item's generation and
                // deliver its held-back collection first, so this selection
                // event arrives in a fresh load's order and stamping.
                self.try_activate_prepared(&all_ids);

                // The hold release used to sit HERE, ahead of the replay
                // decision below, and that ordering is what rendered a
                // just-selected external against the wrong origin. It is now
                // taken after it, so the release can be made to WAIT for the
                // realigning seek. See `Inner::release_owed_hold`. The lever
                // puts the call back in this position, unconditional as it
                // was, so an A/B compares the orderings too and not just the
                // owing.
                let inline_hold_release = std::env::var_os("FCAST_NO_OWED_HOLD_RELEASE").is_some();
                if inline_hold_release {
                    self.unblock_selected_externals(&all_ids, None);
                }

                let seqnum = msg.seqnum();
                // The previously APPLIED subtitle, for the selection-time
                // replay below. Tracked here and not read off the engine:
                // its `applied` is optimistic (set at dispatch), so by
                // confirmation time it already names the new target.
                let previous_subtitle = std::mem::replace(
                    &mut *self.last_applied_subtitle.lock(),
                    subtitle.clone(),
                );
                // Record what applied (and settle/overtake the in-flight
                // dispatch) before the caller sees the event. The caller's
                // pump then dispatches any re-assertion or queued work.
                self.selection.lock().streams_selected(
                    seqnum,
                    &TrackSelection {
                        video: video.clone(),
                        audio: audio.clone(),
                        subtitle: subtitle.clone(),
                    },
                );

                // Selection-time replay, the pad-reuse counterpart of the
                // join-time one: switching between text streams makes
                // decodebin3 swap the stream on the already-linked output
                // pad, so no join (and no join-time replay) ever fires. The
                // replay is what restarts an external whose task died
                // deselected and what re-aligns its timeline, so a selection
                // that MOVES onto an external with the branch already live
                // queues it here. A fresh join sees an unlinked branch and
                // keeps its join-time replay; a same-sid re-assertion is
                // skipped so a redundant SELECT_STREAMS cannot blink the
                // current cue.
                // The external whose hold release the replay below owes, if
                // any (see `Inner::release_owed_hold`).
                let mut owed_release: Option<(ExternalSubId, u32)> = None;
                if let Some(sid) = &subtitle
                    && previous_subtitle.as_deref() != Some(sid.as_str())
                    // A/B lever for diagnosing switch regressions without a
                    // rebuild, like FCAST_SCALETEMPO.
                    && std::env::var_os("FCAST_NO_SELECTION_REPLAY").is_none()
                {
                    let (target, branch_live) = {
                        let routing = self.routing.lock();
                        let branch_live = routing
                            .routed
                            .iter()
                            .any(|r| r.kind == StreamKind::Text && r.downstream.is_some());
                        let target = routing.inputs.iter().find_map(|input| {
                            let external = input.external.as_ref()?;
                            input.stream_ids().contains(sid).then_some((
                                external.id,
                                external.epoch,
                                external.last_origin,
                                external.task_dead,
                            ))
                        });
                        (target, branch_live)
                    };
                    if let Some((id, epoch, last_origin, task_dead)) = target {
                        let (_, origin) = self.overlay_timeline();
                        // With every text branch PARKED there is no pad swap
                        // to wait on and no join to replay from, and a drained
                        // external whose multiqueue slot was reclaimed for
                        // another stream (decodebin3 "Re-using existing unused
                        // slot") has no pad carrying its sid at all: only this
                        // re-push brings it back (`ext-subtitle-regression-2.txt`,
                        // overlay in passthrough + branch on its park sink).
                        // Lever: `FCAST_NO_PARKED_SELECTION_REPLAY`.
                        let parked_needs_push = !branch_live
                            && std::env::var_os("FCAST_NO_PARKED_SELECTION_REPLAY").is_none();
                        if task_dead || origin != last_origin || parked_needs_push {
                            // The input's cues WILL render shifted: the
                            // destructive flush-replay is justified and must
                            // run before anything wrong reaches the screen.
                            debug!(
                                ?id,
                                sid,
                                %origin,
                                %last_origin,
                                task_dead,
                                branch_live,
                                "the selection moved onto a dead, differently-timed or slotless external; replaying it"
                            );
                            // The release of this input's hold now belongs to
                            // that replay, and only if the job is really on
                            // its way: a failed send leaves nothing that could
                            // ever discharge it.
                            if self
                                .work_tx
                                .send(Job::ReplaySub { id, epoch, attempt: 0 })
                                .is_ok()
                                && !inline_hold_release
                            {
                                owed_release = Some((id, epoch));
                            }
                        } else {
                            // Same timeline: a still-alive input (or one
                            // whose slot buffers data) delivers on its own,
                            // and a flush right now races the very swap this
                            // selection started. The verification replays
                            // only if nothing arrives (a dead input).
                            debug!(
                                ?id,
                                sid,
                                "the selection moved onto an external with a live text branch; arming its replay check"
                            );
                            self.arm_replay_verification(id, epoch, 0);
                        }
                    }
                }

                // An external held blocked until selected may flow now (see
                // `ExternalInput::hold_until_selected`), EXCEPT the one whose
                // realigning replay was just queued: that one waits for the
                // seek (see `Inner::release_owed_hold`).
                if !inline_hold_release {
                    self.unblock_selected_externals(&all_ids, owed_release);
                }

                PlaybinEvent::StreamsSelected {
                    video,
                    audio,
                    subtitle,
                    seqnum,
                }
            }
            MessageView::ClockLost(_) => PlaybinEvent::ClockLost,
            MessageView::AsyncDone(_) => {
                if !msg.src().map(|s| s == pipeline_obj).unwrap_or(false) {
                    return None;
                }
                // Settle an in-flight refresh flush (attribution by
                // exclusivity, see the selection module docs).
                self.selection.lock().refresh_done();
                // An async settle is a state edge too (a PAUSED-to-PAUSED
                // seek posts no state-changed), so it also re-attempts the
                // postponed text-branch work.
                if self.has_deferred_text_work() {
                    let _ = self.work_tx.send(Job::DrainTextWork);
                }
                PlaybinEvent::AsyncDone
            }
            MessageView::DurationChanged(_) => {
                // A prefetching prepared input refines the NEXT item's
                // duration, which says nothing about the item playing now. Same
                // treatment as its buffering levels: dropped until it activates
                // and becomes the main input.
                if self.message_from_prepared_input(msg) {
                    debug!("dropping duration-changed from the prepared next input");
                    return None;
                }
                // Past a performed swap the prepared input is the only linked
                // upstream, so the re-query this event asks for would be
                // answered by the NEXT item and latch its duration onto the
                // item still playing. Drop it: the activation that follows
                // resets the caller's duration anyway, and the new item posts
                // its own duration-changed once its demuxer refines it.
                //
                // Minimal lock scope on purpose: this runs on the posting
                // (streaming) thread, and the guard is released before the log.
                let activating = self.swap_gate.state.lock().activation_pending();
                if let Some(generation) = activating {
                    debug!(
                        generation,
                        "dropping duration-changed inside the gapless activation window"
                    );
                    return None;
                }
                PlaybinEvent::DurationChanged
            }
            MessageView::Latency(_) => {
                // An element's latency changed (e.g. the video sink's
                // render-delay): the pipeline must re-query and redistribute
                // latency or the change never takes effect. Runs on the
                // worker, not this posting (streaming) thread (see
                // `Job::RecalculateLatency`).
                let _ = self.work_tx.send(Job::RecalculateLatency);
                return None;
            }
            MessageView::Element(element) => {
                // fimagedec announces what it is decoding (format,
                // dimensions, animated or still) for load classification.
                let s = element.structure()?;
                if s.name() != "fcast-image-stream" {
                    return None;
                }
                PlaybinEvent::ImageStream(s.to_owned())
            }
            _ => return None,
        };
        Some(event)
    }
}

impl FcastPlaybin {
    /// Execute one queued job on the worker thread.
    fn run_job(&self, job: Job) {
        let inner = &self.inner;
        match job {
            Job::SetState { target } => {
                // Downward transitions take the route gate (a pad routed
                // into the descending pipeline deadlocks it).
                //
                // A change to the state the pipeline is ALREADY in posts NO
                // `state-changed`: `gst_element_continue_state` guards it with
                // `old_state != old_next || old_ret == ASYNC` ("don't post silly
                // messages with the same state"). The caller's machine advances
                // only on that message, so a no-op dispatch parks it in
                // `Phase::Changing` for good. Reached routinely, because
                // `StateMachine::buffering` always redispatches a PLAYING target
                // and `player.rs` `uri_loaded` has usually driven it there
                // already (`dash-start-seek-text-join-race.md`).
                //
                // Read BEFORE the call: afterwards "already there" and "just
                // arrived" are indistinguishable. `Ok(Async)` is excluded, that
                // is the one case GStreamer does post.
                // Lever: `FCAST_NO_SYNTHETIC_STATE_EDGE`.
                let (before, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
                let silent = current == target
                    && pending == gst::State::VoidPending
                    && !matches!(before, Ok(gst::StateChangeSuccess::Async))
                    && std::env::var_os("FCAST_NO_SYNTHETIC_STATE_EDGE").is_none();
                let _ = self.set_pipeline_state(target);
                if silent {
                    debug!(
                        ?target,
                        "the pipeline was already in the requested state; \
                         reporting the state edge GStreamer suppresses"
                    );
                    inner.emit(PlaybinEvent::StateChanged {
                        old: target,
                        current: target,
                        pending: gst::State::VoidPending,
                    });
                }
            }
            Job::Stop { target, done } => {
                if let Err(err) = self.teardown(target) {
                    warn!(?err, ?target, "fcastplaybin teardown failed");
                }
                if let Some(done) = done {
                    done();
                    debug!("Sent stop feedback signal");
                }
            }
            Job::Load {
                input,
                start,
                generation,
            } => {
                match self.load_with_generation(input, start, generation) {
                    Ok(outcome) => {
                        if outcome.live {
                            debug!("Pipeline is live");
                        }
                        inner.emit(PlaybinEvent::Loaded { live: outcome.live });
                    }
                    // No event: any user-visible failure arrives through the
                    // pipeline error path.
                    Err(err) => error!(?err, "fcastplaybin load failed"),
                }
            }
            Job::Seek(seek) => {
                // Non-blocking query: a zero timeout returns the in-flight
                // transition instead of waiting for it. An unbounded
                // `state(None)` here wedged the whole worker when a seek
                // arrived mid-preroll and the preroll stalled, queueing
                // every later job behind it forever.
                let (_, state, pending) = inner.pipeline.state(gst::ClockTime::ZERO);

                if state != gst::State::Paused || pending != gst::State::VoidPending {
                    inner.emit(PlaybinEvent::QueueSeek(seek));
                    let _ = inner.pipeline.set_state(gst::State::Paused);
                    // An async transition read above can commit between the
                    // query and the call. The call is then a same-state no-op
                    // posting nothing, and the real settle edge was emitted
                    // BEFORE the hand-back (sync bus handler), so the parked
                    // seek would wait forever for an edge that already
                    // passed. Report the settle GStreamer will not repeat.
                    // Lever: `FCAST_NO_SEEK_REFUSAL_EDGE`.
                    if pending != gst::State::VoidPending
                        && std::env::var_os("FCAST_NO_SEEK_REFUSAL_EDGE").is_none()
                    {
                        let (_, now, now_pending) =
                            inner.pipeline.state(gst::ClockTime::ZERO);
                        if now == gst::State::Paused
                            && now_pending == gst::State::VoidPending
                        {
                            debug!(
                                "the refused seek's PAUSED request was a no-op on an \
                                 already-settled pipeline; reporting the missed settle"
                            );
                            inner.emit(PlaybinEvent::StateChanged {
                                old: gst::State::Paused,
                                current: gst::State::Paused,
                                pending: gst::State::VoidPending,
                            });
                        }
                    }
                    return;
                }

                let position = match seek.position {
                    Some(pos) => pos,
                    None => {
                        // A rate-only seek (SetSpeed) has to ask where the
                        // playhead is. Failing SILENTLY here left the caller's
                        // seek slot in flight with nothing to settle it (it
                        // owns the seek queue and waits for an outcome), so
                        // every later seek parked behind a job that had already
                        // given up. Report it like any other failed seek.
                        let Some(pos) = inner.pipeline.query_position::<gst::ClockTime>() else {
                            error!("Failed to query playback position");
                            inner.emit(PlaybinEvent::SeekFailed);
                            return;
                        };
                        pos
                    }
                };

                let rate = seek.rate.unwrap_or(1.0) as f64;
                debug!(rate, ?position, "Performing seek");

                if let Err(err) = send_rate_seek(&inner.pipeline, rate, position) {
                    error!(?err, "Failed to seek");
                    inner.emit(PlaybinEvent::SeekFailed);
                } else {
                    // The seek's flush restarted the LIVE branches, so for
                    // them "this group's end has entered ssync" no longer
                    // holds, and a stale mirror would wrongly park the next
                    // video re-enable (see the drained-resurrect arm in
                    // `route_db3_pad`, whose lever also gates this clear).
                    // Only a seek that reached a live video branch clears
                    // it. A stream DESELECTED at seek time is not restarted
                    // by the seek (measured on seed 1600058, the source's
                    // video task stays idle at EOS through the seek), so
                    // its pre-seek drained state stands and the park must
                    // still see it.
                    inner.clear_passing_eos_after_flush();
                    *inner.intended_timeline.lock() = (rate, position);
                    inner.forward_seek_to_live_externals(rate, position);
                    inner.emit(PlaybinEvent::RateChanged(rate));
                }
            }
            Job::RefreshSeek { seqnum } => {
                // RE-VALIDATED HERE, not only where it was scheduled.
                // `SelectionEngine::pump` sampled its gates (the caller's
                // quiet, seekable, no external subtitle attached) at dispatch
                // and this job then queued behind SetState/Load/DrainTextWork
                // and the branch disposals. By the time it runs the pipeline
                // can be mid-load, mid-buffering, mid-seek or carrying an
                // external input, and a flushing seek landing there is the
                // FLUSHING-into-an-adaptive-output-loop hazard
                // (FREEZE-DIAGN.md 8.2 #2: adaptivedemux2 serves every track
                // from one task and pauses it for good). `Job::Seek` has such
                // a guard; this one had none.
                //
                // A stale refresh is DROPPED, never re-parked: it is only ever
                // a nicety (the freshly selected track re-emits at its next
                // cue either way), and reporting the failure is what clears
                // the engine's `refreshing` slot, which otherwise blocks every
                // later dispatch. Lever:
                // `FCAST_NO_REFRESH_SEEK_REVALIDATION`.
                if std::env::var_os("FCAST_NO_REFRESH_SEEK_REVALIDATION").is_none() {
                    let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
                    let settled =
                        pending == gst::State::VoidPending && current >= gst::State::Paused;
                    let externals = {
                        let routing = inner.routing.lock();
                        routing.inputs.iter().any(|i| i.external.is_some())
                    };
                    let seekable = {
                        let mut query = gst::query::Seeking::new(gst::Format::Time);
                        inner.pipeline.query(&mut query) && query.result().0
                    };
                    let superseded = inner.selection.lock().refresh_superseded(seqnum);
                    if !settled || externals || !seekable || superseded {
                        debug!(
                            ?current,
                            ?pending,
                            externals,
                            seekable,
                            superseded,
                            ?seqnum,
                            "dropping a refresh seek whose preconditions no longer hold"
                        );
                        inner.selection.lock().refresh_failed(seqnum);
                        inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
                        return;
                    }
                }
                let Some(position) = inner.pipeline.query_position::<gst::ClockTime>() else {
                    debug!("Skipping the refresh seek: no position");
                    inner.selection.lock().refresh_failed(seqnum);
                    inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
                    return;
                };

                // The refresh is a RE-EMIT, not a transport change: it must
                // land on the timeline the item already runs on. Hard-coding
                // rate 1.0 here made every track switch at a non-1.0 speed
                // silently drop the pipeline back to 1.0x, and since a refresh
                // emits no `RateChanged` the caller (and the sender's UI) kept
                // reporting the old speed over 1.0x audio.
                let rate = inner.intended_timeline.lock().0;

                // A flushing seek to the current position in the current
                // state: re-emits the subtitle cue active NOW and flushes
                // the stale one, without a normal seek's Paused round-trip.
                debug!(
                    ?position,
                    rate,
                    ?seqnum,
                    "Refresh seek (flushing, current position)"
                );
                let event = rate_seek_event(rate, position, Some(seqnum));
                if !inner.pipeline.send_event(event) {
                    warn!("Refresh seek failed");
                    inner.selection.lock().refresh_failed(seqnum);
                    inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
                } else {
                    // Same full-pipeline flush as Job::Seek above, same
                    // conditional reset of the passing-EOS mirror.
                    inner.clear_passing_eos_after_flush();
                }
            }
            Job::RecoverClock => {
                debug!("Recovering from clock loss");
                if let Err(err) = inner.pipeline.set_state(gst::State::Paused) {
                    warn!(?err, "Clock recovery: failed to reach Paused");
                    return;
                }
                if let Err(err) = inner.pipeline.set_state(gst::State::Playing) {
                    warn!(?err, "Clock recovery: failed to reach Playing");
                }
            }
            Job::RecalculateLatency => {
                if let Err(err) = inner.pipeline.recalculate_latency() {
                    warn!(?err, "failed to recalculate pipeline latency");
                }
                // Every field freeze so far follows one of these within ~1s;
                // the value the pipeline settled on is the missing datum.
                let mut query = gst::query::Latency::new();
                if inner.pipeline.query(&mut query) {
                    let (live, min, max) = query.result();
                    debug!(live, %min, ?max, "pipeline latency recalculated");
                }
            }
            Job::AttachSub { id, url } => {
                if let Err(err) = self.attach_subtitle_with_id(id, &url) {
                    error!(?err, url, "fcastplaybin subtitle attach failed");
                    inner.emit(PlaybinEvent::ExternalSubtitleFailed { id });
                }
            }
            Job::DetachSub { id } => {
                if let Err(err) = self.detach_subtitle(id) {
                    // Possible for an attach that already failed (nothing
                    // registered), harmless.
                    debug!(?err, ?id, "fcastplaybin subtitle detach failed");
                }
            }
            Job::FailSub { id, epoch } => {
                self.fail_subtitle(id, epoch);
            }
            Job::CheckSub { id, epoch } => {
                self.check_subtitle(id, epoch);
            }
            Job::RetrySub { id, epoch } => {
                self.retry_subtitle(id, epoch);
            }
            Job::AdoptSubState { id, epoch } => {
                let element = {
                    let routing = self.inner.routing.lock();
                    routing
                        .inputs
                        .iter()
                        .find(|input| {
                            input
                                .external
                                .as_ref()
                                .is_some_and(|e| e.id == id && e.epoch == epoch)
                        })
                        .map(|input| input.element.clone())
                };
                let Some(element) = element else {
                    debug!(?id, epoch, "stale state-adopt job; input already gone");
                    return;
                };
                element.set_locked_state(false);
                if let Err(err) = element.sync_state_with_parent() {
                    warn!(?err, ?id, "the materialized external refused the state join");
                }
                debug!(?id, "external input unlocked and joined to the pipeline state");
            }
            Job::ReplaySub { id, epoch, attempt } => {
                self.replay_subtitle(id, epoch, attempt);
            }
            Job::VerifyReplay { id, epoch, attempt } => {
                self.verify_replay(id, epoch, attempt);
            }
            Job::DumpGraph { done } => {
                done(graph::snapshot(inner.pipeline.upcast_ref()));
            }
            Job::PrepareNext { input, generation } => {
                // A newer load or prepare was requested after this one was
                // queued; its reset would remove this input right away.
                if generation != inner.next_generation.load(Ordering::SeqCst) {
                    debug!(generation, "skipping a superseded prepare");
                    return;
                }
                // External subtitle inputs are per-item side inputs on the
                // live core. A swap would carry them across: decodebin3
                // keeps their streams in the (combined) collections it
                // posts for the NEXT item, which corrupts the held-back
                // collection and the per-item selection state. Refuse; the
                // caller's ordinary end-of-stream advance owns that
                // transition.
                if inner
                    .routing
                    .lock()
                    .inputs
                    .iter()
                    .any(|i| i.external.is_some())
                {
                    debug!(
                        generation,
                        "external subtitles attached; refusing the gapless prepare"
                    );
                    inner.emit(PlaybinEvent::PreparedFailed { generation });
                    return;
                }
                // Latest wins: replace a still-pending previous prepare. A
                // swap already PERFORMED cannot unwind: its activation is
                // imminent, and arming over it would hand the activation
                // this prepare's record while the pipeline plays the other
                // item. Refuse instead.
                if matches!(self.cancel_prepared(), CancelOutcome::Declined { .. }) {
                    debug!(
                        generation,
                        "a performed swap is activating; refusing the prepare"
                    );
                    inner.emit(PlaybinEvent::PreparedFailed { generation });
                    return;
                }

                // The current item is in steady playback, so every routed
                // pad's sticky stream-start is present: snapshot the group
                // state now. Activation detection (a group CHANGE at the
                // output) and the old-item EOS drop both depend on the
                // current group being positively known before the switch.
                Inner::refresh_output_groups(inner);

                // Arm the swap gate BEFORE the input exists: from here on
                // any EOS at the decodebin3 outputs is held back, so a
                // drain racing the prepare cannot leak to the sinks. The
                // INPUT side commonly drained long ago (a small or resident
                // file is swallowed whole by the multiqueue at load), which
                // is fine: the swap proceeds immediately and the OUTPUT
                // side still paces the actual switch. Only an output EOS
                // that fully escaped before this point misses the handoff,
                // and then the caller's ordinary end-of-stream advance owns
                // the transition.
                *inner.swap_gate.state.lock() = SwapState {
                    pending: Some(generation),
                    drained: Inner::main_input_drained(inner),
                    swapped: false,
                    dropped_eos: false,
                };

                let built = match input {
                    MediaInput::Uri(uri) => Inner::make_urisourcebin(&uri, true),
                    MediaInput::Element(element) => Ok(element),
                };
                let attached = built.and_then(|element| {
                    Inner::add_prepared_input(inner, element.clone(), generation).map(|_| element)
                });
                match attached {
                    Ok(element) => {
                        debug!(generation, "prepared the next input (blocked, unlinked)");
                        *inner.prepared.lock() = Some(PreparedNext {
                            element,
                            generation,
                            pending_collection: None,
                        });
                    }
                    Err(err) => {
                        error!(?err, generation, "failed to prepare the next input");
                        // Disarm what was armed above.
                        let aborted = inner.swap_gate.abort();
                        // A prepare failing WHILE the pipeline transitions
                        // (its error message aborts a bin's in-flight
                        // async commit) must not strand playback below the
                        // caller's target: re-commit the transition.
                        self.recommit_pipeline_state();
                        inner.emit(PlaybinEvent::PreparedFailed { generation });
                        // The hold may have consumed the current item's
                        // end between the arm and this failure (see
                        // `SwapState::dropped_eos` and `cancel_prepared`).
                        if aborted.pending.is_some() && aborted.dropped_eos {
                            debug!("the item's end was consumed while arming: synthesizing it");
                            inner.emit(PlaybinEvent::EndOfStream);
                        }
                    }
                }
            }
            Job::CancelPrepared { notify } => {
                let outcome = self.cancel_prepared();
                if notify {
                    inner.emit(match outcome {
                        CancelOutcome::Cancelled { generation } => {
                            PlaybinEvent::PreparedCancelled { generation }
                        }
                        CancelOutcome::Declined { generation } => {
                            PlaybinEvent::PreparedCancelDeclined { generation }
                        }
                    });
                }
            }
            Job::FinishActivation => {
                // The prepared item is live (its generation is current):
                // every older input is drained history. This removes the
                // previous item's main input and its external subtitles.
                let current = inner.current_generation();
                let old: Vec<Input> = {
                    let mut routing = inner.routing.lock();
                    let (old, keep) = routing
                        .inputs
                        .drain(..)
                        .partition(|input| input.generation < current);
                    routing.inputs = keep;
                    old
                };
                for input in old {
                    debug!(
                        generation = input.generation,
                        "removing a drained input after the gapless activation"
                    );
                    Inner::remove_input(inner, input);
                }
            }
            Job::SyncTextRunningTime => inner.sync_text_running_time(),
            Job::DrainTextWork => {
                // Diagnostic only. The busy-loop regression test counts the
                // drains the worker actually received.
                inner.drain_jobs_seen.fetch_add(1, Ordering::SeqCst);
                Inner::run_deferred_text_work(inner)
            }
            Job::VideoChainGone => {
                // Text is consumed synchronized against VIDEO buffers, so a
                // text stream left in the overlay after video stops can never
                // drain and blocks decodebin3's reconfiguration until the next
                // flush. Park it, and the policy brings it back once a video
                // stream routes again.
                Inner::park_text_streams(inner);
                // The video pad is gone for good (a mid-item deselect, an
                // input teardown), so take the chain out of the pipeline.
                // Nothing can then aggregate over, or later lift, a sink that
                // will never see data again. A re-select routes a fresh pad
                // and rebuilds.
                //
                // Re-checked here rather than trusted from the posting side:
                // between the pad-removed callback and this job a new video
                // stream may already have routed, and tearing the chain down
                // under it would strand the item with no video at all.
                // LINKED video only. A parked video entry (the resurrected
                // pad of a deselected stream) is not a reason to keep the
                // chain, it is the very thing the deselect is waiting out.
                // Inert before the resurrect park existed, since every
                // routed video entry was linked by construction.
                let video_routed = inner
                    .routing
                    .lock()
                    .routed
                    .iter()
                    .any(|r| r.kind == StreamKind::Video && r.downstream.is_some());
                if video_routed {
                    debug!("a video stream routed again before the chain teardown ran; keeping it");
                } else {
                    inner.remove_video_chain();
                }
            }
            Job::ClearStateFailure => {
                // `Err(_)` here IS `GST_STATE_RETURN == FAILURE`, so a
                // pipeline that can still commit is a no-op.
                let (before, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
                if before.is_ok() {
                    return;
                }
                // Below PAUSED nothing can be stranded, and the next load or
                // teardown clears the latch itself. Staying out keeps this off
                // downward transitions.
                if current < gst::State::Paused {
                    debug!(?current, "leaving the latch to the next load or teardown");
                    return;
                }
                // `pending == current` non-void is what `bin_handle_async_start`
                // writes on a lost state. Re-committing it leaves
                // `old_state == old_next`, so GStreamer posts nothing and the
                // report below is the caller's only announcement.
                let lost_state = pending != gst::State::VoidPending && pending == current;
                // `bin_handle_async_done` owed PENDING, not current. Mid-climb
                // the two differ (an error during a PAUSED->PLAYING commit reads
                // `(Paused, Playing)`), and re-committing `current` there
                // cancels the climb and announces nothing. A void pending means
                // the bin had arrived, and then `current` is what it arrived at.
                // Lever: `FCAST_UNLATCH_RECOMMIT_CURRENT`.
                let owed = if pending == gst::State::VoidPending
                    || std::env::var_os("FCAST_UNLATCH_RECOMMIT_CURRENT").is_some()
                {
                    current
                } else {
                    pending
                };
                warn!(
                    ?current,
                    ?pending,
                    ?owed,
                    lost_state,
                    "the pipeline latched a state-change failure from an error the \
                     crate consumed; re-committing so it can settle again"
                );
                // A pending BELOW paused is a teardown descending (a stop, a
                // load's reset). It clears the latch with its own `set_state`
                // and asserting anything here would fight it, which is this
                // crate's most expensive kind of mistake.
                if owed < gst::State::Paused {
                    debug!(?owed, "leaving the latch to the descending transition");
                    return;
                }
                // Re-commit the state the bin had already decided on and was
                // refused. The TARGET is NOT read back (GStreamer does not
                // expose it) and does not need to be: the caller's state
                // machine owns the transport target and re-asserts it from the
                // edge reported below, which is the same contract every other
                // correction here uses.
                let _ = inner.pipeline.set_state(owed);
                if !lost_state {
                    // A re-committed climb announces itself (`old_state !=
                    // old_next`), so nothing to report.
                    //
                    // UNCOVERED, deliberately: a latch caught with
                    // `pending == VoidPending`. Nothing is owed
                    // (`bin_handle_async_done` takes `nothing_pending` either
                    // way) and no reproducer was ever built. If one turns up,
                    // the predicate is `Job::SetState`'s. It is not applied
                    // here because this job has no target to compare against,
                    // so it would report on every consumed error against a
                    // settled pipeline.
                    return;
                }
                // Unannounced settle: re-committing the pipeline's own state
                // leaves `old_state == old_next`, which both of GStreamer's
                // "don't post silly messages with the same state" guards refuse
                // to post on, so this cannot duplicate a real edge. Not
                // conditioned on reading settled afterwards (measured on 400009
                // the re-commit returns with `pending` still set).
                debug!(
                    ?current,
                    "reporting the settle the latched pipeline could not post"
                );
                inner.emit(PlaybinEvent::StateChanged {
                    old: current,
                    current,
                    pending: gst::State::VoidPending,
                });
            }
        }
    }

    /// Drop a still-pending prepared next input: take it out of the
    /// prepared slot and remove its element from the pipeline. A no-op
    /// when nothing is prepared (or it already activated, which empties the
    /// slot). Worker-thread only (pipeline surgery).
    ///
    /// Returns [`CancelOutcome::Declined`] when a performed swap makes
    /// cancellation impossible: activation is imminent and must be left to
    /// finish (a caller-side load supersedes it normally anyway). Callers
    /// arming a NEW prepare must refuse on it rather than clobber the
    /// in-flight activation.
    fn cancel_prepared(&self) -> CancelOutcome {
        // Atomic against the block probe's surgery: past the swap the
        // relink is live and activation is imminent, cancelling would rip
        // the now-active input out mid-stream. `pending` distinguishes the
        // in-flight window from a long-completed activation (which leaves
        // `swapped` set but clears `pending`).
        let dropped_eos = {
            let mut state = self.inner.swap_gate.state.lock();
            if let Some(generation) = state.activation_pending() {
                debug!("swap already performed, leaving the activation to finish");
                return CancelOutcome::Declined { generation };
            }
            let aborted = std::mem::take(&mut *state);
            self.inner.swap_gate.cond.notify_all();
            aborted.pending.is_some() && aborted.dropped_eos
        };
        let mut cancelled = None;
        if let Some(prepared) = self.inner.prepared.lock().take() {
            debug!(
                generation = prepared.generation,
                "dropping the prepared next input"
            );
            cancelled = Some(prepared.generation);
            let input = {
                let mut routing = self.inner.routing.lock();
                routing
                    .inputs
                    .iter()
                    .position(|i| i.element == prepared.element)
                    .map(|idx| routing.inputs.remove(idx))
            };
            if let Some(input) = input {
                Inner::remove_input(&self.inner, input);
            }
            // A prepared input dying mid-transition can abort the
            // pipeline's in-flight commit (see Job::PrepareNext's failure
            // arm): re-assert it. A no-op when nothing was disturbed.
            self.recommit_pipeline_state();
        }
        // The hold DROPPED an output EOS while this swap was pending: that
        // end is gone for good and nothing else will surface it (a partial
        // drop even blocks the bin's EOS aggregation forever). Synthesize
        // it now; a duplicate (an EOS on a pad the hold had not covered
        // yet) is dropped by the caller's generation guard after it
        // advances. An EOS that only reached the INPUT side keeps flowing
        // normally once the hold disarms, so no synthesis there: ending
        // the item early would cut its buffered tail (and turn a seek-back
        // near the end into a skip to the next item).
        if dropped_eos {
            debug!("prepare cancelled after its end was consumed: synthesizing end-of-stream");
            self.inner.emit(PlaybinEvent::EndOfStream);
        }
        CancelOutcome::Cancelled {
            generation: cancelled,
        }
    }

    /// Re-assert the pipeline's in-flight target after a prepared input's
    /// failure may have aborted an async commit. Worker-thread only.
    fn recommit_pipeline_state(&self) {
        let current = self.inner.pipeline.current_state();
        let pending = self.inner.pipeline.pending_state();
        let target = if pending != gst::State::VoidPending {
            pending
        } else {
            current
        };
        debug!(
            ?current,
            ?pending,
            ?target,
            "re-committing the pipeline state"
        );
        if target > gst::State::Ready {
            let _ = self.inner.pipeline.set_state(target);
        }
    }

    /// Take an external input out of the routing table iff it still exists
    /// under the epoch the job was decided against. A `None` means the job
    /// is stale: the input was detached, replaced by a load, or re-armed
    /// since, and the current holder of the id must be left alone.
    fn take_external_input(&self, id: ExternalSubId, epoch: u32) -> Option<Input> {
        let mut routing = self.inner.routing.lock();
        let idx = routing.inputs.iter().position(|i| {
            i.external
                .as_ref()
                .is_some_and(|e| e.id == id && e.epoch == epoch)
        })?;
        Some(routing.inputs.remove(idx))
    }

    /// Worker side of a genuine external failure: detach the input and
    /// report it (see [`PlaybinEvent::ExternalSubtitleFailed`]).
    fn fail_subtitle(&self, id: ExternalSubId, epoch: u32) {
        let Some(input) = self.take_external_input(id, epoch) else {
            debug!(?id, epoch, "stale subtitle fail job; input already gone");
            return;
        };
        Inner::remove_input(&self.inner, input);
        // A selection desire parked on this input must not park forever.
        self.inner.selection.lock().external_gone(id);
        warn!(?id, "external subtitle input failed; detached");
        self.inner.emit(PlaybinEvent::ExternalSubtitleFailed { id });
    }

    /// Worker side of the never-linked attach retry: an input killed
    /// BEFORE anything of it reached decodebin3 has nothing to select, so
    /// the join-time replay can never run and the watchdog would eat the
    /// user's subtitle. Replacing the element is safe exactly here,
    /// unlike the removed general re-arm: no pad locks for the NULL to
    /// deadlock on, no collection presence to churn. Epoch-capped by the
    /// caller ([`MAX_ATTACH_RETRIES`]); a genuinely bad URL exhausts the
    /// retries and the watchdog delivers the verdict.
    fn retry_subtitle(&self, id: ExternalSubId, epoch: u32) {
        // BEFORE the routing lock below: the check takes it too.
        let slotless = self.inner.external_stream_slotless(id, epoch);
        {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                debug!(?id, epoch, "stale subtitle retry; input already gone");
                return;
            };
            // Linked after all (a pad appeared between the error and this
            // job): the join-time replay owns recovery, and a replacement
            // would reintroduce the hazards the retry exists to avoid.
            //
            // A SLOTLESS stream is the exception: it is linked and advertised
            // and still cannot render, and only a fresh input gets a slot back
            // (see `Inner::external_stream_slotless`). The replay hands those
            // over here deliberately.
            if (!input.stream_ids().is_empty() || !input.db3_sink_pads.is_empty()) && !slotless {
                debug!(
                    ?id,
                    epoch, "input reached decodebin3 after all; leaving it be"
                );
                return;
            }
        }
        let Some(input) = self.take_external_input(id, epoch) else {
            return;
        };
        let uri = input.external.as_ref().expect("external input").uri.clone();
        Inner::remove_input(&self.inner, input);

        let attach = Inner::make_urisourcebin(&uri, false).and_then(|element| {
            Inner::add_input(
                &self.inner,
                element,
                self.inner.current_generation(),
                Some(ExternalInput {
                    id,
                    uri: uri.clone(),
                    epoch: epoch + 1,
                    hold_until_selected: true,
                    hold_release_owed: false,
                    task_dead: false,
                    last_origin: gst::ClockTime::ZERO,
                }),
            )
        });
        match attach {
            Ok(()) => {
                info!(
                    ?id,
                    uri,
                    epoch = epoch + 1,
                    "retried the external subtitle attach"
                );
                self.arm_sub_watchdog(id, epoch + 1);
            }
            Err(err) => {
                error!(?err, ?id, uri, "the attach retry failed");
                self.inner.emit(PlaybinEvent::ExternalSubtitleFailed { id });
            }
        }
    }

    /// Worker side of the join-time replay: a flushing seek into the
    /// input's own source pads (a pipeline seek never reaches side
    /// inputs). The flush resets any slot queue state a previous drain
    /// left FLUSHING, restarts a task the deselect race killed, and the
    /// source re-pushes from the target, exactly like a fresh attach: past
    /// cues fall to sync, the current one shows. Epoch-guarded like the
    /// other subtitle jobs.
    ///
    /// The target is the overlay's running-time ORIGIN
    /// ([`Inner::overlay_timeline`]), not zero and not the current
    /// position. Zero replays the whole file shifted by the origin (the
    /// field bug: after any seek, a re-enable restarted the subtitles from
    /// cue one), and the current position would map that position's cue to
    /// running time zero, rendering everything later early. Only the origin
    /// gives the branch the same stream-time-to-running-time mapping the
    /// video has, which is what makes past cues late and droppable and the
    /// current one on time. Seeking the input, not the pipeline, so an
    /// unseekable main source is untouched.
    fn replay_subtitle(&self, id: ExternalSubId, epoch: u32, attempt: u32) {
        let pads: Vec<gst::Pad> = {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                debug!(?id, epoch, "stale subtitle replay job; input already gone");
                return;
            };
            input.element.src_pads()
        };
        // A replay CANNOT fix a stream decodebin3 has no slot for, and a
        // drained external is exactly that (see
        // `Inner::external_stream_slotless`). Re-attaching is the only way to
        // get a slot back, so hand over to the retry instead of pushing a seek
        // into a pad whose multiqueue peer is gone (which kills the source with
        // "reason not-linked" and, four attempts later, gave up in silence).
        // Lever: `FCAST_NO_SLOTLESS_EXTERNAL_REATTACH`.
        if std::env::var_os("FCAST_NO_SLOTLESS_EXTERNAL_REATTACH").is_none()
            && self.inner.external_stream_slotless(id, epoch)
        {
            warn!(
                ?id,
                epoch, attempt, "the external's stream has no decodebin3 slot; re-attaching it"
            );
            let _ = self.inner.work_tx.send(Job::RetrySub { id, epoch });
            return;
        }
        let (rate, origin) = self.inner.overlay_timeline();
        {
            let mut routing = self.inner.routing.lock();
            if let Some(external) = routing
                .inputs
                .iter_mut()
                .filter_map(|input| input.external.as_mut())
                .find(|external| external.id == id && external.epoch == epoch)
            {
                external.last_origin = origin;
                // The replay seek restarts the source task.
                external.task_dead = false;
            }
        }
        let seek = gst::event::Seek::builder(
            rate,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            origin,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        )
        .build();
        // HANDED OFF, never sent from here. The source performs a flushing
        // seek inline on the sending thread and pushes its FLUSH_START down
        // the live graph from there, where a `queue` parked behind a
        // prerolling sink blocks the sender in `gst_pad_pause_task`
        // indefinitely. Only PLAYING ends that preroll, only `Job::SetState`
        // reaches PLAYING, and only this worker runs jobs, so sending here
        // makes the worker wait on itself. See [`ReplayJob`], and
        // `fuzz_buffering` seed 600055 (iters 4, actions 22) for the deterministic
        // reproducer, which fails 3 of 3 with the seek sent from here.
        //
        // The lever sends inline on the worker again, which is the whole of
        // the change. Both arms run the identical
        // `FcastPlaybin::run_replay_seek`, so nothing else can differ
        // between them.
        let job = ReplayJob {
            pads,
            seek,
            id,
            epoch,
            attempt,
            origin,
            rate,
        };
        if std::env::var_os("FCAST_INLINE_REPLAY_SEEK").is_some() {
            Self::run_replay_seek(&self.inner, job);
        } else if self.inner.replay_tx.send(job).is_err() {
            warn!(?id, "the replay sender is gone, dropping the replay seek");
        }
    }

    /// Send one replay's flushing seek and act on the outcome. Runs on the
    /// replay sender thread (see [`ReplayJob`]), where BLOCKING IS ALLOWED.
    /// Nothing else waits behind that thread, so a seek parked on a queue's
    /// stream lock costs only this replay's latency and completes as soon as
    /// the pipeline flows again.
    fn run_replay_seek(inner: &Arc<Inner>, job: ReplayJob) {
        let ReplayJob {
            pads,
            seek,
            id,
            epoch,
            attempt,
            origin,
            rate,
        } = job;
        let mut accepted = 0usize;
        for pad in &pads {
            info!(pad = %pad.name(), ?id, ?origin, rate, attempt, "replaying the spent external subtitle input");
            if pad.send_event(seek.clone()) {
                accepted += 1;
            } else {
                warn!(pad = %pad.name(), ?id, "the external input refused the replay seek");
            }
        }
        // The source performs a flushing seek INLINE on this thread, so by
        // here its segment is the realigned one and a hold this replay owes
        // can finally come off. Before the loop it could not: the whole point
        // is that nothing may escape the input until the seek has landed. On
        // every outcome, including the refusal below (see the helper).
        Inner::release_owed_hold(inner, id, epoch);
        // Not one pad took it. A pipeline at rest in PAUSED refuses a flushing
        // seek on every pad, every push logging `Failed to push event ...
        // state="paused"`, and the verification then correctly saw the stream
        // still unaligned and replayed again. Four rounds of work that could
        // not succeed by construction. Owe it to the moment the pipeline can
        // carry it instead, and do NOT arm a check that would only rediscover
        // the same thing.
        //
        // Decided from the OUTCOME rather than from the pipeline state: a
        // state check also matched a pipeline transiently at rest during a
        // seek, where the seek IS accepted, and postponing there left the
        // input unaligned for good.
        if accepted == 0 && !pads.is_empty() {
            // A new postponed item invalidates the last drain's no-op
            // verdict (see `Inner::drain_poke_parked`).
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
            let mut owed = inner.deferred_replays.lock();
            if !owed
                .iter()
                .any(|(oid, oepoch, _)| *oid == id && *oepoch == epoch)
            {
                debug!(?id, epoch, attempt, "postponing a replay the pipeline refused");
                owed.push((id, epoch, attempt));
            }
            return;
        }
        // A replay can race the very slot swap that requested it (the
        // re-delivery drains into a slot decodebin3 is still relinking), so
        // check back once: a bounded sleeper, exactly like the sub watchdog.
        if attempt < REPLAY_ATTEMPTS {
            inner.arm_replay_verification(id, epoch, attempt);
            return;
        }
        // Out of attempts. This used to end HERE in silence, which left the
        // receiver reporting a subtitle track that renders nothing (in
        // upstream-selection mode the crate's own merged report has already
        // named this sid, so the caller believes the switch worked).
        //
        // It must NOT end in a detach either, which is what the first version
        // of this escalation did and what the field then punished: exhaustion
        // on an input that materialized and is DELIVERING into decodebin3 means
        // "its branch has not joined subtitleoverlay yet", not "the file is
        // bad". The join runs from `poll_text_policy` on the CALLER's cadence
        // while these attempts run on a 400ms worker timer, so a slow caller
        // loses the race and a perfectly servable external got detached and
        // dropped from the user's track list (`ResourceNotFound` at the
        // sender).
        //
        // So: only the case nothing can serve is failed, i.e. a stream with no
        // carrier pad at all AFTER a re-attach already had its chance
        // (`external_stream_slotless` hands the first occurrence to
        // `Job::RetrySub`, which bumps the epoch). Anything with a carrier is
        // left ATTACHED and merely reported loudly: the branch can still join
        // on any later poll, join-time replay included, and the next selection
        // finds a live input instead of a missing track.
        // Lever: `FCAST_NO_REPLAY_GIVEUP_ESCALATION` (set = the original
        // silent give-up, no warning and no failure either way).
        if std::env::var_os("FCAST_NO_REPLAY_GIVEUP_ESCALATION").is_some() {
            return;
        }
        if inner.external_stream_slotless(id, epoch) && epoch > 0 {
            warn!(
                ?id,
                epoch, attempt, "a re-attached external still has no decodebin3 slot; failing it"
            );
            let _ = inner.work_tx.send(Job::FailSub { id, epoch });
            return;
        }
        // Loud, but not fatal: the input stays attached and servable.
        warn!(
            ?id,
            epoch,
            attempt,
            "the external subtitle has not rendered after every replay attempt; \
             leaving it attached for the next join"
        );
        // The join is what is missing, so poke the thing that performs it
        // rather than waiting for the caller's next settle point.
        Inner::poll_text_policy(inner);
    }


    /// Worker side of the replay verification: the replay took iff the
    /// input's stream reached its decodebin3 OUTPUT pad (the sticky
    /// STREAM_START names it, pad reuse included). If it has not, the
    /// re-delivery was eaten by the racing reconfiguration: replay again,
    /// bounded.
    ///
    /// A verdict is only ever reached at a pipeline settled at PLAYING.
    /// Anywhere below, the check holds itself into
    /// [`Inner::deferred_verifications`] and the deferred-work drain re-arms
    /// it later, because a pipeline that is not flowing leaves the stickies
    /// exactly as the input's previous tenure left them and they prove
    /// nothing about this one.
    fn verify_replay(&self, id: ExternalSubId, epoch: u32, attempt: u32) {
        // The chain this check belongs to has now run, so the next legitimate
        // arming is allowed. See `arm_replay_verification`.
        self.inner.replay_checks_armed.lock().remove(&(id, epoch));
        // A verdict needs evidence, and a pipeline below a settled PLAYING
        // has none. Nothing flows there, so the stickies read below are
        // leftovers of the input's previous tenure, and a spent input that
        // will never push another buffer still passes as aligned delivery.
        // Concluding here is what left the field's overlay linked to a dead
        // input after rapid switches at a pipeline parked in Buffering. Hold
        // the verdict instead. The deferred-work drain re-arms it once the
        // pipeline is settled at PLAYING, where the postponed flush has run
        // and delivery is observable. Checked before the routing lock so the
        // state read never nests inside it. The lever restores the old
        // conclude-anywhere behavior for interleaved A/B measurement and
        // gates this whole change (the hold here, the drain of held
        // verdicts, and the drain's selection re-verification).
        if std::env::var_os("FCAST_NO_REPLAY_VERDICT_DEFERRAL").is_none() {
            let (_, current, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
            if current != gst::State::Playing || pending != gst::State::VoidPending {
                // A new postponed item invalidates the last drain's no-op
                // verdict (see `Inner::drain_poke_parked`).
                self.inner.drain_poke_parked.store(false, Ordering::SeqCst);
                let mut held = self.inner.deferred_verifications.lock();
                if !held
                    .iter()
                    .any(|(hid, hepoch, _)| *hid == id && *hepoch == epoch)
                {
                    debug!(
                        ?id,
                        epoch, attempt, "holding a replay verdict below a settled PLAYING"
                    );
                    held.push((id, epoch, attempt));
                }
                return;
            }
        }
        let delivered = {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                return;
            };
            let sids = input.stream_ids();
            // Only meaningful while this input's stream is still the
            // SELECTED one: a selection that moved on owns its own replay.
            let still_selected = self
                .inner
                .selection
                .lock()
                .subtitle_sid()
                .is_some_and(|sid| sids.contains(&sid));
            if !still_selected {
                debug!(?id, attempt, ?sids, "replay check: selection moved on; not replaying");
                return;
            }
            routing.routed.iter().any(|routed| {
                routed.kind == StreamKind::Text
                    && routed.downstream.is_some()
                    && routed
                        .db3_src_pad
                        .sticky_event::<gst::event::StreamStart>(0)
                        .is_some_and(|event| sids.iter().any(|sid| *sid == event.stream_id()))
            })
        };
        // Delivered is not enough: an input that joined the branch WITHOUT a
        // replay carries its own file-origin segment, and its cues render
        // shifted whenever the video's origin moved (a started-at or sought
        // item). Only aligned delivery needs no replay.
        let aligned = delivered && {
            let (_, video_origin) = self.inner.overlay_timeline();
            let text_origin = self
                .inner
                .overlay
                .static_pad("subtitle_sink")
                .and_then(|pad| pad.sticky_event::<gst::event::Segment>(0))
                .and_then(|event| {
                    let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
                    let rate = segment.rate();
                    let start = segment.start().unwrap_or(gst::ClockTime::ZERO);
                    let base = (segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds()
                        as f64
                        * rate.abs()) as u64;
                    Some(gst::ClockTime::from_nseconds(
                        start.nseconds().saturating_sub(base),
                    ))
                });
            text_origin == Some(video_origin)
        };
        if aligned {
            return;
        }
        debug!(
            ?id,
            attempt, delivered, "the switched-to stream is not rendering aligned; replaying"
        );
        self.replay_subtitle(id, epoch, attempt + 1);
    }

    /// Worker side of the materialization watchdog: an input still without
    /// streams when its check fires never worked (a bad URL can die without
    /// a bus error). Epoch-guarded like the other subtitle jobs.
    fn check_subtitle(&self, id: ExternalSubId, epoch: u32) {
        let materialized = {
            let routing = self.inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                // Detached or re-armed since this check was armed (the
                // re-arm brought its own watchdog).
                return;
            };
            // An external that produced only audio or video has NOT
            // materialized as a subtitle, however healthy it looks. Asking
            // `stream_ids` here let an audio file handed in as "the subtitle"
            // pass the watchdog, and its audio stream was then advertised to
            // the caller as a subtitle track.
            //
            // An input that has not classified its streams yet is left alone.
            // Only a positively known absence of text counts as a failure.
            !input.text_stream_ids().is_empty() || input.has_unclassified_stream()
        };
        if materialized {
            return;
        }
        warn!(
            ?id,
            "external subtitle produced no text stream within the timeout"
        );
        self.fail_subtitle(id, epoch);
    }
}

impl Inner {
    /// urisourcebin configured the way uridecodebin3 configures its source
    /// handlers: parsed streams out. `use_buffering` (main input only)
    /// matches playbin3's `buffering` flag, whose messages drive the
    /// caller's state machine.
    fn make_urisourcebin(uri: &str, use_buffering: bool) -> Result<gst::Element> {
        let usb = gst::ElementFactory::make("urisourcebin")
            .property("uri", uri)
            .property("parse-streams", true)
            .property("use-buffering", use_buffering)
            .build()
            .context("creating urisourcebin")?;
        Ok(usb)
    }

    /// Add an input element to the pipeline and link its (dynamic) source
    /// pads into decodebin3 request pads as they appear.
    fn add_input(
        inner: &Arc<Inner>,
        element: gst::Element,
        generation: u64,
        external: Option<ExternalInput>,
    ) -> Result<()> {
        // Externals attach STATE-LOCKED and only join the pipeline's state
        // machinery once materialized (Job::AdoptSubState): a pipeline state
        // change that recurses into a still-plugging input deadlocks against
        // typefind's streaming thread (state lock held while pausing the
        // task, the task holding its stream lock while syncing the plugged
        // parser's state).
        let lock_until_materialized = external.is_some();
        if lock_until_materialized {
            element.set_locked_state(true);
        }
        inner
            .pipeline
            .add(&element)
            .context("adding input element")?;

        // Register the input BEFORE any pad can appear, so `link_input_pad`
        // always finds it for request-pad bookkeeping (detach releases those
        // pads later).
        inner.routing.lock().inputs.push(Input {
            element: element.clone(),
            generation,
            external,
            db3_sink_pads: Vec::new(),
            taps: Vec::new(),
            pad_added_sig: None,
            block_probes: Vec::new(),
        });

        let pad_added_sig = element.connect_pad_added({
            let inner = Arc::downgrade(inner);
            move |element, pad| {
                let Some(inner) = inner.upgrade() else { return };
                if let Err(err) = Inner::link_input_pad(&inner, element, pad) {
                    warn!(?err, pad = %pad.name(), "failed to link input pad to decodebin3");
                }
            }
        });
        {
            let mut routing = inner.routing.lock();
            if let Some(input) = routing.inputs.iter_mut().find(|i| i.element == element) {
                input.pad_added_sig = Some(pad_added_sig);
            }
        }

        // Pads that already exist (pre-built elements may have static pads).
        for pad in element.src_pads() {
            if let Err(err) = Inner::link_input_pad(inner, &element, &pad) {
                warn!(?err, pad = %pad.name(), "failed to link existing input pad");
            }
        }

        let synced = if lock_until_materialized {
            // Locked children are skipped by the parent's state changes, so
            // drive the input to the pipeline's effective state explicitly.
            // ASYNC is the normal answer while its internals build.
            element
                .set_state(inner.join_state())
                .map(|_| ())
                .map_err(|err| anyhow!("driving the locked external input: {err}"))
        } else {
            element
                .sync_state_with_parent()
                .map_err(|err| anyhow!("{err}"))
        };
        if let Err(err) = synced {
            // Roll back: a half-attached input would keep posting errors
            // from inside the pipeline with nothing owning it.
            let mut routing = inner.routing.lock();
            if let Some(idx) = routing.inputs.iter().position(|i| i.element == element) {
                let input = routing.inputs.remove(idx);
                drop(routing);
                Inner::remove_input(inner, input);
            }
            return Err(err).context("syncing input element state");
        }
        Ok(())
    }

    /// The uridecodebin3 `link_src_pad_to_db3` recipe: request a decodebin3
    /// sink pad and link.
    fn link_input_pad(inner: &Arc<Inner>, element: &gst::Element, pad: &gst::Pad) -> Result<()> {
        let db3 = inner
            .core
            .lock()
            .as_ref()
            .map(|c| c.db3.clone())
            .ok_or_else(|| anyhow!("no dynamic core"))?;
        // A held external's buffers must never reach decodebin3 while its
        // stream is deselected (see `ExternalInput::hold_until_selected`).
        // Installed BEFORE the link so no buffer can slip through: this runs
        // on the element's streaming thread ahead of any push through `pad`.
        Inner::block_held_external_pad(inner, element, pad);
        let sinkpad = {
            // Serialized: see `Inner::db3_pad_request`.
            let _serial = inner.db3_pad_request.lock();
            db3.request_pad_simple("sink_%u")
        }
        .ok_or_else(|| anyhow!("decodebin3 gave no request sink pad"))?;
        pad.link(&sinkpad)
            .with_context(|| format!("linking {} to {}", pad.name(), sinkpad.name()))?;
        debug!(src = %pad.name(), sink = %sinkpad.name(), "linked input pad into decodebin3");

        // Record input-side stream ends for the drained-resurrect park (see
        // `Inner::input_eos_sids`). EOS marks the stream drained, FLUSH_STOP
        // marks it restarted. A seek only flushes the streams it actually
        // restarts (a per-stream source leaves a deselected stream's pad
        // untouched), so the flag self-maintains across seeks. Installed
        // only while the park's lever is unset, so the lever restores the
        // untracked behavior wholesale.
        if std::env::var_os("FCAST_NO_DRAINED_RESURRECT_PARK").is_none() {
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
                let weak = Arc::downgrade(inner);
                move |pad, info| {
                    let Some(gst::PadProbeData::Event(event)) = &info.data else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let drained = match event.view() {
                        gst::EventView::Eos(_) => true,
                        gst::EventView::FlushStop(_) => false,
                        _ => return gst::PadProbeReturn::Ok,
                    };
                    let Some(inner) = weak.upgrade() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let Some(sid) = pad
                        .sticky_event::<gst::event::StreamStart>(0)
                        .map(|event| event.stream_id().to_string())
                    else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let mut drained_sids = inner.input_eos_sids.lock();
                    if drained {
                        debug!(%sid, "input stream drained (EOS into decodebin3)");
                        drained_sids.insert(sid);
                    } else {
                        debug!(%sid, "input stream restarted by a flush");
                        drained_sids.remove(&sid);
                    }
                    gst::PadProbeReturn::Ok
                }
            });
        }

        if !Inner::record_linked_input_pad(inner, element, pad, sinkpad.clone()) {
            // Only reachable for an input already removed (detach racing a
            // late pad). Release the pad we just took.
            warn!("pad appeared for an unregistered input; releasing");
            db3.release_request_pad(&sinkpad);
        }
        // The FIRST linked pad of an external input means its plugging
        // machinery is done: hand the state-locked input over to the
        // pipeline's state handling (see Job::AdoptSubState).
        let adopt = {
            let routing = inner.routing.lock();
            routing.inputs.iter().find(|i| i.element == *element).and_then(|input| {
                let external = input.external.as_ref()?;
                (input.db3_sink_pads.len() == 1).then_some((external.id, external.epoch))
            })
        };
        if let Some((id, epoch)) = adopt {
            let _ = inner.work_tx.send(Job::AdoptSubState { id, epoch });
        }
        Ok(())
    }

    /// Install the hold-until-selected block on one source pad of a held
    /// external input (see [`ExternalInput::hold_until_selected`]).
    /// Serialized events pass, so the stream's sticky events reach
    /// decodebin3 and it stays advertised; buffers hold until
    /// [`Inner::unblock_selected_externals`] removes the probe. A no-op for
    /// every other input.
    ///
    /// Events alone leave the stream advertised but NOT selectable, so the
    /// first held buffer also seeds one GAP (see
    /// [`Inner::seed_slot_for_held_pad`]).
    fn block_held_external_pad(inner: &Arc<Inner>, element: &gst::Element, pad: &gst::Pad) {
        {
            let routing = inner.routing.lock();
            let held = routing.inputs.iter().any(|i| {
                i.element == *element && i.external.as_ref().is_some_and(|e| e.hold_until_selected)
            });
            if !held {
                return;
            }
        }
        let seeded = AtomicBool::new(false);
        let probe = pad.add_probe(
            gst::PadProbeType::BLOCK
                | gst::PadProbeType::BUFFER
                | gst::PadProbeType::BUFFER_LIST
                | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |pad, info| {
                // Every event passes, GAP included: decodebin3 needs one to
                // give the stream a multiqueue slot, and the stream must be
                // slotted to be selectable at all.
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    // A cue-less subtitle reaches EOS without ever pushing a
                    // buffer. Seed off its EOS instead, BEFORE forwarding it:
                    // nothing may be pushed afterwards, and an unslotted
                    // stream would stay unselectable for good.
                    if event.type_() == gst::EventType::Eos && !seeded.swap(true, Ordering::Relaxed)
                    {
                        Inner::seed_slot_for_held_pad(pad, None);
                    }
                    return gst::PadProbeReturn::Pass;
                }
                // A buffer means the sticky events have reached decodebin3
                // (`check_sticky` runs ahead of block probes) and this is the
                // input's streaming thread, parked here for as long as the
                // hold lasts: the one safe moment to seed the slot.
                if !seeded.swap(true, Ordering::Relaxed) {
                    let pts = match &info.data {
                        Some(gst::PadProbeData::Buffer(buffer)) => buffer.pts(),
                        Some(gst::PadProbeData::BufferList(list)) => {
                            list.get(0).and_then(|b| b.pts())
                        }
                        _ => None,
                    };
                    Inner::seed_slot_for_held_pad(pad, pts);
                }
                gst::PadProbeReturn::Ok
            },
        );
        let Some(probe) = probe else { return };
        debug!(pad = %pad.name(), "holding an external input's data until selected");
        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.block_probes.push((pad.clone(), probe));
        } else {
            drop(routing);
            pad.remove_probe(probe);
        }
    }

    /// Give a held external input's stream a decodebin3 multiqueue slot by
    /// pushing one GAP down the (blocked) pad.
    ///
    /// Without this a held stream is advertised but permanently
    /// UNSELECTABLE, and that deadlocks the hold. decodebin3 links an input
    /// stream to a slot in exactly three places: the STREAM_START handler on
    /// the input stream's own source pad, the first buffer's block probe, and
    /// the GAP handler. The first one cannot fire for a pre-parsed input
    /// (`urisourcebin parse-streams=true`): decodebin3 builds the input's
    /// `identity` only once the STREAM_COLLECTION arrives and seeds the input
    /// stream's `active_stream` from the sink pad's sticky STREAM_START, so
    /// when it then replays that STREAM_START the handler sees no change and
    /// skips the linking. The second is what the hold exists to prevent (a
    /// buffer pushed into a slotless or output-less stream returns
    /// not-linked and kills the source). That leaves the GAP.
    ///
    /// A stream with no slot is worse than merely unselectable: it can never
    /// be `all_streams_present`, so no collection containing it becomes
    /// decodebin3's output collection, and every SELECT_STREAMS naming it is
    /// then silently discarded (`handle_select_streams` binds the request to
    /// the collection and only switches for the output one). No selection
    /// applies, no `STREAMS_SELECTED` is posted, and the hold, which only
    /// [`Inner::unblock_selected_externals`] lifts, never lifts.
    ///
    /// The GAP is pushed from the block probe on purpose: on the input's
    /// streaming thread (serialized events must not come from anywhere else)
    /// and after the sticky events have reached decodebin3, so the slot link
    /// happens in the probe and the post-probe sticky re-push carries
    /// STREAM_START into the fresh slot, which is what finally marks the
    /// stream present.
    fn seed_slot_for_held_pad(pad: &gst::Pad, pts: Option<gst::ClockTime>) {
        // Zero duration: this announces no missing content, it only gives
        // decodebin3 a data-like event to react to. A cue may legitimately
        // start at the same instant.
        let gap = gst::event::Gap::builder(pts.unwrap_or(gst::ClockTime::ZERO))
            .duration(gst::ClockTime::ZERO)
            .build();
        debug!(pad = %pad.name(), ?pts, "seeding a decodebin3 slot for the held external stream");
        if !pad.push_event(gap) {
            warn!(pad = %pad.name(), "the held external input refused the slot-seeding gap");
        }
    }

    /// Release the hold-until-selected blocks of every external input whose
    /// stream a just-applied selection names (see
    /// [`ExternalInput::hold_until_selected`]). Once decodebin3 confirmed
    /// the stream selected, the flowing buffers reach the stream's multiqueue
    /// slot, which now has an output, and the subtitle plays.
    ///
    /// `owed` names the one input, if any, whose realigning replay was queued
    /// by the same `STREAMS_SELECTED`. Its hold is NOT released here; the
    /// replay owes it (see [`Inner::release_owed_hold`]).
    fn unblock_selected_externals(
        &self,
        selected_ids: &[String],
        owed: Option<(ExternalSubId, u32)>,
    ) {
        let to_unblock: Vec<(gst::Pad, gst::PadProbeId)> = {
            let mut routing = self.routing.lock();
            let mut probes = Vec::new();
            for input in routing.inputs.iter_mut() {
                let held = input
                    .external
                    .as_ref()
                    .is_some_and(|e| e.hold_until_selected);
                if !held || input.block_probes.is_empty() {
                    continue;
                }
                let sids = input.stream_ids();
                if !sids.iter().any(|sid| selected_ids.iter().any(|s| s == sid)) {
                    continue;
                }
                let Some(external) = input.external.as_mut() else {
                    continue;
                };
                // Selected, so the hold's own condition is discharged either
                // way. What the owed replay holds back is only the PROBES.
                external.hold_until_selected = false;
                if owed == Some((external.id, external.epoch)) {
                    external.hold_release_owed = true;
                    continue;
                }
                probes.append(&mut input.block_probes);
            }
            probes
        };
        for (pad, probe) in to_unblock {
            debug!(pad = %pad.name(), "releasing a selected external input's data hold");
            pad.remove_probe(probe);
        }
    }

    /// Release the block probes of an external input whose hold was owed to
    /// its realigning replay seek (see
    /// [`ExternalInput::hold_release_owed`]). A no-op for every other input.
    ///
    /// This is the whole of the fix for cues rendering against a different
    /// origin than the video. A held external's `last_origin` stays ZERO,
    /// because a forwarded seek only reaches inputs with a live branch and a
    /// held one has none. `STREAMS_SELECTED` then used to release the hold
    /// SYNCHRONOUSLY while merely QUEUEING the realigning replay, and an
    /// as-fast-as-possible source needs no more than that gap to push its
    /// whole file against the stale `[0, ..)` segment: the reproducer's
    /// external has exactly 60 cues and the failure was exactly 60
    /// consecutive misaligned ones, released at 44.106355 against a replay
    /// issued at 44.109360. Ordering the two by MOVING the release later does
    /// not help, since the replay is queued to another thread either way; the
    /// release has to be CAUSED BY the replay, which is what this is.
    ///
    /// Called from [`FcastPlaybin::run_replay_seek`] on ALL of its outcomes,
    /// the refusal included. A refused seek is owed again through
    /// [`Inner::deferred_replays`] and would otherwise leave the input held
    /// for as long as that owing lasts, which is the liveness half of the
    /// problem: a held external that never unblocks shows no subtitles at
    /// all, a strictly worse failure than shifted ones. Nothing is lost by
    /// releasing there, because the deferred seek is itself FLUSHING and
    /// wipes whatever escaped.
    fn release_owed_hold(inner: &Arc<Inner>, id: ExternalSubId, epoch: u32) {
        let to_unblock: Vec<(gst::Pad, gst::PadProbeId)> = {
            let mut routing = inner.routing.lock();
            let Some(input) = routing.inputs.iter_mut().find(|input| {
                input
                    .external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch && e.hold_release_owed)
            }) else {
                return;
            };
            if let Some(external) = input.external.as_mut() {
                external.hold_release_owed = false;
            }
            std::mem::take(&mut input.block_probes)
        };
        for (pad, probe) in to_unblock {
            debug!(pad = %pad.name(), ?id, "releasing an external input's data hold owed to its replay");
            pad.remove_probe(probe);
        }
    }

    /// Queue [`Job::VerifyReplay`] after [`REPLAY_VERIFY_AFTER`], off the
    /// worker (a bounded sleeper, exactly like the sub watchdog).
    fn arm_replay_verification(&self, id: ExternalSubId, epoch: u32, attempt: u32) {
        // ONE chain per input. Two independent paths arm this for the same
        // event: `poll_text_policy` replays on every join of an external, and
        // the selection-time handler arms a check when the selection moves
        // onto one. Both fired for a single switch, each spawned its own
        // `VerifyReplay`, and each of those replayed and armed again, so the
        // attempt counters escalated in lockstep down two rival chains.
        // Observed in the field as paired `VerifyReplay ... attempt=0` a
        // millisecond apart, then paired replays at attempt=1, 2, 3.
        if !self.replay_checks_armed.lock().insert((id, epoch)) {
            debug!(
                ?id,
                epoch, attempt, "a replay verification is already armed for this input"
            );
            return;
        }
        let work_tx = self.work_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("fpb-replay-check".into())
            .spawn(move || {
                std::thread::sleep(REPLAY_VERIFY_AFTER);
                let _ = work_tx.send(Job::VerifyReplay { id, epoch, attempt });
            });
        if let Err(err) = spawned {
            warn!(?err, ?id, "failed to arm the replay verification");
        }
    }

    /// Wake every parked text push before a downward state change. Two
    /// kinds of thread sit parked HOLDING pad locks the state change
    /// needs to deactivate pads, wedging `set_state` forever: a live
    /// overlay branch inside textoverlay's cue sync, and a mid-push input
    /// inside its byte-limited decodebin3 slot. The flush pairs wake
    /// both.
    /// Wake the blocked push on every LIVE text branch, and drop what it has
    /// queued. Needed before a subtitle REPLACE is dispatched: the outgoing text
    /// slot's multiqueue src pad sits inside `gst_pad_push` into
    /// [`RoutedStream::tqueue`], which is a plain `queue` whose default
    /// `max-size-time` of 1s counts the DEAD AIR between sparse cues
    /// (`gst_queue_apply_gap` advances the time level off GAP events), so it
    /// reports itself full while holding ZERO buffers and ZERO bytes.
    /// decodebin3's stream switch cannot deactivate a slot whose pad is
    /// mid-push, so the switch waits out the outgoing track's cue cadence:
    /// measured 1.6s at a 2s cue period and 4.6s at 4s, with essentially the
    /// whole latency in that one push. The flush also discards the outgoing
    /// backlog, so the new track's first cue renders instead of queueing behind
    /// seconds of the old one.
    /// Put every routed text branch back on the A/V branches' RUNNING-TIME
    /// timeline.
    ///
    /// Text deliberately BYPASSES streamsynchronizer (see [`RoutedStream`]), so
    /// it never receives the per-GROUP base streamsynchronizer stamps onto the
    /// A/V segments when a gapless swap moves to the next item. Measured at
    /// subtitleoverlay's own input pads, one swap into an 8s item:
    ///
    /// ```text
    /// video_sink    SEGMENT start=0 base=0:00:08.189219955  rt(pts 0.8s)=8.989s
    /// subtitle_sink SEGMENT start=0 base=0:00:00.000000000  rt(pts 0.8s)=0.800s
    /// ```
    ///
    /// subtitleoverlay composites by running time, so every cue of the new item
    /// lands ~8s in the past and NOTHING renders for the rest of the item: the
    /// selection confirms and the branch links, it is just dead. A pad offset on
    /// the decodebin3 text pad re-pushes its sticky segment with the missing
    /// base; the same run that rendered 0 cue-bearing buffers rendered 32 after.
    ///
    /// The gapless swap is the ONLY transition that opens this delta, measured
    /// by logging every SEGMENT reaching `video_sink` over a load, a mid-item
    /// flushing seek and a swap: the load and the seek both arrive with base 0
    /// on BOTH branches (every seek this crate issues carries
    /// `SeekFlags::FLUSH`, which restarts running time), and only the swap
    /// carries a non-zero base. So this is cheap: it computes 0 and does nothing
    /// everywhere except at a gapless boundary.
    ///
    /// Idempotent: `gst_pad_set_offset` applies the offset on the way OUT to the
    /// peer, so the pad's own sticky segment keeps the raw base and the computed
    /// value is stable across repeated calls. `overlay_timeline`'s `origin`
    /// cannot see this divergence: it folds base into stream time with a
    /// `saturating_sub`, which pins both sides to 0.
    ///
    /// Takes the routing lock, so it must NOT run on a streaming thread: the
    /// probe in [`FcastPlaybin::new`] posts [`Job::SyncTextRunningTime`] rather
    /// than calling this directly, for the reason [`Job::FinishActivation`]
    /// documents.
    fn sync_text_running_time(&self) {
        let base_of = |pad: &gst::Pad| -> Option<i64> {
            let event = pad.sticky_event::<gst::event::Segment>(0)?;
            let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
            Some(segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as i64)
        };
        let Some(video_base) = self
            .overlay
            .static_pad("video_sink")
            .and_then(|pad| base_of(&pad))
        else {
            return;
        };
        let routing = self.routing.lock();
        for routed in routing.routed.iter().filter(|r| r.kind == StreamKind::Text) {
            let Some(text_base) = base_of(&routed.db3_src_pad) else {
                debug!(
                    pad = %routed.db3_src_pad.name(),
                    video_base,
                    "the text branch has no segment yet, so it cannot be aligned"
                );
                continue;
            };
            let offset = video_base - text_base;
            if routed.db3_src_pad.offset() != offset {
                debug!(
                    pad = %routed.db3_src_pad.name(),
                    offset,
                    video_base,
                    text_base,
                    previous = routed.db3_src_pad.offset(),
                    linked = routed.downstream.is_some(),
                    "aligning the text branch's running time with the A/V branches'"
                );
                routed.db3_src_pad.set_offset(offset);
            }
        }
    }

    /// The live text branches' downstream pads, collected so the caller can
    /// flush them with the routing lock RELEASED.
    ///
    /// A flush must never be sent while holding that lock. `send_event` runs
    /// the whole downstream event chain inline on the calling thread, and a
    /// `FLUSH_START` reaching a multiqueue sink pad makes it
    /// `gst_pad_pause_task` its src pad, which blocks on that pad's stream
    /// lock until the streaming task returns. That task is very often inside
    /// one of this crate's own pad probes, and those take the routing lock.
    /// Holding it here inverts the order and deadlocks the process. The
    /// worker holds routing and waits for the stream lock while the streaming
    /// thread holds the stream lock and waits for routing. Observed as a
    /// hard wedge of the whole test binary, with the worker parked in
    /// `gst_pad_pause_task` under `flush_parked_text_pushes` and a
    /// `multiqueue:src` task parked in `route_db3_pad`'s probe.
    fn live_text_downstream_pads(&self) -> Vec<gst::Pad> {
        let routing = self.routing.lock();
        routing
            .routed
            .iter()
            .filter(|routed| routed.kind == StreamKind::Text)
            .filter_map(|routed| routed.downstream.clone())
            .collect()
    }

    /// Send the flush pair to `pads`. Callers must already have dropped the
    /// routing lock (see [`Inner::live_text_downstream_pads`]).
    fn flush_pads(pads: &[gst::Pad]) {
        for pad in pads {
            let _ = pad.send_event(gst::event::FlushStart::new());
            let _ = pad.send_event(gst::event::FlushStop::new(true));
        }
    }

    /// [`Inner::flush_pads`] plus the SEGMENT the pair takes away. decodebin3
    /// SINK pads at a TEARDOWN only, see SCOPE.
    ///
    /// `FLUSH_STOP` deletes the pad's SEGMENT sticky (gstpad.c
    /// `remove_event_by_type`) and nothing re-arms it: `check_sticky` needs
    /// `PENDING_EVENTS`, which only `schedule_events` sets and no flush reaches.
    /// A flush from UPSTREAM costs nothing because it is a seek and the source
    /// re-segments; an injected pair has no such follow-up, so a straggler
    /// buffer chains segmentless. Replaying the captured sticky (not a rebuilt
    /// one, so the timeline is untouched) is the only fix: marking events
    /// pending cannot work, `push_sticky` skips anything already `received`.
    /// The pair is unchanged, so `flush_pairs_matched` still holds.
    ///
    /// SCOPE: widening this was measured WRONG. Replaying on every flushed pad
    /// regressed `external_subtitle_lifecycle` to 16 passed / 3 failed in 127 s
    /// against 19 passed in 8 s, all three on "no FSTA cue reached the overlay":
    /// a restored segment is stale where a branch is about to be re-linked or
    /// released. Text pads, `remove_input` and `dispose_text_branch_on` keep the
    /// bare pair.
    ///
    /// Lever: `FCAST_NO_FLUSH_SEGMENT_RESTORE`.
    fn flush_db3_sink_pads(pads: &[gst::Pad]) {
        if std::env::var_os("FCAST_NO_FLUSH_SEGMENT_RESTORE").is_some() {
            Self::flush_pads(pads);
            return;
        }
        for pad in pads {
            // Read BEFORE the FLUSH_START. After the pair the pad has no
            // segment left to read.
            let segment = pad.sticky_event::<gst::event::Segment>(0);
            let _ = pad.send_event(gst::event::FlushStart::new());
            let _ = pad.send_event(gst::event::FlushStop::new(true));
            if let Some(segment) = segment {
                debug!(pad = %pad.name(), "restoring the segment the flush pair removed");
                let _ = pad.send_event(segment);
            }
        }
    }

    /// Hand `pump_selection`'s eager REPLACE flush to the worker as a
    /// coalescing intent.
    ///
    /// The flush must never run on the dispatching caller's thread. It
    /// blocks until the outgoing branch's streaming thread can be paused,
    /// and whether that can happen right now is not reliably decidable from
    /// pipeline state. A pipeline resting in PAUSED never releases it (the
    /// field deadlock, captured with gdb), one held below PLAYING by
    /// buffering does not either, and the state guard that tried to tell
    /// the two moments apart over-matched three times. So the caller only
    /// records the intent and pokes the worker. The worker runs it at a
    /// settled PLAYING, and every pipeline state edge re-attempts whatever
    /// is still pending, so the intent cannot gate its own drain condition.
    /// The intent slot COALESCES. Fifteen rapid switches write it fifteen
    /// times and the pipeline flushes once.
    ///
    /// # A settled PLAYING does NOT make the flush complete by construction
    ///
    /// This comment used to claim it did. It does not. Selecting a stream
    /// puts the sinks back into `gst_base_sink_wait_preroll` while the
    /// pipeline still reads a settled PLAYING, so a flush dispatched there
    /// can park on a queue's stream lock just the same. The state check is a
    /// good filter, never a guarantee, and nothing may be built on it being
    /// one. What keeps a parked flush from wedging the receiver is a separate
    /// property. The thread it parks on must not be the only route to the
    /// state change that would release it. See [`ReplayJob`] for the seek
    /// that broke exactly that rule and deadlocked the worker.
    fn submit_text_flush(inner: &Arc<Inner>) {
        // A new intent invalidates the last drain's no-op verdict, whichever
        // dispatch path records it below (see `Inner::drain_poke_parked`).
        inner.drain_poke_parked.store(false, Ordering::SeqCst);
        // A/B levers, for interleaved regression measurements without a
        // rebuild. FCAST_INLINE_TEXT_FLUSH restores the previous dispatch
        // (inline on the caller unless the pipeline rests in PAUSED), and
        // FCAST_NO_TEXT_WORK_DEFERRAL keeps meaning "never defer anything".
        let inline_lever = std::env::var_os("FCAST_INLINE_TEXT_FLUSH").is_some();
        let no_deferral = std::env::var_os("FCAST_NO_TEXT_WORK_DEFERRAL").is_some();
        if inline_lever || no_deferral {
            let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
            let resting = current == gst::State::Paused && pending == gst::State::VoidPending;
            if resting && !no_deferral {
                debug!("postponing the eager text-branch flush at a pipeline resting in PAUSED");
                *inner.deferred_text_work.lock() = Some(DeferredTextWork::Flush);
            } else {
                Inner::run_text_work(inner, DeferredTextWork::Flush);
            }
            return;
        }
        debug!("queueing the eager text-branch flush onto the worker");
        *inner.deferred_text_work.lock() = Some(DeferredTextWork::Flush);
        let _ = inner.work_tx.send(Job::DrainTextWork);
    }

    /// Carry out one piece of eager text-branch work. Never call this at a
    /// pipeline resting in PAUSED, see the call site in `pump_selection`.
    fn run_text_work(inner: &Arc<Inner>, work: DeferredTextWork) {
        match work {
            DeferredTextWork::Park => Inner::park_text_streams(inner),
            DeferredTextWork::Flush => inner.flush_live_text_branches(),
        }
    }

    /// Whether ANY postponed text-branch work is pending. One predicate so
    /// the drain triggers and the drain itself can never disagree about what
    /// counts as pending. The old inline check tested only the work slot and
    /// the disposals, so pending replays or input removals with nothing else
    /// pending were skipped by the very drain that owns them.
    fn has_deferred_text_work(&self) -> bool {
        self.deferred_text_work.lock().is_some()
            || !self.deferred_text_disposal.lock().is_empty()
            || !self.deferred_replays.lock().is_empty()
            || !self.deferred_input_removal.lock().is_empty()
            || !self.deferred_verifications.lock().is_empty()
    }

    /// Replay whatever had to be postponed, once the pipeline is playing and
    /// the flush is most likely to complete (only likely, see
    /// [`Inner::submit_text_flush`]). WORKER-ONLY, through
    /// [`Job::DrainTextWork`], because everything in here can block behind a
    /// streaming thread and must therefore stay off the callers. That is a
    /// statement about the CALLERS and not a licence for the worker. The
    /// worker also owns [`Job::SetState`], so anything it blocks on that only
    /// PLAYING can release deadlocks the receiver (see [`ReplayJob`]). The
    /// flushes below are dispatched here because their pads are unlinked or
    /// being disposed of, not because blocking here is safe.
    ///
    /// The job is queued on every pipeline state edge and at every caller
    /// settle point, so no postponed work depends on the caller's poll
    /// cadence and none can gate its own drain condition.
    fn run_deferred_text_work(inner: &Arc<Inner>) {
        if !inner.has_deferred_text_work() {
            return;
        }
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if current != gst::State::Playing || pending != gst::State::VoidPending {
            // A no-op verdict. Until something changes it (a new postponed
            // item, or a state edge whose unconditional re-queue lands back
            // here), re-running this drain from the caller's poll can only
            // reach this same return, so the poll's re-poke is suppressed
            // (see `Inner::drain_poke_parked`). Recording the verdict AFTER
            // the state read is what makes a racing edge safe: an edge that
            // fires in between has already queued its own drain, which runs
            // after this one and refreshes the verdict.
            inner.drain_poke_parked.store(true, Ordering::SeqCst);
            return;
        }
        // The drain proceeds, so the last no-op verdict (if any) is stale.
        inner.drain_poke_parked.store(false, Ordering::SeqCst);
        // Branches unlinked while paused, now safe to flush and drop.
        let disposals = std::mem::take(&mut *inner.deferred_text_disposal.lock());
        let disposed = !disposals.is_empty();
        for disposal in disposals {
            debug!("disposing of a text branch postponed while paused");
            inner.dispose_text_branch(disposal);
        }
        // A disposal holds the overlay seat while it flushes, and the LINK side
        // refuses to wait for it (`TextSeat::try_hold`, so the caller thread is
        // never blocked behind a text branch). Nothing used to retry that
        // skipped link except the caller's next settle point, whenever that
        // came: the field showed a switched-to external whose branch never
        // joined at all while its replays burned out on a 400ms timer. The
        // thread that caused the skip retries it here instead.
        // Lever: `FCAST_NO_DRAIN_TEXT_POLICY_POKE`.
        if disposed && std::env::var_os("FCAST_NO_DRAIN_TEXT_POLICY_POKE").is_none() {
            Inner::poll_text_policy(inner);
        }
        // Replays that could not be delivered while paused.
        let owed = std::mem::take(&mut *inner.deferred_replays.lock());
        for (id, epoch, attempt) in owed {
            debug!(?id, epoch, attempt, "replaying an input postponed while paused");
            let _ = inner.work_tx.send(Job::ReplaySub { id, epoch, attempt });
        }
        // Inputs a detach took out of routing but could not tear down.
        let inputs = std::mem::take(&mut *inner.deferred_input_removal.lock());
        for input in inputs {
            debug!(
                generation = input.generation,
                "removing an input postponed while paused"
            );
            Inner::remove_input(inner, input);
        }
        // Taken only once the pipeline can carry it out, so a postponed piece
        // of work is never dropped by a poll that arrives too early.
        if let Some(work) = inner.deferred_text_work.lock().take() {
            debug!(?work, "running the text-branch work postponed while paused");
            Inner::run_text_work(inner, work);
        }
        // The rest of the drain is the verdict-deferral change, gated whole
        // by the same lever as its `verify_replay` half.
        if std::env::var_os("FCAST_NO_REPLAY_VERDICT_DEFERRAL").is_some() {
            return;
        }
        // Verdicts held while the pipeline could not produce evidence.
        // Re-armed rather than decided inline, so the check fires one
        // verification interval AFTER the flush above ran, against a branch
        // whose delivery is real and not a leftover sticky.
        let held = std::mem::take(&mut *inner.deferred_verifications.lock());
        for (id, epoch, attempt) in held {
            debug!(
                ?id,
                epoch, attempt, "re-arming a replay verification whose verdict was held"
            );
            inner.arm_replay_verification(id, epoch, attempt);
        }
        // The verification arming is edge-triggered by STREAMS_SELECTED, and
        // a run of switches at a parked pipeline either consumes that edge
        // below PLAYING or stalls decodebin3 into never posting it. Either
        // way the settled selection can name an external whose delivery
        // nothing will ever re-check. This drain runs exactly at the settle
        // points where delivery becomes provable, so re-verify here instead
        // of trusting an edge that may never come again. Redundant armings
        // dedupe in `arm_replay_verification`, and an aligned, delivering
        // branch concludes its check quietly.
        let selected = inner.selection.lock().subtitle_sid();
        if let Some(sid) = selected {
            let target = {
                let routing = inner.routing.lock();
                routing.inputs.iter().find_map(|input| {
                    let external = input.external.as_ref()?;
                    input
                        .stream_ids()
                        .contains(&sid)
                        .then_some((external.id, external.epoch))
                })
            };
            if let Some((id, epoch)) = target {
                inner.arm_replay_verification(id, epoch, 0);
            }
        }
    }

    fn flush_live_text_branches(&self) {
        // (feeder, downstream): the decodebin3 output pad and the tqueue sink
        // it feeds.
        let branches: Vec<(gst::Pad, gst::Pad)> = {
            let routing = self.routing.lock();
            routing
                .routed
                .iter()
                .filter(|routed| routed.kind == StreamKind::Text)
                .filter_map(|routed| {
                    routed
                        .downstream
                        .clone()
                        .map(|downstream| (routed.db3_src_pad.clone(), downstream))
                })
                .collect()
        };
        // The ONE flush injector that logged nothing, which made a field log
        // ambiguous exactly when it mattered: the adaptivedemux2 carry-patch
        // discarded a FLUSHING on a dash text track with no crate line anywhere
        // near it, and nobody could tell whether this pair was the source.
        debug!(
            branches = branches.len(),
            pads = ?branches
                .iter()
                .map(|(feeder, downstream)| format!("{} -> {}", feeder.name(), downstream.name()))
                .collect::<Vec<_>>(),
            "flushing the live text branches"
        );
        for (feeder, downstream) in branches {
            // A push racing the pair below lands between FLUSH_START and
            // FLUSH_STOP and returns FLUSHING upstream. An adaptive demuxer
            // serves EVERY track from ONE output loop and pauses it for good
            // on any non-OK push (`gstadaptivedemux.c` output loop, a DEBUG
            // line and `gst_task_pause`), so that one refused text push
            // freezes video and audio with no error anywhere: the
            // frozen-at-the-start field class, `output_time` pinned while
            // input fills to the watermark. The DROP probe makes a racing
            // push return OK (push probes run before the peer lookup),
            // losing at most the backlog this flush discards anyway; a
            // dropped sticky-event push stays pending on the pad and
            // re-delivers with the next buffer.
            // Lever: `FCAST_NO_TEXT_FLUSH_FEEDER_HOLD`.
            let hold = if std::env::var_os("FCAST_NO_TEXT_FLUSH_FEEDER_HOLD").is_none() {
                feeder.add_probe(
                    gst::PadProbeType::BUFFER
                        | gst::PadProbeType::BUFFER_LIST
                        | gst::PadProbeType::EVENT_DOWNSTREAM,
                    |_pad, _info| gst::PadProbeReturn::Drop,
                )
            } else {
                None
            };
            Self::flush_pads(std::slice::from_ref(&downstream));
            if let Some(id) = hold {
                feeder.remove_probe(id);
            }
        }
    }

    /// # A FLUSH_START-only variant here is WRONG, and was measured to be
    ///
    /// The teardown deadlock this flush sits in the middle of (see
    /// `tests/regression_teardown_flush.rs`) is caused by the flush PAIR. Its
    /// FLUSH_STOP re-arms the pad, and a source pushing as fast as it can
    /// re-blocks under the stream lock before the caller reaches its
    /// `set_state(Null)`. Sending FLUSH_START alone and leaving the pads
    /// flushing looks like the obvious answer, and it is not.
    ///
    /// `db3_sink_pads` covers EVERY stream of the input rather than just its
    /// text, so the unmatched FLUSH_START reaches audio and video too, and
    /// `teardown` also runs at READY for a stop-and-reload where the pipeline
    /// is reused afterwards. Measured on the `fuzz_scenarios` driver, seeds
    /// 500001, 500002 and 500010 pass with the pair and fail without it, on
    /// `flush_pairs_matched: flush-start never matched by a flush-stop` with
    /// entries still recorded after it. The wedge is real and stays open, but
    /// its fix has to leave the flush pairing intact.
    fn flush_parked_text_pushes(&self) {
        let pads = self.live_text_downstream_pads();
        let db3_sinks: Vec<gst::Pad> = {
            let routing = self.routing.lock();
            routing
                .inputs
                .iter()
                .flat_map(|input| input.db3_sink_pads.iter().cloned())
                .collect()
        };
        Self::flush_pads(&pads);
        Self::flush_db3_sink_pads(&db3_sinks);
    }

    /// The timeline the overlay renders text against: the rate and the
    /// stream position that running time is measured from, read off the
    /// very segment textoverlay compares text buffers with. A flushing seek
    /// moves that origin to its target (segment start, base 0), so it is
    /// only zero while nothing has sought yet. Text whose own segment
    /// starts elsewhere renders shifted by the difference. Falls back to
    /// (1.0, ZERO) before the first segment arrives.
    fn overlay_timeline(&self) -> (f64, gst::ClockTime) {
        // No segment on the overlay yet (fresh load, or a start seek's flush
        // cleared it): the recorded intent is the best truth available, and
        // zero is only right when nothing ever sought.
        let fallback = *self.intended_timeline.lock();
        let Some(event) = self
            .overlay
            .static_pad("video_sink")
            .and_then(|pad| pad.sticky_event::<gst::event::Segment>(0))
        else {
            return fallback;
        };
        let Some(segment) = event.segment().downcast_ref::<gst::ClockTime>() else {
            return fallback;
        };
        let rate = segment.rate();
        let start = segment.start().unwrap_or(gst::ClockTime::ZERO);
        // Running time is (position - start) / |rate| + base, so the origin
        // (the position whose running time is zero) sits base * |rate| of
        // stream time below `start`.
        let base =
            (segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as f64 * rate.abs()) as u64;
        let origin = gst::ClockTime::from_nseconds(start.nseconds().saturating_sub(base));
        // A reverse segment cannot be expressed as a forward replay.
        if rate > 0.0 {
            (rate, origin)
        } else {
            (1.0, origin)
        }
    }

    /// Send `seek` straight into the load's MAIN input, on every source pad
    /// it has exposed. This is the SOURCE-side twin of a pipeline seek, and
    /// the only delivery that does not depend on the output graph: a
    /// `pipeline.seek()` is an upstream event, so `GstBin` hands it to the
    /// sink children present AT THAT INSTANT and to nothing else (see
    /// `gst_bin_send_event`), and every branch that has no sink downstream
    /// yet is simply skipped. Since the load's chains join one at a time (a
    /// chain per routed stream) and the preroll token retires on the FIRST of
    /// them, "the pipeline is prerolled" does not mean "every stream has a
    /// sink". Delivering at the input instead reaches each elementary
    /// stream's source no matter what the output side looks like: a branch
    /// whose chain has not joined still gets its post-seek segment as a
    /// sticky event, which replays downstream the moment the chain links.
    ///
    /// Returns whether the seek was delivered at all. An input with no source
    /// pads yet, or one that refused every pad, returns false so the caller
    /// falls back to the pipeline broadcast rather than silently skipping the
    /// start position. A pad that refuses individually is NOT worth the
    /// fallback: the broadcast would travel up that same branch and be refused
    /// by the same element, so it would only buy a second flush.
    fn seek_main_input(&self, event: &gst::Event) -> bool {
        let generation = self.current_generation();
        let targets: Vec<gst::Pad> = {
            let routing = self.routing.lock();
            routing
                .inputs
                .iter()
                // The main input only: externals have their own forwarding
                // below, and a PREPARED next item (registered under its
                // future generation) must not be dragged onto this item's
                // timeline.
                .filter(|i| i.external.is_none() && i.generation == generation)
                .flat_map(|i| i.element.src_pads())
                .collect()
        };
        if targets.is_empty() {
            return false;
        }
        let mut accepted = 0usize;
        for pad in &targets {
            if pad.send_event(event.clone()) {
                accepted += 1;
            } else {
                warn!(pad = %pad.name(), "the main input refused the start seek");
            }
        }
        debug!(
            pads = targets.len(),
            accepted,
            // A pad not yet linked into decodebin3 is one the pad-added
            // handler is still wiring; measured over a suite soak this is
            // always zero here, which is why the delivery does not need to
            // exclude them.
            unlinked = targets.iter().filter(|pad| !pad.is_linked()).count(),
            "start seek: delivered to the main input's source pads"
        );
        accepted > 0
    }

    /// Forward a just-performed user seek into every external subtitle
    /// input whose stream is live in the overlay. A pipeline seek travels
    /// the sink chains and decodebin3 forwards it up the MAIN input only,
    /// so a side input's segment stays on the old timeline and its cues
    /// never sync against the sought video again. The same seek through
    /// the input aligns its segment and replays from the target
    /// (uridecodebin3 forwarded seeks to every source handler for the
    /// same reason). Deselected inputs are skipped: their replayed data
    /// would land in the parking sink, and the join-time replay owns
    /// their recovery.
    fn forward_seek_to_live_externals(&self, rate: f64, position: gst::ClockTime) {
        let targets: Vec<gst::Pad> = {
            let mut routing = self.routing.lock();
            let live_text: Vec<String> = routing
                .routed
                .iter()
                .filter(|r| r.kind == StreamKind::Text && r.downstream.is_some())
                .filter_map(|r| r.db3_src_pad.stream_id().map(|s| s.to_string()))
                .collect();
            routing
                .inputs
                .iter_mut()
                .filter(|i| {
                    i.external.is_some() && i.stream_ids().iter().any(|sid| live_text.contains(sid))
                })
                .flat_map(|i| {
                    // The forwarded seek moves this input onto the new
                    // timeline, see `ExternalInput::last_origin`.
                    if let Some(external) = i.external.as_mut() {
                        external.last_origin = position;
                    }
                    i.element.src_pads()
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        // Mirror `send_rate_seek`'s event exactly so both sides of the
        // pipeline land on the same segment.
        let mut flags = gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH;
        if rate < 0.0 || rate > 2.0 {
            flags |= gst::SeekFlags::TRICKMODE;
        }
        let event = if rate >= 0.0 {
            gst::event::Seek::builder(
                rate,
                flags,
                gst::SeekType::Set,
                position,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            )
            .build()
        } else {
            gst::event::Seek::builder(
                rate,
                flags,
                gst::SeekType::Set,
                gst::ClockTime::ZERO,
                gst::SeekType::End,
                position,
            )
            .build()
        };
        for pad in targets {
            debug!(pad = %pad.name(), "forwarding the seek to a live external subtitle input");
            if !pad.send_event(event.clone()) {
                warn!(pad = %pad.name(), "the external input refused the forwarded seek");
            }
        }
    }

    /// Bookkeeping for one linked input pad: the bitrate tap, the drain
    /// watch (gapless), and the input's pad lists. Returns `false` when the
    /// input is no longer registered (the caller releases the request pad).
    fn record_linked_input_pad(
        inner: &Arc<Inner>,
        element: &gst::Element,
        pad: &gst::Pad,
        sinkpad: gst::Pad,
    ) -> bool {
        // Bitrate inspection tap: count the stream's PARSED (compressed)
        // bytes, one relaxed atomic add per buffer. Callers poll cumulative
        // counters and compute rates from deltas (`stream_io_stats`).
        let bytes = Arc::new(AtomicU64::new(0));
        let probe = pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            {
                let bytes = Arc::clone(&bytes);
                move |_pad, info| {
                    let n: usize = match &info.data {
                        Some(gst::PadProbeData::Buffer(buffer)) => buffer.size(),
                        Some(gst::PadProbeData::BufferList(list)) => {
                            list.iter().map(|b| b.size()).sum()
                        }
                        _ => 0,
                    };
                    bytes.fetch_add(n as u64, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                }
            },
        );

        // Drain watch: track whether this pad has pushed EOS into
        // decodebin3 (a seek's SEGMENT or an item switch's STREAM_START
        // reset it). The gapless swap waits for every main-input pad to
        // drain.
        let saw_eos = Arc::new(AtomicBool::new(false));
        let event_probe = pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
            let saw_eos = Arc::clone(&saw_eos);
            let weak = Arc::downgrade(inner);
            move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    match event.type_() {
                        gst::EventType::Eos => {
                            saw_eos.store(true, Ordering::SeqCst);
                            if let Some(inner) = weak.upgrade() {
                                Inner::note_input_pad_eos(&inner);
                            }
                        }
                        gst::EventType::StreamStart | gst::EventType::Segment => {
                            saw_eos.store(false, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
                gst::PadProbeReturn::Ok
            }
        });

        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.db3_sink_pads.push(sinkpad);
            input.taps.push(StreamTap {
                pad: pad.clone(),
                bytes,
                probe,
                saw_eos,
                event_probe,
            });
            true
        } else {
            drop(routing);
            if let Some(probe) = probe {
                pad.remove_probe(probe);
            }
            if let Some(probe) = event_probe {
                pad.remove_probe(probe);
            }
            false
        }
    }

    /// One main-input pad pushed EOS into decodebin3: when EVERY main-input
    /// pad has, the current item is fully drained and a pending gapless
    /// swap may proceed (the prepared input's blocked threads are parked on
    /// the swap gate).
    fn note_input_pad_eos(inner: &Arc<Inner>) {
        let current = inner.current_generation();
        let drained = {
            let routing = inner.routing.lock();
            routing
                .inputs
                .iter()
                .filter(|i| i.generation == current && i.external.is_none())
                .any(|i| {
                    !i.taps.is_empty() && i.taps.iter().all(|t| t.saw_eos.load(Ordering::SeqCst))
                })
        };
        if !drained {
            return;
        }
        let mut state = inner.swap_gate.state.lock();
        if state.pending.is_some() && !state.drained {
            debug!("current input fully drained into decodebin3");
            state.drained = true;
            inner.swap_gate.cond.notify_all();
        }
    }

    /// Whether the current main input has pushed EOS on all its pads.
    fn main_input_drained(inner: &Arc<Inner>) -> bool {
        let current = inner.current_generation();
        let routing = inner.routing.lock();
        routing
            .inputs
            .iter()
            .filter(|i| i.generation == current && i.external.is_none())
            .any(|i| !i.taps.is_empty() && i.taps.iter().all(|t| t.saw_eos.load(Ordering::SeqCst)))
    }

    /// Register a prepared (gapless) next input: added to the running
    /// pipeline and activated, but its source pads are NOT linked into
    /// decodebin3. Each pad gets a block probe that lets serialized events
    /// through (so parsing completes and sticky stream-start/caps/segment
    /// accumulate on the unlinked pads) and holds DATA back; the first
    /// blocked buffer parks its streaming thread on the swap gate until the
    /// current item drains, then performs the relink
    /// ([`Self::perform_gapless_swap`]). The uridecodebin3 recipe.
    fn add_prepared_input(
        inner: &Arc<Inner>,
        element: gst::Element,
        generation: u64,
    ) -> Result<()> {
        // State-locked: the bin's state machinery skips this child, so a
        // broken prepared input (bad URL failing its state change) cannot
        // poison a concurrent pipeline transition (it wedged the current
        // item's PAUSED->PLAYING commit). The crate drives its state
        // explicitly: synced here, unlocked at the swap when it becomes
        // the live input, NULLed at removal.
        element.set_locked_state(true);
        inner
            .pipeline
            .add(&element)
            .context("adding the prepared input element")?;
        inner.routing.lock().inputs.push(Input {
            element: element.clone(),
            generation,
            external: None,
            db3_sink_pads: Vec::new(),
            taps: Vec::new(),
            pad_added_sig: None,
            block_probes: Vec::new(),
        });

        let pad_added_sig = element.connect_pad_added({
            let inner = Arc::downgrade(inner);
            move |element, pad| {
                let Some(inner) = inner.upgrade() else { return };
                Inner::block_prepared_pad(&inner, element, pad, generation);
            }
        });
        {
            let mut routing = inner.routing.lock();
            if let Some(input) = routing.inputs.iter_mut().find(|i| i.element == element) {
                input.pad_added_sig = Some(pad_added_sig);
            }
        }
        for pad in element.src_pads() {
            Inner::block_prepared_pad(inner, &element, &pad, generation);
        }

        if let Err(err) = element.sync_state_with_parent() {
            let mut routing = inner.routing.lock();
            if let Some(idx) = routing.inputs.iter().position(|i| i.element == element) {
                let input = routing.inputs.remove(idx);
                drop(routing);
                Inner::remove_input(inner, input);
            }
            return Err(err).context("syncing the prepared input element state");
        }
        Ok(())
    }

    /// Install the gapless block probe on one prepared-input source pad.
    fn block_prepared_pad(
        inner: &Arc<Inner>,
        element: &gst::Element,
        pad: &gst::Pad,
        generation: u64,
    ) {
        let probe = pad.add_probe(
            gst::PadProbeType::BLOCK
                | gst::PadProbeType::BUFFER
                | gst::PadProbeType::BUFFER_LIST
                | gst::PadProbeType::EVENT_DOWNSTREAM,
            {
                let weak = Arc::downgrade(inner);
                move |pad, info| Inner::prepared_block_probe(&weak, pad, info, generation)
            },
        );
        let Some(probe) = probe else { return };
        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.block_probes.push((pad.clone(), probe));
        } else {
            drop(routing);
            pad.remove_probe(probe);
        }
    }

    /// The prepared input's data gate (see [`Self::add_prepared_input`]).
    /// Runs on the prepared input's streaming threads.
    fn prepared_block_probe(
        weak: &Weak<Inner>,
        pad: &gst::Pad,
        info: &mut gst::PadProbeInfo,
        generation: u64,
    ) -> gst::PadProbeReturn {
        // Serialized events pass (parsing progresses, sticky events
        // accumulate on the unlinked pad). GAP is data-like and blocks
        // with the buffers, exactly like uridecodebin3's block probe.
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && event.type_() != gst::EventType::Gap
        {
            return gst::PadProbeReturn::Pass;
        }
        let Some(inner) = weak.upgrade() else {
            return gst::PadProbeReturn::Remove;
        };

        let mut state = inner.swap_gate.state.lock();
        loop {
            if state.swapped || inner.current_generation() == generation {
                // The swap already ran (this pad's link is live): flow.
                // The generation check covers a straggler probe firing
                // AFTER the next item's prepare re-armed the gate (a
                // sparse pad's first datum can come long past this input's
                // own activation, and the fresh gate no longer names this
                // generation): its input is the LIVE one and flushing here
                // would kill the playing stream.
                return gst::PadProbeReturn::Remove;
            }
            if state.pending != Some(generation) {
                // Cancelled or superseded: flush this input's streaming
                // thread out so teardown can join it.
                debug!(pad = %pad.name(), "prepared pad unblocked by a cancelled swap");
                drop(state);
                let _ = info.data.take();
                info.flow_res = Err(gst::FlowError::Flushing);
                return gst::PadProbeReturn::Handled;
            }
            if state.drained {
                break;
            }
            inner.swap_gate.cond.wait(&mut state);
        }

        // The current input is fully drained and this thread got here
        // first: relink the core to the prepared input for ALL its pads.
        match Inner::perform_gapless_swap(&inner, generation) {
            Ok(()) => {
                info!(generation, "gapless swap performed at drain");
                state.swapped = true;
                inner.swap_gate.cond.notify_all();
                gst::PadProbeReturn::Remove
            }
            Err(err) => {
                error!(?err, generation, "gapless swap failed");
                drop(state);
                // The cancel job cleans the input up and, since the item's
                // EOS was already consumed by the drain, synthesizes the
                // end-of-stream the caller now needs to advance normally.
                inner.emit(PlaybinEvent::PreparedFailed { generation });
                let _ = inner.work_tx.send(Job::CancelPrepared { notify: false });
                let _ = info.data.take();
                info.flow_res = Err(gst::FlowError::Flushing);
                gst::PadProbeReturn::Handled
            }
        }
    }

    /// Relink the live core from the drained current input to the prepared
    /// one: matching streams REUSE the same decodebin3 sink pad (unlink old
    /// pad, link new pad; the slot and its output pad survive, downstream
    /// sees only stream-start/caps/segment), unmatched old pads are
    /// released, unmatched new streams get fresh request pads. Runs on a
    /// prepared-input streaming thread, which is upstream's own recipe;
    /// element removal stays on the worker ([`Job::FinishActivation`]).
    fn perform_gapless_swap(inner: &Arc<Inner>, generation: u64) -> Result<()> {
        let db3 = inner
            .core
            .lock()
            .as_ref()
            .map(|c| c.db3.clone())
            .ok_or_else(|| anyhow!("no dynamic core"))?;
        let current = inner.current_generation();

        let (prepared_element, old_pairs, routed_ids, routed_kinds) = {
            let routing = inner.routing.lock();
            let prepared_element = routing
                .inputs
                .iter()
                .find(|i| i.generation == generation)
                .map(|i| i.element.clone())
                .ok_or_else(|| anyhow!("prepared input no longer registered"))?;
            // (src pad, decodebin3 sink pad) of the drained main input.
            let old_pairs: Vec<(gst::Pad, gst::Pad)> = routing
                .inputs
                .iter()
                .filter(|i| i.generation == current && i.external.is_none())
                .flat_map(|i| {
                    i.db3_sink_pads
                        .iter()
                        .filter_map(|sink| sink.peer().map(|src| (src, sink.clone())))
                })
                .collect();
            // The streams actually flowing OUT of decodebin3 (only selected
            // streams have output slots), by id and by kind.
            let routed_ids: Vec<String> = routing
                .routed
                .iter()
                .filter_map(|r| r.db3_src_pad.stream_id().map(|sid| sid.to_string()))
                .collect();
            let routed_kinds: Vec<StreamKind> = routing.routed.iter().map(|r| r.kind).collect();
            (prepared_element, old_pairs, routed_ids, routed_kinds)
        };

        // A live A/V output slot whose kind has NO successor stream cannot
        // switch gaplessly. Its sink outlives the stream: the hold drops
        // the old item's EOS, nothing removes an audio sink mid-item, and
        // the bin can then never aggregate the NEW item's end-of-stream
        // (a silent autoplay wedge); a dying video slot can conversely EOS
        // every remaining sink between items. Fail the swap BEFORE touching
        // any link: the caller reports PreparedFailed and the ordinary
        // end-of-stream advance owns the transition.
        let new_pads = prepared_element.src_pads();
        for (kind, want) in [
            (StreamKind::Video, gst::StreamType::VIDEO),
            (StreamKind::Audio, gst::StreamType::AUDIO),
        ] {
            if !routed_kinds.contains(&kind) {
                continue;
            }
            let covered = new_pads
                .iter()
                .any(|pad| pad.stream().is_some_and(|s| s.stream_type().contains(want)));
            if !covered {
                return Err(anyhow!(
                    "the prepared item has no {kind:?} stream to continue the live one"
                ));
            }
        }

        // Match new pads to old decodebin3 sink pads by stream type, the
        // uridecodebin3 criterion: a reused sink pad keeps the decodebin3
        // slot (and its output pad) alive across the switch. Prefer the old
        // pad whose stream is ROUTED (selected): a multi-track item also
        // has unselected sibling pads of the same type, and handing the
        // successor to one of those would let the playing slot die instead
        // (the exact wedge the coverage check above exists for). Plan
        // first, mutate after: a failed plan must leave the links alone.
        let mut taken = vec![false; old_pairs.len()];
        let mut links: Vec<(gst::Pad, gst::Pad)> = Vec::new();
        let mut fresh: Vec<gst::Pad> = Vec::new();
        for new_pad in &new_pads {
            let want = new_pad.stream().map(|s| s.stream_type());
            let type_matches = |idx: usize, taken: &[bool]| {
                !taken[idx]
                    && match want {
                        None => true,
                        Some(want) => {
                            old_pairs[idx].0.stream().map(|s| s.stream_type()) == Some(want)
                        }
                    }
            };
            let selected = |idx: usize| {
                old_pairs[idx]
                    .0
                    .stream_id()
                    .is_some_and(|sid| routed_ids.iter().any(|r| *r == sid))
            };
            let matched = (0..old_pairs.len())
                .find(|&idx| type_matches(idx, &taken) && selected(idx))
                .or_else(|| (0..old_pairs.len()).find(|&idx| type_matches(idx, &taken)));
            match matched {
                Some(idx) => {
                    taken[idx] = true;
                    debug!(
                        new = %new_pad.name(),
                        sink = %old_pairs[idx].1.name(),
                        "gapless: reusing the decodebin3 sink pad"
                    );
                    links.push((new_pad.clone(), old_pairs[idx].1.clone()));
                }
                None => fresh.push(new_pad.clone()),
            }
        }
        for (idx, (old_src, db3_sink)) in old_pairs.iter().enumerate() {
            let _ = old_src.unlink(db3_sink);
            if !taken[idx] {
                // An old stream with no successor: its slot ends here.
                debug!(sink = %db3_sink.name(), "gapless: releasing an unmatched old sink pad");
                db3.release_request_pad(db3_sink);
            }
        }
        for (new_pad, db3_sink) in &links {
            new_pad
                .link(db3_sink)
                .with_context(|| format!("relinking {} into decodebin3", new_pad.name()))?;
        }
        for new_pad in fresh {
            let db3_sink = {
                // Serialized: see `Inner::db3_pad_request`.
                let _serial = inner.db3_pad_request.lock();
                db3.request_pad_simple("sink_%u")
            }
            .ok_or_else(|| anyhow!("decodebin3 gave no request sink pad"))?;
            new_pad
                .link(&db3_sink)
                .with_context(|| format!("linking {} into decodebin3", new_pad.name()))?;
            debug!(new = %new_pad.name(), sink = %db3_sink.name(), "gapless: fresh sink pad");
            links.push((new_pad.clone(), db3_sink));
        }

        // The prepared input is the live input from here on: it follows
        // pipeline transitions again (the lock existed so a broken prepare
        // could not poison them, see `add_prepared_input`).
        prepared_element.set_locked_state(false);

        // Bookkeeping: the old input no longer owns any decodebin3 pads
        // (reused ones now belong to the new input, the rest were
        // released), so its later removal cannot touch them. The block
        // probe list clears: every remaining probe self-removes on its
        // next datum (`swapped` is set by our caller).
        {
            let mut routing = inner.routing.lock();
            for input in routing
                .inputs
                .iter_mut()
                .filter(|i| i.generation == current && i.external.is_none())
            {
                input.db3_sink_pads.clear();
            }
            if let Some(input) = routing
                .inputs
                .iter_mut()
                .find(|i| i.generation == generation)
            {
                input.block_probes.clear();
            }
        }
        for (new_pad, db3_sink) in links {
            Inner::record_linked_input_pad(inner, &prepared_element, &new_pad, db3_sink);
        }
        Ok(())
    }

    /// Build and install a fresh dynamic core (see `Core`): decodebin3 +
    /// streamsynchronizer, added to the pipeline at its current state, with
    /// the routing handlers connected.
    fn install_core(inner: &Arc<Inner>) -> Result<()> {
        let db3 = make("decodebin3", "fpb-decodebin")?;
        let ssync = make("streamsynchronizer", "fpb-ssync")?;
        inner
            .pipeline
            .add_many([&db3, &ssync])
            .context("adding the dynamic core")?;
        db3.sync_state_with_parent().context("syncing decodebin3")?;
        ssync
            .sync_state_with_parent()
            .context("syncing streamsynchronizer")?;

        // decodebin3 output pads appear per SELECTED stream. Route them
        // through streamsynchronizer into the chains.
        let pad_added_sig = db3.connect_pad_added({
            let inner = Arc::downgrade(inner);
            move |_, pad| {
                let Some(inner) = inner.upgrade() else { return };
                if let Err(err) = Inner::route_db3_pad(&inner, pad) {
                    warn!(?err, pad = %pad.name(), "failed to route decodebin3 pad");
                }
            }
        });
        let pad_removed_sig = db3.connect_pad_removed({
            let inner = Arc::downgrade(inner);
            move |_, pad| {
                let Some(inner) = inner.upgrade() else { return };
                Inner::unroute_db3_pad(&inner, pad);
            }
        });

        *inner.core.lock() = Some(Core {
            db3,
            ssync,
            pad_added_sig,
            pad_removed_sig,
        });
        Ok(())
    }

    /// Tear down the previous load's dynamic core: clean up any stream
    /// still routed through it, then NULL and drop decodebin3 and
    /// streamsynchronizer.
    fn teardown_core(inner: &Arc<Inner>) {
        let Some(core) = inner.core.lock().take() else {
            return;
        };
        core.db3.disconnect(core.pad_added_sig);
        core.db3.disconnect(core.pad_removed_sig);

        // Any pads deferred from THIS (now superseded) core are stale and
        // the drainer would only re-reject them. Drop them so a fresh load
        // starts clean.
        inner.deferred_pads.lock().clear();

        // Streams normally unroute via pad-removed when the inputs are
        // released. Clean up any straggler entry the same way.
        let leftover = std::mem::take(&mut inner.routing.lock().routed);
        for mut routed in leftover {
            if routed.kind == StreamKind::Text {
                Inner::detach_text_from_overlay(inner, &mut routed, false);
            } else if let (Some(ssync_src), Some(downstream)) =
                (&routed.ssync_src, &routed.downstream)
            {
                let _ = ssync_src.unlink(downstream);
            }
            inner.unpark_stream(&mut routed);
        }

        for element in [&core.db3, &core.ssync] {
            let _ = element.set_state(gst::State::Null);
            let _ = inner.pipeline.remove(element);
        }
        debug!("dropped the previous load's dynamic core");
    }

    /// Build the current load's audio sink and wire it `volume ! sink` if it
    /// isn't up yet. Idempotent within a load. The sink joins the running
    /// pipeline at `join_state`. Its base_time comes from `gst_bin_add`,
    /// which stamps the bin's current one: valid for a steady join, and a
    /// mid-load join is re-stamped by the commit walk.
    fn ensure_audio_sink(&self) -> Result<()> {
        let mut slot = self.audio_sink.lock();
        if slot.is_some() {
            return Ok(());
        }
        let sink = match &self.audio {
            // No fixed name (auto-unique per load), so nothing keyed off the
            // element name can collide with the previous load's
            // still-finalizing sink.
            AudioSink::Auto => gst::ElementFactory::make("autoaudiosink")
                .build()
                .context("creating autoaudiosink")?,
            AudioSink::Factory(factory) => factory().context("building the audio sink")?,
        };
        self.pipeline.add(&sink).context("adding the audio sink")?;
        self.volume
            .link(&sink)
            .context("linking volume to the audio sink")?;
        sink.set_state(self.join_state())
            .context("syncing the audio sink")?;
        *slot = Some(sink);
        Ok(())
    }

    /// Drop the current load's audio sink (see `Inner::audio`): unlink
    /// `volume ! sink`, NULL it, remove it, drop the ref so its pulse
    /// context is fully released. Call only at a quiescent point (load
    /// reset under the route gate): NULLing a linked, streaming sink in
    /// place races its teardown and crashes.
    fn remove_audio_sink(&self) {
        let Some(sink) = self.audio_sink.lock().take() else {
            return;
        };
        self.volume.unlink(&sink);
        let _ = sink.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&sink);
    }

    /// The state a dynamically (re)activated element is driven to so it
    /// joins the pipeline WITHOUT outrunning an in-flight async transition.
    ///
    /// NOT `sync_state_with_parent`: that targets the pipeline's TARGET
    /// state, so a sink activated during a load self-continues to PLAYING
    /// off its own preroll BEFORE the PAUSED->PLAYING commit distributes the
    /// new base_time. The sink then syncs against the previous load's
    /// base_time (or no clock at all on the first load) and every playback
    /// start opened with a QoS drop storm culling the first ~1s of video.
    /// Joining at PAUSED instead parks the preroll in the async set the
    /// commit already waits on, and the commit's child walk lifts the
    /// element to PLAYING with the freshly-selected base_time. If the walk
    /// races past an element mid-activation, its ASYNC_START makes the bin
    /// lose state and re-commit (the standard dynamic-sink dance), so
    /// nothing parks for good.
    ///
    /// With no transition in flight, match the pipeline exactly: the normal
    /// late-joining-sink case (but see the stamp in `ensure_video_chain`).
    fn join_state(&self) -> gst::State {
        let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
        decisions::join_state(current, pending)
    }

    /// Put the video chain (subtitleoverlay + video sink) into the pipeline
    /// and bring it to the join state. Called from `route_db3_pad` when a
    /// video stream routes. Idempotent, and also the recovery from a
    /// mid-item deselect's parked chain (unlocks and re-joins it). The
    /// chain lives in the pipeline ONLY while the item has video: an absent
    /// chain cannot hang a video-less preroll and never counts in the bin's
    /// EOS/STREAM_START aggregation, by construction.
    /// The MEMBERSHIP half of [`Inner::ensure_video_chain`]: put the chain in
    /// the pipeline and link the overlay to the sink. Split out because a
    /// caller must be able to do this without the state half, which blocks:
    /// `gst_pad_link` refuses a link whose two pads have no common ancestor
    /// (`GST_PAD_LINK_CHECK_HIERARCHY`), so the chain has to be IN the
    /// pipeline before `route_db3_pad` can link a stream into it, even when
    /// the activation itself is deferred (see [`ChainJoinJob`]).
    ///
    /// Nothing here blocks on a state or stream lock: `gst_bin_add` takes the
    /// bin's object lock and changes no child state, and the link is a caps
    /// and hierarchy check.
    ///
    /// Under `video_chain_membership` because a route and a chain join now run
    /// concurrently (see [`Inner::join_gate`]): two threads both finding the
    /// chain out of the pipeline would both add it, and the loser's
    /// `gst_bin_add` fails, taking its whole route with it.
    fn attach_video_chain(&self) -> Result<()> {
        let _membership = self.video_chain_membership.lock();
        if self.overlay.parent().is_some() {
            return Ok(());
        }
        let elements: Vec<&gst::Element> = self.video_chain.iter().collect();
        self.pipeline
            .add_many(elements)
            .context("adding the video chain")?;
        // The overlay-to-sink link is made on the first join and persists
        // across membership changes.
        let src = self
            .overlay
            .static_pad("src")
            .expect("subtitleoverlay has a src pad");
        if !src.is_linked() {
            let sink = self.video_chain.last().expect("chain has a sink");
            self.overlay
                .link_pads(Some("src"), sink, None)
                .context("linking subtitleoverlay to the video sink")?;
        }
        Ok(())
    }

    fn ensure_video_chain(&self) -> Result<()> {
        // A previous deselect's drop probe would silently eat the stream this
        // join exists to render, so it comes off first (see
        // `park_video_chain_for_deselect`).
        self.clear_video_park_probe();
        // A video chain can re-preroll mid-load and needs the deep audio
        // buffer to avoid the demuxer-stall deadlock (see `aqueue` in `new`).
        self.audio_entry
            .set_property("max-size-time", AQUEUE_VIDEO_TIME_NS);
        // Idempotent, and already done by a deferred route (see
        // `attach_video_chain`).
        self.attach_video_chain()?;
        let join = self.join_state();
        // Joining a steady PLAYING pipeline renders immediately, so stamp
        // the pipeline's current base_time first: the chain missed every
        // commit walk while it was out of the pipeline, so its own base_time
        // is stale, possibly by many loads.
        let base_time = (join == gst::State::Playing)
            .then(|| self.pipeline.base_time())
            .flatten();
        // Sync sink-first (downstream before upstream), the usual dynamic
        // relink order. The unlock undoes a mid-item deselect's park (see
        // `park_video_chain_for_deselect`).
        for element in self.video_chain.iter().rev() {
            element.set_locked_state(false);
            if let Some(base_time) = base_time {
                element.set_base_time(base_time);
            }
            if let Err(err) = element.set_state(join) {
                warn!(?err, element = %element.name(), "failed to activate a video chain element");
            }
        }
        Ok(())
    }

    /// Take the video chain out of the pipeline: READY it sink-first (aborts
    /// any clock/preroll wait, unwinding a blocked streaming thread out of
    /// the branch), unlink from upstream, remove, and NULL the overlay so no
    /// caps/renderer state leaks into its next join (a stale subtitle
    /// renderer wedged the load after a VOBSUB selection). The caller's
    /// video sink is GL/window-bound and parks at READY outside the
    /// pipeline, never NULLed (playbin3's own treatment of it). Runs at the
    /// load reset and when a mid-item video deselect completes
    /// (`unroute_db3_pad`). Once removed, the bin's EOS aggregation can no
    /// longer wait on a sink that will never see data again.
    fn remove_video_chain(&self) {
        // The pad the deselect's probe sits on is about to be unlinked and
        // released; take the probe off while it still exists.
        self.clear_video_park_probe();
        // No video chain to deadlock: restore the shallow audio-only queue so
        // gapless holds the outgoing EOS near the sink boundary (see `aqueue`
        // in `new`). Unconditional: a load reset removes the chain and re-shallows.
        self.audio_entry
            .set_property("max-size-time", AQUEUE_AUDIO_TIME_NS);
        if self.overlay.parent().is_none() {
            return;
        }
        for element in self.video_chain.iter().rev() {
            element.set_locked_state(false);
            let _ = element.set_state(gst::State::Ready);
        }
        // Unlink from upstream (the streamsynchronizer src, when a stream
        // is still routed into the overlay).
        if let Some(pad) = self.overlay.static_pad("video_sink")
            && let Some(peer) = pad.peer()
        {
            let _ = peer.unlink(&pad);
        }
        for element in &self.video_chain {
            let _ = self.pipeline.remove(element);
        }
        let _ = self.overlay.set_state(gst::State::Null);
        debug!("removed the video chain from the pipeline");
    }

    /// The load's preroll is now carried by a real sink's async (the caller
    /// just activated a chain), so retire the token (see `Inner::token_src`).
    /// Repeats (the second routed chain, post-EOS pushes) are harmlessly
    /// rejected by appsrc.
    fn finish_preroll_token(&self) {
        let _ = self
            .token_src
            .emit_by_name::<gst::FlowReturn>("push-buffer", &[&gst::Buffer::new()]);
        let _ = self
            .token_src
            .emit_by_name::<gst::FlowReturn>("end-of-stream", &[]);
    }

    /// The new selection drops video entirely: park the chain at READY
    /// immediately, mid-item. Without this the video-disable reconfiguration
    /// can deadlock the whole pipeline: the selection change briefly hiccups
    /// audio, the audio-sink-provided pipeline clock freezes, the video sink
    /// sits in `wait_clock` forever, the decodebin3 video slot never goes
    /// IDLE, and the full slot backpressures into the demuxer, which never
    /// produces the audio that would restart the clock. The READY descent
    /// aborts the clock wait, letting the slot idle and the deactivation
    /// finish.
    ///
    /// Three constraints:
    /// - NOT a flush: basesink would post ASYNC_START and wedge the pipeline at
    ///   pending PAUSED on a re-preroll no data will finish.
    /// - NOT a READY descent, which is what this used to be. It aborts the clock
    ///   wait of the multiqueue slot task mid-push, and the resulting
    ///   GST_FLOW_FLUSHING is fatal to `gst_multi_queue_loop`: it parks the
    ///   demuxer on a FLUSH_STOP nobody will send, so the stream dies with no EOS
    ///   and no error and a later re-select gets a pad with no data
    ///   (`fuzz_buffering` 1600031). A DROP probe instead returns GST_FLOW_OK
    ///   (push probes run before the peer lookup), so the slot drains, the clock
    ///   keeps advancing and the thread leaves on its own, cutting the
    ///   backpressure cycle at its source. Buffers only, so ssync grouping and
    ///   the sink's EOS still work. Lever: `FCAST_READY_PARK_DESELECTED_VIDEO`.
    /// - The state LOCK holds until decodebin3 removes the pad, or a state change
    ///   walking its children would lift the dataless chain back up and its sink
    ///   would hold the pipeline async forever. `unroute_db3_pad` then removes
    ///   the chain entirely and a re-select rebuilds it.
    fn park_video_chain_for_deselect(&self) {
        if self.overlay.parent().is_none() {
            return;
        }
        for element in &self.video_chain {
            element.set_locked_state(true);
        }
        // Two deselect dispatches in a row would otherwise leak the first
        // probe onto a pad this one is about to stop tracking.
        self.clear_video_park_probe();
        let feeding = self
            .overlay
            .static_pad("video_sink")
            .and_then(|pad| pad.peer());
        if let Some(feeding) = feeding
            && std::env::var_os("FCAST_READY_PARK_DESELECTED_VIDEO").is_none()
        {
            info!(
                pad = %feeding.name(),
                "selection drops video, dropping the data feeding the video chain"
            );
            let id = feeding.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                |_pad, _info| gst::PadProbeReturn::Drop,
            );
            if let Some(id) = id {
                *self.video_park_probe.lock() = Some((feeding, id));
            }
            self.lift_deselected_video_sink();
            return;
        }
        info!("selection drops video, parking the video chain at READY");
        // Sink-first: the sink's READY aborts its clock/preroll wait,
        // unwinding the blocked streaming thread out of the branch before
        // the upstream elements deactivate their pads.
        for element in self.video_chain.iter().rev() {
            let _ = element.set_state(gst::State::Ready);
        }
    }

    /// Let the ONE video push already inside the chain finish, so decodebin3
    /// can complete the deactivation.
    ///
    /// The DROP probe above stops NEW data, and no probe can touch a push that
    /// is already past it. At a pipeline resting in PAUSED that push sits in
    /// the video sink's `gst_base_sink_wait_preroll`, whose only two exits
    /// gstbasesink.c:2438 names itself: "waiting in preroll for flush or
    /// PLAYING". Until it returns, the decodebin3 multiqueue slot's src pad is
    /// never idle, so the IDLE probe `handle_stream_switch` arms to run
    /// `mq_slot_unassign` (gstdecodebin3.c:4566) never fires,
    /// `is_selection_done` keeps bailing out with "Stream from previous
    /// selection still active" (:3358), and NO `STREAMS_SELECTED` is posted for
    /// the selection AT ALL. The caller's track change then never confirms:
    /// FAST `video_disable_while_paused_v4`, 18 s of silence and a receiver
    /// still waiting.
    ///
    /// Of the two exits, the flush is the one this crate must never take: it
    /// returns FLUSHING into the multiqueue, which latches it, which is exactly
    /// what the reverted READY descent did. So take the other one and lift the
    /// SINK to PLAYING. The parked push returns OK, the pad idles between
    /// pushes, and decodebin3 finishes the deactivation itself and posts the
    /// REAL message, so no crate bookkeeping is short-circuited.
    ///
    /// Only at a PAUSED pipeline: at PLAYING nothing needs preroll, and below
    /// PAUSED the chain is going down anyway. The audio chain never needed this
    /// because `fpb-aqueue` absorbs its slot's pushes, which is why only VIDEO
    /// deselects wedge here.
    ///
    /// Cost: the frames already past the probe (the parked one, plus whatever
    /// subtitleoverlay holds internally) render onto the paused frame just
    /// before video goes away. Lever:
    /// `FCAST_NO_PAUSED_DESELECT_SINK_LIFT`.
    fn lift_deselected_video_sink(&self) {
        if std::env::var_os("FCAST_NO_PAUSED_DESELECT_SINK_LIFT").is_some() {
            return;
        }
        let (_, current, _) = self.pipeline.state(gst::ClockTime::ZERO);
        if current != gst::State::Paused {
            return;
        }
        let Some(sink) = self.video_chain.last() else {
            return;
        };
        debug!(
            sink = %sink.name(),
            "lifting the deselected video sink to PLAYING so its parked preroll push returns"
        );
        if let Err(err) = sink.set_state(gst::State::Playing) {
            warn!(?err, "failed to lift the deselected video sink");
        }
    }

    /// Lift a mid-item video deselect's DROP probe (see
    /// `park_video_chain_for_deselect`). Called wherever the video chain
    /// changes hands, so a re-selected video stream can never be dropped by a
    /// previous deselect's probe. A no-op when nothing is parked.
    fn clear_video_park_probe(&self) {
        if let Some((pad, id)) = self.video_park_probe.lock().take() {
            debug!(pad = %pad.name(), "lifting the deselected video chain's drop probe");
            pad.remove_probe(id);
        }
    }

    /// Create and wire a per-stream parking sink for a text stream that may
    /// not join the overlay yet (see `RoutedStream::park_pad` /
    /// `RoutedStream::park_sink`).
    fn park_stream(&self, source: &gst::Pad) -> Result<(gst::Element, gst::Pad)> {
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .property("enable-last-sample", false)
            .build()
            .context("creating a text parking sink")?;
        // Keep it out of everything GstBin routes through SINK-flagged
        // children (the `fpb-token-sink` treatment, for the same class of
        // reason). An unsynced parking sink consumes at multiqueue speed, so
        // it races toward the item's end: in the bin's POSITION fold (MAX over
        // the flagged children) it can dominate the answer, and a `-1`
        // DURATION from it poisons the duration fold entirely.
        sink.unset_element_flags(gst::ElementFlags::SINK);
        self.pipeline
            .add(&sink)
            .context("adding the text parking sink")?;
        sink.sync_state_with_parent()
            .context("syncing the text parking sink")?;
        let pad = sink.static_pad("sink").expect("fakesink has a sink pad");
        // `source` is the decodebin3 text pad itself (text bypasses ssync).
        source
            .link(&pad)
            .context("linking text into its parking sink")?;
        Ok((sink, pad))
    }

    /// Undo `park_stream`: unlink and remove the stream's parking sink.
    fn unpark_stream(&self, routed: &mut RoutedStream) {
        if let Some(pad) = routed.park_pad.take() {
            // Text bypasses ssync, so its source is the decodebin3 pad.
            let _ = routed.db3_src_pad.unlink(&pad);
        }
        if let Some(sink) = routed.park_sink.take() {
            let _ = sink.set_state(gst::State::Null);
            let _ = self.pipeline.remove(&sink);
        }
    }

    /// Stop and remove one input: NULL the element (its streaming threads
    /// stop pushing), unlink, release the decodebin3 request pads (decodebin3
    /// updates its collection), drop from the pipeline.
    /// Remove an input, or postpone it when the pipeline is at rest in PAUSED.
    ///
    /// ONLY for a user-initiated detach. `remove_input` has to flush the
    /// input's decodebin3 sink pads before it can NULL the element, or the
    /// NULL deadlocks on the input's own parked pushes. That flush travels
    /// down into decodebin3 and ends in `gst_multi_queue_sink_event` calling
    /// `gst_pad_pause_task` on the slot's src task. At a pipeline resting in
    /// PAUSED that task is stuck inside subtitleoverlay behind sinks parked in
    /// `gst_base_sink_wait_preroll`, so the pause never returns and the caller
    /// is wedged. On the worker it took every job queued behind it. Captured
    /// with gdb, `remove_input -> send_event -> ... ->
    /// gst_multi_queue_sink_event -> gst_pad_pause_task`.
    ///
    /// Unlinking the branch first is not enough, and that is worth recording:
    /// the flush blocks one level ABOVE the pad this crate can unlink, inside
    /// decodebin3, and unlinking downstream does not retract a push already in
    /// flight through the multiqueue.
    ///
    /// Teardown, stop, shutdown and the load reset must NOT come through here.
    /// They cannot postpone anything, and their own state change to READY or
    /// NULL releases the sinks from `wait_preroll`, which is what lets the
    /// flush complete there. That holds only because those paths make the
    /// state change BEFORE they block; see [`FcastPlaybin::teardown`], where it
    /// used to come last and wedged the worker for exactly this reason.
    fn remove_input_or_defer(inner: &Arc<Inner>, input: Input) {
        // A text branch of THIS input live in the overlay is one way to leave
        // the slot's multiqueue task stuck inside subtitleoverlay, which is
        // what the flush then cannot pause. It is not the only way: see the
        // postponed-disposal case below.
        //
        // Deferring on the pipeline state alone was too broad: a pipeline
        // mid-LOAD also rests at PAUSED, and postponing there left the input
        // in the graph across the load and hung
        // `attach_then_detach_mid_load_leaves_the_load_intact`.
        let has_live_text = {
            let sids = input.stream_ids();
            let routing = inner.routing.lock();
            routing.routed.iter().any(|routed| {
                routed.kind == StreamKind::Text
                    && routed.downstream.is_some()
                    && routed
                        .db3_src_pad
                        .stream_id()
                        .is_some_and(|sid| sids.contains(&sid.to_string()))
            })
        };
        // A branch of THIS input is not the only thing that can hold the
        // block. A POSTPONED DISPOSAL is a text queue whose loop task was
        // already inside subtitleoverlay when its branch was severed, and it
        // keeps holding that queue's stream lock until the disposal runs (the
        // case `Inner::dispose_text_branch_on` describes). decodebin3 shares
        // one multiqueue across every input, so this input's flush still ends
        // in a `gst_pad_pause_task` that waits behind that stuck queue.
        //
        // Captured with gdb on `fuzz_buffering` seed 1600016, deterministic 3
        // of 3: the worker parked in `remove_input`'s decodebin3-sink flush
        // with `branches=0` for the input it was removing, one second after a
        // "postponing a text branch disposal" at a pipeline resting in PAUSED.
        // The ownership half of the guard was doing its job and the block was
        // simply somewhere else.
        //
        // The drain is ordered to match: `Inner::run_deferred_text_work` runs
        // the disposals before the input removals, so by the time this removal
        // is retried the queue that blocked it is gone.
        //
        // `FCAST_NO_DISPOSAL_AWARE_DETACH_DEFERRAL` restores the old predicate.
        let disposal_pending = !inner.deferred_text_disposal.lock().is_empty()
            && std::env::var_os("FCAST_NO_DISPOSAL_AWARE_DETACH_DEFERRAL").is_none();
        if (has_live_text || disposal_pending)
            && Inner::resting_paused(&inner.pipeline)
            && std::env::var_os("FCAST_NO_TEXT_WORK_DEFERRAL").is_none()
        {
            debug!(
                generation = input.generation,
                "postponing an input removal: the pipeline is at rest in PAUSED"
            );
            // A new postponed item invalidates the last drain's no-op
            // verdict (see `Inner::drain_poke_parked`).
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
            inner.deferred_input_removal.lock().push(input);
        } else {
            Inner::remove_input(inner, input);
        }
    }

    fn remove_input(inner: &Arc<Inner>, input: Input) {
        // Read before the fields below are moved out of `input`.
        let sids = input.stream_ids();
        if let Some(sig) = input.pad_added_sig {
            input.element.disconnect(sig);
        }
        // DROP everything this input still pushes, BEFORE the flush and the
        // unlink below. Its own chain (a queue inside its urisourcebin, upstream
        // of the text queue the detach probe guards) keeps pushing into
        // decodebin3 while this runs, and the first push after the unlink
        // returns GST_FLOW_NOT_LINKED, on which that queue's loop posts
        // "Internal data stream error". The causal error classification treats
        // the source's own task death as a transport race and recovers, but it
        // cannot eat an error a QUEUE posts, so it reached the caller as
        // `PlaybinEvent::Error`: a user-visible Media error for a routine
        // subtitle detach. Pinned on `fuzz_scenarios` seed 500002 iteration 1
        // step 6 (detach of a SELECTED external right after a seek).
        //
        // A push probe returns GST_FLOW_OK before the peer is ever looked up
        // (gstpad.c runs them first), so from here the input pushes into the
        // void harmlessly. Nothing is lost: this input is leaving and goes to
        // NULL a few lines down. Never removed, the pads die with the element.
        // Buffers only, so the flush pair and EOS below still travel.
        // Lever: `FCAST_NO_INPUT_DROP_ON_REMOVE`.
        let guard_pads: Vec<gst::Pad> = input.taps.iter().map(|tap| tap.pad.clone()).collect();
        if std::env::var_os("FCAST_NO_INPUT_DROP_ON_REMOVE").is_none() {
            debug!(
                pads = ?guard_pads.iter().map(|pad| pad.name().to_string()).collect::<Vec<_>>(),
                "dropping the leaving input's data before its decodebin3 chain is taken apart"
            );
            for pad in &guard_pads {
                pad.add_probe(
                    gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                    |_pad, _info| gst::PadProbeReturn::Drop,
                );
            }
        }
        for mut tap in input.taps {
            if let Some(probe) = tap.probe.take() {
                tap.pad.remove_probe(probe);
            }
            if let Some(probe) = tap.event_probe.take() {
                tap.pad.remove_probe(probe);
            }
        }
        // A cancelled prepare's block probes (a performed swap clears the
        // list itself). Any thread parked inside one was already woken and
        // flushed by the swap gate abort that precedes this removal.
        for (pad, probe) in input.block_probes {
            pad.remove_probe(probe);
        }
        // A still-locked prepared input unlocks on its way out (harmless
        // for ordinary inputs).
        input.element.set_locked_state(false);
        // This input's OWN text branches come out of the graph before the
        // flush below. A `FLUSH_START` on a decodebin3 sink pad does not stop
        // at the slot: gstpad forwards it through parsebin and multiqueue, out
        // of decodebin3's src pad and into the text queue, where
        // `gst_queue_handle_sink_event` pauses a task that may be stuck inside
        // subtitleoverlay. That wedged the WORKER, taking every job queued
        // behind it (the next load, the stop, the shutdown barrier). Unlinking
        // first leaves the flush nowhere to travel, and `gst_pad_unlink` takes
        // no stream lock so it is safe at any state. The branches belong to an
        // input that is leaving, so they had to go anyway.
        let text_parts: Vec<(gst::Pad, gst::Pad, Option<gst::Element>)> = {
            let mut routing = inner.routing.lock();
            routing
                .routed
                .iter_mut()
                .filter(|routed| routed.kind == StreamKind::Text)
                .filter(|routed| {
                    routed
                        .db3_src_pad
                        .stream_id()
                        .is_some_and(|sid| sids.contains(&sid.to_string()))
                })
                .filter_map(|routed| {
                    let downstream = routed.downstream.take()?;
                    Some((routed.db3_src_pad.clone(), downstream, routed.tqueue.take()))
                })
                .collect()
        };
        debug!(
            branches = text_parts.len(),
            ?sids,
            "detaching the leaving input's text branches before the decodebin3 flush"
        );
        for (db3_src_pad, downstream, tqueue) in text_parts {
            Inner::detach_text_parts(inner, &db3_src_pad, &downstream, tqueue, false);
        }
        // A mid-push input's streaming thread is parked inside its
        // decodebin3 slot HOLDING ITS OWN PAD LOCKS, and the NULL below
        // deadlocks on them (this wedged the worker in the field). Flush
        // the input's decodebin3 chain first to release the parked pushes.
        // The BARE pair, deliberately. A segment replay here regressed the
        // external-subtitle reattach path, and this window closes at the NULL
        // two lines down anyway (see `Inner::flush_db3_sink_pads`).
        for db3_sink in &input.db3_sink_pads {
            let _ = db3_sink.send_event(gst::event::FlushStart::new());
            let _ = db3_sink.send_event(gst::event::FlushStop::new(true));
        }
        // Losing the state change here is fine, the element is leaving.
        let _ = input.element.set_state(gst::State::Null);
        for db3_sink in &input.db3_sink_pads {
            if let Some(peer) = db3_sink.peer() {
                let _ = peer.unlink(db3_sink);
            }
            // Release against the pad's OWN decodebin3: after a core swap
            // this input's pads belong to the previous instance.
            if let Some(db3) = db3_sink.parent_element() {
                db3.release_request_pad(db3_sink);
            }
        }
        let _ = inner.pipeline.remove(&input.element);
    }

    fn remove_all_inputs(inner: &Arc<Inner>) {
        let inputs = std::mem::take(&mut inner.routing.lock().inputs);
        for input in inputs {
            Inner::remove_input(inner, input);
        }
    }

    /// Re-attempt every deferred pad through the full routing path. Runs on
    /// every [`RouteGate`] release. The guards re-reject stale (superseded
    /// core) or torn-down (not accepting) pads, and a pad still blocked by
    /// another gate holder is re-deferred (that holder's release drains it).
    fn drain_deferred_pads(inner: &Arc<Inner>) {
        let pending = std::mem::take(&mut *inner.deferred_pads.lock());
        for pad in pending {
            if let Err(err) = Inner::route_db3_pad(inner, &pad) {
                warn!(?err, pad = %pad.name(), "failed to route deferred pad");
            }
        }
    }

    /// Route a decodebin3 output pad through streamsynchronizer into its
    /// chain. Text pads obey the link policy (steady PLAYING only).
    fn route_db3_pad(inner: &Arc<Inner>, pad: &gst::Pad) -> Result<()> {
        // pad-added also fires for decodebin3's request SINK pads (our own
        // input links). Only source pads are output streams to route.
        if pad.direction() != gst::PadDirection::Src {
            return Ok(());
        }
        // Refuse pads while a downward transition holds the gate (see
        // `Inner::route_gate`). Hold it for the whole route so a teardown
        // cannot start mid-route either. The deferred-pads lock is held
        // ACROSS the try-lock so a failed attempt's push cannot slip in
        // after the concurrent holder's release already drained (which
        // would orphan the pad): with the lock held, either the drain sees
        // the push, or the push happens after the drain and this try-lock
        // succeeds.
        let gate = {
            let mut deferred = inner.deferred_pads.lock();
            match Inner::try_gate(inner) {
                Some(gate) => Some(gate),
                None => {
                    // The gate is held by a concurrent downward transition.
                    // A pad from the CURRENT core is the ACTIVE load losing
                    // a stream: DEFER it (the holder's release re-attempts
                    // it) rather than dropping it for good (the load-stall
                    // race). A pad from another core is teardown debris.
                    let from_current_core = inner
                        .core
                        .lock()
                        .as_ref()
                        .is_some_and(|c| pad.parent_element().as_ref() == Some(&c.db3));
                    if from_current_core {
                        deferred.push(pad.clone());
                        debug!(pad = %pad.name(), "deferring active-core pad past a teardown");
                    } else {
                        debug!(pad = %pad.name(), "ignoring pad exposed during a teardown");
                    }
                    None
                }
            }
        };
        let Some(_gate) = gate else {
            return Ok(());
        };
        // A pad from a superseded core (the previous load's decodebin3 can
        // still process queued selections while dying) must not be routed:
        // it would occupy the chain entry and wedge the next preroll.
        let (ssync, ssync_owner) = {
            let core = inner.core.lock();
            let Some(core) = core.as_ref() else {
                return Ok(());
            };
            if pad.parent_element().as_ref() != Some(&core.db3) {
                debug!(pad = %pad.name(), "ignoring pad from a superseded core");
                return Ok(());
            }
            (core.ssync.clone(), core.db3.clone())
        };
        // Pads appearing while the pipeline is at/heading to READY are
        // stragglers from a superseded load's teardown. Legitimate pads only
        // appear during a preroll (pending at least PAUSED) or in a settled
        // pipeline at PAUSED or above.
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::pad_accepting(current, pending) {
            warn!(pad = %pad.name(), ?current, ?pending,
                   "ignoring stray pad from a superseded load");
            return Ok(());
        }
        let kind = Inner::stream_kind_of(pad)
            .ok_or_else(|| anyhow!("cannot determine stream kind of {}", pad.name()))?;
        // Runs on a STREAMING THREAD holding decodebin3's SELECTION_LOCK
        // (pad-added comes from `mq_slot_check_reconfiguration`, which took it),
        // so an unbounded blocking call here stops every SELECT_STREAMS, every
        // other slot's reconfiguration and decodebin3's own state changes, i.e.
        // the teardown that would release it. `ensure_video_chain` /
        // `ensure_audio_sink` `set_state` on the calling thread, and a
        // window-bound sink can spend seconds in Ready->Paused.
        //
        // So only the ACTIVATION moves, to `fpb-join`, with the stream held at
        // the ssync src pad meanwhile. The topology stays inline: the selection
        // dance needs it visible the moment `send_event(SELECT_STREAMS)`
        // returns, and this function also runs inline on `fpb-select` when
        // decodebin3 exposes the pad inside that send.
        //
        // Lever: `FCAST_INLINE_CHAIN_JOIN`.
        let deferred_join = std::env::var_os("FCAST_INLINE_CHAIN_JOIN").is_none();

        // Request a streamsynchronizer sink/src pair and link `pad` into it.
        // A/V only, TEXT bypasses ssync entirely (see `RoutedStream`).
        let attach_ssync = || -> Result<(gst::Pad, gst::Pad)> {
            let sink = ssync
                .request_pad_simple("sink_%u")
                .ok_or_else(|| anyhow!("streamsynchronizer gave no request pad"))?;
            // streamsynchronizer pairs sink_N with src_N.
            let src_name = sink.name().replace("sink_", "src_");
            let src = ssync
                .static_pad(&src_name)
                .ok_or_else(|| anyhow!("streamsynchronizer src pad {src_name} missing"))?;
            pad.link(&sink)
                .with_context(|| format!("linking {} into streamsynchronizer", pad.name()))?;
            Ok((sink, src))
        };

        // Set by the A/V arms when the chain's activation was deferred. The
        // job is sent at the very END of this function, once the routing entry
        // exists: `run_chain_join` refuses a join whose stream is no longer
        // routed, and the entry is pushed below.
        let mut queue_join = false;
        let mut join_hold: Option<(gst::Pad, gst::PadProbeId)> = None;

        let (ssync_sink, ssync_src, downstream, park_pad, park_sink) = match kind {
            // A video pad appearing while the crate's last dispatched
            // selection turned video OFF is decodebin3's collection-default
            // auto-select resurrecting the deselected stream (an attach
            // makes it re-select over the applied state). Rebuilding the
            // chain for it drops the pipeline into an async re-preroll that
            // the deselected stream may never feed again, and that
            // unfinished transition holds the receiver's pump gate closed
            // forever, so the engine's corrective re-assert can never run
            // (fuzz_buffering seed 300002, campaign 5). Park the pad the way
            // text parks instead. The parking sink consumes the stream
            // without any async element, the pipeline stays settled, the
            // engine notices the divergence and re-asserts on the next
            // pump, and decodebin3 drops the pad again. A genuine re-enable
            // flips the dispatched intent before its selection reaches
            // decodebin3, so it takes the rebuild arm below. The lever
            // restores the unconditional rebuild and gates this whole
            // change (the mirror it reads is inert without this read).
            StreamKind::Video
                if inner.video_deselected.load(Ordering::SeqCst)
                    && std::env::var_os("FCAST_ROUTE_DESELECTED_VIDEO").is_none() =>
            {
                info!(
                    pad = %pad.name(),
                    "parking a resurrected video pad the dispatched selection has off"
                );
                let (sink, park) = inner.park_stream(pad)?;
                (None, None, None, Some(park), Some(sink))
            }
            // A video pad re-routed for a group whose end has already
            // entered streamsynchronizer cannot preroll a fresh chain. The
            // stream's own end was consumed before the re-route (its EOS
            // either passed with the old chain or died with the dropped
            // slot), so the new branch will never see a buffer or an EOS.
            // The sink parks in Ready->Paused forever, the async transition
            // never completes, the receiver's pump gate never opens, and a
            // sibling EOS arriving after the splice parks its streaming
            // thread inside gst_stream_synchronizer_wait against the fresh
            // pad (fuzz_buffering seed 1600058, gdb captured: multiqueue
            // src in gststreamsynchronizer.c:480 behind the resurrected
            // pad, the video source task idle at EOS, see the findings
            // entry on the drained-resurrect park). This also enforces the
            // invariant the sibling-pass gate below states, that once one
            // EOS of a group entered ssync the group MUST complete, which a
            // fresh EOS-less pad makes impossible.
            //
            // Parking trades the permanent wedge for a silent video slot on
            // the drained remainder of the item. A flushing seek restarts
            // the streams and clears the mirror (see `Job::Seek`), so a
            // re-enable after a seek still builds the chain normally.
            // Lever: `FCAST_NO_DRAINED_RESURRECT_PARK` restores the
            // unconditional rebuild (the seek-side clear is gated on the
            // same lever).
            StreamKind::Video
                if std::env::var_os("FCAST_NO_DRAINED_RESURRECT_PARK").is_none() && {
                    // Two signals, either one marks the stream drained.
                    // The group mirror covers a stream whose end reached
                    // the OUTPUT before the re-route. The input-side set
                    // covers a stream whose end was consumed invisibly
                    // (its slot was dropped while deselected, so no output
                    // probe ever saw it, fuzz_buffering seed 1600008).
                    let sticky = pad.sticky_event::<gst::event::StreamStart>(0);
                    let group = sticky.as_ref().and_then(|event| event.group_id());
                    let group_passing =
                        group.is_some() && group == *inner.passing_eos_group.lock();
                    let sid = sticky.map(|event| event.stream_id().to_string());
                    let input_drained = sid
                        .as_ref()
                        .is_some_and(|sid| inner.input_eos_sids.lock().contains(sid));
                    let rerouted = inner.video_unrouted_once.load(Ordering::SeqCst);
                    debug!(
                        pad = %pad.name(),
                        ?sid,
                        group_passing,
                        input_drained,
                        rerouted,
                        "drained-resurrect check"
                    );
                    (group_passing || input_drained) && rerouted
                } =>
            {
                info!(
                    pad = %pad.name(),
                    "parking a re-routed video pad of a drained stream"
                );
                let (sink, park) = inner.park_stream(pad)?;
                (None, None, None, Some(park), Some(sink))
            }
            StreamKind::Video => {
                let (ss_sink, ss_src) = attach_ssync()?;
                if deferred_join {
                    // Membership only: the chain has to be in the pipeline for
                    // the link below to be legal, but its ACTIVATION is what
                    // blocks, and that goes to `fpb-join`.
                    inner.attach_video_chain()?;
                } else {
                    // Put the video chain into the pipeline (also the recovery
                    // from a mid-item deselect's parked chain).
                    inner.ensure_video_chain()?;
                }
                let entry = inner
                    .overlay
                    .static_pad("video_sink")
                    .ok_or_else(|| anyhow!("subtitleoverlay video_sink missing"))?;
                ss_src.link(&entry).context("linking video chain")?;
                if deferred_join {
                    // AFTER the link, which cannot race: the only thread that
                    // ever pushes on `ss_src` is the one inside this call.
                    join_hold = Inner::hold_chain_entry(&ss_src);
                    queue_join = true;
                } else {
                    inner.finish_preroll_token();
                }
                (Some(ss_sink), Some(ss_src), Some(entry), None, None)
            }
            StreamKind::Audio => {
                let (ss_sink, ss_src) = attach_ssync()?;
                if !deferred_join {
                    // Build this load's fresh audio sink (see
                    // `Inner::audio`). The prefix is already active.
                    inner.ensure_audio_sink()?;
                }
                let entry = inner
                    .audio_entry
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("fpb-aqueue sink missing"))?;
                ss_src.link(&entry).context("linking audio chain")?;
                if deferred_join {
                    join_hold = Inner::hold_chain_entry(&ss_src);
                    queue_join = true;
                } else {
                    inner.finish_preroll_token();
                }
                (Some(ss_sink), Some(ss_src), Some(entry), None, None)
            }
            StreamKind::Text => {
                // BYPASS streamsynchronizer (see `RoutedStream`): link the
                // decodebin3 text pad straight to its parking sink. Text
                // joins subtitleoverlay only via `poll_text_policy`, and
                // until then it drains into the parking sink (it must be
                // consumed, see `RoutedStream::park_pad`).
                let (sink, park) = inner.park_stream(pad)?;
                (None, None, None, Some(park), Some(sink))
            }
        };

        // The post-streamsynchronizer half of the gapless EOS hold. An EOS
        // that entered streamsynchronizer (it slipped the output gate below
        // before a pre-arm armed it, see `Inner::passing_eos_group`) parks
        // its pushing thread there until the whole group is EOS, and
        // streamsynchronizer then re-emits EOS on every src pad. Those must
        // still not reach the sinks while a swap is in flight: drop them
        // here under the same conditions as the output gate. At a true end
        // of playback nothing is pending, the pad's group matches the
        // active one, and the EOS flows to the sinks normally.
        if let Some(ss_src) = &ssync_src {
            let weak = Arc::downgrade(inner);
            ss_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(gst::PadProbeData::Event(event)) = &info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                if !matches!(event.view(), gst::EventView::Eos(_)) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some(inner) = weak.upgrade() else {
                    return gst::PadProbeReturn::Ok;
                };
                let pad_group = pad
                    .sticky_event::<gst::event::StreamStart>(0)
                    .and_then(|event| event.group_id());
                let (should_drop, pending, behind) = inner.gapless_eos_check_and_mark(pad_group);
                if should_drop {
                    debug!(
                        pad = %pad.name(),
                        pending,
                        behind,
                        "gapless: dropping a drained EOS after streamsynchronizer"
                    );
                    return gst::PadProbeReturn::Drop;
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Gapless EOS hold (the uridecodebin3 db_src_probe): while a
        // prepared next item is PENDING, any EOS coming out of decodebin3
        // is the drained current item's and must not reach the sinks (it
        // would end the pipeline between items). Activation clears the
        // pending state, so the new item's own end flows normally; a
        // straggler old-slot EOS emerging just after activation is
        // absorbed by streamsynchronizer (the other streams are active
        // again by definition of the activation trigger).
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
            let weak = Arc::downgrade(inner);
            move |pad, info| {
                let Some(gst::PadProbeData::Event(event)) = &info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                let group = match event.view() {
                    gst::EventView::Eos(_) => None,
                    gst::EventView::StreamStart(stream_start) => Some(stream_start.group_id()),
                    _ => return gst::PadProbeReturn::Ok,
                };
                let Some(inner) = weak.upgrade() else {
                    return gst::PadProbeReturn::Ok;
                };
                match group {
                    None => {
                        // Drop the EOS while a swap is pending or while
                        // THIS pad still carries a previous group (see
                        // `Inner::gapless_eos_check_and_mark`). The pad's
                        // sticky stream-start at EOS time is the ending
                        // stream's, an authoritative fallback when the
                        // recorded group is missing.
                        let pad_group = {
                            let routing = inner.routing.lock();
                            routing
                                .routed
                                .iter()
                                .find(|r| &r.db3_src_pad == pad)
                                .and_then(|r| r.group)
                        }
                        .or_else(|| {
                            pad.sticky_event::<gst::event::StreamStart>(0)
                                .and_then(|event| event.group_id())
                        });
                        let av = matches!(kind, StreamKind::Video | StreamKind::Audio);
                        // Group consistency with streamsynchronizer: once
                        // one EOS of this group passed into ssync, its
                        // siblings MUST follow or ssync never completes the
                        // group and its parked thread (the multiqueue slot
                        // task of the stream that EOSed first) never wakes.
                        // The post-ssync gate consumes them instead (see
                        // `Inner::passing_eos_group`).
                        if av && pad_group.is_some() && pad_group == *inner.passing_eos_group.lock()
                        {
                            debug!(
                                pad = %pad.name(),
                                "gapless: passing a sibling EOS through to complete the group"
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                        let (should_drop, pending, behind) =
                            inner.gapless_eos_check_and_mark(pad_group);
                        if should_drop {
                            debug!(
                                pad = %pad.name(),
                                pending,
                                behind,
                                "gapless: dropping the drained item's EOS"
                            );
                            return gst::PadProbeReturn::Drop;
                        }
                        // This EOS enters streamsynchronizer. Commit the
                        // whole group to passing so a pre-arm landing
                        // between now and the siblings' EOS cannot strand
                        // the group half-ended in ssync.
                        if av && let Some(group) = pad_group {
                            *inner.passing_eos_group.lock() = Some(group);
                        }
                    }
                    // STREAM_START: record the pad's group; a group change
                    // activates a pending prepared item.
                    Some(group) => {
                        if let Some(group) = group {
                            let mut routing = inner.routing.lock();
                            if let Some(routed) =
                                routing.routed.iter_mut().find(|r| &r.db3_src_pad == pad)
                            {
                                routed.group = Some(group);
                            }
                        }
                        inner.note_output_stream_start(group);
                    }
                }
                gst::PadProbeReturn::Ok
            }
        });

        debug!(pad = %pad.name(), ?kind, linked = downstream.is_some(), "routed decodebin3 pad");
        // Seed the pad's group from its sticky stream-start: the live event
        // may already have passed (it flows the moment the link above
        // completes, possibly before the probe existed). The sticky is
        // authoritative either way.
        let group = pad
            .sticky_event::<gst::event::StreamStart>(0)
            .and_then(|event| event.group_id());
        if let Some(group) = group {
            let mut active = inner.active_group.lock();
            if active.is_none() {
                *active = Some(group);
            }
        }
        let mut routing = inner.routing.lock();
        routing.routed.push(RoutedStream {
            db3_src_pad: pad.clone(),
            ssync_sink,
            ssync_src,
            downstream,
            park_pad,
            park_sink,
            tqueue: None,
            group,
            seat_evicted: false,
            kind,
        });
        drop(routing);

        // A new text stream may be linkable right away, and a (re)arriving video
        // stream may unblock a parked one. A DEFERRED video join runs it on
        // `fpb-join` instead, after the chain is up: text must not be spliced
        // into an overlay that is still parked at READY, and this call is
        // itself pipeline surgery that has no business on a streaming thread.
        let poll_here = match kind {
            StreamKind::Text => true,
            StreamKind::Video => !deferred_join,
            StreamKind::Audio => false,
        };
        if poll_here {
            Inner::poll_text_policy(inner);
        }
        // Last, so the routing entry above is already visible: the join
        // re-validates against it. The joiner would block on the route gate
        // this function still holds anyway, but nothing here should depend on
        // that.
        if queue_join {
            Inner::queue_chain_join(inner, &ssync_owner, pad, kind, join_hold);
        }
        Ok(())
    }

    /// Hold a routed stream at the streamsynchronizer src pad until its chain
    /// is activated (see [`ChainJoinJob`]).
    ///
    /// A BLOCKING probe, not a DROP one: no buffer may be lost, and the pad
    /// this blocks is DOWNSTREAM of decodebin3, so the streaming thread parked
    /// here holds no decodebin3 lock at all (`mq_slot_handle_stream_start`
    /// releases SELECTION_LOCK before the push continues, gstdecodebin3.c
    /// `beach:`). It holds the multiqueue src pad's and the ssync sink pad's
    /// stream locks, and nothing the join does needs either.
    ///
    /// Three independent things release it, so the postponed work cannot
    /// outlive its drain: the join itself, any flush (`FLUSH_START` broadcasts
    /// the block cond and is never itself blocked, gstpad.c
    /// `gst_pad_push_event_unchecked`), and any deactivation of the pad (a
    /// downward transition, or `release_request_pad`, which streamsynchronizer
    /// deliberately does src-pad-first for exactly this reason).
    fn hold_chain_entry(ss_src: &gst::Pad) -> Option<(gst::Pad, gst::PadProbeId)> {
        let id = ss_src.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
            gst::PadProbeReturn::Ok
        })?;
        debug!(pad = %ss_src.name(), "holding a routed stream until its chain joins");
        Some((ss_src.clone(), id))
    }

    /// Queue the blocking half of a route (see [`ChainJoinJob`]). A failed
    /// send means the crate is going away, so the hold comes off here instead:
    /// nothing would ever release it.
    fn queue_chain_join(
        inner: &Arc<Inner>,
        db3: &gst::Element,
        pad: &gst::Pad,
        kind: StreamKind,
        hold: Option<(gst::Pad, gst::PadProbeId)>,
    ) {
        let job = ChainJoinJob {
            db3: db3.clone(),
            pad: pad.clone(),
            kind,
            hold,
        };
        if let Err(err) = inner.join_tx.send(job) {
            warn!("the chain joiner is gone; joining inline");
            let ChainJoinJob { hold, kind, .. } = err.0;
            let joined = match kind {
                StreamKind::Video => inner.ensure_video_chain(),
                StreamKind::Audio => inner.ensure_audio_sink(),
                StreamKind::Text => Ok(()),
            };
            if let Err(err) = joined {
                warn!(?err, ?kind, "failed to join a chain after a lost join job");
            }
            if let Some((pad, id)) = hold {
                pad.remove_probe(id);
            }
        }
    }

    /// Activate a routed stream's chain and let it flow (see
    /// [`ChainJoinJob`]). Runs on `fpb-join`, where BLOCKING IS ALLOWED.
    fn run_chain_join(inner: &Arc<Inner>, job: ChainJoinJob) {
        let ChainJoinJob {
            db3,
            pad,
            kind,
            hold,
        } = job;
        {
            // NOT the route gate (see `Inner::join_gate`): a chain must not
            // be activated into a descending pipeline, but a route must never
            // have to wait for a join. Blocking here is what this thread is
            // for.
            let _gate = Inner::join_hold(inner);
            // Re-checked UNDER the gate, because between the route and this
            // join the core can have been swapped, the stream deselected
            // again, or the pipeline taken down. Activating a chain for any of
            // those strands a sink in the pipeline with nothing to feed it,
            // which is the async-forever wedge `remove_video_chain` exists to
            // avoid. The hold still comes off below either way.
            let current_core = inner.core.lock().as_ref().is_some_and(|c| c.db3 == db3);
            let still_routed = inner
                .routing
                .lock()
                .routed
                .iter()
                .any(|r| r.db3_src_pad == pad);
            let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
            let accepting = decisions::pad_accepting(current, pending);
            if !current_core || !still_routed || !accepting {
                debug!(
                    pad = %pad.name(), ?kind, current_core, still_routed, ?current, ?pending,
                    "dropping a stale chain join"
                );
            } else {
                let joined = match kind {
                    StreamKind::Video => inner.ensure_video_chain(),
                    StreamKind::Audio => inner.ensure_audio_sink(),
                    StreamKind::Text => Ok(()),
                };
                match joined {
                    // The load's preroll is carried by a real sink from here
                    // on, exactly as it was when this ran inline.
                    Ok(()) => inner.finish_preroll_token(),
                    Err(err) => warn!(?err, ?kind, pad = %pad.name(), "failed to join a chain"),
                }
            }
            // Under the gate, so the chain is up and flowing as one step as
            // far as any teardown is concerned.
            if let Some((pad, id)) = hold {
                debug!(pad = %pad.name(), "releasing a joined stream");
                pad.remove_probe(id);
            }
            // The overlay is only now out of its parked state, so this is
            // where a re-arriving video stream can take text back (see the
            // tail of `route_db3_pad`).
            if kind == StreamKind::Video {
                Inner::poll_text_policy(inner);
            }
        }
    }

    /// A decodebin3 output pad went away (stream deselected or input
    /// removed): unlink and release its streamsynchronizer pads.
    fn unroute_db3_pad(inner: &Arc<Inner>, pad: &gst::Pad) {
        let mut routing = inner.routing.lock();
        let Some(idx) = routing.routed.iter().position(|r| &r.db3_src_pad == pad) else {
            return;
        };
        let routed = routing.routed.remove(idx);
        drop(routing);

        let mut routed = routed;
        if routed.kind == StreamKind::Text {
            // This callback runs on a streaming thread, where the disposal's
            // blocking flush is forbidden, so it is handed to the worker.
            // The unlink itself still happens here and does not block. The
            // lever restores the previous inline dispatch for interleaved
            // A/B measurement and gates this whole change.
            let defer_disposal = std::env::var_os("FCAST_INLINE_UNROUTE_DISPOSAL").is_none();
            Inner::detach_text_from_overlay(inner, &mut routed, defer_disposal);
        } else if let (Some(ssync_src), Some(downstream)) = (&routed.ssync_src, &routed.downstream)
        {
            let _ = ssync_src.unlink(downstream);
        }
        inner.unpark_stream(&mut routed);
        // A/V held a streamsynchronizer request pad. Text bypassed ssync and
        // has none. Unlink and release only when present.
        if let Some(ssync_sink) = &routed.ssync_sink {
            let _ = pad.unlink(ssync_sink);
            // Release against the pad's OWN streamsynchronizer: after a core
            // swap this stream belongs to the previous instance.
            if let Some(ssync) = ssync_sink.parent_element() {
                ssync.release_request_pad(ssync_sink);
            }
        }
        debug!(pad = %pad.name(), kind = ?routed.kind, "unrouted decodebin3 pad");

        // Text is consumed synchronized against VIDEO buffers, so a text
        // stream that stays in the overlay after video stops can never
        // drain and blocks decodebin3's reconfiguration until the next
        // flush. Park overlay-linked text when video unroutes, and the policy
        // brings it back once a video stream is routed again.
        if routed.kind == StreamKind::Video {
            // Read by the drained-resurrect park in `route_db3_pad`.
            inner.video_unrouted_once.store(true, Ordering::SeqCst);
            // Both halves are pipeline surgery and this runs on decodebin3's
            // pad-removed callback, so they go to the worker. See
            // [`Job::VideoChainGone`] for the wedge that taught us.
            //
            // A/B lever for bisecting regressions without a rebuild, like
            // FCAST_NO_SELECTION_REPLAY.
            if std::env::var_os("FCAST_INLINE_VIDEO_CHAIN_TEARDOWN").is_some() {
                Inner::park_text_streams(inner);
                inner.remove_video_chain();
            } else {
                let _ = inner.work_tx.send(Job::VideoChainGone);
            }
        }
    }

    /// Take a live text stream out of the overlay: wake blocked pushes with
    /// a flush, unlink, and drop its queue.
    ///
    /// The flush BEFORE the unlink is load-bearing: textoverlay prefetches
    /// the next cue and BLOCKS that push waiting for video to reach the
    /// cue's timestamp. If video is stopping, that wait never releases, the
    /// text pad never idles, and decodebin3's IDLE-probe deactivation hangs
    /// (the same deadlock class as playsink's text-chain teardown). The
    /// flush pair travels through the queue into the overlay, wakes the
    /// push (FLUSHING is fine, the stream is leaving) and clears the
    /// lingering cue.
    fn detach_text_from_overlay(
        inner: &Arc<Inner>,
        routed: &mut RoutedStream,
        defer_disposal: bool,
    ) {
        let Some(downstream) = routed.downstream.take() else {
            return;
        };
        let tqueue = routed.tqueue.take();
        Inner::detach_text_parts(
            inner,
            &routed.db3_src_pad,
            &downstream,
            tqueue,
            defer_disposal,
        );
    }

    /// The teardown half of [`Inner::detach_text_from_overlay`], split out so
    /// a caller holding the routing lock can take the pads out of the entry,
    /// RELEASE the lock, and only then run this.
    ///
    /// It must not run under that lock. The flush and the NULL both block on
    /// pad stream locks held by streaming threads that are themselves waiting
    /// on routing (see [`Inner::live_text_downstream_pads`]).
    /// `defer_disposal` forces the blocking disposal onto the worker no
    /// matter the pipeline state. Passed by the pad-removed path, which runs
    /// on a streaming thread where the disposal's flush is forbidden (the
    /// crate rule stated on [`Job::FinishActivation`]). Every other caller
    /// passes false and keeps the resting-PAUSED postponement exactly as it
    /// was.
    fn detach_text_parts(
        inner: &Arc<Inner>,
        db3_src_pad: &gst::Pad,
        downstream: &gst::Pad,
        tqueue: Option<gst::Element>,
        defer_disposal: bool,
    ) {
        // UNLINKING FIRST, and it does not block. `gst_pad_unlink` needs only
        // the two pads' object locks, never a stream lock, so it works even
        // while the branch's task is stuck inside subtitleoverlay. Taking the
        // branch out of the graph immediately is the part the gapless
        // transition depends on, which is why postponing the whole park
        // regressed it (15 failures in 22 runs against 11 in 22, see
        // fuzz-campaign-findings.md).
        //
        // WHAT THE BRANCH'S OWN QUEUE DOES WHILE IT IS BEING TAKEN APART. The
        // unlink below leaves `tqueue`'s src pad without a peer, and the next
        // `gst_queue_loop` push into it returns GST_FLOW_NOT_LINKED, on which
        // the queue posts "Internal data stream error" and the run fails
        // (`fuzz_buffering` seed 600009, iteration 1 step 5, measured at 5 runs
        // in 20). The waking flush only reaches it later, in
        // `dispose_text_branch`, and can be postponed to a state edge, so
        // there is a window of arbitrary length where the queue is running
        // into a hole.
        //
        // A DROP probe closes it. gstpad.c runs push probes BEFORE the peer
        // lookup, so a dropped buffer returns GST_FLOW_OK and the queue never
        // sees NOT_LINKED. Nothing is lost: everything this queue still holds
        // belongs to a branch that is being disposed of, and the disposal
        // flushes it away regardless.
        //
        // NOT the flush the older note in `park_video_chain_for_deselect`
        // implies. Sending FLUSH_START to `downstream` ahead of the unlink
        // would work through `gst_queue_handle_sink_event`, which forwards the
        // event and then calls `gst_pad_pause_task` on the src pad, waiting for
        // a stream lock the queue's own blocked push holds. That is the exact
        // deadlock `tests/teardown_races.rs` is built around, and it would put
        // it at the one call site whose non-blocking property the gapless
        // transition depends on. `gst_pad_add_probe` takes only the pad's
        // object lock and returns at once, so this keeps that property. It also
        // avoids the hazard a flush here would create, of leaving the overlay's
        // `subtitle_sink` (still linked at this instant) flushing until a
        // disposal that a resting PAUSED can defer indefinitely.
        // WHAT THE DROP PROBE CANNOT COVER: a push ALREADY blocked inside the
        // tqueue's CHAIN function, waiting for room. The probe sits on the
        // queue's src pad and the flush that eventually lands on its sink pad
        // sets `srcresult = FLUSHING` and wakes that chain call, which then
        // returns GST_FLOW_FLUSHING (`gstqueue.c` `out_flushing`) into
        // decodebin3's multiqueue loop. That latches the single-queue for good
        // and adaptivedemux2's ONE output task pauses with nothing posted:
        // FREEZE-DIAGN.md sections 1 and 5. No probe can help, the push is
        // past every probe point by then.
        //
        // Raising the limit DOES help, and it is the documented purpose of the
        // signal: `gst_queue_set_property` -> `queue_capacity_change` signals
        // `item_del` precisely so "the _chain function ... might have more room
        // now". So the blocked chain wakes with `srcresult` still OK, enqueues,
        // and returns GST_FLOW_OK.
        //
        // ONLY max-size-TIME, which is the dimension that actually blocks here:
        // this queue reports itself full on dead air between sparse cues
        // (`gst_queue_apply_gap` advances the time level off GAP events) while
        // holding zero buffers and zero bytes, which is the whole reason the
        // push is parked. Lifting all three instead was a real defect: the queue
        // then accepts unboundedly, materially more of the external's data is
        // pulled through its own chain inside the detach window, and that
        // exposes an unguarded NOT_LINKED there. `fuzz_scenarios` seed 500002
        // iteration 1 step 6 (detach of a SELECTED external right after a seek)
        // failed 8 of 11 runs with all three lifted and 0 of 7 without, on "the
        // pipeline posted an error: Internal data stream error" attributed to
        // the external's own uri. The buffer and byte caps stay at their
        // defaults, so the data volume in flight is unchanged from before this
        // wake existed. Lever: `FCAST_NO_TQUEUE_UNCAP_ON_DETACH`.
        //
        // And only on an ADAPTIVE main input, which is where the hazard this
        // wake exists for lives: adaptivedemux2 serves every track from ONE
        // output loop and pauses it for good on a FLUSHING return, killing the
        // whole item (UPSTREAM-GSTREAMER-ISSUES.md D4). A non-adaptive demuxer
        // loses only the flushing stream to the multiqueue latch (D3), which is
        // the status quo this wake never had to change, and time-only still cost
        // seed 500002 one failure in seven there.
        if let Some(tqueue) = &tqueue
            && inner.upstream_owns_selection()
            && std::env::var_os("FCAST_NO_TQUEUE_UNCAP_ON_DETACH").is_none()
        {
            debug!(tqueue = %tqueue.name(), "lifting the text queue's time cap to wake a parked push");
            tqueue.set_property("max-size-time", 0u64);
        }

        if let Some(tqueue) = &tqueue
            && std::env::var_os("FCAST_NO_DETACH_DROP_PROBE").is_none()
            && let Some(qsrc) = tqueue.static_pad("src")
        {
            // Never removed: this pad goes to NULL and leaves the pipeline
            // with its queue in `dispose_text_branch`.
            qsrc.add_probe(gst::PadProbeType::DATA_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Drop
            });
        }

        // Text bypasses ssync, so its source is the decodebin3 pad itself.
        let _ = db3_src_pad.unlink(downstream);
        if let Some(tqueue) = &tqueue {
            // The overlay's subtitle input must not stay wired without a
            // live stream: stale caps/renderer state (e.g. a VOBSUB dvdspu
            // splice) wedges the next load's preroll.
            if let Some(qsrc) = tqueue.static_pad("src")
                && let Some(peer) = qsrc.peer()
            {
                let _ = qsrc.unlink(&peer);
            }
        }

        // The rest CAN block, so it is postponed at a pipeline resting in
        // PAUSED, exactly like the eager flush in `pump_selection`. The flush
        // pauses the queue's task, and that task is stuck pushing into
        // subtitleoverlay behind sinks parked in `gst_base_sink_wait_preroll`.
        // Turning subtitles off while paused wedged the caller, and detaching
        // an external subtitle wedged the worker along with every job behind
        // it.
        //
        // The visible cost of postponing is that a cue already composited into
        // the frozen frame stays on screen until playback resumes, at which
        // point the deferred flush clears it. A stale cue on a paused frame is
        // a great deal better than a receiver that stops responding.
        let disposal = TextDisposal {
            downstream: downstream.clone(),
            tqueue,
        };
        if (defer_disposal || Inner::resting_paused(&inner.pipeline))
            && std::env::var_os("FCAST_NO_TEXT_WORK_DEFERRAL").is_none()
        {
            debug!(
                defer_disposal,
                "postponing a text branch disposal off the calling thread"
            );
            // A new postponed item invalidates the last drain's no-op
            // verdict (see `Inner::drain_poke_parked`).
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
            inner.deferred_text_disposal.lock().push(disposal);
            // The resting-PAUSED postponement waits for a state edge by
            // design (the drain cannot complete below PLAYING anyway). A
            // disposal handed over by the pad-removed path has no edge
            // coming, so poke the worker the way the flush intent does.
            if defer_disposal {
                let _ = inner.work_tx.send(Job::DrainTextWork);
            }
        } else {
            // The last silent FLUSHING injector: this runs at settled PLAYING,
            // i.e. on every mid-play park or replace, and it sends flush pairs
            // to the overlay's subtitle_sink and to the branch queue. The
            // deferred paths above log; this one did not, which made a field
            // log ambiguous when an adaptive demuxer discarded a FLUSHING on a
            // dash text track with no crate line anywhere near it.
            debug!(
                downstream = %disposal.downstream.name(),
                tqueue = ?disposal.tqueue.as_ref().map(|q| q.name().to_string()),
                "disposing of a text branch inline"
            );
            inner.dispose_text_branch(disposal);
        }
    }

    /// Whether this external's stream is advertised but has NO decodebin3
    /// output slot, which no amount of replaying can change.
    ///
    /// decodebin3 REMOVES a multiqueue slot when the input feeding it drains:
    /// `remove_slot_from_streaming_thread` on EOS at the slot's src pad
    /// (gstdecodebin3.c:3717, whose own FIXME notes the removal is async), and
    /// slot creation only ever happens from a parsebin `pad-added`. An external
    /// subtitle's stream reaches EOS as soon as its last cue is parsed, which
    /// for a subtitle file is minutes into a feature-length item, and its
    /// parsebin pad never appears again. From then on the stream is in the
    /// collection, selectable by the caller, and SLOTLESS: a replay's data
    /// arrives at a pad whose multiqueue peer is gone and the source dies
    /// "streaming stopped, reason not-linked".
    ///
    /// Detected from two crate-side facts, no decodebin3 internals: none of the
    /// routed streams carries one of this input's text sids, and the crate's own
    /// EOS probe recorded that sid as drained.
    /// Never holds `input_eos_sids` and `routing` at the same time: the EOS
    /// probe that writes the first runs on streaming threads.
    fn external_stream_slotless(&self, id: ExternalSubId, epoch: u32) -> bool {
        let drained = self.input_eos_sids.lock().clone();
        let routing = self.routing.lock();
        let Some(input) = routing.inputs.iter().find(|i| {
            i.external
                .as_ref()
                .is_some_and(|e| e.id == id && e.epoch == epoch)
        }) else {
            return false;
        };
        let sids = input.text_stream_ids();
        if sids.is_empty() {
            // Nothing advertised yet: the materialization watchdog owns this.
            return false;
        }
        sids.iter().all(|sid| {
            drained.contains(sid)
                && !routing
                    .routed
                    .iter()
                    .any(|routed| routed.db3_src_pad.stream_id().as_deref() == Some(sid.as_str()))
        })
    }

    /// Whether the pipeline has come to rest at PAUSED, where a flush of the
    /// text branch cannot complete. See [`Inner::detach_text_parts`].
    fn resting_paused(pipeline: &gst::Pipeline) -> bool {
        let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
        current == gst::State::Paused && pending == gst::State::VoidPending
    }

    /// The blocking half of a text detach: wake anything parked in the branch
    /// and drop its queue.
    fn dispose_text_branch(&self, disposal: TextDisposal) {
        Self::dispose_text_branch_on(&self.overlay, &self.pipeline, disposal, &self.text_seat);
    }

    /// [`Inner::dispose_text_branch`] against handles rather than `&self`, so
    /// the teardown boundary can run it after `Inner` is gone (see
    /// [`Teardown`]).
    fn dispose_text_branch_on(
        overlay: &gst::Element,
        pipeline: &gst::Pipeline,
        disposal: TextDisposal,
        seat: &TextSeat,
    ) {
        // A cue push that was already parked inside textoverlay when the
        // branch was severed cannot be reached THROUGH the branch any more
        // (both ends are unlinked), and it holds the queue's stream lock,
        // so the queue flush below would wait on it forever. The overlay's
        // own subtitle_sink static pad is the one remaining path to that
        // push, and its FLUSH_START needs no stream lock. Only while the
        // pad is unlinked, a linked pad belongs to a LIVE branch (a replace
        // already relinked) and flushing it would drop the new track's
        // data.
        //
        // The check and the pair are ONE critical section against the link
        // that can make the pad linked between them (see [`Inner::text_seat`]
        // for what losing that race costs). The seat lock is safe to hold
        // across the pair: FLUSH_START is not serialized, so it takes no
        // stream lock and wakes the parked push, and the wake path (an
        // orphaned queue's loop task returning FLUSHING through a DROP probe)
        // takes no crate lock at all. Lever: `FCAST_NO_TEXT_SEAT_LOCK`.
        {
            let _seat = seat.hold();
            if let Some(pad) = overlay.static_pad("subtitle_sink")
                && !pad.is_linked()
            {
                let _ = pad.send_event(gst::event::FlushStart::new());
                let _ = pad.send_event(gst::event::FlushStop::new(true));
            }
        }
        let _ = disposal
            .downstream
            .send_event(gst::event::FlushStart::new());
        let _ = disposal
            .downstream
            .send_event(gst::event::FlushStop::new(true));
        if let Some(tqueue) = disposal.tqueue {
            let _ = tqueue.set_state(gst::State::Null);
            let _ = pipeline.remove(&tqueue);
        }
    }

    /// Run every pending branch disposal NOW, whatever the pipeline state.
    /// For the teardown boundaries only (stop and drop), which run
    /// [`Inner::flush_parked_text_pushes`] next. A postponed branch is
    /// invisible to that flush (its pads live only in the disposal list),
    /// and the decodebin3-sink flush there pauses a multiqueue task that
    /// may be blocked mid-push into the orphaned full queue, so the
    /// disposals MUST drain first or the teardown wedges on work it cannot
    /// see. Captured with gdb from the field sequence pause, subtitles
    /// off, stop.
    fn drain_disposals_for_teardown(&self) {
        let disposals = std::mem::take(&mut *self.deferred_text_disposal.lock());
        for disposal in disposals {
            debug!("disposing of a postponed text branch at teardown");
            self.dispose_text_branch(disposal);
        }
    }

    /// Move overlay-linked text streams back to the parking sink (video
    /// going away, or subtitles dropped). See `detach_text_from_overlay`.
    fn park_text_streams(inner: &Arc<Inner>) {
        // The pads come out of the entries under the lock, everything that
        // touches the pipeline runs with the lock released (see
        // `Inner::live_text_downstream_pads` for the deadlock this avoids).
        let detached: Vec<(gst::Pad, gst::Pad, Option<gst::Element>)> = {
            let mut routing = inner.routing.lock();
            routing
                .routed
                .iter_mut()
                .filter(|r| r.kind == StreamKind::Text && r.downstream.is_some())
                .map(|routed| {
                    (
                        routed.db3_src_pad.clone(),
                        routed.downstream.take().expect("filtered on Some above"),
                        routed.tqueue.take(),
                    )
                })
                .collect()
        };
        for (db3_src_pad, downstream, tqueue) in detached {
            Inner::detach_text_parts(inner, &db3_src_pad, &downstream, tqueue, false);
            let parked = inner.park_stream(&db3_src_pad);
            let orphaned = {
                let mut routing = inner.routing.lock();
                match routing
                    .routed
                    .iter_mut()
                    .find(|r| r.db3_src_pad == db3_src_pad)
                {
                    Some(routed) => {
                        match parked {
                            Ok((sink, park)) => {
                                debug!(pad = %db3_src_pad.name(), "parked text stream");
                                routed.park_sink = Some(sink);
                                routed.park_pad = Some(park);
                            }
                            Err(err) => warn!(?err, "failed to park the text stream"),
                        }
                        None
                    }
                    // The stream unrouted while the lock was down, so its
                    // parking sink has nothing left to belong to.
                    None => parked.ok(),
                }
            };
            if let Some((sink, park)) = orphaned {
                let _ = db3_src_pad.unlink(&park);
                let _ = sink.set_state(gst::State::Null);
                let _ = inner.pipeline.remove(&sink);
            }
        }
    }

    /// Link any routed-but-unlinked text stream into subtitleoverlay, once
    /// the pipeline is SETTLED (at least PAUSED, no async transition
    /// pending) and a video stream is routed (text is consumed against video
    /// buffers, see `park_text_streams`). Driven by the caller's
    /// state-change / streams-selected handlers, an event rather than a
    /// poll.
    ///
    /// The `pending == VoidPending` requirement is load-bearing: splicing
    /// the subtitleoverlay text branch into a load's async preroll adds a
    /// reconfiguration that wedges it under churn. Linking at a SETTLED
    /// PAUSED is safe and necessary: a subtitle switch performed while
    /// paused never reaches PLAYING before the caller's re-emit flush, so
    /// requiring PLAYING would leave the new track's cue invisible until
    /// resume. The idle-video-block gst patch is what makes the branch
    /// reconfiguration reliable at steady PAUSED.
    fn poll_text_policy(inner: &Arc<Inner>) {
        // A settle point is a drain point too, but the drain EXECUTES only on
        // the worker. This poll runs on the caller's event loop, and the
        // drain's flush blocks until the branch's streaming thread can be
        // paused, which is the caller wedge this whole mechanism exists to
        // prevent.
        //
        // The poke is skipped while the last drain's no-op verdict stands
        // (see `Inner::drain_poke_parked`). Without that, a caller polling
        // every 5ms at a pipeline parked below PLAYING re-queued this job on
        // every poll, indefinitely, and the worker spent the whole park
        // spinning on drains that could only early-return (measured at 5099
        // jobs over one 39-second Buffering park on the fuzz driver's seed
        // 400009). The pipeline state edges still queue the drain
        // unconditionally on the bus, so a settled PLAYING still drains
        // promptly with no poll at all. The lever restores the
        // poke-on-every-poll behavior for interleaved A/B measurement and
        // gates this whole change, since the flag it ignores is written
        // nowhere else that behavior can reach.
        if inner.has_deferred_text_work()
            && (!inner.drain_poke_parked.load(Ordering::SeqCst)
                || std::env::var_os("FCAST_NO_DRAIN_POKE_SUPPRESS").is_some())
        {
            let _ = inner.work_tx.send(Job::DrainTextWork);
        }
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::text_may_link(current, pending) {
            return;
        }
        // Before anything links: a gapless boundary leaves the text branch on a
        // different running-time origin than A/V, and a branch linked onto the
        // wrong origin never renders (see there). This is the trigger for a text
        // stream that JOINS after the boundary, since `route_db3_pad` calls us
        // when its pad appears. A branch already live across the boundary never
        // comes through here; that one is trigger #2, the `video_sink` probe.
        inner.sync_text_running_time();
        // A re-attached external can materialize under the very stream id
        // the engine already has applied (stream ids are URI-derived, so
        // detaching a URL and attaching it again reuses the id). The engine
        // then has nothing to dispatch, no SELECT_STREAMS confirmation ever
        // arrives, and the confirmation-time hold release never runs. The
        // input's data stayed blocked at its source pads for good while the
        // selection reported success. Lift the hold here instead whenever
        // the held stream IS the confirmed subtitle (a cheap no-op when
        // nothing is held).
        let confirmed = inner.last_applied_subtitle.lock().clone();
        if let Some(confirmed) = confirmed {
            // Nothing is owed here: this path exists precisely because no
            // `STREAMS_SELECTED` arrived, so no replay was queued against it.
            inner.unblock_selected_externals(std::slice::from_ref(&confirmed), None);
        }
        // Upstream-selection mode has NO confirmation channel for an
        // external's sid at all (decodebin3 never posts there, the demuxer
        // cannot name a foreign stream), so the confirmed release above can
        // never fire for one. The engine's applied slot is the same authority
        // that gates the join below; it drives the hold too, adopted as the
        // confirmed slot, with the realigning replay the confirmation path
        // would have queued. Lever: `FCAST_NO_UPSTREAM_SELECTION_SPLIT`.
        if inner.upstream_owns_selection() {
            let applied = inner.selection.lock().subtitle_sid();
            let held = applied.and_then(|sid| {
                let routing = inner.routing.lock();
                routing.inputs.iter().find_map(|input| {
                    input.external.as_ref().and_then(|external| {
                        (external.hold_until_selected && input.stream_ids().contains(&sid))
                            .then(|| (external.id, external.epoch, sid.clone()))
                    })
                })
            });
            if let Some((id, epoch, sid)) = held {
                debug!(
                    ?id,
                    %sid,
                    "upstream owns selection; releasing the held external the applied slot names"
                );
                *inner.last_applied_subtitle.lock() = Some(sid.clone());
                let owed = inner
                    .work_tx
                    .send(Job::ReplaySub {
                        id,
                        epoch,
                        attempt: 0,
                    })
                    .is_ok()
                    .then_some((id, epoch));
                inner.unblock_selected_externals(std::slice::from_ref(&sid), owed);
            }
        }
        // Only the selected subtitle stream may relink. A disabled stream
        // stays routed until decodebin3 removes its pad, and relinking it
        // here would resurrect the cue the eager detach just cleared.
        // Snapshot before taking the routing lock.
        //
        // A stream the applied selection names while the DESIRED state is an
        // explicit subtitle-off is decodebin3's collection-default
        // auto-select stomping that off (an attach makes it re-select over
        // the applied state), the TEXT twin of the video resurrect
        // `route_db3_pad` guards against. Splicing it into subtitleoverlay
        // adds a reconfiguration to whatever transition is in flight, that
        // transition never completes, and the receiver's pump gate
        // (`quiet = running && !has_async_transition()`) then holds the
        // engine's corrective re-assert back forever, so the only thing that
        // would undo the stomp is postponed behind the stall the stomp
        // caused. Leaving the stream parked keeps the pipeline settled, and
        // the re-assert dispatches on the next pump and makes decodebin3 drop
        // the pad. `fuzz_buffering` seed 400009, whose schedule ends
        // `disable_subtitles` then `attach_external`. The lever restores the
        // unconditional relink and gates this whole change.
        let allowed_sid = {
            let selection = inner.selection.lock();
            if selection.subtitle_explicitly_off()
                && std::env::var_os("FCAST_LINK_STOMPED_SUBTITLE").is_none()
            {
                None
            } else {
                selection.subtitle_sid()
            }
        };
        // The overlay's one subtitle seat may still be held by a DEAD
        // branch. A detached input's decodebin3 output pad can linger
        // linked into the overlay past `remove_input` (the id stays in the
        // collection while a same-id stream re-materializes, so no
        // pad-removed fires), its sticky segment wiped by the removal's
        // flush with nothing upstream left to ever send another one. Left
        // alone it holds the seat forever and the occupied-seat check in
        // the link loop below refuses every later text stream with no error
        // surfaced anywhere. A branch that will render again always gets a
        // segment re-sent by its own reconfigure, so a seat holder WITHOUT
        // one, while a parked stream of the selected sid is waiting, is
        // beyond recovery and gets evicted. The pads come out of the entry
        // under the lock and the detach runs with the lock released, like
        // every other text detach.
        let reclaim = {
            let mut routing = inner.routing.lock();
            let waiting = allowed_sid.as_deref().is_some_and(|allowed| {
                routing.routed.iter().any(|routed| {
                    routed.kind == StreamKind::Text
                        && routed.downstream.is_none()
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                })
            });
            if waiting {
                routing
                    .routed
                    .iter_mut()
                    .find(|routed| {
                        routed.kind == StreamKind::Text
                            && routed.downstream.is_some()
                            && routed
                                .db3_src_pad
                                .sticky_event::<gst::event::Segment>(0)
                                .is_none()
                    })
                    .map(|routed| {
                        routed.seat_evicted = true;
                        (
                            routed.db3_src_pad.clone(),
                            routed.downstream.take().expect("filtered on Some above"),
                            routed.tqueue.take(),
                        )
                    })
            } else {
                None
            }
        };
        if let Some((db3_src_pad, downstream, tqueue)) = reclaim {
            warn!(
                pad = %db3_src_pad.name(),
                "reclaiming the overlay seat from a dead text branch"
            );
            Inner::detach_text_parts(inner, &db3_src_pad, &downstream, tqueue, false);
            // Parked rather than left dangling, exactly like
            // `park_text_streams`. Should the pad live on, decodebin3 must
            // be able to drain it.
            let parked = inner.park_stream(&db3_src_pad);
            let orphaned = {
                let mut routing = inner.routing.lock();
                match routing
                    .routed
                    .iter_mut()
                    .find(|routed| routed.db3_src_pad == db3_src_pad)
                {
                    Some(routed) => {
                        match parked {
                            Ok((sink, park)) => {
                                routed.park_sink = Some(sink);
                                routed.park_pad = Some(park);
                            }
                            Err(err) => warn!(?err, "failed to park the evicted text branch"),
                        }
                        None
                    }
                    // The stream unrouted while the lock was down, so its
                    // parking sink has nothing left to belong to.
                    None => parked.ok(),
                }
            };
            if let Some((sink, park)) = orphaned {
                let _ = db3_src_pad.unlink(&park);
                let _ = sink.set_state(gst::State::Null);
                let _ = inner.pipeline.remove(&sink);
            }
        }
        let mut routing = inner.routing.lock();
        if !routing
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Video && r.downstream.is_some())
        {
            return;
        }
        let mut joined: Option<String> = None;
        // Why each candidate was refused, reported below when nothing joined.
        let mut refusals: Vec<String> = Vec::new();
        for routed in routing
            .routed
            .iter_mut()
            .filter(|r| r.kind == StreamKind::Text && r.downstream.is_none())
        {
            // No stream id yet means wait for a later poll.
            let sid = routed.db3_src_pad.stream_id();
            if sid.is_none() || sid.as_deref() != allowed_sid.as_deref() {
                // Silent until a field log needed it: a switched-to external
                // whose branch never joined left no trace of WHY, and every
                // refusal in this loop is a `continue`.
                refusals.push(format!(
                    "{}: sid {:?} is not the allowed {:?}",
                    routed.db3_src_pad.name(),
                    sid.as_deref(),
                    allowed_sid.as_deref()
                ));
                continue;
            }
            // An entry the seat reclaim evicted stays out of contention
            // while its pad remains segmentless. Relinked, it would only
            // win the seat back from the same-sid stream that can render
            // (routed order is stable, and the evicted pad comes first). A
            // segment on the pad means the branch revived and may compete
            // again.
            if routed.seat_evicted {
                if routed
                    .db3_src_pad
                    .sticky_event::<gst::event::Segment>(0)
                    .is_none()
                {
                    refusals.push(format!(
                        "{}: seat-evicted and still segmentless",
                        routed.db3_src_pad.name()
                    ));
                    continue;
                }
                routed.seat_evicted = false;
            }
            let Some(overlay_entry) = inner.overlay.static_pad("subtitle_sink") else {
                warn!("subtitleoverlay has no subtitle_sink pad");
                continue;
            };
            // Held from the occupancy check through the link, so a disposal
            // running concurrently cannot decide the seat is free and flush it
            // out from under the branch joining here (see [`TextSeat`]).
            // Everything inside is pad and bin surgery, which blocks on
            // nothing. A busy seat is not waited out, see `TextSeat::try_hold`.
            let Ok(_seat) = inner.text_seat.try_hold() else {
                refusals.push(format!(
                    "{}: a branch disposal holds the overlay seat",
                    routed.db3_src_pad.name()
                ));
                // Not the caller's next settle point: the worker pokes this
                // poll when it drains, see `Inner::run_deferred_text_work`.
                continue;
            };
            if overlay_entry.is_linked() {
                warn!("subtitle_sink already linked; skipping extra text stream");
                refusals.push(format!(
                    "{}: subtitle_sink already linked",
                    routed.db3_src_pad.name()
                ));
                continue;
            }
            // Build the per-stream queue (see `RoutedStream::tqueue`) and
            // wire db3-text-pad -> queue -> overlay. The upstream link comes
            // last so data only flows once the chain is complete.
            // NAMED after the decodebin3 pad it serves. This is the only plain
            // `queue` the crate leaves unnamed anywhere in the pipeline, so
            // GStreamer auto-named it `queueN` and a field log reporting
            // "queue5: Failed to push event" took a source audit to attribute
            // (see `dash-start-seek-text-join-race.md`, which had to establish
            // it by exhaustion). A name costs nothing and makes the next such
            // log self-describing.
            let tqueue = match gst::ElementFactory::make("queue")
                .name(format!("fpb-tqueue-{}", routed.db3_src_pad.name()))
                .property("silent", true)
                .build()
            {
                Ok(q) => q,
                Err(err) => {
                    warn!(?err, "failed to create the text queue");
                    continue;
                }
            };
            if let Err(err) = inner.pipeline.add(&tqueue) {
                warn!(?err, "failed to add the text queue");
                continue;
            }
            let queue_entry = tqueue.static_pad("sink").expect("queue has a sink");
            if tqueue
                .link_pads(Some("src"), &inner.overlay, Some("subtitle_sink"))
                .is_err()
                || tqueue.sync_state_with_parent().is_err()
            {
                warn!("failed to wire the text queue into subtitleoverlay");
                let _ = tqueue.set_state(gst::State::Null);
                let _ = inner.pipeline.remove(&tqueue);
                continue;
            }
            // Out of the park, into the overlay (through its queue). Text
            // bypasses ssync, so it links from the decodebin3 pad directly.
            inner.unpark_stream(routed);
            match routed.db3_src_pad.link(&queue_entry) {
                Ok(_) => {
                    info!(pad = %routed.db3_src_pad.name(), "text stream joined subtitleoverlay");
                    routed.downstream = Some(queue_entry);
                    routed.tqueue = Some(tqueue);
                    joined = sid.map(|s| s.to_string());
                }
                Err(err) => {
                    warn!(?err, "failed to link text stream into subtitleoverlay");
                    let _ = tqueue.set_state(gst::State::Null);
                    let _ = inner.pipeline.remove(&tqueue);
                    // The stream was already unparked. It must not stay
                    // unlinked (decodebin3 cannot drain a deselected sparse
                    // stream into an unlinked pad), so park it again.
                    match inner.park_stream(&routed.db3_src_pad) {
                        Ok((sink, park)) => {
                            routed.park_sink = Some(sink);
                            routed.park_pad = Some(park);
                        }
                        Err(err) => warn!(?err, "failed to re-park the text stream"),
                    }
                }
            }
        }
        // A wanted sid nothing carries is otherwise a SILENT wedge (every
        // mismatch above just `continue`s): name the parked inventory so a
        // field log identifies the blocker, e.g. a drained external whose
        // multiqueue slot decodebin3 reclaimed for another stream
        // (`ext-subtitle-regression-2.txt`).
        if joined.is_none()
            && let Some(allowed) = allowed_sid.as_deref()
            && !routing.routed.iter().any(|r| {
                r.kind == StreamKind::Text
                    && r.db3_src_pad.stream_id().as_deref() == Some(allowed)
            })
        {
            let text_pads: Vec<String> = routing
                .routed
                .iter()
                .filter(|r| r.kind == StreamKind::Text)
                .map(|r| {
                    format!(
                        "{}={:?} linked={}",
                        r.db3_src_pad.name(),
                        r.db3_src_pad.stream_id().as_deref(),
                        r.downstream.is_some()
                    )
                })
                .collect();
            debug!(allowed, ?text_pads, "no routed text pad carries the allowed sid");
        }
        // The OTHER silent shape, and the one the field hit: a carrier pad
        // exists, its input delivers, and the branch still did not join, so
        // every candidate was refused for one of the reasons above. Without
        // this the log shows a selection that confirmed and simply never
        // rendered, with nothing to point at.
        // NOT when the allowed sid is already on the overlay. The link loop
        // only considers entries with `downstream: None`, so a branch that
        // joined on an EARLIER poll is invisible to it and every other text
        // entry lands in `refusals`: the first version of this line fired twice
        // on a field log where the wanted stream was joined and rendering, which
        // is worse than no diagnostic. `joined` means "linked by THIS call",
        // which is not the same question.
        let already_joined = allowed_sid.as_deref().is_some_and(|allowed| {
            routing.routed.iter().any(|r| {
                r.kind == StreamKind::Text
                    && r.downstream.is_some()
                    && r.db3_src_pad.stream_id().as_deref() == Some(allowed)
            })
        });
        if joined.is_none() && !refusals.is_empty() && allowed_sid.is_some() && !already_joined {
            debug!(
                allowed = ?allowed_sid.as_deref(),
                ?refusals,
                "no text branch joined subtitleoverlay"
            );
        }
        // EVERY join of an external stream replays its input: by join
        // time anything may have drained its data beyond reach (deselect
        // drains, the deselect-race death, auto-select releasing the hold
        // into a parked branch), no flag can track those orderings, and
        // the replay is idempotent. Queued only AFTER the link, so the
        // replayed data lands in the overlay and not in the parking sink.
        if let Some(sid) = joined {
            for input in routing.inputs.iter() {
                let Some(external) = input.external.as_ref() else {
                    continue;
                };
                if !external.hold_until_selected && input.stream_ids().contains(&sid) {
                    let _ = inner.work_tx.send(Job::ReplaySub {
                        id: external.id,
                        epoch: external.epoch,
                        attempt: 0,
                    });
                }
            }
        }
    }

    /// Stream kind from the pad's sticky stream-start event (decodebin3
    /// stamps a GstStream on its output pads), with a caps fallback.
    fn stream_kind_of(pad: &gst::Pad) -> Option<StreamKind> {
        if let Some(stream) = pad.stream() {
            let ty = stream.stream_type();
            if ty.contains(gst::StreamType::VIDEO) {
                return Some(StreamKind::Video);
            }
            if ty.contains(gst::StreamType::AUDIO) {
                return Some(StreamKind::Audio);
            }
            if ty.contains(gst::StreamType::TEXT) {
                return Some(StreamKind::Text);
            }
        }
        let caps = pad.current_caps()?;
        decisions::kind_from_caps_name(caps.structure(0)?.name())
    }
}

/// The pure routing decisions, separated from the pipeline calls that act on
/// them so the invariants are unit-testable without a live pipeline.
mod decisions {
    use super::{DeferredTextWork, StreamKind};

    /// The eager text-branch work a subtitle transition needs before its
    /// `SELECT_STREAMS` goes out (see `pump_selection`).
    ///
    /// `upstream_owns` is the tri-state from `Inner::upstream_selection_mode`:
    /// `None` means the main input has no decodebin3 sink pad linked yet, so
    /// the SELECTABLE query has nobody to ask. An unknown mode takes the PARK,
    /// not the flush: the pair would land on a text branch that may be fed by
    /// an adaptive demuxer, and one push racing it returns FLUSHING into a
    /// single output loop serving every track, which then pauses for good with
    /// no error posted at all (FREEZE-DIAGN.md section 1). That state cannot
    /// co-occur with a replace today (section 3 proves both
    /// `last_applied_subtitle` setters need the very link that populates the
    /// pads), so this arm is defense in depth and nothing rests on it.
    ///
    /// `eager_flush_on_unknown` is the lever
    /// `FCAST_EAGER_FLUSH_ON_UNKNOWN_MODE` (set = the old behaviour, where an
    /// unknown mode read as a definite false).
    pub(crate) fn eager_text_work(
        subtitle_off: bool,
        replacing: bool,
        upstream_owns: Option<bool>,
        eager_flush_on_unknown: bool,
    ) -> Option<DeferredTextWork> {
        if subtitle_off {
            return Some(DeferredTextWork::Park);
        }
        if !replacing {
            return None;
        }
        match upstream_owns {
            // Upstream owns selection: no pad swap is coming to free the
            // overlay seat, so the park does the handover.
            Some(true) => Some(DeferredTextWork::Park),
            Some(false) => Some(DeferredTextWork::Flush),
            None if eager_flush_on_unknown => Some(DeferredTextWork::Flush),
            None => Some(DeferredTextWork::Park),
        }
    }

    /// Whether applying `selected_ids` drops video ENTIRELY (the video-chain
    /// deactivation case), as opposed to a video-to-video switch, whose new
    /// id is not routed yet and would otherwise look like "no video". An
    /// empty `collection_video_ids` (no bus handler installed) means kinds
    /// are unknowable: never deactivate then.
    pub(crate) fn deselects_video(
        video_linked: bool,
        collection_video_ids: &[String],
        selected_ids: &[String],
    ) -> bool {
        video_linked
            && !collection_video_ids.is_empty()
            && !collection_video_ids
                .iter()
                .any(|vid| selected_ids.contains(vid))
    }

    /// The state a dynamically (re)activated element joins the pipeline at:
    /// cap at PAUSED while a transition is in flight (the commit's child
    /// walk lifts it the rest of the way with the fresh base_time), match
    /// the pipeline exactly otherwise (see `Inner::join_state`).
    pub(crate) fn join_state(current: gst::State, pending: gst::State) -> gst::State {
        if pending == gst::State::VoidPending {
            current
        } else {
            pending.min(gst::State::Paused)
        }
    }

    /// Whether a decodebin3 output pad may be routed: only during a preroll
    /// (pending at least PAUSED) or in a settled pipeline at PAUSED or
    /// above. Anything else is a straggler from a superseded load.
    pub(crate) fn pad_accepting(current: gst::State, pending: gst::State) -> bool {
        pending >= gst::State::Paused
            || (pending == gst::State::VoidPending && current >= gst::State::Paused)
    }

    /// Whether parked text may join subtitleoverlay: only in a SETTLED
    /// pipeline at PAUSED or above (linking mid-transition splices a
    /// reconfiguration into the async preroll and wedges it under churn).
    pub(crate) fn text_may_link(current: gst::State, pending: gst::State) -> bool {
        current >= gst::State::Paused && pending == gst::State::VoidPending
    }

    /// What a bus error from a live external subtitle input means (see
    /// `Inner::handle_external_error` for the mechanism).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ExternalErrorAction {
        /// Genuine failure: detach and report `ExternalSubtitleFailed`.
        Fail,
        /// A transport race: the source is fine, only its task stopped (a
        /// deselect or a flush caught it mid-push). Keep the input
        /// attached; the join-time replay seek restarts it (idempotent,
        /// so a dying input's error burst needs no debounce).
        Recover,
    }

    /// Decide an external input's error by its CAUSE, read from the error
    /// message's debug string: a transport race (a deselect or one of our
    /// own flushes catching the source mid-push) dies as basesrc's
    /// "streaming stopped" with reason not-linked or flushing and
    /// recovers in place; everything else (resource, decode, network) is
    /// a genuine failure, failed fast. Selection state plays NO part:
    /// decodebin3's auto-select can route, join and show a fresh external
    /// before anyone asked for it, so every heuristic on it misclassified
    /// a flush-killed healthy input as a genuine failure.
    pub(crate) fn external_error_action(debug_info: Option<&str>) -> ExternalErrorAction {
        let transport_race = debug_info
            .is_some_and(|d| d.contains("reason not-linked") || d.contains("reason flushing"));
        if transport_race {
            ExternalErrorAction::Recover
        } else {
            ExternalErrorAction::Fail
        }
    }

    /// Caps-name fallback for pads without a GstStream.
    pub(crate) fn kind_from_caps_name(name: &str) -> Option<StreamKind> {
        // image/* is video: parsebin types image streams as VIDEO and
        // fimagedec decodes them into raw video frames.
        if name.starts_with("video/") || name.starts_with("image/") {
            Some(StreamKind::Video)
        } else if name.starts_with("audio/") {
            Some(StreamKind::Audio)
        } else if name.starts_with("text/") || name.starts_with("subpicture/") {
            Some(StreamKind::Text)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeferredTextWork, StreamKind, SwapState, decisions::*};

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// An unanswerable upstream-selection mode is not a "no": the old code
    /// read that ignorance as "nobody upstream owns selection" and chose the
    /// FLUSH, whose pair on a demuxer-fed text branch can return FLUSHING into
    /// the one output loop serving every track and pause it for good. Defense
    /// in depth, see `decisions::eager_text_work`.
    #[test]
    fn an_unknown_upstream_mode_never_chooses_the_eager_flush() {
        assert_eq!(
            eager_text_work(false, true, None, false),
            Some(DeferredTextWork::Park)
        );
        // The lever restores the old, freezing choice.
        assert_eq!(
            eager_text_work(false, true, None, true),
            Some(DeferredTextWork::Flush)
        );
        // Both DEFINITE answers keep their pre-existing arm.
        assert_eq!(
            eager_text_work(false, true, Some(true), false),
            Some(DeferredTextWork::Park)
        );
        assert_eq!(
            eager_text_work(false, true, Some(false), false),
            Some(DeferredTextWork::Flush)
        );
    }

    /// The other two arms of the same decision are mode-independent: turning
    /// subtitles off always parks (that IS the handover), and a selection that
    /// replaces nothing has no outgoing branch to wake.
    #[test]
    fn eager_text_work_off_parks_and_a_non_replace_does_nothing() {
        for mode in [None, Some(true), Some(false)] {
            assert_eq!(
                eager_text_work(true, false, mode, false),
                Some(DeferredTextWork::Park),
                "subtitle-off with mode {mode:?}"
            );
            // A subtitle-off still parks even when it also replaces: the
            // off arm is checked first because there is no incoming track.
            assert_eq!(
                eager_text_work(true, true, mode, false),
                Some(DeferredTextWork::Park),
                "subtitle-off replace with mode {mode:?}"
            );
            assert_eq!(
                eager_text_work(false, false, mode, false),
                None,
                "non-replace with mode {mode:?}"
            );
        }
    }


    /// The gapless activation window is `swapped` AND `pending`, nothing
    /// looser. Two callers depend on exactly this shape: the cancel refusal
    /// (`cancel_prepared`) and the duration-refresh gate in
    /// `translate_message`, which must not let a successor item's duration
    /// reach the caller.
    #[test]
    fn activation_pending_needs_both_swapped_and_pending() {
        assert_eq!(SwapState::default().activation_pending(), None);
        // Armed but not yet performed: upstream is still the playing item.
        assert_eq!(
            SwapState {
                pending: Some(7),
                ..Default::default()
            }
            .activation_pending(),
            None
        );
        // A long-completed activation leaves `swapped` set, `pending` cleared.
        assert_eq!(
            SwapState {
                swapped: true,
                ..Default::default()
            }
            .activation_pending(),
            None
        );
        assert_eq!(
            SwapState {
                pending: Some(7),
                swapped: true,
                ..Default::default()
            }
            .activation_pending(),
            Some(7)
        );
    }

    #[test]
    fn deselects_video_only_when_video_leaves_the_selection_entirely() {
        let collection = ids(&["vid-a", "vid-b"]);
        // Dropping video from the selection deactivates the chain.
        assert!(deselects_video(true, &collection, &ids(&["aud-1"])));
        // A video-to-video switch keeps the chain (decodebin3 reuses the pad).
        assert!(!deselects_video(
            true,
            &collection,
            &ids(&["vid-b", "aud-1"])
        ));
        // Nothing linked, nothing to deactivate.
        assert!(!deselects_video(false, &collection, &ids(&["aud-1"])));
        // Unknown kinds (no cached collection): never deactivate.
        assert!(!deselects_video(true, &[], &ids(&["aud-1"])));
    }

    #[test]
    fn join_state_caps_at_paused_during_transitions() {
        use gst::State::*;
        // Settled: match the pipeline exactly.
        assert_eq!(join_state(Playing, VoidPending), Playing);
        assert_eq!(join_state(Paused, VoidPending), Paused);
        assert_eq!(join_state(Null, VoidPending), Null);
        // In flight: park at PAUSED so the commit walk finishes the climb
        // with the fresh base_time.
        assert_eq!(join_state(Paused, Playing), Paused);
        assert_eq!(join_state(Ready, Paused), Paused);
        // Downward transitions join below PAUSED.
        assert_eq!(join_state(Paused, Ready), Ready);
    }

    #[test]
    fn pad_accepting_rejects_teardown_stragglers() {
        use gst::State::*;
        // Prerolling or settled at/above PAUSED: accept.
        assert!(pad_accepting(Ready, Paused));
        assert!(pad_accepting(Paused, Playing));
        assert!(pad_accepting(Paused, VoidPending));
        assert!(pad_accepting(Playing, VoidPending));
        // At or heading to READY/NULL: straggler.
        assert!(!pad_accepting(Ready, VoidPending));
        assert!(!pad_accepting(Paused, Ready));
        assert!(!pad_accepting(Playing, Null));
    }

    #[test]
    fn text_links_only_into_a_settled_pipeline() {
        use gst::State::*;
        assert!(text_may_link(Paused, VoidPending));
        assert!(text_may_link(Playing, VoidPending));
        // Mid-transition (the async preroll in particular): never.
        assert!(!text_may_link(Ready, Paused));
        assert!(!text_may_link(Paused, Playing));
        assert!(!text_may_link(Ready, VoidPending));
    }

    #[test]
    fn caps_name_kind_fallback() {
        assert_eq!(kind_from_caps_name("video/x-h264"), Some(StreamKind::Video));
        assert_eq!(kind_from_caps_name("audio/mpeg"), Some(StreamKind::Audio));
        assert_eq!(kind_from_caps_name("text/x-raw"), Some(StreamKind::Text));
        assert_eq!(
            kind_from_caps_name("subpicture/x-dvd"),
            Some(StreamKind::Text)
        );
        assert_eq!(kind_from_caps_name("application/x-id3"), None);
    }

    // --- external subtitle error policy --------------------------------------

    #[test]
    fn transport_race_deaths_recover_in_place() {
        // A deselect or one of our own flushes caught the source
        // mid-push: the source is fine, it stays attached, and the
        // join-time replay (or the never-linked retry) restarts it.
        assert_eq!(
            external_error_action(Some("streaming stopped, reason not-linked (-1)")),
            ExternalErrorAction::Recover
        );
        assert_eq!(
            external_error_action(Some("streaming stopped, reason flushing (-2)")),
            ExternalErrorAction::Recover
        );
    }

    #[test]
    fn genuine_errors_fail_fast() {
        // Resource/decode/network errors are real failures, failed fast.
        assert_eq!(
            external_error_action(Some("Could not open resource for reading.")),
            ExternalErrorAction::Fail
        );
        assert_eq!(external_error_action(None), ExternalErrorAction::Fail);
    }
}

#[cfg(test)]
mod pipeline_tests {
    /// gst::init plus the elements the APPLICATION registers in production:
    /// the constructor builds fcastaudiostretch unconditionally, and these
    /// tests are their own application.
    fn test_init() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            fcast_gst_elements::fcastaudiostretch::plugin_init()
                .expect("registering fcastaudiostretch");
        });
    }

    use super::*;
    use std::time::Instant;

    /// Encode `seconds` of silence to an MP3 file (audio/mpeg, the real fcomp
    /// container). Done once per source so playback can go through the real
    /// `urisourcebin` topology below.
    fn make_mp3_file(path: &std::path::Path, seconds: f64) {
        // audiotestsrc defaults: 44100 Hz, 1024 samples/buffer.
        let num_buffers = (seconds * 44100.0 / 1024.0).round() as i32;
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", num_buffers)
            .property("is-live", false)
            .property_from_str("wave", "silence")
            .build()
            .unwrap();
        let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
        let enc = gst::ElementFactory::make("lamemp3enc").build().unwrap();
        let sink = gst::ElementFactory::make("filesink")
            .property("location", path.to_str().unwrap())
            .build()
            .unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&src, &conv, &enc, &sink]).unwrap();
        gst::Element::link_many([&src, &conv, &enc, &sink]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(10)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(err) => panic!("mp3 encode failed: {err:?}"),
                _ => {}
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// A unique temp path under the test dir (no wall clock needed).
    fn temp_mp3(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fcastplaybin-gapless-{}-{tag}-{n}.mp3",
            std::process::id()
        ))
    }

    /// The real gapless source: `urisourcebin` over a file URI with
    /// `parse-streams`, exactly what `media_source::build_uri_source_with_head`
    /// builds (so decodebin3 gets the stream collection urisourcebin forwards).
    fn uri_source(path: &std::path::Path) -> gst::Element {
        gst::ElementFactory::make("urisourcebin")
            .property("uri", format!("file://{}", path.display()))
            .property("parse-streams", true)
            .property("use-buffering", true)
            .build()
            .unwrap()
    }

    fn fake_audio_sinks() -> Sinks {
        Sinks {
            video: None,
            audio: AudioSink::Factory(Box::new(|| {
                Ok(gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?)
            })),
        }
    }

    /// Gapless handoff smoke test, end to end on a real pipeline: play item A
    /// through `urisourcebin` (the field's gapless source topology), pre-arm
    /// item B, and assert B plays to ITS end rather than being cut off at A's.
    /// Guards the generic swap path. NOTE: this passes for `file://`/`filesrc`
    /// sources. The FIELD bug (an fcomp item cut at the previous item's
    /// declared duration) does NOT reproduce here, which localizes it to
    /// `fcompsrc`'s size/segment/EOS behavior, not the swap itself (a
    /// fcompsrc + fake-companion repro belongs in receiver-core).
    #[test]
    fn gapless_swap_plays_the_next_item_to_its_end() {
        test_init();
        let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();

        let (tx, rx) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| match event {
            PlaybinEvent::PreparedActivated => {
                let _ = tx.send(Ev::Activated);
            }
            PlaybinEvent::EndOfStream => {
                let _ = tx.send(Ev::Eos);
            }
            _ => {}
        });

        // A is long enough that decodebin3's multiqueue and the decoupling
        // audio queue cannot swallow it whole, so its EOS is PACED to near its
        // end (mirroring a real track). Pre-arming early then lands before
        // A's EOS reaches the output-side hold, the order the field hits.
        let a_secs = 5.0;
        let b_secs = 2.0;
        let a_path = temp_mp3("a");
        let b_path = temp_mp3("b");
        make_mp3_file(&a_path, a_secs);
        make_mp3_file(&b_path, b_secs);

        playbin
            .load(MediaInput::Element(uri_source(&a_path)), StartPoint::Live)
            .unwrap();
        // Pre-arm B BEFORE A's end-of-stream can reach the output hold. The
        // field pre-arms tens of seconds early; a short test source's input
        // drains at load (its parsed data fits decodebin3's multiqueue whole),
        // so the pre-arm has to be up front to win that race. `pending` is
        // then set when A's EOS drains out, which is what must hold it back.
        playbin.prepare_next_async(MediaInput::Element(uri_source(&b_path)));
        let t0 = Instant::now();
        playbin.play().unwrap();

        let mut activated = false;
        let eos_elapsed = loop {
            match rx.recv_timeout(Duration::from_secs(15)) {
                Ok(Ev::Activated) => activated = true,
                Ok(Ev::Eos) => break Some(t0.elapsed()),
                Err(_) => break None,
            }
        };
        let _ = playbin.stop();
        let _ = std::fs::remove_file(&a_path);
        let _ = std::fs::remove_file(&b_path);

        let eos_elapsed = eos_elapsed.expect("pipeline never reached EOS (wedged)");
        assert!(
            activated,
            "the prepared item never activated (handoff missed)"
        );
        // Gapless success plays A then B back to back (~7s). The bug cuts B off
        // at A's end, so EOS lands near A's length (~5s) instead. The 6s
        // threshold sits between the two with margin for buffering slack.
        assert!(
            eos_elapsed >= Duration::from_millis(6000),
            "playback ended after {eos_elapsed:?}, expected ~{}s (A+B): the \
             next item was cut off at the previous item's segment end",
            a_secs + b_secs,
        );
    }

    #[derive(PartialEq)]
    enum Ev {
        Activated,
        Eos,
    }

    /// The duration-refresh edge, end to end through the real bus
    /// translation: a `DURATION_CHANGED` must reach the caller as
    /// [`PlaybinEvent::DurationChanged`] (its cue to re-query), and must be
    /// dropped while a performed swap waits to activate, where the query would
    /// be answered by the successor item.
    ///
    /// Posting the message is the deterministic trigger: translation is a bus
    /// SYNC handler, so `post` runs it inline on this thread and the channel is
    /// already settled when it returns. The message carries no payload, so a
    /// synthesized one is indistinguishable from a demuxer's (which is the
    /// whole point of the no-payload contract).
    #[test]
    fn duration_changed_reaches_the_caller_except_mid_activation() {
        test_init();
        let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();

        let (tx, rx) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            if matches!(event, PlaybinEvent::DurationChanged) {
                let _ = tx.send(generation);
            }
        });

        let bus = playbin.bus();
        bus.post(gst::message::DurationChanged::new()).unwrap();
        rx.try_recv()
            .expect("a duration-changed on the bus must reach the caller");

        // The swapped-with-pending-activation window (the same predicate the
        // cancel refusal uses).
        *playbin.inner.swap_gate.state.lock() = SwapState {
            pending: Some(42),
            swapped: true,
            ..Default::default()
        };
        bus.post(gst::message::DurationChanged::new()).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "duration-changed must be dropped while a performed swap waits to activate: \
             upstream answers for the successor item there"
        );

        // Leave the gate as found: teardown reads it.
        *playbin.inner.swap_gate.state.lock() = SwapState::default();
        let _ = playbin.stop();
    }

    /// The watchdog end to end, without FAST or media: attach a URI that
    /// never produces a stream (the pipeline sits in NULL, so urisourcebin
    /// never starts) and expect the crate to detach it and report
    /// `ExternalSubtitleFailed` on its own.
    #[test]
    fn watchdog_fails_a_subtitle_that_never_materializes() {
        test_init();
        let playbin = FcastPlaybin::new(Sinks {
            video: None,
            audio: AudioSink::Auto,
        })
        .unwrap();
        playbin.set_external_sub_timeout(Duration::from_millis(200));

        let (tx, rx) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            if let PlaybinEvent::ExternalSubtitleFailed { id } = event {
                let _ = tx.send(id);
            }
        });

        let id = playbin
            .attach_subtitle("file:///nonexistent/fcastplaybin-watchdog-test.srt")
            .unwrap();
        assert!(playbin.has_external_subtitles());

        let failed = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watchdog should fail the stream-less input");
        assert_eq!(failed, id);

        // The crate detached the input itself: nothing external remains and
        // a caller-side detach of the reported id is a (harmless) error.
        assert!(!playbin.has_external_subtitles());
        assert!(playbin.subtitle_stream_ids(id).is_empty());
        assert!(playbin.detach_subtitle(id).is_err());
    }
}
