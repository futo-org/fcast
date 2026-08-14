//! `ftestvdec` and `ftestadec`: knob-driven decoders.
//!
//! Both subclass the GStreamer decoder base classes, so flush, drain,
//! segment and latency handling stay on the real code paths. decodebin3
//! autoplugs these, which rules out properties, so the knobs are resolved
//! from the scenario registry on every STREAM_START. An unregistered
//! stream-id resolves to all-off defaults.
//!
//! The sink templates carry no `parsed` field, so they match whether or not
//! ftestparse marked the stream parsed. A buffer without
//! [`gst::BufferFlags::DELTA_UNIT`] is a keyframe.

use gst::glib::types::StaticType;

pub const VIDEO_FACTORY: &str = "ftestvdec";
pub const AUDIO_FACTORY: &str = "ftestadec";

pub fn register() -> Result<(), gst::glib::BoolError> {
    gst::Element::register(
        None,
        VIDEO_FACTORY,
        gst::Rank::PRIMARY,
        video::FTestVdec::static_type(),
    )?;
    gst::Element::register(
        None,
        AUDIO_FACTORY,
        gst::Rank::PRIMARY,
        audio::FTestAdec::static_type(),
    )
}

/// Knob state shared by both decoders.
mod knobs {
    use std::time::Duration;

    use crate::{caps, prng::Prng, registry, spec::DecoderKnobs};

    /// What the decode path does with one input frame.
    #[derive(Debug)]
    pub enum Step {
        Decode {
            index: u64,
            delay: Option<Duration>,
        },
        /// Post-flush keyframe wait, the frame never reaches the decode path.
        Drop {
            index: u64,
        },
        Error {
            index: u64,
        },
    }

    #[derive(Debug)]
    pub struct State {
        knobs: DecoderKnobs,
        prng: Prng,
        /// Input frames since the last STREAM_START. Monotonic ACROSS flushes,
        /// so `error_at_frame` indexes the input stream, not a
        /// post-seek run.
        index: u64,
        waiting_for_keyframe: bool,
    }

    impl Default for State {
        fn default() -> Self {
            Self {
                knobs: DecoderKnobs::default(),
                prng: Prng::new(0),
                index: 0,
                waiting_for_keyframe: false,
            }
        }
    }

    impl State {
        /// Rebinds to the stream that just started. Unknown stream-ids resolve
        /// to all-off defaults.
        pub fn start_stream(&mut self, stream_id: &str) {
            self.knobs = registry::decoder_knobs_for_stream_id(stream_id).unwrap_or_default();
            self.prng = stream_prng(stream_id);
            self.index = 0;
            self.waiting_for_keyframe = self.knobs.needs_keyframe_after_flush;
        }

        pub fn flushed(&mut self) {
            self.waiting_for_keyframe = self.knobs.needs_keyframe_after_flush;
        }

        pub fn reorder_frames(&self) -> u32 {
            self.knobs.reorder_frames
        }

        /// Latency to advertise, `None` when there is nothing to declare. The
        /// reorder delay is deliberately NOT folded in.
        pub fn reported_latency(&self) -> Option<(gst::ClockTime, gst::ClockTime)> {
            let min = self.knobs.latency.unwrap_or(gst::ClockTime::ZERO);
            let max = min + gst::ClockTime::from_mseconds(self.knobs.jitter_ms);
            (max > gst::ClockTime::ZERO).then_some((min, max))
        }

        pub fn step(&mut self, keyframe: bool) -> Step {
            let index = self.index;
            self.index += 1;

            if self.knobs.error_at_frame == Some(index) {
                return Step::Error { index };
            }
            if self.waiting_for_keyframe {
                if !keyframe {
                    return Step::Drop { index };
                }
                self.waiting_for_keyframe = false;
            }
            Step::Decode {
                index,
                delay: self.delay(),
            }
        }

        /// Fixed latency plus a seeded jitter draw, never a wall-clock random.
        fn delay(&mut self) -> Option<Duration> {
            let mut nanos = self.knobs.latency.map_or(0, |latency| latency.nseconds());
            if self.knobs.jitter_ms > 0 {
                nanos += self.prng.next_range(0..self.knobs.jitter_ms + 1) * 1_000_000;
            }
            (nanos > 0).then(|| Duration::from_nanos(nanos))
        }
    }

    /// Per-stream PRNG, derived from the scenario seed and the stream position.
    fn stream_prng(stream_id: &str) -> Prng {
        let Some(scenario) = registry::scenario_for_stream_id(stream_id) else {
            return Prng::new(0);
        };
        let index = caps::suffix_from_stream_id(stream_id)
            .and_then(|suffix| scenario.spec().stream_index(suffix))
            .unwrap_or(0);
        Prng::derive(scenario.spec().seed, index)
    }
}

