//! Regressions for the DRAIN side of the postponed text-branch work.
//!
//! The crate postpones blocking text-branch work (branch disposals, input
//! removals, replays) at moments the pipeline cannot complete it. Five field
//! deadlocks in one day were all one defect, an unbounded blocking pipeline
//! operation run from a thread that must stay responsive, at a moment the
//! pipeline cannot complete it. The postponement is the dodge, and these
//! tests pin the structural properties that make the dodge safe:
//!
//! 1. EVERY kind of postponed work drains once the pipeline can carry it out.
//!    The drain's old idle check tested only the work slot and the disposal
//!    list, so a pending input removal (or replay) with nothing else pending
//!    was skipped by the very drain that owns it, and a detached-while-paused
//!    external input stayed wired into decodebin3 forever.
//!
//! 2. The drain is driven by the CRATE on every pipeline state edge, not only
//!    by the caller's polls. Postponed work used to drain exclusively through
//!    `poll_text_policy`, so a caller that stopped polling (or a state machine
//!    parked waiting for the very work that was postponed, the `Buffering`
//!    wedge of the rapid-paused-switch report) left the work pending forever.
//!
//! A third property lived here until subtitleoverlay was deleted:
//! `stopping_after_a_paused_subtitle_off_does_not_wedge`, which pinned that a
//! teardown drains the postponed disposals BEFORE flushing the parked text
//! pushes. Its obstacle was a probe sleeping in subtitleoverlay's
//! `subtitle_sink` chain path -- a text push parked INSIDE the renderer at
//! PLAYING, which the consumer transport cannot produce by design (an unsynced
//! appsink with `drop=true` and a bounded queue never blocks its producer). It
//! reported NO VERDICT from the step-6 flip onward. The equivalent geometry on
//! this transport is PAUSED, where basesink prerolls and the branch really
//! does park, and it is covered by `sink_subtitles::an_inline_disposal_of_a_
//! parked_paused_branch_does_not_wedge`. The ORDERING it pinned still holds
//! and is still executed: `Teardown::run` drains the disposals before
//! `flush_pads`.

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

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// How long the postponed work gets to drain after the resume. The drain
/// runs on the first state edge, so a healthy build needs milliseconds.
const DRAIN_BOUND: Duration = Duration::from_secs(15);

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

/// Dense cues, so the overlay always has a next one to prefetch.
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

fn build_playbin() -> Arc<FcastPlaybin> {
    let playbin = Arc::new(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    );
    // Before anything can flow: the consumer arm's cue feed has to exist
    // ahead of the first cue (see `support/text_arm.rs`).
    text_arm::arm(&playbin);
    playbin
}

/// How many children of `pipeline` were built from `factory`.
fn children_of_factory(pipeline: &gst::Pipeline, factory: &str) -> usize {
    pipeline
        .children()
        .iter()
        .filter(|child| child.factory().is_some_and(|f| f.name() == factory))
        .count()
}

fn shutdown(playbin: &Arc<FcastPlaybin>) {
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
}

/// Load `uri`, reach PLAYING, and return an event drain closure's channel.
fn load_and_play(playbin: &Arc<FcastPlaybin>, uri: &str) -> mpsc::Receiver<PlaybinEvent> {
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        let _ = tx.send(event);
    });
    playbin.load_async(
        MediaInput::Uri(uri.to_string()),
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
        playbin.pump_selection(gate(false));
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error during the load: {error}");
            }
            loaded |= matches!(event, PlaybinEvent::Loaded { .. });
        }
        thread::sleep(Duration::from_millis(10));
    }
    playbin.play().expect("play");
    events
}

