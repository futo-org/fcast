#[cfg(target_os = "linux")]
use std::ffi::c_void;
use std::{mem::ManuallyDrop, ptr};

use anyhow::anyhow;
#[cfg(target_os = "linux")]
use drm_fourcc::DrmFourcc;
use gst_video::prelude::*;
use libplacebo::{OpenGL, Renderer, Swapchain, SwapchainFrame, libplacebo_sys::*};
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum RenderProfile {
    Fast,
    Default,
    HighQuality,
}

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
        gst_video::VideoColorMatrix::Bt2020 => pl_color_system::PL_COLOR_SYSTEM_BT_2020_C, // _NC?
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

#[cfg(target_os = "linux")]
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
        let sample_depth = match info.comp_depth(0) {
            8 => 8,
            10 | 12 | 16 => 16,
            _ => unreachable!(),
        };
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
    #[error("frame is missing a plane")]
    MissingPlane,
    #[error("failed to upload plane")]
    PlaneUploadFailed,
}

fn create_pl_frame(
    num_planes: i32,
    info: &gst_video::VideoInfo,
    frame_info: &RenderFrameInfo,
) -> pl_frame {
    let mut frame: pl_frame = unsafe { std::mem::zeroed() };
    frame.num_planes = num_planes;
    frame.repr = frame_info.color_repr;
    frame.color.primaries = frame_info.primaries;
    frame.color.transfer = frame_info.transfer;
    frame.crop = pl_rect2df {
        x0: 0.0,
        y0: 0.0,
        x1: info.width() as f32,
        y1: info.height() as f32,
    };
    frame
}

pub struct PlaceboContext {
    opengl: ManuallyDrop<OpenGL>,
    swapchain: ManuallyDrop<Swapchain>,
    renderer: ManuallyDrop<Renderer>,
    // Textures used for system memory buffers
    cached_textures: [pl_tex; 4],
    rendering_params: pl_render_params,
}

impl PlaceboContext {
    #[cfg(not(target_os = "linux"))]
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

        let mut params = unsafe {
            match opts.profile {
                RenderProfile::Fast => pl_render_fast_params.clone(),
                RenderProfile::Default => pl_render_default_params.clone(),
                RenderProfile::HighQuality => pl_render_high_quality_params.clone(),
            }
        };

        let color_map_params = {
            let mut params = unsafe { (*params.color_map_params).clone() };
            params.visualize_lut = opts.visualize_lut;
            params.show_clipping = opts.show_clipping;
            let boxed = Box::new(params);
            Box::leak(boxed)
        };

        params.color_map_params = color_map_params;

