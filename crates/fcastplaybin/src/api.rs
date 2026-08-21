//! The crate's public value types: what a caller hands in, what it gets
//! back, and the events and payloads the driver reports.

use std::sync::Arc;

use anyhow::Result;

use crate::state_machine::Seek;

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

/// Where a load should begin. Applied by
/// [`FcastPlaybin::load`](crate::FcastPlaybin::load) while the pipeline is
/// still in PAUSED. Applying a non-1.0 rate after PLAYING renders a slice of
/// 1.0x audio that the flushing seek then discards, an audible pop.
#[derive(Debug, Clone, Copy)]
pub enum StartPoint {
    /// Seekable source: after preroll, one flushing seek to `position` at
    /// `rate` (keyframe-landing, no ACCURATE, pinned by
    /// `seek_flags_doc_divergence`). The 1.0x start-of-stream no-op is
    /// skipped, so a plain load never blocks on the seek.
    Seek { position: gst::ClockTime, rate: f64 },
    /// Live source (WHEP/fwebrtc/mirror): preroll only, never seek.
    Live,
}

/// What [`FcastPlaybin::load`](crate::FcastPlaybin::load) learned while
/// prerolling.
#[derive(Debug, Clone, Copy)]
pub struct StartOutcome {
    /// The pipeline prerolled with no data (`NoPreroll`): a live source.
    pub live: bool,
    /// The load's generation (every event carries one, see
    /// [`FcastPlaybin::load_async`](crate::FcastPlaybin::load_async)).
    pub generation: u64,
}

/// Identifies one attached external subtitle input for later detach. The id is
/// STABLE across in-place recoveries (see `Inner::handle_external_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalSubId(pub(crate) u64);

/// A cumulative byte counter for one input stream's PARSED (compressed) data,
/// for bitrate inspection (see
/// [`FcastPlaybin::stream_io_stats`](crate::FcastPlaybin::stream_io_stats)).
/// Counters are per-load by construction (they live and die with the input).
/// Callers sample periodically and derive rates from deltas.
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
/// [`FcastPlaybin::source_summaries`](crate::FcastPlaybin::source_summaries)).
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
/// This replaces playbin3's contextless `failed_uri` guessing. Errors from live
/// external subtitle inputs never surface here: the crate handles them
/// internally (in-place recovery, or [`PlaybinEvent::ExternalSubtitleFailed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorOrigin {
    /// The main input of the CURRENT load.
    Main,
    /// An element of a previous, already-replaced load whose teardown died
    /// noisily. Safe to ignore.
    Stale,
    /// Not attributable to a specific input (sinks, decoders, ...): treat as
    /// the current load's problem.
    Unknown,
}

/// How a cue's payload is encoded.
///
/// [`SubtitleTextFormat::Utf8`] and [`SubtitleTextFormat::PangoMarkup`] mirror
/// the `format` field of the production text caps (`text/x-raw, format={utf8,
/// pango-markup}`): `rssubparse`/`rsssaparse` emit pango-markup by default, the
/// test sources emit utf8.
///
/// [`SubtitleTextFormat::CueIr`] is NOT a third caps format. With
/// `text-format=cue-ir` the parsers still negotiate `text/x-raw, format=utf8`
/// and still push readable UTF-8 text (the IR's own plain text, so buffer and
/// meta cannot disagree about the content). The styling travels beside it in a
/// `CueIrMeta`, which
/// [`Inner::item_from_sample`](crate::Inner::item_from_sample) downcasts off
/// the buffer. The caps gate cannot tell the two apart and does not try. The
/// presence of the meta is the whole signal. See
/// [`decisions::consumer_stream_format`](crate::decisions::consumer_stream_format).
///
/// The renderer lives in another crate (`fcast-video`'s `TextFormat`), which
/// this crate deliberately does not depend on (see
/// [`FcastPlaybin::set_subtitle_consumer`](crate::FcastPlaybin::set_subtitle_consumer)).
/// The IR type is named through `gst-subparse`'s own re-export for the same
/// reason, so there is no second version to keep matched.
#[derive(Debug, Clone, PartialEq)]
pub enum SubtitleTextFormat {
    Utf8,
    PangoMarkup,
    /// A cue parsed with `text-format=cue-ir`. `text` on the enclosing
    /// [`SubtitleFeedItem::Cue`] stays the plain-text rendering, so a consumer
    /// that ignores this variant's payload still shows readable subtitles.
    CueIr {
        /// The styled cue, straight off the buffer's `CueIrMeta`.
        ir: Arc<CueIr>,
        /// The buffer's pts. Karaoke reveal times inside `ir` are absolute on
        /// that timeline, so a renderer needs it to anchor them; `None`
        /// disables per-syllable stepping, which is always safe.
        pts_start: Option<gst::ClockTime>,
    },
}

