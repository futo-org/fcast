//! Regression for the routing-lock / pad-stream-lock order inversion that
//! wedged the whole process.
//!
//! The observed wedge, captured with gdb attached to a hung `scenarios` run:
//!
//! * the worker thread was inside `Job::Stop` -> `teardown` ->
//!   `flush_parked_text_pushes`, holding the routing lock, parked in
//!   `gst_pad_pause_task` waiting for a `multiqueue:src` pad's stream lock,
//! * that `multiqueue:src` task held the stream lock and was parked in
//!   `route_db3_pad`'s pad probe waiting for the routing lock,
//! * the test thread then parked on the routing lock too, in
//!   `pump_selection`, and nothing in the process ever ran again.
//!
//! Reproducing the exact three-thread cycle would need control over which
//! instant decodebin3's multiqueue is mid-push, which is not something a test
//! can pin down. The INVARIANT behind it is, though, and it is the whole
//! defect. The teardown flush must not hold the routing lock, because
//! `send_event` runs the entire downstream event chain inline on the calling
//! thread and any blocking anywhere in that chain becomes routing-lock
//! blocking.
//!
//! So the test blocks the flush deliberately, with a pad probe that sleeps on
//! the `FLUSH_START` the teardown sends, and asserts that a routing-lock
//! caller on another thread is unaffected. Before the fix `pump_selection`
//! waits out the whole probe. After it, it returns immediately.

use std::{
    sync::{
        Arc, Mutex, mpsc,
        atomic::{AtomicBool, Ordering},
    },
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

/// Generous, for the same reason the other suites are generous. The whole
/// crate's tests run concurrently.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// How long the probe pins the teardown flush inside `send_event`.
const FLUSH_BLOCK: Duration = Duration::from_secs(3);

/// A pump that has to wait out the flush is the bug. Well under
/// [`FLUSH_BLOCK`] so a loaded box cannot turn a correct run into a failing
/// one, and well under it in the other direction too so the buggy build
/// cannot sneak in under the bound.
const PUMP_BOUND: Duration = Duration::from_millis(750);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init().expect("registering fcastaudiostretch");
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

/// The pad `Inner::flush_parked_text_pushes` sends its flush pair to for a
/// live text branch, reached the way the crate wires it. `db3_src_pad` links
/// straight to the text queue's sink pad, and that queue feeds the overlay.
fn text_branch_flush_target(playbin: &FcastPlaybin) -> gst::Pad {
    let overlay = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .expect("the overlay is in the pipeline once the branch is live");
    let subtitle_sink = overlay
        .static_pad("subtitle_sink")
        .expect("subtitleoverlay has a subtitle_sink pad");
    let queue_src = subtitle_sink
        .peer()
        .expect("the text branch is linked into the overlay");
    queue_src
        .parent_element()
        .expect("the text queue owns its src pad")
        .static_pad("sink")
        .expect("a queue has a sink pad")
}

/// The invariant. A teardown flush that blocks must not block routing-lock
/// callers.
#[test]
fn the_teardown_flush_does_not_hold_the_routing_lock() {
    init();
    let media = ScenarioBuilder::new("deadlockflush")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(40, gst::ClockTime::from_mseconds(250)))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let video_sink = FTestSink::new();
    let playbin = FcastPlaybin::new(Sinks {
        video: Some(video_sink.upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
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

    // The flush only reaches a text branch that actually joined the overlay.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let linked = playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .and_then(|overlay| overlay.static_pad("subtitle_sink"))
            .is_some_and(|pad| pad.is_linked());
        if linked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the subtitle branch never reached the overlay"
        );
        playbin.poll_text_policy();
        playbin.pump_selection(gate());
        while events.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(10));
    }

    // Pin the teardown flush. The probe fires on the worker thread, inside
    // the `send_event` that `flush_parked_text_pushes` makes.
    let target = text_branch_flush_target(&playbin);
    let (entered_tx, entered_rx) = mpsc::channel();
    let entered_tx = Mutex::new(entered_tx);
    let fired = Arc::new(AtomicBool::new(false));
    let probe_fired = fired.clone();
    // EVENT_FLUSH is not implied by EVENT_DOWNSTREAM. gstpad.c only runs a
    // probe on a flush event when the probe asked for that bit (gstpad.c
    // `probe_hook_marshal`), so a downstream-only mask never fires here.
    let probe_mask = gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH;
    target
        .add_probe(probe_mask, move |_pad, info| {
            let is_flush_start = info
                .event()
                .is_some_and(|event| event.type_() == gst::EventType::FlushStart);
            // Once only. The teardown sends FLUSH_STOP right behind the
            // FLUSH_START, and a second block would just double the test's
            // runtime.
            if is_flush_start && !probe_fired.swap(true, Ordering::SeqCst) {
                let _ = entered_tx.lock().expect("probe channel").send(());
                std::thread::sleep(FLUSH_BLOCK);
            }
            gst::PadProbeReturn::Ok
        })
        .expect("installing the flush probe");

    let (done_tx, done_rx) = mpsc::channel();
    playbin.shutdown_async(Box::new(move || {
        let _ = done_tx.send(());
    }));

    entered_rx
        .recv_timeout(EVENT_TIMEOUT)
        .expect("the teardown never flushed the text branch");

    // The worker is now blocked inside the flush. Whether it is holding the
    // routing lock while it blocks is the entire question.
    let started = Instant::now();
    playbin.pump_selection(gate());
    let waited = started.elapsed();
    assert!(
        waited < PUMP_BOUND,
        "pump_selection waited {waited:?} on a blocked teardown flush, so the \
         flush is holding the routing lock across send_event (bound {PUMP_BOUND:?}, \
         the flush blocks for {FLUSH_BLOCK:?})"
    );

    // Let the shutdown finish so the next test in this process starts clean.
    let deadline = Instant::now() + EVENT_TIMEOUT;
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
    media.unregister();
}
