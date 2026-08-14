//! The subtitle CONSUMER transport, the DEFAULT now,
//! and now the only one.
//!
//! The text branch ends in a per-stream `appsink` and cues leave the driver
//! through the callback installed by
//! [`FcastPlaybin::set_subtitle_consumer`], where subtitleoverlay used to
//! composite them. The consumer IS the probe point here, the rest of the
//! suites reach it through `support/text_arm.rs`.
//!
//! Three groups:
//!
//! * **T3** the flows: link, switch, disable, dispose.
//! * **T4** `Clear` coherence: a seek mid-cue clears before the redelivered
//!   cue; a load supersession clears.
//! * **T5** capability loudness: caps the renderer cannot read leave the branch
//!   parked, report exactly one `SubtitleTrackUnsupported`, and do not wedge
//!   the pipeline.
//!
//! # Why these tests serialize
//!
//! Two pipelines at once would be two subtitle consumers at once. (Until step
//! 6 this file also WROTE its lever into the process environment, because the
//! transport was opt-in and read once per pipeline at construction; the flip
//! retired the write and the lever went with it.)
//!
//! # One harness change here
//!
//! `fcasttest`'s `text_%u` pad template widened from `text/x-raw, format=utf8`
//! to `text/x-raw; subpicture/x-dvd`, so a spec can advertise caps the
//! renderer refuses (`StreamSpec::with_caps`, used by the T5 pair below).

use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use fcastplaybin::{
    AudioSink, BitmapSubFormat, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks,
    StartPoint, SubtitleFeedItem, SubtitleTextFormat, TrackSlot, TrackTarget,
};
use fcasttest::{
    caps as tcaps,
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::{FTestSink, Recording},
    spec::{CueSpec, Pacing, StreamSpec},
};
use gst::prelude::*;

/// Generous bound for anything driven by realtime playback.
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a `Clear` or a first cue may take to reach the consumer once the
/// thing that should produce it has been asked for. Far above the microseconds
/// a healthy hand-off costs and far below [`EVENT_TIMEOUT`].
const FEED_BOUND: Duration = Duration::from_secs(10);

/// Cue cadence. Dense enough that a few seconds of playback produce several
/// cues, sparse enough to stay a genuinely SPARSE stream.
const CUE_STEP: gst::ClockTime = gst::ClockTime::from_mseconds(250);

/// Long enough that no test reaches EOS while it is still asserting.
const MEDIA_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(120);

/// Serializes the whole file. Two pipelines at once would also be two
/// consumers at once, and [`init`] writes the process environment.
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

fn cues(count: u32, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = CUE_STEP * u64::from(index + 1);
            CueSpec::new(start, start + CUE_STEP / 2, format!("{tag}{index:03}"))
        })
        .collect()
}

/// Cues starting at pts ZERO, so the branch's very first delivery is a BUFFER.
///
/// This matters and is not cosmetic. `ftestsrc` emits a GAP to cover the dead
/// air before the first cue, and basesink calls its `preroll` vfunc only for
/// BUFFERS (`gstbasesink.c`: the `if (buf)` around `bclass->preroll`). A GAP
/// therefore completes the branch's one preroll WITHOUT reaching the appsink's
/// callback, and the buffer behind it parks in `wait_preroll` until PLAYING.
/// A leading cue at zero puts a buffer in the preroll slot, which is the case
/// the paused path is about.
fn cues_from_zero(count: u32, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = CUE_STEP * u64::from(index);
            CueSpec::new(start, start + CUE_STEP / 2, format!("{tag}{index:03}"))
        })
        .collect()
}

fn gate() -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused: false,
        seekable: true,
    }
}

