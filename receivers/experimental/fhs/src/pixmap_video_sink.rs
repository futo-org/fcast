//! Fiatlux pixmap-based video sink for the fhs receiver.
//!
//! Renders the decoded video frame into a libplacebo-managed pl_tex that's
//! exported as a DMA-BUF. That DMA-BUF is wrapped in a fiatlux pixmap and
//! presented separately from the slint UI by the fhs main loop. HDR videos
//! use an RGBA16F (half-float) target and carry HDR metadata via
//! `fl_set_pixmap_hdr_metadata`.

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
use rcore::gst;
use rcore::gst_video::prelude::*;
use rcore::libplacebo::libplacebo_sys::*;
use rcore::placebo::PlaceboContext;
use rcore::tracing::{debug, warn};
use rcore::video::RawFrame;
use rcore::{VideoSink, gst_video, glow};

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

/// [`VideoSink`] implementation that hands rendered video frames to the
/// fiatlux compositor as a separate pixmap. Constructor takes:
///
/// - a `fl_Client *` (the fiatlux client connection).
/// - the `fl_protocol_WindowId` to present onto.
/// - a shared `Arc<AtomicU32>` that the main loop reads to know the current
///   video `pixmap_id` (0 when no target is live).
pub struct FhsPixmapSink {
    client: *mut fl_Client,
    target: Option<Target>,
    last_hdr: Option<fl_protocol_HdrMetadata>,
    shared_pixmap_id: Arc<AtomicU32>,
}

impl FhsPixmapSink {
    pub fn new(client: *mut fl_Client, shared_pixmap_id: Arc<AtomicU32>) -> Self {
        Self {
            client,
            target: None,
            last_hdr: None,
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

        let gpu = unsafe { pl_ctx.gpu() };
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
            // We need a dup of libplacebo's fd because the fiatlux client
            // transport closes whatever fd we hand to
            // `fl_create_pixmap_from_dmabuf` after sending it via SCM_RIGHTS.
            // Take ownership as a raw fd and *do not* keep an `OwnedFd`
            // around — fiatlux owns it from the moment we pass it in.
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
                    // The request never reached the transport, so it never
                    // had a chance to close our fd. Reclaim ownership and
                    // let `OwnedFd::drop` close it.
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
                self.last_hdr = None;
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
        source_caps: Option<&gst::CapsRef>,
    ) -> Result<()> {
        let (target_width, target_height) = target_size;
        let info = frame_video_info(frame)?;
        let transfer = info.colorimetry().transfer();
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

        let (target_color, fl_hdr) = build_target_color(source_caps, is_pq, is_hlg);

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
                .last_hdr
                .as_ref()
                .map(|prev| !hdr_metadata_equal(prev, &fl_hdr))
                .unwrap_or(true);
            if need_update {
                unsafe {
                    let seq = fl_set_pixmap_hdr_metadata(self.client, pixmap_id, &fl_hdr);
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
                self.last_hdr = Some(fl_hdr);
            }
        } else {
            self.last_hdr = None;
        }

        // libplacebo leaves its own FBO + viewport set after the dmabuf
        // render; slint/femtovg expects the window framebuffer bound when
        // we return.
        unsafe {
            use glow::HasContext;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, target_width as i32, target_height as i32);
        }

        Ok(())
    }

