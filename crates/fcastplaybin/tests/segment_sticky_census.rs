//! TASK 6 instrument: does a flush pair INJECTED BY THIS CRATE leave a
//! decodebin3 sink pad with no SEGMENT sticky, so the next buffer chains as
//! "Got data flow before segment event"? A yes/no question of one pad, read off
//! that pad's own sticky list. Deliberately NOT a rate measurement: counting
//! warnings across arms was hopeless (0/5/0/0/0 current against 0/0/0/24/8
//! all-levers-off). Lever for the fix: `FCAST_NO_FLUSH_SEGMENT_RESTORE`.
//! Long-form record: NEXT-FIXES-PLAN.md, fuzz-campaign-findings.md.
//!
//! Mechanism (gstpad.c lines from `/home/merb/sub/Programming/gstreamer`):
//! `Inner::flush_pads` (lib.rs:5848) sends FlushStart + FlushStop(true) on
//! decodebin3 SINK pads, from `Teardown::run` (lib.rs:3146),
//! `flush_parked_text_pushes` (lib.rs:6067) and `remove_input` (lib.rs:7241).
//! `gst_pad_send_event_unchecked` then does
//! `remove_event_by_type(pad, GST_EVENT_SEGMENT)` (gstpad.c:6047, 5695 on the
//! push path), and nothing re-arms upstream: only `schedule_events`' three
//! callers set `GST_PAD_FLAG_PENDING_EVENTS` (`pre_activate` gstpad.c:1046,
//! `gst_pad_link_full` 2597, `check_sticky`'s relink abort 4269) and a flush is
//! none of them. The FLUSH_STOP travels DOWNSTREAM from the injection pad,
//! stripping the sticky off each pad on the way, which is the cascade task 6
//! records (wild instance on seed 1600058: 13 warnings at ONE timestamp, one
//! buffer per pad, `fpb-decodebin:sink_3` down to `fakesink21:sink`). A pair
//! arriving FROM UPSTREAM is harmless, a flushing seek brings a fresh SEGMENT.
//!
//! Buffers are counted, not warnings, so the result does not depend on
//! `GST_ENABLE_EXTRA_CHECKS` being compiled in. The GLib log handler runs
//! alongside only to show the census and the real warning agree when it is.

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
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{scenario::ScenarioBuilder, sink::FTestSink, spec::CueSpec, spec::Pacing};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Realtime, and long, so the source is STILL PUSHING when the flush lands.
/// An as-fast-as-possible item races to EOS and there is no later buffer to
/// catch segmentless, which would read as a false negative.
const CLIP: gst::ClockTime = gst::ClockTime::from_seconds(60);

/// How long to let buffers flow after the flush before taking the census.
const OBSERVE: Duration = Duration::from_millis(1200);

/// What crossed a decodebin3 sink pad, in order.
#[derive(Debug, Clone, PartialEq)]
enum Mark {
    FlushStart,
    FlushStop,
    Segment,
    StreamStart,
    Eos,
    /// A buffer, and whether the pad had a SEGMENT sticky as it arrived. The
    /// probe runs at gstpad.c:4582, right after that pad's "before segment
    /// event" check (4566-4580) under the same stream lock, so
    /// `had_segment == false` is exactly the condition that warns.
    Buffer { had_segment: bool },
    Other(String),
}

type Census = Arc<Mutex<Vec<(String, Mark)>>>;

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

/// Counts real `Got data flow before segment event` warnings, to cross-check
/// the census. Returns the counter plus a flag saying whether GLib delivered
/// ANY warning at all.
fn watch_warnings() -> (Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
    let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let any = Arc::new(AtomicBool::new(false));
    let sink = Arc::clone(&warnings);
    let saw = Arc::clone(&any);
    gst::glib::log_set_default_handler(move |domain, level, message| {
        // Re-print, so a failing run stays readable.
        eprintln!("({}) {level:?}: {message}", domain.unwrap_or("?"));
        saw.store(true, Ordering::SeqCst);
        if message.contains("before segment event") {
            sink.lock().expect("warning sink").push(message.to_owned());
        }
    });
    (warnings, any)
}

fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("S{index:02}"))
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

