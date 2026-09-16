//! Subtitle cues on the desktop lane: the engine's display lists, put in
//! front of the dodvg renderer as its own scene type. Driven from the video
//! lane's appsink (`desktop_wgpu_video::Cues`); nothing here knows about the
//! picture it sits over.
//!
//! The engine (`fcast_video::cue`) lays a cue out on its worker and publishes
//! an `Arc<fcast_video::cue_scene::CueScene>`. The renderer takes an
//! `Arc<slint::winit_030::cues::CueScene>`, which mirrors it field for field
//! but cannot be the same type (the renderer must not depend on the receiver).
//! This module is that hop, and it is the only place the two meet.
//!
//! ## What it costs
//!
//! Nothing per video frame. A steady cue is: compare the shown set against what
//! was published (pointer, placement, rank), find it unchanged, return. The SoA
//! copy runs when the engine publishes a NEW scene, when the stack moves, or
//! when a karaoke step needs the multi-cue flatten redone.
//! [`CueOverlay::builds`] counts it and the allocation gate in
//! `desktop_wgpu_video.rs` asserts on it.
//!
//! ## Pooling
//!
//! Two scenes, alternated. The renderer holds the one last handed over, so the
//! other always has a refcount of one and is rebuilt into with its arrays'
//! capacity intact. A warm overlay therefore allocates only when a cue grows
//! past the high-water mark of the one two cues ago.
//!
//! ## Karaoke
//!
//! One cue on screen is copied with its reveal ranks intact, and a step is
//! `set_cue_reveal_rank`, which touches no scene at all. Several cues on screen
//! share one renderer-side threshold, so their ranks are flattened to
//! visible (0) or not (1) at copy time and a step there does cost a copy. That
//! is the trade the fork's one-slot overlay API makes, and a karaoke line
//! overlapping another cue is rare enough to pay it.

use std::sync::Arc;

use fcast_video::cue::ShownScene;
use slint::winit_030::cues;

/// Cues the engine may stack at once ([`fcast_video`]'s own `MAX_ACTIVE_CUES`).
/// Tracking is a fixed array so comparing the shown set never allocates.
const MAX_TRACKED: usize = 8;

/// Distinct font faces remembered. A subtitle file uses one to four; the cap is
/// a backstop against a pathological `\fn`-per-line file, not a working bound.
const MAX_FONTS: usize = 16;

/// The reveal threshold that draws everything, mirroring
/// `fcast_video::cue_scene::ALL_REVEALED`.
const ALL_REVEALED: u16 = u16::MAX;

/// Where a published overlay goes.
///
/// A trait rather than the window directly so the translation and the pooling
/// can be graded headlessly, without a backend, an event loop or a GPU. The
/// production implementation is [`WindowCues`] and it is three lines.
pub(crate) trait CueSink {
    fn set_scene(&self, scene: Option<Arc<cues::CueScene>>);
    fn set_rank(&self, rank: u16);
}

/// The dodvg renderer behind a real window.
pub(crate) struct WindowCues<'a>(pub(crate) &'a slint::Window);

impl CueSink for WindowCues<'_> {
    fn set_scene(&self, scene: Option<Arc<cues::CueScene>>) {
        use slint::winit_030::DodvgWindowAccessor;
        if self
            .0
            .with_dodvg_renderer(|r| r.set_cue_overlay(scene))
            .is_none()
        {
            // Not a winit window, or not a dodvg renderer. Nothing to draw on,
            // and a subtitle that never appears is worth exactly one line.
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("cue overlay: no dodvg renderer, cues will not be drawn")
            });
        }
    }

    fn set_rank(&self, rank: u16) {
        use slint::winit_030::DodvgWindowAccessor;
        self.0.with_dodvg_renderer(|r| r.set_cue_reveal_rank(rank));
    }
}

/// Identity of one published cue: enough to tell "the same thing is still on
/// screen" from "something moved", without touching the arrays.
#[derive(Clone, Copy, Default, PartialEq)]
struct Shown {
    /// The engine scene's allocation. Only ever compared, never dereferenced;
    /// the `Arc` behind it is held by [`CueOverlay::held`] for exactly as long,
    /// so a recycled address cannot answer for a different cue.
    ptr: usize,
    x: i32,
    y: i32,
    rank: u16,
}

/// Font blobs already handed to the renderer, by (blob id, face index).
///
/// The renderer wants an `Arc<Vec<u8>>` and the engine has a `parley` blob, so
/// the bytes are copied once per face and refcounted after that. A face copied
/// per frame, or per cue, would be megabytes of memcpy behind a subtitle track.
#[derive(Default)]
struct FontIntern {
    entries: Vec<cues::CueFont>,
}

impl FontIntern {
    /// Takes the whole run rather than the font so this crate never has to name
    /// `parley`, which it does not depend on. Field access resolves anyway.
    fn intern(&mut self, run: &fcast_video::cue_scene::CueRun) -> cues::CueFont {
        let (id, index) = (run.font.data.id(), run.font.index);
        if let Some(hit) = self.entries.iter().find(|f| f.id == id && f.index == index) {
            return hit.clone();
        }
        let font = cues::CueFont {
            id,
            blob: Arc::new(run.font.data.data().to_vec()),
            index,
        };
        if self.entries.len() >= MAX_FONTS {
            self.entries.remove(0);
        }
        self.entries.push(font.clone());
        font
    }
}

/// What is on the renderer's overlay slot, and what it took to put it there.
pub(crate) struct CueOverlay {
    /// Alternating scene pool, see the module docs.
    pool: [Option<Arc<cues::CueScene>>; 2],
    next: usize,
    /// The engine scenes behind what is published, so `Shown::ptr` stays an
    /// identity for as long as it is compared against.
    held: Vec<Arc<fcast_video::cue_scene::CueScene>>,
    shown: [Shown; MAX_TRACKED],
    shown_len: usize,
    /// The threshold last pushed at the renderer.
    rank: u16,
    fonts: FontIntern,
    /// A cue is up. The lane forces `video-obstructed` on this, because a
    /// scene-layer overlay is invisible under a video surface stacked above the
    /// GUI.
    visible: bool,
    /// Scenes copied. The allocation gate's measurement: a steady cue must not
    /// move it, frame after frame.
    builds: u64,
}

