//! A focused reproducer for ONE `tests/scenario_files/*.toml`, so a wedge can
//! be iterated on in seconds instead of through the 52-second `toml_scenarios`
//! sweep, and under a `GST_DEBUG` that would be unreadable across 29 files.
//!
//! ```sh
//! cargo test -p fcastplaybin --test empty_text_stream_probe -- --ignored
//! FCAST_PROBE_SCENARIO=huge_collection.toml \
//!   GST_DEBUG=decodebin3:9 GST_DEBUG_FILE=/tmp/db3.log \
//!   cargo test -p fcastplaybin --test empty_text_stream_probe -- --ignored
//! ```
//!
//! IGNORED, not red. Its default subject (`empty_text_stream.toml`) is blocked
//! on an upstream decodebin3 defect with no application-side workaround
//! (`UPSTREAM-GSTREAMER-ISSUES.md` C15: a stream that ends without ever
//! producing data leaves its decodebin3 input blocked, so no multiqueue slot is
//! made for it, so no collection containing it is ever `all_streams_present`,
//! so the output collection never advances past whichever partial one
//! decodebin3 formed first, and every other stream is reported "not selected"
//! for the life of the item). The sweep already carries that as its one red
//! file; this is the instrument, not a second copy of the verdict.

use std::{
    path::Path,
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::toml,
    sink::{FTestSink, Recording},
};
use gst::prelude::*;

/// Long enough that a slow box is not the reason, short enough to iterate on.
const BOUND: Duration = Duration::from_secs(12);

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

#[test]
#[ignore = "instrument, not a verdict: run explicitly with --ignored. Its default \
            scenario is blocked on UPSTREAM-GSTREAMER-ISSUES.md C15 and the \
            toml_scenarios sweep already carries that red."]
fn one_scenario_reaches_playing() {
    init();
    // A bare name resolves against `scenario_files/`; anything with a separator
    // is taken as a path, so a VARIANT of a scenario can be probed without
    // putting it where the sweep would find it (every document in that
    // directory has to pass, so a scratch copy there is not an option).
    let file = std::env::var("FCAST_PROBE_SCENARIO").unwrap_or("empty_text_stream.toml".into());
    let path = if file.contains(std::path::MAIN_SEPARATOR) {
        Path::new(&file).to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/scenario_files")
            .join(&file)
    };
    let handle = toml::load_file(&path).unwrap_or_else(|err| panic!("{err}"));

    let video_sink = FTestSink::new();
    let video: Recording = video_sink.recording();
    let playbin = FcastPlaybin::new(Sinks {
        video: Some(video_sink.upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        let _ = tx.send(event);
    });
    let log: Mutex<Vec<String>> = Mutex::new(Vec::new());

    playbin.load_async(
        MediaInput::Uri(handle.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );

    let mut played = false;
    let mut playing_requested = false;
    let deadline = Instant::now() + BOUND;
    while Instant::now() < deadline {
        // The receiver's settle points, which is what drives the crate.
        playbin.poll_text_policy();
        playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
        match events.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                // Truncated: a StreamCollection's Debug is a bare pointer and a
                // TagList runs to pages. What matters is the ORDER.
                let line: String = format!("{event:?}").chars().take(150).collect();
                log.lock().expect("log").push(line.replace('\n', " "));
                if matches!(event, PlaybinEvent::Loaded { .. }) && !playing_requested {
                    playing_requested = true;
                    playbin.play().expect("play");
                }
                if matches!(
                    event,
                    PlaybinEvent::StateChanged {
                        current: gst::State::Playing,
                        pending: gst::State::VoidPending,
                        ..
                    }
                ) {
                    played = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // `routed_summary` is the one read that separates "the crate never routed
    // it" from "the crate routed it and the chain never came up".
    let routed = playbin.routed_summary();
    let events: Vec<String> = log.lock().expect("log").clone();
    let _ = playbin.stop();
    handle.unregister();

    assert!(
        played,
        "{file}: never reached settled PLAYING.\n  routed: {routed:?}\n  \
         video buffers: {}\n  events:\n    {}",
        video.buffer_count(),
        events.join("\n    ")
    );
}
