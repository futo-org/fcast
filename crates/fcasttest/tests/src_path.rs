//! The risky path: `urisourcebin parse-streams=true` over `ftest://`.
//!
//! No typefinder claims the fcasttest caps, so this proves urisourcebin's
//! typefind and parsebin honour the sticky CAPS event ftestsrc pushes, that
//! parsebin plugs ftestparse and exposes the parsed streams, and that data
//! reaches sinks on all three streams.
//!
//! Note the dependency on ftestdec being registered: parsebin only exposes a
//! parsed pad when a decoder for the caps exists. With none it marks the chain a
//! deadend, posts missing-plugin and exposes nothing.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use fcasttest::{
    caps, registry,
    spec::{CueSpec, MediaSpec, StreamSpec},
};
use gst::prelude::*;

const KEY: &str = "srcpath";
const DB3_KEY: &str = "srcpathdb3";
const DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(400);
const TIMEOUT: Duration = Duration::from_secs(15);

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(fcasttest::register_for_tests);
}

#[derive(Debug, Default)]
struct Recorded {
    stream_id: Option<String>,
    caps: Option<gst::Caps>,
    buffers: usize,
    gaps: usize,
    eos: bool,
}

type Recording = Arc<Mutex<HashMap<String, Recorded>>>;

/// Attaches a counting fakesink to every pad urisourcebin exposes. The sinks
/// keep their default async handling, so no branch can drain before the pipeline
/// runs and every stream is represented when EOS is decided.
fn record_pads(pipeline: &gst::Pipeline, src: &gst::Element) -> (Recording, Receiver<String>) {
    let recording: Recording = Arc::new(Mutex::new(HashMap::new()));
    let (exposed_tx, exposed_rx) = mpsc::channel();
    let pipeline = pipeline.downgrade();
    let recording_for_pads = recording.clone();

    src.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline.upgrade() else {
            return;
        };
        let name = pad.name().to_string();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink");
        pipeline.add(&sink).expect("adding fakesink");

        let sinkpad = sink.static_pad("sink").expect("fakesink sink pad");
        let recording = recording_for_pads.clone();
        sinkpad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                let mut recording = recording.lock().unwrap();
                let entry = recording.entry(name.clone()).or_default();
                match &info.data {
                    Some(gst::PadProbeData::Buffer(_)) => entry.buffers += 1,
                    Some(gst::PadProbeData::Event(event)) => match event.view() {
                        gst::EventView::StreamStart(ev) => {
                            entry.stream_id = Some(ev.stream_id().to_string());
                        }
                        gst::EventView::Caps(ev) => entry.caps = Some(ev.caps_owned()),
                        gst::EventView::Gap(_) => entry.gaps += 1,
                        gst::EventView::Eos(_) => entry.eos = true,
                        _ => (),
                    },
                    _ => (),
                }
                gst::PadProbeReturn::Ok
            },
        );

        pad.link(&sinkpad).expect("linking an exposed pad");
        sink.sync_state_with_parent().expect("syncing fakesink");
        let _ = exposed_tx.send(pad.name().to_string());
    });

    (recording, exposed_rx)
}

/// Waits for `expected` pads while PAUSED, then plays to EOS. Returns every
/// stream collection posted on the way.
fn run(
    pipeline: &gst::Pipeline,
    exposed: Receiver<String>,
    expected: usize,
) -> Vec<gst::StreamCollection> {
    let bus = pipeline.bus().expect("pipeline bus");
    pipeline
        .set_state(gst::State::Paused)
        .expect("pipeline to PAUSED");

    let mut collections = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    let mut pads = Vec::new();
    while pads.len() < expected {
        check_bus(&bus, &mut collections, deadline);
        match exposed.recv_timeout(Duration::from_millis(50)) {
            Ok(name) => pads.push(name),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    Instant::now() < deadline,
                    "only {pads:?} of {expected} pads were exposed"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("pad channel closed"),
        }
    }

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline to PLAYING");

    loop {
        if check_bus(&bus, &mut collections, deadline) {
            break;
        }
        assert!(Instant::now() < deadline, "timed out before EOS");
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    collections
}

/// Drains the bus for up to 50 ms. Returns true on EOS, panics on errors.
fn check_bus(
    bus: &gst::Bus,
    collections: &mut Vec<gst::StreamCollection>,
    deadline: Instant,
) -> bool {
    let left = deadline.saturating_duration_since(Instant::now());
    let wait = left.min(Duration::from_millis(50));
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(wait.as_nanos() as u64)) {
        match msg.view() {
            gst::MessageView::Eos(_) => return true,
            gst::MessageView::Error(err) => panic!(
                "pipeline error from {:?}: {} ({:?})",
                msg.src().map(|src| src.path_string()),
                err.error(),
                err.debug()
            ),
            gst::MessageView::StreamCollection(msg) => collections.push(msg.stream_collection()),
            _ => (),
        }
    }
    false
}

