//! Desktop wgpu video lane. Decoded planes are handed to slint as planes:
//! the lane builds one `slint::Image` per frame with
//! `Image::try_from_video_frame` and sets it on the `sw-video-frame` bridge
//! image, the same slot the android software bridge feeds. The conversion
//! (CSC, HDR tone mapping, gamut, scaling) runs inside the frame slint's own
//! renderer is already drawing.
//!
//! # Zero copy
//!
//! When the process starts, `create_shared_device` builds one device on the
//! platform's backend (Vulkan on linux and windows, Metal on mac, overridable
//! with `FCAST_WGPU_BACKEND`) and the desktop binary hands it to slint through
//! `BackendSelector::require_wgpu_30(Manual { .. })`. Slint then draws on the
//! same device and queue, which is what makes a plane texture crossable at
//! all: a texture import has to happen on the device that samples it.
//!
//! A box with no hardware Vulkan (a software-only or missing driver) falls
//! through to wgpu's GL backend on linux and windows, which is the floor: the
//! same hardware OpenGL driver the lane this replaces ran on there, with the
//! upload arm carrying every frame since GL has no dmabuf import. On a hybrid
//! box the GL pass pins glvnd to the Mesa vendor first (see `egl_vendor`).
//!
//! The floor's device is opened differently. A GL instance presents only on
//! the display it was opened with, which on Wayland is one connection: the
//! window's, and that does not exist at startup. So the startup pass only
//! PROVES a hardware GL adapter, slint is asked to open the real device on the
//! window's display (GL by name, see `gl_floor_settings`), and the lane adopts
//! it from the first `RenderingSetup` (`adopt_renderer_device`); the sink
//! waits for that handover for a moment before it is built.
//!
//! Nothing here renders. There is no video render target, no three-deep
//! rotation and no readback: the lane describes the frame's own planes in
//! `slint::wgpu_30::video`'s vocabulary and the renderer converts them
//! straight into the scene, one pass and one full screen 8-bit intermediate
//! less than the two step route this replaces.
//!
//! # Lifetime
//!
//! Slint holds the frame `Arc` until the last submit that read the planes has
//! retired, then drops it, so dropping the frame IS the release. The decoder's
//! buffer therefore rides inside the frame object and goes back to its pool in
//! that `Drop`, which is what the lane's hand-rolled three-deep `held` array
//! used to do.
//!
//! The frame object is built on the UI thread out of a `Send` payload, and the
//! `Arc<dyn VideoFrame>` never leaves it. That is what lets `on_release` hold
//! plain `Box<dyn FnOnce()>` callbacks: registration and firing are both the
//! UI thread's.
//!
//! # Zero copy on the way in, too
//!
//! On linux the appsink offers `memory:DMABuf` `DMA_DRM` caps ahead of the
//! system-memory ones, so a VA-API decoder hands its frames over as dmabuf fds
//! and [`desktop_wgpu_dmabuf`] imports them straight into Vulkan. Nothing maps
//! them: mapping a VA surface readable makes the driver de-tile the whole
//! frame on the CPU, which at 4K60 is most of a streaming thread. Software
//! decoders keep negotiating the system-memory structures behind them and take
//! the upload path unchanged.
//!
//! Each dmabuf is imported once, not once a frame: the decoder cycles a fixed
//! pool of surfaces, so a settled stream is a key lookup, a render and nothing
//! else. Both arms allocate nothing of the lane's own per frame at that point,
//! which the steady-state tests at the bottom of this file measure.
//!
//! On mac the route is [`desktop_wgpu_iosurface`] and the caps do not move at
//! all. VideoToolbox decodes into CVPixelBuffers, and `vtdec` hands the same
//! IOSurface-backed memory out whether the caps say `memory:IOSurface` or
//! plain system memory, so the offer stays as it was and the lane asks the
//! MEMORY whether it carries a surface. Each plane is then wrapped as an
//! `MTLTexture` on the shared device, cached per surface exactly as the dmabuf
//! arm caches per buffer. A frame the import refuses is mapped and uploaded,
//! which is safe here in a way it is not for a VA surface: a CVPixelBuffer
//! maps to ordinary pixels.
//!
//! On windows there is no import route at all and every frame takes the upload
//! arm.
//!
//! # Fallback
//!
//! With no shared device (no adapter, or the backend selection refused it)
//! slint is on dodvg's OpenGL executor, which has no video path at all and
//! draws nothing for a video image. So the lane declines: `make_sink` answers
//! `None` and the receiver plays with no video sink. The rule is that the
//! capability query answers unsupported before a producer commits to the path.
//!
//! That is also why `FrameSource::SystemMemory` is not the answer here even
//! though the ingress vocabulary has it: the machine that refused the shared
//! device is not on the wgpu executor either, so an upload arm inside the fork
//! would never be reached. Frames that arrive in system memory are uploaded on
//! the streaming thread, into plane textures on the shared device, and cross
//! as `FrameSource::Wgpu30` like every other frame. See the wave 5 report.
//!
//! A frame the import refuses is dropped rather than mapped, and the appsink
//! is narrowed to system memory with a reconfigure asked of upstream. That ask
//! is best effort: a VA decoder ignores it once its input state is settled, so
//! the lane keeps retrying the import instead of latching itself off.
//!
//! `FCAST_DESKTOP_WGPU_DMABUF=0` leaves the appsink advertising system memory
//! only, which is the A/B control for the import.
//! `FCAST_DESKTOP_WGPU_IOSURFACE=0` is the mac one, and puts every frame back
//! on the upload arm without moving the caps.
//! `FCAST_WGPU_SOFTWARE=0` refuses a CPU adapter (lavapipe, llvmpipe), which
//! is otherwise the last resort after every hardware pass declined.
//! `__EGL_VENDOR_LIBRARY_FILENAMES` set by hand overrides the GL pass's own
//! vendor pin.

use fcast_video::render_options::RenderProfile;
use gst::prelude::*;
use gst_video::prelude::*;
use i_slint_video_wgpu::{
    ChromaLocation, DebandParams, FrameDesc, HdrMetadata, Matrix, PixelFormat, Primaries, Range,
    Renderer, ScaleFilter, TonemapCurve, Transfer,
};
use slint::ComponentHandle;
/// The frame ingress vocabulary, `i-slint-video` through slint's own
/// re-export. Reached this way and not as a second path dependency, so the
/// types can only ever be the ones the linked slint speaks.
use slint::wgpu_30::video as iv;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use tracing::{debug, error, info, warn};

/// Planes the sysmem upload arm carries, and so the size of every per-frame
/// plane array on it. The renderer's own bound, which is four: A420 and GBRA
/// are three color planes plus alpha. Stack arrays, nothing allocated.
const UPLOAD_MAX_PLANES: usize = i_slint_video_wgpu::gpu::MAX_PLANES;

/// `gst_video_sink_init`'s pacing, which an appsink does not inherit from
/// basesink: a frame later than this is dropped instead of rendered.
const VIDEO_SINK_MAX_LATENESS: gst::ClockTime = gst::ClockTime::from_mseconds(5);
/// Same source, what the sink reports it needs to render one frame.
const VIDEO_SINK_PROCESSING_DEADLINE: gst::ClockTime = gst::ClockTime::from_mseconds(15);

// ---------------------------------------------------------------------------
// caps -> FrameDesc
// ---------------------------------------------------------------------------

/// Pixel layouts this lane negotiates. The 16-bit-container ones need a
/// device feature, so the caps are built from what the adapter granted.
///
/// The planar wide formats are what a software decoder emits for 10-bit
/// content: dav1ddec, libde265 and the vpx decoders all hand out
/// `I420_10LE`, never P010, so refusing them killed HDR10 playback on any
/// box without hardware AV1 decode.
pub(crate) fn map_format(format: gst_video::VideoFormat) -> Option<PixelFormat> {
    use gst_video::VideoFormat as F;
    match format {
        F::Nv12 => Some(PixelFormat::Nv12),
        F::Nv21 => Some(PixelFormat::Nv21),
        F::Nv16 => Some(PixelFormat::Nv16),
        F::Nv24 => Some(PixelFormat::Nv24),
        F::P01010le => Some(PixelFormat::P010),
        F::P012Le => Some(PixelFormat::P012),
        F::P016Le => Some(PixelFormat::P016),
        F::I420 => Some(PixelFormat::I420),
        F::Yv12 => Some(PixelFormat::Yv12),
        F::Y41b => Some(PixelFormat::I411),
        F::Y42b => Some(PixelFormat::I422),
        F::Y444 => Some(PixelFormat::I444),
        F::A420 => Some(PixelFormat::A420),
        F::I42010le => Some(PixelFormat::I420P10),
        F::I42012le => Some(PixelFormat::I420P12),
        F::I42210le => Some(PixelFormat::I422P10),
        F::I42212le => Some(PixelFormat::I422P12),
        F::Y44410le => Some(PixelFormat::I444P10),
        F::Y44412le => Some(PixelFormat::I444P12),
        F::Y44416le => Some(PixelFormat::I444P16),
        F::Ayuv => Some(PixelFormat::Ayuv),
        F::Vuya => Some(PixelFormat::Vuya),
        F::Rgba => Some(PixelFormat::Rgba),
        F::Rgbx => Some(PixelFormat::Rgbx),
        F::Bgra => Some(PixelFormat::Bgra),
        F::Bgrx => Some(PixelFormat::Bgrx),
        F::Argb => Some(PixelFormat::Argb),
        F::Abgr => Some(PixelFormat::Abgr),
        F::Xrgb => Some(PixelFormat::Xrgb),
        F::Xbgr => Some(PixelFormat::Xbgr),
        F::Rgb => Some(PixelFormat::Rgb),
        F::Bgr => Some(PixelFormat::Bgr),
        F::Gbr => Some(PixelFormat::Gbr),
        F::Gbr10le => Some(PixelFormat::GbrP10),
        F::Gbr12le => Some(PixelFormat::GbrP12),
        F::Gbra => Some(PixelFormat::Gbra),
        _ => None,
    }
}

/// The layout decides, not the signalled matrix: an RGB format is Identity
/// whatever the caps claim and a YCbCr format never is, because the crate
/// refuses either mismatch. Unsignalled YCbCr follows the resolution split
/// every player uses, SD is BT.601 and anything taller is BT.709.
fn map_matrix(format: PixelFormat, matrix: gst_video::VideoColorMatrix, height: u32) -> Matrix {
    use gst_video::VideoColorMatrix as M;
    if format.layout().rgb {
        return Matrix::Identity;
    }
    match matrix {
        M::Bt709 => Matrix::Bt709,
        M::Bt601 | M::Fcc | M::Smpte240m => Matrix::Bt601,
        M::Bt2020 => Matrix::Bt2020Ncl,
        // Rgb on a YCbCr layout is bogus signalling, unknown and anything
        // the enum grows land on the resolution split.
        _ => {
            if height > 576 {
                Matrix::Bt709
            } else {
                Matrix::Bt601
            }
        }
    }
}

/// Signalled range wins. Unsignalled follows pl_color_levels_guess: RGB
/// layouts are full range, YCbCr is limited.
fn map_range(format: PixelFormat, range: gst_video::VideoColorRange) -> Range {
    match range {
        gst_video::VideoColorRange::Range0_255 => Range::Full,
        gst_video::VideoColorRange::Range16_235 => Range::Limited,
        _ if format.layout().rgb => Range::Full,
        _ => Range::Limited,
    }
}

/// A plain SDR stream tagged bt709/bt601/bt2020-10 decodes as BT.1886, not
/// sRGB; only content really tagged sRGB gets the piecewise curve. Exotic
/// transfers (log, adobergb, pure gammas) have no equivalent in the crate and
/// fall back to BT.1886 rather than dropping the frame.
fn map_transfer(transfer: gst_video::VideoTransferFunction) -> Transfer {
    use gst_video::VideoTransferFunction as T;
    match transfer {
        T::Smpte2084 => Transfer::Pq,
        T::AribStdB67 => Transfer::Hlg,
        T::Srgb => Transfer::Sdr,
        _ => Transfer::Bt1886,
    }
}

/// The gamut the frame was mastered in, for the linear-light matrix to
/// BT.709. The SD sets matter because a `bt601` colorimetry string carries
/// SMPTE 170M primaries, which is every NTSC-era stream, and the old lane
/// converted them. DCI-P3 (`smpte-rp431`) has a greenish white the crate
/// has no adaptation for and is taken as Display P3, which is far closer
/// than BT.709 would be. The rest have no variant and are close enough to
/// BT.709 to pass through, or are so rare (BT.470M, film) that the old
/// lane's handling of them was never exercised either.
fn map_primaries(primaries: gst_video::VideoColorPrimaries) -> Primaries {
    use gst_video::VideoColorPrimaries as P;
    match primaries {
        P::Bt2020 => Primaries::Bt2020,
        P::Smpte170m | P::Smpte240m => Primaries::Bt601_525,
        P::Bt470bg | P::Ebu3213 => Primaries::Bt601_625,
        P::Smpteeg432 | P::Smpterp431 => Primaries::DisplayP3,
        _ => Primaries::Bt709,
    }
}

/// Left siting unless the caps say the chroma sample sits in the middle.
/// An empty flag set is "unknown", which libplacebo and this crate both read
/// as left, so it must not fall through to center.
fn map_chroma(site: gst_video::VideoChromaSite) -> ChromaLocation {
    if site.is_empty() || site.contains(gst_video::VideoChromaSite::H_COSITED) {
        ChromaLocation::Left
    } else {
        ChromaLocation::Center
    }
}

/// The smallest peak a stream may signal and be believed, in nits.
///
/// The libplacebo lane took a mastering luminance only from 100 nits up, and
/// pl floors its tone mapping input there regardless. Below it the number is
/// a muxer's units mistake (a 1 meant as 1000, or 0.0001-nit units written
/// as nits), and the crate would take it at its word: a peak at or under 203
/// nits maps every code to `v.min(peak) / 203`, which for 0.1 nits is a
/// black frame under playing audio.
const MIN_SIGNALLED_PEAK_NITS: f32 = 100.0;

/// The signalled peak, or zero when it is absent or too small to believe.
/// Zero is what the crate reads as unknown, and it then answers with the
/// transfer's own peak, exactly as the old lane did by not setting it.
fn believable_peak(nits: f32) -> f32 {
    // the negated compare also sends a NaN to unknown
    if !(nits >= MIN_SIGNALLED_PEAK_NITS) {
        return 0.0;
    }
    nits
}

/// Mastering peak and MaxCLL off the caps, in nits. Absent fields stay zero,
/// which the crate reads as "unknown" and falls back to the transfer peak;
/// so does anything under [`MIN_SIGNALLED_PEAK_NITS`].
pub fn hdr_metadata(caps: &gst::CapsRef) -> HdrMetadata {
    let max_mastering_nits = gst_video::VideoMasteringDisplayInfo::from_caps(caps)
        .map(|mdi| mdi.max_display_mastering_luminance() as f32 * 0.0001)
        .unwrap_or(0.0);
    let max_cll = gst_video::VideoContentLightLevel::from_caps(caps)
        .map(|cll| cll.max_content_light_level() as f32)
        .unwrap_or(0.0);
    HdrMetadata {
        max_mastering_nits: believable_peak(max_mastering_nits),
        max_cll: believable_peak(max_cll),
    }
}

/// The whole caps mapping. `None` means the lane cannot render these caps and
/// the sample is dropped.
pub fn frame_desc(info: &gst_video::VideoInfo, hdr: HdrMetadata) -> Option<FrameDesc> {
    let colorimetry = info.colorimetry();
    let (width, height) = (info.width(), info.height());
    let format = map_format(info.format())?;
    Some(FrameDesc {
        format,
        matrix: map_matrix(format, colorimetry.matrix(), height),
        range: map_range(format, colorimetry.range()),
        transfer: map_transfer(colorimetry.transfer()),
        tonemap: TonemapCurve::Spline,
        primaries: map_primaries(colorimetry.primaries()),
        chroma_location: map_chroma(info.chroma_site()),
        width,
        height,
        hdr,
    })
}

/// Sizes past this are refused rather than rendered. The device limit is
/// well under it and the crate checks that too; this only keeps a garbage
/// pixel aspect from asking for a texture nothing could allocate.
const MAX_DISPLAY_DIM: u64 = 32768;

/// The pixel shape the frame declares, sanitized to what the lane will stand
/// behind.
///
/// A frame hands slint its raw size and this ratio, and slint resolves the
/// two; nothing is pre-corrected on the way in, because correcting by scaling
/// the buffer would resample a picture the renderer is about to resample
/// anyway. `(1, 1)` for a degenerate ratio and for one that would blow the
/// frame past [`MAX_DISPLAY_DIM`], so a garbage aspect cannot ask for a
/// picture nothing could place.
pub fn sane_par(info: &gst_video::VideoInfo) -> (u32, u32) {
    let par = info.par();
    let (n, d) = (par.numer() as i64, par.denom() as i64);
    if n <= 0 || d <= 0 || n == d {
        return (1, 1);
    }
    let scaled = (info.width() as u64 * n as u64) / d as u64;
    if scaled == 0 || scaled > MAX_DISPLAY_DIM {
        return (1, 1);
    }
    (n as u32, d as u32)
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
pub fn display_size(coded: (u32, u32), par: (u32, u32)) -> (u32, u32) {
    let (n, d) = par;
    if n == 0 || d == 0 || n == d {
        return coded;
    }
    let scaled = ((coded.0 as u64 * n as u64) / d as u64).max(1);
    (scaled as u32, coded.1)
}

/// Whether the 8-bit write needs the ordered dither. A source with more
/// than 8 bits of luma, or one tone mapped off an absolute-light curve,
/// lands smooth gradients on codes the target cannot hold and bands
/// visibly; 8-bit SDR is already on the output's own grid and is left
/// alone, which is also what keeps its single-pass route and its bit-exact
/// parity against libplacebo.
fn wants_dither(desc: &FrameDesc) -> bool {
    i_slint_video_wgpu::math::coded_bits(desc.format) > 8 || desc.transfer.is_hdr()
}

/// When the dither runs, which is the one profile knob that still depends
/// on the frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DitherRule {
    /// Only where a tone map lands absolute light on the 8-bit grid, which
    /// is the one thing that bands hard enough to be worth a pass on the
    /// profile that exists to be cheap.
    HdrOnly,
    /// Whatever carries more than the target's own eight bits, which is
    /// [`wants_dither`] and what this lane has always done.
    WiderThanTarget,
    /// The write quantizes on every frame, so it always dithers.
    Always,
}

impl DitherRule {
    fn applies(self, desc: &FrameDesc) -> bool {
        match self {
            Self::HdrOnly => desc.transfer.is_hdr(),
            Self::WiderThanTarget => wants_dither(desc),
            Self::Always => true,
        }
    }
}

/// The render profile as this lane's knobs. Plain data, resolved once at
/// startup from the receiver's config, and then read per frame.
///
/// The mapping follows libplacebo's three preset structs
/// (`src/renderer.c:202`), which are what the other lane hands to
/// `pl_render_image`:
///
/// | knob | Fast | Balanced | HighQuality |
/// | --- | --- | --- | --- |
/// | upscaler | none, hardware bilinear | `pl_filter_lanczos` | `pl_filter_ewa_lanczossharp` |
/// | downscaler | none, hardware bilinear | `pl_filter_hermite` | `pl_filter_hermite` |
/// | dither | none | `pl_dither_default_params` | `pl_dither_default_params` |
/// | deband | none | none | `pl_deband_default_params` |
///
/// Two deliberate departures, both noted where they are made below: this
/// crate has no EWA kernel, so high quality upscales with the best
/// separable one it has; and fast still dithers a tone mapped frame,
/// because that is the case where an undithered 8-bit write bands hard and
/// fast is the receiver's default profile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Quality {
    /// Kernel when the output is larger than the source.
    up: ScaleFilter,
    /// Kernel when either axis shrinks, which is how libplacebo picks
    /// between its two (`renderer.c`, `PL_MAX` takes DOWN over UP).
    down: ScaleFilter,
    dither: DitherRule,
    /// The deband strengths, seedless. The seed is per frame and is filled
    /// in at render time.
    deband: Option<DebandParams>,
}

impl Default for Quality {
    fn default() -> Self {
        profile_quality(RenderProfile::Fast)
    }
}

impl Quality {
    /// The kernel for a given size change, `pl_render_params::upscaler` or
    /// `downscaler`. Ignored entirely when the sizes match, which is every
    /// square-pixel stream, so the two profiles that differ only here
    /// render identically on all of them.
    fn filter(self, src: (u32, u32), dst: (u32, u32)) -> ScaleFilter {
        if dst.0 < src.0 || dst.1 < src.1 {
            self.down
        } else {
            self.up
        }
    }

    /// The lane's knobs as the ingress vocabulary spells them, which is what
    /// crosses to slint beside the planes.
    ///
    /// A profile is a decision about a stream and not about a picture, and
    /// slint takes it that way: the handle compares by value, and the
    /// renderer keys its registered source on it, so a profile that moved
    /// per frame would miss that cache on every frame and rebuild the views
    /// and bind groups it exists to keep. Everything here is therefore
    /// constant for as long as the caps and the fitted size are.
    ///
    /// `dst` is the size the picture is drawn at, which is what picks between
    /// the up and the down kernel. It is the renderer that scales now, so
    /// unlike the lane this replaces the kernel actually reaches the picture.
    ///
    /// The deband seed is pinned rather than moved per frame for the cache
    /// reason above, so the grain is a pure function of the input instead of
    /// crawling. It is the one thing that changed meaning in the move.
    fn profile(self, desc: &FrameDesc, dst: (u32, u32)) -> iv::VideoProfile {
        iv::VideoProfile {
            filter: match self.filter((desc.width, desc.height), dst) {
                ScaleFilter::Bilinear => iv::ScaleFilter::Bilinear,
                ScaleFilter::Lanczos3 => iv::ScaleFilter::Lanczos3,
                ScaleFilter::Hermite => iv::ScaleFilter::Hermite,
            },
            dither: self.dither.applies(desc),
            deband: self.deband.map(|d| iv::Deband {
                iterations: d.iterations,
                threshold: d.threshold,
                radius: d.radius,
                grain: d.grain,
                seed: d.seed,
            }),
            tonemap: match desc.tonemap {
                TonemapCurve::Spline => iv::TonemapCurve::Spline,
                TonemapCurve::Bt2390 => iv::TonemapCurve::Bt2390,
            },
        }
    }
}

/// The two vocabularies are deliberately spelled the same, so this is a
/// rename and the compiler checks it stays one: a format added to either side
/// without the other fails to build here rather than at run time. Same shape
/// as the fork's own rename on the far side of the seam.
macro_rules! ingress_format {
    ($($v:ident),* $(,)?) => {
        fn ingress_format(f: PixelFormat) -> iv::PixelFormat {
            match f { $(PixelFormat::$v => iv::PixelFormat::$v,)* }
        }
    };
}

ingress_format!(
    Nv12, Nv21, Nv16, Nv24, P010, P012, P016, I420, Yv12, I420P10, I420P12, I422, I422P10, I422P12,
    I411, I444, I444P10, I444P12, I444P16, A420, Ayuv, Vuya, Rgba, Rgbx, Bgra, Bgrx, Argb, Abgr,
    Xrgb, Xbgr, Rgb, Bgr, Gbr, GbrP10, GbrP12, Gbra,
);

/// The colorimetry half of the same rename. The tone curve is left out: it is
/// a decision about the output, so it rides the profile, not the source.
fn ingress_color(desc: &FrameDesc) -> iv::ColorInfo {
    iv::ColorInfo {
        matrix: match desc.matrix {
            Matrix::Bt601 => iv::Matrix::Bt601,
            Matrix::Bt709 => iv::Matrix::Bt709,
            Matrix::Bt2020Ncl => iv::Matrix::Bt2020Ncl,
            Matrix::Identity => iv::Matrix::Identity,
        },
        range: match desc.range {
            Range::Limited => iv::Range::Limited,
            Range::Full => iv::Range::Full,
        },
        transfer: match desc.transfer {
            Transfer::Sdr => iv::Transfer::Sdr,
            Transfer::Bt1886 => iv::Transfer::Bt1886,
            Transfer::Pq => iv::Transfer::Pq,
            Transfer::Hlg => iv::Transfer::Hlg,
        },
        primaries: match desc.primaries {
            Primaries::Bt709 => iv::Primaries::Bt709,
            Primaries::Bt2020 => iv::Primaries::Bt2020,
            Primaries::Bt601_525 => iv::Primaries::Bt601_525,
            Primaries::Bt601_625 => iv::Primaries::Bt601_625,
            Primaries::DisplayP3 => iv::Primaries::DisplayP3,
        },
        chroma_location: match desc.chroma_location {
            ChromaLocation::Left => iv::ChromaLocation::Left,
            ChromaLocation::Center => iv::ChromaLocation::Center,
        },
        hdr: iv::HdrMetadata {
            max_mastering_nits: desc.hdr.max_mastering_nits,
            max_cll: desc.hdr.max_cll,
        },
        // Derived from the format on the far side too, so it is named rather
        // than defaulted: a default here would describe 8-bit planes for a
        // 10-bit source.
        bits: iv::BitEncoding::of(ingress_format(desc.format)),
    }
}

/// The profile's knobs. One table, no behaviour, so the mapping is
/// inspectable and testable without a device.
fn profile_quality(profile: RenderProfile) -> Quality {
    match profile {
        // pl_render_fast_params leaves every scaler, the dither and the
        // deband null: bilinear off the hardware sampler, nothing else
        RenderProfile::Fast => Quality {
            up: ScaleFilter::Bilinear,
            down: ScaleFilter::Bilinear,
            // pl would not dither here at all. This lane keeps it for tone
            // mapped frames only: fast is the receiver's default and an
            // undithered HDR-to-8-bit write bands where nothing else does.
            dither: DitherRule::HdrOnly,
            deband: None,
        },
        // pl_render_default_params: lanczos up, hermite down, dither on,
        // still no deband
        RenderProfile::Balanced => Quality {
            up: ScaleFilter::Lanczos3,
            down: ScaleFilter::Hermite,
            dither: DitherRule::WiderThanTarget,
            deband: None,
        },
        // pl_render_high_quality_params. Its upscaler is
        // pl_filter_ewa_lanczossharp, a polar kernel this crate does not
        // have; lanczos3 is the sharpest separable one it does, and the
        // downscaler and the deband defaults are pl's exactly.
        RenderProfile::HighQuality => Quality {
            up: ScaleFilter::Lanczos3,
            down: ScaleFilter::Hermite,
            dither: DitherRule::Always,
            deband: Some(DebandParams::default()),
        },
    }
}

/// The container's orientation tag, which is where a phone records that
/// the sensor was sideways. Same tag and same mapping every lane before
/// this one read, so a clip turns the way it always did.
/// It answers in [`iv::BufferTransform`], the orientation the frame declares
/// to slint. Only the four quarter turns are ever produced: the mirrored
/// variants exist in the vocabulary because a compositor applies them for
/// free, but the convert shader generates four position swizzles and no flip,
/// so a frame that declared one would be refused at draw and show nothing at
/// all. Upright is the better answer, and it is the one this lane has always
/// given.
fn rotation_from_tags(tags: &gst::TagListRef) -> Option<iv::BufferTransform> {
    let orientation = tags.get::<gst::tags::ImageOrientation>()?;
    Some(match orientation.get() {
        "rotate-0" => iv::BufferTransform::Normal,
        "rotate-90" => iv::BufferTransform::Rotate90,
        "rotate-180" => iv::BufferTransform::Rotate180,
        "rotate-270" => iv::BufferTransform::Rotate270,
        other => {
            warn!(other, "wgpu video lane: unsupported image-orientation");
            iv::BufferTransform::Normal
        }
    })
}

/// Rotation as it rides the sink's atomic. A `u8` because the tag arrives
/// on the streaming thread and is read by the render on the same thread,
/// but the probe that writes it is a separate closure.
fn rotation_from_code(code: u8) -> iv::BufferTransform {
    match code {
        1 => iv::BufferTransform::Rotate90,
        2 => iv::BufferTransform::Rotate180,
        3 => iv::BufferTransform::Rotate270,
        _ => iv::BufferTransform::Normal,
    }
}

fn rotation_code(rotation: iv::BufferTransform) -> u8 {
    match rotation.rotation_degrees() {
        90 => 1,
        180 => 2,
        270 => 3,
        _ => 0,
    }
}

/// The size a picture is seen at once its turn is applied.
fn turned(transform: iv::BufferTransform, size: (u32, u32)) -> (u32, u32) {
    if transform.swaps_axes() {
        (size.1, size.0)
    } else {
        size
    }
}

/// Everything one caps object settles: how to read the buffers, what the
/// renderer is told about them, the square-pixel size, and the DRM modifier
/// when the frames arrive as dmabufs.
#[derive(Clone)]
struct CapsPlan {
    info: gst_video::VideoInfo,
    desc: FrameDesc,
    /// The shape of one buffer pixel, as the frame declares it to slint.
    par: (u32, u32),
    /// The square-pixel size, still in the buffer's own orientation. Not sent
    /// anywhere: it is what the lane anchors cues against, and slint derives
    /// the same number from the raw size and [`Self::par`].
    size: (u32, u32),
    /// `Some` only for `memory:DMABuf` `DMA_DRM` caps, and then it is the
    /// layout every plane import has to be told.
    modifier: Option<u64>,
}

/// Caps mapping straight off a sample, so the sink path and the tests share
/// one entry point. The display size rides along because it comes off the
/// same caps and is then cached with them.
///
/// `DMA_DRM` caps carry no gst video format of their own, so the real layout
/// is recovered from the `drm-format` field first and everything downstream
/// then works off the ordinary `VideoInfo` that describes.
fn desc_from_caps(caps: &gst::CapsRef) -> Option<CapsPlan> {
    let (info, modifier) = if gst_video::is_dma_drm_caps(caps) {
        let drm = gst_video::VideoInfoDmaDrm::from_caps(caps).ok()?;
        (drm.to_video_info().ok()?, Some(drm.modifier()))
    } else {
        (gst_video::VideoInfo::from_caps(caps).ok()?, None)
    };
    let desc = frame_desc(&info, hdr_metadata(caps))?;
    let par = sane_par(&info);
    let size = display_size((info.width(), info.height()), par);
    Some(CapsPlan {
        info,
        desc,
        par,
        size,
        modifier,
    })
}

// ---------------------------------------------------------------------------
// device and per-frame state
// ---------------------------------------------------------------------------

/// Dep-free executor. wgpu's native futures resolve on the first poll, so the
/// park branch is only a safety net.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::{
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
    };
    struct Parker(std::thread::Thread);
    impl Wake for Parker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Parker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// The gpu device slint's renderer and this lane share, plus the handles the
/// backend selection needs to hand it over. Vulkan on linux and windows,
/// Metal on mac; see [`create_shared_device`].
pub struct SharedDevice {
    pub instance: wgpu::Instance,
    /// `None` when the device was adopted from slint's renderer (the GL
    /// floor): the notifier hands over no adapter. The Manual backend
    /// selection needs one, so only a device opened here takes that route.
    pub adapter: Option<wgpu::Adapter>,
    /// What the device runs on, for the log line and the tests.
    pub info: wgpu::AdapterInfo,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The adapter granted R16Unorm, so the 16-bit-container formats can be
    /// negotiated.
    pub(crate) norm16: bool,
    /// The adapter granted the dmabuf import extensions, so the appsink can
    /// offer `memory:DMABuf` and the decoder's planes reach the gpu unmapped.
    /// False on the GL floor, which has no import route at all.
    pub(crate) dmabuf: bool,
}

/// Set once, after slint accepted the device. Read by every sink built later,
/// which is what puts the lane in zero-copy mode.
static SHARED: OnceLock<SharedDevice> = OnceLock::new();

/// Why no shared device was adopted, when one was not: slint refused it, or
/// the only adapter was a software one. Kept because the selection runs
/// before the log subscriber exists, and a silent fall back to the libplacebo
/// sink is exactly the thing that would go unnoticed.
static REFUSED: OnceLock<String> = OnceLock::new();

/// Records a refusal for the sink to report once logging is up.
pub fn note_shared_device_refused(err: String) {
    let _ = REFUSED.set(err);
}

/// Whether the lane may present on an adapter of this type.
///
/// A CPU adapter (lavapipe, swiftshader) is what wgpu hands back on a box
/// whose only Vulkan driver is a software one: `request_adapter` orders it
/// last but still returns it when nothing else exists, and
/// `force_fallback_adapter: false` does not exclude it. Slint renders on this
/// device too, so taking it early would put the whole UI and the video
/// through a software rasterizer on a machine whose OpenGL driver is hardware
/// (Intel gen7 without hasvk, a VM on virgl, a distribution without the
/// Vulkan ICD). The GL pass behind it is what that machine renders on, and
/// only when that declines too does a software pass take the adapter.
fn adapter_acceptable(device_type: wgpu::DeviceType, allow_software: bool) -> bool {
    allow_software || device_type != wgpu::DeviceType::Cpu
}

/// Whether a software adapter may be taken once every hardware pass declined.
/// On by default: with no other video path in the receiver, a box with only
/// lavapipe or llvmpipe plays slowly rather than not at all.
/// `FCAST_WGPU_SOFTWARE=0` refuses it, for an A/B that wants the decline.
fn software_adapter_allowed() -> bool {
    std::env::var("FCAST_WGPU_SOFTWARE")
        .ok()
        .is_none_or(|v| v != "0")
}

/// The backends tried for an adapter, in order, as [`backend_passes`] resolves
/// them from the `FCAST_WGPU_BACKEND` override.
///
/// Without an override: the platform's own backend first (Vulkan on linux and
/// windows, Metal on mac), then the GL backend as the floor on linux and
/// windows. The floor is what a box whose only Vulkan driver is a software
/// one, or none at all, lands on: its OpenGL driver is hardware, and this is
/// the same driver the lane this replaces ran on there. Not on mac, where the
/// GL backend is not compiled (it would need ANGLE).
///
/// An override names one backend and that is the only pass. An unrecognized
/// value is a typo, not a request; it warns and takes the default list instead
/// of silently leaving the lane adapterless.
fn backend_passes_for(override_: Option<&str>) -> Vec<wgpu::Backends> {
    let default = i_slint_video_wgpu::default_backends();
    let mut passes = vec![default];
    if !cfg!(target_vendor = "apple") {
        passes.push(wgpu::Backends::GL);
    }
    match override_ {
        Some("gl") => vec![wgpu::Backends::GL],
        Some("vulkan") => vec![wgpu::Backends::VULKAN],
        Some("metal") => vec![wgpu::Backends::METAL],
        Some(v) => {
            warn!(
                value = %v,
                "wgpu video lane: unknown FCAST_WGPU_BACKEND, using the platform order"
            );
            passes
        }
        None => passes,
    }
}

fn backend_passes() -> Vec<wgpu::Backends> {
    backend_passes_for(std::env::var("FCAST_WGPU_BACKEND").ok().as_deref())
}

/// Steering glvnd for the GL pass. Linux only, where glvnd is.
///
/// glvnd hands an EGL display to the first vendor library, in file order,
/// that accepts the platform. On a hybrid laptop that is `10_nvidia.json`
/// before `50_mesa.json`, and the NVIDIA EGL accepts the surfaceless and the
/// Wayland platform requests wgpu makes, then dies creating the device: the
/// session, the compositor and VA-API all run on the Mesa integrated GPU and
/// the discrete one is render offload. A GL pass that gets there has already
/// found no hardware Vulkan, so the Mesa device is the only sensible answer,
/// and the pin is the variable glvnd itself reads for exactly this. A value
/// the user set stands.
#[cfg(target_os = "linux")]
mod egl_vendor {
    use std::path::PathBuf;

    /// glvnd's own knob: a colon list of vendor JSONs that replaces its
    /// directory scan.
    pub const FILENAMES: &str = "__EGL_VENDOR_LIBRARY_FILENAMES";
    /// glvnd's other knob, the directories it scans instead of the defaults.
    const DIRS: &str = "__EGL_VENDOR_LIBRARY_DIRS";
    /// Where glvnd looks when [`DIRS`] is unset. The first two are its
    /// compiled default on every distribution, the third is where NixOS
    /// patches it to, the last is a common local install.
    const DEFAULT_DIRS: [&str; 4] = [
        "/etc/glvnd/egl_vendor.d",
        "/usr/share/glvnd/egl_vendor.d",
        "/run/opengl-driver/share/glvnd/egl_vendor.d",
        "/usr/local/share/glvnd/egl_vendor.d",
    ];

    pub fn vendor_dirs() -> Vec<PathBuf> {
        match std::env::var_os(DIRS) {
            Some(v) if !v.is_empty() => std::env::split_paths(&v).collect(),
            _ => DEFAULT_DIRS.iter().map(PathBuf::from).collect(),
        }
    }

    /// The `library_path` of one vendor JSON. No JSON parser: the file is
    /// one object with one key glvnd's own reader wants, and a malformed one
    /// is skipped the way glvnd skips it.
    pub fn library_path(json: &str) -> Option<String> {
        const KEY: &str = "\"library_path\"";
        let rest = &json[json.find(KEY)? + KEY.len()..];
        let rest = &rest[rest.find(':')? + 1..];
        let rest = &rest[rest.find('"')? + 1..];
        Some(rest[..rest.find('"')?].to_string())
    }

    /// The Mesa vendor JSON to pin, when there is a reason to: another vendor
    /// is installed beside it. Alone, or absent, glvnd's own choice stands.
    /// Sorted by path so the answer is the same one every time.
    pub fn mesa_pin(dirs: &[PathBuf]) -> Option<PathBuf> {
        let mut vendors: Vec<(PathBuf, String)> = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|x| x == "json")
                    && let Ok(text) = std::fs::read_to_string(&path)
                    && let Some(lib) = library_path(&text)
                {
                    vendors.push((path, lib));
                }
            }
        }
        vendors.sort();
        let mesa: Vec<&PathBuf> = vendors
            .iter()
            .filter(|(_, lib)| lib.to_ascii_lowercase().contains("mesa"))
            .map(|(path, _)| path)
            .collect();
        if mesa.is_empty() || mesa.len() == vendors.len() {
            return None;
        }
        Some(mesa[0].clone())
    }

    /// Pin glvnd to Mesa before the GL pass opens its first display, when
    /// another vendor is installed beside it.
    ///
    /// Process-global, and glvnd reads it once, on the first EGL call the
    /// process makes. So this has to run before that call, which is why the
    /// lane's device is opened at startup before slint or GStreamer exist;
    /// called later it would be too late to matter and harmless.
    pub fn pin_mesa_on_hybrid() {
        if std::env::var_os(FILENAMES).is_some_and(|v| !v.is_empty()) {
            return;
        }
        let Some(json) = mesa_pin(&vendor_dirs()) else {
            return;
        };
        tracing::info!(
            vendor = %json.display(),
            "wgpu video lane: GL pass pinned to the Mesa EGL vendor"
        );
        // SAFETY: the environment is written at startup, on the main thread,
        // before the runtime or slint have spawned anything that reads it.
        // The one caller is `create_shared_device`, whose contract this is.
        unsafe { std::env::set_var(FILENAMES, &json) };
    }
}

