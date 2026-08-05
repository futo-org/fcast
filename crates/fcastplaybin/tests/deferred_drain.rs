//! Regressions for the DRAIN side of the postponed text-branch work.
//!
//! The crate postpones blocking text-branch work (branch disposals, input
//! removals, replays) at moments the pipeline cannot complete it. Five field
//! deadlocks in one day were all one defect, an unbounded blocking pipeline
//! operation run from a thread that must stay responsive, at a moment the
//! pipeline cannot complete it. The postponement is the dodge, and these
//! tests pin the structural properties that make the dodge safe:
//!
//! 1. EVERY kind of postponed work drains once the pipeline can carry it
//!    out. The drain's old idle check tested only the work slot and the
//!    disposal list, so a pending input removal (or replay) with nothing
//!    else pending was skipped by the very drain that owns it, and a
//!    detached-while-paused external input stayed wired into decodebin3
//!    forever.
//!
//! 2. The drain is driven by the CRATE on every pipeline state edge, not
//!    only by the caller's polls. Postponed work used to drain exclusively
//!    through `poll_text_policy`, so a caller that stopped polling (or a
//!    state machine parked waiting for the very work that was postponed,
//!    the `Buffering` wedge of the rapid-paused-switch report) left the
//!    work pending forever.

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
        fcast_gst_elements::fcastaudiostretch::plugin_init().expect("registering fcastaudiostretch");
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
    Arc::new(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    )
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
        wait_for_with(playbin, events, "the external subtitle to materialize", move || {
            !probe.subtitle_stream_ids(id).is_empty()
        });
    }
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    {
        let probe = playbin.clone();
        wait_for_with(playbin, events, "the subtitle branch to link", move || {
            probe
                .pipeline()
                .by_name("fpb-suboverlay")
                .and_then(|overlay| overlay.static_pad("subtitle_sink"))
                .is_some_and(|pad| pad.is_linked())
        });
    }
    let overlay_subtitle = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("subtitle_sink"))
        .expect("the overlay's subtitle_sink");
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
        wait_for_with(playbin, events, "text to flow into the overlay", move || {
            seen.load(Ordering::SeqCst) >= 2
        });
    }
    id
}

fn settle_paused(playbin: &Arc<FcastPlaybin>, events: &mpsc::Receiver<PlaybinEvent>) {
    playbin.pause().expect("pause");
    let probe = playbin.clone();
    wait_for_with(playbin, events, "the pipeline to settle at PAUSED", move || {
        let (_, current, pending) = probe.pipeline().state(gst::ClockTime::ZERO);
        current == gst::State::Paused && pending == gst::State::VoidPending
    });
}

/// How long a stop may take. A healthy teardown runs in milliseconds, and
/// the wedge this bounds never returns at all.
const STOP_BOUND: Duration = Duration::from_secs(15);

/// How long the manufactured obstacle holds the text branch unless a flush
/// releases it. Far above [`STOP_BOUND`], so a wedged stop cannot sneak
/// under the bound by waiting the obstacle out.
const TEXT_BRANCH_HELD: Duration = Duration::from_secs(60);

