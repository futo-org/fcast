//! Regressions for `fcastplaybin/src/lib.rs` itself: the worker job loop and
//! the seek paths. A real pipeline with a real `urisourcebin`/`parsebin`/
//! `decodebin3` over real elementary streams; only the sink is ours, and it
//! records the seek events the crate actually issues, so these assert on
//! behaviour rather than on the shape of the code.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcastplaybin::{AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Seek, Sinks, StartPoint};
use gst::prelude::*;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        // The constructor builds fcastaudiostretch unconditionally; the
        // APPLICATION registers it in production, and this test is its own
        // application.
        fcast_gst_elements::fcastaudiostretch::plugin_init().expect("registering fcastaudiostretch");
    });
}

/// Encode `seconds` of silence to MP3 so playback runs through the real
/// urisourcebin/parsebin/decodebin3 topology (the recipe the in-crate pipeline
/// tests use).
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

fn temp_mp3(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fcastplaybin-regression-{}-{tag}-{n}.mp3",
        std::process::id()
    ))
}

/// One SEEK event as it reached the audio sink, i.e. as the crate issued it.
#[derive(Debug, Clone, Copy)]
struct SeenSeek {
    seqnum: gst::Seqnum,
    rate: f64,
}

type SeekLog = Arc<Mutex<Vec<SeenSeek>>>;

/// Sinks whose per-load audio sink records every SEEK event pushed upstream
/// through it. `gst_element_seek`/`send_event` on a pipeline reaches its sinks
/// first, so this observes the crate's own seek exactly as sent.
fn recording_sinks(log: &SeekLog) -> Sinks {
    let log = log.clone();
    Sinks {
        video: None,
        audio: AudioSink::Factory(Box::new(move || {
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()?;
            let pad = sink.static_pad("sink").expect("fakesink has a sink pad");
            let log = log.clone();
            pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && let gst::EventView::Seek(seek) = event.view()
                {
                    let (rate, ..) = seek.get();
                    log.lock().unwrap().push(SeenSeek {
                        seqnum: event.seqnum(),
                        rate,
                    });
                }
                gst::PadProbeReturn::Ok
            });
            Ok(sink)
        })),
    }
}

/// The rate the audio sink is actually running, read off the segment governing
/// its pad. `rate * applied_rate`, because the crate's pitch-preserving stretch
/// consumes the rate the way scaletempo does: it resamples the buffers and
/// hands downstream a `rate: 1.0, applied_rate: N` segment, so neither field
/// alone is the item's speed.
///
/// The LOAD's start seek cannot be observed in [`SeekLog`]: it is delivered at
/// the input (`Inner::seek_main_input`), not broadcast to the sinks, precisely
/// because at that point in a load the chains are still joining one at a time
/// and a broadcast misses whatever has not joined yet. The rate it applied is
/// still visible here, on the segment that came back DOWN from the source,
/// which is the property this test actually needs to establish.
fn audio_sink_effective_rate(playbin: &FcastPlaybin) -> Option<f64> {
    playbin
        .pipeline()
        .children()
        .iter()
        .filter(|child| child.element_flags().contains(gst::ElementFlags::SINK))
        .find_map(|sink| {
            let pad = sink.static_pad("sink")?;
            let event = pad.sticky_event::<gst::event::Segment>(0)?;
            let segment = event.segment();
            Some(segment.rate() * segment.applied_rate())
        })
}

fn wait_for_seek(log: &SeekLog, seqnum: gst::Seqnum, what: &str) -> SeenSeek {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(seen) = log.lock().unwrap().iter().rev().find(|s| s.seqnum == seqnum) {
            return *seen;
        }
        assert!(
            Instant::now() < deadline,
            "no seek stamped with the {what} seqnum reached the sink; saw {:?}",
            log.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Load `path` at `rate` and return once the pipeline settled in PAUSED.
fn load_at_rate(playbin: &FcastPlaybin, path: &std::path::Path, rate: f64) {
    playbin
        .load(
            MediaInput::Uri(format!("file://{}", path.display())),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate,
            },
        )
        .expect("load");
    let (res, current, pending) = playbin.pipeline().state(gst::ClockTime::from_seconds(10));
    assert!(
        res.is_ok() && current == gst::State::Paused && pending == gst::State::VoidPending,
        "the load did not settle in PAUSED: {res:?} {current:?} {pending:?}"
    );
}

/// A refresh seek (the flush a subtitle/audio track switch schedules so the new
/// track re-emits its current cue) must keep the item's PLAYBACK RATE.
///
/// `Job::RefreshSeek` hard-coded rate 1.0 into its seek event, so switching a
/// track while playing at 1.5x silently reset the pipeline to 1.0x. Nothing
/// reported it either: no `RateChanged` follows a refresh, so the caller's
/// transport state machine (and the sender's UI) kept showing the old speed
/// while the audio played at 1.0x.
#[test]
fn refresh_seek_keeps_the_playback_rate() {
    init();

    let log: SeekLog = Arc::new(Mutex::new(Vec::new()));
    let playbin = FcastPlaybin::new(recording_sinks(&log)).unwrap();
    playbin.set_event_handler(None, |_event, _generation| {});

    let path = temp_mp3("refresh-rate");
    make_mp3_file(&path, 5.0);

    // The field's "resume this item at the speed the user left it at" load.
    load_at_rate(&playbin, &path, 1.5);
    let start_rate = audio_sink_effective_rate(&playbin)
        .expect("the load's start seek must leave the sink on a segment");
    assert_eq!(start_rate, 1.5, "the item is not actually playing at 1.5x");

    // Exactly what the selection engine schedules after a subtitle or audio
    // track switch (`selection::Command::RefreshSeek`).
    let seqnum = gst::Seqnum::next();
    playbin.refresh_seek_async(seqnum);
    let seen = wait_for_seek(&log, seqnum, "refresh");

    let _ = playbin.stop();
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        seen.rate, 1.5,
        "the refresh seek reset the playback rate to {}; seeks seen: {:?}",
        seen.rate,
        log.lock().unwrap()
    );
}

