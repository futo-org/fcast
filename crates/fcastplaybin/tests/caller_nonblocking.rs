//! The eager REPLACE flush must never run on the thread that calls
//! `pump_selection`, whatever the pipeline state.
//!
//! `tests/regression_paused_switch.rs` pins the PAUSED case, where the old
//! dispatch postponed the flush and the caller stayed free. This pins every
//! OTHER state. The old dispatch gated on "resting in PAUSED" and ran the
//! flush inline on the caller otherwise, so any moment where the flush
//! could not complete promptly but the gate did not match blocked the
//! caller for the duration. A pipeline held below PLAYING by buffering is
//! the field shape (current PAUSED, pending PLAYING, gate says inline), and
//! a branch held up under PLAYING is the same wedge with a shorter fuse.
//! The gate kept over- or under-matching because the bad moment is not
//! decidable from pipeline state, which is why the fix is structural. The
//! caller only records a coalescing intent, and the worker does the flush.
//!
//! The obstacle is MANUFACTURED, exactly like regression_paused_switch and
//! for the reason its header records. A pad probe sleeping in the overlay's
//! subtitle_sink chain path holds that pad's stream lock on demand, which is
//! what real media only does once in a while. The flush pair blocks behind
//! it (FLUSH_STOP is serialized, and the queue's task pause waits on the
//! very thread parked in the probe), so a dispatch that touches the text
//! branch inline waits out the hold.

use std::{
    sync::{
        Arc, mpsc,
        atomic::{AtomicUsize, Ordering},
    },
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
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// The switch must not wait on the text branch at all, so this sits WELL
/// BELOW [`TEXT_BRANCH_HELD`] (a bound above the hold would go green on a
/// build that blocks, the trap regression_paused_switch's header records).
const SWITCH_BOUND: Duration = Duration::from_secs(2);

/// How long the probe holds the overlay's subtitle_sink stream lock.
const TEXT_BRANCH_HELD: Duration = Duration::from_secs(12);

/// Text buffers that must have crossed into the overlay first, so the
/// branch's task is live in the overlay rather than idle.
const TEXT_BUFFERS_BEFORE_HOLD: usize = 2;

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

fn cues(count: u32, step: gst::ClockTime, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{tag}{index:02}"))
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
fn switching_subtitle_tracks_never_blocks_the_caller_behind_the_text_branch() {
    init();
    let media = ScenarioBuilder::new("callerfreemain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    // AS FAST AS POSSIBLE subtitle sources against a REALTIME item, so the
    // text branch runs ahead and its task is busy in the overlay.
    let subs_a = ScenarioBuilder::new("callerfreesubsa")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "A"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let subs_b = ScenarioBuilder::new("callerfreesubsb")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "B"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let playbin = Arc::new(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    );
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        let _ = tx.send(event);
    });

    let drain = || {
        playbin.poll_text_policy();
        playbin.pump_selection(gate(false));
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
        }
    };
    let wait_for = |what: &str, mut done: Box<dyn FnMut() -> bool>| {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            drain();
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
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut loaded = false;
        while !loaded {
            assert!(Instant::now() < deadline, "the load never finished");
            playbin.poll_text_policy();
            playbin.pump_selection(gate(false));
            while let Ok(event) = events.try_recv() {
                if let PlaybinEvent::Error { error, .. } = &event {
                    panic!("pipeline error during the load: {error}");
                }
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    playbin.play().expect("play");

    let id_a = playbin.attach_subtitle(&subs_a.uri()).expect("attach A");
    let id_b = playbin.attach_subtitle(&subs_b.uri()).expect("attach B");
    {
        let playbin = playbin.clone();
        wait_for(
            "both external subtitle streams to materialize",
            Box::new(move || {
                !playbin.subtitle_stream_ids(id_a).is_empty()
                    && !playbin.subtitle_stream_ids(id_b).is_empty()
            }),
        );
    }

    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_a));
    {
        let playbin = playbin.clone();
        wait_for(
            "the first external subtitle to reach the overlay",
            Box::new(move || {
                playbin
                    .pipeline()
                    .by_name("fpb-suboverlay")
                    .and_then(|overlay| overlay.static_pad("subtitle_sink"))
                    .is_some_and(|pad| pad.is_linked())
            }),
        );
    }

    let overlay_subtitle = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("subtitle_sink"))
        .expect("the overlay's subtitle_sink");
    let text_buffers = Arc::new(AtomicUsize::new(0));
    let counter = text_buffers.clone();
    overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting text buffers into the overlay");
    {
        let text_buffers = text_buffers.clone();
        wait_for(
            "text to start flowing into the overlay",
            Box::new(move || text_buffers.load(Ordering::SeqCst) >= TEXT_BUFFERS_BEFORE_HOLD),
        );
    }

    // The manufactured obstacle. It engages on the next text buffer and
    // holds the subtitle_sink stream lock for TEXT_BRANCH_HELD, with the
    // pipeline staying in PLAYING throughout.
    let held = Arc::new(AtomicUsize::new(0));
    let holder = held.clone();
    overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if holder.fetch_add(1, Ordering::SeqCst) == 0 {
                thread::sleep(TEXT_BRANCH_HELD);
            }
            gst::PadProbeReturn::Ok
        })
        .expect("holding the overlay's subtitle_sink");
    {
        let held = held.clone();
        wait_for(
            "the subtitle_sink holder probe to engage",
            Box::new(move || held.load(Ordering::SeqCst) > 0),
        );
    }

    // The switch, dispatched while the branch is held and the pipeline is
    // PLAYING. On a build that flushes inline this blocks for the rest of
    // the hold, so it runs on its own thread and the assertion is on this
    // one.
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_b));
    let (done_tx, done_rx) = mpsc::channel();
    {
        let playbin = playbin.clone();
        thread::Builder::new()
            .name("playing-switch".into())
            .spawn(move || {
                let started = Instant::now();
                playbin.pump_selection(gate(false));
                let _ = done_tx.send(started.elapsed());
            })
            .expect("spawning the switch thread");
    }
    match done_rx.recv_timeout(SWITCH_BOUND) {
        Ok(waited) => assert!(
            waited < SWITCH_BOUND,
            "the switch took {waited:?}, so it waited out the held text branch"
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The switch thread is wedged behind the hold. Do not let the
            // playbin drop, teardown would wait behind the same branch and
            // hang the process instead of reporting the failure.
            std::mem::forget(playbin);
            std::mem::forget(overlay_subtitle);
            panic!(
                "switching subtitle tracks did not return within {SWITCH_BOUND:?} while the \
                 text branch was held under PLAYING, so pump_selection ran the eager flush \
                 inline on the caller"
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the switch thread died"),
    }

    // The worker carries the flush instead, blocking at most for the hold.
    // Prove the pipeline outlives it. Video still flows once the hold
    // releases, and the playbin still shuts down.
    let video_sink_pad = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("video_sink"))
        .expect("the overlay's video_sink");
    let video_seen = Arc::new(AtomicUsize::new(0));
    let counter = video_seen.clone();
    video_sink_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting video after the switch");
    {
        let video_seen = video_seen.clone();
        wait_for(
            "video to keep flowing after the held switch",
            Box::new(move || video_seen.load(Ordering::SeqCst) >= 2),
        );
    }

    let (stop_tx, stop_rx) = mpsc::channel();
    playbin.shutdown_async(Box::new(move || {
        let _ = stop_tx.send(());
    }));
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match stop_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the shutdown never finished");
                playbin.pump_selection(gate(false));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
        }
    }
    media.unregister();
    subs_a.unregister();
    subs_b.unregister();
}
