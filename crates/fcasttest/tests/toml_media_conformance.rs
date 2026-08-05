//! Does a scenario file actually produce the media it describes?
//!
//! Every other test in the tree takes that on trust. This one reads one TOML
//! document, plays it through `ftestsrc`, and checks the bytes that came out
//! against the document field by field: caps, buffer counts, PTS, durations,
//! payload sizes, keyframe flags, cue text and the sparse-stream gaps. A
//! generator that quietly disagrees with its own description makes every
//! scenario file a lie, so the assertions are on the OUTPUT, never on the parsed
//! `MediaSpec`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcasttest::{caps, scenario::toml};
use gst::prelude::*;

const TIMEOUT: Duration = Duration::from_secs(15);

/// The document under test. Every number here is asserted against the media.
const DOCUMENT: &str = r#"
key = "conform"
duration_ms = 800
bytes_per_buffer = 48
pacing = "as_fast_as_possible"

[[stream]]
id = "video_0"
kind = "video"
width = 32
height = 24
fps = 20
keyframe_interval = 3

[[stream]]
id = "audio_0"
kind = "audio"
rate = 44100
channels = 1

[[stream]]
id = "text_0"
kind = "text"

[[stream.cue]]
start_ms = 100
end_ms = 250
text = "FIRST"

[[stream.cue]]
start_ms = 400
end_ms = 500
text = "SECOND"
"#;

#[derive(Debug)]
struct Buf {
    pts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
    size: usize,
    payload: Vec<u8>,
    keyframe: bool,
}

#[derive(Debug, Default)]
struct PadLog {
    caps: Option<gst::Caps>,
    buffers: Vec<Buf>,
    gaps: Vec<(gst::ClockTime, Option<gst::ClockTime>)>,
    eos: bool,
}

type Logs = Arc<Mutex<HashMap<String, PadLog>>>;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(fcasttest::register_for_tests);
}

