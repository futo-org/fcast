//! What the android AHardwareBuffer lane is allowed to propose, decided
//! without touching libandroid.
//!
//! Everything here is a pure function of the caps, the format table and a
//! small [`AhbEnv`] the caller fills in from the runtime probe. That split is
//! deliberate: the interesting part of the lane is the gating (which formats
//! map, when a proposal is skipped, what a gralloc layout means) and none of
//! it needs a device, so it is tested on the host while [`crate::android_ahb`]
//! keeps the FFI.

use gst_video::VideoFormat;

/// Buffers asked of the pool up front, same arithmetic as the desktop
/// udmabuf lane: the appsink holds two, the render borrows three more while
/// the GPU samples them, and one is in flight. Anything under six makes the
/// decoder grow the pool on its first pass. `max` stays zero so a decoder
/// with a deep DPB is never blocked on a slot the lane holds.
pub const MIN_BUFFERS: u32 = 6;

/// The AHardwareBuffer pixel formats the lane can allocate.
///
/// The raw values are the NDK's own and are asserted against `ndk_sys` in
/// [`crate::android_ahb`], so this table can be read and tested on a host
/// that has no NDK.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AhbFormat {
    /// `AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420`, API 29. A single allocation
    /// holding all three components; the concrete plane layout is gralloc's
    /// choice and only `AHardwareBuffer_lockPlanes` reports it.
    Yuv420,
    /// `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM`, API 26.
    Rgba8888,
}

impl AhbFormat {
    pub const fn raw(self) -> u32 {
        match self {
            Self::Yuv420 => 0x23,
            Self::Rgba8888 => 0x01,
        }
    }

    /// Bytes per pixel of the whole frame, used only to size the ALLOCATION
    /// query's advisory `size` before any real allocation has happened. The
    /// pool replaces it with gralloc's answer as soon as it has one.
    const fn bits_per_pixel(self) -> u32 {
        match self {
            Self::Yuv420 => 12,
            Self::Rgba8888 => 32,
        }
    }
}

/// The AHardwareBuffer format a gst video format can live in.
///
/// # What is missing and why
///
/// * 10-bit. There is no portable YUV AHardwareBuffer format for it.
///   `AHARDWAREBUFFER_FORMAT_YCbCr_P010` exists but is API 33, and even there
///   `AHardwareBuffer_lockPlanes` is not specified to work on it, so a
///   CPU-writing decoder has nothing to write through. P010, I420_10LE and
///   NV12_10LE40 stay on the bridge path.
/// * RGBx and friends. `R8G8B8A8_UNORM` always has a real alpha channel and the
///   import samples it; a format whose fourth byte is padding would arrive with
///   undefined alpha. Only fully defined RGBA maps.
/// * Everything packed, planar-with-alpha, or greater than 8 bits per
///   component. Nothing in that set is what a software video decoder emits.
pub fn ahb_format_for(format: VideoFormat) -> Option<AhbFormat> {
    match format {
        VideoFormat::Nv12 | VideoFormat::Nv21 | VideoFormat::I420 | VideoFormat::Yv12 => {
            Some(AhbFormat::Yuv420)
        }
        VideoFormat::Rgba => Some(AhbFormat::Rgba8888),
        _ => None,
    }
}

/// What gralloc actually did with a `Y8Cb8Cr8_420` request, as reported by
/// `AHardwareBuffer_lockPlanes`.
///
/// The format name is not a layout. The same AHardwareBuffer format is NV12
/// on one device, NV21 on the next and I420 on a third, and nothing but a
/// real lock says which. So the lane allocates one buffer at startup, reads
/// this out of it, and afterwards proposes a pool only for the one gst format
/// that matches. Proposing for a format the decoder would then write in a
/// different order is the one way this lane can produce wrong pixels rather
/// than merely fall back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaneLayout {
    /// Distance in bytes between two horizontally adjacent samples of the
    /// second plane. 2 means the chroma is interleaved, 1 means it is not.
    pub chroma_pixel_stride: u32,
    /// The Cr plane starts before the Cb plane in memory.
    pub cr_first: bool,
}