/// Opens the lane's gpu device. Also the device slint renders on when the
/// caller hands it over with [`adopt_shared_device`].
///
/// One pass per backend in [`backend_passes`] order, and the first pass that
/// yields a hardware adapter with a working device wins. A pass with no
/// adapter, a software one, or a device that refuses to open is declined
/// and the next backend is tried, so a box whose Vulkan is lavapipe or
/// missing lands on the GL backend rather than on nothing. `None` only when
/// every pass declined, and then the reasons are kept for the sink to report
/// once logging is up.
///
/// Called at startup, before slint and before any other thread: the GL pass
/// steers glvnd through the environment (see [`egl_vendor`]).
///
/// LowPower on purpose: the integrated adapter is where slint rendered
/// before, it is where VA-API decode lands on a hybrid box, and sharing one
/// device means the UI and the video must agree on it anyway. A single-GPU
/// box gets that GPU either way. `WGPU_POWER_PREF` overrides, same variable
/// slint reads.
pub fn create_shared_device() -> Option<SharedDevice> {
    let mut declined = Vec::new();
    for (backends, allow_software) in device_passes(software_adapter_allowed()) {
        #[cfg(target_os = "linux")]
        if backends == wgpu::Backends::GL {
            egl_vendor::pin_mesa_on_hybrid();
        }
        match open_on(backends, allow_software) {
            Ok(shared) => return Some(shared),
            Err(why) => {
                // Lost when this runs before the subscriber; the sink
                // reports the collected reasons.
                warn!(%why, ?backends, "wgpu video lane: backend pass declined");
                declined.push(format!("{backends:?}: {why}"));
            }
        }
    }
    note_shared_device_refused(declined.join("; "));
    None
}

/// The passes [`create_shared_device`] makes, in order: every backend on a
/// hardware adapter first, and only then the same backends again taking a
/// software one, so a box whose OpenGL driver is hardware never runs the UI
/// on a rasterizer because its Vulkan driver is not.
fn device_passes(software: bool) -> Vec<(wgpu::Backends, bool)> {
    let hardware = backend_passes();
    let mut passes: Vec<_> = hardware.iter().map(|b| (*b, false)).collect();
    if software {
        passes.extend(hardware.iter().map(|b| (*b, true)));
    }
    passes
}

/// One backend pass of [`create_shared_device`]: the adapter, the gate on its
/// type, and the device. The error is the reason, for the log.
fn open_on(backends: wgpu::Backends, allow_software: bool) -> Result<SharedDevice, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    // No compatible_surface: no window exists yet. Every driver that presents
    // at all presents from its only graphics queue, so the surface slint
    // creates later fits.
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference:
            wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower),
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .map_err(|err| format!("no adapter: {err}"))?;
    let info = adapter.get_info();
    if !adapter_acceptable(info.device_type, allow_software) {
        return Err(format!(
            "only a software adapter ({} on {:?})",
            info.name, info.backend
        ));
    }
    // Every 16-bit-container format rides one device feature, and the crate
    // is the authority on which. P010 stands in for the whole class.
    let wanted =
        i_slint_video_wgpu::required_device_features(&[PixelFormat::P010, PixelFormat::I444P16]);
    let mut features = adapter.features() & wanted;
    // Importing the decoder's dmabufs needs VK_EXT_external_memory_dma_buf and
    // VK_EXT_image_drm_format_modifier, which wgpu grants as one feature. It
    // costs nothing when the lane never imports, so it is taken whenever the
    // adapter has it. Never granted off vulkan, and the import module is not
    // compiled off linux, so the cfg is what keeps the two in step.
    let dmabuf = cfg!(target_os = "linux")
        && adapter
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF);
    if dmabuf {
        features |= wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
    }
    // Defaults, with the resolution limits lifted to the adapter's, the way
    // slint's own wgpu init does it: the swapchain and a 4K video frame both
    // have to fit.
    let limits = wgpu::Limits::default().using_resolution(adapter.limits());
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("fcast-desktop-video"),
        required_features: features,
        required_limits: limits,
        ..Default::default()
    }))
    .map_err(|err| format!("device creation failed on {}: {err}", info.name))?;
    install_device_error_handlers(&device);
    let norm16 = features.contains(wanted);
    info!(
        adapter = %info.name,
        backend = ?info.backend,
        device_type = ?info.device_type,
        norm16,
        dmabuf,
        "wgpu video lane: device opened"
    );
    Ok(SharedDevice {
        instance,
        adapter: Some(adapter),
        info,
        device,
        queue,
        norm16,
        dmabuf,
    })
}

/// Set when the GL floor was chosen: slint opens the device on the window's
/// display and hands it to [`adopt_renderer_device`] at its first
/// `RenderingSetup`, and until then the lane has no device to build on.
static ADOPTION_PENDING: AtomicBool = AtomicBool::new(false);

/// Whether the device the startup pass opened is the GL floor, where slint has
/// to open the presenting device itself. See the module docs.
pub fn is_gl_floor(shared: &SharedDevice) -> bool {
    shared.info.backend == wgpu::Backend::Gl
}

/// The settings slint opens the GL floor's device with: the GL backend BY
/// NAME (the renderer avoids it otherwise), the same power preference as the
/// startup pass, and the 16-bit norm feature when the pass's adapter granted
/// it. The pass proved the adapter; this asks for its twin on the window's
/// display.
pub fn gl_floor_settings(shared: &SharedDevice) -> slint::wgpu_30::WGPUSettings {
    let mut settings = slint::wgpu_30::WGPUSettings::default();
    settings.backends = wgpu::Backends::GL;
    settings.power_preference =
        wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
    settings.device_required_features = if shared.norm16 {
        i_slint_video_wgpu::required_device_features(&[PixelFormat::P010, PixelFormat::I444P16])
    } else {
        wgpu::Features::empty()
    };
    settings.device_label = Some("fcast-desktop-video".into());
    settings
}

/// Tells the lane that slint will hand over the device it opens, so the sink
/// waits for it instead of declining before it exists.
pub fn expect_renderer_device() {
    ADOPTION_PENDING.store(true, Ordering::Release);
}

/// Whether a promised renderer device has not arrived yet.
pub fn adoption_pending() -> bool {
    ADOPTION_PENDING.load(Ordering::Acquire) && SHARED.get().is_none()
}

/// Waits for the renderer's device when one was promised, for at most
/// `timeout`. The window is up within a frame of startup, so this is a few
/// milliseconds; a window that never comes up (a tray-only start) leaves the
/// promise unmet and the lane then declines the way it does with no device.
pub async fn await_renderer_device(timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while adoption_pending() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if adoption_pending() {
        warn!(
            "wgpu video lane: the renderer never handed over its device, playing without video"
        );
    }
}

/// Adopts the device slint's renderer opened, from the `RenderingSetup`
/// notifier. Only acts while a device is expected: the Manual path published
/// its own device before slint ever saw it, and this leaves that alone.
///
/// The error handlers go on here for the same reason they do in [`open_on`]:
/// wgpu's default panics, and this device is used from the streaming thread
/// too.
pub fn adopt_renderer_device(
    instance: &wgpu::Instance,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) {
    if !ADOPTION_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    let wanted =
        i_slint_video_wgpu::required_device_features(&[PixelFormat::P010, PixelFormat::I444P16]);
    let features = device.features();
    let norm16 = features.contains(wanted);
    let dmabuf = cfg!(target_os = "linux")
        && features.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF);
    install_device_error_handlers(device);
    let info = device.adapter_info();
    info!(
        adapter = %info.name,
        backend = ?info.backend,
        device_type = ?info.device_type,
        norm16,
        dmabuf,
        "wgpu video lane: adopted the device slint opened on the window's display"
    );
    adopt_shared_device(SharedDevice {
        instance: instance.clone(),
        adapter: None,
        info,
        device: device.clone(),
        queue: queue.clone(),
        norm16,
        dmabuf,
    });
}

/// Set when the driver dropped the device. Nothing recreates it, so the lane
/// stops presenting and the view falls back to its idle state instead of
/// pushing handles to a dead GPU.
static DEVICE_LOST: AtomicBool = AtomicBool::new(false);

/// Test hook: refuse the next N dmabuf imports, so a test can drive the lane
/// into the state a real driver refusal leaves it in without a driver that
/// refuses.
#[cfg(test)]
static FAIL_IMPORTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// wgpu's default uncaptured-error handler panics, and this device is used
/// from the streaming thread inside an appsink callback and from the event
/// loop by slint's renderer. A panic on either is an unrecoverable process
/// exit (the `release-small` profile is `panic = "abort"`, which also compiles
/// out the appsink trampoline's `catch_unwind`), so errors are logged instead
/// and the lane degrades. Device loss additionally stops the lane, since no
/// later frame can succeed on a lost device.
fn install_device_error_handlers(device: &wgpu::Device) {
    // One line per distinct message: a bad pipeline state repeats per frame.
    let last: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
    device.on_uncaptured_error(Arc::new(move |err: wgpu::Error| {
        let msg = err.to_string();
        let mut last = last.lock();
        if last.as_deref() != Some(msg.as_str()) {
            error!(err = %msg, "wgpu video lane: device error");
            *last = Some(msg);
        }
    }));
    device.set_device_lost_callback(|reason, msg| {
        // Destroyed is our own teardown, only Unknown is a driver loss.
        if matches!(reason, wgpu::DeviceLostReason::Destroyed) {
            return;
        }
        DEVICE_LOST.store(true, Ordering::Release);
        error!(%msg, "wgpu video lane: device lost, stopping presentation");
    });
}

/// Publishes the device slint accepted, so the sinks built later import their
/// planes on the device the scene is drawn with.
///
/// Called only after `BackendSelector::select` succeeded: a device slint
/// refused must not start the lane, or it would hand out plane textures the
/// renderer cannot sample.
pub fn adopt_shared_device(shared: SharedDevice) {
    if SHARED.set(shared).is_err() {
        warn!("wgpu video lane: shared device published twice, keeping the first");
    }
}

/// Whether the shared-device lane is live, which is what makes the gpu
/// cover downscale below possible.
pub fn has_shared_device() -> bool {
    SHARED.get().is_some()
}

/// Renderer and staging buffer for [`downscale_cover`], kept so a track
/// change after the first pays no pipeline compile and no staging alloc.
static COVER_GPU: parking_lot::Mutex<Option<(Renderer, i_slint_video_wgpu::Readback)>> =
    parking_lot::Mutex::new(None);

/// Antialiased downscale of a decoded cover on the shared device, feeding
/// the audio-cover blur. The full-res upload and lanczos reduction are the
/// part that costs real time at 4K+; the caller blurs the small result on
/// the cpu in microseconds. None when the lane has no device or the render
/// fails, and the caller falls back to the cpu downscale.
pub fn downscale_cover(
    img: &receiver_core::image::RgbaImage,
    thumb_dim: u32,
) -> Option<receiver_core::image::RgbaImage> {
    let shared = SHARED.get()?;
    let mut cache = COVER_GPU.lock();
    downscale_cover_on(&shared.device, &shared.queue, &mut cache, img, thumb_dim)
        .map_err(|err| warn!(?err, "gpu cover downscale failed, cpu fallback"))
        .ok()
}

fn downscale_cover_on(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    cache: &mut Option<(Renderer, i_slint_video_wgpu::Readback)>,
    img: &receiver_core::image::RgbaImage,
    thumb_dim: u32,
) -> Result<receiver_core::image::RgbaImage, i_slint_video_wgpu::VideoError> {
    let (w, h) = img.dimensions();
    let long = w.max(h).max(1);
    let (tw, th) = if long > thumb_dim {
        ((w * thumb_dim / long).max(1), (h * thumb_dim / long).max(1))
    } else {
        (w.max(1), h.max(1))
    };
    let desc = FrameDesc {
        // Rgbx sidesteps the chain's straight-in premultiplied-out alpha
        // contract, the blurred background is opaque either way
        format: PixelFormat::Rgbx,
        matrix: Matrix::Identity,
        range: Range::Full,
        transfer: Transfer::Sdr,
        tonemap: TonemapCurve::default(),
        primaries: Primaries::Bt709,
        chroma_location: ChromaLocation::default(),
        width: w,
        height: h,
        hdr: HdrMetadata::default(),
    };
    let out = i_slint_video_wgpu::OutputDesc {
        width: tw,
        height: th,
        filter: ScaleFilter::Lanczos3,
        dither: false,
        rotation: i_slint_video_wgpu::Rotation::Rotate0,
        deband: None,
    };
    let (renderer, readback) =
        cache.get_or_insert_with(|| (Renderer::new(device), Default::default()));
    let mut bytes = vec![0u8; tw as usize * 4 * th as usize];
    let res = (|| {
        let frame = renderer.upload(device, queue, desc, &[img.as_raw()], &[w * 4])?;
        let tex = renderer.render(device, queue, &frame, out)?;
        i_slint_video_wgpu::gpu::read_rgba8_into(device, queue, &tex, tw, th, readback, &mut bytes)
    })();
    // A cover renders once, so drop the bind groups now or the renderer's
    // 48-entry bind cache pins this full-res texture through the next 47
    // covers, and drop the chain's source-sized Rgba16Float scratch or it
    // stays resident until a differently-sized cover swaps it (134MB after
    // a 4096px cover). Unconditional so an error path cannot pin either.
    renderer.forget_binds();
    renderer.forget_scratch();
    res?;
    Ok(receiver_core::image::RgbaImage::from_raw(tw, th, bytes)
        .expect("buffer sized to tw*4*th above"))
}

// ---------------------------------------------------------------------------
// the frame handed to slint
// ---------------------------------------------------------------------------

/// The producer's content counter. Monotonic for the life of the process,
/// which is what the ingress contract asks for and what makes a pooled import
/// tell two pictures apart: a decoder hands the same dmabuf back a few frames
/// later, so the plane textures are identical handles and only this separates
/// them. Without it a settled stream would freeze on its first picture,
/// because slint's handle compares equal and a property that compares equal
/// never marks dirty.
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// The storage one in-flight frame borrows, recycled through [`FramePool`].
///
/// Two arms live in one struct because a stream is one arm at a time and the
/// slot outlives neither: [`Slot::upload`] is the sysmem arm's plane textures,
/// kept so `upload_into` rewrites them instead of reallocating, and
/// [`Slot::buffer`] is the import arm's decoder buffer, kept because the
/// planes read the decoder's own memory rather than a copy of it.
#[derive(Default)]
struct Slot {
    /// The planes as the ingress contract wants them: one texture per plane
    /// of the format, in plane order. Refcount clones of whichever arm
    /// produced them, so the slice is contiguous and the frame owns what it
    /// hands out.
    planes: Vec<wgpu::Texture>,
    /// The sysmem arm's own plane set. `None` on the import arm.
    upload: Option<i_slint_video_wgpu::Frame>,
    /// The decoder's buffer, on the import arm. Dropping it is what hands it
    /// back to the pool, and that happens when slint drops the frame.
    buffer: Option<gst::Buffer>,
}

/// Free slots waiting to carry the next frame.
///
/// Depth is bounded by the sink's one-frame-in-flight rule plus the three
/// generations slint holds a frame for, so the steady state settles at four
/// or five and the cap is slack on top of that. Past it a returned slot is
/// dropped instead of kept, which is what stops a stalled event loop from
/// pinning plane textures forever.
const MAX_POOLED_SLOTS: usize = 8;

/// Recycles [`Slot`]s between the streaming thread that fills them and the UI
/// thread that drops them.
///
/// A plain mutex over a vector: it is taken twice per frame, uncontended, and
/// the vector keeps its capacity, so the steady state is two uncontended
/// atomics and no allocation.
#[derive(Default)]
struct FramePool {
    free: parking_lot::Mutex<Vec<Slot>>,
}

impl FramePool {
    fn take(&self) -> Slot {
        self.free.lock().pop().unwrap_or_default()
    }

    /// Put a slot back, dropping what it borrowed. The plane handles and the
    /// decoder's buffer go now; the uploaded plane textures stay, because
    /// they are what the next frame of the same stream writes into.
    fn put(&self, mut slot: Slot) {
        slot.planes.clear();
        slot.buffer = None;
        let mut free = self.free.lock();
        if free.len() < MAX_POOLED_SLOTS {
            free.push(slot);
        }
    }

    /// Drop every free slot, and with them the plane textures they hold. For
    /// a caps change, where the geometry they were sized for is gone.
    fn clear(&self) {
        self.free.lock().clear();
    }
}

/// One frame on its way to the UI thread. `Send`, which the frame object
/// itself is not: the release callbacks it grows there are plain
/// `Box<dyn FnOnce()>`, so it is built where they are registered and fired.
struct FramePayload {
    /// `None` only after [`Drop`] took it.
    slot: Option<Slot>,
    pool: Arc<FramePool>,
    /// The whole source description. The ingress vocabulary is derived from
    /// it rather than stored twice, since both are plain matches on data the
    /// caps already settled.
    desc: FrameDesc,
    transform: iv::BufferTransform,
    par: (u32, u32),
    generation: u64,
}

impl Drop for FramePayload {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.pool.put(slot);
        }
    }
}

/// One decoded frame, described for slint.
///
/// Built on the UI thread out of a [`FramePayload`] and never sent anywhere:
/// slint holds it until the last submit that read its planes has retired and
/// then drops it, which is what returns the slot and the decoder's buffer.
struct SinkFrame {
    payload: FramePayload,
    /// Registered by a consumer that wants to know before the frame is
    /// dropped. Nothing on this lane does (slint's release IS the drop), so
    /// this is empty in every real run, but the contract says the callbacks
    /// fire exactly once and in registration order, so they do.
    release: std::cell::RefCell<Vec<Box<dyn FnOnce()>>>,
}

impl SinkFrame {
    fn new(payload: FramePayload) -> Self {
        Self {
            payload,
            release: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl Drop for SinkFrame {
    fn drop(&mut self) {
        // Before the payload, so a callback that inspects the producer's
        // state sees it as it was while the frame was alive.
        for cb in self.release.borrow_mut().drain(..) {
            cb();
        }
    }
}

impl iv::VideoFrame for SinkFrame {
    fn size(&self) -> iv::Size {
        // Raw buffer pixels: the planes are coded sized, and correcting here
        // would describe a texture that does not exist.
        iv::Size::new(self.payload.desc.width, self.payload.desc.height)
    }

    fn transform(&self) -> iv::BufferTransform {
        self.payload.transform
    }

    fn pixel_aspect_ratio(&self) -> (u32, u32) {
        self.payload.par
    }

    fn fit(&self) -> iv::Fit {
        // The `Image` element's own `image-fit` is what places this lane's
        // picture; the answer here is what the compositor lane would use.
        iv::Fit::Contain
    }

    fn as_native_buffer(&self) -> Option<iv::NativeBuffer<'_>> {
        // The platform import already happened, on the device slint renders
        // with, so there is nothing left to hand over untouched.
        None
    }

    fn source(&self) -> iv::FrameSource<'_> {
        let Some(slot) = self.payload.slot.as_ref() else {
            return iv::FrameSource::None;
        };
        iv::FrameSource::Wgpu30 {
            planes: &slot.planes,
            format: ingress_format(self.payload.desc.format),
            generation: self.payload.generation,
        }
    }

    fn color(&self) -> iv::ColorInfo {
        ingress_color(&self.payload.desc)
    }

    fn overlay_count(&self) -> usize {
        // Cues are drawn by the renderer's own overlay slot and bitmap
        // subtitles by a scene image, both over the picture rather than in
        // it, so nothing is baked into the frame.
        0
    }

    fn overlay(&self, index: usize) -> iv::Overlay<'_> {
        unreachable!("overlay {index} asked for on a frame that declares none")
    }

    fn on_release(&self, cb: Box<dyn FnOnce()>) {
        self.release.borrow_mut().push(cb);
    }
}

/// Everything the streaming thread reuses across frames. The upload arm's
/// plane sets, the import arm's cache, and the slot pool the two hand their
/// planes to. Nothing here is built per frame once a stream has settled,
/// which is what leaves the steady state allocating almost nothing of the
/// lane's own on either arm.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Kept for its upload entry points only. Nothing on this lane renders
    /// any more, so its pass caches and its scratch slots stay empty; it is
    /// two samplers and a device limit.
    renderer: Renderer,
    /// Slots recycled between the frames in flight. Shared with the frames
    /// themselves, which return their slot from the UI thread when slint
    /// drops them.
    pool: Arc<FramePool>,
    /// Last parsed caps and what they mapped to. GStreamer hands the same
    /// caps object to every frame of a stream and `gst_caps_is_equal`
    /// short-circuits on that pointer, so the steady state costs one compare
    /// instead of a VideoInfo parse plus two metadata string parses a frame.
    /// Held as an owned ref, not a raw address, so a freed caps object cannot
    /// be mistaken for the cached one at the same allocation. The pixel
    /// aspect correction is cached with them for the same reason.
    ///
    /// Behind an `Arc` so a frame handing the plan to the render costs one
    /// refcount instead of copying a `GstVideoInfo` out of the cache.
    caps: Option<(gst::Caps, Arc<CapsPlan>)>,
    /// P010 needs R16Unorm, which is a device feature, so the caps drop it
    /// when the adapter refused.
    norm16: bool,
    /// The DRM modifiers the device can import, per pixel format. Empty when
    /// the adapter has no dmabuf import, which is also what leaves the
    /// `memory:DMABuf` structures out of the offer.
    dmabuf: Vec<(PixelFormat, Vec<u64>)>,
    /// Whether the appsink still advertises the `memory:DMABuf` structures.
    /// Cleared the first time an import fails, which together with the
    /// reconfigure event the sink pushes is how upstream is asked for system
    /// memory instead.
    ///
    /// It does not gate the import itself. A VA decoder ignores a downstream
    /// reconfigure once its input state is settled (`gst_va_base_dec_negotiate`
    /// returns early unless the input caps changed), so latching the import off
    /// as well would leave a stream that keeps arriving as dmabufs with no
    /// route at all. Retrying every frame instead means a transient failure
    /// recovers on its own and a permanent one only ever drops frames.
    offer_dmabuf: bool,
    /// The pixel formats a software decoder can be handed a udmabuf pool for:
    /// everything the system-memory offer lists whose planes this device can
    /// sample through the LINEAR modifier, which is the only layout plain
    /// pages can have. Empty when the adapter has no dmabuf import, when the
    /// arm is switched off, or off linux.
    ///
    /// Read by the appsink's `propose_allocation` (through the copy the sink
    /// holds) and per frame by the import detection, so it is a plain list of
    /// at most five entries rather than anything that allocates to search.
    #[cfg(target_os = "linux")]
    udmabuf: Arc<[PixelFormat]>,
    /// Set when a udmabuf-backed frame failed to import, which stops the lane
    /// retrying it per frame. Unlike the `DMA_DRM` route this needs no
    /// renegotiation: the caps are already system memory and the fallback is
    /// the upload arm the buffer would have taken anyway.
    #[cfg(target_os = "linux")]
    linear_import_off: bool,
    /// The pixel formats a VideoToolbox surface can be imported as on mac:
    /// NV12, and P010 when the device took the 16-bit norm feature. Empty
    /// when the arm is switched off or the device is not metal, and then
    /// every frame takes the upload arm.
    ///
    /// Read per frame by the import detection, so it is a list of at most two
    /// entries rather than anything that allocates to search.
    #[cfg(target_os = "macos")]
    iosurface: Arc<[PixelFormat]>,
    /// Set when an IOSurface frame failed to import, which stops the lane
    /// retrying it per frame. No renegotiation here either: the caps say
    /// system memory already and the fallback is the map and upload the
    /// buffer would have taken anyway.
    #[cfg(target_os = "macos")]
    iosurface_import_off: bool,
    /// The last caps the lane had no render path for, so the drop is reported
    /// once per stream instead of once per frame.
    unmappable: Option<gst::Caps>,
    /// What the inspector's stream card was last told, so it is re-published
    /// only when the caps, the orientation or the route moved.
    announced: Option<(Arc<CapsPlan>, iv::BufferTransform, &'static str)>,
    /// Import failures on this stream, for the one-shot log and the tests.
    import_fails: u64,
    /// Imported dmabuf planes, keyed on the buffer they came from. The
    /// decoder cycles a fixed pool, so after its first pass every frame is a
    /// cache hit and the route allocates nothing on either the cpu or the
    /// gpu. See [`crate::desktop_wgpu_dmabuf::ImportCache`].
    #[cfg(target_os = "linux")]
    imports: crate::desktop_wgpu_dmabuf::ImportCache,
    /// The same, for the surfaces VideoToolbox cycles.
    /// See [`crate::desktop_wgpu_iosurface::ImportCache`].
    #[cfg(target_os = "macos")]
    imports: crate::desktop_wgpu_iosurface::ImportCache,
    /// Frames taken by each route, for the log line and the tests.
    #[cfg(target_os = "linux")]
    dmabuf_frames: u64,
    #[cfg(target_os = "macos")]
    iosurface_frames: u64,
    sysmem_frames: u64,
    /// One error line per desc change instead of one per frame.
    last_error: Option<String>,
    /// The render profile's knobs, fixed at startup the way the libplacebo
    /// lane fixes its preset struct.
    quality: Quality,
}

/// The DRM modifiers the device can import for each format the appsink will
/// offer as a dmabuf. Empty off linux, without the adapter feature, or with
/// the lane's dmabuf switch turned off, and then the offer is system memory
/// only and every frame takes the upload path.
#[cfg(target_os = "linux")]
fn import_table(device: &wgpu::Device, dmabuf: bool, norm16: bool) -> Vec<(PixelFormat, Vec<u64>)> {
    if !dmabuf || !crate::desktop_wgpu_dmabuf::enabled() {
        return Vec::new();
    }
    // I420 is left out on purpose. Nothing that decodes into a dmabuf hands
    // out three planes, and offering it only widens the caps every upstream
    // element has to intersect.
    let mut wanted = vec![PixelFormat::Nv12];
    if norm16 {
        wanted.push(PixelFormat::P010);
    }
    let mut out = Vec::new();
    for format in wanted {
        let modifiers = crate::desktop_wgpu_dmabuf::importable_modifiers(device, format);
        if modifiers.is_empty() {
            warn!(?format, "wgpu video lane: no importable drm modifier");
            continue;
        }
        info!(
            ?format,
            modifiers = ?modifiers.iter().map(|m| format!("{m:#x}")).collect::<Vec<_>>(),
            "wgpu video lane: dmabuf import offered"
        );
        out.push((format, modifiers));
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn import_table(_: &wgpu::Device, _: bool, _: bool) -> Vec<(PixelFormat, Vec<u64>)> {
    Vec::new()
}

/// The formats a software decoder can be offered a udmabuf pool for.
///
/// Wider than [`import_table`] on purpose. That one lists what a decoder can
/// be asked to ALLOCATE as a dmabuf, where three planes and exotic layouts buy
/// nothing; this one lists what the lane can IMPORT once a pool has already
/// produced it, and a udmabuf is always linear, so the only question is
/// whether the device samples the format's planes through modifier 0. That
/// takes in the planar wide formats, which is exactly what a software AV1 or
/// HEVC decoder emits for HDR content.
///
/// Empty when the adapter has no dmabuf import at all, which is also what
/// leaves the proposal unmade and every software frame on the upload arm.
#[cfg(target_os = "linux")]
fn udmabuf_table(
    device: &wgpu::Device,
    dmabuf: bool,
    formats: &[gst_video::VideoFormat],
) -> Arc<[PixelFormat]> {
    if !dmabuf
        || !crate::desktop_wgpu_dmabuf::enabled()
        || !crate::desktop_wgpu_udmabuf::enabled()
        || !crate::desktop_wgpu_udmabuf::available()
    {
        return Arc::from([]);
    }
    let out: Vec<PixelFormat> = formats
        .iter()
        .filter_map(|f| map_format(*f))
        .filter(|f| crate::desktop_wgpu_dmabuf::importable_modifiers(device, *f).contains(&0))
        .collect();
    if out.is_empty() {
        warn!("wgpu video lane: no format imports linearly, no udmabuf pool will be proposed");
    } else {
        info!(formats = ?out, "wgpu video lane: udmabuf pool offered to software decoders");
    }
    Arc::from(out)
}

/// The formats a VideoToolbox surface can be imported as.
///
/// There is no caps side to this one. `vtdec` hands the same
/// `GstAppleCoreVideoMemory` out whether the caps say `memory:IOSurface` or
/// plain system memory, so the lane leaves its offer alone and asks the
/// memory itself (see [`crate::desktop_wgpu_iosurface`]); this list is only
/// what the import can then describe to the renderer.
///
/// Empty when the arm is switched off or the device is not metal, and then
/// every frame takes the upload arm.
#[cfg(target_os = "macos")]
fn iosurface_table(device: &wgpu::Device, norm16: bool) -> Arc<[PixelFormat]> {
    if !crate::desktop_wgpu_iosurface::enabled() {
        return Arc::from([]);
    }
    if unsafe { device.as_hal::<wgpu::hal::api::Metal>() }.is_none() {
        warn!("wgpu video lane: not a metal device, no iosurface import");
        return Arc::from([]);
    }
    let out: Vec<PixelFormat> = [PixelFormat::Nv12, PixelFormat::P010]
        .into_iter()
        .filter(|f| crate::desktop_wgpu_iosurface::importable(*f, norm16))
        .collect();
    info!(formats = ?out, "wgpu video lane: importing videotoolbox surfaces");
    Arc::from(out)
}

/// One `memory:DMABuf` structure listing the `fourcc:modifier` pairs the
/// device can import for this format.
#[cfg(target_os = "linux")]
fn drm_caps(format: PixelFormat, modifiers: &[u64]) -> Option<gst::Caps> {
    let strings = crate::desktop_wgpu_dmabuf::drm_format_strings(format, modifiers);
    if strings.is_empty() {
        return None;
    }
    Some(
        gst_video::VideoCapsBuilder::new()
            .features([gst_allocators::CAPS_FEATURE_MEMORY_DMABUF])
            .format(gst_video::VideoFormat::DmaDrm)
            .field(
                "drm-format",
                gst::List::new(strings.iter().map(|s| s.as_str())),
            )
            .build(),
    )
}

#[cfg(not(target_os = "linux"))]
fn drm_caps(_: PixelFormat, _: &[u64]) -> Option<gst::Caps> {
    None
}

/// Whether software frames can arrive as dmabufs on this box, for the lane's
/// one startup line. False everywhere but linux, where the allocator lives.
fn udmabuf_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::desktop_wgpu_udmabuf::enabled() && crate::desktop_wgpu_udmabuf::available()
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// The system-memory format list, free of the `Gpu` so the udmabuf table can
/// be built before one exists. See [`Gpu::formats`] for what the order means.
///
/// Every raw format the old libplacebo sink accepted, so no content regresses
/// to the accept-and-drop black screen. Preferred formats lead, then the rest
/// grouped by layout class. Formats in a 16-bit container ride the norm16
/// device feature.
fn sysmem_formats(norm16: bool) -> Vec<gst_video::VideoFormat> {
    use gst_video::VideoFormat as F;
    let mut v = vec![
        F::Nv12,
        F::I420,
        F::Nv21,
        F::Yv12,
        F::Nv16,
        F::Nv24,
        F::Y41b,
        F::Y42b,
        F::Y444,
        F::A420,
        F::Ayuv,
        F::Vuya,
        F::Rgba,
        F::Rgbx,
        F::Bgra,
        F::Bgrx,
        F::Argb,
        F::Abgr,
        F::Xrgb,
        F::Xbgr,
        F::Rgb,
        F::Bgr,
        F::Gbr,
        F::Gbra,
    ];
    if norm16 {
        v.extend([
            F::P01010le,
            F::P012Le,
            F::P016Le,
            F::I42010le,
            F::I42012le,
            F::I42210le,
            F::I42212le,
            F::Y44410le,
            F::Y44412le,
            F::Y44416le,
            F::Gbr10le,
            F::Gbr12le,
        ]);
    }
    v
}

impl Gpu {
    /// The lane runs only on the device slint renders with. Without one
    /// there is nothing to hand plane textures to: a machine that refused the
    /// shared device has slint on dodvg's OpenGL executor, which draws
    /// nothing at all for a video image, so the honest answer is `None` and
    /// the player runs with no video sink.
    fn new(quality: Quality) -> Option<Self> {
        let Some(shared) = SHARED.get() else {
            match REFUSED.get() {
                Some(err) => warn!(
                    %err,
                    "wgpu video lane: no shared device to present on, playing without video"
                ),
                None => info!(
                    "wgpu video lane: no shared device, playing without video"
                ),
            }
            return None;
        };
        let info = &shared.info;
        info!(
            adapter = %info.name,
            backend = ?info.backend,
            norm16 = shared.norm16,
            dmabuf = shared.dmabuf,
            udmabuf = udmabuf_available(),
            "wgpu video lane: presenting planes on the device slint renders with"
        );
        Some(Self::build(
            shared.device.clone(),
            shared.queue.clone(),
            shared.norm16,
            shared.dmabuf,
            quality,
        ))
    }

    fn build(
        device: wgpu::Device,
        queue: wgpu::Queue,
        norm16: bool,
        dmabuf: bool,
        quality: Quality,
    ) -> Self {
        #[cfg(target_os = "linux")]
        let udmabuf = udmabuf_table(&device, dmabuf, &sysmem_formats(norm16));
        #[cfg(target_os = "macos")]
        let iosurface = iosurface_table(&device, norm16);
        Self {
            renderer: Renderer::new(&device),
            dmabuf: import_table(&device, dmabuf, norm16),
            #[cfg(target_os = "linux")]
            udmabuf,
            #[cfg(target_os = "linux")]
            linear_import_off: false,
            #[cfg(target_os = "macos")]
            iosurface,
            #[cfg(target_os = "macos")]
            iosurface_import_off: false,
            device,
            queue,
            pool: Arc::new(FramePool::default()),
            caps: None,
            norm16,
            offer_dmabuf: true,
            unmappable: None,
            announced: None,
            import_fails: 0,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            imports: Default::default(),
            #[cfg(target_os = "linux")]
            dmabuf_frames: 0,
            #[cfg(target_os = "macos")]
            iosurface_frames: 0,
            sysmem_frames: 0,
            last_error: None,
            quality,
        }
    }

    /// Formats the appsink offers in system memory. The wide ones ride on
    /// the device feature. Order is preference order: 8-bit first so an
    /// 8-bit decoder never picks a wide layout, then the 10-bit pair, then
    /// 12-bit.
    ///
    /// This list is also what a mac negotiates on, and it intersects
    /// VideoToolbox: vtdec's raw src template is
    /// `{NV12, AYUV64, ARGB64_BE, P010_10LE, AV12, BGRA, ARGB}`, so NV12
    /// carries 8-bit and P010 carries 10-bit. I420 and the planar wide
    /// formats are not in that template and are there for the software
    /// decoders, which is the same division as on linux. The GL and IOSurface
    /// structures of vtdec's template intersect empty against this offer, so
    /// it settles on plain system memory and never drags a GstGLContext into
    /// the pipeline.
    ///
    /// Settling on system memory does not cost the mac its zero copy. The
    /// buffers behind these caps are still CVPixelBuffers, and the lane
    /// imports their IOSurfaces off the memory rather than the caps feature:
    /// see [`crate::desktop_wgpu_iosurface`] for why that is the shape the
    /// offer keeps.
    fn formats(&self) -> Vec<gst_video::VideoFormat> {
        sysmem_formats(self.norm16)
    }

    /// The system-memory half of the offer, which is also what the appsink is
    /// narrowed to after a refused import.
    fn sysmem_caps(&self) -> gst::Caps {
        gst_video::VideoCapsBuilder::new()
            .format_list(self.formats())
            .build()
    }

    /// The whole offer. The `memory:DMABuf` structures come first so a VA-API
    /// decoder settles on one and hands out fds; the system-memory structure
    /// stays behind them for software decoders, and is all that is left once
    /// [`Self::offer_dmabuf`] is cleared.
    fn caps(&self) -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        if self.offer_dmabuf {
            let out = caps.get_mut().expect("freshly built, uniquely owned");
            for (format, modifiers) in &self.dmabuf {
                if let Some(drm) = drm_caps(*format, modifiers) {
                    out.append(drm);
                }
            }
        }
        caps.get_mut()
            .expect("freshly built, uniquely owned")
            .append(self.sysmem_caps());
        caps
    }

    /// Map the sample's caps, reusing the last result while the stream's caps
    /// object is unchanged. `None` means the lane cannot render them.
    fn parse_caps(&mut self, caps: gst::Caps) -> Option<Arc<CapsPlan>> {
        if let Some((cached, plan)) = self.caps.as_ref()
            && *cached == caps
        {
            return Some(Arc::clone(plan));
        }
        let Some(desc) = desc_from_caps(&caps) else {
            // The offer says what the lane draws, not what it is sent: see
            // `accept_any_raw_video`. Once per distinct caps, since the whole
            // stream arrives this way.
            if self.unmappable.as_ref() != Some(&caps) {
                error!(%caps, "wgpu video lane: no render path for these caps, dropping frames");
                self.unmappable = Some(caps);
            }
            return None;
        };
        self.unmappable = None;
        let plan = Arc::new(desc);
        debug!(
            desc = ?plan.desc,
            size = ?plan.size,
            modifier = plan.modifier.map(|m| format!("{m:#x}")),
            "wgpu video lane: caps mapped"
        );
        // The imports describe the old caps' geometry and layout, and so do
        // the upload arm's plane textures sitting in free slots. A frame
        // still in flight keeps its own, which is why this is the free list
        // and not everything.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.imports.clear();
        self.pool.clear();
        self.caps = Some((caps, Arc::clone(&plan)));
        Some(plan)
    }

    /// Whether the device can import at all, which is what the sink reads
    /// before it takes a `DMA_DRM` sample down the zero-copy route. A past
    /// failure does not clear it, see [`Self::offer_dmabuf`].
    ///
    /// Linux only, with the route it guards. Off it the offer never carries a
    /// `DMA_DRM` structure, so nothing asks.
    #[cfg(target_os = "linux")]
    fn can_import(&self) -> bool {
        !self.dmabuf.is_empty()
    }

    /// Whether a frame of this format arriving as a dmabuf under plain
    /// system-memory caps can be imported with the linear modifier, which is
    /// what a udmabuf pool produces.
    #[cfg(target_os = "linux")]
    fn can_import_linear(&self, format: PixelFormat) -> bool {
        !self.linear_import_off && self.udmabuf.contains(&format)
    }

    /// The software arm's zero-copy route: a decoder that took the lane's
    /// udmabuf pool writes its pixels into dma_buf pages, so the frame can be
    /// imported instead of uploaded even though the caps say system memory.
    ///
    /// `None` means this frame is not one of those, or the import refused it,
    /// and the caller uploads it instead. Falling back is safe here in a way
    /// it is not for a VA surface: these pages are ordinary cached memory, so
    /// mapping them gives pixels rather than the decoder's tiling.
    #[cfg(target_os = "linux")]
    fn present_linear(
        &mut self,
        sample: &gst::Sample,
        buffer: &gst::BufferRef,
        plan: &CapsPlan,
        rotation: iv::BufferTransform,
    ) -> Option<Result<FramePayload, String>> {
        if !self.can_import_linear(plan.desc.format)
            || !crate::desktop_wgpu_udmabuf::is_dmabuf(buffer)
        {
            return None;
        }
        let owned = sample.buffer_owned()?;
        match self.present_imported(owned, plan, rotation, 0) {
            Ok(presented) => Some(Ok(presented)),
            Err(err) => {
                self.linear_import_off = true;
                warn!(
                    %err,
                    "wgpu video lane: udmabuf frame refused the import, uploading instead"
                );
                None
            }
        }
    }

    /// Imports the buffer's planes and renders them. Nothing here maps the
    /// buffer, which is the whole point: a readable map of a VA surface makes
    /// the driver de-tile the frame on the cpu.
    ///
    /// The import itself only happens the first time a given dmabuf arrives.
    /// A decoder hands the same pool round, so the steady state is a lookup:
    /// no VkImage, no imported memory, no view, no descriptor set, nothing
    /// allocated on either side.
    ///
    /// The buffer rides inside the frame that is handed over, since the
    /// imported textures read the decoder's own memory rather than a copy of
    /// it. Slint drops the frame once the last submit that read those planes
    /// has retired, and that drop is what hands the buffer back.
    #[cfg(target_os = "linux")]
    fn present_imported(
        &mut self,
        buffer: gst::Buffer,
        plan: &CapsPlan,
        rotation: iv::BufferTransform,
        modifier: u64,
    ) -> Result<FramePayload, String> {
        use crate::desktop_wgpu_dmabuf::{MAX_PLANES, PlaneSource};
        #[cfg(test)]
        if FAIL_IMPORTS.load(Ordering::Relaxed) > 0 {
            FAIL_IMPORTS.fetch_sub(1, Ordering::Relaxed);
            return Err("test: forced import refusal".into());
        }
        let mut sources = [PlaneSource::EMPTY; MAX_PLANES];
        let n = crate::desktop_wgpu_dmabuf::plane_sources(&buffer, &plan.info, &mut sources)?;
        let sources = &sources[..n];
        let index = self
            .imports
            .get_or_import(&self.device, &plan.desc, modifier, sources)?;
        self.dmabuf_frames += 1;
        // one line, not one a frame: which route the stream took is the thing
        // worth reading in a log, and the counters carry the rest
        if self.dmabuf_frames == 1 {
            info!(
                modifier = format_args!("{modifier:#x}"),
                width = plan.desc.width,
                height = plan.desc.height,
                format = ?plan.desc.format,
                layout = ?sources.iter().map(|s| (s.offset(), s.stride())).collect::<Vec<_>>(),
                "wgpu video lane: importing decoder dmabufs, no cpu map on the video path"
            );
        }
        let mut slot = self.pool.take();
        slot.planes.clear();
        // Refcount clones of the cache's own textures, so the cache is free
        // to evict the entry while this frame still reads them.
        slot.planes
            .extend_from_slice(self.imports.frame(index).planes());
        slot.buffer = Some(buffer);
        Ok(self.payload(slot, plan.desc, plan.par, rotation))
    }

    /// Whether a frame of this format arriving as a VideoToolbox surface can
    /// be imported. The caps say system memory either way, so this is the
    /// whole gate on the mac zero-copy route.
    #[cfg(target_os = "macos")]
    fn can_import_iosurface(&self, format: PixelFormat) -> bool {
        !self.iosurface_import_off && self.iosurface.contains(&format)
    }

    /// The mac zero-copy route: every VideoToolbox frame is an IOSurface, so
    /// its planes can be wrapped as metal textures instead of locked, mapped
    /// and uploaded.
    ///
    /// `None` means this frame is not one of those, or the import refused it,
    /// and the caller uploads it instead. Falling back is safe here the way
    /// it is for a udmabuf and not for a VA surface: a CVPixelBuffer maps to
    /// ordinary pixels, with a video meta carrying the real strides.
    ///
    /// The buffer rides inside the frame that is handed over, since the
    /// textures read the decoder's own surface rather than a copy of it.
    /// Slint drops the frame once the last submit that read those planes has
    /// retired, and that drop is what hands the pixel buffer back to
    /// VideoToolbox's pool.
    #[cfg(target_os = "macos")]
    fn present_iosurface(
        &mut self,
        sample: &gst::Sample,
        buffer: &gst::BufferRef,
        plan: &CapsPlan,
        rotation: iv::BufferTransform,
    ) -> Option<Result<FramePayload, String>> {
        use crate::desktop_wgpu_iosurface::{MAX_PLANES, PlaneSource};
        if !self.can_import_iosurface(plan.desc.format)
            || !crate::desktop_wgpu_iosurface::is_iosurface(buffer)
        {
            return None;
        }
        // the state a real refusal leaves the lane in: latched off for this
        // stream, with the frame uploaded instead of dropped
        #[cfg(test)]
        if FAIL_IMPORTS.load(Ordering::Relaxed) > 0 {
            FAIL_IMPORTS.fetch_sub(1, Ordering::Relaxed);
            self.iosurface_import_off = true;
            return None;
        }
        let owned = sample.buffer_owned()?;
        let mut sources = [PlaneSource::EMPTY; MAX_PLANES];
        let imported =
            crate::desktop_wgpu_iosurface::plane_sources(&owned, &plan.info, &mut sources)
                .map_err(str::to_string)
                .and_then(|n| {
                    self.imports
                        .get_or_import(&self.device, &plan.desc, &sources[..n])
                });
        let index = match imported {
            Ok(index) => index,
            Err(err) => {
                self.iosurface_import_off = true;
                warn!(
                    %err,
                    "wgpu video lane: iosurface frame refused the import, uploading instead"
                );
                return None;
            }
        };
        self.iosurface_frames += 1;
        // one line, not one a frame: which route the stream took is the thing
        // worth reading in a log, and the counters carry the rest
        if self.iosurface_frames == 1 {
            info!(
                width = plan.desc.width,
                height = plan.desc.height,
                format = ?plan.desc.format,
                "wgpu video lane: importing videotoolbox surfaces, no cpu map on the video path"
            );
        }
        let mut slot = self.pool.take();
        slot.planes.clear();
        // Refcount clones of the cache's own textures, so the cache is free
        // to evict the entry while this frame still reads them.
        slot.planes
            .extend_from_slice(self.imports.frame(index).planes());
        slot.buffer = Some(owned);
        Some(Ok(self.payload(slot, plan.desc, plan.par, rotation)))
    }

    /// Wraps a filled slot as the frame that crosses to the UI thread.
    fn payload(
        &self,
        slot: Slot,
        desc: FrameDesc,
        par: (u32, u32),
        transform: iv::BufferTransform,
    ) -> FramePayload {
        FramePayload {
            slot: Some(slot),
            pool: Arc::clone(&self.pool),
            desc,
            transform,
            par,
            generation: next_generation(),
        }
    }

    /// Uploads the sample's planes out of system memory, the route every
    /// software decoder and every driver without an importable modifier
    /// takes.
    ///
    /// The upload stays here, on the streaming thread, rather than crossing
    /// as `FrameSource::SystemMemory` for slint to do: it is up to twelve
    /// megabytes a frame at 4K, and the renderer's own thread is the one
    /// place it must not land. The plane textures belong to the slot, so a
    /// frame slint is still reading is never the one being written.
    fn present(
        &mut self,
        desc: FrameDesc,
        par: (u32, u32),
        rotation: iv::BufferTransform,
        planes: &[&[u8]],
        strides: &[u32],
    ) -> Result<FramePayload, i_slint_video_wgpu::VideoError> {
        let mut slot = self.pool.take();
        match slot.upload.as_mut() {
            Some(frame) => self.renderer.upload_into(
                &self.device,
                &self.queue,
                desc,
                planes,
                strides,
                frame,
            )?,
            None => {
                slot.upload =
                    Some(
                        self.renderer
                            .upload(&self.device, &self.queue, desc, planes, strides)?,
                    );
            }
        }
        // Destructured so the plane list and the frame it copies from are
        // two disjoint borrows of the slot.
        let Slot {
            planes,
            upload: Some(uploaded),
            ..
        } = &mut slot
        else {
            unreachable!("the arm above either filled or replaced it")
        };
        planes.clear();
        planes.extend_from_slice(uploaded.planes());
        self.sysmem_frames += 1;
        if self.sysmem_frames == 1 {
            info!(
                format = ?desc.format,
                width = desc.width,
                height = desc.height,
                "wgpu video lane: uploading frames from system memory"
            );
        }
        Ok(self.payload(slot, desc, par, rotation))
    }

    /// Log a failure once per distinct message, so a permanently unsupported
    /// desc does not print at frame rate.
    fn note_error(&mut self, msg: String) {
        if self.last_error.as_deref() != Some(msg.as_str()) {
            error!(err = %msg, "wgpu video lane: frame render failed");
            self.last_error = Some(msg);
        }
    }

    /// Drops the imports and every plane texture waiting in the pool. Called
    /// at end of stream and when the import route stops being used, which is
    /// also what closes the dup'd fds and frees the imported device memory.
    ///
    /// The decoder's own buffers are not here any more: each rides inside the
    /// frame that borrowed it and goes back when slint drops that frame. What
    /// this does reach is the free list, so the ones already returned do not
    /// keep an ending stream's imports alive.
    ///
    /// Off linux there is nothing imported, so only the pool goes.
    fn release_held(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.imports.clear();
        self.pool.clear();
        // The stream is going with them, so the next one announces itself
        // even when its caps look the same.
        self.announced = None;
    }

    /// The inspector's stream card, when the stream's shape moved since the
    /// last one: the caps plan by identity, the orientation, and which route
    /// the frame took. Nothing on a steady stream.
    fn announce(
        &mut self,
        plan: &Arc<CapsPlan>,
        rotation: iv::BufferTransform,
        arm: &'static str,
    ) -> Option<VideoCard> {
        if let Some((seen, seen_rotation, seen_arm)) = &self.announced
            && Arc::ptr_eq(seen, plan)
            && *seen_rotation == rotation
            && *seen_arm == arm
        {
            return None;
        }
        self.announced = Some((Arc::clone(plan), rotation, arm));
        Some(video_card(plan, rotation, arm))
    }

    /// Records a refused import and says whether the caller should ask
    /// upstream to renegotiate, which is only worth doing on the first one.
    ///
    /// The frame itself is dropped, never mapped: a `DMA_DRM` buffer read on
    /// the CPU gives back the decoder's tiling rather than pixels, so the last
    /// good picture stays up instead of a scrambled one. Audio and control are
    /// untouched either way.
    fn note_import_failure(&mut self, err: &str) -> bool {
        self.import_fails += 1;
        if !self.offer_dmabuf {
            return false;
        }
        self.offer_dmabuf = false;
        self.release_held();
        error!(
            %err,
            "wgpu video lane: dmabuf import refused, asking upstream for system memory"
        );
        true
    }

    /// Puts the dmabuf structures back in the offer for a new stream, and
    /// hands back the caps to publish. `None` when there is nothing to put
    /// back: the offer is already whole, or the device never had an import
    /// route to offer.
    ///
    /// Until this existed a narrowed offer stayed narrowed for the life of the
    /// process. Nothing set [`Self::offer_dmabuf`] back, so ONE transient
    /// refusal on one decoder's buffers cost every later item in the session
    /// the zero-copy path, with a single error line minutes earlier as the only
    /// trace. The refusal belongs to the stream it happened on; the next stream
    /// is a new decoder, usually a new format, always a new pool.
    fn rearm_dmabuf(&mut self) -> Option<gst::Caps> {
        // The software arm's latch belongs to the stream it tripped on too,
        // and unlike the offer it costs nothing to try again: the next item is
        // a new decoder and a new pool. Same for the mac import, which is the
        // only latch there is on that side.
        #[cfg(target_os = "linux")]
        {
            self.linear_import_off = false;
        }
        #[cfg(target_os = "macos")]
        {
            self.iosurface_import_off = false;
        }
        if self.offer_dmabuf || self.dmabuf.is_empty() {
            return None;
        }
        self.offer_dmabuf = true;
        self.import_fails = 0;
        Some(self.caps())
    }
}

/// A texture slint refused to wrap. The crate's target is Rgba8Unorm with
/// both usages, so this cannot fire unless that changes, and it would fire
/// every frame; log the first one only.
fn note_import_error(err: &slint::wgpu_30::VideoFrameImportError) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        error!(err = %err, "wgpu video lane: video frame refused");
    }
}

