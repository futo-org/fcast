//! TASK 4 reproducer: the teardown wedge where the input NULL loop waits on a
//! pad stream lock held by a source blocked pushing into decodebin3's FULL
//! multiqueue, whose drain is parked in `gst_base_sink_wait_preroll`.
//!
//! `regression_teardown_flush.rs` has every other ingredient the gdb capture
//! names but never reaches the window (its text backlog fits), so this file
//! manufactures the missing one. decodebin3 builds its multiqueue with
//! `max-size-buffers=0` (gstdecodebin3.c:760), so a sparse text slot is limited
//! by BYTES alone and the 2 MB default swallows a few hundred short cues. Fat
//! cue payloads plus a shrunk `max-size-bytes` (applied once the pipeline is
//! parked) close that gap deterministically, without inventing a state
//! GStreamer would not reach on its own.
//!
//! The precondition is ASSERTED, not assumed (assuming it is why the sibling
//! test quietly stopped reproducing): text into decodebin3's sink pad must STOP
//! advancing while that pad is neither flushing nor EOS. A run that cannot
//! reach the window prints NO VERDICT and returns green rather than passing for
//! the wrong reason. The drop runs on its own thread and a wedge exits the
//! process hard, so it never hangs the rest of the suite.

use std::{
    process,
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
    spec::{BufferingDip, BufferingRecovery, BufferingSpec, CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// An honest teardown here is milliseconds and the captured wedge never returns
/// at all, so anything in between is still a failure. Must stay well below any
/// manufactured hold, the trap `teardown_races.rs` paid for once.
const TEARDOWN_BOUND: Duration = Duration::from_secs(20);

/// Buffering that never completes within the test's lifetime, so the sink
/// stays in `gst_base_sink_wait_preroll` and the multiqueue cannot drain.
const NEVER_RECOVERS_MS: u64 = 10 * 60 * 1000;

const LOW_PERCENT: i32 = 12;
const DIP_AT_VIDEO_BUFFER: u64 = 3;

/// 4 KB x 900 cues is ~3.6 MB, past multiqueue's 2 MB default even unshrunk.
const CUE_PAYLOAD_BYTES: usize = 4096;
/// Deliberately more cues than the clip can consume, so the source cannot reach
/// EOS during the test and "stopped advancing" can only mean backpressure.
const CUE_COUNT: u32 = 4000;

/// What `max-size-bytes` is shrunk to once the pipeline is parked.
const TINY_QUEUE_BYTES: u64 = 4096;

/// The text pad's buffer count must be unchanged for this long before the
/// source counts as blocked rather than merely slow.
const STALL_CONFIRM: Duration = Duration::from_millis(400);

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

/// Cues whose payloads are large enough to actually fill a byte-limited slot.
fn fat_cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    let filler = "x".repeat(CUE_PAYLOAD_BYTES);
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(
                start,
                start + step / 2,
                format!("C{index:04}-{filler}"),
            )
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

/// decodebin3's own multiqueue, so its limits can be shrunk.
fn decodebin_multiqueue(playbin: &FcastPlaybin) -> gst::Element {
    let db3 = playbin
        .pipeline()
        .by_name("fpb-decodebin")
        .expect("fpb-decodebin")
        .downcast::<gst::Bin>()
        .expect("decodebin3 is a bin");
    db3.iterate_elements()
        .into_iter()
        .flatten()
        .find(|element| {
            element
                .factory()
                .is_some_and(|factory| factory.name() == "multiqueue")
        })
        .expect("decodebin3's multiqueue")
}

/// The decodebin3 sink pad carrying text, i.e. the one the external subtitle
/// input is linked into.
fn text_sink_pad(playbin: &FcastPlaybin) -> Option<gst::Pad> {
    let db3 = playbin.pipeline().by_name("fpb-decodebin")?;
    db3.sink_pads().into_iter().find(|pad| {
        pad.current_caps()
            .map(|caps| caps.to_string().contains("text/x-raw"))
            .unwrap_or(false)
    })
}

#[test]
fn dropping_the_playbin_while_a_source_is_blocked_on_a_full_multiqueue_returns() {
    init();

    let media = ScenarioBuilder::new("tdfullmqmain")
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
    // ahead and keeps a backlog pressed against the multiqueue.
    let subs = ScenarioBuilder::new("tdfullmqsubs")
        .text("text_0", fat_cues(CUE_COUNT, gst::ClockTime::from_mseconds(50)))
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

    // The handler must not capture the playbin: the drop below has to be the
    // LAST strong reference so `Inner::drop` runs on that thread.
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
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

    // The branch must be FLOWING, not just linked, or there is no push to catch
    // mid flight.
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
    let flowed = Arc::new(AtomicUsize::new(0));
    {
        let counter = flowed.clone();
        let counting = overlay_subtitle
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            })
            .expect("counting text into the overlay");
        let seen = flowed.clone();
        wait_for(
            "text to actually flow into the overlay",
            Box::new(move || seen.load(Ordering::SeqCst) >= 5),
        );
        overlay_subtitle.remove_probe(counting);
    }

    // Count text arriving at decodebin3, which is what has to STALL.
    let text_pad = {
        let probe = playbin.clone();
        let mut found = None;
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while found.is_none() {
            assert!(
                Instant::now() < deadline,
                "decodebin3 never exposed a text sink pad"
            );
            found = text_sink_pad(&probe);
            if found.is_none() {
                playbin.poll_text_policy();
                playbin.pump_selection(gate(false));
                thread::sleep(Duration::from_millis(10));
            }
        }
        found.expect("text sink pad")
    };
    let into_db3 = Arc::new(AtomicUsize::new(0));
    {
        let counter = into_db3.clone();
        text_pad
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            })
            .expect("counting text into decodebin3");
    }

    // Park below PLAYING, as the receiver's state machine does on buffering. A
    // parked sink sits in `gst_base_sink_wait_preroll` and stops the multiqueue
    // draining, which is the precondition the whole wedge rests on.
    playbin.pause().expect("park below PLAYING as the receiver does");
    wait_for(
        "the pipeline to park below PLAYING",
        Box::new(|| {
            let (_, current, _) = playbin.pipeline().state(gst::ClockTime::ZERO);
            current != gst::State::Playing
        }),
    );

    // NOW make "full" reachable. Unlimited buffers plus a 2 MB byte limit is
    // what let the sibling test's backlog fit.
    let multiqueue = decodebin_multiqueue(&playbin);
    multiqueue.set_property("max-size-bytes", TINY_QUEUE_BYTES as u32);
    multiqueue.set_property("max-size-time", 0u64);
    eprintln!(
        "shrank decodebin3's multiqueue to max-size-bytes={TINY_QUEUE_BYTES}, \
         text buffers into decodebin3 so far: {}",
        into_db3.load(Ordering::SeqCst)
    );

    // The text source must actually be BLOCKED before the teardown, or this is
    // the sibling test again. An as-fast-as-possible source whose count stops
    // advancing has been stopped by backpressure.
    let blocked = {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut confirmed = false;
        while !confirmed {
            // NO VERDICT rather than a failure. The claim is conditional ("if
            // the source is blocked, the teardown must return"), so a run that
            // never reaches the window has proven nothing and must say so
            // loudly rather than turn the suite red or falsely pass.
            if Instant::now() >= deadline {
                eprintln!(
                    "NO VERDICT: the as-fast-as-possible text source never stopped \
                     advancing within {EVENT_TIMEOUT:?}, so it is not blocked on the \
                     multiqueue and this run cannot reach the wedge window. Nothing \
                     is asserted. Re-run on a quieter box, or lower TINY_QUEUE_BYTES."
                );
                media.unregister();
                subs.unregister();
                return;
            }
            let before = into_db3.load(Ordering::SeqCst);
            thread::sleep(STALL_CONFIRM);
            let after = into_db3.load(Ordering::SeqCst);
            // A FINISHED source also stops advancing and holds no stream lock,
            // so counting it as blocked would pass for the wrong reason.
            let finished = text_pad.sticky_event::<gst::event::Eos>(0).is_some();
            confirmed = before == after && before > 0 && !finished;
            if !confirmed {
                playbin.poll_text_policy();
                playbin.pump_selection(gate(true));
            }
        }
        into_db3.load(Ordering::SeqCst)
    };
    eprintln!(
        "text source is blocked with {blocked} buffers delivered into decodebin3; \
         tearing down"
    );

    // The teardown the backtrace caught. It must return rather than block on a
    // stream lock the blocked input will never release.
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
            eprintln!(
                "FAILED: the teardown did not return within {TEARDOWN_BOUND:?}. The input \
                 NULL loop is blocked on a pad stream lock held by a push into \
                 decodebin3's full, undrainable multiqueue."
            );
            process::exit(101);
        }
    }
}
