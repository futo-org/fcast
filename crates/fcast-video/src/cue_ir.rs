// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The cue-IR rasterizer: parley for layout, vello_cpu for pixels.
//!
//! Lifted from the cue-IR demo renderer in `gst-subparse-rs`. That code is a
//! whole engine, scheduler and renderer. Only the RENDERER half lives here. The
//! scheduling half stays in [`crate::cue`], which carries fixes the demo copy
//! never had (whole-file `PENDING_LIMIT`, redelivery merging, the
//! drop-furthest-future trim policy) and must not be regressed by a wholesale
//! swap.
//!
//! What that split buys is coexistence: `text/x-raw, format=pango-markup` and
//! `format=utf8` keep going through the pango/cairo rasterizer in
//! [`crate::cue`] (byte-identical output to before) while a stream parsed
//! with `text-format=cue-ir` gets this one, which understands styled spans,
//! per-cue positioning and karaoke reveal times.
//!
//! No pango and no cairo on this path: layout and rasterization are pure Rust.
//! NOT no fontconfig, though the module this was lifted from claimed as much:
//! parley enumerates system fonts through `fontique`, which links
//! `yeslogic-fontconfig-sys` on Linux. That is not a new dependency for this
//! workspace (slint already pulls `fontique` for the same reason), but it does
//! mean retiring the pango arm would free the pango/cairo stack and NOT
//! fontconfig.
//!
//! ## Contracts this module owns
//!
//! * **Position against the video rect.** Everything a subtitle file expresses
//!   (SSA `\pos`, WebVTT `line:`/`position:`, margins, SSA font sizes) is
//!   relative to the PICTURE, not the window (see [`place`] and [`VideoRect`]).
//!   A cue the file says nothing about is house policy, and
//!   [`CueStyle::use_window_margins`] lets it sit in the letterbox bars instead
//!   of covering the picture (mpv's `sub-use-margins`).
//! * **House style.** [`CueStyle`] is what the cue looks like where the file
//!   says nothing; everything the IR does specify overrides the corresponding
//!   field.
//! * **Hostile input is data, not a crash.** IR floats come straight out of
//!   subtitle files: every geometry value is validated in float space (NaN
//!   included) before any integer cast, and font sizes are clamped. Rejection
//!   is a "no pixels" answer, never a panic.

use std::sync::Arc;

use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamilyName, FontWeight, GenericFamily, GlyphRun,
    LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty,
};
use peniko::Color;
use tracing::{debug, warn};
use vello_cpu::{
    Glyph, Pixmap, RenderContext,
    kurbo::{Affine, Cap, Join, Rect, Stroke, Vec2},
};

/// The IR the parser elements attach to their buffers as a `CueIrMeta`.
///
/// Named through `gst-subparse`'s re-export rather than a direct
/// `subparse-formats` dependency, which is what that re-export is for: one
/// pin, no version to keep matched.
pub use gstrssubparse::subparse_formats::ir::{self, CueIr};

/// Refuse to allocate a raster larger than this in either dimension.
const MAX_RASTER_PX: i32 = 8192;

/// The rectangle the video actually occupies inside the window (after
/// aspect-ratio scaling), in window coordinates. The sink computes this for
/// rendering anyway; feeding it to `CueEngine::set_video_rect` is what anchors
/// positioned cues (SSA `\pos`, WebVTT `line:`/`position:`) to the *picture*
/// rather than the window, and sizes text against the picture height. Without
/// it, the whole window doubles as the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

// -- house style
// ----------------------------------------------------------------

/// Straight-alpha RGBA, toolkit-agnostic.
pub type Rgba = [u8; 4];

/// How cue text is presented when (and wherever) the subtitle file itself says
/// nothing: the *house style*. Set it with `CueEngine::set_style`, e.g. from a
/// user settings menu, and the active cue re-rasterizes. Everything the IR
/// specifies (colors, fonts, per-cue outline/shadow, positioning) still
/// overrides the corresponding field here.
///
/// All fractions of the font size are em-like (`0.5` = half the font size); the
/// font size itself is a fraction of the canvas height, so cues scale with the
/// real display.
#[derive(Debug, Clone, PartialEq)]
pub struct CueStyle {
    /// Font family; `None` = the platform's sans-serif.
    pub font_family: Option<String>,
    /// CSS-style weight (400 normal, 700 bold).
    pub font_weight: f32,
    /// Font size as a fraction of canvas height.
    pub font_height_fraction: f32,
    /// Never smaller than this, however small the window gets.
    pub min_font_px: f32,
    /// Wrap width as a fraction of canvas width.
    pub wrap_width_fraction: f32,
    /// Distance from the bottom edge, as a fraction of canvas height.
    pub bottom_margin_fraction: f32,
    /// Whether *default-placed* subtitles (no positioning in the file) may sit
    /// in the window's letterbox bars instead of covering the picture (mpv's
    /// `sub-use-margins`, and what the pango arm effectively does). Cues the
    /// file positions explicitly always track the video rectangle regardless.
    pub use_window_margins: bool,
    /// Stroked border behind the glyphs; `None` = no outline.
    pub outline: Option<OutlineStyle>,
    /// Box painted behind the whole cue; `None` = no box. When the subtitle
    /// itself asks for a cue background (SSA `BorderStyle=3`, WebVTT
    /// `::cue { background }`) that color wins, drawn with this box's geometry
    /// (or square and snug when this is `None`).
    pub background: Option<BackgroundStyle>,
}

/// A stroked border around the glyph edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlineStyle {
    pub color: Rgba,
    /// Stroke width as a fraction of the font size.
    pub width_fraction: f32,
}

/// The readability box behind the cue text.
///
/// Note this is a *tint*, not frosted glass: the raster is composited over the
/// video later, so a true backdrop blur cannot happen here (the video pixels do
/// not exist at raster time): it belongs to the GPU compositor.
/// `edge_softness` gives the CPU-side approximation: a gaussian-feathered rim
/// instead of a hard edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundStyle {
    pub color: Rgba,
    /// Corner radius as a fraction of the font size (`0.0` = square).
    pub corner_radius: f32,
    /// Space between the text's ink and the box edge, as a fraction of the
    /// font size.
    pub padding: f32,
    /// Gaussian feathering of the box edge, as a fraction of the font size
    /// (`0.0` = hard edge).
    pub edge_softness: f32,
}

/// The default readability box: semi-transparent black, modest padding,
/// rounded corners. Named because BOTH rasterizers use it: the pango arm in
/// [`crate::cue`] reads these very numbers, so the two arms cannot drift.
///
/// The values are the ones [`CueStyle::boxed`] was already tuned with upstream
/// (d82e9d5) rather than new ones: 160/255 is dark enough to carry white text
/// over a bright shot without hiding the picture, and the padding and radius
/// are fractions of the font size, so the box scales with the text. The one
/// departure from upstream: the corner radius is half of d82e9d5's 0.35,
/// owner preference, 2026-08-11 ("50% sharper").
pub const DEFAULT_BACKGROUND: BackgroundStyle = BackgroundStyle {
    color: [0, 0, 0, 160],
    corner_radius: 0.1,
    padding: 0.45,
    edge_softness: 0.0,
};

/// The default glyph outline: near-opaque black, 0.14em wide.
///
/// Also shared with the pango arm, which has always hardcoded exactly these
/// two numbers (width 0.14 of the font size, alpha 0.85 ≈ 217/255).
pub const DEFAULT_OUTLINE: OutlineStyle = OutlineStyle {
    color: [0, 0, 0, 217],
    width_fraction: 0.14,
};

impl Default for CueStyle {
    /// Bold white text, a black glyph outline, and a tinted rounded box behind
    /// the cue.
    ///
    /// THE BOX IS ON BY DEFAULT. It used to be opt-in (`background: None`,
    /// outline only), which is the mpv look: fine over dark footage and poor
    /// over bright or busy footage, where a thin outline is all that separates
    /// white glyphs from white background. The box is what broadcast and
    /// streaming captions use for the same reason.
    ///
    /// The outline stays. Box and outline are not alternatives here: the
    /// outline keeps the glyph edges crisp against the tint, and keeping it
    /// makes this change purely additive to what a cue already looked like.
    /// [`CueStyle::boxed`] is the box WITHOUT it, for anyone who wants the
    /// flatter caption look.
    fn default() -> Self {
        Self {
            font_family: None,
            font_weight: 700.0,
            font_height_fraction: 0.045,
            min_font_px: 12.0,
            wrap_width_fraction: 0.90,
            bottom_margin_fraction: 0.04,
            use_window_margins: true,
            outline: Some(DEFAULT_OUTLINE),
            background: Some(DEFAULT_BACKGROUND),
        }
    }
}

