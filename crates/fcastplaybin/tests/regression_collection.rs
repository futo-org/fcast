//! Regression for the partial stream collections that reached the selection
//! engine.
//!
//! Several elements post `GstStreamCollection` onto this bus: urisourcebin,
//! decodebin3, and every parsebin inside decodebin3. Only decodebin3's is the
//! merged collection, and it is the only one whose stream ids a
//! `SELECT_STREAMS` sent to decodebin3 may name.
//!
//! With a per-stream source every input pad gets its own parsebin, so
//! single-stream partials arrive interleaved with the merged ones and the
//! collection appears to shrink. The engine then reconciles against a
//! collection with no video in it, reads the empty video slot as "video off",
//! the no-text-without-video rule strips the subtitle too, and the composed
//! event deselects both in the middle of a load that asked for neither.
//!
//! decodebin3's own collection grows as its inputs report and never shrinks
//! mid-load. That is the property asserted here.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("CUE{index:02}"))
        })
        .collect()
}

fn gate() -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused: false,
        seekable: false,
    }
}

#[test]
fn the_advertised_collection_never_shrinks_during_a_load() {
    init();
    let media = ScenarioBuilder::new("collectiongrowth")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(40, gst::ClockTime::from_mseconds(250)))
        // Long enough that nothing drains while the assertions run. A
        // collection MAY legitimately shrink at an item's end.
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let collections: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = collections.clone();

    let playbin = FcastPlaybin::new(Sinks {
        video: Some(FTestSink::new().upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");
    let (tx, events) = std::sync::mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        if let PlaybinEvent::StreamCollection(collection) = &event {
            let ids: Vec<String> = collection
                .iter()
                .filter_map(|stream| stream.stream_id().map(|id| id.to_string()))
                .collect();
            recorder.lock().expect("collection log").push(ids);
        }
        let _ = tx.send(event);
    });

    playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let mut loaded = false;
    while !loaded {
        assert!(Instant::now() < deadline, "the load never finished");
        playbin.poll_text_policy();
        playbin.pump_selection(gate());
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error during the load: {error}");
            }
            loaded |= matches!(event, PlaybinEvent::Loaded { .. });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    playbin.play().expect("play");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let mut playing = false;
    while !playing {
        assert!(
            Instant::now() < deadline,
            "the pipeline never settled PLAYING"
        );
        playbin.poll_text_policy();
        playbin.pump_selection(gate());
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error while starting: {error}");
            }
            playing |= matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let seen = collections.lock().expect("collection log").clone();
    assert!(
        !seen.is_empty(),
        "no stream collection ever reached the caller"
    );
    let mut union: Vec<String> = Vec::new();
    for (index, ids) in seen.iter().enumerate() {
        let missing: Vec<&String> = union.iter().filter(|id| !ids.contains(id)).collect();
        assert!(
            missing.is_empty(),
            "collection #{index} lost {missing:?}, so it is a partial one from urisourcebin or a \
             parsebin rather than decodebin3's merged collection. Full sequence: {seen:#?}"
        );
        for id in ids {
            if !union.contains(id) {
                union.push(id.clone());
            }
        }
    }
    // The item's three streams all made it, so the sequence really did reach
    // the merged collection rather than stopping at a partial one.
    assert_eq!(
        union.len(),
        3,
        "expected the item's video, audio and text; got {union:?}"
    );

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    playbin.shutdown_async(Box::new(move || {
        let _ = done_tx.send(());
    }));
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match done_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the shutdown never finished");
                playbin.pump_selection(gate());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the worker died during shutdown")
            }
        }
    }
    media.unregister();
}
