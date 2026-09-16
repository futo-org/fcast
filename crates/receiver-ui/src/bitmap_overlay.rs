//! Bitmap subtitles (PGS, VobSub, DVB) on the desktop wgpu lane: the engine's
//! decoded regions, composited into one slint image over the picture.
//!
//! ## Why they are not on the cue overlay slot
//!
//! A text cue reaches the dodvg renderer as a display list and is drawn by the
//! renderer (`cue_overlay.rs`). A subpicture has no display list. It is decoded
//! pixels the moment it leaves `fcast_video::subpic`, which is why
//! `active_scenes` refuses to answer with one and why this lane showed nothing
//! at all for the three bitmap formats. The pixels have to be put in the scene
//! as an image instead, which is what this module does.
//!
//! ## The joint bounding box
//!
//! One image, not one per region. A display set is a handful of regions
//! clustered on the same subtitle line, so their joint bounding box is barely
//! larger than the regions themselves, and one image is one slint property, one
//! element and one pooled buffer instead of a model whose length changes with
//! the stream. The gaps between regions are transparent. A set whose regions
//! are far apart pays for the space between them; PGS windows and DVB regions
//! do not do that, and the cost is bounded by the picture either way, since the
//! decoders clip every region into it.
//!
//! ## What it costs
//!
//! Nothing per video frame. A display set is event driven: it arrives, it sits
//! there for seconds, it goes. The per frame work is [`BitmapOverlay::latch`],
//! which compares the engine's active regions against the composited set by
//! `Arc` pointer and rectangle and returns, and [`BitmapOverlay::place`], which
//! recomputes the picture-space rectangle and returns without touching a slint
//! property when it has not moved. The composite runs on change only, and
//! [`BitmapOverlay::builds`] counts it.
//!
//! ## Pooling
//!
//! Two pixel buffers, alternated, the same trick the readback presenter uses.
//! The bridge property holds the one last handed over, so the other is at
//! refcount one and `make_mut_bytes` rewrites it in place rather than copying a
//! full page first.

use std::sync::Arc;

use fcast_video::{cue_ir::VideoRect, subpic::BitmapRegion};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};

/// Canvas edge the composite refuses to go past. The decoders clip regions into
/// the coded picture, so this is only ever reached by an 8K source, and a
/// subtitle scaled down by a few percent beats an allocation nobody bounded.
const MAX_CANVAS: u32 = 4096;

/// Where a composited set goes.
///
/// A trait rather than the window directly so the compositing and the placement
/// can be graded headlessly, without a backend, an event loop or a GPU. The
/// production implementation is [`BridgeBitmaps`] and it is five lines.
pub(crate) trait BitmapSink {
    /// Place the image at `rect`, in logical pixels relative to the window
    /// origin.
    fn set_overlay(&self, image: slint::Image, rect: [f32; 4]);
    /// Take whatever is up down.
    fn clear_overlay(&self);
}

/// The bridge property behind a real window.
pub(crate) struct BridgeBitmaps<'a>(pub(crate) &'a crate::MainWindow);

impl BitmapSink for BridgeBitmaps<'_> {
    fn set_overlay(&self, image: slint::Image, rect: [f32; 4]) {
        self.0
            .global::<crate::Bridge>()
            .set_bitmap_subtitle(crate::SubtitleOverlay {
                img: image,
                x: rect[0],
                y: rect[1],
                w: rect[2],
                h: rect[3],
            });
    }

    fn clear_overlay(&self) {
        self.0
            .global::<crate::Bridge>()
            .set_bitmap_subtitle(crate::SubtitleOverlay::default());
    }
}

