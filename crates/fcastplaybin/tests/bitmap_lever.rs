//! THE MASTER LEVER: `FCAST_NO_BITMAP_SUBS=1` puts every bitmap subtitle
//! format back on the loud refusal path it took before it had a decoder.
//!
//! This is the rollback, and it is the reason the phase can ship a decoder
//! without betting the release on it. With the lever set, a `subpicture/*`
//! stream is refused at the caps gate exactly as it was: the branch is never
//! built, the packets never reach the consumer, one
//! [`PlaybinEvent::SubtitleTrackUnsupported`] goes out naming the stream and
//! its caps, and the pipeline plays on with the track parked. Byte for byte the
//! contract `sink_subtitles.rs` asserted for PGS before it had a decoder, the
//! test below is that test, kept, with the lever in front of it.
//!
//! # Why this is its own test binary
//!
//! `BitmapSubsEnabled` is read ONCE per `FcastPlaybin`, from the process
//! environment, at construction. A lever that could change under a running
//! pipeline would let the caps gate and the per-sample decision disagree, which
//! is a defect and not a feature. So the environment has to be set before
//! anything builds a playbin, which means owning the process, hence a separate
//! binary and the set inside the same `Once` every test here goes through.
//! (`tests/cue_ir_lever.rs` is the precedent, for the same reason.)

use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
    SubtitleFeedItem, TrackSlot, TrackTarget,
};
use fcasttest::{
    caps as tcaps,
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{CueSpec, Pacing, StreamSpec},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const FEED_BOUND: Duration = Duration::from_secs(10);
const SET_STEP: gst::ClockTime = gst::ClockTime::from_mseconds(250);
const MEDIA_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(120);

static PIPELINE: Mutex<()> = Mutex::new(());

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // BEFORE ANYTHING BUILDS A PLAYBIN. Every test in this binary comes
        // through this `Once` before it touches the crate.
        //
        // SAFETY: no other thread is running yet, `Once` serializes the test
        // threads here, and none has called into the crate or spawned anything
        // that reads the environment.
        unsafe {
            std::env::set_var("FCAST_NO_BITMAP_SUBS", "1");
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

fn gate() -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused: false,
        seekable: true,
    }
}

struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: Arc<Mutex<Vec<PlaybinEvent>>>,
    feed: Arc<Mutex<Vec<SubtitleFeedItem>>>,
    cursor: Mutex<usize>,
}

