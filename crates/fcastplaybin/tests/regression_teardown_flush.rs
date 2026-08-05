//! Regression for the teardown deadlock between the flush pair and an input
//! that is still pushing.
//!
//! Found by the campaign-5 buffering fuzzer (seeds 100014, 200030, 300046,
//! 200057) and captured with gdb on `wedge_s200030_r1.bt`. The cycle:
//!
//! * the teardown caller sits in the input-NULL loop of `Inner::drop`, inside
//!   `gst_uri_source_bin_change_state` -> `activate_pads` ->
//!   `activate_mode_internal`, waiting for a pad STREAM LOCK,
//! * the input's text source task holds that lock, blocked in
//!   `gst_multi_queue_sink_event` -> `gst_data_queue_push` on decodebin3's
//!   FULL multiqueue,
//! * that multiqueue's src task is parked in `gst_base_sink_wait_preroll`,
//!   so the queue cannot drain while the pipeline sits below PLAYING.
//!
//! The window is the flush PAIR. `flush_pads` sends FLUSH_START and then
//! immediately FLUSH_STOP; the stop re-arms the pad, and an
//! as-fast-as-possible source refills the multiqueue slot and blocks on a
//! serialized event again BEFORE the loop reaches its `set_state(Null)`.
//!
//! # STATUS: the bug is OPEN and this test does NOT yet reproduce it
//!
//! Read a pass here as "nothing regressed", never as "the wedge is fixed".
//!
//! The obvious fix, sending FLUSH_START only and leaving the pads flushing,
//! was implemented and MEASURED TO BE WRONG. `db3_sink_pads` covers every
//! stream of the input rather than just its text, so the unmatched
//! FLUSH_START reaches audio and video, and `teardown` also runs at READY for
//! a stop-and-reload that reuses the pipeline afterwards. On `fuzz_scenarios`
//! seeds 500001, 500002 and 500010 the pair passes and FLUSH_START-only
//! fails, on `flush_pairs_matched: flush-start never matched by a
//! flush-stop`. Any real fix has to keep the flush pairing intact. See the
//! note on `Inner::flush_parked_text_pushes`.
//!
//! What this test currently is: a scenario-level check that a teardown
//! returns while a text branch is live and the pipeline is parked below
//! PLAYING. It sets up every ingredient the backtrace names, and it still
//! completes in about a second, which means the source is NOT sitting inside
//! a blocked push when the teardown starts. The missing ingredient is a
//! multiqueue actually full enough to block the source; the text backlog here
//! evidently fits. Approaches already tried and found not to reproduce:
//!
//! * replaying the four campaign wedge seeds (that evidence was void, the
//!   drivers are `#[ignore]`d and were being run without `--ignored`),
//! * waiting for the overlay's `subtitle_sink` to be LINKED, which is not the
//!   same as flowing and made the test finish in 0.96s,
//! * waiting for real text buffers before parking, which fixed the above and
//!   still did not fill the queue,
//! * a sleeping probe on `subtitle_sink` to manufacture the held stream lock,
//!   which never fires because nothing pushes during the teardown window.
//!
//! The bound is deliberately far below any plausible honest teardown. A
//! teardown that has not returned in `TEARDOWN_BOUND` is not slow, it is
//! wedged on a lock nothing will release.
//!
//! Nothing here may hang the harness, so the drop runs on its own thread with
//! the assertion on the main one, and a wedge exits the process hard rather
//! than leaving a stuck thread behind for the rest of the suite.

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

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// An honest teardown of this pipeline is milliseconds. The gdb-captured
/// wedge never returns at all, so anything in between is still a failure.
///
/// This sits WELL BELOW `HOLD_THE_STREAM_LOCK` on purpose. A bound above the
/// manufactured hold would let a teardown that blocks for the entire hold
/// still pass, which is the trap `teardown_races.rs` paid for once already.
const TEARDOWN_BOUND: Duration = Duration::from_secs(20);

/// How long the manufactured obstacle keeps the text branch's stream lock. A
/// teardown that waits on this lock at all waits longer than `TEARDOWN_BOUND`
/// and is reported, while one that never touches it returns in milliseconds.
const HOLD_THE_STREAM_LOCK: Duration = Duration::from_secs(60);

