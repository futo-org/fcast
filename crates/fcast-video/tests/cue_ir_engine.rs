//! THE CUE-IR PATH, END TO END: a real subtitle file through the real parser
//! element in `text-format=cue-ir` mode, into the real [`CueEngine`], out as
//! pixels.
//!
//! Everything here runs the production chain except the transport: the driver's
//! `item_from_sample` is the piece that lifts `CueIrMeta` off a buffer and the
//! `flapjack` suites own that half (`tests/cue_ir_transport.rs`), so this
//! one brackets the parser with appsrc/appsink and does the same lift inline.
//! What it owns is the claim the driver cannot make from its side: that the IR
//! a real file produces actually RENDERS, with its styling, its placement and
//! its karaoke.
//!
//! No display server, no GL: parley lays out and vello_cpu rasterizes into a
//! plain pixmap, exactly as the sink's raster worker does.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use fcast_video::{
    cue::{CueEngine, CueInput, TextFormat},
    cue_ir::{CueIr, CueStyle, VideoRect},
};
use gst::prelude::*;
use gstrssubparse::cueir::CueIrMeta;

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gst::init().expect("gst init");
        gstrssubparse::plugin_register_static().expect("registering rssubparse");
    });
}

/// One cue as the transport would hand it over: the parsed IR, the plain text
/// that travels as the buffer payload, and the window it covers.
#[derive(Debug)]
struct Delivered {
    ir: Arc<CueIr>,
    text: String,
    pts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
}

/// Push `bytes` through `element` in cue-IR mode and collect what comes out.
///
/// `caps` picks the parser: `application/x-subtitle` for everything
/// `rssubparse` autodetects, `application/x-ssa` for the separate `rsssaparse`
/// element, the same split the receiver's autoplugging makes.
fn parse_as_cue_ir(element: &str, caps: &str, bytes: &[u8]) -> Vec<Delivered> {
    init();

    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src caps={caps} ! {element} name=parse ! appsink name=sink sync=false"
    ))
    .expect("building the parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("a launch line is a pipeline");

    let parser = pipeline.by_name("parse").expect("the parser element");
    // THE SELECTION MECHANISM, verbatim: a property, set before the element
    // leaves READY (it is `mutable_ready`, and the mode is latched when the src
    // caps are chosen). The receiver sets exactly this, through a
    // `deep-element-added` hook, because decodebin3 builds the element itself.
    parser.set_property_from_str("text-format", "cue-ir");

    let src = pipeline
        .by_name("src")
        .expect("appsrc")
        .downcast::<gst_app::AppSrc>()
        .expect("appsrc downcast");
    let sink = pipeline
        .by_name("sink")
        .expect("appsink")
        .downcast::<gst_app::AppSink>()
        .expect("appsink downcast");

    pipeline.set_state(gst::State::Playing).expect("playing");
    src.push_buffer(gst::Buffer::from_slice(bytes.to_vec()))
        .expect("pushing the subtitle data");
    src.end_of_stream().expect("eos");

    let mut out = Vec::new();
    while let Ok(sample) = sink.pull_sample() {
        let Some(buffer) = sample.buffer() else {
            continue;
        };
        // THE CAPS ARE PLAIN UTF-8. Cue-IR is not a caps format: the payload is
        // the cue's own plain text and the styling rides in a buffer meta, so a
        // consumer that only knows `text/x-raw` still gets readable subtitles.
        // The driver's caps gate therefore cannot tell this stream from any
        // other utf8 one, and deliberately does not try.
        let structure = sample
            .caps()
            .and_then(|c| c.structure(0).map(|s| s.to_owned()))
            .expect("caps on the sample");
        assert_eq!(structure.name(), "text/x-raw");
        assert_eq!(
            structure.get::<&str>("format").ok(),
            Some("utf8"),
            "cue-ir must negotiate plain utf8, not a format of its own"
        );

        let meta = buffer
            .meta::<CueIrMeta>()
            .expect("every cue-ir buffer carries a CueIrMeta");
        let map = buffer.map_readable().expect("mapping the payload");
        let text = std::str::from_utf8(map.as_slice())
            .expect("the payload is the IR's plain text, so always valid UTF-8")
            .to_owned();
        out.push(Delivered {
            ir: Arc::new(meta.ir().clone()),
            text,
            pts: buffer.pts(),
            duration: buffer.duration(),
        });
    }
    pipeline.set_state(gst::State::Null).expect("null");
    out
}

