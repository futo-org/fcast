//! Teardown and switch races: public operations that must not block their
//! caller behind a streaming thread that cannot proceed.
//!
//! Every deadlock found in this crate so far has one shape. A thread makes an
//! unbounded blocking GStreamer call while something else cannot run, and the
//! call never returns. `send_event(FLUSH_START)` into a `queue` sink pad is
//! the canonical one: `gst_queue_handle_sink_event` forwards the flush and
//! then calls `gst_pad_pause_task` on its src pad, which waits for that pad's
//! stream lock, which the queue's own loop task holds for as long as its push
//! downstream has not returned.
//!
//! The obstacle is MANUFACTURED rather than waited for, exactly as
//! `tests/regression_deadlock.rs` and `tests/regression_paused_switch.rs` do
//! it. A pad probe that sleeps in the chain function holds that pad's stream
//! lock for the same reason a genuinely stuck streaming thread does, and it
//! does so on demand instead of once in a while. A test that passes against a
//! build that deadlocks in the field is worse than no test.
//!
//! Two traps worth repeating, both paid for once already:
//!
//! * `GST_PAD_PROBE_TYPE_EVENT_DOWNSTREAM` does NOT imply `EVENT_FLUSH`.
//!   gstpad.c only runs a probe on a flush event when the probe asked for
//!   that bit.
//! * The assertion bound goes WELL BELOW the manufactured hold. Putting it
//!   above means blocking for the entire hold still passes.
//!
//! Nothing here may hang the harness, so every risky call runs on a spawned
//! thread with the assertion on the main one, and the playbin is never
//! dropped afterwards: `Drop` runs a teardown down the very branch that is
//! held, and would hang the process instead of reporting the result.
//!
//! # STATUS: all five pass, so a failure here is a real regression
//!
//! Three of these were written as reproducers for defects that were open at
//! the time and failed on purpose. All three are fixed and the file is now a
//! live gate throughout. Read a failure as a regression rather than as the
//! documented state.
//!
//! | test | role | the defect it pins |
//! | --- | --- | --- |
//! | `replacing_the_subtitle_track_at_rest_in_paused_returns` | control | the deferral covers it |
//! | `attaching_another_external_subtitle_while_paused_returns` | control | the input side is off the branch |
//! | `turning_subtitles_off_at_rest_in_paused_returns` | was failing | `pump_selection` deferred `Flush` but not `Park`, and now defers both |
//! | `detaching_the_live_external_subtitle_while_paused_returns` | was failing | `Inner::remove_input`'s decodebin3-sink flush reached the branch, and is now gated on a live text branch of THAT input |
//! | `the_worker_survives_an_async_detach_while_paused` | was failing | the same call on the worker, killing every job behind it |
//!
//! The controls also prove the harness is not blind. Running
//! `replacing_the_subtitle_track_at_rest_in_paused_returns` with the crate's
//! own `FCAST_NO_TEXT_WORK_DEFERRAL=1` lever, which is exactly "do the eager
//! flush inline while paused", turns it from 3 passes in 3 into 3 failures in
//! 3, with the same message the subject tests produce.

