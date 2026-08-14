use gst::{glib, prelude::*};

mod imp {
    use std::sync::{LazyLock, Mutex};

    use gst::{glib, subclass::prelude::*};
    use gst_base::{
        prelude::*,
        subclass::{base_transform::GenerateOutputSuccess, prelude::*},
    };

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fcastaudiostretch",
            gst::DebugColorFlags::empty(),
            Some("Pitch-preserving time stretcher (PICOLA)"),
        )
    });

    /// Pitch range the period search covers. Unvoiced or silent stretches have
    /// no true period, but the minimum the search returns splices fine because
    /// noise has no phase to misalign.
    const MIN_PITCH_HZ: usize = 65;
    const MAX_PITCH_HZ: usize = 400;
    /// The AMDF coarse pass runs on a mixdown decimated to roughly this rate.
    /// The winner is then refined at full resolution.
    const AMDF_FREQ: usize = 4000;
    /// Scales this close to 1.0 bypass the stretcher entirely.
    const BYPASS_EPSILON: f64 = 0.02;
    /// Frames per verbatim-copy round.
    const COPY_CHUNK: usize = 1024;

    /// Rate clamp: outside this the algorithm stops being sane.
    const RATE_MIN: f64 = 0.25;
    const RATE_MAX: f64 = 4.0;

    /// Fixed-capacity input buffer, period search, splice schedule. Built once
    /// the format is known and rebuilt if it changes. `generate` never
    /// allocates.
    struct Engine {
        channels: usize,
        min_period: usize,
        max_period: usize,
        amdf_skip: usize,
        /// Frames a splice round must have buffered (`3 * max_period`).
        window: usize,
        /// Buffered input, interleaved f32. `start` is the read head, in
        /// SAMPLES.
        buf: Vec<f32>,
        start: usize,
        /// Frames still to copy verbatim before the next splice.
        copy_remaining: usize,
        /// Last measured period and its AMDF score. Reused through silence and
        /// preferred over a new measurement that matches worse, because
        /// unvoiced frames have no true period and their random minima jitter
        /// the splice cadence.
        prev_period: usize,
        prev_min_diff: f32,
        /// Scratch: mono mixdown of the search window, and its decimated copy.
        mono: Vec<f32>,
        mono_coarse: Vec<f32>,
        /// Fractional frames of skip/insert owed, carried between splices so
        /// the long-run tempo is exact.
        frac: f64,
        /// One round's output, interleaved f32.
        out: Vec<f32>,
    }

    impl Engine {
        fn new(rate_hz: usize, channels: usize) -> Self {
            let max_period = (rate_hz / MIN_PITCH_HZ).max(2);
            let amdf_skip = (rate_hz / AMDF_FREQ).max(1);
            let window = 3 * max_period;
            // A full search window plus a feed chunk, so a push always makes progress
            // after a drain without reallocating.
            let buf_frames = window + 2 * COPY_CHUNK;
            // Worst-case round output: an EOS tail passes through a whole window.
            let out_frames = window.max(2 * max_period).max(COPY_CHUNK);
            Self {
                channels,
                min_period: (rate_hz / MAX_PITCH_HZ).max(1),
                max_period,
                amdf_skip,
                window,
                buf: Vec::with_capacity(buf_frames * channels),
                start: 0,
                copy_remaining: 0,
                prev_period: 0,
                prev_min_diff: 0.0,
                mono: Vec::with_capacity(2 * max_period),
                mono_coarse: Vec::with_capacity(2 * max_period / amdf_skip + 2),
                frac: 0.0,
                out: Vec::with_capacity(out_frames * channels),
            }
        }

        /// Drop every trace of the audio seen so far (flush or format change).
        fn reset(&mut self) {
            self.buf.clear();
            self.start = 0;
            self.copy_remaining = 0;
            self.prev_period = 0;
            self.prev_min_diff = 0.0;
            self.frac = 0.0;
            self.out.clear();
        }

        fn buffered_frames(&self) -> usize {
            (self.buf.len() - self.start) / self.channels
        }

        fn free_frames(&self) -> usize {
            (self.buf.capacity().saturating_sub(self.buf.len())) / self.channels
        }

        /// Slide the unread remainder to the front, reclaiming the consumed
        /// prefix.
        fn compact(&mut self) {
            if self.start == 0 {
                return;
            }
            let keep = self.buf.len() - self.start;
            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(keep);
            self.start = 0;
        }

        /// Append interleaved f32 frames. The caller must respect
        /// [`Engine::free_frames`].
        fn push(&mut self, samples: &[f32]) {
            self.buf.extend_from_slice(samples);
        }

        /// AMDF pitch period of the input at the read head, the lag in
        /// `[min_period, max_period]` minimising the mean `|s[i] - s[i+lag]|`.
        /// Coarse pass on a decimated mixdown, refined at full
        /// resolution. (Ross et al., IEEE Trans. ASSP 22(5), 1974.)
        fn find_pitch_period(&mut self) -> usize {
            let ch = self.channels;
            let input = &self.buf[self.start..];
            self.mono.clear();
            self.mono.extend(
                input
                    .chunks_exact(ch)
                    .take(2 * self.max_period)
                    .map(|f| f.iter().sum::<f32>()),
            );

            // Silence carries no period. Keep the last one so pauses splice at the
            // same cadence.
            let energy: f32 = self.mono.iter().map(|s| s * s).sum();
            if energy < 1e-6 && self.prev_period != 0 {
                return self.prev_period;
            }

            let skip = self.amdf_skip;
            self.mono_coarse.clear();
            self.mono_coarse.extend(self.mono.iter().step_by(skip));

            fn amdf(signal: &[f32], lag: usize) -> f32 {
                let n = lag.min(signal.len().saturating_sub(lag));
                if n == 0 {
                    return f32::MAX;
                }
                let diff: f32 = signal[..n]
                    .iter()
                    .zip(&signal[lag..lag + n])
                    .map(|(a, b)| (a - b).abs())
                    .sum();
                diff / n as f32
            }

            let (lo, hi) = (self.min_period / skip, self.max_period / skip);
            let mut best = lo.max(1);
            let mut best_diff = f32::MAX;
            for lag in lo.max(1)..=hi.max(lo.max(1)) {
                let d = amdf(&self.mono_coarse, lag);
                if d < best_diff {
                    best_diff = d;
                    best = lag;
                }
            }

            // Refine around the coarse winner at full resolution.
            let center = best * skip;
            let lo = center.saturating_sub(2 * skip).max(self.min_period);
            let hi = (center + 2 * skip).min(self.max_period);
            let mut best = lo;
            let mut best_diff = f32::MAX;
            for lag in lo..=hi.max(lo) {
                let d = amdf(&self.mono, lag);
                if d < best_diff {
                    best_diff = d;
                    best = lag;
                }
            }

            // A match fitting worse than the previous frame's is likely unvoiced.
            // Keep the established period instead.
            let chosen = if self.prev_period != 0 && best_diff > self.prev_min_diff {
                self.prev_period
            } else {
                best
            };
            self.prev_period = best;
            // A non-finite score would make every later comparison false and freeze
            // the hysteresis.
            self.prev_min_diff = if best_diff.is_finite() {
                best_diff
            } else {
                f32::MAX
            };
            chosen.clamp(1, self.max_period)
        }

        /// Emit `frames` frames crossfading `from` (out) into `to` (in), both
        /// frame offsets relative to the read head. Linear rather than
        /// equal-power because the two sides are phase-aligned and correlated
        /// signals sum linearly.
        fn overlap_add(&mut self, frames: usize, from: usize, to: usize) {
            let ch = self.channels;
            let base = self.start;
            // Unreachable with the current schedule; here so a future one degrades
            // into a short crossfade rather than a panic.
            let avail = self.buffered_frames().saturating_sub(from.max(to));
            let frames = frames.min(avail);
            for i in 0..frames {
                let t = i as f32 / frames as f32;
                for c in 0..ch {
                    let a = self.buf[base + (from + i) * ch + c];
                    let b = self.buf[base + (to + i) * ch + c];
                    self.out.push(a * (1.0 - t) + b * t);
                }
            }
        }

        /// Copy frames verbatim from the read head and consume them.
        fn pass_through(&mut self, frames: usize) {
            let n = frames * self.channels;
            let s = self.start;
            self.out.extend_from_slice(&self.buf[s..s + n]);
            self.consume(frames);
        }

        fn consume(&mut self, frames: usize) {
            self.start += frames * self.channels;
        }

        /// Runs one round, a verbatim run or one pitch-synchronous splice,
        /// appended to `out`. False when the round needs more input. `eos`
        /// unlocks the tail branch.
        fn generate(&mut self, scale: f64, eos: bool) -> bool {
            // A NaN here would turn every later `ideal`/`copy` into NaN and pin the
            // splice length at its clamp forever.
            if !self.frac.is_finite() {
                self.frac = 0.0;
            }
            let buffered = self.buffered_frames();

            // Near-unity scale is a verbatim forward. Also guards the slow-down
            // schedule below, whose `1.0 - scale` denominator is zero at exactly 1.0.
            if (scale - 1.0).abs() < BYPASS_EPSILON {
                if buffered == 0 {
                    return false;
                }
                let frames = buffered.min(COPY_CHUNK);
                self.pass_through(frames);
                self.copy_remaining = 0;
                self.frac = 0.0;
                return true;
            }

            // The run between splices is a plain copy.
            if self.copy_remaining > 0 {
                let want = self.copy_remaining.min(COPY_CHUNK);
                if buffered < want && !eos {
                    return false;
                }
                let frames = buffered.min(self.copy_remaining).min(COPY_CHUNK);
                if frames == 0 {
                    self.copy_remaining = 0;
                    return true;
                }
                self.pass_through(frames);
                self.copy_remaining -= frames;
                return true;
            }

            // A splice needs a full search window plus the period being blended.
            if buffered < self.window {
                if !eos || buffered == 0 {
                    return false;
                }
                // Tail of the stream: pass the remainder through, tempo drifting for
                // the final few tens of ms.
                self.pass_through(buffered);
                return true;
            }

            let period = self.find_pitch_period();

            if scale > 1.0 {
                // Speed up: blend one period into the next, dropping a period. The
                // blend shrinks as the rate grows (Sonic's schedule).
                if scale >= 2.0 {
                    let ideal = period as f64 / (scale - 1.0) + self.frac;
                    let new_frames = (ideal as usize).clamp(1, period);
                    self.frac = ideal - new_frames as f64;
                    self.overlap_add(new_frames, 0, period);
                    self.consume(period + new_frames);
                } else {
                    let copy = period as f64 * (2.0 - scale) / (scale - 1.0) + self.frac;
                    self.copy_remaining = copy as usize;
                    self.frac = copy - self.copy_remaining as f64;
                    self.overlap_add(period, 0, period);
                    self.consume(2 * period);
                }
            } else {
                // Slow down: emit one period verbatim, then blend back to repeat it,
                // inserting output the input does not advance through.
                let ch = self.channels;
                if scale < 0.5 {
                    let ideal = period as f64 * scale / (1.0 - scale) + self.frac;
                    let new_frames = (ideal as usize).clamp(1, period);
                    self.frac = ideal - new_frames as f64;
                    let s = self.start;
                    self.out.extend_from_slice(&self.buf[s..s + period * ch]);
                    self.overlap_add(new_frames, period, 0);
                    self.consume(new_frames);
                } else {
                    let copy = period as f64 * (2.0 * scale - 1.0) / (1.0 - scale) + self.frac;
                    self.copy_remaining = copy as usize;
                    self.frac = copy - self.copy_remaining as f64;
                    let s = self.start;
                    self.out.extend_from_slice(&self.buf[s..s + period * ch]);
                    self.overlap_add(period, period, 0);
                    self.consume(period);
                }
            }
            true
        }
    }

    struct State {
        info: gst_audio::AudioInfo,
        engine: Engine,
        /// `|segment.rate|` taken from the last SEGMENT, 1.0 when bypassing.
        scale: f64,
        /// Whether the last SEGMENT asked for reverse playback.
        reverse: bool,
        /// The segment as it arrived, before we rewrote it.
        in_segment: gst::FormattedSegment<gst::ClockTime>,
        /// Output timeline anchor, set from the first buffer after a segment or
        /// flush.
        base_pts: Option<gst::ClockTime>,
        /// Frames emitted since `base_pts` was anchored.
        emitted: u64,
    }

    pub struct AudioStretch {
        state: Mutex<Option<State>>,
        /// Scale taken from the segment, kept outside `state` so a SEGMENT
        /// arriving before caps is not lost.
        pending: Mutex<(f64, bool)>,
    }

    impl Default for AudioStretch {
        fn default() -> Self {
            // Explicit rather than derived. `f64::default()` is 0.0, which takes the
            // slow-down branch with a zero numerator and stretches one input frame into
            // a whole period indefinitely. `set_caps` really does read this value.
            Self {
                state: Mutex::new(None),
                pending: Mutex::new((1.0, false)),
            }
        }
    }

    impl AudioStretch {
        /// Run rounds until the engine stalls, returning the produced buffer if
        /// any.
        ///
        /// `engine.out` is deliberately NOT cleared on entry.
        /// `submit_input_buffer` also runs rounds, and clearing here
        /// would discard that output. It is cleared only after being
        /// turned into a buffer.
        fn drain(&self, state: &mut State, eos: bool) -> Option<gst::Buffer> {
            let limit = state.engine.out.capacity();
            while state.engine.generate(state.scale, eos) {
                if state.engine.out.len() >= limit {
                    break;
                }
            }
            state.engine.compact();

            if state.engine.out.is_empty() {
                return None;
            }

            let info = &state.info;
            let ch = info.channels() as usize;
            let frames = (state.engine.out.len() / ch) as u64;
            let bpf = info.bpf() as usize;

            let mut buffer = gst::Buffer::with_size(frames as usize * bpf).ok()?;
            {
                let bufref = buffer.get_mut()?;
                {
                    let mut map = bufref.map_writable().ok()?;
                    write_samples(info.format(), &state.engine.out, map.as_mut_slice());
                }

                let rate = info.rate() as u64;
                let base = state.base_pts.unwrap_or(gst::ClockTime::ZERO);
                let pts = base
                    + gst::ClockTime::from_nseconds(
                        state
                            .emitted
                            .saturating_mul(gst::ClockTime::SECOND.nseconds())
                            / rate,
                    );
                let end = base
                    + gst::ClockTime::from_nseconds(
                        (state.emitted + frames).saturating_mul(gst::ClockTime::SECOND.nseconds())
                            / rate,
                    );
                bufref.set_pts(pts);
                bufref.set_duration(end - pts);
            }
            state.emitted += frames;
            state.engine.out.clear();
            Some(buffer)
        }
    }

    /// Convert interleaved f32 into the negotiated sample format.
    fn write_samples(format: gst_audio::AudioFormat, src: &[f32], dst: &mut [u8]) {
        match format {
            gst_audio::AUDIO_FORMAT_S16 => {
                for (s, d) in src.iter().zip(dst.chunks_exact_mut(2)) {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    d.copy_from_slice(&v.to_ne_bytes());
                }
            }
            _ => {
                for (s, d) in src.iter().zip(dst.chunks_exact_mut(4)) {
                    d.copy_from_slice(&s.to_ne_bytes());
                }
            }
        }
    }

    /// Convert the negotiated sample format into interleaved f32.
    fn read_samples(format: gst_audio::AudioFormat, src: &[u8], dst: &mut Vec<f32>) {
        match format {
            gst_audio::AUDIO_FORMAT_S16 => {
                for c in src.chunks_exact(2) {
                    let v = i16::from_ne_bytes([c[0], c[1]]);
                    dst.push(v as f32 / 32768.0);
                }
            }
            _ => {
                for c in src.chunks_exact(4) {
                    dst.push(f32::from_ne_bytes([c[0], c[1], c[2], c[3]]));
                }
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AudioStretch {
        const NAME: &'static str = "FCastAudioStretch";
        type Type = super::AudioStretch;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for AudioStretch {}
    impl GstObjectImpl for AudioStretch {}

    impl ElementImpl for AudioStretch {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "FCast Audio Stretch",
                    "Filter/Effect/Audio",
                    "Pitch-preserving playback rate change (PICOLA)",
                    "Marcus Hanestad <marlhan@proton.me>",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst_audio::AudioCapsBuilder::new_interleaved()
                    .format_list([gst_audio::AUDIO_FORMAT_F32, gst_audio::AUDIO_FORMAT_S16])
                    .build();
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                ]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for AudioStretch {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::NeverInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn transform_caps(
            &self,
            _direction: gst::PadDirection,
            caps: &gst::Caps,
            filter: Option<&gst::Caps>,
        ) -> Option<gst::Caps> {
            // The format never changes, only the number of frames.
            let out = caps.clone();
            Some(match filter {
                Some(f) => f.intersect_with_mode(&out, gst::CapsIntersectMode::First),
                None => out,
            })
        }

        fn set_caps(
            &self,
            incaps: &gst::Caps,
            _outcaps: &gst::Caps,
        ) -> Result<(), gst::LoggableError> {
            let info = gst_audio::AudioInfo::from_caps(incaps)
                .map_err(|_| gst::loggable_error!(CAT, "invalid audio caps"))?;
            let (scale, reverse) = *self.pending.lock().unwrap();
            let engine = Engine::new(info.rate() as usize, info.channels() as usize);
            gst::debug!(
                CAT,
                imp = self,
                "configured {} Hz, {} ch, scale {scale}",
                info.rate(),
                info.channels()
            );
            *self.state.lock().unwrap() = Some(State {
                info,
                engine,
                scale,
                reverse,
                in_segment: gst::FormattedSegment::new(),
                base_pts: None,
                emitted: 0,
            });
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            *self.state.lock().unwrap() = None;
            Ok(())
        }

        fn sink_event(&self, event: gst::Event) -> bool {
            use gst::EventView;

            match event.view() {
                EventView::Segment(e) => {
                    let segment = e.segment();
                    let Some(seg) = segment.downcast_ref::<gst::ClockTime>() else {
                        // Non-time segments cannot be stretched; pass through untouched.
                        *self.pending.lock().unwrap() = (1.0, false);
                        if let Some(s) = self.state.lock().unwrap().as_mut() {
                            s.scale = 1.0;
                            s.reverse = false;
                        }
                        self.obj().set_passthrough(true);
                        return self.parent_sink_event(event);
                    };

                    let rate = seg.rate();
                    let scale = rate.abs();
                    let reverse = rate < 0.0;
                    let bypass = (scale - 1.0).abs() < BYPASS_EPSILON && !reverse;
                    let scale = if bypass {
                        1.0
                    } else {
                        scale.clamp(RATE_MIN, RATE_MAX)
                    };

                    *self.pending.lock().unwrap() = (scale, reverse);

                    // A SEGMENT can arrive without a preceding flush, so resetting
                    // unconditionally would swallow the tail of the outgoing stream.
                    // Drain first. After a flushing seek FLUSH_STOP already emptied
                    // the engine, so this is a no-op.
                    let tail = {
                        let mut guard = self.state.lock().unwrap();
                        guard.as_mut().and_then(|s| {
                            if s.scale == 1.0 {
                                None
                            } else {
                                self.drain(s, true)
                            }
                        })
                    };
                    if let Some(tail) = tail {
                        let _ = self.obj().src_pad().push(tail);
                    }

                    self.obj().set_passthrough(bypass);

                    if let Some(s) = self.state.lock().unwrap().as_mut() {
                        s.scale = scale;
                        s.reverse = reverse;
                        s.in_segment = seg.clone();
                        s.engine.reset();
                        s.base_pts = None;
                        s.emitted = 0;
                    }

                    if bypass {
                        return self.parent_sink_event(event);
                    }

                    // Consume the rate. Downstream plays at 1.0 over compressed audio, and
                    // `stop` shrinks by the same factor so the segment still describes what
                    // is emitted. Mirrors gst_scaletempo_sink_event.
                    let mut out = seg.clone();
                    out.set_applied_rate(rate);
                    out.set_rate(1.0);
                    if let Some(stop) = out.stop() {
                        let start = out.start().unwrap_or(gst::ClockTime::ZERO);
                        let span = stop.saturating_sub(start).nseconds() as f64 / scale;
                        out.set_stop(start + gst::ClockTime::from_nseconds(span as u64));
                    }

                    let new_event = gst::event::Segment::builder(&out)
                        .seqnum(event.seqnum())
                        .running_time_offset(event.running_time_offset())
                        .build();
                    self.obj().src_pad().push_event(new_event)
                }
                EventView::FlushStop(_) => {
                    if let Some(s) = self.state.lock().unwrap().as_mut() {
                        s.engine.reset();
                        s.base_pts = None;
                        s.emitted = 0;
                    }
                    self.parent_sink_event(event)
                }
                EventView::Eos(_) => {
                    // Drain what the engine still holds before letting EOS through, or the
                    // tail of the stream is lost.
                    let buffer = {
                        let mut guard = self.state.lock().unwrap();
                        guard.as_mut().and_then(|s| {
                            if s.scale == 1.0 {
                                None
                            } else {
                                self.drain(s, true)
                            }
                        })
                    };
                    if let Some(buffer) = buffer {
                        let _ = self.obj().src_pad().push(buffer);
                    }
                    self.parent_sink_event(event)
                }
                _ => self.parent_sink_event(event),
            }
        }

        fn submit_input_buffer(
            &self,
            is_discont: bool,
            inbuf: gst::Buffer,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let mut guard = self.state.lock().unwrap();
            let Some(state) = guard.as_mut() else {
                return Err(gst::FlowError::NotNegotiated);
            };

            // `is_discont` must NOT reset the engine. GstBaseTransform clears the
            // discont flag only once an output buffer is pushed, and this element
            // needs a whole search window buffered before it can emit anything, so
            // resetting here livelocks. Discontinuities that do require dropping
            // audio arrive as FLUSH_STOP and SEGMENT, handled in `sink_event`.
            let _ = is_discont;

            // Anchor the output timeline on the first buffer of a segment, mapping its
            // stream time into the compressed timeline as scaletempo does.
            if state.base_pts.is_none() {
                let start = state.in_segment.start().unwrap_or(gst::ClockTime::ZERO);
                let pts = inbuf.pts().unwrap_or(start);
                let offset = pts.saturating_sub(start).nseconds() as f64 / state.scale;
                state.base_pts = Some(start + gst::ClockTime::from_nseconds(offset as u64));
            }

            let map = inbuf.map_readable().map_err(|_| gst::FlowError::Error)?;
            let format = state.info.format();
            let ch = state.info.channels() as usize;

            // Feed in chunks the engine has room for, running rounds in between so a
            // large input buffer cannot overrun the fixed-capacity queue.
            let bps = if format == gst_audio::AUDIO_FORMAT_S16 {
                2
            } else {
                4
            };
            let frame_bytes = bps * ch;
            let mut offset = 0usize;
            let data = map.as_slice();

            while offset < data.len() {
                state.engine.compact();
                let room = state.engine.free_frames();
                if room == 0 {
                    if !state.engine.generate(state.scale, false) {
                        break;
                    }
                    continue;
                }
                let take = room.min((data.len() - offset) / frame_bytes.max(1));
                if take == 0 {
                    break;
                }
                let bytes = take * frame_bytes;
                let mut scratch = Vec::with_capacity(take * ch);
                read_samples(format, &data[offset..offset + bytes], &mut scratch);
                state.engine.push(&scratch);
                offset += bytes;
            }

            Ok(gst::FlowSuccess::Ok)
        }

        fn generate_output(&self) -> Result<GenerateOutputSuccess, gst::FlowError> {
            let mut guard = self.state.lock().unwrap();
            let Some(state) = guard.as_mut() else {
                gst::debug!(CAT, imp = self, "generate_output: no state");
                return Ok(GenerateOutputSuccess::NoOutput);
            };
            let buffered = state.engine.buffered_frames();
            let scale = state.scale;
            let window = state.engine.window;
            match self.drain(state, false) {
                Some(buffer) => {
                    gst::debug!(CAT, imp = self, "generate_output: {} bytes", buffer.size());
                    Ok(GenerateOutputSuccess::Buffer(buffer))
                }
                None => {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "generate_output: nothing (scale {scale}, buffered {buffered}, window {window})"
                    );
                    Ok(GenerateOutputSuccess::NoOutput)
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct AudioStretch(ObjectSubclass<imp::AudioStretch>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcastaudiostretch",
        gst::Rank::NONE,
        AudioStretch::static_type(),
    )
}