/// Hand a delivered cue to the engine the way the receiver's consumer closure
/// does, resolving its window into running time (here: pts as-is, since the
/// test segment starts at zero).
fn submit(engine: &CueEngine, cue: &Delivered) {
    let start_rt = cue.pts.unwrap_or(gst::ClockTime::ZERO);
    engine.submit(CueInput {
        format: TextFormat::CueIr {
            ir: cue.ir.clone(),
            pts_start: cue.pts,
        },
        text: cue.text.clone(),
        start_rt,
        end_rt: cue.duration.and_then(|d| start_rt.checked_add(d)),
    });
}

fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn ready_overlay(engine: &CueEngine) -> fcast_video::video::Overlay {
    wait_for("the raster to become ready", || {
        !engine.current_overlays().is_empty()
    });
    engine
        .current_overlays()
        .into_iter()
        .next()
        .expect("just checked")
}

/// Pixels matching `pred`, as a count.
fn count(overlay: &fcast_video::video::Overlay, pred: impl Fn(&[u8]) -> bool) -> usize {
    overlay.pixels.chunks_exact(4).filter(|px| pred(px)).count()
}

const STYLED_SRT: &str = "\
1
00:00:01,000 --> 00:00:03,000
plain and <font color=\"#ff0000\">red</font>

2
00:00:04,000 --> 00:00:06,000
{\\an8}pinned to the top
";

/// A voice span and a class span, the two WebVTT constructs behind the field
/// report. `<v Voice1>` is what pango's parser rejects outright.
const VOICE_VTT: &str = "\
WEBVTT

00:00:01.000 --> 00:00:03.000
<v Voice1>Hello there</v> and <c.yellow>yellow</c>
";

/// THE FIELD CASE, ON THE ARM THAT HANDLES IT: a WebVTT voice span through
/// `text-format=cue-ir` renders clean, no literal tags anywhere, and the
/// speaker survives as structure rather than as text.
///
/// The pango arm cannot do this. `subparse-formats` keeps `<v Speaker>` in its
/// pango-markup output deliberately (byte-identical to the C `subparse`, whose
/// whitelist has the same wart), pango rejects it, and before the sanitizer in
/// `cue.rs` the viewer read the tags. Here the tag never becomes text at all:
/// the IR consumes it into `Span::voice`, and `<c.yellow>` becomes an actual
/// colour, which is the styling the other arm has no way to express.
#[test]
fn a_webvtt_voice_span_renders_cleanly_through_cue_ir() {
    let cues = parse_as_cue_ir("rssubparse", "application/x-subtitle", VOICE_VTT.as_bytes());
    assert_eq!(cues.len(), 1, "one cue in the file: {cues:#?}");
    let cue = &cues[0];

    // THE PAYLOAD CARRIES NO MARKUP. This is the whole difference: a consumer
    // that does nothing but display `text` already shows the right thing.
    assert_eq!(cue.text, "Hello there and yellow");
    assert!(
        !cue.text.contains('<') && !cue.text.contains('>'),
        "no tag may survive into the payload: {:?}",
        cue.text
    );

    let spans: Vec<_> = cue.ir.lines.iter().flat_map(|l| l.spans.iter()).collect();
    // The speaker is attributed, not printed.
    let voiced = spans
        .iter()
        .find(|s| s.voice.is_some())
        .expect("the voice span is attributed: {cue:#?}");
    assert_eq!(voiced.voice.as_deref(), Some("Voice1"));
    assert_eq!(voiced.text, "Hello there");
    // `<c.yellow>` is one of WebVTT's default colour classes, and 651558c maps
    // it through to a real colour on this path.
    let classed = spans
        .iter()
        .find(|s| !s.classes.is_empty())
        .expect("the class span is kept: {cue:#?}");
    assert_eq!(classed.classes, vec!["yellow".to_string()]);
    assert_eq!(
        classed.style.foreground.map(|c| (c.r, c.g, c.b)),
        Some((255, 255, 0)),
        "the yellow class carries its colour: {:#?}",
        classed
    );

    // ...and it reaches the screen as yellow pixels.
    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    submit(&engine, cue);
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));

    let overlay = ready_overlay(&engine);
    let white = count(&overlay, |px| {
        px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200
    });
    let yellow = count(&overlay, |px| {
        px[3] > 200 && px[0] > 180 && px[1] > 180 && px[2] < 60
    });
    assert!(
        white > 50,
        "the unclassed words paint white, got {white} px"
    );
    assert!(
        yellow > 50,
        "the <c.yellow> span paints yellow, got {yellow} px"
    );
}

