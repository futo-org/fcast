//! What `wait_quiescent` actually promises.
//!
//! Everything built on the scenario harness sweeps its sinks right after this
//! call, so the exact strength of the guarantee decides whether those sweeps are
//! racing. These tests pin it in both directions: what it does catch, and the
//! shapes it provably does not.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

use fcasttest::{
    scenario::{check_all_named, wait_quiescent},
    sink::{RecordEntry, Recording},
};

fn buffer() -> RecordEntry {
    RecordEntry::synthetic_buffer(gst::ClockTime::ZERO)
}

/// A producer whose gaps are shorter than `settle` is waited out, on every
/// recording and not just the first.
#[test]
fn it_waits_out_a_producer_on_every_recording() {
    fcasttest::register_for_tests();
    let quiet = Recording::new();
    let busy = Recording::new();
    quiet.push(buffer());

    let writer = busy.clone();
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let producer = std::thread::spawn(move || {
        for _ in 0..8 {
            writer.push(buffer());
            std::thread::sleep(Duration::from_millis(5));
        }
        flag.store(true, Ordering::SeqCst);
    });

    // `quiet` is first, so a wait that only watched recordings[0] would return
    // immediately and leave `busy` mid-flight.
    assert!(wait_quiescent(
        &[("quiet", &quiet), ("busy", &busy)],
        Duration::from_millis(60),
        Duration::from_secs(10),
    ));
    assert!(
        done.load(Ordering::SeqCst),
        "returned while the producer was still running"
    );
    assert_eq!(busy.buffer_count(), 8);
    producer.join().expect("producer thread");
}

/// A producer that is still going at `bound` is reported, not waited out
/// forever: the caller gets a false and can name its own failure.
#[test]
fn it_gives_up_at_the_bound_instead_of_hanging() {
    fcasttest::register_for_tests();
    let recording = Recording::new();
    let writer = recording.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let producer = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            writer.push(buffer());
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let started = Instant::now();
    let quiescent = wait_quiescent(
        &[("busy", &recording)],
        Duration::from_millis(20),
        Duration::from_millis(200),
    );
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    producer.join().expect("producer thread");

    assert!(!quiescent, "a running producer was called quiescent");
    assert!(
        elapsed < Duration::from_secs(5),
        "the bound was not honoured: {elapsed:?}"
    );
}

/// THE LIMIT, pinned so nobody mistakes this for a real settle guarantee: a
/// producer whose gaps exceed `settle` is called quiescent while it is still
/// running. `settle` has to be longer than the slowest inter-arrival gap the
/// media can produce, and no caller is checked for that.
#[test]
fn a_producer_slower_than_the_settle_is_called_quiescent() {
    fcasttest::register_for_tests();
    let recording = Recording::new();
    let writer = recording.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let producer = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            writer.push(buffer());
            std::thread::sleep(Duration::from_millis(80));
        }
    });

    let quiescent = wait_quiescent(
        &[("slow", &recording)],
        Duration::from_millis(10),
        Duration::from_secs(2),
    );
    stop.store(true, Ordering::SeqCst);
    producer.join().expect("producer thread");

    assert!(
        quiescent,
        "the settle heuristic changed shape; the doc comment on wait_quiescent \
         has to change with it"
    );
}

/// A sink that never saw anything is "quiescent" and passes every sequence
/// invariant. Both are correct in isolation and together they are a silent pass,
/// so a caller has to assert that data arrived BEFORE it sweeps.
#[test]
fn an_empty_recording_is_quiescent_and_legal() {
    fcasttest::register_for_tests();
    let recording = Recording::new();
    assert!(wait_quiescent(
        &[("never used", &recording)],
        Duration::from_millis(5),
        Duration::from_secs(1),
    ));
    check_all_named(&[("never used", &recording)]).expect("an empty log breaks no rule");
    assert_eq!(recording.buffer_count(), 0);

    // And with no recordings at all there is nothing to be quiescent about.
    assert!(wait_quiescent(&[], Duration::from_millis(5), Duration::ZERO));
    check_all_named(&[]).expect("nothing to check");
}