/// The cue IR the parser elements attach to their buffers, re-exported so a
/// consumer can name it without adding (and version-matching) its own
/// `subparse-formats` dependency.
pub use gstrssubparse::subparse_formats::ir::CueIr;

/// A bitmap ("subpicture") subtitle format, decided from the caps NAME alone.
///
/// These streams carry compressed pictures with their own palettes and
/// geometry, and some reassemble across several buffers. This crate decodes
/// none of it. It names the format so the consumer can hand the bytes to a
/// decoder behind the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitmapSubFormat {
    /// Blu-ray Presentation Graphic Stream, `subpicture/x-pgs`.
    Pgs,
    /// DVD subpicture units, `subpicture/x-dvd`. The palette is out of band,
    /// in the caps' `codec_data`.
    Vobsub,
    /// DVB subtitles (ETSI EN 300 743), `subpicture/x-dvb`.
    Dvb,
}

impl BitmapSubFormat {
    /// Every bitmap format this driver names, in one place. The caps gate,
    /// the lever, and the agreement tests must each cover the whole enum, and
    /// separately written-out lists drift when a format is added.
    pub const ALL: [BitmapSubFormat; 3] = [
        BitmapSubFormat::Pgs,
        BitmapSubFormat::Vobsub,
        BitmapSubFormat::Dvb,
    ];
}

/// Whether a consumer behind this crate can be expected to draw this format.
///
/// Mirrors `fcast_video::subpic::implemented`, duplicated rather than imported
/// because this crate deliberately does not depend on the renderer
/// ([`FcastPlaybin::set_subtitle_consumer`]). The receiver, which sees both
/// crates, asserts the two answers agree for every format.
///
/// Doc-hidden because it exists for that cross-crate assertion, not as API.
///
/// All three formats are implemented. Unsupported caps and lever-disabled
/// formats still take the loud
/// [`PlaybinEvent::SubtitleTrackUnsupported`] path.
#[doc(hidden)]
pub fn bitmap_format_implemented(format: BitmapSubFormat) -> bool {
    match format {
        BitmapSubFormat::Pgs | BitmapSubFormat::Vobsub | BitmapSubFormat::Dvb => true,
    }
}

/// What the subtitle transport hands the consumer installed by
/// [`FcastPlaybin::set_subtitle_consumer`](crate::FcastPlaybin::set_subtitle_consumer).
///
/// Bounds are running times, already resolved against the segment the cue
/// arrived under, so they are directly comparable with the running time of
/// the video frame a renderer is showing.
///
/// **No `PartialEq`, deliberately.** [`SubtitleFeedItem::Bitmap`] carries
/// `gst::Buffer`s, and gstreamer-rs implements buffer equality as a content
/// comparison. A derived `==` would call distinct deliveries of identical
/// bytes equal (common for subtitle packets), so de-duplication would swallow
/// real re-deliveries, and it is not even an equivalence relation because an
/// unmappable buffer compares unequal to itself. Consumers compare the fields
/// they care about. If equality is ever needed, hand-write it by pointer
/// (`a.data.as_ptr() == b.data.as_ptr()`). [`SubtitleTextFormat`] keeps its
/// derive because text carries no buffers.
#[derive(Debug, Clone)]
pub enum SubtitleFeedItem {
    Cue {
        format: SubtitleTextFormat,
        text: String,
        start_rt: gst::ClockTime,
        /// `None` when the buffer carried no duration: an open-ended cue,
        /// live until it is superseded or cleared.
        end_rt: Option<gst::ClockTime>,
        /// The running-time ORIGIN of the segment `start_rt` was resolved
        /// against (the stream position whose running time is zero), stamped
        /// where that segment is in hand. A pad-sticky read cannot answer
        /// this for a park-replayed cue: the fresh branch has not carried its
        /// segment yet at delivery time.
        origin: gst::ClockTime,
    },
    /// One appsink sample of a bitmap subtitle stream, untouched.
    ///
    /// The driver is a byte pipe here: it decides the format from the caps,
    /// resolves the pts to running time and hands the buffer over by
    /// reference-count. Nothing is mapped, copied or validated on the delivery
    /// thread: reassembly and decode belong to the consumer, off-thread,
    /// because these formats are stateful and one of them is measured in
    /// megabytes per page.
    ///
    /// A packet is not a picture: PGS and DVB spread a display set across
    /// several of these, so a consumer that draws one packet at a time draws
    /// nothing.
    Bitmap {
        format: BitmapSubFormat,
        /// The sample's buffer, riding by reference-count.
        data: gst::Buffer,
        /// Out-of-band setup bytes from the caps' `codec_data` field, when the
        /// container supplied any. VOBSUB's palette travels here (matroska
        /// attaches the `.idx` text as CodecPrivate); the other two formats
        /// leave it `None`. Re-read from the caps on every sample, so a
        /// mid-stream renegotiation reaches the decoder with its packet.
        codec_data: Option<gst::Buffer>,
        /// The buffer's pts in running time, on the same base as
        /// [`SubtitleFeedItem::Cue`]'s bounds.
        rt: gst::ClockTime,
        /// The buffer's duration when the container gave one (mkv's
        /// BlockDuration). Most bitmap formats carry their own display
        /// timing in-band and leave this `None`.
        duration: Option<gst::ClockTime>,
    },
    /// Everything delivered so far is stale: drop it. Sent by the transport's
    /// own pad probe on FLUSH_STOP and STREAM_START, and by the driver on a
    /// consumer branch's disposal, on a load/stop supersession, and on a
    /// switch to subtitles-off. Redundant `Clear`s are expected and must be
    /// idempotent.
    Clear,
}