/// THE HEADLINE: an SRT through `rssubparse text-format=cue-ir` renders a cue,
/// with the styling the pango arm throws away.
///
/// `<font color>` is the one styling tag real SRT files actually use, and the C
/// element's tag whitelist deletes it, so "the cue is red" is only expressible
/// at all on this path. The assertion is on PIXELS, not on the IR: the point is
/// that the styling survives parse, IR, layout and raster together.
#[test]
fn a_styled_srt_cue_renders_its_colour_through_the_engine() {
    let cues = parse_as_cue_ir(
        "rssubparse",
        "application/x-subtitle",
        STYLED_SRT.as_bytes(),
    );
    assert_eq!(cues.len(), 2, "two cues in the file: {cues:#?}");

    // The payload stayed readable: a consumer ignoring the IR shows this.
    assert_eq!(cues[0].text, "plain and red");
    // ...and the structure is there beside it.
    let reds = cues[0]
        .ir
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| {
            s.style
                .foreground
                .is_some_and(|c| c.r > 200 && c.g < 60 && c.b < 60)
        })
        .count();
    assert_eq!(reds, 1, "one red span: {:#?}", cues[0].ir);

    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    submit(&engine, &cues[0]);
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));

    let overlay = ready_overlay(&engine);
    assert_eq!(
        overlay.pixels.len(),
        (overlay.width * overlay.height * 4) as usize
    );
    assert_eq!(overlay.space, fcast_video::video::OverlaySpace::Window);
    // White for "plain and", red for "red", dark for the outline behind both.
    let white = count(&overlay, |px| {
        px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200
    });
    let red = count(&overlay, |px| {
        px[3] > 200 && px[0] > 180 && px[1] < 60 && px[2] < 60
    });
    let dark = count(&overlay, |px| {
        px[3] > 200 && px[0] < 60 && px[1] < 60 && px[2] < 60
    });
    assert!(white > 50, "expected white glyph fill, got {white} px");
    assert!(red > 50, "expected the red span to paint, got {red} px");
    assert!(dark > 50, "expected the outline, got {dark} px");
    // Default placement is the bottom strip.
    assert!(overlay.y as u32 > 360 / 2, "cue at y={}", overlay.y);
}