/// The gst format a gralloc layout is, or `None` when it is something no gst
/// format describes (a chroma pixel stride of 4, say, which some tiled
/// layouts report).
pub fn layout_format(layout: PlaneLayout) -> Option<VideoFormat> {
    match (layout.chroma_pixel_stride, layout.cr_first) {
        (2, false) => Some(VideoFormat::Nv12),
        (2, true) => Some(VideoFormat::Nv21),
        (1, false) => Some(VideoFormat::I420),
        (1, true) => Some(VideoFormat::Yv12),
        _ => None,
    }
}

/// The runtime facts the gating needs, behind a trait so the decision is
/// testable without a device. [`crate::android_ahb`] implements it from the
/// startup probe and the latch; the tests implement it from a literal.
pub trait AhbEnv {
    /// `FCAST_ANDROID_AHB` has not turned the lane off.
    fn enabled(&self) -> bool;
    /// A pool or an allocation failed at runtime and the lane latched off.
    fn refused(&self) -> bool;
    /// The gst format this device's gralloc lays a `Y8Cb8Cr8_420` buffer out
    /// as, `None` when the probe never got one or its layout was unnameable.
    fn yuv_layout(&self) -> Option<VideoFormat>;
    /// An `R8G8B8A8_UNORM` allocation went through at probe time.
    fn rgba_ok(&self) -> bool;
    /// The renderer can import a gralloc buffer as a GL texture at all: a
    /// GL context is current on its render thread and the EGL import entry
    /// points resolved there. False on the wgpu/Vulkan lane, and false until
    /// a first frame has rendered and could check, so a proposal never
    /// parks frames the import could not turn into pixels.
    fn gl_import(&self) -> bool;
    /// The renderer samples `GL_TEXTURE_EXTERNAL_OES`, which every YUV
    /// AHardwareBuffer binds as. skia and femtovg do; dodvg refuses the
    /// target and draws nothing, so YUV is not proposed under it while RGBA,
    /// an ordinary 2D texture, still is.
    fn external_textures(&self) -> bool;
}

/// Everything the pool needs, once the gating has said yes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolPlan {
    pub ahb_format: AhbFormat,
    pub video_format: VideoFormat,
    /// The frame the caps ask for, before any decoder padding.
    pub width: u32,
    pub height: u32,
    /// Advisory only. gralloc picks the real stride, so the true size is not
    /// known until the pool has allocated once and it corrects this.
    pub size_estimate: u32,
    pub min_buffers: u32,
}

/// The whole proposal decision, minus the allocation itself.
///
/// `None` means leave the query alone, which is not a failure: the decoder
/// then allocates its own system memory and the frame takes the bridge path
/// exactly as it does today.
pub fn plan(env: &dyn AhbEnv, caps: &gst::Caps, need_pool: bool) -> Option<PoolPlan> {
    if !need_pool || !env.enabled() || env.refused() || !env.gl_import() {
        return None;
    }
    // A decoder that allocates its own importable surfaces (MediaCodec's
    // direct-surface path, or anything under a memory feature) does not want
    // a pool from us and a proposal would only compete with it. System
    // memory, however it is spelled, is the one the lane replaces; a `meta:`
    // feature is a promise about what rides on the buffer rather than about
    // where it lives, so it says nothing either way.
    let plain = caps.features(0).is_none_or(|features| {
        features.is_empty()
            || features
                .iter()
                .all(|name| name == "memory:SystemMemory" || name.starts_with("meta:"))
    });
    if !plain {
        return None;
    }
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    if info.width() == 0 || info.height() == 0 {
        return None;
    }
    let ahb_format = ahb_format_for(info.format())?;
    match ahb_format {
        // Only the one layout the device really produces. Any other yuv
        // format would be written in an order the import does not expect.
        // And only for a renderer that can sample an external texture at
        // all, or the frame would be parked and drawn as nothing.
        AhbFormat::Yuv420
            if !env.external_textures() || env.yuv_layout() != Some(info.format()) =>
        {
            return None;
        }
        AhbFormat::Rgba8888 if !env.rgba_ok() => return None,
        _ => {}
    }

    // A gralloc stride is at least the width and is in practice rounded to
    // 16, 32 or 64. Guessing 64 keeps the advisory size at or above the truth
    // for every layout seen in the wild, and the pool corrects it anyway.
    let stride = width_estimate(info.width());
    let size_estimate = stride
        .checked_mul(info.height())?
        .checked_mul(ahb_format.bits_per_pixel())?
        / 8;

    Some(PoolPlan {
        ahb_format,
        video_format: info.format(),
        width: info.width(),
        height: info.height(),
        size_estimate,
        min_buffers: MIN_BUFFERS,
    })
}

