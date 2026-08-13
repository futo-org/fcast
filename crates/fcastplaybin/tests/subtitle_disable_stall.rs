//! How long does turning subtitles OFF stall the video?
//!
//! Field report: "disabling subs stalls the video for a bit." No duration in
//! the log, the observation was visual, so this measures it. The video
//! `FTestSink` records a monotonic timestamp per buffer
//! (`RecordEntry::monotonic`), so the stall is exactly the largest wall gap
//! between consecutive rendered buffers inside the disable window, compared
//! against the same statistic over a quiet window of the same run.
//!
//! Reported by the failure path and by `--nocapture`, so a future change can be
//! compared against the numbers in the report rather than against a feeling.
//!
//! The bound asserted is deliberately generous: this exists to catch a stall
//! regressing into seconds, not to police jitter on a loaded box.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::{FTestSink, Recording},
    spec::{CueSpec, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

/// Disables to measure.
const CYCLES: usize = 10;

/// Frame period of the fixture: 20 fps, so a healthy gap is ~50 ms and anything
/// the eye would call a stall is several multiples of it.
const FPS: i32 = 20;

/// A stall this long is a defect worth failing on. The measured max is an order
/// of magnitude below it, see the module doc.
const STALL_BUDGET: Duration = Duration::from_millis(1500);

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

/// The largest wall gap between consecutive rendered video buffers whose
/// arrival falls inside `window`, plus how many buffers that was measured over.
fn max_gap(video: &Recording, window: (Instant, Instant)) -> (Duration, usize) {
    let arrivals: Vec<Instant> = video
        .snapshot()
        .iter()
        .filter(|entry| entry.is_buffer())
        .map(|entry| entry.monotonic())
        .filter(|at| *at >= window.0 && *at <= window.1)
        .collect();
    let worst = arrivals
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]))
        .max()
        .unwrap_or_default();
    (worst, arrivals.len())
}

fn percentile(mut values: Vec<Duration>, fraction: f64) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values.sort();
    let index = ((values.len() - 1) as f64 * fraction).round() as usize;
    values[index]
}

struct Rig {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    video: Recording,
    /// Recorded by `pump`, which is the ONLY event drain: a closure that drains
    /// separately races it and loses.
    loaded: std::cell::Cell<bool>,
    text_sid: std::cell::RefCell<Option<String>>,
}

impl Rig {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        Self {
            playbin,
            events,
            video,
            loaded: std::cell::Cell::new(false),
            text_sid: std::cell::RefCell::new(None),
        }
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            match &event {
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                PlaybinEvent::Loaded { .. } => self.loaded.set(true),
                PlaybinEvent::StreamCollection(collection) => {
                    if let Some(sid) = collection
                        .iter()
                        .find(|s| s.stream_type().contains(gst::StreamType::TEXT))
                        .and_then(|s| s.stream_id().map(|id| id.to_string()))
                    {
                        *self.text_sid.borrow_mut() = Some(sid);
                    }
                }
                _ => {}
            }
        }
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Pump for `how_long` without asking for anything, so video flows.
    fn run_for(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        while Instant::now() < until {
            self.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn subtitle(&self, target: TrackTarget) {
        self.playbin.request_track(TrackSlot::Subtitle, target);
        self.pump();
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "shutdown never finished");
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

#[test]
fn turning_subtitles_off_does_not_stall_the_video() {
    init();
    // The video spec `regression_video_reenable` uses, which routes reliably,
    // plus the pacing that keeps the SOURCE ahead of the clock-synced sink. That
    // is both the field's shape (a demuxer runs ahead) and the right one for
    // this measurement: gaps at the sink are then pipeline stalls, not source
    // starvation.
    let media = ScenarioBuilder::new("stallmain")
        .stream(StreamSpec::new(
            "video_0",
            StreamKind::Video {
                width: 16,
                height: 16,
                fps: gst::Fraction::new(FPS, 1),
                keyframe_interval: 1,
            },
        ))
        .stream(StreamSpec::audio("audio_0"))
        .text("text_0", cues(400, gst::ClockTime::from_mseconds(250)))
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::Jitter {
            base_ms: 5,
            jitter_ms: 0,
        })
        .register();

    let rig = Rig::new();
    rig.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    rig.wait_until("the load", || rig.loaded.get());
    rig.playbin.play().expect("play");
    rig.wait_until("a settled PLAYING", || {
        rig.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });
    // The text stream the scenario advertises, so the re-enable asks for the
    // same track the disable turned off.
    rig.wait_until("the collection to name a text stream", || {
        rig.text_sid.borrow().is_some()
    });
    let sid = rig.text_sid.borrow().clone().expect("set above");

    // Steady state first, then the baseline: the same statistic over a window
    // in which nothing is requested at all.
    rig.run_for(Duration::from_millis(800));
    let baseline_from = Instant::now();
    rig.run_for(Duration::from_millis(1500));
    let (baseline, baseline_buffers) = max_gap(&rig.video, (baseline_from, Instant::now()));
    assert!(
        baseline_buffers > 10,
        "the baseline window rendered only {baseline_buffers} buffers; nothing was flowing"
    );

    // Now the disables. Each cycle: disable, let video flow, re-enable, settle.
    let mut stalls = Vec::new();
    for cycle in 0..CYCLES {
        let from = Instant::now();
        rig.subtitle(TrackTarget::Stream(None));
        rig.run_for(Duration::from_millis(500));
        let (gap, buffers) = max_gap(&rig.video, (from, Instant::now()));
        assert!(
            buffers > 3,
            "cycle {cycle}: only {buffers} buffers rendered around the disable"
        );
        stalls.push(gap);

        rig.subtitle(TrackTarget::Stream(Some(sid.clone())));
        rig.run_for(Duration::from_millis(400));
    }

    let p50 = percentile(stalls.clone(), 0.5);
    let worst = stalls.iter().copied().max().unwrap_or_default();
    // Printed so a run is a measurement, not just a verdict.
    println!(
        "subtitle-disable video stall over {CYCLES} disables: p50 {:.1} ms, max {:.1} ms; \
         no-disable baseline max {:.1} ms ({baseline_buffers} buffers)",
        p50.as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0,
        baseline.as_secs_f64() * 1000.0,
    );
    assert!(
        worst < STALL_BUDGET,
        "turning subtitles off stalled the video for {:.1} ms (p50 {:.1} ms, baseline {:.1} ms), \
         budget {:.1} ms",
        worst.as_secs_f64() * 1000.0,
        p50.as_secs_f64() * 1000.0,
        baseline.as_secs_f64() * 1000.0,
        STALL_BUDGET.as_secs_f64() * 1000.0,
    );

    media.release_all();
    rig.shutdown();
    media.unregister();
}