/// ftestsrc into one recording fakesink per pad, linked from `pad-added` before
/// the streaming tasks start.
fn play(uri: &str) -> (Logs, gst::Element) {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("ftestsrc")
        .property("uri", uri)
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
                gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_, info| {
                    let mut logs = logs.lock().unwrap();
                    let log = logs.entry(name.clone()).or_default();
                    match &info.data {
                        Some(gst::PadProbeData::Buffer(buffer)) => {
                            let map = buffer.map_readable().expect("mapping a recorded buffer");
                            log.buffers.push(Buf {
                                pts: buffer.pts(),
                                duration: buffer.duration(),
                                size: buffer.size(),
                                payload: map.to_vec(),
                                keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                            });
                        }
                        Some(gst::PadProbeData::Event(event)) => match event.view() {
                            gst::EventView::Caps(ev) => log.caps = Some(ev.caps().to_owned()),
                            gst::EventView::Gap(ev) => log.gaps.push(ev.get()),
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

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline to PLAYING");
    let bus = pipeline.bus().expect("pipeline bus");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out before EOS: {:#?}",
            logs.lock().unwrap()
        );
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(err) => panic!("pipeline error: {}", err.error()),
            _ => (),
        }
    }

    // The duration query is answered by the source, so ask before teardown.
    let src_for_query = src.clone();
    let _ = pipeline.set_state(gst::State::Null);
    (logs, src_for_query)
}

#[test]
fn the_generated_media_matches_the_document() {
    init();
    let handle = toml::load_str(DOCUMENT).expect("the document parses");
    let (logs, _src) = play(&handle.uri());
    let logs = logs.lock().unwrap();

    // ---------------------------------------------------------------- video
    let video = logs.get("video_0").expect("a video pad");
    let video_caps = video.caps.as_ref().expect("video caps");
    let structure = video_caps.structure(0).expect("a video caps structure");
    assert_eq!(structure.name(), caps::VIDEO_MEDIA_TYPE);
    assert_eq!(
        structure.get::<i32>("width").ok(),
        Some(32),
        "width = 32 in the document, caps say {video_caps}"
    );
    assert_eq!(
        structure.get::<i32>("height").ok(),
        Some(24),
        "height = 24 in the document, caps say {video_caps}"
    );
    assert_eq!(
        structure.get::<gst::Fraction>("framerate").ok(),
        Some(gst::Fraction::new(20, 1)),
        "fps = 20 in the document, caps say {video_caps}"
    );

    // fps = 20 is a 50 ms frame, 800 ms of media is 16 of them.
    let frame = gst::ClockTime::from_mseconds(50);
    assert_eq!(
        video.buffers.len(),
        16,
        "duration_ms = 800 at fps = 20 is 16 frames"
    );
    for (index, buffer) in video.buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(buffer.pts, Some(frame * index), "video frame {index} pts");
        assert_eq!(buffer.duration, Some(frame), "video frame {index} duration");
        assert_eq!(
            buffer.size, 48,
            "bytes_per_buffer = 48 in the document, frame {index} is {} bytes",
            buffer.size
        );
        // keyframe_interval = 3, and ftestdec's needs-keyframe-after-flush knob
        // depends on this exact rule.
        assert_eq!(
            buffer.keyframe,
            index.is_multiple_of(3),
            "video frame {index} keyframe flag at keyframe_interval = 3"
        );
    }
    // The last frame ends on the declared duration, so the media is neither
    // short nor long.
    let last = video.buffers.last().expect("a last video frame");
    assert_eq!(
        last.pts.unwrap() + last.duration.unwrap(),
        gst::ClockTime::from_mseconds(800),
        "the video ends on duration_ms"
    );
    assert!(video.eos, "video EOS");

    // ---------------------------------------------------------------- audio
    let audio = logs.get("audio_0").expect("an audio pad");
    let audio_caps = audio.caps.as_ref().expect("audio caps");
    let structure = audio_caps.structure(0).expect("an audio caps structure");
    assert_eq!(structure.name(), caps::AUDIO_MEDIA_TYPE);
    assert_eq!(
        structure.get::<i32>("rate").ok(),
        Some(44100),
        "rate = 44100 in the document, caps say {audio_caps}"
    );
    assert_eq!(
        structure.get::<i32>("channels").ok(),
        Some(1),
        "channels = 1 in the document, caps say {audio_caps}"
    );

    // One packet per 20 ms, so 800 ms is 40 packets.
    let packet = gst::ClockTime::from_mseconds(20);
    assert_eq!(audio.buffers.len(), 40, "duration_ms = 800 is 40 packets");
    for (index, buffer) in audio.buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(buffer.pts, Some(packet * index), "audio packet {index} pts");
        assert_eq!(
            buffer.duration,
            Some(packet),
            "audio packet {index} duration"
        );
        assert_eq!(buffer.size, 48, "audio packet {index} size");
        assert!(buffer.keyframe, "every audio packet is a keyframe");
    }
    assert!(audio.eos, "audio EOS");

    // ----------------------------------------------------------------- text
    let text = logs.get("text_0").expect("a text pad");
    let text_caps = text.caps.as_ref().expect("text caps");
    assert!(
        text_caps.can_intersect(&caps::text_caps()),
        "text caps {text_caps} are not the parsed utf8 caps"
    );

    // One buffer per cue, carrying the cue text verbatim. bytes_per_buffer does
    // NOT apply to a text stream, which is exactly the claim being checked.
    assert_eq!(text.buffers.len(), 2, "one buffer per cue");
    let cues = [
        ("FIRST", 100u64, 250u64),
        ("SECOND", 400u64, 500u64),
    ];
    for (buffer, (expected_text, start_ms, end_ms)) in text.buffers.iter().zip(cues) {
        assert_eq!(
            buffer.pts,
            Some(gst::ClockTime::from_mseconds(start_ms)),
            "cue {expected_text} start_ms"
        );
        assert_eq!(
            buffer.duration,
            Some(gst::ClockTime::from_mseconds(end_ms - start_ms)),
            "cue {expected_text} length"
        );
        assert_eq!(
            std::str::from_utf8(&buffer.payload).expect("utf8 cue payload"),
            expected_text,
            "cue payload"
        );
        assert_eq!(
            buffer.size,
            expected_text.len(),
            "a cue buffer is its text, not bytes_per_buffer"
        );
    }

    // The gaps fill everything the cues do not: 0..100, 250..400, 500..800.
    let ms = gst::ClockTime::from_mseconds;
    assert_eq!(
        text.gaps,
        vec![
            (ms(0), Some(ms(100))),
            (ms(250), Some(ms(150))),
            (ms(500), Some(ms(300))),
        ],
        "the sparse stream is covered end to end by cues and gaps"
    );
    assert!(text.eos, "text EOS");

    handle.unregister();
}