// ---------------------------------------------------------------------------
// the sink
// ---------------------------------------------------------------------------

/// Drops the last presented frame and puts the view back to no-video. The
/// image holds the only reference the scene has to a render target, so this
/// is also what lets it go.
fn clear_bridge_frame(ui: crate::MainWindow) {
    let bridge = ui.global::<crate::Bridge>();
    bridge.set_sw_video_frame(slint::Image::default());
    bridge.set_sw_video_active(false);
    bridge.set_have_video_dbg_info(false);
    // A bitmap subtitle over no picture is a subtitle floating on black, and
    // this is reached on paths that never pump again (device lost, idle).
    bridge.set_bitmap_subtitle(crate::SubtitleOverlay::default());
}

/// The inspector's stream card and the frame size beside it. Built on the
/// streaming thread; every field is a `SharedString` or a word, so it crosses
/// to the UI as it is.
struct VideoCard {
    info: crate::UiVideoDbgInfo,
    width: i32,
    height: i32,
}

fn video_card(plan: &CapsPlan, rotation: iv::BufferTransform, arm: &'static str) -> VideoCard {
    use slint::ToSharedString;
    let info = &plan.info;
    let colorimetry = info.colorimetry();
    let fps = info.fps();
    let par = info.par();
    let framerate = if fps.denom() == 0 {
        String::new()
    } else {
        format!("{:.3} fps", fps.numer() as f64 / fps.denom() as f64)
    };
    let desc = &plan.desc;
    let hdr = if desc.transfer.is_hdr() {
        format!(
            "{:?}, mastering {:.0} nits, MaxCLL {:.0}",
            desc.transfer, desc.hdr.max_mastering_nits, desc.hdr.max_cll
        )
    } else {
        "SDR".to_owned()
    };
    let rotation = match rotation {
        iv::BufferTransform::Normal => "0°",
        iv::BufferTransform::Rotate90 => "90°",
        iv::BufferTransform::Rotate180 => "180°",
        iv::BufferTransform::Rotate270 => "270°",
        iv::BufferTransform::Flipped => "flipped",
        iv::BufferTransform::Flipped90 => "flipped 90°",
        iv::BufferTransform::Flipped180 => "flipped 180°",
        iv::BufferTransform::Flipped270 => "flipped 270°",
    };
    VideoCard {
        width: info.width() as i32,
        height: info.height() as i32,
        info: crate::UiVideoDbgInfo {
            format: format!("{:?} ({}-bit)", info.format(), info.comp_depth(0)).to_shared_string(),
            resolution: format!("{}x{}", info.width(), info.height()).to_shared_string(),
            framerate: framerate.to_shared_string(),
            pixel_aspect: format!("{}:{}", par.numer(), par.denom()).to_shared_string(),
            rotation: rotation.to_shared_string(),
            memory: arm.to_shared_string(),
            primaries: format!("{:?}", colorimetry.primaries()).to_shared_string(),
            transfer: format!("{:?}", colorimetry.transfer()).to_shared_string(),
            matrix: format!("{:?}", colorimetry.matrix()).to_shared_string(),
            range: format!("{:?}", colorimetry.range()).to_shared_string(),
            hdr: hdr.to_shared_string(),
        },
    }
}

fn publish_video_card(ui: &crate::MainWindow, card: VideoCard) {
    let bridge = ui.global::<crate::Bridge>();
    bridge.set_video_frame_width(card.width);
    bridge.set_video_frame_height(card.height);
    bridge.set_video_dbg_info(card.info);
    bridge.set_have_video_dbg_info(true);
}

/// The cue engine, the geometry it has been told about, and the overlay slot in
/// front of the renderer.
///
/// The canvas and the picture rect are pushed from the frame that lands in the
/// scene, which is the only place both the window and the video size are known,
/// and the same place the schedule is advanced and the display list handed
/// over.
///
/// Behind one `Arc` so the per frame UI closure takes it with a refcount bump
/// and no allocation of its own.
struct Cues {
    engine: fcast_video::cue::CueEngine,
    geometry: crate::video_math::CueGeometry,
    /// Only ever touched on the UI thread, but `Cues` is shared with the
    /// streaming thread, so the interior mutability has to be a lock. It is
    /// uncontended by construction, which is a compare and swap per frame.
    overlay: parking_lot::Mutex<crate::cue_overlay::CueOverlay>,
    /// The bitmap subtitle set, which has no display list and so cannot go on
    /// the renderer's overlay slot. Same locking argument as `overlay`.
    bitmaps: parking_lot::Mutex<crate::bitmap_overlay::BitmapOverlay>,
    /// The coded video size, latched. This lane has no sink of its own, so it
    /// comes off the appsink's caps plan, and the bitmap decoders need it to
    /// scale their regions onto
    /// the picture. Written by the streaming thread, one relaxed swap a frame.
    coded: AtomicU64,
    /// What the bridge property was last told, so a steady cue does not dirty
    /// a slint property (and everything that reads it) on every frame.
    obstructed: AtomicBool,
}

impl Cues {
    /// Advance the cue schedule and put whatever is showing in front of the
    /// renderer. UI thread only.
    ///
    /// `frame_rt` is the running time of the frame being handed to the scene,
    /// or `None` for a repaint with no frame behind it (a cue landing, expiring
    /// or being cleared while paused), which re-evaluates against the frozen
    /// clock exactly as a raster consumer's `current_overlays` does.
    ///
    /// Costs nothing at rest: the engine's schedule pass, then a compare per
    /// showing cue. The display list is copied only when the engine publishes
    /// a new scene or the stack moves.
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
        // The bitmap set rides beside the display lists, never instead of them:
        // a source can carry a subpicture track and a text track at once.
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
        // Regions are in CODED video pixels, so the picture they are placed
        // against is the coded one. A turned picture would need the regions
        // turned with it, which nothing here does, so it takes them down rather
        // than putting them somewhere wrong.
        let coded = self.coded();
        let rect = (coded == self.geometry.picture())
            .then(|| crate::video_math::video_rect(coded, self.geometry.window()))
            .flatten();
        bitmaps.place(&sink, changed, rect, coded, ui.window().scale_factor())
    }

    /// The coded video size, or `(0, 0)` before the first caps.
    fn coded(&self) -> (u32, u32) {
        let word = self.coded.load(Ordering::Relaxed);
        ((word >> 32) as u32, word as u32)
    }

    /// Tell the engine the coded size when it changes. Streaming thread.
    fn note_coded(&self, size: (u32, u32)) {
        let word = (u64::from(size.0) << 32) | u64::from(size.1);
        if self.coded.swap(word, Ordering::Relaxed) != word {
            self.engine.set_video_size(size.0, size.1);
        }
    }

    /// Re-anchor and re-publish after the window moved. UI thread only.
    ///
    /// The per frame path lives in the appsink's UI closure, which is the only
    /// place the window and the picture are both known. PAUSED there are no
    /// frames, so without this a resize leaves every cue laid out against the
    /// window it had before, until playback resumes. The picture is the one the
    /// last frame carried, latched by [`crate::video_math::CueGeometry`].
    ///
    /// Cheap enough to call from every render pass: one compare when nothing
    /// moved.
    fn resize(&self, ui: &crate::MainWindow) {
        let size = ui.window().size();
        if !self.re_anchor((size.width, size.height)) {
            return;
        }
        // The engine re-keys on the new canvas and keeps the previous display
        // list up meanwhile, so this publishes the cue that is already on
        // screen now and the re-laid-out one when the worker answers.
        self.pump(ui, None);
    }

    /// Push the new canvas and picture rect, and say whether the window moved
    /// at all. Split out of [`Self::resize`] so the geometry half can be graded
    /// without a window behind it.
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

    /// Take the overlay down, for EOS and for a lane that has stopped drawing.
    fn clear(&self, ui: &crate::MainWindow) {
        self.overlay
            .lock()
            .sync(&crate::cue_overlay::WindowCues(ui.window()), &[]);
        self.bitmaps
            .lock()
            .clear(&crate::bitmap_overlay::BridgeBitmaps(ui));
        self.note_obstruction(ui, false);
    }

    /// A scene layer overlay is invisible under a video surface stacked above
    /// the GUI, so a cue on screen has to count as an obstruction.
    ///
    /// Only on a change: writing a slint property dirties everything that reads
    /// it, and `video-obstructed` is read by the whole player view.
    fn note_obstruction(&self, ui: &crate::MainWindow, visible: bool) {
        if self.obstructed.swap(visible, Ordering::Relaxed) != visible {
            ui.global::<crate::Bridge>()
                .set_cue_overlay_visible(visible);
        }
    }
}

/// The lane's cue state, for the one thing the appsink cannot see: the window
/// moving while no frame is arriving.
///
/// A refcount clone of what the sink already holds, handed to the app so its
/// rendering notifier can tick the geometry. Everything else about cues on this
/// lane stays inside the sink.
#[derive(Clone)]
pub struct CueTick(Arc<Cues>);

impl CueTick {
    /// Re-anchor and re-publish if the window moved. UI thread only, cheap
    /// enough for every render pass. See [`Cues::resize`].
    pub fn on_render(&self, ui: &crate::MainWindow) {
        self.0.resize(ui);
    }
}

/// What the two sample callbacks share. Preroll and playing frames take the
/// same route, so the sink is one method called from both.
/// What the inspector's card calls the route a frame arriving under plain
/// system-memory caps can still take without being uploaded: a udmabuf pool's
/// pages on linux, a VideoToolbox surface on mac.
#[cfg(target_os = "linux")]
const IMPORT_ARM: &str = "udmabuf import";
#[cfg(target_os = "macos")]
const IMPORT_ARM: &str = "iosurface import";
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const IMPORT_ARM: &str = "import";

struct Sink {
    /// Only the streaming thread touches it, but the callbacks are `Fn` and
    /// must be Send, so the interior mutability has to be a lock.
    gpu: parking_lot::Mutex<Gpu>,
    ui: slint::Weak<crate::MainWindow>,
    /// One frame in flight to the UI. `upgrade_in_event_loop` posts to an
    /// unbounded winit queue and never coalesces, so a stalled event loop
    /// would otherwise pile up closures for frames nothing will ever show,
    /// each one paid for with a full upload first.
    ///
    /// It is also what bounds the slot pool. With it, the frames alive at any
    /// moment are the one being filled, the one posted, and the three slint
    /// holds while their submits retire, so the pool settles at a handful
    /// instead of growing with the backlog. This is the drop rule the plan
    /// keeps on the producer's side: slint's contribution is only that a
    /// newer frame releases the older one, not that a slow consumer stops
    /// costing memory.
    ///
    /// Its own `Arc` because the closure that clears it outlives this borrow.
    pending: Arc<AtomicBool>,
    /// One clear when the device goes, not one per dropped frame.
    lost_cleared: AtomicBool,
    /// The container's orientation, as [`rotation_code`]. Written by the
    /// event probe and read by the render, both on the streaming thread but
    /// from separate closures, and a tag can arrive mid-stream, so the turn
    /// has to be picked up per frame rather than pinned at caps time.
    rotation: AtomicU8,
    cues: Arc<Cues>,
}

impl Sink {
    fn set_rotation(&self, rotation: iv::BufferTransform) {
        let code = rotation_code(rotation);
        if self.rotation.swap(code, Ordering::Relaxed) != code {
            info!(?rotation, "wgpu video lane: image-orientation");
        }
    }

    fn rotation(&self) -> iv::BufferTransform {
        rotation_from_code(self.rotation.load(Ordering::Relaxed))
    }

    /// Renders one sample and hands the result to the scene. Errors are
    /// logged and the frame dropped, never propagated: a frame the lane
    /// cannot draw must not tear the pipeline down under the audio.
    ///
    /// The appsink comes in with the sample because a refused import has to
    /// narrow its caps and ask upstream to renegotiate, and taking it from the
    /// callback's own argument keeps the sink from holding a ref back to the
    /// element that owns it.
    fn present(&self, appsink: &gst_app::AppSink, sample: &gst::Sample) {
        // Nothing recreates the device, so stop presenting and put the view
        // back where it is without a frame. The audio keeps playing.
        if DEVICE_LOST.load(Ordering::Acquire) {
            if !self.lost_cleared.swap(true, Ordering::Relaxed) {
                let _ = self.ui.upgrade_in_event_loop(clear_bridge_frame);
            }
            return;
        }
        // Only the streaming thread sets the flag, so the load/store pair
        // below cannot race itself; the UI thread only clears it.
        if self.pending.load(Ordering::Acquire) {
            return;
        }
        // caps_owned is a refcount bump, and holding the ref is what makes
        // the identity compare in parse_caps sound.
        let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps_owned()) else {
            return;
        };
        let rotation = self.rotation();
        // The frame's place on the cue timeline, resolved here because the
        // segment lives beside the engine and the pts beside the buffer. A
        // `Copy` word from here on, so the UI closure carries no allocation.
        let frame_rt = self.cues.engine.video_running_time(buffer.pts());
        let mut gpu = self.gpu.lock();
        let Some(plan) = gpu.parse_caps(caps) else {
            return;
        };
        // What the scene letterboxes: square pixels, turned the right way up.
        // Slint derives the same size from the raw size and the pixel aspect
        // ratio the frame declares, which is what lets the cue rect computed
        // from this one land exactly on the picture the renderer draws.
        // Copied out here so the UI closure carries a word instead of the
        // plan.
        let picture = turned(rotation, plan.size);
        // Both `Copy`, so the UI closure can resolve the profile once it
        // knows how big the picture is drawn, and still carry no allocation.
        let quality = gpu.quality;
        let desc = plan.desc;
        // The coded size, which is what a bitmap subtitle decoder scales its
        // regions onto. Latched, so a steady stream never takes the engine lock
        // here.
        self.cues.note_coded(plan.size);
        let (result, arm) = match plan.modifier {
            // dmabuf frames, the route that never maps the buffer
            #[cfg(target_os = "linux")]
            Some(modifier) if gpu.can_import() => {
                let Some(owned) = sample.buffer_owned() else {
                    return;
                };
                match gpu.present_imported(owned, &plan, rotation, modifier) {
                    Ok(presented) => (Ok(presented), "dmabuf import"),
                    Err(err) => {
                        if gpu.note_import_failure(&err) {
                            self.renegotiate(appsink, &gpu);
                        }
                        return;
                    }
                }
            }
            // DMA_DRM caps on a device that cannot import at all, which the
            // offer should have made impossible. Mapping them would hand the
            // convert pass the decoder's tiling rather than pixels, so the
            // frame is dropped and the offer narrowed instead.
            Some(_) => {
                if gpu.note_import_failure("no import route on this device") {
                    self.renegotiate(appsink, &gpu);
                }
                return;
            }
            None => {
                // A software decoder that took the lane's udmabuf pool writes
                // into dma_buf pages, and a VideoToolbox one writes into an
                // IOSurface; both arrive under these very caps, because the
                // memory type is what opens the import route here, not the
                // caps feature. `None` back means this is an ordinary sysmem
                // frame (or the import refused it) and it is uploaded.
                #[cfg(target_os = "linux")]
                let imported = gpu.present_linear(sample, buffer, &plan, rotation);
                #[cfg(target_os = "macos")]
                let imported = gpu.present_iosurface(sample, buffer, &plan, rotation);
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                let imported: Option<Result<FramePayload, String>> = None;
                match imported {
                    Some(result) => (result, IMPORT_ARM),
                    None => {
                        let Ok(frame) =
                            gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info)
                        else {
                            return;
                        };
                        let n = plan.desc.format.plane_count();
                        // Sized off the renderer's own bound, not the three a
                        // planar YUV has: A420 and GBRA carry an alpha plane
                        // and the dmabuf path refuses them, so they land here.
                        let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
                        let mut strides = [0u32; UPLOAD_MAX_PLANES];
                        let pitch = frame.plane_stride();
                        for i in 0..n {
                            let Ok(data) = frame.plane_data(i as u32) else {
                                return;
                            };
                            planes[i] = data;
                            strides[i] = pitch[i] as u32;
                        }
                        (
                            gpu.present(plan.desc, plan.par, rotation, &planes[..n], &strides[..n])
                                .map_err(|err| err.to_string()),
                            "upload",
                        )
                    }
                }
            }
        };

        match result {
            Ok(presented) => {
                gpu.last_error = None;
                // The inspector's card, once per stream shape rather than
                // per frame.
                if let Some(card) = gpu.announce(&plan, rotation, arm) {
                    let _ = self
                        .ui
                        .upgrade_in_event_loop(move |ui| publish_video_card(&ui, card));
                }
                drop(gpu);
                self.pending.store(true, Ordering::Release);
                // The frame object itself is built here, on the UI thread:
                // `slint::Image` is not Send, and neither is a frame that can
                // grow `Box<dyn FnOnce()>` release callbacks. What crosses is
                // the payload, which is.
                let posted = self.ui.upgrade_in_event_loop({
                    let pending = Arc::clone(&self.pending);
                    let cues = Arc::clone(&self.cues);
                    move |ui| {
                        // cleared first: a refused frame still has to let
                        // the next one through
                        pending.store(false, Ordering::Release);
                        // The window size is only knowable here, so this is
                        // where positioned cues get anchored to the picture.
                        // Two relaxed loads on a steady frame: the engine is
                        // only touched by a resize or a caps change.
                        let size = ui.window().size();
                        let window = (size.width, size.height);
                        cues.geometry.sync(&cues.engine, window, picture);
                        // After the geometry, so a resize re-keys before the
                        // schedule is read and the cue never lands one frame
                        // behind the window it was laid out against.
                        cues.pump(&ui, frame_rt);
                        // The size the renderer draws the picture at, which
                        // is what picks the up or the down kernel. The same
                        // rect the cue engine was just told about, since both
                        // sides fit the same picture into the same box; a few
                        // float operations, and the answer only moves when the
                        // window or the stream does.
                        let drawn = crate::video_math::video_rect(picture, window)
                            .map(|r| (r.width, r.height))
                            .unwrap_or(picture);
                        let frame: Arc<dyn iv::VideoFrame> = Arc::new(SinkFrame::new(presented));
                        let image = match slint::Image::try_from_video_frame(
                            frame,
                            quality.profile(&desc, drawn),
                        ) {
                            Ok(image) => image,
                            Err(err) => return note_import_error(&err),
                        };
                        let bridge = ui.global::<crate::Bridge>();
                        bridge.set_sw_video_frame(image);
                        bridge.set_sw_video_active(true);
                    }
                });
                if posted.is_err() {
                    // no event loop left to clear it
                    self.pending.store(false, Ordering::Release);
                }
            }
            Err(err) => gpu.note_error(err),
        }
    }

    /// Puts the dmabuf half of the offer back for a new stream.
    ///
    /// A refused import narrows the appsink to system memory, and until this
    /// existed it stayed narrowed for the life of the process: nothing ever set
    /// [`Gpu::offer_dmabuf`] back. One transient refusal on one decoder's
    /// buffers therefore cost every later item in the session the zero-copy
    /// path, silently, with the only trace a single error line minutes earlier.
    /// The refusal belongs to the stream it happened on; the next stream is a
    /// new decoder, often a new format and always a new pool.
    ///
    /// Streaming thread, off the sink pad's stream-start. A decoder that
    /// renegotiates BEFORE it forwards stream-start (which is the order
    /// `gst_video_decoder` uses when the input caps change) reads the narrowed
    /// offer once more, so the cost of a refusal is at most the stream it
    /// landed on and the one after it, never the session.
    fn rearm_dmabuf(&self, pad: &gst::Pad) {
        let Some(caps) = self.gpu.lock().rearm_dmabuf() else {
            return;
        };
        let Some(appsink) = pad
            .parent_element()
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        else {
            return;
        };
        appsink.set_caps(Some(&caps));
        info!("wgpu video lane: new stream, offering dmabufs again");
    }

    /// Narrows the appsink to system memory and asks upstream to renegotiate.
    ///
    /// Best effort by design. It is the only way to ask, and elements that
    /// re-run their output negotiation on a reconfigure will switch, but a VA
    /// decoder will not: `gst_va_base_dec_negotiate` returns early unless its
    /// input state changed, so it keeps pushing dmabufs. That is why the lane
    /// keeps retrying the import rather than latching itself off.
    fn renegotiate(&self, appsink: &gst_app::AppSink, gpu: &Gpu) {
        appsink.set_caps(Some(&gpu.caps()));
        match appsink.static_pad("sink") {
            Some(pad) => {
                pad.push_event(gst::event::Reconfigure::new());
            }
            None => warn!("wgpu video lane: no sink pad to renegotiate on"),
        }
    }
}

/// Answers the allocation query with `GstVideoMeta` support.
///
/// This is what makes the whole dmabuf offer usable: a VA decoder refuses to
/// negotiate `memory:DMABuf` at all against a downstream that has not said it
/// reads the meta ("DMABuf caps negotiated without the mandatory support of
/// VideoMeta"), because a dmabuf's plane offsets and pitches exist nowhere
/// else. An appsink does not offer it on its own.
fn offer_video_meta(pad: &gst::Pad) {
    add_video_meta_probe(pad);
}

/// Answers `ACCEPT_CAPS` with yes for every raw video format, whatever the
/// offer currently says.
///
/// The appsink's `caps` property is the lane's negotiation PREFERENCE, and
/// `gst_base_sink` also uses it as a hard filter: its `ACCEPT_CAPS` answer is
/// a strict subset test against that one property. A caps event carrying
/// anything outside it fails `pre_eventfunc_check`, which returns
/// `GST_FLOW_NOT_NEGOTIATED` to whoever pushed it, and that travels all the
/// way up to the source. The item then dies with "streaming stopped, reason
/// not-negotiated", taking audio and control with it, and the only trace is
/// one `GST_CAPS` warning naming a pad inside decodebin3.
///
/// Upstream reaches that state for reasons the lane cannot prevent. A decoder
/// that fixes its output format from the bitstream never asks downstream at
/// all; one that negotiated while its chain was detached or parked saw ANY
/// caps and picked its own preference; and plenty of ordinary content decodes
/// to a format the renderer has no path for (4:2:2, 4:4:4, RGB, alpha). None
/// of those is worth a dead item. `parse_caps` drops what it cannot map and
/// says so once, and the CAPS query still advertises the narrow offer, so
/// everything that negotiates properly still settles on a format the lane
/// draws.
///
/// The probe fires on both phases of the query; the one after the element
/// answered is the one that counts, and setting the same result twice costs
/// nothing.
fn accept_any_raw_video(pad: &gst::Pad) {
    pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_, info| {
        if let Some(gst::PadProbeData::Query(query)) = &mut info.data
            && let gst::QueryViewMut::AcceptCaps(accept) = query.view_mut()
            && accept
                .caps()
                .structure(0)
                .is_some_and(|s| s.name() == "video/x-raw")
        {
            accept.set_result(true);
        }
        gst::PadProbeReturn::Ok
    });
}

/// The probe body both halves of the contract use: put `GstVideoMeta` on an
/// allocation query that does not carry it yet.
fn add_video_meta_probe(pad: &gst::Pad) -> Option<gst::PadProbeId> {
    pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_, info| {
        if let Some(gst::PadProbeData::Query(query)) = &mut info.data
            && let gst::QueryViewMut::Allocation(alloc) = query.view_mut()
            && alloc
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_none()
        {
            alloc.add_allocation_meta::<gst_video::VideoMeta>(None);
        }
        gst::PadProbeReturn::Ok
    })
}

/// Marks a pad the meta probe is already on, so the walk and the hook below
/// cannot both probe the same decoder.
const META_PROBE_KEY: &str = "fcast-wgpu-video-meta";

/// Puts the same answer on the pad the DECODER asks on, not only on the pad it
/// is meant to reach.
///
/// [`offer_video_meta`] answers the query where it lands. That is not enough,
/// because the query has to survive the trip.
/// `gst_video_decoder_negotiate_pool` cannot tell a downstream that answered
/// "no video meta" from one that never answered at all: a failed
/// `gst_pad_peer_query` is a debug line, and the subclass is then handed the
/// untouched query. For a VA decoder on `DMA_DRM` caps that empty query is
/// fatal on the spot, "DMABuf caps negotiated without the mandatory support of
/// VideoMeta", `GST_FLOW_NOT_NEGOTIATED` out of the decoder and an error posted
/// by whatever source is feeding it.
///
/// Losing the query is ordinary, not exotic. An allocation query is serialized,
/// so a `queue` between the decoder and the sink carries it IN BAND and answers
/// `FALSE` outright whenever its source task is not running: any flush, any
/// state below PAUSED, an EOS latch, an unlinked source pad. A stream
/// transition is several of those at once, and `gst_pad_link` sends a
/// RECONFIGURE upstream on every link it makes, which schedules exactly one
/// extra renegotiation on the decoder one frame after the new stream's first,
/// right inside that window.
///
/// `gst_pad_peer_query` runs the querying pad's probes BEFORE it hands the
/// query to the peer, and the query object is not reset when the peer fails.
/// So a probe here is on the query whatever becomes of it downstream, which is
/// what makes the offer's contract hold instead of merely being advertised.
fn guarantee_video_meta(element: &gst::Element) {
    if !element.is::<gst_video::VideoDecoder>() {
        return;
    }
    let Some(pad) = element.static_pad("src") else {
        return;
    };
    // SAFETY: the key is only ever written here and read here, both under the
    // object's own lock, and the value is a plain bool.
    if unsafe { pad.data::<bool>(META_PROBE_KEY) }.is_some() {
        return;
    }
    unsafe { pad.set_data(META_PROBE_KEY, true) };
    add_video_meta_probe(&pad);
    debug!(
        decoder = %element.name(),
        "wgpu video lane: guaranteeing video meta on the decoder's own query"
    );
}

/// Arms [`guarantee_video_meta`] over a whole pipeline: the decoders already in
/// it, and every one added later.
///
/// Called once, when the sink first lands in a pipeline. The decoder for the
/// first item is usually built before the video chain is added, so the walk is
/// not redundant with the hook.
fn guarantee_video_meta_in(pipeline: &gst::Pipeline) {
    use gst::prelude::GstBinExt;
    // Keyed on the pipeline, not on the sink, so a host that rebuilt its
    // pipeline is armed again and one that relinks the same sink is not armed
    // twice.
    // SAFETY: the key is only written and read here, and the value is a bool.
    if unsafe { pipeline.data::<bool>(META_PROBE_KEY) }.is_some() {
        return;
    }
    unsafe { pipeline.set_data(META_PROBE_KEY, true) };
    for element in pipeline.iterate_recurse().into_iter().flatten() {
        guarantee_video_meta(&element);
    }
    pipeline.connect_deep_element_added(|_, _, element| guarantee_video_meta(element));
}

/// Walks up to the pipeline an element ended up in, if any.
fn toplevel_pipeline(element: &gst::Element) -> Option<gst::Pipeline> {
    let mut object = element.parent()?;
    loop {
        match object.parent() {
            Some(parent) => object = parent,
            None => return object.downcast::<gst::Pipeline>().ok(),
        }
    }
}

