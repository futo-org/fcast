//! Declarative description of the media a scenario serves.

use crate::caps;

pub const DEFAULT_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(10);
pub const DEFAULT_BYTES_PER_BUFFER: usize = 1024;
pub const DEFAULT_KEYFRAME_INTERVAL: u32 = 25;

/// One audio buffer per 20 ms, the shape of a compressed audio frame.
pub const AUDIO_PACKET: gst::ClockTime = gst::ClockTime::from_mseconds(20);
/// Frame duration ftestsrc falls back to when a spec carries a degenerate
/// framerate (a non-positive numerator or denominator).
pub const FALLBACK_FRAME: gst::ClockTime = gst::ClockTime::from_mseconds(40);

/// PTS of frame `index` at `fps`, the rule ftestsrc schedules by. Computed from
/// the index rather than accumulated, so a frame length that is not a whole
/// number of nanoseconds never drifts: a frame's duration is the difference
/// between two consecutive values.
pub fn frame_pts(index: u64, fps: gst::Fraction) -> gst::ClockTime {
    let (num, den) = (fps.numer(), fps.denom());
    if num <= 0 || den <= 0 {
        return FALLBACK_FRAME * index;
    }
    let nanos =
        (index as u128 * gst::ClockTime::SECOND.nseconds() as u128 * den as u128) / num as u128;
    gst::ClockTime::from_nseconds(nanos as u64)
}

/// Whole buffers of `step` that fit in `duration`. A zero step schedules
/// nothing, which is how a degenerate framerate ends up describing no media.
pub fn buffer_count(duration: gst::ClockTime, step: gst::ClockTime) -> u64 {
    if step.is_zero() {
        return 0;
    }
    duration.nseconds() / step.nseconds()
}

#[derive(Clone, Debug, Default)]
pub struct MediaSpec {
    pub streams: Vec<StreamSpec>,
    /// Root of every jitter draw. All PRNGs derive from it, see [`crate::prng`].
    pub seed: u64,
    /// Scheduled buffering messages over the whole media, see [`BufferingSpec`].
    pub buffering: Option<BufferingSpec>,
    /// Model an adaptive demuxer (DASH/HLS): answer the SELECTABLE query with
    /// TRUE, which makes decodebin3 defer ALL stream selection upstream, and
    /// handle SELECT_STREAMS with adaptivedemux2's semantics (one unknown id
    /// rejects the whole event; a post only on an actual selection change).
    pub upstream_selection: bool,
}

impl MediaSpec {
    pub fn new(seed: u64) -> Self {
        Self {
            streams: Vec::new(),
            seed,
            buffering: None,
            upstream_selection: false,
        }
    }

    pub fn with_stream(mut self, stream: StreamSpec) -> Self {
        self.streams.push(stream);
        self
    }

    pub fn with_buffering(mut self, buffering: BufferingSpec) -> Self {
        self.buffering = Some(buffering);
        self
    }

    pub fn stream(&self, id: &str) -> Option<&StreamSpec> {
        self.streams.iter().find(|s| s.id == id)
    }

    pub fn stream_index(&self, id: &str) -> Option<usize> {
        self.streams.iter().position(|s| s.id == id)
    }
}

#[derive(Clone, Debug)]
pub struct StreamSpec {
    /// Stream-id suffix, see [`caps::stream_id`].
    pub id: String,
    pub kind: StreamKind,
    pub duration: gst::ClockTime,
    /// Buffer payload size. Lets tests hit multiqueue byte limits cheaply.
    pub bytes_per_buffer: usize,
    pub pacing: Pacing,
    pub faults: Vec<Fault>,
    pub decoder: DecoderKnobs,
}

impl StreamSpec {
    pub fn video(id: impl Into<String>) -> Self {
        Self::new(
            id,
            StreamKind::Video {
                width: caps::RAW_VIDEO_WIDTH,
                height: caps::RAW_VIDEO_HEIGHT,
                fps: gst::Fraction::new(25, 1),
                keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
            },
        )
    }

    pub fn audio(id: impl Into<String>) -> Self {
        Self::new(
            id,
            StreamKind::Audio {
                rate: caps::RAW_AUDIO_RATE,
                channels: caps::RAW_AUDIO_CHANNELS,
            },
        )
    }

    pub fn text(id: impl Into<String>, cues: Vec<CueSpec>) -> Self {
        Self::new(id, StreamKind::Text { cues })
    }

