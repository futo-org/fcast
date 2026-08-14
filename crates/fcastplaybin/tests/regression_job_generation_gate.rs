//! Queued jobs are validated AT EXECUTION against the load/stop that
//! superseded them.
//!
//! The worker queue is strict FIFO with no coalescing, so a job can sit
//! behind a state change, a load or a text-branch disposal for as long as
//! those take. Everything an `_async` call decided at dispatch time can be
//! obsolete by the time the worker gets to it, and the caller has no way to
//! recall a job it already queued. Three of those jobs do real damage when
//! they run late: a superseded `Load` re-wires an item the caller has moved
//! past (and, after a SYNC load or stop, resurrects one that was torn down),
//! a `RecoverClock` drives a stopped pipeline back up uninvited, and an
//! `AttachSub` hangs the previous item's subtitle URL onto the item that
//! replaced it - a cross-item ghost external, which suppresses refresh seeks,
//! refuses gapless prepares and can wedge selection for the rest of the item.
//!
//! The crate stamps every job with a queue epoch at enqueue and compares it
//! once when the worker picks the job up; only a load or a stop moves that
//! epoch, so a mismatch means precisely "an item change happened in between".
//! These three tests are the three shapes that mismatch takes.
//!
//! The interleaving is deterministic, not timed: the tests park the worker
//! inside a `debug_graph_async` whose callback blocks (it runs ON the worker),
//! queue their work into the stalled queue, supersede it, and release. A
//! second `debug_graph_async` is the barrier that says every earlier job has
//! finished. No sleeps decide anything here.
//!
//! # Verification
//!
//! * Green: no env vars.
//! * RED with `FCAST_NO_JOB_GENERATION_GATE=1`, per test:
//!   * `a_load_superseded_by_a_newer_load_never_runs`: both loads run, so the
//!     first `Loaded` after the release carries the SUPERSEDED generation and
//!     `stale_job_drops()` stays 0.
//!   * `an_attach_queued_before_a_load_is_dropped_not_applied`: the attach
//!     executes against the new item, so `ExternalSubtitleFailed` never arrives
//!     (the wait times out) and a TEXT stream joins the collection of an
//!     audio-only item.
//!   * `a_clock_recovery_queued_before_a_stop_does_not_restart_the_pipeline`:
//!     the recovery runs its Paused->Playing cycle against the stopped
//!     pipeline, which climbs back out of NULL - measured as `(Ready, Playing)`
//!     at the barrier against `(Null, VoidPending)` when green.

use std::{
    io::Write,
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Sinks, StartPoint};
use gst::prelude::*;

const BOUND: Duration = Duration::from_secs(20);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// A real MP3 so the item runs through the real
/// urisourcebin/parsebin/decodebin3 topology (`regression_lib.rs`'s recipe).
/// Audio-only on purpose: any TEXT stream in a collection here can only have
/// come from an external subtitle input.
fn make_mp3_file(path: &std::path::Path, seconds: f64) {
    let num_buffers = (seconds * 44100.0 / 1024.0).round() as i32;
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", false)
        .property_from_str("wave", "silence")
        .build()
        .unwrap();
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let enc = gst::ElementFactory::make("lamemp3enc").build().unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, &conv, &enc, &sink]).unwrap();
    gst::Element::link_many([&src, &conv, &enc, &sink]).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(10)) {
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(err) => panic!("mp3 encode failed: {err:?}"),
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

fn temp_file(tag: &str, ext: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-jobgate-{}-{tag}.{ext}",
        std::process::id()
    ))
}

