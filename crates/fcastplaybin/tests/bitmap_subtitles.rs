//! Bitmap subtitle transport: packets from `ftest://` media delivered as
//! [`SubtitleFeedItem::Bitmap`].
//!
//! The claim is about the transport only. `fcastplaybin` decides from caps
//! that a stream is a bitmap format, converts each buffer's pts to running
//! time on the video base, and forwards the buffer untouched. Reassembly and
//! decode live in the renderer's engine.
//!
//! Asserted here is delivery shape. The branch links, packets arrive in order
//! with the format the caps named, fragments of one display set arrive as the
//! separate buffers they were with one running time between them, no
//! `SubtitleTrackUnsupported` goes out, a flushing seek clears before it
//! redelivers, and a paused pipeline never wedges.
//!
//! Serialized because there is one subtitle consumer per pipeline and the
//! crate's feed is the probe point.

use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use fcastplaybin::{
    AudioSink, BitmapSubFormat, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks,
    StartPoint, SubtitleFeedItem, TrackSlot, TrackTarget,
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
/// One display set every 250 ms.
const SET_STEP: gst::ClockTime = gst::ClockTime::from_mseconds(250);
const MEDIA_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(120);

static PIPELINE: Mutex<()> = Mutex::new(());

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

/// Display sets from pts zero, one per [`SET_STEP`], with every fourth one
/// split across two buffers.
///
/// Starting at zero matters. The source covers dead air with a GAP, and
/// basesink prerolls only on buffers, so a set at pts zero is what puts a
/// buffer in the preroll slot for the paused tests below.
fn display_sets(count: u32) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = SET_STEP * u64::from(index);
            let end = start + SET_STEP / 2;
            let tag = index as u8;
            // The branch links some way into playback, so which sets a test
            // sees depends on scheduling. A fragmented set every fourth one is
            // seen whenever anything is.
            if index % 4 == 3 {
                CueSpec::packets(start, end, fcasttest::pgs::fragmented_display_set(tag))
            } else {
                CueSpec::packets(start, end, vec![fcasttest::pgs::display_set(tag)])
            }
        })
        .collect()
}

/// Subpicture units from pts zero, one per [`SET_STEP`], each lasting half a
/// step, so a position inside a unit's span is covered by it.
fn subpicture_units(count: u32) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = SET_STEP * u64::from(index);
            CueSpec::packets(
                start,
                start + SET_STEP / 2,
                vec![fcasttest::vobsub::subpicture_unit(index as u8)],
            )
        })
        .collect()
}

/// DVB display sets, one per [`SET_STEP`], with every fourth one split across
/// two packets.
fn dvb_sets(count: u32) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = SET_STEP * u64::from(index);
            let end = start + SET_STEP / 2;
            let tag = index as u8;
            if index % 4 == 3 {
                CueSpec::packets(start, end, fcasttest::dvb::fragmented_display_set(tag))
            } else {
                CueSpec::packets(start, end, vec![fcasttest::dvb::display_set(tag)])
            }
        })
        .collect()
}

fn dvb_scenario(key: &str) -> ScenarioHandle {
    ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(StreamSpec::text("text_0", dvb_sets(400)).with_caps(tcaps::dvb_caps()))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register()
}

fn vobsub_scenario(key: &str) -> ScenarioHandle {
    ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(
            StreamSpec::text("text_0", subpicture_units(400))
                .with_caps(tcaps::vobsub_caps(fcasttest::vobsub::SAMPLE_IDX)),
        )
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register()
}

fn scenario(key: &str) -> ScenarioHandle {
    ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(StreamSpec::text("text_0", display_sets(400)).with_caps(tcaps::pgs_caps()))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register()
}

fn gate() -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused: false,
        seekable: true,
    }
}