fn width_estimate(width: u32) -> u32 {
    width.div_ceil(64) * 64
}

/// Whether the lane can serve a decoder that asked for this padding.
///
/// Trailing padding is fine: it lives past the picture, and the import clips
/// it away with the source rect the renderer is given. Leading padding is
/// not, because it would move the picture's origin inside the texture and
/// the whole point of the single allocation is that the origin is (0, 0).
/// Every video decoder in the tree that asks for padding at all asks for it
/// on the right and the bottom, which is where the SIMD overrun goes, so
/// refusing the other two costs nothing in practice and removes an entire
/// class of off-by-an-origin bug.
pub fn alignment_is_supported(pad_left: u32, pad_top: u32) -> bool {
    pad_left == 0 && pad_top == 0
}

/// The extent of a gralloc allocation, from the plane pointers it reported.
///
/// gralloc reports where each plane starts, never how big the whole thing is,
/// so the size has to be derived: the distance from the first plane to the
/// furthest plane end. Which plane that is cannot be assumed, because a plane
/// index is the component and not its position in memory. A cr_first layout
/// puts the last-indexed plane (Cr) before Cb, so taking the last index would
/// miss a whole chroma plane on YV12 and a byte on NV21. A 420 layout's chroma
/// planes are half height, every single-plane one is full height. `None` on a
/// plane count no lock produces or on arithmetic that would wrap.
pub fn frame_size(
    format: AhbFormat,
    height: u32,
    n_planes: u32,
    plane_data: &[usize; 4],
    row_stride: &[u32; 4],
) -> Option<usize> {
    if n_planes == 0 || n_planes > 4 {
        return None;
    }
    let mut end = 0usize;
    for i in 0..n_planes as usize {
        let rows = if format == AhbFormat::Yuv420 && i > 0 {
            height.div_ceil(2) as usize
        } else {
            height as usize
        };
        let plane_end = plane_data[i].checked_add((row_stride[i] as usize).checked_mul(rows)?)?;
        end = end.max(plane_end);
    }
    end.checked_sub(plane_data[0])
}

/// Where each plane starts inside the allocation and how wide its rows are.
///
/// The picture sits at the allocation's origin, so this is the plane-to-plane
/// distances gralloc reported, rebased on the first plane and then reordered
/// into gst's planes.
///
/// The reordering is the whole subtlety. A `lockPlanes` index is the
/// COMPONENT, always Y then Cb then Cr, never the position in memory, and
/// gralloc reports three of them for `Y8Cb8Cr8_420` whatever the layout is.
///
/// * NV12 and NV21 are one interleaved chroma plane to gst, starting at its
///   first byte. That is Cb on NV12 and Cr on NV21, so plane 1 is whichever of
///   the two is the lower address, and the third gralloc plane has no gst plane
///   at all. A decoder handed the wrong one writes the sample pairs a byte in,
///   which is chroma shifted by a sample and swapped.
/// * YV12 is I420 with planes 1 and 2 swapped, so gst wants Cr in plane 1 and
///   Cb in plane 2 while the lock reports them the other way round. Unswapped,
///   the decoder's U and V land in each other's gralloc planes.
pub fn plane_geometry(
    video_format: VideoFormat,
    n_planes: u32,
    plane_data: &[usize; 4],
    row_stride: &[u32; 4],
) -> ([usize; 4], [i32; 4]) {
    let mut offsets = [0usize; 4];
    let mut strides = [0i32; 4];
    let base = plane_data[0];
    let n = (n_planes as usize).min(4);
    for i in 0..n {
        offsets[i] = plane_data[i].saturating_sub(base);
        strides[i] = row_stride[i] as i32;
    }
    match video_format {
        VideoFormat::Nv12 | VideoFormat::Nv21 => {
            if n >= 3 {
                offsets[1] = plane_data[1].min(plane_data[2]).saturating_sub(base);
            }
            offsets[2] = 0;
            strides[2] = 0;
        }
        VideoFormat::Yv12 if n >= 3 => {
            offsets.swap(1, 2);
            strides.swap(1, 2);
        }
        _ => {}
    }
    (offsets, strides)
}