impl CueStyle {
    /// The flat boxed-captions look: the readability box with no glyph outline.
    pub fn boxed() -> Self {
        Self {
            outline: None,
            background: Some(DEFAULT_BACKGROUND),
            ..Self::default()
        }
    }

    /// No box: a bare glyph outline over the picture, which is what cues looked
    /// like before the box became the default. The escape hatch behind
    /// `CueEngine::set_style` for anyone who wants the picture unobscured.
    pub fn outline_only() -> Self {
        Self {
            background: None,
            outline: Some(DEFAULT_OUTLINE),
            ..Self::default()
        }
    }
}

// -- karaoke
// ---------------------------------------------------------------------

/// The reveal steps of a cue, as running times: sorted, deduplicated, and
/// anchored to `start_rt` by the pts the reveal times are absolute against,
/// scaled by the playback `rate` (reveal offsets are stream time; the engine's
/// clock is running time). Empty when the cue has no karaoke (the common case),
/// no pts anchor, or a non-forward rate.
pub fn reveal_steps(
    ir: &CueIr,
    start_rt: gst::ClockTime,
    pts_start: Option<gst::ClockTime>,
    rate: f64,
) -> Vec<gst::ClockTime> {
    let Some(pts) = pts_start else {
        return Vec::new();
    };
    if rate.is_nan() || rate <= 0.0 {
        return Vec::new();
    }
    let mut steps: Vec<u64> = ir
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter_map(|s| s.reveal_ns)
        .collect();
    steps.sort_unstable();
    steps.dedup();
    steps
        .into_iter()
        .map(|ns| {
            // Offset from the cue's own start; a reveal at/before the start is
            // step zero (visible immediately). Reveal times come straight out
            // of hostile subtitle files, so every operation here must be total:
            // the saturating add can land on u64::MAX, which is
            // GST_CLOCK_TIME_NONE and panics `from_nseconds`.
            let offset = ns.saturating_sub(pts.nseconds());
            let offset = (offset as f64 / rate) as u64; // saturating cast
            let rt = start_rt.nseconds().saturating_add(offset).min(u64::MAX - 1);
            gst::ClockTime::from_nseconds(rt)
        })
        .collect()
}

/// Whether `ir` carries any karaoke timing at all.
pub fn has_reveals(ir: &CueIr) -> bool {
    ir.lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .any(|s| s.reveal_ns.is_some())
}

/// How many reveal thresholds a span at `reveal_ns` sits behind, i.e. its rank
/// in the sorted step list. Rank 0 means "visible from the start".
fn reveal_rank(ir_steps: &[u64], reveal_ns: Option<u64>) -> usize {
    match reveal_ns {
        None => 0,
        Some(ns) => ir_steps.partition_point(|s| *s < ns) + 1,
    }
}

// -- the parley/vello rasterizer
// ----------------------------------------------------

/// Finished pixels plus their placement, in window coordinates. The engine
/// wraps this in its own `Raster`.
pub struct RasterOut {
    /// Tightly packed RGBA with straight (non-premultiplied) alpha.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

/// Parley brush for cue text: fill color, optional background box, the outline
/// stroke, and whether this span has been revealed yet (karaoke).
#[derive(Clone, Debug, PartialEq)]
struct CueBrush {
    fg: Color,
    bg: Option<Color>,
    /// `(color, width_px)`; every cue has one (the house style's if the IR says
    /// nothing).
    outline: (Color, f32),
    /// `(color, dx_px, dy_px)` drop shadow, when the IR sets one.
    shadow: Option<(Color, f32, f32)>,
    /// Unrevealed karaoke spans still occupy their space (layout must not
    /// reflow as syllables appear) but paint nothing.
    revealed: bool,
}

impl Default for CueBrush {
    fn default() -> Self {
        Self {
            fg: Color::WHITE,
            bg: None,
            outline: (Color::from_rgba8(0, 0, 0, 217), 1.0),
            shadow: None,
            revealed: true,
        }
    }
}

/// Line height as a multiple of the font size, for the cue and for the ruby
/// annotations that ride above it.
const LINE_HEIGHT: f32 = 1.2;

/// Ruby annotation size as a fraction of the text it annotates. Half is the
/// typographic convention for furigana (JIS: the annotation is set at half the
/// base's em), and it is what browsers use for `<ruby>` by default.
const RUBY_FONT_FRACTION: f32 = 0.5;

/// One laid-out ruby annotation, waiting to be drawn over (or under) the base
/// text it belongs to.
///
/// A layout of its own rather than part of the cue's: an annotation is not in
/// the reading order and is positioned against its base's ADVANCE, not flowed
/// beside it. What ties the two together is `range`, the base span's byte
/// range in the cue's flattened text, which pass 2 resolves into the base's
/// place on screen.
struct RubyRun {
    range: std::ops::Range<usize>,
    position: ir::RubyPosition,
    layout: parley::Layout<CueBrush>,
    /// Alignment inset of the widest line inside its own wrap box; drawing
    /// subtracts it so the ink starts at the annotation's origin.
    origin_x: f32,
    width: f32,
    height: f32,
    /// The base text's own line height, which the leading is computed from.
    base_line_height: f32,
    /// Where the annotation's ink goes, in cue-layout coordinates (pass 2).
    x: f32,
    y: f32,
}

/// Where a base span ended up: the horizontal extent of its glyphs and the
/// vertical metrics of the line they landed on.
struct BaseExtent {
    x0: f32,
    x1: f32,
    baseline: f32,
    ascent: f32,
    descent: f32,
}

/// Find the base text of a ruby annotation in the laid-out cue.
///
/// Runs are split at style boundaries and every ruby base carries a style the
/// text around it does not (its own line height, at least), so the base's
/// glyphs are whole runs rather than parts of one. A base that broke across
/// lines despite the no-break spaces is annotated on the FIRST line it appears
/// on. Half an annotation in the right place beats a whole one in the wrong
/// place.
fn base_extent(
    layout: &parley::Layout<CueBrush>,
    range: &std::ops::Range<usize>,
) -> Option<BaseExtent> {
    for line in layout.lines() {
        let m = line.metrics();
        let (mut x0, mut x1) = (f32::MAX, f32::MIN);
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run = glyph_run.run().text_range();
            if run.start >= range.end || run.end <= range.start {
                continue;
            }
            x0 = x0.min(glyph_run.offset());
            x1 = x1.max(glyph_run.offset() + glyph_run.advance());
        }
        if x1 > x0 {
            return Some(BaseExtent {
                x0,
                x1,
                baseline: m.baseline,
                ascent: m.ascent,
                descent: m.descent,
            });
        }
    }
    None
}

/// The worker's layout/render state. Both contexts cache aggressively, so one
/// long-lived instance per worker thread.
pub struct RasterCtx {
    font_cx: FontContext,
    layout_cx: LayoutContext<CueBrush>,
    /// The render surface, reused across rasters of the same size (karaoke
    /// steps in particular): keeps vello's glyph outline/hinting cache warm
    /// (`reset()` retains it) and avoids two large allocations per raster.
    /// `render_to_pixmap` clears before writing, so reuse is safe.
    surface: Option<((u16, u16), RenderContext, Pixmap)>,
}

impl Default for RasterCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl RasterCtx {
    pub fn new() -> Self {
        Self {
            font_cx: FontContext::new(),
            layout_cx: LayoutContext::new(),
            surface: None,
        }
    }

    /// The reusable `(RenderContext, Pixmap)` for a `dims`-sized raster.
    fn surface(&mut self, dims: (u16, u16)) -> (&mut RenderContext, &mut Pixmap) {
        let reusable = matches!(&self.surface, Some((have, _, _)) if *have == dims);
        if !reusable {
            let (w, h) = dims;
            self.surface = Some((dims, RenderContext::new(w, h), Pixmap::new(w, h)));
        }
        let (_, rc, pixmap) = self.surface.as_mut().expect("just ensured");
        if reusable {
            rc.reset();
        }
        (rc, pixmap)
    }

