//! TOML front-end over [`ScenarioBuilder`].
//!
//! The document maps 1:1 onto the builder and [`crate::spec`], so a scenario
//! file and the equivalent Rust are interchangeable.
//!
//! ```
//! # fcasttest::register_for_tests();
//! let scenario = fcasttest::scenario::toml::load_str(
//!     r#"
//!     key = "docsmoke"
//!     duration_ms = 1200
//!     bytes_per_buffer = 64
//!
//!     [[stream]]
//!     id = "video_0"
//!     kind = "video"
//!
//!     [[stream]]
//!     id = "audio_0"
//!     kind = "audio"
//!     "#,
//! )
//! .expect("a valid document");
//! assert_eq!(scenario.uri(), "ftest://docsmoke");
//! # scenario.unregister();
//! ```
//!
//! Every table denies unknown fields, so a typo is a readable error and never a
//! knob that silently did nothing. The same rule covers media that cannot
//! exist. A dense stream too short for one whole buffer is refused rather than
//! served as an immediate EOS, see [`check_every_stream_has_media`].

use std::{fmt, path::Path};

use serde::Deserialize;

use crate::spec::{
    BufferingDip, BufferingRecovery, BufferingSpec, CuePayload, CueSpec, DecoderKnobs, Fault,
    FlowStopReason, Pacing, PeriodicBuffering, StreamKind, StreamSpec,
};

use super::builder::{ScenarioBuilder, ScenarioHandle};

/// A document that could not be turned into a scenario. Always names what was
/// wrong and, for a file, where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadableError {
    /// The file the document came from, when it came from one.
    pub source: Option<String>,
    pub message: String,
}

impl ReadableError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            source: None,
            message: message.into(),
        }
    }

    fn in_file(mut self, path: &Path) -> Self {
        self.source = Some(path.display().to_string());
        self
    }
}

impl fmt::Display for ReadableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "{source}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ReadableError {}

/// Parses `toml` and registers the scenario, replacing any previous entry under
/// the same key. Call BEFORE building the pipeline.
pub fn load_str(toml: &str) -> Result<ScenarioHandle, ReadableError> {
    Ok(parse_str(toml)?.register())
}

/// [`load_str`] over a file. Read and parse errors both name the path.
pub fn load_file(path: impl AsRef<Path>) -> Result<ScenarioHandle, ReadableError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|err| {
        ReadableError::new(format!("cannot read the scenario file: {err}")).in_file(path)
    })?;
    parse_str(&text)
        .map_err(|err| err.in_file(path))
        .map(ScenarioBuilder::register)
}

/// Parses without registering. Lets a caller inspect or amend the builder
/// first.
pub fn parse_str(toml: &str) -> Result<ScenarioBuilder, ReadableError> {
    let document: Document = ::toml::from_str(toml).map_err(|err| {
        // toml's message already carries the offending field and its span.
        ReadableError::new(err.to_string().trim_end().to_owned())
    })?;
    document.into_builder()
}

/// Top-level keys are the [`ScenarioBuilder`] setters, `[[stream]]` its
/// streams.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    /// Registry key, also the `ftest://` host and the seed when `seed` is
    /// absent.
    key: String,
    seed: Option<u64>,
    /// Applied to every stream, whatever the individual entries say.
    duration_ms: Option<u64>,
    pacing: Option<PacingDoc>,
    bytes_per_buffer: Option<usize>,
    /// Scheduled buffering messages over the whole media.
    buffering: Option<BufferingDoc>,
    #[serde(default, rename = "stream")]
    streams: Vec<StreamDoc>,
}

impl Document {
    fn into_builder(self) -> Result<ScenarioBuilder, ReadableError> {
        if self.streams.is_empty() {
            return Err(ReadableError::new(
                "stream: a scenario needs at least one [[stream]] entry",
            ));
        }
        let mut builder = ScenarioBuilder::new(validated_key(&self.key)?);
        if let Some(seed) = self.seed {
            builder = builder.seed(seed);
        }
        if let Some(ms) = self.duration_ms {
            builder = builder.duration(gst::ClockTime::from_mseconds(ms));
        }
        if let Some(pacing) = self.pacing {
            builder = builder.pacing(pacing.into_spec()?);
        }
        if let Some(bytes) = self.bytes_per_buffer {
            builder = builder.bytes_per_buffer(bytes);
        }
        if let Some(buffering) = self.buffering {
            builder = builder.buffering(buffering.into_spec()?);
        }
        for (index, stream) in self.streams.into_iter().enumerate() {
            builder = builder.stream(stream.into_spec(index)?);
        }
        check_every_stream_has_media(&builder)?;
        check_every_dip_names_a_stream(&builder)?;
        Ok(builder)
    }
}

