//! ftestsrc's schedule contract: buffer timing and keyframe flags, sparse text
//! gaps, stall faults parked on a sync point, and flushing zero-seeks that
//! restart a stream from the beginning.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcasttest::{
    caps, registry,
    spec::{CueSpec, Fault, FlowStopReason, MediaSpec, StreamSpec},
};
use gst::prelude::*;

const TIMEOUT: Duration = Duration::from_secs(15);
/// 400 ms at the default 25 fps.
const TEN_FRAMES: gst::ClockTime = gst::ClockTime::from_mseconds(400);
const FRAME: gst::ClockTime = gst::ClockTime::from_mseconds(40);

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(fcasttest::register_for_tests);
}

#[derive(Debug)]
struct BufferInfo {
    pts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
    offset: u64,
    delta_unit: bool,
}

#[derive(Debug, Default)]
struct PadLog {
    stream_id: Option<String>,
    buffers: Vec<BufferInfo>,
    gaps: Vec<(gst::ClockTime, Option<gst::ClockTime>)>,
    segments: usize,
    flush_starts: usize,
    flush_stops: usize,
    eos: bool,
}

impl PadLog {
    fn pts(&self) -> Vec<Option<gst::ClockTime>> {
        self.buffers.iter().map(|buffer| buffer.pts).collect()
    }
}

type Logs = Arc<Mutex<HashMap<String, PadLog>>>;

struct Harness {
    pipeline: gst::Pipeline,
    src: gst::Element,
    bus: gst::Bus,
    logs: Logs,
    key: String,
}

impl Harness {
    /// ftestsrc with a recording fakesink per pad. Pads are linked from the
    /// pad-added handler, before ftestsrc starts its streaming tasks.
    fn new(key: &str, spec: MediaSpec) -> (Self, Arc<registry::Scenario>) {
        init();
        let scenario = registry::register_scenario(key, spec);

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("ftestsrc")
            .property("uri", caps::uri_for_key(key))
            .build()
            .expect("ftestsrc");
        pipeline.add(&src).expect("adding ftestsrc");

        let logs: Logs = Arc::new(Mutex::new(HashMap::new()));
        src.connect_pad_added({
            let pipeline = pipeline.downgrade();
            let logs = logs.clone();
            move |_, pad| {
                let Some(pipeline) = pipeline.upgrade() else {
                    return;
                };
                let sink = gst::ElementFactory::make("fakesink")
                    .property("sync", false)
                    .build()
                    .expect("fakesink");
                pipeline.add(&sink).expect("adding fakesink");

                let sinkpad = sink.static_pad("sink").expect("fakesink sink pad");
                let name = pad.name().to_string();
                let logs = logs.clone();
                sinkpad.add_probe(
                    gst::PadProbeType::BUFFER
                        | gst::PadProbeType::EVENT_DOWNSTREAM
                        | gst::PadProbeType::EVENT_FLUSH,
                    move |_, info| {
                        let mut logs = logs.lock().unwrap();
                        let log = logs.entry(name.clone()).or_default();
                        match &info.data {
                            Some(gst::PadProbeData::Buffer(buffer)) => {
                                log.buffers.push(BufferInfo {
                                    pts: buffer.pts(),
                                    duration: buffer.duration(),
                                    offset: buffer.offset(),
                                    delta_unit: buffer
                                        .flags()
                                        .contains(gst::BufferFlags::DELTA_UNIT),
                                });
                            }
                            Some(gst::PadProbeData::Event(event)) => match event.view() {
                                gst::EventView::StreamStart(ev) => {
                                    log.stream_id = Some(ev.stream_id().to_string());
                                }
                                gst::EventView::Segment(_) => log.segments += 1,
                                gst::EventView::Gap(ev) => log.gaps.push(ev.get()),
                                gst::EventView::FlushStart(_) => log.flush_starts += 1,
                                gst::EventView::FlushStop(_) => log.flush_stops += 1,
                                gst::EventView::Eos(_) => log.eos = true,
                                _ => (),
                            },
                            _ => (),
                        }
                        gst::PadProbeReturn::Ok
                    },
                );

                pad.link(&sinkpad).expect("linking an ftestsrc pad");
                sink.sync_state_with_parent().expect("syncing fakesink");
            }
        });

        let bus = pipeline.bus().expect("pipeline bus");
        (
            Self {
                pipeline,
                src,
                bus,
                logs,
                key: key.to_owned(),
            },
            scenario,
        )
    }