/// One playbin on the consumer arm, recording events and feed items.
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

    fn wait_for_text_sids(&self, count: usize) -> Vec<String> {
        self.wait_for("the text streams to be advertised", || {
            self.text_sids().len() >= count
        });
        self.text_sids()
    }

    fn select_subtitle(&self, sid: Option<&str>) {
        self.playbin.request_track(
            TrackSlot::Subtitle,
            TrackTarget::Stream(sid.map(|s| s.to_string())),
        );
        self.drain();
    }

    fn feed_len(&self) -> usize {
        self.feed.lock().len()
    }

    fn feed_since(&self, from: usize) -> Vec<SubtitleFeedItem> {
        self.feed.lock()[from..].to_vec()
    }

    fn wait_for_packets(&self, from: usize, count: usize) {
        self.wait_for_packets_within(from, count, FEED_BOUND);
    }

    /// The same, with the budget named. Real media has its own cadence and may
    /// not deliver inside the synthesized scenarios' bound.
    fn wait_for_packets_within(&self, from: usize, count: usize, budget: Duration) {
        self.wait_for_within(
            &format!("{count} bitmap packets to reach the consumer"),
            budget,
            &mut || packets_in(&self.feed_since(from)).len() >= count,
        );
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

    /// Names of the consumer arm's per-stream sinks currently in the pipeline.
    ///
    /// Tests only ask whether a branch exists, never its name. Pad replacement
    /// is normal, so a name is not an identity.
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

    fn pause(&self) {
        self.playbin.pause().expect("pause");
        self.wait_for("the pipeline to settle at PAUSED", || {
            let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Paused && pending == gst::State::VoidPending
        });
    }

    fn play(&self) {
        self.playbin.play().expect("play");
        self.wait_for("the pipeline to reach PLAYING", || {
            let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Playing && pending == gst::State::VoidPending
        });
    }

    fn state(&self) -> (gst::State, gst::State) {
        let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
        (current, pending)
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

/// Format, running time, payload size and whether caps carried `codec_data`.
fn packets_in(items: &[SubtitleFeedItem]) -> Vec<(BitmapSubFormat, gst::ClockTime, usize, bool)> {
    items
        .iter()
        .filter_map(|item| match item {
            SubtitleFeedItem::Bitmap {
                format,
                data,
                codec_data,
                rt,
                ..
            } => Some((*format, *rt, data.size(), codec_data.is_some())),
            SubtitleFeedItem::Cue { .. } | SubtitleFeedItem::Clear => None,
        })
        .collect()
}

fn clears_in(items: &[SubtitleFeedItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(item, SubtitleFeedItem::Clear))
        .count()
}

/// The first element in `bin`, recursively, built by the named factory.
fn find_element(bin: &gst::Bin, factory: &str) -> Option<gst::Element> {
    let mut iter = bin.iterate_recurse();
    while let Ok(Some(element)) = iter.next() {
        if element.factory().is_some_and(|f| f.name() == factory) {
            return Some(element);
        }
    }
    None
}

// ------------------------------------------------------------------ delivery

/// A PGS track selected, and what the consumer is handed.
///
/// Claims: the branch links; every item is a `Bitmap` of the format the caps
/// named, carrying bytes, in non-decreasing running time on the video base;
/// the fragmented display set arrives as the two buffers it was, sharing one
/// running time (the driver cannot know a set spans buffers, and the engine
/// cannot reassemble what it was never given); no `SubtitleTrackUnsupported`.
#[test]
fn a_pgs_track_reaches_the_consumer_as_bitmap_packets() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("bitmappgsfeed");
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_packets(from, 5);

    assert!(
        !harness.text_sinks().is_empty(),
        "no consumer branch was built for a format the renderer can draw"
    );
    let packets = packets_in(&harness.feed_since(from));
    assert!(
        packets
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Pgs),
        "the consumer was handed the wrong format: {:?}",
        packets
            .iter()
            .map(|(format, ..)| *format)
            .collect::<Vec<_>>()
    );
    assert!(
        packets.iter().all(|(_, _, bytes, _)| *bytes > 0),
        "a bitmap packet arrived with no payload"
    );
    assert!(
        packets.iter().all(|(.., codec_data)| !*codec_data),
        "PGS carries its palette in band; nothing should have attached codec_data"
    );
    assert!(
        packets.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "running times went backwards: {:?}",
        packets.iter().map(|(_, rt, ..)| *rt).collect::<Vec<_>>()
    );

    // This media starts at zero and never swaps items, so pts is the running
    // time and every packet must land on the sets' schedule. Which sets arrive
    // depends on when the branch linked, so assert the times, not a prefix.
    let times: Vec<_> = packets.iter().map(|(_, rt, ..)| *rt).collect();
    assert!(
        times
            .iter()
            .all(|rt| rt.nseconds() % SET_STEP.nseconds() == 0),
        "a packet arrived off the display sets' own schedule: {times:?}"
    );

    // Fragmented sets are two packets at one time. Eight packets guarantees at
    // least one, since every fourth set is split.
    harness.wait_for_packets(from, 8);
    let times: Vec<_> = packets_in(&harness.feed_since(from))
        .iter()
        .map(|(_, rt, ..)| *rt)
        .collect();
    assert!(
        times.windows(2).any(|pair| pair[0] == pair[1]),
        "no display set arrived as the two buffers it was written as: the driver is joining \
         or dropping fragments instead of forwarding them: {times:?}"
    );

    assert!(
        harness.unsupported_reports().is_empty(),
        "a format with a decoder behind it was reported unsupported: {:?}",
        harness.unsupported_reports()
    );

    harness.shutdown();
    media.unregister();
}

