//! Regression for the DrainTextWork busy loop.
//!
//! Postponed text-branch work is re-attempted on every pipeline state edge
//! AND on every caller poll. The poll half used to re-queue
//! `Job::DrainTextWork` unconditionally whenever anything was pending, so a
//! caller polling every 5ms at a pipeline parked below PLAYING (a Buffering
//! park, a long rest in PAUSED) put one job on the worker per poll,
//! indefinitely, each one early-returning against the same pipeline state.
//! Captured on the fuzz driver's seed 400009 as 5099 consecutive
//! `Got job job=DrainTextWork` lines over a 39-second park with no other
//! crate activity between them.
//!
//! The fix suppresses only the REDUNDANT poll-driven pokes, behind the last
//! drain's recorded no-op verdict. The properties pinned here:
//!
//! 1. A caller polling at a pipeline that cannot drain produces a bounded
//!    number of drain jobs, not one per poll.
//! 2. The suppression never delays the real drain: with the verdict standing
//!    and ZERO further polls, resuming playback alone still drains the
//!    postponed work, off the pipeline's own state edge.
//! 3. Since the poll itself became a worker job, the same caller must not
//!    accumulate THOSE either: every poll is accounted for exactly once, as a
//!    job or as a fold into one already queued, and the jobs never outnumber
//!    the polls. A poll loop is the crate's most frequent caller interaction,
//!    so "the receiver polls faster than the worker decides" has to cost an
//!    atomic swap rather than a queue entry.

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

/// Caller polls issued against the parked pipeline.
const POLL_ROUNDS: u64 = 400;

/// Polls issued back to back, with no sleep between them, to outrun the
/// worker on purpose. Large enough that the coalescing bit is certain to be
/// found already set at least once: the caller's side of a poll is one atomic
/// swap, the worker's side is a pipeline state query and several locks.
const POLL_BURST: u64 = 2000;

/// The most drain jobs those polls may put on the worker. The old behavior
/// queued one per poll (about [`POLL_ROUNDS`]), the suppressed one queues a
/// handful (the first poke, plus one per stray deferral recorded inside the
/// window, plus scheduling slack while the verdict is still in flight).
const POKE_BOUND: u64 = 40;

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

