//! Regression for the deadlock that freezes the whole pipeline when a
//! subtitle track is switched while the pipeline is PAUSED.
//!
//! Reported from the field and captured with gdb on the running receiver.
//! Four threads, and both of the threads that could have resumed playback are
//! among them, which is why nothing recovers:
//!
//! ```text
//! multiqueue1:src  gst_subtitle_overlay_src_proxy_chain -> the video sink
//!                  -> gst_base_sink_wait_preroll     [waits for PLAYING]
//! queue0:src       the text queue pushing sticky events into the overlay
//!                  -> gst_pad_send_event_unchecked   [waits for the overlay]
//! main-async-work  pump_selection -> flush_live_text_branches -> flush_pads
//!                  -> gst_queue_handle_sink_event
//!                  -> gst_pad_send_event_unchecked   [waits for the overlay]
//! fcastplaybin     the worker, in another flush
//!                  -> gst_pad_pause_task             [waits for queue0:src]
//! ```
//!
//! In PAUSED both sinks park in `gst_base_sink_wait_preroll` holding their
//! stream locks. The text queue's task then blocks pushing into
//! subtitleoverlay behind the video path, and the eager flush that
//! `pump_selection` performs before dispatching a REPLACE blocks behind that.
//! `fpb-select` was idle in the capture, so the selection never even reached
//! decodebin3.
//!
//! HOW THIS TEST WORKS, and why it does not try to reproduce the race.
//!
//! Three attempts to provoke the natural race over `ftest://` media all
//! passed against a build that deadlocks in the field: pausing once the
//! branch linked, then with text actually flowing, then with the text source
//! racing ahead of a realtime video clock. In each the flush completed, which
//! means subtitleoverlay never blocked the text push the way it does on real
//! media. A test that passes against the broken build is worse than no test.
//!
//! So the obstacle is MANUFACTURED rather than waited for. What blocks the
//! caller in the capture above is simply that something holds
//! subtitleoverlay's `subtitle_sink` stream lock: `FLUSH_STOP` is a
//! serialized event, so `gst_pad_send_event_unchecked` has to take that lock,
//! which is exactly the frame thread 42 is parked in. A pad probe that sleeps
//! in the chain function holds the same lock for the same reason, and it does
//! so on demand instead of once in a while. `tests/regression_deadlock.rs`
//! pins the sibling routing-lock inversion the same way.
//!
//! So this asserts the invariant the field deadlock violates: a switch must
//! not block the caller behind the text branch, whatever is holding it up.

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

/// A switch must not wait on the text branch at all, so this sits WELL BELOW
/// [`TEXT_BRANCH_HELD`]. Setting it above the hold was an earlier mistake
/// here: blocking for the entire hold then still passed, and the test went
/// green against the broken build.
const SWITCH_BOUND: Duration = Duration::from_secs(2);

/// Text buffers that must have crossed into the overlay before the pause, so
/// the text queue's task is live in the overlay rather than idle.
const TEXT_BUFFERS_BEFORE_PAUSE: usize = 2;

/// How long the probe holds subtitleoverlay's subtitle_sink stream lock.
/// Comfortably longer than a healthy switch and comfortably shorter than
/// [`SWITCH_BOUND`], so neither side of the assertion is marginal.
const TEXT_BRANCH_HELD: Duration = Duration::from_secs(12);

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