/// One playbin on the consumer arm, with the consumer and the event handler
/// both recording into vectors the test reads.
struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: Arc<Mutex<Vec<PlaybinEvent>>>,
    feed: Arc<Mutex<Vec<SubtitleFeedItem>>>,
    /// What the video sink has shown. The frame on screen is one half of every
    /// cue-timing question -- a cue is being shown iff it covers that frame's
    /// running time -- and while PAUSED it is the prerolled one.
    video: Recording,
    /// How far `drain` has read the event log.
    cursor: Mutex<usize>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(video_sink.upcast()),
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
            video,
            cursor: Mutex::new(0),
        }
    }

    /// One turn of the caller's pump, plus the error check every wait needs.
    /// Only NEW events are inspected, so a long wait does not re-scan the
    /// whole log on every tick.
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

    /// Text stream ids from the most recent collection, in collection order.
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

    /// Wait for `count` more CUES (not `Clear`s) beyond `from`.
    fn wait_for_cues(&self, from: usize, count: usize) {
        self.wait_for_within(
            &format!("{count} cues to reach the consumer"),
            FEED_BOUND,
            &mut || cues_in(&self.feed_since(from)).len() >= count,
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

    /// The running time of the frame the video sink is showing.
    ///
    /// The pts comes off the sink's own recording (a PREROLL while PAUSED, a
    /// buffer while playing). Turning it into a running time wants the
    /// governing SEGMENT, and where that is readable -- the last pad inside
    /// the crate the frame crossed -- it is used. Where it is not, the pts IS
    /// the running time: every case in this file loads from zero and never
    /// swaps items, so the segment is `[0, inf)` with base zero throughout,
    /// and the cue running times on the other side of the comparison are
    /// computed against the same one. The distinction would matter across a
    /// gapless boundary, which this file does not have; `regression_gapless.rs`
    /// and `gapless_timeline.rs` own that case, at pads that do carry it.
    fn frame_running_time(&self) -> Option<gst::ClockTime> {
        let pts = self
            .video
            .snapshot()
            .iter()
            .rev()
            .find_map(|entry| entry.pts())?;
        let segment = self
            .playbin
            .video_sink()
            .static_pad("sink")
            .and_then(|pad| pad.sticky_event::<gst::event::Segment>(0));
        match segment {
            Some(event) => event
                .segment()
                .downcast_ref::<gst::ClockTime>()
                .and_then(|segment| segment.to_running_time(pts)),
            None => Some(pts),
        }
    }

    fn pause(&self) {
        self.playbin.pause().expect("pause");
        self.wait_for("the pipeline to settle at PAUSED", || {
            let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Paused && pending == gst::State::VoidPending
        });
    }

    /// Give up the playbin so a test can control when the last reference dies.
    fn into_playbin(self) -> Arc<FcastPlaybin> {
        self.playbin
    }

    fn play(&self) {
        self.playbin.play().expect("play");
        self.wait_for("the pipeline to reach PLAYING", || {
            let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == gst::State::Playing && pending == gst::State::VoidPending
        });
    }

    /// Shut down, and prove the shutdown itself completes: every test ends
    /// here, so a disposal that wedges fails the test that provoked it rather
    /// than hanging the binary.
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

fn cues_in(
    items: &[SubtitleFeedItem],
) -> Vec<(
    SubtitleTextFormat,
    String,
    gst::ClockTime,
    Option<gst::ClockTime>,
)> {
    items
        .iter()
        .filter_map(|item| match item {
            SubtitleFeedItem::Cue {
                format,
                text,
                start_rt,
                end_rt,
                origin: _,
            } => Some((format.clone(), text.clone(), *start_rt, *end_rt)),
            // A bitmap packet is not a cue: it carries no text and no end.
            // Named rather than caught by `_`, see [`bitmaps_in`], which is
            // the other half of this pair.
            SubtitleFeedItem::Bitmap { .. } | SubtitleFeedItem::Clear => None,
        })
        .collect()
}

/// The bitmap twin of [`cues_in`]: format, running time and payload size.
///
/// The size rather than the bytes, because the driver is a byte pipe for these
/// formats, what a test can meaningfully assert here is that a non-empty
/// payload arrived, in order, tagged with the format the caps named. What the
/// bytes MEAN is `fcast-video`'s question and is asked in its own vectors.
fn bitmaps_in(items: &[SubtitleFeedItem]) -> Vec<(BitmapSubFormat, gst::ClockTime, usize)> {
    items
        .iter()
        .filter_map(|item| match item {
            SubtitleFeedItem::Bitmap {
                format, data, rt, ..
            } => Some((*format, *rt, data.size())),
            SubtitleFeedItem::Cue { .. } | SubtitleFeedItem::Clear => None,
        })
        .collect()
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

fn clears_in(items: &[SubtitleFeedItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(item, SubtitleFeedItem::Clear))
        .count()
}

/// A scenario with video, audio and `text_count` embedded text tracks, each
/// tagged so a cue names the track it came from.
fn scenario(key: &str, text_count: usize) -> ScenarioHandle {
    let mut builder = ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime);
    for index in 0..text_count {
        let tag = (b'A' + index as u8) as char;
        builder = builder.text(format!("text_{index}"), cues(400, &tag.to_string()));
    }
    builder.register()
}

// ---------------------------------------------------------------- T3: flows

/// LINK. The cues of a selected embedded track arrive at the consumer, in
/// running time, in the caps' format.
#[test]
fn cues_reach_the_consumer() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("sinksublink", 1);
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));

    harness.wait_for("the consumer branch to be built", || {
        !harness.text_sinks().is_empty()
    });
    let from = harness.feed_len();
    harness.wait_for_cues(from, 3);

    let delivered = cues_in(&harness.feed_since(from));
    for (format, text, start_rt, end_rt) in &delivered {
        assert_eq!(
            *format,
            SubtitleTextFormat::Utf8,
            "the harness serves text/x-raw,format=utf8"
        );
        assert!(
            text.starts_with('A'),
            "cue text {text:?} is not this track's payload"
        );
        let end = end_rt.expect("the harness gives every cue a duration");
        assert!(
            end > *start_rt,
            "cue {text:?} ends at {end} before it starts at {start_rt}"
        );
    }
    let starts: Vec<_> = delivered.iter().map(|(_, _, start, _)| *start).collect();
    assert!(
        starts.windows(2).all(|pair| pair[0] <= pair[1]),
        "cue running times are not monotonic: {starts:?}"
    );

    harness.shutdown();
    media.unregister();
}

/// SWITCH. A track change clears what the consumer holds BEFORE the new
/// track's first cue, and no cue of the outgoing track survives the clear.
#[test]
fn switching_tracks_clears_before_the_new_track_arrives() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("sinksubswitch", 2);
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(2);
    harness.select_subtitle(Some(&sids[0]));
    let first = harness.feed_len();
    harness.wait_for_cues(first, 2);

    let mark = harness.feed_len();
    harness.select_subtitle(Some(&sids[1]));
    harness.wait_for_within(
        "the second track's cues to reach the consumer",
        FEED_BOUND,
        &mut || {
            cues_in(&harness.feed_since(mark))
                .iter()
                .any(|(_, text, _, _)| text.starts_with('B'))
        },
    );

    let after = harness.feed_since(mark);
    let first_b = after
        .iter()
        .position(
            |item| matches!(item, SubtitleFeedItem::Cue { text, .. } if text.starts_with('B')),
        )
        .expect("waited for a B cue");
    // The LAST Clear before the first B cue, not any Clear. Asking only
    // "is there a Clear somewhere in front" accepts [A, Clear, A_stale, B] --
    // a stale outgoing cue delivered AFTER the clear that was supposed to
    // retire it, which is exactly the residual this assertion exists to catch.
    let last_clear = after[..first_b]
        .iter()
        .rposition(|item| matches!(item, SubtitleFeedItem::Clear))
        .unwrap_or_else(|| {
            panic!(
                "the switch delivered the new track's first cue with no Clear in front of it: \
                 a renderer would keep showing the outgoing track's cue. Feed: {:?}",
                &after[..=first_b]
            )
        });
    assert!(
        !after[last_clear..].iter().any(
            |item| matches!(item, SubtitleFeedItem::Cue { text, .. } if text.starts_with('A'))
        ),
        "an outgoing-track cue arrived after the Clear that retired it: {:?}",
        &after[last_clear..]
    );

    harness.shutdown();
    media.unregister();
}

/// DISABLE. Turning subtitles off clears and stops the feed.
#[test]
fn disabling_subtitles_clears_and_stops_the_feed() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("sinksuboff", 1);
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    let from = harness.feed_len();
    harness.wait_for_cues(from, 2);

    let mark = harness.feed_len();
    harness.select_subtitle(None);
    harness.wait_for_within("the disable to clear the consumer", FEED_BOUND, &mut || {
        clears_in(&harness.feed_since(mark)) > 0
    });
    harness.wait_for_within(
        "the consumer branch to be torn down",
        FEED_BOUND,
        &mut || harness.text_sinks().is_empty(),
    );

    // The feed is now quiet. Several cue periods of pumping must add nothing:
    // a branch that survived the disable would keep delivering.
    let quiet = harness.feed_len();
    let settle = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < settle {
        harness.drain();
        thread::sleep(Duration::from_millis(10));
    }
    let after = harness.feed_since(quiet);
    assert!(
        cues_in(&after).is_empty(),
        "cues kept arriving after subtitles were turned off: {after:?}"
    );

    harness.shutdown();
    media.unregister();
}

