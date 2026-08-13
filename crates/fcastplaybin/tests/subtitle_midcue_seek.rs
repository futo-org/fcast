//! A seek that lands INSIDE a cue must show that cue.
//!
//! The demuxers resend the covering unit with its ORIGINAL pts, which now
//! precedes the new segment's start, measured, before this suite existed, on
//! both containers the receiver plays:
//!
//! ```text
//! matroskademux  cue 1s..3s, seek to 2.0s -> resent pts=1.0s, segment.start=2.0s
//! qtdemux (tx3g) cue 5s..6s, seek to 5.5s -> resent pts=5.0s, segment.start=5.5s
//! ```
//!
//! The transport used to drop exactly that unit (`to_running_time` answers
//! nothing outside the segment), which is why a paused seek left the frozen
//! frame bare and a playing seek showed nothing until the NEXT cue began. The
//! bounds arithmetic is pinned by the units in `src/tests.rs`; these two are
//! the end-to-end statements, through a real demuxer.

use std::{
    cell::Cell,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Seek, SelectionGate, Sinks, StartPoint,
    SubtitleFeedItem, TrackSlot, TrackTarget,
};
use gst::prelude::*;
use parking_lot::Mutex;

#[path = "support/text_arm.rs"]
mod text_arm;

mod support;

/// Generous bound for anything driven by realtime playback.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The cue under test: 1s -> 3s, so 2s is strictly inside it.
const CUE_START: u64 = 1;
const CUE_END: u64 = 3;
const SEEK_TO: u64 = 2;
const SPANNING: &str = "SPANNING";

/// Two pipelines at once would be two subtitle consumers at once.
static PIPELINE: Mutex<()> = Mutex::new(());

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

fn plugins_available() -> bool {
    [
        "videotestsrc",
        "audiotestsrc",
        "vp8enc",
        "vorbisenc",
        "matroskamux",
        "matroskademux",
        "subparse",
        "decodebin3",
    ]
    .iter()
    .all(|f| gst::ElementFactory::find(f).is_some())
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("fcastplaybin-midcue-{}-{name}", std::process::id()))
}