/// Probe every current sink pad of `fpb-decodebin`. Returns the pads, so a
/// test can inject into exactly the pads it is watching.
///
/// `EVENT_FLUSH` must be requested EXPLICITLY: `EVENT_DOWNSTREAM` does not
/// imply it, a trap `tests/teardown_races.rs` paid for once already.
fn install_census(playbin: &FcastPlaybin, census: &Census) -> Vec<gst::Pad> {
    let db3 = playbin
        .pipeline()
        .by_name("fpb-decodebin")
        .expect("fpb-decodebin");
    // An input's pads are linked from a streaming thread, so `Loaded` can land
    // a hair before the link. Wait rather than assert, or the census races the
    // setup it is trying to observe.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !db3.sink_pads().iter().any(|pad| pad.is_linked()) {
        assert!(
            Instant::now() < deadline,
            "fpb-decodebin never got a linked sink pad"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let mut watched = Vec::new();
    for pad in db3.sink_pads() {
        // decodebin3 has a STATIC `sink` pad as well as the `sink_%u` request
        // pads the crate links inputs into. It carries no data here, so it
        // never holds a segment and would only pollute the census.
        if !pad.is_linked() {
            continue;
        }
        let name = format!("{}:{}", db3.name(), pad.name());
        let sink = census.clone();
        pad.add_probe(
            gst::PadProbeType::BUFFER
                | gst::PadProbeType::BUFFER_LIST
                | gst::PadProbeType::EVENT_DOWNSTREAM
                | gst::PadProbeType::EVENT_FLUSH,
            move |pad, info| {
                let mark = match &info.data {
                    Some(gst::PadProbeData::Buffer(_)) | Some(gst::PadProbeData::BufferList(_)) => {
                        Mark::Buffer {
                            had_segment: pad
                                .sticky_event::<gst::event::Segment>(0)
                                .is_some(),
                        }
                    }
                    Some(gst::PadProbeData::Event(event)) => match event.type_() {
                        gst::EventType::FlushStart => Mark::FlushStart,
                        gst::EventType::FlushStop => Mark::FlushStop,
                        gst::EventType::Segment => Mark::Segment,
                        gst::EventType::StreamStart => Mark::StreamStart,
                        gst::EventType::Eos => Mark::Eos,
                        other => Mark::Other(format!("{other:?}")),
                    },
                    _ => return gst::PadProbeReturn::Ok,
                };
                sink.lock().expect("census").push((name.clone(), mark));
                gst::PadProbeReturn::Ok
            },
        )
        .expect("census probe");
        watched.push(pad);
    }
    assert!(!watched.is_empty(), "fpb-decodebin had no sink pads to watch");
    watched
}

/// Buffers that arrived with no SEGMENT sticky, per pad.
fn segmentless(census: &Census) -> Vec<(String, usize)> {
    let log = census.lock().expect("census");
    let mut per_pad: Vec<(String, usize)> = Vec::new();
    for (pad, mark) in log.iter() {
        if *mark == (Mark::Buffer { had_segment: false }) {
            match per_pad.iter_mut().find(|(name, _)| name == pad) {
                Some((_, count)) => *count += 1,
                None => per_pad.push((pad.clone(), 1)),
            }
        }
    }
    per_pad
}

/// The census around each FLUSH_STOP, which is the part worth reading.
fn render(census: &Census) -> String {
    let log = census.lock().expect("census");
    let mut out = String::new();
    let mut counts: Vec<(String, usize, usize)> = Vec::new();
    for (pad, mark) in log.iter() {
        if let Mark::Buffer { had_segment } = mark {
            let slot = match counts.iter_mut().find(|(name, _, _)| name == pad) {
                Some(slot) => slot,
                None => {
                    counts.push((pad.clone(), 0, 0));
                    counts.last_mut().expect("just pushed")
                }
            };
            if *had_segment {
                slot.1 += 1;
            } else {
                slot.2 += 1;
            }
        }
    }
    for (pad, with, without) in &counts {
        out.push_str(&format!(
            "  {pad}: {with} buffers with a segment, {without} WITHOUT\n"
        ));
    }
    // The sequence, collapsing buffer runs so the events stay legible.
    out.push_str("  sequence (buffer runs collapsed):\n");
    let mut last: Option<(String, bool)> = None;
    let mut run = 0usize;
    let mut flush_sequence: Vec<String> = Vec::new();
    for (pad, mark) in log.iter() {
        match mark {
            Mark::Buffer { had_segment } => {
                let key = (pad.clone(), *had_segment);
                if last.as_ref() == Some(&key) {
                    run += 1;
                } else {
                    if let Some((prev, seg)) = last.take() {
                        flush_sequence.push(format!(
                            "{prev} {}xBUFFER({})",
                            run,
                            if seg { "seg" } else { "NO-SEG" }
                        ));
                    }
                    last = Some(key);
                    run = 1;
                }
            }
            other => {
                if let Some((prev, seg)) = last.take() {
                    flush_sequence.push(format!(
                        "{prev} {}xBUFFER({})",
                        run,
                        if seg { "seg" } else { "NO-SEG" }
                    ));
                    run = 0;
                }
                flush_sequence.push(format!("{pad} {other:?}"));
            }
        }
    }
    if let Some((prev, seg)) = last.take() {
        flush_sequence.push(format!(
            "{prev} {}xBUFFER({})",
            run,
            if seg { "seg" } else { "NO-SEG" }
        ));
    }
    for line in flush_sequence {
        out.push_str(&format!("    {line}\n"));
    }
    out
}

/// A playing pipeline on a realtime item, with the census not yet installed.
struct Rig {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    main: fcasttest::scenario::ScenarioHandle,
}

impl Rig {
    fn start(key: &str) -> Self {
        let main = ScenarioBuilder::new(key)
            .video("video_0")
            .audio("audio_0")
            .duration(CLIP)
            .bytes_per_buffer(64)
            .pacing(Pacing::Realtime)
            .register();

        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(FTestSink::new().upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });

        let rig = Rig {
            playbin,
            events,
            main,
        };
        rig.playbin.load_async(
            MediaInput::Uri(rig.main.uri()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        rig.wait_for("the load to finish", |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        rig.playbin.play().expect("play");
        rig
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(gate());
        while self.events.try_recv().is_ok() {}
    }

    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.playbin.poll_text_policy();
            self.playbin.pump_selection(gate());
            while let Ok(event) = self.events.try_recv() {
                if pred(&event) {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump();
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Pump for a while, letting the realtime source keep pushing.
    fn observe(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        while Instant::now() < until {
            self.pump();
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// CONTROL. Without it the other tests prove nothing: a pipeline that always
/// did this would look identical.
#[test]
fn control_a_flowing_pipeline_produces_no_segmentless_buffer() {
    init();
    let (warnings, _any) = watch_warnings();
    let rig = Rig::start("segcensuscontrol");

    let census: Census = Arc::new(Mutex::new(Vec::new()));
    install_census(&rig.playbin, &census);
    rig.observe(OBSERVE);

    let offenders = segmentless(&census);
    let report = render(&census);
    let seen = warnings.lock().expect("warnings").len();
    eprintln!("CONTROL census:\n{report}  warnings: {seen}");
    assert!(
        offenders.is_empty(),
        "a pipeline with NO injected flush produced segmentless buffers, so the \
         instrument cannot attribute anything to a flush: {offenders:?}\n{report}"
    );
    rig.main.unregister();
}

/// The hypothesis, isolated from any crate path: the crate's exact pair, sent
/// to a live decodebin3 sink pad whose source is still pushing.
#[test]
fn an_injected_flush_pair_strands_a_decodebin_sink_pad_without_a_segment() {
    init();
    let (warnings, any_warning) = watch_warnings();
    let rig = Rig::start("segcensusinject");

    let census: Census = Arc::new(Mutex::new(Vec::new()));
    let watched = install_census(&rig.playbin, &census);

    // Let real buffers flow first, so the pads hold a SEGMENT sticky the flush
    // can then remove.
    {
        let census = census.clone();
        rig.wait_until("buffers to flow into decodebin3", move || {
            census
                .lock()
                .expect("census")
                .iter()
                .filter(|(_, mark)| matches!(mark, Mark::Buffer { .. }))
                .count()
                >= 5
        });
    }
    // Only a pad that holds a SEGMENT sticky can show a flush removing one.
    let targets: Vec<gst::Pad> = watched
        .iter()
        .filter(|pad| pad.sticky_event::<gst::event::Segment>(0).is_some())
        .cloned()
        .collect();
    assert!(
        !targets.is_empty(),
        "no watched decodebin3 sink pad held a SEGMENT sticky before the flush, \
         so the census cannot show the flush removing one (watched: {:?})",
        watched.iter().map(|p| p.name()).collect::<Vec<_>>()
    );
    eprintln!(
        "injecting into {:?}",
        targets.iter().map(|p| p.name()).collect::<Vec<_>>()
    );

    // Byte-for-byte `Inner::flush_pads` (lib.rs:5848-5851).
    for pad in &targets {
        let _ = pad.send_event(gst::event::FlushStart::new());
        let _ = pad.send_event(gst::event::FlushStop::new(true));
    }

    // The decisive read, taken IMMEDIATELY: is the sticky gone?
    let stripped: Vec<String> = targets
        .iter()
        .filter(|pad| pad.sticky_event::<gst::event::Segment>(0).is_none())
        .map(|pad| pad.name().to_string())
        .collect();

    rig.observe(OBSERVE);

    let offenders = segmentless(&census);
    let report = render(&census);
    let warned = warnings.lock().expect("warnings").len();
    eprintln!(
        "INJECTED census (pads whose SEGMENT sticky the flush removed: {stripped:?}):\n\
         {report}  \"before segment event\" warnings: {warned} \
         (GLib delivered any warning at all: {})\n",
        any_warning.load(Ordering::SeqCst)
    );

    assert!(
        !stripped.is_empty(),
        "the crate's own flush pair did NOT remove the SEGMENT sticky from any \
         watched decodebin3 sink pad, which REFUTES the task 6 hypothesis at its \
         first step\n{report}"
    );
    assert!(
        !offenders.is_empty(),
        "the SEGMENT sticky was removed ({stripped:?}) but no later buffer arrived \
         segmentless, so the flush alone does not produce the warning: either the \
         source resent a segment or it stopped pushing\n{report}"
    );
    rig.main.unregister();
}

/// THE FIX, measured against `an_injected_flush_pair_...` directly above: same
/// rig, same injection point, same pads. The only difference is the third
/// event, the SEGMENT captured off each pad before the `FlushStart` and resent
/// after the `FlushStop`, which is what `Inner::flush_db3_sink_pads` does
/// (lever `FCAST_NO_FLUSH_SEGMENT_RESTORE`). Read that function's SCOPE section
/// before assuming the same replay is safe on a text-branch pad: measurably it
/// is not.
///
/// The two asserts are different claims. The sticky being back on the injection
/// pad only says the replay landed. Zero segmentless buffers says it
/// PROPAGATED, since every pad from there to the sink lost its segment to the
/// same FLUSH_STOP. A/B: 60 + 30 segmentless and 30 warnings there, 0 and 0
/// here.
#[test]
fn a_flush_pair_that_replays_the_segment_strands_nothing() {
    init();
    let (warnings, any_warning) = watch_warnings();
    let rig = Rig::start("segcensusreplay");

    let census: Census = Arc::new(Mutex::new(Vec::new()));
    let watched = install_census(&rig.playbin, &census);

    {
        let census = census.clone();
        rig.wait_until("buffers to flow into decodebin3", move || {
            census
                .lock()
                .expect("census")
                .iter()
                .filter(|(_, mark)| matches!(mark, Mark::Buffer { .. }))
                .count()
                >= 5
        });
    }
    let targets: Vec<gst::Pad> = watched
        .iter()
        .filter(|pad| pad.sticky_event::<gst::event::Segment>(0).is_some())
        .cloned()
        .collect();
    assert!(
        !targets.is_empty(),
        "no watched decodebin3 sink pad held a SEGMENT sticky before the flush, so \
         there is nothing for the replay to restore and this test would pass \
         vacuously (watched: {:?})",
        watched.iter().map(|p| p.name()).collect::<Vec<_>>()
    );
    eprintln!(
        "injecting into {:?}",
        targets.iter().map(|p| p.name()).collect::<Vec<_>>()
    );

    // `Inner::flush_pads` as it stands now: capture, pair, replay.
    for pad in &targets {
        let segment = pad.sticky_event::<gst::event::Segment>(0);
        let _ = pad.send_event(gst::event::FlushStart::new());
        let _ = pad.send_event(gst::event::FlushStop::new(true));
        if let Some(segment) = segment {
            let _ = pad.send_event(segment);
        }
    }

    let stripped: Vec<String> = targets
        .iter()
        .filter(|pad| pad.sticky_event::<gst::event::Segment>(0).is_none())
        .map(|pad| pad.name().to_string())
        .collect();

    rig.observe(OBSERVE);

    let offenders = segmentless(&census);
    let report = render(&census);
    let warned = warnings.lock().expect("warnings").len();
    eprintln!(
        "REPLAY census (pads still missing a SEGMENT sticky after the replay: \
         {stripped:?}):\n{report}  \"before segment event\" warnings: {warned} \
         (GLib delivered any warning at all: {})\n",
        any_warning.load(Ordering::SeqCst)
    );

    assert!(
        stripped.is_empty(),
        "the replayed SEGMENT did not land back on {stripped:?}, so the flush pair \
         still strands the injection pad itself\n{report}"
    );
    assert!(
        offenders.is_empty(),
        "the replayed SEGMENT landed on the injection pad but buffers still arrived \
         segmentless further down ({offenders:?}), so it did not propagate along the \
         path the FLUSH_STOP took\n{report}"
    );
    rig.main.unregister();
}

/// ATTRIBUTION. Which crate call site can do this to a still-running stream?
/// `FcastPlaybin::teardown` flushes every input's `db3_sink_pads` at
/// lib.rs:2478 (via `flush_parked_text_pushes`) and only descends the pipeline
/// at lib.rs:2486. `Teardown::run` has the same shape (flush 3146, descent
/// 3156). In between the pipeline is still PLAYING and the sources still
/// pushing into pads whose SEGMENT the flush just removed. That ordering long
/// predates this session, which is why task 6 is intermittent AND present with
/// every lever off.
///
/// Reported rather than asserted: whether a buffer lands inside that window is
/// a race. The mechanism is pinned by the injected-pair test.
#[test]
fn stopping_flushes_the_decodebin_sink_pads_while_the_sources_still_push() {
    init();
    let (warnings, _any) = watch_warnings();
    let rig = Rig::start("segcensusstop");

    let census: Census = Arc::new(Mutex::new(Vec::new()));
    install_census(&rig.playbin, &census);
    {
        let census = census.clone();
        rig.wait_until("buffers to flow into decodebin3", move || {
            census
                .lock()
                .expect("census")
                .iter()
                .filter(|(_, mark)| matches!(mark, Mark::Buffer { .. }))
                .count()
                >= 5
        });
    }

    // The crate's own stop, i.e. `teardown`: flush at lib.rs:2478, descent at
    // lib.rs:2486.
    rig.playbin.stop().expect("stop");
    rig.observe(Duration::from_millis(300));

    let offenders = segmentless(&census);
    let report = render(&census);
    let warned = warnings.lock().expect("warnings").len();
    eprintln!(
        "STOP census:\n{report}  \"before segment event\" warnings: {warned}\n  \
         segmentless buffers per pad: {offenders:?}"
    );
    rig.main.unregister();
}

/// The same thing through the crate's own `remove_input`, i.e. the way
/// `external_subtitle_lifecycle` reaches it.
#[test]
fn detaching_an_external_flushes_its_decodebin_pad_without_a_replacement_segment() {
    init();
    let (warnings, _any) = watch_warnings();
    let rig = Rig::start("segcensusdetach");

    let subs = ScenarioBuilder::new("segcensussubs")
        .text("text_0", cues(140, gst::ClockTime::from_mseconds(400)))
        .duration(CLIP)
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let id = rig.playbin.attach_subtitle(&subs.uri()).expect("attach");
    {
        let playbin = rig.playbin.clone();
        rig.wait_until("the external to materialize", move || {
            !playbin.subtitle_stream_ids(id).is_empty()
        });
    }
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    rig.observe(Duration::from_millis(400));

    // Census AFTER the external is linked, so its own decodebin3 sink pad is
    // among the watched pads.
    let census: Census = Arc::new(Mutex::new(Vec::new()));
    let watched = install_census(&rig.playbin, &census);
    eprintln!("watching {} fpb-decodebin sink pads", watched.len());
    rig.observe(Duration::from_millis(400));

    rig.playbin.detach_subtitle(id).expect("detach");
    rig.observe(OBSERVE);

    let offenders = segmentless(&census);
    let report = render(&census);
    let warned = warnings.lock().expect("warnings").len();
    eprintln!("DETACH census:\n{report}  \"before segment event\" warnings: {warned}");
    // Reported, not asserted: which pads a detach flushes is a property of
    // `remove_input`, and a bare `assert!` would turn that routing detail into
    // a red test. The mechanism is pinned by the injected-pair test.
    eprintln!("DETACH segmentless buffers per pad: {offenders:?}");

    rig.main.unregister();
    subs.unregister();
}