    /// Lay one ruby annotation out on its own, returning it with its ink size.
    ///
    /// Its own layout rather than part of the cue's: an annotation is not in
    /// the reading order, is half the size of the text it belongs to, and is
    /// positioned against that text's advance rather than flowed. `None` when
    /// it lays out to nothing (empty, whitespace, a font that produced no ink),
    /// which is a normal outcome for hostile input and not an error.
    ///
    /// It is broken to the same wrap width as the cue, so a pathological
    /// annotation wraps instead of widening the raster without bound.
    fn annotation_layout(
        &mut self,
        text: &str,
        px: f32,
        family: Option<&str>,
        weight: f32,
        brush: CueBrush,
        wrap: f32,
    ) -> Option<(parley::Layout<CueBrush>, f32, f32, f32)> {
        let mut b = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, text, 1.0, true);
        b.push_default(StyleProperty::Brush(brush));
        b.push_default(GenericFamily::SansSerif);
        b.push_default(LineHeight::FontSizeRelative(LINE_HEIGHT));
        b.push_default(StyleProperty::FontSize(px));
        b.push_default(FontWeight::new(weight));
        if let Some(family) = family {
            b.push_default(FontFamilyName::Named(family.into()));
        }
        let mut layout = b.build(text);
        layout.break_all_lines(Some(wrap));
        layout.align(Alignment::Center, AlignmentOptions::default());

