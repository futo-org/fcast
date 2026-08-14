//! The cue-IR transport: a real `.srt` through the real external chain with
//! the parser in `text-format=cue-ir` mode, and what the consumer is handed.
//!
//! Cue-IR negotiates the same utf8 caps as plain text and carries its styling
//! in a buffer meta. The interesting assertions are that the payload is still
//! readable text, that the structure arrived beside it, and that the meta
//! survived the whole transport rather than being dropped by a buffer copy.
//!
//! `tests/cue_ir_lever.rs` owns the `FCAST_NO_CUE_IR=1` half.
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

/// A small styled file. The pango output's tag whitelist deletes
/// `<font color>`, and `{\an8}` is shown literally by the classic output, so
/// both only take effect on the cue-IR path.
const STYLED_SRT: &str = "\
1
00:00:01,000 --> 00:00:03,000
plain and <font color=\"#ff0000\">red</font>

2
00:00:04,000 --> 00:00:06,000
{\\an8}pinned to the top
";

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gst::init().expect("gst init");
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
        // The receiver's rank swap. The Rust parsers own every subtitle
        // stream, matching the chain the receiver really autoplugs.
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

fn write_srt(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("fcast-cueir-{tag}-{}.srt", std::process::id()));
    std::fs::write(&path, STYLED_SRT).expect("writing the srt");
    path
}

pub struct Attached {
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

    /// The cues delivered so far, in order.
    ///
    /// A cue may appear more than once. The branch is unsynced, so the parser
    /// hands the whole file over at once and a replay re-delivers it. Tests
    /// pick cues by content, never by position.
    fn cues(&self) -> Vec<(SubtitleTextFormat, String)> {
        self.feed
            .lock()
            .expect("feed")
            .iter()
            .filter_map(|item| match item {
                SubtitleFeedItem::Cue { format, text, .. } => Some((format.clone(), text.clone())),
                // Unreachable here. Spelled out rather than swept under a
                // catch-all.
                SubtitleFeedItem::Bitmap { .. } | SubtitleFeedItem::Clear => None,
            })
            .collect()
    }

    /// The first delivery whose plain text is `text`.
    fn cue_saying(&self, text: &str) -> Option<(SubtitleTextFormat, String)> {
        self.cues().into_iter().find(|(_, got)| got.trim() == text)
    }

    /// Whether both of the file's cues have been delivered at least once.
    fn both_cues_in(&self, second: &str) -> bool {
        self.cue_saying("plain and red").is_some() && self.cue_saying(second).is_some()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.playbin.stop();
        self.media.unregister();
        let _ = std::fs::remove_file(&self.srt);
    }
}

/// Stage a playing item with the styled SRT attached and selected.
///
/// `select_cue_ir` is the receiver's own wiring, a `deep-element-added` hook
/// that sets `text-format=cue-ir` on the autoplugged parsers. `false` is the
/// pre-cue-IR behaviour.
pub fn attach_and_select(tag: &str, select_cue_ir: bool) -> Attached {
    init();
    let media = ScenarioBuilder::new(tag)
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(60))
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let srt = write_srt(tag);

    let playbin = FcastPlaybin::new(Sinks {
        video: Some(FTestSink::new().upcast()),
        audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    })
    .expect("building fcastplaybin");

    if select_cue_ir {
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

/// Cues arrive as [`SubtitleTextFormat::CueIr`], carrying the structure the
/// classic output cannot express, through the whole real transport.
#[test]
fn an_srt_parsed_as_cue_ir_reaches_the_consumer_with_its_styling() {
    let attached = attach_and_select("cueirtransport", true);
    // The payload is still readable text, and finding cues by that text is
    // itself the assertion. A consumer ignoring the IR still shows subtitles.
    attached.wait_for("both cues", || attached.both_cues_in("pinned to the top"));

    let (first, _) = attached
        .cue_saying("plain and red")
        .expect("just waited for it");
    let SubtitleTextFormat::CueIr { ir, pts_start } = &first else {
        panic!("expected a cue-IR cue, got {first:?}");
    };
    // The meta survived decodebin3's queues, the text queue and the appsink.
    let reds = ir
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| {
            s.style
                .foreground
                .is_some_and(|c| c.r > 200 && c.g < 60 && c.b < 60)
        })
        .count();
    assert_eq!(
        reds, 1,
        "the <font color> span reached the consumer: {ir:#?}"
    );
    // The pts anchor karaoke reveal times are absolute against.
    assert!(pts_start.is_some(), "a cue-IR cue carries its pts anchor");

    let (second, _) = attached
        .cue_saying("pinned to the top")
        .expect("just waited for it");
    let SubtitleTextFormat::CueIr { ir, .. } = &second else {
        panic!("expected a cue-IR cue, got {second:?}");
    };
    assert!(
        ir.layout.anchor.is_some(),
        "the {{\\an8}} placement reached the consumer: {ir:#?}"
    );
}

/// Without the receiver's property hook the parsers stay in default mode and
/// every cue arrives as `PangoMarkup`, styling inline in the text.
///
/// Same staging as the test above with one input changed, so the difference
/// is attributable to that input alone.
#[test]
fn without_the_property_the_transport_is_unchanged() {
    let attached = attach_and_select("cueirdefault", false);
    let saying = |needle: &str| {
        attached
            .cues()
            .into_iter()
            .find(|(_, text)| text.contains(needle))
    };
    attached.wait_for("both cues", || {
        saying("plain and").is_some() && saying("pinned to the top").is_some()
    });

    for (format, text) in attached.cues() {
        assert_eq!(
            format,
            SubtitleTextFormat::PangoMarkup,
            "the default mode negotiates pango-markup; got {format:?} for {text:?}"
        );
    }
    // The classic lossiness is intact. The tag whitelist deletes
    // <font color>, and the positioning block is shown as literal text.
    let (_, first) = saying("plain and").expect("just waited for it");
    assert_eq!(
        first, "plain and red",
        "the <font color> tag is deleted from the classic output"
    );
    let (_, second) = saying("pinned to the top").expect("just waited for it");
    assert!(
        second.contains("{\\an8}"),
        "the classic output shows the positioning block as text: {second:?}"
    );
}