/// DISPOSE. Repeated link/unlink cycles leave nothing behind and the teardown
/// completes: the consumer branch's disposal skips the overlay seat entirely
/// (there is none), so this is where that path would wedge if it did not.
#[test]
fn repeated_link_and_dispose_cycles_leave_no_branch_behind() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("sinksubcycle", 1);
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);

    for round in 0..4 {
        harness.select_subtitle(Some(&sids[0]));
        harness.wait_for(
            &format!("round {round}: the consumer branch to link"),
            || !harness.text_sinks().is_empty(),
        );
        harness.select_subtitle(None);
        harness.wait_for(
            &format!("round {round}: the consumer branch to be disposed of"),
            || harness.text_sinks().is_empty(),
        );
    }

    harness.shutdown();
    media.unregister();
}

// ------------------------------------------------------ T4: Clear coherence

/// A flushing seek while a cue is live clears the consumer BEFORE the
/// redelivered cue arrives. Without it the renderer would show the old
/// timeline's cue against the new one's frames.
#[test]
fn a_seek_clears_before_the_redelivered_cue() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("sinksubseek", 1);
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    let from = harness.feed_len();
    harness.wait_for_cues(from, 3);

    let mark = harness.feed_len();
    // THE SEEK IS SENT TO THE SOURCE, deliberately, and this is a property of
    // the harness rather than a shortcut. `ftestsrc` serves each stream from
    // its own pad and handles a seek per-pad, so a seek travelling upstream
    // from the video sink restarts video alone: the text pad never sees it and
    // the text branch is never flushed. Only `ftestsrc`'s element-level
    // `send_event` fans the seek out to every stream, which is what a real
    // demuxer does with one upstream seek, and a flush reaching the text
    // branch is the whole subject here.
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
    // The REDELIVERED cue, named rather than counted: cues already in flight
    // when the seek was issued land after `mark` too, and they belong to the
    // timeline being left. `A000` is the media's first cue, so it can only be
    // here because the seek restarted the stream.
    const REDELIVERED: &str = "A000";
    harness.wait_for_within("the redelivered first cue", EVENT_TIMEOUT, &mut || {
        cues_in(&harness.feed_since(mark))
            .iter()
            .any(|(_, text, _, _)| text == REDELIVERED)
    });

    let after = harness.feed_since(mark);
    let redelivered = after
        .iter()
        .position(|item| matches!(item, SubtitleFeedItem::Cue { text, .. } if text == REDELIVERED))
        .expect("waited for the redelivered cue");
    assert!(
        after[..redelivered]
            .iter()
            .any(|item| matches!(item, SubtitleFeedItem::Clear)),
        "the seek redelivered a cue with no Clear in front of it, so a renderer would \
         still be showing the pre-seek cue: {:?}",
        &after[..=redelivered]
    );

    harness.shutdown();
    media.unregister();
}

/// A load that supersedes the running item clears the consumer. The old
/// item's cues describe a timeline the pipeline has left; nothing else in the
/// crate tells the renderer so, because a load need not flush the old text
/// branch at all.
#[test]
fn a_load_supersession_clears_the_consumer() {
    let _lock = PIPELINE.lock();
    init();
    let first = scenario("sinksubload1", 1);
    let second = scenario("sinksubload2", 1);
    let harness = Harness::new();
    harness.load(&first);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));
    let from = harness.feed_len();
    harness.wait_for_cues(from, 2);

    let mark = harness.feed_len();
    harness.load(&second);
    assert!(
        clears_in(&harness.feed_since(mark)) > 0,
        "the load left the consumer holding the previous item's cues: {:?}",
        harness.feed_since(mark)
    );

    harness.shutdown();
    first.unregister();
    second.unregister();
}

// -------------------------------------------------- T5: capability loudness

/// Shared body for a bitmap format that HAS a decoder: the branch links, the
/// packets arrive as `Bitmap` items of the right format, no unsupported-track
/// report goes out however long the poll runs, and the pipeline plays on.
///
/// The inverse of [`unrenderable_track_is_loud_and_harmless`], deliberately
/// clause for clause: VOBSUB and DVB move from that body to this one as their
/// decoders land, and the two being mirror images is what makes the move
/// a one-line change and a visible one.
fn bitmap_track_is_carried_and_quiet(key: &str, caps: gst::Caps, format: BitmapSubFormat) {
    // THE MASTER LEVER TAKES THIS CLAIM AWAY ON PURPOSE. With
    // `FCAST_NO_BITMAP_SUBS=1` no bitmap track is carried at all, which is the
    // rollback working rather than a failure, so the three flipped cases
    // report NO VERDICT there instead of asserting a carriage the lever
    // forbids. Their old loud contract is not lost: `tests/bitmap_lever.rs` is
    // that contract, kept, with the lever in front of it.
    if std::env::var_os("FCAST_NO_BITMAP_SUBS").is_some() {
        println!(
            "NO VERDICT: FCAST_NO_BITMAP_SUBS is set, so {key} is refused rather than carried; \
             tests/bitmap_lever.rs owns that arm"
        );
        return;
    }
    let cues = (0..400u32)
        .map(|index| {
            let start = CUE_STEP * u64::from(index);
            // Each format's own bytes: a PGS display set is a run of segments,
            // a VOBSUB subpicture unit is one self-sized packet, and the driver
            // is supposed to be indifferent to which, that indifference is
            // what this shared body is testing.
            let payload = match format {
                BitmapSubFormat::Pgs => vec![fcasttest::pgs::display_set(index as u8)],
                BitmapSubFormat::Vobsub => vec![fcasttest::vobsub::subpicture_unit(index as u8)],
                BitmapSubFormat::Dvb => vec![fcasttest::dvb::display_set(index as u8)],
            };
            CueSpec::packets(start, start + CUE_STEP / 2, payload)
        })
        .collect();
    let media = ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(StreamSpec::text("text_0", cues).with_caps(caps))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register();
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    let from = harness.feed_len();
    harness.select_subtitle(Some(&sids[0]));

    harness.wait_for_within("the first bitmap packet", FEED_BOUND, &mut || {
        !bitmaps_in(&harness.feed_since(from)).is_empty()
    });
    assert!(
        !harness.text_sinks().is_empty(),
        "no consumer branch was built for a format the renderer can draw"
    );

    // Poll well past the point where a report would show up, exactly as the
    // loud body does: a format that is carried must be carried QUIETLY.
    let settle = Instant::now() + Duration::from_millis(2000);
    while Instant::now() < settle {
        harness.drain();
        thread::sleep(Duration::from_millis(10));
    }

    let reports = harness.unsupported_reports();
    assert!(
        reports.is_empty(),
        "a format with a decoder behind it was reported unsupported: {reports:?}"
    );
    let packets = bitmaps_in(&harness.feed_since(from));
    assert!(
        packets.iter().all(|(carried, ..)| *carried == format),
        "the consumer was handed the wrong bitmap format: {:?}",
        packets
            .iter()
            .map(|(format, ..)| *format)
            .collect::<Vec<_>>()
    );
    assert!(
        packets.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "the packets' running times went backwards: {:?}",
        packets.iter().map(|(_, rt, _)| *rt).collect::<Vec<_>>()
    );
    assert!(
        packets.iter().all(|(_, _, bytes)| *bytes > 0),
        "a bitmap packet arrived empty"
    );

    // NO WEDGE, the half of the loud body that survives the flip unchanged.
    let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(
        (current, pending),
        (gst::State::Playing, gst::State::VoidPending),
        "the pipeline left PLAYING while the bitmap track was rendering"
    );
    let before = harness.playbin.position();
    harness.wait_for_within("the position to advance", FEED_BOUND, &mut || {
        harness.playbin.position() > before
    });

    harness.shutdown();
    media.unregister();
}