use std::{
    sync::{
        Arc, mpsc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks,
    StartPoint, TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

/// Generous, for the same reason the other suites are. The whole crate's
/// tests run concurrently.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// How long the manufactured obstacle holds subtitleoverlay's `subtitle_sink`
/// stream lock. Comfortably longer than any healthy operation and comfortably
/// longer than [`OP_BOUND`], so neither side of an assertion is marginal.
const TEXT_BRANCH_HELD: Duration = Duration::from_secs(15);

/// What a public operation is allowed to take while the text branch is held.
/// WELL BELOW [`TEXT_BRANCH_HELD`]: an operation that waits the hold out must
/// fail, and one that stays off the branch finishes in milliseconds.
const OP_BOUND: Duration = Duration::from_secs(3);

/// Text buffers that must have crossed into the overlay before the hold
/// engages, so the text queue's task is live in the overlay rather than idle.
const TEXT_BUFFERS_FIRST: usize = 2;

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

/// A loaded playbin with one external subtitle live in subtitleoverlay, plus
/// however many spare externals the test needs to switch to.
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    /// Kept alive so the installed probes stay installed.
    #[allow(dead_code)]
    overlay_subtitle: gst::Pad,
    externals: Vec<ExternalSubId>,
    /// Kept alive so the `ftest://` media stays registered. Deliberately
    /// never unregistered: the harness is leaked rather than torn down (see
    /// [`must_return`]), and pulling the media out from under a still-running
    /// source would only add noise.
    #[allow(dead_code)]
    scenarios: Vec<ScenarioHandle>,
}

fn drain(playbin: &FcastPlaybin, events: &mpsc::Receiver<PlaybinEvent>, paused: bool) {
    playbin.poll_text_policy();
    playbin.pump_selection(gate(paused));
    while let Ok(event) = events.try_recv() {
        if let PlaybinEvent::Error { error, .. } = &event {
            panic!("pipeline error: {error}");
        }
    }
}

fn wait_for(
    playbin: &FcastPlaybin,
    events: &mpsc::Receiver<PlaybinEvent>,
    what: &str,
    mut done: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        drain(playbin, events, false);
        thread::sleep(Duration::from_millis(10));
    }
}

/// `tag` names the scenarios, so every test owns its own media. `spares` is
/// how many external subtitle inputs to attach BEYOND the one that goes live
/// (a REPLACE needs one to switch to).
fn setup(tag: &str, spares: usize) -> Harness {
    init();
    // Video and audio run in REALTIME while the subtitle sources run as fast
    // as possible. That puts the text branch far ahead of the video clock,
    // which is what makes subtitleoverlay block the text push while it waits
    // for video to reach the cue. That blocked push is what the eager
    // text-branch work deadlocks behind.
    let media = ScenarioBuilder::new(&format!("{tag}main"))
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs: Vec<ScenarioHandle> = (0..=spares)
        .map(|index| {
            ScenarioBuilder::new(&format!("{tag}subs{index}"))
                .text(
                    "text_0",
                    cues(300, gst::ClockTime::from_mseconds(100), &format!("S{index}")),
                )
                .duration(gst::ClockTime::from_seconds(30))
                .pacing(Pacing::AsFastAsPossible)
                .register()
        })
        .collect();

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

    let externals: Vec<ExternalSubId> = subs
        .iter()
        .map(|sub| playbin.attach_subtitle(&sub.uri()).expect("attach"))
        .collect();
    wait_for(
        &playbin,
        &events,
        "the external subtitle streams to materialize",
        || {
            externals
                .iter()
                .all(|id| !playbin.subtitle_stream_ids(*id).is_empty())
        },
    );

    playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::ExternalSubtitle(externals[0]),
    );
    wait_for(
        &playbin,
        &events,
        "the external subtitle to reach the overlay",
        || {
            playbin
                .pipeline()
                .by_name("fpb-suboverlay")
                .and_then(|overlay| overlay.static_pad("subtitle_sink"))
                .is_some_and(|pad| pad.is_linked())
        },
    );
    let overlay_subtitle = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("subtitle_sink"))
        .expect("the overlay's subtitle_sink");

    let mut scenarios = vec![media];
    scenarios.extend(subs);
    Harness {
        playbin,
        events,
        overlay_subtitle,
        externals,
        scenarios,
    }
}