    pub fn new(id: impl Into<String>, kind: StreamKind) -> Self {
        Self {
            id: id.into(),
            kind,
            duration: DEFAULT_DURATION,
            bytes_per_buffer: DEFAULT_BYTES_PER_BUFFER,
            pacing: Pacing::default(),
            faults: Vec::new(),
            decoder: DecoderKnobs::default(),
        }
    }

    pub fn with_duration(mut self, duration: gst::ClockTime) -> Self {
        self.duration = duration;
        self
    }

    pub fn with_bytes_per_buffer(mut self, bytes: usize) -> Self {
        self.bytes_per_buffer = bytes;
        self
    }

    pub fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }

    pub fn with_fault(mut self, fault: Fault) -> Self {
        self.faults.push(fault);
        self
    }

    pub fn with_decoder(mut self, decoder: DecoderKnobs) -> Self {
        self.decoder = decoder;
        self
    }

    /// Step between two scheduled buffers: one video frame or one audio packet.
    /// `None` for a text stream, which is scheduled from its cues instead.
    pub fn buffer_step(&self) -> Option<gst::ClockTime> {
        match &self.kind {
            StreamKind::Video { fps, .. } => Some(frame_pts(1, *fps)),
            StreamKind::Audio { .. } => Some(AUDIO_PACKET),
            StreamKind::Text { .. } => None,
        }
    }

    /// How many buffers ftestsrc will push for this stream. The source's own
    /// rule, exposed so a description can be checked before it is registered:
    /// a dense stream that schedules zero buffers is an immediate EOS, not the
    /// media the spec claims.
    pub fn scheduled_buffers(&self) -> u64 {
        match (&self.kind, self.buffer_step()) {
            (StreamKind::Text { cues }, _) => cues.len() as u64,
            (_, Some(step)) => buffer_count(self.duration, step),
            (_, None) => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub enum StreamKind {
    Video {
        width: i32,
        height: i32,
        fps: gst::Fraction,
        keyframe_interval: u32,
    },
    Audio {
        rate: i32,
        channels: i32,
    },
    Text {
        cues: Vec<CueSpec>,
    },
}

impl StreamKind {
    /// Caps ftestsrc puts on the pad. Video and audio are encoded fcasttest caps,
    /// text is already parsed.
    pub fn caps(&self) -> gst::Caps {
        match self {
            Self::Video {
                width, height, fps, ..
            } => caps::video_caps_at(*width, *height, *fps),
            Self::Audio { rate, channels } => caps::audio_caps_at(*rate, *channels),
            Self::Text { .. } => caps::text_caps(),
        }
    }

    pub fn stream_type(&self) -> gst::StreamType {
        match self {
            Self::Video { .. } => gst::StreamType::VIDEO,
            Self::Audio { .. } => gst::StreamType::AUDIO,
            Self::Text { .. } => gst::StreamType::TEXT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CueSpec {
    pub start: gst::ClockTime,
    pub end: gst::ClockTime,
    pub text: String,
}

impl CueSpec {
    pub fn new(start: gst::ClockTime, end: gst::ClockTime, text: impl Into<String>) -> Self {
        Self {
            start,
            end,
            text: text.into(),
        }
    }
}

/// How fast ftestsrc pushes. Jitter draws come from the per-stream PRNG.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pacing {
    #[default]
    AsFastAsPossible,
    Realtime,
    Jitter {
        base_ms: u64,
        jitter_ms: u64,
    },
}

/// Injected source-side failures, indexed by buffer number within the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Blocks the streaming thread on the named sync point before pushing.
    StallAt {
        buffer_index: u64,
        sync_point: String,
    },
    ErrorAt {
        buffer_index: u64,
    },
    /// `GST_ELEMENT_FLOW_ERROR`'s exact shape: "Internal data stream error."
    /// with [`FlowStopReason::debug_text`] as debug info. NOT interchangeable
    /// with [`Fault::ErrorAt`]: fcastplaybin classifies on that debug text
    /// (`decisions::external_error_action`) and recovers in place.
    FlowStoppedAt {
        buffer_index: u64,
        reason: FlowStopReason,
    },
    EosAt {
        buffer_index: u64,
    },
}

/// The flow return a [`Fault::FlowStoppedAt`] blames: a deselect unlinked the
/// branch, or a flush caught the push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowStopReason {
    NotLinked,
    Flushing,
}

impl FlowStopReason {
    /// `GST_ELEMENT_FLOW_ERROR`'s debug text, byte for byte: `gstelement.h:505`
    /// formats `"streaming stopped, reason %s (%d)"` over `gst_flow_get_name`
    /// (`gstpad.c:237-238`) and the enum value (`gstpad.h:172-173`). Consumers
    /// match on this text, so it must not differ from a real give-up.
    pub fn debug_text(self) -> &'static str {
        match self {
            Self::NotLinked => "streaming stopped, reason not-linked (-1)",
            Self::Flushing => "streaming stopped, reason flushing (-2)",
        }
    }
}

/// Scheduled `GST_MESSAGE_BUFFERING` posts covering the whole media.
///
/// `ftest://` media has no buffering element, and a consumer reacts to the bus
/// message rather than to flow control, so ftestsrc posts the messages itself
/// and keeps serving data throughout. Every dip posts [`low_percent`] exactly
/// once at its start and 100 exactly once at its end, and dips run one after
/// another, so a consumer can always leave its buffering state.
///
/// [`low_percent`]: Self::low_percent
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BufferingSpec {
    /// Percent carried by the low post of every dip. Clamped to 0..=99 at post
    /// time, so a dip can never claim the buffer is already full.
    pub low_percent: i32,
    /// A buffering period that begins with the source itself, before any media
    /// flows. The low percent is posted as soon as the element starts and 100
    /// follows after this many milliseconds.
    pub initial_ms: Option<u64>,
    /// Wall-clock dips to low and back to 100, repeating for the life of the
    /// element.
    pub periodic: Option<PeriodicBuffering>,
    /// Dips anchored to a stream's schedule, so a test can time one
    /// deterministically.
    pub dips: Vec<BufferingDip>,
}

impl BufferingSpec {
    pub fn new(low_percent: i32) -> Self {
        Self {
            low_percent,
            initial_ms: None,
            periodic: None,
            dips: Vec::new(),
        }
    }

    pub fn with_initial_ms(mut self, ms: u64) -> Self {
        self.initial_ms = Some(ms);
        self
    }

    pub fn with_periodic(mut self, period_ms: u64, low_ms: u64) -> Self {
        self.periodic = Some(PeriodicBuffering { period_ms, low_ms });
        self
    }

    pub fn with_dip(mut self, dip: BufferingDip) -> Self {
        self.dips.push(dip);
        self
    }
}

/// One wall-clock buffering dip per period, see [`BufferingSpec::periodic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeriodicBuffering {
    /// Time from one dip's start to the next dip's start.
    pub period_ms: u64,
    /// How long each dip holds the low percent before 100 is posted.
    pub low_ms: u64,
}

