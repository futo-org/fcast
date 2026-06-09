use std::{
    ffi::{CString, c_int, c_uint},
    fs::OpenOptions,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
};

use anyhow::{Result, anyhow};
use fiatlux::*;
use rcore::{
    VideoSink, glow,
    gst_video::{self, prelude::*},
    libplacebo::libplacebo_sys::*,
    placebo::PlaceboContext,
    tracing::{debug, warn},
    video::{RawFrame, RawFrameData},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetFormat {
    Rgba8,
    Rgba16F,
}

impl TargetFormat {
    fn pl_name(self) -> &'static str {
        match self {
            TargetFormat::Rgba8 => "rgba8",
            TargetFormat::Rgba16F => "rgba16hf",
        }
    }
}

struct Target {
    tex: pl_tex,
    bo: *mut gbm_bo,
    pixmap_id: fl_protocol_PixmapId,
    width: u32,
    height: u32,
    format: TargetFormat,
}

pub struct FhsPixmapSink {
    client: *mut fl_Client,
    target: Option<Target>,
    gbm: Option<GbmAllocator>,
    hdr_metadata: Option<fl_protocol_HdrMetadata>,
    surface_id: fl_protocol_SurfaceId,
    surface_has_hdr_metadata: bool,
}

impl FhsPixmapSink {
    pub fn new(client: *mut fl_Client, surface_id: fl_protocol_SurfaceId) -> Self {
        Self {
            client,
            target: None,
            gbm: None,
            hdr_metadata: None,
            surface_id,
            surface_has_hdr_metadata: false,
        }
    }

    fn ensure_target(
        &mut self,
        pl_ctx: &PlaceboContext,
        width: u32,
        height: u32,
        format: TargetFormat,
    ) -> Result<()> {
        if let Some(t) = &self.target
            && t.width == width
            && t.height == height
            && t.format == format
        {
            return Ok(());
        }

        self.destroy_target(pl_ctx);

        let gpu = pl_ctx.gpu();
        unsafe {
            if (*gpu).import_caps.tex & pl_handle_type_PL_HANDLE_DMA_BUF as u64 == 0 {
                return Err(anyhow!(
                    "libplacebo GPU does not support DMA-BUF tex import"
                ));
            }
        }

        let fmt_name = CString::new(format.pl_name())?;
        let fmt = unsafe { pl_find_named_fmt(gpu, fmt_name.as_ptr()) };
        if fmt.is_null() {
            return Err(anyhow!(
                "libplacebo has no named format '{}'",
                format.pl_name()
            ));
        }

        let fmt_caps = unsafe { (*fmt).caps as u32 };
        if fmt_caps & pl_fmt_caps::PL_FMT_CAP_RENDERABLE as u32 == 0 {
            return Err(anyhow!(
                "target format '{}' not renderable",
                format.pl_name()
            ));
        }

        let fourcc = unsafe { (*fmt).fourcc };
        if fourcc == 0 {
            return Err(anyhow!(
                "target format '{}' has no DRM fourcc",
                format.pl_name()
            ));
        }

        let modifiers = rcore::egl::get_importable_modifiers(fourcc);
        if modifiers.is_empty() {
            return Err(anyhow!(
                "no importable (non external_only) modifiers for fourcc {fourcc:#010x}"
            ));
        }

        if self.gbm.is_none() {
            self.gbm = Some(GbmAllocator::new(self.client)?);
        }
        let bo = self
            .gbm
            .as_ref()
            .unwrap()
            .create_bo(width, height, fourcc, &modifiers)?;

        match self.import_and_register(pl_ctx, bo, fmt, fourcc, width, height, format) {
            Ok(target) => {
                self.target = Some(target);
                self.hdr_metadata = None;
                Ok(())
            }
            Err(err) => {
                unsafe { gbm_bo_destroy(bo) };
                Err(err)
            }
        }
    }