fn uri_of(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

/// A throwaway synced audio sink; `video: None` gives the crate's own fake
/// video sink. Nothing here inspects the output, only the events and the
/// pipeline state.
fn test_sinks() -> Sinks {
    Sinks {
        video: None,
        audio: AudioSink::Factory(Box::new(|| {
            Ok(gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()?)
        })),
    }
}

/// Every event with the generation it was stamped with. The generation is the
/// point of test (a), so the handler must keep it.
type Events = mpsc::Receiver<(PlaybinEvent, u64)>;

fn playbin_with_events() -> (FcastPlaybin, Events) {
    let playbin = FcastPlaybin::new(test_sinks()).expect("building fcastplaybin");
    let (tx, events) = mpsc::channel();
    playbin.set_event_handler(None, move |event, generation| {
        let _ = tx.send((event, generation));
    });
    (playbin, events)
}

/// Pull events until `found` accepts one, recording everything seen on the
/// way (the recording is both the assertion material and the failure
/// message). Every wait in this file terminates at [`BOUND`].
fn pump_until(
    events: &Events,
    seen: &mut Vec<(PlaybinEvent, u64)>,
    what: &str,
    mut found: impl FnMut(&PlaybinEvent, u64) -> bool,
) {
    let deadline = Instant::now() + BOUND;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok((event, generation)) = events.recv_timeout(left) else {
            panic!("timed out waiting for {what}; events seen: {seen:?}");
        };
        let hit = found(&event, generation);
        seen.push((event, generation));
        if hit {
            return;
        }
    }
}

fn drain_events(events: &Events, seen: &mut Vec<(PlaybinEvent, u64)>) {
    while let Ok(item) = events.try_recv() {
        seen.push(item);
    }
}

/// Wait for a load to report itself, and answer with its generation.
fn wait_loaded(events: &Events, seen: &mut Vec<(PlaybinEvent, u64)>) -> u64 {
    let mut loaded = None;
    pump_until(
        events,
        seen,
        "a load to report Loaded",
        |event, generation| match event {
            PlaybinEvent::Loaded { .. } => {
                loaded = Some(generation);
                true
            }
            PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
            _ => false,
        },
    );
    loaded.expect("the pump only returns on a match")
}

/// Park the worker until the returned sender is dropped or used.
///
/// `DumpGraph`'s callback runs ON the worker thread and the snapshot walk is
/// already finished when it is invoked, so the parked worker holds no crate
/// lock: the test thread can keep queueing jobs AND call the synchronous API
/// meanwhile. That is the whole interleaving primitive - it replaces the
/// sleep a "queue something, then supersede it before the worker looks"
/// test would otherwise need.
fn stall_worker(playbin: &FcastPlaybin) -> mpsc::Sender<()> {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    playbin.debug_graph_async(Box::new(move |_snapshot| {
        let _ = parked_tx.send(());
        let _ = release_rx.recv_timeout(BOUND);
    }));
    parked_rx
        .recv_timeout(BOUND)
        .expect("the worker never reached the stall");
    release_tx
}

/// A barrier: when this returns, every job queued before it has run to
/// completion on the worker. Lets the assertions be made once, with no
/// polling and no grace period.
fn drain_worker(playbin: &FcastPlaybin) {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    playbin.debug_graph_async(Box::new(move |_snapshot| {
        let _ = done_tx.send(());
    }));
    done_rx
        .recv_timeout(BOUND)
        .expect("the worker never reached the barrier");
}

/// The stream ids of every TEXT stream in every collection seen so far.
fn text_stream_ids(seen: &[(PlaybinEvent, u64)]) -> Vec<String> {
    let mut ids = Vec::new();
    for (event, _) in seen {
        let PlaybinEvent::StreamCollection(collection) = event else {
            continue;
        };
        ids.extend(
            collection
                .iter()
                .filter(|stream| stream.stream_type().contains(gst::StreamType::TEXT))
                .filter_map(|stream| stream.stream_id().map(|id| id.to_string())),
        );
    }
    ids
}