/// The consumer callback installed by
/// [`FcastPlaybin::set_subtitle_consumer`](crate::FcastPlaybin::set_subtitle_consumer).
pub(crate) type SubtitleConsumer = Arc<dyn Fn(SubtitleFeedItem) + Send + Sync>;

/// A typed pipeline event, delivered through the callback installed by
/// [`FcastPlaybin::set_event_handler`](crate::FcastPlaybin::set_event_handler).
/// Bus messages are translated on the posting (streaming) thread, and worker
/// feedback (load completion, seek outcomes) arrives through the same callback:
/// one ordered event source instead of a raw bus plus side channels.
#[derive(Debug)]
pub enum PlaybinEvent {
    EndOfStream,
    /// An async load
    /// ([`FcastPlaybin::load_async`](crate::FcastPlaybin::load_async)) finished
    /// wiring and prerolling its input. `live` mirrors
    /// [`StartOutcome::live`].
    Loaded {
        live: bool,
    },
    Tags(gst::TagList),
    /// The volume changed, a deterministic `notify::volume` from the
    /// dedicated volume element (see
    /// [`FcastPlaybin::set_volume`](crate::FcastPlaybin::set_volume)). Also
    /// re-emitted on demand by
    /// [`FcastPlaybin::renotify_volume`](crate::FcastPlaybin::renotify_volume).
    VolumeChanged(f64),
    /// A stream collection for the caller's stream list. Partial collections
    /// posted by external subtitle inputs are already filtered out so they
    /// cannot clobber the main collection.
    StreamCollection(gst::StreamCollection),
    /// An async state change or flushing seek finished prerolling. Not
    /// attributable to a specific operation because `GstBin` posts its
    /// aggregated ASYNC_DONE with a fresh seqnum.
    AsyncDone,
    /// The media's duration changed and any cached value is stale. Re-query
    /// [`duration`](crate::FcastPlaybin::duration). No payload, mirroring
    /// GStreamer's own `DURATION_CHANGED` contract. Only a fresh query is
    /// authoritative.
    ///
    /// Needed because push-mode demuxers may report an approximate duration
    /// up front and refine it during playback.
    ///
    /// Not emitted for anything describing the next item, i.e. refinements
    /// posted by a prefetching prepared input, or anything while a performed
    /// gapless swap waits to activate (upstream then answers for the
    /// successor, which would poison the caller's view of the current item).
    /// See `Inner::translate_message`.
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
    /// confirms (see
    /// [`FcastPlaybin::select_streams`](crate::FcastPlaybin::select_streams)).
    StreamsSelected {
        video: Option<String>,
        audio: Option<String>,
        subtitle: Option<String>,
        seqnum: gst::Seqnum,
    },
    /// A refresh seek
    /// ([`FcastPlaybin::refresh_seek_async`](crate::FcastPlaybin::refresh_seek_async))
    /// could not be performed. `seqnum` is the one the caller stamped on
    /// it.
    RefreshSeekFailed {
        seqnum: gst::Seqnum,
    },
    RateChanged(f64),
    SeekFailed,
    /// The element providing the pipeline clock went away (e.g. the audio
    /// sink after audio was deselected). Call
    /// [`FcastPlaybin::recover_clock_async`](crate::FcastPlaybin::recover_clock_async) to elect a new clock.
    ClockLost,
    Error {
        /// Which input the error came from (generation-tagged attribution).
        origin: ErrorOrigin,
        error: gst::glib::Error,
        /// URI of the failing source element, when the source is one.
        failed_uri: Option<String>,
    },
    /// An attached external subtitle input failed for good and has already been
    /// DETACHED by the crate: its attach failed outright, a bus error
    /// arrived while its stream was selected (or before it ever produced
    /// one), or it produced no stream within the materialization timeout.
    /// Deselect-race errors recover in place and never surface (see
    /// `Inner::handle_external_error`). The caller drops its bookkeeping for
    /// the id and reports the failure.
    ExternalSubtitleFailed {
        id: ExternalSubId,
    },
    /// The selected subtitle stream carries caps the subtitle transport cannot
    /// deliver, so it stays parked and nothing renders. Loud by design. The
    /// alternative is a user-selected track that silently shows nothing.
    ///
    /// The transport carries `text/x-raw` in utf8 or pango-markup and the
    /// bitmap formats in [`BitmapSubFormat`]. What reaches here is the rest,
    /// e.g. raw ASS/SSA and `subpicture/x-xsub`. Emitted at most once per
    /// (stream id, load generation), so a repeating poll cannot turn one
    /// unrenderable track into an event storm.
    SubtitleTrackUnsupported {
        sid: String,
        caps: gst::Caps,
    },
    /// fimagedec's announcement of an image load: the "fcast-image-stream"
    /// structure with format (str), width/height (i32) and animated (bool).
    /// The caller uses it to classify the load as an image and feed its
    /// inspector; animations otherwise look like ordinary video streams.
    ImageStream(gst::Structure),
    /// The media source is throttled: its server directed a backoff of
    /// `remaining_ms` before the next request (posted by `sabrumpsrc` while
    /// starved). `remaining_ms == 0` means the backoff ended. The caller shows
    /// this as a "server busy" countdown instead of an unexplained stall.
    SourceBackoff {
        remaining_ms: u64,
    },
    /// A prepared next input
    /// ([`FcastPlaybin::prepare_next_async`](crate::FcastPlaybin::prepare_next_async))
    /// went live: the current item drained and decodebin3 switched to the
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
    /// A caller-requested cancel
    /// ([`FcastPlaybin::cancel_prepared_async`](crate::FcastPlaybin::cancel_prepared_async))
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
    /// A non-fatal bus warning, with enough of the original message kept for
    /// the caller to classify it (domain/code in `error`, the message's debug
    /// string, and the posting element's name).
    Warning {
        error: gst::glib::Error,
        src: Option<String>,
        debug: Option<String>,
    },
}