    fn import_and_register(
        &self,
        pl_ctx: &PlaceboContext,
        bo: *mut gbm_bo,
        fmt: pl_fmt,
        fourcc: u32,
        width: u32,
        height: u32,
        format: TargetFormat,
    ) -> Result<Target> {
        let gpu = pl_ctx.gpu();

        let stride = unsafe { gbm_bo_get_stride(bo) };
        let offset = unsafe { gbm_bo_get_offset(bo, 0) };
        let modifier = unsafe { gbm_bo_get_modifier(bo) };
        // libplacebo dup's this on import, and the fiatlux client closes it after sending
        let fd = unsafe { gbm_bo_get_fd(bo) };
        if fd < 0 {
            return Err(anyhow!("gbm_bo_get_fd failed"));
        }

        let mut shared_mem: pl_shared_mem = unsafe { std::mem::zeroed() };
        shared_mem.handle.fd = fd;
        shared_mem.size = stride as usize * height as usize;
        shared_mem.offset = offset as usize;
        shared_mem.drm_format_mod = modifier;
        shared_mem.stride_w = stride as usize;

        let tex_params = pl_tex_params {
            w: width as i32,
            h: height as i32,
            d: 0,
            format: fmt,
            sampleable: true,
            renderable: true,
            storable: false,
            blit_src: false,
            blit_dst: false,
            host_writable: false,
            host_readable: false,
            export_handle: 0,
            import_handle: pl_handle_type_PL_HANDLE_DMA_BUF,
            shared_mem,
            initial_data: ptr::null(),
            user_data: ptr::null_mut(),
            debug_tag: ptr::null(),
        };

        let mut tex: pl_tex = unsafe { pl_tex_create(gpu, &tex_params) };
        if tex.is_null() {
            unsafe { drop(OwnedFd::from_raw_fd(fd)) };
            return Err(anyhow!("pl_tex_create (dma-buf import) failed"));
        }

        let offsets = [offset, 0u32, 0u32, 0u32];
        let pitches = [stride, 0u32, 0u32, 0u32];
        let modifiers = [modifier, 0u64, 0u64, 0u64];
        let fds = [fd];

        let pixmap_id = unsafe {
            let seq = fl_create_pixmap_from_dmabuf(
                self.client,
                width,
                height,
                fourcc,
                1,
                offsets.as_ptr(),
                pitches.as_ptr(),
                modifiers.as_ptr(),
                1,
                fds.as_ptr(),
            );
            if seq.value == 0 {
                // The client only closes the fd once it has been sent; on a send
                // failure we still own it.
                drop(OwnedFd::from_raw_fd(fd));
                pl_tex_destroy(gpu, &mut tex);
                return Err(anyhow!("fl_create_pixmap_from_dmabuf failed"));
            }

            let mut reply: fl_reply_CreatePixmapFromDmaBuf = std::mem::zeroed();
            if !fl_receive_reply_create_pixmap_from_dma_buf(self.client, seq, &mut reply) {
                pl_tex_destroy(gpu, &mut tex);
                return Err(anyhow!("fl_create_pixmap_from_dmabuf reply was null"));
            }

            reply.pixmap_id
        };

        debug!(
            width,
            height,
            ?format,
            fourcc,
            modifier,
            "Created pixmap-backed render target (gbm import)"
        );

        unsafe {
            fl_discard_reply(
                self.client,
                fl_set_surface_pixmap(self.client, self.surface_id, pixmap_id).value,
            );
        }

        Ok(Target {
            tex,
            bo,
            pixmap_id,
            width,
            height,
            format,
        })
    }

    fn destroy_target(&mut self, pl_ctx: &PlaceboContext) {
        let Some(target) = self.target.take() else {
            return;
        };
        unsafe {
            fl_discard_reply(
                self.client,
                fl_destroy_pixmap(self.client, target.pixmap_id).value,
            );
            let mut tex = target.tex;
            pl_tex_destroy(pl_ctx.gpu(), &mut tex);
            gbm_bo_destroy(target.bo);
        }
    }
}

