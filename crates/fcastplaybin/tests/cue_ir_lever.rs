//! `FCAST_NO_CUE_IR=1` puts the subtitle path back exactly as it was before
//! cue-IR existed.
//!
//! The lever has two halves, and either alone would leave a way for the new
//! arm to reach production:
//!
//!  1. **Negotiation.** The receiver does not ask the parsers for cue-ir
//!     output, so the caps that flow are pango-markup and the consumer sees
//!     [`SubtitleTextFormat::PangoMarkup`], derived from the caps alone.
//!  2. **The driver.** Even if something else turns the parsers on,
//!     `item_from_sample` must not look at the meta. The second test forces
//!     cue-ir output with a `CueIrMeta` attached, and the consumer must still
//!     be handed `Utf8`.
//!
//! Own test binary because `fcastplaybin::cue_ir_enabled()` is read once per
//! process. A lever that could change under a running pipeline would let the
//! caps and the payload disagree, so the env var must be set before anything
//! reads it, which means owning the process.
//!
//! Serialized because there is one subtitle consumer per pipeline and the
//! crate's cue feed is the probe point.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
    SubtitleFeedItem, SubtitleTextFormat, TrackSlot, TrackTarget,
};
use fcasttest::{scenario::ScenarioBuilder, sink::FTestSink, spec::Pacing};
use gst::prelude::*;

const STYLED_SRT: &str = "\
1
00:00:01,000 --> 00:00:03,000
plain and <font color=\"#ff0000\">red</font>
";

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Set before anything reads it. Every test comes through this `Once`
        // before touching the crate, so the `LazyLock` behind
        // `cue_ir_enabled()` cannot have been forced yet.
        //
        // SAFETY: `Once` serializes the test threads and none has called into
        // the crate or spawned anything that reads the environment.
        unsafe {
            std::env::set_var("FCAST_NO_CUE_IR", "1");
        }
        assert!(
            !fcastplaybin::cue_ir_enabled(),
            "the lever must be off for this whole binary"
        );

        gst::init().expect("gst init");
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
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

struct Attached {
    playbin: FcastPlaybin,
    feed: Arc<Mutex<Vec<SubtitleFeedItem>>>,
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
            "timed out waiting for {what}; {} items delivered",
            self.feed.lock().expect("feed").len()
        );
    }

    fn cues(&self) -> Vec<(SubtitleTextFormat, String)> {
        self.feed
            .lock()
            .expect("feed")
            .iter()
            .filter_map(|item| match item {
                SubtitleFeedItem::Cue { format, text, .. } => Some((format.clone(), text.clone())),
                // Nothing reaches this harness through the bitmap arm. The
                // match is total so a new variant is not silently hidden.
                SubtitleFeedItem::Bitmap { .. } | SubtitleFeedItem::Clear => None,
            })
            .collect()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.playbin.stop();
        self.media.unregister();
        let _ = std::fs::remove_file(&self.srt);
    }
}

/// `force_property` mirrors the receiver's `deep-element-added` hook. `false`
/// is what the receiver does under the lever. `true` forces the parsers on
/// anyway, to isolate the driver's half of the lever.
fn attach_and_select(tag: &str, force_property: bool) -> Attached {
    init();
    let media = ScenarioBuilder::new(tag)
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(60))
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let srt = std::env::temp_dir().join(format!("fcast-lever-{tag}-{}.srt", std::process::id()));
    std::fs::write(&srt, STYLED_SRT).expect("writing the srt");

    let playbin = FcastPlaybin::new(Sinks {
        video: Some(FTestSink::new().upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");

    if force_property {
        playbin
            .pipeline()
            .connect_deep_element_added(|_, _, element| {
                let Some(factory) = element.factory() else {
                    return;
                };
                if matches!(factory.name().as_str(), "rssubparse" | "rsssaparse") {
                    element.set_property_from_str("text-format", "cue-ir");
                }
            });
    }

    let feed: Arc<Mutex<Vec<SubtitleFeedItem>>> = Default::default();
    let tap = feed.clone();
    playbin.set_subtitle_consumer(move |item| {
        tap.lock().expect("feed").push(item);
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

/// Negotiation half. With the lever set the receiver never asks for cue-ir,
/// so the parsers negotiate `pango-markup` and the consumer is handed the
/// format the caps imply.
#[test]
fn the_lever_restores_the_pango_markup_negotiation() {
    let attached = attach_and_select("leverdefault", false);
    attached.wait_for("a cue", || !attached.cues().is_empty());

    for (format, text) in attached.cues() {
        assert_eq!(
            format,
            SubtitleTextFormat::PangoMarkup,
            "under the lever the caps are pango-markup and nothing else; \
             got {format:?} for {text:?}"
        );
    }
}

/// Driver half. The parsers are forced into cue-ir mode, so every buffer
/// carries a `CueIrMeta`. The lever must make the driver ignore it and hand
/// the consumer `Utf8`, never a `CueIr` variant. Otherwise the lever would be
/// only a receiver-side switch.
#[test]
fn the_lever_makes_the_driver_ignore_the_meta() {
    let attached = attach_and_select("leverforced", true);
    attached.wait_for("a cue", || !attached.cues().is_empty());

    let cues = attached.cues();
    for (format, text) in &cues {
        assert_eq!(
            *format,
            SubtitleTextFormat::Utf8,
            "the parsers are in cue-ir mode (caps=utf8) but the lever forbids \
             reading the meta; got {format:?} for {text:?}"
        );
    }
    // Proof the parsers really were in cue-ir mode. That mode consumes the
    // styling tag, so the payload differs visibly from the markup-stripped
    // classic output.
    assert!(
        cues.iter().any(|(_, text)| text == "plain and red"),
        "expected the cue-ir payload, got {:?}",
        cues.iter().map(|(_, t)| t).collect::<Vec<_>>()
    );
}