/// Whether the text policy is running inline on the caller (the phase-3
/// step-5 lever), which is the one arm where property 3 says something else.
fn inline_poll() -> bool {
    std::env::var_os("FCAST_INLINE_TEXT_POLL").is_some()
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
    // The consumer arm's cue feed must exist before the first cue can be
    // delivered (see `support/text_arm.rs`).
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

/// Load `uri`, reach PLAYING, and return the event channel.
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
/// selected, linked into the overlay and buffers crossing.
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
    // Counted from the arm's own feed: see `support/text_arm.rs` on why a
    // probe installed here would miss an unsynced branch's delivery burst.
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

/// Property 1 and 2 in one run, in that order.
///
/// Subtitles turned off at a pipeline resting in PAUSED postpone the text
/// branch's disposal, which is exactly the busy-loop shape: work is
/// pending, the pipeline cannot drain it, and the caller keeps polling. The
/// poll storm must not reach the worker (bounded job count), and the
/// verdict standing at the resume must not delay the real drain (the
/// disposal completes off the resume's own state edge, with no polls).
#[test]
fn parked_drain_pokes_are_bounded_and_the_edge_drain_still_runs() {
    init();
    let media = ScenarioBuilder::new("pokemain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("pokesubs")
        .text("text_0", cues(300, gst::ClockTime::from_mseconds(100), "P"))
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

    // Subtitles off at rest in PAUSED. The park runs inline, its blocking
    // half (the branch disposal) is postponed, so deferred work is now
    // pending against a pipeline that cannot drain it.
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
        "the postponed disposal should leave the text queue in the pipeline for now"
    );

    // Property 1. The poll storm. Every poll finds postponed work and a
    // pipeline that cannot drain it. The old behavior queued one drain job
    // per poll (about POLL_ROUNDS of them, this exact loop is the caller
    // side of the field busy loop). The verdict-based suppression admits
    // the first poke and swallows the rest.
    let before = playbin.drain_text_job_count();
    let polls_before = playbin.poll_policy_job_count();
    let folds_before = playbin.poll_policy_coalesced();
    for _ in 0..POLL_ROUNDS {
        playbin.poll_text_policy();
        while let Ok(event) = events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error during the poll storm: {error}");
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let queued = playbin.drain_text_job_count() - before;

    // Property 3, first half. The polls themselves are now worker jobs, and
    // this loop is the caller shape that must not accumulate them.
    let poll_jobs = playbin.poll_policy_job_count() - polls_before;
    let folded = playbin.poll_policy_coalesced() - folds_before;
    assert!(
        poll_jobs <= POLL_ROUNDS,
        "{poll_jobs} text-policy jobs reached the worker for {POLL_ROUNDS} polls: a poll \
         must never produce more than one job"
    );
    if inline_poll() {
        // The lever's arm: the poll runs on the CALLER, so there is nothing
        // to queue and nothing to fold. Asserted rather than skipped, so the
        // arm proves it is the arm it claims to be.
        assert_eq!(
            (poll_jobs, folded),
            (0, 0),
            "FCAST_INLINE_TEXT_POLL is set, so no poll may reach the worker at all"
        );
    } else {
        // Minus one: the counter is bumped by the WORKER, and the last
        // poll's job may still be sitting in the queue. At most one can be,
        // because the coalescing admits exactly one outstanding poll.
        assert!(
            poll_jobs + folded >= POLL_ROUNDS - 1,
            "{poll_jobs} jobs + {folded} folds account for fewer than {POLL_ROUNDS} polls: \
             a poll that is neither queued nor folded is a lost edge, and the text branch \
             it would have linked waits for a settle point that may never come"
        );
    }
    assert!(
        queued >= 1,
        "no drain job reached the worker at all during {POLL_ROUNDS} polls: the poke \
         suppression swallowed the FIRST poke too, so newly postponed work would rely \
         entirely on a state edge that may never come"
    );
    assert!(
        queued <= POKE_BOUND,
        "{queued} drain jobs reached the worker during {POLL_ROUNDS} polls at a pipeline \
         that cannot drain (expected at most {POKE_BOUND}): the per-poll re-poke is back, \
         which is the DrainTextWork busy loop"
    );

    // Property 3, second half. The polls above were paced at 5ms, which an
    // idle worker keeps up with, so they prove the accounting but not the
    // FOLD. Outrun it deliberately: a caller that asks faster than the
    // decider answers must find its question already pending.
    let polls_before = playbin.poll_policy_job_count();
    let folds_before = playbin.poll_policy_coalesced();
    for _ in 0..POLL_BURST {
        playbin.poll_text_policy();
    }
    let poll_jobs = playbin.poll_policy_job_count() - polls_before;
    let folded = playbin.poll_policy_coalesced() - folds_before;
    if !inline_poll() {
        assert!(
            folded >= 1,
            "{POLL_BURST} back-to-back polls produced {poll_jobs} jobs and folded none of \
             them: the coalescing is not happening, so a polling caller's load reaches the \
             worker one job at a time"
        );
        assert!(
            poll_jobs <= POLL_BURST && poll_jobs + folded >= POLL_BURST - 1,
            "{poll_jobs} jobs + {folded} folds do not account for {POLL_BURST} polls \
             exactly once each (one may legitimately still be queued and unrun)"
        );
    }

    // Property 2. The suppression verdict is standing right now, and the
    // resume must still drain the postponed disposal with NO further
    // caller polls, off the pipeline's own state edge. This is the trap
    // the suppression must not fall into: gating the drain on a condition
    // the postponed work itself blocks.
    playbin.play().expect("resume");
    let deadline = Instant::now() + DRAIN_BOUND;
    loop {
        // The text branch's queue is gone; the audio and video-chain queues stay.
        if children_of_factory(playbin.pipeline(), "queue") == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the postponed branch disposal never drained after the resume: the poke \
             suppression delayed the state-edge drain, not just the redundant polls"
        );
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
