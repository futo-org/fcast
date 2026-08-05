//! ftestvdec and ftestadec driven straight from appsrc, without ftestsrc. Every
//! knob is registered as a scenario, because that is the only channel an
//! autoplugged decoder has.

use std::time::{Duration, Instant};

use fcasttest::{
    caps, dec, registry,
    spec::{DecoderKnobs, MediaSpec, StreamSpec},
};
use gst::prelude::*;

const TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(10);
/// Negative checks only: how long to wait before concluding nothing else comes.
const QUIET: gst::ClockTime = gst::ClockTime::from_mseconds(300);

const VIDEO_STREAM: &str = "video_0";
const AUDIO_STREAM: &str = "audio_0";
const FRAME_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(40);
const PACKET_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(10);
/// 10ms of 48kHz stereo S16LE.
const PACKET_SAMPLES: usize = 480;
const PACKET_BYTES: usize = PACKET_SAMPLES * 4;
/// I420 16x16.
const FRAME_BYTES: usize = 384;

struct Harness {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    dec: gst::Element,
    sink: gst_app::AppSink,
    pushed: u64,
}

impl Harness {
    /// `appsrc -> decoder -> appsink(sync=false)`, already PLAYING. The stream-id
    /// appsrc invents is rewritten to `key`/`suffix` so the decoder resolves its
    /// knobs from the registry.
    fn new(factory: &str, key: &str, suffix: &str, input_caps: &gst::Caps) -> Self {
        fcasttest::register_for_tests();

        let src = gst_app::AppSrc::builder()
            .caps(input_caps)
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let dec = gst::ElementFactory::make(factory)
            .build()
            .unwrap_or_else(|_| panic!("{factory} is registered"));
        let sink = gst_app::AppSink::builder().sync(false).build();

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([src.upcast_ref::<gst::Element>(), &dec, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([src.upcast_ref::<gst::Element>(), &dec, sink.upcast_ref()])
            .unwrap();

        let stream_id = caps::stream_id(key, suffix);
        src.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                let replacement = match &info.data {
                    Some(gst::PadProbeData::Event(event)) => match event.view() {
                        gst::EventView::StreamStart(original) => Some(
                            gst::event::StreamStart::builder(&stream_id)
                                .flags(original.stream_flags())
                                .group_id_if_some(original.group_id())
                                .seqnum(event.seqnum())
                                .build(),
                        ),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    info.data = Some(gst::PadProbeData::Event(replacement));
                }
                gst::PadProbeReturn::Ok
            },
        );

        pipeline.set_state(gst::State::Playing).unwrap();

        Self {
            pipeline,
            src,
            dec,
            sink,
            pushed: 0,
        }
    }

    fn video(key: &str, suffix: &str) -> Self {
        fcasttest::register_for_tests();
        let input_caps = mark_parsed(caps::video_caps_at(
            caps::RAW_VIDEO_WIDTH,
            caps::RAW_VIDEO_HEIGHT,
            gst::Fraction::new(25, 1),
        ));
        Self::new(dec::VIDEO_FACTORY, key, suffix, &input_caps)
    }

    fn audio(key: &str, suffix: &str) -> Self {
        fcasttest::register_for_tests();
        let input_caps = mark_parsed(caps::audio_caps_at(
            caps::RAW_AUDIO_RATE,
            caps::RAW_AUDIO_CHANNELS,
        ));
        Self::new(dec::AUDIO_FACTORY, key, suffix, &input_caps)
    }

    fn push(&mut self, keyframe: bool, duration: gst::ClockTime) {
        let pts = duration * self.pushed;
        let mut buffer = gst::Buffer::with_size(64).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(pts);
            buffer.set_dts(pts);
            buffer.set_duration(duration);
            if !keyframe {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
            buffer
                .map_writable()
                .unwrap()
                .as_mut_slice()
                .fill((self.pushed & 0xff) as u8);
        }
        self.pushed += 1;
        // A rejected push means the pipeline already failed, which the callers
        // assert on through the bus.
        let _ = self.src.push_buffer(buffer);
    }

    fn push_video(&mut self, keyframes: &[bool]) {
        for keyframe in keyframes {
            self.push(*keyframe, FRAME_DURATION);
        }
    }

    fn push_audio(&mut self, keyframes: &[bool]) {
        for keyframe in keyframes {
            self.push(*keyframe, PACKET_DURATION);
        }
    }

    fn pull(&self, what: &str) -> gst::Sample {
        self.sink
            .try_pull_sample(TIMEOUT)
            .unwrap_or_else(|| panic!("timed out waiting for {what}"))
    }

    /// Presentation timestamps of the next `count` samples, in arrival order.
    fn pull_timestamps(&self, count: usize) -> Vec<gst::ClockTime> {
        (0..count)
            .map(|index| {
                let sample = self.pull(&format!("sample {index}"));
                sample
                    .buffer()
                    .and_then(|buffer| buffer.pts())
                    .expect("output buffer carries a pts")
            })
            .collect()
    }

    fn assert_quiet(&self) {
        assert!(
            self.sink.try_pull_sample(QUIET).is_none(),
            "an extra sample arrived"
        );
    }

    fn eos(&self) {
        self.src.end_of_stream().unwrap();
    }

    fn expect_eos(&self) {
        assert!(
            self.sink.try_pull_sample(TIMEOUT).is_none(),
            "a sample arrived instead of eos"
        );
        assert!(self.sink.is_eos(), "the sink never reached eos");
    }

    /// Downstream flush straight into the decoder, plus the segment FLUSH_STOP
    /// takes away.
    fn flush(&self) {
        let sink_pad = self.dec.static_pad("sink").unwrap();
        assert!(sink_pad.send_event(gst::event::FlushStart::new()));
        assert!(sink_pad.send_event(gst::event::FlushStop::builder(false).build()));
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        assert!(sink_pad.send_event(gst::event::Segment::new(&segment)));
    }

    /// First ERROR on the bus, as (source name, message, debug).
    fn expect_error(&self) -> (String, String, String) {
        let message = self
            .pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(TIMEOUT, &[gst::MessageType::Error, gst::MessageType::Eos])
            .expect("an error before the timeout");
        match message.view() {
            gst::MessageView::Error(err) => (
                message
                    .src()
                    .map(|src| src.name().to_string())
                    .unwrap_or_default(),
                err.error().to_string(),
                err.debug()
                    .map(|debug| debug.to_string())
                    .unwrap_or_default(),
            ),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// Latency the decoder adds on top of its (non-live, zero latency) upstream.
    fn latency(&self) -> (bool, gst::ClockTime, Option<gst::ClockTime>) {
        let mut query = gst::query::Latency::new();
        assert!(
            self.dec.static_pad("src").unwrap().query(&mut query),
            "the decoder answers latency queries"
        );
        query.result()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// The convention agreed with ftestparse: the parser may add `parsed`, so the
/// decoder sink templates have to match caps that carry it.
fn mark_parsed(caps: gst::Caps) -> gst::Caps {
    let mut caps = caps;
    caps.get_mut()
        .unwrap()
        .structure_mut(0)
        .unwrap()
        .set("parsed", true);
    caps
}

/// Registers `knobs` under `key` so the decoder for that stream picks them up.
fn register(key: &str, stream: StreamSpec) {
    fcasttest::register_for_tests();
    registry::register_scenario(key, MediaSpec::new(0x5EED).with_stream(stream));
}

fn video_knobs(key: &str, knobs: DecoderKnobs) {
    register(key, StreamSpec::video(VIDEO_STREAM).with_decoder(knobs));
}

fn audio_knobs(key: &str, knobs: DecoderKnobs) {
    register(key, StreamSpec::audio(AUDIO_STREAM).with_decoder(knobs));
}

/// No registered scenario at all: the knobs default to off and the element still
/// decodes, which is what makes it usable outside a scenario.
#[test]
fn video_decodes_every_frame_with_no_scenario() {
    let mut harness = Harness::video("decvplain", VIDEO_STREAM);
    harness.push_video(&[true, false, false, false, false]);
    harness.eos();

    let first = harness.pull("first frame");
    let buffer = first.buffer().unwrap();
    assert_eq!(buffer.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(buffer.duration(), Some(FRAME_DURATION));
    assert!(buffer.size() >= FRAME_BYTES, "size {}", buffer.size());

    let caps = first.caps().expect("negotiated caps").to_owned();
    let structure = caps.structure(0).unwrap();
    assert_eq!(structure.name(), "video/x-raw");
    assert_eq!(structure.get::<String>("format").unwrap(), "I420");
    assert_eq!(
        structure.get::<i32>("width").unwrap(),
        caps::RAW_VIDEO_WIDTH
    );
    assert_eq!(
        structure.get::<i32>("height").unwrap(),
        caps::RAW_VIDEO_HEIGHT
    );
    // The framerate comes from the input caps through the reference input state.
    assert_eq!(
        structure.get::<gst::Fraction>("framerate").unwrap(),
        gst::Fraction::new(25, 1)
    );

    // Pattern fill: plane 0 is the frame index, the chroma planes are neutral.
    let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();
    assert_eq!(frame.plane_data(0).unwrap()[0], 0);
    assert_eq!(frame.plane_data(1).unwrap()[0], 0x80);

    let expected: Vec<gst::ClockTime> = (1..5).map(|index| FRAME_DURATION * index).collect();
    assert_eq!(harness.pull_timestamps(4), expected);
    harness.expect_eos();
}

#[test]
fn video_needs_keyframe_after_flush() {
    let key = "decvkey";
    video_knobs(
        key,
        DecoderKnobs {
            needs_keyframe_after_flush: true,
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::video(key, VIDEO_STREAM);

    // The wait is armed from the start, so the leading delta never decodes.
    harness.push_video(&[false, true, false]);
    assert_eq!(
        harness.pull_timestamps(2),
        vec![FRAME_DURATION, FRAME_DURATION * 2u64]
    );
    harness.assert_quiet();

    // Re-armed by the flush: only the keyframe and what follows it decode.
    harness.flush();
    harness.push_video(&[false, true]);
    assert_eq!(harness.pull_timestamps(1), vec![FRAME_DURATION * 4u64]);
    harness.assert_quiet();

    harness.eos();
    harness.expect_eos();
    registry::unregister(key);
}

#[test]
fn video_reorder_delays_output_and_drains_on_eos() {
    let key = "decvreorder";
    video_knobs(
        key,
        DecoderKnobs {
            reorder_frames: 2,
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::video(key, VIDEO_STREAM);

    harness.push_video(&[true, false, false, false, false]);
    // Five in, two held back, and output stays in input order.
    let expected: Vec<gst::ClockTime> = (0..3).map(|index| FRAME_DURATION * index).collect();
    assert_eq!(harness.pull_timestamps(3), expected);
    harness.assert_quiet();

    harness.eos();
    assert_eq!(
        harness.pull_timestamps(2),
        vec![FRAME_DURATION * 3u64, FRAME_DURATION * 4u64]
    );
    harness.expect_eos();
    registry::unregister(key);
}

/// A flush throws the held frames away instead of pushing them, the same as a
/// real decoder losing its reorder window over a seek.
#[test]
fn video_reorder_drops_held_frames_on_flush() {
    let key = "decvreflush";
    video_knobs(
        key,
        DecoderKnobs {
            reorder_frames: 2,
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::video(key, VIDEO_STREAM);

    harness.push_video(&[true, false, false, false, false]);
    assert_eq!(harness.pull_timestamps(3).len(), 3);
    harness.flush();
    harness.assert_quiet();

    harness.eos();
    harness.expect_eos();
    registry::unregister(key);
}

#[test]
fn video_error_at_frame_stops_exactly_there() {
    let key = "decverr";
    video_knobs(
        key,
        DecoderKnobs {
            error_at_frame: Some(3),
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::video(key, VIDEO_STREAM);

    harness.push_video(&[true, false, false, false, false, false]);
    let expected: Vec<gst::ClockTime> = (0..3).map(|index| FRAME_DURATION * index).collect();
    assert_eq!(harness.pull_timestamps(3), expected);

    let (source, message, debug) = harness.expect_error();
    assert_eq!(source, harness.dec.name(), "the decoder posted the error");
    assert!(message.contains("injected decode error"), "{message}");
    assert!(debug.contains("input frame 3"), "{debug}");
    harness.assert_quiet();
    registry::unregister(key);
}

#[test]
fn video_latency_knob_is_advertised_and_slept() {
    let key = "decvlat";
    let latency = gst::ClockTime::from_mseconds(50);
    video_knobs(
        key,
        DecoderKnobs {
            latency: Some(latency),
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::video(key, VIDEO_STREAM);

    let started = Instant::now();
    harness.push_video(&[true, false, false, false]);
    assert_eq!(harness.pull_timestamps(4).len(), 4);
    let elapsed = started.elapsed();

    // Every frame sleeps the knob, all four sleeps land inside the window.
    assert!(
        elapsed >= Duration::from_millis(190),
        "decoded four frames in {elapsed:?}"
    );

    // Resolved on STREAM_START, so the query only answers after the first frame.
    let (live, min, max) = harness.latency();
    assert!(!live);
    assert_eq!(min, latency);
    assert_eq!(max, Some(latency));

    harness.eos();
    harness.expect_eos();
    registry::unregister(key);
}

/// Jitter only widens the reported maximum, every frame still comes out. The draw
/// itself is seeded, see the prng unit tests.
#[test]
fn video_jitter_keeps_every_frame() {
    let key = "decvjit";
    video_knobs(
        key,
        DecoderKnobs {
            jitter_ms: 5,
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::video(key, VIDEO_STREAM);

    harness.push_video(&[true, false, false, false]);
    assert_eq!(harness.pull_timestamps(4).len(), 4);

    let (_, min, max) = harness.latency();
    assert_eq!(min, gst::ClockTime::ZERO);
    assert_eq!(max, Some(gst::ClockTime::from_mseconds(5)));

    harness.eos();
    harness.expect_eos();
    registry::unregister(key);
}

#[test]
fn audio_decodes_every_packet_with_no_scenario() {
    let mut harness = Harness::audio("decaplain", AUDIO_STREAM);
    harness.push_audio(&[true, true, true, true, true]);
    harness.eos();

    let first = harness.pull("first packet");
    let buffer = first.buffer().unwrap();
    assert_eq!(buffer.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(buffer.size(), PACKET_BYTES);

    let caps = first.caps().expect("negotiated caps").to_owned();
    let structure = caps.structure(0).unwrap();
    assert_eq!(structure.name(), "audio/x-raw");
    assert_eq!(structure.get::<String>("format").unwrap(), "S16LE");
    assert_eq!(structure.get::<String>("layout").unwrap(), "interleaved");
    assert_eq!(
        structure.get::<i32>("channels").unwrap(),
        caps::RAW_AUDIO_CHANNELS
    );
    assert_eq!(structure.get::<i32>("rate").unwrap(), caps::RAW_AUDIO_RATE);

    let expected: Vec<gst::ClockTime> = (1..5).map(|index| PACKET_DURATION * index).collect();
    assert_eq!(harness.pull_timestamps(4), expected);
    harness.expect_eos();
}

#[test]
fn audio_reorder_delays_output_and_drains_on_eos() {
    let key = "decareorder";
    audio_knobs(
        key,
        DecoderKnobs {
            reorder_frames: 2,
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::audio(key, AUDIO_STREAM);

    harness.push_audio(&[true, true, true, true, true]);
    let expected: Vec<gst::ClockTime> = (0..3).map(|index| PACKET_DURATION * index).collect();
    assert_eq!(harness.pull_timestamps(3), expected);
    harness.assert_quiet();

    harness.eos();
    assert_eq!(
        harness.pull_timestamps(2),
        vec![PACKET_DURATION * 3u64, PACKET_DURATION * 4u64]
    );
    harness.expect_eos();
    registry::unregister(key);
}

#[test]
fn audio_needs_keyframe_after_flush() {
    let key = "decakey";
    audio_knobs(
        key,
        DecoderKnobs {
            needs_keyframe_after_flush: true,
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::audio(key, AUDIO_STREAM);

    harness.push_audio(&[false, true, false]);
    assert_eq!(
        harness.pull_timestamps(2),
        vec![PACKET_DURATION, PACKET_DURATION * 2u64]
    );
    harness.assert_quiet();

    harness.flush();
    harness.push_audio(&[false, true]);
    assert_eq!(harness.pull_timestamps(1), vec![PACKET_DURATION * 4u64]);
    harness.assert_quiet();

    harness.eos();
    harness.expect_eos();
    registry::unregister(key);
}

#[test]
fn audio_error_at_frame_stops_exactly_there() {
    let key = "decaerr";
    audio_knobs(
        key,
        DecoderKnobs {
            error_at_frame: Some(2),
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::audio(key, AUDIO_STREAM);

    harness.push_audio(&[true, true, true, true, true]);
    assert_eq!(
        harness.pull_timestamps(2),
        vec![gst::ClockTime::ZERO, PACKET_DURATION]
    );

    let (source, message, debug) = harness.expect_error();
    assert_eq!(source, harness.dec.name(), "the decoder posted the error");
    assert!(message.contains("injected decode error"), "{message}");
    assert!(debug.contains("input frame 2"), "{debug}");
    registry::unregister(key);
}

#[test]
fn audio_latency_knob_is_advertised() {
    let key = "decalat";
    let latency = gst::ClockTime::from_mseconds(30);
    audio_knobs(
        key,
        DecoderKnobs {
            latency: Some(latency),
            ..DecoderKnobs::default()
        },
    );
    let mut harness = Harness::audio(key, AUDIO_STREAM);

    harness.push_audio(&[true, true]);
    assert_eq!(harness.pull_timestamps(2).len(), 2);

    let (live, min, max) = harness.latency();
    assert!(!live);
    assert_eq!(min, latency);
    assert_eq!(max, Some(latency));

    harness.eos();
    harness.expect_eos();
    registry::unregister(key);
}