/// A flushing seek clears the consumer before it redelivers.
///
/// A PGS display set has no end time and stays on screen until replaced, so a
/// missing `Clear` leaves the pre-seek subtitle painted over the post-seek
/// picture.
#[test]
fn a_seek_clears_before_the_redelivered_display_sets() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("bitmappgsseek");
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_packets(from, 4);

    let mark = harness.feed_len();
    // The seek goes to the source element, which fans it out to every stream
    // as a real demuxer would. A seek from the video sink alone would never
    // flush the text branch.
    let source = find_element(harness.playbin.pipeline().upcast_ref(), "ftestsrc")
        .expect("the harness source is in the pipeline");
    assert!(
        source
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::ZERO,
            )
            .is_ok(),
        "the harness source refused a flushing seek to the start"
    );

    // The redelivered first set is named rather than counted. Packets in
    // flight at seek time land after the mark too. A packet at running time
    // zero can only come from the restart.
    harness.wait_for_within(
        "the redelivered first display set",
        EVENT_TIMEOUT,
        &mut || {
            packets_in(&harness.feed_since(mark))
                .iter()
                .any(|(_, rt, ..)| *rt == gst::ClockTime::ZERO)
        },
    );

    let after = harness.feed_since(mark);
    let redelivered = after
        .iter()
        .position(|item| {
            matches!(item, SubtitleFeedItem::Bitmap { rt, .. } if *rt == gst::ClockTime::ZERO)
        })
        .expect("waited for the redelivered set");
    assert!(
        clears_in(&after[..redelivered]) > 0,
        "the seek redelivered a display set with no Clear in front of it, so the renderer \
         would still be showing the pre-seek picture"
    );

    harness.shutdown();
    media.unregister();
}

// -------------------------------------------------------------------- paused

/// Load a scenario whose first display set sits at pts zero, settle at PAUSED,
/// select the track and make the branch deliver.
///
/// A pipeline at rest in PAUSED moves no data, so delivery while paused is
/// always a redelivery, driven here through the harness source.
fn paused_with_a_live_branch(key: &str) -> (Harness, ScenarioHandle) {
    let media = scenario(key);
    let harness = Harness::new();
    harness.load(&media);
    harness.pause();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for("the consumer branch to link while PAUSED", || {
        !harness.text_sinks().is_empty()
    });
    let source = find_element(harness.playbin.pipeline().upcast_ref(), "ftestsrc")
        .expect("the harness source is in the pipeline");
    assert!(
        source
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::ZERO,
            )
            .is_ok(),
        "the harness source refused a flushing seek at PAUSED"
    );
    (harness, media)
}

/// A display set reaches the consumer with the pipeline at rest in PAUSED and
/// no resume anywhere in the test.
///
/// Below PLAYING basesink routes the branch's first buffer through its preroll
/// path, which never reaches `new_sample`, so the consumer tail must install
/// both callbacks.
#[test]
fn a_paused_link_delivers_a_display_set_without_resuming() {
    let _lock = PIPELINE.lock();
    init();
    let (harness, media) = paused_with_a_live_branch("bitmappgspaused");
    let from = harness.feed_len();
    harness.wait_for_packets(from, 1);

    assert_eq!(
        harness.state(),
        (gst::State::Paused, gst::State::VoidPending),
        "the pipeline resumed on its own; the delivery proves nothing"
    );

    harness.shutdown();
    media.unregister();
}

