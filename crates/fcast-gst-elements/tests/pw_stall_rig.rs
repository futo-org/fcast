//! Live-daemon rig for `fcastpwaudiosink`'s stall bail-outs.
//!
//! A PipeWire graph cannot be faked, so this drives a real one and is
//! `#[ignore]`d: nothing here runs in a normal `cargo test`. It plays DIGITAL
//! SILENCE, so it stays inaudible even if the session manager links it to a
//! real device.
//!
//! Healthy path, against the running session (asserts that neither the
//! total-block cap nor the cycle watchdog false-fires):
//!
//! ```text
//! FCAST_PW_RIG_SECS=20 cargo test -p fcast-gst-elements \
//!   --test pw_stall_rig -- --ignored --nocapture
//! ```
//!
//! Stall path, in a PRIVATE daemon so the session is untouched. It has no
//! sink at all, so the graph never cycles our stream and the watchdog must
//! fire exactly once, after which the writer parks:
//!
//! ```text
//! PIPEWIRE_CORE=fcast-test pipewire &
//! PIPEWIRE_REMOTE=fcast-test FCAST_PW_RIG_EXPECT=stall FCAST_PW_RIG_SECS=14 \
//!   cargo test -p fcast-gst-elements --test pw_stall_rig -- --ignored --nocapture
//! ```
//!
//! Mid-play fault injection (`FCAST_PW_RIG_CMD` runs at `FCAST_PW_RIG_AT`
//! seconds). Both injections RECOVER. Removing the target node makes the
//! session manager re-link us, and suspending it makes the stream
//! re-negotiate.
//!
//! ```text
//! M=$(pactl load-module module-null-sink sink_name=fcastrig \
//!       sink_properties='priority.session=1 priority.driver=1')
//! PIPEWIRE_NODE=fcastrig FCAST_PW_RIG_CMD="pactl unload-module $M" ...
//! ```

use std::time::{Duration, Instant};

use gst::prelude::*;

fn secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Play silence into the sink, print every bus message with its offset, and
/// optionally run a shell command mid-play (the fault injection).
#[test]
#[ignore = "needs a live PipeWire session"]
fn silence_through_a_live_pipewire_graph() {
    gst::init().unwrap();
    fcast_gst_elements::pwaudiosink::plugin_init().unwrap();
    assert!(
        fcast_gst_elements::pwaudiosink::is_available(),
        "no reachable PipeWire daemon"
    );

    let pipeline = gst::parse::launch(
        "audiotestsrc wave=silence samplesperbuffer=480 \
         ! audio/x-raw,format=F32LE,channels=2,rate=48000 \
         ! fcastpwaudiosink name=sink",
    )
    .expect("pipeline should parse")
    .downcast::<gst::Pipeline>()
    .unwrap();

    let started = Instant::now();
    let _ = pipeline.set_state(gst::State::Playing);

    let run = Duration::from_secs(secs("FCAST_PW_RIG_SECS", 20));
    let inject = std::env::var("FCAST_PW_RIG_CMD")
        .ok()
        .map(|cmd| (Duration::from_secs(secs("FCAST_PW_RIG_AT", 4)), cmd));
    let mut injected = inject.is_none();
    let expect_stall = std::env::var("FCAST_PW_RIG_EXPECT").is_ok_and(|v| v == "stall");
    // `FCAST_PW_RIG_PAUSE=6` pauses at 6s and resumes `FCAST_PW_RIG_PAUSE_FOR`
    // seconds later. The soft-cork paths (pending-transition hand-back,
    // stale-cork reconcile, the stall clocks the cork invalidates) must stay
    // quiet across a legitimate pause of any length.
    let pause_at = std::env::var("FCAST_PW_RIG_PAUSE")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs);
    let pause_for = Duration::from_secs(secs("FCAST_PW_RIG_PAUSE_FOR", 3));
    let mut paused = false;
    let mut resumed = pause_at.is_none();
    let bus = pipeline.bus().unwrap();
    let mut errors = Vec::new();

    while started.elapsed() < run {
        if let Some((at, cmd)) = &inject
            && !injected
            && started.elapsed() >= *at
        {
            injected = true;
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .expect("injection command should run");
            println!(
                "[{:6.2}s] INJECT {cmd} -> {} {}",
                started.elapsed().as_secs_f64(),
                out.status,
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        if let Some(at) = pause_at {
            let now = started.elapsed();
            if !paused && now >= at {
                paused = true;
                println!("[{:6.2}s] PAUSE", now.as_secs_f64());
                let ret = pipeline.set_state(gst::State::Paused);
                println!("[{:6.2}s] paused: {ret:?}", started.elapsed().as_secs_f64());
            } else if paused && !resumed && now >= at + pause_for {
                resumed = true;
                println!("[{:6.2}s] RESUME", now.as_secs_f64());
                let ret = pipeline.set_state(gst::State::Playing);
                println!(
                    "[{:6.2}s] playing: {ret:?}",
                    started.elapsed().as_secs_f64()
                );
            }
        }
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
            let at = started.elapsed().as_secs_f64();
            match msg.view() {
                gst::MessageView::Error(e) => {
                    let text = e.error().to_string();
                    println!(
                        "[{at:6.2}s] ERROR from {:?}: {text}",
                        e.src().map(|s| s.name())
                    );
                    errors.push((at, text));
                }
                gst::MessageView::Warning(w) => {
                    println!(
                        "[{at:6.2}s] WARNING from {:?}: {}",
                        w.src().map(|s| s.name()),
                        w.error()
                    );
                }
                gst::MessageView::StateChanged(s) if s.src() == Some(pipeline.upcast_ref()) => {
                    println!("[{at:6.2}s] pipeline {:?} -> {:?}", s.old(), s.current());
                }
                gst::MessageView::Eos(_) => println!("[{at:6.2}s] EOS"),
                _ => {}
            }
        }
    }

    let position = pipeline.query_position::<gst::ClockTime>();
    println!(
        "[{:6.2}s] done: position {position:?}, {} error(s)",
        started.elapsed().as_secs_f64(),
        errors.len()
    );
    let _ = pipeline.set_state(gst::State::Null);

    if expect_stall {
        // The stall must be reported exactly once. A refused segment is
        // skipped and written straight back, so an unlatched report floods
        // the bus.
        assert_eq!(errors.len(), 1, "expected exactly one stall error");
        assert!(
            errors[0].1.contains("PipeWire playback stalled"),
            "unexpected error text: {}",
            errors[0].1
        );
    } else if inject.is_none() {
        assert!(
            errors.is_empty(),
            "a healthy graph must not bail out: {errors:?}"
        );
        assert!(
            position.is_some_and(|p| p > gst::ClockTime::from_seconds(1)),
            "position should advance"
        );
    }
}
