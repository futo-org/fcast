//! Real-pipeline tests for `fcastaudiostretch`.
//!
//! `gst_check::Harness` drives the element directly and does not reproduce
//! state changes, flushing seeks or downstream preroll, the conditions under
//! which the rate-change stall appears. These tests build an actual pipeline
//! and perform an actual rate seek.

use std::{
    sync::{
        Arc, Once,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gst::prelude::*;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init().unwrap();
    });
}

/// Count buffers reaching the sink, so "did anything come out after the seek"
/// is directly observable rather than inferred from state.
fn build() -> (gst::Pipeline, Arc<AtomicU64>) {
    let pipeline = gst::parse::launch(
        "audiotestsrc wave=sine freq=200 samplesperbuffer=1024 num-buffers=4000 \
         ! audioconvert ! audioresample ! fcastaudiostretch name=st \
         ! fakesink name=sink sync=false signal-handoffs=true",
    )
    .expect("pipeline should parse")
    .downcast::<gst::Pipeline>()
    .unwrap();

    let count = Arc::new(AtomicU64::new(0));
    let sink = pipeline.by_name("sink").unwrap();
    let c = count.clone();
    sink.connect("handoff", false, move |_| {
        c.fetch_add(1, Ordering::Relaxed);
        None
    });

    (pipeline, count)
}

/// Wait until `f()` holds, or return false after `timeout`.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Regression test for the rate-change stall. After a flushing rate seek at a
/// non-zero position, buffers must keep reaching the sink. A failure here
/// wedges the receiver with its audio sink stuck ASYNC in PAUSED.
#[test]
fn rate_seek_mid_playback_keeps_buffers_flowing() {
    init();

    let (pipeline, count) = build();
    pipeline.set_state(gst::State::Playing).unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || count.load(Ordering::Relaxed)
            > 10),
        "pipeline never started producing at rate 1.0"
    );

    // A rate change as `fcastplaybin::send_rate_seek` issues it: flushing,
    // accurate, to the current position rather than to zero.
    pipeline
        .seek(
            1.5,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::from_seconds(2),
            gst::SeekType::End,
            gst::ClockTime::NONE,
        )
        .expect("rate seek should be accepted");

    count.store(0, Ordering::Relaxed);
    let flowed = wait_until(Duration::from_secs(5), || {
        count.load(Ordering::Relaxed) > 10
    });

    let state = pipeline.state(gst::ClockTime::from_seconds(1));
    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        flowed,
        "no buffers reached the sink in 5s after a 1.5x rate seek \
         (pipeline state {state:?}), this is the rate-change stall"
    );
}

/// The same for a slow-down, which takes the other branch of the splice
/// schedule.
#[test]
fn slow_rate_seek_mid_playback_keeps_buffers_flowing() {
    init();

    let (pipeline, count) = build();
    pipeline.set_state(gst::State::Playing).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || count.load(Ordering::Relaxed)
            > 10),
        "pipeline never started producing at rate 1.0"
    );

    pipeline
        .seek(
            0.5,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::from_seconds(2),
            gst::SeekType::End,
            gst::ClockTime::NONE,
        )
        .expect("rate seek should be accepted");

    count.store(0, Ordering::Relaxed);
    let flowed = wait_until(Duration::from_secs(5), || {
        count.load(Ordering::Relaxed) > 10
    });
    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        flowed,
        "no buffers reached the sink in 5s after a 0.5x rate seek"
    );
}
