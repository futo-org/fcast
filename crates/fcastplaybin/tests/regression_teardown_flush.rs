//! Regression for a teardown deadlock between the flush pair and an input
//! that is still pushing.
//!
//! The cycle: the teardown caller waits for a pad stream lock in the input
//! NULL loop, the input's source task holds that lock while blocked pushing
//! into a full multiqueue, and the queue cannot drain because the sink is
//! parked in preroll below PLAYING. The window is the flush pair. FLUSH_STOP
//! re-arms the pad, so a fast source can refill and re-block before the loop
//! reaches `set_state(Null)`.
//!
//! # STATUS: the bug is OPEN and this test does NOT yet reproduce it
//!
//! Read a pass here as "nothing regressed", never as "the wedge is fixed".
//!
//! FLUSH_START-only is not a valid fix. The flush covers every stream of the
//! input, and teardown also runs at READY for a stop-and-reload that reuses
//! the pipeline, so the flush pairing must stay intact. See the note on
//! `Inner::flush_parked_text_pushes`.
//!
//! What this test currently is: a scenario-level check that a teardown
//! returns while a text branch is live and the pipeline is parked below
//! PLAYING. The missing ingredient for a true reproduction is a multiqueue
//! full enough to actually block the source.
//!
//! Every probe point goes through `text_arm`. The wedge is not specific to
//! either transport arm. The text branch is only what keeps the input
//! pushing.
//!
//! The bound is deliberately far below any honest teardown. A teardown that
//! has not returned in `TEARDOWN_BOUND` is wedged, not slow.
//!
//! Nothing here may hang the harness, so the drop runs on its own thread
//! with the assertion on the main one, and a wedge exits the process hard.

use std::{
    process,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{BufferingDip, BufferingRecovery, BufferingSpec, CueSpec, Pacing},
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// An honest teardown is milliseconds and the wedge never returns, so
/// anything in between is still a failure.
///
/// Must sit well below `HOLD_THE_STREAM_LOCK`. A bound above the
/// manufactured hold would let a teardown that blocks for the entire hold
/// still pass.
const TEARDOWN_BOUND: Duration = Duration::from_secs(20);

/// How long the manufactured obstacle keeps the text branch's stream lock. A
/// teardown that waits on this lock at all waits longer than `TEARDOWN_BOUND`
/// and is reported, while one that never touches it returns in milliseconds.
const HOLD_THE_STREAM_LOCK: Duration = Duration::from_secs(60);

/// Buffering that never completes within the test's lifetime, so the sink
/// stays parked in preroll and the multiqueue cannot drain.
const NEVER_RECOVERS_MS: u64 = 10 * 60 * 1000;

const LOW_PERCENT: i32 = 12;
const DIP_AT_VIDEO_BUFFER: u64 = 3;

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
            CueSpec::new(start, start + step / 2, format!("C{index:02}"))
        })
        .collect()
}

fn gate(paused: bool) -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused,
        seekable: false,
    }
}

#[test]
fn dropping_the_playbin_while_an_input_pushes_into_a_stalled_queue_returns() {
    init();

    // Realtime main item whose buffering dips and never recovers, so the
    // pipeline sits below PLAYING and the multiqueue cannot drain.
    let media = ScenarioBuilder::new("tdflushmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::Realtime)
        .buffering(BufferingSpec::new(LOW_PERCENT).with_dip(BufferingDip {
            stream: "video_0".to_owned(),
            buffer_index: DIP_AT_VIDEO_BUFFER,
            recovery: BufferingRecovery::AfterMs(NEVER_RECOVERS_MS),
        }))
        .register();

    // As fast as possible against a realtime item, so the text branch keeps
    // a genuine backlog pressed against the multiqueue. A slow source would
    // drain before the teardown and close the window under test.
    let subs = ScenarioBuilder::new("tdflushsubs")
        .text("text_0", cues(900, gst::ClockTime::from_mseconds(50)))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let playbin = Arc::new(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    );

    // Armed before anything flows. An unsynced external can hand its whole
    // file over within milliseconds of linking, and the flow check below
    // would otherwise count an empty window.
    text_arm::arm(&playbin);

    // The handler must not capture the playbin. The drop below must be the
    // last strong reference.
    let buffering: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = buffering.clone();
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        if let PlaybinEvent::Buffering(percent) = &event {
            seen.lock().expect("buffering").push(*percent);
        }
        let _ = tx.send(event);
    });

    let wait_for = |what: &str, mut done: Box<dyn FnMut() -> bool>| {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            playbin.poll_text_policy();
            playbin.pump_selection(gate(false));
            while events.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(10));
        }
    };

    playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    {
        let mut loaded = false;
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !loaded {
            assert!(Instant::now() < deadline, "the load never finished");
            playbin.poll_text_policy();
            playbin.pump_selection(gate(false));
            while let Ok(event) = events.try_recv() {
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    playbin.play().expect("play");

    let id = playbin.attach_subtitle(&subs.uri()).expect("attach");
    wait_for(
        "the external subtitle to materialize",
        Box::new(|| !playbin.subtitle_stream_ids(id).is_empty()),
    );
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));

    // The branch must be flowing, not merely linked. Text has to be moving
    // before the pipeline parks, or there is no push to catch mid flight and
    // no backlog against the multiqueue.
    {
        let probe = playbin.clone();
        wait_for(
            "the subtitle branch to reach the renderer",
            Box::new(move || text_arm::text_branch_linked(&probe)),
        );
    }
    let flowed = text_arm::count_text_arrivals(&playbin);
    wait_for(
        "text to actually flow into the renderer",
        Box::new(|| flowed.count() >= 5),
    );

    // Park the pipeline below PLAYING. The playbin does not do this itself
    // on buffering. That belongs to the receiver's state machine. A parked
    // sink stops the multiqueue draining, the precondition the wedge rests
    // on.
    playbin
        .pause()
        .expect("park below PLAYING as the receiver does");
    wait_for(
        "the pipeline to park below PLAYING",
        Box::new(|| {
            let (_, current, _) = playbin.pipeline().state(gst::ClockTime::ZERO);
            current != gst::State::Playing
        }),
    );

    // Let the AFAP source press a real backlog against the stalled queue.
    thread::sleep(Duration::from_millis(750));

    // Manufacture the obstacle rather than wait for it. A probe that sleeps
    // in the chain function holds the pad's stream lock the same way a
    // genuinely stuck source does, and does so every run. Pads test flushing
    // before running probes, so after FLUSH_STOP the next buffer enters the
    // chain and the sleep holds the lock, while a still-flushing pad never
    // reaches the probe and holds nothing.
    text_arm::live_text_tail_pad(&playbin)
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            thread::sleep(HOLD_THE_STREAM_LOCK);
            gst::PadProbeReturn::Ok
        })
        .expect("holding the text branch's stream lock");

    // Drop on its own thread. The teardown must return rather than block on
    // a stream lock the stalled input will never release.
    let (done_tx, done_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(playbin);
        let _ = done_tx.send(());
    });

    match done_rx.recv_timeout(TEARDOWN_BOUND) {
        Ok(()) => {
            let _ = dropper.join();
            media.unregister();
            subs.unregister();
        }
        Err(_) => {
            // A wedged teardown leaves an unjoinable thread holding pipeline
            // state, so unwinding would hang the harness. Exit hard.
            eprintln!(
                "FAILED: the teardown did not return within {TEARDOWN_BOUND:?}. The input NULL \
                 loop is blocked on a pad stream lock held by a push into decodebin3's stalled \
                 multiqueue, which is the flush-pair window this test pins."
            );
            process::exit(101);
        }
    }
}