#[test]
fn urisourcebin_exposes_every_stream() {
    init();

    let spec = MediaSpec::new(11)
        .with_stream(
            StreamSpec::video("video_0")
                .with_duration(DURATION)
                .with_bytes_per_buffer(64),
        )
        .with_stream(
            StreamSpec::audio("audio_0")
                .with_duration(DURATION)
                .with_bytes_per_buffer(64),
        )
        .with_stream(
            StreamSpec::text(
                "text_0",
                vec![
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
                ],
            )
            .with_duration(DURATION),
        );
    registry::register_scenario(KEY, spec);

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("urisourcebin")
        .property("uri", caps::uri_for_key(KEY))
        .property("parse-streams", true)
        .build()
        .expect("urisourcebin");
    pipeline.add(&src).expect("adding urisourcebin");

    let (recording, exposed) = record_pads(&pipeline, &src);
    let collections = run(&pipeline, exposed, 3);
    let recording = recording.lock().unwrap();

    assert_eq!(
        recording.len(),
        3,
        "expected one exposed pad per stream, got {recording:?}"
    );

    let mut by_suffix = HashMap::new();
    for recorded in recording.values() {
        let stream_id = recorded.stream_id.as_deref().expect("a stream-id");
        let (key, suffix) =
            caps::split_stream_id(stream_id).expect("a stream-id carrying our key and suffix");
        assert_eq!(key, KEY);
        by_suffix.insert(suffix.to_owned(), recorded);
    }

    let video = by_suffix.get("video_0").expect("the video stream");
    let audio = by_suffix.get("audio_0").expect("the audio stream");
    let text = by_suffix.get("text_0").expect("the text stream");

    // 400 ms at 25 fps, and at one 20 ms audio packet per buffer.
    assert_eq!(video.buffers, 10, "video buffers");
    assert_eq!(audio.buffers, 20, "audio buffers");
    // One buffer per cue, gaps around them.
    assert_eq!(text.buffers, 2, "text buffers");
    assert!(text.gaps >= 2, "text gaps: {}", text.gaps);
    assert!(video.eos && audio.eos && text.eos, "EOS on every stream");

    // ftestparse ran, so the exposed video and audio caps are marked parsed.
    for (name, recorded) in [("video", video), ("audio", audio)] {
        let caps = recorded.caps.as_ref().expect("caps on the exposed pad");
        let structure = caps.structure(0).unwrap();
        assert_eq!(
            structure.get::<bool>("parsed").ok(),
            Some(true),
            "{name} caps {caps} should be parsed"
        );
    }
    let text_caps = text.caps.as_ref().expect("caps on the text pad");
    assert_eq!(
        text_caps.structure(0).unwrap().name(),
        caps::TEXT_MEDIA_TYPE
    );

    let streams: Vec<String> = collections
        .last()
        .expect("a stream collection")
        .iter()
        .filter_map(|stream| stream.stream_id().map(|id| id.to_string()))
        .collect();
    // urisourcebin exposes the raw text pad directly (text/x-raw is in its raw
    // caps set) and only parsebin-derived streams reach its aggregated
    // collection, so the text stream is deliberately absent here. decodebin3
    // makes the complete one, see decodebin3_collects_every_stream.
    assert_eq!(streams.len(), 2, "aggregated collection {streams:?}");
    for suffix in ["video_0", "audio_0"] {
        assert!(
            streams.iter().any(|id| id.contains(suffix)),
            "collection {streams:?} misses {suffix}"
        );
    }

    registry::unregister(KEY);
}