/// A PAUSED track toggle does not wedge, the timeline survives it, and packets
/// flow again at resume.
///
/// A paused switch onto a PGS track does not generally put a picture on screen
/// while still paused. A PGS set is delivered once when it appears, so the set
/// covering the paused position predates any redelivery from the seek point.
/// VOBSUB, self-contained per packet, is the format that claims the instant
/// case. What must never happen is a wedge or a timeline reset.
#[test]
fn a_paused_pgs_toggle_does_not_wedge_and_delivers_at_resume() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("bitmappgstoggle");
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_packets(from, 4);

    // Far enough in that a timeline reset would be unmistakable.
    harness.wait_for_within(
        "the position to pass half a second",
        FEED_BOUND,
        &mut || {
            harness
                .playbin
                .position()
                .is_some_and(|position| position > gst::ClockTime::from_mseconds(500))
        },
    );
    harness.pause();
    let before = harness
        .playbin
        .position()
        .expect("a paused pipeline still has a position");

    // Toggle off, then on again, at rest in PAUSED.
    harness.select_subtitle(None);
    harness.wait_for("the branch to go away", || harness.text_sinks().is_empty());
    let mark = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));

    // No wedge. The pipeline stays where it was told to be and the poll that
    // services the switch keeps running.
    let settle = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < settle {
        harness.drain();
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        harness.state(),
        (gst::State::Paused, gst::State::VoidPending),
        "the paused toggle moved the pipeline out of PAUSED"
    );

    // A flush that reset the running time would put the position back at the
    // start.
    let after_toggle = harness
        .playbin
        .position()
        .expect("a paused pipeline still has a position");
    assert!(
        after_toggle + gst::ClockTime::from_mseconds(250) >= before,
        "the paused toggle sent the position backwards: {before} -> {after_toggle}"
    );

    // At resume the branch is joined and packets flow again. Not how many,
    // since a redelivery may repeat what was already sent.
    harness.play();
    harness.wait_for_packets(mark, 1);
    assert!(
        !harness.text_sinks().is_empty(),
        "the track never re-joined after the toggle"
    );
    let resumed = packets_in(&harness.feed_since(mark));
    assert!(
        resumed
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Pgs),
        "something other than PGS packets arrived after the toggle"
    );
    assert!(
        harness.unsupported_reports().is_empty(),
        "the toggle produced an unsupported-track report: {:?}",
        harness.unsupported_reports()
    );

    harness.shutdown();
    media.unregister();
}

// ------------------------------------------------------------------- VOBSUB

/// The VOBSUB transport leg. Same claims as the PGS one, plus the out-of-band
/// palette this format carries on the caps.
#[test]
fn a_vobsub_track_reaches_the_consumer_with_its_palette() {
    let _lock = PIPELINE.lock();
    init();
    let media = vobsub_scenario("bitmapvobfeed");
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_packets(from, 4);

    assert!(
        !harness.text_sinks().is_empty(),
        "no consumer branch was built"
    );
    let packets = packets_in(&harness.feed_since(from));
    assert!(
        packets
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Vobsub),
        "the consumer was handed the wrong format"
    );
    assert!(
        packets.iter().all(|(.., codec_data)| *codec_data),
        "VOBSUB's palette rides on the caps, and every packet must arrive with it: \
         a decoder handed the picture without the palette draws a grey ramp"
    );
    assert!(
        packets.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "running times went backwards"
    );
    assert!(
        harness.unsupported_reports().is_empty(),
        "a format with a decoder behind it was reported unsupported"
    );

    harness.shutdown();
    media.unregister();
}