/// `duration_ms` is what the source reports upstream, not just what it schedules.
#[test]
fn the_reported_duration_matches_the_document() {
    init();
    let handle = toml::load_str(
        r#"
        key = "conformdur"
        duration_ms = 640
        [[stream]]
        id = "video_0"
        kind = "video"
        fps = 25
        "#,
    )
    .expect("the document parses");

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("ftestsrc")
        .property("uri", handle.uri())
        .build()
        .expect("ftestsrc");
    pipeline.add(&src).expect("adding ftestsrc");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline.add(&sink).expect("adding fakesink");
    src.connect_pad_added({
        let sink = sink.clone();
        move |_, pad| {
            let sinkpad = sink.static_pad("sink").expect("fakesink sink pad");
            if !sinkpad.is_linked() {
                pad.link(&sinkpad).expect("linking");
            }
        }
    });
    pipeline
        .set_state(gst::State::Paused)
        .expect("pipeline to PAUSED");
    let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(5));
    res.expect("the PAUSED commit");

    let mut query = gst::query::Duration::new(gst::Format::Time);
    assert!(src.query(&mut query), "the source answers a duration query");
    let gst::GenericFormattedValue::Time(duration) = query.result() else {
        panic!("the duration came back in a format other than time: {:?}", query.result());
    };
    assert_eq!(
        duration,
        Some(gst::ClockTime::from_mseconds(640)),
        "duration_ms = 640 in the document"
    );

    let _ = pipeline.set_state(gst::State::Null);
    handle.unregister();
}

/// A document must not be able to describe media that does not exist. At fps = 1
/// a 500 ms clip holds no whole frame, so `build_schedule` produced nothing and
/// the "video" stream was an immediate EOS with zero buffers - a scenario file
/// claiming video and serving silence, with nothing anywhere saying so.
#[test]
fn a_stream_that_would_schedule_no_buffers_is_refused() {
    init();
    let cases = [
        (
            r#"key = "conformempty"
               duration_ms = 500
               [[stream]]
               id = "video_0"
               kind = "video"
               fps = 1"#,
            "video_0",
        ),
        (
            // 10 ms is half an audio packet.
            r#"key = "conformempty2"
               duration_ms = 10
               [[stream]]
               id = "audio_0"
               kind = "audio""#,
            "audio_0",
        ),
        (
            // The top-level duration_ms wins over the per-stream one, so the
            // check has to run on the resolved spec.
            r#"key = "conformempty3"
               duration_ms = 20
               [[stream]]
               id = "video_0"
               kind = "video"
               duration_ms = 10000"#,
            "video_0",
        ),
    ];
    for (document, stream) in cases {
        match toml::parse_str(document) {
            Ok(_) => panic!("a stream with no schedulable buffer was accepted:\n{document}"),
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains(stream) && message.contains("immediate EOS"),
                    "the error does not say which stream has no media: {message}"
                );
            }
        }
    }

    // A text stream with no cues is sparse by design, not empty media.
    let handle = toml::load_str(
        r#"
        key = "conformsparse"
        duration_ms = 500
        [[stream]]
        id = "text_0"
        kind = "text"
        "#,
    )
    .expect("an all-gap text stream is legitimate");
    handle.unregister();
}