    fn play(&self) {
        self.pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline to PLAYING");
    }

    /// [`play`](Self::play), waiting out the ASYNC commit. Every test that
    /// seeks must start from a settled pipeline the way a real player does: a
    /// flushing seek racing the initial PLAYING commit can wedge the commit,
    /// and a sink whose transition never completes defers its EOS message
    /// forever (seen as a parallel-suite flake in the offset-seek tests).
    fn play_settled(&self) {
        self.play();
        let (res, _, _) = self.pipeline.state(gst::ClockTime::from_seconds(10));
        res.expect("the PLAYING commit");
    }

    /// Drains the bus until EOS, panicking on errors and on the deadline.
    fn wait_for_eos(&self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out before EOS; sink logs: {:#?}",
                    self.logs.lock().unwrap()
                );
            }
            let Some(msg) = self.bus.timed_pop(gst::ClockTime::from_mseconds(50)) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::Eos(_) => return,
                gst::MessageView::Error(err) => panic!(
                    "pipeline error from {:?}: {} ({:?})",
                    msg.src().map(|src| src.path_string()),
                    err.error(),
                    err.debug()
                ),
                _ => (),
            }
        }
    }

    /// Drains the bus until an error arrives, returning its debug string.
    fn wait_for_error(&self) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timed out before the error");
            let Some(msg) = self.bus.timed_pop(gst::ClockTime::from_mseconds(50)) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::Error(err) => {
                    return err
                        .debug()
                        .map(|debug| debug.to_string())
                        .unwrap_or_default();
                }
                gst::MessageView::Eos(_) => panic!("EOS instead of the injected error"),
                _ => (),
            }
        }
    }

    /// Polls until `condition` holds over the recorded logs.
    fn wait_until(&self, what: &str, condition: impl Fn(&HashMap<String, PadLog>) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if condition(&self.logs.lock().unwrap()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {what}: {:?}", self.logs.lock());
    }

    fn log_of(&self, pad: &str) -> std::sync::MutexGuard<'_, HashMap<String, PadLog>> {
        let logs = self.logs.lock().unwrap();
        assert!(logs.contains_key(pad), "no recording for {pad}: {logs:?}");
        logs
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // A stall left parked would wedge the teardown, so release everything.
        if let Some(scenario) = registry::lookup(&self.key) {
            scenario.release_all_sync_points();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        registry::unregister(&self.key);
    }
}

fn flushing_zero_seek() -> gst::Event {
    gst::event::Seek::new(
        1.0,
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
        gst::SeekType::Set,
        gst::ClockTime::ZERO,
        gst::SeekType::None,
        gst::ClockTime::NONE,
    )
}

#[test]
fn video_keyframes_follow_the_interval() {
    let spec = MediaSpec::new(3).with_stream(
        StreamSpec::new(
            "video_0",
            fcasttest::spec::StreamKind::Video {
                width: caps::RAW_VIDEO_WIDTH,
                height: caps::RAW_VIDEO_HEIGHT,
                fps: gst::Fraction::new(25, 1),
                keyframe_interval: 4,
            },
        )
        .with_duration(TEN_FRAMES)
        .with_bytes_per_buffer(32),
    );
    let (harness, _scenario) = Harness::new("schedkey", spec);
    harness.play();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");
    let log = &logs["video_0"];
    assert_eq!(log.buffers.len(), 10);
    assert_eq!(log.segments, 1);
    assert!(log.eos);
    assert_eq!(log.stream_id.as_deref(), Some("ftest-schedkey-video_0"));

    for (index, buffer) in log.buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(buffer.offset, index, "buffer {index} offset");
        assert_eq!(buffer.pts, Some(FRAME * index), "buffer {index} pts");
        assert_eq!(buffer.duration, Some(FRAME), "buffer {index} duration");
        // The convention ftestdec relies on.
        assert_eq!(
            buffer.delta_unit,
            !index.is_multiple_of(4),
            "buffer {index} delta-unit flag"
        );
    }
}

