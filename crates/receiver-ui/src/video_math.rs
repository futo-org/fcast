//! Pure geometry and pixel math for the video presenters, host-testable.

use std::sync::atomic::{AtomicU64, Ordering};

use fcast_video::{cue::CueEngine, cue_ir::VideoRect};

/// Centered rect of src aspect inside dst.
#[allow(dead_code)]
pub(crate) fn letterbox(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (i32, i32, i32, i32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return (0, 0, dst_w as i32, dst_h as i32);
    }
    let (w, h) = if (dst_w as u64) * (src_h as u64) > (dst_h as u64) * (src_w as u64) {
        ((dst_h as u64 * src_w as u64 / src_h as u64) as u32, dst_h)
    } else {
        (dst_w, (dst_w as u64 * src_h as u64 / src_w as u64) as u32)
    };
    (
        ((dst_w - w) / 2) as i32,
        ((dst_h - h) / 2) as i32,
        w as i32,
        h as i32,
    )
}

/// Coded size scaled to square pixels. Anamorphic content (DVD-era SD, and
/// anything a broadcaster still stretches) carries a pixel aspect ratio in
/// the caps, and showing it 1:1 leaves the frame squeezed no matter what the
/// scene does with it, since `image-fit: contain` only ever preserves the
/// aspect it is given.
///
/// The width carries the correction, which is the rule every player uses:
/// 720x576 with a 16:15 PAR is the 768x576 4:3 frame it was authored as.
///
/// This is the same arithmetic `slint`'s `VideoFrameSource` runs on the raw
/// size and [`sane_par`], truncating and clamped to one, and it has to stay
/// the same: the lane anchors its cues against what it computes here and the
/// renderer draws the picture against what slint computes there, so a
/// disagreement of one pixel is a subtitle one pixel off the picture. Pinned
/// by `the_scene_picture_is_the_size_slint_computes`, which asks a real
/// `slint::Image` rather than trusting the comment.
pub(crate) fn display_size(coded: (u32, u32), par: (u32, u32)) -> (u32, u32) {
    let (n, d) = par;
    if n == 0 || d == 0 || n == d {
        return coded;
    }
    let scaled = ((coded.0 as u64 * n as u64) / d as u64).max(1);
    (scaled as u32, coded.1)
}

/// The size the picture is drawn at: square pixels, then the frame's own
/// quarter turn. What [`video_rect`] wants and what a consumer placing
/// anything in coded pixels has to undo.
#[allow(dead_code)]
pub(crate) fn picture_size(coded: (u32, u32), par: (u32, u32), quarter_turn: bool) -> (u32, u32) {
    let size = display_size(coded, par);
    if quarter_turn {
        (size.1, size.0)
    } else {
        size
    }
}

/// Where the picture sits inside the window, as every lane fits it: aspect
/// preserved and centered. `picture` is the DISPLAYED size, i.e. already
/// corrected for pixel aspect and turned by the rotation; feeding coded dims
/// here puts the rect on the wrong pixels for anamorphic or rotated content.
///
/// The fit is slint's own [`place_with_par`], not [`letterbox`], and that is
/// the point: on the wgpu lane the renderer places the picture itself and the
/// cue engine has to be told the very rect it placed, or every positioned cue
/// sits beside the picture instead of on it. Both sides therefore run one
/// arithmetic on one pair of sizes. The pixel aspect and the turn are already
/// resolved into `picture`, which is why the ratio handed over is square and
/// the transform upright; correcting twice would move the rect off the
/// picture exactly as surely as not correcting at all.
///
/// [`place_with_par`]: slint::wgpu_30::video::place_with_par
#[allow(dead_code)]
pub(crate) fn video_rect(picture: (u32, u32), window: (u32, u32)) -> Option<VideoRect> {
    if picture.0 == 0 || picture.1 == 0 || window.0 == 0 || window.1 == 0 {
        return None;
    }
    let (dw, dh) = (window.0 as f32, window.1 as f32);
    let (sw, sh) = (picture.0 as f32, picture.1 as f32);
    let scale = (dw / sw).min(dh / sh);
    let (w, h) = (sw * scale, sh * scale);
    let (x0, y0) = ((dw - w) / 2.0, (dh - h) / 2.0);
    // Each edge rounded on its own and the size taken from the rounded edges,
    // so two neighbouring rectangles meet with no gap, and clamped inside the
    // window the way the placement it mirrors clamps.
    let x = x0.round() as i32;
    let y = y0.round() as i32;
    let width = ((x0 + w).round() as i32 - x).clamp(1, window.0 as i32 - x);
    let height = ((y0 + h).round() as i32 - y).clamp(1, window.1 as i32 - y);
    Some(VideoRect {
        x,
        y,
        width: width as u32,
        height: height as u32,
    })
}