impl Default for CueOverlay {
    fn default() -> Self {
        Self {
            pool: [None, None],
            next: 0,
            held: Vec::new(),
            shown: [Shown::default(); MAX_TRACKED],
            shown_len: 0,
            rank: ALL_REVEALED,
            fonts: FontIntern::default(),
            visible: false,
            builds: 0,
        }
    }
}

impl CueOverlay {
    /// Put `shown` in front of the renderer and answer whether a cue is up.
    ///
    /// The fast paths, in the order they are taken: nothing moved (a compare
    /// per cue and no more), only the reveal rank moved with one cue on screen
    /// (a `u16` at the renderer), everything else (one SoA copy into the spare
    /// pool slot and an `Arc` swap).
    pub(crate) fn sync(&mut self, sink: &dyn CueSink, shown: &[ShownScene]) -> bool {
        let n = shown.len().min(MAX_TRACKED);
        let same_set = n == self.shown_len
            && shown[..n]
                .iter()
                .zip(&self.shown[..n])
                .all(|(s, p)| Arc::as_ptr(&s.scene) as usize == p.ptr && s.x == p.x && s.y == p.y);

        if same_set {
            if n == 1 {
                // One cue: the copy kept its per glyph ranks, so a syllable
                // lighting up is a threshold and nothing else.
                if shown[0].rank != self.rank {
                    self.rank = shown[0].rank;
                    self.shown[0].rank = shown[0].rank;
                    sink.set_rank(self.rank);
                }
                return self.visible;
            }
            if shown[..n]
                .iter()
                .zip(&self.shown[..n])
                .all(|(s, p)| s.rank == p.rank)
            {
                return self.visible;
            }
            // A stacked cue's rank moved, and a stack shares one threshold, so
            // the flatten below has to be redone. Falls through to the copy.
        }

        if n == 0 {
            if self.visible || self.shown_len != 0 {
                self.shown_len = 0;
                self.held.clear();
                self.visible = false;
                sink.set_scene(None);
            }
            return false;
        }

        self.build(sink, &shown[..n]);
        true
    }

    /// Copy the shown set into the spare pool slot and hand it over.
    fn build(&mut self, sink: &dyn CueSink, shown: &[ShownScene]) {
        let slot = self.next;
        self.next ^= 1;
        // Reuse only when the renderer has moved on from this slot, which
        // alternating guarantees. The fallback is a fresh scene rather than a
        // clobbered one.
        let mut arc = match self.pool[slot].take() {
            Some(scene) if Arc::strong_count(&scene) == 1 => scene,
            _ => Arc::new(cues::CueScene::default()),
        };
        let dst = Arc::get_mut(&mut arc).expect("the slot was taken at refcount one");
        dst.clear();

        // One cue keeps its own placement and its own ranks, which is what
        // makes a karaoke step free. A stack has to share one origin and one
        // threshold, so both are folded in.
        let single = shown.len() == 1;
        for s in shown {
            copy_scene(dst, s, single, &mut self.fonts);
        }
        if single {
            dst.origin = [shown[0].x, shown[0].y];
            dst.translate = shown[0].scene.translate;
            dst.size = shown[0].scene.size;
            self.rank = shown[0].rank;
        } else {
            dst.size = extent(shown);
            self.rank = 0;
        }
        self.builds += 1;

        self.held.clear();
        for (i, s) in shown.iter().enumerate() {
            self.shown[i] = Shown {
                ptr: Arc::as_ptr(&s.scene) as usize,
                x: s.x,
                y: s.y,
                rank: s.rank,
            };
            self.held.push(Arc::clone(&s.scene));
        }
        self.shown_len = shown.len();

        self.pool[slot] = Some(Arc::clone(&arc));
        // Both land before the next frame: the renderer draws on this thread,
        // so nothing can be recorded between the two.
        sink.set_scene(Some(arc));
        sink.set_rank(self.rank);
        self.visible = true;
    }

    /// Scenes copied since construction. The gate's measurement.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn builds(&self) -> u64 {
        self.builds
    }
}

/// The union extent of a stack, for diagnostics. The overlay is not clipped to
/// it, so a wrong answer here cannot hide a glyph.
fn extent(shown: &[ShownScene]) -> [u32; 2] {
    let mut size = [0u32; 2];
    for s in shown {
        size[0] = size[0].max((s.x.max(0) as u32).saturating_add(s.scene.size[0]));
        size[1] = size[1].max((s.y.max(0) as u32).saturating_add(s.scene.size[1]));
    }
    size
}