/// Manufacture the obstacle: a probe that sleeps in the chain function of
/// subtitleoverlay's `subtitle_sink`, so the text queue's loop task cannot
/// return from its push and keeps holding `queue:src`'s stream lock. That is
/// precisely the state the field captures show, where the push is instead
/// stuck behind a video sink parked in `gst_base_sink_wait_preroll`.
///
/// Engaged BEFORE any pause, because a paused pipeline pushes no buffers and
/// a chain-function probe needs one to bite. It keeps holding across the
/// pause, which is what puts the branch in the field's state.
fn hold_text_branch(harness: &Harness) {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    harness
        .overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting text buffers into the overlay");
    wait_for(
        &harness.playbin,
        &harness.events,
        "text to start flowing into the overlay",
        || seen.load(Ordering::SeqCst) >= TEXT_BUFFERS_FIRST,
    );

    let held = Arc::new(AtomicUsize::new(0));
    let holder = held.clone();
    harness
        .overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if holder.fetch_add(1, Ordering::SeqCst) == 0 {
                thread::sleep(TEXT_BRANCH_HELD);
            }
            gst::PadProbeReturn::Ok
        })
        .expect("holding the overlay's subtitle_sink");
    wait_for(
        &harness.playbin,
        &harness.events,
        "the subtitle_sink holder probe to engage",
        || held.load(Ordering::SeqCst) > 0,
    );
}

/// Bring the pipeline to rest in PAUSED, which is where the field deadlocks
/// were captured: both sinks parked in `gst_base_sink_wait_preroll` holding
/// their stream locks.
fn settle_paused(harness: &Harness) {
    harness.playbin.pause().expect("pause");
    wait_for(
        &harness.playbin,
        &harness.events,
        "the pipeline to settle at PAUSED",
        || {
            let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Paused && pending == gst::State::VoidPending
        },
    );
}

/// Run `op` on its own thread and assert it returns inside [`OP_BOUND`].
///
/// The harness is leaked either way. On timeout the operation is wedged and
/// dropping the playbin would run a teardown down the held branch and hang
/// the process instead of reporting the failure; on success the hold is still
/// running and the same applies until it expires. Leaking a pipeline for the
/// rest of a test binary is the cheap half of that trade.
fn must_return(harness: Harness, what: &str, op: impl FnOnce(Arc<FcastPlaybin>) + Send + 'static) {
    let (done_tx, done_rx) = mpsc::channel();
    {
        let playbin = harness.playbin.clone();
        thread::Builder::new()
            .name("teardown-race-op".into())
            .spawn(move || {
                let started = Instant::now();
                op(playbin);
                let _ = done_tx.send(started.elapsed());
            })
            .expect("spawning the operation thread");
    }
    let outcome = done_rx.recv_timeout(OP_BOUND);
    std::mem::forget(harness);
    match outcome {
        Ok(waited) => assert!(
            waited < OP_BOUND,
            "{what} took {waited:?}, so it waited out the held text branch instead of \
             staying off it"
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "{what} did not return within {OP_BOUND:?} while the text branch was held for \
             {TEXT_BRANCH_HELD:?}: it is blocking its caller behind a streaming thread that \
             cannot proceed"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the operation thread died"),
    }
}

/// Run `op` (a queued, worker-thread operation) and assert the WORKER is
/// still answering afterwards.
///
/// `debug_graph_async` is the probe: it only walks the graph, so a worker
/// that runs it is a worker that reached the front of its queue. A worker
/// parked in a blocking GStreamer call never gets there, and every queued job
/// behind it (the next load, the shutdown) is dead with it. Nothing on the
/// caller side blocks here, so the operation itself is issued inline.
fn worker_must_answer(harness: Harness, what: &str, op: impl FnOnce(&FcastPlaybin)) {
    op(&harness.playbin);
    let (done_tx, done_rx) = mpsc::channel();
    harness.playbin.debug_graph_async(Box::new(move |_snapshot| {
        let _ = done_tx.send(());
    }));
    let outcome = done_rx.recv_timeout(OP_BOUND);
    std::mem::forget(harness);
    match outcome {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "the worker did not answer a queued job within {OP_BOUND:?} after {what} while \
             the text branch was held for {TEXT_BRANCH_HELD:?}: the worker is parked in a \
             blocking call and every job behind it is dead"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
    }
}

/// CONTROL. The REPLACE half of the eager text-branch work, which
/// `pump_selection` already postpones at a pipeline resting in PAUSED.
///
/// It shares every ingredient with the subject test below: the same media
/// shape, the same manufactured hold, the same bound, the same paused
/// pipeline. Only the requested track differs. It passing while the subject
/// fails is what establishes that the subject's failure is the missing
/// deferral and not the harness.
#[test]
fn replacing_the_subtitle_track_at_rest_in_paused_returns() {
    let harness = setup("tdrepl", 1);
    hold_text_branch(&harness);
    settle_paused(&harness);
    harness.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::ExternalSubtitle(harness.externals[1]),
    );
    must_return(harness, "a subtitle REPLACE while paused", |playbin| {
        playbin.pump_selection(gate(true));
    });
}