        let (mut x0, mut x1) = (f32::MAX, 0.0f32);
        for line in layout.lines() {
            let m = line.metrics();
            x0 = x0.min(m.offset);
            x1 = x1.max(m.offset + m.advance - m.trailing_whitespace);
        }
        let height = layout.height();
        if !(x1 > x0 && height > 0.0 && x1 - x0 <= MAX_RASTER_PX as f32) {
            return None;
        }
        // `x0` is the alignment inset of the widest line inside the wrap box.
        // Drawing subtracts it, which puts the annotation's ink at its own
        // origin while narrower lines keep their centring relative to it.
        Some((layout, x0, x1 - x0, height))
    }

    /// Force the font stack to actually load: a throwaway cue, laid out and
    /// rasterized exactly like a real one.
    ///
    /// Parley's font collection is much cheaper to build than a fontconfig
    /// fontmap, but "much cheaper" is still not "free": it must happen on the
    /// dedicated raster thread, never on a streaming or event-loop one.
    pub fn warm(&mut self) {
        let ir = CueIr::from_plain_text("Warming the font stack");
        if self
            .render(&ir, &CueStyle::default(), (640, 360), None, 0)
            .is_none()
        {
            warn!("cue-ir raster warm-up produced no pixels");
        }
    }

    /// Rasterize `ir` at reveal `step` (0 = only the un-timed spans are
    /// visible; `usize::MAX` = everything).
    pub fn render(
        &mut self,
        ir: &CueIr,
        house: &CueStyle,
        canvas: (u32, u32),
        video_rect: Option<VideoRect>,
        step: usize,
    ) -> Option<RasterOut> {
        let (canvas_w, canvas_h) = canvas;
        if canvas_w == 0 || canvas_h == 0 {
            return None;
        }
        let (cw, ch) = (canvas_w as f32, canvas_h as f32);
        // The *frame*: where the picture sits in the window. Everything the
        // subtitle file expresses (positions, margins, SSA font sizes) is
        // relative to it; without a known rect the window doubles as it.
        let frame = match video_rect {
            Some(r) => Frame {
                x: r.x as f32,
                y: r.y as f32,
                w: r.width as f32,
                h: r.height as f32,
            },
            None => Frame {
                x: 0.0,
                y: 0.0,
                w: cw,
                h: ch,
            },
        };

        // Text scales with the picture, not the window: a pillarboxed video
        // should not get subtitles sized for the full screen.
        let base_px = (frame.h * house.font_height_fraction).max(house.min_font_px);
        // Cue box width from the IR's `size` (WebVTT), else the house wrap.
        let size_pct = ir
            .layout
            .size
            .unwrap_or(house.wrap_width_fraction * 100.0)
            .clamp(1.0, 100.0);
        let wrap_px = (frame.w * size_pct / 100.0).max(1.0);

        // Which reveal ranks are visible at this step (see `reveal_rank`).
        let mut step_ns: Vec<u64> = ir
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.reveal_ns)
            .collect();
        step_ns.sort_unstable();
        step_ns.dedup();

        // Flatten the cue into one string plus per-span byte ranges, the shape
        // parley's ranged builder styles with.
        //
        // A span carrying a RUBY annotation goes in with its spaces replaced by
        // no-break spaces: the annotation is drawn over the base's advance, so
        // a line break inside the base would leave the annotation floating over
        // half of it. (`\u{a0}` stops the break at spaces, which is where a
        // Latin base breaks. A base long enough to break BETWEEN ideographs can
        // still do so (parley has no "do not break inside this range") and
        // the drawing below handles it by annotating the first line's part and
        // saying so in a debug line. Ruby bases are one to four characters in
        // practice.)
        let mut text = String::new();
        let mut spans: Vec<(std::ops::Range<usize>, &ir::Span)> = Vec::new();
        for (i, line) in ir.lines.iter().enumerate() {
            if i != 0 {
                text.push('\n');
            }
            for span in &line.spans {
                let start = text.len();
                let clean = sanitize_control_chars(&span.text);
                if span.ruby.is_some() {
                    text.push_str(&clean.replace(' ', "\u{a0}"));
                } else {
                    text.push_str(&clean);
                }
                spans.push((start..text.len(), span));
            }
        }
        if text.trim().is_empty() {
            debug!("cue laid out to nothing");
            return None;
        }

        let base = &ir.base;
        let house_outline = house
            .outline
            .map(|o| (rgba(o.color), (base_px * o.width_fraction).max(1.0)))
            .unwrap_or((Color::TRANSPARENT, 0.0));
        let base_brush = CueBrush {
            fg: base.foreground.map(color).unwrap_or(Color::WHITE),
            bg: base.background.map(color),
            outline: base
                .outline
                .map(|o| (color(o.color), pt_to_px(o.width).max(1.0)))
                .unwrap_or(house_outline),
            shadow: base
                .shadow
                .map(|s| (color(s.color), pt_to_px(s.dx), pt_to_px(s.dy))),
            revealed: true,
        };

        // The brush a span paints with: its own colours where it sets them, the
        // cue's underneath. Shared with the ruby pre-pass below, so an
        // annotation is drawn in the same ink as the text it annotates.
        let brush_for = |s: &ir::SpanStyle, revealed: bool| CueBrush {
            fg: s.foreground.map(color).unwrap_or(base_brush.fg),
            bg: s.background.map(color).or(base_brush.bg),
            outline: s
                .outline
                .map(|o| (color(o.color), pt_to_px(o.width).max(1.0)))
                .unwrap_or(base_brush.outline),
            shadow: s
                .shadow
                .map(|sh| (color(sh.color), pt_to_px(sh.dx), pt_to_px(sh.dy)))
                .or(base_brush.shadow),
            revealed,
        };

        // ---- RUBY, pass 1 of 2: lay the annotations out BEFORE the cue.
        //
        // Their height is what decides how much leading the lines carrying them
        // need, and leading is a style that has to be pushed before the cue is
        // laid out, so the annotation layout has to exist first. It also has to
        // happen before the ranged builder borrows this context.
        let mut rubies: Vec<RubyRun> = Vec::new();
        for (range, span) in &spans {
            let Some(ruby) = span.ruby.as_ref() else {
                continue;
            };
            if ruby.text.trim().is_empty() {
                continue;
            }
            let base_font = font_px(span.style.font_size, base_px, frame.h);
            let revealed = reveal_rank(&step_ns, span.reveal_ns) <= step;
            let brush = brush_for(&span.style, revealed);
            let family = span
                .style
                .font_family
                .as_deref()
                .or(base.font_family.as_deref())
                .or(house.font_family.as_deref());
            let weight = span
                .style
                .font_weight
                .map(|w| w as f32)
                .unwrap_or_else(|| base.font_weight.map_or(house.font_weight, f32::from));
            let Some((layout, origin_x, width, height)) = self.annotation_layout(
                &sanitize_control_chars(&ruby.text),
                (base_font * RUBY_FONT_FRACTION).max(1.0),
                family,
                weight,
                brush,
                wrap_px,
            ) else {
                continue;
            };
            rubies.push(RubyRun {
                range: range.clone(),
                position: ruby.position,
                layout,
                origin_x,
                width,
                height,
                base_line_height: base_font * LINE_HEIGHT,
                // Filled in by pass 2, once the cue has been laid out.
                x: 0.0,
                y: 0.0,
            });
        }

        let mut b = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, &text, 1.0, true);
        b.push_default(StyleProperty::Brush(base_brush.clone()));
        b.push_default(GenericFamily::SansSerif);
        b.push_default(LineHeight::FontSizeRelative(LINE_HEIGHT));
        b.push_default(StyleProperty::FontSize(font_px(
            base.font_size,
            base_px,
            frame.h,
        )));
        match (base.font_family.as_deref(), house.font_family.as_deref()) {
            (Some(family), _) | (None, Some(family)) => {
                b.push_default(FontFamilyName::Named(family.into()));
            }
            (None, None) => {}
        }
        if let Some(style) = base.font_style {
            b.push_default(font_style(style));
        }
        // The house weight (bold by default); the IR (styles, <b>, CSS)
        // overrides it.
        b.push_default(FontWeight::new(
            base.font_weight.map_or(house.font_weight, f32::from),
        ));
        if base.underline == Some(true) {
            b.push_default(StyleProperty::Underline(true));
        }
        if base.strikethrough == Some(true) {
            b.push_default(StyleProperty::Strikethrough(true));
        }

        for (range, span) in &spans {
            let s = &span.style;
            let revealed = reveal_rank(&step_ns, span.reveal_ns) <= step;
            if !revealed
                || s.foreground.is_some()
                || s.background.is_some()
                || s.outline.is_some()
                || s.shadow.is_some()
            {
                b.push(StyleProperty::Brush(brush_for(s, revealed)), range.clone());
            }
            if let Some(style) = s.font_style {
                b.push(font_style(style), range.clone());
            }
            if let Some(weight) = s.font_weight {
                b.push(FontWeight::new(weight as f32), range.clone());
            }
            if let Some(underline) = s.underline {
                b.push(StyleProperty::Underline(underline), range.clone());
            }
            if let Some(strikethrough) = s.strikethrough {
                b.push(StyleProperty::Strikethrough(strikethrough), range.clone());
            }
            if s.font_size.is_some() {
                b.push(
                    StyleProperty::FontSize(font_px(s.font_size, base_px, frame.h)),
                    range.clone(),
                );
            }
            if let Some(family) = s.font_family.as_deref() {
                b.push(FontFamilyName::Named(family.into()), range.clone());
            }
            if let Some(spacing) = s.letter_spacing {
                b.push(
                    StyleProperty::LetterSpacing(pt_to_px(spacing)),
                    range.clone(),
                );
            }
        }

        // ---- RUBY, the leading. A line carrying an annotation is given room
        // for it, by making the annotated run taller: parley splits a line's
        // leading evenly above and below the text, so `2 x height` of extra
        // buys `height` of clear space on the side the annotation is drawn.
        // The room belongs to the LINE, which is what keeps an annotation off
        // the line above it (and off the readability box, which is sized from
        // the layout).
        for ruby in &rubies {
            b.push(
                LineHeight::Absolute(ruby.base_line_height + 2.0 * ruby.height),
                ruby.range.clone(),
            );
        }

        let mut playout = b.build(&text);
        playout.break_all_lines(Some(wrap_px));
        playout.align(alignment(ir.layout.align), AlignmentOptions::default());

        // ---- RUBY, pass 2 of 2: place each annotation over (or under) the
        // advance of the base text now that the cue has been broken and
        // aligned. An annotation whose base did not make it into the layout at
        // all is dropped rather than drawn somewhere arbitrary.
        rubies.retain_mut(|ruby| match base_extent(&playout, &ruby.range) {
            Some(base) => {
                ruby.x = base.x0 + (base.x1 - base.x0 - ruby.width) / 2.0;
                ruby.y = match ruby.position {
                    ir::RubyPosition::Over => base.baseline - base.ascent - ruby.height,
                    ir::RubyPosition::Under => base.baseline + base.descent,
                };
                true
            }
            None => {
                debug!("a ruby annotation's base text is not in the layout; dropping it");
                false
            }
        });

        // Tight ink extents: alignment offsets lines within `wrap_px`, so the
        // raster hugs the widest line rather than the whole wrap box.
        let lh = playout.height();
        let (mut ink_x0, mut ink_x1) = (f32::MAX, 0.0f32);
        for line in playout.lines() {
            let m = line.metrics();
            ink_x0 = ink_x0.min(m.offset);
            ink_x1 = ink_x1.max(m.offset + m.advance - m.trailing_whitespace);
        }
        if ink_x0 >= ink_x1 {
            debug!("cue laid out to nothing");
            return None;
        }
        // An annotation wider than the text it annotates is part of the cue:
        // the surface has to hold it and the readability box has to cover it.
        // Vertically it lives inside the leading bought above, so `lh` already
        // includes it -- except when a font's own metrics leave the leading
        // short, which `ink_y0`/`ink_y1` absorb rather than clip.
        let (mut ink_y0, mut ink_y1) = (0.0f32, lh);
        for ruby in &rubies {
            ink_x0 = ink_x0.min(ruby.x);
            ink_x1 = ink_x1.max(ruby.x + ruby.width);
            ink_y0 = ink_y0.min(ruby.y);
            ink_y1 = ink_y1.max(ruby.y + ruby.height);
        }

        // Padding must cover whatever paints outside the glyph boxes: the
        // widest outline and the farthest shadow reach.
        let mut reach = base_brush.outline.1;
        let mut consider = |brush: &CueBrush| {
            reach = reach.max(brush.outline.1);
            if let Some((_, dx, dy)) = brush.shadow {
                reach = reach.max(dx.abs()).max(dy.abs());
            }
        };
        consider(&base_brush);
        for line in playout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(run) = item {
                    consider(&run.style().brush);
                }
            }
        }
        // The box geometry (from the house style; the subtitle's own cue
        // background reuses it, square and snug when there is none).
        let box_pad = house.background.map_or(reach, |b| b.padding * base_px);
        let box_radius = house.background.map_or(0.0, |b| b.corner_radius * base_px);
        let box_soft = house.background.map_or(0.0, |b| b.edge_softness * base_px);
        // Padding must cover whatever paints outside the ink: outline and
        // shadow reach, the box padding, and the box's feathered rim
        // (~3 standard deviations to fade out).
        let pad = reach.max(box_pad + 3.0 * box_soft).ceil() + 1.0;

        // Everything above came from attacker-controlled f32s (IR sizes,
        // spacing, shadow offsets), so validate in float space, where nothing
        // has wrapped yet, before any integer cast. `as i32` on an overflowed
        // value used to wrap past the size check and reach the `as u16` casts
        // below as garbage (multi-GB pixmap allocations), or panic in debug
        // builds. NaN fails these comparisons and is rejected too. Rejection
        // must be a `Failed` cue, never a panic.
        let surface_w = (ink_x1 - ink_x0).ceil() + 2.0 * pad;
        let surface_h = (ink_y1 - ink_y0).ceil() + 2.0 * pad;
        if !(surface_w >= 1.0
            && surface_h >= 1.0
            && surface_w <= MAX_RASTER_PX as f32
            && surface_h <= MAX_RASTER_PX as f32)
        {
            warn!(surface_w, surface_h, "cue raster size unusable, skipping");
            return None;
        }
        let surface_w = surface_w as i32;
        let surface_h = surface_h as i32;

        let (rc, pixmap) = self.surface((surface_w as u16, surface_h as u16));
        rc.set_transform(Affine::translate(Vec2::new(
            (pad - ink_x0) as f64,
            (pad - ink_y0) as f64,
        )));

        // Paint order, whole layout at a time so nothing overdraws a
        // neighbouring run: cue box, span boxes, shadows, outlines, fills (with
        // decorations).
        //
        // The subtitle's own cue background (SSA BorderStyle=3, WebVTT
        // ::cue { background }) wins over the house box color; the house
        // geometry applies either way.
        let box_color = ir
            .layout
            .background
            .map(color)
            .or_else(|| house.background.map(|b| rgba(b.color)));
        if let Some(bg) = box_color {
            rc.set_paint(bg);
            let rect = Rect::new(
                (ink_x0 - box_pad) as f64,
                (ink_y0 - box_pad) as f64,
                (ink_x1 + box_pad) as f64,
                (ink_y1 + box_pad) as f64,
            );
            if box_soft > 0.0 {
                rc.fill_blurred_rounded_rect(&rect, box_radius, box_soft);
            } else if box_radius > 0.0 {
                use vello_cpu::kurbo::{RoundedRect, Shape};
                rc.fill_path(&RoundedRect::from_rect(rect, box_radius as f64).to_path(0.1));
            } else {
                rc.fill_rect(&rect);
            }
        }
        for line in playout.lines() {
            let m = line.metrics();
            let (top, bottom) = (
                (m.baseline - m.ascent) as f64,
                (m.baseline + m.descent) as f64,
            );
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let brush = glyph_run.style().brush.clone();
                if !brush.revealed {
                    continue;
                }
                if let Some(bg) = brush.bg {
                    rc.set_paint(bg);
                    rc.fill_rect(&Rect::new(
                        glyph_run.offset() as f64,
                        top,
                        (glyph_run.offset() + glyph_run.advance()) as f64,
                        bottom,
                    ));
                }
            }
        }
        for_each_revealed_run(&playout, |glyph_run| {
            if let Some((shadow, dx, dy)) = glyph_run.style().brush.shadow {
                rc.set_paint(shadow);
                draw_glyphs(rc, glyph_run, Pass::Fill, Vec2::new(dx as f64, dy as f64));
            }
        });
        for_each_revealed_run(&playout, |glyph_run| {
            let (outline, width) = glyph_run.style().brush.outline;
            if width > 0.0 {
                rc.set_paint(outline);
                rc.set_stroke(
                    Stroke::new(width as f64)
                        .with_join(Join::Round)
                        .with_caps(Cap::Round),
                );
                draw_glyphs(rc, glyph_run, Pass::Stroke, Vec2::ZERO);
            }
        });
        for_each_revealed_run(&playout, |glyph_run| {
            let brush = glyph_run.style().brush.clone();
            rc.set_paint(brush.fg);
            draw_glyphs(rc, glyph_run, Pass::Fill, Vec2::ZERO);

            let run = glyph_run.run();
            let style = glyph_run.style();
            if let Some(decoration) = &style.underline {
                let offset = decoration.offset.unwrap_or(run.metrics().underline_offset);
                let size = decoration.size.unwrap_or(run.metrics().underline_size);
                draw_decoration(rc, glyph_run, brush.fg, offset, size);
            }
            if let Some(decoration) = &style.strikethrough {
                let offset = decoration
                    .offset
                    .unwrap_or(run.metrics().strikethrough_offset);
                let size = decoration.size.unwrap_or(run.metrics().strikethrough_size);
                draw_decoration(rc, glyph_run, brush.fg, offset, size);
            }
        });

        // ---- RUBY, drawn last and in its own coordinates: shadow, outline,
        // fill, the same three passes the cue's own text gets, offset to where
        // pass 2 put the annotation. Visibility follows the BASE span (the
        // brush was built from it), so a karaoke syllable and its furigana
        // appear together.
        for ruby in &rubies {
            let offset = Vec2::new((ruby.x - ruby.origin_x) as f64, ruby.y as f64);
            for_each_revealed_run(&ruby.layout, |glyph_run| {
                if let Some((shadow, dx, dy)) = glyph_run.style().brush.shadow {
                    rc.set_paint(shadow);
                    draw_glyphs(
                        rc,
                        glyph_run,
                        Pass::Fill,
                        offset + Vec2::new(dx as f64, dy as f64),
                    );
                }
            });
            for_each_revealed_run(&ruby.layout, |glyph_run| {
                let (outline, width) = glyph_run.style().brush.outline;
                if width > 0.0 {
                    rc.set_paint(outline);
                    rc.set_stroke(
                        Stroke::new(width as f64)
                            .with_join(Join::Round)
                            .with_caps(Cap::Round),
                    );
                    draw_glyphs(rc, glyph_run, Pass::Stroke, offset);
                }
            });
            for_each_revealed_run(&ruby.layout, |glyph_run| {
                rc.set_paint(glyph_run.style().brush.fg);
                draw_glyphs(rc, glyph_run, Pass::Fill, offset);
            });
        }

        rc.flush();
        rc.render_to_pixmap(pixmap);
        let pixels = premul_to_straight_rgba(pixmap.data_as_u8_slice());

        let (x, y) = place(
            ir,
            house,
            (cw, ch),
            frame,
            surface_w as f32,
            surface_h as f32,
        );
        Some(RasterOut {
            pixels,
            width: surface_w as u32,
            height: surface_h as u32,
            x,
            y,
        })
    }
}

