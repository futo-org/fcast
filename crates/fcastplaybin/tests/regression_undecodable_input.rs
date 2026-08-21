//! Regression for the decodebin3 abort on undecodable input.
//!
//! A sender pointing the receiver at a random URL that serves an HTML page (a
//! parked domain, a captive portal) walks this path: typefind classifies the
//! payload as text/html, no decoder exists, decodebin3's reconfigure failure
//! drops the stream from the requested selection, the drained slot is removed
//! on EOS, and a collection adopted via collection_extends() re-requests the
//! stream with no slot left. handle_stream_switch() then hit upstream's
//! g_assert(FALSE) ("Stream switch requested for future collection") and
//! aborted the whole receiver process. Carried fix:
//! `xtask/patches/decodebin3-tolerate-slotless-requested-stream.patch`.
//!
//! The abort races the input draining, so the load is repeated. Pre-fix a hit
//! kills this test process outright, post-fix every attempt must end in a
//! clean pipeline error (no suitable plugins), never an abort. The content is
//! served from a file, typefind is content-based so the transport does not
//! matter.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::sink::FTestSink;
use gst::prelude::*;

const ATTEMPTS: u32 = 8;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

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

fn gate() -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused: false,
        seekable: false,
    }
}

/// The carried decodebin3 patches travel together, so the auto-select-text
/// property doubles as the marker for a patched build. On an unpatched
/// GStreamer the collection_extends() adoption does not exist, the item
/// strands instead of erroring, and the terminal-event assertion below would
/// fail for reasons this regression is not about.
fn patched_decodebin3() -> bool {
    let Some(db3) = gst::ElementFactory::make("decodebin3").build().ok() else {
        return false;
    };
    db3.has_property("auto-select-text")
}

#[test]
fn an_undecodable_load_errors_instead_of_aborting() {
    init();
    if !patched_decodebin3() {
        eprintln!("skipping: unpatched decodebin3, run under `cargo xtask test`");
        return;
    }

    let page = std::env::temp_dir().join(format!(
        "fcast-undecodable-{}.mkv",
        std::process::id()
    ));
    std::fs::write(
        &page,
        "<!DOCTYPE html><html><head><title>totally a video</title></head>\
         <body><p>parked domain</p></body></html>",
    )
    .expect("writing the fake page");
    let uri = format!("file://{}", page.display());

    let playbin = FcastPlaybin::new(Sinks {
        video: Some(FTestSink::new().upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        let _ = tx.send(event);
    });

    for attempt in 0..ATTEMPTS {
        playbin.load_async(
            MediaInput::Uri(uri.clone()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        // The field crash hit with the pipeline driving to PLAYING, so mirror
        // the receiver: request PLAYING right away rather than waiting for the
        // load to settle. A refused request is fine, the error may win.
        let _ = playbin.play();

        let deadline = Instant::now() + ATTEMPT_TIMEOUT;
        let mut terminal = None;
        while terminal.is_none() {
            assert!(
                Instant::now() < deadline,
                "attempt {attempt}: the load neither errored nor ended, the item stranded"
            );
            playbin.poll_text_policy();
            playbin.pump_selection(gate());
            while let Ok(event) = events.try_recv() {
                match event {
                    PlaybinEvent::Error { error, .. } => {
                        terminal = Some(format!("error: {error}"));
                        break;
                    }
                    PlaybinEvent::EndOfStream => {
                        terminal = Some("end of stream".to_string());
                        break;
                    }
                    // A bogus "loaded" is the field behavior (the collection
                    // holds one text stream); the error must still follow.
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!("attempt {attempt}: {}", terminal.unwrap());
    }

    let (done_tx, done_rx) = mpsc::channel();
    playbin.shutdown_async(Box::new(move || {
        let _ = done_tx.send(());
    }));
    let deadline = Instant::now() + ATTEMPT_TIMEOUT;
    loop {
        match done_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the shutdown never finished");
                playbin.pump_selection(gate());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the worker died during shutdown")
            }
        }
    }
    let _ = std::fs::remove_file(&page);
}