/// The desktop wgpu video sink: a synced appsink whose frames are rendered by
/// `i-slint-video-wgpu` and land in the `sw-video-frame` bridge image.
///
/// `None` when no gpu device could be created, so the caller keeps the
/// libplacebo path.
///
/// `cues` is the engine the app publishes subtitles to. The lane owns it end to
/// end: it is the only thing that knows where the picture ends up, so it pushes
/// the canvas and the video rect, it advances the schedule from the frame it
/// hands to the scene, and it draws the display lists through the dodvg overlay
/// slot.
///
/// The [`CueTick`] that comes back is the one thing the caller has to drive:
/// a window resize produces no frame, so nothing here would see it.
pub fn make_sink(
    ui: slint::Weak<crate::MainWindow>,
    cues: fcast_video::cue::CueEngine,
    profile: RenderProfile,
) -> Option<(gst::Element, CueTick)> {
    let quality = profile_quality(profile);
    info!(?profile, ?quality, "wgpu video lane: render profile");
    Some(make_sink_on(Gpu::new(quality)?, ui, cues))
}

/// [`make_sink`] past the device decision: the sink for a gpu already built.
///
/// Split out so the tests drive the whole appsink, probes and callbacks
/// included, on a device of their own. The test binary never publishes a
/// shared one, and before this existed every test that went through
/// `make_sink` skipped on that and passed empty.
fn make_sink_on(
    gpu: Gpu,
    ui: slint::Weak<crate::MainWindow>,
    cues: fcast_video::cue::CueEngine,
) -> (gst::Element, CueTick) {
    let caps = gpu.caps();
    info!(%caps, "wgpu video lane: appsink offering");
    // The formats the software arm may hand a udmabuf pool out for, taken off
    // the gpu before it goes behind the lock so the allocation query never has
    // to take it.
    #[cfg(target_os = "linux")]
    let udmabuf = Arc::clone(&gpu.udmabuf);
    // This lane draws display lists, never rasters, so the engine's vello_cpu
    // paint lane is dead weight behind it: worker milliseconds per cue and a
    // key clone per frame for a want that is never satisfied.
    cues.set_scene_consumer(true);
    // Pay the fontconfig/fontmap first-use cost on the raster thread now,
    // instead of inside the first cue.
    cues.warm();
    let sink = Arc::new(Sink {
        gpu: parking_lot::Mutex::new(gpu),
        ui: ui.clone(),
        pending: Arc::new(AtomicBool::new(false)),
        lost_cleared: AtomicBool::new(false),
        rotation: AtomicU8::new(0),
        cues: Arc::new(Cues {
            engine: cues,
            geometry: crate::video_math::CueGeometry::new(),
            overlay: parking_lot::Mutex::new(Default::default()),
            bitmaps: parking_lot::Mutex::new(Default::default()),
            coded: AtomicU64::new(0),
            obstructed: AtomicBool::new(false),
        }),
    });

    // The cue set changed with no frame behind it: a scene finished laying out,
    // a cue activated or expired, a track was switched. While PAUSED this is
    // the only thing that can put a cue on screen, since no frame is coming.
    // The repaint is requested too: the overlay is drawn from the render pass.
    sink.cues.engine.set_on_change({
        let ui = ui.clone();
        let cues = Arc::clone(&sink.cues);
        move || {
            let cues = Arc::clone(&cues);
            let _ = ui.upgrade_in_event_loop(move |ui| {
                cues.pump(&ui, None);
                ui.window().request_redraw();
            });
        }
    });

    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(2)
        .drop(true)
        .build();
    // An appsink keeps the basesink defaults, and the sink this replaces did
    // not: gst_video_sink_init turns QoS on, drops a frame more than 5ms late
    // instead of rendering it, and reports a 15ms processing deadline. A box
    // that cannot keep up relies on all three. Without them every late frame
    // is drawn anyway and the picture falls further behind the audio for as
    // long as the load lasts, where the old sink skipped and let the decoder
    // skip too.
    appsink.set_qos(true);
    appsink.set_max_lateness(VIDEO_SINK_MAX_LATENESS.nseconds() as i64);
    appsink.set_processing_deadline(VIDEO_SINK_PROCESSING_DEADLINE);
    // The whole software zero-copy arm. A software decoder writes with the
    // CPU, so it can never negotiate `memory:DMABuf` caps; what it can do is
    // write into pages that happen to be a dma_buf, which is what a udmabuf
    // pool hands it. Answering the query here is the only place to offer one.
    //
    // Nothing is proposed for `DMA_DRM` caps, on a box without `/dev/udmabuf`,
    // or for a format the device cannot sample linearly, and a decoder is free
    // to ignore what is proposed. Every one of those leaves the upload arm
    // carrying the stream exactly as before.
    #[cfg(target_os = "linux")]
    let callbacks = gst_app::AppSinkCallbacks::builder()
        .propose_allocation(move |_, query| crate::desktop_wgpu_udmabuf::propose(query, &udmabuf));
    #[cfg(not(target_os = "linux"))]
    let callbacks = gst_app::AppSinkCallbacks::builder();
    appsink.set_callbacks(
        callbacks
            // A seek while paused only ever prerolls, so without this the
            // scene keeps the frame from before the seek.
            .new_preroll({
                let sink = Arc::clone(&sink);
                move |appsink| {
                    let sample = appsink.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                    sink.present(appsink, &sample);
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .new_sample({
                let sink = Arc::clone(&sink);
                move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    sink.present(appsink, &sample);
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .eos({
                let sink = Arc::clone(&sink);
                move |_| {
                    debug!("wgpu video lane: eos, clearing the frame");
                    // the decoder's frames go back to its pool with the stream
                    sink.gpu.lock().release_held();
                    let cues = Arc::clone(&sink.cues);
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        // The cue goes with the picture: an overlay left up
                        // over an empty scene is a subtitle floating on black.
                        cues.clear(&ui);
                        clear_bridge_frame(ui);
                    });
                }
            })
            .build(),
    );

    // The orientation tag rides the event stream, not the caps and not the
    // buffer, so it is read off the pad rather than out of the sample. An
    // appsink swallows tags into its own list; the probe sees them first,
    // and sees them again the moment they change mid-stream.
    if let Some(pad) = appsink.static_pad("sink") {
        offer_video_meta(&pad);
        accept_any_raw_video(&pad);
        let sink = Arc::clone(&sink);
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                match event.view() {
                    gst::EventView::Tag(tag) => {
                        if let Some(rotation) = rotation_from_tags(tag.tag()) {
                            sink.set_rotation(rotation);
                        }
                    }
                    // The cue engine schedules in running time, so it needs the
                    // same segment the frames are timestamped against. A sink
                    // subclass takes these in its own `event` handler; an
                    // appsink has none, so the probe is the only place they
                    // can be seen.
                    gst::EventView::Segment(ev) => {
                        sink.cues.engine.set_video_segment(ev.segment());
                    }
                    gst::EventView::FlushStop(_) => sink.cues.engine.flush(),
                    // A new stream carries its own orientation, and one that
                    // carries none is upright. Without this the turn of the
                    // clip before it would stick. The cue timeline restarts
                    // with it, for the same reason.
                    gst::EventView::StreamStart(_) => {
                        sink.set_rotation(iv::BufferTransform::Normal);
                        sink.cues.engine.reset_timeline();
                        // The frames the render still borrows belong to the
                        // decoder that is ending, and so do the imports that
                        // read them. `parse_caps` only drops them on a caps
                        // CHANGE, and two items at the same size negotiate
                        // caps that compare EQUAL, so without this a session
                        // of same-size items keeps every previous decoder's
                        // dmabufs open until the cache cycles them out.
                        sink.gpu.lock().release_held();
                        sink.rearm_dmabuf(pad);
                    }
                    _ => (),
                }
            }
            gst::PadProbeReturn::Ok
        });
    } else {
        warn!("wgpu video lane: appsink has no sink pad, rotation tags will be ignored");
    }

    // The decoders are not ours and the pipeline is not ours either, so the
    // lane has to catch the one moment it can see both: its own sink pad being
    // linked, which a host does only after the sink is in the pipeline.
    // `notify::parent` would be the obvious hook and is not one, GstObject
    // leaves the notify for it commented out. Once is enough: the player keeps
    // one pipeline for its whole life.
    if let Some(pad) = appsink.static_pad("sink") {
        pad.connect_linked(|pad, _| {
            if let Some(pipeline) = pad.parent_element().as_ref().and_then(toplevel_pipeline) {
                guarantee_video_meta_in(&pipeline);
            }
        });
    }

    let tick = CueTick(Arc::clone(&sink.cues));
    (appsink.upcast(), tick)
}

/// Counts heap allocations on the calling thread, so a test can put a number
/// on what one frame costs. Test builds only, and per thread so a parallel
/// harness cannot pollute a measurement.
#[cfg(test)]
pub(crate) mod alloc_counter {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    // const init and no destructor, so reading it never allocates itself
    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            COUNT.with(|c| c.set(c.get() + 1));
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            COUNT.with(|c| c.set(c.get() + 1));
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
            COUNT.with(|c| c.set(c.get() + 1));
            unsafe { System.realloc(ptr, layout, new) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    /// Allocations this thread has made so far.
    pub fn count() -> u64 {
        COUNT.with(|c| c.get())
    }

    /// Allocations `f` makes, which is the measurement every steady-state
    /// test below is written against.
    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, u64) {
        let before = count();
        let out = f();
        let after = count();
        (out, after - before)
    }
}

#[cfg(test)]
#[global_allocator]
static COUNTING_ALLOC: alloc_counter::Counting = alloc_counter::Counting;

#[cfg(test)]
mod tests {
    use super::{alloc_counter::measure, *};
    use std::str::FromStr;