/// Property 3. The teardown boundaries drain the postponed disposals
/// BEFORE flushing the parked text pushes.
///
/// The field sequence pause, subtitles off, stop froze the receiver for
/// good. The paused subtitle-off postpones the branch disposal, and the
/// orphaned queue's pads then exist ONLY in the disposal list, where
/// teardown's parked-push flush cannot see them. That flush pauses a
/// multiqueue task that is blocked mid-push into the orphaned FULL queue,
/// whose own task holds its stream lock parked inside textoverlay, so the
/// stop waits forever on work that only the drain it skipped could have
/// released. Captured with gdb on the worker thread. And since both ends
/// of the branch are already unlinked by then, the ONLY path left to the
/// parked cue push is the overlay's own subtitle_sink pad, which is where
/// the drain's flush must go.
///
/// The obstacle is MANUFACTURED, for regression_paused_switch's reason.
/// Against the patched playback the natural pause-plus-off winds the
/// branch down cleanly (verified with gdb, every task idle), so a test
/// waiting for the real thing goes green on the broken build. A probe
/// sleeping in the overlay's subtitle_sink chain path recreates the
/// captured geometry on demand. It releases the moment a FLUSH_START
/// reaches that pad, so the one legitimate way out (flushing the overlay
/// pad) works, and nothing else does.
#[test]
fn stopping_after_a_paused_subtitle_off_does_not_wedge() {
    init();
    let media = ScenarioBuilder::new("drainstopmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    // AS FAST AS POSSIBLE against a realtime item, so the text queue fills
    // (dead air counts against its time bound) and the multiqueue's push
    // into it parks while the branch is held.
    let subs = ScenarioBuilder::new("drainstopsubs")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "C"))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let playbin = build_playbin();
    let events = load_and_play(&playbin, &media.uri());
    let _id = attach_and_flow(&playbin, &events, &subs.uri());

    let overlay_subtitle = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("subtitle_sink"))
        .expect("the overlay's subtitle_sink");

    // The release valve. A FLUSH_START reaching the overlay pad frees the
    // holder below, which is exactly what the drain's flush provides and
    // nothing else does. Flush probes run on the event path, independent
    // of the thread sleeping in the data path.
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let released = released.clone();
        overlay_subtitle
            .add_probe(gst::PadProbeType::EVENT_FLUSH, move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::FlushStart(_))
                {
                    released.store(true, Ordering::SeqCst);
                }
                gst::PadProbeReturn::Ok
            })
            .expect("watching for the releasing flush");
    }

    // The holder. It parks the text branch's task inside the overlay's
    // chain path, holding the subtitle_sink stream lock, which is the
    // captured field geometry.
    let held = Arc::new(AtomicUsize::new(0));
    {
        let held = held.clone();
        let released = released.clone();
        overlay_subtitle
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                if held.fetch_add(1, Ordering::SeqCst) == 0 {
                    let deadline = Instant::now() + TEXT_BRANCH_HELD;
                    while !released.load(Ordering::SeqCst) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
                gst::PadProbeReturn::Ok
            })
            .expect("holding the overlay's subtitle_sink");
    }
    {
        let held = held.clone();
        wait_for_with(&playbin, &events, "the holder probe to engage", move || {
            held.load(Ordering::SeqCst) > 0
        });
    }

    // With the branch held, the backlog fills the text queue and parks the
    // multiqueue's push into it, completing the geometry.
    {
        let tqueue = playbin
            .pipeline()
            .children()
            .into_iter()
            .find(|child| {
                child.factory().is_some_and(|f| f.name() == "queue") && child.name() != "fpb-aqueue"
            })
            .expect("the text branch queue");
        let full = move || {
            let time: u64 = tqueue.property("current-level-time");
            let buffers: u32 = tqueue.property("current-level-buffers");
            time >= 1_000_000_000 || buffers >= 200
        };
        wait_for_with(&playbin, &events, "the text queue to fill", full);
        // Give the multiqueue's next push time to actually park in it.
        thread::sleep(Duration::from_millis(500));
    }

    settle_paused(&playbin, &events);

    // Subtitles off at rest in PAUSED, postponing the branch disposal.
    playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    playbin.pump_selection(gate(true));
    while let Ok(event) = events.try_recv() {
        if let PlaybinEvent::Error { error, .. } = &event {
            panic!("pipeline error at the paused subtitle-off: {error}");
        }
    }

    // The stop. On a wedged build the worker never comes back, so the
    // assertion is a bounded wait on its completion callback.
    let (done_tx, done_rx) = mpsc::channel();
    playbin.stop_async();
    playbin.shutdown_async(Box::new(move || {
        let _ = done_tx.send(());
    }));
    match done_rx.recv_timeout(STOP_BOUND) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The worker is wedged in the teardown. Do NOT let the playbin
            // drop, its Drop walks the identical path and would hang the
            // process instead of reporting the failure.
            std::mem::forget(playbin);
            std::mem::forget(overlay_subtitle);
            panic!(
                "stopping after a paused subtitle-off did not finish within {STOP_BOUND:?}: \
                 teardown's parked-push flush wedged on the postponed branch disposal it \
                 cannot see, which is the field freeze"
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
    }

    media.unregister();
    subs.unregister();
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

    // The audio decoupling queue plus the live text branch's queue.
    assert_eq!(
        children_of_factory(playbin.pipeline(), "queue"),
        2,
        "expected the audio queue and the text branch queue"
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
        2,
        "the postponed disposal should leave the detached queue in the pipeline for now"
    );

    // Resume. From here the caller goes silent, with no poll_text_policy
    // and no pump_selection. The pipeline's own Paused-to-Playing edge must
    // drain the postponed disposal.
    playbin.play().expect("resume");
    let deadline = Instant::now() + DRAIN_BOUND;
    loop {
        if children_of_factory(playbin.pipeline(), "queue") == 1 {
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