/// Shared body for the two unrenderable-caps cases: the branch stays parked,
/// exactly one report goes out however long the poll runs, no cue is ever
/// delivered, and the pipeline plays on.
fn unrenderable_track_is_loud_and_harmless(key: &str, caps: gst::Caps) {
    let media = ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(StreamSpec::text("text_0", cues(400, "X")).with_caps(caps.clone()))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register();
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(1);
    harness.select_subtitle(Some(&sids[0]));

    harness.wait_for_within("the unsupported-track report", FEED_BOUND, &mut || {
        !harness.unsupported_reports().is_empty()
    });

    // Keep polling well past the point where a re-report would show up: the
    // link poll runs on every pump and every tick, and the whole value of the
    // dedupe is that a permanently unrenderable track does not become an
    // event storm.
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
        "a consumer branch was built for caps the renderer cannot read: {:?}",
        harness.text_sinks()
    );
    assert!(
        cues_in(&harness.feed_since(0)).is_empty(),
        "cues were delivered for an unrenderable track"
    );

    // NO WEDGE: the pipeline is still playing and its position still advances,
    // which is what a parked-but-undrained sparse stream would prevent.
    let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(
        (current, pending),
        (gst::State::Playing, gst::State::VoidPending),
        "the pipeline left PLAYING while the unrenderable track was parked"
    );
    let before = harness.playbin.position();
    harness.wait_for_within("the position to advance", FEED_BOUND, &mut || {
        harness.playbin.position() > before
    });

    harness.shutdown();
    media.unregister();
}

/// VOBSUB (`subpicture/x-dvd`), RENDERED, the second of the three to stop
/// being a loudness case.
///
/// This is the ORIGINAL of these tests, the one that guarded the loud path for
/// every bitmap format before the gate learned to answer per format. It keeps
/// its subject and inverts its claim: the branch links, the packets reach the
/// consumer, and the `SubtitleTrackUnsupported` it used to demand never fires.
/// Its old contract lives on under the master lever, in
/// `tests/bitmap_lever.rs`.
///
/// The caps here carry `codec_data`, which is the real matroska shape and the
/// one thing VOBSUB needs that the other two do not: its palette is out of
/// band.
#[test]
fn subpicture_subtitles_are_carried_instead_of_refused() {
    let _lock = PIPELINE.lock();
    init();
    bitmap_track_is_carried_and_quiet(
        "sinksubpicture",
        tcaps::vobsub_caps(fcasttest::vobsub::SAMPLE_IDX),
        BitmapSubFormat::Vobsub,
    );
}

/// PGS (`subpicture/x-pgs`), RENDERED, the first of the three to stop being a
/// loudness case.
///
/// This is the same test as its neighbours, inverted: the branch links, the
/// packets reach the consumer, and the `SubtitleTrackUnsupported` that used to
/// be the whole point never fires. Its old contract is not gone, it is
/// asserted under the master lever, in `tests/bitmap_lever.rs`, which is where
/// a rollback would be caught.
#[test]
fn pgs_subtitles_are_carried_instead_of_refused() {
    let _lock = PIPELINE.lock();
    init();
    bitmap_track_is_carried_and_quiet("sinksubpgs", tcaps::pgs_caps(), BitmapSubFormat::Pgs);
}

/// DVB (`subpicture/x-dvb`), RENDERED, the last of the three to stop being a
/// loudness case.
///
/// With this one inverted, every bitmap subtitle format the protocol
/// advertises is carried, and the loud path below belongs permanently to the
/// two formats nothing plans to draw: `subpicture/x-xsub` and unparsed ASS.
#[test]
fn dvb_subtitles_are_carried_instead_of_refused() {
    let _lock = PIPELINE.lock();
    init();
    bitmap_track_is_carried_and_quiet("sinksubdvb", tcaps::dvb_caps(), BitmapSubFormat::Dvb);
}

/// Raw ASS/SSA: `text/x-raw` in a format nothing here can lay out. Through the
/// production parse path ASS arrives as pango-markup and renders; unparsed, it
/// takes the same loud refusal as a bitmap track.
#[test]
fn raw_ass_text_is_refused_loudly_and_stays_parked() {
    let _lock = PIPELINE.lock();
    init();
    unrenderable_track_is_loud_and_harmless("sinksubass", tcaps::raw_ass_text_caps());
}

// ------------------------------------------- PAUSED: delivery and teardown