fn run_to_eos(desc: &str) {
    let pipeline = gst::parse::launch(desc).expect("encode pipeline parses");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("the encode finishes");
    if let gst::MessageView::Error(err) = msg.view() {
        panic!(
            "encode pipeline failed: {} ({:?})",
            err.error(),
            err.debug()
        );
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// 8s of A/V with ONE long cue over `CUE_START..CUE_END` and a far-away second
/// one. The gap matters: if the driver only ever showed the cue AFTER the seek
/// target, the second cue would hide the bug.
fn subtitled_mkv() -> String {
    let srt = tmp("midcue.srt");
    std::fs::write(
        &srt,
        format!(
            "1\n00:00:0{CUE_START},000 --> 00:00:0{CUE_END},000\n{SPANNING}\n\n\
             2\n00:00:07,000 --> 00:00:08,000\nLATER\n\n"
        ),
    )
    .expect("writing the srt");
    let path = tmp("midcue.mkv");
    run_to_eos(&format!(
        "videotestsrc num-buffers=240 pattern=black \
           ! video/x-raw,width=320,height=240,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers=344 wave=silence ! audioconvert ! vorbisenc ! mux. \
         filesrc location={} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        srt.display(),
        path.display()
    ));
    format!("file://{}", path.display())
}

/// How many wall-to-wall one-second cues [`subtitled_mp4_dir`] writes.
const HTTP_CUES: u64 = 12;

/// The `n`th cue's text in the HTTP fixture.
fn http_cue(n: u64) -> String {
    format!("HTTPCUE-{n:02}")
}

/// Whether the MP4 leg's encoders and the HTTP source are all present.
fn mp4_plugins_available() -> bool {
    [
        "videotestsrc",
        "audiotestsrc",
        "openh264enc",
        "h264parse",
        "avenc_aac",
        "mp4mux",
        "qtdemux",
        "souphttpsrc",
        "subparse",
        "decodebin3",
    ]
    .iter()
    .all(|f| gst::ElementFactory::find(f).is_some())
}

/// A DIRECTORY holding one `field.mp4`: 12 s of A/V with a tx3g text track
/// whose cues are WALL-TO-WALL one-second records, so every seek target is
/// covered by a cue and no tolerance is needed to have something to show.
///
/// A directory rather than a file because the point of this fixture is the
/// TRANSPORT: [`support::FileServer`] serves a directory, and serving it is
/// what puts `souphttpsrc` in front of qtdemux and qtdemux into PUSH mode.
///
/// `faststart=true` so the `moov` is at the head, which is what a sender
/// streams and what keeps the run free of a second round trip for the atom.
fn subtitled_mp4_dir() -> std::path::PathBuf {
    let dir = tmp("http-mp4");
    std::fs::create_dir_all(&dir).expect("creating the served directory");
    let path = dir.join("field.mp4");
    let srt = tmp("http-mp4.srt");
    let mut cues = String::new();
    for n in 0..HTTP_CUES {
        cues.push_str(&format!(
            "{}\n00:00:{:02},000 --> 00:00:{:02},000\n{}\n\n",
            n + 1,
            n,
            n + 1,
            http_cue(n)
        ));
    }
    std::fs::write(&srt, cues).expect("writing the srt");
    run_to_eos(&format!(
        "videotestsrc num-buffers={} pattern=black \
           ! video/x-raw,width=320,height=240,framerate=30/1 \
           ! openh264enc ! h264parse ! mux. \
         audiotestsrc num-buffers={} wave=silence ! audioconvert \
           ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! mux. \
         filesrc location={} ! subparse ! mux. \
         mp4mux name=mux faststart=true ! filesink location={}",
        HTTP_CUES * 30,
        HTTP_CUES * 48000 / 1024 + 2,
        srt.display(),
        path.display()
    ));
    dir
}

struct Harness {
    playbin: FcastPlaybin,
    feed: Arc<Mutex<Vec<SubtitleFeedItem>>>,
    events: mpsc::Receiver<PlaybinEvent>,
    log: Mutex<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    /// A seek the driver refused and parked, for the caller to re-issue.
    parked_seek: Cell<Option<Seek>>,
    /// Set by a lost-state edge, cleared by the settle that follows.
    lost_state: Cell<bool>,
    /// The transport the caller wants, which a parked seek must not lose.
    want_playing: Cell<bool>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: None,
            audio: AudioSink::Factory(Box::new(|| {
                Ok(gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?)
            })),
        })
        .expect("building fcastplaybin");
        let feed: Arc<Mutex<Vec<SubtitleFeedItem>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = feed.clone();
        playbin.set_subtitle_consumer(move |item| sink.lock().push(item));
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        // NOT `text_arm::arm`: that installs a consumer of its own, and
        // `set_subtitle_consumer` takes the last one. This suite reads the
        // feed directly and only borrows `text_arm`'s pad inspection.
        Self {
            playbin,
            feed,
            events,
            log: Mutex::new(Vec::new()),
            paused: Cell::new(true),
            parked_seek: Cell::new(None),
            lost_state: Cell::new(false),
            want_playing: Cell::new(false),
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: true,
        }
    }

    /// The receiver's settle-point call, run from every wait so the link
    /// policy gets constant chances to act.
    fn pump(&self) {
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok(event) = self.events.try_recv() {
            match &event {
                PlaybinEvent::QueueSeek(seek) => self.parked_seek.set(Some(*seek)),
                PlaybinEvent::StateChanged {
                    current: gst::State::Paused,
                    pending: gst::State::Paused,
                    ..
                } => self.lost_state.set(true),
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    ..
                } => self.lost_state.set(false),
                _ => {}
            }
            self.log.lock().push(event);
        }
    }

    /// Put back the transport the driver parked. `Job::Seek` refuses a seek
    /// that did not arrive at a settled PAUSED, posts `QueueSeek` and commits
    /// PAUSED expecting the caller to re-issue; a pipeline that loses state
    /// will not resume by itself either. The receiver's state machine does
    /// both, and without them this suite would sit at PAUSED for good.
    fn redrive(&self) {
        if !self.playbin.is_settled() {
            return;
        }
        if let Some(seek) = self.parked_seek.take() {
            self.playbin.seek_async(seek);
            return;
        }
        if self.want_playing.get()
            && (self.lost_state.get() || self.playbin.state_summary().0 != gst::State::Playing)
        {
            self.lost_state.set(false);
            let _ = self.playbin.play();
            self.paused.set(false);
        }
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.drain_events();
            self.redrive();
            self.pump();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn text_sids(&self) -> Vec<String> {
        self.log
            .lock()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => Some(
                    collection
                        .iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::TEXT))
                        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Every delivered cue with its running-time WINDOW, which is what tells a
    /// zero-length twin from the cue it stands in front of.
    fn cue_windows(&self) -> Vec<(String, gst::ClockTime, Option<gst::ClockTime>)> {
        self.feed
            .lock()
            .iter()
            .filter_map(|item| match item {
                SubtitleFeedItem::Cue {
                    text,
                    start_rt,
                    end_rt,
                    ..
                } => Some((text.clone(), *start_rt, *end_rt)),
                _ => None,
            })
            .collect()
    }

    fn cues(&self) -> Vec<String> {
        self.feed
            .lock()
            .iter()
            .filter_map(|item| match item {
                SubtitleFeedItem::Cue { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Select the one embedded text track and wait for its branch to link.
    fn select_the_text_track(&self) {
        self.wait_for("the text stream to be advertised", |h| {
            !h.text_sids().is_empty()
        });
        let sid = self.text_sids().remove(0);
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
        self.pump();
        self.wait_for("the text branch to link", |_| {
            text_arm::text_branch_linked(&self.playbin)
        });
    }

    /// Load the media, select the one embedded text track and reach PLAYING
    /// with cues actually arriving -- the state the field reports the defect
    /// from.
    fn playing_with_subtitles() -> Self {
        Self::playing_with_subtitles_from(subtitled_mkv())
    }

    /// [`Harness::playing_with_subtitles`] against any URI.
    fn playing_with_subtitles_from(media: String) -> Self {
        let harness = Self::new();
        harness
            .playbin
            .load(
                MediaInput::Uri(media),
                StartPoint::Seek {
                    position: gst::ClockTime::ZERO,
                    rate: 1.0,
                },
            )
            .expect("the load prerolls");
        harness.playbin.play().expect("play");
        harness.paused.set(false);
        harness.want_playing.set(true);
        harness.select_the_text_track();
        harness.wait_for("the first cue", |h| !h.cues().is_empty());
        harness
    }

    /// Load at `start`, ask for the one embedded text track, and NEVER call
    /// [`FcastPlaybin::play`], the transport stays PAUSED from the load
    /// onwards.
    ///
    /// The shape [`Harness::playing_with_subtitles`] cannot express, and the
    /// reason the defect below stayed uncovered: every other text guard in
    /// this crate reaches PLAYING first, and PLAYING brings joins and drains
    /// of its own that re-drive the text link policy.
    ///
    /// Deliberately does NOT wait for the branch to link. The claim these
    /// tests make is about the CUE, and a constructor that waited would move
    /// the failure off the assertion and onto itself.
    fn paused_at(media: String, start: gst::ClockTime) -> Self {
        let harness = Self::new();
        harness
            .playbin
            .load(
                MediaInput::Uri(media),
                StartPoint::Seek {
                    position: start,
                    rate: 1.0,
                },
            )
            .expect("the load prerolls");
        // `paused` stays true and `want_playing` stays false, so `redrive`
        // never resumes what this test is pinning to PAUSED.
        harness.wait_for("the load to settle paused", |h| {
            h.playbin.state_summary().0 == gst::State::Paused && h.playbin.is_settled()
        });
        harness.wait_for("the text stream to be advertised", |h| {
            !h.text_sids().is_empty()
        });
        let sid = harness.text_sids().remove(0);
        harness
            .playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
        harness.pump();
        harness
    }

    /// The seek under test. The driver may park it; [`Harness::redrive`] is
    /// what puts it back, exactly as the receiver does.
    fn seek_mid_cue(&self) {
        let target = gst::ClockTime::from_seconds(SEEK_TO);
        self.playbin.seek_async(Seek::new(Some(target), None));
        self.wait_for("the seek to reach the target", |h| {
            h.playbin
                .position()
                .is_some_and(|p| p >= target && p < gst::ClockTime::from_seconds(CUE_END + 2))
        });
    }

    fn shutdown(&self) {
        let _ = self.playbin.stop();
    }
}

/// FIELD PROBE, not a guard: the owner's own MP4, which reports the paused
/// seek showing no cue where the DASH path shows one.
///
/// `#[ignore]`d and gated on `FCAST_PROBE_MP4` because the fixture is a 296 MB
/// file that is not in the tree. Run it with:
///
/// ```text
/// FCAST_PROBE_MP4=/path/to/DJI_0019.MP4 \
///   cargo test -p fcastplaybin --test subtitle_midcue_seek -- --ignored --nocapture
/// ```
///
/// The file's text track is tx3g through qtdemux, 25 cues one second apart and
/// wall-to-wall contiguous, so EVERY seek target is covered by a cue and no
/// tolerance is needed to have something to show. It prints the whole delivery
/// rather than only asserting, because the first question is WHERE the chain
/// breaks, not whether it broke.
#[test]
#[ignore = "needs the owner's local MP4; set FCAST_PROBE_MP4"]
fn probe_paused_seek_into_a_cue_on_a_field_mp4() {
    let _lock = PIPELINE.lock();
    init();
    let Ok(path) = std::env::var("FCAST_PROBE_MP4") else {
        eprintln!("skipping: FCAST_PROBE_MP4 unset");
        return;
    };
    if !std::path::Path::new(&path).is_file() {
        eprintln!("skipping: {path} is not a file");
        return;
    }
    let harness = Harness::playing_with_subtitles_from(format!("file://{path}"));
    eprintln!("PROBE: cues while PLAYING = {}", harness.cues().len());

    harness.wait_for("playback to pass 8s", |h| {
        h.playbin
            .position()
            .is_some_and(|p| p > gst::ClockTime::from_seconds(8))
    });

    harness.want_playing.set(false);
    harness.playbin.pause().expect("pause");
    harness.paused.set(true);
    harness.wait_for("the pipeline to settle paused", |h| {
        h.playbin.state_summary().0 == gst::State::Paused && h.playbin.is_settled()
    });
    harness.feed.lock().clear();

    // 5.5s is strictly inside the cue spanning 5s..6s ("D 78.10m").
    let target = gst::ClockTime::from_mseconds(5_500);
    harness.playbin.seek_async(Seek::new(Some(target), None));

    // A BOUNDED observation, not a wait_for: the probe has to be able to report
    // "nothing arrived" as data instead of dying on a timeout.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        harness.drain_events();
        harness.redrive();
        harness.pump();
        if !harness.cue_windows().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // Let anything that trails the delivery -- a Clear on the branch's probe,
    // a second sample -- land before the feed is read, so the ORDER is visible.
    let settle = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle {
        harness.drain_events();
        harness.redrive();
        harness.pump();
        std::thread::sleep(Duration::from_millis(25));
    }

    let windows = harness.cue_windows();
    eprintln!(
        "PROBE: state after the paused seek = {:?}, position = {:?}",
        harness.playbin.state_summary().0,
        harness.playbin.position()
    );
    eprintln!(
        "PROBE: {} cue(s) delivered after the paused seek",
        windows.len()
    );
    // The WHOLE feed in order, Clears included: a cue wiped by a Clear that
    // trails it is indistinguishable from one that never arrived, on screen.
    for item in harness.feed.lock().iter() {
        match item {
            SubtitleFeedItem::Cue {
                text,
                start_rt,
                end_rt,
                ..
            } => eprintln!(
                "PROBE:   CUE start_rt={start_rt} end_rt={end_rt:?} text={:?}",
                text.chars().take(56).collect::<String>()
            ),
            SubtitleFeedItem::Bitmap { rt, .. } => eprintln!("PROBE:   BITMAP rt={rt}"),
            SubtitleFeedItem::Clear => eprintln!("PROBE:   CLEAR"),
        }
    }
    harness.shutdown();

    assert!(
        windows
            .iter()
            .any(|(_, start, end)| end.is_none_or(|end| end > *start)),
        "no cue occupying time reached the consumer after the paused seek: {windows:?}"
    );
}

/// PLAYING: a seek into the middle of a cue shows that cue, rather than
/// leaving the viewer with nothing until the next one starts (here, five
/// seconds later -- the whole of the reported "delay").
#[test]
fn a_playing_seek_into_a_cue_delivers_the_covering_cue() {
    let _lock = PIPELINE.lock();
    init();
    if !plugins_available() {
        eprintln!("skipping: encoder/demuxer plugins missing");
        return;
    }
    let harness = Harness::playing_with_subtitles();

    // Leave the cue's window entirely, so nothing already delivered can be
    // mistaken for the redelivery.
    harness.wait_for("playback to pass the cue", |h| {
        h.playbin
            .position()
            .is_some_and(|p| p > gst::ClockTime::from_seconds(CUE_END + 1))
    });
    harness.feed.lock().clear();

    harness.seek_mid_cue();
    // The driver parks a seek that did not arrive at a settled PAUSED, commits
    // PAUSED and asks the caller to resume -- what the receiver's state machine
    // does, and what makes this the PLAYING case rather than a second paused
    // one.
    harness.wait_for("the covering cue to reach the consumer", |h| {
        h.cues().iter().any(|text| text == SPANNING)
    });
    // The point of the long gap: without the clip the next thing the consumer
    // could possibly see is LATER, five seconds down the timeline. Seeing
    // SPANNING first is the whole claim.
    let cues = harness.cues();
    let spanning = cues.iter().position(|text| text == SPANNING).unwrap();
    assert!(
        !cues[..spanning].iter().any(|text| text == "LATER"),
        "the covering cue must arrive at the seek, not after the next cue: {cues:?}"
    );

    harness.shutdown();
}

/// PAUSED: the same seek with no resume at all. The covering cue is the ONLY
/// one that will ever arrive -- nothing else flows -- so if the transport
/// drops it the frozen frame stays bare for good.
#[test]
fn a_paused_seek_into_a_cue_delivers_the_covering_cue() {
    let _lock = PIPELINE.lock();
    init();
    if !plugins_available() {
        eprintln!("skipping: encoder/demuxer plugins missing");
        return;
    }
    let harness = Harness::playing_with_subtitles();
    harness.wait_for("playback to pass the cue", |h| {
        h.playbin
            .position()
            .is_some_and(|p| p > gst::ClockTime::from_seconds(CUE_END + 1))
    });

    harness.want_playing.set(false);
    harness.playbin.pause().expect("pause");
    harness.paused.set(true);
    harness.wait_for("the pipeline to settle paused", |h| {
        h.playbin.state_summary().0 == gst::State::Paused && h.playbin.is_settled()
    });
    harness.feed.lock().clear();

    harness.seek_mid_cue();
    harness.wait_for("the covering cue to reach the paused consumer", |h| {
        h.cues().iter().any(|text| text == SPANNING)
    });
    assert_eq!(
        harness.playbin.state_summary().0,
        gst::State::Paused,
        "the cue must arrive without the pipeline resuming"
    );

    harness.shutdown();
}

/// NEVER PLAYED: a load that comes to rest inside a cue must show that cue on
/// the frozen preroll frame, with no `play()` anywhere in the run.
///
/// The gesture is "open a file (or resume one) and look at it". The receiver
/// pauses on a video frame at the start position, and a cue covering that
/// position belongs on it, every comparable player does this.
///
/// # What broke, and why nothing caught it
///
/// The break was at the FIRST link: the text branch was never BUILT.
/// `Inner::poll_text_policy` refuses to link below a settled `>= PAUSED`, and
/// the only things that asked it to run, a decodebin3 pad appearing, a video
/// chain join, both fire DURING the load's async preroll, where that gate
/// refuses them. Nothing then turned the settle that followed into a second
/// ask, so a pipeline that never left PAUSED sat with `text_0` routed, its
/// consumer seat empty, and no cue at all: measured at 0 cues in 60 s on
/// qtdemux/tx3g, on an 11 MB sample of it, and on this fixture. Reaching
/// PLAYING hides it, because the PLAYING edge brings polls of its own, which
/// is exactly what every other text guard in this crate does, and why this was
/// never seen. The repair is in `bus.rs`: a pipeline settle re-drives the text
/// LINK, the way it already re-drove the text DRAIN.
#[test]
fn a_never_played_paused_load_shows_the_cue_covering_its_position() {
    let _lock = PIPELINE.lock();
    init();
    if !plugins_available() {
        eprintln!("skipping: encoder/demuxer plugins missing");
        return;
    }
    // Start INSIDE the cue, so the one frame this pipeline ever prerolls is a
    // frame the cue covers and there is something to be wrong about.
    let harness = Harness::paused_at(subtitled_mkv(), gst::ClockTime::from_seconds(SEEK_TO));

    harness.wait_for(
        "the covering cue to reach the consumer of a never-played load",
        |h| h.cues().iter().any(|text| text == SPANNING),
    );
    assert_eq!(
        harness.playbin.state_summary().0,
        gst::State::Paused,
        "the cue must arrive without the pipeline ever playing"
    );

    harness.shutdown();
}

/// The scrub half of the same shape: load, never play, and drag the scrubber
/// into a cue. The seek is issued from a pipeline that has only ever
/// PREROLLED, which is a different starting point from
/// [`a_paused_seek_into_a_cue_delivers_the_covering_cue`], that one plays
/// first, and the playing leg is what used to build the branch.
#[test]
fn a_never_played_paused_scrub_shows_the_cue_it_lands_in() {
    let _lock = PIPELINE.lock();
    init();
    if !plugins_available() {
        eprintln!("skipping: encoder/demuxer plugins missing");
        return;
    }
    let harness = Harness::paused_at(subtitled_mkv(), gst::ClockTime::ZERO);
    // Whatever the load's own preroll delivered is not what this test is
    // about; the cue after the scrub is.
    harness.feed.lock().clear();

    harness
        .playbin
        .seek_async(Seek::new(Some(gst::ClockTime::from_seconds(SEEK_TO)), None));
    harness.wait_for(
        "the covering cue after a scrub from a never-played load",
        |h| h.cues().iter().any(|text| text == SPANNING),
    );
    assert_eq!(
        harness.playbin.state_summary().0,
        gst::State::Paused,
        "the cue must arrive without the pipeline ever playing"
    );

    harness.shutdown();
}

/// THE TRANSPORT, not the container: the same paused seek over HTTP, where the
/// demuxer runs in PUSH mode.
///
/// # What this owns that no other test here does
///
/// Every other guard in this suite reads its media off the local filesystem, so
/// `qtdemux` and `matroskademux` both run in PULL mode. The receiver's own
/// sender casts over HTTP, `souphttpsrc` is push-only, and push-mode seeking is
/// a different implementation: `gst_qtdemux_do_push_seek` asks
/// `gst_qtdemux_adjust_seek` for a byte offset with `use_sparse = FALSE`
/// (qtdemux.c:1351), so the SPARSE text track is skipped when the target offset
/// is chosen (qtdemux.c:1161). The byte seek lands on the video keyframe before
/// the target and the text pad is fronted with a GAP to carry it to the new
/// segment start. The pull path passes `use_sparse = TRUE` (qtdemux.c:1444) and
/// emits no such GAP.
///
/// That GAP is the whole defect. basesink prerolls ON a gap
/// (gstbasesink.c:2485) and then parks in `gst_base_sink_wait_preroll`
/// (gstbasesink.c:2438), so a PAUSED pipeline -- one preroll object per sink --
/// spends its only slot on an object that carries no cue, and the covering cue
/// sits behind it until PLAYING. Measured on this fixture before the drop, the
/// consumer received the seek's `Clear` and NOTHING else while paused, on both
/// seeks; the field reported exactly that, with a cue landing ~20 ms after
/// resume.
///
/// # The gesture is the field's, both halves
///
/// Playing, pause, seek BACKWARD into a cue, then seek FORWARD into another --
/// the reporter did two in a row, and only the first one having been fixed
/// would still be a broken player. Neither seek is allowed to resume: this
/// harness only resumes when `want_playing` is set, and it is not.
#[test]
fn a_paused_seek_over_http_delivers_the_covering_cue_in_push_mode() {
    let _lock = PIPELINE.lock();
    init();
    if !mp4_plugins_available() {
        eprintln!("skipping: mp4 encoder/demuxer plugins missing");
        return;
    }
    let server = support::FileServer::serve(subtitled_mp4_dir());
    let harness = Harness::playing_with_subtitles_from(server.url("field.mp4"));

    // Past both targets, so nothing already on screen can be mistaken for a
    // redelivery and the BACKWARD seek is genuinely backward.
    harness.wait_for("playback to pass 5s", |h| {
        h.playbin
            .position()
            .is_some_and(|p| p > gst::ClockTime::from_seconds(5))
    });
    harness.want_playing.set(false);
    harness.playbin.pause().expect("pause");
    harness.paused.set(true);
    harness.wait_for("the pipeline to settle paused", |h| {
        h.playbin.state_summary().0 == gst::State::Paused && h.playbin.is_settled()
    });

    // BACKWARD, into the middle of cue 2 (2.000 -> 3.000).
    harness.feed.lock().clear();
    harness
        .playbin
        .seek_async(Seek::new(Some(gst::ClockTime::from_mseconds(2_500)), None));
    harness.wait_for("the covering cue after the backward paused seek", |h| {
        h.cues().iter().any(|text| *text == http_cue(2))
    });
    assert_eq!(
        harness.playbin.state_summary().0,
        gst::State::Paused,
        "the cue must arrive without the pipeline resuming"
    );

    // FORWARD, into the middle of cue 8 (8.000 -> 9.000). The second seek of a
    // pair is its own case: it starts from a pipeline that has already been
    // flushed once and is still paused.
    harness.feed.lock().clear();
    harness
        .playbin
        .seek_async(Seek::new(Some(gst::ClockTime::from_mseconds(8_500)), None));
    harness.wait_for("the covering cue after the forward paused seek", |h| {
        h.cues().iter().any(|text| *text == http_cue(8))
    });
    assert_eq!(
        harness.playbin.state_summary().0,
        gst::State::Paused,
        "the second seek's cue must arrive without the pipeline resuming"
    );

    harness.shutdown();
}

/// A PAUSED pipeline prerolls exactly ONE buffer per sink, and some subtitle
/// tracks put a ZERO-LENGTH record in front of every real cue -- same start,
/// same text, `start == end`. A record that occupies no time can never be
/// shown, so when a paused seek lands in the gap before such a pair, spending
/// the one preroll slot on the twin leaves the frozen frame blank and the real
/// cue blocked in the queue until playback resumes.
///
/// Measured on the reporter's stream (a whole-period WebVTT of exactly this
/// shape) before the drop existed: a seek landing at 54.830 delivered
/// `start_rt = end_rt = 40ms` and the engine held no overlay at any running
/// time. The fixture reproduces it -- cue N ends at N.900, the twin for N+1
/// starts at N+1.000, and the seek lands between them.
///
/// WHAT THIS SUITE OWNS, now that the gap has a second half. The seek target
/// lands in a 100 ms hole, so even a perfect delivery leaves the frozen frame
/// covered by no cue at all, and the render side answers that separately:
/// `fcast-video`'s `PAUSED_CUE_LOOKAHEAD` lets a paused frame show a cue
/// starting within 200 ms ahead of it, which is what finally puts a line on
/// this screen. The two halves are independent and the split is the crate
/// boundary -- this crate cannot depend on `fcast-video`, and no
/// tolerance can rescue a delivery that never happened or that carries only a
/// zero-length record. So the assertion below stays exactly what it was: a cue
/// that OCCUPIES TIME must reach the consumer. That the engine would now also
/// draw it 50 ms early is the other crate's test to make, and it makes it in
/// `cue::tests::a_paused_frame_in_a_gap_shows_a_cue_just_ahead_of_it`.
#[test]
fn a_paused_seek_past_a_zero_length_twin_delivers_the_real_cue() {
    let _lock = PIPELINE.lock();
    init();
    let root = support::fixtures();
    if !root.join("vod/manifest-text-twins.mpd").is_file() {
        eprintln!("skipping: no twin-bearing manifest in the fixtures");
        return;
    }
    let server = support::FileServer::serve(root.clone());
    let harness = Harness::playing_with_subtitles_from(server.url("vod/manifest-text-twins.mpd"));

    harness.wait_for("playback to pass the seek target", |h| {
        h.playbin
            .position()
            .is_some_and(|p| p > gst::ClockTime::from_seconds(24))
    });
    harness.want_playing.set(false);
    harness.playbin.pause().expect("pause");
    harness.paused.set(true);
    harness.wait_for("the pipeline to settle paused", |h| {
        h.playbin.state_summary().0 == gst::State::Paused && h.playbin.is_settled()
    });
    harness.feed.lock().clear();

    // 20.950 sits in the gap between cue 20 (ends 20.900) and the twin pair at
    // 21.000, so the NEXT record the branch can deliver is the twin.
    let target = gst::ClockTime::from_mseconds(20_950);
    harness.playbin.seek_async(Seek::new(Some(target), None));
    harness.wait_for("a cue after the paused seek", |h| {
        !h.cue_windows().is_empty()
    });

    let windows = harness.cue_windows();
    assert!(
        windows
            .iter()
            .any(|(_, start, end)| end.is_none_or(|end| end > *start)),
        "every cue delivered after the paused seek occupies no time, so nothing can be \
         drawn on the frozen frame -- the zero-length twin spent the one preroll slot a \
         paused sink has: {windows:?}"
    );
    assert_eq!(
        harness.playbin.state_summary().0,
        gst::State::Paused,
        "the cue must arrive without the pipeline resuming"
    );

    harness.shutdown();
}
