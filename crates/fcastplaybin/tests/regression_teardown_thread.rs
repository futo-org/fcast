//! The last strong `Inner` reference must never die on a GStreamer streaming
//! thread.
//!
//! Every internal callback in the crate holds a `Weak` and upgrades for the
//! duration of its work, so any of them is the LAST strong reference whenever
//! the caller drops its final handle inside that window. Dropping it there
//! runs `Inner::drop`, which NULLs the pipeline, on that thread.
//!
//! The captured field shape (`toml_scenarios`, 1 run in 10 ending in signal
//! 11) is the bus SYNC handler:
//!
//! ```text
//! gst_multi_queue_loop -> sticky push into decodebin3's sink
//!   -> gst_decodebin_input_setup_identity -> gst_element_sync_state_with_parent
//!   -> state-changed -> gst_bus_post -> the crate's sync closure
//!   -> Arc<Inner>::drop_slow -> Inner::drop -> set_state(Null)
//! ```
//!
//! The NULL descent then tries to deactivate the pad whose task IS the calling
//! thread, gives up half-way with the pipeline at READY, and the dispose
//! cascade over the still-live elements segfaults.
//!
//! Which callback it is does not matter and must not be assumed: fixing the
//! bus sync handler alone left the rate unchanged, because the terminal
//! reference simply lands on the next callback. `Inner::drop` is where the
//! guarantee lives, so this test only has to get the drop onto SOME thread
//! GStreamer owns.
//!
//! # Manufacturing it instead of waiting for it
//!
//! At 1 run in 42 measured here, a soak proves nothing anyone can afford to
//! run. So the race is MANUFACTURED, the way `regression_deadlock.rs` and
//! `regression_paused_switch.rs` manufacture theirs:
//!
//! * the `MessageHook` (raw first look at every bus message, documented as
//!   running on the posting thread) takes the first message posted from a
//!   thread that is NOT the caller's after an external subtitle is attached to
//!   a PLAYING pipeline. The attach is the provocation: it puts a fresh input
//!   into a running decodebin3, and the input's own source task is what posts.
//!   Observed here as `ftestsrc1`'s `text_0` task, one step upstream of the
//!   multiqueue in the field capture and the same hazard;
//! * it then signals the main thread and SLEEPS, holding its upgraded
//!   reference open for [`HOLD`];
//! * the main thread drops its one and only handle inside that window, so the
//!   hook's upgrade is left holding the terminal reference.
//!
//! Firing on the FIRST such message rather than a named one is deliberate.
//! The defect is not about which streaming thread it is; it is that the
//! teardown runs on one at all.
//!
//! # The verdict
//!
//! A GLib log handler collects the two messages the captured run emitted,
//! "Trying to join task ... from its thread would deadlock" and "Failed to
//! deactivate pad ..., very bad". They are emitted by GStreamer core with
//! `g_warning`/`g_critical`, not by GST_DEBUG, so they arrive here whatever
//! `GST_DEBUG` is set to. Neither can be produced by a teardown that runs on a
//! thread GStreamer does not own, which is what makes them the assertion.
//!
//! The process surviving at all is the second half of the verdict: the failing
//! path segfaults outright about one time in ten.
//!
//! # STATUS
//!
//! Fails 3 of 3 with `FCAST_TEARDOWN_ON_ANY_THREAD=1` (the pre-fix in-place
//! descent) and passes 3 of 3 without it.
//!
//! It does NOT stand for the whole class. The `toml_scenarios` soak the defect
//! was found in stayed at 1 signal-11 in 42 runs with the fix against 2 in 42
//! without, which at that base rate is noise. Some other route to the same
//! symptom is open; see the comment above `impl Drop for Inner` for where the
//! search should start.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// How long the hook holds its upgraded reference open. The main thread has to
/// finish dropping its handle well inside this, and dropping a handle is a
/// refcount decrement, so milliseconds are plenty and this is generous.
const HOLD: Duration = Duration::from_millis(600);

/// How long the teardown gets to finish after the handle is gone. The fixed
/// path hands it to `fpb-teardown`, so the test has to outlive that
/// thread's work rather than the caller's.
const TEARDOWN_GRACE: Duration = Duration::from_secs(6);

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

fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("C{index:02}"))
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
fn dropping_the_last_handle_inside_a_bus_hook_does_not_null_the_pipeline_there() {
    init();

    // Everything GLib core logs, so the teardown's own complaints can be read
    // back. Installed before the pipeline exists; the default handler it
    // replaces only printed, so re-printing keeps a failing run readable.
    let logged: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let sink = Arc::clone(&logged);
        gst::glib::log_set_default_handler(move |domain, level, message| {
            eprintln!("({}) {level:?}: {message}", domain.unwrap_or("?"));
            sink.lock()
                .expect("log sink")
                .push(format!("{}: {message}", domain.unwrap_or("?")));
        });
    }

    // Realtime main item, so the pipeline is still genuinely streaming when
    // the subtitle input joins. A finished item posts nothing from a
    // multiqueue thread and the hook would never fire.
    let media = ScenarioBuilder::new("tdthreadmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::Realtime)
        .register();

    let subs = ScenarioBuilder::new("tdthreadsubs")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(100)))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let mut playbin = Some(
        FcastPlaybin::new(Sinks {
            video: Some(FTestSink::new().upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin"),
    );

    // NOTHING here may capture the handle: the drop below has to be the last
    // strong reference for the race to exist at all.
    let caller = thread::current().id();
    let armed = Arc::new(AtomicBool::new(false));
    let hook_armed = Arc::clone(&armed);
    let (fire_tx, fired) = mpsc::channel::<String>();
    let (tx, events) = mpsc::channel();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let hook_seen = Arc::clone(&seen);
    playbin.as_ref().expect("handle").set_event_handler(
        Some(Box::new(move |msg| {
            if hook_armed.load(Ordering::SeqCst) && thread::current().id() != caller {
                let src = msg
                    .src()
                    .map(|src| src.name().to_string())
                    .unwrap_or_default();
                hook_seen
                    .lock()
                    .expect("seen")
                    .push(format!("{:?} <{src}>", msg.type_()));
                // Exactly one message gets the treatment.
                if hook_armed.swap(false, Ordering::SeqCst) {
                    let _ = fire_tx.send(src);
                    // The caller drops its handle in here.
                    thread::sleep(HOLD);
                }
            }
            false
        })),
        move |event, _generation| {
            let _ = tx.send(event);
        },
    );

    let pump = |playbin: &FcastPlaybin| {
        playbin.poll_text_policy();
        playbin.pump_selection(gate());
    };

    {
        let pb = playbin.as_ref().expect("handle");
        pb.load_async(
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
            pump(pb);
            while let Ok(event) = events.try_recv() {
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
        pb.play().expect("play");

        // Let the graph actually run: the hook needs a multiqueue task alive
        // and pushing for decodebin3's input setup to land on one.
        thread::sleep(Duration::from_millis(300));
        pump(pb);

        armed.store(true, Ordering::SeqCst);
        // The attach is the provocation: decodebin3 builds an identity for the
        // new input and syncs its state from whichever thread reached the
        // setup, which for a running pipeline is a multiqueue streaming one.
        let id = pb.attach_subtitle(&subs.uri()).expect("attach");

        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Ok(name) = fired.try_recv() {
                eprintln!("hook holding the reference open on <{name}>");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "nothing was posted from a streaming thread, so the race was never \
                 manufactured and this run proves nothing. Seen: {:#?}",
                seen.lock().expect("seen")
            );
            pump(pb);
            while events.try_recv().is_ok() {}
            // Keep the input materializing; a stalled attach posts nothing.
            let _ = pb.subtitle_stream_ids(id);
            thread::sleep(Duration::from_millis(5));
        }
    }

    // THE MOMENT UNDER TEST. The hook is asleep holding the only other strong
    // reference, so this decrement leaves it terminal.
    drop(playbin.take());

    // The teardown now runs on whichever thread ends up with it. Give it room
    // to finish and to complain.
    thread::sleep(TEARDOWN_GRACE);

    let logged = logged.lock().expect("log sink").clone();
    let offenders: Vec<&String> = logged
        .iter()
        .filter(|line| {
            line.contains("would deadlock")
                || line.contains("Failed to deactivate pad")
                || line.contains("but it is in PAUSED instead of the NULL state")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "the pipeline was NULLed from a GStreamer streaming thread; the teardown \
         could not join its own task and left elements live for the dispose \
         cascade: {offenders:#?}"
    );
}