/// Build a scenario whose text track's first cue sits at pts zero, load it,
/// settle at PAUSED, select the track and make the branch DELIVER. Returns the
/// harness with a live consumer branch and the pipeline still at PAUSED.
///
/// # Why a seek is part of "delivering while paused"
///
/// A pipeline at rest in PAUSED moves no data at all: every sink has prerolled
/// and parked, the multiqueues are full behind them, and the source is
/// back-pressured. A text branch that links there receives NOTHING, and no
/// amount of waiting changes that -- measured, with a pad probe on the
/// appsink's sink pad recording zero events and zero buffers over ten seconds.
///
/// So delivery while paused is always somebody's REDELIVERY, which is exactly
/// what the driver already does for a paused track switch: a flushing seek to
/// the current position (`FcastPlaybin::refresh_seek_async`) re-prerolls every
/// branch and pushes the cue covering that position. This drives the same
/// mechanism through the harness source (per T4's note, `ftestsrc` fans an
/// element-level seek out to every stream, which a real demuxer does with one
/// upstream seek), so the branch sees FLUSH_STOP, a fresh segment and the cue
/// at pts zero -- a BUFFER in the preroll slot, which is the case
/// `new_preroll` exists for.
///
/// # The mark, and why the callers need one
///
/// The third return value is `feed_len()` taken IMMEDIATELY BEFORE the seek,
/// and every caller that is about the redelivery must read the feed from it
/// rather than from zero.
///
/// The reason is `Inner::take_parked_text_cues`: a text pad is parked from
/// the moment it is routed, the park KEEPS what it consumes, and the join
/// hands it back. So a branch that links at PAUSED now delivers the cues that
/// crossed while the link was still being decided, and the feed is no longer
/// empty when the seek goes out. Reading from zero measures that replay
/// instead of the redelivery, and the two are different mechanisms with
/// different evidence: the replay is the park's memory, the redelivery is the
/// preroll path these tests exist to pin. Reading from zero also made the
/// tests' own "never resumed" check race the seek it had not waited for.
fn paused_with_a_live_branch(key: &str) -> (Harness, ScenarioHandle, usize) {
    let media = ScenarioBuilder::new(key)
        .video("video_0")
        .audio("audio_0")
        .stream(StreamSpec::text("text_0", cues_from_zero(400, "P")))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register();
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
    let before_seek = harness.feed_len();
    assert!(
        source
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::ZERO,
            )
            .is_ok(),
        "the harness source refused a flushing seek at PAUSED"
    );
    (harness, media, before_seek)
}

/// THE PAUSED PREMISE. A cue reaches the consumer with the pipeline at rest in
/// PAUSED and no resume anywhere in the test.
///
/// Below PLAYING basesink routes the branch's first buffer through its PREROLL
/// path, which reaches `new_preroll` and never `new_sample`. With only a
/// `new_sample` callback installed (and `emit-signals` false, so the signal
/// fires into nothing) this delivers precisely nothing, and the whole reason
/// for moving cues out of subtitleoverlay -- a track switch that shows its cue
/// while paused -- is unreachable. RED without the `new_preroll` callback.
#[test]
fn a_paused_link_delivers_its_cue_without_resuming() {
    let _lock = PIPELINE.lock();
    init();
    let (harness, media, before_seek) = paused_with_a_live_branch("sinksubpaused");

    // From the SEEK's mark, so what this waits for is the redelivery through
    // the preroll path and not the park's replay (see the helper).
    harness.wait_for_within(
        "a cue to reach the consumer while PAUSED",
        FEED_BOUND,
        &mut || !cues_in(&harness.feed_since(before_seek)).is_empty(),
    );
    let delivered = cues_in(&harness.feed_since(before_seek));
    assert!(
        delivered
            .iter()
            .all(|(_, text, _, _)| text.starts_with('P')),
        "unexpected cue payload while paused: {delivered:?}"
    );

    // Never resumed: the whole point.
    let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(
        (current, pending),
        (gst::State::Paused, gst::State::VoidPending),
        "the test resumed the pipeline, so it proves nothing about PAUSED"
    );

    harness.shutdown();
    media.unregister();
}

/// A consumer branch disposed of AT REST IN PAUSED, and a shutdown behind it.
///
/// The branch's tqueue loop is parked inside the appsink's `wait_preroll` by
/// then: basesink prerolls one object and blocks the next until PLAYING,
/// whatever `async` says. Nothing but a flush at the appsink's own sink pad
/// wakes it, so this wedges without the disposal pair (`FlushReason::
/// DisposalConsumer`/`TeardownConsumer`).
#[test]
fn subtitles_off_and_shutdown_from_paused_do_not_wedge() {
    let _lock = PIPELINE.lock();
    init();
    let (harness, media, before_seek) = paused_with_a_live_branch("sinksubpausedoff");
    harness.wait_for_within(
        "a cue to reach the consumer while PAUSED",
        FEED_BOUND,
        &mut || !cues_in(&harness.feed_since(before_seek)).is_empty(),
    );

    // The disable itself must return promptly; a caller blocked behind a
    // parked branch is the field deadlock this crate is built around.
    let mark = harness.feed_len();
    let started = Instant::now();
    harness.select_subtitle(None);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "turning subtitles off at rest in PAUSED blocked the caller for {:?}",
        started.elapsed()
    );
    harness.wait_for_within("the disable to clear the consumer", FEED_BOUND, &mut || {
        clears_in(&harness.feed_since(mark)) > 0
    });

    // And the branch LEAVES, at a resting PAUSED, without waiting for a
    // resume. The overlay arm postpones this disposal (its parked push is
    // inside subtitleoverlay and nothing there could wake it); a consumer
    // branch's is inside its own appsink, which the disposal pair flushes,
    // so `detach_text_parts` runs it inline. See
    // [`a_paused_disposal_frees_the_branch_for_the_next_link`] for what the
    // postponement costs when it does not.
    harness.wait_for_within("the branch to leave the graph", FEED_BOUND, &mut || {
        harness.text_sinks().is_empty()
    });

    harness.shutdown();
    media.unregister();
}