impl VideoSink for FhsPixmapSink {
    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &RawFrame,
        target_size: (u32, u32),
    ) -> Result<()> {
        let (target_width, target_height) = target_size;
        let colorimetry = frame_video_colorimetry(frame);
        let transfer = colorimetry.transfer();
        let is_pq = matches!(transfer, gst_video::VideoTransferFunction::Smpte2084);
        let is_hlg = matches!(transfer, gst_video::VideoTransferFunction::AribStdB67);
        let is_hdr = is_pq || is_hlg;

        let format = if is_hdr {
            TargetFormat::Rgba16F
        } else {
            TargetFormat::Rgba8
        };

        self.ensure_target(placebo, target_width, target_height, format)?;
        let target = self.target.as_ref().expect("target exists after ensure");

        let (target_color, new_hdr_metadata) = build_target_color(frame, is_pq, is_hlg);

        placebo
            .render_frame_to_tex(
                target.tex,
                target.width as i32,
                target.height as i32,
                target_color,
                frame,
            )
            .map_err(|err| anyhow!("placebo render failed: {err}"))?;

        unsafe { pl_gpu_finish(placebo.gpu()) };

        if is_hdr {
            let need_update = self
                .hdr_metadata
                .as_ref()
                .map(|prev| !hdr_metadata_equal(prev, &new_hdr_metadata))
                .unwrap_or(true);
            if need_update {
                debug!(
                    transfer_function = new_hdr_metadata.transfer_function,
                    primaries = new_hdr_metadata.primaries,
                    max_mastering = new_hdr_metadata.max_mastering_luminance,
                    min_mastering = new_hdr_metadata.min_mastering_luminance,
                    max_cll = new_hdr_metadata.max_cll,
                    max_fall = new_hdr_metadata.max_fall,
                    "Sending HDR pixmap metadata to display server"
                );
                unsafe {
                    fl_discard_reply(
                        self.client,
                        fl_set_surface_hdr_metadata(
                            self.client,
                            self.surface_id,
                            &new_hdr_metadata,
                        )
                        .value,
                    );
                }
                self.hdr_metadata = Some(new_hdr_metadata);
                self.surface_has_hdr_metadata = true;
            }
        } else {
            if self.surface_has_hdr_metadata {
                unsafe {
                    fl_discard_reply(
                        self.client,
                        fl_set_surface_hdr_metadata(self.client, self.surface_id, std::ptr::null())
                            .value,
                    );
                }
            }
            self.hdr_metadata = None;
            self.surface_has_hdr_metadata = false;
        }

        // libplacebo leaves its own FBO + viewport set after the dmabuf render.
        // Reset framebuffer to the default framebuffer which slint expects.
        unsafe {
            use glow::HasContext;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, target_width as i32, target_height as i32);
        }

        Ok(())
    }

    fn get_clear_color(&self) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn teardown(&mut self, placebo: &mut PlaceboContext) {
        self.destroy_target(placebo);
    }
}

const GBM_BO_USE_RENDERING: u32 = 1 << 2;

#[repr(C)]
struct gbm_device {
    _opaque: [u8; 0],
}

#[repr(C)]
struct gbm_bo {
    _opaque: [u8; 0],
}

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: c_int) -> *mut gbm_device;
    fn gbm_device_destroy(gbm: *mut gbm_device);
    fn gbm_bo_create_with_modifiers2(
        gbm: *mut gbm_device,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: c_uint,
        flags: u32,
    ) -> *mut gbm_bo;
    fn gbm_bo_destroy(bo: *mut gbm_bo);
    fn gbm_bo_get_fd(bo: *mut gbm_bo) -> c_int;
    fn gbm_bo_get_modifier(bo: *mut gbm_bo) -> u64;
    fn gbm_bo_get_stride(bo: *mut gbm_bo) -> u32;
    fn gbm_bo_get_offset(bo: *mut gbm_bo, plane: c_int) -> u32;
}

struct GbmAllocator {
    _drm_fd: OwnedFd,
    device: *mut gbm_device,
}

impl GbmAllocator {
    fn new(client: *mut fl_Client) -> Result<Self> {
        let render_device_path_resp = unsafe {
            fl_receive_reply_dri_get_render_device_path(
                client,
                fl_dri_get_render_device_path(client),
            )
        };
        let render_device_path = if render_device_path_resp.is_null() {
            warn!(
                "Failed to get render device path from the fiatlux server, falling back to /dev/dri/renderD128"
            );
            "/dev/dri/renderD128".to_string()
        } else {
            unsafe {
                let path = (*render_device_path_resp).render_device_path;
                String::from_raw_parts(path.ptr, path.len as usize, path.len as usize)
            }
        };
        unsafe {
            fl_free_reply_dri_get_render_device_path(render_device_path_resp);
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&render_device_path)
            .map_err(|e| anyhow!("failed to open DRM render node {render_device_path}: {e}"))?;
        let drm_fd = OwnedFd::from(file);

        let device = unsafe { gbm_create_device(drm_fd.as_raw_fd()) };
        if device.is_null() {
            return Err(anyhow!("gbm_create_device failed for {render_device_path}"));
        }

        Ok(Self {
            _drm_fd: drm_fd,
            device,
        })
    }

    fn create_bo(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
    ) -> Result<*mut gbm_bo> {
        let bo = unsafe {
            gbm_bo_create_with_modifiers2(
                self.device,
                width,
                height,
                fourcc,
                modifiers.as_ptr(),
                modifiers.len() as c_uint,
                GBM_BO_USE_RENDERING,
            )
        };
        if bo.is_null() {
            return Err(anyhow!(
                "gbm_bo_create_with_modifiers2 failed for {width}x{height} fourcc {fourcc:#010x}"
            ));
        }
        Ok(bo)
    }
}

