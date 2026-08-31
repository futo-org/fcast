// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! [`CueScene`]: what a cue paints, as data, plus the vello_cpu backend that
//! turns it into pixels.
//!
//! [`crate::cue_ir`] owns the layout half (parley, the house style, the
//! placement geometry) and ends at [`CueScene`]. Everything past that point is
//! a backend: [`VelloBackend`] here, and the dodvg scene consumer in the slint
//! fork. One scene, two backends, which is what lets the oracle diff them.
//!
//! ## Layout
//!
//! Struct of arrays, one `Vec` per field, cleared and refilled rather than
//! reallocated (`CueScene::clear`). No enum per primitive, no `Box<dyn>`, no
//! trait objects: recording a scene is three linear scans over contiguous
//! memory.
//!
//! ## Coordinates
//!
//! Rect corners and glyph pens are in LAYOUT space; [`CueScene::translate`]
//! moves layout space onto the surface. The two are kept apart on purpose: a
//! backend that has a transform (vello's `set_transform`, dodvg's
//! `push_translate_scale`) applies it in its own precision, and folding the
//! translate into f32 coordinates here would quantize every pen position
//! before the backend ever sees it.
//!
//! ## Paint order
//!
//! Glyphs are in paint order (shadow pass, stroke pass, fill pass, then the
//! ruby annotations' own three passes). Rects carry
//! [`CueScene::rect_after_glyph`], the number of glyphs that must already be
//! painted when the rect goes down, which is what keeps an underline on top of
//! its own run's glyphs and underneath the next run's without an interleaved
//! command stream.
//!
//! ## Reveal rank (karaoke)
//!
//! A scene holds the WHOLE cue, every syllable of it, and each glyph and rect
//! carries the reveal rank it appears at ([`CueScene::glyph_rank`],
//! [`CueScene::rect_rank`]). Painting takes a threshold: rank <= threshold is
//! drawn, the rest is skipped ([`ALL_REVEALED`] draws everything). A karaoke
//! step is therefore a comparison at draw time, not another layout and not
//! another scene, which is what took the step dimension out of the engine's
//! scene key.
//!
//! The rank is a property of the CUE, never of the clock, so the geometry of a
//! cue is fixed the moment it is laid out: a syllable lighting up cannot move a
//! glyph, because there is only ever one shaping.

use peniko::Color;
use vello_cpu::{
    Glyph, Pixmap, RenderContext,
    kurbo::{Affine, Cap, Join, Rect, RoundedRect, Shape, Stroke, Vec2},
};

/// Straight-alpha RGBA, toolkit-agnostic.
pub type Rgba = [u8; 4];

/// The reveal threshold that draws a whole cue, karaoke included. A rank is a
/// `u16`, so no glyph can outrank it.
pub const ALL_REVEALED: u16 = u16::MAX;

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

/// The font a stretch of glyphs is drawn with, plus the pass it belongs to.
///
/// Interned: a run change is a `u16` in the glyph stream, not a branch per
/// glyph, and the shadow and fill passes of the same shaped run share one
/// entry (both are fills at the same size).
#[derive(Clone, Debug)]
pub struct CueRun {
    /// The font blob and the face index inside it; `Arc`-backed, so cloning a
    /// run is a refcount bump.
    pub font: parley::FontData,
    pub font_size: f32,
    /// Variation coordinates, a slice of [`CueScene::coords`].
    pub coords_start: u32,
    pub coords_len: u32,
    /// Stroke width in px; `0.0` means the fill pass. Round join, round cap.
    pub stroke_width: f32,
}

/// One cue as primitives: everything [`crate::cue_ir::RasterCtx::render`]
/// paints, with nothing about how it is painted.
#[derive(Default)]
pub struct CueScene {
    /// Where the surface lands, in window coordinates.
    pub origin: [i32; 2],
    /// Surface size in px.
    pub size: [u32; 2],
    /// Layout space to surface space.
    pub translate: [f32; 2],