/// A branch disposed of at a resting PAUSED frees its NAME, so the next track
/// can be wired without a resume.
///
/// The bite-proof for disposing consumer branches inline at PAUSED
/// (`Inner::detach_text_parts`). A branch queue is named after the decodebin3
/// pad it serves (`fpb-tqueue-<pad>`), and decodebin3 hands the same pad to the
/// next stream it serves; `gst_bin_add` refuses a duplicate name. So a
/// postponed disposal does not merely leave a stale element behind, it makes
/// the NEXT branch unbuildable -- measured as `failed to add the text queue`
/// once per poll, for as long as the pipeline stays paused, with the incoming
/// track never linked and no cue after the switch.
///
/// RED with the postponement restored (`FCAST_NO_TEXT_WORK_DEFERRAL` is the
/// other direction -- it forces the inline path everywhere -- so the red arm
/// here is the code change itself).
#[test]
fn a_paused_disposal_frees_the_branch_for_the_next_link() {
    let _lock = PIPELINE.lock();
    init();
    let (harness, media, before_seek) = paused_with_a_live_branch("sinksubpausedrelink");
    harness.wait_for_within(
        "a cue to reach the consumer while PAUSED",
        FEED_BOUND,
        &mut || !cues_in(&harness.feed_since(before_seek)).is_empty(),
    );
    let sid = harness.text_sids().first().cloned().expect("a text stream");

    harness.select_subtitle(None);
    harness.wait_for_within("the branch to leave the graph", FEED_BOUND, &mut || {
        harness.text_sinks().is_empty()
    });

    // The same stream again, still paused: a fresh branch has to be buildable.
    harness.select_subtitle(Some(&sid));
    harness.wait_for_within(
        "a fresh consumer branch to be wired while PAUSED",
        FEED_BOUND,
        &mut || !harness.text_sinks().is_empty(),
    );

    let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(
        (current, pending),
        (gst::State::Paused, gst::State::VoidPending),
        "the pipeline left PAUSED, so this proves nothing about a paused relink"
    );

    harness.shutdown();
    media.unregister();
}

/// THE DROP PATH: the last reference goes away while a consumer branch is live
/// and its queue is parked inside the appsink's preroll wait.
///
/// A LIVENESS GUARD, not a pair-D proof, and the difference is worth naming. A
/// branch that is still ROUTED is not in `deferred_text_disposal`, so
/// `Teardown::run` does not dispose of it at all -- it rides the pipeline's
/// bounded descent, and the descent is itself what takes the sink out of
/// `wait_preroll`. The disposal geometry that genuinely needs the flush pair is
/// the INLINE one below. This test exists because "the drop path must not hang"
/// is worth an assertion of its own whatever the mechanism behind it.
#[test]
fn dropping_the_playbin_from_paused_with_a_live_branch_is_bounded() {
    let _lock = PIPELINE.lock();
    init();
    let (harness, media, before_seek) = paused_with_a_live_branch("sinksubpauseddrop");
    harness.wait_for_within(
        "a cue to reach the consumer while PAUSED",
        FEED_BOUND,
        &mut || !cues_in(&harness.feed_since(before_seek)).is_empty(),
    );

    // A healthy teardown of this graph is milliseconds; the budget is
    // enormous by comparison so a slow box can never be the reason it fires.
    const DROP_BOUND: Duration = Duration::from_secs(30);
    let playbin = harness.into_playbin();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("paused-drop".into())
        .spawn(move || {
            drop(playbin);
            let _ = tx.send(());
        })
        .expect("spawning the drop thread");

    match rx.recv_timeout(DROP_BOUND) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "dropping the playbin from PAUSED with a live consumer branch did not return \
             within {DROP_BOUND:?}: the branch is parked in the appsink's preroll wait and \
             the teardown has nothing left to wake it"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the drop thread died"),
    }
    media.unregister();
}

// --------------------------------------------------- T5, continued (F9 iii)

/// An unrenderable track must not poison the slot: a renderable one selected
/// afterwards links and renders normally.
#[test]
fn a_renderable_track_selects_after_an_unsupported_one() {
    let _lock = PIPELINE.lock();
    init();
    let media = ScenarioBuilder::new("sinksubmixed")
        .video("video_0")
        .audio("audio_0")
        // RAW ASS, not a subpicture format. These two tests are about the
        // report machinery, the slot recovering, the dedupe re-arming across a
        // load, so what they need is caps that are PERMANENTLY unrenderable.
        // `subpicture/x-dvd` was that until it got a decoder, and
        // whichever bitmap format is used here would break the same way when
        // its own vertical lands; raw ASS never will.
        .stream(StreamSpec::text("text_0", cues(400, "X")).with_caps(tcaps::raw_ass_text_caps()))
        .stream(StreamSpec::text("text_1", cues(400, "R")))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register();
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(2);

    harness.select_subtitle(Some(&sids[0]));
    harness.wait_for_within("the unsupported-track report", FEED_BOUND, &mut || {
        !harness.unsupported_reports().is_empty()
    });
    assert!(
        harness.text_sinks().is_empty(),
        "a consumer branch was built for the unrenderable track"
    );

    harness.select_subtitle(Some(&sids[1]));
    let mark = harness.feed_len();
    harness.wait_for_cues(mark, 2);
    let delivered = cues_in(&harness.feed_since(mark));
    assert!(
        delivered
            .iter()
            .all(|(_, text, _, _)| text.starts_with('R')),
        "the renderable track delivered something else: {delivered:?}"
    );
    assert_eq!(
        harness.unsupported_reports().len(),
        1,
        "selecting a renderable track re-reported the unsupported one"
    );

    harness.shutdown();
    media.unregister();
}

/// The dedupe is per (sid, LOAD), not forever: a second load of the same media
/// reports the same unrenderable track again, because it is news again.
#[test]
fn a_second_load_reports_the_unsupported_track_again() {
    let _lock = PIPELINE.lock();
    init();
    let media = ScenarioBuilder::new("sinksubreload")
        .video("video_0")
        .audio("audio_0")
        // RAW ASS, not a subpicture format. These two tests are about the
        // report machinery, the slot recovering, the dedupe re-arming across a
        // load, so what they need is caps that are PERMANENTLY unrenderable.
        // `subpicture/x-dvd` was that until it got a decoder, and
        // whichever bitmap format is used here would break the same way when
        // its own vertical lands; raw ASS never will.
        .stream(StreamSpec::text("text_0", cues(400, "X")).with_caps(tcaps::raw_ass_text_caps()))
        .duration(MEDIA_DURATION)
        .pacing(Pacing::Realtime)
        .register();
    let harness = Harness::new();

    for round in 0..2 {
        harness.load(&media);
        harness.play();
        let sids = harness.wait_for_text_sids(1);
        harness.select_subtitle(Some(&sids[0]));
        harness.wait_for_within(
            &format!("round {round}: the unsupported-track report"),
            FEED_BOUND,
            &mut || harness.unsupported_reports().len() > round,
        );
        assert_eq!(
            harness.unsupported_reports().len(),
            round + 1,
            "round {round} produced the wrong number of reports"
        );
    }

    harness.shutdown();
    media.unregister();
}