/// The composited set's bounding box, in coded video pixels.
#[derive(Clone, Copy, PartialEq)]
struct Bbox {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// Same predicate the engine's own no-op adoption check uses
/// (`fcast_video::cue::same_update`): the pixels by pointer, the geometry by
/// value. The `Arc` is held by [`BitmapOverlay::set`] for exactly as long as
/// the address is compared, so a recycled allocation cannot answer for a
/// different region.
fn same_region(a: &BitmapRegion, b: &BitmapRegion) -> bool {
    Arc::ptr_eq(&a.pixels, &b.pixels)
        && a.x == b.x
        && a.y == b.y
        && a.width == b.width
        && a.height == b.height
        && a.render_width == b.render_width
        && a.render_height == b.render_height
}

/// A region with nothing to draw. The decoders clamp their rectangles to at
/// least one pixel, so this is a malformed stream rather than a normal case.
fn is_empty(region: &BitmapRegion) -> bool {
    region.width == 0
        || region.height == 0
        || region.render_width == 0
        || region.render_height == 0
        || region.pixels.len() < (region.width as usize * region.height as usize * 4)
}

pub(crate) struct BitmapOverlay {
    /// The composited set, held so the pointer comparison in [`Self::latch`] is
    /// sound and so the composite can run after the engine lock is released.
    set: Vec<BitmapRegion>,
    /// Bounding box of `set` in coded video pixels, `None` when nothing is up.
    bbox: Option<Bbox>,
    /// Ping-pong pair. `live` is the one the bridge holds, the other is at
    /// refcount one and is the one [`Self::composite`] writes.
    pool: [SharedPixelBuffer<Rgba8Pixel>; 2],
    live: usize,
    /// What the sink was last told, so a steady set does not dirty a slint
    /// property (and everything that reads it) on every frame.
    placed: Option<[f32; 4]>,
    builds: u64,
}

impl Default for BitmapOverlay {
    fn default() -> Self {
        Self {
            set: Vec::new(),
            bbox: None,
            // One pixel each: the first composite resizes them, and an empty
            // SharedPixelBuffer is not constructible.
            pool: [SharedPixelBuffer::new(1, 1), SharedPixelBuffer::new(1, 1)],
            live: 0,
            placed: None,
            builds: 0,
        }
    }
}

impl BitmapOverlay {
    /// Adopt `regions` when they differ from what is composited, and answer
    /// whether they did.
    ///
    /// Cheap by construction: this is what runs under the engine's state lock,
    /// once per frame, and on the frames that matter (all of them but a
    /// handful) it is a length compare and a pointer compare per region.
    /// The composite is deliberately NOT done here.
    pub(crate) fn latch(&mut self, regions: &[BitmapRegion]) -> bool {
        if self.set.len() == regions.len()
            && self
                .set
                .iter()
                .zip(regions.iter())
                .all(|(a, b)| same_region(a, b))
        {
            return false;
        }
        // `clear` then `extend` rather than a fresh Vec: the capacity is kept,
        // so a stream whose sets are the same shape stops allocating after the
        // first one. Cloning a region is an `Arc` bump and six words.
        //
        // Adopted whole, undrawable regions included: the comparison above is
        // against what the engine answers with, and filtering here would make a
        // set carrying one of them differ from itself on every frame.
        self.set.clear();
        self.set.extend_from_slice(regions);
        true
    }

    /// Paint the latched set into the spare pool buffer. Call after
    /// [`Self::latch`] answered `true`, with no engine lock held.
    pub(crate) fn composite(&mut self) {
        self.bbox = None;
        let Some(bbox) = bounding_box(&self.set) else {
            return;
        };
        // Enough resolution that no region is drawn below its own texture, so
        // the common single-region set is a straight row copy.
        let (sx, sy) = canvas_scale(&self.set);
        let width = scaled(bbox.width, sx);
        let height = scaled(bbox.height, sy);
        // The clamp in `scaled` may have shrunk the canvas below what the
        // scale asked for. Place the regions against what it actually is, or a
        // clamped set would lose the ones furthest from the origin.
        let sx = width as f32 / bbox.width as f32;
        let sy = height as f32 / bbox.height as f32;

        let slot = &mut self.pool[self.live ^ 1];
        if slot.width() != width || slot.height() != height {
            *slot = SharedPixelBuffer::new(width, height);
        }
        let canvas = slot.make_mut_bytes();
        // The gaps between regions are transparent, and so is everything
        // outside them.
        canvas.fill(0);
        for region in self.set.iter().filter(|r| !is_empty(r)) {
            blit(canvas, width, height, region, &bbox, sx, sy);
        }
        self.bbox = Some(bbox);
        self.live ^= 1;
        self.builds += 1;
    }

    /// Map the composited box onto the letterboxed picture and publish it.
    /// Answers whether a bitmap subtitle is on screen.
    ///
    /// `video` is the CODED size the regions were scaled onto
    /// (`CueEngine::set_video_size`) and `rect` is where that picture landed in
    /// the window, both in physical pixels; `scale` turns the result into the
    /// logical pixels a slint length is in.
    ///
    /// `rebuilt` forces the push even when the rectangle did not move, because
    /// the image behind it changed.
    pub(crate) fn place(
        &mut self,
        sink: &dyn BitmapSink,
        rebuilt: bool,
        rect: Option<VideoRect>,
        video: (u32, u32),
        scale: f32,
    ) -> bool {
        let (Some(bbox), Some(rect)) = (self.bbox, rect) else {
            return self.take_down(sink);
        };
        if video.0 == 0 || video.1 == 0 || scale <= 0.0 {
            return self.take_down(sink);
        }
        let sx = rect.width as f32 / video.0 as f32;
        let sy = rect.height as f32 / video.1 as f32;
        let placed = [
            (rect.x as f32 + bbox.x as f32 * sx) / scale,
            (rect.y as f32 + bbox.y as f32 * sy) / scale,
            (bbox.width as f32 * sx) / scale,
            (bbox.height as f32 * sy) / scale,
        ];
        if !rebuilt && self.placed == Some(placed) {
            return true;
        }
        // Built here rather than held: `slint::Image` is not Send and this
        // struct is shared with the streaming thread. The buffer is, and the
        // wrap is a refcount bump over the pool's storage.
        let image = slint::Image::from_rgba8(self.pool[self.live].clone());
        sink.set_overlay(image, placed);
        self.placed = Some(placed);
        true
    }