/// `{\an8}` is a POSITIONING instruction the C shows as literal text. Here it
/// moves the cue, and, because it is an explicit placement, it anchors to the
/// PICTURE, not the window, even under the window-margins policy.
#[test]
fn an_anchored_srt_cue_is_placed_against_the_video_rect() {
    let cues = parse_as_cue_ir(
        "rssubparse",
        "application/x-subtitle",
        STYLED_SRT.as_bytes(),
    );
    let anchored = &cues[1];
    assert_eq!(
        anchored.text, "pinned to the top",
        "the {{\\an8}} block is consumed, not shown: {anchored:#?}"
    );
    assert!(
        anchored.ir.layout.anchor.is_some(),
        "the anchor reached the IR: {:#?}",
        anchored.ir
    );

    // A letterboxed picture: 45px bars top and bottom of a 640x360 window.
    let rect = VideoRect {
        x: 0,
        y: 45,
        width: 640,
        height: 270,
    };
    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    engine.set_video_rect(Some(rect));
    submit(&engine, anchored);
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(4_500)));

    let overlay = ready_overlay(&engine);
    // Top-anchored, and inside the picture rather than up in the window's bar.
    assert!(
        overlay.y >= 45,
        "a top-anchored cue belongs over the picture's top (y>=45), got {}",
        overlay.y
    );
    assert!(
        (overlay.y as u32) < 360 / 2,
        "{{\\an8}} must move the cue off the bottom strip, got y={}",
        overlay.y
    );
}

/// THE PAUSED-CUE CONTRACT, for cue-IR cues.
///
/// A cue that covers the frame already on screen must become visible with NO
/// frame flowing: the engine repaints off `current_overlays` and signals
/// through the `on_change` callback the sink turns into `overlays-changed`.
/// The staging mirrors `flapjack`'s
/// `a_paused_cue_covers_the_frozen_frame_without_resuming`, from the engine's
/// side: one frame establishes the frozen running time, then nothing moves.
#[test]
fn a_paused_cue_ir_cue_repaints_without_a_frame() {
    let cues = parse_as_cue_ir(
        "rssubparse",
        "application/x-subtitle",
        STYLED_SRT.as_bytes(),
    );

    let engine = CueEngine::new();
    engine.set_canvas(640, 360);

    let repaints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = repaints.clone();
    engine.set_on_change(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });

    // One frame at 1.5s freezes the clock there. Nothing is scheduled yet, so
    // nothing is showing.
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));
    assert!(engine.current_overlays().is_empty());
    let before = repaints.load(std::sync::atomic::Ordering::Relaxed);

    // The cue arrives late, covering the frozen frame. No `overlays_for` call
    // from here on: this is the paused path in full.
    submit(&engine, &cues[0]);

    wait_for("the paused repaint", || {
        !engine.current_overlays().is_empty()
    });
    assert!(
        repaints.load(std::sync::atomic::Ordering::Relaxed) > before,
        "the engine must signal a repaint with no frame flowing"
    );
    assert!(
        engine.take_dirty(),
        "the dirty flag is what the sink's overlays-changed handler reads"
    );
    let overlay = ready_overlay(&engine);
    assert!(overlay.width > 0 && overlay.height > 0);
}

const KARAOKE_ASS: &str = "\
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,32,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:04.00,Default,,0,0,0,,{\\k100}first {\\k100}second {\\k100}third
";

