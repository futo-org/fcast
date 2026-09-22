//! The cue overlay, driven from the shared sink.
//!
//! The element does not own subtitles and should not: they are the receiver's,
//! they are laid out against its window, and the text half goes on the
//! renderer's overlay slot rather than into the picture. What the element
//! gives is the moment, through `on_presented`, and what it gives with it is
//! the picture that moment belongs to: its running time, its coded size, its
//! transform and its pixel aspect. That is everything the schedule and the
//! placement need, so this drives the same machinery the old lane drives,
//! without a second copy of any of it.
//!
//! One engine, one overlay, both lanes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fcast_video::cue::CueEngine;
use parking_lot::Mutex;
use slint::ComponentHandle;
use slint_gstreamer_video::{BufferTransform, Shown};

/// Everything the overlay keeps between frames, for one window in front of one
/// renderer.
///
/// Shared with the streaming thread through the presenter's closure, so the
/// interior mutability has to be a lock even though only the UI thread ever
/// touches it. Uncontended by construction, which is a compare and swap a
/// frame.
pub(crate) struct ElementCues {
    engine: CueEngine,
    geometry: crate::video_math::CueGeometry,
    overlay: Mutex<crate::cue_overlay::CueOverlay>,
    /// The bitmap subtitle set, which has no display list and so cannot go on
    /// the renderer's overlay slot.
    bitmaps: Mutex<crate::bitmap_overlay::BitmapOverlay>,
    /// The coded video size of the picture that is up, latched, because the
    /// bitmap decoders place their regions in coded pixels.
    coded: AtomicU64,
    /// Whether the picture is drawn exactly as it was coded. A turn swaps the
    /// displayed dimensions and a flip mirrors them, and neither is applied to
    /// a subpicture region, so anything but upright takes the bitmaps down
    /// rather than putting them somewhere wrong.
    upright: AtomicBool,
    /// What the bridge property was last told, so a steady cue does not dirty
    /// a slint property (and everything that reads it) every frame.
    obstructed: AtomicBool,
}

impl ElementCues {
    pub(crate) fn new(engine: CueEngine) -> Self {
        Self {
            engine,
            geometry: crate::video_math::CueGeometry::new(),
            overlay: Default::default(),
            bitmaps: Default::default(),
            coded: AtomicU64::new(0),
            upright: AtomicBool::new(true),
            obstructed: AtomicBool::new(false),
        }
    }

    /// A picture went up. UI thread, right after the setter ran.
    ///
    /// The schedule runs on the frame's own running time rather than the
    /// clock, so a cue lands with the picture it belongs to even when the
    /// pipeline is ahead of or behind the wall clock. The sink resolves that
    /// from its segment; the engine's own conversion is the fallback for a
    /// frame that arrived before any segment did.
    pub(crate) fn presented(&self, ui: &crate::MainWindow, shown: &Shown) {
        let cleared = shown.coded.0 == 0 || shown.coded.1 == 0;
        if cleared {
            self.clear(ui);
            return;
        }
        let quarter_turn = matches!(
            shown.transform,
            BufferTransform::Rotate90 | BufferTransform::Rotate270
        );
        self.upright.store(placeable(shown.transform), Ordering::Relaxed);
        self.note_coded(shown.coded);
        // The picture the cues are anchored against is what the renderer
        // actually draws: square pixels, then the turn.
        let picture = crate::video_math::picture_size(shown.coded, shown.par, quarter_turn);
        let window = ui.window().size();
        self.geometry
            .sync(&self.engine, (window.width, window.height), picture);

        let frame_rt = shown
            .running_time
            .or_else(|| self.engine.video_running_time(shown.pts));
        self.pump(ui, frame_rt);
    }