mod video {
    use gst::glib;

    mod imp {
        use std::sync::LazyLock;

        use gst::glib;
        use gst_video::{
            VideoCodecFrame, VideoCodecState, prelude::*, subclass::prelude::*,
            video_codec_state::Readable,
        };
        use parking_lot::Mutex;

        use crate::{
            caps,
            dec::knobs::{self, Step},
        };

        static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
            gst::DebugCategory::new(
                crate::dec::VIDEO_FACTORY,
                gst::DebugColorFlags::empty(),
                Some("fcasttest video decoder"),
            )
        });

        #[derive(Default)]
        struct State {
            knobs: knobs::State,
            info: Option<gst_video::VideoInfo>,
            /// Decoded frames the reorder knob holds back.
            held: u32,
        }

        #[derive(Default)]
        pub struct FTestVdec {
            state: Mutex<State>,
        }

        impl FTestVdec {
            /// Pushes out every frame the reorder knob is holding back.
            fn release_held(&self) -> Result<(), gst::FlowError> {
                let dec = self.obj();
                loop {
                    {
                        let mut state = self.state.lock();
                        if state.held == 0 {
                            break;
                        }
                        state.held -= 1;
                    }
                    let Some(frame) = dec.oldest_frame() else {
                        break;
                    };
                    dec.finish_frame(frame)?;
                }
                Ok(())
            }
        }

        #[glib::object_subclass]
        impl ObjectSubclass for FTestVdec {
            const NAME: &str = "FTestVdec";
            type Type = super::FTestVdec;
            type ParentType = gst_video::VideoDecoder;
        }

        impl ObjectImpl for FTestVdec {}

        impl GstObjectImpl for FTestVdec {}

        impl ElementImpl for FTestVdec {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                    LazyLock::new(|| {
                        gst::subclass::ElementMetadata::new(
                            "fcasttest video decoder",
                            "Codec/Decoder/Video",
                            "Turns video/x-fcasttest into tiny raw frames",
                            "FUTO",
                        )
                    });

                Some(&*ELEMENT_METADATA)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink = gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps::video_caps(),
                    )
                    .unwrap();
                    let src = gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps::raw_video_caps(),
                    )
                    .unwrap();

                    vec![sink, src]
                });

                PAD_TEMPLATES.as_ref()
            }
        }

        impl VideoDecoderImpl for FTestVdec {
            fn start(&self) -> Result<(), gst::ErrorMessage> {
                *self.state.lock() = State::default();
                self.parent_start()
            }

            fn stop(&self) -> Result<(), gst::ErrorMessage> {
                *self.state.lock() = State::default();
                self.parent_stop()
            }

            fn sink_event(&self, event: gst::Event) -> bool {
                if let gst::EventView::StreamStart(stream_start) = event.view() {
                    let stream_id = stream_start.stream_id();
                    let latency = {
                        let mut state = self.state.lock();
                        state.knobs.start_stream(stream_id);
                        state.held = 0;
                        state.knobs.reported_latency()
                    };
                    gst::debug!(CAT, imp = self, "stream {stream_id}, latency {latency:?}");
                    if let Some((min, max)) = latency {
                        self.obj().set_latency(min, max);
                    }
                }

                self.parent_sink_event(event)
            }

            fn set_format(
                &self,
                state: &VideoCodecState<'static, Readable>,
            ) -> Result<(), gst::LoggableError> {
                let dec = self.obj();
                // The input state is the reference, so framerate and aspect ratio
                // carry over when the caps declare them.
                let output = dec
                    .set_output_state(
                        caps::RAW_VIDEO_FORMAT,
                        caps::RAW_VIDEO_WIDTH as u32,
                        caps::RAW_VIDEO_HEIGHT as u32,
                        Some(state),
                    )
                    .map_err(|err| gst::loggable_error!(CAT, "no output state: {err:?}"))?;
                dec.negotiate(output)
                    .map_err(|err| gst::loggable_error!(CAT, "negotiation failed: {err:?}"))?;

                let info = dec
                    .output_state()
                    .map(|output| output.info().clone())
                    .ok_or_else(|| {
                        gst::loggable_error!(CAT, "negotiated without an output state")
                    })?;
                gst::debug!(CAT, imp = self, "output info {info:?}");
                self.state.lock().info = Some(info);

                Ok(())
            }

            fn handle_frame(
                &self,
                mut frame: VideoCodecFrame,
            ) -> Result<gst::FlowSuccess, gst::FlowError> {
                let keyframe = frame
                    .flags()
                    .contains(gst_video::VideoCodecFrameFlags::SYNC_POINT);
                let (step, info, reorder) = {
                    let mut state = self.state.lock();
                    let step = state.knobs.step(keyframe);
                    (step, state.info.clone(), state.knobs.reorder_frames())
                };
                let dec = self.obj();

                let (index, delay) = match step {
                    Step::Error { index } => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Decode,
                            ("injected decode error"),
                            ["at input frame {index}"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                    Step::Drop { index } => {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "dropped frame {index}, still waiting for a keyframe"
                        );
                        dec.release_frame(frame);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    Step::Decode { index, delay } => (index, delay),
                };

                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }

                let info = info.ok_or(gst::FlowError::NotNegotiated)?;
                dec.allocate_output_frame(&mut frame, None)?;
                {
                    let buffer = frame.output_buffer_mut().ok_or(gst::FlowError::Error)?;
                    let mut output =
                        gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &info)
                            .map_err(|_| gst::FlowError::Error)?;
                    fill_pattern(&mut output, index);
                }
                // Unreffing does not finish the frame: it stays in the base class
                // pending list, which is how a reordering decoder holds it.
                drop(frame);

                let release = {
                    let mut state = self.state.lock();
                    state.held += 1;
                    let release = state.held > reorder;
                    if release {
                        state.held -= 1;
                    }
                    release
                };
                if !release {
                    return Ok(gst::FlowSuccess::Ok);
                }

                let oldest = dec.oldest_frame().ok_or(gst::FlowError::Error)?;
                dec.finish_frame(oldest)
            }

            fn finish(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
                self.release_held()?;
                self.parent_finish()
            }

            fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
                self.release_held()?;
                self.parent_drain()
            }

            fn flush(&self) -> bool {
                // The base class already released the pending frames.
                let mut state = self.state.lock();
                state.held = 0;
                state.knobs.flushed();
                true
            }
        }

        /// Flat per-plane fill keyed on the frame index, enough for sinks to
        /// consume and for tests to tell frames apart.
        fn fill_pattern(frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>, index: u64) {
            for plane in 0..frame.n_planes() {
                let value = if plane == 0 {
                    (index & 0xff) as u8
                } else {
                    0x80
                };
                if let Ok(data) = frame.plane_data_mut(plane) {
                    data.fill(value);
                }
            }
        }
    }

    glib::wrapper! {
        pub struct FTestVdec(ObjectSubclass<imp::FTestVdec>)
            @extends gst_video::VideoDecoder, gst::Element, gst::Object;
    }
}