#[test]
fn text_cues_are_surrounded_by_gaps() {
    let cues = vec![
        CueSpec::new(
            gst::ClockTime::from_mseconds(50),
            gst::ClockTime::from_mseconds(150),
            "first cue",
        ),
        CueSpec::new(
            gst::ClockTime::from_mseconds(250),
            gst::ClockTime::from_mseconds(350),
            "second cue",
        ),
    ];
    let spec =
        MediaSpec::new(4).with_stream(StreamSpec::text("text_0", cues).with_duration(TEN_FRAMES));
    let (harness, _scenario) = Harness::new("schedtext", spec);
    harness.play();
    harness.wait_for_eos();

    let logs = harness.log_of("text_0");
    let log = &logs["text_0"];

    assert_eq!(
        log.pts(),
        vec![
            Some(gst::ClockTime::from_mseconds(50)),
            Some(gst::ClockTime::from_mseconds(250)),
        ]
    );
    for buffer in &log.buffers {
        assert_eq!(buffer.duration, Some(gst::ClockTime::from_mseconds(100)));
        assert!(!buffer.delta_unit, "text buffers are keyframes");
    }
    // Leading gap, the gap between the cues, and the tail up to the duration.
    assert_eq!(
        log.gaps,
        vec![
            (
                gst::ClockTime::ZERO,
                Some(gst::ClockTime::from_mseconds(50))
            ),
            (
                gst::ClockTime::from_mseconds(150),
                Some(gst::ClockTime::from_mseconds(100))
            ),
            (
                gst::ClockTime::from_mseconds(350),
                Some(gst::ClockTime::from_mseconds(50))
            ),
        ]
    );
}

#[test]
fn stall_fault_parks_until_released() {
    let spec = MediaSpec::new(5).with_stream(
        StreamSpec::video("video_0")
            .with_duration(TEN_FRAMES)
            .with_bytes_per_buffer(32)
            .with_fault(Fault::StallAt {
                buffer_index: 3,
                sync_point: "mid".to_owned(),
            }),
    );
    let (harness, scenario) = Harness::new("schedstall", spec);
    let gate = scenario.sync_point("mid");
    harness.play();

    assert!(
        gate.wait_for_arrival(TIMEOUT),
        "the push never reached the gate"
    );
    // Parked before pushing buffer 3, so exactly three buffers made it out and
    // nothing more can follow while the gate holds.
    harness.wait_until("three buffers", |logs| {
        logs.get("video_0")
            .is_some_and(|log| log.buffers.len() == 3)
    });
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(harness.log_of("video_0")["video_0"].buffers.len(), 3);
    assert_eq!(gate.arrivals(), 1, "the park registered one arrival");

    gate.release();
    harness.wait_for_eos();
    assert_eq!(harness.log_of("video_0")["video_0"].buffers.len(), 10);
}

