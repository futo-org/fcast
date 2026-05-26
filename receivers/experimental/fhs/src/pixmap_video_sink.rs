use std::{
    ffi::CString,
    os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use anyhow::{Result, anyhow};
use fiatlux::*;
use rcore::{
    VideoSink, glow, gst_video::{self, prelude::*},
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
    pixmap_id: fl_protocol_PixmapId,
    width: u32,
    height: u32,
    format: TargetFormat,
}

pub struct FhsPixmapSink {
    client: *mut fl_Client,
    target: Option<Target>,
    hdr_metadata: Option<fl_protocol_HdrMetadata>,
    shared_pixmap_id: Arc<AtomicU32>,
}

impl FhsPixmapSink {
    pub fn new(client: *mut fl_Client, shared_pixmap_id: Arc<AtomicU32>) -> Self {
        Self {
            client,
            target: None,
            hdr_metadata: None,
            shared_pixmap_id,
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
            if (*gpu).export_caps.tex & pl_handle_type_PL_HANDLE_DMA_BUF as u64 == 0 {
                return Err(anyhow!(
                    "libplacebo GPU does not support DMA-BUF tex export"
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

        let tex_params = pl_tex_params {
            w: width as i32,
            h: height as i32,
            d: 0,
            format: fmt,
            sampleable: true,
            renderable: true,
            storable: false,
            blit_src: fmt_caps & pl_fmt_caps::PL_FMT_CAP_BLITTABLE as u32 != 0,
            blit_dst: fmt_caps & pl_fmt_caps::PL_FMT_CAP_BLITTABLE as u32 != 0,
            host_writable: false,
            host_readable: false,
            export_handle: pl_handle_type_PL_HANDLE_DMA_BUF,
            import_handle: 0,
            shared_mem: unsafe { std::mem::zeroed() },
            initial_data: ptr::null(),
            user_data: ptr::null_mut(),
            debug_tag: ptr::null(),
        };

        let mut tex: pl_tex = unsafe { pl_tex_create(gpu, &tex_params) };
        if tex.is_null() {
            return Err(anyhow!("pl_tex_create (dma-buf export) failed"));
        }

        let result = (|| -> Result<Target> {
            let shared_mem = unsafe { (*tex).shared_mem };
            let raw_fd = unsafe { shared_mem.handle.fd };
            if raw_fd < 0 {
                return Err(anyhow!("exported texture has invalid fd"));
            }

            // Duplicate the dmabuf file descriptor because the fiatlux client
            // closes it after it has been sent
            let dup_fd = unsafe { BorrowedFd::borrow_raw(raw_fd) }
                .try_clone_to_owned()
                .map_err(|e| anyhow!("dup of exported dma-buf fd failed: {e}"))?
                .into_raw_fd();

            let offset = shared_mem.offset as u32;
            let stride_w = if shared_mem.stride_w != 0 {
                shared_mem.stride_w as u32
            } else {
                width * bytes_per_pixel(format) as u32
            };
            let modifier = shared_mem.drm_format_mod;

            let offsets = [offset, 0u32, 0u32, 0u32];
            let pitches = [stride_w, 0u32, 0u32, 0u32];
            let modifiers = [modifier, 0u64, 0u64, 0u64];
            let fds = [dup_fd];

            let pixmap_id = unsafe {
                let seq = fl_create_pixmap_from_dmabuf(
                    self.client,
                    width,
                    height,
                    fl_protocol_ContentType_fl_protocol_ContentType_video,
                    fourcc,
                    1,
                    offsets.as_ptr(),
                    pitches.as_ptr(),
                    modifiers.as_ptr(),
                    1,
                    fds.as_ptr(),
                );
                if seq.value == 0 {
                    drop(OwnedFd::from_raw_fd(dup_fd));
                    return Err(anyhow!("fl_create_pixmap_from_dmabuf failed"));
                }

                let reply = fl_receive_reply_create_pixmap_from_dma_buf(self.client, seq);
                if reply.is_null() {
                    return Err(anyhow!("fl_create_pixmap_from_dmabuf reply was null"));
                }

                let pixmap_id = (*reply).pixmap_id;
                fl_free_reply_create_pixmap_from_dma_buf(reply);
                pixmap_id
            };

            debug!(
                width,
                height,
                ?format,
                fourcc,
                modifier,
                "Created pixmap-backed render target"
            );

            Ok(Target {
                tex,
                pixmap_id,
                width,
                height,
                format,
            })
        })();

        match result {
            Ok(target) => {
                self.shared_pixmap_id
                    .store(target.pixmap_id.value, Ordering::Release);
                self.target = Some(target);
                self.hdr_metadata = None;
                Ok(())
            }
            Err(err) => {
                unsafe { pl_tex_destroy(gpu, &mut tex) };
                Err(err)
            }
        }
    }

    fn destroy_target(&mut self, pl_ctx: &PlaceboContext) {
        let Some(target) = self.target.take() else {
            return;
        };
        self.shared_pixmap_id.store(0, Ordering::Release);
        unsafe {
            let seq = fl_destroy_pixmap(self.client, target.pixmap_id);
            if seq.value != 0 {
                let reply = fl_receive_reply_destroy_pixmap(self.client, seq);
                if !reply.is_null() {
                    fl_free_reply_destroy_pixmap(reply);
                }
            }
            let mut tex = target.tex;
            pl_tex_destroy(pl_ctx.gpu(), &mut tex);
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

        let pixmap_id = target.pixmap_id;

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
                    let seq = fl_set_pixmap_hdr_metadata(self.client, pixmap_id, &new_hdr_metadata);
                    if seq.value == 0 {
                        warn!("fl_set_pixmap_hdr_metadata failed");
                    } else {
                        let reply = fl_receive_reply_set_pixmap_hdr_metadata(self.client, seq);
                        if reply.is_null() {
                            warn!("fl_receive_reply_set_pixmap_hdr_metadata returned null");
                        } else {
                            fl_free_reply_set_pixmap_hdr_metadata(reply);
                        }
                    }
                }
                self.hdr_metadata = Some(new_hdr_metadata);
            }
        } else {
            self.hdr_metadata = None;
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

fn bytes_per_pixel(format: TargetFormat) -> u8 {
    match format {
        TargetFormat::Rgba8 => 4,
        TargetFormat::Rgba16F => 8,
    }
}

fn frame_video_colorimetry(frame: &RawFrame) -> gst_video::VideoColorimetry {
    match &frame.data {
        RawFrameData::SystemMemory { frame } => frame.info().colorimetry(),
        RawFrameData::DmaBuf { dma_info, .. } => dma_info.colorimetry()
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
        // Keep default values for hdr soe the renderer treats luminance as unknown
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
                hdr_metadata.mastering_display_white_point = fl_protocol_XyColor {
                    x: wp.x,
                    y: wp.y,
                };
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