    fn wants_transparent_clear(&self) -> bool {
        true
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

fn frame_video_info(frame: &RawFrame) -> Result<gst_video::VideoInfo> {
    Ok(match frame {
        RawFrame::SystemMemory { frame } => frame.info().clone(),
        RawFrame::DmaBuf { dma_info, .. } => dma_info
            .to_video_info()
            .map_err(|e| anyhow!("invalid dma-buf video info: {e}"))?,
    })
}

fn build_target_color(
    caps: Option<&gst::CapsRef>,
    is_pq: bool,
    is_hlg: bool,
) -> (pl_color_space, fl_protocol_HdrMetadata) {
    let is_hdr = is_pq || is_hlg;

    let pl_primaries = if is_hdr {
        pl_color_primaries::PL_COLOR_PRIM_BT_2020
    } else {
        pl_color_primaries::PL_COLOR_PRIM_BT_709
    };
    // Render HDR content as PQ- or HLG-encoded values into the RGBA16F
    // surface and tag the pixmap with the matching transfer function so the
    // compositor can decode and display-map it.
    let pl_transfer = if is_pq {
        pl_color_transfer::PL_COLOR_TRC_PQ
    } else if is_hlg {
        pl_color_transfer::PL_COLOR_TRC_HLG
    } else {
        pl_color_transfer::PL_COLOR_TRC_SRGB
    };

    let mut color_space: pl_color_space = unsafe { std::mem::zeroed() };
    color_space.primaries = pl_primaries;
    color_space.transfer = pl_transfer;

    let mut fl_hdr: fl_protocol_HdrMetadata = unsafe { std::mem::zeroed() };
    fl_hdr.transfer_function = if is_pq {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_pq
    } else if is_hlg {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_hlg
    } else {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_srgb
    } as u8;
    fl_hdr.primaries = if is_hdr {
        fl_protocol_Primaries_fl_protocol_Primaries_bt_2020
    } else {
        fl_protocol_Primaries_fl_protocol_Primaries_bt_709
    } as u8;

    // libplacebo render target: keep `color_space.hdr` zeroed so the renderer
    // treats target luminance as "unknown" and preserves the source dynamic
    // range. Setting a wrong target peak (e.g. a file's broken metadata
    // bottoming out at 0.0152 cd/m^2) would otherwise compress the picture
    // to black.

    if is_hdr {
        let mut have_valid_mastering = false;

        if let Some(caps) = caps {
            if let Ok(mdi) = gst_video::VideoMasteringDisplayInfo::from_caps(caps) {
                let primaries = mdi.display_primaries();
                let wp = mdi.white_point();
                let max_cd = mdi.max_display_mastering_luminance() as f32 / 10000.0;
                let min_cd = mdi.min_display_mastering_luminance() as f32 / 10000.0;

                if max_cd >= 100.0 {
                    fl_hdr.mastering_display_primaries[0] = fl_protocol_XyColor {
                        x: primaries[0].x(),
                        y: primaries[0].y(),
                    };
                    fl_hdr.mastering_display_primaries[1] = fl_protocol_XyColor {
                        x: primaries[1].x(),
                        y: primaries[1].y(),
                    };
                    fl_hdr.mastering_display_primaries[2] = fl_protocol_XyColor {
                        x: primaries[2].x(),
                        y: primaries[2].y(),
                    };
                    fl_hdr.mastering_display_white_point = fl_protocol_XyColor {
                        x: wp.x(),
                        y: wp.y(),
                    };
                    fl_hdr.max_mastering_luminance = max_cd;
                    fl_hdr.min_mastering_luminance = min_cd;
                    have_valid_mastering = true;
                } else {
                    debug!(
                        raw_max = mdi.max_display_mastering_luminance(),
                        max_cd, "Ignoring implausible mastering-display max luminance"
                    );
                }
            }

            if let Ok(cll) = gst_video::VideoContentLightLevel::from_caps(caps) {
                fl_hdr.max_cll = cll.max_content_light_level() as f32;
                fl_hdr.max_fall = cll.max_frame_average_light_level() as f32;
            }
        }

        if !have_valid_mastering {
            fl_hdr.max_mastering_luminance = 1000.0;
            fl_hdr.min_mastering_luminance = 0.005;
        }

        debug!(
            transfer = ?pl_transfer,
            primaries = ?pl_primaries,
            max_mastering = fl_hdr.max_mastering_luminance,
            min_mastering = fl_hdr.min_mastering_luminance,
            max_cll = fl_hdr.max_cll,
            max_fall = fl_hdr.max_fall,
            "Configured HDR pixmap metadata"
        );
    }

    (color_space, fl_hdr)
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
