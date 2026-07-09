#[cfg(target_os = "linux")]
use std::ffi::c_void;
use std::{mem::ManuallyDrop, ptr};

use anyhow::anyhow;
#[cfg(target_os = "linux")]
use drm_fourcc::DrmFourcc;
use gst_video::prelude::*;
#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
use libplacebo::Vulkan;
use libplacebo::{OpenGL, Renderer, Swapchain, SwapchainFrame, libplacebo_sys::*};
use tracing::{debug, warn};

use crate::video::{MasteringDisplayInfo, Overlay, Rotation};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum RenderProfile {
    Fast,
    Balanced,
    HighQuality,
}

#[derive(Clone, Copy)]
pub struct RenderingOptions {
    pub profile: RenderProfile,
    pub visualize_lut: bool,
    pub show_clipping: bool,
}

fn gst_matrix_to_placebo(matrix: gst_video::VideoColorMatrix) -> pl_color_system {
    match matrix {
        gst_video::VideoColorMatrix::Rgb => pl_color_system::PL_COLOR_SYSTEM_RGB,
        gst_video::VideoColorMatrix::Bt709 => pl_color_system::PL_COLOR_SYSTEM_BT_709,
        gst_video::VideoColorMatrix::Bt601 => pl_color_system::PL_COLOR_SYSTEM_BT_601,
        gst_video::VideoColorMatrix::Smpte240m => pl_color_system::PL_COLOR_SYSTEM_SMPTE_240M,
        gst_video::VideoColorMatrix::Bt2020 => pl_color_system::PL_COLOR_SYSTEM_BT_2020_NC,
        gst_video::VideoColorMatrix::Unknown | gst_video::VideoColorMatrix::Fcc | _ => {
            pl_color_system::PL_COLOR_SYSTEM_BT_709
        }
    }
}

fn gst_primaries_to_placebo(primaries: gst_video::VideoColorPrimaries) -> pl_color_primaries {
    match primaries {
        gst_video::VideoColorPrimaries::Bt709 => pl_color_primaries::PL_COLOR_PRIM_BT_709,
        gst_video::VideoColorPrimaries::Bt470m => pl_color_primaries::PL_COLOR_PRIM_BT_470M,
        gst_video::VideoColorPrimaries::Bt470bg => pl_color_primaries::PL_COLOR_PRIM_BT_601_625,
        gst_video::VideoColorPrimaries::Smpte170m => pl_color_primaries::PL_COLOR_PRIM_BT_601_525,
        gst_video::VideoColorPrimaries::Smpte240m => pl_color_primaries::PL_COLOR_PRIM_BT_601_525,
        gst_video::VideoColorPrimaries::Film => pl_color_primaries::PL_COLOR_PRIM_FILM_C,
        gst_video::VideoColorPrimaries::Bt2020 => pl_color_primaries::PL_COLOR_PRIM_BT_2020,
        gst_video::VideoColorPrimaries::Adobergb => pl_color_primaries::PL_COLOR_PRIM_ADOBE,
        gst_video::VideoColorPrimaries::Smptest428 => pl_color_primaries::PL_COLOR_PRIM_CIE_1931,
        gst_video::VideoColorPrimaries::Smpterp431 => pl_color_primaries::PL_COLOR_PRIM_DCI_P3,
        gst_video::VideoColorPrimaries::Smpteeg432 => pl_color_primaries::PL_COLOR_PRIM_DISPLAY_P3,
        gst_video::VideoColorPrimaries::Ebu3213 => pl_color_primaries::PL_COLOR_PRIM_EBU_3213,
        gst_video::VideoColorPrimaries::Unknown | _ => pl_color_primaries::PL_COLOR_PRIM_BT_709,
    }
}

fn gst_range_to_placebo(range: gst_video::VideoColorRange) -> pl_color_levels {
    match range {
        gst_video::VideoColorRange::Range0_255 => pl_color_levels::PL_COLOR_LEVELS_FULL,
        gst_video::VideoColorRange::Range16_235 => pl_color_levels::PL_COLOR_LEVELS_LIMITED,
        gst_video::VideoColorRange::Unknown | _ => pl_color_levels::PL_COLOR_LEVELS_UNKNOWN,
    }
}