    /// gst plus the plugins linked into the binary. Without the second half
    /// no VA or VideoToolbox factory exists and every decoder proof below
    /// skips silently, which is how the lane shipped a negotiation defect no
    /// test could see. Only the decoder proofs need it, so it is built where
    /// they are.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn init_gst() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().unwrap();
            gstreamer_src::init_static_plugins();
            // dav1ddec is a rust plugin, so it is not in the static tree the
            // line above registers. The AV1 arm of the udmabuf proof is the
            // only thing here that wants it.
            #[cfg(target_os = "linux")]
            gstdav1d::plugin_register_static().unwrap();
        });
    }

    /// Caps strings as they arrive off a real decoder, so the mapping is
    /// graded on the text the pipeline actually negotiates rather than on a
    /// hand-built `VideoInfo`.
    fn desc(caps: &str) -> Option<FrameDesc> {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(caps).unwrap();
        desc_from_caps(&caps).map(|p| p.desc)
    }

    /// The size the lane renders, off the same caps text.
    fn display(caps: &str) -> (u32, u32) {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(caps).unwrap();
        desc_from_caps(&caps).expect("caps must map").size
    }

    // -----------------------------------------------------------------
    // the presentation lanes, on a real device
    // -----------------------------------------------------------------

    /// A device, or `None` on a box with no usable adapter so these skip
    /// instead of failing. Not the process-wide shared one: nothing published
    /// it here, and the mode is passed explicitly anyway.
    /// The options a test builds a sink with: whatever the receiver
    /// defaults to, so a test that does not care renders the default lane.
    /// The whole appsink on a device of this test's own, since the binary
    /// never publishes a shared one. `None` without an adapter.
    pub(super) fn test_sink(
        engine: fcast_video::cue::CueEngine,
    ) -> Option<(gst::Element, CueTick)> {
        Some(super::make_sink_on(
            texture_gpu()?,
            slint::Weak::default(),
            engine,
        ))
    }

    /// The pacing the appsink has to be given by hand. The sink this lane
    /// replaces is a GstVideoSink, whose init turns these on
    /// (`gstvideosink.c`, `gst_video_sink_init`), and a box that cannot keep
    /// up relies on them: a late frame is dropped instead of rendered and the
    /// QoS event lets the decoder skip. At the basesink defaults the picture
    /// drifts behind the audio for as long as the load lasts.
    #[test]
    fn the_appsink_paces_like_the_video_sink_it_replaces() {
        gst::init().unwrap();
        let Some((sink, _tick)) = test_sink(fcast_video::cue::CueEngine::new()) else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        assert!(sink.property::<bool>("qos"), "qos events must go upstream");
        assert_eq!(
            sink.property::<i64>("max-lateness"),
            5_000_000,
            "a frame more than 5ms late must be dropped"
        );
        assert_eq!(sink.property::<u64>("processing-deadline"), 15_000_000);
        // and the defaults this is set apart from, so a plain appsink put back
        // in its place fails here rather than in the field
        let plain = gst_app::AppSink::builder().build();
        assert!(!plain.property::<bool>("qos"));
        assert_eq!(plain.property::<i64>("max-lateness"), -1);
    }

    /// A software Vulkan adapter is refused unless asked for: it would take
    /// slint's whole UI through a software rasterizer on a box whose OpenGL
    /// driver is hardware. Every hardware type, and the unknown one the GL
    /// backend reports, stays acceptable.
    #[test]
    fn a_software_adapter_is_refused_on_the_hardware_passes() {
        use wgpu::DeviceType as D;
        assert!(!adapter_acceptable(D::Cpu, false));
        assert!(
            adapter_acceptable(D::Cpu, true),
            "the software pass takes it"
        );
        for hardware in [D::IntegratedGpu, D::DiscreteGpu, D::VirtualGpu, D::Other] {
            assert!(
                adapter_acceptable(hardware, false),
                "{hardware:?} must stay"
            );
        }
    }

    /// Every hardware pass comes before any software one, so a hardware GL
    /// driver beats a software Vulkan one, and refusing software drops the
    /// tail without touching the order.
    #[test]
    fn the_software_passes_trail_every_hardware_pass() {
        let hardware = backend_passes();
        let passes = device_passes(true);
        assert_eq!(passes.len(), hardware.len() * 2);
        let (first, second) = passes.split_at(hardware.len());
        assert!(first.iter().all(|(_, software)| !software));
        assert!(second.iter().all(|(_, software)| *software));
        assert_eq!(first.iter().map(|(b, _)| *b).collect::<Vec<_>>(), hardware);
        assert_eq!(second.iter().map(|(b, _)| *b).collect::<Vec<_>>(), hardware);
        assert_eq!(
            device_passes(false).len(),
            hardware.len(),
            "refused software is no pass at all"
        );
    }

    /// The platform backend is tried first and the GL backend is the floor
    /// behind it; an override names one pass and a typo keeps the order.
    #[test]
    fn the_backend_passes_end_on_the_gl_floor() {
        use wgpu::Backends as B;
        let platform = i_slint_video_wgpu::default_backends();
        let expect = if cfg!(target_vendor = "apple") {
            vec![B::METAL]
        } else {
            vec![platform, B::GL]
        };
        assert_eq!(backend_passes_for(None), expect);
        assert_eq!(
            backend_passes_for(Some("bogus")),
            expect,
            "a typo is not a request"
        );
        assert_eq!(backend_passes_for(Some("gl")), vec![B::GL]);
        assert_eq!(backend_passes_for(Some("vulkan")), vec![B::VULKAN]);
        assert_eq!(backend_passes_for(Some("metal")), vec![B::METAL]);
    }

    /// Which pass this box takes, printed for the run and pinned to the rule:
    /// whatever opened is not a software adapter unless that was asked for.
    #[test]
    fn a_device_opens_on_a_hardware_adapter_or_says_why_not() {
        match create_shared_device() {
            Some(shared) => {
                let info = &shared.info;
                eprintln!(
                    "device: {} on {:?} ({:?}), norm16 {}, dmabuf {}",
                    info.name, info.backend, info.device_type, shared.norm16, shared.dmabuf
                );
                assert!(
                    software_adapter_allowed() || info.device_type != wgpu::DeviceType::Cpu,
                    "a software adapter got through the gate"
                );
            }
            None => eprintln!("no device: {:?}", REFUSED.get()),
        }
    }

    /// glvnd's vendor JSON is one key deep and comes in both spellings the
    /// two vendors ship.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_vendor_json_gives_up_its_library_path() {
        use super::egl_vendor::library_path;
        let nvidia = "{\n  \"file_format_version\": \"1.0.0\",\n  \"ICD\": {\n    \
                      \"library_path\": \"/nix/store/x/lib/libEGL_nvidia.so.0\"\n  }\n}\n";
        let mesa = "{\n    \"file_format_version\" : \"1.0.0\",\n    \"ICD\" : {\n        \
                    \"library_path\" : \"/usr/lib/libEGL_mesa.so.0\"\n    }\n}\n";
        assert_eq!(
            library_path(nvidia).as_deref(),
            Some("/nix/store/x/lib/libEGL_nvidia.so.0")
        );
        assert_eq!(
            library_path(mesa).as_deref(),
            Some("/usr/lib/libEGL_mesa.so.0")
        );
        assert_eq!(library_path("{}"), None);
        assert_eq!(library_path("{\"library_path\": 3}"), None);
    }

    /// The pin exists for one shape, Mesa beside another vendor, and stays out
    /// of the way otherwise: Mesa alone, another vendor alone, nothing at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn mesa_is_pinned_only_beside_another_vendor() {
        use super::egl_vendor::mesa_pin;
        let root = std::env::temp_dir().join(format!("fcast-egl-vendor-{}", std::process::id()));
        let dir = |name: &str, files: &[(&str, &str)]| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            for (file, lib) in files {
                std::fs::write(
                    d.join(file),
                    format!("{{ \"file_format_version\": \"1.0.0\", \"ICD\": {{ \"library_path\": \"{lib}\" }} }}"),
                )
                .unwrap();
            }
            d
        };
        let hybrid = dir(
            "hybrid",
            &[
                ("10_nvidia.json", "/lib/libEGL_nvidia.so.0"),
                ("50_mesa.json", "/lib/libEGL_mesa.so.0"),
            ],
        );
        assert_eq!(
            mesa_pin(&[hybrid.clone()]),
            Some(hybrid.join("50_mesa.json")),
            "two vendors, one of them mesa: pin mesa"
        );
        let mesa_only = dir("mesa", &[("50_mesa.json", "/lib/libEGL_mesa.so.0")]);
        assert_eq!(mesa_pin(&[mesa_only]), None, "mesa alone needs no pin");
        let nvidia_only = dir("nvidia", &[("10_nvidia.json", "/lib/libEGL_nvidia.so.0")]);
        assert_eq!(
            mesa_pin(&[nvidia_only.clone()]),
            None,
            "no mesa, nothing to pin to"
        );
        assert_eq!(
            mesa_pin(&[root.join("missing")]),
            None,
            "no directory, no pin"
        );
        // split across two directories, the way a distro and a local install are
        let mesa_elsewhere = dir("elsewhere", &[("50_mesa.json", "/opt/libEGL_mesa.so.0")]);
        assert_eq!(
            mesa_pin(&[nvidia_only, mesa_elsewhere.clone()]),
            Some(mesa_elsewhere.join("50_mesa.json"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cover_downscale_matches_geometry_and_holds_flat_color() {
        let Some(shared) = create_shared_device() else {
            return;
        };
        let mut cache = None;
        let flat: Vec<u8> = [10, 200, 30, 255].repeat(1000 * 500);
        let img = receiver_core::image::RgbaImage::from_raw(1000, 500, flat).unwrap();
        let out = downscale_cover_on(&shared.device, &shared.queue, &mut cache, &img, 96)
            .expect("downscale renders");
        assert_eq!(out.dimensions(), (96, 48));
        // lanczos of a flat field is the flat field, and Rgbx must pin alpha
        for p in out.pixels() {
            assert_eq!(p.0, [10, 200, 30, 255]);
        }

        // a smooth horizontal ramp survives as a monotonic ramp
        let px: Vec<u8> = (0..1000u32)
            .flat_map(|_| (0..2000u32).flat_map(|x| [(x / 8) as u8, 0, 0, 255]))
            .collect();
        let ramp = receiver_core::image::RgbaImage::from_raw(2000, 1000, px).unwrap();
        let out = downscale_cover_on(&shared.device, &shared.queue, &mut cache, &ramp, 96)
            .expect("cache reuse renders");
        assert_eq!(out.dimensions(), (96, 48));
        let row: Vec<u8> = (0..96).map(|x| out.get_pixel(x, 24).0[0]).collect();
        assert!(
            row.windows(2).all(|w| w[0] <= w[1]),
            "ramp not monotonic: {row:?}"
        );
        assert!(row[95] > row[0] + 100, "ramp lost its range: {row:?}");
    }

    #[test]
    #[ignore]
    fn cover_downscale_timing() {
        let Some(shared) = create_shared_device() else {
            return;
        };
        let mut cache = None;
        for (w, h) in [(600u32, 600u32), (1400, 1400), (3000, 3000), (4096, 4096)] {
            let img = receiver_core::image::RgbaImage::from_raw(
                w,
                h,
                [10u8, 200, 30, 255].repeat((w * h) as usize),
            )
            .unwrap();
            let t = std::time::Instant::now();
            let out = downscale_cover_on(&shared.device, &shared.queue, &mut cache, &img, 96)
                .expect("downscale renders");
            println!("{}x{} -> {:?} in {:?}", w, h, out.dimensions(), t.elapsed());
        }
    }

    /// The same, on a chosen profile, so a test can render the way the
    /// receiver would with that setting.
    fn test_gpu_with(quality: Quality) -> Option<Gpu> {
        let shared = create_shared_device()?;
        Some(Gpu::build(
            shared.device,
            shared.queue,
            shared.norm16,
            shared.dmabuf,
            quality,
        ))
    }

    fn texture_gpu() -> Option<Gpu> {
        test_gpu_with(Quality::default())
    }

    /// The crate's own quarter turn, which is what the fork derives from the
    /// frame's transform on the far side of the seam.
    fn crate_rotation(transform: iv::BufferTransform) -> i_slint_video_wgpu::Rotation {
        use i_slint_video_wgpu::Rotation as R;
        match transform.rotation_degrees() {
            90 => R::Rotate90,
            180 => R::Rotate180,
            270 => R::Rotate270,
            _ => R::Rotate0,
        }
    }

    /// Renders a presented frame's planes the way slint's renderer will,
    /// straight through the crate, so every pixel assertion below still
    /// grades the picture the lane produces.
    ///
    /// The lane itself renders nothing any more: it hands over planes and a
    /// profile and the renderer converts them inside its own frame. What is
    /// reconstructed here is the other half of that seam — the `OutputDesc`
    /// the profile and the frame's turn amount to — so these tests keep
    /// measuring the mapping and the crate, which is what they always
    /// measured. It is deliberately built from [`Quality`] and the frame
    /// rather than from a second table.
    fn render_presented(
        gpu: &Gpu,
        payload: &FramePayload,
        dst: (u32, u32),
    ) -> Result<wgpu::Texture, i_slint_video_wgpu::VideoError> {
        let profile = gpu.quality.profile(&payload.desc, dst);
        let rotation = crate_rotation(payload.transform);
        let (width, height) = rotation.swap(dst.0, dst.1);
        let out = i_slint_video_wgpu::OutputDesc {
            width,
            height,
            filter: match profile.filter {
                iv::ScaleFilter::Bilinear => ScaleFilter::Bilinear,
                iv::ScaleFilter::Lanczos3 => ScaleFilter::Lanczos3,
                iv::ScaleFilter::Hermite => ScaleFilter::Hermite,
            },
            dither: profile.dither,
            rotation,
            deband: profile.deband.map(|d| DebandParams {
                iterations: d.iterations,
                threshold: d.threshold,
                radius: d.radius,
                grain: d.grain,
                seed: d.seed,
            }),
        };
        let planes = payload
            .slot
            .as_ref()
            .expect("a live payload keeps its slot")
            .planes
            .to_vec();
        let frame = i_slint_video_wgpu::Frame::from_planes(payload.desc, planes)?;
        gpu.renderer.render(&gpu.device, &gpu.queue, &frame, out)
    }

    /// The same, at the frame's own display size, which is what the scene
    /// letterboxes and what these tests grade against.
    fn render_at_display(gpu: &Gpu, payload: &FramePayload, display: (u32, u32)) -> wgpu::Texture {
        render_presented(gpu, payload, display).expect("the render must succeed")
    }

    // -----------------------------------------------------------------
    // the dmabuf input path, on a real VA-API decoder
    // -----------------------------------------------------------------

    /// Eight vertical bars of rising luma over neutral chroma, packed NV12.
    /// A tiling the import read wrong interleaves 32-row bands and breaks the
    /// rise, which is what the bar check downstream grades.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn nv12_bars(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut buf = vec![128u8; w * h + w * h / 2];
        for row in 0..h {
            for col in 0..w {
                let bar = col * 8 / w;
                buf[row * w + col] = 16 + (bar as u8) * 31;
            }
        }
        buf
    }

    /// Runs a VA-API encode/decode round trip into a plain appsink offering
    /// `caps`, then feeds the last frame through the gpu exactly the way
    /// [`Sink::present`] dispatches on the caps. Returns what was negotiated
    /// and what came out.
    ///
    /// The source is an appsrc rather than videotestsrc: the receiver's
    /// GStreamer is the static playback build, which has no test sources, and
    /// pushing the frames also pins exactly what the bar check expects.
    ///
    /// `None` when the box has no VA-API elements, so this skips on a machine
    /// without one instead of failing.
    /// The pipeline half of [`decode_through`], with every decoded frame
    /// handed to `on_sample` as it arrives rather than only the last. Frames
    /// are presented inside the pull loop on purpose: holding a whole run's
    /// samples would drain the decoder's surface pool and wedge it.
    ///
    /// Returns how many frames came out, or `None` when the box has no
    /// VA-API elements.
    #[cfg(target_os = "linux")]
    fn decode_each(
        caps: &gst::Caps,
        width: u32,
        height: u32,
        frames: u64,
        mut on_sample: impl FnMut(&gst::Sample),
    ) -> Option<usize> {
        init_gst();
        let make = |name: &str| {
            let e = gst::ElementFactory::make(name).build().ok();
            if e.is_none() {
                eprintln!("missing element {name}");
            }
            e
        };
        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::from_str(&format!(
                    "video/x-raw, format=(string)NV12, width=(int){width}, \
                     height=(int){height}, framerate=(fraction)30/1, \
                     colorimetry=(string)bt709"
                ))
                .unwrap(),
            )
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let enc = make("vah264enc")?;
        let parse = make("h264parse")?;
        let dec = make("vah264dec")?;
        let sink = gst_app::AppSink::builder()
            .caps(caps)
            .max_buffers(16)
            .sync(false)
            .build();
        // the same arming make_sink does, and the decoder will not hand out a
        // dmabuf without it
        offer_video_meta(&sink.static_pad("sink").unwrap());

        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &enc, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &enc, &parse, &dec, &sink_element]).unwrap();
        pipeline.set_state(gst::State::Playing).ok()?;

        // identical frames, so the encoder has settled and any decoded one
        // carries the same picture
        let pixels = nv12_bars(width, height);
        for i in 0..frames {
            let mut buffer = gst::Buffer::from_slice(pixels.clone());
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 33));
            if src.push_buffer(buffer).is_err() {
                break;
            }
        }
        let _ = src.end_of_stream();

        let mut seen = 0;
        while let Ok(sample) = sink.pull_sample() {
            seen += 1;
            on_sample(&sample);
        }
        if seen == 0 {
            for msg in pipeline.bus().unwrap().iter() {
                if let gst::MessageView::Error(e) = msg.view() {
                    eprintln!("pipeline error: {} ({:?})", e.error(), e.debug());
                }
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        Some(seen)
    }

    #[cfg(target_os = "linux")]
    fn decode_through(
        gpu: &mut Gpu,
        caps: &gst::Caps,
        width: u32,
        height: u32,
    ) -> Option<(gst::Caps, u32, u32, Vec<u8>, bool)> {
        gst::init().unwrap();
        // eight frames in, the last one out
        let mut last = None;
        decode_each(caps, width, height, 8, |sample| last = Some(sample.clone()))?;
        let sample = last?;

        let caps = sample.caps_owned()?;
        let plan = gpu.parse_caps(caps.clone()).expect("decoder caps must map");
        let presented = match plan.modifier {
            Some(modifier) => {
                assert!(gpu.can_import(), "the lane must be able to import");
                // the decoder's real plane layout, printed so a run shows what
                // the import was told rather than what the caps imply
                let mut sources = [crate::desktop_wgpu_dmabuf::PlaneSource::EMPTY;
                    crate::desktop_wgpu_dmabuf::MAX_PLANES];
                let n = crate::desktop_wgpu_dmabuf::plane_sources(
                    sample.buffer().unwrap(),
                    &plan.info,
                    &mut sources,
                )
                .expect("the dmabuf buffer must carry a layout");
                let layout: Vec<_> = sources[..n]
                    .iter()
                    .map(|s| (s.offset(), s.stride()))
                    .collect();
                eprintln!("plane (offset, stride): {layout:?}");
                gpu.present_imported(
                    sample.buffer_owned().unwrap(),
                    &plan,
                    iv::BufferTransform::Normal,
                    modifier,
                )
                .expect("the dmabuf import must succeed")
            }
            None => {
                let buffer = sample.buffer().unwrap();
                let frame =
                    gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info).unwrap();
                let n = plan.desc.format.plane_count();
                let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
                let mut strides = [0u32; UPLOAD_MAX_PLANES];
                let pitch = frame.plane_stride();
                for i in 0..n {
                    planes[i] = frame.plane_data(i as u32).unwrap();
                    strides[i] = pitch[i] as u32;
                }
                gpu.present(
                    plan.desc,
                    plan.par,
                    iv::BufferTransform::Normal,
                    &planes[..n],
                    &strides[..n],
                )
                .expect("the upload must succeed")
            }
        };
        // The import arm keeps the decoder's buffer inside the frame, which
        // is the whole lifetime rule; the upload arm copied out and keeps
        // nothing. Reported so a caller can grade it.
        let held = presented.slot.as_ref().is_some_and(|s| s.buffer.is_some());
        let tex = as_texture(&gpu, &presented);
        let (w, h) = (tex.width(), tex.height());
        let pixels = i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, &tex, w, h)
            .expect("readback must succeed");
        Some((caps, w, h, pixels, held))
    }

    /// The red channel at the middle of each of the eight bars, sampled on
    /// three rows so a tiling that only scrambles some bands still shows.
    fn bar_reds(pixels: &[u8], w: u32, h: u32) -> Vec<[u8; 8]> {
        [h / 8, h / 2, h * 7 / 8]
            .iter()
            .map(|y| {
                let mut row = [0u8; 8];
                for (i, out) in row.iter_mut().enumerate() {
                    let x = (2 * i as u32 + 1) * w / 16;
                    *out = pixels[((y * w + x) * 4) as usize];
                }
                row
            })
            .collect()
    }

    /// The whole zero-copy input path against a real VA-API decoder.
    ///
    /// Three things are proved at once: the appsink's own caps negotiate
    /// `memory:DMABuf` `DMA_DRM` with the decoder's modifier, the planes are
    /// imported and rendered without the buffer ever being mapped (the sysmem
    /// counter stays at zero), and the result matches the very same clip taken
    /// through the upload path, which is what rules out a tiling the import
    /// The whole lane end to end on a real decoder, with no VA-API needed:
    /// AV1 is decoded, the frame is presented, and what comes out is a
    /// `slint::Image` whose planes and description are the ones the renderer
    /// will convert.
    ///
    /// The VA sibling below proves the same for the zero-copy import arm. This
    /// one runs everywhere dav1d does, which is the machine most of these
    /// gates are run on, and it is the one that keeps the seam covered when
    /// no VA driver is present.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_software_decoded_frame_reaches_slint_as_a_video_image() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let offer = gpu.sysmem_caps();
        let mut last = None;
        let Some(seen) = av1_decode_each(AV1_BARS_8BIT, &[], &offer, |sample| {
            last = Some(sample.clone())
        }) else {
            eprintln!("no dav1ddec in this build, skipping");
            return;
        };
        assert_eq!(seen, 60, "the whole clip must decode");
        let sample = last.expect("a decoded sample");
        let caps = sample.caps_owned().expect("the sample carries caps");
        let plan = gpu.parse_caps(caps).expect("the caps must map");
        assert_eq!(plan.desc.format, PixelFormat::I420);

        // Exactly what `Sink::present` does for a system memory frame.
        let buffer = sample.buffer().unwrap();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info).unwrap();
        let n = plan.desc.format.plane_count();
        let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
        let mut strides = [0u32; UPLOAD_MAX_PLANES];
        let pitch = frame.plane_stride();
        for i in 0..n {
            planes[i] = frame.plane_data(i as u32).unwrap();
            strides[i] = pitch[i] as u32;
        }
        let payload = gpu
            .present(
                plan.desc,
                plan.par,
                iv::BufferTransform::Normal,
                &planes[..n],
                &strides[..n],
            )
            .expect("a decoded frame must present");
        assert_eq!(gpu.sysmem_frames, 1);
        let picture = turned(payload.transform, plan.size);

        let held: Arc<dyn iv::VideoFrame> = Arc::new(SinkFrame::new(payload));
        let image = slint::Image::try_from_video_frame(held, iv::VideoProfile::default())
            .expect("a decoded frame's planes must reach slint");
        assert_eq!((image.size().width, image.size().height), picture);
        let frame = image.to_video_frame().expect("the image wraps the frame");
        let iv::FrameSource::Wgpu30 { planes, format, .. } = frame.source() else {
            panic!("the frame must offer wgpu 30 planes")
        };
        assert_eq!(format, iv::PixelFormat::I420);
        assert_eq!(planes.len(), 3, "i420 is Y + Cb + Cr");
        assert_eq!(frame.color().matrix, iv::Matrix::Bt709);
        assert_eq!(frame.transform(), iv::BufferTransform::Normal);

        // And the picture the renderer will make of them is still the bars.
        let frame = image.to_video_frame().unwrap();
        let iv::FrameSource::Wgpu30 { planes, .. } = frame.source() else {
            unreachable!()
        };
        let rendered = i_slint_video_wgpu::Frame::from_planes(plan.desc, planes.to_vec())
            .expect("the planes must match the description");
        let out = gpu
            .renderer
            .render(
                &gpu.device,
                &gpu.queue,
                &rendered,
                i_slint_video_wgpu::OutputDesc {
                    width: picture.0,
                    height: picture.1,
                    filter: ScaleFilter::Bilinear,
                    dither: false,
                    rotation: i_slint_video_wgpu::Rotation::Rotate0,
                    deband: None,
                },
            )
            .expect("the conversion must run");
        let pixels = i_slint_video_wgpu::gpu::read_rgba8(
            &gpu.device,
            &gpu.queue,
            &out,
            picture.0,
            picture.1,
        )
        .expect("readback must succeed");
        for row in bar_reds(&pixels, picture.0, picture.1) {
            for pair in row.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "the luma ramp is not monotonic across the bars, got {row:?}"
                );
            }
        }
    }

    /// The whole lane end to end, on a real VA-API decoder: the decoder's own
    /// dmabuf is imported, described, and accepted by `slint::Image` as a
    /// video frame.
    ///
    /// The pixel proof lives in
    /// [`a_va_decoded_dmabuf_frame_imports_and_matches_the_uploaded_one`];
    /// what this adds is the last link, which nothing else drives with real
    /// decoder output: that the planes a VA surface imports to satisfy every
    /// check the public constructor makes, that the size slint derives is the
    /// one the lane anchors cues against, and that the decoder's buffer is
    /// still alive inside the image.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_va_decoded_frame_reaches_slint_as_a_video_image() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let (width, height) = (1280u32, 720u32);
        let offer = gpu.caps();
        let mut last = None;
        let Some(seen) = decode_each(&offer, width, height, 8, |sample| {
            last = Some(sample.clone())
        }) else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        assert_eq!(seen, 8, "the whole clip must decode");
        let sample = last.expect("a decoded sample");
        let caps = sample.caps_owned().expect("the sample carries caps");
        assert!(
            gst_video::is_dma_drm_caps(&caps),
            "the decoder did not negotiate memory:DMABuf, got {caps}"
        );
        let plan = gpu.parse_caps(caps).expect("the caps must map");
        let modifier = plan.modifier.expect("dmabuf caps carry a modifier");
        let payload = gpu
            .present_imported(
                sample.buffer_owned().unwrap(),
                &plan,
                iv::BufferTransform::Normal,
                modifier,
            )
            .expect("the import must succeed");
        assert_eq!(gpu.dmabuf_frames, 1);
        assert_eq!(gpu.sysmem_frames, 0, "a frame took the cpu upload path");
        assert!(
            payload.slot.as_ref().unwrap().buffer.is_some(),
            "the decoder's buffer must ride inside the frame"
        );
        let picture = turned(payload.transform, plan.size);
        let generation = payload.generation;

        let frame: Arc<dyn iv::VideoFrame> = Arc::new(SinkFrame::new(payload));
        let image = slint::Image::try_from_video_frame(frame, iv::VideoProfile::default())
            .expect("a VA surface's planes must reach slint");
        assert_eq!((image.size().width, image.size().height), picture);
        let held = image.to_video_frame().expect("the image wraps the frame");
        let iv::FrameSource::Wgpu30 {
            planes,
            format,
            generation: seen_generation,
        } = held.source()
        else {
            panic!("the frame must offer wgpu 30 planes")
        };
        assert_eq!(planes.len(), 2, "nv12 is Y + CbCr");
        assert_eq!(format, iv::PixelFormat::Nv12);
        assert_eq!(seen_generation, generation);
        assert_eq!(held.pixel_aspect_ratio(), plan.par);

        // The pool cannot take the slot back while ANY handle holds the
        // frame, which is the whole lifetime rule: the planes read the
        // decoder's memory. `held` is a second Arc, so dropping the image
        // alone must not release.
        assert_eq!(gpu.pool.free.lock().len(), 0);
        drop(image);
        assert_eq!(
            gpu.pool.free.lock().len(),
            0,
            "a live frame handle must keep the slot"
        );
        drop(held);
        assert_eq!(
            gpu.pool.free.lock().len(),
            1,
            "dropping the last handle is the release"
        );
    }

    /// read wrong.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_va_decoded_dmabuf_frame_imports_and_matches_the_uploaded_one() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let (width, height) = (1280u32, 720u32);
        let offer = gpu.caps();
        let Some((caps, w, h, imported, presented_buffer_held)) =
            decode_through(&mut gpu, &offer, width, height)
        else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        eprintln!("negotiated: {caps}");

        // the offer put the dmabuf structures first and the decoder took one
        assert!(
            gst_video::is_dma_drm_caps(&caps),
            "the decoder did not negotiate memory:DMABuf, got {caps}"
        );
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("format").unwrap(), "DMA_DRM");
        let drm = s.get::<String>("drm-format").unwrap();
        assert!(drm.starts_with("NV12"), "unexpected drm-format {drm}");
        assert_eq!((w, h), (width, height));
        // the proof that nothing was mapped: the upload path never ran
        assert_eq!(gpu.dmabuf_frames, 1);
        assert_eq!(gpu.sysmem_frames, 0, "a frame took the cpu upload path");
        // and the decoder's buffer is still held, since the frame's planes
        // read it rather than a copy of it
        assert!(
            presented_buffer_held,
            "the imported frame did not keep the decoder's buffer"
        );

        // the bars, which a mis-read tiling would scramble. Y-tiling bands 32
        // rows at a time, so the rise has to hold on every sampled row.
        for row in bar_reds(&imported, w, h) {
            for pair in row.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "the luma ramp is not monotonic across the bars, got {row:?}"
                );
            }
            assert!(row[0] < 16, "bar 0 should be black, got {row:?}");
            assert!(row[7] > 235, "bar 7 should be white, got {row:?}");
        }

        // the oracle: the same clip through system memory. videotestsrc and
        // the encoder are both deterministic, so the two renders differ only
        // by the route the planes took, which is to say not at all.
        let mut sysmem = texture_gpu().expect("the device opened once already");
        let sysmem_offer = sysmem.sysmem_caps();
        let Some((sys_caps, sw, sh, uploaded)) =
            decode_through(&mut sysmem, &sysmem_offer, width, height).map(|t| (t.0, t.1, t.2, t.3))
        else {
            eprintln!("no VA-API elements on the second run, skipping the compare");
            return;
        };
        assert!(!gst_video::is_dma_drm_caps(&sys_caps));
        assert_eq!(sysmem.sysmem_frames, 1);
        assert_eq!((sw, sh), (w, h));
        let worst = imported
            .iter()
            .zip(uploaded.iter())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(
            worst <= 4,
            "the imported frame differs from the uploaded one by {worst} codes"
        );
    }

    /// The import arm at rest, on a real decoder pool.
    ///
    /// A VA decoder cycles a fixed set of surfaces, so after its first pass
    /// every frame must find its planes already imported and the renderer
    /// must find that frame's bind group already built. What a miss costs is
    /// measured next to what a hit costs, because a miss is exactly what
    /// every frame cost before the cache existed: two VkImages, two imported
    /// VkDeviceMemory, two views and a descriptor set.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_frame_after_the_decoders_first_pass_is_a_cached_import() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let (width, height) = (640u32, 480u32);
        let offer = gpu.caps();
        let mut miss_cost = Vec::new();
        let mut hit_cost = Vec::new();
        let mut lookup_cost = Vec::new();
        let Some(seen) = decode_each(&offer, width, height, 60, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            let Some(modifier) = plan.modifier else {
                return;
            };
            let buffer = sample.buffer_owned().unwrap();
            let again = sample.buffer_owned().unwrap();
            let before = gpu.imports.misses;
            let (result, allocs) = measure(|| {
                gpu.present_imported(buffer, &plan, iv::BufferTransform::Normal, modifier)
            });
            result.expect("the import must succeed");
            if gpu.imports.misses > before {
                miss_cost.push(allocs);
                return;
            }
            hit_cost.push(allocs);
            // the lane's own share of a cached frame, with the render left
            // out: locate the planes, stat them, match a key. Nothing else on
            // this path is ours, so this is the number that has to be zero.
            let (_, lookup) = measure(|| {
                let mut sources = [crate::desktop_wgpu_dmabuf::PlaneSource::EMPTY;
                    crate::desktop_wgpu_dmabuf::MAX_PLANES];
                let n = crate::desktop_wgpu_dmabuf::plane_sources(&again, &plan.info, &mut sources)
                    .expect("the layout must read back");
                gpu.imports
                    .get_or_import(&gpu.device, &plan.desc, modifier, &sources[..n])
                    .expect("the entry is already there")
            });
            lookup_cost.push(lookup);
        }) else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        assert!(seen > 16, "too few frames to see a pool cycle, got {seen}");

        // counted off the presents, not off the cache, since the lookups
        // measured above hit it a second time
        let (hits, misses) = (hit_cost.len() as u64, miss_cost.len() as u64);
        eprintln!(
            "{seen} frames, {hits} cached imports, {misses} built, \
             pool {}; allocations on a miss {miss_cost:?}, on a hit {hit_cost:?}, \
             on the lookup alone {lookup_cost:?}",
            gpu.imports.len()
        );
        assert_eq!(hits + misses, gpu.dmabuf_frames);
        assert_eq!(gpu.sysmem_frames, 0, "a frame took the cpu upload path");
        // the pool is bounded, so the imports stop after its first cycle
        assert!(
            misses <= crate::desktop_wgpu_dmabuf::MAX_IMPORTS as u64,
            "{misses} imports for one stream, the pool is not being reused"
        );
        assert_eq!(gpu.imports.len() as u64, misses);
        assert_eq!(
            gpu.imports.evictions, 0,
            "the cache is smaller than the pool"
        );
        assert!(hits >= seen as u64 - misses);
        // the receiver renders nothing since wave 5, so the convert bind
        // cache moved into the fork's executor, whose one-adoption-per-pool-
        // buffer discipline is pinned by the dodvg video tests. Here the
        // receiver-side renderer must simply have stayed out of the path.
        assert_eq!(
            gpu.renderer.bind_stats(),
            (0, 0),
            "the import arm must not touch the receiver-side renderer"
        );
        // the lane's own share of a steady-state frame
        assert!(
            lookup_cost.iter().all(|c| *c == 0),
            "finding a cached import allocated: {lookup_cost:?}"
        );

        // a hit is the steady state, and it costs a fraction of the import it
        // replaced
        let worst_hit = *hit_cost.iter().max().unwrap();
        let best_miss = *miss_cost.iter().min().unwrap();
        assert!(
            worst_hit <= FRAME_ALLOC_BUDGET,
            "a cached import frame allocates {worst_hit}, over the {FRAME_ALLOC_BUDGET} wgpu leaves"
        );
        assert!(
            worst_hit < best_miss,
            "the cache saves nothing: hit {worst_hit}, miss {best_miss}"
        );
    }

    // -----------------------------------------------------------------
    // the udmabuf input path, on a real software decoder
    // -----------------------------------------------------------------

    /// [`decode_each`]'s software twin: an h264 round trip through openh264
    /// and libav, into an appsink armed the way [`make_sink`] arms it.
    ///
    /// `formats` is what the sink is allowed to propose a udmabuf pool for, so
    /// the same run with an empty list is the upload-arm control. The decoder
    /// picks its own output format off the offer, which for libav h264 is
    /// I420; that is the point, since a software decoder is the only thing
    /// that ever emits three planes into this lane.
    ///
    /// `None` when the box has no software h264 elements.
    #[cfg(target_os = "linux")]
    fn sw_decode_each(
        formats: &[PixelFormat],
        caps: &gst::Caps,
        width: u32,
        height: u32,
        frames: u64,
        mut on_sample: impl FnMut(&gst::Sample),
    ) -> Option<usize> {
        init_gst();
        let make = |name: &str| {
            let e = gst::ElementFactory::make(name).build().ok();
            if e.is_none() {
                eprintln!("missing element {name}");
            }
            e
        };
        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::from_str(&format!(
                    "video/x-raw, format=(string)I420, width=(int){width}, \
                     height=(int){height}, framerate=(fraction)30/1, \
                     colorimetry=(string)bt709"
                ))
                .unwrap(),
            )
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let enc = make("openh264enc")?;
        let parse = make("h264parse")?;
        let dec = make("avdec_h264")?;
        let sink = gst_app::AppSink::builder()
            .caps(caps)
            .max_buffers(16)
            .sync(false)
            .build();
        let owned: Vec<PixelFormat> = formats.to_vec();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .propose_allocation(move |_, query| {
                    crate::desktop_wgpu_udmabuf::propose(query, &owned)
                })
                .build(),
        );
        // the same arming make_sink does, on both halves of the contract
        offer_video_meta(&sink.static_pad("sink").unwrap());

        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &enc, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &enc, &parse, &dec, &sink_element]).unwrap();
        guarantee_video_meta_in(&pipeline);
        pipeline.set_state(gst::State::Playing).ok()?;

        let pixels = i420_bars(width, height);
        for i in 0..frames {
            let mut buffer = gst::Buffer::from_slice(pixels.clone());
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 33));
            if src.push_buffer(buffer).is_err() {
                break;
            }
        }
        let _ = src.end_of_stream();

        let mut seen = 0;
        while let Ok(sample) = sink.pull_sample() {
            seen += 1;
            on_sample(&sample);
        }
        if seen == 0 {
            for msg in pipeline.bus().unwrap().iter() {
                if let gst::MessageView::Error(e) = msg.view() {
                    eprintln!("pipeline error: {} ({:?})", e.error(), e.debug());
                }
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        Some(seen)
    }

    /// [`nv12_bars`] in three planes, which is what a software h264 decoder
    /// hands back and so what the udmabuf arm has to carry.
    #[cfg(target_os = "linux")]
    fn i420_bars(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut buf = vec![128u8; w * h + w * h / 2];
        for row in 0..h {
            for col in 0..w {
                let bar = col * 8 / w;
                buf[row * w + col] = 16 + (bar as u8) * 31;
            }
        }
        buf
    }

    /// The same eight bars, encoded once as AV1. 640x480, 60 frames, one from
    /// SVT-AV1 at 8 bit and one at 10 bit, both raw OBU streams.
    ///
    /// Canned rather than synthesized because the static tree carries no AV1
    /// encoder at all (`aom` and `svtav1` are both in the build's disable
    /// list), so unlike the h264 arm there is nothing in the pipeline that
    /// could make the clip. Two kilobytes each, which is what eight flat bars
    /// compress to.
    #[cfg(target_os = "linux")]
    const AV1_BARS_8BIT: &[u8] = include_bytes!("testdata/av1-bars-8bit.obu");
    #[cfg(target_os = "linux")]
    const AV1_BARS_10BIT: &[u8] = include_bytes!("testdata/av1-bars-10bit.obu");

    /// The same again at 700x468, which is neither a multiple of the 128 byte
    /// picture alignment nor of the encoder's own block size, so dav1d decodes
    /// 704x472 and crops. That is the case where the pool's video alignment
    /// carries a left and a bottom padding rather than nothing, and where a
    /// plane offset read off the wrong info would shear the picture.
    #[cfg(target_os = "linux")]
    const AV1_BARS_10BIT_UNALIGNED: &[u8] = include_bytes!("testdata/av1-bars-10bit-unaligned.obu");

    /// [`sw_decode_each`]'s AV1 twin: a canned bitstream through dav1ddec into
    /// an appsink armed the way [`make_sink`] arms it.
    ///
    /// `formats` is what the sink may propose a udmabuf pool for, so the same
    /// run with an empty list is the upload-arm control. dav1ddec picks its
    /// output format off the bitstream rather than off the offer, which is
    /// what makes this the 10-bit planar case the h264 arm cannot reach.
    ///
    /// `None` when dav1ddec is not in this build.
    #[cfg(target_os = "linux")]
    fn av1_decode_each(
        stream: &'static [u8],
        formats: &[PixelFormat],
        caps: &gst::Caps,
        mut on_sample: impl FnMut(&gst::Sample),
    ) -> Option<usize> {
        init_gst();
        let make = |name: &str| {
            let e = gst::ElementFactory::make(name).build().ok();
            if e.is_none() {
                eprintln!("missing element {name}");
            }
            e
        };
        let src = gst_app::AppSrc::builder()
            .caps(&gst::Caps::from_str("video/x-av1").unwrap())
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let parse = make("av1parse")?;
        let dec = make("dav1ddec")?;
        let sink = gst_app::AppSink::builder()
            .caps(caps)
            .max_buffers(16)
            .sync(false)
            .build();
        let owned: Vec<PixelFormat> = formats.to_vec();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .propose_allocation(move |_, query| {
                    crate::desktop_wgpu_udmabuf::propose(query, &owned)
                })
                .build(),
        );
        offer_video_meta(&sink.static_pad("sink").unwrap());

        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &parse, &dec, &sink_element]).unwrap();
        guarantee_video_meta_in(&pipeline);
        pipeline.set_state(gst::State::Playing).ok()?;

        // one push, the parser splits it into temporal units
        let mut buffer = gst::Buffer::from_slice(stream);
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_nseconds(0));
        let _ = src.push_buffer(buffer);
        let _ = src.end_of_stream();

        let mut seen = 0;
        while let Ok(sample) = sink.pull_sample() {
            seen += 1;
            on_sample(&sample);
        }
        if seen == 0 {
            for msg in pipeline.bus().unwrap().iter() {
                if let gst::MessageView::Error(e) = msg.view() {
                    eprintln!("pipeline error: {} ({:?})", e.error(), e.debug());
                }
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        Some(seen)
    }

    /// Process cpu time in milliseconds, user plus system, off
    /// `/proc/self/stat`. One scheduler tick of resolution, which is all a
    /// log line comparing two arms needs.
    #[cfg(target_os = "linux")]
    fn cpu_millis() -> u64 {
        let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
            return 0;
        };
        // the comm field can hold spaces and brackets, so split at its close
        let Some((_, tail)) = stat.rsplit_once(')') else {
            return 0;
        };
        let fields: Vec<&str> = tail.split_whitespace().collect();
        // state is field 3, so utime and stime are offsets 11 and 12 here
        let ticks: u64 = fields
            .get(11)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            + fields
                .get(12)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
        // USER_HZ is 100 on every linux this ships on
        ticks * 10
    }

    /// The whole software zero-copy arm, end to end on a real libav decoder.
    ///
    /// Five things at once: the decoder takes the proposed pool and its frames
    /// arrive as dmabufs under plain system-memory caps, the lane recognises
    /// them off the memory type rather than the caps and imports them, no
    /// frame is ever mapped for pixels, the import cache settles into hits
    /// after the pool's first cycle, and a steady-state frame stays inside the
    /// same allocation budget the upload arm is held to.
    #[cfg(target_os = "linux")]
    #[test]
    fn software_decoded_frames_arrive_as_udmabuf_imports() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if gpu.udmabuf.is_empty() {
            eprintln!("no udmabuf import route on this box, skipping");
            return;
        }
        let formats: Vec<PixelFormat> = gpu.udmabuf.to_vec();
        let (width, height) = (640u32, 480u32);
        let offer = gpu.sysmem_caps();

        let mut dmabuf_seen = 0usize;
        let mut plain_seen = 0usize;
        let mut hit_cost = Vec::new();
        let mut miss_cost = Vec::new();
        let Some(seen) = sw_decode_each(&formats, &offer, width, height, 60, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            assert!(plan.modifier.is_none(), "software caps carry no modifier");
            let buffer = sample.buffer().unwrap();
            if !crate::desktop_wgpu_udmabuf::is_dmabuf(buffer) {
                plain_seen += 1;
                return;
            }
            dmabuf_seen += 1;
            let before = gpu.imports.misses;
            let (result, allocs) =
                measure(|| gpu.present_linear(sample, buffer, &plan, iv::BufferTransform::Normal));
            let result = result.expect("a dmabuf frame must take the import route");
            result.expect("the linear import must succeed");
            if gpu.imports.misses > before {
                miss_cost.push(allocs);
            } else {
                hit_cost.push(allocs);
            }
        }) else {
            eprintln!("no software h264 elements, skipping");
            return;
        };
        assert!(seen > 16, "too few frames to see a pool cycle, got {seen}");

        eprintln!(
            "{seen} software frames, {dmabuf_seen} as dmabufs, {plain_seen} as plain sysmem, \
             pool {}; allocations on a miss {miss_cost:?}, on a hit {hit_cost:?}",
            gpu.imports.len()
        );
        assert!(
            dmabuf_seen > 0,
            "the decoder never took the proposed udmabuf pool"
        );
        assert_eq!(
            plain_seen, 0,
            "the decoder took the pool and then handed out plain memory"
        );
        assert_eq!(gpu.dmabuf_frames, dmabuf_seen as u64);
        assert_eq!(
            gpu.sysmem_frames, 0,
            "a software frame was mapped and copied on the cpu"
        );
        // the pool is walked in a cycle, so the imports stop after its first
        assert_eq!(gpu.imports.len(), miss_cost.len());
        assert_eq!(
            gpu.imports.evictions, 0,
            "the cache is smaller than the pool"
        );
        assert!(
            hit_cost.len() > miss_cost.len(),
            "the cache never settled: {} hits against {} imports",
            hit_cost.len(),
            miss_cost.len()
        );
        let worst_hit = *hit_cost.iter().max().unwrap();
        assert!(
            worst_hit <= FRAME_ALLOC_BUDGET,
            "a cached udmabuf frame allocates {worst_hit}, over the {FRAME_ALLOC_BUDGET} \
             wgpu leaves"
        );
    }

    /// The two software arms side by side, on the same clip.
    ///
    /// The picture has to match: a udmabuf is linear and the video meta
    /// carries the decoder's padded pitch, so an import that read either wrong
    /// would show a sheared frame rather than fail. The cost is printed rather
    /// than asserted, because what it is worth depends on the box, the
    /// resolution and how much of the upload the driver hides.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_udmabuf_arm_renders_what_the_upload_arm_does_and_is_measured_against_it() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if gpu.udmabuf.is_empty() {
            eprintln!("no udmabuf import route on this box, skipping");
            return;
        }
        let formats: Vec<PixelFormat> = gpu.udmabuf.to_vec();
        let (width, height) = (1280u32, 720u32);
        let offer = gpu.sysmem_caps();
        let frames = 40;

        // the upload arm first, with nothing proposed, which is the lane as it
        // was before this module existed. Only the present is timed: the
        // encode and the decode either side of it are the same work on both
        // arms and would bury the difference.
        let mut uploaded = None;
        let mut upload_wall = std::time::Duration::ZERO;
        let mut upload_cpu = 0u64;
        let mut upload_bytes = 0u64;
        let Some(seen) = sw_decode_each(&[], &offer, width, height, frames, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            let buffer = sample.buffer().unwrap();
            assert!(
                !crate::desktop_wgpu_udmabuf::is_dmabuf(buffer),
                "nothing was proposed, so nothing can be a dmabuf"
            );
            let frame =
                gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info).unwrap();
            let n = plan.desc.format.plane_count();
            let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
            let mut strides = [0u32; UPLOAD_MAX_PLANES];
            let pitch = frame.plane_stride();
            for i in 0..n {
                planes[i] = frame.plane_data(i as u32).unwrap();
                strides[i] = pitch[i] as u32;
                upload_bytes += planes[i].len() as u64;
            }
            let at = std::time::Instant::now();
            let cpu = cpu_millis();
            let presented = gpu
                .present(
                    plan.desc,
                    plan.par,
                    iv::BufferTransform::Normal,
                    &planes[..n],
                    &strides[..n],
                )
                .expect("the upload must succeed");
            upload_wall += at.elapsed();
            upload_cpu += cpu_millis() - cpu;
            uploaded = Some(presented);
        }) else {
            eprintln!("no software h264 elements, skipping");
            return;
        };
        let upload_pixels = read_presented(&gpu, uploaded.take().expect("a frame must arrive"));

        // and the udmabuf arm on the same clip
        gpu.release_held();
        gpu.caps = None;
        let mut imported = None;
        let mut import_wall = std::time::Duration::ZERO;
        let mut import_cpu = 0u64;
        sw_decode_each(&formats, &offer, width, height, frames, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            let buffer = sample.buffer().unwrap();
            let at = std::time::Instant::now();
            let cpu = cpu_millis();
            let Some(result) =
                gpu.present_linear(sample, buffer, &plan, iv::BufferTransform::Normal)
            else {
                return;
            };
            import_wall += at.elapsed();
            import_cpu += cpu_millis() - cpu;
            imported = Some(result.expect("the linear import must succeed"));
        })
        .expect("the elements were there a moment ago");
        let Some(last) = imported.take() else {
            eprintln!("the decoder ignored the proposed pool, nothing to compare");
            return;
        };
        let import_pixels = read_presented(&gpu, last);

        eprintln!(
            "{seen} frames at {width}x{height}, present only: upload arm {upload_wall:?} wall / \
             {upload_cpu}ms cpu over {}MB mapped and copied, udmabuf arm {import_wall:?} wall / \
             {import_cpu}ms cpu over 0MB, {} imports for {} frames",
            upload_bytes / (1024 * 1024),
            gpu.imports.len(),
            gpu.dmabuf_frames
        );
        assert!(gpu.dmabuf_frames > 0, "the decoder ignored the pool");
        assert_eq!(
            gpu.sysmem_frames, seen as u64,
            "the control arm is the upload"
        );

        // the same eight bars out of both arms
        let a = bar_reds(&upload_pixels, width, height);
        let b = bar_reds(&import_pixels, width, height);
        let worst = a
            .iter()
            .zip(&b)
            .flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| p.abs_diff(*q)))
            .max()
            .unwrap();
        assert!(
            worst <= 4,
            "the imported frame differs from the uploaded one by {worst} codes: \
             {a:?} against {b:?}"
        );
    }

    /// The udmabuf arm on dav1ddec, both arms of it, on one canned clip.
    ///
    /// Software AV1 is what carries HDR on a box with no AV1 hardware, and at
    /// 4K 10-bit that is ~24MB of upload a frame. dav1ddec does not write into
    /// buffers it was handed, it writes through a dav1d picture allocator with
    /// a 128 byte alignment and edge padding contract, so what is asked here is
    /// whether a generic downstream pool can be made to satisfy that contract:
    /// every frame arrives as a dmabuf, none falls back to the upload arm, and
    /// the picture is still the bars that went in.
    ///
    /// The upload arm runs first on the same clip and is the control for both
    /// halves, the counters and the pixels.
    #[cfg(target_os = "linux")]
    fn the_av1_udmabuf_arm(
        stream: &'static [u8],
        expect: gst_video::VideoFormat,
        (width, height): (u32, u32),
    ) {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if gpu.udmabuf.is_empty() {
            eprintln!("no udmabuf import route on this box, skipping");
            return;
        }
        if !gpu
            .udmabuf
            .contains(&map_format(expect).expect("a format the lane knows"))
        {
            eprintln!("{expect:?} does not import linearly on this box, skipping");
            return;
        }
        let formats: Vec<PixelFormat> = gpu.udmabuf.to_vec();
        let offer = gpu.sysmem_caps();

        // the upload arm first, with nothing proposed: the lane before the
        // pool existed, and the picture the imported frames are graded on
        let mut uploaded = None;
        let Some(seen) = av1_decode_each(stream, &[], &offer, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            assert_eq!(
                (plan.info.format(), plan.info.width(), plan.info.height()),
                (expect, width, height),
                "dav1ddec decoded the clip to the wrong format or size"
            );
            let buffer = sample.buffer().unwrap();
            assert!(
                !crate::desktop_wgpu_udmabuf::is_dmabuf(buffer),
                "nothing was proposed, so nothing can be a dmabuf"
            );
            let frame =
                gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info).unwrap();
            let n = plan.desc.format.plane_count();
            let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
            let mut strides = [0u32; UPLOAD_MAX_PLANES];
            let pitch = frame.plane_stride();
            for i in 0..n {
                planes[i] = frame.plane_data(i as u32).unwrap();
                strides[i] = pitch[i] as u32;
            }
            uploaded = Some(
                gpu.present(
                    plan.desc,
                    plan.par,
                    iv::BufferTransform::Normal,
                    &planes[..n],
                    &strides[..n],
                )
                .expect("the upload must succeed"),
            );
        }) else {
            eprintln!("no dav1ddec in this build, skipping");
            return;
        };
        assert!(seen > 16, "too few frames to see a pool cycle, got {seen}");
        assert_eq!(
            gpu.sysmem_frames, seen as u64,
            "the control arm is the upload"
        );
        assert_eq!(gpu.dmabuf_frames, 0, "nothing was proposed to import");
        let upload_pixels = read_presented(&gpu, uploaded.take().expect("a frame must arrive"));

        // and the same clip with the pool on offer
        gpu.release_held();
        gpu.caps = None;
        let mut imported = None;
        let mut dmabuf_seen = 0usize;
        let mut plain_seen = 0usize;
        av1_decode_each(stream, &formats, &offer, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            assert!(plan.modifier.is_none(), "software caps carry no modifier");
            let buffer = sample.buffer().unwrap();
            if !crate::desktop_wgpu_udmabuf::is_dmabuf(buffer) {
                plain_seen += 1;
                return;
            }
            dmabuf_seen += 1;
            let result = gpu
                .present_linear(sample, buffer, &plan, iv::BufferTransform::Normal)
                .expect("a dmabuf frame must take the import route");
            imported = Some(result.expect("the linear import must succeed"));
        })
        .expect("dav1ddec was there a moment ago");

        eprintln!(
            "{seen} {expect:?} av1 frames, {dmabuf_seen} as dmabufs, {plain_seen} as plain \
             sysmem, {} imports",
            gpu.imports.len()
        );
        assert!(
            dmabuf_seen > 0,
            "dav1ddec never took the proposed udmabuf pool"
        );
        assert_eq!(
            plain_seen, 0,
            "dav1ddec took the pool and then handed out plain memory"
        );
        assert_eq!(gpu.dmabuf_frames, dmabuf_seen as u64);
        assert_eq!(
            gpu.sysmem_frames, seen as u64,
            "the control arm is the upload, and nothing after it"
        );
        // the pool is walked in a cycle, so the imports stop after its first
        assert!(
            gpu.imports.len() < dmabuf_seen,
            "the import cache never hit"
        );
        assert_eq!(
            gpu.imports.evictions, 0,
            "the cache is smaller than the pool"
        );

        // the same eight bars out of both arms
        let import_pixels = read_presented(&gpu, imported.take().expect("a frame must arrive"));
        let a = bar_reds(&upload_pixels, width, height);
        let b = bar_reds(&import_pixels, width, height);
        let worst = a
            .iter()
            .zip(&b)
            .flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| p.abs_diff(*q)))
            .max()
            .unwrap();
        assert!(
            worst <= 4,
            "the imported frame differs from the uploaded one by {worst} codes: \
             {a:?} against {b:?}"
        );
    }

    /// 8-bit AV1, which decodes to three-plane I420.
    #[cfg(target_os = "linux")]
    #[test]
    fn eight_bit_av1_frames_arrive_as_udmabuf_imports() {
        the_av1_udmabuf_arm(AV1_BARS_8BIT, gst_video::VideoFormat::I420, (640, 480));
    }

    /// 10-bit AV1, which decodes to I420_10LE and is the HDR case the whole
    /// arm exists for. Nothing else in this suite reaches a wide planar format
    /// off a real decoder.
    #[cfg(target_os = "linux")]
    #[test]
    fn ten_bit_av1_frames_arrive_as_udmabuf_imports() {
        the_av1_udmabuf_arm(AV1_BARS_10BIT, gst_video::VideoFormat::I42010le, (640, 480));
    }

    /// The same, at a size dav1d has to pad and crop on every axis. Both
    /// paddings are non-zero here, so a pool whose alignment the decoder
    /// could not express would either refuse the config or hand back a
    /// sheared picture.
    #[cfg(target_os = "linux")]
    #[test]
    fn unaligned_ten_bit_av1_frames_arrive_as_udmabuf_imports() {
        the_av1_udmabuf_arm(
            AV1_BARS_10BIT_UNALIGNED,
            gst_video::VideoFormat::I42010le,
            (700, 468),
        );
    }

    /// dav1ddec with nothing downstream that proposes anything, which is every
    /// other consumer of the decoder. It must still decode, and every frame
    /// must come out of its own pool as plain system memory.
    #[cfg(target_os = "linux")]
    #[test]
    fn av1_still_decodes_when_downstream_proposes_no_pool() {
        init_gst();
        let offer = gst_video::VideoCapsBuilder::new()
            .format_list([gst_video::VideoFormat::I420])
            .build();
        let mut seen_plain = 0usize;
        let Some(seen) = av1_decode_each(AV1_BARS_8BIT, &[], &offer, |sample| {
            let buffer = sample.buffer().unwrap();
            assert!(
                !crate::desktop_wgpu_udmabuf::is_dmabuf(buffer),
                "nothing was proposed, so nothing can be a dmabuf"
            );
            seen_plain += 1;
        }) else {
            eprintln!("no dav1ddec in this build, skipping");
            return;
        };
        assert_eq!(seen, 60, "the whole clip must decode");
        assert_eq!(seen_plain, seen);
    }

    // -----------------------------------------------------------------
    // the iosurface input path, on a real VideoToolbox decoder
    // -----------------------------------------------------------------

    /// A VideoToolbox encode/decode round trip into a plain appsink offering
    /// `caps`, with every decoded frame handed to `on_sample` as it arrives.
    ///
    /// The mac twin of [`decode_each`], the same shape for the same reasons:
    /// the receiver's GStreamer is the static playback build with no test
    /// sources, so the bars are pushed in, and frames are consumed inside the
    /// pull loop rather than collected, or the decoder's pixel buffer pool
    /// drains and it wedges.
    ///
    /// `None` when the box has no VideoToolbox elements.
    #[cfg(target_os = "macos")]
    fn vt_decode_each(
        caps: &gst::Caps,
        width: u32,
        height: u32,
        frames: u64,
        on_sample: impl FnMut(&gst::Sample),
    ) -> Option<usize> {
        vt_decode_each_of(VtClip::H264_8BIT, caps, width, height, frames, on_sample)
    }

    /// One encode/decode round trip's codec, and the raw format that feeds
    /// it. The 10-bit arm is what makes vtdec pick P010 over NV12, since it
    /// reads the parsed bit depth off its input caps.
    #[cfg(target_os = "macos")]
    #[derive(Clone, Copy)]
    struct VtClip {
        format: &'static str,
        encoder: &'static str,
        parser: &'static str,
        /// What the encoder's output is pinned to. VideoToolbox picks its
        /// profile off the DOWNSTREAM caps rather than off the raw format it
        /// is fed, so a 10-bit stream has to be asked for by name or the
        /// encoder quantizes to Main and the decode comes back NV12.
        encoded: &'static str,
    }

    #[cfg(target_os = "macos")]
    impl VtClip {
        const H264_8BIT: VtClip = VtClip {
            format: "NV12",
            encoder: "vtenc_h264",
            parser: "h264parse",
            encoded: "video/x-h264",
        };
        const H265_10BIT: VtClip = VtClip {
            format: "P010_10LE",
            encoder: "vtenc_h265",
            parser: "h265parse",
            encoded: "video/x-h265, profile=(string)main-10",
        };

        fn bars(&self, width: u32, height: u32) -> Vec<u8> {
            if self.format == "NV12" {
                nv12_bars(width, height)
            } else {
                p010_bars(width, height)
            }
        }
    }

    /// The same bars as [`nv12_bars`], in P010: ten bits of luma sitting in
    /// the top of a 16-bit word, neutral chroma interleaved at half
    /// resolution. An import that read the wide planes as 8-bit would break
    /// the rise the bar check grades.
    #[cfg(target_os = "macos")]
    fn p010_bars(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut buf = vec![0u8; (w * h + w * h / 2) * 2];
        for row in 0..h {
            for col in 0..w {
                let bar = col * 8 / w;
                // the 8-bit code the other clip uses, left in the top ten
                // bits of the word
                let code = ((16 + bar as u16 * 31) << 8).to_le_bytes();
                let at = (row * w + col) * 2;
                buf[at] = code[0];
                buf[at + 1] = code[1];
            }
        }
        // neutral chroma, 512 in ten bits
        let chroma = (512u16 << 6).to_le_bytes();
        for at in (w * h * 2..buf.len()).step_by(2) {
            buf[at] = chroma[0];
            buf[at + 1] = chroma[1];
        }
        buf
    }

    #[cfg(target_os = "macos")]
    fn vt_decode_each_of(
        clip: VtClip,
        caps: &gst::Caps,
        width: u32,
        height: u32,
        frames: u64,
        mut on_sample: impl FnMut(&gst::Sample),
    ) -> Option<usize> {
        init_gst();
        let make = |name: &str| {
            let e = gst::ElementFactory::make(name).build().ok();
            if e.is_none() {
                eprintln!("missing element {name}");
            }
            e
        };
        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::from_str(&format!(
                    "video/x-raw, format=(string){}, width=(int){width}, \
                     height=(int){height}, framerate=(fraction)30/1, \
                     colorimetry=(string)bt709",
                    clip.format
                ))
                .unwrap(),
            )
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let enc = make(clip.encoder)?;
        // in input order and without a reordering delay, so the pull loop
        // sees the pool cycle rather than the encoder's lookahead
        enc.set_property("allow-frame-reordering", false);
        enc.set_property("realtime", true);
        let parse = make(clip.parser)?;
        let encoded = gst::ElementFactory::make("capsfilter")
            .property("caps", gst::Caps::from_str(clip.encoded).unwrap())
            .build()
            .ok()?;
        let dec = make("vtdec_hw").or_else(|| make("vtdec"))?;
        let sink = gst_app::AppSink::builder()
            .caps(caps)
            .max_buffers(16)
            .sync(false)
            .build();
        // the same arming make_sink does. VideoToolbox pads its rows, and
        // without the meta the padding would be copied out on the way down.
        offer_video_meta(&sink.static_pad("sink").unwrap());

        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &enc, &encoded, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &enc, &encoded, &parse, &dec, &sink_element])
            .unwrap();
        pipeline.set_state(gst::State::Playing).ok()?;

        // identical frames, so the encoder has settled and any decoded one
        // carries the same picture
        let pixels = clip.bars(width, height);
        for i in 0..frames {
            let mut buffer = gst::Buffer::from_slice(pixels.clone());
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 33));
            if src.push_buffer(buffer).is_err() {
                break;
            }
        }
        let _ = src.end_of_stream();

        let mut seen = 0;
        while let Ok(sample) = sink.pull_sample() {
            seen += 1;
            on_sample(&sample);
        }
        if seen == 0 {
            for msg in pipeline.bus().unwrap().iter() {
                if let gst::MessageView::Error(e) = msg.view() {
                    eprintln!("pipeline error: {} ({:?})", e.error(), e.debug());
                }
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        Some(seen)
    }

    /// The whole mac lane end to end, on a real VideoToolbox decoder: the
    /// decoder's own IOSurface is imported as metal textures, rendered, and
    /// graded against the same frame taken through the map-and-upload arm.
    ///
    /// One sample feeds both routes, which is a sharper oracle than the VA
    /// test can build: a dmabuf cannot be mapped for pixels, a CVPixelBuffer
    /// can, so the two renders come from the same decoded picture and any
    /// difference is the import's.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_videotoolbox_frame_imports_and_matches_the_uploaded_one() {
        init_gst();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if gpu.iosurface.is_empty() {
            eprintln!("no iosurface import on this device, skipping");
            return;
        }
        let (width, height) = (1280u32, 720u32);
        let offer = gpu.caps();
        // the offer is system memory, exactly as it was before the import
        assert!(
            !offer.iter().any(|s| s.has_field("drm-format")),
            "the mac offer must carry no dmabuf structure, got {offer}"
        );
        let mut last = None;
        let Some(seen) = vt_decode_each(&offer, width, height, 8, |sample| {
            last = Some(sample.clone())
        }) else {
            eprintln!("no VideoToolbox elements, skipping");
            return;
        };
        assert!(seen > 0, "the clip must decode");
        let sample = last.expect("a decoded sample");
        let caps = sample.caps_owned().expect("the sample carries caps");
        eprintln!("negotiated: {caps}");
        let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
        assert_eq!(plan.desc.format, PixelFormat::Nv12);
        assert_eq!((plan.desc.width, plan.desc.height), (width, height));

        // the frame arrived under plain system memory caps and is an
        // IOSurface anyway, which is the whole premise of the route
        let buffer = sample.buffer().expect("the sample carries a buffer");
        assert!(
            crate::desktop_wgpu_iosurface::is_iosurface(buffer),
            "a VideoToolbox frame must expose its surface"
        );

        let payload = gpu
            .present_iosurface(&sample, buffer, &plan, iv::BufferTransform::Normal)
            .expect("an iosurface frame must take the import route")
            .expect("the import must succeed");
        assert_eq!(gpu.iosurface_frames, 1);
        assert_eq!(gpu.sysmem_frames, 0, "the frame took the cpu upload path");
        // the decoder's buffer is still held, since the frame's planes read
        // its surface rather than a copy of it
        assert!(
            payload.slot.as_ref().is_some_and(|s| s.buffer.is_some()),
            "the imported frame did not keep the decoder's buffer"
        );
        // the last link, which nothing else on this lane drives with real
        // VideoToolbox output: the planes an IOSurface imports to satisfy
        // every check the public constructor makes, the size slint derives
        // is the one the lane anchors cues against, and the decoder's buffer
        // is still inside the image.
        let picture = turned(payload.transform, plan.size);
        let held: Arc<dyn iv::VideoFrame> = Arc::new(SinkFrame::new(payload));
        let image = slint::Image::try_from_video_frame(held, iv::VideoProfile::default())
            .expect("an imported frame's planes must reach slint");
        assert_eq!((image.size().width, image.size().height), picture);
        let shown = image.to_video_frame().expect("the image wraps the frame");
        let iv::FrameSource::Wgpu30 { planes, format, .. } = shown.source() else {
            panic!("the frame must offer wgpu 30 planes")
        };
        assert_eq!(format, iv::PixelFormat::Nv12);
        assert_eq!(planes.len(), 2, "nv12 is luma + interleaved chroma");
        assert_eq!(shown.color().matrix, iv::Matrix::Bt709);

        let rendered = i_slint_video_wgpu::Frame::from_planes(plan.desc, planes.to_vec())
            .expect("the planes must match the description");
        let out = gpu
            .renderer
            .render(
                &gpu.device,
                &gpu.queue,
                &rendered,
                i_slint_video_wgpu::OutputDesc {
                    width: picture.0,
                    height: picture.1,
                    filter: ScaleFilter::Bilinear,
                    dither: false,
                    rotation: i_slint_video_wgpu::Rotation::Rotate0,
                    deband: None,
                },
            )
            .expect("the conversion must run");
        let imported = i_slint_video_wgpu::gpu::read_rgba8(
            &gpu.device,
            &gpu.queue,
            &out,
            picture.0,
            picture.1,
        )
        .expect("readback must succeed");

        // the bars, which a mis-read surface would scramble
        for row in bar_reds(&imported, width, height) {
            for pair in row.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "the luma ramp is not monotonic across the bars, got {row:?}"
                );
            }
            assert!(row[0] < 16, "bar 0 should be black, got {row:?}");
            assert!(row[7] > 235, "bar 7 should be white, got {row:?}");
        }

        // the oracle: the same buffer, mapped and uploaded, which is what the
        // lane did for every mac frame before this route existed
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info).unwrap();
        let n = plan.desc.format.plane_count();
        let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
        let mut strides = [0u32; UPLOAD_MAX_PLANES];
        let pitch = frame.plane_stride();
        for i in 0..n {
            planes[i] = frame.plane_data(i as u32).unwrap();
            strides[i] = pitch[i] as u32;
        }
        let uploaded = gpu
            .present(
                plan.desc,
                plan.par,
                iv::BufferTransform::Normal,
                &planes[..n],
                &strides[..n],
            )
            .expect("the upload must succeed");
        let uploaded = read_presented_mac(&gpu, uploaded);
        let worst = imported
            .iter()
            .zip(uploaded.iter())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(
            worst <= 1,
            "the imported frame differs from the uploaded one by {worst} codes"
        );
    }

    /// Renders a presented frame at its own size and reads it back as rgba8.
    #[cfg(target_os = "macos")]
    fn read_presented_mac(gpu: &Gpu, payload: FramePayload) -> Vec<u8> {
        let tex = as_texture(gpu, &payload);
        let (w, h) = (tex.width(), tex.height());
        i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, &tex, w, h)
            .expect("readback must succeed")
    }

    /// The wide arm of the same proof: a 10-bit stream decodes to P010 and
    /// its planes import as 16-bit norm textures.
    ///
    /// Worth its own run because everything about it is a different code
    /// path from NV12: vtdec picks the format off the parsed bit depth, the
    /// plane table asks for R16Unorm and Rg16Unorm, and Metal has to accept
    /// those over a `kCVPixelFormatType_420YpCbCr10BiPlanar` surface. The
    /// oracle is the same buffer through the map-and-upload arm, so a wrong
    /// texel format shows as a mismatch rather than as a plausible picture.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_ten_bit_videotoolbox_frame_imports_as_p010() {
        init_gst();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.iosurface.contains(&PixelFormat::P010) {
            eprintln!("no 16-bit norm textures on this device, skipping");
            return;
        }
        let (width, height) = (1280u32, 720u32);
        let offer = gpu.caps();
        let mut last = None;
        let Some(seen) =
            vt_decode_each_of(VtClip::H265_10BIT, &offer, width, height, 8, |sample| {
                last = Some(sample.clone())
            })
        else {
            eprintln!("no VideoToolbox hevc elements, skipping");
            return;
        };
        assert!(seen > 0, "the clip must decode");
        let sample = last.expect("a decoded sample");
        let caps = sample.caps_owned().expect("the sample carries caps");
        eprintln!("negotiated: {caps}");
        let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
        // the depth-matched pick: an 8-bit format here would mean the
        // decoder quantized the stream on its way out
        assert_eq!(plan.desc.format, PixelFormat::P010);

        let buffer = sample.buffer().expect("the sample carries a buffer");
        let payload = gpu
            .present_iosurface(&sample, buffer, &plan, iv::BufferTransform::Normal)
            .expect("a 10-bit iosurface frame must take the import route")
            .expect("the import must succeed");
        assert_eq!(gpu.iosurface_frames, 1);
        assert_eq!(gpu.sysmem_frames, 0, "the frame took the cpu upload path");
        let imported = read_presented_mac(&gpu, payload);
        for row in bar_reds(&imported, width, height) {
            for pair in row.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "the luma ramp is not monotonic across the bars, got {row:?}"
                );
            }
            assert!(row[0] < 16, "bar 0 should be black, got {row:?}");
            assert!(row[7] > 235, "bar 7 should be white, got {row:?}");
        }

        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &plan.info).unwrap();
        let n = plan.desc.format.plane_count();
        let mut planes: [&[u8]; UPLOAD_MAX_PLANES] = [&[]; UPLOAD_MAX_PLANES];
        let mut strides = [0u32; UPLOAD_MAX_PLANES];
        let pitch = frame.plane_stride();
        for i in 0..n {
            planes[i] = frame.plane_data(i as u32).unwrap();
            strides[i] = pitch[i] as u32;
        }
        let uploaded = gpu
            .present(
                plan.desc,
                plan.par,
                iv::BufferTransform::Normal,
                &planes[..n],
                &strides[..n],
            )
            .expect("the upload must succeed");
        let uploaded = read_presented_mac(&gpu, uploaded);
        let worst = imported
            .iter()
            .zip(uploaded.iter())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(
            worst <= 1,
            "the imported frame differs from the uploaded one by {worst} codes"
        );
    }

    /// The cache's invalidation rule on a live VideoToolbox decoder: entries
    /// describe one geometry, so a stream that changes size must drop every
    /// one of them rather than match a key against surfaces of the wrong
    /// shape. The mac twin of
    /// [`a_resolution_change_drops_every_import`], which pins the same rule
    /// for dmabufs.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_resolution_change_drops_every_iosurface_import() {
        init_gst();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if gpu.iosurface.is_empty() {
            eprintln!("no iosurface import on this device, skipping");
            return;
        }
        let mut held = Vec::new();
        for (i, (width, height)) in [(1280u32, 720u32), (640u32, 480u32)]
            .into_iter()
            .enumerate()
        {
            let offer = gpu.caps();
            let mut presented = 0u32;
            let Some(seen) = vt_decode_each(&offer, width, height, 24, |sample| {
                let Some(caps) = sample.caps_owned() else {
                    return;
                };
                let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
                assert_eq!(
                    (plan.desc.width, plan.desc.height),
                    (width, height),
                    "the decoder changed size under the test"
                );
                let buffer = sample.buffer().unwrap();
                let payload = gpu
                    .present_iosurface(sample, buffer, &plan, iv::BufferTransform::Normal)
                    .expect("an iosurface frame must take the import route")
                    .expect("the import must succeed");
                presented += 1;
                // A few frames stay alive across the size change, which is
                // what a real stream does: the sink holds the last picture
                // while the next one negotiates.
                if presented <= 2 {
                    held.push(payload);
                }
            }) else {
                eprintln!("no VideoToolbox elements, skipping");
                return;
            };
            assert!(seen > 2, "too few frames at {width}x{height}, got {seen}");
            if i == 0 {
                // the decoder's pool cycled, so more than one surface is cached
                assert!(
                    gpu.imports.len() > 1,
                    "one import for a whole stream, the pool did not cycle"
                );
            } else {
                // the new geometry cleared the old entries and rebuilt its own
                assert!(
                    gpu.imports.len() <= (seen).min(32),
                    "the cache kept more entries than the new stream produced"
                );
                assert_eq!(
                    gpu.imports.evictions, 0,
                    "a cleared cache must not have evicted anything"
                );
            }
        }
        // the frames held across the change still own their planes, so
        // dropping them here is what returns the old surfaces
        assert_eq!(held.len(), 4, "two frames held from each size");
    }

    /// The import arm at rest, on a real VideoToolbox pool.
    ///
    /// VideoToolbox cycles a fixed set of pixel buffers, so after its first
    /// pass every frame must find its planes already imported. What a miss
    /// costs is measured next to what a hit costs, because a miss is what
    /// every frame would cost without the cache: two MTLTextures, two views
    /// and a bind group.
    #[cfg(target_os = "macos")]
    #[test]
    fn every_videotoolbox_frame_after_the_first_pass_is_a_cached_import() {
        init_gst();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if gpu.iosurface.is_empty() {
            eprintln!("no iosurface import on this device, skipping");
            return;
        }
        let (width, height) = (640u32, 480u32);
        let offer = gpu.caps();
        let mut miss_cost = Vec::new();
        let mut hit_cost = Vec::new();
        let mut lookup_cost = Vec::new();
        let Some(seen) = vt_decode_each(&offer, width, height, 60, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            let buffer = sample.buffer().unwrap();
            let before = gpu.imports.misses;
            let (result, allocs) = measure(|| {
                gpu.present_iosurface(sample, buffer, &plan, iv::BufferTransform::Normal)
            });
            result
                .expect("an iosurface frame must take the import route")
                .expect("the import must succeed");
            if gpu.imports.misses > before {
                miss_cost.push(allocs);
                return;
            }
            hit_cost.push(allocs);
            // the lane's own share of a cached frame, with the render left
            // out: read the surfaces off the memories and match a key.
            // Nothing else on this path is ours, so this is the number that
            // has to be zero.
            let (_, lookup) = measure(|| {
                let mut sources = [crate::desktop_wgpu_iosurface::PlaneSource::EMPTY;
                    crate::desktop_wgpu_iosurface::MAX_PLANES];
                let n =
                    crate::desktop_wgpu_iosurface::plane_sources(buffer, &plan.info, &mut sources)
                        .expect("the surfaces must read back");
                gpu.imports
                    .get_or_import(&gpu.device, &plan.desc, &sources[..n])
                    .expect("the entry is already there")
            });
            lookup_cost.push(lookup);
        }) else {
            eprintln!("no VideoToolbox elements, skipping");
            return;
        };
        assert!(seen > 16, "too few frames to see a pool cycle, got {seen}");

        // counted off the presents, not off the cache, since the lookups
        // measured above hit it a second time
        let (hits, misses) = (hit_cost.len() as u64, miss_cost.len() as u64);
        eprintln!(
            "{seen} frames, {hits} cached imports, {misses} built, \
             pool {}; allocations on a miss {miss_cost:?}, on a hit {hit_cost:?}, \
             on the lookup alone {lookup_cost:?}",
            gpu.imports.len()
        );
        assert_eq!(hits + misses, gpu.iosurface_frames);
        assert_eq!(gpu.sysmem_frames, 0, "a frame took the cpu upload path");
        // the pool is bounded, so the imports stop after its first cycle
        assert!(
            misses <= crate::desktop_wgpu_iosurface::MAX_IMPORTS as u64,
            "{misses} imports for one stream, the pool is not being reused"
        );
        assert_eq!(gpu.imports.len() as u64, misses);
        assert_eq!(
            gpu.imports.evictions, 0,
            "the cache is smaller than the pool"
        );
        assert!(hits >= seen as u64 - misses);
        // the lane's own share of a steady-state frame
        assert!(
            lookup_cost.iter().all(|c| *c == 0),
            "finding a cached import allocated: {lookup_cost:?}"
        );
        let worst_hit = *hit_cost.iter().max().unwrap();
        let best_miss = *miss_cost.iter().min().unwrap();
        assert!(
            worst_hit <= FRAME_ALLOC_BUDGET,
            "a cached import frame allocates {worst_hit}, over the {FRAME_ALLOC_BUDGET} wgpu leaves"
        );
        assert!(
            worst_hit < best_miss,
            "the cache saves nothing: hit {worst_hit}, miss {best_miss}"
        );
    }

    /// What the import will describe to the renderer. The wide layout rides
    /// the device feature, since its planes are 16-bit norms.
    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_biplanar_videotoolbox_layouts_import() {
        use crate::desktop_wgpu_iosurface::importable;
        assert!(importable(PixelFormat::Nv12, false));
        assert!(importable(PixelFormat::P010, true));
        assert!(!importable(PixelFormat::P010, false));
        // decoded by software, never by VideoToolbox into a surface the lane
        // knows how to describe
        assert!(!importable(PixelFormat::I420, true));
        assert!(!importable(PixelFormat::Bgra, true));
    }

    /// Renders a presented frame at its own size and reads it back as rgba8.
    #[cfg(target_os = "linux")]
    fn read_presented(gpu: &Gpu, payload: FramePayload) -> Vec<u8> {
        let tex = as_texture(gpu, &payload);
        let (w, h) = (tex.width(), tex.height());
        i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, &tex, w, h)
            .expect("readback must succeed")
    }

    /// The cache's first invalidation rule, on a live decoder: entries
    /// describe one desc and one modifier, so a stream that changes size must
    /// drop every one of them rather than match a key against planes of the
    /// wrong geometry.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_resolution_change_drops_every_import() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let offer = gpu.caps();
        let run = |gpu: &mut Gpu, w: u32, h: u32| -> Option<usize> {
            decode_each(&offer, w, h, 24, |sample| {
                let caps = sample.caps_owned().unwrap();
                let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
                let Some(modifier) = plan.modifier else {
                    return;
                };
                gpu.present_imported(
                    sample.buffer_owned().unwrap(),
                    &plan,
                    iv::BufferTransform::Normal,
                    modifier,
                )
                .expect("the import must succeed");
            })
        };
        let Some(_) = run(&mut gpu, 640, 480) else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        let first_pool = gpu.imports.len();
        assert!(first_pool > 0);
        let before = gpu.imports.misses;

        run(&mut gpu, 320, 240).expect("the elements were there a moment ago");
        // nothing from the first size survived, so every buffer of the second
        // stream had to be imported again
        assert!(
            gpu.imports.len() <= gpu.imports.misses as usize - before as usize,
            "the cache kept entries across a resolution change"
        );
        assert_eq!(gpu.sysmem_frames, 0);
        assert_eq!(gpu.imports.evictions, 0);
    }

    /// The correctness the import cache rests on: a cached `VkImage` reads
    /// the decoder's memory, and the decoder wrote a new picture into it
    /// since the last hit, so a hit must show the new frame.
    ///
    /// Every frame carries a different flat luma, so a stale entry would come
    /// back as the picture from a whole pool cycle earlier, tens of codes
    /// away from what is expected here.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cached_import_shows_the_frame_the_decoder_just_wrote() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let (width, height) = (320u32, 240u32);
        let frames = 36u64;
        let luma = |i: u64| 40 + (i as u8) * 5;
        let offer = gpu.caps();

        // a fresh flat frame per push, so each decoded frame is its own value
        let src_caps = gst::Caps::from_str(&format!(
            "video/x-raw, format=(string)NV12, width=(int){width}, height=(int){height}, \
             framerate=(fraction)30/1, colorimetry=(string)bt709"
        ))
        .unwrap();
        let src = gst_app::AppSrc::builder()
            .caps(&src_caps)
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let make = |name: &str| gst::ElementFactory::make(name).build().ok();
        let (Some(enc), Some(parse), Some(dec)) =
            (make("vah264enc"), make("h264parse"), make("vah264dec"))
        else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        // all-intra, so a decoded frame is exactly the frame that was pushed
        enc.set_property("key-int-max", 1u32);
        let sink = gst_app::AppSink::builder()
            .caps(&offer)
            .max_buffers(16)
            .sync(false)
            .build();
        offer_video_meta(&sink.static_pad("sink").unwrap());
        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &enc, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &enc, &parse, &dec, &sink_element]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        for i in 0..frames {
            let (y, uv) = nv12_planes(width, height, luma(i));
            let mut bytes = y;
            bytes.extend_from_slice(&uv);
            let mut buffer = gst::Buffer::from_slice(bytes);
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 33));
            src.push_buffer(buffer).unwrap();
        }
        let _ = src.end_of_stream();

        let mut seen = 0u64;
        while let Ok(sample) = sink.pull_sample() {
            let caps = sample.caps_owned().unwrap();
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            let Some(modifier) = plan.modifier else {
                eprintln!("not a dmabuf stream, skipping");
                let _ = pipeline.set_state(gst::State::Null);
                return;
            };
            let presented = gpu
                .present_imported(
                    sample.buffer_owned().unwrap(),
                    &plan,
                    iv::BufferTransform::Normal,
                    modifier,
                )
                .expect("the import must succeed");
            let tex = as_texture(&gpu, &presented);
            let got = center_red(&gpu, &tex);
            // limited range luma to full range rgb, neutral chroma
            let want = ((luma(seen) as f32 - 16.0) * 255.0 / 219.0).round() as i32;
            assert!(
                (got as i32 - want).abs() <= 12,
                "frame {seen} came back at {got}, expected about {want}; \
                 a cached import is showing a stale picture"
            );
            seen += 1;
        }
        let _ = pipeline.set_state(gst::State::Null);
        assert_eq!(seen, frames, "not every pushed frame came back");
        // and the run really did go round the pool more than once
        assert!(
            gpu.imports.hits > 0,
            "no frame took the cached path, the check proved nothing"
        );
    }

    /// Decodes through the shape flapjack builds (a `queue` between the
    /// decoder and the sink) with the decoder's allocation query swallowed at
    /// the queue's sink pad. Returns the frames that came out and whatever the
    /// bus said.
    ///
    /// A probe that drops a query makes `gst_pad_query` return FALSE, which is
    /// bit for bit what `gst_queue_handle_sink_query` does for a serialized
    /// query whenever its source task is not running.
    #[cfg(target_os = "linux")]
    fn decode_with_a_lost_allocation_query(guaranteed: bool) -> Option<(usize, Vec<String>)> {
        init_gst();
        let (width, height) = (320u32, 240u32);
        let make = |name: &str| gst::ElementFactory::make(name).build().ok();
        let (Some(enc), Some(parse), Some(dec), Some(queue)) = (
            make("vah264enc"),
            make("h264parse"),
            make("vah264dec"),
            make("queue"),
        ) else {
            eprintln!("no VA-API elements, skipping");
            return None;
        };
        let gpu = texture_gpu()?;
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return None;
        }

        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::from_str(&format!(
                    "video/x-raw, format=(string)NV12, width=(int){width}, \
                     height=(int){height}, framerate=(fraction)30/1, \
                     colorimetry=(string)bt709"
                ))
                .unwrap(),
            )
            .format(gst::Format::Time)
            .is_live(false)
            .build();
        let sink = gst_app::AppSink::builder()
            .caps(&gpu.caps())
            .max_buffers(16)
            .sync(false)
            .build();
        // the production arming, on the pad the query is MEANT to reach
        offer_video_meta(&sink.static_pad("sink").unwrap());
        // and the production guarantee, on the pad the decoder ASKS on
        if guaranteed {
            super::guarantee_video_meta(&dec);
        }
        // the window: the query never gets past the queue
        queue.static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::QUERY_DOWNSTREAM,
            |_, info| {
                if let Some(gst::PadProbeData::Query(query)) = &info.data
                    && matches!(query.view(), gst::QueryView::Allocation(_))
                {
                    return gst::PadProbeReturn::Drop;
                }
                gst::PadProbeReturn::Ok
            },
        );

        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &enc, &parse, &dec, &queue, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &enc, &parse, &dec, &queue, &sink_element]).unwrap();
        pipeline.set_state(gst::State::Playing).ok()?;

        let pixels = nv12_bars(width, height);
        for i in 0..16u64 {
            let mut buffer = gst::Buffer::from_slice(pixels.clone());
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 33));
            if src.push_buffer(buffer).is_err() {
                break;
            }
        }
        let _ = src.end_of_stream();

        let mut seen = 0;
        while sink.pull_sample().is_ok() {
            seen += 1;
        }
        let mut errors = Vec::new();
        for msg in pipeline.bus().unwrap().iter() {
            if let gst::MessageView::Error(e) = msg.view() {
                errors.push(format!("{} / {:?}", e.error(), e.debug()));
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        Some((seen, errors))
    }

    /// What a stream boundary has to drop.
    ///
    /// The import cache is keyed on the dma_buf a decoder handed out, and it is
    /// only invalidated by a caps CHANGE. Two items at the same coded size
    /// negotiate caps that compare equal, so nothing on the caps path fires,
    /// and the lane would hold the previous decoder's surfaces open (an
    /// imported `VkImage`, its device memory and a dup'd fd per plane) for as
    /// long as the size holds still. Across a session of same-size items that
    /// is every decoder that ever ran.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stream_boundary_drops_the_previous_decoders_frames() {
        init_gst();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let (width, height) = (320u32, 240u32);
        let offer = gpu.caps();
        let mut imported = false;
        let Some(_) = decode_each(&offer, width, height, 12, |sample| {
            let Some(caps) = sample.caps_owned() else {
                return;
            };
            let plan = gpu.parse_caps(caps).expect("the decoder caps must map");
            let Some(modifier) = plan.modifier else {
                return;
            };
            let buffer = sample.buffer_owned().unwrap();
            gpu.present_imported(buffer, &plan, iv::BufferTransform::Normal, modifier)
                .expect("the import must succeed");
            imported = true;
        }) else {
            return;
        };
        if !imported {
            eprintln!("the decoder never handed out a dmabuf, skipping");
            return;
        }
        assert!(gpu.imports.len() > 0, "nothing was cached to drop");

        // the same caps object a second item at the same size would carry, so
        // the caps path stays quiet and the boundary is the only thing that can
        // let the old decoder's surfaces go
        let again = gpu.caps.as_ref().map(|(c, _)| c.clone()).unwrap();
        gpu.parse_caps(again);
        assert!(
            gpu.imports.len() > 0,
            "equal caps must not invalidate on their own, or this proves nothing"
        );

        gpu.release_held();
        assert_eq!(gpu.imports.len(), 0, "the imports outlived the stream");
        assert_eq!(
            gpu.pool.free.lock().len(),
            0,
            "a returned slot outlived the stream, holding its imports open"
        );
    }

    /// The field failure, and the guarantee that closes it.
    ///
    /// A VA decoder on `DMA_DRM` caps refuses its own output the moment its
    /// allocation query comes back without `GstVideoMeta`, and
    /// `gst_video_decoder_negotiate_pool` hands it the untouched query when the
    /// peer query FAILED rather than answered. So a query lost anywhere between
    /// the decoder and this sink reads, to the decoder, exactly like a sink
    /// that refused the contract: `NOT_NEGOTIATED` out of the decoder and
    /// "streaming stopped, reason not-negotiated (-4)" posted by the source.
    ///
    /// Both halves are graded here, so the test says what the guarantee is FOR
    /// rather than only that it is present.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lost_allocation_query_no_longer_takes_the_pipeline_down() {
        let Some((unguarded, errors)) = decode_with_a_lost_allocation_query(false) else {
            return;
        };
        eprintln!("without the guarantee: {unguarded} frames, errors {errors:?}");
        assert_eq!(
            unguarded, 0,
            "the losable query stopped being fatal on its own; if gst now \
             tolerates it, this test and the guarantee it grades can go"
        );
        assert!(
            errors.iter().any(|e| e.contains("not-negotiated")),
            "expected the field's own error text, got {errors:?}"
        );

        let (guarded, errors) = decode_with_a_lost_allocation_query(true).unwrap();
        eprintln!("with the guarantee: {guarded} frames, errors {errors:?}");
        assert!(
            errors.is_empty(),
            "the guarded run still errored: {errors:?}"
        );
        assert!(
            guarded > 0,
            "the guarded run decoded nothing, so the query was still lost"
        );
    }

    /// The fallback, on a live VA-API pipeline rather than on the caps alone.
    ///
    /// Two things are graded. The narrowing and the reconfigure the sink
    /// pushes must not disturb a running decoder: the stream keeps flowing and
    /// the lane keeps rendering, which is what makes a refused import cost
    /// frames rather than the pipeline. And the decoder is measured, not
    /// assumed: `gst_va_base_dec_negotiate` returns early unless its input
    /// state changed, so a VA decoder keeps pushing dmabufs through the ask.
    /// That measurement is the reason the lane retries the import every frame
    /// instead of latching itself onto a route the decoder will never take.
    #[cfg(target_os = "linux")]
    #[test]
    fn narrowing_the_offer_under_a_running_decoder_keeps_the_stream_alive() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let (width, height) = (320u32, 240u32);
        let make = |name: &str| gst::ElementFactory::make(name).build().ok();
        let (Some(enc), Some(parse), Some(dec)) =
            (make("vah264enc"), make("h264parse"), make("vah264dec"))
        else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::from_str(&format!(
                    "video/x-raw, format=(string)NV12, width=(int){width}, \
                     height=(int){height}, framerate=(fraction)30/1"
                ))
                .unwrap(),
            )
            .format(gst::Format::Time)
            .is_live(false)
            .max_bytes(0)
            .build();
        // shallow and non-dropping, so the decoder is still mid-stream and
        // blocked on this sink when the caps are narrowed under it
        let sink = gst_app::AppSink::builder()
            .caps(&gpu.caps())
            .max_buffers(2)
            .drop(false)
            .sync(false)
            .build();
        offer_video_meta(&sink.static_pad("sink").unwrap());

        let pipeline = gst::Pipeline::new();
        let src_element = src.clone().upcast::<gst::Element>();
        let sink_element = sink.clone().upcast::<gst::Element>();
        pipeline
            .add_many([&src_element, &enc, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src_element, &enc, &parse, &dec, &sink_element]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let pixels = nv12_bars(width, height);
        for i in 0..60u64 {
            let mut buffer = gst::Buffer::from_slice(pixels.clone());
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 33));
            src.push_buffer(buffer).unwrap();
        }
        let _ = src.end_of_stream();

        let first = sink.pull_sample().expect("a first frame must arrive");
        let first_caps = first.caps_owned().unwrap();
        assert!(
            gst_video::is_dma_drm_caps(&first_caps),
            "the decoder did not start on dmabufs, got {first_caps}"
        );

        // exactly what the sink does on a refused import, minus the failure
        assert!(gpu.note_import_failure("test"), "the first one asks");
        assert!(!gpu.note_import_failure("test again"), "the rest do not");
        assert!(gpu.can_import(), "the device can still import");
        assert!(!gpu.offer_dmabuf, "but it no longer advertises dmabufs");
        sink.set_caps(Some(&gpu.caps()));
        sink.static_pad("sink")
            .unwrap()
            .push_event(gst::event::Reconfigure::new());

        let mut seen = 0;
        let mut still_dmabuf = 0;
        let mut last = None;
        while let Ok(sample) = sink.pull_sample() {
            seen += 1;
            let caps = sample.caps_owned().unwrap();
            if gst_video::is_dma_drm_caps(&caps) {
                still_dmabuf += 1;
            }
            last = Some((sample, caps));
        }
        let _ = pipeline.set_state(gst::State::Null);

        // the stream survived the ask, which is the part that matters
        assert!(seen > 0, "the pipeline stopped delivering after the ask");
        eprintln!("after the ask: {seen} frames, {still_dmabuf} still dmabuf");
        assert_eq!(
            still_dmabuf, seen,
            "this decoder was expected to ignore the reconfigure; if it now \
             honours it the lane can latch itself off instead of retrying"
        );

        // and every one of them still renders, since the import kept working
        let (sample, caps) = last.unwrap();
        let plan = gpu.parse_caps(caps).expect("the caps must still map");
        let modifier = plan.modifier.expect("still a dmabuf");
        let presented = gpu
            .present_imported(
                sample.buffer_owned().unwrap(),
                &plan,
                iv::BufferTransform::Normal,
                modifier,
            )
            .expect("the import must still work after the ask");
        let tex = as_texture(&gpu, &presented);
        let pixels =
            i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, &tex, width, height)
                .unwrap();
        assert_eq!(gpu.sysmem_frames, 0, "a frame took the cpu upload path");
        for row in bar_reds(&pixels, width, height) {
            for pair in row.windows(2) {
                assert!(pair[1] > pair[0], "the frame is scrambled, {row:?}");
            }
        }
    }

    /// 4K, the size the whole change exists for. The decoder pads a 3840 wide
    /// luma plane to its own pitch and puts the chroma plane at that pitch
    /// times the aligned height inside the same fd, so a layout computed from
    /// the caps instead of read off the `VideoMeta` lands in the wrong place.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_4k_dmabuf_frame_imports_at_the_decoders_own_pitch() {
        gst::init().unwrap();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        let offer = gpu.caps();
        let Some((caps, w, h, pixels, _)) = decode_through(&mut gpu, &offer, 3840, 2160) else {
            eprintln!("no VA-API elements, skipping");
            return;
        };
        assert!(
            gst_video::is_dma_drm_caps(&caps),
            "not a dmabuf, got {caps}"
        );
        assert_eq!((w, h), (3840, 2160));
        assert_eq!(gpu.dmabuf_frames, 1);
        assert_eq!(gpu.sysmem_frames, 0, "a frame took the cpu upload path");
        for row in bar_reds(&pixels, w, h) {
            for pair in row.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "the luma ramp is not monotonic across the bars, got {row:?}"
                );
            }
        }
    }

    /// A caps event carrying a raw format the lane has no render path for must
    /// still be ACCEPTED.
    ///
    /// `gst_base_sink` answers `ACCEPT_CAPS` with a strict subset test against
    /// the appsink's `caps` property, and a refused caps event fails
    /// `pre_eventfunc_check` with `GST_FLOW_NOT_NEGOTIATED`, which travels up
    /// to the source and kills the whole item, audio and control included.
    /// Seen in the field on ordinary 4:2:2 content: "caps video/x-raw,
    /// format=(string)Y42B ... not accepted" on a pad inside decodebin3,
    /// immediately followed by "streaming stopped, reason not-negotiated".
    #[test]
    fn caps_the_lane_cannot_render_are_accepted_not_refused() {
        gst::init().unwrap();
        let engine = fcast_video::cue::CueEngine::new();
        let Some((sink, _tick)) = test_sink(engine) else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let pad = sink.static_pad("sink").expect("the appsink has a sink pad");
        let offer = sink
            .property::<Option<gst::Caps>>("caps")
            .expect("the sink was built with an offer");
        // Formats no version of the offer carries: grayscale, packed 4:2:2
        // wire formats, high-depth BE, bayer.
        for form in ["GRAY8", "GRAY16_LE", "UYVY", "YUY2", "v210", "I420_10BE"] {
            let caps = gst::Caps::from_str(&format!(
                "video/x-raw, format=(string){form}, width=(int)640, \
                 height=(int)360, framerate=(fraction)25/1"
            ))
            .unwrap();
            assert!(
                !offer.can_intersect(&caps),
                "{form} is in the offer, pick a format that is not"
            );
            assert!(
                pad.query_accept_caps(&caps),
                "{form} was refused, which is what kills the pipeline"
            );
        }
        // The offer is a PREFERENCE and stays narrow: anything that negotiates
        // properly still settles on a format the lane draws.
        assert_eq!(
            pad.query_caps(None),
            offer,
            "the caps query stopped advertising the offer"
        );
    }

    /// The same refusal, through the element chain the field failure ran
    /// through: the query starts at decodebin3's output pad and is proxied by
    /// streamsynchronizer and by the queue in front of the sink, so the
    /// sink's answer is the only one in it.
    #[test]
    fn the_chain_in_front_of_the_sink_accepts_what_the_sink_cannot_render() {
        gst::init().unwrap();
        let engine = fcast_video::cue::CueEngine::new();
        let Some((sink, _tick)) = test_sink(engine) else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let Ok(ssync) = gst::ElementFactory::make("streamsynchronizer").build() else {
            eprintln!("no streamsynchronizer, skipping");
            return;
        };
        let vqueue = gst::ElementFactory::make("queue").build().unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&ssync, &vqueue, &sink]).unwrap();
        vqueue.link(&sink).unwrap();
        let ss_sink = ssync.request_pad_simple("sink_%u").unwrap();
        let ss_src = ssync
            .static_pad(&ss_sink.name().replace("sink_", "src_"))
            .unwrap();
        ss_src.link(&vqueue.static_pad("sink").unwrap()).unwrap();
        let caps = gst::Caps::from_str(
            "video/x-raw, format=(string)Y42B, width=(int)640, height=(int)360, \
             interlace-mode=(string)progressive, pixel-aspect-ratio=(fraction)1/1, \
             chroma-site=(string)mpeg2, framerate=(fraction)49/2",
        )
        .unwrap();
        // The chain is in the pipeline but not activated, which is the state a
        // chain join leaves it in while the caps event is already travelling.
        assert!(
            ss_sink.query_accept_caps(&caps),
            "refused before activation"
        );
        pipeline.set_state(gst::State::Paused).unwrap();
        assert!(ss_sink.query_accept_caps(&caps), "refused while activated");
        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// A stream whose caps the lane cannot import must not be mapped and must
    /// not black the view out: the sink narrows the appsink and asks upstream
    /// for system memory instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_refused_import_narrows_the_offer_to_system_memory() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.can_import() {
            eprintln!("no importable drm modifier, skipping");
            return;
        }
        gst::init().unwrap();
        let before = gpu.caps();
        assert!(
            before.iter().any(|s| s.name() == "video/x-raw"
                && s.get::<&str>("format").is_ok_and(|f| f == "DMA_DRM")),
            "no DMA_DRM structure in {before}"
        );

        assert!(gpu.note_import_failure("test"));
        let after = gpu.caps();
        assert!(
            !after
                .iter()
                .any(|s| s.get::<&str>("format").is_ok_and(|f| f == "DMA_DRM")),
            "the narrowed offer still carries a dmabuf structure: {after}"
        );
        // and it is still a usable system-memory offer, not an empty one
        assert!(!after.is_empty());
        assert_eq!(after, gpu.sysmem_caps());
        // the import itself stays available, since a decoder that ignores the
        // ask would otherwise be left with no route at all
        assert!(gpu.can_import());
        // a second refusal asks for nothing more, so the log line prints once
        assert!(!gpu.note_import_failure("test again"));
        assert_eq!(gpu.import_fails, 2);
        assert_eq!(gpu.pool.free.lock().len(), 0, "returned slots released");

        // The narrowing is for the stream it happened on. A new stream is a new
        // decoder and a new pool, so the offer goes back whole; without this the
        // session never saw a dmabuf again.
        let rearmed = gpu.rearm_dmabuf().expect("a narrowed offer re-arms");
        assert_eq!(rearmed, before, "the re-armed offer is the original one");
        assert!(gpu.offer_dmabuf);
        assert_eq!(gpu.import_fails, 0, "the one-shot log arms again too");
        // and a stream that never narrowed publishes nothing, so a steady
        // multi-item session does not set the appsink's caps once per item
        assert!(gpu.rearm_dmabuf().is_none());
    }

    /// A device with no import route must not be talked into offering one by
    /// the re-arm, or the sink would advertise dmabufs it cannot take.
    #[test]
    fn a_device_without_an_import_route_never_re_arms_into_one() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        gst::init().unwrap();
        gpu.dmabuf.clear();
        gpu.offer_dmabuf = false;
        assert!(gpu.rearm_dmabuf().is_none());
        assert!(!gpu.offer_dmabuf);
    }

    /// EVERY FORMAT THE SINK OFFERS MUST FIT THE ARRAYS THAT UPLOAD IT.
    ///
    /// The sysmem arm writes plane pointers and strides into fixed stack
    /// arrays sized by [`UPLOAD_MAX_PLANES`]. They were three long while
    /// `sysmem_formats` offered `A420` and `GBRA`, which are three color planes
    /// plus alpha, so the first frame of either one indexed past the end and
    /// panicked on the streaming thread inside a C callback. The dmabuf arm
    /// refuses four planes with an error, so those formats always land here.
    ///
    /// Stated over the offered set rather than over the two known formats, so a
    /// format added to the caps later cannot reintroduce it.
    #[test]
    fn every_offered_format_fits_the_upload_plane_arrays() {
        gst::init().unwrap();
        for norm16 in [false, true] {
            for format in sysmem_formats(norm16) {
                let Some(mapped) = map_format(format) else {
                    panic!("{format:?} is offered but does not map to a pixel format");
                };
                let planes = mapped.plane_count();
                assert!(
                    planes <= UPLOAD_MAX_PLANES,
                    "{format:?} has {planes} planes, past the {UPLOAD_MAX_PLANES} the \
                     sysmem upload arrays carry"
                );
            }
        }
    }

    /// The four-plane arm end to end, on a real device.
    ///
    /// The guard above is arithmetic; this one actually uploads and renders an
    /// alpha format, so a four-plane bind group and the alpha tap are proved
    /// alongside the array bound.
    #[test]
    fn an_alpha_format_uploads_and_renders_all_four_planes() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (w, h) = (64u32, 64u32);
        for name in ["A420", "GBRA"] {
            let caps = gst::Caps::from_str(&format!(
                "video/x-raw, format=(string){name}, width=(int){w}, height=(int){h}, \
                 colorimetry=(string)bt709"
            ))
            .unwrap();
            let desc = desc_from_caps(&caps)
                .unwrap_or_else(|| panic!("{name} must map"))
                .desc;
            assert_eq!(desc.format.plane_count(), 4, "{name} must be four planes");
            // A420 is Y, U, V, A at 4:2:0; GBRA is G, B, R, A at full rate.
            let sub = if name == "A420" { 2 } else { 1 };
            let full = vec![160u8; (w * h) as usize];
            let chroma = vec![128u8; ((w / sub) * (h / sub)) as usize];
            let planes: [&[u8]; 4] = [&full, &chroma, &chroma, &full];
            let strides = [w, w / sub, w / sub, w];
            present_plain(&mut gpu, desc, &planes, &strides)
                .unwrap_or_else(|e| panic!("{name} must render, got {e}"));
        }
    }

    /// The bt709 SDR desc the vast majority of streams negotiate.
    fn nv12_desc(width: u32, height: u32) -> FrameDesc {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(&format!(
            "video/x-raw, format=(string)NV12, width=(int){width}, height=(int){height}, \
             colorimetry=(string)bt709"
        ))
        .unwrap();
        desc_from_caps(&caps).expect("plain nv12 must map").desc
    }

    /// The upright, square-pixel, undithered present every case below uses
    /// unless it is testing one of those three.
    fn present_plain(
        gpu: &mut Gpu,
        desc: FrameDesc,
        planes: &[&[u8]],
        strides: &[u32],
    ) -> Result<FramePayload, i_slint_video_wgpu::VideoError> {
        gpu.present(desc, (1, 1), iv::BufferTransform::Normal, planes, strides)
    }

    /// Flat luma, neutral chroma. Strides are the plane widths, which is what
    /// a packed decoder buffer hands out.
    fn nv12_planes(width: u32, height: u32, luma: u8) -> (Vec<u8>, Vec<u8>) {
        (
            vec![luma; (width * height) as usize],
            vec![128u8; (width * height / 2) as usize],
        )
    }

    fn present_flat(gpu: &mut Gpu, width: u32, height: u32, luma: u8) -> FramePayload {
        let (y, uv) = nv12_planes(width, height, luma);
        present_plain(gpu, nv12_desc(width, height), &[&y, &uv], &[width, width])
            .expect("a flat nv12 frame must render")
    }

    /// What one frame is allowed to allocate once the lane has settled.
    ///
    /// The lane's own steady state is zero: plane textures, render targets,
    /// uniforms, bind groups, imports and the caps plan are all pooled or
    /// refcounted. What is left is wgpu's own per-frame work, measured at
    /// about 20 allocations for a bare command encoder and submit and about
    /// 60 for one recorded pass plus two plane writes, none of it reachable
    /// from here. The budget is that with headroom, so it catches a texture
    /// or an import creeping back onto the frame path (hundreds) rather than
    /// a single Vec, which the sharp tests below are for.
    const FRAME_ALLOC_BUDGET: u64 = 100;

    /// The upload arm at rest: same caps, same size, frame after frame.
    #[test]
    fn a_steady_state_upload_frame_allocates_nothing_of_ours() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (w, h) = (640u32, 480u32);
        let desc = nv12_desc(w, h);
        let (y, uv) = nv12_planes(w, h, 128);
        let planes: [&[u8]; 2] = [&y, &uv];
        let strides = [w, w];
        // the first frames build the plane textures, the target, the pipeline
        // and the bind group, which is what the steady state then reuses
        for _ in 0..8 {
            present_plain(&mut gpu, desc, &planes, &strides).expect("must render");
        }
        let mut counts = [0u64; 4];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let presented = present_plain(&mut gpu, desc, &planes, &strides);
                drop(presented.expect("must render"));
            });
            *c = n;
        }
        // the floor: what wgpu costs for a command encoder and a submit with
        // nothing recorded into it, which the frames above cannot go under
        let (_, floor) = measure(|| {
            let e = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            gpu.queue.submit(Some(e.finish()));
        });
        eprintln!("upload arm allocations per frame: {counts:?}, bare wgpu submit: {floor}");
        // The bind group check that was here belonged to the render this lane
        // no longer does. Its subject moved with it: the renderer keys its
        // registered source on the plane handles, so what has to hold now is
        // that the slot pool hands the same plane textures back rather than
        // allocating a set per frame. One slot in and out for the whole run,
        // and it kept its upload frame.
        {
            let free = gpu.pool.free.lock();
            assert_eq!(free.len(), 1, "the steady state cycles one slot");
            assert!(
                free[0]
                    .upload
                    .as_ref()
                    .is_some_and(|f| f.planes().len() == 2),
                "the slot's plane textures did not survive the recycle"
            );
        }
        assert!(
            counts.iter().all(|c| *c <= FRAME_ALLOC_BUDGET),
            "an upload frame allocates {counts:?}, over the {FRAME_ALLOC_BUDGET} wgpu leaves"
        );
    }

    /// THE WAVE 6 GATE: the budget does not move because subtitles arrived.
    ///
    /// The same steady-state frame as above, with a real cue on the overlay
    /// slot and the lane's whole cue path (schedule advance, overlay compare)
    /// inside the measurement. The plan's claim is that a scene overlay costs
    /// the frame nothing, and the strictest allocation guard in the tree is
    /// where it gets checked.
    #[test]
    fn a_frame_with_a_cue_on_screen_stays_inside_the_budget() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (w, h) = (640u32, 480u32);
        let desc = nv12_desc(w, h);
        let (y, uv) = nv12_planes(w, h, 128);
        let planes: [&[u8]; 2] = [&y, &uv];
        let strides = [w, w];

        let (engine, cue) = crate::cue_overlay::tests::one("a subtitle over the picture");
        assert!(cue.scene.glyph_count() > 0, "the cue must have glyphs");
        // A QUEUE BEHIND THE CUE, which is what a real subtitle track looks
        // like for its whole duration. With nothing pending the engine's
        // boundary warm bails before it does any per frame work, so a gate
        // measuring one lone cue measures the one shape the field never has.
        queue_cues_behind(&engine, 64);
        let recorder = crate::cue_overlay::tests::Recorder::default();
        let mut overlay = crate::cue_overlay::CueOverlay::default();
        let at = gst::ClockTime::from_seconds(crate::cue_overlay::tests::AT);

        settle_cues(&engine, at);
        for _ in 0..8 {
            present_plain(&mut gpu, desc, &planes, &strides).expect("must render");
            let shown = engine.scenes_for(Some(at));
            overlay.sync(&recorder, &shown);
        }
        let mut counts = [0u64; 4];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let presented = present_plain(&mut gpu, desc, &planes, &strides);
                let shown = engine.scenes_for(Some(at));
                overlay.sync(&recorder, &shown);
                drop(presented.expect("must render"));
            });
            *c = n;
        }
        eprintln!("allocations per frame with a cue up: {counts:?}");
        assert_eq!(
            overlay.builds(),
            1,
            "the display list was rebuilt while the cue stood still"
        );
        assert!(
            counts.iter().all(|c| *c <= FRAME_ALLOC_BUDGET),
            "a frame with a cue allocates {counts:?}, over the {FRAME_ALLOC_BUDGET} wgpu leaves"
        );
    }

    /// Cues queued behind whatever is showing, far enough out that none of them
    /// ever becomes due during a measurement.
    ///
    /// Distinct text on purpose: identical lines would collapse onto one scene
    /// key and the warm would have nothing to do.
    fn queue_cues_behind(engine: &fcast_video::cue::CueEngine, count: usize) {
        for i in 0..count {
            let at = 600 + i as u64 * 10;
            engine.submit(fcast_video::cue::CueInput {
                format: fcast_video::cue::TextFormat::Utf8,
                text: format!("queued line number {i}"),
                start_rt: gst::ClockTime::from_seconds(at),
                end_rt: Some(gst::ClockTime::from_seconds(at + 5)),
            });
        }
    }

    /// Run the schedule until a frame costs nothing, so the boundary warm's own
    /// build has finished and the measurement grades the steady state rather
    /// than the engine still asking for the next cue.
    fn settle_cues(engine: &fcast_video::cue::CueEngine, at: gst::ClockTime) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            engine.scenes_for(Some(at));
            std::thread::sleep(std::time::Duration::from_millis(10));
            if measure(|| engine.scenes_for(Some(at))).1 == 0 {
                return;
            }
        }
        panic!("the cue schedule never reached a steady state");
    }

    /// A display set on screen, through the REAL PGS decoder, must cost the
    /// frame path no more than a frame with nothing on it.
    ///
    /// The bitmap twin of the gate above, and the sharper half of the claim
    /// lives in `bitmap_overlay`'s own gate, which asserts an exact zero. This
    /// one is here because it is the whole frame: present, schedule, read,
    /// compare, place.
    #[test]
    fn a_frame_with_a_bitmap_subtitle_stays_inside_the_budget() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        gst::init().unwrap();
        let (w, h) = (640u32, 480u32);
        let desc = nv12_desc(w, h);
        let (y, uv) = nv12_planes(w, h, 128);
        let planes: [&[u8]; 2] = [&y, &uv];
        let strides = [w, w];

        let engine = fcast_video::cue::CueEngine::new();
        engine.set_scene_consumer(true);
        engine.set_video_size(1920, 1080);
        // Real PGS bytes through the production decoder, not a stand-in set.
        engine.submit_bitmap(fcast_video::subpic::BitmapPacket {
            format: fcast_video::subpic::BitmapFormat::Pgs,
            data: gst::Buffer::from_slice(simulator::pgs::display_set(1)),
            codec_data: None,
            rt: gst::ClockTime::ZERO,
            duration: None,
        });
        let at = gst::ClockTime::from_seconds(1);
        let recorder = crate::bitmap_overlay::tests::Recorder::default();
        let mut overlay = crate::bitmap_overlay::BitmapOverlay::default();
        let rect = crate::video_math::video_rect((1920, 1080), (1280, 720));

        let mut settled = false;
        for _ in 0..200 {
            engine.scenes_for(Some(at));
            let changed = engine.with_shown_bitmaps(|regions| overlay.latch(regions));
            if changed {
                overlay.composite();
            }
            if overlay.place(&recorder, changed, rect, (1920, 1080), 1.0) {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(settled, "the display set never reached the overlay");

        for _ in 0..8 {
            present_plain(&mut gpu, desc, &planes, &strides).expect("must render");
            let changed = engine.with_shown_bitmaps(|regions| overlay.latch(regions));
            overlay.place(&recorder, changed, rect, (1920, 1080), 1.0);
        }
        let builds = overlay.builds();
        let mut counts = [0u64; 4];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let presented = present_plain(&mut gpu, desc, &planes, &strides);
                engine.scenes_for(Some(at));
                let changed = engine.with_shown_bitmaps(|regions| overlay.latch(regions));
                overlay.place(&recorder, changed, rect, (1920, 1080), 1.0);
                drop(presented.expect("must render"));
            });
            *c = n;
        }
        eprintln!("allocations per frame with a bitmap set up: {counts:?}");
        assert_eq!(
            overlay.builds(),
            builds,
            "the composite was rebuilt while the display set stood still"
        );
        assert!(
            counts.iter().all(|c| *c <= FRAME_ALLOC_BUDGET),
            "a frame with a bitmap set allocates {counts:?}, over the {FRAME_ALLOC_BUDGET} wgpu \
             leaves"
        );
    }

    /// THE WHOLE FRAME AT ONCE: video, a text cue and a bitmap set together.
    ///
    /// The two gates above each hold one thing over the picture. A source can
    /// carry a subpicture track and a text track at the same time, and that
    /// frame runs both overlay paths, both engine reads and the geometry sync
    /// on top of the present. Nothing measured that combination, so nothing
    /// would have caught a cost that only appears when both are up: a shown set
    /// spilling because the cue and the regions share it, or a rebuild the two
    /// compares trigger in each other.
    ///
    /// The cue also has a queue behind it, so this is the shape a real file
    /// with both tracks selected actually presents, frame after frame.
    #[test]
    fn a_frame_with_a_cue_and_a_bitmap_and_video_stays_inside_the_budget() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        gst::init().unwrap();
        let (w, h) = (640u32, 480u32);
        let desc = nv12_desc(w, h);
        let (y, uv) = nv12_planes(w, h, 128);
        let planes: [&[u8]; 2] = [&y, &uv];
        let strides = [w, w];

        // One engine feeding both lanes, which is how the sink drives it.
        let engine = fcast_video::cue::CueEngine::new();
        engine.set_scene_consumer(true);
        engine.set_video_size(1920, 1080);
        engine.set_canvas(1920, 1080);
        engine.submit(fcast_video::cue::CueInput {
            format: fcast_video::cue::TextFormat::Utf8,
            text: "a subtitle beside a subpicture".to_owned(),
            start_rt: gst::ClockTime::ZERO,
            end_rt: Some(gst::ClockTime::from_seconds(60)),
        });
        queue_cues_behind(&engine, 64);
        engine.submit_bitmap(fcast_video::subpic::BitmapPacket {
            format: fcast_video::subpic::BitmapFormat::Pgs,
            data: gst::Buffer::from_slice(simulator::pgs::display_set(1)),
            codec_data: None,
            rt: gst::ClockTime::ZERO,
            duration: None,
        });

        let at = gst::ClockTime::from_seconds(1);
        let cue_sink = crate::cue_overlay::tests::Recorder::default();
        let bitmap_sink = crate::bitmap_overlay::tests::Recorder::default();
        let mut cues = crate::cue_overlay::CueOverlay::default();
        let mut bitmaps = crate::bitmap_overlay::BitmapOverlay::default();
        let rect = crate::video_math::video_rect((1920, 1080), (1280, 720));

        // Both lanes have to genuinely be up, otherwise this measures the same
        // thing the single-overlay gates already do.
        let mut up = (false, false);
        for _ in 0..400 {
            let shown = engine.scenes_for(Some(at));
            up.0 |= cues.sync(&cue_sink, &shown) || !shown.is_empty();
            let changed = engine.with_shown_bitmaps(|regions| bitmaps.latch(regions));
            if changed {
                bitmaps.composite();
            }
            up.1 |= bitmaps.place(&bitmap_sink, changed, rect, (1920, 1080), 1.0);
            if up.0 && up.1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(up.0, "the text cue never reached the overlay");
        assert!(up.1, "the display set never reached the overlay");
        settle_cues(&engine, at);

        for _ in 0..8 {
            present_plain(&mut gpu, desc, &planes, &strides).expect("must render");
            let shown = engine.scenes_for(Some(at));
            cues.sync(&cue_sink, &shown);
            let changed = engine.with_shown_bitmaps(|regions| bitmaps.latch(regions));
            bitmaps.place(&bitmap_sink, changed, rect, (1920, 1080), 1.0);
        }
        let (cue_builds, bitmap_builds) = (cues.builds(), bitmaps.builds());

        let mut counts = [0u64; 4];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let presented = present_plain(&mut gpu, desc, &planes, &strides);
                let shown = engine.scenes_for(Some(at));
                cues.sync(&cue_sink, &shown);
                let changed = engine.with_shown_bitmaps(|regions| bitmaps.latch(regions));
                bitmaps.place(&bitmap_sink, changed, rect, (1920, 1080), 1.0);
                drop(presented.expect("must render"));
            });
            *c = n;
        }
        eprintln!("allocations per frame with a cue and a bitmap and video: {counts:?}");
        assert_eq!(cues.builds(), cue_builds, "the display list was rebuilt");
        assert_eq!(bitmaps.builds(), bitmap_builds, "the composite was rebuilt");
        assert!(
            counts.iter().all(|c| *c <= FRAME_ALLOC_BUDGET),
            "a frame with both overlays allocates {counts:?}, over the \
             {FRAME_ALLOC_BUDGET} wgpu leaves"
        );
    }

    /// A RESIZE WITH NO FRAME BEHIND IT still moves the cue.
    ///
    /// Everything this lane does with cues hangs off the appsink's UI closure,
    /// which is the only place the window and the picture are both known. While
    /// PAUSED no frame is coming, so a window drag left every cue laid out
    /// against the window it had before, until playback resumed. This is the
    /// notifier's hook minus the window size read.
    ///
    /// Three claims: the re-anchor reaches the engine, an unmoved window costs
    /// nothing, and the cue that is already up stays up while the replacement
    /// is built rather than blinking out.
    #[test]
    fn a_resize_with_no_frame_behind_it_re_anchors_the_cues() {
        gst::init().unwrap();
        // The picture the last frame carried, which is what a resize has to
        // re-letterbox against.
        const PICTURE: (u32, u32) = (1920, 1080);
        const AT: gst::ClockTime = gst::ClockTime::from_seconds(1);

        let cues = Cues {
            engine: fcast_video::cue::CueEngine::new(),
            geometry: crate::video_math::CueGeometry::new(),
            overlay: parking_lot::Mutex::new(Default::default()),
            bitmaps: parking_lot::Mutex::new(Default::default()),
            coded: AtomicU64::new(0),
            obstructed: AtomicBool::new(false),
        };
        cues.engine.set_scene_consumer(true);
        // Latched before the cue exists, so nothing below is racing a re-key
        // this setup started.
        cues.geometry.sync(&cues.engine, (1280, 720), PICTURE);
        cues.engine.submit(crate::cue_overlay::tests::cue(
            "a subtitle over the picture",
            0,
            10,
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let before = loop {
            let shown = cues.engine.scenes_for(Some(AT));
            if shown.len() == 1 {
                break shown;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the cue never made it on screen, so the resize below proves nothing"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert!(
            cues.re_anchor((1280, 1000)),
            "the resize never reached the engine"
        );
        assert!(
            !cues.re_anchor((1280, 1000)),
            "a window that did not move re-keyed every cue anyway"
        );

        // Never blank: either the old list is still up (into_stale) or the
        // worker has already landed the new one, which the loop below grades.
        // Which of the two it is races the worker, so it is not asserted.
        let during = cues.engine.current_scenes();
        assert_eq!(during.len(), 1, "the resize blanked the cue");

        loop {
            let now = cues.engine.current_scenes();
            assert_eq!(now.len(), 1, "the cue went away mid re-layout");
            if !Arc::ptr_eq(&now[0].scene, &before[0].scene) {
                assert!(
                    now[0].y > before[0].y,
                    "the cue was re-laid out against the old canvas, y {} then {}",
                    before[0].y,
                    now[0].y
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the resized layout never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The caps cache, which every frame goes through before anything else.
    /// A miss is a `VideoInfo` parse plus two metadata string parses; a hit
    /// has to be a pointer compare and a refcount, and nothing else.
    #[test]
    fn parsing_the_streams_caps_again_allocates_nothing() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        gst::init().unwrap();
        let caps = gst::Caps::from_str(
            "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt709",
        )
        .unwrap();
        gpu.parse_caps(caps.clone()).expect("must map");
        for _ in 0..4 {
            let (plan, n) = measure(|| gpu.parse_caps(caps.clone()));
            assert!(plan.is_some());
            assert_eq!(n, 0, "a repeat of the stream's caps allocated");
        }
        // and a different caps object is a real parse again
        let other = gst::Caps::from_str(
            "video/x-raw, format=(string)NV12, width=(int)1280, height=(int)720, \
             colorimetry=(string)bt709",
        )
        .unwrap();
        assert_eq!(gpu.parse_caps(other).unwrap().size, (1280, 720));
    }

    /// The picture a presented frame's planes convert to, at the frame's own
    /// coded size. What the lane used to hand back directly, now reconstructed
    /// through the crate the renderer will run.
    fn as_texture(gpu: &Gpu, payload: &FramePayload) -> wgpu::Texture {
        let display = display_size((payload.desc.width, payload.desc.height), payload.par);
        render_at_display(gpu, payload, display)
    }

    /// Center pixel's red channel, which is the whole signal for a neutral
    /// chroma frame.
    fn center_red(gpu: &Gpu, tex: &wgpu::Texture) -> u8 {
        let (w, h) = (tex.width(), tex.height());
        let rgba = i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, tex, w, h)
            .expect("readback must succeed");
        rgba[((h / 2 * w + w / 2) * 4) as usize]
    }

    /// The zero-copy contract, and the only thing that makes the lane work:
    /// the frame slint is handed describes plane textures it agrees to draw.
    /// Its checks are the plane count against the format's layout, and every
    /// plane being a bindable 2D texture, so assert those too rather than
    /// only the verdict.
    #[test]
    fn a_presented_frame_is_a_video_image_slint_accepts() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let payload = present_flat(&mut gpu, 64, 32, 128);
        assert_eq!(
            payload.slot.as_ref().unwrap().planes.len(),
            2,
            "nv12 is Y + CbCr"
        );
        for plane in &payload.slot.as_ref().unwrap().planes {
            assert_eq!(plane.dimension(), wgpu::TextureDimension::D2);
            assert!(plane.usage().contains(wgpu::TextureUsages::TEXTURE_BINDING));
        }
        let frame: Arc<dyn iv::VideoFrame> = Arc::new(SinkFrame::new(payload));
        let image = slint::Image::try_from_video_frame(frame, iv::VideoProfile::default())
            .expect("the bridge image takes the planes themselves");
        assert_eq!((image.size().width, image.size().height), (64, 32));
    }

    /// The picture the lane anchors its cues against is the very size slint
    /// derives from the frame, or every positioned cue sits beside the
    /// picture instead of on it.
    ///
    /// Asked of a real `slint::Image` rather than of a comment, and over an
    /// anamorphic case and a turned one, because those are the two places the
    /// two derivations could drift.
    #[test]
    fn the_scene_picture_is_the_size_slint_computes() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let cases = [
            ((64u32, 32u32), (1u32, 1u32), iv::BufferTransform::Normal),
            ((720, 576), (16, 15), iv::BufferTransform::Normal),
            ((720, 480), (8, 9), iv::BufferTransform::Normal),
            ((64, 32), (1, 1), iv::BufferTransform::Rotate90),
            ((720, 576), (16, 15), iv::BufferTransform::Rotate270),
            // A ratio that does not divide, where a rounding difference
            // between the two sides would show up as one pixel.
            ((1278, 720), (4, 3), iv::BufferTransform::Normal),
        ];
        for ((w, h), par, transform) in cases {
            let (y, uv) = nv12_planes(w, h, 128);
            let payload = gpu
                .present(nv12_desc(w, h), par, transform, &[&y, &uv], &[w, w])
                .expect("a flat nv12 frame must present");
            // What the sink copies out for the cue geometry.
            let picture = turned(transform, display_size((w, h), par));
            let frame: Arc<dyn iv::VideoFrame> = Arc::new(SinkFrame::new(payload));
            let image = slint::Image::try_from_video_frame(frame, iv::VideoProfile::default())
                .expect("the frame must be accepted");
            assert_eq!(
                (image.size().width, image.size().height),
                picture,
                "{w}x{h} par {par:?} {transform:?}"
            );
        }
    }

    /// The pool must not hand a slot back while slint can still read its
    /// planes. Four frames are presented and all four held, which is one more
    /// than the three generations the renderer keeps, so a slot that came
    /// back early would show up as two frames sharing plane textures.
    #[test]
    fn a_held_frame_never_shares_its_planes_with_a_later_one() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let lumas = [16u8, 96, 160, 235];
        let held: Vec<FramePayload> = lumas
            .iter()
            .map(|&l| present_flat(&mut gpu, 64, 32, l))
            .collect();

        // Distinct generations, and each frame still the picture it was
        // presented with, which is what a shared slot would break: four
        // frames off one plane set all read back the last luma written.
        for (i, a) in held.iter().enumerate() {
            for b in held.iter().skip(i + 1) {
                assert_ne!(a.generation, b.generation, "generations must not repeat");
            }
        }
        let reds: Vec<u8> = held
            .iter()
            .map(|p| center_red(&gpu, &as_texture(&gpu, p)))
            .collect();
        assert!(
            reds.windows(2).all(|w| w[0] < w[1]),
            "each held frame still carries its own picture, got {reds:?}"
        );
        assert!(reds[0] < 16, "limited-range 16 is black, got {}", reds[0]);
        assert!(reds[3] > 240, "limited-range 235 is white, got {}", reds[3]);
    }

    /// And the other half of the same rule: a released frame's slot comes
    /// back, so a settled stream cycles a handful of them instead of growing
    /// one per frame.
    #[test]
    fn a_released_frame_returns_its_slot_to_the_pool() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let pool = Arc::clone(&gpu.pool);
        assert_eq!(pool.free.lock().len(), 0);
        {
            let _held: Vec<FramePayload> = (0..4)
                .map(|_| present_flat(&mut gpu, 64, 32, 128))
                .collect();
            assert_eq!(pool.free.lock().len(), 0, "nothing comes back while held");
        }
        assert_eq!(
            pool.free.lock().len(),
            4,
            "every dropped frame returns its slot"
        );

        // And the returned slots are what the next frames are built from, so
        // the plane textures survive rather than being reallocated.
        let again = present_flat(&mut gpu, 64, 32, 128);
        assert_eq!(pool.free.lock().len(), 3);
        assert!(again.slot.as_ref().unwrap().upload.is_some());
    }

    /// A resolution change mid-stream rewrites the plane textures of the slot
    /// it lands in and leaves the ones still in flight alone, so a frame
    /// slint is holding keeps its own size.
    #[test]
    fn a_resolution_change_is_carried_by_the_slot_it_lands_in() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let small = present_flat(&mut gpu, 64, 32, 128);
        let large = present_flat(&mut gpu, 128, 64, 128);
        let small_tex = as_texture(&gpu, &small);
        let large_tex = as_texture(&gpu, &large);
        assert_eq!((small_tex.width(), small_tex.height()), (64, 32));
        assert_eq!((large_tex.width(), large_tex.height()), (128, 64));
        assert_eq!(
            small.slot.as_ref().unwrap().planes[0].width(),
            64,
            "the held frame's own plane did not move"
        );
    }

    /// A mid-stream format switch, which is what an ABR ladder does when it
    /// changes representation. NV12 has two planes and I420 three, so the
    /// reused slot has to grow and shrink its plane set rather than describe
    /// the old one's textures against the new desc.
    #[test]
    fn a_format_change_regrows_the_reused_planes() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (w, h) = (64u32, 32u32);
        // nv12 first, so a slot exists with two planes
        let payload = present_flat(&mut gpu, w, h, 235);
        assert_eq!(payload.slot.as_ref().unwrap().planes.len(), 2);
        assert!(center_red(&gpu, &as_texture(&gpu, &payload)) > 240);
        drop(payload);

        // i420 white: three planes, neutral chroma, into the returned slot
        let i420 = gst::Caps::from_str(&format!(
            "video/x-raw, format=(string)I420, width=(int){w}, height=(int){h}, \
             colorimetry=(string)bt709"
        ))
        .unwrap();
        let desc = desc_from_caps(&i420).expect("i420 must map").desc;
        let y = vec![235u8; (w * h) as usize];
        let u = vec![128u8; (w * h / 4) as usize];
        let payload = present_plain(&mut gpu, desc, &[&y, &u, &u], &[w, w / 2, w / 2])
            .expect("the format change must present");
        assert_eq!(payload.slot.as_ref().unwrap().planes.len(), 3);
        assert!(
            center_red(&gpu, &as_texture(&gpu, &payload)) > 240,
            "i420 white read back dark"
        );
        drop(payload);

        // and back down to two
        let payload = present_flat(&mut gpu, w, h, 16);
        assert_eq!(payload.slot.as_ref().unwrap().planes.len(), 2);
        assert!(
            center_red(&gpu, &as_texture(&gpu, &payload)) < 16,
            "nv12 black read back light"
        );
    }

    /// The lane declines when slint is not on its device.
    ///
    /// This is where the readback arm was. It is gone rather than moved: a
    /// machine that refused the shared device has slint on dodvg's OpenGL
    /// executor, which draws nothing at all for a video image, so there is
    /// nothing for a rendered picture to be handed to. `make_sink` answering
    /// `None` is what leaves the player with no video sink.
    #[test]
    fn without_a_shared_device_the_lane_declines() {
        // Nothing in this binary ever publishes one: the sinks under test
        // build their gpu explicitly.
        assert!(SHARED.get().is_none(), "the test binary adopts no device");
        assert!(Gpu::new(Quality::default()).is_none());
    }

    /// P010 is offered only when the device granted the 16-bit norm feature,
    /// otherwise the decoder would negotiate a format the upload refuses.
    #[test]
    fn the_offered_caps_follow_the_devices_features() {
        let Some(gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let formats = gpu.formats();
        assert!(formats.contains(&gst_video::VideoFormat::Nv12));
        assert!(formats.contains(&gst_video::VideoFormat::I420));
        for wide in [
            gst_video::VideoFormat::P01010le,
            gst_video::VideoFormat::I42010le,
            gst_video::VideoFormat::I42012le,
        ] {
            assert_eq!(formats.contains(&wide), gpu.norm16, "{wide:?}");
        }
    }

    /// Eight vertical bars of rising luma over neutral chroma, packed as
    /// three LSB-aligned 10-bit planes. Codes span the legal range so the
    /// encoder has nothing to clip.
    fn i420p10_bars(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut buf = Vec::with_capacity((w * h + 2 * cw * ch) * 2);
        for _ in 0..h {
            for col in 0..w {
                let bar = col * 8 / w;
                let code = 64u16 + bar as u16 * 125;
                buf.extend_from_slice(&code.to_le_bytes());
            }
        }
        for _ in 0..2 * cw * ch {
            buf.extend_from_slice(&512u16.to_le_bytes());
        }
        buf
    }

    /// The real decoder end of the gap. Everything else here builds the
    /// 10-bit planes itself, which only proves the crate agrees with the
    /// crate; this pushes the same bars through a lossless HEVC Main10
    /// round trip so gst decides where the codes sit inside the 16-bit
    /// words, and asserts the appsink negotiates `I420_10LE` off the lane's
    /// own offer and that the decoded frame renders.
    ///
    /// Reading LSB-aligned codes as if they were MSB-aligned divides every
    /// sample by 64, which sends the whole frame under limited black, so
    /// the white bar is what catches a wrong alignment.
    ///
    /// The static plugin set the receiver links carries no video encoder,
    /// so the bitstream is made out of process by ffmpeg and only the
    /// decode runs in here. No ffmpeg, no test.
    #[test]
    fn a_real_ten_bit_decoder_negotiates_and_renders() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };

        gst::init().unwrap();
        gstreamer_src::init_static_plugins();
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        if !gpu.norm16 {
            eprintln!("no 16-bit norm textures, skipping");
            return;
        }
        let make = |name: &str| gst::ElementFactory::make(name).build().ok();
        let (Some(parse), Some(dec)) = (make("h265parse"), make("avdec_h265")) else {
            eprintln!("no hevc decoder, skipping");
            return;
        };

        let (width, height) = (320u32, 240u32);
        let raw = i420p10_bars(width, height);
        let path = std::env::temp_dir().join(format!("fcast-i420p10-{}.h265", std::process::id()));
        let mut child = match Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p10le",
                "-s",
                &format!("{width}x{height}"),
                "-r",
                "30",
                "-i",
                "-",
                "-c:v",
                "libx265",
                // lossless and all-intra, so the codes that come back out
                // are the codes that went in
                "-x265-params",
                "lossless=1:keyint=1:log-level=none",
                "-f",
                "hevc",
                "-y",
            ])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                eprintln!("no ffmpeg, skipping");
                return;
            }
        };
        {
            let stdin = child.stdin.as_mut().unwrap();
            for _ in 0..4 {
                stdin.write_all(&raw).unwrap();
            }
        }
        let status = child.wait().unwrap();
        if !status.success() {
            let _ = std::fs::remove_file(&path);
            eprintln!("ffmpeg has no 10-bit x265, skipping");
            return;
        }

        // system memory only: a software decoder has no dmabufs to hand out
        // and the wide formats are never offered as one
        let sink = gst_app::AppSink::builder()
            .caps(&gpu.sysmem_caps())
            .max_buffers(4)
            .drop(false)
            .sync(false)
            .build();
        let src = gst::ElementFactory::make("filesrc")
            .property("location", path.to_str().unwrap())
            .build()
            .unwrap();
        let sink_element = sink.clone().upcast::<gst::Element>();
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&src, &parse, &dec, &sink_element])
            .unwrap();
        gst::Element::link_many([&src, &parse, &dec, &sink_element]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let sample = sink.pull_sample().expect("a decoded frame must arrive");
        let caps = sample.caps_owned().unwrap();
        let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
        assert_eq!(
            info.format(),
            gst_video::VideoFormat::I42010le,
            "the decoder settled on {caps} instead of the 10-bit planar offer"
        );
        let plan = gpu
            .parse_caps(caps)
            .expect("the lane must map what it offered");
        assert_eq!(plan.desc.format, PixelFormat::I420P10);
        assert!(wants_dither(&plan.desc));

        // the sysmem route, exactly as the sink runs it
        let out = {
            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(
                sample.buffer().unwrap(),
                &plan.info,
            )
            .unwrap();
            let pitch = frame.plane_stride();
            let planes: Vec<&[u8]> = (0..3).map(|i| frame.plane_data(i).unwrap()).collect();
            let strides: Vec<u32> = (0..3).map(|i| pitch[i] as u32).collect();
            let presented = gpu
                .present(
                    plan.desc,
                    plan.par,
                    iv::BufferTransform::Normal,
                    &planes,
                    &strides,
                )
                .expect("a decoded 10-bit frame must present");
            let tex = as_texture(&gpu, &presented);
            i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, &tex, width, height)
                .unwrap()
        };
        let _ = pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_file(&path);

        for row in bar_reds(&out, width, height) {
            for pair in row.windows(2) {
                assert!(pair[1] >= pair[0], "the bars are not rising: {row:?}");
            }
            // black bar stays dark, white bar reaches white. A misread
            // alignment puts every one of these at 0.
            assert!(row[0] < 40, "the black bar is not black: {row:?}");
            assert!(row[7] > 215, "the white bar is not white: {row:?}");
        }
    }

    /// The user-hit gap: a 4K AV1 HDR10+ clip software-decoded by dav1ddec
    /// arrives as three LSB-aligned 10-bit planes, and the lane used to
    /// refuse the caps outright and take the pipeline down with it.
    #[test]
    fn software_decoded_hdr10_av1_maps_as_planar_ten_bit() {
        let d = desc(
            "video/x-raw, format=(string)I420_10LE, width=(int)3840, height=(int)1606, \
             colorimetry=(string)bt2100-pq, chroma-site=(string)mpeg2, \
             framerate=(fraction)24000/1001",
        )
        .expect("bt2100-pq I420_10LE must map");
        assert_eq!(d.format, PixelFormat::I420P10);
        assert_eq!(d.format.plane_count(), 3);
        assert_eq!(d.matrix, Matrix::Bt2020Ncl);
        assert_eq!(d.transfer, Transfer::Pq);
        assert_eq!(d.primaries, Primaries::Bt2020);
        assert_eq!(d.range, Range::Limited);
        assert_eq!((d.width, d.height), (3840, 1606));
        // more than 8 bits of luma, so the 8-bit write must dither
        assert!(wants_dither(&d));
    }

    #[test]
    fn planar_twelve_bit_sdr_maps_too() {
        let d = desc(
            "video/x-raw, format=(string)I420_12LE, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt709",
        )
        .expect("I420_12LE must map");
        assert_eq!(d.format, PixelFormat::I420P12);
        assert_eq!(d.transfer, Transfer::Bt1886);
        assert!(wants_dither(&d));
    }

    /// The 16-bit MSB-aligned twin of the planar formats stays refused:
    /// nothing maps `I420_10BE`, and a wrong guess would read the bytes
    /// swapped rather than fail.
    #[test]
    fn big_endian_planar_ten_bit_is_still_refused() {
        assert!(
            desc(
                "video/x-raw, format=(string)I420_10BE, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt709",
            )
            .is_none()
        );
    }

    #[test]
    fn plain_sdr_h264_is_bt709_limited_bt1886_left() {
        let d = desc(
            "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
             framerate=(fraction)30/1, colorimetry=(string)bt709, \
             chroma-site=(string)mpeg2, interlace-mode=(string)progressive",
        )
        .expect("plain bt709 nv12 must map");
        assert_eq!(d.format, PixelFormat::Nv12);
        assert_eq!(d.matrix, Matrix::Bt709);
        assert_eq!(d.range, Range::Limited);
        // The trap: bt709 tags the OETF, and a decoded SDR stream is BT.1886
        // on the display side. Reading it as sRGB lifts every midtone.
        assert_eq!(d.transfer, Transfer::Bt1886);
        assert_eq!(d.primaries, Primaries::Bt709);
        assert_eq!(d.chroma_location, ChromaLocation::Left);
        assert_eq!((d.width, d.height), (1920, 1080));
    }

    #[test]
    fn hdr10_is_bt2020_pq_with_the_gamut_matrix_armed() {
        let d = desc(
            "video/x-raw, format=(string)P010_10LE, width=(int)3840, height=(int)2160, \
             colorimetry=(string)bt2100-pq, chroma-site=(string)mpeg2",
        )
        .expect("bt2100-pq p010 must map");
        assert_eq!(d.format, PixelFormat::P010);
        assert_eq!(d.matrix, Matrix::Bt2020Ncl);
        assert_eq!(d.transfer, Transfer::Pq);
        assert_eq!(d.primaries, Primaries::Bt2020);
        assert_eq!(d.range, Range::Limited);
        assert!(d.transfer.is_hdr());
    }

    #[test]
    fn hlg_maps_to_the_log_gamma_curve() {
        let d = desc(
            "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt2100-hlg",
        )
        .expect("bt2100-hlg must map");
        assert_eq!(d.transfer, Transfer::Hlg);
        assert_eq!(d.primaries, Primaries::Bt2020);
        assert_eq!(d.matrix, Matrix::Bt2020Ncl);
    }

    #[test]
    fn full_range_jpeg_chroma_is_center_sited() {
        let d = desc(
            "video/x-raw, format=(string)I420, width=(int)640, height=(int)480, \
             colorimetry=(string)bt601, chroma-site=(string)jpeg",
        )
        .expect("jpeg-sited i420 must map");
        assert_eq!(d.format, PixelFormat::I420);
        // The negative control in the crate measures a wrong siting at 107
        // codes on an upscale, so this is not cosmetic.
        assert_eq!(d.chroma_location, ChromaLocation::Center);
        assert_eq!(d.matrix, Matrix::Bt601);
    }

    /// Full range has no colorimetry alias in GStreamer, so it always arrives
    /// as the numeric `range:matrix:transfer:primaries` form. Reading it as
    /// limited stretches the signal and is the negative control the crate
    /// measures at 35 codes.
    #[test]
    fn sdtv_full_range_arrives_as_the_numeric_colorimetry_form() {
        let d = desc(
            "video/x-raw, format=(string)I420, width=(int)720, height=(int)576, \
             colorimetry=(string)1:4:16:4",
        )
        .expect("full range bt601 must map");
        assert_eq!(d.range, Range::Full);
        assert_eq!(d.matrix, Matrix::Bt601);
        assert_eq!(d.transfer, Transfer::Bt1886);
        assert_eq!(
            d.primaries,
            Primaries::Bt601_525,
            "smpte170m is the SD gamut"
        );
    }

    /// The gamut sets the old lane converted and this one used to flatten
    /// onto BT.709: every SD stream, and the P3 that phones tag.
    #[test]
    fn sd_and_p3_primaries_get_their_gamut_matrix() {
        // the numeric form is range:matrix:transfer:primaries, with gst's
        // primaries numbering: 3 bt470bg, 4 smpte170m, 5 smpte240m, 10
        // smpte-rp431, 11 smpte-eg432, 12 ebu3213
        for (colorimetry, want) in [
            ("bt601", Primaries::Bt601_525),
            ("1:4:0:4", Primaries::Bt601_525),
            ("smpte240m", Primaries::Bt601_525),
            ("1:4:0:5", Primaries::Bt601_525),
            ("1:4:0:3", Primaries::Bt601_625),
            ("1:4:0:12", Primaries::Bt601_625),
            ("1:4:0:11", Primaries::DisplayP3),
            ("1:4:0:10", Primaries::DisplayP3),
            ("bt709", Primaries::Bt709),
            ("bt2020", Primaries::Bt2020),
            // no variant: passes through as BT.709, as it always did
            ("1:4:0:2", Primaries::Bt709),
        ] {
            let d = desc(&format!(
                "video/x-raw, format=(string)NV12, width=(int)720, height=(int)576, \
                 colorimetry=(string){colorimetry}"
            ))
            .unwrap_or_else(|| panic!("{colorimetry} must map"));
            assert_eq!(d.primaries, want, "{colorimetry}");
        }
    }

    /// Caps with no colorimetry at all, which is what a raw testsrc or a
    /// stream that signals nothing produces. Every field must still land on
    /// the codec default rather than dropping the frame.
    #[test]
    fn unsignalled_caps_fall_back_to_the_codec_defaults() {
        let hd = desc(
            "video/x-raw, format=(string)NV12, width=(int)1280, height=(int)720, \
             colorimetry=(string)UNKNOWN",
        )
        .expect("unsignalled hd must still map");
        assert_eq!(hd.matrix, Matrix::Bt709);
        assert_eq!(hd.range, Range::Limited);
        assert_eq!(hd.transfer, Transfer::Bt1886);
        assert_eq!(hd.chroma_location, ChromaLocation::Left);

        let sd = desc(
            "video/x-raw, format=(string)I420, width=(int)720, height=(int)480, \
             colorimetry=(string)UNKNOWN",
        )
        .expect("unsignalled sd must still map");
        assert_eq!(sd.matrix, Matrix::Bt601, "SD without a tag is BT.601");
    }

    #[test]
    fn rgb_and_chroma_variants_map_with_the_right_color_decisions() {
        let rgba = desc("video/x-raw, format=(string)RGBA, width=(int)64, height=(int)64")
            .expect("RGBA maps now");
        assert_eq!(rgba.matrix, Matrix::Identity, "RGB layouts are Identity");
        assert_eq!(rgba.range, Range::Full, "unsignalled RGB is full range");

        // Bogus matrix signalling on an RGB layout is overridden, and an
        // explicitly signalled limited range is honored.
        let tagged = desc(
            "video/x-raw, format=(string)BGRA, width=(int)64, height=(int)64, \
             colorimetry=(string)bt709",
        )
        .expect("tagged BGRA maps");
        assert_eq!(tagged.matrix, Matrix::Identity);
        let limited = desc(
            "video/x-raw, format=(string)RGBA, width=(int)64, height=(int)64, \
             colorimetry=(string)2:1:5:1",
        )
        .expect("limited-range RGBA maps");
        assert_eq!(limited.range, Range::Limited, "signalled range wins");

        let y42b = desc("video/x-raw, format=(string)Y42B, width=(int)64, height=(int)64")
            .expect("4:2:2 maps now");
        assert_ne!(y42b.matrix, Matrix::Identity, "YCbCr is never Identity");
        assert_eq!(y42b.range, Range::Limited);

        assert!(
            desc("video/x-raw, format=(string)GRAY8, width=(int)64, height=(int)64").is_none(),
            "grayscale has no render path and stays refused at the mapping"
        );
    }

    /// Mastering metadata and MaxCLL ride the caps, and the crate picks the
    /// mastering peak first. Zeros mean unknown, not "peak zero".
    #[test]
    fn hdr_static_metadata_comes_off_the_caps() {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(
            "video/x-raw, format=(string)P010_10LE, width=(int)3840, height=(int)2160, \
             colorimetry=(string)bt2100-pq, \
             mastering-display-info=(string)\"34000:16000:13250:34500:7500:3000:15635:16450:10000000:1\", \
             content-light-level=(string)\"1000:400\"",
        )
        .unwrap();
        let hdr = hdr_metadata(&caps);
        assert_eq!(hdr.max_mastering_nits, 1000.0);
        assert_eq!(hdr.max_cll, 1000.0);

        let bare =
            gst::Caps::from_str("video/x-raw, format=(string)NV12, width=(int)64, height=(int)64")
                .unwrap();
        let hdr = hdr_metadata(&bare);
        assert_eq!(hdr.max_mastering_nits, 0.0);
        assert_eq!(hdr.max_cll, 0.0);
    }

    /// A peak too small to be real is unknown, not a peak. The old lane only
    /// believed a mastering luminance from 100 nits up, and the crate takes
    /// whatever it is handed: a 0.0001-nit peak would tone map the whole
    /// frame to black under playing audio.
    #[test]
    fn an_unbelievable_peak_is_treated_as_unknown() {
        gst::init().unwrap();
        // the units mistake seen in the wild: "1" where 10000000 was meant,
        // and a MaxCLL in the wrong unit too
        let caps = gst::Caps::from_str(
            "video/x-raw, format=(string)P010_10LE, width=(int)3840, height=(int)2160, \
             colorimetry=(string)bt2100-pq, \
             mastering-display-info=(string)\"34000:16000:13250:34500:7500:3000:15635:16450:1:1\", \
             content-light-level=(string)\"10:4\"",
        )
        .unwrap();
        let hdr = hdr_metadata(&caps);
        assert_eq!(hdr.max_mastering_nits, 0.0, "0.0001 nits is not a peak");
        assert_eq!(hdr.max_cll, 0.0, "10 nits is not a peak either");
        // and the crate then falls back to the transfer, not to black
        let info = gst_video::VideoInfo::from_caps(&caps).unwrap();
        let desc = frame_desc(&info, hdr).unwrap();
        assert_eq!(i_slint_video_wgpu::gpu::peak_nits(&desc), 10_000.0);

        // the floor itself, on both sides of it
        assert_eq!(believable_peak(99.9), 0.0);
        assert_eq!(believable_peak(100.0), 100.0);
        assert_eq!(believable_peak(1000.0), 1000.0);
        assert_eq!(believable_peak(f32::NAN), 0.0);
    }

    // -----------------------------------------------------------------
    // pixel aspect ratio
    // -----------------------------------------------------------------

    /// PAL SD, the case the whole correction exists for: 720x576 coded,
    /// 16:15 pixels, a 4:3 frame. Rendered 1:1 it is 16 pixels too narrow
    /// and `image-fit: contain` faithfully preserves the wrong aspect.
    #[test]
    fn anamorphic_pal_sd_renders_at_its_display_width() {
        assert_eq!(
            display(
                "video/x-raw, format=(string)I420, width=(int)720, height=(int)576, \
                 pixel-aspect-ratio=(fraction)16/15, colorimetry=(string)bt601"
            ),
            (768, 576)
        );
    }

    /// The other SD case, and the direction that shrinks: NTSC 720x480 with
    /// 8:9 pixels is the 640x480 frame it was authored as.
    #[test]
    fn anamorphic_ntsc_sd_renders_at_its_display_width() {
        assert_eq!(
            display(
                "video/x-raw, format=(string)I420, width=(int)720, height=(int)480, \
                 pixel-aspect-ratio=(fraction)8/9, colorimetry=(string)bt601"
            ),
            (640, 480)
        );
    }

    /// Square pixels, signalled or not, must leave the size alone, or every
    /// stream on earth would take a scale pass it does not need.
    #[test]
    fn square_pixels_are_left_at_the_coded_size() {
        assert_eq!(
            display(
                "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
                 pixel-aspect-ratio=(fraction)1/1"
            ),
            (1920, 1080)
        );
        assert_eq!(
            display("video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080"),
            (1920, 1080)
        );
    }

    /// A hand-built info covers what caps parsing will not let through: a
    /// ratio big enough to overflow a naive multiply, and the odd widths
    /// where the rounding has to land somewhere.
    #[test]
    fn a_degenerate_or_enormous_aspect_falls_back_to_the_coded_size() {
        gst::init().unwrap();
        let info = |w: u32, h: u32, n: i32, d: i32| {
            gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, w, h)
                .par(gst::Fraction::new(n, d))
                .build()
                .unwrap()
        };
        let sized = |w, h, n, d| {
            let i = info(w, h, n, d);
            display_size((i.width(), i.height()), sane_par(&i))
        };
        // a ratio that would ask for a 130000 pixel wide texture
        assert_eq!(sane_par(&info(1920, 1080, 10000, 147)), (1, 1));
        assert_eq!(sized(1920, 1080, 10000, 147), (1920, 1080));
        // u32::MAX numerator, the overflow trap the u64 math exists for
        assert_eq!(sized(65535, 1080, i32::MAX, 1), (65535, 1080));
        // And one that is only slightly off square. Truncating, not rounded:
        // it is slint that derives the shown width from the raw size and this
        // ratio, and the lane has to land on the same number it does.
        assert_eq!(sized(1279, 720, 4, 3), (1705, 720));
        assert_eq!(sized(1281, 721, 3, 4), (960, 721));
    }

    // -----------------------------------------------------------------
    // dither gating
    // -----------------------------------------------------------------

    /// More than 8 bits in, or an absolute-light curve tone mapped down to
    /// 8, is where a smooth gradient bands. Plain 8-bit SDR is already on
    /// the target's grid, and leaving it undithered is what keeps its
    /// single-pass route and its bit-exact parity.
    #[test]
    fn dither_is_armed_for_deep_and_hdr_sources_only() {
        let hdr10 = desc(
            "video/x-raw, format=(string)P010_10LE, width=(int)3840, height=(int)2160, \
             colorimetry=(string)bt2100-pq",
        )
        .unwrap();
        assert!(wants_dither(&hdr10));

        // 8-bit HLG, so the depth is not what arms it
        let hlg = desc(
            "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt2100-hlg",
        )
        .unwrap();
        assert!(wants_dither(&hlg));

        // 10-bit SDR, so the transfer is not what arms it either
        let deep_sdr = desc(
            "video/x-raw, format=(string)P010_10LE, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt709",
        )
        .unwrap();
        assert!(wants_dither(&deep_sdr));

        for sdr in [
            "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
             colorimetry=(string)bt709",
            "video/x-raw, format=(string)I420, width=(int)720, height=(int)576, \
             colorimetry=(string)bt601",
            "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
             colorimetry=(string)sRGB",
        ] {
            assert!(!wants_dither(&desc(sdr).unwrap()), "{sdr}");
        }
    }

    // -----------------------------------------------------------------
    // rotation
    // -----------------------------------------------------------------

    /// The four turns a container can ask for, plus the flipped forms that
    /// have no render for them and must not be mistaken for a turn.
    #[test]
    fn the_orientation_tag_maps_to_the_four_turns() {
        gst::init().unwrap();
        let tagged = |value: &str| {
            let mut tags = gst::TagList::new();
            tags.get_mut()
                .unwrap()
                .add::<gst::tags::ImageOrientation>(&value, gst::TagMergeMode::Replace);
            rotation_from_tags(&tags)
        };
        assert_eq!(tagged("rotate-0"), Some(iv::BufferTransform::Normal));
        assert_eq!(tagged("rotate-90"), Some(iv::BufferTransform::Rotate90));
        assert_eq!(tagged("rotate-180"), Some(iv::BufferTransform::Rotate180));
        assert_eq!(tagged("rotate-270"), Some(iv::BufferTransform::Rotate270));
        assert_eq!(tagged("flip-rotate-90"), Some(iv::BufferTransform::Normal));
        // a tag list with no orientation at all leaves the last one standing
        assert_eq!(rotation_from_tags(&gst::TagList::new()), None);
    }

    /// The code the sink's atomic carries has to survive both directions,
    /// or a mid-stream tag would come back as a different turn.
    #[test]
    fn the_rotation_code_round_trips() {
        for rotation in [
            iv::BufferTransform::Normal,
            iv::BufferTransform::Rotate90,
            iv::BufferTransform::Rotate180,
            iv::BufferTransform::Rotate270,
        ] {
            assert_eq!(rotation_from_code(rotation_code(rotation)), rotation);
        }
        // anything else is upright rather than a panic
        assert_eq!(rotation_from_code(9), iv::BufferTransform::Normal);
    }

    /// The siting flags are a bitfield and "unknown" is the empty set, not a
    /// named value. Falling through to center on empty would shift every
    /// unsignalled stream by a quarter chroma texel.
    #[test]
    fn unknown_siting_is_left_not_center() {
        use gst_video::VideoChromaSite as S;
        assert_eq!(map_chroma(S::empty()), ChromaLocation::Left);
        assert_eq!(map_chroma(S::MPEG2), ChromaLocation::Left);
        assert_eq!(map_chroma(S::COSITED), ChromaLocation::Left);
        assert_eq!(map_chroma(S::JPEG), ChromaLocation::Center);
        assert_eq!(map_chroma(S::NONE), ChromaLocation::Center);
    }

    // -----------------------------------------------------------------
    // the geometry, on a real device
    // -----------------------------------------------------------------

    /// White in the source's top-left quadrant, black elsewhere, so a turn
    /// is visible as the corner it lands in.
    fn corner_planes(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        let mut y = vec![16u8; (width * height) as usize];
        for row in 0..height / 2 {
            for col in 0..width / 2 {
                y[(row * width + col) as usize] = 235;
            }
        }
        (y, vec![128u8; (width * height / 2) as usize])
    }

    /// One present through whichever arm the gpu is in, unpacked to rgba8
    /// with the size that came out, so a case grades both arms with one
    /// body.
    fn present_rgba(
        gpu: &mut Gpu,
        desc: FrameDesc,
        par: (u32, u32),
        rotation: iv::BufferTransform,
        planes: &[&[u8]],
        strides: &[u32],
    ) -> (u32, u32, Vec<u8>) {
        let payload = gpu
            .present(desc, par, rotation, planes, strides)
            .expect("the frame must be presentable");
        let tex = as_texture(gpu, &payload);
        let (w, h) = (tex.width(), tex.height());
        let pixels = i_slint_video_wgpu::gpu::read_rgba8(&gpu.device, &gpu.queue, &tex, w, h)
            .expect("readback must succeed");
        (w, h, pixels)
    }

    /// The lit quadrant after a clockwise turn: top left, then top right,
    /// bottom right, bottom left.
    fn assert_lit_quadrant(pixels: &[u8], w: u32, h: u32, rotation: iv::BufferTransform) {
        let at = |x: u32, y: u32| pixels[((y * w + x) * 4) as usize];
        let (qx, qy) = (w / 4, h / 4);
        let reds = [
            at(qx, qy),
            at(w - qx - 1, qy),
            at(w - qx - 1, h - qy - 1),
            at(qx, h - qy - 1),
        ];
        let want = match rotation.rotation_degrees() {
            90 => 1,
            180 => 2,
            270 => 3,
            _ => 0,
        };
        for (i, red) in reds.iter().enumerate() {
            if i == want {
                assert!(*red > 240, "{rotation:?}: quadrant {i} is dark, {reds:?}");
            } else {
                assert!(*red < 16, "{rotation:?}: quadrant {i} is lit, {reds:?}");
            }
        }
    }

    // -----------------------------------------------------------------
    // render profiles
    // -----------------------------------------------------------------

    /// The mapping against libplacebo's three preset structs
    /// (`src/renderer.c:202`), which is what the other lane hands
    /// `pl_render_image`.
    #[test]
    fn the_profiles_are_the_libplacebo_presets() {
        // pl_render_fast_params sets nothing at all: no scaler, no dither,
        // no deband
        let fast = profile_quality(RenderProfile::Fast);
        assert_eq!(fast.up, ScaleFilter::Bilinear);
        assert_eq!(fast.down, ScaleFilter::Bilinear);
        assert_eq!(fast.deband, None);

        // pl_render_default_params: lanczos up, hermite down, dither, still
        // no deband
        let balanced = profile_quality(RenderProfile::Balanced);
        assert_eq!(balanced.up, ScaleFilter::Lanczos3);
        assert_eq!(balanced.down, ScaleFilter::Hermite);
        assert_eq!(balanced.dither, DitherRule::WiderThanTarget);
        assert_eq!(balanced.deband, None);

        // pl_render_high_quality_params, whose deband_params is the only
        // one of the three that is set, and set to the defaults
        let hq = profile_quality(RenderProfile::HighQuality);
        assert_eq!(hq.down, ScaleFilter::Hermite);
        assert_eq!(hq.dither, DitherRule::Always);
        let d = hq.deband.expect("high quality debands");
        assert_eq!(
            (d.iterations, d.threshold, d.radius, d.grain),
            (1, 3.0, 16.0, 4.0)
        );
        // the crate has no ewa kernel, so pl_filter_ewa_lanczossharp becomes
        // the sharpest separable one it does have
        assert_eq!(hq.up, ScaleFilter::Lanczos3);

        // and the receiver's default really is the cheap one
        assert_eq!(Quality::default(), fast);
    }

    /// The scale direction picks the kernel the way `renderer.c` does, with
    /// a shrink on either axis counting as a downscale.
    #[test]
    fn the_kernel_follows_the_scale_direction() {
        let q = profile_quality(RenderProfile::Balanced);
        assert_eq!(q.filter((1920, 1080), (3840, 2160)), ScaleFilter::Lanczos3);
        assert_eq!(q.filter((1920, 1080), (1280, 720)), ScaleFilter::Hermite);
        // one axis shrinking is enough
        assert_eq!(q.filter((1920, 1080), (1280, 1080)), ScaleFilter::Hermite);
        // and the kernel does not depend on the direction on Fast, which has
        // only the one
        let f = profile_quality(RenderProfile::Fast);
        assert_eq!(f.filter((1920, 1080), (3840, 2160)), ScaleFilter::Bilinear);
        assert_eq!(f.filter((1920, 1080), (1280, 720)), ScaleFilter::Bilinear);
    }

    /// Each profile has to reach the output desc as its own thing, which is
    /// the whole point of plumbing it through.
    #[test]
    fn each_profile_produces_its_own_video_profile() {
        // a 10-bit HDR source being scaled up, so every knob is in play
        let mut d = nv12_desc(1920, 1080);
        d.format = PixelFormat::P010;
        d.transfer = Transfer::Pq;
        let profiles: Vec<iv::VideoProfile> = [
            RenderProfile::Fast,
            RenderProfile::Balanced,
            RenderProfile::HighQuality,
        ]
        .into_iter()
        .map(|p| profile_quality(p).profile(&d, (3840, 2160)))
        .collect();
        assert_ne!(
            profiles[0], profiles[1],
            "fast and balanced render the same"
        );
        assert_ne!(
            profiles[1], profiles[2],
            "balanced and high quality render the same"
        );
        assert_eq!(profiles[0].filter, iv::ScaleFilter::Bilinear);
        assert_eq!(profiles[1].filter, iv::ScaleFilter::Lanczos3);
        assert!(
            profiles.iter().all(|p| p.dither),
            "an hdr frame always dithers"
        );
        assert_eq!(profiles[0].deband, None);
        assert_eq!(profiles[1].deband, None);
        assert!(profiles[2].deband.is_some());

        // The turn is not in here at all any more: it is a property of the
        // frame, so slint reads it off the frame and joins the two itself.
        // Pinned, because a rotation field creeping back into the profile
        // would make the handle compare unequal on a stream that never turned.
        let same = profile_quality(RenderProfile::Fast).profile(&d, (3840, 2160));
        assert_eq!(profiles[0], same, "a profile is a property of the stream");
    }

    /// The dither rules, which are the one knob still reading the frame.
    #[test]
    fn the_dither_rule_follows_the_source() {
        let sdr8 = nv12_desc(64, 32);
        let mut wide = sdr8;
        wide.format = PixelFormat::P010;
        let mut hdr = sdr8;
        hdr.transfer = Transfer::Pq;

        // Fast dithers only the tone mapped write, the one that bands hard
        let f = profile_quality(RenderProfile::Fast);
        assert!(!f.dither.applies(&sdr8));
        assert!(!f.dither.applies(&wide));
        assert!(f.dither.applies(&hdr));

        // Balanced is exactly what this lane always did
        let b = profile_quality(RenderProfile::Balanced);
        for d in [&sdr8, &wide, &hdr] {
            assert_eq!(b.dither.applies(d), wants_dither(d));
        }

        // and high quality dithers every quantizing write, which is all of
        // them since the target is always 8-bit
        let h = profile_quality(RenderProfile::HighQuality);
        for d in [&sdr8, &wide, &hdr] {
            assert!(h.dither.applies(d));
        }
    }

    /// The regression gate. An 8-bit SDR frame at its own size is what
    /// nearly every stream is, and on it the two cheap profiles have to
    /// render the bytes this lane rendered before profiles existed:
    /// bilinear is unused at 1:1, and neither dithers nor debands.
    #[test]
    fn balanced_renders_the_bytes_the_lane_always_did() {
        let (w, h) = (64u32, 32u32);
        let (y, uv) = corner_planes(w, h);
        let mut shots = Vec::new();
        for profile in [RenderProfile::Fast, RenderProfile::Balanced] {
            let Some(mut gpu) = test_gpu_with(profile_quality(profile)) else {
                eprintln!("no gpu adapter, skipping");
                return;
            };
            let (ow, oh, px) = present_rgba(
                &mut gpu,
                nv12_desc(w, h),
                (1, 1),
                iv::BufferTransform::Normal,
                &[&y, &uv],
                &[w, w],
            );
            assert_eq!((ow, oh), (w, h));
            shots.push(px);
        }
        assert_eq!(shots[0], shots[1], "balanced moved the common render");

        // and the profile the two hand slint is the render the lane used to
        // build by hand: bilinear, dither only where the source is deeper
        // than the target, no deband
        let d = nv12_desc(w, h);
        let was = iv::VideoProfile {
            filter: iv::ScaleFilter::Bilinear,
            dither: wants_dither(&d),
            deband: None,
            tonemap: iv::TonemapCurve::Spline,
        };
        let fast = profile_quality(RenderProfile::Fast).profile(&d, (w, h));
        assert_eq!(fast, was, "fast is not the render that was there before");
        let balanced = profile_quality(RenderProfile::Balanced).profile(&d, (w, h));
        // only the kernel differs, and at 1:1 the renderer never reads it
        assert_eq!(
            iv::VideoProfile {
                filter: iv::ScaleFilter::Bilinear,
                ..balanced
            },
            was
        );
    }

    /// High quality actually changes the picture, and does it with a pinned
    /// grain rather than one that crawls.
    ///
    /// The seed used to move per frame, the way libplacebo's temporal
    /// `sh_prng` does. It cannot any more: the profile is what slint keys its
    /// registered source on, so a seed that moved would miss that cache on
    /// every frame and rebuild the views and bind groups it exists to avoid.
    /// So the grain is a pure function of the input, which is the one thing
    /// that changed meaning in the move to the fused lane.
    #[test]
    fn high_quality_debands_with_a_pinned_grain() {
        let (w, h) = (64u32, 32u32);
        // a gentle luma gradient, which is what bands
        let mut y = vec![16u8; (w * h) as usize];
        for row in 0..h {
            for col in 0..w {
                y[(row * w + col) as usize] = (80.0 + 20.0 * col as f64 / w as f64) as u8;
            }
        }
        let uv = vec![128u8; (w * h / 2) as usize];
        let shoot = |profile: RenderProfile, frames: usize| -> Option<Vec<Vec<u8>>> {
            let mut gpu = test_gpu_with(profile_quality(profile))?;
            Some(
                (0..frames)
                    .map(|_| {
                        present_rgba(
                            &mut gpu,
                            nv12_desc(w, h),
                            (1, 1),
                            iv::BufferTransform::Normal,
                            &[&y, &uv],
                            &[w, w],
                        )
                        .2
                    })
                    .collect(),
            )
        };
        let Some(plain) = shoot(RenderProfile::Fast, 1) else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let hq = shoot(RenderProfile::HighQuality, 2).unwrap();
        assert_ne!(plain[0], hq[0], "high quality rendered the cheap picture");
        assert_eq!(
            hq[0], hq[1],
            "the grain is seeded per stream, not per frame"
        );
        assert_eq!(
            profile_quality(RenderProfile::HighQuality)
                .profile(&nv12_desc(w, h), (w, h))
                .deband
                .map(|d| d.seed),
            Some(0),
            "a seed that moves would re-key slint's source every frame"
        );
    }

    #[test]
    fn the_frame_turns_and_swaps_its_size() {
        let (w, h) = (64u32, 32u32);
        let (y, uv) = corner_planes(w, h);
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        for rotation in [
            iv::BufferTransform::Normal,
            iv::BufferTransform::Rotate90,
            iv::BufferTransform::Rotate180,
            iv::BufferTransform::Rotate270,
        ] {
            let (ow, oh, pixels) = present_rgba(
                &mut gpu,
                nv12_desc(w, h),
                (1, 1),
                rotation,
                &[&y, &uv],
                &[w, w],
            );
            assert_eq!((ow, oh), turned(rotation, (w, h)), "{rotation:?}");
            assert_lit_quadrant(&pixels, ow, oh, rotation);
        }
    }

    /// Anamorphic PAL SD: 720x576 coded with 16:15 pixels is seen as
    /// 768x576, which is the 4:3 frame the scene fits without squeezing it.
    #[test]
    fn anamorphic_content_is_seen_at_its_display_size() {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(
            "video/x-raw, format=(string)NV12, width=(int)720, height=(int)576, \
             pixel-aspect-ratio=(fraction)16/15, colorimetry=(string)bt709",
        )
        .unwrap();
        let CapsPlan { desc, size, .. } = desc_from_caps(&caps).expect("pal sd must map");
        assert_eq!(size, (768, 576));
        let (y, uv) = corner_planes(720, 576);
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (ow, oh, pixels) = present_rgba(
            &mut gpu,
            desc,
            (16, 15),
            iv::BufferTransform::Normal,
            &[&y, &uv],
            &[720, 720],
        );
        assert_eq!((ow, oh), (768, 576));
        assert_lit_quadrant(&pixels, ow, oh, iv::BufferTransform::Normal);
    }

    /// The two corrections compose: the aspect is fixed in the source's
    /// orientation and the turn then swaps the corrected size, which is the
    /// order a rotated phone clip with non-square pixels needs.
    #[test]
    fn a_turn_composes_with_the_aspect_correction() {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(
            "video/x-raw, format=(string)NV12, width=(int)720, height=(int)576, \
             pixel-aspect-ratio=(fraction)16/15, colorimetry=(string)bt709",
        )
        .unwrap();
        let CapsPlan { desc, par, .. } = desc_from_caps(&caps).expect("pal sd must map");
        let (y, uv) = corner_planes(720, 576);
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (ow, oh, pixels) = present_rgba(
            &mut gpu,
            desc,
            par,
            iv::BufferTransform::Rotate90,
            &[&y, &uv],
            &[720, 720],
        );
        assert_eq!((ow, oh), (576, 768), "the turn swaps the corrected size");
        assert_lit_quadrant(&pixels, ow, oh, iv::BufferTransform::Rotate90);
    }

    /// A tag arriving mid-stream must land on the next frame with no
    /// restart, which is what the sink's atomic is for. The caps cache must
    /// survive it: the turn is not part of the caps.
    #[test]
    fn a_mid_stream_tag_turns_the_next_frame_without_a_reparse() {
        let Some(mut gpu) = texture_gpu() else {
            eprintln!("no gpu adapter, skipping");
            return;
        };
        let (w, h) = (64u32, 32u32);
        let (y, uv) = corner_planes(w, h);
        let desc = nv12_desc(w, h);
        let (ow, oh, pixels) = present_rgba(
            &mut gpu,
            desc,
            (1, 1),
            iv::BufferTransform::Normal,
            &[&y, &uv],
            &[w, w],
        );
        assert_lit_quadrant(&pixels, ow, oh, iv::BufferTransform::Normal);
        let (ow, oh, pixels) = present_rgba(
            &mut gpu,
            desc,
            (1, 1),
            iv::BufferTransform::Rotate270,
            &[&y, &uv],
            &[w, w],
        );
        assert_eq!((ow, oh), (h, w));
        assert_lit_quadrant(&pixels, ow, oh, iv::BufferTransform::Rotate270);
        // and back, so a stream-start reset shows up on the next frame too
        let (ow, oh, pixels) = present_rgba(
            &mut gpu,
            desc,
            (1, 1),
            iv::BufferTransform::Normal,
            &[&y, &uv],
            &[w, w],
        );
        assert_eq!((ow, oh), (w, h));
        assert_lit_quadrant(&pixels, ow, oh, iv::BufferTransform::Normal);
    }
}

