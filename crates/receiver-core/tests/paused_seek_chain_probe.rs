//! DIAGNOSTIC: the paused mid-cue seek, link by link, over the REAL chain.
//!
//! `#[ignore]`d on purpose, it is an instrument, not an assertion. It plays a
//! file, pauses, seeks into the middle of a cue, and prints a TIMELINE of what
//! each link did, so a "no subtitles after a paused seek" report can be
//! localised in one run instead of guessed at.
//!
//! ```sh
//! cargo test -p receiver-core --test paused_seek_chain_probe -- --ignored --nocapture
//!
//! # against the reporter's own content (file path, or any URI GStreamer takes)
//! FCAST_PROBE_URI=https://example/stream.mpd \
//! FCAST_PROBE_SEEK_SECS=61.5 FCAST_PROBE_PLAY_SECS=5 \
//!   cargo test -p receiver-core --test paused_seek_chain_probe -- --ignored --nocapture
//!
//! # the load-then-scrub shape: never leave PAUSED (see FCAST_PROBE_NO_PLAY below)
//! FCAST_PROBE_URI=/path/file.mp4 FCAST_PROBE_NO_PLAY=1 \
//!   cargo test -p receiver-core --test paused_seek_chain_probe -- --ignored --nocapture
//! ```
//!
//! # The chain, and what a break looks like
//!
//! The whole path is here: `fcastplaybin`'s transport into `fcast-video`'s
//! [`FSink`] and the [`CueEngine`] it owns, wired exactly as
//! `receiver-core::player` wires it.
//!
//! 1. **DELIVERY**, does the covering cue reach the subtitle consumer while
//!    PAUSED at all? A seek lands inside a cue and the demuxer resends the unit
//!    covering it; sparse text still has to cross decodebin3's multiqueue
//!    during a paused preroll. *Broken:* the `LINK 1+2` section is empty, or
//!    holds a `Clear` and no `Cue`. Nothing downstream can paint what never
//!    arrived, and the fix would be driver-side.
//! 2. **CLIPPING**, the resent unit's pts PRECEDES the new segment, so its
//!    running time is only computable by clipping
//!    (`Inner::clipped_running_time`). *Broken:* a `Cue` line is missing for a
//!    seek that landed inside one, or its `start_rt` is not `0`, the clip
//!    regressed.
//! 3. **SCHEDULING**, `current_overlays()` is the PAUSED path: it re-evaluates
//!    against the frozen `last_shown_rt`, which `flush()` nulls at FLUSH_STOP
//!    and the post-seek PREROLL frame must restore. It is printed beside
//!    `overlays_for(Some(rt))`, which supplies an rt instead of reading one.
//!    *Broken:* `current_overlays()` is 0 while `overlays_for(Some(0ms))` is 1
//!    the cue is schedulable but `last_shown_rt` is `None`, so no preroll frame
//!    reached `show_frame` after the flush.
//! 4. **REPAINT**, `overlays-changed` is what tells a frozen frame to redraw
//!    (there is no next frame to carry it). *Broken:* the count is 0 while
//!    `current_overlays()` is non-zero, the engine has the overlay and nobody
//!    is being told.
//!
//! All four measured green on `DJI_0019_sample.MP4` (qtdemux/tx3g, 25 distinct
//! cue texts so the raster is genuinely cold) at the time this was written.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use fcast_video::{
    cue::{CueInput, TextFormat},
    video::FSink,
};
use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Seek, SelectionGate, Sinks, StartPoint,
    SubtitleFeedItem, TrackSlot, TrackTarget,
};
use gst::prelude::*;
use parking_lot::Mutex;

const TIMEOUT: Duration = Duration::from_secs(60);

/// The sample the probe defaults to (resolved under `$HOME` so no local path
/// lands in the tree): tx3g through qtdemux, one-second cues at every integer
/// second, and 25 DISTINCT texts, so a seek lands on a raster that has never
/// been drawn.
fn default_media() -> Option<String> {
    Some(format!(
        "{}/Videos/DJI_0019_sample.MP4",
        std::env::var("HOME").ok()?
    ))
}
const DEFAULT_SEEK_SECS: f64 = 5.5;

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