/// SUBJECT. The PARK half of the same eager work, which is NOT postponed.
///
/// `pump_selection` composes `DeferredTextWork::Park` when the subtitle slot
/// goes off, and the deferral in front of `run_text_work` only covers
/// `DeferredTextWork::Flush`. So the park runs inline at a pipeline resting
/// in PAUSED, and `park_text_streams` -> `detach_text_parts` opens with
/// `downstream.send_event(FLUSH_START)` on the text queue's sink pad. That is
/// the identical call the deferral exists to keep off a paused pipeline, on
/// the identical pad.
#[test]
fn turning_subtitles_off_at_rest_in_paused_returns() {
    let harness = setup("tdpark", 0);
    hold_text_branch(&harness);
    settle_paused(&harness);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    must_return(harness, "turning subtitles off while paused", |playbin| {
        playbin.pump_selection(gate(true));
    });
}

/// A SECOND, independent call site with the same shape, in `remove_input`.
///
/// `detach_subtitle` runs `Inner::remove_input` inline on the calling thread,
/// and that opens by flushing the input's decodebin3 sink pads so a push
/// parked inside the slot cannot deadlock the NULL behind it. A `FLUSH_START`
/// does not stop at the slot: gstpad forwards it through parsebin, through
/// `gst_multi_queue_sink_event`, out of decodebin3's src pad and straight
/// into the text queue, where `gst_queue_handle_sink_event` pauses the very
/// task that is held. Captured with gdb, the caller's stack runs
/// `detach_subtitle` -> `remove_input` -> `send_event` -> ... ->
/// `gst_multi_queue_sink_event` -> ... -> `gst_queue_handle_sink_event` ->
/// `gst_pad_pause_task`.
///
/// So a caller that never mentions the text branch still waits on it, and no
/// deferral can help: the input is leaving and the flush is what makes that
/// safe.
#[test]
fn detaching_the_live_external_subtitle_while_paused_returns() {
    let harness = setup("tddetach", 0);
    hold_text_branch(&harness);
    settle_paused(&harness);
    let id = harness.externals[0];
    must_return(
        harness,
        "detaching the live external subtitle while paused",
        move |playbin| {
            let _ = playbin.detach_subtitle(id);
        },
    );
}

/// The worker-thread twin of the test above. `detach_subtitle_async` queues
/// `Job::DetachSub`, so the same blocking call runs on the worker, and a
/// worker parked there takes every queued job with it: the next load, the
/// stop, the shutdown barrier.
#[test]
fn the_worker_survives_an_async_detach_while_paused() {
    let harness = setup("tdadetach", 0);
    hold_text_branch(&harness);
    settle_paused(&harness);
    let id = harness.externals[0];
    worker_must_answer(harness, "an async detach", move |playbin| {
        playbin.detach_subtitle_async(id);
    });
}

/// CONTROL for both detach tests. Attaching builds a fresh input and drives
/// its own state, touching nothing downstream of decodebin3, so it must be
/// unaffected by a held text branch. It is here to show that the held branch
/// does not simply block everything, which would make the two tests above
/// vacuous. The URI is the one already attached, on purpose: same media, so
/// the only difference from the detach tests is the direction of the
/// operation.
#[test]
fn attaching_another_external_subtitle_while_paused_returns() {
    let harness = setup("tdattach", 0);
    hold_text_branch(&harness);
    settle_paused(&harness);
    let uri = harness.scenarios[1].uri();
    must_return(
        harness,
        "attaching another external subtitle while paused",
        move |playbin| {
            let _ = playbin.attach_subtitle(&uri);
        },
    );
}