/// Dense cues, so the overlay always has a next one to prefetch and block on.
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
fn switching_subtitle_tracks_while_paused_does_not_deadlock_the_pipeline() {
    init();
    // Two EXTERNAL inputs, which is what the field report switched between.
    // The outgoing one owns the text queue whose task the flush has to pause.
    let media = ScenarioBuilder::new("pausedswitchmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    // The subtitle sources run AS FAST AS POSSIBLE while the main item runs
    // in REALTIME. That puts the text branch far ahead of the video clock,
    // which is what makes subtitleoverlay block the text push while it waits
    // for video to reach the cue. That blocked push is the thing the eager
    // flush deadlocks behind, and without it the flush simply completes.
    let subs_a = ScenarioBuilder::new("pausedswitchsubsa")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "A"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let subs_b = ScenarioBuilder::new("pausedswitchsubsb")
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

    let drain = |paused: bool| {
        playbin.poll_text_policy();
        playbin.pump_selection(gate(paused));
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
        }
    };
    let wait_for = |what: &str, paused: bool, mut done: Box<dyn FnMut() -> bool>| {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            drain(paused);
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
        let mut got = false;
        while !got {
            assert!(Instant::now() < deadline, "the load never finished");
            playbin.poll_text_policy();
            playbin.pump_selection(gate(false));
            while let Ok(event) = events.try_recv() {
                if let PlaybinEvent::Error { error, .. } = &event {
                    panic!("pipeline error during the load: {error}");
                }
                got |= matches!(event, PlaybinEvent::Loaded { .. });
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
            false,
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
            false,
            Box::new(move || {
                playbin
                    .pipeline()
                    .by_name("fpb-suboverlay")
                    .and_then(|overlay| overlay.static_pad("subtitle_sink"))
                    .is_some_and(|pad| pad.is_linked())
            }),
        );
    }

    // Count text buffers crossing into the overlay. Waiting for the branch to
    // merely LINK is not enough: the queue's task must be live in the overlay
    // when the pause lands, or the flush never has anything to block behind.
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
            false,
            Box::new(move || text_buffers.load(Ordering::SeqCst) >= TEXT_BUFFERS_BEFORE_PAUSE),
        );
    }

    // Engage the holder BEFORE pausing, because a paused pipeline pushes no
    // buffers and a chain-function probe needs one to bite. It keeps holding
    // across the pause, which is what puts the branch in the state the field
    // capture shows: the pipeline at rest in PAUSED with the text branch
    // stuck.
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
            false,
            Box::new(move || held.load(Ordering::SeqCst) > 0),
        );
    }

    // PAUSE, so the pipeline comes to rest exactly as it had in the field
    // report, with both sinks parked in `wait_preroll`.
    playbin.pause().expect("pause");
    {
        let playbin = playbin.clone();
        wait_for(
            "the pipeline to settle at PAUSED",
            false,
            Box::new(move || {
                let (_, current, pending) = playbin.pipeline().state(gst::ClockTime::ZERO);
                current == gst::State::Paused && pending == gst::State::VoidPending
            }),
        );
    }

    // The switch. On a build with the bug this never returns, so it runs on
    // its own thread and the assertion is on the main one. A plain call here
    // would hang the test binary instead of failing it.
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_b));
    let (done_tx, done_rx) = mpsc::channel();
    {
        let playbin = playbin.clone();
        thread::Builder::new()
            .name("paused-switch".into())
            .spawn(move || {
                let started = Instant::now();
                playbin.pump_selection(gate(true));
                let _ = done_tx.send(started.elapsed());
            })
            .expect("spawning the switch thread");
    }

    match done_rx.recv_timeout(SWITCH_BOUND) {
        Ok(waited) => assert!(
            waited < SWITCH_BOUND,
            "the switch took {waited:?}, so it waited out the held text branch \
             instead of staying off it"
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The switch thread is wedged. Do NOT let the playbin drop:
            // teardown flushes the same branch and would hang the process
            // instead of reporting the failure.
            std::mem::forget(playbin);
            std::mem::forget(overlay_subtitle);
            panic!(
                "switching subtitle tracks did not return within {SWITCH_BOUND:?} while the \
                 text branch was held: pump_selection is blocking the caller behind the eager \
                 text-branch flush, which is the field deadlock"
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the switch thread died"),
    }

    // Resume and confirm the pipeline is genuinely alive afterwards, not just
    // that one call returned.
    playbin.play().expect("play again");
    {
        let playbin = playbin.clone();
        wait_for(
            "the pipeline to reach PLAYING again after the switch",
            false,
            Box::new(move || {
                let (_, current, pending) = playbin.pipeline().state(gst::ClockTime::ZERO);
                current == gst::State::Playing && pending == gst::State::VoidPending
            }),
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