/// A subtitle covering the frozen frame reaches the consumer at a settled
/// PAUSED, with no resume anywhere in the test.
///
/// A VOBSUB packet is self-contained (picture, palette indices, position,
/// schedule), so redelivering the packet at the paused position hands the
/// renderer the whole subtitle. PGS cannot make this claim; see the toggle
/// test above.
#[test]
fn a_paused_vobsub_redelivery_covers_the_frozen_frame() {
    let _lock = PIPELINE.lock();
    init();
    let media = vobsub_scenario("bitmapvobpaused");
    let harness = Harness::new();
    harness.load(&media);
    harness.pause();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for("the consumer branch to link while PAUSED", || {
        !harness.text_sinks().is_empty()
    });

    // The position is a unit's own start. A flushing seek starts the segment
    // at the requested position, so a packet whose pts precedes it has no
    // running time and is dropped. A unit that merely covers the position is
    // out of reach of a seek-driven redelivery.
    let position = SET_STEP * 4;
    let from = harness.feed_len();
    let source = find_element(harness.playbin.pipeline().upcast_ref(), "ftestsrc")
        .expect("the harness source is in the pipeline");
    assert!(
        source
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position)
            .is_ok(),
        "the harness source refused a flushing seek at PAUSED"
    );

    harness.wait_for_within(
        "a subpicture covering the frozen position",
        FEED_BOUND,
        &mut || !packets_in(&harness.feed_since(from)).is_empty(),
    );

    // STILL PAUSED. The delivery proves nothing if the pipeline resumed to get
    // it.
    assert_eq!(
        harness.state(),
        (gst::State::Paused, gst::State::VoidPending),
        "the pipeline resumed on its own"
    );
    let covering = packets_in(&harness.feed_since(from));
    assert!(
        covering
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Vobsub),
        "something other than a subpicture arrived"
    );
    assert!(
        covering.iter().all(|(.., codec_data)| *codec_data),
        "the redelivered packet arrived without the palette it needs"
    );
    // The covering unit is at running time zero on the new segment, so the
    // packet a renderer needs is the first thing it gets.
    assert_eq!(
        covering[0].1,
        gst::ClockTime::ZERO,
        "the redelivery did not start at the position it was asked for"
    );

    harness.shutdown();
    media.unregister();
}

/// The real sample, start to seek to end, with zero decode errors.
///
/// Everything else in this file is synthesized. This sample was muxed
/// elsewhere and plays through the real demuxer, so it can catch wrong
/// assumptions about what the field actually sends. The seek asserts that no
/// packet after it depends on state the decoder never saw.
#[test]
fn the_real_vobsub_sample_plays_through_a_seek_without_a_decode_error() {
    let _lock = PIPELINE.lock();
    init();
    let Some(uri) = sample_media() else {
        eprintln!(
            "skipping: ../fcast-sample-media/video/video_with_vobsub.mkv is not present \
             (the sample repository is separate and its media are untracked)"
        );
        return;
    };

    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(uri),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_for("the load to finish", || {
        harness
            .events
            .lock()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::Loaded { .. }))
    });
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    // From the start of the feed, not from here. The only subtitle track is
    // selected by default, so packets can be delivered while the collection is
    // still being waited on.
    let from = 0;
    harness.wait_for_packets_within(from, 1, EVENT_TIMEOUT);

    let before = packets_in(&harness.feed_since(from));
    assert!(
        before
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Vobsub),
        "the real sample's subtitle track is not being carried as VOBSUB"
    );
    assert!(
        before.iter().all(|(.., codec_data)| *codec_data),
        "matroska did not attach the .idx as codec_data, which is the assumption \
         the whole out-of-band palette path rests on"
    );

    // Through a seek, where a stateful decoder would report errors for objects
    // it never saw. The target sits near the sample's own subtitle schedule to
    // keep the wait short. A seek not issued at a settled PAUSED is handed
    // back as `QueueSeek`, so this settles first, seeks, then resumes, as a
    // receiver does.
    let mark = harness.feed_len();
    harness.pause();
    harness.playbin.seek_async(fcastplaybin::Seek::new(
        Some(gst::ClockTime::from_mseconds(50_000)),
        None,
    ));
    harness.wait_for("the seek to settle", || {
        harness
            .playbin
            .position()
            .is_some_and(|position| position >= gst::ClockTime::from_mseconds(49_000))
    });
    harness.play();
    harness.wait_for_packets_within(mark, 1, EVENT_TIMEOUT);

    assert!(
        harness.unsupported_reports().is_empty(),
        "the real sample's subtitle track was reported unsupported"
    );
    let after = packets_in(&harness.feed_since(mark));
    assert!(
        after.iter().all(|(_, _, bytes, _)| *bytes > 0),
        "an empty packet after the seek"
    );

    harness.shutdown();
}

/// The real sample, or `None` when the separate media repository is not
/// checked out beside this one.
fn sample_media() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fcast-sample-media/video/video_with_vobsub.mkv");
    let path = path.canonicalize().ok()?;
    path.is_file().then(|| format!("file://{}", path.display()))
}

// ---------------------------------------------------------------------- DVB