mod audio {
    use gst::glib;

    mod imp {
        use std::{collections::VecDeque, sync::LazyLock};

        use gst::glib;
        use gst_audio::{prelude::*, subclass::prelude::*};
        use parking_lot::Mutex;

        use crate::{
            caps,
            dec::knobs::{self, Step},
        };

        /// Samples per output packet when the input buffer has no duration.
        const DEFAULT_SAMPLES: usize = 1024;

        static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
            gst::DebugCategory::new(
                crate::dec::AUDIO_FACTORY,
                gst::DebugColorFlags::empty(),
                Some("fcasttest audio decoder"),
            )
        });

        #[derive(Default)]
        struct State {
            knobs: knobs::State,
            info: Option<gst_audio::AudioInfo>,
            /// Decoded packets the reorder knob holds back.
            held: VecDeque<gst::Buffer>,
        }

        #[derive(Default)]
        pub struct FTestAdec {
            state: Mutex<State>,
        }

        impl FTestAdec {
            /// Pushes out every packet the reorder knob is holding back. One
            /// input frame is consumed per packet, keeping the base
            /// class timestamp bookkeeping aligned.
            fn release_held(&self) -> Result<(), gst::FlowError> {
                let dec = self.obj();
                loop {
                    let Some(buffer) = self.state.lock().held.pop_front() else {
                        break;
                    };
                    dec.finish_frame(Some(buffer), 1)?;
                }
                Ok(())
            }
        }

        #[glib::object_subclass]
        impl ObjectSubclass for FTestAdec {
            const NAME: &str = "FTestAdec";
            type Type = super::FTestAdec;
            type ParentType = gst_audio::AudioDecoder;
        }

        impl ObjectImpl for FTestAdec {}

        impl GstObjectImpl for FTestAdec {}

        impl ElementImpl for FTestAdec {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                    LazyLock::new(|| {
                        gst::subclass::ElementMetadata::new(
                            "fcasttest audio decoder",
                            "Codec/Decoder/Audio",
                            "Turns audio/x-fcasttest into tiny raw packets",
                            "FUTO",
                        )
                    });

                Some(&*ELEMENT_METADATA)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink = gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps::audio_caps(),
                    )
                    .unwrap();
                    let src = gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps::raw_audio_caps(),
                    )
                    .unwrap();

                    vec![sink, src]
                });

                PAD_TEMPLATES.as_ref()
            }
        }

        impl AudioDecoderImpl for FTestAdec {
            fn start(&self) -> Result<(), gst::ErrorMessage> {
                *self.state.lock() = State::default();
                self.parent_start()
            }

            fn stop(&self) -> Result<(), gst::ErrorMessage> {
                *self.state.lock() = State::default();
                self.parent_stop()
            }

            fn sink_event(&self, event: gst::Event) -> bool {
                if let gst::EventView::StreamStart(stream_start) = event.view() {
                    let stream_id = stream_start.stream_id();
                    let latency = {
                        let mut state = self.state.lock();
                        state.knobs.start_stream(stream_id);
                        state.held.clear();
                        state.knobs.reported_latency()
                    };
                    gst::debug!(CAT, imp = self, "stream {stream_id}, latency {latency:?}");
                    if let Some((min, max)) = latency {
                        self.obj().set_latency(min, max);
                    }
                }

                self.parent_sink_event(event)
            }

            fn set_format(&self, input_caps: &gst::Caps) -> Result<(), gst::LoggableError> {
                // Raw output is always stereo (the src template fixes it).
                // Only the rate follows the input caps.
                let rate = input_caps
                    .structure(0)
                    .and_then(|structure| structure.get::<i32>("rate").ok())
                    .unwrap_or(caps::RAW_AUDIO_RATE);
                let info = gst_audio::AudioInfo::from_caps(&caps::raw_audio_caps_at(rate))
                    .map_err(|err| gst::loggable_error!(CAT, "invalid output caps: {err}"))?;
                self.obj()
                    .set_output_format(&info)
                    .map_err(|err| gst::loggable_error!(CAT, "output format refused: {err:?}"))?;
                gst::debug!(CAT, imp = self, "output info {info:?}");
                self.state.lock().info = Some(info);

                Ok(())
            }

            fn handle_frame(
                &self,
                buffer: Option<&gst::Buffer>,
            ) -> Result<gst::FlowSuccess, gst::FlowError> {
                let Some(buffer) = buffer else {
                    self.release_held()?;
                    return Ok(gst::FlowSuccess::Ok);
                };

                let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                let (step, info, reorder) = {
                    let mut state = self.state.lock();
                    let step = state.knobs.step(keyframe);
                    (step, state.info.clone(), state.knobs.reorder_frames())
                };
                let dec = self.obj();

                let (index, delay) = match step {
                    Step::Error { index } => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Decode,
                            ("injected decode error"),
                            ["at input frame {index}"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                    Step::Drop { index } => {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "dropped frame {index}, still waiting for a keyframe"
                        );
                        // Consume the input frame without output, or the base class
                        // would timestamp later packets from it.
                        return dec.finish_frame(None, 1);
                    }
                    Step::Decode { index, delay } => (index, delay),
                };

                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }

                let info = info.ok_or(gst::FlowError::NotNegotiated)?;
                let samples = samples_for(buffer, info.rate());
                let mut packet = dec.allocate_output_buffer(samples * info.bpf() as usize);
                {
                    let packet = packet.get_mut().ok_or(gst::FlowError::Error)?;
                    let mut map = packet.map_writable().map_err(|_| gst::FlowError::Error)?;
                    fill_pattern(map.as_mut_slice(), index);
                }

                let release = {
                    let mut state = self.state.lock();
                    state.held.push_back(packet);
                    if state.held.len() > reorder as usize {
                        state.held.pop_front()
                    } else {
                        None
                    }
                };
                match release {
                    Some(packet) => dec.finish_frame(Some(packet), 1),
                    None => Ok(gst::FlowSuccess::Ok),
                }
            }

            fn flush(&self, hard: bool) {
                let mut state = self.state.lock();
                state.held.clear();
                if hard {
                    state.knobs.flushed();
                }
                drop(state);

                self.parent_flush(hard)
            }
        }

        /// One output packet per input frame. The input duration decides its
        /// length, so output timestamps stay contiguous.
        fn samples_for(buffer: &gst::Buffer, rate: u32) -> usize {
            buffer
                .duration()
                .map(|duration| {
                    let nanos = u128::from(duration.nseconds()) * u128::from(rate);
                    (nanos / u128::from(gst::ClockTime::SECOND.nseconds())) as usize
                })
                .unwrap_or(DEFAULT_SAMPLES)
                .max(1)
        }

        /// Deterministic S16LE ramp keyed on the frame index.
        fn fill_pattern(data: &mut [u8], index: u64) {
            let base = (index & 0xff) as i16;
            for (sample, chunk) in data.chunks_mut(2).enumerate() {
                let value = base
                    .wrapping_add(sample as i16)
                    .wrapping_mul(64)
                    .to_le_bytes();
                let len = chunk.len();
                chunk.copy_from_slice(&value[..len]);
            }
        }
    }

    glib::wrapper! {
        pub struct FTestAdec(ObjectSubclass<imp::FTestAdec>)
            @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
    }
}