/// KARAOKE: `\k` syllables become per-span reveal times, and the engine re-keys
/// its raster as the frame clock passes each one.
///
/// Two things are being held down at once. The obvious one is that the picture
/// CHANGES at a syllable boundary, more ink, same geometry, because an
/// unrevealed span still occupies its space so the line cannot reflow. The
/// subtle one is that it never goes BLANK doing it: a re-key keeps the previous
/// raster on screen (`RasterState::Stale`) until the replacement lands, which
/// is the difference between a line that fills in and a line that strobes.
#[test]
fn karaoke_syllables_reveal_progressively_without_blinking() {
    let cues = parse_as_cue_ir("rsssaparse", "application/x-ssa", KARAOKE_ASS.as_bytes());
    assert_eq!(cues.len(), 1, "one dialogue line: {cues:#?}");
    let cue = &cues[0];

    let reveals: Vec<u64> = cue
        .ir
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter_map(|s| s.reveal_ns)
        .collect();
    assert!(
        reveals.len() >= 2,
        "the \\k tags must become reveal times: {:#?}",
        cue.ir
    );

    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    submit(&engine, cue);

    // Before any syllable has fired.
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(100)));
    let first = ready_overlay(&engine);

    // Past the first \k boundary (100 centiseconds = 1s).
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));
    assert!(
        !engine.current_overlays().is_empty(),
        "a step crossing must never blank the line: the previous raster keeps \
         showing until the replacement lands"
    );

    wait_for("the revealed raster", || {
        engine
            .current_overlays()
            .first()
            .is_some_and(|o| o.pixels != first.pixels)
    });
    let later = engine
        .current_overlays()
        .into_iter()
        .next()
        .expect("just checked");
    // No reflow: the unrevealed spans were already holding their space.
    assert_eq!(
        (later.width, later.height),
        (first.width, first.height),
        "revealing a syllable must not resize the cue"
    );
    // GLYPH INK, not "any painted pixel". Since the readability box became the
    // default, every pixel of the cue's rectangle has non-zero alpha, so an
    // alpha>0 count measures the box and is identical before and after. The
    // white fill is the part a reveal actually adds.
    let ink = |o: &fcast_video::video::Overlay| {
        count(o, |px| {
            px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200
        })
    };
    assert!(
        ink(&later) > ink(&first),
        "revealing a syllable must paint MORE ink, got {} then {}",
        ink(&first),
        ink(&later)
    );
}

/// The house style is live-settable and the active cue re-rasters, paused
/// included, and it never blanks while doing so, for the same `Stale` reason
/// karaoke does not.
#[test]
fn a_house_style_change_rerasters_the_active_cue_while_paused() {
    let cues = parse_as_cue_ir(
        "rssubparse",
        "application/x-subtitle",
        STYLED_SRT.as_bytes(),
    );

    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    submit(&engine, &cues[0]);
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));
    let before = ready_overlay(&engine);

    // No frame flows from here: a settings toggle alone must repaint.
    //
    // The toggle is now the box OFF rather than on. It used to switch the
    // default (outline, no box) to `boxed()` and assert coverage GREW; since
    // the box became the default, `boxed()` differs from it only by dropping
    // the outline, and the covered area is identical either way. Turning the
    // box off is the change that still has a direction -- and it is the one a
    // user reaching for this setting is most likely to make.
    engine.set_style(CueStyle::outline_only());
    assert!(
        !engine.current_overlays().is_empty(),
        "the previous raster must keep showing while the restyle renders"
    );
    wait_for("the restyled raster", || {
        engine
            .current_overlays()
            .first()
            .is_some_and(|o| o.pixels != before.pixels)
    });

    // Without the slab behind it the cue covers far less of the picture.
    let after = engine
        .current_overlays()
        .into_iter()
        .next()
        .expect("just checked");
    let covered = |o: &fcast_video::video::Overlay| count(o, |px| px[3] > 100);
    assert!(
        covered(&after) < covered(&before),
        "dropping the readability box must uncover the picture: {} then {}",
        covered(&before),
        covered(&after)
    );
}

/// COEXISTENCE: the same file parsed the DEFAULT way still renders through the
/// pango arm, unchanged. Nothing about cue-IR is allowed to disturb it, test
/// sources emit utf8 and any stream negotiating pango-markup must render as it
/// always did.
#[test]
fn the_pango_arm_still_renders_the_same_file() {
    init();
    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    engine.submit(CueInput {
        format: TextFormat::PangoMarkup,
        text: "plain and <i>italic</i>".to_owned(),
        start_rt: gst::ClockTime::from_mseconds(1_000),
        end_rt: Some(gst::ClockTime::from_mseconds(3_000)),
    });
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));

    let overlay = ready_overlay(&engine);
    let white = count(&overlay, |px| {
        px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200
    });
    assert!(white > 50, "the pango arm still paints, got {white} px");
    assert!(overlay.y as u32 > 360 / 2);
}