    /// Forget the set and take the overlay down. The stop, track-switch and
    /// lost-the-screen path.
    pub(crate) fn clear(&mut self, sink: &dyn BitmapSink) -> bool {
        self.set.clear();
        self.bbox = None;
        self.take_down(sink)
    }

    fn take_down(&mut self, sink: &dyn BitmapSink) -> bool {
        if self.placed.is_some() {
            self.placed = None;
            sink.clear_overlay();
        }
        false
    }

    /// Composites since construction. The allocation gate asserts on it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn builds(&self) -> u64 {
        self.builds
    }

    /// The canvas the last composite painted, for the tests that read pixels.
    #[cfg(test)]
    pub(crate) fn canvas(&self) -> (u32, u32, &[u8]) {
        let slot = &self.pool[self.live];
        (slot.width(), slot.height(), slot.as_bytes())
    }
}

/// The box every drawable region fits in, or `None` when there are none.
fn bounding_box(regions: &[BitmapRegion]) -> Option<Bbox> {
    let mut x0 = i32::MAX;
    let mut y0 = i32::MAX;
    let mut x1 = i32::MIN;
    let mut y1 = i32::MIN;
    let mut any = false;
    for region in regions.iter().filter(|r| !is_empty(r)) {
        any = true;
        x0 = x0.min(region.x);
        y0 = y0.min(region.y);
        x1 = x1.max(region.x.saturating_add(region.render_width as i32));
        y1 = y1.max(region.y.saturating_add(region.render_height as i32));
    }
    any.then(|| Bbox {
        x: x0,
        y: y0,
        width: (x1 - x0).max(1) as u32,
        height: (y1 - y0).max(1) as u32,
    })
}

/// Canvas pixels per video pixel, picked so the region with the finest texture
/// is drawn at its own resolution and nothing is downsampled twice.
fn canvas_scale(regions: &[BitmapRegion]) -> (f32, f32) {
    let mut sx: f32 = 0.0;
    let mut sy: f32 = 0.0;
    for region in regions.iter().filter(|r| !is_empty(r)) {
        sx = sx.max(region.width as f32 / region.render_width as f32);
        sy = sy.max(region.height as f32 / region.render_height as f32);
    }
    (sx.max(f32::MIN_POSITIVE), sy.max(f32::MIN_POSITIVE))
}

fn scaled(extent: u32, scale: f32) -> u32 {
    ((extent as f32 * scale).round() as u32).clamp(1, MAX_CANVAS)
}

/// Draw one region into the canvas, nearest neighbour, source over.
///
/// The single-region case lands on `dw == region.width` by construction (see
/// [`canvas_scale`]) and takes the row copy, so the common set is composited at
/// memcpy speed and resampled exactly once, by slint, on the way to the screen.
fn blit(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    region: &BitmapRegion,
    bbox: &Bbox,
    sx: f32,
    sy: f32,
) {
    let dx = ((region.x - bbox.x) as f32 * sx).round() as i64;
    let dy = ((region.y - bbox.y) as f32 * sy).round() as i64;
    if dx < 0 || dy < 0 {
        return;
    }
    let (dx, dy) = (dx as u32, dy as u32);
    if dx >= canvas_w || dy >= canvas_h {
        return;
    }
    let dw = scaled(region.render_width, sx).min(canvas_w - dx);
    let dh = scaled(region.render_height, sy).min(canvas_h - dy);
    let one_to_one = dw == region.width && dh == region.height;
    let src = &region.pixels[..region.width as usize * region.height as usize * 4];

    for row in 0..dh {
        let sr = match one_to_one {
            true => row,
            // Integer nearest neighbour: no float per pixel, and the last row
            // of the destination maps to the last row of the source.
            false => (row as u64 * region.height as u64 / dh as u64) as u32,
        };
        let src_row = &src[sr as usize * region.width as usize * 4..][..region.width as usize * 4];
        let dst_start = ((dy + row) as usize * canvas_w as usize + dx as usize) * 4;
        let dst_row = &mut canvas[dst_start..dst_start + dw as usize * 4];
        if one_to_one {
            dst_row.copy_from_slice(src_row);
            continue;
        }
        for col in 0..dw as usize {
            let sc = (col as u64 * region.width as u64 / dw as u64) as usize;
            let s = &src_row[sc * 4..sc * 4 + 4];
            let d = &mut dst_row[col * 4..col * 4 + 4];
            over(d, s);
        }
    }
}

