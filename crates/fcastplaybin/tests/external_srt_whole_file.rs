//! THE FIELD SCENARIO: a real `.srt` attached to a playing item, through the
//! real external chain the receiver ships (`urisourcebin` -> `decodebin3` ->
//! `rssubparse` -> the text branch's `appsink`).
//!
//! A 19-minute film with an external Chinese SRT showed its opening lines and
//! nothing after them. The transport was never the loss: the branch is
//! unsynced, so the parser hands the WHOLE FILE over within milliseconds of the
//! branch linking, and every cue of it reaches the consumer. What could not
//! hold it was the renderer's 16-cue backlog, which spent the file's tail
//! admitting cues further and further into the future
//! (`fcast-video::cue::PENDING_LIMIT`, and the engine suites own that half).
//!
//! What this suite owns is the DELIVERY half, at field scale: a few hundred
//! cues in one burst, all of them arriving, and a mid-file cue still arriving
//! after a seek, the moment the field user never got to.
//!
//! # Why this test serializes
//!
//! One subtitle consumer per pipeline, and the crate's cue feed is the probe
//! point (see `sink_subtitles.rs`, which states the same for the same reason).

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{scenario::ScenarioBuilder, sink::FTestSink, spec::Pacing};
use gst::prelude::*;

/// Cues in the file. A few hundred is the field's order (a 19-minute film had
/// ~400) and comfortably past the 16 the old backlog held.
const CUES: usize = 300;
/// Cue spacing and length, so cue `n` covers `[START + n*STEP, +STEP)`.
const STEP_MS: u64 = 100;
const START_MS: u64 = 1000;
/// The cue whose delivery after a mid-file seek is the point of the second
/// test: deep enough into the file that the old backlog had long since dropped
/// it.
const MID: usize = 150;

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gst::init().expect("gst init");
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
        // The receiver's rank swap (receiver-core/src/gstreamer.rs): the Rust
        // parsers own every subtitle stream, the C ones stay registered at
        // NONE. This suite is about the chain the receiver really autoplugs.
        gstrssubparse::plugin_register_static().expect("registering rssubparse");
        let registry = gst::Registry::get();
        for c_name in ["subparse", "ssaparse"] {
            if let Some(feature) = registry.lookup_feature(c_name) {
                feature.set_rank(gst::Rank::NONE);
            }
        }
        for rs_name in ["rssubparse", "rsssaparse"] {
            registry
                .lookup_feature(rs_name)
                .expect("registered by gstrssubparse above")
                .set_rank(gst::Rank::PRIMARY);
        }
    });
}

/// `CUES` cues, one per `STEP_MS`, each named for its index so a cue can be
/// recognised whatever timeline it is delivered in.
fn write_srt() -> std::path::PathBuf {
    let fmt = |ms: u64| {
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60,
            ms % 1000
        )
    };
    let mut srt = String::new();
    for index in 0..CUES {
        let begin = START_MS + index as u64 * STEP_MS;
        srt.push_str(&format!(
            "{}\n{} --> {}\nCUE{index:03}\n\n",
            index + 1,
            fmt(begin),
            fmt(begin + STEP_MS)
        ));
    }
    let path = std::env::temp_dir().join(format!(
        "fcastplaybin-wholefile-{}-{:?}.srt",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, srt).expect("writing the srt");
    path
}

/// The playbin, its cue feed, and the loaded+playing item with the external
/// subtitle selected.
struct Attached {
    playbin: FcastPlaybin,
    /// Every cue the driver delivered, newest last.
    feed: Arc<Mutex<Vec<(String, gst::ClockTime, Option<gst::ClockTime>)>>>,
    media: fcasttest::scenario::ScenarioHandle,
    srt: std::path::PathBuf,
}

impl Attached {
    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: false,
            seekable: true,
        }
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    /// Pump until `done`, or panic with what the feed did hold.
    fn wait_for(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            self.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for {what}; {} cues delivered",
            self.feed.lock().expect("feed").len()
        );
    }

    /// Which cue indices have been delivered.
    fn delivered(&self) -> std::collections::BTreeSet<usize> {
        self.feed
            .lock()
            .expect("feed")
            .iter()
            .filter_map(|(text, _, _)| text.trim().strip_prefix("CUE")?.parse().ok())
            .collect()
    }
}