impl Drop for GbmAllocator {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.device) };
    }
}

fn frame_video_colorimetry(frame: &RawFrame) -> gst_video::VideoColorimetry {
    match &frame.data {
        RawFrameData::SystemMemory { frame } => frame.info().colorimetry(),
        RawFrameData::DmaBuf { dma_info, .. } => dma_info.colorimetry(),
    }
}

fn build_target_color(
    frame: &RawFrame,
    is_pq: bool,
    is_hlg: bool,
) -> (pl_color_space, fl_protocol_HdrMetadata) {
    let is_hdr = is_pq || is_hlg;

    let pl_primaries = if is_hdr {
        pl_color_primaries::PL_COLOR_PRIM_BT_2020
    } else {
        pl_color_primaries::PL_COLOR_PRIM_BT_709
    };

    let pl_transfer = if is_pq {
        pl_color_transfer::PL_COLOR_TRC_PQ
    } else if is_hlg {
        pl_color_transfer::PL_COLOR_TRC_HLG
    } else {
        pl_color_transfer::PL_COLOR_TRC_SRGB
    };

    let color_space = pl_color_space {
        primaries: pl_primaries,
        transfer: pl_transfer,
        // Keep default values for hdr so the renderer treats luminance as unknown
        // and preserves the soruce dynamic range
        hdr: unsafe { std::mem::zeroed() },
    };

    let mut hdr_metadata: fl_protocol_HdrMetadata = unsafe { std::mem::zeroed() };
    hdr_metadata.transfer_function = if is_pq {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_pq
    } else if is_hlg {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_hlg
    } else {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_srgb
    } as u8;

    hdr_metadata.primaries = if is_hdr {
        fl_protocol_Primaries_fl_protocol_Primaries_bt_2020
    } else {
        fl_protocol_Primaries_fl_protocol_Primaries_bt_709
    } as u8;

    if is_hdr {
        let mut have_valid_mastering = false;

        if let Some(mdi) = &frame.mastering_display_info {
            let primaries = &mdi.display_primaries;
            let wp = &mdi.white_point;
            let max_cd = mdi.max_display_mastering_luminance as f32 / 10000.0;
            let min_cd = mdi.min_display_mastering_luminance as f32 / 10000.0;

            if max_cd >= 100.0 {
                hdr_metadata.mastering_display_primaries[0] = fl_protocol_XyColor {
                    x: primaries[0].x,
                    y: primaries[0].y,
                };
                hdr_metadata.mastering_display_primaries[1] = fl_protocol_XyColor {
                    x: primaries[1].x,
                    y: primaries[1].y,
                };
                hdr_metadata.mastering_display_primaries[2] = fl_protocol_XyColor {
                    x: primaries[2].x,
                    y: primaries[2].y,
                };
                hdr_metadata.mastering_display_white_point =
                    fl_protocol_XyColor { x: wp.x, y: wp.y };
                hdr_metadata.max_mastering_luminance = max_cd;
                hdr_metadata.min_mastering_luminance = min_cd;
                have_valid_mastering = true;
            } else {
                debug!(
                    raw_max = mdi.max_display_mastering_luminance,
                    max_cd, "Ignoring implausible mastering-display max luminance"
                );
            }
        }

        if let Some(cll) = &frame.content_light_level {
            hdr_metadata.max_cll = cll.max_content_light_level as f32;
            hdr_metadata.max_fall = cll.max_frame_average_light_level as f32;
        }

        if !have_valid_mastering {
            hdr_metadata.max_mastering_luminance = 1000.0;
            hdr_metadata.min_mastering_luminance = 0.005;
        }
    }

    (color_space, hdr_metadata)
}

fn hdr_metadata_equal(a: &fl_protocol_HdrMetadata, b: &fl_protocol_HdrMetadata) -> bool {
    a.transfer_function == b.transfer_function
        && a.primaries == b.primaries
        && a.max_mastering_luminance == b.max_mastering_luminance
        && a.min_mastering_luminance == b.min_mastering_luminance
        && a.max_cll == b.max_cll
        && a.max_fall == b.max_fall
        && xy_eq(
            &a.mastering_display_white_point,
            &b.mastering_display_white_point,
        )
        && (0..3).all(|i| {
            xy_eq(
                &a.mastering_display_primaries[i],
                &b.mastering_display_primaries[i],
            )
        })
}

fn xy_eq(a: &fl_protocol_XyColor, b: &fl_protocol_XyColor) -> bool {
    a.x == b.x && a.y == b.y
}