/// Source over in STRAIGHT alpha, which is the space the decoders hand pixels
/// out in and the space slint takes them in.
///
/// The canvas starts transparent and regions in a display set do not overlap,
/// so the first arm is what runs. The blend is there so a stream that does
/// overlap them gets a soft edge rather than a hole punched by the later
/// region's transparent border.
fn over(dst: &mut [u8], src: &[u8]) {
    if dst[3] == 0 {
        dst.copy_from_slice(src);
        return;
    }
    if src[3] == 0 {
        return;
    }
    let sa = src[3] as u32;
    let da = dst[3] as u32;
    let out_a = sa * 255 + da * (255 - sa);
    for c in 0..3 {
        let num = src[c] as u32 * sa * 255 + dst[c] as u32 * da * (255 - sa);
        dst[c] = (num / out_a) as u8;
    }
    dst[3] = (out_a / 255) as u8;
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records what the bridge would have been told.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub(crate) last: Mutex<Option<([f32; 4], (u32, u32))>>,
        pub(crate) sets: Mutex<u32>,
        pub(crate) clears: Mutex<u32>,
    }

    impl BitmapSink for Recorder {
        fn set_overlay(&self, image: slint::Image, rect: [f32; 4]) {
            let size = image.size();
            *self.last.lock().unwrap() = Some((rect, (size.width, size.height)));
            *self.sets.lock().unwrap() += 1;
        }

        fn clear_overlay(&self) {
            *self.last.lock().unwrap() = None;
            *self.clears.lock().unwrap() += 1;
        }
    }

    /// One opaque region of `tag` in every channel.
    pub(crate) fn region(
        tag: u8,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        render: (u32, u32),
    ) -> BitmapRegion {
        let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..w * h {
            pixels.extend_from_slice(&[tag, tag, tag, 255]);
        }
        BitmapRegion {
            pixels: Arc::new(pixels),
            width: w,
            height: h,
            x,
            y,
            render_width: render.0,
            render_height: render.1,
        }
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> VideoRect {
        VideoRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_single_region_is_composited_at_its_own_resolution() {
        let mut overlay = BitmapOverlay::default();
        let set = [region(0x40, 100, 900, 4, 2, (4, 2))];
        assert!(overlay.latch(&set), "a first set must be adopted");
        overlay.composite();
        let (w, h, pixels) = overlay.canvas();
        assert_eq!((w, h), (4, 2), "the canvas is not the region's own texture");
        assert!(
            pixels.chunks_exact(4).all(|p| p == [0x40, 0x40, 0x40, 255]),
            "the region's pixels did not survive the copy"
        );
    }

    #[test]
    fn two_regions_share_a_box_with_a_transparent_gap() {
        let mut overlay = BitmapOverlay::default();
        let set = [
            region(0x10, 0, 0, 2, 2, (2, 2)),
            region(0x20, 6, 0, 2, 2, (2, 2)),
        ];
        assert!(overlay.latch(&set));
        overlay.composite();
        let (w, h, pixels) = overlay.canvas();
        assert_eq!((w, h), (8, 2), "the joint bounding box is wrong");
        let at = |x: usize, y: usize| &pixels[(y * w as usize + x) * 4..][..4];
        assert_eq!(at(0, 0), [0x10, 0x10, 0x10, 255], "the left region moved");
        assert_eq!(at(6, 0), [0x20, 0x20, 0x20, 255], "the right region moved");
        assert_eq!(
            at(3, 0)[3],
            0,
            "the gap between the regions is not transparent"
        );
    }

    /// The decoders leave a texture at its native size and scale only the
    /// render rectangle, so a region drawn larger than its texture is the
    /// normal case for a downscaled picture.
    #[test]
    fn a_region_drawn_larger_than_its_texture_fills_its_rect() {
        let mut overlay = BitmapOverlay::default();
        let set = [region(0x30, 10, 10, 2, 2, (8, 4))];
        assert!(overlay.latch(&set));
        overlay.composite();
        let (w, h, pixels) = overlay.canvas();
        assert_eq!(
            (w, h),
            (2, 2),
            "the canvas resolution should follow the texture, not the render rect"
        );
        assert!(pixels.chunks_exact(4).all(|p| p[3] == 255));

        let recorder = Recorder::default();
        // A 640x360 picture at the window origin, regions authored on 1280x720.
        let up = overlay.place(
            &recorder,
            true,
            Some(rect(0, 0, 640, 360)),
            (1280, 720),
            1.0,
        );
        assert!(up, "the overlay is not on screen");
        let (placed, size) = recorder.last.lock().unwrap().expect("nothing was pushed");
        assert_eq!(size, (2, 2), "the pushed image is not the composite");
        // The render rect is 8x4 video px at (10,10), halved by the letterbox.
        assert_eq!(placed, [5.0, 5.0, 4.0, 2.0], "the rect landed wrong");
    }

    #[test]
    fn an_unchanged_set_is_neither_recomposited_nor_republished() {
        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let set = [region(0x50, 4, 4, 4, 4, (4, 4))];
        let view = rect(0, 0, 100, 100);

        assert!(overlay.latch(&set));
        overlay.composite();
        assert!(overlay.place(&recorder, true, Some(view), (100, 100), 1.0));
        assert_eq!(overlay.builds(), 1);
        assert_eq!(*recorder.sets.lock().unwrap(), 1);

        for _ in 0..16 {
            assert!(!overlay.latch(&set), "an unchanged set reported a change");
            assert!(overlay.place(&recorder, false, Some(view), (100, 100), 1.0));
        }
        assert_eq!(overlay.builds(), 1, "a still set was recomposited");
        assert_eq!(
            *recorder.sets.lock().unwrap(),
            1,
            "a still set dirtied the bridge property"
        );

        // A DIFFERENT set with the same shape: same length, same rectangles,
        // different pixels. Only the pointer tells them apart.
        let next = [region(0x60, 4, 4, 4, 4, (4, 4))];
        assert!(
            overlay.latch(&next),
            "a new set with the same geometry was mistaken for the old one"
        );
        overlay.composite();
        assert!(overlay.place(&recorder, true, Some(view), (100, 100), 1.0));
        assert_eq!(overlay.builds(), 2);
        assert_eq!(*recorder.sets.lock().unwrap(), 2);
    }

    #[test]
    fn a_resize_moves_the_rect_without_recompositing() {
        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let set = [region(0x70, 10, 10, 4, 4, (4, 4))];
        assert!(overlay.latch(&set));
        overlay.composite();
        overlay.place(&recorder, true, Some(rect(0, 0, 100, 100)), (100, 100), 1.0);
        let first = recorder.last.lock().unwrap().expect("nothing pushed").0;

        overlay.place(
            &recorder,
            false,
            Some(rect(20, 0, 200, 200)),
            (100, 100),
            1.0,
        );
        let second = recorder.last.lock().unwrap().expect("nothing pushed").0;
        assert_ne!(first, second, "the rect did not follow the picture");
        assert_eq!(second, [40.0, 20.0, 8.0, 8.0]);
        assert_eq!(overlay.builds(), 1, "a resize recomposited the pixels");
    }

    #[test]
    fn an_empty_set_takes_the_overlay_down_once() {
        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let set = [region(0x80, 0, 0, 4, 4, (4, 4))];
        assert!(overlay.latch(&set));
        overlay.composite();
        overlay.place(&recorder, true, Some(rect(0, 0, 100, 100)), (100, 100), 1.0);

        assert!(overlay.latch(&[]), "the clear was not seen");
        overlay.composite();
        assert!(!overlay.place(&recorder, true, Some(rect(0, 0, 100, 100)), (100, 100), 1.0));
        assert!(recorder.last.lock().unwrap().is_none());
        assert_eq!(*recorder.clears.lock().unwrap(), 1);

        // ...and it stays down without saying so again.
        for _ in 0..8 {
            assert!(!overlay.latch(&[]));
            assert!(!overlay.place(
                &recorder,
                false,
                Some(rect(0, 0, 100, 100)),
                (100, 100),
                1.0
            ));
        }
        assert_eq!(
            *recorder.clears.lock().unwrap(),
            1,
            "the take-down repeated"
        );
    }

    #[test]
    fn losing_the_picture_takes_the_overlay_down() {
        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let set = [region(0x90, 0, 0, 4, 4, (4, 4))];
        overlay.latch(&set);
        overlay.composite();
        overlay.place(&recorder, true, Some(rect(0, 0, 100, 100)), (100, 100), 1.0);
        assert!(recorder.last.lock().unwrap().is_some());

        // No picture rect: the window has no size yet, or the caps never landed.
        assert!(!overlay.place(&recorder, false, None, (100, 100), 1.0));
        assert!(recorder.last.lock().unwrap().is_none());

        overlay.clear(&recorder);
        assert!(overlay.latch(&set), "clear must forget the set");
    }

    /// The pool is what keeps a rebuild from allocating, and it only works if
    /// the buffer the bridge is NOT holding is the one written.
    #[test]
    fn the_pool_alternates_so_the_live_buffer_is_never_rewritten() {
        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let view = rect(0, 0, 100, 100);
        let mut seen = Vec::new();
        for tag in 0..4u8 {
            let set = [region(0x10 * (tag + 1), 0, 0, 4, 4, (4, 4))];
            assert!(overlay.latch(&set));
            overlay.composite();
            overlay.place(&recorder, true, Some(view), (100, 100), 1.0);
            let (_, _, pixels) = overlay.canvas();
            assert_eq!(
                pixels[0],
                0x10 * (tag + 1),
                "the live buffer is the wrong one"
            );
            seen.push(overlay.live);
        }
        assert_eq!(seen, vec![1, 0, 1, 0], "the pool did not alternate");
    }

    /// THE PER FRAME CLAIM: a display set that is not moving costs the frame
    /// path nothing at all, and one that moves is composited exactly once.
    ///
    /// The counting allocator lives in `desktop_wgpu_video`; this borrows it,
    /// the way `cue_overlay`'s own gate does.
    #[test]
    fn a_still_bitmap_set_allocates_nothing_per_frame() {
        use crate::desktop_wgpu_video::alloc_counter::measure;

        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let view = rect(0, 0, 1280, 720);
        // Two regions, so the set exercises the joint box rather than the
        // single-region shortcut.
        let set = [
            region(0x11, 100, 900, 200, 40, (200, 40)),
            region(0x22, 400, 900, 200, 40, (200, 40)),
        ];

        // Settle: the first pass grows the tracking vector and the pool.
        for _ in 0..4 {
            let changed = overlay.latch(&set);
            if changed {
                overlay.composite();
            }
            overlay.place(&recorder, changed, Some(view), (1920, 1080), 1.0);
        }
        assert_eq!(overlay.builds(), 1, "the set was composited more than once");

        let mut counts = [0u64; 8];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let changed = overlay.latch(&set);
                overlay.place(&recorder, changed, Some(view), (1920, 1080), 1.0);
            });
            *c = n;
        }
        eprintln!("allocations per frame with a bitmap set up: {counts:?}");
        assert_eq!(
            counts, [0u64; 8],
            "a still bitmap set allocates per frame, which is what the identity compare exists \
             to avoid"
        );
        assert_eq!(overlay.builds(), 1, "a still set was recomposited");
        assert_eq!(
            *recorder.sets.lock().unwrap(),
            1,
            "a still set dirtied the bridge property"
        );

        // ...and a set that DOES move is rebuilt, once, and published once.
        let next = [
            region(0x33, 100, 900, 200, 40, (200, 40)),
            region(0x44, 400, 900, 200, 40, (200, 40)),
        ];
        assert!(overlay.latch(&next), "the new set was not seen");
        overlay.composite();
        overlay.place(&recorder, true, Some(view), (1920, 1080), 1.0);
        assert_eq!(
            overlay.builds(),
            2,
            "the changed set was not composited once"
        );
        assert_eq!(*recorder.sets.lock().unwrap(), 2);
        let (_, _, pixels) = overlay.canvas();
        assert_eq!(pixels[0], 0x33, "the canvas still holds the previous set");

        // The rebuild reused the pool: same dimensions, so nothing was
        // reallocated for it.
        let mut steady = [0u64; 4];
        for c in steady.iter_mut() {
            let (_, n) = measure(|| {
                let changed = overlay.latch(&next);
                overlay.place(&recorder, changed, Some(view), (1920, 1080), 1.0);
            });
            *c = n;
        }
        assert_eq!(steady, [0u64; 4], "the lane did not settle after a change");
    }

    /// A region with nothing in it is skipped by the composite but STAYS in the
    /// latched set, or a set carrying one would differ from itself on every
    /// frame and rebuild forever.
    #[test]
    fn an_undrawable_region_is_skipped_without_making_the_set_unstable() {
        use crate::desktop_wgpu_video::alloc_counter::measure;

        let mut overlay = BitmapOverlay::default();
        let recorder = Recorder::default();
        let view = rect(0, 0, 100, 100);
        let mut broken = region(0x99, 50, 50, 4, 4, (4, 4));
        broken.render_width = 0;
        let set = [region(0xAA, 0, 0, 4, 4, (4, 4)), broken];

        assert!(overlay.latch(&set));
        overlay.composite();
        assert!(overlay.place(&recorder, true, Some(view), (100, 100), 1.0));
        let (w, h, _) = overlay.canvas();
        assert_eq!((w, h), (4, 4), "the undrawable region widened the box");

        for _ in 0..4 {
            let changed = overlay.latch(&set);
            overlay.place(&recorder, changed, Some(view), (100, 100), 1.0);
        }
        let (_, allocations) = measure(|| {
            for _ in 0..4 {
                let changed = overlay.latch(&set);
                assert!(!changed, "a set with an undrawable region never settles");
                overlay.place(&recorder, changed, Some(view), (100, 100), 1.0);
            }
        });
        assert_eq!(allocations, 0);
        assert_eq!(overlay.builds(), 1);

        // ...and a set with nothing drawable in it at all is a take-down.
        let mut only_broken = region(0xBB, 0, 0, 4, 4, (4, 4));
        only_broken.render_height = 0;
        assert!(overlay.latch(&[only_broken]));
        overlay.composite();
        assert!(!overlay.place(&recorder, true, Some(view), (100, 100), 1.0));
        assert!(recorder.last.lock().unwrap().is_none());
    }

    /// Straight-alpha source over, on the path a malformed stream can reach.
    #[test]
    fn overlapping_regions_blend_instead_of_punching_a_hole() {
        let mut dst = [200u8, 0, 0, 255];
        over(&mut dst, &[0, 0, 200, 0]);
        assert_eq!(
            dst,
            [200, 0, 0, 255],
            "a transparent source erased the target"
        );
        let mut dst = [200u8, 0, 0, 255];
        over(&mut dst, &[0, 0, 200, 255]);
        assert_eq!(dst, [0, 0, 200, 255], "an opaque source did not win");
        let mut dst = [0u8, 0, 0, 0];
        over(&mut dst, &[9, 9, 9, 128]);
        assert_eq!(
            dst,
            [9, 9, 9, 128],
            "the empty-target fast path is not a copy"
        );
    }
}