/// The lane under a real player doing many stream transitions.
///
/// The single-item proofs above cannot see this: the appsink lives for the
/// whole session and negotiates once per item, so anything that survives a
/// transition only shows after several of them.
///
/// # Running these against a real VA decoder
///
/// `flapjack-test` blanks `LIBVA_DRIVERS_PATH` from a ctor, so a plain
/// `cargo test` run has no VA elements and these fall back to software decode
/// with no dmabuf at all. Setting `LIBVA_DRIVER_NAME` opts back in, which is
/// what the zero-copy half of this module needs:
///
/// ```text
/// LIBVA_DRIVER_NAME=iHD cargo test -p receiver-ui --features desktop \
///     --lib -- --test-threads=1 transitions
/// ```
#[cfg(all(test, target_os = "linux"))]
mod transitions {
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc as StdArc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use flapjack::{
        AudioSink, MediaInput, Player, PlayerEvent, SelectionGate, Sinks, StartPoint, VideoSink,
    };
    use simulator::sink::FTestSink;
    use gst::prelude::*;

    /// Sizes the clips are encoded at.
    const SIZES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 360)];

    /// Transitions per run. The field failure landed on the eighth item, so a
    /// default well past that still finishes in well under a minute; the soak
    /// number goes in the env.
    fn rounds() -> usize {
        std::env::var("FCAST_TRANSITION_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12)
    }

    /// Two seconds of h264 at each size, encoded once per process. `None`
    /// without ffmpeg, so this skips instead of failing.
    /// One fixture clip, generated once per process under a lock. The soaks
    /// run in parallel and share this directory, and an existence check on
    /// its own let one test open the file another test's ffmpeg was still
    /// writing ("This file contains no playable streams"). Written beside
    /// its final name and renamed into place, so a killed ffmpeg leaves no
    /// half file for the next caller to find.
    fn fixture_clip(name: &str, args: &[&str]) -> Option<PathBuf> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fcast-transitions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(name);
        if path.exists() {
            return Some(path);
        }
        // same extension, so ffmpeg still picks the muxer off it
        let partial = dir.join(format!("partial-{name}"));
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(args)
            .arg("-y")
            .arg(&partial)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        std::fs::rename(&partial, &path).ok()?;
        Some(path)
    }

    fn clips() -> Option<Vec<PathBuf>> {
        let mut out = Vec::new();
        for (w, h) in SIZES {
            let video = format!("testsrc2=size={w}x{h}:rate=25:duration=2");
            out.push(fixture_clip(
                &format!("clip-{w}x{h}.mp4"),
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    &video,
                    // audio too: the field failure landed on the audio half
                    // of a join, and a video-only item never builds the
                    // second streamsynchronizer pair
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=2",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-g",
                    "25",
                    "-c:a",
                    "aac",
                    "-shortest",
                ],
            )?);
        }
        Some(out)
    }

    /// An audio-only item. Between two video items it takes the video chain
    /// out of the pipeline, so the next video item joins a chain that has to
    /// be BUILT rather than reused, which is the shape the field failures all
    /// share ("a chain join finished kind=Video").
    fn audio_only_clip() -> Option<PathBuf> {
        fixture_clip(
            "clip-audio.m4a",
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "aac",
            ],
        )
    }

    fn uri(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    /// gst, the static plugins, and the test-only elements flapjack builds.
    fn init() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // The lane's own lines are the only view of which route a
            // transition took, so a run with RUST_LOG set prints them.
            if std::env::var_os("RUST_LOG").is_some() {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                    .try_init();
            }
            crate::gstreamer::init_and_load_plugins();
            simulator::register_for_tests();
            let _ = flapjack::audiostretch::plugin_init();
        });
    }

    /// Whether a VA h264 decoder exists AND its driver opened. Without one the
    /// runs still grade the transition itself, just over system memory.
    fn va_present() -> bool {
        gst::ElementFactory::find("vah264dec").is_some()
    }

    /// What one run observed at the sink pad.
    #[derive(Default)]
    struct Seen {
        /// Caps events, in order, as strings.
        caps: Mutex<Vec<String>>,
        /// Allocation queries the sink pad answered.
        allocations: AtomicUsize,
        /// Allocation queries that arrived without `GstVideoMeta` already on
        /// them, which is every one the lane has to add it to.
        allocations_without_meta: AtomicUsize,
    }

    /// Watches the sink pad the way the field log does: which caps each item
    /// negotiated, and whether the allocation query behind them ever went
    /// missing.
    fn watch_pad(sink: &gst::Element) -> StdArc<Seen> {
        let seen = StdArc::new(Seen::default());
        let pad = sink.static_pad("sink").expect("the appsink has a sink pad");
        {
            let seen = StdArc::clone(&seen);
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && let gst::EventView::Caps(caps) = event.view()
                {
                    seen.caps.lock().unwrap().push(caps.caps().to_string());
                }
                gst::PadProbeReturn::Ok
            });
        }
        {
            let seen = StdArc::clone(&seen);
            pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Query(query)) = &mut info.data
                    && let gst::QueryViewMut::Allocation(alloc) = query.view_mut()
                {
                    seen.allocations.fetch_add(1, Ordering::AcqRel);
                    if alloc
                        .find_allocation_meta::<gst_video::VideoMeta>()
                        .is_none()
                    {
                        seen.allocations_without_meta.fetch_add(1, Ordering::AcqRel);
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
        seen
    }

    /// How one round moves to the next item.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum How {
        /// Pre-armed on the live core. decodebin3 reuses the slot and the
        /// decoder, so the sink sees stream-start, caps and segment only.
        Gapless,
        /// Pre-arms and fresh loads, alternating, which is what a real session
        /// does. A fresh load takes the video chain out of the pipeline and
        /// puts it back, which is the route/unroute pair the field log shows
        /// as a chain join.
        Alternating,
    }

    /// One transition soak. `pick` chooses which clip each round switches to,
    /// so a caller can hold the coded size still (the shape that makes the
    /// lane's caps compare equal across items) or cycle it (the shape that
    /// makes every join renegotiate).
    fn soak(name: &str, how: How, pick: impl Fn(usize) -> usize) {
        let Some(clips) = clips() else {
            eprintln!("no ffmpeg, skipping");
            return;
        };
        soak_with(name, how, clips, false, pick);
    }

    fn soak_with(
        name: &str,
        how: How,
        clips: Vec<PathBuf>,
        refuse_imports: bool,
        pick: impl Fn(usize) -> usize,
    ) {
        init();
        super::FAIL_IMPORTS.store(0, Ordering::Release);
        let engine = fcast_video::cue::CueEngine::new();
        let Some((video_sink, _tick)) = super::tests::test_sink(engine.clone()) else {
            eprintln!("no vulkan device, skipping");
            return;
        };
        let seen = watch_pad(&video_sink);
        // What the lane actually advertised on this box. Without it the dmabuf
        // assertion below would fail on a machine whose VA driver is present
        // but whose adapter has no importable modifier.
        let offered_dmabuf = video_sink
            .property::<Option<gst::Caps>>("caps")
            .is_some_and(|c| c.to_string().contains("memory:DMABuf"));

        let player = StdArc::new(
            Player::new(Sinks {
                video: VideoSink::Element(video_sink),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
                subtitle: flapjack::SubtitleSink::None,
            })
            .expect("building flapjack"),
        );

        let errors: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));
        let loaded = StdArc::new(AtomicBool::new(false));
        let activated = StdArc::new(AtomicUsize::new(0));
        {
            let errors = StdArc::clone(&errors);
            let loaded = StdArc::clone(&loaded);
            let activated = StdArc::clone(&activated);
            player.set_event_handler(None, move |flapjack::Event { kind: event, .. }| match event {
                PlayerEvent::Error { message, .. } => errors.lock().unwrap().push(message),
                PlayerEvent::Loaded { .. } => loaded.store(true, Ordering::Release),
                PlayerEvent::PreparedActivated { .. } => {
                    activated.fetch_add(1, Ordering::AcqRel);
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
            let held = errors.lock().unwrap();
            assert!(held.is_empty(), "{name}: pipeline error: {held:?}");
        };

        player.load(
            MediaInput::uri(uri(&clips[pick(0)])),
            StartPoint::at(gst::ClockTime::ZERO),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while !loaded.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "{name}: the load never finished");
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        player.play(player.allocate_op());

        let rounds = rounds();
        let mut gapless_done = 0usize;
        for round in 1..=rounds {
            let next = uri(&clips[pick(round)]);
            // Audio-only items always take the load path: that is what makes
            // the video chain leave the pipeline and the next video item join
            // a chain that has to be BUILT.
            let gapless = !next.ends_with(".m4a")
                && match how {
                    How::Gapless => true,
                    How::Alternating => round % 2 == 1,
                };
            // A refused import narrows the sink's offer mid-stream, which is
            // the state every join below has to survive.
            if refuse_imports {
                super::FAIL_IMPORTS.store(1, Ordering::Release);
            }
            // A gapless swap only activates when the current item ends, so the
            // wait has to cover the longest clip in the cycle, not the 2s ones.
            let deadline = Instant::now() + Duration::from_secs(45);
            if gapless {
                gapless_done += 1;
                player.prepare_next(MediaInput::uri(next));
                while activated.load(Ordering::Acquire) < gapless_done {
                    assert!(
                        Instant::now() < deadline,
                        "{name}: transition {round} never activated"
                    );
                    pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                let expects_video = !next.ends_with(".m4a");
                let want = seen.caps.lock().unwrap().len() + 1;
                loaded.store(false, Ordering::Release);
                player.load(
                    MediaInput::uri(next),
                    StartPoint::at(gst::ClockTime::ZERO),
                );
                while !loaded.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "{name}: transition {round} never loaded"
                    );
                    pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
                player.play(player.allocate_op());
                // Wait for the new item's caps to actually land, so the round
                // really did renegotiate rather than just change state. An
                // audio-only item never negotiates video caps.
                while expects_video && seen.caps.lock().unwrap().len() < want {
                    assert!(
                        Instant::now() < deadline,
                        "{name}: transition {round} never negotiated caps"
                    );
                    pump();
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        pump();
        // The guarantee has to be on the decoders of the pipeline the player
        // built, not only on the ones a hand-made test pipeline holds: the hook
        // that puts it there is armed off the sink landing in that pipeline.
        let decoders = player
            .pipeline()
            .iterate_recurse()
            .into_iter()
            .flatten()
            .filter(|e| e.is::<gst_video::VideoDecoder>())
            .collect::<Vec<_>>();
        let guaranteed = decoders
            .iter()
            .filter(|e| {
                e.static_pad("src")
                    .is_some_and(|pad| unsafe { pad.data::<bool>(super::META_PROBE_KEY) }.is_some())
            })
            .count();
        assert_eq!(
            guaranteed,
            decoders.len(),
            "{name}: {} of {} decoders carry the video meta guarantee",
            guaranteed,
            decoders.len()
        );
        // An audio-only last item leaves no video decoder behind.
        if !uri(&clips[pick(rounds)]).ends_with(".m4a") {
            assert!(!decoders.is_empty(), "{name}: no decoder was ever built");
        }
        // Synchronous, not `stop()`: that is queued on the worker, and a test
        // that returns while the teardown runs leaves the worker alive at
        // process exit when it is the last one to finish. That segfaulted a
        // flapjack thread in libc once these soaks began to run for real.
        // The shutdown barrier fires when everything is down; a disconnect
        // means the worker is already gone, which is the same thing.
        let (down_tx, down_rx) = std::sync::mpsc::channel();
        player.shutdown(Box::new(move || {
            let _ = down_tx.send(());
        }));
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            down_rx.recv_timeout(std::time::Duration::from_secs(30))
        {
            panic!("{name}: the player's teardown never finished");
        }

        let caps = seen.caps.lock().unwrap();
        let dmabuf = caps.iter().filter(|c| c.contains("memory:DMABuf")).count();
        eprintln!(
            "{name}: {rounds} transitions, {} caps events ({dmabuf} dmabuf), \
             {} allocation queries, {} of them without a video meta",
            caps.len(),
            seen.allocations.load(Ordering::Acquire),
            seen.allocations_without_meta.load(Ordering::Acquire),
        );
        assert!(
            !caps.is_empty(),
            "{name}: no caps ever reached the sink, nothing was graded"
        );
        if va_present() && offered_dmabuf {
            assert!(
                dmabuf > 0,
                "{name}: a VA decoder was available and the lane never took the \
                 dmabuf route, so the zero-copy path went ungraded: {caps:?}"
            );
        }
    }

    /// Every join changes the coded size, so the decoder re-runs its output
    /// negotiation each time. This is the shape the field failure had: the
    /// item before the break was a different resolution, which is why the lane
    /// logged a coded-size change one millisecond before the pipeline died.
    #[test]
    fn transitions_at_changing_sizes_keep_the_negotiation() {
        soak("changing sizes", How::Gapless, |round| round % SIZES.len());
    }

    /// Every join keeps the coded size, so the lane's caps compare EQUAL
    /// across items and its per-stream caches are never invalidated by the
    /// caps path. Anything that has to be dropped at a stream boundary has to
    /// be dropped by something else here.
    #[test]
    fn transitions_at_one_size_keep_the_negotiation() {
        soak("one size", How::Gapless, |_| 0);
    }

    /// Loads and gapless swaps interleaved, which is what takes the video
    /// chain out of the pipeline and puts it back between items. That
    /// route/unroute pair is what the field log calls a chain join, and it is
    /// the only thing that re-activates the sink pad under a live decoder.
    #[test]
    fn reloads_between_gapless_swaps_keep_the_negotiation() {
        soak("reloads", How::Alternating, |round| round % SIZES.len());
    }

    /// Clips whose decoded format the lane has no render path for. 4:2:2 and
    /// 4:4:4 are what an ordinary camera or screen recorder produces, and
    /// neither is in the offer, so both take the caps event down the path
    /// that used to end the item.
    fn odd_format_clips() -> Option<Vec<PathBuf>> {
        let mut out = Vec::new();
        for pix in ["yuv422p", "yuv444p"] {
            out.push(fixture_clip(
                &format!("clip-{pix}.mp4"),
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=640x360:rate=25:duration=2",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=2",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    pix,
                    "-g",
                    "25",
                    "-c:a",
                    "aac",
                    "-shortest",
                ],
            )?);
        }
        Some(out)
    }

    /// The field failure, end to end. A 4:2:2 and a 4:4:4 item are cycled with
    /// ordinary ones and with an audio-only item, so every join shape runs
    /// while a format outside the offer is negotiated. Before the sink
    /// answered ACCEPT_CAPS itself this died on the first odd item with
    /// "streaming stopped, reason not-negotiated" out of the source, which the
    /// pump below grades as a pipeline error.
    #[test]
    fn formats_the_lane_cannot_render_never_kill_the_pipeline() {
        let (Some(mut clips), Some(odd)) = (clips(), odd_format_clips()) else {
            eprintln!("no ffmpeg, skipping");
            return;
        };
        clips.truncate(1);
        clips.extend(odd);
        if let Some(audio) = audio_only_clip() {
            clips.push(audio);
        }
        // A field file joins the cycle without being checked in.
        if let Some(extra) = std::env::var_os("FCAST_EXTRA_CLIP") {
            clips.push(PathBuf::from(extra));
        }
        let n = clips.len();
        soak_with(
            "odd formats",
            How::Alternating,
            clips,
            false,
            move |round| round % n,
        );
    }

    /// One refused import per item, over both join shapes. A refusal narrows
    /// the sink's offer while the decoder is still on dmabuf caps, and every
    /// later join has to survive that narrowing.
    #[test]
    fn a_refused_import_never_kills_the_pipeline() {
        let Some(clips) = clips() else {
            eprintln!("no ffmpeg, skipping");
            return;
        };
        soak_with("refused imports", How::Alternating, clips, true, |round| {
            round % SIZES.len()
        });
    }
}