fn gst_transfer_to_placebo(transfer: gst_video::VideoTransferFunction) -> pl_color_transfer {
    match transfer {
        gst_video::VideoTransferFunction::Gamma10 => pl_color_transfer::PL_COLOR_TRC_LINEAR,
        gst_video::VideoTransferFunction::Gamma18 => pl_color_transfer::PL_COLOR_TRC_GAMMA18,
        gst_video::VideoTransferFunction::Gamma20 => pl_color_transfer::PL_COLOR_TRC_GAMMA20,
        gst_video::VideoTransferFunction::Gamma22 => pl_color_transfer::PL_COLOR_TRC_GAMMA22,
        gst_video::VideoTransferFunction::Bt709 => pl_color_transfer::PL_COLOR_TRC_BT_1886,
        gst_video::VideoTransferFunction::Smpte240m => pl_color_transfer::PL_COLOR_TRC_BT_1886,
        gst_video::VideoTransferFunction::Srgb => pl_color_transfer::PL_COLOR_TRC_SRGB,
        gst_video::VideoTransferFunction::Gamma28 => pl_color_transfer::PL_COLOR_TRC_GAMMA28,
        gst_video::VideoTransferFunction::Bt202012 => pl_color_transfer::PL_COLOR_TRC_BT_1886,
        gst_video::VideoTransferFunction::Bt202010 => pl_color_transfer::PL_COLOR_TRC_BT_1886,
        gst_video::VideoTransferFunction::Smpte2084 => pl_color_transfer::PL_COLOR_TRC_PQ,
        gst_video::VideoTransferFunction::AribStdB67 => pl_color_transfer::PL_COLOR_TRC_HLG,
        gst_video::VideoTransferFunction::Bt601 => pl_color_transfer::PL_COLOR_TRC_BT_1886,
        gst_video::VideoTransferFunction::Log100
        | gst_video::VideoTransferFunction::Log316
        | gst_video::VideoTransferFunction::Adobergb
        | gst_video::VideoTransferFunction::Unknown
        | _ => pl_color_transfer::PL_COLOR_TRC_BT_1886,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe fn destroy_textures(gpu: *const pl_gpu_t, num_planes: i32, planes: &mut [pl_plane; 4]) {
    for p in 0..num_planes {
        let mut tex = planes[p as usize].texture;
        if !tex.is_null() {
            unsafe {
                pl_tex_destroy(gpu, &mut tex);
            }
        }
    }
}

struct RenderFrameInfo {
    primaries: pl_color_primaries,
    transfer: pl_color_transfer,
    color_repr: pl_color_repr,
    sample_depth: i32,
    format: gst_video::VideoFormatInfo,
}

impl RenderFrameInfo {
    pub fn new(info: &gst_video::VideoInfo) -> Self {
        let colorimetry = info.colorimetry();
        let color_system = gst_matrix_to_placebo(colorimetry.matrix());
        let primaries = gst_primaries_to_placebo(colorimetry.primaries());
        let levels = gst_range_to_placebo(colorimetry.range());
        let transfer = gst_transfer_to_placebo(colorimetry.transfer());

        let format_info = info.format_info();
        let sample_depth = if info.comp_depth(0) <= 8 { 8 } else { 16 };
        let color_depth = info.comp_depth(0) as i32;
        let bit_shift = format_info.shift()[0] as i32;

        let color_repr = pl_color_repr {
            sys: color_system,
            levels,
            alpha: pl_alpha_mode::PL_ALPHA_NONE,
            bits: pl_bit_encoding {
                sample_depth,
                color_depth,
                bit_shift,
            },
            dovi: ptr::null(),
        };

        Self {
            primaries,
            transfer,
            color_repr,
            sample_depth,
            format: format_info,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RenderFrameError {
    #[cfg(target_os = "linux")]
    #[error("invalid DRM FourCC")]
    InvalidFourcc,
    #[cfg(target_os = "linux")]
    #[error("invalid DMABuf file descriptor(s)")]
    InvalidDmaBufFd,
    #[cfg(target_os = "linux")]
    #[error("unsupported DMABuf plane format")]
    UnsupportedPlaneFormat,
    #[cfg(target_os = "linux")]
    #[error("DMA_DRM buffer is missing VideoMeta")]
    MissingVideoMeta,
    #[cfg(target_os = "linux")]
    #[error("invalid format info")]
    InvalidFormatInfo,
    #[cfg(target_os = "linux")]
    #[error("failed to create texture from DMABuf")]
    TextureCreation,
    #[cfg(target_os = "macos")]
    #[error("buffer memory is not IOSurface-backed")]
    NotIOSurface,
    #[cfg(target_os = "macos")]
    #[error("unsupported IOSurface plane format")]
    UnsupportedPlaneFormat,
    #[cfg(target_os = "macos")]
    #[error("failed to import IOSurface plane into a GL texture")]
    IOSurfaceImport,
    #[cfg(target_os = "macos")]
    #[error("failed to wrap GL texture with libplacebo")]
    TextureWrap,
    #[error("frame is missing a plane")]
    MissingPlane,
    #[error("failed to upload plane")]
    PlaneUploadFailed,
}

fn create_pl_frame(
    num_planes: i32,
    info: &gst_video::VideoInfo,
    frame_info: &RenderFrameInfo,
    mastering_display_info: &Option<MasteringDisplayInfo>,
) -> pl_frame {
    let mut frame: pl_frame = unsafe { std::mem::zeroed() };
    frame.num_planes = num_planes;
    frame.repr = frame_info.color_repr;
    frame.color.primaries = frame_info.primaries;
    frame.color.transfer = frame_info.transfer;
    if let Some(mdi) = mastering_display_info.as_ref() {
        frame.color.hdr.prim.red = mdi.display_primaries[0].as_pl_cie_xy();
        frame.color.hdr.prim.green = mdi.display_primaries[1].as_pl_cie_xy();
        frame.color.hdr.prim.blue = mdi.display_primaries[2].as_pl_cie_xy();
        frame.color.hdr.prim.white = mdi.white_point.as_pl_cie_xy();
        let max_luma = mdi.max_luminance_as_nits();
        if max_luma >= 100.0 {
            frame.color.hdr.max_luma = max_luma;
            frame.color.hdr.min_luma = mdi.min_luminance_as_nits();
        }
    }
    frame.crop = pl_rect2df {
        x0: 0.0,
        y0: 0.0,
        x1: info.width() as f32,
        y1: info.height() as f32,
    };
    frame
}

enum Backend {
    OpenGL {
        opengl: ManuallyDrop<OpenGL>,
        swapchain: ManuallyDrop<Swapchain>,
    },
    #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
    Vulkan(ManuallyDrop<Vulkan>),
}

pub struct PlaceboContext {
    backend: Backend,
    renderer: ManuallyDrop<Renderer>,
    // Textures used for system memory buffers
    cached_textures: [pl_tex; 4],
    // Reusable textures backing composited overlays
    overlay_textures: Vec<pl_tex>,
    rendering_params: pl_render_params,
    // Warn-once latch for the IOSurface -> CPU-readback fallback in `render_frame`.
    #[cfg(target_os = "macos")]
    iosurface_fallback_warned: bool,
}

impl PlaceboContext {
    pub fn new(log: &libplacebo::Log, opts: &RenderingOptions) -> anyhow::Result<Self> {
        let opengl =
            libplacebo::OpenGL::new(log).ok_or(anyhow!("failed to create opengl context"))?;
        Self::new_from_gl(log, opengl, opts)
    }

    #[cfg(target_os = "linux")]
    pub unsafe fn new_egl(
        log: &libplacebo::Log,
        opts: &RenderingOptions,
        display: *mut c_void,
        context: *mut c_void,
    ) -> anyhow::Result<Self> {
        let opengl = unsafe { libplacebo::OpenGL::new_egl(log, display, context) }
            .ok_or(anyhow!("failed to create opengl context"))?;
        Self::new_from_gl(log, opengl, opts)
    }

    fn new_from_gl(
        log: &libplacebo::Log,
        opengl: OpenGL,
        opts: &RenderingOptions,
    ) -> anyhow::Result<Self> {
        let swapchain = Swapchain::new(&opengl).ok_or(anyhow!("failed to create swapchain"))?;
        let renderer = Renderer::new(log, &opengl).ok_or(anyhow!("failed to create renderer"))?;

        Ok(Self {
            backend: Backend::OpenGL {
                opengl: ManuallyDrop::new(opengl),
                swapchain: ManuallyDrop::new(swapchain),
            },
            renderer: ManuallyDrop::new(renderer),
            cached_textures: [std::ptr::null(); 4],
            overlay_textures: Vec::new(),
            rendering_params: build_render_params(opts),
            #[cfg(target_os = "macos")]
            iosurface_fallback_warned: false,
        })
    }

    /// A windowless Vulkan context: it has no swapchain and can't present, but renders into
    /// (dmabuf-exported) textures. Used by the Wayland subsurface sink. `drm_device` (a `dev_t`,
    /// e.g. the compositor's dmabuf-feedback main device) pins the GPU selection so exported
    /// dmabufs are importable on multi-GPU systems.
    #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
    pub fn new_vulkan(
        log: &libplacebo::Log,
        opts: &RenderingOptions,
        drm_device: Option<u64>,
    ) -> anyhow::Result<Self> {
        let vulkan =
            Vulkan::new(log, drm_device).ok_or(anyhow!("failed to create vulkan context"))?;
        let renderer = unsafe { Renderer::new_from_gpu(log, vulkan.gpu()) }
            .ok_or(anyhow!("failed to create renderer"))?;

        Ok(Self {
            backend: Backend::Vulkan(ManuallyDrop::new(vulkan)),
            renderer: ManuallyDrop::new(renderer),
            cached_textures: [std::ptr::null(); 4],
            overlay_textures: Vec::new(),
            rendering_params: build_render_params(opts),
        })
    }

    #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
    pub fn is_vulkan(&self) -> bool {
        matches!(self.backend, Backend::Vulkan(_))
    }

    fn swapchain(&self) -> Option<&Swapchain> {
        match &self.backend {
            Backend::OpenGL { swapchain, .. } => Some(swapchain),
            #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
            Backend::Vulkan(_) => None,
        }
    }

    pub fn resize_swapchain(&self, width: i32, height: i32) {
        match self.swapchain() {
            Some(swapchain) => swapchain.resize(width, height),
            None => warn!("resize_swapchain called on a swapchain-less context"),
        }
    }

    fn flush_texture_cache(&mut self) {
        let gpu = self.gpu();
        for i in 0..self.cached_textures.len() {
            if !self.cached_textures[i].is_null() {
                unsafe {
                    pl_tex_destroy(gpu, &mut self.cached_textures[i]);
                    assert!(self.cached_textures[i].is_null());
                }
            }
        }
        for tex in self.overlay_textures.iter_mut() {
            if !tex.is_null() {
                unsafe {
                    pl_tex_destroy(gpu, tex);
                    assert!(tex.is_null());
                }
            }
        }
        self.overlay_textures.clear();
    }

    pub fn flush_cache(&mut self) {
        self.renderer.flush_cache();
        self.flush_texture_cache();
        debug!("Flushed cache");
    }

    pub fn start_frame(&self) -> Option<SwapchainFrame> {
        self.swapchain()?.start_frame()
    }

    #[tracing::instrument(skip_all)]
    pub fn submit_frame(&self) {
        if !self.swapchain().is_some_and(|s| s.submit_frame()) {
            warn!("Failed to submit frame");
        }
    }

    fn upload_sys_mem(
        &mut self,
        info: &gst_video::VideoInfo,
        frame_info: &RenderFrameInfo,
        frame: &gst_video::VideoFrame<gst_video::video_frame::Readable>,
        image: &mut pl_frame,
    ) -> std::result::Result<(), RenderFrameError> {
        let format_pixel_strides = frame_info.format.pixel_stride();

        let strides = frame.plane_stride();
        for plane_idx in 0..image.num_planes {
            let Ok(data) = frame.plane_data(plane_idx as u32) else {
                return Err(RenderFrameError::MissingPlane);
            };
            let plane = &mut image.planes[plane_idx as usize];
            let pixel_stride = format_pixel_strides[plane_idx as usize];
            let plane_width = frame_info.format.scale_width(plane_idx as u8, info.width());
            let plane_height = frame_info
                .format
                .scale_height(plane_idx as u8, info.height());
            let plane_stride = strides[plane_idx as usize];

            let mut plane_data = pl_plane_data {
                type_: pl_fmt_type::PL_FMT_UNORM,
                width: plane_width as i32,
                height: plane_height as i32,
                component_size: [0; 4],
                component_pad: [0; 4],
                component_map: [pl_channel::PL_CHANNEL_NONE as i32; 4],
                pixel_stride: pixel_stride as usize,
                row_stride: plane_stride as usize,
                swapped: false,
                pixels: data.as_ptr() as *const _,
                buf: std::ptr::null(),
                buf_offset: 0,
                callback: None,
                priv_: std::ptr::null_mut(),
            };

            // `pl_plane_data` describes components in *memory order* (component_map maps the
            // n-th component in memory to a color channel). For planar/biplanar YUV memory
            // order coincides with component index order, but packed formats don't (BGRA
            // stores B first while B is component 2) — sort by the component's byte offset
            // into the pixel. The stable sort keeps index order for planar formats where all
            // offsets are 0. Padding bytes that aren't a component (the X in xRGB/BGRx) are
            // expressed via component_pad.
            let poffsets = frame_info.format.poffset();
            let mut comps = [(0u32, 0u32); 4];
            let mut components = 0;
            for comp_idx in 0..frame.n_components() {
                if info.comp_plane(comp_idx as u8) == plane_idx as u32 {
                    comps[components] = (poffsets[comp_idx as usize], comp_idx);
                    components += 1;
                }
            }
            let comps = &mut comps[..components];
            comps.sort_by_key(|&(poffset, _)| poffset);
            let comp_bytes = (frame_info.sample_depth / 8) as u32;
            let mut next_offset = 0u32;
            for (slot, &(poffset, comp_idx)) in comps.iter().enumerate() {
                plane_data.component_map[slot] = comp_idx as i32;
                plane_data.component_size[slot] = frame_info.sample_depth;
                plane_data.component_pad[slot] = (poffset.saturating_sub(next_offset) * 8) as i32;
                next_offset = poffset + comp_bytes;
            }

            unsafe {
                if !pl_upload_plane(
                    self.gpu(),
                    plane,
                    &mut self.cached_textures[plane_idx as usize],
                    &plane_data,
                ) {
                    return Err(RenderFrameError::PlaneUploadFailed);
                }
            }
        }

        Ok(())
    }

    /// Upload each overlay into a reusable texture and build the matching
    /// `pl_overlay`/`pl_overlay_part` lists. The returned parts must be kept alive alongside the
    /// overlays for the duration of the `pl_render_image` call (the overlays hold raw pointers into
    /// the parts vec).
    ///
    /// Overlays are addressed in *source-frame* coordinates (`PL_OVERLAY_COORDS_SRC_FRAME`) and set
    /// on the source image, so libplacebo scales AND rotates them together with the video (matching
    /// the frame's `rotation`) for free.
    fn upload_overlays(&mut self, overlays: &[Overlay]) -> (Vec<pl_overlay>, Vec<pl_overlay_part>) {
        if self.overlay_textures.len() < overlays.len() {
            self.overlay_textures.resize(overlays.len(), ptr::null());
        }

        let mut parts = Vec::with_capacity(overlays.len());
        let mut texs = Vec::with_capacity(overlays.len());
        for (i, ov) in overlays.iter().enumerate() {
            let mut plane = libplacebo::new_plane();
            let plane_data = pl_plane_data {
                type_: pl_fmt_type::PL_FMT_UNORM,
                width: ov.width as i32,
                height: ov.height as i32,
                component_size: [8, 8, 8, 8],
                component_pad: [0; 4],
                // Pixels are packed RGBA (the sink already swapped B/R).
                component_map: [0, 1, 2, 3],
                pixel_stride: 4,
                row_stride: ov.width as usize * 4,
                swapped: false,
                pixels: ov.pixels.as_ptr() as *const _,
                buf: ptr::null(),
                buf_offset: 0,
                callback: None,
                priv_: ptr::null_mut(),
            };

            let ok = unsafe {
                pl_upload_plane(
                    self.gpu(),
                    &mut plane,
                    &mut self.overlay_textures[i],
                    &plane_data,
                )
            };
            if !ok || self.overlay_textures[i].is_null() {
                warn!("failed to upload overlay texture");
                continue;
            }

            parts.push(pl_overlay_part {
                src: pl_rect2df {
                    x0: 0.0,
                    y0: 0.0,
                    x1: ov.width as f32,
                    y1: ov.height as f32,
                },
                // Source-frame pixel placement; libplacebo maps it through the same
                // scale/letterbox/rotation as the video.
                dst: pl_rect2df {
                    x0: ov.x as f32,
                    y0: ov.y as f32,
                    x1: (ov.x + ov.render_width as i32) as f32,
                    y1: (ov.y + ov.render_height as i32) as f32,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            });
            texs.push(self.overlay_textures[i]);
        }

        let mut pl_overlays = Vec::with_capacity(parts.len());
        for (part, &tex) in parts.iter().zip(texs.iter()) {
            pl_overlays.push(pl_overlay {
                tex,
                mode: pl_overlay_mode::PL_OVERLAY_NORMAL,
                coords: pl_overlay_coords::PL_OVERLAY_COORDS_SRC_FRAME,
                repr: pl_color_repr {
                    sys: pl_color_system::PL_COLOR_SYSTEM_RGB,
                    levels: pl_color_levels::PL_COLOR_LEVELS_FULL,
                    // GStreamer overlay ARGB is straight (non-premultiplied) alpha.
                    alpha: pl_alpha_mode::PL_ALPHA_INDEPENDENT,
                    bits: pl_bit_encoding {
                        sample_depth: 8,
                        color_depth: 8,
                        bit_shift: 0,
                    },
                    dovi: ptr::null(),
                },
                color: overlay_color_space(),
                parts: part as *const pl_overlay_part,
                num_parts: 1,
            });
        }

        (pl_overlays, parts)
    }

    /// Attach `overlays` to the source `image` (in source-frame coordinates) if any, then render it
    /// into `destination`. Overlays ride the image through the renderer, so they scale and rotate
    /// with the video (`image.rotation`). The overlay/part vecs are kept alive across the
    /// `pl_render_image` call because `pl_frame.overlays` holds borrowed pointers.
    fn render_image_with_overlays(
        &mut self,
        image: &mut pl_frame,
        destination: &pl_frame,
        overlays: &[Overlay],
    ) {
        let (pl_overlays, _parts) = if overlays.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            self.upload_overlays(overlays)
        };
        if !pl_overlays.is_empty() {
            image.overlays = pl_overlays.as_ptr();
            image.num_overlays = pl_overlays.len() as i32;
        }

        unsafe {
            pl_render_image(
                self.renderer.renderer,
                image,
                destination,
                &self.rendering_params,
            );
        }
    }

    fn render_sysmem(
        &mut self,
        destination: &libplacebo::SwapchainFrame,
        source: &gst_video::VideoFrame<gst_video::video_frame::Readable>,
        mdi: &Option<MasteringDisplayInfo>,
        overlays: &[Overlay],
        rotation: Rotation,
    ) -> std::result::Result<(), RenderFrameError> {
        let mut target = unsafe {
            let mut t = std::mem::zeroed();
            pl_frame_from_swapchain(&mut t, &destination.frame);
            t
        };
        self.render_sysmem_to_frame(&mut target, source, mdi, overlays, rotation)
    }

    fn render_sysmem_to_frame(
        &mut self,
        destination: &mut pl_frame,
        source: &gst_video::VideoFrame<gst_video::video_frame::Readable>,
        mdi: &Option<MasteringDisplayInfo>,
        overlays: &[Overlay],
        rotation: Rotation,
    ) -> std::result::Result<(), RenderFrameError> {
        let info = source.info();
        let frame_info = RenderFrameInfo::new(info);

        let mut image = create_pl_frame(source.n_planes() as i32, info, &frame_info, mdi);
        image.rotation = rotation_to_pl(rotation);
        if let Err(err) = self.upload_sys_mem(info, &frame_info, source, &mut image) {
            self.flush_texture_cache();
            return Err(err);
        };

        destination.crop =
            libplacebo::scale_and_fit(&destination.crop, &rotated_fit_rect(&image.crop, rotation));

        self.render_image_with_overlays(&mut image, destination, overlays);

        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn render_dmabuf(
        &mut self,
        destination: &libplacebo::SwapchainFrame,
        source_buffer: &gst::Buffer,
        source_dma_info: &gst_video::VideoInfoDmaDrm,
        mdi: &Option<MasteringDisplayInfo>,
        overlays: &[Overlay],
        rotation: Rotation,
    ) -> std::result::Result<(), RenderFrameError> {
        let mut target = unsafe {
            let mut t = std::mem::zeroed();
            pl_frame_from_swapchain(&mut t, &destination.frame);
            t
        };
        self.render_dmabuf_to_frame(
            &mut target,
            source_buffer,
            source_dma_info,
            mdi,
            overlays,
            rotation,
        )
    }

    #[cfg(target_os = "linux")]
    fn render_dmabuf_to_frame(
        &mut self,
        destination: &mut pl_frame,
        source_buffer: &gst::Buffer,
        source_dma_info: &gst_video::VideoInfoDmaDrm,
        mdi: &Option<MasteringDisplayInfo>,
        overlays: &[Overlay],
        rotation: Rotation,
    ) -> std::result::Result<(), RenderFrameError> {
        use tracing::error;

        let Some(vmeta) = source_buffer.meta::<gst_video::VideoMeta>() else {
            return Err(RenderFrameError::MissingVideoMeta);
        };

        let mut fds = [-1i32; 4];
        let mut offsets = [0; 4];
        let mut strides = [0; 4];
        let mut sizes = [0usize; 4];
        let n_planes = vmeta.n_planes() as usize;
        let dma_drm_fourcc = DrmFourcc::try_from(source_dma_info.fourcc())
            .map_err(|_| RenderFrameError::InvalidFourcc)?;
        let modifier = source_dma_info.modifier();

        let vmeta_offsets = vmeta.offset();
        let vmeta_strides = vmeta.stride();

        for plane in 0..n_planes {
            let Some((range, skip)) =
                source_buffer.find_memory(vmeta_offsets[plane]..(vmeta_offsets[plane] + 1))
            else {
                break;
            };

            let mem = source_buffer.peek_memory(range.start);
            let Some(mem) = mem.downcast_memory_ref::<gst_allocators::DmaBufMemory>() else {
                break;
            };

            let size = mem.size();
            let fd = mem.fd();
            fds[plane] = fd;
            offsets[plane] = mem.offset() + skip;
            strides[plane] = vmeta_strides[plane] as usize;
            sizes[plane] = size;
        }

        if !fds[0..n_planes].iter().all(|fd| *fd != -1) {
            return Err(RenderFrameError::InvalidDmaBufFd);
        }

        let normal_info = source_dma_info
            .to_video_info()
            .map_err(|_| RenderFrameError::InvalidFormatInfo)?;
        let frame_info = RenderFrameInfo::new(&normal_info);

        let mut image = create_pl_frame(n_planes as i32, &normal_info, &frame_info, mdi);
        image.rotation = rotation_to_pl(rotation);
        for plane_idx in 0..image.num_planes {
            let fmt_fourcc = crate::dmabuf::fourcc_from_plane(plane_idx, dma_drm_fourcc);
            let fmt = unsafe {
                libplacebo::libplacebo_sys::pl_find_fourcc(self.gpu(), fmt_fourcc as u32)
            };
            if fmt.is_null() {
                error!(?fmt_fourcc, "Plane has unsupported fourcc");
                unsafe { destroy_textures(self.gpu(), image.num_planes, &mut image.planes) };
                return Err(RenderFrameError::UnsupportedPlaneFormat);
            }

            let mut tex_params: pl_tex_params = unsafe { std::mem::zeroed() };
            let plane_width = frame_info
                .format
                .scale_width(plane_idx as u8, normal_info.width());
            let plane_height = frame_info
                .format
                .scale_height(plane_idx as u8, normal_info.height());
            tex_params.w = plane_width as i32;
            tex_params.h = plane_height as i32;
            tex_params.format = fmt;
            tex_params.sampleable = true;
            let caps = unsafe { (*fmt).caps as u32 };
            tex_params.blit_src = caps & pl_fmt_caps::PL_FMT_CAP_BLITTABLE as u32 > 0;
            tex_params.import_handle = pl_handle_type_PL_HANDLE_DMA_BUF;
            tex_params.shared_mem = pl_shared_mem {
                handle: pl_handle {
                    fd: fds[plane_idx as usize],
                },
                size: sizes[plane_idx as usize],
                offset: offsets[plane_idx as usize],
                drm_format_mod: modifier,
                stride_w: strides[plane_idx as usize],
                stride_h: 0,
                plane: 0,
            };

            let tex = unsafe { pl_tex_create(self.gpu(), &tex_params) };
            if tex.is_null() {
                unsafe {
                    destroy_textures(self.gpu(), plane_idx + 1, &mut image.planes);
                }
                return Err(RenderFrameError::TextureCreation);
            }
            image.planes[plane_idx as usize].texture = tex;

            let mut components = 0;
            for comp_idx in 0..normal_info.n_components() {
                if normal_info.comp_plane(comp_idx as u8) == plane_idx as u32 {
                    image.planes[plane_idx as usize].component_mapping[components] =
                        comp_idx as i32;
                    components += 1;
                }
            }

            image.planes[plane_idx as usize].components = components as i32;
        }

        destination.crop =
            libplacebo::scale_and_fit(&destination.crop, &rotated_fit_rect(&image.crop, rotation));

        self.render_image_with_overlays(&mut image, destination, overlays);
        unsafe {
            destroy_textures(self.gpu(), image.num_planes, &mut image.planes);
        }

        Ok(())
    }

    /// Zero-copy render of a VideoToolbox IOSurface frame. Mirrors [`render_dmabuf`]: import each
    /// plane's IOSurface into a `GL_TEXTURE_RECTANGLE`, wrap it with `pl_opengl_wrap`, then render.
    ///
    /// Runs inside `SwapchainSink::render` where Slint's CGL context (the same one libplacebo was
    /// created against) is current — so no context sharing and no sync meta are needed.
    #[cfg(target_os = "macos")]
    fn render_iosurface(
        &mut self,
        destination: &libplacebo::SwapchainFrame,
        source_buffer: &gst::Buffer,
        info: &gst_video::VideoInfo,
        mdi: &Option<MasteringDisplayInfo>,
        overlays: &[Overlay],
        rotation: Rotation,
    ) -> std::result::Result<(), RenderFrameError> {
        let mut target = unsafe {
            let mut t = std::mem::zeroed();
            pl_frame_from_swapchain(&mut t, &destination.frame);
            t
        };
        self.render_iosurface_to_frame(&mut target, source_buffer, info, mdi, overlays, rotation)
    }

    #[cfg(target_os = "macos")]
    fn render_iosurface_to_frame(
        &mut self,
        destination: &mut pl_frame,
        source_buffer: &gst::Buffer,
        info: &gst_video::VideoInfo,
        mdi: &Option<MasteringDisplayInfo>,
        overlays: &[Overlay],
        rotation: Rotation,
    ) -> std::result::Result<(), RenderFrameError> {
        use smallvec::SmallVec;

        use crate::iosurface;

        let frame_info = RenderFrameInfo::new(info);
        let n_planes = info.n_planes() as i32;

        let mut image = create_pl_frame(n_planes, info, &frame_info, mdi);
        image.rotation = rotation_to_pl(rotation);

        // GL texture guards: each backs one `image.planes[i].texture`. They must outlive the
        // render call and be dropped only after the `pl_tex` wrappers are destroyed (the wrapper
        // does not own the GL object).
        let mut plane_textures: SmallVec<[iosurface::PlaneTexture; 4]> = SmallVec::new();

        let import_result = (|| {
            for plane_idx in 0..n_planes as usize {
                if plane_idx >= source_buffer.n_memory() {
                    return Err(RenderFrameError::MissingPlane);
                }
                let mem = source_buffer.peek_memory(plane_idx);
                if !iosurface::is_iosurface_memory(mem) {
                    return Err(RenderFrameError::NotIOSurface);
                }
                let Some((surface, surface_plane)) = iosurface::peek_surface(mem) else {
                    return Err(RenderFrameError::NotIOSurface);
                };
                let Some(gl_format) = iosurface::plane_gl_format(info.format(), plane_idx) else {
                    return Err(RenderFrameError::UnsupportedPlaneFormat);
                };

                let plane_tex =
                    unsafe { iosurface::import_plane(surface, surface_plane, gl_format) }
                        .ok_or(RenderFrameError::IOSurfaceImport)?;

                let mut wrap_params: pl_opengl_wrap_params = unsafe { std::mem::zeroed() };
                wrap_params.texture = plane_tex.id;
                wrap_params.target = iosurface::PlaneTexture::TARGET;
                wrap_params.iformat = plane_tex.gl_format.pl_iformat;
                wrap_params.width = plane_tex.width;
                wrap_params.height = plane_tex.height;

                let tex = unsafe { pl_opengl_wrap(self.gpu(), &wrap_params) };
                if tex.is_null() {
                    return Err(RenderFrameError::TextureWrap);
                }
                image.planes[plane_idx].texture = tex;

                let mut components = 0;
                for comp_idx in 0..info.n_components() {
                    if info.comp_plane(comp_idx as u8) == plane_idx as u32 {
                        image.planes[plane_idx].component_mapping[components] = comp_idx as i32;
                        components += 1;
                    }
                }
                image.planes[plane_idx].components = components as i32;

                plane_textures.push(plane_tex);
            }
            Ok(())
        })();

        if let Err(err) = import_result {
            unsafe { destroy_textures(self.gpu(), n_planes, &mut image.planes) };
            return Err(err);
        }

        destination.crop =
            libplacebo::scale_and_fit(&destination.crop, &rotated_fit_rect(&image.crop, rotation));

        self.render_image_with_overlays(&mut image, destination, overlays);

        // Destroy the pl_tex wrappers first, then drop `plane_textures` (which deletes the GL
        // textures). Order matters: the wrapper references the GL object.
        unsafe { destroy_textures(self.gpu(), n_planes, &mut image.planes) };
        drop(plane_textures);

        Ok(())
    }

    pub fn render_frame(
        &mut self,
        swframe: &libplacebo::SwapchainFrame,
        frame: &crate::video::Frame,
    ) -> std::result::Result<(), RenderFrameError> {
        match &frame.data {
            crate::video::FrameData::SystemMemory { frame: v_frame } => self.render_sysmem(
                swframe,
                &v_frame,
                &frame.mastering_display_info,
                &frame.overlays,
                frame.rotation,
            ),
            #[cfg(target_os = "linux")]
            crate::video::FrameData::DmaBuf {
                buffer, dma_info, ..
            } => self.render_dmabuf(
                swframe,
                &buffer,
                &dma_info,
                &frame.mastering_display_info,
                &frame.overlays,
                frame.rotation,
            ),
            #[cfg(target_os = "macos")]
            crate::video::FrameData::IOSurface { buffer, info } => {
                match self.render_iosurface(
                    swframe,
                    buffer,
                    info,
                    &frame.mastering_display_info,
                    &frame.overlays,
                    frame.rotation,
                ) {
                    Err(err) => {
                        if !self.iosurface_fallback_warned {
                            self.iosurface_fallback_warned = true;
                            tracing::warn!(
                                %err,
                                "IOSurface zero-copy render failed; falling back to CPU readback"
                            );
                        }
                        let v_frame =
                            gst_video::VideoFrame::from_buffer_readable(buffer.clone(), info)
                                .map_err(move |_| err)?;
                        self.render_sysmem(
                            swframe,
                            &v_frame,
                            &frame.mastering_display_info,
                            &frame.overlays,
                            frame.rotation,
                        )
                    }
                    ok => ok,
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn render_frame_to_tex(
        &mut self,
        destination_tex: pl_tex,
        destination_width: i32,
        destination_height: i32,
        destination_color: pl_color_space,
        source_frame: &crate::video::Frame,
    ) -> std::result::Result<(), RenderFrameError> {
        let mut destination_frame: pl_frame = unsafe { std::mem::zeroed() };
        destination_frame.num_planes = 1;
        destination_frame.planes[0] = libplacebo::new_plane();
        destination_frame.planes[0].texture = destination_tex;
        destination_frame.planes[0].components = 4;
        destination_frame.planes[0].component_mapping = [0, 1, 2, 3];
        let depth = unsafe {
            let fmt = (*destination_tex).params.format;
            if fmt.is_null() {
                8
            } else {
                (*fmt).component_depth.iter().copied().max().unwrap_or(8)
            }
        };
        destination_frame.repr = pl_color_repr {
            sys: pl_color_system::PL_COLOR_SYSTEM_RGB,
            levels: pl_color_levels::PL_COLOR_LEVELS_FULL,
            alpha: pl_alpha_mode::PL_ALPHA_NONE,
            bits: pl_bit_encoding {
                sample_depth: depth,
                color_depth: depth,
                bit_shift: 0,
            },
            dovi: ptr::null(),
        };
        destination_frame.color = destination_color;
        destination_frame.crop = pl_rect2df {
            x0: 0.0,
            y0: 0.0,
            x1: destination_width as f32,
            y1: destination_height as f32,
        };

        match &source_frame.data {
            crate::video::FrameData::SystemMemory { frame } => self.render_sysmem_to_frame(
                &mut destination_frame,
                &frame,
                &None, /* TODO? */
                &source_frame.overlays,
                source_frame.rotation,
            ),
            crate::video::FrameData::DmaBuf {
                buffer, dma_info, ..
            } => self.render_dmabuf_to_frame(
                &mut destination_frame,
                &buffer,
                &dma_info,
                &None, /* TODO? */
                &source_frame.overlays,
                source_frame.rotation,
            ),
        }
    }

    pub fn gpu(&self) -> *const pl_gpu_t {
        unsafe {
            match &self.backend {
                Backend::OpenGL { opengl, .. } => opengl.gpu(),
                #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
                Backend::Vulkan(vulkan) => vulkan.gpu(),
            }
        }
    }
}

fn rotation_to_pl(rotation: Rotation) -> pl_rotation {
    (match rotation {
        Rotation::Rotate0 => PL_ROTATION_0,
        Rotation::Rotate90 => PL_ROTATION_90,
        Rotation::Rotate180 => PL_ROTATION_180,
        Rotation::Rotate270 => PL_ROTATION_270,
    }) as pl_rotation
}

fn rotated_fit_rect(crop: &pl_rect2df, rotation: Rotation) -> pl_rect2df {
    match rotation {
        Rotation::Rotate90 | Rotation::Rotate270 => pl_rect2df {
            x0: 0.0,
            y0: 0.0,
            x1: crop.y1 - crop.y0,
            y1: crop.x1 - crop.x0,
        },
        Rotation::Rotate0 | Rotation::Rotate180 => *crop,
    }
}

fn overlay_color_space() -> pl_color_space {
    let mut color: pl_color_space = unsafe { std::mem::zeroed() };
    color.primaries = pl_color_primaries::PL_COLOR_PRIM_BT_709;
    color.transfer = pl_color_transfer::PL_COLOR_TRC_SRGB;
    color
}

fn build_render_params(opts: &RenderingOptions) -> pl_render_params {
    let mut params = unsafe {
        match opts.profile {
            RenderProfile::Fast => pl_render_fast_params,
            RenderProfile::Balanced => pl_render_default_params,
            RenderProfile::HighQuality => pl_render_high_quality_params,
        }
    };

    let color_map_params = {
        let mut params = unsafe { *params.color_map_params };
        params.visualize_lut = opts.visualize_lut;
        params.show_clipping = opts.show_clipping;
        let boxed = Box::new(params);
        Box::leak(boxed)
    };

    params.color_map_params = color_map_params;
    params
}

impl Drop for PlaceboContext {
    fn drop(&mut self) {
        unsafe {
            self.flush_texture_cache();
            ManuallyDrop::drop(&mut self.renderer);
            match &mut self.backend {
                Backend::OpenGL { opengl, swapchain } => {
                    ManuallyDrop::drop(swapchain);
                    ManuallyDrop::drop(opengl);
                }
                #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
                Backend::Vulkan(vulkan) => ManuallyDrop::drop(vulkan),
            }
            let _ =
                Box::from_raw(self.rendering_params.color_map_params as *mut pl_color_map_params);
        }
    }
}