impl Harness {
    fn new() -> Self {
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(FTestSink::new().upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        let events: Arc<Mutex<Vec<PlaybinEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        playbin.set_event_handler(None, move |event, _generation| {
            sink.lock().push(event);
        });
        let feed: Arc<Mutex<Vec<SubtitleFeedItem>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = feed.clone();
        playbin.set_subtitle_consumer(move |item| {
            sink.lock().push(item);
        });
        Self {
            playbin,
            events,
            feed,
            cursor: Mutex::new(0),
        }
    }

    fn drain(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(gate());
        let events = self.events.lock();
        let mut cursor = self.cursor.lock();
        for event in events[*cursor..].iter() {
            if let PlaybinEvent::Error { error, .. } = event {
                panic!("pipeline error: {error}");
            }
        }
        *cursor = events.len();
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        self.wait_for_within(what, EVENT_TIMEOUT, &mut done);
    }

    fn wait_for_within(&self, what: &str, budget: Duration, done: &mut dyn FnMut() -> bool) {
        let deadline = Instant::now() + budget;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.drain();
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn load(&self, scenario: &ScenarioHandle) {
        self.playbin.load_async(
            MediaInput::Uri(scenario.uri()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        let events = self.events.clone();
        let before = events.lock().len();
        self.wait_for("the load to finish", move || {
            events.lock()[before..]
                .iter()
                .any(|event| matches!(event, PlaybinEvent::Loaded { .. }))
        });
    }

    fn text_sids(&self) -> Vec<String> {
        self.events
            .lock()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => Some(
                    collection
                        .iter()
                        .filter(|stream| stream.stream_type().contains(gst::StreamType::TEXT))
                        .filter_map(|stream| stream.stream_id().map(|s| s.to_string()))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn unsupported_reports(&self) -> Vec<(String, String)> {
        self.events
            .lock()
            .iter()
            .filter_map(|event| match event {
                PlaybinEvent::SubtitleTrackUnsupported { sid, caps } => {
                    Some((sid.clone(), caps.to_string()))
                }
                _ => None,
            })
            .collect()
    }

    fn text_sinks(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut iter = self.playbin.pipeline().iterate_elements();
        while let Ok(Some(element)) = iter.next() {
            let name = element.name().to_string();
            if name.starts_with("fpb-textsink-") {
                names.push(name);
            }
        }
        names
    }

    fn play(&self) {
        self.playbin.play().expect("play");
        self.wait_for("the pipeline to reach PLAYING", || {
            let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Playing && pending == gst::State::VoidPending
        });
    }

    fn shutdown(self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.playbin.pump_selection(gate());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// The step-3 loud contract, asserted with the lever in force.
fn levered_track_is_loud_and_harmless(key: &str, caps: gst::Caps, payload: fn(u8) -> Vec<u8>) {
    let cues: Vec<CueSpec> = (0..400u32)
        .map(|index| {
            let start = SET_STEP * u64::from(index);
            CueSpec::packets(start, start + SET_STEP / 2, vec![payload(index as u8)])
        })
        .collect();
    let media = ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(StreamSpec::text("text_0", cues).with_caps(caps.clone()))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register();
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    harness.wait_for("the text stream to be advertised", || {
        !harness.text_sids().is_empty()
    });
    let sids = harness.text_sids();
    harness.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some(sids[0].clone())),
    );
    harness.drain();

    harness.wait_for_within("the unsupported-track report", FEED_BOUND, &mut || {
        !harness.unsupported_reports().is_empty()
    });

    // Keep polling past the point where a re-report would show up: the dedupe
    // is the other half of the loud contract.
    let settle = Instant::now() + Duration::from_millis(2000);
    while Instant::now() < settle {
        harness.drain();
        thread::sleep(Duration::from_millis(10));
    }

    let reports = harness.unsupported_reports();
    assert_eq!(
        reports.len(),
        1,
        "expected exactly one SubtitleTrackUnsupported, got {reports:?}"
    );
    assert_eq!(reports[0].0, sids[0], "the report names the wrong stream");
    assert_eq!(
        reports[0].1,
        caps.to_string(),
        "the report does not carry the offending caps"
    );
    assert!(
        harness.text_sinks().is_empty(),
        "the lever left a consumer branch built for a refused format: {:?}",
        harness.text_sinks()
    );
    // No PAYLOAD reached the consumer. `Clear` may legitimately be there, the
    // driver sends it on a switch or a flush whatever the track turned out to
    // be, and asserting on an empty feed would be asserting on that instead.
    let payload: Vec<_> = harness
        .feed
        .lock()
        .iter()
        .filter(|item| !matches!(item, SubtitleFeedItem::Clear))
        .cloned()
        .collect();
    assert!(
        payload.is_empty(),
        "the lever let subtitle payload through to the consumer: {payload:?}"
    );

    // NO WEDGE, kept from both arms: the parked track must not stop the
    // pipeline, whichever way the lever is thrown.
    let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(
        (current, pending),
        (gst::State::Playing, gst::State::VoidPending),
        "the pipeline left PLAYING while the levered track was parked"
    );
    let before = harness.playbin.position();
    harness.wait_for_within("the position to advance", FEED_BOUND, &mut || {
        harness.playbin.position() > before
    });

    harness.shutdown();
    media.unregister();
}

/// PGS under the master lever: the format that renders by default is refused
/// again, loudly, and nothing reaches the consumer.
///
/// The rollback proof. Without the lever this same media takes the branch
/// asserted in `sink_subtitles::pgs_subtitles_are_carried_instead_of_refused`
/// and `tests/bitmap_subtitles.rs`; with it, the driver behaves as it did
/// before a PGS decoder existed.
#[test]
fn pgs_is_refused_loudly_under_the_master_lever() {
    let _lock = PIPELINE.lock();
    init();
    levered_track_is_loud_and_harmless("leverpgs", tcaps::pgs_caps(), fcasttest::pgs::display_set);
}

/// DVB under the master lever: the last format to be carried by default, and
/// the one whose refusal path the other two used to share.
#[test]
fn dvb_is_refused_loudly_under_the_master_lever() {
    let _lock = PIPELINE.lock();
    init();
    levered_track_is_loud_and_harmless("leverdvb", tcaps::dvb_caps(), fcasttest::dvb::display_set);
}

/// VOBSUB under the master lever: the same rollback, for the format whose
/// paused-instant contract (P12(b)) depends on the branch existing at all.
///
/// Its caps carry `codec_data`, which is the real matroska shape, and under
/// the lever the palette that rides there reaches nothing, because the branch
/// is never built.
#[test]
fn vobsub_is_refused_loudly_under_the_master_lever() {
    let _lock = PIPELINE.lock();
    init();
    levered_track_is_loud_and_harmless(
        "levervobsub",
        tcaps::vobsub_caps(fcasttest::vobsub::SAMPLE_IDX),
        fcasttest::vobsub::subpicture_unit,
    );
}

/// The lever's own reading, pinned beside the behaviour: it is the process
/// environment that turns the formats off, and it turns off ALL of them.
///
/// The behavioural tests above cover the three formats one at a time; this one
/// covers the RULE, so the master lever cannot silently become a per-format one
/// and so that a fourth format added to `BitmapSubFormat::ALL` is asserted
/// about here without anyone remembering to come back.
#[test]
fn the_master_lever_is_read_from_the_environment_and_takes_every_format() {
    init();
    assert_eq!(
        std::env::var("FCAST_NO_BITMAP_SUBS").ok().as_deref(),
        Some("1"),
        "this binary owns the process environment and must have set the lever"
    );
    // THE IMPLEMENTED SET IS UNCHANGED BY THE LEVER, format by format. The
    // lever gates CARRIAGE; the set says which formats have a decoder at all,
    // and conflating the two is how a rollback becomes a permanent removal,
    // the driver's mirror would stop matching the engine's table and the
    // cross-crate agreement test would be asserting agreement about the
    // environment.
    for format in fcastplaybin::BitmapSubFormat::ALL {
        assert!(
            fcastplaybin::bitmap_format_implemented(format),
            "{format:?}: the master lever moved the implemented set, which it must not touch"
        );
    }
}