/// A dip anchored to an undeclared stream would never fire, which is the "knob
/// that silently did nothing" this module refuses.
fn check_every_dip_names_a_stream(builder: &ScenarioBuilder) -> Result<(), ReadableError> {
    let spec = builder.resolved_spec();
    let Some(buffering) = &spec.buffering else {
        return Ok(());
    };
    for (index, dip) in buffering.dips.iter().enumerate() {
        if spec.stream(&dip.stream).is_none() {
            return Err(ReadableError::new(format!(
                "buffering.dip[{index}]: stream {:?} names no [[stream]] in this \
                 document, so the dip could never fire",
                dip.stream
            )));
        }
    }
    Ok(())
}

/// A dense stream whose duration holds no whole buffer schedules nothing and
/// its pad is an immediate EOS. Checked against the RESOLVED spec, since a
/// top-level `duration_ms` wins over per-stream.
///
/// Text is exempt. A sparse stream with no cues is all GAP by design.
fn check_every_stream_has_media(builder: &ScenarioBuilder) -> Result<(), ReadableError> {
    let spec = builder.resolved_spec();
    for (index, stream) in spec.streams.iter().enumerate() {
        let Some(step) = stream.buffer_step() else {
            continue;
        };
        if stream.scheduled_buffers() > 0 {
            continue;
        }
        return Err(ReadableError::new(format!(
            "stream[{index}] ({}): {} ms of media holds no whole buffer of {} ns, so \
             the stream schedules none at all and the pad is an immediate EOS. Raise \
             duration_ms, or lower fps.",
            stream.id,
            stream.duration.mseconds(),
            step.nseconds(),
        )));
    }
    Ok(())
}