        Ok(Self {
            opengl: ManuallyDrop::new(opengl),
            swapchain: ManuallyDrop::new(swapchain),
            renderer: ManuallyDrop::new(renderer),
            cached_textures: [std::ptr::null(); 4],
            rendering_params: params,
        })
    }

    pub fn resize_swapchain(&self, width: i32, height: i32) {
        self.swapchain.resize(width, height);
    }

    fn flush_texture_cache(&mut self) {
        for i in 0..self.cached_textures.len() {
            if !self.cached_textures[i].is_null() {
                unsafe {
                    pl_tex_destroy(self.opengl.gpu(), &mut self.cached_textures[i]);
                    assert!(self.cached_textures[i].is_null());
                }
            }
        }
    }

    pub fn flush_cache(&mut self) {
        self.renderer.flush_cache();
        self.flush_texture_cache();
        debug!("Flushed cache");
    }

    pub fn start_frame(&self) -> Option<SwapchainFrame> {
        self.swapchain.start_frame()
    }

    #[tracing::instrument(skip_all)]
    pub fn submit_frame(&self) {
        if !self.swapchain.submit_frame() {
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

            let mut components = 0;
            for comp_idx in 0..frame.n_components() {
                if info.comp_plane(comp_idx as u8) == plane_idx as u32 {
                    plane_data.component_map[components] = comp_idx as i32;
                    plane_data.component_size[components] = frame_info.sample_depth;
                    components += 1;
                }
            }

            unsafe {
                if !pl_upload_plane(
                    self.opengl.gpu(),
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

    fn render_sysmem(
        &mut self,
        swframe: &libplacebo::SwapchainFrame,
        frame: &gst_video::VideoFrame<gst_video::video_frame::Readable>,
    ) -> std::result::Result<(), RenderFrameError> {
        let info = frame.info();
        let frame_info = RenderFrameInfo::new(info);

        let mut image = create_pl_frame(frame.n_planes() as i32, info, &frame_info);
        if let Err(err) = self.upload_sys_mem(info, &frame_info, frame, &mut image) {
            self.flush_texture_cache();
            return Err(err);
        };

        let mut target = unsafe {
            let mut t = std::mem::zeroed();
            pl_frame_from_swapchain(&mut t, &swframe.frame);
            t
        };

        target.crop = libplacebo::scale_and_fit(&target.crop, &image.crop);

        unsafe {
            pl_render_image(
                self.renderer.renderer,
                &image,
                &target,
                &self.rendering_params,
            );
        }

        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn render_dmabuf(
        &self,
        swframe: &libplacebo::SwapchainFrame,
        buffer: &gst::Buffer,
        dma_info: &gst_video::VideoInfoDmaDrm,
    ) -> std::result::Result<(), RenderFrameError> {
        use tracing::error;

        let Some(vmeta) = buffer.meta::<gst_video::VideoMeta>() else {
            return Err(RenderFrameError::MissingVideoMeta);
        };

        let mut fds = [-1i32; 4];
        let mut offsets = [0; 4];
        let mut strides = [0; 4];
        let mut sizes = [0usize; 4];
        let n_planes = vmeta.n_planes() as usize;
        let dma_drm_fourcc =
            DrmFourcc::try_from(dma_info.fourcc()).map_err(|_| RenderFrameError::InvalidFourcc)?;
        let modifier = dma_info.modifier();

        let vmeta_offsets = vmeta.offset();
        let vmeta_strides = vmeta.stride();

        for plane in 0..n_planes {
            let Some((range, skip)) =
                buffer.find_memory(vmeta_offsets[plane]..(vmeta_offsets[plane] + 1))
            else {
                break;
            };

            let mem = buffer.peek_memory(range.start);
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

        let normal_info = dma_info
            .to_video_info()
            .map_err(|_| RenderFrameError::InvalidFormatInfo)?;
        let frame_info = RenderFrameInfo::new(&normal_info);

        let mut image = create_pl_frame(n_planes as i32, &normal_info, &frame_info);
        for plane_idx in 0..image.num_planes {
            let fmt_fourcc = crate::dmabuf::fourcc_from_plane(plane_idx, dma_drm_fourcc);
            let fmt = unsafe {
                libplacebo::libplacebo_sys::pl_find_fourcc(self.opengl.gpu(), fmt_fourcc as u32)
            };
            if fmt.is_null() {
                error!(?fmt_fourcc, "Plane has unsupported fourcc");
                unsafe { destroy_textures(self.opengl.gpu(), image.num_planes, &mut image.planes) };
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

            let tex = unsafe { pl_tex_create(self.opengl.gpu(), &tex_params) };
            if tex.is_null() {
                unsafe {
                    destroy_textures(self.opengl.gpu(), plane_idx + 1, &mut image.planes);
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

        let mut target = unsafe {
            let mut t = std::mem::zeroed();
            pl_frame_from_swapchain(&mut t, &swframe.frame);
            t
        };

        target.crop = libplacebo::scale_and_fit(&target.crop, &image.crop);

        unsafe {
            pl_render_image(
                self.renderer.renderer,
                &image,
                &target,
                &self.rendering_params,
            );
            destroy_textures(self.opengl.gpu(), image.num_planes, &mut image.planes);
        }

        Ok(())
    }

    pub fn render_frame(
        &mut self,
        swframe: &libplacebo::SwapchainFrame,
        frame: &crate::video::RawFrame,
    ) -> std::result::Result<(), RenderFrameError> {
        match frame {
            crate::video::RawFrame::SystemMemory { frame } => self.render_sysmem(swframe, frame),
            #[cfg(target_os = "linux")]
            crate::video::RawFrame::DmaBuf {
                buffer, dma_info, ..
            } => self.render_dmabuf(swframe, buffer, dma_info),
            #[cfg(target_os = "macos")]
            crate::video::RawFrame::Gl { .. } => Ok(()),
        }
    }
}

impl Drop for PlaceboContext {
    fn drop(&mut self) {
        unsafe {
            self.flush_texture_cache();
            ManuallyDrop::drop(&mut self.renderer);
            ManuallyDrop::drop(&mut self.swapchain);
            ManuallyDrop::drop(&mut self.opengl);
            let _ =
                Box::from_raw(self.rendering_params.color_map_params as *mut pl_color_map_params);
        }
    }
}