/// One step further along the field topology. The text stream is missing from
/// urisourcebin's collection, but ftestsrc's STREAM_START carries no GstStream,
/// so decodebin3 parses that input itself and its collection is complete. That
/// is what makes text selectable through fcastplaybin.
#[test]
fn decodebin3_collects_every_stream() {
    init();

    let spec = MediaSpec::new(12)
        .with_stream(
            StreamSpec::video("video_0")
                .with_duration(DURATION)
                .with_bytes_per_buffer(64),
        )
        .with_stream(
            StreamSpec::audio("audio_0")
                .with_duration(DURATION)
                .with_bytes_per_buffer(64),
        )
        .with_stream(
            StreamSpec::text(
                "text_0",
                vec![CueSpec::new(
                    gst::ClockTime::from_mseconds(50),
                    gst::ClockTime::from_mseconds(150),
                    "first cue",
                )],
            )
            .with_duration(DURATION),
        );
    registry::register_scenario(DB3_KEY, spec);

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("urisourcebin")
        .property("uri", caps::uri_for_key(DB3_KEY))
        .property("parse-streams", true)
        .build()
        .expect("urisourcebin");
    let decodebin = gst::ElementFactory::make("decodebin3")
        .build()
        .expect("decodebin3");
    pipeline
        .add_many([&src, &decodebin])
        .expect("adding the elements");

    src.connect_pad_added({
        let decodebin = decodebin.clone();
        let inputs = Mutex::new(0usize);
        move |_, pad| {
            let mut inputs = inputs.lock().unwrap();
            // The first input goes to the static pad, exactly how fcastplaybin
            // attaches the main input and its extra sources.
            let sinkpad = if *inputs == 0 {
                decodebin.static_pad("sink").expect("decodebin3 sink pad")
            } else {
                decodebin
                    .request_pad_simple("sink_%u")
                    .expect("decodebin3 request pad")
            };
            *inputs += 1;
            pad.link(&sinkpad).expect("linking into decodebin3");
        }
    });

    // The pipeline never posts EOS here: adding the later sinks drops the EOS the
    // first one already reported, so the streams are counted out individually.
    let decoded_eos = Arc::new(Mutex::new(Vec::<String>::new()));
    decodebin.connect_pad_added({
        let pipeline = pipeline.downgrade();
        let decoded_eos = decoded_eos.clone();
        move |_, pad| {
            // pad-added also fires for the request sink pads.
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
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
            let decoded_eos = decoded_eos.clone();
            sinkpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && let gst::EventView::Eos(_) = event.view()
                {
                    decoded_eos.lock().unwrap().push(name.clone());
                }
                gst::PadProbeReturn::Ok
            });

            pad.link(&sinkpad).expect("linking a decoded pad");
            sink.sync_state_with_parent().expect("syncing fakesink");
        }
    });

    let bus = pipeline.bus().expect("pipeline bus");
    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline to PLAYING");

    let deadline = Instant::now() + TIMEOUT;
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline && seen.len() < 3 {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(left.as_nanos() as u64)) else {
            break;
        };
        match msg.view() {
            gst::MessageView::Error(err) => panic!(
                "pipeline error from {:?}: {} ({:?})",
                msg.src().map(|src| src.path_string()),
                err.error(),
                err.debug()
            ),
            gst::MessageView::StreamCollection(collection) => {
                if msg.src() != Some(decodebin.upcast_ref::<gst::Object>()) {
                    continue;
                }
                seen = collection
                    .stream_collection()
                    .iter()
                    .filter_map(|stream| stream.stream_id().map(|id| id.to_string()))
                    .collect();
            }
            _ => (),
        }
    }

    // Drains all three streams before tearing down: a teardown while decodebin3
    // is still linking slots trips "data flow before stream-start" warnings.
    while Instant::now() < deadline && decoded_eos.lock().unwrap().len() < 3 {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) else {
            continue;
        };
        if let gst::MessageView::Error(err) = msg.view() {
            panic!(
                "pipeline error from {:?}: {} ({:?})",
                msg.src().map(|src| src.path_string()),
                err.error(),
                err.debug()
            );
        }
    }
    let decoded_eos = std::mem::take(&mut *decoded_eos.lock().unwrap());

    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    for suffix in ["video_0", "audio_0", "text_0"] {
        assert!(
            seen.iter().any(|id| id.contains(suffix)),
            "decodebin3 collection {seen:?} misses {suffix}"
        );
        assert!(
            decoded_eos.iter().any(|name| name.contains(suffix)),
            "decoded streams {decoded_eos:?} miss {suffix}"
        );
    }

    registry::unregister(DB3_KEY);
}