/// [`ScenarioBuilder::new`] asserts on a bad key. A document is data, so it
/// gets an error instead.
fn validated_key(key: &str) -> Result<&str, ReadableError> {
    if key.is_empty() {
        return Err(ReadableError::new("key: must not be empty"));
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ReadableError::new(format!(
            "key: {key:?} must be ASCII alphanumeric or '_' \
             ('-' separates stream-ids, '/' separates recording handles)"
        )));
    }
    Ok(key)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamDoc {
    /// Stream-id suffix. Reaches fcastplaybin's selection API verbatim.
    id: String,
    kind: KindDoc,
    // Video only.
    width: Option<i32>,
    height: Option<i32>,
    fps: Option<FpsDoc>,
    keyframe_interval: Option<u32>,
    // Audio only.
    rate: Option<i32>,
    channels: Option<i32>,
    // Text only.
    #[serde(default, rename = "cue")]
    cues: Vec<CueDoc>,
    duration_ms: Option<u64>,
    bytes_per_buffer: Option<usize>,
    pacing: Option<PacingDoc>,
    #[serde(default, rename = "fault")]
    faults: Vec<FaultDoc>,
    decoder: Option<DecoderDoc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum KindDoc {
    Video,
    Audio,
    Text,
}

/// `fps = 25`, or `fps = [30000, 1001]` for real framerates (30000/1001 is
/// 29.97). The builder's framerate is a `gst::Fraction`, so a whole-number-only
/// document could not describe every scenario, and [`to_toml`] would round
/// 29.97 to 29.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
enum FpsDoc {
    Whole(i64),
    Ratio([i32; 2]),
}

impl FpsDoc {
    fn into_fraction(self, at: impl Fn(&str) -> String) -> Result<gst::Fraction, ReadableError> {
        let (numer, denom) = match self {
            Self::Whole(whole) => (whole, 1i64),
            Self::Ratio([numer, denom]) => (i64::from(numer), i64::from(denom)),
        };
        if numer <= 0 || denom <= 0 {
            return Err(ReadableError::new(at("fps must be positive")));
        }
        let numer = i32::try_from(numer)
            .map_err(|_| ReadableError::new(at("fps does not fit in an i32")))?;
        let denom = i32::try_from(denom)
            .map_err(|_| ReadableError::new(at("fps does not fit in an i32")))?;
        Ok(gst::Fraction::new(numer, denom))
    }
}

impl StreamDoc {
    fn into_spec(self, index: usize) -> Result<StreamSpec, ReadableError> {
        let at = |field: &str| format!("stream[{index}] ({}): {field}", self.id);
        let wrong_kind = |field: &str, kind: &str| {
            Err(ReadableError::new(at(&format!(
                "{field} only applies to a {kind} stream"
            ))))
        };

        let kind = match self.kind {
            KindDoc::Video => {
                if !self.cues.is_empty() {
                    return wrong_kind("cue", "text");
                }
                if self.rate.is_some() || self.channels.is_some() {
                    return wrong_kind("rate/channels", "audio");
                }
                let fps = match self.fps {
                    Some(fps) => fps.into_fraction(at)?,
                    None => gst::Fraction::new(25, 1),
                };
                StreamKind::Video {
                    width: self.width.unwrap_or(crate::caps::RAW_VIDEO_WIDTH),
                    height: self.height.unwrap_or(crate::caps::RAW_VIDEO_HEIGHT),
                    fps,
                    keyframe_interval: self
                        .keyframe_interval
                        .unwrap_or(crate::spec::DEFAULT_KEYFRAME_INTERVAL),
                }
            }
            KindDoc::Audio => {
                if !self.cues.is_empty() {
                    return wrong_kind("cue", "text");
                }
                if self.width.is_some()
                    || self.height.is_some()
                    || self.fps.is_some()
                    || self.keyframe_interval.is_some()
                {
                    return wrong_kind("width/height/fps/keyframe_interval", "video");
                }
                StreamKind::Audio {
                    rate: self.rate.unwrap_or(crate::caps::RAW_AUDIO_RATE),
                    channels: self.channels.unwrap_or(crate::caps::RAW_AUDIO_CHANNELS),
                }
            }
            KindDoc::Text => {
                if self.width.is_some()
                    || self.height.is_some()
                    || self.fps.is_some()
                    || self.keyframe_interval.is_some()
                {
                    return wrong_kind("width/height/fps/keyframe_interval", "video");
                }
                if self.rate.is_some() || self.channels.is_some() {
                    return wrong_kind("rate/channels", "audio");
                }
                let mut cues = Vec::with_capacity(self.cues.len());
                for (cue_index, cue) in self.cues.iter().enumerate() {
                    if cue.end_ms < cue.start_ms {
                        return Err(ReadableError::new(at(&format!(
                            "cue[{cue_index}] ends before it starts"
                        ))));
                    }
                    let start = gst::ClockTime::from_mseconds(cue.start_ms);
                    let end = gst::ClockTime::from_mseconds(cue.end_ms);
                    cues.push(match (&cue.text, &cue.packets) {
                        (Some(text), None) => CueSpec::new(start, end, text.clone()),
                        (None, Some(packets)) => CueSpec::packets(start, end, packets.clone()),
                        _ => {
                            return Err(ReadableError::new(at(&format!(
                                "cue[{cue_index}] needs exactly one of `text` and `packets`"
                            ))));
                        }
                    });
                }
                StreamKind::Text { cues }
            }
        };

        let mut spec = StreamSpec::new(self.id.clone(), kind);
        if let Some(ms) = self.duration_ms {
            spec = spec.with_duration(gst::ClockTime::from_mseconds(ms));
        }
        if let Some(bytes) = self.bytes_per_buffer {
            spec = spec.with_bytes_per_buffer(bytes);
        }
        if let Some(pacing) = self.pacing {
            spec = spec.with_pacing(pacing.into_spec()?);
        }
        for fault in &self.faults {
            spec = spec.with_fault(fault.to_spec(&at("fault"))?);
        }
        if let Some(decoder) = self.decoder {
            spec = spec.with_decoder(decoder.into_spec());
        }
        Ok(spec)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CueDoc {
    start_ms: u64,
    end_ms: u64,
    /// A text cue's payload. Exactly one of this and `packets` is set.
    text: Option<String>,
    /// A bitmap cue's payload: one byte array per buffer, all pushed at
    /// `start_ms` (see `CueSpec::packets`).
    packets: Option<Vec<Vec<u8>>>,
}

/// `pacing = "as_fast_as_possible" | "realtime"` or
/// `pacing = { jitter = { base_ms = 10, jitter_ms = 5 } }`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PacingDoc {
    AsFastAsPossible,
    Realtime,
    Jitter { base_ms: u64, jitter_ms: u64 },
}

impl PacingDoc {
    fn into_spec(self) -> Result<Pacing, ReadableError> {
        Ok(match self {
            Self::AsFastAsPossible => Pacing::AsFastAsPossible,
            Self::Realtime => Pacing::Realtime,
            Self::Jitter { base_ms, jitter_ms } => Pacing::Jitter { base_ms, jitter_ms },
        })
    }
}

/// One injected source-side failure. `sync_point` belongs to `stall` alone.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FaultDoc {
    /// Buffer number within the stream, what every fault indexes.
    buffer_index: u64,
    /// One of `stall`, `error`, `deselected`, `flushed`, `eos`.
    kind: FaultKindDoc,
    sync_point: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FaultKindDoc {
    Stall,
    Error,
    /// [`Fault::FlowStoppedAt`] with [`FlowStopReason::NotLinked`], a deselect
    /// catching the source. Named by cause, which is what a scenario describes.
    Deselected,
    /// [`Fault::FlowStoppedAt`] with [`FlowStopReason::Flushing`], a flush
    /// catching the source mid-push.
    Flushed,
    Eos,
}

impl FaultDoc {
    fn to_spec(&self, at: &str) -> Result<Fault, ReadableError> {
        match self.kind {
            FaultKindDoc::Stall => {
                let Some(sync_point) = self.sync_point.clone() else {
                    return Err(ReadableError::new(format!(
                        "{at}: a stall fault needs a sync_point to park on"
                    )));
                };
                Ok(Fault::StallAt {
                    buffer_index: self.buffer_index,
                    sync_point,
                })
            }
            _ if self.sync_point.is_some() => Err(ReadableError::new(format!(
                "{at}: sync_point only applies to a stall fault"
            ))),
            FaultKindDoc::Error => Ok(Fault::ErrorAt {
                buffer_index: self.buffer_index,
            }),
            FaultKindDoc::Deselected => Ok(Fault::FlowStoppedAt {
                buffer_index: self.buffer_index,
                reason: FlowStopReason::NotLinked,
            }),
            FaultKindDoc::Flushed => Ok(Fault::FlowStoppedAt {
                buffer_index: self.buffer_index,
                reason: FlowStopReason::Flushing,
            }),
            FaultKindDoc::Eos => Ok(Fault::EosAt {
                buffer_index: self.buffer_index,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecoderDoc {
    /// Reported latency, and the delay applied to every frame.
    latency_ms: Option<u64>,
    #[serde(default)]
    jitter_ms: u64,
    #[serde(default)]
    reorder_frames: u32,
    #[serde(default)]
    needs_keyframe_after_flush: bool,
    error_at_frame: Option<u64>,
}

impl DecoderDoc {
    fn into_spec(self) -> DecoderKnobs {
        DecoderKnobs {
            latency: self.latency_ms.map(gst::ClockTime::from_mseconds),
            jitter_ms: self.jitter_ms,
            reorder_frames: self.reorder_frames,
            needs_keyframe_after_flush: self.needs_keyframe_after_flush,
            error_at_frame: self.error_at_frame,
        }
    }
}

/// The `[buffering]` table, scheduled `GST_MESSAGE_BUFFERING` posts, see
/// [`crate::spec::BufferingSpec`]. A table that schedules nothing is refused,
/// the same way an unknown field is.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BufferingDoc {
    /// Percent carried by every low post, 0..=99.
    low_percent: i32,
    /// A buffering period that begins with the source, before any media flows.
    initial_ms: Option<u64>,
    periodic: Option<PeriodicDoc>,
    #[serde(default, rename = "dip")]
    dips: Vec<DipDoc>,
}

/// The `[buffering.periodic]` table, wall-clock dips to low and back to 100.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeriodicDoc {
    period_ms: u64,
    low_ms: u64,
}

/// One `[[buffering.dip]]`, anchored to a stream's buffer index. Exactly one of
/// `recover_after_ms` and `recover_on` is set.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DipDoc {
    /// Stream-id suffix of the stream whose schedule anchors the dip.
    stream: String,
    buffer_index: u64,
    /// Milliseconds from the low post to the 100 post.
    recover_after_ms: Option<u64>,
    /// Sync point whose release posts the 100.
    recover_on: Option<String>,
}

impl BufferingDoc {
    fn into_spec(self) -> Result<BufferingSpec, ReadableError> {
        if !(0..=99).contains(&self.low_percent) {
            return Err(ReadableError::new(format!(
                "buffering: low_percent {} is outside 0..=99, and a dip that starts \
                 at 100 never buffers at all",
                self.low_percent
            )));
        }
        if self.initial_ms.is_none() && self.periodic.is_none() && self.dips.is_empty() {
            return Err(ReadableError::new(
                "buffering: describes no buffering at all. Set initial_ms, add \
                 [buffering.periodic] or add a [[buffering.dip]]",
            ));
        }
        let mut spec = BufferingSpec::new(self.low_percent);
        spec.initial_ms = self.initial_ms;
        spec.periodic = self.periodic.map(|periodic| PeriodicBuffering {
            period_ms: periodic.period_ms,
            low_ms: periodic.low_ms,
        });
        for (index, dip) in self.dips.into_iter().enumerate() {
            spec.dips.push(dip.into_spec(index)?);
        }
        Ok(spec)
    }
}

impl DipDoc {
    fn into_spec(self, index: usize) -> Result<BufferingDip, ReadableError> {
        let recovery = match (self.recover_after_ms, self.recover_on) {
            (Some(ms), None) => BufferingRecovery::AfterMs(ms),
            (None, Some(name)) => BufferingRecovery::OnSyncPoint(name),
            _ => {
                return Err(ReadableError::new(format!(
                    "buffering.dip[{index}]: set exactly one of recover_after_ms and \
                     recover_on, or the dip cannot decide how its 100 is posted"
                )));
            }
        };
        Ok(BufferingDip {
            stream: self.stream,
            buffer_index: self.buffer_index,
            recovery,
        })
    }
}

/// Serialises a registered scenario back into a document [`load_str`] accepts,
/// so a fuzz crash becomes a file that replays it.
pub fn to_toml(handle: &ScenarioHandle) -> String {
    let spec = handle.spec();
    let mut out = String::new();
    out.push_str(&format!("key = {}\n", toml_string(handle.key())));
    out.push_str(&format!("seed = {}\n", spec.seed));
    if let Some(buffering) = &spec.buffering {
        out.push_str("\n[buffering]\n");
        out.push_str(&format!("low_percent = {}\n", buffering.low_percent));
        if let Some(ms) = buffering.initial_ms {
            out.push_str(&format!("initial_ms = {ms}\n"));
        }
        if let Some(periodic) = buffering.periodic {
            out.push_str("\n[buffering.periodic]\n");
            out.push_str(&format!(
                "period_ms = {}\nlow_ms = {}\n",
                periodic.period_ms, periodic.low_ms
            ));
        }
        for dip in &buffering.dips {
            out.push_str("\n[[buffering.dip]]\n");
            out.push_str(&format!("stream = {}\n", toml_string(&dip.stream)));
            out.push_str(&format!("buffer_index = {}\n", dip.buffer_index));
            match &dip.recovery {
                BufferingRecovery::AfterMs(ms) => {
                    out.push_str(&format!("recover_after_ms = {ms}\n"))
                }
                BufferingRecovery::OnSyncPoint(name) => {
                    out.push_str(&format!("recover_on = {}\n", toml_string(name)))
                }
            }
        }
    }
    for stream in &spec.streams {
        out.push_str("\n[[stream]]\n");
        out.push_str(&format!("id = {}\n", toml_string(&stream.id)));
        match &stream.kind {
            StreamKind::Video {
                width,
                height,
                fps,
                keyframe_interval,
            } => {
                out.push_str("kind = \"video\"\n");
                out.push_str(&format!("width = {width}\nheight = {height}\n"));
                // A non-whole framerate keeps its denominator, or the replay
                // plays at a different rate.
                out.push_str(&match (fps.numer(), fps.denom()) {
                    (numer, 1) => format!("fps = {numer}\n"),
                    (numer, denom) => format!("fps = [{numer}, {denom}]\n"),
                });
                out.push_str(&format!("keyframe_interval = {keyframe_interval}\n"));
            }
            StreamKind::Audio { rate, channels } => {
                out.push_str("kind = \"audio\"\n");
                out.push_str(&format!("rate = {rate}\nchannels = {channels}\n"));
            }
            StreamKind::Text { .. } => out.push_str("kind = \"text\"\n"),
        }
        out.push_str(&format!("duration_ms = {}\n", stream.duration.mseconds()));
        out.push_str(&format!("bytes_per_buffer = {}\n", stream.bytes_per_buffer));
        out.push_str(&format!("pacing = {}\n", pacing_to_toml(stream.pacing)));
        let decoder = stream.decoder;
        if decoder != DecoderKnobs::default() {
            out.push_str("\n[stream.decoder]\n");
            if let Some(latency) = decoder.latency {
                out.push_str(&format!("latency_ms = {}\n", latency.mseconds()));
            }
            out.push_str(&format!("jitter_ms = {}\n", decoder.jitter_ms));
            out.push_str(&format!("reorder_frames = {}\n", decoder.reorder_frames));
            out.push_str(&format!(
                "needs_keyframe_after_flush = {}\n",
                decoder.needs_keyframe_after_flush
            ));
            if let Some(frame) = decoder.error_at_frame {
                out.push_str(&format!("error_at_frame = {frame}\n"));
            }
        }
        for fault in &stream.faults {
            out.push_str("\n[[stream.fault]]\n");
            match fault {
                Fault::StallAt {
                    buffer_index,
                    sync_point,
                } => out.push_str(&format!(
                    "kind = \"stall\"\nbuffer_index = {buffer_index}\nsync_point = {}\n",
                    toml_string(sync_point)
                )),
                Fault::ErrorAt { buffer_index } => out.push_str(&format!(
                    "kind = \"error\"\nbuffer_index = {buffer_index}\n"
                )),
                Fault::FlowStoppedAt {
                    buffer_index,
                    reason,
                } => {
                    let kind = match reason {
                        FlowStopReason::NotLinked => "deselected",
                        FlowStopReason::Flushing => "flushed",
                    };
                    out.push_str(&format!(
                        "kind = {}\nbuffer_index = {buffer_index}\n",
                        toml_string(kind)
                    ));
                }
                Fault::EosAt { buffer_index } => {
                    out.push_str(&format!("kind = \"eos\"\nbuffer_index = {buffer_index}\n"))
                }
            }
        }
        if let StreamKind::Text { cues } = &stream.kind {
            for cue in cues {
                out.push_str("\n[[stream.cue]]\n");
                out.push_str(&format!(
                    "start_ms = {}\nend_ms = {}\n",
                    cue.start.mseconds(),
                    cue.end.mseconds(),
                ));
                match &cue.payload {
                    CuePayload::Text(text) => {
                        out.push_str(&format!("text = {}\n", toml_string(text)));
                    }
                    // Byte arrays rather than a string. The bytes may be
                    // non-printable and the dump must read back exactly.
                    CuePayload::Packets(packets) => {
                        out.push_str("packets = [");
                        for (index, packet) in packets.iter().enumerate() {
                            if index > 0 {
                                out.push_str(", ");
                            }
                            out.push('[');
                            for (byte_index, byte) in packet.iter().enumerate() {
                                if byte_index > 0 {
                                    out.push_str(", ");
                                }
                                out.push_str(&byte.to_string());
                            }
                            out.push(']');
                        }
                        out.push_str("]\n");
                    }
                }
            }
        }
    }
    out
}

/// One TOML basic string, quoted and escaped as the TOML spec defines.
///
/// Rust's `{:?}` is NOT a substitute. It writes escapes TOML does not accept,
/// so a control character in a value would make the dump unparseable.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            // Every other control character, which TOML forbids literally.
            other if (other as u32) < 0x20 || other == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn pacing_to_toml(pacing: Pacing) -> String {
    match pacing {
        Pacing::AsFastAsPossible => "\"as_fast_as_possible\"".to_owned(),
        Pacing::Realtime => "\"realtime\"".to_owned(),
        Pacing::Jitter { base_ms, jitter_ms } => {
            format!("{{ jitter = {{ base_ms = {base_ms}, jitter_ms = {jitter_ms} }} }}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neither builder nor handle is Debug, so the tests unwrap the error side
    /// by hand.
    fn err_of<T>(result: Result<T, ReadableError>, what: &str) -> ReadableError {
        match result {
            Ok(_) => panic!("expected an error: {what}"),
            Err(err) => err,
        }
    }

    const FULL: &str = r#"
        key = "tomlfull"
        seed = 1234
        duration_ms = 900
        pacing = { jitter = { base_ms = 3, jitter_ms = 7 } }
        bytes_per_buffer = 128

        [buffering]
        low_percent = 35
        initial_ms = 120

        [buffering.periodic]
        period_ms = 700
        low_ms = 90

        [[buffering.dip]]
        stream = "video_0"
        buffer_index = 4
        recover_after_ms = 60

        [[buffering.dip]]
        stream = "audio_0"
        buffer_index = 9
        recover_on = "refill"

        [[stream]]
        id = "video_0"
        kind = "video"
        width = 32
        height = 24
        fps = 10
        keyframe_interval = 5

        [stream.decoder]
        latency_ms = 40
        jitter_ms = 2
        reorder_frames = 3
        needs_keyframe_after_flush = true
        error_at_frame = 99

        [[stream.fault]]
        kind = "stall"
        buffer_index = 4
        sync_point = "midpush"

        [[stream.fault]]
        kind = "eos"
        buffer_index = 12

        [[stream]]
        id = "audio_0"
        kind = "audio"
        rate = 44100
        channels = 2
        pacing = "realtime"

        [[stream.fault]]
        kind = "error"
        buffer_index = 7

        [[stream.fault]]
        kind = "deselected"
        buffer_index = 8

        [[stream.fault]]
        kind = "flushed"
        buffer_index = 9

        [[stream]]
        id = "text_0"
        kind = "text"

        [[stream.cue]]
        start_ms = 100
        end_ms = 400
        text = "CUE00"

        [[stream.cue]]
        start_ms = 500
        end_ms = 800
        text = "CUE01"
    "#;

    #[test]
    fn a_full_document_reaches_every_knob() {
        gst::init().unwrap();
        let handle = load_str(FULL).expect("the full document");
        assert_eq!(handle.key(), "tomlfull");
        assert_eq!(handle.uri(), "ftest://tomlfull");
        let spec = handle.spec();
        assert_eq!(spec.seed, 1234);
        assert_eq!(spec.streams.len(), 3);

        let video = spec.stream("video_0").expect("the video stream");
        assert!(matches!(
            video.kind,
            StreamKind::Video {
                width: 32,
                height: 24,
                keyframe_interval: 5,
                ..
            }
        ));
        // Top-level overrides win over per-stream defaults, as
        // ScenarioBuilder::register applies them.
        assert_eq!(video.duration, gst::ClockTime::from_mseconds(900));
        assert_eq!(video.bytes_per_buffer, 128);
        assert_eq!(
            video.pacing,
            Pacing::Jitter {
                base_ms: 3,
                jitter_ms: 7
            }
        );
        assert_eq!(
            video.decoder,
            DecoderKnobs {
                latency: Some(gst::ClockTime::from_mseconds(40)),
                jitter_ms: 2,
                reorder_frames: 3,
                needs_keyframe_after_flush: true,
                error_at_frame: Some(99),
            }
        );
        assert_eq!(
            video.faults,
            vec![
                Fault::StallAt {
                    buffer_index: 4,
                    sync_point: "midpush".to_owned()
                },
                Fault::EosAt { buffer_index: 12 },
            ]
        );

        let audio = spec.stream("audio_0").expect("the audio stream");
        assert!(matches!(
            audio.kind,
            StreamKind::Audio {
                rate: 44100,
                channels: 2
            }
        ));
        assert_eq!(
            audio.faults,
            vec![
                Fault::ErrorAt { buffer_index: 7 },
                Fault::FlowStoppedAt {
                    buffer_index: 8,
                    reason: FlowStopReason::NotLinked
                },
                Fault::FlowStoppedAt {
                    buffer_index: 9,
                    reason: FlowStopReason::Flushing
                },
            ]
        );

        let text = spec.stream("text_0").expect("the text stream");
        let StreamKind::Text { cues } = &text.kind else {
            panic!("the text stream lost its kind");
        };
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].text(), Some("CUE01"));
        assert_eq!(cues[1].start, gst::ClockTime::from_mseconds(500));

        assert_eq!(
            spec.buffering,
            Some(
                BufferingSpec::new(35)
                    .with_initial_ms(120)
                    .with_periodic(700, 90)
                    .with_dip(BufferingDip {
                        stream: "video_0".to_owned(),
                        buffer_index: 4,
                        recovery: BufferingRecovery::AfterMs(60),
                    })
                    .with_dip(BufferingDip {
                        stream: "audio_0".to_owned(),
                        buffer_index: 9,
                        recovery: BufferingRecovery::OnSyncPoint("refill".to_owned()),
                    })
            )
        );

        handle.unregister();
    }

    #[test]
    fn the_full_document_round_trips_through_to_toml() {
        gst::init().unwrap();
        let first = load_str(FULL).expect("the full document");
        let rendered = to_toml(&first);
        let second = load_str(&rendered)
            .unwrap_or_else(|err| panic!("re-reading the rendered document: {err}\n{rendered}"));

        // Per-stream pacing survives the round trip because to_toml writes the
        // resolved value on every stream and no top-level override.
        let (left, right) = (first.spec(), second.spec());
        assert_eq!(left.seed, right.seed);
        assert_eq!(left.buffering, right.buffering);
        assert!(
            left.buffering.is_some(),
            "the FULL document must exercise the buffering knob"
        );
        assert_eq!(left.streams.len(), right.streams.len());
        for (left, right) in left.streams.iter().zip(&right.streams) {
            assert_eq!(left.id, right.id);
            assert_eq!(left.duration, right.duration);
            assert_eq!(left.bytes_per_buffer, right.bytes_per_buffer);
            assert_eq!(left.pacing, right.pacing);
            assert_eq!(left.faults, right.faults);
            assert_eq!(left.decoder, right.decoder);
            assert_eq!(
                format!("{:?}", left.kind),
                format!("{:?}", right.kind),
                "the stream kind changed shape"
            );
        }
        second.unregister();
    }

    #[test]
    fn a_minimal_document_takes_every_default() {
        gst::init().unwrap();
        let handle = load_str(
            r#"
            key = "tomlmin"
            [[stream]]
            id = "audio_0"
            kind = "audio"
            "#,
        )
        .expect("the minimal document");
        let audio = handle.spec().stream("audio_0").expect("the audio stream");
        assert_eq!(audio.duration, crate::spec::DEFAULT_DURATION);
        assert_eq!(
            audio.bytes_per_buffer,
            crate::spec::DEFAULT_BYTES_PER_BUFFER
        );
        assert_eq!(audio.pacing, Pacing::AsFastAsPossible);
        assert_eq!(audio.decoder, DecoderKnobs::default());
        assert!(audio.faults.is_empty());
        // No explicit seed, so the key determines it, see ScenarioBuilder.
        assert_eq!(handle.spec().seed, super::super::stable_seed("tomlmin"));
        handle.unregister();
    }

    #[test]
    fn fps_takes_a_whole_number_or_a_ratio() {
        gst::init().unwrap();
        let fps_of = |fps: &str| {
            let document = format!(
                r#"key = "tomlfps"
                   duration_ms = 4000
                   [[stream]]
                   id = "video_0"
                   kind = "video"
                   fps = {fps}"#
            );
            let builder =
                parse_str(&document).unwrap_or_else(|err| panic!("fps = {fps} was refused: {err}"));
            let spec = builder.resolved_spec();
            match spec.stream("video_0").expect("the video stream").kind {
                StreamKind::Video { fps, .. } => fps,
                ref other => panic!("not a video stream: {other:?}"),
            }
        };
        assert_eq!(fps_of("25"), gst::Fraction::new(25, 1));
        assert_eq!(fps_of("[30000, 1001]"), gst::Fraction::new(30000, 1001));
        assert_eq!(fps_of("[24, 1]"), gst::Fraction::new(24, 1));

        for bad in ["0", "-3", "[30000, 0]", "[-1, 1]", "5000000000"] {
            let document = format!(
                r#"key = "tomlfpsbad"
                   [[stream]]
                   id = "video_0"
                   kind = "video"
                   fps = {bad}"#
            );
            let err = err_of(parse_str(&document), &document);
            assert!(
                err.to_string().contains("fps"),
                "fps = {bad} was not refused with a message about fps: {err}"
            );
        }
    }

    #[test]
    fn an_unknown_field_names_itself() {
        gst::init().unwrap();
        let err = err_of(
            parse_str(
                r#"
            key = "tomlbad"
            bytes_per_bufer = 64
            [[stream]]
            id = "audio_0"
            kind = "audio"
            "#,
            ),
            "the misspelled key is refused",
        );
        assert!(
            err.to_string().contains("bytes_per_bufer"),
            "the error does not name the offending field: {err}"
        );

        let err = err_of(
            parse_str(
                r#"
            key = "tomlbad"
            [[stream]]
            id = "audio_0"
            kind = "audio"
            reorder_frames = 2
            "#,
            ),
            "a decoder knob outside [stream.decoder] is refused",
        );
        assert!(
            err.to_string().contains("reorder_frames"),
            "the error does not name the offending field: {err}"
        );
    }

    #[test]
    fn wrong_kind_and_missing_pieces_are_named() {
        gst::init().unwrap();
        let cases = [
            (
                r#"key = "tk1"
                   [[stream]]
                   id = "audio_0"
                   kind = "audio"
                   fps = 30"#,
                "fps",
            ),
            (
                r#"key = "tk2"
                   [[stream]]
                   id = "video_0"
                   kind = "video"
                   [[stream.fault]]
                   kind = "stall"
                   buffer_index = 2"#,
                "sync_point",
            ),
            (
                r#"key = "tk3"
                   [[stream]]
                   id = "video_0"
                   kind = "video"
                   [[stream.fault]]
                   kind = "eos"
                   buffer_index = 2
                   sync_point = "nope""#,
                "sync_point",
            ),
            (
                r#"key = "tk4"
                   [[stream]]
                   id = "text_0"
                   kind = "text"
                   [[stream.cue]]
                   start_ms = 400
                   end_ms = 100
                   text = "backwards""#,
                "ends before it starts",
            ),
            (r#"key = "tk5""#, "at least one [[stream]]"),
            (
                r#"key = "bad-key"
                   [[stream]]
                   id = "audio_0"
                   kind = "audio""#,
                "ASCII alphanumeric",
            ),
            (
                r#"key = "tb1"
                   [buffering]
                   low_percent = 100
                   initial_ms = 50
                   [[stream]]
                   id = "audio_0"
                   kind = "audio""#,
                "outside 0..=99",
            ),
            (
                r#"key = "tb2"
                   [buffering]
                   low_percent = 20
                   [[stream]]
                   id = "audio_0"
                   kind = "audio""#,
                "describes no buffering",
            ),
            (
                r#"key = "tb3"
                   [buffering]
                   low_percent = 20
                   [[buffering.dip]]
                   stream = "audio_0"
                   buffer_index = 2
                   recover_after_ms = 10
                   recover_on = "gate"
                   [[stream]]
                   id = "audio_0"
                   kind = "audio""#,
                "exactly one of recover_after_ms and recover_on",
            ),
            (
                r#"key = "tb4"
                   [buffering]
                   low_percent = 20
                   [[buffering.dip]]
                   stream = "audio_0"
                   buffer_index = 2
                   [[stream]]
                   id = "audio_0"
                   kind = "audio""#,
                "exactly one of recover_after_ms and recover_on",
            ),
            (
                r#"key = "tb5"
                   [buffering]
                   low_percent = 20
                   [[buffering.dip]]
                   stream = "video_0"
                   buffer_index = 2
                   recover_after_ms = 10
                   [[stream]]
                   id = "audio_0"
                   kind = "audio""#,
                "names no [[stream]]",
            ),
        ];
        for (document, expected) in cases {
            let err = err_of(parse_str(document), document);
            assert!(
                err.to_string().contains(expected),
                "the error {err:?} does not mention {expected:?} for:\n{document}"
            );
        }
    }

    #[test]
    fn a_missing_file_names_the_path() {
        gst::init().unwrap();
        let err = err_of(load_file("/nonexistent/scenario.toml"), "no such file");
        assert!(
            err.to_string().contains("/nonexistent/scenario.toml"),
            "{err}"
        );
        assert!(err.to_string().contains("cannot read"), "{err}");
    }
}
