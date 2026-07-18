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

use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use parking_lot::Mutex;
use tracing::{debug, debug_span, error, info, warn};

pub mod state_machine;

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

/// Identifies one attached external subtitle input for later detach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalSubId(u64);

/// A cumulative byte counter for one input stream's PARSED (compressed)
/// data, for bitrate inspection (see [`FcastPlaybin::stream_io_stats`]).
/// Counters are per-load by construction (they live and die with the
/// input); callers sample periodically and derive rates from deltas.
#[derive(Debug, Clone)]
pub struct StreamIoStats {
    /// The GStreamer stream id, for correlating with the stream collection
    /// (`None` until the pad has carried its stream-start).
    pub stream_id: Option<String>,
    /// Set when the stream belongs to an external subtitle input.
    pub external: Option<ExternalSubId>,
    /// Compressed bytes that have passed into decodebin3 so far.
    pub bytes: u64,
    /// The stream's current caps (codec, dimensions, rate, ...).
    pub caps: Option<gst::Caps>,
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

/// Where a bus error originated, derived from the generation-tagged inputs.
/// This replaces playbin3's contextless `failed_uri` guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorOrigin {
    /// The main input of the CURRENT load.
    Main,
    /// A live-attached external subtitle input.
    ExternalSubtitle(ExternalSubId),
    /// An element of a previous, already-replaced load whose teardown died
    /// noisily. Safe to ignore.
    Stale,
    /// Not attributable to a specific input (sinks, decoders, ...): treat as
    /// the current load's problem.
    Unknown,
}

/// A typed pipeline event, delivered through the callback installed by
/// [`FcastPlaybin::set_event_handler`]. Bus messages are translated on the
/// posting (streaming) thread, and worker feedback (load completion, seek
/// outcomes) arrives through the same callback: one ordered event source
/// instead of a raw bus plus side channels.
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
    Warning(String),
}

/// First look at every raw bus message, invoked on the posting (streaming)
/// thread, for caller-specific messages the crate does not understand
/// (`NeedContext` for custom source elements, missing-plugin reports).
/// Return `true` to consume the message. No event is emitted for it.
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
    /// Dot-dump the pipeline graph for debugging. On the worker so the
    /// element walk cannot race a load's sink teardown.
    DumpDot {
        done: Box<dyn FnOnce(String) + Send>,
    },
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
            Job::DumpDot { .. } => write!(f, "DumpDot"),
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
    event: gst::Event,
    /// The selected ids, kept for the video-deselect check after the send.
    stream_ids: Vec<String>,
}

/// A byte counter on one input stream's parsed data, for bitrate
/// inspection (see [`FcastPlaybin::stream_io_stats`]). The probe lives on
/// the input's source pad and is removed with the input.
struct StreamTap {
    /// The input element's source pad (one parsed elementary stream).
    pad: gst::Pad,
    bytes: Arc<AtomicU64>,
    probe: Option<gst::PadProbeId>,
}

/// One live input: an element (urisourcebin or caller-provided) whose source
/// pads are linked into decodebin3 request sink pads.
struct Input {
    element: gst::Element,
    /// Which load (or attach) this input belongs to. A bumped generation
    /// makes this input's errors [`ErrorOrigin::Stale`].
    generation: u64,
    /// External-subtitle id, `None` for the main input.
    external: Option<ExternalSubId>,
    /// decodebin3 request sink pads we hold for this input.
    db3_sink_pads: Vec<gst::Pad>,
    /// Per-stream byte counters (see [`StreamTap`]).
    taps: Vec<StreamTap>,
    /// Signal handlers to disconnect on removal.
    pad_added_sig: Option<gst::glib::SignalHandlerId>,
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
    routing: Mutex<RoutingState>,
}

/// An RAII hold on [`Inner::route_gate`]. Dropping it releases the gate
/// FIRST and then re-attempts `deferred_pads`, so the invariant is simply
/// "every gate release drains": a pad deferred while any holder had the gate
/// is re-routed the moment that holder finishes, with no polling thread.
struct RouteGate<'a> {
    inner: &'a Arc<Inner>,
    guard: Option<parking_lot::MutexGuard<'a, ()>>,
}

