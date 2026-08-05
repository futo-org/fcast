//! Regression for the receiver parking in `Buffering` forever after rapid
//! subtitle track changes while paused.
//!
//! Reported from the field. Roughly fifteen switches over five seconds at a
//! pipeline resting in PAUSED, each logging `postponing the eager text-branch
//! work ... work=Flush`, and then:
//!
//! ```text
//! 65.007  State changed new=Paused pending=VoidPending      <- the last one
//! 66.1..71.4  ~15 x "postponing the eager text-branch work"
//! 72.693  op=SetPlaybackState(Playing)                      <- no effect
//! 75.609  Cannot resume or pause in player current state: Buffering
//! ```
//!
//! Not one deferred item ever drained. The eager flush is what stops a switch
//! queueing behind the outgoing track's backlog, so deferring a run of them
//! leaves those decodebin3 slots undrained, the multiqueue fills, and the
//! pipeline reports buffering and never reaches PLAYING again.
//!
//! THE SHAPE OF THE BUG IS THE POINT: the deferred work drains on reaching
//! PLAYING, and the deferral itself is what prevents the pipeline getting
//! there. Whatever replaces it must not be able to block its own drain
//! condition, so this test asserts the property that matters rather than the
//! mechanism: after rapid paused switching, the pipeline still resumes.
//!
//! HONEST LIMIT, READ THIS BEFORE TRUSTING A GREEN RUN. It does NOT reproduce
//! the field failure. It passes against the build that hangs in the field.
//!
//! The field symptom is the receiver's application state machine parking in
//! `Buffering` and refusing `ResumeOrPause`, and that state is driven by
//! GStreamer buffering messages from the real network source. `ftest://`
//! media has no buffering element and emits none, so the failure mode is out
//! of reach here by construction. The field case also switched between
//! EXTERNAL subtitle inputs, each with its own decodebin3 slot and hold and
//! replay machinery, where this uses embedded tracks on one input.
//!
//! So this is coverage of the user action, not a regression test for the bug.
//! A real reproduction needs a source that reports buffering, which fcasttest
//! would have to grow, plus external inputs rather than embedded tracks.

use std::{
    sync::{Arc, mpsc},
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

/// Matches the field report: about fifteen changes over a few seconds.
const SWITCHES: usize = 15;

/// The field capture spaced them 200 to 400 ms apart.
const BETWEEN_SWITCHES: Duration = Duration::from_millis(120);

/// Resuming is a state change, not a rebuild. If it has not happened in this
/// long the pipeline is not coming back.
const RESUME_BOUND: Duration = Duration::from_secs(20);

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
fn rapid_subtitle_switching_while_paused_still_resumes() {
    init();
    // Four text tracks, so the switches are genuine REPLACEs rather than a
    // re-assertion of the same stream.
    let media = ScenarioBuilder::new("rapidpausedmain")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(100), "A"))
        .text("text_1", cues(200, gst::ClockTime::from_mseconds(100), "B"))
        .text("text_2", cues(200, gst::ClockTime::from_mseconds(100), "C"))
        .text("text_3", cues(200, gst::ClockTime::from_mseconds(100), "D"))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::Realtime)
        .register();

    let playbin = Arc::new(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    );
    let (tx, events) = mpsc::channel();
    let collection = Arc::new(std::sync::Mutex::new(None::<gst::StreamCollection>));
    let sink = collection.clone();
    playbin.set_event_handler(None, move |event, _generation| {
        if let PlaybinEvent::StreamCollection(c) = &event {
            *sink.lock().expect("collection") = Some(c.clone());
        }
        let _ = tx.send(event);
    });

    let drain = || {
        playbin.poll_text_policy();
        playbin.pump_selection(gate(false));
        while events.try_recv().is_ok() {}
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
        // Tracked separately. The Loaded event is consumed once, so folding
        // the collection into the same flag would clear a latch that can
        // never be set again.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut got_loaded = false;
        loop {
            // decodebin3 GROWS its merged collection as each input reports,
            // so the first one to arrive can be video and audio only. Wait
            // for the one that carries all four text tracks.
            let got_collection = collection
                .lock()
                .expect("collection")
                .as_ref()
                .is_some_and(|c| {
                    c.iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::TEXT))
                        .count()
                        == 4
                });
            if got_loaded && got_collection {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the load never finished (loaded={got_loaded} collection={got_collection})"
            );
            playbin.poll_text_policy();
            playbin.pump_selection(gate(false));
            while let Ok(event) = events.try_recv() {
                got_loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    playbin.play().expect("play");

    let text_sids: Vec<String> = collection
        .lock()
        .expect("collection")
        .clone()
        .expect("a collection arrived")
        .iter()
        .filter(|s| s.stream_type().contains(gst::StreamType::TEXT))
        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
        .collect();
    assert_eq!(text_sids.len(), 4, "expected four text tracks: {text_sids:?}");

    // One track live in the overlay, so every switch below is a REPLACE.
    playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some(text_sids[0].clone())),
    );
    {
        let probe = playbin.clone();
        wait_for(
            "the first subtitle track to reach the overlay",
            Box::new(move || {
                probe
                    .pipeline()
                    .by_name("fpb-suboverlay")
                    .and_then(|overlay| overlay.static_pad("subtitle_sink"))
                    .is_some_and(|pad| pad.is_linked())
            }),
        );
    }

    playbin.pause().expect("pause");
    {
        let probe = playbin.clone();
        wait_for(
            "the pipeline to settle at PAUSED",
            Box::new(move || {
                let (_, current, pending) = probe.pipeline().state(gst::ClockTime::ZERO);
                current == gst::State::Paused && pending == gst::State::VoidPending
            }),
        );
    }

    // The run of switches, at rest in PAUSED, exactly as reported.
    for i in 0..SWITCHES {
        let sid = &text_sids[(i + 1) % text_sids.len()];
        playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid.clone())));
        playbin.poll_text_policy();
        playbin.pump_selection(gate(true));
        while events.try_recv().is_ok() {}
        thread::sleep(BETWEEN_SWITCHES);
    }

    // The whole point: the pipeline must still come back.
    playbin.play().expect("resume");
    let started = Instant::now();
    let deadline = started + RESUME_BOUND;
    loop {
        let (_, current, pending) = playbin.pipeline().state(gst::ClockTime::ZERO);
        if current == gst::State::Playing && pending == gst::State::VoidPending {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pipeline never reached PLAYING again after {SWITCHES} paused subtitle switches \
             (stuck at current={current:?} pending={pending:?} after {:?}), which is the field \
             report: every switch postponed its text-branch work, the slots never drained, and \
             the pipeline parked in buffering with its own drain condition out of reach",
            started.elapsed()
        );
        drain();
        thread::sleep(Duration::from_millis(20));
    }

    // And it must actually be running, not merely claiming PLAYING.
    let video = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("video_sink"))
        .expect("the overlay's video_sink");
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = seen.clone();
    video
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting video after the resume");
    {
        let seen = seen.clone();
        wait_for(
            "video to flow again after the resume",
            Box::new(move || seen.load(std::sync::atomic::Ordering::SeqCst) >= 2),
        );
    }

    let (done_tx, done_rx) = mpsc::channel();
    playbin.shutdown_async(Box::new(move || {
        let _ = done_tx.send(());
    }));
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match done_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the shutdown never finished");
                playbin.pump_selection(gate(false));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
        }
    }
    media.unregister();
}