/// The DVB transport leg. A page's segments reach the consumer as the packets
/// they were, in order, on the video's base.
///
/// A DVB display set may span several packets sharing one timestamp, and only
/// the last carries the end-of-display-set segment, so dropping, merging or
/// reordering packets leaves the decoder with a page it can never close.
#[test]
fn a_dvb_track_reaches_the_consumer_as_its_own_packets() {
    let _lock = PIPELINE.lock();
    init();
    let media = dvb_scenario("bitmapdvbfeed");
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_packets(from, 8);

    assert!(
        !harness.text_sinks().is_empty(),
        "no consumer branch was built"
    );
    let packets = packets_in(&harness.feed_since(from));
    assert!(
        packets
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Dvb),
        "the consumer was handed the wrong format"
    );
    assert!(
        packets.iter().all(|(_, _, bytes, _)| *bytes > 0),
        "an empty packet"
    );
    let times: Vec<_> = packets.iter().map(|(_, rt, ..)| *rt).collect();
    assert!(
        times.windows(2).all(|pair| pair[0] <= pair[1]),
        "running times went backwards: {times:?}"
    );
    assert!(
        times
            .iter()
            .all(|rt| rt.nseconds() % SET_STEP.nseconds() == 0),
        "a packet arrived off the display sets' own schedule: {times:?}"
    );
    assert!(
        times.windows(2).any(|pair| pair[0] == pair[1]),
        "no display set arrived as the several packets it was written as: the driver is \
         joining or dropping them instead of forwarding them: {times:?}"
    );
    assert!(
        harness.unsupported_reports().is_empty(),
        "a format with a decoder behind it was reported unsupported"
    );

    harness.shutdown();
    media.unregister();
}

/// A DVB track selected, the pipeline PLAYING, the position advancing.
#[test]
fn a_dvb_track_does_not_wedge_the_pipeline() {
    let _lock = PIPELINE.lock();
    init();
    let media = dvb_scenario("bitmapdvbwedge");
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_packets(from, 4);

    assert_eq!(
        harness.state(),
        (gst::State::Playing, gst::State::VoidPending),
        "the pipeline left PLAYING while the DVB track was rendering"
    );
    let before = harness.playbin.position();
    harness.wait_for_within("the position to advance", FEED_BOUND, &mut || {
        harness.playbin.position() > before
    });

    harness.shutdown();
    media.unregister();
}

/// The generated transport stream through the real demuxer, with the DVB
/// track selected.
///
/// The fixture comes from an independent implementation of the format
/// (`tools/make-dvb-fixture.sh`), so this test can catch wrong assumptions
/// about the framing the field actually delivers.
#[test]
fn the_generated_transport_stream_reaches_the_consumer_through_tsdemux() {
    let _lock = PIPELINE.lock();
    init();
    let Some(uri) = dvb_fixture() else {
        eprintln!(
            "skipping: ../fcast-sample-media/video/dvb_subtitles.ts is not present \
             (regenerate it with tools/make-dvb-fixture.sh)"
        );
        return;
    };

    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(uri),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_for("the load to finish", || {
        harness
            .events
            .lock()
            .iter()
            .any(|event| matches!(event, PlaybinEvent::Loaded { .. }))
    });
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    // From the start of the feed. Subtitles begin immediately and the only
    // subtitle track is selected by default, so packets can arrive while the
    // collection is still being waited on.
    harness.wait_for_packets_within(0, 2, EVENT_TIMEOUT);

    let packets = packets_in(&harness.feed_since(0));
    assert!(
        packets
            .iter()
            .all(|(format, ..)| *format == BitmapSubFormat::Dvb),
        "the transport stream's subtitle track is not being carried as DVB"
    );
    assert!(
        packets.iter().all(|(_, _, bytes, _)| *bytes > 0),
        "an empty packet came out of tsdemux"
    );
    assert!(
        harness.unsupported_reports().is_empty(),
        "the real stream's subtitle track was reported unsupported: {:?}",
        harness.unsupported_reports()
    );
    assert_eq!(
        harness.state(),
        (gst::State::Playing, gst::State::VoidPending),
        "the pipeline left PLAYING while a real DVB track was rendering"
    );

    harness.shutdown();
}

/// The generated transport stream, or `None` when the separate media
/// repository is not checked out beside this one.
fn dvb_fixture() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fcast-sample-media/video/dvb_subtitles.ts");
    let path = path.canonicalize().ok()?;
    path.is_file().then(|| format!("file://{}", path.display()))
}