/// Append one engine scene to a renderer scene.
///
/// `single` keeps the coordinates and the reveal ranks exactly as the engine
/// laid them out, with the placement left to the scene's own origin. Otherwise
/// the placement is folded into every coordinate (a stack has one origin) and
/// the ranks are flattened against this cue's own threshold (a stack has one
/// threshold), which is what lets several cues share one display list.
fn copy_scene(dst: &mut cues::CueScene, shown: &ShownScene, single: bool, fonts: &mut FontIntern) {
    let src = shown.scene.as_ref();
    let (dx, dy) = if single {
        (0.0, 0.0)
    } else {
        (
            shown.x as f32 + src.translate[0],
            shown.y as f32 + src.translate[1],
        )
    };
    let rank_of = |rank: u16| -> u16 {
        if single {
            rank
        } else if rank <= shown.rank {
            0
        } else {
            1
        }
    };

    // The run table is shared, so this cue's run indices shift past whatever is
    // already in it. Beyond the u16 the stream cannot address a run, so the cue
    // is refused rather than drawn against another cue's font.
    let run_base = dst.runs.len();
    if run_base + src.runs.len() > u16::MAX as usize {
        return;
    }
    for (i, run) in src.runs.iter().enumerate() {
        let coords_start = dst.coords.len() as u32;
        dst.coords.extend_from_slice(src.run_coords(i as u16));
        dst.runs.push(cues::CueRun {
            font: fonts.intern(run),
            font_size: run.font_size,
            coords_start,
            coords_len: run.coords_len,
            stroke_width: run.stroke_width,
        });
    }
    let run_base = run_base as u16;

    let glyph_base = dst.glyph_id.len() as u32;
    for i in 0..src.glyph_count() {
        let [gx, gy] = src.glyph_xy[i];
        dst.glyph_id.push(src.glyph_id[i]);
        dst.glyph_xy.push([gx + dx, gy + dy]);
        dst.glyph_run.push(run_base + src.glyph_run[i]);
        dst.glyph_color.push(src.glyph_color[i]);
        dst.glyph_rank.push(rank_of(src.glyph_rank[i]));
    }

    // `rect_after_glyph` stays non decreasing across the concatenation: this
    // cue's rects all sit at or past `glyph_base`, and the previous cue's all
    // sit at or before it. So the renderer's merged walk paints cue by cue.
    for r in 0..src.rect_count() {
        let [x0, y0, x1, y1] = src.rect_xyxy[r];
        dst.rect_xyxy.push([x0 + dx, y0 + dy, x1 + dx, y1 + dy]);
        dst.rect_radii.push(src.rect_radii[r]);
        dst.rect_sigma.push(src.rect_sigma[r]);
        dst.rect_color.push(src.rect_color[r]);
        dst.rect_after_glyph
            .push(glyph_base + src.rect_after_glyph[r]);
        dst.rect_rank.push(rank_of(src.rect_rank[r]));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        cell::RefCell,
        time::{Duration, Instant},
    };

    use fcast_video::cue::{CueEngine, CueInput, TextFormat};

    use super::*;

    /// A [`CueSink`] that behaves like the renderer's overlay slot: it HOLDS
    /// exactly what it was last handed and drops the rest, which is what makes
    /// the pool's refcount test meaningful. Everything else is a log.
    #[derive(Default)]
    pub(crate) struct Recorder {
        /// The slot itself, one scene deep, exactly as the renderer keeps it.
        slot: RefCell<Option<Arc<cues::CueScene>>>,
        /// Every scene ever put on the slot, by address, and every take-down as
        /// a zero.
        sets: RefCell<Vec<usize>>,
        ranks: RefCell<Vec<u16>>,
    }

    impl Recorder {
        pub(crate) fn last(&self) -> Arc<cues::CueScene> {
            self.slot
                .borrow()
                .clone()
                .expect("a scene must be on the slot")
        }
    }

    impl CueSink for Recorder {
        fn set_scene(&self, scene: Option<Arc<cues::CueScene>>) {
            self.sets
                .borrow_mut()
                .push(scene.as_ref().map_or(0, |s| Arc::as_ptr(s) as usize));
            *self.slot.borrow_mut() = scene;
        }
        fn set_rank(&self, rank: u16) {
            self.ranks.borrow_mut().push(rank);
        }
    }

    pub(crate) const CANVAS: (u32, u32) = (1280, 720);
    pub(crate) const AT: u64 = 1;

    pub(crate) fn cue(text: &str, start_s: u64, end_s: u64) -> CueInput {
        CueInput {
            format: TextFormat::Utf8,
            text: text.to_owned(),
            start_rt: gst::ClockTime::from_seconds(start_s),
            end_rt: Some(gst::ClockTime::from_seconds(end_s)),
        }
    }

    /// A warm engine on the scene lane, and the display lists it shows at
    /// [`AT`]. The wait is the same one the cue-IR suite uses: layout happens
    /// on the engine's own worker on every lane.
    pub(crate) fn shown(cues: &[CueInput]) -> (CueEngine, Vec<ShownScene>) {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_scene_consumer(true);
        engine.set_canvas(CANVAS.0, CANVAS.1);
        let wanted = cues.len();
        for cue in cues {
            engine.submit(cue.clone());
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let shown = engine.scenes_for(Some(gst::ClockTime::from_seconds(AT)));
            if shown.len() == wanted {
                return (engine, shown.into_vec());
            }
            assert!(Instant::now() < deadline, "the cue scenes never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn one(text: &str) -> (CueEngine, ShownScene) {
        let (engine, mut shown) = shown(&[cue(text, 0, 10)]);
        (engine, shown.remove(0))
    }

    /// Two cues laid out by ONE engine, so they share its fontmap and its font
    /// blobs. Two engines would each load their own copy of the face, which is
    /// a different blob id and would make the intern below look broken when it
    /// is not.
    fn two() -> (CueEngine, ShownScene, ShownScene) {
        let (engine, mut shown) = shown(&[cue("alpha line", 0, 10), cue("beta line", 0, 10)]);
        let second = shown.remove(1);
        (engine, shown.remove(0), second)
    }

    /// Every invariant the renderer checks before it will draw a scene at all
    /// (`cues::is_consistent`, which is private over there).
    fn assert_consistent(scene: &cues::CueScene) {
        let (glyphs, rects) = (scene.glyph_count(), scene.rect_count());
        assert_eq!(scene.glyph_xy.len(), glyphs);
        assert_eq!(scene.glyph_run.len(), glyphs);
        assert_eq!(scene.glyph_color.len(), glyphs);
        assert_eq!(scene.glyph_rank.len(), glyphs);
        assert_eq!(scene.rect_xyxy.len(), rects);
        assert_eq!(scene.rect_radii.len(), rects);
        assert_eq!(scene.rect_sigma.len(), rects);
        assert_eq!(scene.rect_after_glyph.len(), rects);
        assert_eq!(scene.rect_rank.len(), rects);
        assert!(
            scene
                .glyph_run
                .iter()
                .all(|r| (*r as usize) < scene.runs.len()),
            "a glyph names a run that is not in the table"
        );
        assert!(
            scene
                .runs
                .iter()
                .all(|r| r.coords_start as usize + r.coords_len as usize <= scene.coords.len()),
            "a run slices past the coordinate arena"
        );
        assert!(
            scene.rect_after_glyph.windows(2).all(|w| w[0] <= w[1]),
            "the rect paint order is not monotonic, the renderer's merged walk would skip rects"
        );
    }

    /// THE CLAIM OF THE WAVE, on the translation half: what the engine laid out
    /// is what the renderer is handed, primitive for primitive.
    #[test]
    fn a_single_cue_is_copied_field_for_field() {
        let (_engine, shown) = one("the quick brown fox");
        let src = Arc::clone(&shown.scene);
        assert!(src.glyph_count() > 0, "an utf8 cue must have glyphs");

        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        assert!(overlay.sync(&recorder, std::slice::from_ref(&shown)));

        let dst = recorder.last();
        assert_consistent(&dst);
        assert_eq!(dst.glyph_count(), src.glyph_count());
        assert_eq!(dst.rect_count(), src.rect_count());
        assert_eq!(dst.runs.len(), src.runs.len());
        // One cue keeps its own placement, so the coordinates are untouched and
        // the origin carries the stacking answer.
        assert_eq!(dst.origin, [shown.x, shown.y]);
        assert_eq!(dst.translate, src.translate);
        assert_eq!(dst.size, src.size);
        assert_eq!(dst.glyph_xy, src.glyph_xy);
        assert_eq!(dst.glyph_id, src.glyph_id);
        assert_eq!(dst.glyph_rank, src.glyph_rank);
        assert_eq!(dst.glyph_run, src.glyph_run);
        assert_eq!(dst.rect_xyxy, src.rect_xyxy);
        assert_eq!(dst.rect_sigma, src.rect_sigma);
        assert_eq!(dst.rect_rank, src.rect_rank);
        for (a, b) in dst.runs.iter().zip(src.runs.iter()) {
            assert_eq!(a.font_size, b.font_size);
            assert_eq!(a.stroke_width, b.stroke_width);
            assert_eq!(a.font.index, b.font.index);
            assert_eq!(a.font.id, b.font.data.id());
            assert_eq!(a.font.blob.len(), b.font.data.data().len());
        }
        assert_eq!(*recorder.ranks.borrow().last().unwrap(), shown.rank);
    }

    /// THE ALLOCATION GATE'S PREMISE: a cue that is still the same cue is not
    /// copied again, however many frames go past.
    #[test]
    fn a_steady_cue_is_translated_once_and_never_again() {
        let (_engine, shown) = one("steady");
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let set = std::slice::from_ref(&shown);
        for _ in 0..240 {
            assert!(overlay.sync(&recorder, set));
        }
        assert_eq!(
            overlay.builds(),
            1,
            "the display list was rebuilt per frame"
        );
        assert_eq!(recorder.sets.borrow().len(), 1);
        assert_eq!(recorder.ranks.borrow().len(), 1);
    }

    /// A syllable lighting up is a `u16` at the renderer. No copy, no layout,
    /// and the same scene stays on the slot.
    #[test]
    fn a_karaoke_step_pushes_a_rank_and_leaves_the_scene_alone() {
        let (_engine, shown) = one("karaoke");
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        overlay.sync(&recorder, std::slice::from_ref(&shown));
        let published = recorder.last();

        for rank in 1..8u16 {
            let step = ShownScene {
                scene: Arc::clone(&shown.scene),
                rank,
                x: shown.x,
                y: shown.y,
            };
            // several frames per step, as a real sweep arrives
            for _ in 0..4 {
                overlay.sync(&recorder, std::slice::from_ref(&step));
            }
        }
        assert_eq!(
            overlay.builds(),
            1,
            "a reveal step rebuilt the display list"
        );
        assert_eq!(recorder.sets.borrow().len(), 1);
        assert!(
            Arc::ptr_eq(&recorder.last(), &published),
            "the slot was re-set with a different scene"
        );
        assert_eq!(
            *recorder.ranks.borrow(),
            vec![shown.rank, 1, 2, 3, 4, 5, 6, 7],
            "one push per step and no more"
        );
    }

    /// Two cues share one overlay slot, so they share one display list. Paint
    /// order and run indices have to survive the concatenation or the renderer
    /// draws one cue's glyphs in the other's font.
    #[test]
    fn a_stack_merges_into_one_display_list_in_paint_order() {
        let (_engine, shown) = shown(&[cue("first line", 0, 10), cue("second line", 0, 10)]);
        assert_eq!(shown.len(), 2, "both cues must be on screen");
        let (a, b) = (&shown[0], &shown[1]);

        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        assert!(overlay.sync(&recorder, &shown));
        let dst = recorder.last();
        assert_consistent(&dst);

        assert_eq!(
            dst.glyph_count(),
            a.scene.glyph_count() + b.scene.glyph_count()
        );
        assert_eq!(
            dst.rect_count(),
            a.scene.rect_count() + b.scene.rect_count()
        );
        assert_eq!(dst.runs.len(), a.scene.runs.len() + b.scene.runs.len());
        // A stack has one origin, so every coordinate carries its own cue's
        // placement, including the move the stacking made.
        assert_eq!(dst.origin, [0, 0]);
        assert_eq!(dst.translate, [0.0, 0.0]);
        let first = a.scene.glyph_count();
        let (dx, dy) = (
            b.x as f32 + b.scene.translate[0],
            b.y as f32 + b.scene.translate[1],
        );
        for i in 0..b.scene.glyph_count() {
            let [sx, sy] = b.scene.glyph_xy[i];
            assert_eq!(dst.glyph_xy[first + i], [sx + dx, sy + dy]);
            assert_eq!(
                dst.glyph_run[first + i],
                b.scene.glyph_run[i] + a.scene.runs.len() as u16,
                "the second cue's run index was not rebased"
            );
        }
        // and one threshold, so the ranks were flattened against it
        assert_eq!(*recorder.ranks.borrow().last().unwrap(), 0);
        assert!(dst.glyph_rank.iter().all(|r| *r == 0 || *r == 1));
    }

    /// The pool: a rebuild lands in the slot the renderer is not holding, so
    /// two scenes serve a whole session and their arrays keep their capacity.
    #[test]
    fn rebuilds_alternate_between_two_pooled_scenes() {
        let (_engine, first, second) = two();
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();

        for i in 0..6 {
            let s = if i % 2 == 0 { &first } else { &second };
            overlay.sync(&recorder, std::slice::from_ref(s));
        }
        assert_eq!(overlay.builds(), 6, "each swap is a different cue");
        let seen = recorder.sets.borrow().clone();
        let distinct: std::collections::BTreeSet<_> = seen.iter().collect();
        assert_eq!(distinct.len(), 2, "the pool grew past two scenes: {seen:?}");
        assert_eq!(seen[0], seen[2], "the slots did not alternate");
        assert_eq!(seen[1], seen[3]);
    }

    /// A cue ending, an EOS or a stop takes the overlay down, and does it once.
    #[test]
    fn an_empty_set_takes_the_overlay_down_once() {
        let (_engine, shown) = one("goodbye");
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        overlay.sync(&recorder, std::slice::from_ref(&shown));
        for _ in 0..10 {
            assert!(!overlay.sync(&recorder, &[]));
        }
        let sets = recorder.sets.borrow();
        assert_eq!(sets.len(), 2, "the take-down repeated: {sets:?}");
        assert_eq!(sets[1], 0, "the second set was not the take-down");
    }

    /// The font blob is copied once per face, never per cue and never per
    /// frame: a face is megabytes and a subtitle track redraws forever.
    #[test]
    fn a_face_is_copied_once_however_many_cues_use_it() {
        let (_engine, first, second) = two();
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        overlay.sync(&recorder, std::slice::from_ref(&first));
        let a = recorder.last();
        overlay.sync(&recorder, std::slice::from_ref(&second));
        let b = recorder.last();
        assert!(!a.runs.is_empty() && !b.runs.is_empty());
        assert!(
            Arc::ptr_eq(&a.runs[0].font.blob, &b.runs[0].font.blob),
            "the second cue re-copied the same face"
        );
    }
}

/// THE WAVE 7 GATE: the desktop lane, taken off the engine's painted subtitle
/// overlays and put on the renderer's scene slot, draws the same cue in the
/// same place.
///
/// A full pixel side by side is not reachable from a host test. The new lane's
/// pixels come out of the dodvg GL executor, which needs a window, a GL context
/// and a readback this crate's harness has none of. So the chain is closed in
/// three links instead, and each one is checked here or already checked
/// somewhere named:
///
///  1. the raster libplacebo uploaded is vello_cpu's paint of ONE `CueScene`,
///     and the scene lane hands the renderer THAT scene. Checked below by
///     repainting the published scene and diffing it against the overlay the
///     raster lane produced, byte for byte;
///  2. placement is the same. Both readers walk the same active list through
///     the same `stacked_y` and take the surface origin from the same scene, so
///     it is checked on the engine's own output, one cue and stacked;
///  3. dodvg's paint of a `CueScene` equals vello_cpu's paint of it, at the
///     oracle's thresholds. That is the fork's permanent oracle, not this
///     crate's to re-prove.
///
/// Link 1 is the one that could have gone wrong here and the one no other test
/// covers: it is what says the switch changed the painter and nothing else.
#[cfg(test)]
mod wave7_gate {
    use std::time::{Duration, Instant};

    use fcast_video::cue::{CueEngine, CueInput};

    use super::{
        tests::{AT, CANVAS, Recorder, cue},
        *,
    };

    /// A warm engine on the RASTER lane, which is what the desktop ran before
    /// this wave: vello_cpu paints every cue and `current_overlays` is what
    /// the video compositor uploaded.
    fn painted(cues: &[CueInput]) -> CueEngine {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_canvas(CANVAS.0, CANVAS.1);
        for cue in cues {
            engine.submit(cue.clone());
        }
        let at = gst::ClockTime::from_seconds(AT);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if engine.overlays_for(Some(at)).len() == cues.len() {
                return engine;
            }
            assert!(Instant::now() < deadline, "the cue rasters never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Link 1 and link 2 on one cue: the display list the renderer is handed
    /// repaints to the exact pixels libplacebo was given, at the exact place it
    /// was given them.
    #[test]
    fn the_scene_handed_over_is_the_scene_the_raster_lane_painted() {
        let engine = painted(&[cue("the quick brown fox, 42%", 0, 10)]);
        let overlays = engine.current_overlays();
        let shown = engine.shown_scenes();
        assert_eq!(overlays.len(), 1, "one cue, one overlay");
        assert_eq!(shown.len(), 1, "one cue, one display list");
        let (overlay, shown) = (&overlays[0], &shown[0]);
        assert!(overlay.pixels.len() > 4, "the raster lane painted nothing");
        assert!(
            shown.scene.glyph_count() > 0,
            "the display list carries no glyphs"
        );

        assert_eq!(
            (overlay.x, overlay.y),
            (shown.x, shown.y),
            "the scene lane would place the cue somewhere else on screen"
        );
        assert_eq!(
            (overlay.width, overlay.height),
            (shown.scene.size[0], shown.scene.size[1]),
            "the display list's surface is not the one that was uploaded"
        );

        // The pixels, from the display list the renderer gets. Equal means the
        // switch replaced the painter and nothing upstream of it.
        let repaint = fcast_video::cue_scene::VelloBackend::new()
            .rasterize(&shown.scene, shown.rank)
            .expect("the scene the raster lane painted must repaint");
        assert_eq!(
            (repaint.width, repaint.height, repaint.x, repaint.y),
            (overlay.width, overlay.height, overlay.x, overlay.y),
        );
        assert_eq!(
            repaint.pixels.as_slice(),
            overlay.pixels.as_slice(),
            "the display list handed to the renderer paints different pixels than the \
             raster lane composited, so the two lanes are not showing the same cue"
        );

        // And that display list is what lands on the overlay slot, at the
        // placement the raster lane used.
        let recorder = Recorder::default();
        let mut published = CueOverlay::default();
        assert!(published.sync(&recorder, &engine.shown_scenes()));
        let scene = recorder.last();
        assert_eq!(scene.origin, [overlay.x, overlay.y]);
        assert_eq!(scene.size, [overlay.width, overlay.height]);
        assert_eq!(scene.glyph_count(), shown.scene.glyph_count());
        assert_eq!(scene.rect_count(), shown.scene.rect_count());
    }

    /// Link 2 with the stacking pass in it, which is the half of placement the
    /// scene does not carry: two cues at the same anchor, so the second is
    /// pushed off the first by `stacked_y`.
    #[test]
    fn a_stack_lands_where_the_raster_lane_stacked_it() {
        let engine = painted(&[cue("upper line", 0, 10), cue("lower line", 0, 10)]);
        let overlays = engine.current_overlays();
        let shown = engine.shown_scenes();
        assert_eq!(overlays.len(), 2, "both cues must be on screen");
        assert_eq!(shown.len(), 2);
        for (i, (overlay, shown)) in overlays.iter().zip(shown.iter()).enumerate() {
            assert_eq!(
                (overlay.x, overlay.y),
                (shown.x, shown.y),
                "cue {i} of the stack moved when the lane changed"
            );
            assert_eq!(
                (overlay.width, overlay.height),
                (shown.scene.size[0], shown.scene.size[1]),
                "cue {i} of the stack changed size when the lane changed"
            );
        }
        assert_ne!(
            shown[0].y, shown[1].y,
            "the two cues did not stack, so this proves nothing about stacking"
        );
    }

    /// The switch itself, on the engine: a scene consumer stops paying for
    /// pixels, and the display lists keep coming.
    #[test]
    fn the_switch_stops_the_paint_and_keeps_the_display_list() {
        let engine = painted(&[cue("a line that was painted", 0, 10)]);
        assert!(
            engine.cached_pixels() > 0,
            "the raster lane painted nothing"
        );
        let before = engine.shown_scenes();
        assert_eq!(before.len(), 1);

        engine.set_scene_consumer(true);
        let at = gst::ClockTime::from_seconds(AT);
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            engine.overlays_for(Some(at));
            std::thread::sleep(Duration::from_millis(10));
        }
        let after = engine.shown_scenes();
        assert_eq!(
            after.len(),
            1,
            "the cue left the screen with the paint lane"
        );
        assert!(
            Arc::ptr_eq(&before[0].scene, &after[0].scene),
            "the switch re-laid the cue out instead of reusing the display list"
        );
    }
}

/// The allocation gate for wave 6: a cue on screen must cost the frame path
/// nothing at all.
///
/// Separate from the suite above because it needs the counting allocator
/// `desktop_wgpu_video` installs, and because it is the claim the plan makes
/// (`FRAME_ALLOC_BUDGET` unchanged with cues on screen) rather than a property
/// of the translation.
#[cfg(all(test, not(target_os = "android")))]
mod alloc_gate {
    use std::time::{Duration, Instant};

    use crate::desktop_wgpu_video::alloc_counter::measure;

    use super::{
        tests::{AT, Recorder, cue, one, shown},
        *,
    };

    /// The whole lane-side cue path per frame: advance the schedule, compare
    /// the shown set, publish nothing. Zero, not "small".
    #[test]
    fn a_steady_displayed_cue_costs_the_frame_nothing() {
        let (engine, first) = one("a steady subtitle line");
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let at = gst::ClockTime::from_seconds(AT);
        // settle: the first pass publishes, and the SmallVec/Vec capacities the
        // steady state reuses are grown here
        for _ in 0..4 {
            let shown = engine.scenes_for(Some(at));
            overlay.sync(&recorder, &shown);
        }
        assert_eq!(
            overlay.builds(),
            1,
            "the settle pass rebuilt more than once"
        );
        assert!(!first.scene.glyph_id.is_empty(), "the cue must have glyphs");

        let mut counts = [0u64; 8];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let shown = engine.scenes_for(Some(at));
                overlay.sync(&recorder, &shown);
            });
            *c = n;
        }
        eprintln!("cue path allocations per frame: {counts:?}");
        assert_eq!(
            counts, [0u64; 8],
            "a displayed cue allocates per frame, which is what the scene lane exists to avoid"
        );
        assert_eq!(
            overlay.builds(),
            1,
            "the display list was rebuilt per frame"
        );
    }

    /// The same, with the stack the fork's one-slot overlay has to merge. It
    /// costs a copy when a rank moves and nothing when nothing does.
    #[test]
    fn a_steady_stack_of_cues_costs_the_frame_nothing_either() {
        let (engine, both) = shown(&[cue("upper line", 0, 10), cue("lower line", 0, 10)]);
        assert_eq!(both.len(), 2);
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let at = gst::ClockTime::from_seconds(AT);
        for _ in 0..4 {
            let shown = engine.scenes_for(Some(at));
            overlay.sync(&recorder, &shown);
        }
        let mut counts = [0u64; 8];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let shown = engine.scenes_for(Some(at));
                overlay.sync(&recorder, &shown);
            });
            *c = n;
        }
        eprintln!("stacked cue path allocations per frame: {counts:?}");
        assert_eq!(counts, [0u64; 8], "a stack of cues allocates per frame");
        assert_eq!(overlay.builds(), 1);
    }

    /// The scene lane must not keep the vello_cpu paint alive behind it. Its
    /// pixel cache is the megabyte-per-cue lane the dodvg overlay replaces, and
    /// a paint nobody reads is worker milliseconds on every cue.
    #[test]
    fn the_scene_lane_never_paints_a_raster() {
        let (engine, _shown) = one("no pixels here");
        let at = gst::ClockTime::from_seconds(AT);
        // Give the worker every chance: it feeds itself the next wanted job
        // after every publish, so anything it still wants would land in here.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            engine.scenes_for(Some(at));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            engine.cached_pixels(),
            0,
            "the worker painted rasters for a consumer that draws display lists"
        );
        assert!(
            engine.current_overlays().is_empty(),
            "the overlay lane answered on a scene consumer"
        );
        assert_eq!(
            engine.scene_builds(),
            1,
            "the cue was laid out more than once"
        );
    }
}