/// The end-to-end proof: a real PGS track, through the real pipeline, the real
/// driver and the real decoder, ends up as a nonempty image on the bridge at
/// the right place and at the right time, and is gone afterwards.
///
/// The bitmap sibling of `cue_overlay`'s `pipeline_proof`, and it exists for
/// the same reason: every unit above drives the seam with regions somebody
/// wrote by hand, and none of them can tell whether the lane is wired to the
/// stream at all. This one carries no fixture files and never opens a movie;
/// the scenario source synthesizes the container and the PGS display sets.
///
/// No audio ever leaves the process: both sinks are the test sink, which
/// records and drops.
#[cfg(test)]
mod pipeline_proof {
    use std::{
        sync::{
            Arc as StdArc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use fcast_video::{
        cue::CueEngine,
        subpic::{BitmapFormat, BitmapPacket},
    };
    use flapjack::{
        AudioSink, MediaInput, Player, PlayerEvent, SelectionGate, Sinks, StartPoint,
        SubtitleFeedItem, SubtitleTrack, TrackSlot, VideoSink,
    };
    use simulator::{
        caps as tcaps,
        scenario::ScenarioBuilder,
        sink::FTestSink,
        spec::{CueSpec, Pacing, StreamSpec},
    };
    use gst::prelude::Cast;

    use super::{tests::Recorder, *};

    /// The window the lane letterboxes into.
    const WINDOW: (u32, u32) = (1280, 720);
    /// The canvas the simulator's display sets are authored on.
    const CODED: (u32, u32) = (1920, 1080);
    /// Sets start late enough that the track is selected and its branch built
    /// well before the first one.
    const FIRST_MS: u64 = 4_000;
    /// One set every second, each taken down half a second later by the
    /// format's own empty composition.
    const STEP_MS: u64 = 1_000;
    const SETS: u64 = 10;

    fn sid_of(sid: &str) -> Option<flapjack::StreamId> {
        sid.strip_prefix("stream#")?
            .parse()
            .ok()
            .map(flapjack::StreamId::from_raw)
    }

    /// A display set, then the empty composition that takes it down. Both are
    /// real PGS framing; the driver forwards them untouched and the engine's
    /// own decoder turns them into regions.
    fn display_sets() -> Vec<CueSpec> {
        let mut cues = Vec::new();
        for i in 0..SETS {
            let up = gst::ClockTime::from_mseconds(FIRST_MS + STEP_MS * i);
            let down = up + gst::ClockTime::from_mseconds(STEP_MS / 2);
            cues.push(CueSpec::packets(
                up,
                down,
                vec![simulator::pgs::display_set(i as u8)],
            ));
            cues.push(CueSpec::packets(
                down,
                down + gst::ClockTime::from_mseconds(10),
                vec![simulator::pgs::clear_set()],
            ));
        }
        cues
    }

    #[test]
    fn a_bitmap_subtitle_track_reaches_the_bridge_image() {
        gst::init().unwrap();
        crate::gstreamer::init_and_load_plugins();
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            simulator::register_for_tests();
            let _ = flapjack::audiostretch::plugin_init();
        });

        let scenario = ScenarioBuilder::new("wgpu_lane_bitmap_subtitles")
            .stream(StreamSpec::video("video_0").with_pacing(Pacing::Realtime))
            .stream(
                StreamSpec::text("text_0", display_sets())
                    .with_caps(tcaps::pgs_caps())
                    .with_pacing(Pacing::Jitter {
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

        // THE ENGINE THE LANE OWNS, wired exactly as `make_sink` arms it, with
        // the coded size `Sink::present` latches off the caps plan.
        let engine = CueEngine::new();
        engine.set_scene_consumer(true);
        engine.set_canvas(WINDOW.0, WINDOW.1);
        engine.set_video_size(CODED.0, CODED.1);

        let packets = StdArc::new(AtomicUsize::new(0));
        let clears = StdArc::new(AtomicUsize::new(0));
        {
            let engine = engine.clone();
            let seen = StdArc::clone(&packets);
            let cleared = StdArc::clone(&clears);
            // The receiver's own consumer, arm for arm (`receiver-core`'s
            // `Player::new`).
            player.set_subtitle_consumer(move |item| match item {
                SubtitleFeedItem::Bitmap {
                    format,
                    data,
                    codec_data,
                    rt,
                    duration,
                } => {
                    seen.fetch_add(1, Ordering::Release);
                    engine.submit_bitmap(BitmapPacket {
                        format: match format {
                            flapjack::BitmapSubFormat::Pgs => BitmapFormat::Pgs,
                            flapjack::BitmapSubFormat::Vobsub => BitmapFormat::Vobsub,
                            flapjack::BitmapSubFormat::Dvb => BitmapFormat::Dvb,
                        },
                        data,
                        codec_data,
                        rt,
                        duration,
                    });
                }
                SubtitleFeedItem::Clear => {
                    cleared.fetch_add(1, Ordering::Release);
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

        let deadline = Instant::now() + Duration::from_secs(30);
        while text_sids.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the subpicture stream was never advertised"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let sid = text_sids.lock().unwrap()[0].clone();
        player.set_subtitle_track(sid_of(&sid).map_or(SubtitleTrack::Off, SubtitleTrack::Stream));

        let deadline = Instant::now() + Duration::from_secs(60);
        while packets.load(Ordering::Acquire) < 4 {
            assert!(
                Instant::now() < deadline,
                "no packet ever reached the consumer, so the branch never carried the track"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }

        // THE LANE'S OWN LOOP, per frame, exactly as `Cues::pump_bitmaps` runs
        // it: the schedule advanced by the scene read, the active set compared,
        // composited on change, placed against the letterboxed picture.
        let recorder = Recorder::default();
        let mut overlay = BitmapOverlay::default();
        let rect = crate::video_math::video_rect(CODED, WINDOW);
        let last = gst::ClockTime::from_mseconds(FIRST_MS + STEP_MS * SETS);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut seen = 0usize;
        // (frame running time, whether a subtitle was on the bridge, its rect)
        let mut walk: Vec<(gst::ClockTime, bool, [f32; 4])> = Vec::new();
        while Instant::now() < deadline {
            pump();
            let log = frames.snapshot();
            for entry in log[seen.min(log.len())..].iter() {
                let Some(pts) = entry.pts().filter(|_| entry.is_buffer()) else {
                    continue;
                };
                engine.scenes_for(Some(pts));
                let changed = engine.with_shown_bitmaps(|regions| overlay.latch(regions));
                if changed {
                    overlay.composite();
                }
                let up = overlay.place(&recorder, changed, rect, CODED, 1.0);
                let placed = recorder
                    .last
                    .lock()
                    .unwrap()
                    .map(|(rect, _)| rect)
                    .unwrap_or_default();
                walk.push((pts, up, placed));
            }
            seen = log.len();
            if walk.last().is_some_and(|(pts, ..)| *pts >= last) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // The canvas as the last composite left it, read before the shutdown
        // flushes the branch.
        let (canvas_w, canvas_h, ink) = {
            let (w, h, pixels) = overlay.canvas();
            (w, h, pixels.chunks_exact(4).filter(|p| p[3] > 0).count())
        };
        let delivered = packets.load(Ordering::Acquire);
        let (tx, rx) = std::sync::mpsc::channel();
        player.shutdown(Box::new(move || {
            let _ = tx.send(());
        }));
        let _ = rx.recv_timeout(Duration::from_secs(30));

        // The simulator's object is a 4x2 block at (100 + tag, 900) on a
        // 1920x1080 canvas, so the composite is exactly that texture and the
        // rect is that block letterboxed into the window.
        assert_eq!(
            (canvas_w, canvas_h),
            (4, 2),
            "the composite is not the decoded region's own texture"
        );
        assert_eq!(
            ink,
            (canvas_w * canvas_h) as usize,
            "the composited page is not opaque, so nothing would be visible"
        );

        let frames_up = walk.iter().filter(|(_, up, _)| *up).count();
        eprintln!(
            "walked {} frames, {frames_up} with a bitmap subtitle on the bridge, {delivered} \
             packets delivered",
            walk.len()
        );
        assert!(
            frames_up >= 8,
            "only {frames_up} frames of {} carried a bitmap subtitle; the lane is not wired",
            walk.len()
        );

        // THE PLACEMENT. `video_rect(1920x1080 into 1280x720)` is the full
        // window, so a region at x=100+tag, y=900 lands at two thirds scale.
        let scale = WINDOW.0 as f32 / CODED.0 as f32;
        for (pts, up, placed) in &walk {
            if !up {
                continue;
            }
            assert!(
                (placed[2] - 4.0 * scale).abs() < 1.0 && (placed[3] - 2.0 * scale).abs() < 1.0,
                "the subtitle is the wrong size at {pts}: {placed:?}"
            );
            assert!(
                placed[0] >= 100.0 * scale - 1.0 && placed[0] < 128.0 * scale,
                "the subtitle drifted horizontally at {pts}: {placed:?}"
            );
            assert!(
                (placed[1] - 900.0 * scale).abs() < 1.0,
                "the subtitle is not on the line it was authored on at {pts}: {placed:?}"
            );
            assert!(
                placed[1] > WINDOW.1 as f32 / 2.0,
                "the subtitle is not in the lower half of the picture at {pts}: {placed:?}"
            );
        }

        // ...AND IT COMES DOWN. Every set is followed by the format's own empty
        // composition, so frames between two sets must show nothing.
        let down = walk.iter().filter(|(_, up, _)| !*up).count();
        assert!(
            down >= 4,
            "the subtitle never came down in {} frames, so the clear path is dead",
            walk.len()
        );
        assert!(
            *recorder.clears.lock().unwrap() >= 4,
            "the bridge property was never taken down"
        );
        // ...and the lane never asked the engine to paint a single text pixel.
        assert_eq!(engine.cached_pixels(), 0);
    }
}