/// What the caller does to the pipeline right after a cancel
/// ([`FcastPlaybin::cancel_prepared_async`](crate::FcastPlaybin::cancel_prepared_async)).
///
/// It decides one thing: whether the crate synthesizes the item end the
/// gapless output hold consumed. While a prepare is pending every EOS at
/// decodebin3's outputs is dropped, and that happens up to a video queue
/// depth (30 s) before the item is audibly over, so a cancel commonly lands
/// with the item still playing an end nothing can surface any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AfterCancel {
    /// Nothing restarts the item's sources (autoplay off, a queue mutation,
    /// a track change). A consumed end is gone for good and is synthesized
    /// as [`PlaybinEvent::EndOfStream`].
    #[default]
    Nothing,
    /// A flushing seek follows, which is the invariant the parked-op slot
    /// exists for: cancel first, then seek. The seek restarts the sources
    /// and regenerates the item's real end, so synthesizing one here would
    /// turn the seek into a skip to the next queue item.
    FlushingSeek,
}

/// First look at every raw bus message, invoked on the posting (streaming)
/// thread, for caller-specific messages the crate does not understand
/// (`NeedContext` for custom source elements, missing-plugin reports).  Return
/// `true` to consume the message. No event is emitted for it.
pub type MessageHook = Box<dyn Fn(&gst::Message) -> bool + Send + Sync>;

/// The caller's event sink. The second argument is the generation of the
/// load the event belongs to (see [`FcastPlaybin::load_async`]).
pub(crate) type EventCallback = Arc<dyn Fn(PlaybinEvent, u64) + Send + Sync>;

/// Builds a fresh audio sink. See [`AudioSink::Factory`].
pub(crate) type AudioSinkFactory = Box<dyn Fn() -> Result<gst::Element> + Send + Sync>;

/// How the audio sink is built. Whatever the choice, the sink is built FRESH
/// for every load and dropped at the next load's reset (see `Inner::audio`
/// for why reuse degrades pulsesink).
pub enum AudioSink {
    /// `autoaudiosink` per load.
    Auto,
    /// Caller-provided factory, invoked once per load.
    Factory(AudioSinkFactory),
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