    /// Re-anchor and re-publish if the window moved. UI thread only, cheap
    /// enough for every render pass.
    ///
    /// The per frame path above is the only place the window and the picture
    /// are both known, and PAUSED there are no frames. Without this a resize
    /// leaves every cue laid out against the window it had before, until
    /// playback resumes.
    pub(crate) fn on_render(&self, ui: &crate::MainWindow) {
        let size = ui.window().size();
        if !self.re_anchor((size.width, size.height)) {
            return;
        }
        // The engine re-keys on the new canvas and keeps the previous display
        // list up meanwhile, so this publishes the cue that is already on
        // screen now and the re-laid-out one when the worker answers.
        self.pump(ui, None);
    }

    /// Take the overlay down, for end of stream and for a lane that has
    /// stopped drawing.
    pub(crate) fn clear(&self, ui: &crate::MainWindow) {
        self.overlay
            .lock()
            .sync(&crate::cue_overlay::WindowCues(ui.window()), &[]);
        self.bitmaps
            .lock()
            .clear(&crate::bitmap_overlay::BridgeBitmaps(ui));
        self.note_obstruction(ui, false);
    }

    /// Advance the schedule and put whatever is showing in front of the
    /// renderer. UI thread only.
    ///
    /// `frame_rt` is the running time of the picture that is up, or `None` for
    /// a repaint with no frame behind it (a cue landing, expiring or being
    /// cleared while paused), which re-evaluates against the frozen clock
    /// exactly as a raster consumer would.
    fn pump(&self, ui: &crate::MainWindow, frame_rt: Option<gst::ClockTime>) {
        // The overlay lives on the renderer, not in the picture, so a player
        // that lost the screen must not leave a cue over the idle view.
        let bridge = ui.global::<crate::Bridge>();
        let owns_screen = bridge.get_app_state() == crate::ui_types::AppState::Playing.into()
            && bridge.get_player_variant() == crate::ui_types::UiPlayerVariant::Video.into();
        let shown = match (owns_screen, frame_rt) {
            (false, _) => Default::default(),
            (true, Some(_)) => self.engine.scenes_for(frame_rt),
            (true, None) => self.engine.current_scenes(),
        };
        let visible = self
            .overlay
            .lock()
            .sync(&crate::cue_overlay::WindowCues(ui.window()), &shown);
        // The bitmap set rides beside the display lists, never instead of
        // them: a source can carry a subpicture track and a text track at once.
        let bitmaps = self.pump_bitmaps(ui, owns_screen);
        self.note_obstruction(ui, visible || bitmaps);
    }

    /// The subpicture half of [`Self::pump`]. UI thread only.
    ///
    /// The schedule was already advanced by the scene read above (both
    /// `scenes_for` and `current_scenes` evaluate the bitmap schedule too), so
    /// this only reads what is showing. Two passes on purpose: the compare runs
    /// under the engine's state lock and the composite does not, because a full
    /// page is megabytes and the subtitle feed thread submits into that lock.
    fn pump_bitmaps(&self, ui: &crate::MainWindow, owns_screen: bool) -> bool {
        let sink = crate::bitmap_overlay::BridgeBitmaps(ui);
        let mut bitmaps = self.bitmaps.lock();
        if !owns_screen {
            return bitmaps.clear(&sink);
        }
        let changed = self
            .engine
            .with_shown_bitmaps(|regions| bitmaps.latch(regions));
        if changed {
            bitmaps.composite();
        }
        // Regions are in CODED video pixels, and the rect they are scaled onto
        // is the one the renderer drew the picture into, so anamorphic content
        // places correctly: the ratio is rect width over coded width either
        // way. A turn or a flip is what cannot be placed, because neither is
        // applied to the region.
        let coded = self.coded();
        let rect = self
            .upright
            .load(Ordering::Relaxed)
            .then(|| crate::video_math::video_rect(self.geometry.picture(), self.geometry.window()))
            .flatten();
        bitmaps.place(&sink, changed, rect, coded, ui.window().scale_factor())
    }