/// THE OWNER'S REPRO, END TO END: two overlapping WebVTT cues, through the real
/// parser, both on screen at once and stacked.
///
/// `OVERLAPPING_VTT` is `vtt-overlapping-cues-single-active.vtt` at the repo
/// root, byte for byte. Before the multi-active change this file showed one
/// subtitle at a time: the second cue's start REPLACED the first, and the
/// viewer never saw the two lines together the file describes.
///
/// Both cues carry an explicit `line:` position (100% and 99%), which after the
/// keep-it-inside-the-window clamp resolves to the SAME strip. Honouring the
/// file is therefore not enough on its own, the two would land in the same
/// pixels, and it is the stack that separates them, which is what a browser
/// does with the same file.
const OVERLAPPING_VTT: &str = "\
WEBVTT

00:00:00.000 --> 00:00:05.000 line:100%
This is a test subtitle

00:00:01.000 --> 00:00:05.000 line:99%
This is another test subtitle
";

/// The same shape with the windows staggered, so that per-cue EXPIRY is
/// visible: the two cues in the repro end together, which cannot distinguish
/// "each cue leaves on its own end" from "they both left at once".
///
/// Staggered the other way -- a second cue ENDING before the first -- is not
/// testable through this parser and that is a parser property, not an engine
/// one: `webvtt.rs`'s weak monotonicity guard (`prev_end <= ts_end`, mirroring
/// the C element's `start_time += duration`) drops a cue whose end is before
/// the previous cue's end outright, so a fully CONTAINED cue never reaches any
/// engine. The engine's own side of that case is pinned in `cue.rs`'s unit
/// tests, which schedule the cues directly.
const STAGGERED_VTT: &str = "\
WEBVTT

00:00:00.000 --> 00:00:03.000
The first line

00:00:01.000 --> 00:00:05.000
The second line
";

#[test]
fn two_overlapping_webvtt_cues_are_on_screen_together_and_stacked() {
    let cues = parse_as_cue_ir(
        "rssubparse",
        "application/x-subtitle",
        OVERLAPPING_VTT.as_bytes(),
    );
    assert_eq!(cues.len(), 2, "two cues in the file: {cues:#?}");
    assert_eq!(cues[0].pts, Some(gst::ClockTime::ZERO));
    assert_eq!(cues[1].pts, Some(gst::ClockTime::from_seconds(1)));

    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    for cue in &cues {
        submit(&engine, cue);
    }

    // Before the second cue starts: one cue, where the file put it.
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(500)));
    let first = ready_overlay(&engine);
    assert_eq!(
        engine.current_overlays().len(),
        1,
        "the second cue was on screen before its start time"
    );

    // DURING THE OVERLAP: both, and neither covering the other.
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(2_000)));
    wait_for("both cues to reach the screen", || {
        engine.current_overlays().len() == 2
    });
    let overlays = engine.current_overlays();
    let (bottom, top) = (&overlays[0], &overlays[1]);
    assert!(
        bottom.y > top.y,
        "the earlier cue must hold the bottom line: {} and {}",
        bottom.y,
        top.y
    );
    assert!(
        top.y + top.height as i32 <= bottom.y,
        "the two cues overlap: the upper one spans {}..{} and the lower starts at {}",
        top.y,
        top.y + top.height as i32,
        bottom.y
    );
    assert!(
        Arc::ptr_eq(&bottom.pixels, &first.pixels),
        "the cue already showing was re-rastered when the second arrived"
    );
    for overlay in overlays.iter() {
        let ink = count(overlay, |px| {
            px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200
        });
        assert!(
            ink > 50,
            "an overlay carries no glyph ink at all, got {ink}"
        );
    }

    // Both end together in this file, and both go.
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(5_000)));
    assert!(engine.current_overlays().is_empty());
}