/// The buffer geometry to ask gralloc for, given the caps size and the
/// trailing padding the decoder attached to the pool config.
///
/// Padding grows the allocation rather than eating into the picture, and both
/// axes are rounded to even so the chroma plane still pairs up.
pub fn padded_geometry(width: u32, height: u32, pad_right: u32, pad_bottom: u32) -> (u32, u32) {
    (
        (width + pad_right).next_multiple_of(2),
        (height + pad_bottom).next_multiple_of(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    struct Env {
        enabled: bool,
        refused: bool,
        yuv: Option<VideoFormat>,
        rgba: bool,
        gl_import: bool,
        external: bool,
    }

    impl Default for Env {
        fn default() -> Self {
            Self {
                enabled: true,
                refused: false,
                yuv: Some(VideoFormat::Nv12),
                rgba: true,
                gl_import: true,
                external: true,
            }
        }
    }

    impl AhbEnv for Env {
        fn enabled(&self) -> bool {
            self.enabled
        }
        fn refused(&self) -> bool {
            self.refused
        }
        fn yuv_layout(&self) -> Option<VideoFormat> {
            self.yuv
        }
        fn rgba_ok(&self) -> bool {
            self.rgba
        }
        fn gl_import(&self) -> bool {
            self.gl_import
        }
        fn external_textures(&self) -> bool {
            self.external
        }
    }

    fn caps(format: &str) -> gst::Caps {
        gst::init().unwrap();
        gst::Caps::from_str(&format!(
            "video/x-raw, format=(string){format}, width=(int)1920, height=(int)1080, \
             framerate=(fraction)30/1"
        ))
        .unwrap()
    }

    /// The format table. The three the appsink negotiates map, RGBA maps, and
    /// everything 10-bit is out because no portable AHardwareBuffer format
    /// holds it.
    #[test]
    fn the_format_table_covers_the_negotiated_set_and_excludes_ten_bit() {
        for f in [
            VideoFormat::Nv12,
            VideoFormat::Nv21,
            VideoFormat::I420,
            VideoFormat::Yv12,
        ] {
            assert_eq!(
                ahb_format_for(f),
                Some(AhbFormat::Yuv420),
                "{f:?} must map to yuv"
            );
        }
        assert_eq!(ahb_format_for(VideoFormat::Rgba), Some(AhbFormat::Rgba8888));
        for f in [
            VideoFormat::P01010le,
            VideoFormat::I42010le,
            VideoFormat::Nv1210le40,
            VideoFormat::Rgbx,
            VideoFormat::Rgb,
            VideoFormat::Gray8,
        ] {
            assert_eq!(ahb_format_for(f), None, "{f:?} must not map");
        }
    }

    /// The NDK's own constants, spelled out here so the host test pins them
    /// and the android build asserts the same values against ndk_sys.
    #[test]
    fn the_raw_format_values_are_the_ndk_ones() {
        assert_eq!(AhbFormat::Yuv420.raw(), 35);
        assert_eq!(AhbFormat::Rgba8888.raw(), 1);
    }

    /// A gralloc layout is read back as a gst format, which is the whole
    /// point of the startup probe: the AHardwareBuffer format name does not
    /// say whether the chroma is interleaved or which of Cb and Cr is first.
    #[test]
    fn a_gralloc_layout_names_a_gst_format() {
        let cases = [
            (2, false, Some(VideoFormat::Nv12)),
            (2, true, Some(VideoFormat::Nv21)),
            (1, false, Some(VideoFormat::I420)),
            (1, true, Some(VideoFormat::Yv12)),
            // a tiled or otherwise exotic layout has no gst name, and a lane
            // that cannot name it must not propose for it
            (4, false, None),
            (0, false, None),
        ];
        for (stride, cr_first, want) in cases {
            let got = layout_format(PlaneLayout {
                chroma_pixel_stride: stride,
                cr_first,
            });
            assert_eq!(got, want, "stride {stride} cr_first {cr_first}");
        }
    }

    /// The happy path: the negotiated format is exactly what the device's
    /// gralloc produces, so a pool is planned for it.
    #[test]
    fn the_probed_layout_is_the_one_format_proposed_for() {
        let got = plan(&Env::default(), &caps("NV12"), true).expect("nv12 must plan");
        assert_eq!(got.ahb_format, AhbFormat::Yuv420);
        assert_eq!(got.video_format, VideoFormat::Nv12);
        assert_eq!((got.width, got.height), (1920, 1080));
        assert_eq!(got.min_buffers, MIN_BUFFERS);
        assert!(
            got.size_estimate >= 1920 * 1080 * 3 / 2,
            "estimate {} is under a frame",
            got.size_estimate
        );

        // the same device, asked for a layout it does not produce
        assert!(
            plan(&Env::default(), &caps("I420"), true).is_none(),
            "a format the gralloc does not lay out must not be proposed for"
        );
        let i420 = Env {
            yuv: Some(VideoFormat::I420),
            ..Env::default()
        };
        assert!(plan(&i420, &caps("I420"), true).is_some());
        assert!(plan(&i420, &caps("NV12"), true).is_none());
    }

    /// Every reason to leave the query alone. Each of these is a real state
    /// the lane reaches, and every one of them has to end on the bridge path
    /// rather than on a proposal the import would refuse.
    #[test]
    fn nothing_is_planned_when_the_lane_cannot_serve_it() {
        let nv12 = caps("NV12");
        assert!(
            plan(&Env::default(), &nv12, false).is_none(),
            "no pool wanted"
        );
        assert!(
            plan(
                &Env {
                    enabled: false,
                    ..Env::default()
                },
                &nv12,
                true
            )
            .is_none(),
            "the off switch"
        );
        assert!(
            plan(
                &Env {
                    refused: true,
                    ..Env::default()
                },
                &nv12,
                true
            )
            .is_none(),
            "the runtime latch"
        );
        assert!(
            plan(
                &Env {
                    yuv: None,
                    ..Env::default()
                },
                &nv12,
                true
            )
            .is_none(),
            "the probe never got a buffer"
        );
        assert!(
            plan(
                &Env {
                    rgba: false,
                    ..Env::default()
                },
                &caps("RGBA"),
                true
            )
            .is_none(),
            "rgba refused at probe time"
        );
        assert!(
            plan(&Env::default(), &caps("P010_10LE"), true).is_none(),
            "10-bit has no AHardwareBuffer format"
        );
        // The renderer half. No GL context on the render thread (the wgpu
        // lane, or no frame rendered yet) refuses everything; a GL renderer
        // without external sampling (dodvg) refuses yuv and keeps rgba.
        let no_gl = Env {
            gl_import: false,
            ..Env::default()
        };
        assert!(plan(&no_gl, &nv12, true).is_none(), "no GL import, yuv");
        assert!(
            plan(&no_gl, &caps("RGBA"), true).is_none(),
            "no GL import, rgba"
        );
        let no_external = Env {
            external: false,
            ..Env::default()
        };
        assert!(
            plan(&no_external, &nv12, true).is_none(),
            "yuv binds external, which this renderer cannot sample"
        );
        assert!(
            plan(&no_external, &caps("RGBA"), true).is_some(),
            "rgba is an ordinary 2D texture and stays"
        );
        gst::init().unwrap();
        let zero = gst::Caps::from_str(
            "video/x-raw, format=(string)NV12, width=(int)0, height=(int)0, \
             framerate=(fraction)30/1",
        )
        .unwrap();
        assert!(plan(&Env::default(), &zero, true).is_none(), "a zero frame");
        let audio = gst::Caps::from_str("audio/x-raw, format=(string)S16LE").unwrap();
        assert!(
            plan(&Env::default(), &audio, true).is_none(),
            "not video at all"
        );
    }

    /// A decoder that allocates its own importable surfaces asks under a
    /// memory feature. The lane must not fight it for them, which is the
    /// android twin of the desktop lane leaving DMA_DRM caps alone.
    #[test]
    fn caps_under_a_memory_feature_are_never_proposed_for() {
        gst::init().unwrap();
        let featured = gst::Caps::from_str(
            "video/x-raw(memory:GLMemory), format=(string)NV12, width=(int)1920, \
             height=(int)1080, framerate=(fraction)30/1",
        )
        .unwrap();
        assert!(plan(&Env::default(), &featured, true).is_none());

        // the video meta feature is not a memory feature, it is a promise the
        // sink can read strides, and that one the lane does serve
        let meta = gst::Caps::from_str(
            "video/x-raw(meta:GstVideoMeta), format=(string)NV12, width=(int)1920, \
             height=(int)1080, framerate=(fraction)30/1",
        )
        .unwrap();
        assert!(plan(&Env::default(), &meta, true).is_some());
    }

    /// A real 1920x1080 NV12 gralloc layout at a 2048 stride: Y, then the
    /// interleaved chroma, then the odd byte of it that gst has no plane for.
    /// Getting the third plane wrong is how a decoder ends up writing chroma
    /// nothing reads.
    #[test]
    fn a_semi_planar_layout_becomes_two_gst_planes() {
        let base = 0x4000_0000usize;
        let chroma = 2048 * 1080;
        let stride = [2048, 2048, 2048, 0];

        // NV12: Cb is the interleave start, Cr the odd byte after it
        let data = [base, base + chroma, base + chroma + 1, 0];
        let (offsets, strides) = plane_geometry(VideoFormat::Nv12, 3, &data, &stride);
        assert_eq!(offsets, [0, chroma, 0, 0]);
        assert_eq!(strides, [2048, 2048, 0, 0]);
        let size = frame_size(AhbFormat::Yuv420, 1080, 3, &data, &stride).unwrap();
        assert_eq!(size, chroma + 1 + 2048 * 540);
        assert!(size >= 1920 * 1080 * 3 / 2, "a frame must fit in it");
    }

    /// The same device, laid out the other way round. A plane index is the
    /// component, so a real NV21 gralloc reports Cb one byte past Cr, and that
    /// lower Cr address is exactly what the probe reads as cr_first. gst's
    /// plane 1 is the interleaved plane from its first byte, so it is Cr's
    /// address here, and pointing it at Cb would shift the chroma by a sample
    /// and swap it.
    #[test]
    fn a_cr_first_semi_planar_layout_starts_at_the_interleave() {
        let base = 0x4000_0000usize;
        let chroma = 2048 * 1080;
        let stride = [2048, 2048, 2048, 0];
        let data = [base, base + chroma + 1, base + chroma, 0];

        let (offsets, strides) = plane_geometry(VideoFormat::Nv21, 3, &data, &stride);
        assert_eq!(
            offsets,
            [0, chroma, 0, 0],
            "plane 1 is the interleave start"
        );
        assert_eq!(strides, [2048, 2048, 0, 0]);

        // the odd byte is past the last-indexed plane's end, so the size has
        // to come from the furthest plane rather than from index order
        assert_eq!(
            frame_size(AhbFormat::Yuv420, 1080, 3, &data, &stride),
            Some(chroma + 1 + 2048 * 540)
        );
    }

    /// The fully planar case keeps all three, and the RGBA case is one plane
    /// at full height rather than the 420 arithmetic.
    #[test]
    fn planar_and_packed_layouts_keep_their_planes() {
        let base = 0x5000_0000usize;
        let (y, c) = (2048 * 1088, 1024 * 544);
        let data = [base, base + y, base + y + c, 0];
        let stride = [2048, 1024, 1024, 0];
        let (offsets, strides) = plane_geometry(VideoFormat::I420, 3, &data, &stride);
        assert_eq!(offsets, [0, y, y + c, 0]);
        assert_eq!(strides, [2048, 1024, 1024, 0]);
        assert_eq!(
            frame_size(AhbFormat::Yuv420, 1088, 3, &data, &stride),
            Some(y + 2 * c)
        );

        let rgba = [base, 0, 0, 0];
        let rgba_stride = [1920 * 4, 0, 0, 0];
        assert_eq!(
            plane_geometry(VideoFormat::Rgba, 1, &rgba, &rgba_stride),
            ([0, 0, 0, 0], [1920 * 4, 0, 0, 0])
        );
        assert_eq!(
            frame_size(AhbFormat::Rgba8888, 1080, 1, &rgba, &rgba_stride),
            Some(1920 * 4 * 1080),
            "a single plane is full height, not half"
        );
        assert_eq!(
            frame_size(AhbFormat::Yuv420, 1080, 0, &rgba, &rgba_stride),
            None
        );
    }

    /// The cr_first planar layout. gralloc still reports Cb at index 1, but
    /// its address is the higher one, and gst YV12 is I420 with planes 1 and 2
    /// swapped, so the two chroma planes trade places. Unswapped, the decoder
    /// writes U where the sampler reads V.
    #[test]
    fn a_cr_first_planar_layout_swaps_the_chroma_planes() {
        let base = 0x6000_0000usize;
        let (y, c) = (2048 * 1088, 1024 * 544);
        // Cr at base + y, Cb after it, reported as Y, Cb, Cr
        let data = [base, base + y + c, base + y, 0];
        let stride = [2048, 1024, 1024, 0];

        let (offsets, strides) = plane_geometry(VideoFormat::Yv12, 3, &data, &stride);
        assert_eq!(offsets, [0, y, y + c, 0], "plane 1 is Cr, plane 2 is Cb");
        assert_eq!(strides, [2048, 1024, 1024, 0]);

        // the trailing Cb plane is past the last-indexed one, so index order
        // would size the allocation a whole chroma plane short
        assert_eq!(
            frame_size(AhbFormat::Yuv420, 1088, 3, &data, &stride),
            Some(y + 2 * c)
        );
    }

    /// Trailing padding grows the allocation and the picture keeps the
    /// origin, which is what lets the renderer clip the overrun away with a
    /// plain source rect.
    #[test]
    fn trailing_padding_grows_the_allocation() {
        assert_eq!(padded_geometry(1920, 1080, 0, 0), (1920, 1080));
        // libav's usual edge overrun
        assert_eq!(padded_geometry(1920, 1080, 32, 32), (1952, 1112));
        // odd extents round up so the chroma plane still pairs
        assert_eq!(padded_geometry(1919, 1079, 0, 0), (1920, 1080));
    }

    /// Leading padding is refused rather than served wrong. A decoder that
    /// wants it keeps its own buffers, which is the same fallback every other
    /// gate here ends in.
    #[test]
    fn leading_padding_is_refused_and_trailing_padding_is_not() {
        assert!(alignment_is_supported(0, 0));
        assert!(
            alignment_is_supported(0, 0),
            "trailing only is the normal case"
        );
        assert!(!alignment_is_supported(16, 0));
        assert!(!alignment_is_supported(0, 16));
        assert!(!alignment_is_supported(16, 16));
    }
}