/// One buffering dip anchored to a stream's schedule. It fires when the named
/// stream DELIVERS the buffer at `buffer_index`, so the low post always trails
/// that buffer downstream. A schedule restarted behind the index by a flushing
/// seek delivers the buffer again and fires the dip again, exactly like
/// [`Fault`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferingDip {
    /// Stream-id suffix of the stream whose schedule anchors the dip.
    pub stream: String,
    /// Buffer number within that stream, the same numbering [`Fault`] uses.
    pub buffer_index: u64,
    pub recovery: BufferingRecovery,
}

/// How an anchored dip's 100 post is scheduled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferingRecovery {
    /// 100 is posted this many milliseconds after the low post.
    AfterMs(u64),
    /// 100 is posted once the test releases the named sync point, so a test
    /// can hold a consumer in its buffering state for as long as it wants. A
    /// gate stays released once released, which makes a dip that refires
    /// after a schedule restart recover immediately.
    OnSyncPoint(String),
}

/// Decoder behaviour. Autoplugged elements cannot be configured by the test, so
/// ftestdec resolves these from the registry via its sink-pad stream-id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecoderKnobs {
    /// Reported latency, and the delay applied to every frame.
    pub latency: Option<gst::ClockTime>,
    pub jitter_ms: u64,
    /// Frames held back before output starts.
    pub reorder_frames: u32,
    /// Drop until a KEYFRAME-flagged buffer arrives after FLUSH_STOP.
    pub needs_keyframe_after_flush: bool,
    pub error_at_frame: Option<u64>,
}