/// Per-cue expiry, on the same path: the cue that ends first leaves, and the
/// one that outlives it stays exactly where it was.
///
/// Under the single-active rule this frame was BLANK for the first cue's window
/// and the second cue's alone afterwards; here the two coexist and then the
/// survivor drops to the bottom line the first one vacated.
#[test]
fn each_overlapping_webvtt_cue_leaves_on_its_own_end() {
    let cues = parse_as_cue_ir(
        "rssubparse",
        "application/x-subtitle",
        STAGGERED_VTT.as_bytes(),
    );
    assert_eq!(cues.len(), 2, "two cues in the file: {cues:#?}");

    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    for cue in &cues {
        submit(&engine, cue);
    }

    engine.overlays_for(Some(gst::ClockTime::from_mseconds(2_000)));
    wait_for("both cues to reach the screen", || {
        engine.current_overlays().len() == 2
    });
    let during = engine.current_overlays();
    let survivor = during[1].pixels.clone();
    let bottom_y = during[0].y;
    assert!(during[1].y < bottom_y);

    // The first cue's end passes. The second is untouched by it -- and takes
    // the bottom line, since nothing is under it any more.
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(4_000)));
    let after = engine.current_overlays();
    assert_eq!(
        after.len(),
        1,
        "the surviving cue went with the expired one"
    );
    assert!(
        Arc::ptr_eq(&after[0].pixels, &survivor),
        "the wrong cue survived, or it was re-rastered"
    );
    assert_eq!(
        after[0].y, bottom_y,
        "with nothing below it, the surviving cue sits where the file asked"
    );

    engine.overlays_for(Some(gst::ClockTime::from_mseconds(5_000)));
    assert!(engine.current_overlays().is_empty());
}

/// RUBY, END TO END: `<ruby>base<rt>annotation</rt></ruby>` through the real
/// parser, rendered with the annotation over the text it belongs to.
///
/// The parser side was verified before any of this was written: `rssubparse`
/// in cue-ir mode fills `Span::ruby` with `Ruby { text, position }` and leaves
/// the annotation OUT of the plain-text payload (which is the documented
/// contract -- a consumer that only shows `text` shows the base). The renderer
/// was the half that dropped it.
const RUBY_VTT: &str = "\
WEBVTT

00:00:01.000 --> 00:00:03.000
<ruby>\u{6f22}\u{5b57}<rt>\u{304b}\u{3093}\u{3058}</rt></ruby> is kanji
";

/// The same cue with the annotation stripped, for the comparison.
const NO_RUBY_VTT: &str = "\
WEBVTT

00:00:01.000 --> 00:00:03.000
\u{6f22}\u{5b57} is kanji
";

fn only_overlay(vtt: &str) -> fcast_video::video::Overlay {
    let cues = parse_as_cue_ir("rssubparse", "application/x-subtitle", vtt.as_bytes());
    assert_eq!(cues.len(), 1, "one cue in the file: {cues:#?}");
    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    submit(&engine, &cues[0]);
    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));
    ready_overlay(&engine)
}

/// Rows of an overlay that carry GLYPH ink -- white, opaque pixels.
///
/// Not "any alpha": the default style paints a readability box behind the whole
/// cue, so every row inside it is non-transparent and an alpha test would
/// measure the box. The white fill is the text.
fn glyph_rows(overlay: &fcast_video::video::Overlay) -> Vec<usize> {
    let stride = overlay.width as usize * 4;
    (0..overlay.height as usize)
        .filter(|row| {
            overlay.pixels[row * stride..(row + 1) * stride]
                .chunks_exact(4)
                .any(|px| px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200)
        })
        .collect()
}