/// Which vello pass `draw_glyphs` runs.
enum Pass {
    Fill,
    Stroke,
}

/// One glyph run through vello's glyph pipeline, optionally offset (shadows).
fn draw_glyphs(
    rc: &mut RenderContext,
    glyph_run: &GlyphRun<'_, CueBrush>,
    pass: Pass,
    offset: Vec2,
) {
    let run = glyph_run.run();
    let builder = rc
        .glyph_run(run.font())
        .font_size(run.font_size())
        .hint(true)
        .normalized_coords(run.normalized_coords());
    let glyphs = glyph_run.positioned_glyphs().map(|g| Glyph {
        id: g.id,
        x: g.x + offset.x as f32,
        y: g.y + offset.y as f32,
    });
    match pass {
        Pass::Fill => builder.fill_glyphs(glyphs),
        Pass::Stroke => builder.stroke_glyphs(glyphs),
    }
}

/// Iterate the revealed glyph runs of the whole layout, in order.
fn for_each_revealed_run<'a>(
    layout: &'a parley::Layout<CueBrush>,
    mut f: impl FnMut(&GlyphRun<'a, CueBrush>),
) {
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            if glyph_run.style().brush.revealed {
                f(&glyph_run);
            }
        }
    }
}

/// A decoration (underline/strikethrough) is a filled rectangle across the
/// run's advance.
fn draw_decoration(
    rc: &mut RenderContext,
    glyph_run: &GlyphRun<'_, CueBrush>,
    color: Color,
    offset: f32,
    size: f32,
) {
    rc.set_paint(color);
    let y = (glyph_run.baseline() - offset) as f64;
    let x = glyph_run.offset() as f64;
    rc.fill_rect(&Rect::new(
        x,
        y,
        x + glyph_run.advance() as f64,
        y + size as f64,
    ));
}