    // Rects: the cue box, span backgrounds, underlines, strikethroughs.
    /// `[x0, y0, x1, y1]`, min and max corner. Corners rather than a width and
    /// a height because that is what both the geometry above and vello's
    /// `Rect` speak, and a round trip through an extent shifts an edge by an
    /// ULP.
    pub rect_xyxy: Vec<[f32; 4]>,
    /// Per-corner radii, top-left first. Uniform today; carried per corner
    /// because dodvg's rounded rect is.
    pub rect_radii: Vec<[f32; 4]>,
    /// Gaussian standard deviation of the feathered rim, `0.0` for a hard
    /// edge, which is what selects a plain fill over a blurred one.
    pub rect_sigma: Vec<f32>,
    pub rect_color: Vec<Rgba>,
    /// How many glyphs precede this rect in paint order. Non-decreasing.
    pub rect_after_glyph: Vec<u32>,
    /// Reveal rank of the rect; 0 is always visible. A span background or a
    /// decoration belongs to its own run, so it appears with it.
    pub rect_rank: Vec<u16>,

    // Glyphs, in paint order: shadow pass, then stroke pass, then fill pass.
    pub glyph_id: Vec<u32>,
    pub glyph_xy: Vec<[f32; 2]>,
    pub glyph_run: Vec<u16>,
    pub glyph_color: Vec<Rgba>,
    /// Reveal rank of the glyph; 0 is always visible. See the module's reveal
    /// section: the rank is the karaoke timeline made into data, and a step is
    /// a comparison against it rather than another scene.
    pub glyph_rank: Vec<u16>,

    pub runs: Vec<CueRun>,
    /// Variation coordinate arena, sliced by [`CueRun::coords_start`].
    pub coords: Vec<i16>,
}

/// A run table index of this value means the table is full and the cue must be
/// refused rather than half-drawn.
const RUN_LIMIT: usize = u16::MAX as usize;

/// Counts and geometry, never the arrays: a scene is thousands of glyphs and
/// this is read in log lines and panic messages.
impl std::fmt::Debug for CueScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CueScene")
            .field("origin", &self.origin)
            .field("size", &self.size)
            .field("glyphs", &self.glyph_count())
            .field("rects", &self.rect_count())
            .field("runs", &self.runs.len())
            .field("max_rank", &self.max_rank())
            .finish()
    }
}

impl CueScene {
    /// Drop the contents, keep the capacity. A pooled scene never allocates
    /// after its first cue.
    pub fn clear(&mut self) {
        self.origin = [0, 0];
        self.size = [0, 0];
        self.translate = [0.0, 0.0];
        self.rect_xyxy.clear();
        self.rect_radii.clear();
        self.rect_sigma.clear();
        self.rect_color.clear();
        self.rect_after_glyph.clear();
        self.rect_rank.clear();
        self.glyph_id.clear();
        self.glyph_xy.clear();
        self.glyph_run.clear();
        self.glyph_color.clear();
        self.glyph_rank.clear();
        self.runs.clear();
        self.coords.clear();
    }

    pub fn glyph_count(&self) -> usize {
        self.glyph_id.len()
    }

    pub fn rect_count(&self) -> usize {
        self.rect_color.len()
    }

    /// The variation coordinates of run `i`.
    pub fn run_coords(&self, i: u16) -> &[i16] {
        let run = &self.runs[i as usize];
        let start = run.coords_start as usize;
        &self.coords[start..start + run.coords_len as usize]
    }

    /// Record a rect at the current paint position, visible from `rank` on.
    pub fn push_rect(&mut self, xyxy: [f32; 4], radius: f32, sigma: f32, color: Rgba, rank: u16) {
        self.rect_xyxy.push(xyxy);
        self.rect_radii.push([radius; 4]);
        self.rect_sigma.push(sigma);
        self.rect_color.push(color);
        self.rect_after_glyph.push(self.glyph_id.len() as u32);
        self.rect_rank.push(rank);
    }