    /// The coded video size, or `(0, 0)` before the first frame.
    fn coded(&self) -> (u32, u32) {
        let word = self.coded.load(Ordering::Relaxed);
        ((word >> 32) as u32, word as u32)
    }

    /// Tell the engine the coded size when it changes.
    fn note_coded(&self, size: (u32, u32)) {
        let word = (u64::from(size.0) << 32) | u64::from(size.1);
        if self.coded.swap(word, Ordering::Relaxed) != word {
            self.engine.set_video_size(size.0, size.1);
        }
    }

    /// Push the new canvas and say whether the window moved at all. Split out
    /// so the geometry half can be graded without a window behind it.
    fn re_anchor(&self, window: (u32, u32)) -> bool {
        // A zero dimension is a mid-create or mid-minimize report, which
        // `CueGeometry::sync` refuses to latch, so answering true for it would
        // re-pump on every pass for as long as the window stays minimized.
        if window.0 == 0 || window.1 == 0 || window == self.geometry.window() {
            return false;
        }
        self.geometry
            .sync(&self.engine, window, self.geometry.picture());
        true
    }

    /// A scene layer overlay is invisible under a video surface stacked above
    /// the GUI, so a cue on screen has to count as an obstruction.
    ///
    /// Only on a change: writing a slint property dirties everything that
    /// reads it, and `video-obstructed` is read by the whole player view.
    fn note_obstruction(&self, ui: &crate::MainWindow, visible: bool) {
        if self.obstructed.swap(visible, Ordering::Relaxed) != visible {
            ui.global::<crate::Bridge>()
                .set_cue_overlay_visible(visible);
        }
    }
}

/// Whether a subpicture region, which is given in coded pixels, can be put on
/// a picture drawn with `transform`.
///
/// Only an untouched picture: a turn swaps the axes and a flip mirrors one,
/// and neither is applied to the region, so anything else would place it
/// somewhere the eye can see is wrong.
fn placeable(transform: BufferTransform) -> bool {
    matches!(transform, BufferTransform::Normal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window moving is what a re-anchor is for, and a minimize is not
    /// that: a zero canvas latches nothing, so answering true for it would
    /// re-pump on every render pass for as long as the window stays down.
    #[test]
    fn re_anchoring_ignores_a_zero_canvas_and_a_still_window() {
        let cues = ElementCues::new(CueEngine::new());
        assert!(!cues.re_anchor((0, 1080)), "a zero width latches nothing");
        assert!(!cues.re_anchor((1920, 0)), "a zero height latches nothing");
        assert!(cues.re_anchor((1920, 1080)), "the first real canvas moves");
        assert!(!cues.re_anchor((1920, 1080)), "the same canvas does not");
        assert!(cues.re_anchor((1280, 720)), "a different one does");
    }

    /// A region given in coded pixels only lands on an untouched picture.
    /// Every turn and every flip has to take the set down instead.
    #[test]
    fn only_an_upright_picture_takes_a_subpicture() {
        assert!(placeable(BufferTransform::Normal));
        for turned in [
            BufferTransform::Rotate90,
            BufferTransform::Rotate180,
            BufferTransform::Rotate270,
            BufferTransform::Flipped,
            BufferTransform::Flipped90,
            BufferTransform::Flipped180,
            BufferTransform::Flipped270,
        ] {
            assert!(!placeable(turned), "{turned:?} must not take a subpicture");
        }
    }

    /// The coded size reaches the engine once per change, not once per frame:
    /// a steady stream must not push the same size at it thirty times a second.
    #[test]
    fn the_coded_size_is_latched() {
        let cues = ElementCues::new(CueEngine::new());
        assert_eq!(cues.coded(), (0, 0));
        cues.note_coded((1920, 1080));
        assert_eq!(cues.coded(), (1920, 1080));
        cues.note_coded((1920, 1080));
        assert_eq!(cues.coded(), (1920, 1080));
        cues.note_coded((1280, 720));
        assert_eq!(cues.coded(), (1280, 720));
    }
}