/// Buffering that never completes within the test's lifetime, so the sink
/// stays in `gst_base_sink_wait_preroll` and decodebin3's multiqueue cannot
/// drain.
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
fn dropping_the_playbin_while_an_input_pushes_into_a_stalled_queue_returns() {
    init();

    // Realtime main item whose buffering dips and never comes back. The sink
    // parks in its preroll wait and the pipeline sits below PLAYING, which is
    // what stops decodebin3's multiqueue from draining.
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

    // As fast as possible against a realtime item, so the text branch runs
    // far ahead and keeps a genuine backlog pressed against the multiqueue.
    // A leisurely source would drain before the teardown and close the very
    // window under test.
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

    // The handler must not capture the playbin. This test's whole point is
    // that the drop below is the LAST strong reference, so `Inner::drop` runs
    // on that thread.
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

    // The branch must be FLOWING, not merely linked. Waiting for the pad to
    // be linked is what made the first version of this test finish in under
    // a second with the teardown having nothing to block on. Text has to be
    // moving before the pipeline parks, or there is no push to catch mid
    // flight and no backlog to press against the multiqueue.
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
    let flowed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = flowed.clone();
    let counting = overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting text into the overlay");
    {
        let flowed = flowed.clone();
        wait_for(
            "text to actually flow into the overlay",
            Box::new(move || flowed.load(std::sync::atomic::Ordering::SeqCst) >= 5),
        );
    }
    overlay_subtitle.remove_probe(counting);

    // Park the pipeline below PLAYING. The playbin does NOT do this itself on
    // buffering; that belongs to the receiver's state machine (see
    // `receiver-core/src/player.rs`), which is why the scenario tests drive
    // it through `Receiver`. A parked sink sits in `gst_base_sink_wait_
    // preroll` and stops decodebin3's multiqueue draining, which is the
    // precondition the whole wedge rests on.
    playbin.pause().expect("park below PLAYING as the receiver does");
    wait_for(
        "the pipeline to park below PLAYING",
        Box::new(|| {
            let (_, current, _) = playbin.pipeline().state(gst::ClockTime::ZERO);
            current != gst::State::Playing
        }),
    );

    // Let the AFAP source press a real backlog against the stalled queue.
    thread::sleep(Duration::from_millis(750));

    // MANUFACTURE the obstacle rather than wait for it, exactly as
    // `teardown_races.rs` does. The fuzzer reaches this window in about 1.5%
    // of units, because it needs the source to refill and re-block inside the
    // few milliseconds between the flush pair's FLUSH_STOP and the caller's
    // `set_state(Null)`. A probe that sleeps in the chain function holds that
    // pad's stream lock for the same reason a genuinely stuck source does,
    // and it does so every run instead of once in sixty.
    //
    // This is what separates the arms, and it separates them for the reason
    // the bug is about rather than by construction. `gst_pad_chain_data_
    // unchecked` tests GST_PAD_FLUSHING BEFORE it runs probes, so:
    //
    //   * flush pair: FLUSH_STOP clears flushing, the next buffer enters the
    //     chain, this probe sleeps holding the stream lock, and the teardown's
    //     pad deactivation waits behind it,
    //   * FLUSH_START only: the pad stays flushing, the push returns FLUSHING
    //     without ever reaching the probe, and no lock is held.
    let overlay_subtitle = playbin
        .pipeline()
        .by_name("fpb-suboverlay")
        .and_then(|overlay| overlay.static_pad("subtitle_sink"))
        .expect("the overlay's subtitle_sink");
    overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            thread::sleep(HOLD_THE_STREAM_LOCK);
            gst::PadProbeReturn::Ok
        })
        .expect("holding the text branch's stream lock");

    // Drop on its own thread. This is the teardown the backtrace caught, and
    // it must return rather than block on a stream lock the stalled input
    // will never release.
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
            // state, so unwinding would hang the harness instead of
            // reporting. Exit hard, the way the resurrect reproducer does.
            eprintln!(
                "FAILED: the teardown did not return within {TEARDOWN_BOUND:?}. The input NULL \
                 loop is blocked on a pad stream lock held by a push into decodebin3's stalled \
                 multiqueue, which is the flush-pair window this test pins."
            );
            process::exit(101);
        }
    }
}