fn attach_and_select(tag: &str) -> Attached {
    init();
    let media = ScenarioBuilder::new(tag)
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(60))
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let srt = write_srt();

    let playbin = FcastPlaybin::new(Sinks {
        video: Some(FTestSink::new().upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");

    let feed: Arc<Mutex<Vec<(String, gst::ClockTime, Option<gst::ClockTime>)>>> =
        Default::default();
    let tap = feed.clone();
    playbin.set_subtitle_consumer(move |item| {
        if let fcastplaybin::SubtitleFeedItem::Cue {
            text,
            start_rt,
            end_rt,
            ..
        } = item
        {
            tap.lock().expect("feed").push((text, start_rt, end_rt));
        }
    });

    let (tx, events) = std::sync::mpsc::channel();
    playbin.set_event_handler(None, move |event, _| {
        let _ = tx.send(event);
    });

    let attached = Attached {
        playbin,
        feed,
        media,
        srt,
    };
    attached.playbin.load_async(
        MediaInput::Uri(attached.media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    let collected = Arc::new(Mutex::new(false));
    {
        let collected = collected.clone();
        let events = Mutex::new(events);
        attached.wait_for("the stream collection", || {
            while let Ok(event) = events.lock().expect("events").try_recv() {
                if let PlaybinEvent::Error { error, .. } = &event {
                    panic!("pipeline error: {error}");
                }
                if matches!(event, PlaybinEvent::StreamCollection(_)) {
                    *collected.lock().expect("collected") = true;
                }
            }
            *collected.lock().expect("collected")
        });
    }

    let id = attached
        .playbin
        .attach_subtitle(&format!("file://{}", attached.srt.display()))
        .expect("attaching the external subtitle");
    attached.wait_for("the external subtitle stream to materialize", || {
        !attached.playbin.subtitle_stream_ids(id).is_empty()
    });
    attached
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    attached.playbin.pump_selection(attached.gate());
    attached.playbin.play().expect("play");
    attached
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.playbin.stop();
        self.media.unregister();
        let _ = std::fs::remove_file(&self.srt);
    }
}

/// THE BURST: the whole file reaches the consumer, not a prefix of it.
///
/// The unsynced branch delivers an external subtitle as fast as the parser can
/// produce it, so "was the file delivered" and "was it delivered in time" are
/// the same question here, every cue of it arrives within a moment of the
/// branch linking, hundreds of them, long before all but the first is due.
#[test]
fn the_whole_external_file_reaches_the_consumer_in_one_burst() {
    let attached = attach_and_select("wholefileburst");
    attached.wait_for("the whole file to be delivered", || {
        attached.delivered().len() >= CUES
    });

    let delivered = attached.delivered();
    assert_eq!(
        delivered.len(),
        CUES,
        "the transport delivered {} of {CUES} cues; missing {:?}",
        delivered.len(),
        (0..CUES)
            .filter(|i| !delivered.contains(i))
            .take(8)
            .collect::<Vec<_>>()
    );
    // Well-formed windows: the renderer schedules on these, and a cue whose
    // end is not after its start can never be shown.
    let ill_formed: Vec<_> = attached
        .feed
        .lock()
        .expect("feed")
        .iter()
        .filter(|(_, start, end)| end.is_some_and(|end| end <= *start))
        .map(|(text, start, end)| (text.clone(), *start, *end))
        .take(4)
        .collect();
    assert!(
        ill_formed.is_empty(),
        "cues arrived with an empty window: {ill_formed:?}"
    );
}

/// A MID-FILE cue still arrives after a seek into the middle of the item.
///
/// This is the exact moment the field user never reached: the subtitles beyond
/// the opening lines. The seek replays the external input from the video's new
/// origin, so the whole file is delivered again, and cue `MID`, 15 seconds
/// deep, is in it.
#[test]
fn a_mid_file_cue_arrives_after_a_seek_into_the_middle() {
    let attached = attach_and_select("wholefileseek");
    attached.wait_for("the first delivery", || attached.delivered().len() >= CUES);

    // Forget the first burst: what matters is what arrives AFTER the seek.
    attached.feed.lock().expect("feed").clear();
    let target = gst::ClockTime::from_mseconds(START_MS + MID as u64 * STEP_MS);
    attached.playbin.seek(target).expect("seeking mid-file");

    attached.wait_for("the redelivery to reach the mid-file cue", || {
        attached.delivered().contains(&MID)
    });
    assert!(
        attached.delivered().contains(&MID),
        "the cue covering the seek target never arrived"
    );
}