/// Drive the pipeline until `done`, pumping the caller-side hooks.
fn wait_for_with(
    playbin: &Arc<FcastPlaybin>,
    events: &mpsc::Receiver<PlaybinEvent>,
    what: &str,
    mut done: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        playbin.poll_text_policy();
        playbin.pump_selection(gate(false));
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reach a text-flowing steady state, with the external subtitle attached,
/// selected, linked into the overlay and buffers crossing. Returns the
/// subtitle id.
fn attach_and_flow(
    playbin: &Arc<FcastPlaybin>,
    events: &mpsc::Receiver<PlaybinEvent>,
    subs_uri: &str,
) -> fcastplaybin::ExternalSubId {
    let id = playbin.attach_subtitle(subs_uri).expect("attach");
    {
        let probe = playbin.clone();
        wait_for_with(
            playbin,
            events,
            "the external subtitle to materialize",
            move || !probe.subtitle_stream_ids(id).is_empty(),
        );
    }
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    {
        let probe = playbin.clone();
        wait_for_with(playbin, events, "the subtitle branch to link", move || {
            text_arm::text_branch_linked(&probe)
        });
    }
    // Counted from the feed the harness armed at construction, not from a
    // probe installed here: on the consumer arm the branch is unsynced and an
    // external subtitle's cues are all delivered within milliseconds of
    // linking, so a counter installed at this point would watch an empty
    // stream (see `support/text_arm.rs`).
    let arrivals = text_arm::count_text_arrivals(playbin);
    wait_for_with(
        playbin,
        events,
        "text to flow into the renderer",
        move || arrivals.count() >= 2,
    );
    id
}

fn settle_paused(playbin: &Arc<FcastPlaybin>, events: &mpsc::Receiver<PlaybinEvent>) {
    playbin.pause().expect("pause");
    let probe = playbin.clone();
    wait_for_with(
        playbin,
        events,
        "the pipeline to settle at PAUSED",
        move || {
            let (_, current, pending) = probe.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Paused && pending == gst::State::VoidPending
        },
    );
}

/// Property 1. A detach postponed while paused is drained by the drain that
/// owns it, even when it is the ONLY postponed work.
///
/// Detaching a live external subtitle at a pipeline resting in PAUSED
/// rightly postpones the input's removal (running it there wedged the worker
/// in the field). The postponed removal was then invisible to the drain. Its
/// idle check consulted only the work slot and the disposal list, so a
/// pending input removal alone made the drain return before looking at it,
/// and the detached input stayed wired into decodebin3 for the rest of the
/// item, polls or no polls.
#[test]
fn input_removal_deferred_while_paused_drains_after_resume() {
    init();
    let media = ScenarioBuilder::new("drainremovalmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("drainremovalsubs")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "A"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let playbin = build_playbin();
    let events = load_and_play(&playbin, &media.uri());
    let id = attach_and_flow(&playbin, &events, &subs.uri());

    // Two inputs are live, the main item and the external subtitle.
    assert_eq!(
        children_of_factory(playbin.pipeline(), "urisourcebin"),
        2,
        "expected the main input and the external input"
    );

    settle_paused(&playbin, &events);

    // The user detaches the subtitle while paused. The removal is postponed
    // (running it here is the worker wedge), which is fine as long as it
    // actually drains later.
    playbin.detach_subtitle(id).expect("detach");
    assert_eq!(
        children_of_factory(playbin.pipeline(), "urisourcebin"),
        2,
        "the postponed removal should leave the input in the pipeline for now"
    );

    // Resume, and keep the caller's DRAIN poll running, deliberately with
    // no pump_selection. Pumping would dispatch the engine's subtitle-off
    // reaction, whose park can postpone a DISPOSAL mid-transition, and a
    // pending disposal masked this very hole (the old idle check drained
    // removals only when other work happened to be pending too). This test
    // is about the drain skipping work it owns, not about who triggers it.
    playbin.play().expect("resume");
    let deadline = Instant::now() + DRAIN_BOUND;
    loop {
        if children_of_factory(playbin.pipeline(), "urisourcebin") == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the input removal postponed while paused never drained after the resume: the \
             detached external subtitle input is still wired into the pipeline, so the drain \
             skipped the pending removal it owns"
        );
        playbin.poll_text_policy();
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error after the resume: {error}");
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    shutdown(&playbin);
    media.unregister();
    subs.unregister();
}

/// Property 2. Postponed work drains on the pipeline's own state edges, with
/// NO caller polls at all after the resume.
///
/// Turning subtitles off at a pipeline resting in PAUSED postpones the
/// branch disposal. The drain used to run only through `poll_text_policy`,
/// at the caller's discretion, so a caller that stopped polling after
/// resuming left the disposal pending forever. The crate now queues a drain
/// on every pipeline state edge, so resuming is enough on its own.
#[test]
fn branch_disposal_deferred_while_paused_drains_without_caller_polls() {
    init();
    let media = ScenarioBuilder::new("draindisposalmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("draindisposalsubs")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "B"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let playbin = build_playbin();
    let events = load_and_play(&playbin, &media.uri());
    let _id = attach_and_flow(&playbin, &events, &subs.uri());

    // The audio decoupling queue, the video chain's queue, and the live
    // text branch's queue.
    assert_eq!(
        children_of_factory(playbin.pipeline(), "queue"),
        3,
        "expected the audio, video-chain and text branch queues"
    );

    settle_paused(&playbin, &events);

    // Subtitles off at rest in PAUSED. The park detaches the branch inline
    // and rightly postpones the blocking disposal.
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    playbin.pump_selection(gate(true));
    while let Ok(event) = events.try_recv() {
        if let PlaybinEvent::Error { error, .. } = &event {
            panic!("pipeline error at the paused subtitle-off: {error}");
        }
    }
    assert_eq!(
        children_of_factory(playbin.pipeline(), "queue"),
        3,
        "the postponed disposal should leave the detached queue in the pipeline for now"
    );

    // Resume. From here the caller goes silent, with no poll_text_policy
    // and no pump_selection. The pipeline's own Paused-to-Playing edge must
    // drain the postponed disposal.
    playbin.play().expect("resume");
    let deadline = Instant::now() + DRAIN_BOUND;
    loop {
        // The text branch's queue is gone; the audio and video-chain queues stay.
        if children_of_factory(playbin.pipeline(), "queue") == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the branch disposal postponed while paused never drained after the resume: with \
             no caller polls, nothing ever ran the drain, so the postponed work outlived its \
             own drain condition"
        );
        thread::sleep(Duration::from_millis(20));
    }

    shutdown(&playbin);
    media.unregister();
    subs.unregister();
}
