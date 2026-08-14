//! `FCAST_SINGLE_ACTIVE_CUE=1` restores the single-active-cue policy.
//!
//! That policy is latest-start-wins replacement, not a display limit. A cue
//! whose turn comes takes the screen from whatever was showing, and the
//! displaced cue never comes back, so overlapping cues show one line at a
//! time and can go blank early. Both halves are pinned below.
//!
//! Own test binary because `cue::single_active_cues()` is read once per
//! process. The environment must be set before anything reads it.

use std::time::{Duration, Instant};

use fcast_video::{
    cue::{CueEngine, CueInput, TextFormat},
    video::{Overlay, OverlaySpace},
};

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Every test enters through this `Once` before touching the crate, so
        // the `LazyLock` behind `single_active_cues()` is not yet forced.
        //
        // SAFETY: `Once` serializes the test threads here and none has yet
        // spawned anything that reads the environment.
        unsafe {
            std::env::set_var("FCAST_SINGLE_ACTIVE_CUE", "1");
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

fn overlays(engine: &CueEngine, rt: u64) -> Vec<Overlay> {
    engine.overlays_for(Some(ms(rt)));
    engine.current_overlays().into_iter().collect()
}

/// Two cues covering the same frame show one at a time, the later-starting
/// one.
#[test]
fn under_the_lever_the_later_cue_replaces_the_earlier_one() {
    init();
    let engine = CueEngine::new();
    engine.set_canvas(1280, 720);
    // Different text lengths give different raster widths, which is how the
    // test tells which cue is on screen without engine internals.
    engine.submit(cue("Short", 0, 8_000));
    engine.submit(cue("A considerably longer second line", 1_000, 8_000));

    engine.overlays_for(Some(ms(500)));
    wait_for("the first cue to rasterize", || {
        !engine.current_overlays().is_empty()
    });
    let before = overlays(&engine, 500);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].space, OverlaySpace::Window);
    let short_width = before[0].width;

    // The second cue's turn comes while the first is still inside its window.
    engine.overlays_for(Some(ms(2_000)));
    wait_for("the second cue to take the screen", || {
        engine
            .current_overlays()
            .first()
            .is_some_and(|o| o.width != short_width)
    });
    let during = overlays(&engine, 2_000);
    assert_eq!(
        during.len(),
        1,
        "under the lever exactly one cue may be on screen, got {}",
        during.len()
    );
    assert!(
        during[0].width > short_width,
        "the cue on screen is not the later-starting one"
    );
}

/// The displaced cue is gone for good. When the displacing cue ends, the
/// screen is blank even though the first cue's window has time left. This is
/// what makes the lever a replacement policy rather than a display limit.
#[test]
fn under_the_lever_a_displaced_cue_never_comes_back() {
    init();
    let engine = CueEngine::new();
    engine.set_canvas(1280, 720);
    engine.submit(cue("The long-lived line", 0, 8_000));
    engine.submit(cue("The short interruption", 1_000, 1_000));

    engine.overlays_for(Some(ms(500)));
    wait_for("the first cue to rasterize", || {
        !engine.current_overlays().is_empty()
    });

    // The interruption takes the screen...
    engine.overlays_for(Some(ms(1_500)));
    wait_for("the interrupting cue to rasterize", || {
        !overlays(&engine, 1_500).is_empty()
    });
    assert_eq!(overlays(&engine, 1_500).len(), 1);

    // ...and when it ends, nothing is left, though the first cue's window has
    // not expired. With the lever unset this frame carries that cue.
    engine.overlays_for(Some(ms(2_500)));
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        overlays(&engine, 2_500).is_empty(),
        "the displaced cue came back, which is not what the single-active rule did"
    );
}