impl Drop for RouteGate<'_> {
    fn drop(&mut self) {
        // Release the mutex before draining: the drain re-enters
        // `route_db3_pad`, which must be able to take the gate itself.
        self.guard.take();
        Inner::drain_deferred_pads(self.inner);
    }
}

impl Inner {
    /// Take the route gate (blocking). See [`RouteGate`].
    fn gate(inner: &Arc<Inner>) -> RouteGate<'_> {
        RouteGate {
            inner,
            guard: Some(inner.route_gate.lock()),
        }
    }

    /// Take the route gate without blocking. See [`RouteGate`].
    fn try_gate(inner: &Arc<Inner>) -> Option<RouteGate<'_>> {
        inner.route_gate.try_lock().map(|guard| RouteGate {
            inner,
            guard: Some(guard),
        })
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

/// A flushing ACCURATE seek to `position` at `rate`, handling reverse rates
/// (seek from the end). TRICKMODE lets decoders drop frames to keep up:
/// right for fast-scrub, wrong for pitch-corrected speed playback where
/// scaletempo wants every frame. Only high forward rates and reverse (which
/// can't be decoded frame-complete anyway) enable it, so a 1.25x/1.5x/2x
/// "watch faster" stays full quality.
fn send_rate_seek(
    pipeline: &gst::Pipeline,
    rate: f64,
    position: gst::ClockTime,
) -> std::result::Result<(), gst::glib::error::BoolError> {
    let mut flags = gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH;
    if rate < 0.0 || rate > 2.0 {
        flags |= gst::SeekFlags::TRICKMODE;
    }
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
            Some(sink) => sink,
            None => {
                let sink = make("fakesink", "fpb-fake-vsink")?;
                sink.set_property("sync", true);
                sink
            }
        };

        let aconv = make("audioconvert", "fpb-aconv")?;
        let aresample = make("audioresample", "fpb-aresample")?;
        let scaletempo = make("scaletempo", "fpb-scaletempo")?;
        let volume = make("volume", "fpb-volume")?;
        // Decoupling queue at the head of the audio branch. Without it, a
        // paused audio sink (parked in wait_preroll during a mid-load
        // re-preroll) backpressures through streamsynchronizer into
        // decodebin3's multiqueue and stalls the single demuxer thread,
        // which then can't feed VIDEO, so the video sink never re-prerolls
        // and the whole pipeline deadlocks. The queue absorbs the audio that
        // piles up during the video re-preroll window. Bounded by TIME (the
        // default 1s cap is what bites, 30s caps memory to a few MB of PCM)
        // with no min-threshold, so it adds no playback latency.
        let aqueue = make("queue", "fpb-aqueue")?;
        aqueue.set_property("max-size-time", 30u64 * 1_000_000_000);
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

        let inner = Arc::new(Inner {
            video_chain: vec![overlay.clone(), video_sink.clone()],
            audio: sinks.audio,
            audio_sink: Mutex::new(None),
            pipeline,
            core: Mutex::new(None),
            token_src,
            route_gate: Mutex::new(()),
            deferred_pads: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            next_generation: AtomicU64::new(0),
            overlay,
            // The audio branch's head is the decoupling queue. ssync links here.
            audio_entry: aqueue,
            volume,
            events: Mutex::new(None),
            work_tx,
            select_tx,
            routing: Mutex::new(RoutingState::default()),
        });

        Inner::install_core(&inner)?;

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

    /// Classify a bus error message by its source element (generation tags).
    pub fn classify_error(&self, msg: &gst::message::Error) -> ErrorOrigin {
        self.inner.classify_error_src(msg.src())
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
    fn apply_start_seek(inner: &Arc<Inner>, position: gst::ClockTime, rate: f64) {
        let (res, _, _) = inner.pipeline.state(PREROLL_TIMEOUT);
        if res.is_err() {
            return;
        }
        let mut q = gst::query::Seeking::new(gst::Format::Time);
        if !inner.pipeline.query(q.query_mut()) || !q.result().0 {
            return;
        }
        if send_rate_seek(&inner.pipeline, rate, position).is_ok() {
            let _ = inner.pipeline.state(PREROLL_TIMEOUT);
        }
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
    /// selectable once decodebin3 announces the updated collection.
    pub fn attach_subtitle_with_id(&self, id: ExternalSubId, uri: &str) -> Result<()> {
        let generation = self.inner.current_generation();
        // NO buffering on subtitle side-inputs (uridecodebin3 also buffers
        // only the main item): a fresh input's own queue2 levels would drive
        // the caller's buffering state machine and wedge a paused pipeline
        // in "Buffering".
        let element = Inner::make_urisourcebin(uri, false)?;
        Inner::add_input(&self.inner, element, generation, Some(id))?;
        info!(?id, uri, "attached external subtitle input");
        Ok(())
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
            .position(|i| i.external == Some(id))
            .ok_or_else(|| anyhow!("no attached subtitle {id:?}"))?;
        let input = routing.inputs.remove(idx);
        drop(routing);
        Inner::remove_input(inner, input);
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
        let Some(input) = routing.inputs.iter().find(|i| i.external == Some(id)) else {
            return Vec::new();
        };
        input
            .element
            .src_pads()
            .iter()
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
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
    fn teardown(&self, target: gst::State) -> Result<()> {
        // A chain parked by a mid-item deselect is state-locked; unlock so
        // it follows the pipeline down.
        for element in &self.inner.video_chain {
            element.set_locked_state(false);
        }
        {
            let _gate = Inner::gate(&self.inner);
            self.inner
                .pipeline
                .set_state(target)
                .with_context(|| format!("pipeline to {target:?} for teardown"))?;
        }
        Inner::remove_all_inputs(&self.inner);
        self.inner.remove_audio_sink();
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

    /// Queue a dot dump of the pipeline graph, delivered to `done` ON THE
    /// WORKER THREAD (hand it off, do not block). Queued so the element walk
    /// cannot race a concurrent load or teardown.
    pub fn debug_dot_data_async(&self, done: Box<dyn FnOnce(String) + Send>) {
        self.queue_job(Job::DumpDot { done });
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

    pub fn position(&self) -> Option<gst::ClockTime> {
        self.inner.pipeline.query_position::<gst::ClockTime>()
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.inner.pipeline.query_duration::<gst::ClockTime>()
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

    pub fn dump_dot(&self, name: &str) {
        self.inner
            .pipeline
            .debug_to_dot_file_with_ts(gst::DebugGraphDetails::ALL, name);
    }

    /// Cumulative parsed-byte counters for every live input stream (all
    /// streams, selected or not: decodebin3 keeps consuming deselected
    /// inputs). Poll and diff to plot per-stream bitrate; correlate with
    /// the stream collection via `stream_id` and with track selection via
    /// the caller's selected ids.
    pub fn stream_io_stats(&self) -> Vec<StreamIoStats> {
        let routing = self.inner.routing.lock();
        routing
            .inputs
            .iter()
            .flat_map(|input| {
                input.taps.iter().map(|tap| StreamIoStats {
                    stream_id: tap.pad.stream_id().map(|sid| sid.to_string()),
                    external: input.external,
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
                    external: input.external,
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
    /// buffer counts), when a video sink is configured.
    pub fn video_sink_stats(&self) -> Option<gst::Structure> {
        let sink = self.inner.video_chain.last()?;
        Some(sink.property::<gst::Structure>("stats"))
    }

    /// The per-load audio sink's negotiated caps and base-sink `stats`
    /// structure, while one exists.
    pub fn audio_sink_health(&self) -> Option<(Option<gst::Caps>, gst::Structure)> {
        let slot = self.inner.audio_sink.lock();
        let sink = slot.as_ref()?;
        let caps = sink.static_pad("sink").and_then(|pad| pad.current_caps());
        Some((caps, sink.property::<gst::Structure>("stats")))
    }
}

// Teardown lives on `Inner`, NOT on the cloneable handle: a `Drop` on
// `FcastPlaybin` fires for EVERY dropped clone, including the worker's
// per-job temporaries. A handle-level Drop once NULLed the pipeline from a
// streaming thread mid-post and deadlocked a concurrent load's state change.
impl Drop for Inner {
    fn drop(&mut self) {
        // A chain parked by a mid-item deselect is state-locked; unlock so
        // it follows the pipeline down.
        for element in &self.video_chain {
            element.set_locked_state(false);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        // Between video items the caller sink parks at READY OUTSIDE the
        // pipeline (`remove_video_chain`), so the NULL above never reaches
        // it and the final unref would trip GStreamer's dispose-in-READY
        // CRITICAL. Down any orphaned chain element explicitly.
        for element in &self.video_chain {
            if element.parent().is_none() {
                let _ = element.set_state(gst::State::Null);
            }
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
    /// here after the job, a safe thread for it.
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

            // send_event runs decodebin3's selection handling inline on THIS
            // thread. It may stall behind streaming threads, which is the
            // point of this thread (see `select_streams`).
            let seqnum = job.event.seqnum();
            if !job.db3.send_event(job.event) {
                warn!("decodebin3 refused the SELECT_STREAMS event");
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

    /// Classify a bus message source by the generation-tagged inputs.
    fn classify_error_src(&self, src: Option<&gst::Object>) -> ErrorOrigin {
        let Some(src) = src else {
            return ErrorOrigin::Unknown;
        };
        let generation = self.current_generation();
        let routing = self.routing.lock();
        for input in &routing.inputs {
            let is_from_input = src == input.element.upcast_ref::<gst::Object>()
                || src.has_as_ancestor(&input.element);
            if !is_from_input {
                continue;
            }
            if input.generation != generation {
                return ErrorOrigin::Stale;
            }
            return match input.external {
                Some(id) => ErrorOrigin::ExternalSubtitle(id),
                None => ErrorOrigin::Main,
            };
        }
        ErrorOrigin::Unknown
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

    /// Translate a bus message into its typed event, applying the crate's
    /// filters: per-element state changes and foreign ASYNC_DONEs are
    /// dropped, external-input collections are swallowed, and errors from
    /// elements no longer in the pipeline (a superseded load's teardown
    /// dying noisily) are discarded.
    fn translate_message(&self, msg: &gst::Message) -> Option<PlaybinEvent> {
        use gst::MessageView;

        let pipeline_obj = self.pipeline.upcast_ref::<gst::Object>();
        let event = match msg.view() {
            MessageView::Eos(_) => PlaybinEvent::EndOfStream,
            MessageView::Error(error) => {
                if let Some(src) = msg.src()
                    && src != pipeline_obj
                    && !src.has_as_ancestor(&self.pipeline)
                {
                    debug!(
                        src = %src.name(),
                        "Dropping error from element no longer in the current pipeline"
                    );
                    return None;
                }
                // Diagnostic only: supersession is decided by the event's
                // generation and attribution by `origin`, not by this URI.
                let failed_uri = msg
                    .src()
                    .and_then(|src| src.dynamic_cast_ref::<gst::URIHandler>())
                    .and_then(|handler| handler.uri())
                    .map(|uri| uri.to_string());
                PlaybinEvent::Error {
                    origin: self.classify_error_src(msg.src()),
                    error: error.error(),
                    failed_uri,
                }
            }
            MessageView::Warning(warning) => {
                PlaybinEvent::Warning(warning.error().message().to_string())
            }
            MessageView::Tag(tag) => PlaybinEvent::Tags(tag.tags()),
            MessageView::Buffering(buffering) => PlaybinEvent::Buffering(buffering.percent()),
            MessageView::StateChanged(change) => {
                if !msg.src().map(|s| s == pipeline_obj).unwrap_or(false) {
                    return None;
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
                PlaybinEvent::StreamCollection(collection)
            }
            MessageView::StreamsSelected(streams) => {
                let mut video = None;
                let mut audio = None;
                let mut subtitle = None;

                for stream in streams.streams() {
                    let typ = stream.stream_type();
                    let id = stream.stream_id().map(|id| id.to_string());

                    if typ.contains(gst::StreamType::VIDEO) {
                        video = id;
                    } else if typ.contains(gst::StreamType::AUDIO) {
                        audio = id;
                    } else if typ.contains(gst::StreamType::TEXT) {
                        subtitle = id;
                    }
                }

                PlaybinEvent::StreamsSelected {
                    video,
                    audio,
                    subtitle,
                    seqnum: msg.seqnum(),
                }
            }
            MessageView::ClockLost(_) => PlaybinEvent::ClockLost,
            MessageView::AsyncDone(_) => {
                if !msg.src().map(|s| s == pipeline_obj).unwrap_or(false) {
                    return None;
                }
                PlaybinEvent::AsyncDone
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
                let _ = self.set_pipeline_state(target);
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
                    return;
                }

                let position = match seek.position {
                    Some(pos) => pos,
                    None => {
                        let Some(pos) = inner.pipeline.query_position::<gst::ClockTime>() else {
                            error!("Failed to query playback position");
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
                    inner.emit(PlaybinEvent::RateChanged(rate));
                }
            }
            Job::RefreshSeek { seqnum } => {
                let Some(position) = inner.pipeline.query_position::<gst::ClockTime>() else {
                    debug!("Skipping the refresh seek: no position");
                    inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
                    return;
                };

                // A flushing seek to the current position in the current
                // state: re-emits the subtitle cue active NOW and flushes
                // the stale one, without a normal seek's Paused round-trip.
                debug!(
                    ?position,
                    ?seqnum,
                    "Refresh seek (flushing, current position)"
                );
                let event = gst::event::Seek::builder(
                    1.0,
                    gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                    gst::SeekType::Set,
                    position,
                    gst::SeekType::None,
                    gst::ClockTime::NONE,
                )
                .seqnum(seqnum)
                .build();
                if !inner.pipeline.send_event(event) {
                    warn!("Refresh seek failed");
                    inner.emit(PlaybinEvent::RefreshSeekFailed { seqnum });
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
            }
            Job::AttachSub { id, url } => {
                if let Err(err) = self.attach_subtitle_with_id(id, &url) {
                    // No event from here: the input never produces a
                    // stream, which the caller's watchdog detects.
                    error!(?err, url, "fcastplaybin subtitle attach failed");
                }
            }
            Job::DetachSub { id } => {
                if let Err(err) = self.detach_subtitle(id) {
                    // Possible for an attach that already failed (nothing
                    // registered), harmless.
                    debug!(?err, ?id, "fcastplaybin subtitle detach failed");
                }
            }
            Job::DumpDot { done } => {
                let dot = inner
                    .pipeline
                    .debug_to_dot_data(gst::DebugGraphDetails::all());
                done(dot.to_string());
            }
        }
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
        external: Option<ExternalSubId>,
    ) -> Result<()> {
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

        if let Err(err) = element.sync_state_with_parent() {
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
        let sinkpad = db3
            .request_pad_simple("sink_%u")
            .ok_or_else(|| anyhow!("decodebin3 gave no request sink pad"))?;
        pad.link(&sinkpad)
            .with_context(|| format!("linking {} to {}", pad.name(), sinkpad.name()))?;
        debug!(src = %pad.name(), sink = %sinkpad.name(), "linked input pad into decodebin3");

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

        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.db3_sink_pads.push(sinkpad);
            input.taps.push(StreamTap {
                pad: pad.clone(),
                bytes,
                probe,
            });
        } else {
            // Only reachable for an input already removed (detach racing a
            // late pad). Release the pad we just took.
            drop(routing);
            warn!("pad appeared for an unregistered input; releasing");
            db3.release_request_pad(&sinkpad);
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
                Inner::detach_text_from_overlay(inner, &mut routed);
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
    /// video stream routes; idempotent, and also the recovery from a
    /// mid-item deselect's parked chain (unlocks and re-joins it). The
    /// chain lives in the pipeline ONLY while the item has video: an absent
    /// chain cannot hang a video-less preroll and never counts in the bin's
    /// EOS/STREAM_START aggregation, by construction.
    fn ensure_video_chain(&self) -> Result<()> {
        if self.overlay.parent().is_none() {
            let elements: Vec<&gst::Element> = self.video_chain.iter().collect();
            self.pipeline
                .add_many(elements)
                .context("adding the video chain")?;
            // The overlay-to-sink link is made on the first join and
            // persists across membership changes.
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
        }
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
    /// (`unroute_db3_pad`); once removed, the bin's EOS aggregation can no
    /// longer wait on a sink that will never see data again.
    fn remove_video_chain(&self) {
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
    /// Two constraints shape this:
    /// - READY rather than a flush: a FLUSH makes basesink post ASYNC_START
    ///   and the pipeline wedges at pending PAUSED waiting on a re-preroll
    ///   no data will ever finish. Pushes into a READY chain return FLUSHING
    ///   upstream, which decodebin3's deactivation tolerates (an unlink here
    ///   would return NOT_LINKED instead and error the source).
    /// - The state LOCK covers the window until decodebin3 actually removes
    ///   the pad: a pipeline state change walking its children in that
    ///   window would lift the dataless chain back up, and its sink would
    ///   hold the pipeline async forever. `unroute_db3_pad` then removes the
    ///   chain from the pipeline entirely (unlocking it), so the EOS
    ///   aggregation never waits on it either. A re-select routes a fresh
    ///   pad (video-count 0->1 is never a decodebin3 pad reuse) and
    ///   `ensure_video_chain` rebuilds.
    fn park_video_chain_for_deselect(&self) {
        if self.overlay.parent().is_none() {
            return;
        }
        info!("selection drops video, parking the video chain at READY");
        for element in &self.video_chain {
            element.set_locked_state(true);
        }
        // Sink-first: the sink's READY aborts its clock/preroll wait,
        // unwinding the blocked streaming thread out of the branch before
        // the upstream elements deactivate their pads.
        for element in self.video_chain.iter().rev() {
            let _ = element.set_state(gst::State::Ready);
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
    fn remove_input(inner: &Arc<Inner>, input: Input) {
        if let Some(sig) = input.pad_added_sig {
            input.element.disconnect(sig);
        }
        for mut tap in input.taps {
            if let Some(probe) = tap.probe.take() {
                tap.pad.remove_probe(probe);
            }
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
        let ssync = {
            let core = inner.core.lock();
            let Some(core) = core.as_ref() else {
                return Ok(());
            };
            if pad.parent_element().as_ref() != Some(&core.db3) {
                debug!(pad = %pad.name(), "ignoring pad from a superseded core");
                return Ok(());
            }
            core.ssync.clone()
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

        let (ssync_sink, ssync_src, downstream, park_pad, park_sink) = match kind {
            StreamKind::Video => {
                let (ss_sink, ss_src) = attach_ssync()?;
                // Put the video chain into the pipeline (also the recovery
                // from a mid-item deselect's parked chain).
                inner.ensure_video_chain()?;
                let entry = inner
                    .overlay
                    .static_pad("video_sink")
                    .ok_or_else(|| anyhow!("subtitleoverlay video_sink missing"))?;
                ss_src.link(&entry).context("linking video chain")?;
                inner.finish_preroll_token();
                (Some(ss_sink), Some(ss_src), Some(entry), None, None)
            }
            StreamKind::Audio => {
                let (ss_sink, ss_src) = attach_ssync()?;
                // Build this load's fresh audio sink (see
                // `Inner::audio`). The prefix is already active.
                inner.ensure_audio_sink()?;
                let entry = inner
                    .audio_entry
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("audioconvert sink missing"))?;
                ss_src.link(&entry).context("linking audio chain")?;
                inner.finish_preroll_token();
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

        debug!(pad = %pad.name(), ?kind, linked = downstream.is_some(), "routed decodebin3 pad");
        let mut routing = inner.routing.lock();
        routing.routed.push(RoutedStream {
            db3_src_pad: pad.clone(),
            ssync_sink,
            ssync_src,
            downstream,
            park_pad,
            park_sink,
            tqueue: None,
            kind,
        });
        drop(routing);

        // A new text stream may be linkable right away, and a (re)arriving video
        // stream may unblock a parked one.
        if matches!(kind, StreamKind::Text | StreamKind::Video) {
            Inner::poll_text_policy(inner);
        }
        Ok(())
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
            Inner::detach_text_from_overlay(inner, &mut routed);
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
            Inner::park_text_streams(inner);
            // The video pad is gone for good (a mid-item deselect, an input
            // teardown): take the chain out of the pipeline so nothing can
            // aggregate over, or later lift, a sink that will never see
            // data again. A re-select routes a fresh pad and rebuilds.
            inner.remove_video_chain();
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
    fn detach_text_from_overlay(inner: &Arc<Inner>, routed: &mut RoutedStream) {
        let Some(downstream) = routed.downstream.take() else {
            return;
        };
        let _ = downstream.send_event(gst::event::FlushStart::new());
        let _ = downstream.send_event(gst::event::FlushStop::new(true));
        // Text bypasses ssync, so its source is the decodebin3 pad itself.
        let _ = routed.db3_src_pad.unlink(&downstream);
        if let Some(tqueue) = routed.tqueue.take() {
            // The overlay's subtitle input must not stay wired without a
            // live stream: stale caps/renderer state (e.g. a VOBSUB dvdspu
            // splice) wedges the next load's preroll.
            if let Some(qsrc) = tqueue.static_pad("src")
                && let Some(peer) = qsrc.peer()
            {
                let _ = qsrc.unlink(&peer);
            }
            let _ = tqueue.set_state(gst::State::Null);
            let _ = inner.pipeline.remove(&tqueue);
        }
    }

    /// Move overlay-linked text streams back to the parking sink (video is
    /// going away, see `detach_text_from_overlay` for the mechanics).
    fn park_text_streams(inner: &Arc<Inner>) {
        let mut routing = inner.routing.lock();
        for routed in routing
            .routed
            .iter_mut()
            .filter(|r| r.kind == StreamKind::Text && r.downstream.is_some())
        {
            Inner::detach_text_from_overlay(inner, routed);
            match inner.park_stream(&routed.db3_src_pad) {
                Ok((sink, park)) => {
                    debug!(pad = %routed.db3_src_pad.name(), "parked text stream (no video)");
                    routed.park_sink = Some(sink);
                    routed.park_pad = Some(park);
                }
                Err(err) => warn!(?err, "failed to park the text stream"),
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
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::text_may_link(current, pending) {
            return;
        }
        let mut routing = inner.routing.lock();
        if !routing
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Video && r.downstream.is_some())
        {
            return;
        }
        for routed in routing
            .routed
            .iter_mut()
            .filter(|r| r.kind == StreamKind::Text && r.downstream.is_none())
        {
            let Some(overlay_entry) = inner.overlay.static_pad("subtitle_sink") else {
                warn!("subtitleoverlay has no subtitle_sink pad");
                continue;
            };
            if overlay_entry.is_linked() {
                warn!("subtitle_sink already linked; skipping extra text stream");
                continue;
            }
            // Build the per-stream queue (see `RoutedStream::tqueue`) and
            // wire db3-text-pad -> queue -> overlay. The upstream link comes
            // last so data only flows once the chain is complete.
            let tqueue = match gst::ElementFactory::make("queue")
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
    use super::StreamKind;

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

    /// Caps-name fallback for pads without a GstStream.
    pub(crate) fn kind_from_caps_name(name: &str) -> Option<StreamKind> {
        if name.starts_with("video/") {
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
    use super::{StreamKind, decisions::*};

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
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
}
