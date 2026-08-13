//! `FCAST_NO_PAUSED_CUE_LOOKAHEAD` removes the paused gap tolerance, putting
//! the text engine back on an exact schedule.
//!
//! The tolerance lets a frame frozen in a short inter-cue gap show the
//! upcoming cue instead of a blank. Showing a cue early is a deliberate
//! semantic choice, so it is reversible via one environment variable. This
//! binary proves the reversal is total, not partial. The gap goes back to
//! blank and the exact schedule keeps working, so the lever is not an off
//! switch for paused rendering.
//!
//! Own test binary because `cue::paused_cue_lookahead()` is read once per
//! process. The environment must be set before anything reads it.

use std::time::{Duration, Instant};

use fcast_video::cue::{CueEngine, CueInput, TextFormat};

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Every test enters through this `Once` before touching the crate, so
        // the `LazyLock` behind `paused_cue_lookahead()` is not yet forced.
        //
        // SAFETY: `Once` serializes the test threads here and none has yet
        // spawned anything that reads the environment.
        unsafe {
            std::env::set_var("FCAST_NO_PAUSED_CUE_LOOKAHEAD", "1");
        }
    });
}

fn ms(value: u64) -> gst::ClockTime {
    gst::ClockTime::from_mseconds(value)
}

fn cue(text: &str, start: u64, duration: u64) -> CueInput {
    CueInput {
        format: TextFormat::Utf8,
        text: text.to_owned(),
        start_rt: ms(start),
        end_rt: Some(ms(start + duration)),
    }
}

fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// A frame frozen shortly before the next cue stays blank under the lever.
/// 20.930 sits in the gap between a cue ending at 20.900 and one starting at
/// 20.970.
#[test]
fn under_the_lever_a_frozen_frame_in_a_gap_stays_blank() {
    init();
    let engine = CueEngine::new();
    engine.set_canvas(1280, 720);
    engine.submit(cue("Before", 20_000, 900));
    engine.submit(cue("After", 20_970, 2_000));

    engine.overlays_for(Some(ms(20_930)));
    // The assertion is an absence. A raster not yet landed would look the same
    // as the policy, so give the worker time to be wrong first.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        engine.current_overlays().is_empty(),
        "the paused gap tolerance survived the lever that exists to remove it"
    );

    // The cue still arrives at its own start. The lever removed the tolerance,
    // not the cue.
    engine.overlays_for(Some(ms(20_970)));
    wait_for("the cue to arrive at its own start", || {
        !engine.current_overlays().is_empty()
    });
}

/// A cue that genuinely covers the frozen frame still reaches the screen with
/// no frame flowing. If this failed, the lever would be disabling the paused
/// path itself rather than just the tolerance.
#[test]
fn under_the_lever_a_covering_cue_still_reaches_a_frozen_frame() {
    init();
    let engine = CueEngine::new();
    engine.set_canvas(1280, 720);

    engine.overlays_for(Some(ms(4_100)));
    assert!(engine.current_overlays().is_empty());

    engine.submit(cue("Incoming", 4_000, 500));
    wait_for("the covering cue to reach the frozen frame", || {
        !engine.current_overlays().is_empty()
    });
}