/// The rectangle the file's coordinates are relative to (the video rect, or the
/// whole window when it is unknown), in window coordinates.
#[derive(Debug, Clone, Copy)]
struct Frame {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Place the finished raster, in window coordinates.
///
/// Everything the subtitle file expresses is resolved inside the *frame* (the
/// picture): an explicit SSA origin (`\pos`) pins the anchor point exactly,
/// then WebVTT `position`/`line` percentages, then the anchor's own frame
/// region with the IR margins. A cue the file says nothing about is house
/// policy: bottom-center of the frame, or of the *window* when
/// [`CueStyle::use_window_margins`] is set, letting default subtitles sit in
/// the letterbox bars instead of covering the picture (the margin itself stays
/// proportional to the picture so the look does not change with the bars).
fn place(
    ir: &CueIr,
    house: &CueStyle,
    (cw, ch): (f32, f32),
    frame: Frame,
    w: f32,
    h: f32,
) -> (i32, i32) {
    let anchor = ir.layout.anchor.unwrap_or(ir::Anchor::BottomCenter);
    let (col, row) = anchor_cell(anchor);
    let l = &ir.layout;
    // "The file said nothing": no explicit placement of any kind. Note the
    // anchor alone (SSA alignment, {\an8}) keeps frame placement: a
    // top-anchored cue belongs over the picture's top, not the window's.
    let positioned = l.origin.is_some()
        || l.position.is_some()
        || l.line.is_some()
        || l.margins.is_some()
        || l.anchor.is_some();
    let window_margins = house.use_window_margins && !positioned;

    let x = if let Some((ox, _)) = l.origin {
        frame.x + frame.w * ox / 100.0 - w * col
    } else if let Some(p) = l.position {
        // WebVTT position alignment: which edge of the cue box sits at the
        // position. `Auto` (or unset) follows the text alignment, centered by
        // default.
        let at = match l.position_align {
            Some(ir::PositionAlign::LineLeft) => 0.0,
            Some(ir::PositionAlign::Center) => 0.5,
            Some(ir::PositionAlign::LineRight) => 1.0,
            Some(ir::PositionAlign::Auto) | None => match l.align {
                Some(ir::TextAlign::Left | ir::TextAlign::Start) => 0.0,
                Some(ir::TextAlign::Right | ir::TextAlign::End) => 1.0,
                _ => 0.5,
            },
        };
        frame.x + frame.w * p / 100.0 - w * at
    } else {
        // Centered on the picture (which is centered in the window anyway).
        frame.x + (frame.w - w) / 2.0
    };
    let y = if let Some((_, oy)) = l.origin {
        frame.y + frame.h * oy / 100.0 - h * row
    } else if let Some(pos) = l.line {
        let base = match pos {
            ir::LinePosition::Percent(p) => frame.y + frame.h * p / 100.0,
            // Snap-to-lines: the step is the cue's own line height.
            // Non-negative counts from the frame's top edge; negative from the
            // bottom, with the cue kept inside (`line:-1` = the last line, i.e.
            // bottom-aligned).
            ir::LinePosition::Line(n) => {
                let line_h = h / ir.lines.len().max(1) as f32;
                if n >= 0 {
                    frame.y + line_h * n as f32
                } else {
                    frame.y + frame.h + line_h * (n + 1) as f32 - h
                }
            }
        };
        match l.line_align {
            Some(ir::LineAlign::Center) => base - h / 2.0,
            Some(ir::LineAlign::End) => base - h,
            _ => base,
        }
    } else {
        // Margin proportional to the picture, applied to the frame or, for
        // unpositioned cues under the window-margins policy, to the window,
        // whose bottom bar it may then use.
        let mv = l
            .margins
            .map(|m| m.vertical)
            .filter(|v| *v > 0.0)
            .map(|v| v / 100.0)
            .unwrap_or(house.bottom_margin_fraction)
            * frame.h;
        let (top, bottom) = if window_margins {
            (mv, ch - mv - h)
        } else {
            (frame.y + mv, frame.y + frame.h - mv - h)
        };
        match anchor_row(anchor) {
            AnchorRow::Top => top,
            AnchorRow::Center => frame.y + (frame.h - h) / 2.0,
            AnchorRow::Bottom => bottom,
        }
    };

    // Whatever was asked for, the raster must land inside the window.
    (
        (x.clamp(0.0, (cw - w).max(0.0))) as i32,
        (y.clamp(0.0, (ch - h).max(0.0))) as i32,
    )
}

enum AnchorRow {
    Top,
    Center,
    Bottom,
}

fn anchor_row(a: ir::Anchor) -> AnchorRow {
    use ir::Anchor::*;
    match a {
        TopLeft | TopCenter | TopRight => AnchorRow::Top,
        CenterLeft | Center | CenterRight => AnchorRow::Center,
        BottomLeft | BottomCenter | BottomRight => AnchorRow::Bottom,
    }
}

/// The anchor's fractional cell: `(column, row)` with `0.0` = left/top, `0.5` =
/// center, `1.0` = right/bottom, the fraction of the cue box that sits before
/// the anchor point.
fn anchor_cell(a: ir::Anchor) -> (f32, f32) {
    use ir::Anchor::*;
    match a {
        TopLeft => (0.0, 0.0),
        TopCenter => (0.5, 0.0),
        TopRight => (1.0, 0.0),
        CenterLeft => (0.0, 0.5),
        Center => (0.5, 0.5),
        CenterRight => (1.0, 0.5),
        BottomLeft => (0.0, 1.0),
        BottomCenter => (0.5, 1.0),
        BottomRight => (1.0, 1.0),
    }
}

fn color(c: ir::Color) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn rgba(c: Rgba) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

fn pt_to_px(pt: f32) -> f32 {
    pt * 96.0 / 72.0
}

/// IR font size → pixels: absolute points via the CSS factor, scales against
/// the house base size, frame-height percents (SSA) against the canvas. IR
/// values are attacker-controlled f32s (a CSS `font-size: NaN%` parses), so the
/// result is clamped to something a raster can hold, and anything non-finite
/// falls back to the base size.
fn font_px(size: Option<ir::FontSize>, base_px: f32, canvas_h: f32) -> f32 {
    let px = match size {
        Some(ir::FontSize::Points(pt)) => pt_to_px(pt),
        Some(ir::FontSize::Scale(s)) => base_px * s,
        Some(ir::FontSize::FrameHeightPercent(p)) => canvas_h * p / 100.0,
        None => base_px,
    };
    if px.is_finite() {
        px.clamp(1.0, MAX_RASTER_PX as f32)
    } else {
        base_px
    }
}

fn font_style(style: ir::FontStyle) -> parley::FontStyle {
    match style {
        ir::FontStyle::Normal => parley::FontStyle::Normal,
        ir::FontStyle::Italic => parley::FontStyle::Italic,
        ir::FontStyle::Oblique => parley::FontStyle::Oblique(None),
    }
}

fn alignment(align: Option<ir::TextAlign>) -> Alignment {
    match align {
        // Subtitles center by default.
        None | Some(ir::TextAlign::Center) => Alignment::Center,
        Some(ir::TextAlign::Start) => Alignment::Start,
        Some(ir::TextAlign::End) => Alignment::End,
        Some(ir::TextAlign::Left) => Alignment::Left,
        Some(ir::TextAlign::Right) => Alignment::Right,
    }
}

/// `⌈255/a⌉` in 16.16 fixed point, per alpha value: `(v * RECIP[a]) >> 16`
/// rounds to within 1 LSB of `v * 255 / a` without a per-pixel division.
static UNPREMUL_RECIP: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut a = 1usize;
    while a < 256 {
        table[a] = ((255u32 << 16) + (a as u32) / 2) / (a as u32);
        a += 1;
    }
    table
};

/// vello_cpu's pixmap is premultiplied RGBA; overlays are tightly packed
/// straight-alpha RGBA (`Overlay::pixels`, uploaded as `PL_ALPHA_INDEPENDENT`).
///
/// This runs over every pixel of every raster (~20% of a raster's cost before
/// it was tuned), so the two dominant alpha populations are fast-pathed (fully
/// transparent padding and fully opaque glyph interiors) and the remainder
/// uses the reciprocal table instead of three integer divisions per pixel.
fn premul_to_straight_rgba(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.as_chunks::<4>().0 {
        let alpha = px[3];
        match alpha {
            0 => out.extend_from_slice(&[0, 0, 0, 0]),
            255 => out.extend_from_slice(px),
            _ => {
                let recip = UNPREMUL_RECIP[alpha as usize];
                let unpremultiply =
                    |value: u8| -> u8 { ((value as u32 * recip + (1 << 15)) >> 16).min(255) as u8 };
                out.push(unpremultiply(px[0]));
                out.push(unpremultiply(px[1]));
                out.push(unpremultiply(px[2]));
                out.push(alpha);
            }
        }
    }
    out
}

/// The readable words of pango markup, with the markup removed.
///
/// This is the PANGO ARM's rescue path, not a cue-IR one: when
/// `pango::parse_markup` refuses a cue, the alternative to this is showing the
/// viewer the raw source, tags and all. Which is not hypothetical: WebVTT's
/// `<v Speaker>` is the case that motivated it. `subparse-formats` keeps voice
/// spans in its pango-markup output on purpose, because that output is
/// byte-identical to the C `subparse`, whose tag whitelist has the same wart;
/// pango then rejects `<v Voice1>` ("expected a `=` after attribute name") and
/// every such cue reached the screen as literal angle brackets.
///
/// It parses rather than strips, using `subparse-formats`' own lenient markup
/// parser (no pango involved) and keeping ONLY the text. Parsing matters
/// because a strip cannot tell a tag from a less-than: `I <3 you` survives here
/// and would lose three characters to any `<...>` regex. Entities are decoded
/// too, so a cue does not trade literal `<v Voice1>` for literal `&amp;`.
///
/// STYLING IS DELIBERATELY DISCARDED. The markup was rejected, so nothing about
/// it is trustworthy enough to interpret; the contract is only that the viewer
/// reads the words. A stream that wants its styling honoured should be parsed
/// with `text-format=cue-ir`, which is the whole point of the other arm.
pub fn plain_text_of_markup(markup: &str) -> String {
    CueIr::from_pango_markup(markup).plain_text()
}

