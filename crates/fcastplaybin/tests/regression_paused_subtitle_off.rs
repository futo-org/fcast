//! Regression for the negotiation error raised when subtitles are turned off
//! while the pipeline is paused and playback is then resumed.
//!
//! Reported from the field:
//!
//! ```text
//! 96.458  Selecting track id=-1 (subtitles off)
//! 96.458  postponing a text branch disposal: the pipeline is at rest in PAUSED
//! 98.102  ResumeOrPause -> SetState Playing
//! 98.103  disposing of a text branch postponed while paused
//! 98.116  WARN subtitleoverlay: Subtitle sink is blocked but we have no
//!         subtitle caps
//! 98.118  WARN Media warning: GStreamer error: negotiation problem.
//! ```
//!
//! `Inner::detach_text_parts` unlinks the branch inline and postpones the
//! flush, so that turning subtitles off at a pipeline resting in PAUSED does
//! not wedge the caller. The flush used to run BEFORE the unlink, and the
//! comment on `detach_text_from_overlay` says that ordering is load-bearing:
//! the flush pair travels through the queue into subtitleoverlay and clears
//! it. Postponing it leaves the overlay blocked on a subtitle sink whose
//! branch has gone, and the resume surfaces that to the caller as a
//! negotiation error.
//!
//! The freeze this replaced was worse, so the fix is to keep the postponement
//! and give the overlay what the flush used to give it, not to revert.

use std::{
    sync::{
        Arc, Mutex, mpsc,
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

/// Long enough for the overlay to raise its complaint, which arrived 15 ms
/// after the resume in the field capture.
const SETTLE_AFTER_RESUME: Duration = Duration::from_secs(3);

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
fn turning_subtitles_off_while_paused_leaves_no_negotiation_error_on_resume() {
    init();
    let media = ScenarioBuilder::new("pausedoffmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("pausedoffsubs")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(100)))
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

    // Every warning and error the crate surfaces, which is what the receiver
    // shows the user.
    let complaints: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = complaints.clone();
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        match &event {
            PlaybinEvent::Warning(text) => sink.lock().expect("complaints").push(text.clone()),
            PlaybinEvent::Error { error, .. } => sink
                .lock()
                .expect("complaints")
                .push(format!("error: {error}")),
            _ => {}
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
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut loaded = false;
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
    {
        let playbin = playbin.clone();
        wait_for(
            "the external subtitle to materialize",
            Box::new(move || !playbin.subtitle_stream_ids(id).is_empty()),
        );
    }
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));

    let overlay_subtitle = {
        let probe = playbin.clone();
        wait_for(
            "the subtitle branch to reach the overlay",
            Box::new(move || {
                probe
                    .pipeline()
                    .by_name("fpb-suboverlay")
                    .and_then(|overlay| overlay.static_pad("subtitle_sink"))
                    .is_some_and(|pad| pad.is_linked())
            }),
        );
        playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .and_then(|overlay| overlay.static_pad("subtitle_sink"))
            .expect("the overlay's subtitle_sink")
    };

    // Text must actually be flowing, so the overlay is mid-render rather than
    // idle when the branch is pulled out from under it.
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting text into the overlay");
    {
        let seen = seen.clone();
        wait_for(
            "text to flow into the overlay",
            Box::new(move || seen.load(Ordering::SeqCst) >= 2),
        );
    }

    playbin.pause().expect("pause");
    {
        let playbin = playbin.clone();
        wait_for(
            "the pipeline to settle at PAUSED",
            Box::new(move || {
                let (_, current, pending) = playbin.pipeline().state(gst::ClockTime::ZERO);
                current == gst::State::Paused && pending == gst::State::VoidPending
            }),
        );
    }

    // Subtitles off, at rest in PAUSED. This is the receiver's `Selecting
    // track id=-1`.
    complaints.lock().expect("complaints").clear();
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    playbin.pump_selection(gate(true));

    // Resume. The postponed disposal runs here.
    playbin.play().expect("resume");
    {
        let playbin = playbin.clone();
        wait_for(
            "the pipeline to reach PLAYING again",
            Box::new(move || {
                let (_, current, pending) = playbin.pipeline().state(gst::ClockTime::ZERO);
                current == gst::State::Playing && pending == gst::State::VoidPending
            }),
        );
    }
    let settle = Instant::now() + SETTLE_AFTER_RESUME;
    while Instant::now() < settle {
        drain();
        thread::sleep(Duration::from_millis(20));
    }

    let raised = complaints.lock().expect("complaints").clone();
    let negotiation: Vec<&String> = raised
        .iter()
        .filter(|text| text.to_lowercase().contains("negotiation"))
        .collect();
    assert!(
        negotiation.is_empty(),
        "turning subtitles off while paused and resuming raised a negotiation problem, so the \
         postponed disposal left subtitleoverlay blocked on a subtitle sink whose branch is \
         gone: {negotiation:?} (all complaints: {raised:?})"
    );

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
    subs.unregister();
}