#[test]
fn a_webvtt_ruby_annotation_reaches_the_screen_above_its_base() {
    let cues = parse_as_cue_ir("rssubparse", "application/x-subtitle", RUBY_VTT.as_bytes());
    assert_eq!(cues.len(), 1, "one cue in the file: {cues:#?}");

    // The parser's half, stated rather than assumed.
    let spans: Vec<_> = cues[0]
        .ir
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .collect();
    let annotated = spans
        .iter()
        .find(|s| s.ruby.is_some())
        .expect("the base span carries its annotation");
    let ruby = annotated.ruby.as_ref().expect("just checked");
    assert_eq!(annotated.text, "\u{6f22}\u{5b57}");
    assert_eq!(ruby.text, "\u{304b}\u{3093}\u{3058}");
    assert_eq!(
        ruby.position,
        gstrssubparse::subparse_formats::ir::RubyPosition::Over
    );
    assert_eq!(
        cues[0].text, "\u{6f22}\u{5b57} is kanji",
        "the annotation must not leak into the plain-text payload"
    );

    let with_ruby = only_overlay(RUBY_VTT);
    let without = only_overlay(NO_RUBY_VTT);

    // The annotation band: with it, the cue is taller and its ink starts
    // farther from the bottom edge than the base's ink ever does.
    let (ruby_rows, plain_rows) = (glyph_rows(&with_ruby), glyph_rows(&without));
    assert!(!ruby_rows.is_empty() && !plain_rows.is_empty());
    let from_bottom = |o: &fcast_video::video::Overlay, rows: &[usize]| o.height as usize - rows[0];
    assert!(
        from_bottom(&with_ruby, &ruby_rows) > from_bottom(&without, &plain_rows),
        "nothing was drawn above the base: ink starts {} px from the bottom against {} px without \
         the annotation",
        from_bottom(&with_ruby, &ruby_rows),
        from_bottom(&without, &plain_rows)
    );
    assert!(
        with_ruby.height > without.height,
        "the annotated cue is no taller than the bare one, so the annotation is sharing the \
         base's line"
    );

    // THE READABILITY BOX COVERS IT. The default style paints a tinted slab
    // behind the whole cue; the annotation band must be inside it, or furigana
    // over a bright shot is unreadable exactly where the text below it is fine.
    let stride = with_ruby.width as usize * 4;
    let top = ruby_rows[0];
    let tinted = with_ruby.pixels[top * stride..(top + 1) * stride]
        .chunks_exact(4)
        .filter(|px| px[3] > 100 && px[0] < 80 && px[1] < 80 && px[2] < 80)
        .count();
    // MOST of the row, not merely some of it: the glyphs' own black outline
    // would satisfy "some dark pixels" with no box at all, while the box spans
    // the cue's whole width.
    assert!(
        tinted * 2 > with_ruby.width as usize,
        "the row the annotation starts on is only {tinted} of {} px dark: the box stops at the \
         base text and the furigana has nothing behind it",
        with_ruby.width
    );
}

/// A ruby-bearing cue takes its FULL height in the multi-active stack: the cue
/// stacked above it must clear the annotation, not just the base text.
#[test]
fn a_ruby_cue_claims_its_annotation_when_cues_stack() {
    let cues = parse_as_cue_ir("rssubparse", "application/x-subtitle", RUBY_VTT.as_bytes());
    let engine = CueEngine::new();
    engine.set_canvas(640, 360);
    submit(&engine, &cues[0]);
    // A second, plain cue overlapping it.
    engine.submit(CueInput {
        format: TextFormat::Utf8,
        text: "The line above".to_owned(),
        start_rt: gst::ClockTime::from_mseconds(1_200),
        end_rt: Some(gst::ClockTime::from_mseconds(3_000)),
    });

    engine.overlays_for(Some(gst::ClockTime::from_mseconds(1_500)));
    wait_for("both cues to reach the screen", || {
        engine.current_overlays().len() == 2
    });

    let overlays = engine.current_overlays();
    let (bottom, top) = (&overlays[0], &overlays[1]);
    assert!(
        top.y + top.height as i32 <= bottom.y,
        "the cue above overlaps the annotated one: it spans {}..{} and the ruby cue starts at {}",
        top.y,
        top.y + top.height as i32,
        bottom.y
    );
}