/// The geometry the cue engine has already been told about.
///
/// Positioned cues (`\pos`, `{\anN}`, WebVTT `line`/`position`) anchor to the
/// PICTURE, not the window, and the engine can only do that if something
/// pushes it the rect. Nothing did, on any lane, so they all placed such cues
/// against the window.
///
/// Both halves are latched here so a steady frame costs two relaxed loads and
/// a compare: the engine is only touched when the window or the picture
/// actually moved, and a re-key of every active raster is what a spurious
/// push would cost. One writer per instance (the UI thread on each lane), so
/// the two halves never have to be read as a pair.
pub(crate) struct CueGeometry {
    window: AtomicU64,
    picture: AtomicU64,
}

/// A size as one word, so a compare is one instruction.
fn pack(size: (u32, u32)) -> u64 {
    ((size.0 as u64) << 32) | size.1 as u64
}

fn unpack(word: u64) -> (u32, u32) {
    ((word >> 32) as u32, word as u32)
}

impl CueGeometry {
    pub(crate) const fn new() -> Self {
        Self {
            window: AtomicU64::new(0),
            picture: AtomicU64::new(0),
        }
    }

    /// Push the canvas and the picture rect at `engine` when either moved,
    /// and answer with the rect that was pushed.
    ///
    /// `window` is the cue canvas in physical pixels; `picture` the displayed
    /// video size (see [`video_rect`]). A zero window is a mid-create or
    /// mid-minimize report and latches nothing, so the restore still
    /// registers. A picture that is not known yet still gets the canvas
    /// through.
    pub(crate) fn sync(
        &self,
        engine: &CueEngine,
        window: (u32, u32),
        picture: (u32, u32),
    ) -> Option<VideoRect> {
        if window.0 == 0 || window.1 == 0 {
            return None;
        }
        let (w, p) = (pack(window), pack(picture));
        // Load then store rather than swap: one writer per instance, so a
        // locked exchange buys nothing over a compare, and the steady frame is
        // the two loads this type's doc claims it is.
        let window_moved = self.window.load(Ordering::Relaxed) != w;
        let picture_moved = self.picture.load(Ordering::Relaxed) != p;
        if !window_moved && !picture_moved {
            return None;
        }
        if window_moved {
            self.window.store(w, Ordering::Relaxed);
        }
        if picture_moved {
            self.picture.store(p, Ordering::Relaxed);
        }
        if window_moved {
            engine.set_canvas(window.0, window.1);
        }
        let rect = video_rect(picture, window)?;
        engine.set_video_rect(Some(rect));
        Some(rect)
    }

    /// The canvas last pushed, `(0, 0)` before the first one.
    pub(crate) fn window(&self) -> (u32, u32) {
        unpack(self.window.load(Ordering::Relaxed))
    }

    /// The picture last pushed, `(0, 0)` before the first frame. A resize has
    /// to re-anchor against the picture that is still on screen, and only the
    /// frame path knows what that is.
    pub(crate) fn picture(&self) -> (u32, u32) {
        unpack(self.picture.load(Ordering::Relaxed))
    }
}

