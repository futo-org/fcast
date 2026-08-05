//! Timeline anchors have to be anchors.
//!
//! `crates/fcasttest/src/scenario/timeline.rs` opens with "Every anchor is a
//! bounded wait on something the pipeline actually did [...] No wall-clock sleep
//! is ever a correctness wait". These tests hold the module to that, including
//! the case a seek creates: the SAME gate parked on a second time.

use std::{sync::Arc, time::Duration};

use fcasttest::{
    registry::SyncPoint,
    scenario::{ScenarioBuilder, ScenarioHandle},
};

const BOUND: Duration = Duration::from_secs(5);

fn scenario(key: &str) -> ScenarioHandle {
    fcasttest::register_for_tests();
    ScenarioBuilder::new(key).audio("audio_0").register()
}

/// One thread parks on the gate, so the arrival is a fact and not a sleep.
fn park(gate: &Arc<SyncPoint>) -> std::thread::JoinHandle<()> {
    let gate = gate.clone();
    std::thread::spawn(move || gate.wait())
}

/// A stall that is hit once, released, and hit AGAIN (what a flushing seek does
/// to a stalled schedule) must be waitable a second time. `wait_for_arrival`
/// only reports "anything ever arrived", so it returns instantly the second
/// time round and the test races the restarted push.
#[test]
fn a_gate_parked_on_twice_is_waitable_twice() {
    let handle = scenario("anchortwice");
    let gate = handle.sync_point("mid");

    let first = park(&gate);
    assert!(gate.wait_for_arrivals(1, BOUND), "the first park");
    assert_eq!(gate.arrivals(), 1);

    // The one-arrival wait is now satisfied forever, which is why waiting for
    // the SECOND park needs a count.
    assert!(gate.wait_for_arrival(Duration::ZERO));
    assert!(
        !gate.wait_for_arrivals(2, Duration::from_millis(20)),
        "nothing has parked a second time yet"
    );

    gate.release();
    first.join().expect("the first parked push continues");

    // A released gate lets a later arrival straight through, and it still counts.
    let second = park(&gate);
    assert!(
        gate.wait_for_arrivals(2, BOUND),
        "the second park was never observed"
    );
    second.join().expect("the second push");

    handle.unregister();
}

/// The timeline exposes the same anchor, so a test never has to poll `arrivals()`
/// in a sleep loop to find the second park.
#[test]
fn the_timeline_anchors_on_the_nth_arrival() {
    let handle = scenario("anchornth");
    let timeline = handle.timeline().with_timeout(Duration::from_millis(50));
    let gate = handle.sync_point("mid");

    let err = timeline
        .on_sync_point_arrivals("mid", 2)
        .expect_err("nothing has parked at all");
    assert!(err.anchor.contains("2"), "{err}");
    assert!(err.anchor.contains("mid"), "{err}");

    let first = park(&gate);
    timeline
        .on_sync_point_arrival("mid")
        .expect("the first park");
    gate.release();
    first.join().expect("the first push");

    let second = park(&gate);
    handle
        .timeline()
        .with_timeout(BOUND)
        .on_sync_point_arrivals("mid", 2)
        .expect("the second park");
    second.join().expect("the second push");

    handle.unregister();
}

/// An anchor that is never reached fails inside its bound instead of hanging the
/// suite, and says what it was waiting for.
#[test]
fn an_unreached_anchor_fails_inside_its_bound() {
    let handle = scenario("anchormiss");
    let timeline = handle.timeline().with_timeout(Duration::from_millis(30));

    let started = std::time::Instant::now();
    let err = timeline
        .on_sync_point_arrivals("never", 3)
        .expect_err("nothing parks on it");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the bound was not honoured"
    );
    assert!(err.to_string().contains("never"), "{err}");
    assert!(err.to_string().contains("30ms"), "{err}");

    handle.unregister();
}