    /// The index of the run with this font, size, coords and stroke width,
    /// appending it if it is new. `None` when the run table is full.
    pub fn intern_run(
        &mut self,
        font: &parley::FontData,
        font_size: f32,
        coords: &[i16],
        stroke_width: f32,
    ) -> Option<u16> {
        let existing = self.runs.iter().position(|r| {
            // `FontData` compares by blob id and face index, not by content.
            r.font == *font
                && r.font_size.to_bits() == font_size.to_bits()
                && r.stroke_width.to_bits() == stroke_width.to_bits()
                && r.coords_len as usize == coords.len()
                && self.coords[r.coords_start as usize..][..coords.len()] == *coords
        });
        if let Some(i) = existing {
            return Some(i as u16);
        }
        if self.runs.len() >= RUN_LIMIT {
            return None;
        }
        let coords_start = self.coords.len() as u32;
        self.coords.extend_from_slice(coords);
        self.runs.push(CueRun {
            font: font.clone(),
            font_size,
            coords_start,
            coords_len: coords.len() as u32,
            stroke_width,
        });
        Some((self.runs.len() - 1) as u16)
    }

    /// Record one glyph of run `run`, visible from `rank` on.
    pub fn push_glyph(&mut self, run: u16, id: u32, x: f32, y: f32, color: Rgba, rank: u16) {
        self.glyph_id.push(id);
        self.glyph_xy.push([x, y]);
        self.glyph_run.push(run);
        self.glyph_color.push(color);
        self.glyph_rank.push(rank);
    }

    /// The highest rank any part of this cue carries, i.e. the threshold at
    /// which the last syllable lights up. 0 for a cue with no karaoke.
    pub fn max_rank(&self) -> u16 {
        let glyphs = self.glyph_rank.iter().copied().max().unwrap_or(0);
        let rects = self.rect_rank.iter().copied().max().unwrap_or(0);
        glyphs.max(rects)
    }
}

/// vello_cpu's straight-alpha RGBA answer for a [`CueScene`].
///
/// Holds the render surface, reused across scenes of the same size (karaoke
/// steps in particular): keeps vello's glyph outline/hinting cache warm
/// (`reset()` retains it) and avoids two large allocations per raster.
/// `render_to_pixmap` clears before writing, so reuse is safe.
#[derive(Default)]
pub struct VelloBackend {
    surface: Option<((u16, u16), RenderContext, Pixmap)>,
}

impl VelloBackend {
    pub fn new() -> Self {
        Self::default()
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

    /// Paint everything in `scene` that has been revealed at `rank` (see the
    /// module's reveal section; [`ALL_REVEALED`] is the whole cue). `None` when
    /// the scene carries a size no surface can hold, which the builder already
    /// refuses, so it is a belt on a brace.
    pub fn rasterize(&mut self, scene: &CueScene, rank: u16) -> Option<RasterOut> {
        let [w, h] = scene.size;
        if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
            return None;
        }
        let (rc, pixmap) = self.surface((w as u16, h as u16));
        rc.set_transform(Affine::translate(Vec2::new(
            scene.translate[0] as f64,
            scene.translate[1] as f64,
        )));

        // One merged linear scan: the pending rects that belong before glyph
        // `i`, then the longest stretch of glyphs sharing a run and a colour
        // that no rect cuts into. Unrevealed elements are stepped over without
        // being drawn, and their INDEX still counts, so skipping a syllable
        // cannot slide a rect past the glyphs it belongs behind.
        let glyphs = scene.glyph_count();
        let rects = scene.rect_count();
        let (mut i, mut r) = (0usize, 0usize);
        loop {
            while r < rects && scene.rect_after_glyph[r] as usize <= i {
                if scene.rect_rank[r] <= rank {
                    fill_rect(rc, scene, r);
                }
                r += 1;
            }
            if i >= glyphs {
                break;
            }
            if scene.glyph_rank[i] > rank {
                i += 1;
                continue;
            }
            let (run, color) = (scene.glyph_run[i], scene.glyph_color[i]);
            let mut j = i + 1;
            while j < glyphs
                && scene.glyph_run[j] == run
                && scene.glyph_color[j] == color
                && scene.glyph_rank[j] <= rank
                && !(r < rects && scene.rect_after_glyph[r] as usize <= j)
            {
                j += 1;
            }
            draw_glyphs(rc, scene, run, color, i, j);
            i = j;
        }

        rc.flush();
        rc.render_to_pixmap(pixmap);
        Some(RasterOut {
            pixels: premul_to_straight_rgba(pixmap.data_as_u8_slice()),
            width: w,
            height: h,
            x: scene.origin[0],
            y: scene.origin[1],
        })
    }
}