/// Chroma sources for 4:2:0 to RGBA, as a decoder's planes arrive.
#[allow(dead_code)]
pub(crate) enum Chroma<'a> {
    /// One interleaved plane, `swap` for NV21 (VU order).
    SemiPlanar {
        data: &'a [u8],
        stride: usize,
        swap: bool,
    },
    Planar {
        u: &'a [u8],
        ustride: usize,
        v: &'a [u8],
        vstride: usize,
    },
    /// No usable chroma, the output is neutral.
    Gray,
}

/// Rows of `row_bytes` that fit whole inside a `len` byte plane.
fn full_rows(len: usize, stride: usize, row_bytes: usize) -> usize {
    if stride == 0 || row_bytes == 0 || len < row_bytes {
        return 0;
    }
    (len - row_bytes) / stride + 1
}

// BT.601 limited range, y pre-scaled, u/v pre-biased
#[inline(always)]
fn write_px(px: &mut [u8], y: u8, du: i32, dv: i32) {
    let y = 298 * (y as i32 - 16).max(0);
    px[0] = ((y + 409 * dv + 128) >> 8).clamp(0, 255) as u8;
    px[1] = ((y - 100 * du - 208 * dv + 128) >> 8).clamp(0, 255) as u8;
    px[2] = ((y + 516 * du + 128) >> 8).clamp(0, 255) as u8;
    px[3] = 255;
}

/// Converts a `w` by `h` 4:2:0 frame into packed RGBA rows of `w * 4`
/// bytes. Plane strides may exceed the row width and planes may hold more
/// rows than the frame shows; neither padding is ever read. A plane that
/// ends short of `h` (or of `h/2`) repeats its last whole row rather than
/// reading past it, so a truncated chroma plane costs an edge row of
/// colour and never leaks allocator garbage. Row indices are clamped once
/// per row, outside the pixel loops.
#[allow(dead_code)]
pub(crate) fn yuv420_to_rgba(
    ydata: &[u8],
    ystride: usize,
    chroma: Chroma<'_>,
    w: usize,
    h: usize,
    out: &mut [u8],
) {
    if w == 0 || h == 0 || out.len() < w * h * 4 {
        return;
    }
    let cw = w.div_ceil(2);
    let yrows = full_rows(ydata.len(), ystride, w);
    if yrows == 0 {
        return;
    }
    let crows = match &chroma {
        Chroma::SemiPlanar { data, stride, .. } => full_rows(data.len(), *stride, cw * 2),
        Chroma::Planar {
            u,
            ustride,
            v,
            vstride,
        } => full_rows(u.len(), *ustride, cw).min(full_rows(v.len(), *vstride, cw)),
        Chroma::Gray => 1,
    };
    let chroma = if crows == 0 { Chroma::Gray } else { chroma };
    let crows = crows.max(1);

    for row in 0..h {
        let yo = row.min(yrows - 1) * ystride;
        let yrow = &ydata[yo..yo + w];
        let orow = &mut out[row * w * 4..(row + 1) * w * 4];
        let crow = (row / 2).min(crows - 1);
        match &chroma {
            Chroma::SemiPlanar { data, stride, swap } => {
                let co = crow * stride;
                let c = &data[co..co + cw * 2];
                let pairs = orow
                    .chunks_mut(8)
                    .zip(yrow.chunks(2))
                    .zip(c.chunks_exact(2));
                for ((opair, ypair), uv) in pairs {
                    let (a, b) = (uv[0] as i32 - 128, uv[1] as i32 - 128);
                    let (du, dv) = if *swap { (b, a) } else { (a, b) };
                    for (px, &y) in opair.chunks_mut(4).zip(ypair) {
                        write_px(px, y, du, dv);
                    }
                }
            }
            Chroma::Planar {
                u,
                ustride,
                v,
                vstride,
            } => {
                let (uo, vo) = (crow * ustride, crow * vstride);
                let (urow, vrow) = (&u[uo..uo + cw], &v[vo..vo + cw]);
                let pairs = orow
                    .chunks_mut(8)
                    .zip(yrow.chunks(2))
                    .zip(urow.iter().zip(vrow));
                for ((opair, ypair), (&u, &v)) in pairs {
                    let (du, dv) = (u as i32 - 128, v as i32 - 128);
                    for (px, &y) in opair.chunks_mut(4).zip(ypair) {
                        write_px(px, y, du, dv);
                    }
                }
            }
            Chroma::Gray => {
                for (px, &y) in orow.chunks_mut(4).zip(yrow) {
                    write_px(px, y, 0, 0);
                }
            }
        }
    }
}