/// THE PAIR-D GEOMETRY, reproduced: a MID-PLAY disposal of a branch whose
/// queue is parked inside the appsink's preroll wait.
///
/// Every other disposal path is covered by something else. A deferred one is
/// drained at a teardown boundary, where pair E is sent unconditionally and its
/// FLUSH_START travels through the queue to the appsink, waking the parked
/// push on the way. A live branch at drop rides the descent. What is left is
/// the mid-play disposal, where pair E is SKIPPED whenever the branch
/// quiesces -- and it does quiesce, because the quiescence probe trylocks the
/// queue's SINK pad while the parked push holds its SRC pad's stream lock.
/// `tqueue.set_state(Null)` then joins that loop task and never returns.
///
/// `FCAST_NO_TEXT_WORK_DEFERRAL` is what puts a resting-PAUSED detach on the
/// mid-play path; the geometry it exposes is the same one a detach during any
/// non-resting transition reaches on its own.
#[test]
fn an_inline_disposal_of_a_parked_paused_branch_does_not_wedge() {
    let _lock = PIPELINE.lock();
    init();
    let (harness, media, before_seek) = paused_with_a_live_branch("sinksubpausedinline");
    harness.wait_for_within(
        "a cue to reach the consumer while PAUSED",
        FEED_BOUND,
        &mut || !cues_in(&harness.feed_since(before_seek)).is_empty(),
    );
    // Let the branch's queue get properly parked pushing the cue BEHIND the
    // prerolled one into the appsink: that parked push is the whole subject.
    thread::sleep(Duration::from_millis(300));

    // SAFETY: `PIPELINE` is held, so no other test in this binary is running.
    unsafe { std::env::set_var("FCAST_NO_TEXT_WORK_DEFERRAL", "1") };
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: as above, the pipeline lock is still held.
            unsafe { std::env::remove_var("FCAST_NO_TEXT_WORK_DEFERRAL") };
        }
    }
    let _restore = Restore;

    // On a build without the pair this never returns, so it runs off the test
    // thread and the assertion stays on it.
    const DISPOSAL_BOUND: Duration = Duration::from_secs(30);
    let playbin = harness.playbin.clone();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("paused-inline-off".into())
        .spawn(move || {
            playbin.request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
            playbin.poll_text_policy();
            playbin.pump_selection(gate());
            let _ = tx.send(());
        })
        .expect("spawning the disable thread");

    match rx.recv_timeout(DISPOSAL_BOUND) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Do NOT let the playbin drop: its teardown walks the same parked
            // branch and would hang the binary instead of reporting.
            std::mem::forget(harness);
            panic!(
                "an inline disposal of a paused consumer branch did not return within \
                 {DISPOSAL_BOUND:?}: its queue is parked inside the appsink's preroll wait \
                 and the disposal has nothing left to wake it"
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the disable thread died"),
    }

    // THE ASSERTION THAT BITES. Without the pair the disable CALL still
    // returns -- the disposal runs on the worker -- and it is the worker that
    // wedges, joining the queue's parked loop task inside
    // `tqueue.set_state(Null)`. So "the caller came back" is not enough; the
    // branch has to actually leave the graph.
    {
        let deadline = Instant::now() + FEED_BOUND;
        while !harness.text_sinks().is_empty() {
            if Instant::now() >= deadline {
                // The worker is wedged on the branch; its teardown would hang
                // the binary rather than report.
                std::mem::forget(harness);
                panic!(
                    "the consumer branch never left the graph after an inline disposal: \
                     the disposal is joining the queue's loop task, which is parked inside \
                     the appsink's preroll wait with nothing to wake it"
                );
            }
            harness.drain();
            thread::sleep(Duration::from_millis(10));
        }
    }
    harness.shutdown();
    media.unregister();
}

// ---------------------------------- T7: the paused cue, in front of a frame

/// How long a cue redelivered while paused may take to reach the renderer.
///
/// It is a LATENCY bound, not a liveness one: the
/// work between the redelivery and the delivery is a flush, a segment and one
/// buffer through an unsynced branch.
const PAUSED_SWITCH_BOUND: Duration = Duration::from_millis(200);

