//! DIAGNOSTIC: does the text source REDELIVER the cue a paused seek lands in?
//!
//! `#[ignore]`d on purpose. An instrument, not an assertion. It answers the
//! one question the engine cannot be blamed for: after a flushing seek to a pts
//! inside a cue, with the pipeline paused throughout, does that cue reach the
//! subtitle consumer at all? Nothing downstream can paint what never arrives.
//!
//! ```sh
//! # the built-in comparison (whole-period vs segmented DASH, both directions)
//! cargo test -p fcastplaybin --test paused_seek_redelivery_probe -- --ignored --nocapture
//!
//! # against the reporter's own content
//! FCAST_PROBE_URI=https://example/stream.mpd \
//! FCAST_PROBE_SEEK_SECS=61.5 FCAST_PROBE_PLAY_SECS=5 \
//!   cargo test -p fcastplaybin --test paused_seek_redelivery_probe -- --ignored --nocapture
//! ```
//!
//! # What it prints, and what a break looks like
//!
//! A per-run timeline of everything the consumer received after the seek.
//!
//! * **`Clear` then a `Cue` with `start_rt=0`** is healthy. The source resent
//!   the covering unit and the transport clipped it onto the new segment. If
//!   subtitles are still missing on screen, the break is downstream.
//! * **`Clear` and no `Cue`** means the source did not redeliver while paused.
//!   No transport change can paint it while paused and the fix is driver-side.
//! * **a `Cue` whose `start_rt` is not 0** means the clip regressed.
//!
//! # Why the built-in fixtures are three shapes
//!
//! Whole-period `text/vtt` is one unsegmented Representation pushed once, the
//! shape where "the source has nothing left to send" is a real worry. The
//! segmented variant re-fetches per position and is the control. The third
//! carries zero-length twin records and seeks into the gap in front of a pair.
//! A paused sink prerolls exactly one buffer, so a `Cue` with
//! `start_rt == end_rt` there means the unshowable twin spent the slot.

use std::{
    cell::Cell,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Seek, SelectionGate, Sinks, StartPoint,
    SubtitleFeedItem, TrackSlot, TrackTarget,
};
use parking_lot::Mutex;

mod support;

const TIMEOUT: Duration = Duration::from_secs(60);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCAST_PROBE_LOG") {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

fn secs(value: f64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((value * 1e9) as u64)
}

fn env_secs(key: &str) -> Option<f64> {
    std::env::var(key).ok()?.parse().ok()
}

#[derive(Debug)]
enum Feed {
    Cue {
        at: Duration,
        text: String,
        start_rt: gst::ClockTime,
        end_rt: Option<gst::ClockTime>,
    },
    Clear {
        at: Duration,
    },
}

struct Probe {
    playbin: FcastPlaybin,
    feed: Arc<Mutex<Vec<Feed>>>,
    events: mpsc::Receiver<PlaybinEvent>,
    log: Mutex<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    parked: Cell<Option<Seek>>,
}

impl Probe {
    fn new(t0: Instant) -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: None,
            audio: AudioSink::Factory(Box::new(|| {
                Ok(gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?)
            })),
        })
        .expect("building fcastplaybin");
        let feed: Arc<Mutex<Vec<Feed>>> = Arc::new(Mutex::new(Vec::new()));
        let log = feed.clone();
        playbin.set_subtitle_consumer(move |item| match item {
            SubtitleFeedItem::Cue {
                text,
                start_rt,
                end_rt,
                ..
            } => log.lock().push(Feed::Cue {
                at: t0.elapsed(),
                text,
                start_rt,
                end_rt,
            }),
            SubtitleFeedItem::Clear => log.lock().push(Feed::Clear { at: t0.elapsed() }),
            _ => {}
        });
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _| {
            let _ = tx.send(event);
        });
        Self {
            playbin,
            feed,
            events,
            log: Mutex::new(Vec::new()),
            paused: Cell::new(true),
            parked: Cell::new(None),
        }
    }

    /// One settle-point pass, including putting back a seek the driver parked.
    fn pump(&self) {
        while let Ok(event) = self.events.try_recv() {
            if let PlaybinEvent::QueueSeek(seek) = &event {
                self.parked.set(Some(*seek));
            }
            self.log.lock().push(event);
        }
        if self.playbin.is_settled()
            && let Some(seek) = self.parked.take()
        {
            self.playbin.seek_async(seek);
        }
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: true,
        });
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if Instant::now() >= deadline {
                println!("!! timed out waiting for {what}");
                return false;
            }
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

    fn cue_count(&self) -> usize {
        self.feed
            .lock()
            .iter()
            .filter(|f| matches!(f, Feed::Cue { .. }))
            .count()
    }
}

