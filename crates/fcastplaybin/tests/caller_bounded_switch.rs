//! A public operation on the caller's thread returns bounded while the text
//! branch's streaming thread is held inside the renderer.
//!
//! The caller can be made to wait behind the eager park. It runs inline on
//! the deciding thread, and for a live consumer branch the disposal behind it
//! is inline too. That disposal ends in `tqueue.set_state(Null)`, which stops
//! and joins the queue's loop task, so a task parked inside the renderer
//! makes the disposal wait for it.
//!
//! The obstacle is manufactured but real. `text_arm::hold_the_text_tail`
//! installs a subtitle consumer whose callback sleeps. Cues arrive on the
//! branch's streaming thread inside the appsink's `new_sample`, so a sleeping
//! consumer parks exactly the task the disposal has to join, and a slow
//! renderer can produce the same shape in the product.
//!
//! At PLAYING, deliberately. The hold keeps the appsink's preroll lock while
//! it sleeps, so a pipeline that pauses after the hold engages does not
//! settle until the hold expires.
//!
//! Verification: green with no env vars, red with `FCAST_INLINE_DISPATCH=1`,
//! which puts the dispatch (and the park's inline disposal) back on the
//! calling thread.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
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

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// The switch must not wait on the text branch at all, so this sits well
/// below [`TEXT_BRANCH_HELD`]. A bound above the hold would go green on a
/// build that blocks.
const SWITCH_BOUND: Duration = Duration::from_secs(2);

/// How long the renderer holds the text branch.
const TEXT_BRANCH_HELD: Duration = Duration::from_secs(12);

/// Cues that must have reached the renderer before the hold engages, so the
/// branch's task is live inside it rather than idle.
const CUES_BEFORE_HOLD: usize = 2;

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

fn cues(count: u32, step: gst::ClockTime, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{tag}{index:02}"))
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
fn switching_subtitle_tracks_never_blocks_the_caller_behind_the_text_branch() {
    init();
    let media = ScenarioBuilder::new("callerboundmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    // As-fast-as-possible subtitle sources against a realtime item, so the
    // text branch runs ahead and its task is busy in the renderer rather than
    // waiting for the next cue's timestamp.
    let subs_a = ScenarioBuilder::new("callerboundsubsa")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "A"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let subs_b = ScenarioBuilder::new("callerboundsubsb")
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
        playbin.pump_selection(gate());
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
            playbin.pump_selection(gate());
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

    // The obstacle, armed before the track is selected so it cannot miss the
    // burst an unsynced external hands over the instant its branch links. It
    // engages after CUES_BEFORE_HOLD cues and parks the branch's streaming
    // thread for TEXT_BRANCH_HELD, with the pipeline in PLAYING throughout.
    let hold = text_arm::hold_the_text_tail(&playbin, CUES_BEFORE_HOLD, TEXT_BRANCH_HELD);

    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_a));
    wait_for(
        "the renderer to take hold of the text branch",
        Box::new(|| hold.engaged()),
    );

    // The switch, dispatched while the branch is held and the pipeline is
    // PLAYING. On a build that parks inline this blocks for the rest of the
    // hold, so it runs on its own thread and the assertion is on this one.
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_b));
    let (done_tx, done_rx) = mpsc::channel();
    {
        let playbin = playbin.clone();
        thread::Builder::new()
            .name("playing-switch".into())
            .spawn(move || {
                let started = Instant::now();
                playbin.pump_selection(gate());
                let _ = done_tx.send(started.elapsed());
            })
            .expect("spawning the switch thread");
    }
    match done_rx.recv_timeout(SWITCH_BOUND) {
        Ok(waited) => {
            // The hold must still be in effect. A switch that returned
            // promptly because the obstacle had already expired measures
            // nothing.
            assert!(
                hold.holding(),
                "the switch returned in {waited:?}, but the renderer had already released \
                 the branch by then, so nothing was being waited on"
            );
            assert!(
                waited < SWITCH_BOUND,
                "the switch took {waited:?}, so it waited out the held text branch"
            );
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The switch thread is wedged behind the hold. Do not let the
            // playbin drop, or teardown would wait behind the same branch and
            // hang the process instead of reporting the failure.
            std::mem::forget(playbin);
            panic!(
                "switching subtitle tracks did not return within {SWITCH_BOUND:?} while the \
                 text branch was held under PLAYING, so the dispatch ran the eager park, and \
                 the inline disposal behind it, on the caller"
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the switch thread died"),
    }

    // The decider carries the park instead, blocking at most for the hold.
    // Prove the pipeline outlives it. Video flows again once the hold
    // releases, and the playbin still shuts down.
    let video_pad = text_arm::video_tap_pad(&playbin).expect("the video sink's sink pad");
    let video_seen = Arc::new(AtomicUsize::new(0));
    let counter = video_seen.clone();
    video_pad
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
                playbin.pump_selection(gate());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
        }
    }
    media.unregister();
    subs_a.unregister();
    subs_b.unregister();
}