#[test]
fn a_load_superseded_by_a_newer_load_never_runs() {
    init();

    let item_a = temp_file("a", "mp3");
    let item_b = temp_file("b", "mp3");
    let item_c = temp_file("c", "mp3");
    make_mp3_file(&item_a, 2.0);
    make_mp3_file(&item_b, 2.0);
    make_mp3_file(&item_c, 2.0);

    let (playbin, events) = playbin_with_events();
    let mut seen = Vec::new();

    let start = StartPoint::Seek {
        position: gst::ClockTime::ZERO,
        rate: 1.0,
    };
    playbin.load_async(MediaInput::Uri(uri_of(&item_a)), start);
    wait_loaded(&events, &mut seen);

    // Two loads queued behind a parked worker: the classic "the caller
    // skipped ahead twice while a disposal held the queue".
    let release = stall_worker(&playbin);
    let gen_b = playbin.load_async(MediaInput::Uri(uri_of(&item_b)), start);
    let gen_c = playbin.load_async(MediaInput::Uri(uri_of(&item_c)), start);
    assert_ne!(gen_b, gen_c, "each load allocates its own generation");
    drop(release);

    // FIFO: had B run, its Loaded would have arrived BEFORE C's. So the
    // FIRST Loaded after the release is decisive, with no grace period.
    let mut first_after_release = None;
    pump_until(
        &events,
        &mut seen,
        "the surviving load to report Loaded",
        |event, generation| match event {
            PlaybinEvent::Loaded { .. } => {
                first_after_release = Some(generation);
                true
            }
            PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
            _ => false,
        },
    );
    assert_eq!(
        first_after_release,
        Some(gen_c),
        "the superseded load ran: the first Loaded after the release is not the newest load's"
    );

    drain_worker(&playbin);
    drain_events(&events, &mut seen);
    assert!(
        seen.iter().all(|(_, generation)| *generation != gen_b),
        "the superseded load left a trace: an event carries its generation {gen_b}; \
         events seen: {seen:?}"
    );
    assert!(
        playbin.stale_job_drops() >= 1,
        "nothing was reported dropped, so the load cannot have been gated"
    );

    let _ = playbin.stop();
    for path in [&item_a, &item_b, &item_c] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn an_attach_queued_before_a_load_is_dropped_not_applied() {
    init();

    let item_a = temp_file("attach-a", "mp3");
    let item_b = temp_file("attach-b", "mp3");
    make_mp3_file(&item_a, 2.0);
    make_mp3_file(&item_b, 2.0);
    let subs = temp_file("attach-subs", "srt");
    {
        let mut file = std::fs::File::create(&subs).expect("writing the subtitle fixture");
        file.write_all(b"1\n00:00:00,500 --> 00:00:02,000\nCUE\n\n")
            .expect("writing the subtitle fixture");
    }

    let (playbin, events) = playbin_with_events();
    let mut seen = Vec::new();

    let start = StartPoint::Seek {
        position: gst::ClockTime::ZERO,
        rate: 1.0,
    };
    playbin.load_async(MediaInput::Uri(uri_of(&item_a)), start);
    wait_loaded(&events, &mut seen);

    // The subtitle belongs to item A, and is dispatched while item A is what
    // plays.
    let release = stall_worker(&playbin);
    let id = playbin.allocate_subtitle_id();
    playbin.attach_subtitle_async(id, uri_of(&subs));

    // The item change the FIFO cannot serialize against that attach: a
    // SYNCHRONOUS load, which runs here on the test thread while the worker
    // is parked. Whatever the queue holds was formed for the item this call
    // just replaced.
    playbin
        .load(MediaInput::Uri(uri_of(&item_b)), start)
        .expect("the synchronous load of the replacement item");
    // Item A's pipeline is gone by the time the synchronous load returns, so
    // everything the channel holds at this instant is A's and everything after
    // it is B's. The index is what lets the ghost-stream check below wait for
    // B's OWN collection instead of accepting whatever happens to be in `seen`.
    drain_events(&events, &mut seen);
    let after_load_b = seen.len();
    drop(release);

    // The drop is LOUD: this is the outcome event the caller already handles
    // for an attach, so nothing is left waiting on a subtitle that will never
    // materialize.
    pump_until(
        &events,
        &mut seen,
        "the superseded attach to report ExternalSubtitleFailed",
        |event, _| matches!(event, PlaybinEvent::ExternalSubtitleFailed { id: failed } if *failed == id),
    );

    // `load` returns at ASYNC, so item B's collection need not have been
    // posted yet, and the worker barrier below does not wait for one (it
    // drains the job queue, not the bus). Without this wait the ghost-stream
    // assert could pass by inspecting no collection of item B at all.
    if !seen[after_load_b..]
        .iter()
        .any(|(event, _)| matches!(event, PlaybinEvent::StreamCollection(_)))
    {
        pump_until(
            &events,
            &mut seen,
            "the replacement item to advertise its stream collection",
            |event, _| matches!(event, PlaybinEvent::StreamCollection(_)),
        );
    }

    drain_worker(&playbin);
    drain_events(&events, &mut seen);
    // Both items are audio-only MP3s, so a TEXT stream anywhere can only be
    // the leaked external: the cross-item ghost this gate exists to prevent.
    assert!(
        text_stream_ids(&seen).is_empty(),
        "the superseded attach reached the new item; TEXT streams seen: {:?}",
        text_stream_ids(&seen)
    );
    assert!(
        playbin.stale_job_drops() >= 1,
        "nothing was reported dropped, so the attach cannot have been gated"
    );

    let _ = playbin.stop();
    let _ = std::fs::remove_file(&item_a);
    let _ = std::fs::remove_file(&item_b);
    let _ = std::fs::remove_file(&subs);
}

#[test]
fn a_clock_recovery_queued_before_a_stop_does_not_restart_the_pipeline() {
    init();

    let item = temp_file("clock", "mp3");
    make_mp3_file(&item, 5.0);

    let (playbin, events) = playbin_with_events();
    let mut seen = Vec::new();

    playbin.load_async(
        MediaInput::Uri(uri_of(&item)),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    wait_loaded(&events, &mut seen);
    playbin.play().expect("play");
    let deadline = Instant::now() + BOUND;
    while playbin.state_summary() != (gst::State::Playing, gst::State::VoidPending) {
        assert!(
            Instant::now() < deadline,
            "the pipeline never settled in PLAYING: {:?}",
            playbin.state_summary()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // A ClockLost answer formed while the item was playing.
    let release = stall_worker(&playbin);
    playbin.recover_clock_async();

    // And the item is gone before the worker gets to it. Synchronous, so it
    // bypasses the queue the recovery is sitting in.
    playbin.stop().expect("the synchronous stop");
    let stopped_state = playbin.state_summary();
    assert!(
        stopped_state.0 <= gst::State::Ready,
        "the stop did not take the pipeline down: {stopped_state:?}"
    );
    // Everything up to here belongs to the item that was torn down; only what
    // the released worker does from now on is under test.
    drain_events(&events, &mut seen);
    let after_stop = seen.len();

    drop(release);
    drain_worker(&playbin);
    drain_events(&events, &mut seen);

    // PENDING as well as CURRENT: the recovery's climb out of a sink-less
    // torn-down pipeline is ASYNC, so the resurrect shows up as a pending
    // PLAYING before it shows up as a current one.
    let state = playbin.state_summary();
    assert!(
        state.0 <= gst::State::Ready && state.1 <= gst::State::Ready,
        "the superseded clock recovery drove the stopped pipeline back up: {state:?}"
    );
    let resurrected: Vec<_> = seen[after_stop..]
        .iter()
        .filter(|(event, _)| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    ..
                }
            )
        })
        .collect();
    assert!(
        resurrected.is_empty(),
        "the stopped pipeline reported PLAYING again: {resurrected:?}"
    );
    assert!(
        playbin.stale_job_drops() >= 1,
        "nothing was reported dropped, so the recovery cannot have been gated"
    );

    let _ = std::fs::remove_file(&item);
}