fn probe(label: &str, url: String, play_to: gst::ClockTime, seek_to: gst::ClockTime) {
    println!("\n================ {label} ================");
    println!("media   : {url}");
    println!("play to : {play_to}   seek to: {seek_to}");
    let t0 = Instant::now();
    let p = Probe::new(t0);
    if let Err(error) = p.playbin.load(
        MediaInput::Uri(url),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    ) {
        println!("!! the load failed: {error}");
        return;
    }
    if !p.wait_for("a text stream", |p| !p.text_sids().is_empty()) {
        println!("!! no text stream; there is nothing to probe");
        return;
    }
    let sid = p.text_sids().remove(0);
    p.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
    p.playbin.play().expect("play");
    p.paused.set(false);
    if !p.wait_for("the first cues", |p| p.cue_count() > 0) {
        return;
    }
    p.wait_for("playback to reach the pause point", |p| {
        p.playbin.position().is_some_and(|pos| pos > play_to)
    });
    let before = p.cue_count();
    p.playbin.pause().expect("pause");
    p.paused.set(true);
    p.wait_for("a settled PAUSED", |p| {
        p.playbin.state_summary().0 == gst::State::Paused && p.playbin.is_settled()
    });
    println!("cues delivered before the seek: {before}");
    p.feed.lock().clear();

    println!("--- the PAUSED seek ---");
    p.playbin.seek_async(Seek::new(Some(seek_to), None));
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        p.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "state={:?} settled={} position={:?}",
        p.playbin.state_summary(),
        p.playbin.is_settled(),
        p.playbin.position()
    );
    println!("--- TIMELINE: what reached the consumer after the seek ---");
    let feed = p.feed.lock();
    if feed.is_empty() {
        println!("   (NOTHING, the source did not redeliver while paused)");
    }
    for item in feed.iter().take(16) {
        match item {
            Feed::Cue {
                at,
                text,
                start_rt,
                end_rt,
            } => println!(
                "   +{:>7.3}s Cue {:?} start_rt={start_rt} end_rt={end_rt:?}",
                at.as_secs_f64(),
                text.chars().take(40).collect::<String>()
            ),
            Feed::Clear { at } => println!("   +{:>7.3}s Clear", at.as_secs_f64()),
        }
    }
    println!(
        "   TOTAL cues after the seek: {}",
        feed.iter()
            .filter(|f| matches!(f, Feed::Cue { .. }))
            .count()
    );
    drop(feed);
    let _ = p.playbin.stop();
}

#[test]
#[ignore = "diagnostic instrument; run explicitly with --nocapture"]
fn probe_paused_seek_redelivery() {
    init();

    // A user-supplied URI overrides the built-in fixtures.
    if let Ok(value) = std::env::var("FCAST_PROBE_URI") {
        let seek_to = secs(env_secs("FCAST_PROBE_SEEK_SECS").unwrap_or(20.5));
        let play_to = secs(
            env_secs("FCAST_PROBE_PLAY_SECS")
                .unwrap_or_else(|| seek_to.nseconds() as f64 / 1e9 + 3.0),
        );
        // The transport A/B. `FCAST_PROBE_SERVE=<dir>` serves the same bytes
        // over the fixture HTTP server, reading `FCAST_PROBE_URI` as a path
        // relative to it, so one fixture can be probed as `file://` and as
        // `http://` with nothing else changed. The difference matters. A local
        // file lets demuxers run in pull mode, while an HTTP source is
        // push-only and exercises a different seek implementation entirely.
        let (label, url, _server) = match std::env::var("FCAST_PROBE_SERVE") {
            Ok(dir) => {
                let server = support::FileServer::serve(&dir);
                let url = server.url(value.trim_start_matches('/'));
                ("FCAST_PROBE_URI over http (PUSH)", url, Some(server))
            }
            Err(_) => {
                let url = if value.contains("://") {
                    value
                } else {
                    format!("file://{value}")
                };
                ("FCAST_PROBE_URI", url, None)
            }
        };
        probe(label, url, play_to, seek_to);
        return;
    }

    let root = support::fixtures();
    let server = support::FileServer::serve(root.clone());
    // Fixture cue N covers [N, N+0.9), so a `.5` target is strictly inside one.
    // Backward seeks into territory the demuxer has already walked. Forward is
    // the scrub-ahead shape, where it has not.
    let back = (secs(24.0), secs(20.5));
    let fwd = (secs(5.0), secs(60.5));

    if support::has_embedded_text(&root) {
        probe(
            "WHOLE-PERIOD text/vtt, BACKWARD",
            server.url("vod/manifest-text.mpd"),
            back.0,
            back.1,
        );
        probe(
            "WHOLE-PERIOD text/vtt, FORWARD",
            server.url("vod/manifest-text.mpd"),
            fwd.0,
            fwd.1,
        );
    } else {
        println!("no embedded-text manifest in the fixtures");
    }
    // The twin shape puts a zero-length record in front of every real cue. The
    // seek lands in the gap between a cue's end and the next twin's start, so
    // the next deliverable record is the twin, and a paused sink prerolls
    // exactly one buffer. A `Cue` with `start_rt` equal to `end_rt` here is
    // the twin winning that slot, the failure this shape exists to show.
    if root.join("vod/manifest-text-twins.mpd").is_file() {
        probe(
            "TWIN-BEARING whole-period text/vtt, landing in the gap",
            server.url("vod/manifest-text-twins.mpd"),
            secs(24.0),
            secs(20.95),
        );
    } else {
        println!("no twin-bearing manifest in the fixtures");
    }
    if support::has_segmented_text(&root) {
        probe(
            "SEGMENTED text/vtt, FORWARD (the control)",
            server.url("vod/manifest-text-seg.mpd"),
            fwd.0,
            fwd.1,
        );
    } else {
        println!("no segmented-text manifest in the fixtures");
    }
}