/// Map C0 controls (except `\n`) and DEL to a space: fonts carry no glyph for
/// them, so parley shapes `.notdef` and vello draws a box (field: a literal
/// tab). One byte for one byte, so span ranges stay valid.
fn sanitize_control_chars(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().any(|b| (b < 0x20 && b != b'\n') || b == 0x7f) {
        std::borrow::Cow::Owned(
            s.chars()
                .map(|c| {
                    if (c < '\u{20}' && c != '\n') || c == '\u{7f}' {
                        ' '
                    } else {
                        c
                    }
                })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Structural identity of two IRs, with an `Arc` pointer fast path: a re-shown
/// cue is usually the same allocation. Same IR ⇒ same pixels, which is what
/// makes the raster cache sound. (`CueIr` holds `f32`s and so has no lawful
/// `Eq`/`Hash`; the cache is a bounded LRU `Vec` with a linear scan, so
/// `PartialEq` is all a lookup takes.)
pub fn ir_eq(a: &Arc<CueIr>, b: &Arc<CueIr>) -> bool {
    Arc::ptr_eq(a, b) || a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ruby annotations ----

    /// A cue with `text` as its only span, optionally annotated.
    fn ruby_cue(text: &str, anno: Option<(&str, ir::RubyPosition)>) -> CueIr {
        let mut cue = CueIr::from_plain_text(text);
        if let Some((anno, position)) = anno {
            cue.lines[0].spans[0].ruby = Some(ir::Ruby {
                text: anno.to_owned(),
                position,
            });
        }
        cue
    }

    /// Straight-alpha RGBA rows that carry any ink at all.
    fn ink_rows(out: &RasterOut) -> Vec<usize> {
        (0..out.height as usize)
            .filter(|row| {
                let start = row * out.width as usize * 4;
                out.pixels[start..start + out.width as usize * 4]
                    .iter()
                    .skip(3)
                    .step_by(4)
                    .any(|alpha| *alpha > 40)
            })
            .collect()
    }

    fn ink_pixels(out: &RasterOut) -> usize {
        out.pixels
            .iter()
            .skip(3)
            .step_by(4)
            .filter(|a| **a > 40)
            .count()
    }

    /// The annotation is drawn ABOVE the text it annotates for `Over` and BELOW
    /// it for `Under` -- measured in pixels, against the same cue without one.
    ///
    /// "Above" is stated as a distance rather than a band: with an annotation,
    /// ink starts FARTHER FROM THE BOTTOM of the raster than the bare cue's
    /// does, and with `Under` it ends farther from the top. That holds whatever
    /// the font's metrics are, which a fixed band would not.
    #[test]
    fn control_chars_become_spaces_before_layout_so_no_notdef_box_renders() {
        use std::borrow::Cow;
        assert_eq!(sanitize_control_chars("a\tb"), "a b");
        assert_eq!(sanitize_control_chars("a\u{7}b\r"), "a b ");
        assert_eq!(sanitize_control_chars("a\nb"), "a\nb");
        // One byte for one byte: span ranges computed over the sanitized text
        // stay valid against the original lengths.
        assert_eq!(
            sanitize_control_chars("a\tb\u{7f}").len(),
            "a\tb\u{7f}".len()
        );
        // Clean text borrows.
        assert!(matches!(sanitize_control_chars("clean"), Cow::Borrowed(_)));
        // And the full path rasterizes a tab-bearing cue without panicking.
        let mut ctx = RasterCtx::new();
        let ir = CueIr::from_plain_text("left\tright");
        assert!(
            ctx.render(&ir, &CueStyle::outline_only(), (640, 360), None, 0)
                .is_some()
        );
    }

    #[test]
    fn a_ruby_annotation_is_drawn_over_its_base_and_under_it_on_request() {
        let mut ctx = RasterCtx::new();
        let style = CueStyle::outline_only();
        let canvas = (640, 360);

        let bare = ctx
            .render(&ruby_cue("base", None), &style, canvas, None, 0)
            .expect("the bare cue rasterizes");
        let over = ctx
            .render(
                &ruby_cue("base", Some(("anno", ir::RubyPosition::Over))),
                &style,
                canvas,
                None,
                0,
            )
            .expect("the annotated cue rasterizes");
        let under = ctx
            .render(
                &ruby_cue("base", Some(("anno", ir::RubyPosition::Under))),
                &style,
                canvas,
                None,
                0,
            )
            .expect("the annotated cue rasterizes");

        assert!(
            ink_pixels(&over) > ink_pixels(&bare),
            "the annotation painted nothing: {} px against the bare cue's {}",
            ink_pixels(&over),
            ink_pixels(&bare)
        );

        let (bare_rows, over_rows, under_rows) =
            (ink_rows(&bare), ink_rows(&over), ink_rows(&under));
        for (name, rows) in [
            ("bare", &bare_rows),
            ("over", &over_rows),
            ("under", &under_rows),
        ] {
            assert!(!rows.is_empty(), "the {name} cue laid out to nothing");
        }

        // From the BOTTOM edge up: with the annotation over it, ink starts
        // higher than the base's own ink ever does.
        let from_bottom = |out: &RasterOut, rows: &[usize]| out.height as usize - rows[0];
        assert!(
            from_bottom(&over, &over_rows) > from_bottom(&bare, &bare_rows),
            "nothing is drawn above the base text: ink starts {} px from the bottom, the bare cue's \
             starts {} px from it",
            from_bottom(&over, &over_rows),
            from_bottom(&bare, &bare_rows)
        );
        // ...and with the annotation under it, from the TOP edge down.
        let from_top = |rows: &[usize]| *rows.last().expect("non-empty");
        assert!(
            from_top(&under_rows) > from_top(&bare_rows),
            "nothing is drawn below the base text: ink ends at row {}, the bare cue's ends at {}",
            from_top(&under_rows),
            from_top(&bare_rows)
        );
        // The two are each other's mirror, not the same picture.
        assert_ne!(
            over.pixels, under.pixels,
            "Over and Under rendered the same"
        );
    }

    /// A base and its annotation are ONE unit through line breaking: the base
    /// may not be split, because half a base cannot carry the annotation drawn
    /// over the whole of it.
    ///
    /// Measured by WIDTH, which is what the two outcomes differ in. At a wrap
    /// width that cannot hold the whole line, an unbreakable base moves to the
    /// next line whole (the raster is then as wide as the base), while a
    /// breakable one splits and leaves the wider prefix behind.
    #[test]
    fn a_ruby_base_is_never_split_by_a_line_break() {
        let mut ctx = RasterCtx::new();
        let style = CueStyle::outline_only();
        let canvas = (640, 360);

        // A wrap width that holds "base" but not "base text": the only break
        // opportunity in the cue is the one inside the annotated base.
        let mut annotated = ruby_cue("base text", Some(("x", ir::RubyPosition::Over)));
        annotated.layout.size = Some(10.0);
        let mut breakable = annotated.clone();
        breakable.lines[0].spans[0].ruby = None;

        let with_ruby = ctx
            .render(&annotated, &style, canvas, None, 0)
            .expect("rasterizes");
        let without = ctx
            .render(&breakable, &style, canvas, None, 0)
            .expect("rasterizes");

        // Unannotated, the cue wraps and the raster is one word wide.
        // Annotated, the base overflows the wrap in one piece and the raster is
        // as wide as both words -- which is the only way the annotation can sit
        // over the whole of it.
        assert!(
            with_ruby.width as f32 > without.width as f32 * 1.3,
            "the annotated base broke across lines like any other text: {} px wide against the \
             wrapped {} px",
            with_ruby.width,
            without.width
        );
    }

    /// The line carrying an annotation is given the room it needs, so a
    /// two-line cue does not draw furigana through the line above.
    ///
    /// Measured as height, which is where the room shows: the annotation is
    /// inside the cue's own box either way (the extents absorb an overhang), so
    /// a cue that did NOT buy the leading is exactly as tall as one with no
    /// annotation at all -- and its annotation is sitting on top of the
    /// previous line.
    #[test]
    fn a_line_carrying_ruby_is_given_the_room_for_it() {
        let mut ctx = RasterCtx::new();
        let style = CueStyle::outline_only();
        let canvas = (640, 360);

        let mut plain = CueIr::from_plain_text("first line");
        plain.lines.push(ir::Line {
            spans: vec![ir::Span::plain("second")],
        });
        let mut annotated = plain.clone();
        annotated.lines[1].spans[0].ruby = Some(ir::Ruby {
            text: "anno".to_owned(),
            position: ir::RubyPosition::Over,
        });

        let bare = ctx
            .render(&plain, &style, canvas, None, 0)
            .expect("rasterizes");
        let with_ruby = ctx
            .render(&annotated, &style, canvas, None, 0)
            .expect("rasterizes");

        // The annotation is ~0.6 em tall and the leading is twice that; a cue
        // that bought none would be the same height as the bare one.
        let grown = with_ruby.height as i32 - bare.height as i32;
        assert!(
            grown >= 10,
            "the annotated line grew by {grown} px: the annotation is being drawn into the line \
             above it"
        );
    }

    /// An annotation appears with the syllable it belongs to, not before it:
    /// karaoke visibility follows the BASE span, because the annotation is not
    /// a span of its own and has no reveal time.
    #[test]
    fn a_ruby_annotation_is_revealed_with_its_base() {
        let mut ctx = RasterCtx::new();
        let style = CueStyle::outline_only();
        let canvas = (640, 360);

        let mut ir = CueIr::from_plain_text("");
        let mut annotated = ir::Span::plain("later");
        annotated.reveal_ns = Some(1_000_000_000);
        annotated.ruby = Some(ir::Ruby {
            text: "anno".to_owned(),
            position: ir::RubyPosition::Over,
        });
        ir.lines[0].spans = vec![ir::Span::plain("now "), annotated];

        // Step 0: nothing with a reveal time has fired yet.
        let hidden = ctx
            .render(&ir, &style, canvas, None, 0)
            .expect("the un-timed span still paints");
        // Everything revealed.
        let shown = ctx
            .render(&ir, &style, canvas, None, usize::MAX)
            .expect("rasterizes");

        assert_eq!(
            (hidden.width, hidden.height),
            (shown.width, shown.height),
            "revealing a syllable must not reflow the cue -- the annotation's room is reserved \
             whether or not it is painted yet"
        );
        let (hidden_rows, shown_rows) = (ink_rows(&hidden), ink_rows(&shown));
        assert!(!hidden_rows.is_empty(), "the un-timed span painted nothing");
        assert!(
            shown_rows[0] < hidden_rows[0],
            "the annotation was painted before its base span was revealed: ink starts at row {} \
             either way",
            shown_rows[0]
        );
        assert!(ink_pixels(&shown) > ink_pixels(&hidden));
    }

    /// Annotations come out of subtitle files, so every degenerate one is data:
    /// empty, whitespace, enormous, and attached to a base with no text at all.
    /// None of them may panic, and none may produce a raster the size checks
    /// would have refused.
    #[test]
    fn hostile_ruby_annotations_are_data() {
        let mut ctx = RasterCtx::new();
        let style = CueStyle::default();
        let canvas = (640, 360);

        let plain = ctx
            .render(&ruby_cue("base", None), &style, canvas, None, 0)
            .expect("rasterizes");

        for empty in ["", "   ", "\u{a0}\u{a0}"] {
            let out = ctx
                .render(
                    &ruby_cue("base", Some((empty, ir::RubyPosition::Over))),
                    &style,
                    canvas,
                    None,
                    0,
                )
                .expect("an empty annotation must not lose the cue");
            assert_eq!(
                (out.width, out.height),
                (plain.width, plain.height),
                "an annotation of {empty:?} took up room"
            );
        }

        // Longer than any line: it wraps inside its own layout instead of
        // widening the raster without bound.
        let huge = "annotation ".repeat(80);
        let out = ctx
            .render(
                &ruby_cue("base", Some((&huge, ir::RubyPosition::Over))),
                &style,
                canvas,
                None,
                0,
            )
            .expect("an enormous annotation must still render");
        assert!(
            out.width <= MAX_RASTER_PX as u32 && out.height <= MAX_RASTER_PX as u32,
            "{}x{} is over the raster bound",
            out.width,
            out.height
        );

        // An annotation whose base is empty: the parser emits exactly this for
        // an `<rt>` with nothing in front of it.
        let mut orphan = CueIr::from_plain_text("x");
        orphan.lines[0].spans[0] = ir::Span {
            text: String::new(),
            ruby: Some(ir::Ruby {
                text: "orphan".to_owned(),
                position: ir::RubyPosition::Over,
            }),
            ..ir::Span::default()
        };
        let _ = ctx.render(&orphan, &style, canvas, None, 0);
    }

    /// The LUT must stay within 1 LSB of the exact rounded division for every
    /// (value, alpha) pair, and be exact on the fast paths.
    #[test]
    fn unpremultiply_matches_the_reference_formula() {
        for alpha in 0..=255u32 {
            for value in 0..=255u32 {
                let out = premul_to_straight_rgba(&[value as u8, 0, 0, alpha as u8]);
                let expect = match alpha {
                    0 => 0,
                    a => ((value * 255 + a / 2) / a).min(255),
                };
                let got = out[0] as u32;
                assert!(
                    got.abs_diff(expect) <= 1,
                    "v={value} a={alpha}: got {got}, want {expect}"
                );
                if alpha == 255 || alpha == 0 {
                    assert_eq!(got, expect, "fast paths must be exact");
                }
                assert_eq!(out[3], alpha as u8);
            }
        }
    }

    /// Bug review #6 upstream: `LinePosition::Line` used to fall through to the
    /// bottom strip; `line:0` means the frame's top.
    #[test]
    fn line_number_positioning() {
        let frame = Frame {
            x: 0.0,
            y: 0.0,
            w: 640.0,
            h: 360.0,
        };
        let style = CueStyle::default();
        let mut ir = CueIr::from_plain_text("one line");
        ir.layout.line = Some(ir::LinePosition::Line(0));
        let (_, y) = place(&ir, &style, (640.0, 360.0), frame, 100.0, 40.0);
        assert_eq!(y, 0, "line:0 sits at the frame top");
        ir.layout.line = Some(ir::LinePosition::Line(-1));
        let (_, y) = place(&ir, &style, (640.0, 360.0), frame, 100.0, 40.0);
        assert_eq!(y, 320, "line:-1 is bottom-aligned");
    }

    /// A `\pos` cue anchors to the PICTURE, not the window.
    #[test]
    fn positioned_cues_track_the_video_rect() {
        let frame = Frame {
            x: 160.0,
            y: 90.0,
            w: 320.0,
            h: 180.0,
        };
        let mut ir = CueIr::from_plain_text("sign");
        ir.layout.origin = Some((50.0, 50.0));
        let (x, y) = place(
            &ir,
            &CueStyle::default(),
            (640.0, 360.0),
            frame,
            100.0,
            40.0,
        );
        // Default anchor is bottom-center: the anchor point sits at the
        // picture's center (320, 180 in window coords).
        assert_eq!((x + 50, y + 40), (320, 180));
    }

    /// The window-margins policy lets an unpositioned cue use the letterbox
    /// bar; opting out keeps it over the picture.
    #[test]
    fn default_cues_may_use_the_window_bars() {
        let frame = Frame {
            x: 0.0,
            y: 45.0,
            w: 640.0,
            h: 270.0,
        };
        let ir = CueIr::from_plain_text("plain subtitle");
        let (_, y) = place(
            &ir,
            &CueStyle::default(),
            (640.0, 360.0),
            frame,
            200.0,
            40.0,
        );
        assert!(y + 40 > 315, "window-margins policy must use the bar");

        let over_video = CueStyle {
            use_window_margins: false,
            ..CueStyle::default()
        };
        let (_, y) = place(&ir, &over_video, (640.0, 360.0), frame, 200.0, 40.0);
        assert!(
            y + 40 <= 315,
            "without window margins the cue stays on the picture"
        );
    }

    /// Hostile reveal timestamps must not reach `ClockTime::from_nseconds`'s
    /// `u64::MAX` sentinel assert.
    #[test]
    fn hostile_reveal_times_do_not_panic() {
        let mut ir = CueIr::from_plain_text("boom");
        ir.lines[0].spans[0].reveal_ns = Some(u64::MAX);
        let mut tail = ir.lines[0].spans[0].clone();
        tail.text = " tail".into();
        tail.reveal_ns = Some(u64::MAX - 1);
        ir.lines[0].spans.push(tail);

        let steps = reveal_steps(
            &ir,
            gst::ClockTime::from_seconds(u32::MAX as u64),
            Some(gst::ClockTime::ZERO),
            1.0,
        );
        assert!(steps.iter().all(|s| s.nseconds() < u64::MAX));
    }

    /// Reveal offsets are stream time: at 2x a +1000ms syllable fires at
    /// +500ms running time.
    #[test]
    fn playback_rate_scales_reveal_times() {
        let mut ir = CueIr::from_plain_text("la");
        let mut second = ir.lines[0].spans[0].clone();
        second.text = "la2".into();
        second.reveal_ns = Some(gst::ClockTime::from_mseconds(1_000).nseconds());
        ir.lines[0].spans.push(second);

        let steps = reveal_steps(&ir, gst::ClockTime::ZERO, Some(gst::ClockTime::ZERO), 2.0);
        assert_eq!(steps, vec![gst::ClockTime::from_mseconds(500)]);

        // A non-forward rate has no usable anchor: no stepping at all.
        assert!(
            reveal_steps(&ir, gst::ClockTime::ZERO, Some(gst::ClockTime::ZERO), -1.0).is_empty()
        );
        assert!(reveal_steps(&ir, gst::ClockTime::ZERO, None, 1.0).is_empty());
    }

    /// The classic element output still parses, without pango.
    #[test]
    fn pango_markup_parses_to_ir() {
        let ir = CueIr::from_pango_markup("<i>Hello</i> &amp; more");
        assert_eq!(ir.plain_text(), "Hello & more");
        assert_eq!(
            plain_text_of_markup("<i>Hello</i> &amp; more"),
            "Hello & more"
        );
    }
}