/// `FCAST_PROBE_URI`, taken as a URI when it carries a scheme and as a file
/// path otherwise. `None` when the default sample is not on this machine.
fn media_uri() -> Option<String> {
    match std::env::var("FCAST_PROBE_URI") {
        Ok(value) if value.contains("://") => Some(value),
        Ok(value) => Some(format!("file://{value}")),
        Err(_) => {
            let path = PathBuf::from(default_media()?);
            path.is_file().then(|| format!("file://{}", path.display()))
        }
    }
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
    engine: fcast_video::cue::CueEngine,
    feed: Arc<Mutex<Vec<Feed>>>,
    repaints: Arc<AtomicUsize>,
    events: mpsc::Receiver<PlaybinEvent>,
    log: Mutex<Vec<PlaybinEvent>>,
    paused: std::cell::Cell<bool>,
    parked: std::cell::Cell<Option<Seek>>,
}

impl Probe {
    fn new(t0: Instant) -> Self {
        let video_sink = FSink::new();
        let engine = video_sink.cue_engine();
        engine.set_canvas(1280, 720);

        let repaints = Arc::new(AtomicUsize::new(0));
        let counter = repaints.clone();
        video_sink.connect("overlays-changed", false, move |_values| {
            counter.fetch_add(1, Ordering::Release);
            None
        });

        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.clone().upcast()),
            audio: AudioSink::Factory(Box::new(|| {
                Ok(gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?)
            })),
        })
        .expect("building fcastplaybin");

        // Wired EXACTLY as `receiver-core::player::set_subtitle_consumer` wires
        // it, so a break here is a break there.
        let feed: Arc<Mutex<Vec<Feed>>> = Arc::new(Mutex::new(Vec::new()));
        let log = feed.clone();
        let sink = engine.clone();
        playbin.set_subtitle_consumer(move |item| match item {
            SubtitleFeedItem::Cue {
                text,
                start_rt,
                end_rt,
                ..
            } => {
                log.lock().push(Feed::Cue {
                    at: t0.elapsed(),
                    text: text.clone(),
                    start_rt,
                    end_rt,
                });
                sink.submit(CueInput {
                    format: TextFormat::Utf8,
                    text,
                    start_rt,
                    end_rt,
                });
            }
            SubtitleFeedItem::Clear => {
                log.lock().push(Feed::Clear { at: t0.elapsed() });
                sink.clear();
            }
            _ => {}
        });

        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _| {
            let _ = tx.send(event);
        });

        Self {
            playbin,
            engine,
            feed,
            repaints,
            events,
            log: Mutex::new(Vec::new()),
            paused: std::cell::Cell::new(true),
            parked: std::cell::Cell::new(None),
        }
    }

    /// One settle-point pass: absorb events, put back a seek the driver parked
    /// (`Job::Seek` refuses one that did not arrive at a settled PAUSED), and
    /// give the link policy its chance, what the receiver does on every edge.
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