/// SUBTITLES ON A LANE THAT HAS NEVER HAD THEM, proved without a window.
///
/// Everything upstream of the renderer is the real thing: the flapjack player
/// owns the pipeline, decodebin3 builds the text branch, the subtitle track is
/// selected the way the app selects it, cues arrive on the branch's own
/// streaming thread through the same consumer `receiver-core`'s `Player::new`
/// installs, the engine lays them out with parley on its own worker, and the
/// display list is translated here into exactly the `Arc` that
/// `DodvgRendererAccess::set_cue_overlay` takes.
///
/// The last hop, the `Arc` becoming pixels, is the fork's own ground
/// (`cues_tests.rs`, wave 4, diffed against the vello_cpu backend at the plan's
/// section 5 thresholds). What is proved here is that it arrives, that it
/// arrives on the frames a delivered cue covers, and that it carries a real cue
/// rather than an empty scene.
///
/// The media is synthesized rather than a file on disk because the receiver's
/// GStreamer is the static PLAYBACK build: it ships no muxer at all
/// (`matroskamux`, `qtmux` and `avimux` are all absent), so a muxed fixture
/// cannot be produced here, and there is no video-plus-subtitle clip in the
/// tree to point `filesrc` at. The scenario driver is the same stand-in
/// `receiver-core`'s own subtitle-delivery suites use.
#[cfg(test)]
mod pipeline_proof {
    use std::{
        sync::{
            Arc as StdArc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use fcast_video::cue::{CueEngine, CueInput, TextFormat};
    use flapjack::{
        AudioSink, MediaInput, Player, PlayerEvent, SelectionGate, Sinks, StartPoint,
        SubtitleFeedItem, SubtitleTrack, TrackSlot, VideoSink,
    };
    use simulator::{
        scenario::ScenarioBuilder,
        sink::FTestSink,
        spec::{CueSpec, Pacing, StreamSpec},
    };
    use gst::prelude::Cast;

    use super::{tests::Recorder, *};

    const CANVAS: (u32, u32) = (1280, 720);
    /// Cues start late enough that the track is selected and its branch built
    /// well before the first one, which is what the delivery assert below
    /// depends on.
    const FIRST_CUE_MS: u64 = 4_000;
    const CUE_MS: u64 = 500;
    const CUES: u64 = 12;

    fn sid_of(sid: &str) -> Option<flapjack::StreamId> {
        sid.strip_prefix("stream#")?
            .parse()
            .ok()
            .map(flapjack::StreamId::from_raw)
    }

    /// No audio ever leaves the process: both sinks are the test sink, which
    /// records and drops.
    #[test]
    fn a_subtitle_track_reaches_the_renderers_overlay_slot() {
        gst::init().unwrap();
        crate::gstreamer::init_and_load_plugins();
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            simulator::register_for_tests();
            let _ = flapjack::audiostretch::plugin_init();
        });

        let cues: Vec<CueSpec> = (0..CUES)
            .map(|i| {
                CueSpec::new(
                    gst::ClockTime::from_mseconds(FIRST_CUE_MS + CUE_MS * 2 * i),
                    gst::ClockTime::from_mseconds(FIRST_CUE_MS + CUE_MS * (2 * i + 1)),
                    format!("subtitle line {i}"),
                )
            })
            .collect();
        let scenario = ScenarioBuilder::new("wgpu_lane_subtitles")
            .stream(StreamSpec::video("video_0").with_pacing(Pacing::Realtime))
            // A fixed hold per item, so the text branch stays alive across the
            // selection while every cue still leads its own running time, which
            // is the shape a real container gives. See the note on the same
            // pacing in receiver-core's contiguous-cue suite.
            .stream(
                StreamSpec::text("text_0", cues).with_pacing(Pacing::Jitter {
                    base_ms: 100,
                    jitter_ms: 0,
                }),
            )
            .duration(gst::ClockTime::from_seconds(30))
            .register();

        let video_sink = FTestSink::new();
        let frames = video_sink.recording();
        let player = StdArc::new(
            Player::new(Sinks {
                video: VideoSink::Element(video_sink.upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
                subtitle: flapjack::SubtitleSink::None,
            })
            .expect("building flapjack"),
        );

        // THE ENGINE THE LANE OWNS, wired exactly as `make_sink` arms it and fed
        // exactly as `receiver-core`'s `Player::new` feeds it.
        let engine = CueEngine::new();
        engine.set_scene_consumer(true);
        engine.set_canvas(CANVAS.0, CANVAS.1);
        engine.warm();

        let delivered: StdArc<Mutex<Vec<(gst::ClockTime, gst::ClockTime)>>> =
            StdArc::new(Mutex::new(Vec::new()));
        let clears = StdArc::new(AtomicUsize::new(0));
        {
            let engine = engine.clone();
            let log = StdArc::clone(&delivered);
            let clear_count = StdArc::clone(&clears);
            player.set_subtitle_consumer(move |item| match item {
                SubtitleFeedItem::Cue {
                    format,
                    text,
                    start_rt,
                    end_rt,
                    ..
                } => {
                    if let Some(end) = end_rt {
                        log.lock().unwrap().push((start_rt, end));
                    }
                    engine.submit(CueInput {
                        format: match format {
                            flapjack::SubtitleTextFormat::Utf8 => TextFormat::Utf8,
                            flapjack::SubtitleTextFormat::PangoMarkup => TextFormat::PangoMarkup,
                            flapjack::SubtitleTextFormat::CueIr { ir, pts_start } => {
                                TextFormat::CueIr { ir, pts_start }
                            }
                        },
                        text,
                        start_rt,
                        end_rt,
                    });
                }
                SubtitleFeedItem::Clear => {
                    log.lock().unwrap().clear();
                    clear_count.fetch_add(1, Ordering::Release);
                    engine.clear();
                }
                _ => {}
            });
        }

        let errors: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));
        let loaded = StdArc::new(AtomicBool::new(false));
        let text_sids: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));
        {
            let errors = StdArc::clone(&errors);
            let loaded = StdArc::clone(&loaded);
            let sids = StdArc::clone(&text_sids);
            player.set_event_handler(None, move |flapjack::Event { kind: event, .. }| match event {
                PlayerEvent::Error { message, .. } => errors.lock().unwrap().push(message),
                PlayerEvent::Loaded { .. } => loaded.store(true, Ordering::Release),
                PlayerEvent::StreamCollection(collection) => {
                    *sids.lock().unwrap() = collection
                        .iter()
                        .filter(|s| s.slot == TrackSlot::Subtitle)
                        .map(|s| s.id.to_string())
                        .collect();
                }
                _ => {}
            });
        }
        let gate = SelectionGate {
            quiet: true,
            paused: false,
            seekable: true,
        };
        let pump = || {
            player.poll_text_policy();
            player.pump_selection(gate);
            assert!(
                errors.lock().unwrap().is_empty(),
                "pipeline error: {:?}",
                errors.lock().unwrap()
            );
        };

        player.load(
            MediaInput::uri(scenario.uri()),
            StartPoint::at(gst::ClockTime::ZERO),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while !loaded.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the load never finished");
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        player.play(player.allocate_op());

        // The subtitle track is off until something asks for it, exactly as in
        // the app.
        let deadline = Instant::now() + Duration::from_secs(30);
        while text_sids.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the text stream was never advertised"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let sid = text_sids.lock().unwrap()[0].clone();
        player.set_subtitle_track(sid_of(&sid).map_or(SubtitleTrack::Off, SubtitleTrack::Stream));

        // Nothing can be claimed until the branch has actually carried cues.
        let deadline = Instant::now() + Duration::from_secs(60);
        while delivered.lock().unwrap().len() < 4 {
            assert!(
                Instant::now() < deadline,
                "no cue ever reached the consumer, so the branch never carried the track"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }

        // THE LANE'S OWN LOOP, per frame, exactly as `Sink::present` runs it:
        // the frame's running time in, the schedule advanced, the display list
        // put on the renderer's slot.
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let last = gst::ClockTime::from_mseconds(FIRST_CUE_MS + CUE_MS * 2 * CUES);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut seen = 0usize;
        // (frame running time, glyphs on the slot, the slot's origin)
        let mut walk: Vec<(gst::ClockTime, usize, [i32; 2])> = Vec::new();
        while Instant::now() < deadline {
            pump();
            let log = frames.snapshot();
            for entry in log[seen.min(log.len())..].iter() {
                let Some(pts) = entry.pts().filter(|_| entry.is_buffer()) else {
                    continue;
                };
                let shown = engine.scenes_for(Some(pts));
                let up = overlay.sync(&recorder, &shown);
                let (glyphs, origin) = match up {
                    true => {
                        let scene = recorder.last();
                        (scene.glyph_count(), scene.origin)
                    }
                    false => (0, [0, 0]),
                };
                walk.push((pts, glyphs, origin));
            }
            seen = log.len();
            if walk.last().is_some_and(|(pts, ..)| *pts >= last) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Read before the shutdown: tearing down flushes the branch, and the
        // flush is a `Clear` that would wipe the record.
        let windows = delivered.lock().unwrap().clone();
        let (tx, rx) = std::sync::mpsc::channel();
        player.shutdown(Box::new(move || {
            let _ = tx.send(());
        }));
        let _ = rx.recv_timeout(Duration::from_secs(30));

        assert!(
            windows.len() >= 4,
            "only {} cues were delivered; too thin to claim anything",
            windows.len()
        );
        // A cue's first 100 ms are exempt: its layout can still be in flight on
        // the worker, which is a latency question and not this one.
        let grace = gst::ClockTime::from_mseconds(100);

        let mut covered = 0usize;
        let mut drawn = 0usize;
        let mut uncovered_with_a_cue = 0usize;
        for (pts, glyphs, origin) in &walk {
            match windows.iter().any(|(s, e)| pts >= s && pts < e) {
                true => {
                    let inside_grace = windows
                        .iter()
                        .any(|(s, e)| pts >= s && pts < e && *pts < *s + grace);
                    if inside_grace {
                        continue;
                    }
                    covered += 1;
                    if *glyphs > 0 {
                        drawn += 1;
                        // The house style puts an unpositioned cue in the lower
                        // part of the picture, and it must be on the canvas.
                        assert!(
                            origin[0] >= 0 && (origin[0] as u32) < CANVAS.0,
                            "the cue landed off the canvas horizontally at {pts}: {origin:?}"
                        );
                        assert!(
                            origin[1] >= 0 && (origin[1] as u32) < CANVAS.1,
                            "the cue landed off the canvas vertically at {pts}: {origin:?}"
                        );
                        assert!(
                            (origin[1] as u32) > CANVAS.1 / 2,
                            "an unpositioned cue is not in the lower half at {pts}: {origin:?}"
                        );
                    }
                }
                // Outside every delivered window nothing may be on the slot.
                false => {
                    if *glyphs > 0 {
                        uncovered_with_a_cue += 1;
                    }
                }
            }
        }

        eprintln!(
            "walked {} frames, {covered} covered by a delivered cue, {drawn} of them with a \
             display list on the renderer's slot",
            walk.len()
        );
        assert!(
            covered >= 8,
            "only {covered} frames of {} fell inside a delivered cue; the run is too thin",
            walk.len()
        );
        // THE CLAIM: a frame a cue covers gets that cue's display list.
        assert!(
            drawn * 10 >= covered * 9,
            "only {drawn} of {covered} covered frames carried a cue on the renderer's slot"
        );
        assert_eq!(
            uncovered_with_a_cue, 0,
            "a cue stayed on the slot past its own window"
        );
        // ...and the lane never asked the engine for a single pixel.
        assert_eq!(engine.cached_pixels(), 0);
    }
}