fn paint(c: Rgba) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

/// A hard square rect, a rounded one, or a gaussian-feathered one, picked by
/// the radius and the sigma.
fn fill_rect(rc: &mut RenderContext, scene: &CueScene, r: usize) {
    let [x0, y0, x1, y1] = scene.rect_xyxy[r];
    let radius = scene.rect_radii[r][0];
    let sigma = scene.rect_sigma[r];
    rc.set_paint(paint(scene.rect_color[r]));
    let rect = Rect::new(x0 as f64, y0 as f64, x1 as f64, y1 as f64);
    if sigma > 0.0 {
        rc.fill_blurred_rounded_rect(&rect, radius, sigma);
    } else if radius > 0.0 {
        rc.fill_path(&RoundedRect::from_rect(rect, radius as f64).to_path(0.1));
    } else {
        rc.fill_rect(&rect);
    }
}

/// Glyphs `[from, to)` through vello's glyph pipeline, filled or stroked.
fn draw_glyphs(
    rc: &mut RenderContext,
    scene: &CueScene,
    run: u16,
    color: Rgba,
    from: usize,
    to: usize,
) {
    let width = scene.runs[run as usize].stroke_width;
    rc.set_paint(paint(color));
    if width > 0.0 {
        rc.set_stroke(
            Stroke::new(width as f64)
                .with_join(Join::Round)
                .with_caps(Cap::Round),
        );
    }
    let entry = &scene.runs[run as usize];
    let builder = rc
        .glyph_run(&entry.font)
        .font_size(entry.font_size)
        .hint(true)
        .normalized_coords(scene.run_coords(run));
    let positioned = (from..to).map(|k| Glyph {
        id: scene.glyph_id[k],
        x: scene.glyph_xy[k][0],
        y: scene.glyph_xy[k][1],
    });
    if width > 0.0 {
        builder.stroke_glyphs(positioned);
    } else {
        builder.fill_glyphs(positioned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue_ir::{CueStyle, RasterCtx, ir};
    use ir::CueIr;

    /// A cue with a per-span colour break, an underline and a drop shadow, so
    /// every pass and every primitive kind is in the scene.
    fn busy_cue() -> CueIr {
        let mut ir = CueIr::from_plain_text("");
        let mut first = ir::Span::plain("first ");
        first.style.underline = Some(true);
        let mut second = ir::Span::plain("second");
        second.style.foreground = Some(ir::Color::rgb(255, 0, 0));
        ir.lines[0].spans = vec![first, second];
        ir.base.shadow = Some(ir::Shadow {
            color: ir::Color::rgba(0, 0, 0, 180),
            dx: 2.0,
            dy: 3.0,
            blur: 0.0,
        });
        ir
    }

    /// The split's whole contract: the scene plus the vello_cpu backend paint
    /// exactly what the one-piece rasterizer painted. Byte for byte, because
    /// anything less means the display list quantized a pen position or
    /// reordered a primitive.
    #[test]
    fn a_scene_through_the_vello_backend_is_the_raster_render_produces() {
        let mut ctx = RasterCtx::new();
        let mut backend = VelloBackend::new();
        for house in [
            CueStyle::default(),
            CueStyle::boxed(),
            CueStyle::outline_only(),
        ] {
            for (name, ir) in [
                ("plain", CueIr::from_plain_text("The quick brown fox")),
                ("busy", busy_cue()),
            ] {
                let direct = ctx
                    .render(&ir, &house, (1280, 720), None, usize::MAX)
                    .expect("the cue rasterizes");
                let scene = ctx
                    .build_scene(&ir, &house, (1280, 720), None)
                    .expect("the cue builds a scene");
                let staged = backend
                    .rasterize(&scene, ALL_REVEALED)
                    .expect("the scene rasterizes");
                assert_eq!(
                    (direct.width, direct.height, direct.x, direct.y),
                    (staged.width, staged.height, staged.x, staged.y),
                    "{name}: the scene disagrees about the surface"
                );
                assert!(
                    direct.pixels == staged.pixels,
                    "{name}: the scene painted different pixels"
                );
            }
        }
    }

    /// A decoration belongs to its own run's fill, not to the whole cue: it
    /// covers that run's glyphs and sits under the next run's. The ordering
    /// key is what carries that, since the rects are not in the glyph stream.
    #[test]
    fn a_decoration_is_ordered_between_its_own_run_and_the_next() {
        let mut ctx = RasterCtx::new();
        let scene = ctx
            .build_scene(&busy_cue(), &CueStyle::default(), (1280, 720), None)
            .expect("the cue builds a scene");

        assert!(
            scene.rect_after_glyph.windows(2).all(|w| w[0] <= w[1]),
            "the rect order key must be non-decreasing: {:?}",
            scene.rect_after_glyph
        );
        // The cue box goes down before any glyph; the underline goes down
        // after its own run's fill and before the next run's.
        assert_eq!(scene.rect_after_glyph.first(), Some(&0), "the box is first");
        let last = *scene.rect_after_glyph.last().expect("a decoration rect");
        assert!(
            last > 0 && (last as usize) < scene.glyph_count(),
            "the underline landed at {last} of {} glyphs, so it is not inside the fill pass",
            scene.glyph_count()
        );
    }

    /// The run table is interned, not appended per pass: the shadow and the
    /// fill of one shaped run are the same font at the same size and share an
    /// entry, while the stroke pass is a different pass and does not.
    #[test]
    fn the_run_table_interns_the_passes_that_are_the_same_draw() {
        let mut ctx = RasterCtx::new();
        let scene = ctx
            .build_scene(&busy_cue(), &CueStyle::outline_only(), (1280, 720), None)
            .expect("the cue builds a scene");

        let stroked = scene.runs.iter().filter(|r| r.stroke_width > 0.0).count();
        let filled = scene.runs.len() - stroked;
        assert_eq!(
            stroked,
            filled,
            "every shaped run must appear once stroked and once filled: {:?}",
            scene.runs.len()
        );
        // Three passes over the same runs, one table entry per (run, pass).
        assert!(
            scene.runs.len() * 2 <= scene.glyph_count(),
            "{} run entries for {} glyphs is not an interned table",
            scene.runs.len(),
            scene.glyph_count()
        );
    }

    /// The pool claim: a cleared scene keeps every allocation, so a steady
    /// stream of cues allocates nothing after the first.
    #[test]
    fn clearing_a_scene_keeps_its_capacity() {
        let mut ctx = RasterCtx::new();
        let mut scene = ctx
            .build_scene(&busy_cue(), &CueStyle::default(), (1280, 720), None)
            .expect("the cue builds a scene");
        let (glyphs, rects, runs) = (
            scene.glyph_id.capacity(),
            scene.rect_color.capacity(),
            scene.runs.capacity(),
        );
        assert!(glyphs > 0 && rects > 0 && runs > 0);
        scene.clear();
        assert_eq!(scene.glyph_count(), 0);
        assert_eq!(scene.rect_count(), 0);
        assert_eq!(
            (
                scene.glyph_id.capacity(),
                scene.rect_color.capacity(),
                scene.runs.capacity()
            ),
            (glyphs, rects, runs)
        );
    }

    // ---- karaoke: the rank, and the step it replaces ----

    /// Three syllables, the last two timed, each in its own colour, with an
    /// underline on the middle one so a RECT carries a rank as well.
    fn karaoke_cue() -> CueIr {
        let mut ir = CueIr::from_plain_text("");
        let mut spans = Vec::new();
        for (i, word) in ["first ", "second ", "third"].into_iter().enumerate() {
            let mut span = ir::Span::plain(word);
            // The first syllable is untimed, so the cue has rank-0 ink too.
            if i > 0 {
                span.reveal_ns = Some(i as u64 * 1_000_000_000);
            }
            span.style.foreground = Some(ir::Color::rgb(255, (80 * i) as u8, 0));
            if i == 1 {
                span.style.underline = Some(true);
            }
            spans.push(span);
        }
        ir.lines[0].spans = spans;
        ir
    }

    /// Opaque pixels, i.e. how much of the cue has lit up.
    fn ink(out: &RasterOut) -> usize {
        out.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] > 200)
            .count()
    }

    /// THE WAVE 5 CONTRACT: one scene serves the whole reveal, and the step
    /// that used to key a raster of its own is now a threshold on it.
    ///
    /// Both halves are here. The scene painted at rank k must be byte for byte
    /// what [`RasterCtx::render`] answers at step k, which is the step-to-rank
    /// mapping the engine relies on and the compatibility every pixel test in
    /// the tree was written against; and the sweep must actually reveal, with
    /// the geometry frozen throughout, since a syllable lighting up may not
    /// move a glyph.
    #[test]
    fn a_reveal_step_is_a_threshold_over_one_scene() {
        let mut ctx = RasterCtx::new();
        let mut backend = VelloBackend::new();
        let ir = karaoke_cue();
        let house = CueStyle::default();

        let scene = ctx
            .build_scene(&ir, &house, (1280, 720), None)
            .expect("the karaoke cue builds a scene");
        let top = scene.max_rank();
        assert_eq!(top, 2, "two timed syllables are two ranks");

        let mut inks = Vec::new();
        for rank in 0..=top {
            let staged = backend
                .rasterize(&scene, rank)
                .expect("the scene rasterizes");
            let stepped = ctx
                .render(&ir, &house, (1280, 720), None, rank as usize)
                .expect("the cue rasterizes");
            assert_eq!(
                (staged.width, staged.height, staged.x, staged.y),
                (stepped.width, stepped.height, stepped.x, stepped.y),
                "rank {rank}: the threshold moved the surface"
            );
            assert!(
                staged.pixels == stepped.pixels,
                "rank {rank}: the scene at a threshold is not the raster that step produces"
            );
            assert_eq!(
                (staged.width, staged.height),
                (scene.size[0], scene.size[1]),
                "rank {rank}: the cue resized as it revealed"
            );
            inks.push(ink(&staged));
        }
        assert!(
            inks.windows(2).all(|w| w[1] > w[0]),
            "each step must paint more ink than the one before it: {inks:?}"
        );
        // And the last step is the whole cue, so nothing is left over above it.
        let all = backend
            .rasterize(&scene, ALL_REVEALED)
            .expect("the scene rasterizes");
        assert_eq!(
            ink(&all),
            *inks.last().expect("a sweep"),
            "the top rank is not the whole cue"
        );
    }

    /// The rank is per SYLLABLE and it reaches every primitive that syllable
    /// paints: its glyphs in all three passes, and the underline under them.
    #[test]
    fn every_primitive_of_a_syllable_carries_its_rank() {
        let mut ctx = RasterCtx::new();
        let scene = ctx
            .build_scene(&karaoke_cue(), &CueStyle::default(), (1280, 720), None)
            .expect("the karaoke cue builds a scene");

        let mut ranks: Vec<u16> = scene.glyph_rank.clone();
        ranks.sort_unstable();
        ranks.dedup();
        assert_eq!(ranks, vec![0, 1, 2], "a rank per syllable, and one untimed");
        // The underline belongs to the middle syllable, so it waits for it.
        assert!(
            scene.rect_rank.contains(&1),
            "the middle syllable's underline is visible from the start: {:?}",
            scene.rect_rank
        );
        // The readability box is the cue's, not a syllable's.
        assert_eq!(
            scene.rect_rank.first(),
            Some(&0),
            "the box waits for nobody"
        );
        // Every rank appears three times over (shadow, stroke, fill), which is
        // what proves the stamp is on the pass and not on the span table.
        for rank in [0u16, 1, 2] {
            let count = scene.glyph_rank.iter().filter(|r| **r == rank).count();
            assert!(count > 0, "rank {rank} painted nothing");
        }
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
pub(crate) fn premul_to_straight_rgba(data: &[u8]) -> Vec<u8> {
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