#[test]
#[ignore = "diagnostic instrument; run explicitly with --nocapture"]
fn probe_the_paused_seek_chain() {
    init();
    let Some(uri) = media_uri() else {
        println!(
            "no media: set FCAST_PROBE_URI, or put the default sample at {}",
            default_media().unwrap_or_else(|| "$HOME/Videos/DJI_0019_sample.MP4".into())
        );
        return;
    };
    let seek_to = secs(env_secs("FCAST_PROBE_SEEK_SECS").unwrap_or(DEFAULT_SEEK_SECS));
    // Where to pause before seeking. Past the target by default (a BACKWARD
    // seek, into territory the demuxer has walked); set it lower than the
    // target for the forward-scrub shape.
    let play_to = secs(
        env_secs("FCAST_PROBE_PLAY_SECS").unwrap_or_else(|| seek_to.nseconds() as f64 / 1e9 + 2.0),
    );

    println!("\n================ paused mid-cue seek ================");
    println!("media    : {uri}");
    println!("play to  : {play_to}");
    println!("seek to  : {seek_to}");

    let t0 = Instant::now();
    let p = Probe::new(t0);
    if let Err(error) = p.playbin.load(
        MediaInput::Uri(uri),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    ) {
        println!("!! the load failed: {error}");
        return;
    }
    if !p.wait_for("a text stream to be advertised", |p| {
        !p.text_sids().is_empty()
    }) {
        println!("!! no text stream; there is nothing to probe");
        return;
    }
    let sids = p.text_sids();
    println!("text sids: {sids:?}");
    p.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some(sids[0].clone())),
    );
    // `FCAST_PROBE_NO_PLAY=1`: never leave PAUSED at all. The field gesture
    // "open the file and drag the scrubber" seeks from a pipeline that has only
    // ever PREROLLED; every other shape here calls `play()` first.
    //
    // MEASURED on this tree, and the reason the knob is worth keeping: the
    // requested text branch never goes live in this shape at all. No preroll
    // cue at position 0 within 60s, and nothing after the seek either -- on
    // qtdemux/tx3g AND on the matroskademux/subparse fixture the green guards
    // in `fcastplaybin` are built from. So it is the driver's paused selection,
    // not a demuxer or a container property.
    let no_play = std::env::var_os("FCAST_PROBE_NO_PLAY").is_some();
    if no_play {
        println!("(FCAST_PROBE_NO_PLAY: staying PAUSED from the load)");
        p.wait_for("a settled PAUSED after the load", |p| {
            p.playbin.state_summary().0 == gst::State::Paused && p.playbin.is_settled()
        });
        // The branch must actually be LINKED and delivering before the seek, or
        // this measures "seeked before the text branch joined" -- a probe
        // artifact -- instead of the field gesture, which has subtitles already
        // on screen. The preroll cue at position 0 is that evidence: PAUSED,
        // `new_preroll` is the only callback that can produce it.
        let linked = p.wait_for("the preroll cue at position 0", |p| p.cue_count() > 0);
        println!(
            "preroll cue before the seek: {} (linked={linked})",
            p.cue_count()
        );
    } else {
        p.playbin.play().expect("play");
        p.paused.set(false);
        p.wait_for("the first cue", |p| p.cue_count() > 0);
        p.wait_for("playback to reach the pause point", |p| {
            p.playbin.position().is_some_and(|pos| pos > play_to)
        });

        p.playbin.pause().expect("pause");
    }
    p.paused.set(true);
    p.wait_for("a settled PAUSED", |p| {
        p.playbin.state_summary().0 == gst::State::Paused && p.playbin.is_settled()
    });
    println!(
        "before the seek: overlays={} repaints={}",
        p.engine.current_overlays().len(),
        p.repaints.load(Ordering::Acquire)
    );
    p.feed.lock().clear();
    let repaints_before = p.repaints.load(Ordering::Acquire);

    println!("\n--- the PAUSED seek ---");
    p.playbin.seek_async(Seek::new(Some(seek_to), None));
    let deadline = Instant::now() + Duration::from_secs(12);
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

    println!("\n--- LINK 1+2 DELIVERY/CLIPPING: what reached the consumer ---");
    let feed = p.feed.lock();
    if feed.is_empty() {
        println!("   (NOTHING, the cue never arrived; the break is upstream of the engine)");
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
    drop(feed);

    println!("\n--- LINK 3 SCHEDULING: the engine ---");
    println!(
        "   current_overlays() [the PAUSED path, frozen last_shown_rt]: {}",
        p.engine.current_overlays().len()
    );
    for rt_ms in [0u64, 1, 100, 500] {
        println!(
            "   overlays_for(Some({rt_ms}ms)) [an rt supplied]: {}",
            p.engine
                .overlays_for(Some(gst::ClockTime::from_mseconds(rt_ms)))
                .len()
        );
    }

    println!("\n--- LINK 4 REPAINT: the frozen-frame signal ---");
    println!(
        "   overlays-changed since the seek: {}",
        p.repaints.load(Ordering::Acquire) - repaints_before
    );

    let _ = p.playbin.stop();
}