#[test]
fn flushing_zero_seek_restarts_the_schedule() {
    let spec = MediaSpec::new(6).with_stream(
        StreamSpec::video("video_0")
            .with_duration(gst::ClockTime::from_mseconds(200))
            .with_bytes_per_buffer(32)
            .with_fault(Fault::StallAt {
                buffer_index: 2,
                sync_point: "mid".to_owned(),
            }),
    );
    let (harness, scenario) = Harness::new("schedseek", spec);
    let gate = scenario.sync_point("mid");
    harness.play_settled();
    assert!(gate.wait_for_arrival(TIMEOUT), "no park before the seek");

    // Sent through the pipeline, so it travels upstream from the sink the way a
    // player's seek does.
    assert!(
        harness.pipeline.send_event(flushing_zero_seek()),
        "the flushing zero-seek was refused"
    );

    // The restarted schedule parks on the same buffer again.
    harness.wait_until("the second park", |_| gate.arrivals() == 2);
    gate.release();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");
    let log = &logs["video_0"];
    assert_eq!(log.flush_starts, 1);
    assert_eq!(log.flush_stops, 1);
    // A segment before the flush and one after it.
    assert_eq!(log.segments, 2);
    // Two buffers before the seek, then the whole five-frame schedule again.
    assert_eq!(
        log.pts(),
        vec![
            Some(gst::ClockTime::ZERO),
            Some(FRAME),
            Some(gst::ClockTime::ZERO),
            Some(FRAME),
            Some(FRAME * 2u64),
            Some(FRAME * 3u64),
            Some(FRAME * 4u64),
        ]
    );
}