#[cfg(test)]
mod cue_geometry_tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use fcast_video::{
        cue::{CueEngine, CueInput, TextFormat},
        cue_ir::{CueIr, ir},
        video::Overlay,
    };

    use super::{CueGeometry, video_rect};

    /// A 16:9 window, and a 4:3 picture inside it: 240px pillars either side.
    const WINDOW: (u32, u32) = (1920, 1080);
    const PICTURE_4_3: (u32, u32) = (1440, 1080);
    const PILLAR: i32 = 240;

    /// A cue with an explicit `\pos`, anchored at its top left corner so its
    /// placement is the position and nothing else. 10% / 20% of the FRAME,
    /// which is the whole point: 10% of the picture is 240 + 144, 10% of the
    /// window is 192.
    fn positioned_cue() -> CueInput {
        let mut cue = CueIr::from_plain_text("positioned");
        cue.layout.origin = Some((10.0, 20.0));
        cue.layout.anchor = Some(ir::Anchor::TopLeft);
        CueInput {
            format: TextFormat::CueIr {
                ir: Arc::new(cue),
                pts_start: Some(gst::ClockTime::ZERO),
            },
            text: "positioned".to_owned(),
            start_rt: gst::ClockTime::ZERO,
            end_rt: Some(gst::ClockTime::from_seconds(10)),
        }
    }

    /// The cue's raster, once the worker has it. Same wait the cue-IR suite
    /// uses; the raster is built off the UI thread on every lane.
    fn shown(engine: &CueEngine) -> Overlay {
        engine.submit(positioned_cue());
        engine.overlays_for(Some(gst::ClockTime::from_seconds(1)));
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(overlay) = engine.current_overlays().into_iter().next() {
                return overlay;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for the cue raster");
    }

    /// THE BUG THIS WAVE FIXES: with the rect pushed, a positioned cue is
    /// placed against the picture; without it, against the window.
    #[test]
    fn a_positioned_cue_lands_on_the_picture_not_the_window() {
        let anchored = CueEngine::new();
        let geometry = CueGeometry::new();
        let rect = geometry
            .sync(&anchored, WINDOW, PICTURE_4_3)
            .expect("the first sync pushes");
        assert_eq!((rect.x, rect.width), (PILLAR, PICTURE_4_3.0));

        let bare = CueEngine::new();
        bare.set_canvas(WINDOW.0, WINDOW.1);

        let (on_picture, on_window) = (shown(&anchored), shown(&bare));
        assert!(
            on_picture.x >= PILLAR,
            "10% into the picture is past the left pillar, got x={}",
            on_picture.x
        );
        assert!(
            (on_picture.x as u32) < PILLAR as u32 + PICTURE_4_3.0,
            "and still inside it, got x={}",
            on_picture.x
        );
        assert!(
            on_window.x < PILLAR,
            "the same cue with no rect sits in the window's own 10%, got x={}",
            on_window.x
        );
        // The whole difference is the pillar plus its share of the narrower
        // frame, which is what anchoring to the picture means.
        assert_eq!(on_picture.x - on_window.x, PILLAR + 144 - 192);
    }

    #[test]
    fn a_four_three_video_pillarboxes_in_a_sixteen_nine_window() {
        let rect = video_rect(PICTURE_4_3, WINDOW).expect("both sizes are real");
        assert_eq!((rect.x, rect.y), (PILLAR, 0));
        assert_eq!((rect.width, rect.height), (1440, 1080));
    }

    /// The shape the corpus is actually made of: a scope movie in a 16:9
    /// window. Both bars are horizontal, so the cue band keeps the window's
    /// full width and only the vertical anchoring moves.
    #[test]
    fn a_scope_movie_letterboxes_in_a_sixteen_nine_window() {
        // 1920x804 (2.388:1), what a BluRay rip of a scope feature carries.
        let rect = video_rect((1920, 804), WINDOW).expect("real sizes");
        assert_eq!((rect.x, rect.width), (0, 1920));
        assert_eq!((rect.y, rect.height), (138, 804));

        // 1920x800 (2.40:1), the other common crop.
        let rect = video_rect((1920, 800), WINDOW).expect("real sizes");
        assert_eq!((rect.y, rect.height), (140, 800));
    }

    /// The rect the cue engine is told about is the rect slint's renderer
    /// draws the picture in.
    ///
    /// The renderer fits the picture itself, inside the draw, so an
    /// application cannot ask it where the picture went: it computes the same
    /// fit and trusts the answer. This pins that the two are one arithmetic,
    /// against the fork's own [`place_with_par`], which is what the compositor
    /// lane and the video seam both place with. A drift here is every
    /// positioned cue landing beside the picture instead of on it, and
    /// nothing else in the tree would notice.
    ///
    /// [`place_with_par`]: slint::wgpu_30::video::place_with_par
    #[cfg(not(target_os = "android"))]
    #[test]
    fn the_cue_rect_is_the_one_slint_places_the_picture_in() {
        use slint::wgpu_30::video as iv;
        let cases = [
            ((1920u32, 1080u32), (1920u32, 1080u32)),
            ((768, 576), WINDOW),
            ((1080, 1920), WINDOW),
            ((1920, 804), WINDOW),
            ((101, 99), (100, 100)),
            ((1705, 720), (1281, 721)),
            ((3840, 2160), (1366, 768)),
        ];
        for (picture, window) in cases {
            let ours = video_rect(picture, window).expect("real sizes");
            let theirs = iv::place_with_par(
                iv::Size::new(window.0, window.1),
                iv::Size::new(picture.0, picture.1),
                (1, 1),
                iv::BufferTransform::Normal,
                iv::Fit::Contain,
            )
            .expect("real sizes");
            assert_eq!(
                (ours.x, ours.y, ours.width, ours.height),
                (
                    theirs.x,
                    theirs.y,
                    theirs.width as u32,
                    theirs.height as u32
                ),
                "{picture:?} in {window:?}"
            );
        }
    }

    /// A quarter turn trades the axes before the fit, so a portrait clip
    /// recorded sideways letterboxes as the portrait it plays as. The turn
    /// itself is `BufferTransform::swaps_axes` in the fork and `Shown::picture`
    /// on the sink; what is asserted here is the fit that follows it.
    #[test]
    fn a_rotated_clip_fits_by_its_turned_size() {
        let upright = video_rect((1920, 1080), WINDOW).expect("real sizes");
        assert_eq!((upright.x, upright.width), (0, 1920));
        let turned_rect = video_rect((1080, 1920), WINDOW).expect("real sizes");
        // 1080x1920 fits to the window height: 1080 * 1080/1920 = 607.5px
        // wide, centered. Each edge is rounded on its own, which is what
        // slint's own placement does and why the width comes out 608 rather
        // than the truncated 607: the left edge lands at 656.25 and the right
        // at 1263.75, so 656 to 1264.
        assert_eq!((turned_rect.y, turned_rect.height), (0, 1080));
        assert_eq!(turned_rect.width, 608);
        assert_eq!(turned_rect.x, 656);
    }

    /// Anamorphic content: the rect follows the SQUARE PIXEL size. 720x576
    /// with a 16:15 pixel aspect is the 768x576 4:3 frame it was authored as,
    /// and that is what the picture on screen is.
    #[test]
    fn anamorphic_content_fits_by_its_square_pixel_size() {
        let corrected = video_rect((768, 576), WINDOW).expect("real sizes");
        assert_eq!((corrected.x, corrected.width), (PILLAR, 1440));

        let coded = video_rect((720, 576), WINDOW).expect("real sizes");
        assert_ne!(
            coded.width, corrected.width,
            "coded dims would put the rect on the wrong pixels"
        );
    }

    /// The engine is only touched when something moved: everything else is
    /// per frame, and a push re-keys every active raster.
    #[test]
    fn an_unchanged_geometry_is_never_pushed_twice() {
        let engine = CueEngine::new();
        let geometry = CueGeometry::new();
        assert!(geometry.sync(&engine, WINDOW, PICTURE_4_3).is_some());
        assert!(geometry.sync(&engine, WINDOW, PICTURE_4_3).is_none());
        // a resize
        assert!(geometry.sync(&engine, (1280, 720), PICTURE_4_3).is_some());
        // a track switch to a different shape
        assert!(geometry.sync(&engine, (1280, 720), (1920, 1080)).is_some());
        assert!(geometry.sync(&engine, (1280, 720), (1920, 1080)).is_none());
    }

    /// A window mid-create reports 0x0, and must not latch as the geometry
    /// or the restore to a real size would be taken for no change. A picture
    /// nothing has described yet still lets the canvas through.
    #[test]
    fn degenerate_sizes_latch_nothing() {
        let engine = CueEngine::new();
        let geometry = CueGeometry::new();
        assert!(geometry.sync(&engine, (0, 0), PICTURE_4_3).is_none());
        assert!(geometry.sync(&engine, (1920, 0), PICTURE_4_3).is_none());
        assert!(geometry.sync(&engine, WINDOW, PICTURE_4_3).is_some());

        let early = CueEngine::new();
        let geometry = CueGeometry::new();
        assert!(
            geometry.sync(&early, WINDOW, (0, 0)).is_none(),
            "no caps yet"
        );
        // ...and the canvas still reached it, so the rect is all that waits
        assert!(geometry.sync(&early, WINDOW, PICTURE_4_3).is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::letterbox;

    #[test]
    fn wide_video_in_tall_window_pillarboxes_vertically() {
        // 16:9 in a portrait 1440x2768 window, full width, centered bands
        let (x, y, w, h) = letterbox(1920, 1080, 1440, 2768);
        assert_eq!((x, w), (0, 1440));
        assert_eq!(h, 810);
        assert_eq!(y, (2768 - 810) / 2);
    }

    #[test]
    fn tall_video_in_wide_window_pillarboxes_horizontally() {
        let (x, y, w, h) = letterbox(1080, 1920, 2560, 1440);
        assert_eq!((y, h), (0, 1440));
        assert_eq!(w, 810);
        assert_eq!(x, (2560 - 810) / 2);
    }

    #[test]
    fn matching_aspect_fills() {
        assert_eq!(letterbox(1920, 1080, 3840, 2160), (0, 0, 3840, 2160));
    }

    #[test]
    fn zero_dims_return_full_dst() {
        assert_eq!(letterbox(0, 1080, 1440, 900), (0, 0, 1440, 900));
        assert_eq!(letterbox(1920, 0, 1440, 900), (0, 0, 1440, 900));
        assert_eq!(letterbox(1920, 1080, 0, 0), (0, 0, 0, 0));
    }

    #[test]
    fn no_overflow_on_4k_times_4k() {
        // the u64 widening keeps 8k-era products exact
        let (x, y, w, h) = letterbox(7680, 4320, 7680, 8640);
        assert_eq!((x, w), (0, 7680));
        assert_eq!(h, 4320);
        assert_eq!(y, 2160);
    }

    #[test]
    fn result_never_exceeds_dst() {
        for (sw, sh) in [(1279, 533), (853, 1281), (2, 10000), (10000, 2)] {
            let (x, y, w, h) = letterbox(sw, sh, 1440, 2768);
            assert!(x >= 0 && y >= 0);
            assert!(x + w <= 1440, "{sw}x{sh}");
            assert!(y + h <= 2768, "{sw}x{sh}");
        }
    }
}

#[cfg(test)]
mod yuv_tests {
    use super::{Chroma, yuv420_to_rgba};

    /// Fills every byte the conversion must not read. Reading it shows up
    /// as a colour shift, valid chroma in these fixtures is neutral or a
    /// known per row pair.
    const PAD: u8 = 0xa5;

    fn luma(row: usize) -> u8 {
        16 + (row % 200) as u8
    }

    fn uv(crow: usize) -> (u8, u8) {
        (40 + (crow % 150) as u8, 200 - (crow % 150) as u8)
    }

    fn y_plane(w: usize, h: usize, stride: usize, rows: usize) -> Vec<u8> {
        let mut p = vec![PAD; stride * rows];
        for r in 0..h {
            p[r * stride..r * stride + w].fill(luma(r));
        }
        p
    }

    fn uv_plane(
        cw: usize,
        ch: usize,
        stride: usize,
        rows: usize,
        f: impl Fn(usize) -> (u8, u8),
    ) -> Vec<u8> {
        let mut p = vec![PAD; stride * rows];
        for r in 0..ch {
            let (u, v) = f(r);
            for c in 0..cw {
                p[r * stride + 2 * c] = u;
                p[r * stride + 2 * c + 1] = v;
            }
        }
        p
    }

    fn c_plane(
        cw: usize,
        ch: usize,
        stride: usize,
        rows: usize,
        f: impl Fn(usize) -> u8,
    ) -> Vec<u8> {
        let mut p = vec![PAD; stride * rows];
        for r in 0..ch {
            p[r * stride..r * stride + cw].fill(f(r));
        }
        p
    }

    fn rgba(ydata: &[u8], ystride: usize, chroma: Chroma<'_>, w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h * 4];
        yuv420_to_rgba(ydata, ystride, chroma, w, h, &mut out);
        out
    }

    fn semi(data: &[u8], stride: usize, swap: bool) -> Chroma<'_> {
        Chroma::SemiPlanar { data, stride, swap }
    }

    // display size, coded size the decoder pads to
    const GEOM: [(usize, usize, usize); 5] = [
        (64, 1080, 1088),
        (64, 1081, 1088),
        (64, 719, 720),
        (63, 405, 416),
        (2, 2, 16),
    ];

    #[test]
    fn nv12_coded_and_stride_padding_never_reaches_output() {
        for (w, h, coded) in GEOM {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let stride = w + 48;
            let padded = rgba(
                &y_plane(w, h, stride, coded),
                stride,
                semi(
                    &uv_plane(cw, ch, stride, coded.div_ceil(2), uv),
                    stride,
                    false,
                ),
                w,
                h,
            );
            let tight = rgba(
                &y_plane(w, h, w, h),
                w,
                semi(&uv_plane(cw, ch, cw * 2, ch, uv), cw * 2, false),
                w,
                h,
            );
            assert_eq!(padded, tight, "{w}x{h} in {coded}");
        }
    }

    #[test]
    fn i420_coded_and_stride_padding_never_reaches_output() {
        for (w, h, coded) in GEOM {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (ys, cs) = (w + 48, cw + 24);
            let (crows, u, v) = (coded.div_ceil(2), |r: usize| uv(r).0, |r: usize| uv(r).1);
            let (up, vp) = (c_plane(cw, ch, cs, crows, u), c_plane(cw, ch, cs, crows, v));
            let padded = rgba(
                &y_plane(w, h, ys, coded),
                ys,
                Chroma::Planar {
                    u: &up,
                    ustride: cs,
                    v: &vp,
                    vstride: cs,
                },
                w,
                h,
            );
            let (ut, vt) = (c_plane(cw, ch, cw, ch, u), c_plane(cw, ch, cw, ch, v));
            let tight = rgba(
                &y_plane(w, h, w, h),
                w,
                Chroma::Planar {
                    u: &ut,
                    ustride: cw,
                    v: &vt,
                    vstride: cw,
                },
                w,
                h,
            );
            assert_eq!(padded, tight, "{w}x{h} in {coded}");
        }
    }

    #[test]
    fn poisoned_padding_leaves_every_row_neutral() {
        // neutral chroma everywhere valid, so any padding byte that leaks
        // in tints its pixel
        for (w, h, coded) in GEOM {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let stride = w + 48;
            let out = rgba(
                &y_plane(w, h, stride, coded),
                stride,
                semi(
                    &uv_plane(cw, ch, stride, coded.div_ceil(2), |_| (128, 128)),
                    stride,
                    false,
                ),
                w,
                h,
            );
            for (i, px) in out.chunks_exact(4).enumerate() {
                assert!(
                    px[0] == px[1] && px[1] == px[2],
                    "{w}x{h} in {coded}: pixel {} at row {} is tinted {px:?}",
                    i % w,
                    i / w
                );
            }
        }
    }

    #[test]
    fn short_interleaved_plane_repeats_the_last_chroma_row() {
        // android's AImage reports the interleaved plane one byte shy of
        // its last V sample, the bottom chroma row is then unreadable
        let (w, h) = (64usize, 720usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let stride = w + 48;
        let mut plane = uv_plane(cw, ch, stride, ch, uv);
        plane.truncate(stride * (ch - 1) + cw * 2 - 1);
        let got = rgba(
            &y_plane(w, h, stride, h),
            stride,
            semi(&plane, stride, false),
            w,
            h,
        );

        let repeated = uv_plane(cw, ch, stride, ch, |r| uv(r.min(ch - 2)));
        let want = rgba(
            &y_plane(w, h, stride, h),
            stride,
            semi(&repeated, stride, false),
            w,
            h,
        );
        assert_eq!(got, want);

        // the bottom two rows keep their colour, they do not fall back to
        // neutral grey
        for row in h - 2..h {
            let px = &got[row * w * 4..row * w * 4 + 4];
            assert_ne!(px[0], px[1], "row {row} went neutral");
        }
    }

    #[test]
    fn nv21_swaps_on_every_row_including_the_last() {
        for (w, h, _) in GEOM {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let stride = w + 48;
            let vu = rgba(
                &y_plane(w, h, stride, h),
                stride,
                semi(
                    &uv_plane(cw, ch, stride, ch, |r| (uv(r).1, uv(r).0)),
                    stride,
                    true,
                ),
                w,
                h,
            );
            let want = rgba(
                &y_plane(w, h, stride, h),
                stride,
                semi(&uv_plane(cw, ch, stride, ch, uv), stride, false),
                w,
                h,
            );
            assert_eq!(vu, want, "{w}x{h}");
        }
    }

    #[test]
    fn unusable_planes_stay_safe() {
        let (w, h) = (16, 8);
        let y = y_plane(w, h, w, h);
        // chroma too short for one whole row falls back to neutral
        let out = rgba(&y, w, semi(&[1, 2, 3], w, false), w, h);
        let gray = rgba(&y, w, Chroma::Gray, w, h);
        assert_eq!(out, gray);
        // an empty luma plane writes nothing and does not panic
        assert_eq!(rgba(&[], w, Chroma::Gray, w, h), vec![0u8; w * h * 4]);
        // an output buffer that is too small is left alone
        let mut small = vec![0u8; 4];
        yuv420_to_rgba(&y, w, Chroma::Gray, w, h, &mut small);
        assert_eq!(small, vec![0u8; 4]);
    }
}