/// T7, the transport's half: a cue delivered at rest in PAUSED COVERS the
/// frame on screen, and gets there inside the bound, with no resume.
///
/// The step past [`a_paused_link_delivers_its_cue_without_resuming`], which
/// asks only whether anything arrived: a cue that arrives but does not cover
/// the frozen frame changes nothing a viewer can see. The assertion here is
/// the renderer's own comparison -- `start_rt <= frame_rt < end_rt`, the rule
/// `fcast-video`'s cue engine applies in `overlays_for` -- against the running
/// time of the frame the video sink is actually showing, read off the sink's
/// recording and its pad's segment.
///
/// # The half of T7 that does not hold, and why it is not forced
///
/// The plan words T7 as an EMBEDDED TRACK SWITCH at a settled PAUSED: the
/// driver's own `Command::RefreshSeek` redelivering the covering cue. That
/// trigger is not reachable, and the obstacle is upstream of everything this
/// phase touches. Measured three ways, on `ftest://` media and on a real
/// matroska clip with two muxed SubRip tracks:
///
/// * the SELECT_STREAMS naming the incoming track goes out and decodebin3 never
///   activates it. Activating a stream needs data to move through the
///   multiqueue, and at a settled PAUSED every sink has prerolled and parked,
///   so nothing moves. No confirmation, no link, no refresh seek: the switch
///   never gets far enough for a redelivery to be owed;
/// * passing through subtitles-OFF first does not help. A DISABLE is
///   deliberately excluded from the refresh (`selection.rs`: flushing across a
///   branch teardown wedges), so it moves no data either, and the engine is
///   still awaiting its unconfirmed seqnum when the next request arrives -- the
///   switch behind it never dispatches at all;
/// * on `ftest://` media the redelivery cannot even be manufactured from
///   outside: a pipeline seek reaches the crate's SINK-flagged children (and
///   the consumer branch's appsink deliberately is not one), so it arrives at
///   the source through the video path, and `ftestsrc` restarts one stream per
///   src pad. Only an ELEMENT-level seek fans out, which is why
///   [`paused_with_a_live_branch`] reaches in and seeks the element.
///
/// So the claim is exactly this test's claim, and no more: WHEN a cue
/// is redelivered while paused, it is in front of the renderer immediately and
/// covers the frozen frame. The undelivered case is already carved out as a
/// documented residual ("the flip claims the delivered case, and the residual
/// is documented"); this records that an embedded switch at a settled PAUSED is
/// one of them, on decodebin3's account rather than the transport's.
///
/// # Where the other half lives
///
/// The delivery is paired with "`engine.current_overlays()` is non-empty".
/// That cannot be asserted here: `fcastplaybin` does not depend on
/// `fcast-video` and must not, and a
/// dev-dependency would unify `gst`'s feature set across the two crates,
/// silently lifting this crate's deliberate `v1_22` pin to `v1_26` for every
/// test build. So the pair is split at the callback boundary and joined by the
/// QUANTITY both sides use: this test pins that the transport delivers a cue
/// whose window covers the frozen frame's running time, and `fcast_video::
/// cue`'s `a_paused_cue_covering_the_frozen_frame_reaches_the_screen` pins
/// that the engine, given exactly that, turns it into a non-empty
/// `current_overlays()` with no new frame.
#[test]
fn a_paused_cue_covers_the_frozen_frame_without_resuming() {
    let _lock = PIPELINE.lock();
    init();
    // Returns with the redelivery already sent (its last act is the seek) and
    // the pipeline still PAUSED, so this is a tight upper bound on when the
    // cue was owed.
    let (harness, media, before_seek) = paused_with_a_live_branch("sinksubpausedcover");
    let redelivered = Instant::now();

    let frame_rt = harness
        .frame_running_time()
        .expect("the video sink has prerolled a frame at a settled PAUSED");
    // From the SEEK's mark: the claim is about what the REDELIVERY puts on the
    // frozen frame, and the park's replay would otherwise answer for it (see
    // `paused_with_a_live_branch`).
    harness.wait_for_within(
        "a cue COVERING the frozen frame to reach the consumer while PAUSED",
        PAUSED_SWITCH_BOUND,
        &mut || {
            cues_in(&harness.feed_since(before_seek))
                .iter()
                .any(|(_, _, start_rt, end_rt)| {
                    *start_rt <= frame_rt && end_rt.is_none_or(|end| end > frame_rt)
                })
        },
    );
    let elapsed = redelivered.elapsed();

    // Never resumed. Without this the test would pass on a build where the cue
    // only appears once frames flow again, which is the contract the consumer
    // transport replaces.
    let (_, current, pending) = harness.playbin.pipeline().state(gst::ClockTime::ZERO);
    assert_eq!(
        (current, pending),
        (gst::State::Paused, gst::State::VoidPending),
        "the pipeline left PAUSED, so this proves nothing about a paused cue"
    );
    println!("paused cue covering {frame_rt} at the consumer within {elapsed:?}");

    harness.shutdown();
    media.unregister();
}

// --------------------------------- T8: cue-at-consumer to composited frame

/// T8, integration half. A cue delivered to the consumer is picked up by the
/// renderer's own comparison on the NEXT frame, and it gets to the consumer
/// inside the switch-latency suite's CUE bound.
///
/// The bound is `switch_latency_probe.rs`'s `CUE_BOUND`, carried here rather
/// than re-derived: that suite measures the same quantity on the overlay arm
/// (a switch request to the first cue of the new track) and this one measures
/// it "at the consumer", so the number has to be the same number
/// or the comparison is not a comparison.
///
/// "Picked up on the next frame" is asserted the way the renderer decides it:
/// the frames that flow AFTER the cue arrives are checked against the cue's
/// window with `overlays_for`'s rule, and one of them must fall inside it.
/// The synthetic frame is the video sink's own -- a real buffer at a real
/// running time, not a number the test invents.
///
/// The raster half of T8 (submit to pixels, under 50ms warm at p99) is
/// `fcast_video::cue`'s `raster_latency_stays_under_the_gate_when_warm`: it is
/// the renderer's own instrument and needs no pipeline.
const CUE_BOUND: Duration = Duration::from_millis(1500);

#[test]
fn a_delivered_cue_covers_a_frame_within_the_cue_bound() {
    let _lock = PIPELINE.lock();
    init();
    let media = scenario("sinksubcuebound", 2);
    let harness = Harness::new();
    harness.load(&media);
    harness.play();
    let sids = harness.wait_for_text_sids(2);

    harness.select_subtitle(Some(&sids[0]));
    let first = harness.feed_len();
    harness.wait_for_cues(first, 1);

    // The switch, timed at the consumer.
    let mark = harness.feed_len();
    let requested = Instant::now();
    harness.select_subtitle(Some(&sids[1]));
    harness.wait_for_within(
        "the switched-to track's first cue to reach the consumer",
        CUE_BOUND,
        &mut || {
            cues_in(&harness.feed_since(mark))
                .iter()
                .any(|(_, text, _, _)| text.starts_with('B'))
        },
    );
    let cue_latency = requested.elapsed();

    let (_, _, start_rt, end_rt) = cues_in(&harness.feed_since(mark))
        .into_iter()
        .find(|(_, text, _, _)| text.starts_with('B'))
        .expect("waited for a B cue");

    // The pickup: a frame whose running time falls inside that cue's window
    // has to go past. The cue is delivered EARLY (the branch is unsynced), so
    // this is a wait, and its bound is the cue's own start relative to where
    // playback is -- generously covered by FEED_BOUND at this cadence.
    harness.wait_for_within("a frame the delivered cue covers", FEED_BOUND, &mut || {
        harness
            .frame_running_time()
            .is_some_and(|frame_rt| start_rt <= frame_rt && end_rt.is_none_or(|end| end > frame_rt))
    });
    println!(
        "cue at the consumer in {cue_latency:?} (bound {CUE_BOUND:?}), window \
         {start_rt}..{end_rt:?} covered a frame"
    );

    harness.shutdown();
    media.unregister();
}