/// A refresh seek the worker cannot run must still report `RefreshSeekFailed`,
/// or the selection engine's `refreshing` slot never clears and every later
/// selection is held behind it. (Already true before the rate fix; kept so the
/// rewritten job keeps reporting.)
#[test]
fn refresh_seek_reports_failure_when_it_cannot_run() {
    init();

    let log: SeekLog = Arc::new(Mutex::new(Vec::new()));
    let playbin = FcastPlaybin::new(recording_sinks(&log)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        if let PlaybinEvent::RefreshSeekFailed { seqnum } = event {
            let _ = tx.send(seqnum);
        }
    });

    // Nothing loaded: no sink can answer a position query.
    let seqnum = gst::Seqnum::next();
    playbin.refresh_seek_async(seqnum);
    let failed = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a refresh seek that cannot run must report RefreshSeekFailed");
    assert_eq!(failed, seqnum);
    let _ = playbin.stop();
}

/// Every queued seek must produce an outcome event. `Job::Seek` returned
/// SILENTLY when its position query failed, which is reachable for the
/// `Seek { position: None }` shape a SetSpeed produces: the caller owns the seek
/// queue and marks the seek in flight, so a job that neither seeks nor reports
/// leaves that slot in flight with nothing left to settle it, and every later
/// seek parks behind it.
///
/// The unanswerable position is forced here by taking the sinks out from under
/// the query while the pipeline stays settled in PAUSED, which is the state the
/// job checks before it commits to performing the seek.
#[test]
fn a_rate_seek_the_worker_cannot_perform_reports_seek_failed() {
    init();

    let log: SeekLog = Arc::new(Mutex::new(Vec::new()));
    let playbin = FcastPlaybin::new(recording_sinks(&log)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| match event {
        PlaybinEvent::SeekFailed => {
            let _ = tx.send("failed");
        }
        PlaybinEvent::RateChanged(_) => {
            let _ = tx.send("rate");
        }
        PlaybinEvent::QueueSeek(_) => {
            let _ = tx.send("queued");
        }
        _ => {}
    });

    let path = temp_mp3("rate-no-position");
    make_mp3_file(&path, 3.0);
    load_at_rate(&playbin, &path, 1.0);
    while rx.try_recv().is_ok() {}

    // Nothing left that can answer POSITION, while the pipeline itself stays
    // settled in PAUSED (removing a child does not change the bin's state), so
    // the job takes its perform branch and the query fails.
    let sinks: Vec<gst::Element> = playbin
        .pipeline()
        .children()
        .iter()
        .filter(|child| child.element_flags().contains(gst::ElementFlags::SINK))
        .cloned()
        .collect();
    assert!(!sinks.is_empty(), "the load built no sink to remove");
    for sink in &sinks {
        playbin.pipeline().remove(sink).expect("removing the sink");
        let _ = sink.set_state(gst::State::Null);
    }
    assert!(
        playbin
            .pipeline()
            .query_position::<gst::ClockTime>()
            .is_none(),
        "the pipeline can still answer POSITION; the setup did not reach the branch"
    );

    // A SetSpeed: rate only, no position (`state_machine::Seek`).
    playbin.seek_async(Seek::new(None, Some(1.5)));

    let outcome = rx.recv_timeout(Duration::from_secs(5));
    let _ = playbin.stop();
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        outcome.ok(),
        Some("failed"),
        "a rate seek the worker could not perform reported no outcome at all"
    );
}