/// A flushing 1.0x seek to a non-zero position restarts each stream at the
/// target: the new segment begins there, video walks back to the nearest
/// keyframe (the lead-in sits before the segment start), and audio resumes at
/// the first packet still inside the segment.
#[test]
fn flushing_offset_seek_restarts_at_the_target() {
    let stall = |stream: StreamSpec| {
        stream
            .with_duration(TEN_FRAMES)
            .with_bytes_per_buffer(32)
            .with_fault(Fault::StallAt {
                buffer_index: 2,
                sync_point: "mid".to_owned(),
            })
    };
    let spec = MediaSpec::new(11)
        .with_stream(stall(StreamSpec::new(
            "video_0",
            fcasttest::spec::StreamKind::Video {
                width: caps::RAW_VIDEO_WIDTH,
                height: caps::RAW_VIDEO_HEIGHT,
                fps: gst::Fraction::new(25, 1),
                keyframe_interval: 4,
            },
        )))
        .with_stream(stall(StreamSpec::audio("audio_0")));
    let (harness, scenario) = Harness::new("schedoffset", spec);
    let gate = scenario.sync_point("mid");
    harness.play_settled();
    harness.wait_until("both streams parked", |_| gate.arrivals() == 2);

    // 230 ms: inside video frame 5 (200..240 ms, mid-GOP of a 4-frame
    // interval) and inside audio packet 11 (220..240 ms).
    let target = gst::ClockTime::from_mseconds(230);
    let seek = gst::event::Seek::new(
        1.0,
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
        gst::SeekType::Set,
        target,
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    assert!(
        harness.src.send_event(seek),
        "the flushing offset seek was refused"
    );
    gate.release();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");

    let video = &logs["video_0"];
    assert_eq!(video.flush_starts, 1, "video flush-start");
    assert_eq!(video.segments, 2, "video segments");
    // Two buffers before the park, then the restart: frame 5 covers 230 ms
    // but the restart walks back to its keyframe, frame 4 at 160 ms.
    let pts = video.pts();
    assert_eq!(pts[..2], [Some(gst::ClockTime::ZERO), Some(FRAME)]);
    assert_eq!(pts[2], Some(FRAME * 4u64), "video restart pts");
    assert_eq!(
        pts.last().copied().flatten(),
        Some(FRAME * 9u64),
        "video plays out from the restart"
    );
    assert!(video.eos, "video EOS");

    let audio = &logs["audio_0"];
    let packet = gst::ClockTime::from_mseconds(20);
    assert_eq!(audio.segments, 2, "audio segments");
    // Audio has no keyframes to walk back to: packet 11 spans the target.
    assert_eq!(
        audio.pts()[2],
        Some(packet * 11u64),
        "audio restart pts"
    );
    assert!(audio.eos, "audio EOS");
}

/// A seek past every schedule item is not an error: the restarted stream
/// delivers an immediate EOS, the way a demuxer ends a past-the-end seek.
#[test]
fn offset_seek_past_the_end_goes_straight_to_eos() {
    let spec = MediaSpec::new(12).with_stream(
        StreamSpec::video("video_0")
            .with_duration(TEN_FRAMES)
            .with_bytes_per_buffer(32)
            .with_fault(Fault::StallAt {
                buffer_index: 1,
                sync_point: "hold".to_owned(),
            }),
    );
    let (harness, scenario) = Harness::new("schedpastend", spec);
    let gate = scenario.sync_point("hold");
    harness.play_settled();
    assert!(gate.wait_for_arrival(TIMEOUT), "no park before the seek");

    let seek = gst::event::Seek::new(
        1.0,
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
        gst::SeekType::Set,
        gst::ClockTime::from_seconds(5),
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    assert!(harness.src.send_event(seek), "the past-the-end seek was refused");
    gate.release();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");
    let log = &logs["video_0"];
    // One buffer before the park, none after the seek.
    assert_eq!(log.buffers.len(), 1);
    assert!(log.eos);
}

#[test]
fn element_seek_restarts_every_stream() {
    let stall = |stream: StreamSpec| {
        stream
            .with_duration(gst::ClockTime::from_mseconds(200))
            .with_bytes_per_buffer(32)
            .with_fault(Fault::StallAt {
                buffer_index: 2,
                sync_point: "both".to_owned(),
            })
    };
    let spec = MediaSpec::new(7)
        .with_stream(stall(StreamSpec::video("video_0")))
        .with_stream(stall(StreamSpec::audio("audio_0")));
    let (harness, scenario) = Harness::new("schedmulti", spec);
    let gate = scenario.sync_point("both");
    harness.play_settled();
    harness.wait_until("both streams parked", |_| gate.arrivals() == 2);

    // Sent to the element: its default handler would pick a single random src
    // pad and leave the other stream running on the old schedule.
    assert!(
        harness.src.send_event(flushing_zero_seek()),
        "the element refused the flushing zero-seek"
    );
    harness.wait_until("both streams parked again", |_| gate.arrivals() == 4);
    gate.release();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");
    for pad in ["video_0", "audio_0"] {
        let log = &logs[pad];
        assert_eq!(log.flush_starts, 1, "{pad} flush-start");
        assert_eq!(log.flush_stops, 1, "{pad} flush-stop");
        assert_eq!(log.segments, 2, "{pad} segments");
        // Restarted from zero after two buffers.
        let pts = log.pts();
        assert_eq!(pts[2], Some(gst::ClockTime::ZERO), "{pad} restart pts");
        assert!(log.eos, "{pad} EOS");
    }
    assert_eq!(logs["video_0"].buffers.len(), 2 + 5);
    assert_eq!(logs["audio_0"].buffers.len(), 2 + 10);
}

#[test]
fn error_fault_stops_the_stream() {
    let spec = MediaSpec::new(8).with_stream(
        StreamSpec::video("video_0")
            .with_duration(TEN_FRAMES)
            .with_bytes_per_buffer(32)
            .with_fault(Fault::ErrorAt { buffer_index: 4 }),
    );
    let (harness, _scenario) = Harness::new("schederror", spec);
    harness.play();

    let debug = harness.wait_for_error();
    assert!(
        debug.contains("injected error at buffer 4"),
        "unexpected error debug: {debug}"
    );
    assert_eq!(harness.log_of("video_0")["video_0"].buffers.len(), 4);
}

/// The fault must be byte-identical to a real source giving up: fcastplaybin
/// reads "reason not-linked" / "reason flushing" out of the debug string and
/// recovers in place, treating anything else as a genuine failure. Asserting
/// less would let the text drift and the fault would come to mean the opposite.
#[test]
fn flow_stop_fault_posts_the_flow_error_shape() {
    for (reason, key) in [
        (FlowStopReason::NotLinked, "schedflownl"),
        (FlowStopReason::Flushing, "schedflowfl"),
    ] {
        let spec = MediaSpec::new(11).with_stream(
            StreamSpec::video("video_0")
                .with_duration(TEN_FRAMES)
                .with_bytes_per_buffer(32)
                .with_fault(Fault::FlowStoppedAt {
                    buffer_index: 3,
                    reason,
                }),
        );
        let (harness, _scenario) = Harness::new(key, spec);
        harness.play();

        let debug = harness.wait_for_error();
        assert!(
            debug.contains(reason.debug_text()),
            "the debug text must carry GST_ELEMENT_FLOW_ERROR's {:?} verbatim, got: {debug}",
            reason.debug_text()
        );
        assert_eq!(harness.log_of("video_0")["video_0"].buffers.len(), 3);
    }
}

#[test]
fn eos_fault_ends_the_stream_early() {
    let spec = MediaSpec::new(9).with_stream(
        StreamSpec::video("video_0")
            .with_duration(TEN_FRAMES)
            .with_bytes_per_buffer(32)
            .with_fault(Fault::EosAt { buffer_index: 6 }),
    );
    let (harness, _scenario) = Harness::new("schedeos", spec);
    harness.play();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");
    assert_eq!(logs["video_0"].buffers.len(), 6);
    assert!(logs["video_0"].eos);
}

#[test]
fn missing_scenario_errors_on_the_bus() {
    init();
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("ftestsrc")
        .property("uri", caps::uri_for_key("schednosuch"))
        .build()
        .expect("ftestsrc");
    pipeline.add(&src).expect("adding ftestsrc");

    // The state change fails, and the reason has to be readable on the bus.
    assert!(pipeline.set_state(gst::State::Playing).is_err());
    let bus = pipeline.bus().expect("pipeline bus");
    let mut debug = None;
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
        if let gst::MessageView::Error(err) = msg.view() {
            debug = err.debug().map(|debug| debug.to_string());
            break;
        }
    }
    let debug = debug.expect("an error message on the bus");
    assert!(
        debug.contains("no scenario registered for key 'schednosuch'"),
        "unexpected error debug: {debug}"
    );

    let _ = pipeline.set_state(gst::State::Null);
}

#[test]
fn unsupported_seeks_are_refused_without_disturbing_the_schedule() {
    let spec = MediaSpec::new(10).with_stream(
        StreamSpec::video("video_0")
            .with_duration(TEN_FRAMES)
            .with_bytes_per_buffer(32)
            .with_fault(Fault::StallAt {
                buffer_index: 1,
                sync_point: "hold".to_owned(),
            }),
    );
    let (harness, scenario) = Harness::new("schedrefuse", spec);
    let gate = scenario.sync_point("hold");
    harness.play_settled();
    assert!(gate.wait_for_arrival(TIMEOUT), "no park before the seeks");

    let rated = gst::event::Seek::new(
        1.5,
        gst::SeekFlags::FLUSH,
        gst::SeekType::Set,
        gst::ClockTime::ZERO,
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    assert!(!harness.src.send_event(rated), "a non-1.0x seek was taken");

    let non_flushing = gst::event::Seek::new(
        1.0,
        gst::SeekFlags::empty(),
        gst::SeekType::Set,
        gst::ClockTime::ZERO,
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    assert!(
        !harness.src.send_event(non_flushing),
        "a non-flushing seek was taken"
    );

    gate.release();
    harness.wait_for_eos();

    let logs = harness.log_of("video_0");
    let log = &logs["video_0"];
    assert_eq!(log.flush_starts, 0);
    assert_eq!(log.segments, 1);
    assert_eq!(log.buffers.len(), 10);
}
