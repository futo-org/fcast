use std::{
    collections::VecDeque,
    ffi::{c_int, c_uint, c_void},
    fs::OpenOptions,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
};

use anyhow::{Result, anyhow};
use fiatlux::*;
use rcore::{
    glow::{self, HasContext},
    gst, gst_allocators,
    gst_video::{self, prelude::*},
    tracing::{debug, warn},
    video::{Frame, FrameData},
};

const SCANOUT_HOLD: usize = 3;

type EglImageTargetTexture2dOes = unsafe extern "C" fn(target: u32, image: *const c_void);

struct HeldBuffer {
    #[allow(dead_code)]
    buffer: Option<gst::Buffer>,
    pixmap_id: fl_protocol_PixmapId,
}

pub struct FhsPixmapSink {
    client: *mut fl_Client,
    surface_id: fl_protocol_SurfaceId,
    hdr_metadata: Option<fl_protocol_HdrMetadata>,
    surface_has_hdr_metadata: bool,
    held: VecDeque<HeldBuffer>,
    gbm: GbmAllocator,
    gl: glow::Context,
    egl_display: *const c_void,
    egl_image_target: Option<EglImageTargetTexture2dOes>,
}

impl FhsPixmapSink {
    pub fn new(
        client: *mut fl_Client,
        surface_id: fl_protocol_SurfaceId,
        gl: glow::Context,
        egl_display: *const c_void,
        egl_image_target: Option<EglImageTargetTexture2dOes>,
    ) -> Result<Self> {
        Ok(Self {
            client,
            surface_id,
            hdr_metadata: None,
            surface_has_hdr_metadata: false,
            held: VecDeque::new(),
            gbm: GbmAllocator::new(client)?,
            gl,
            egl_display,
            egl_image_target,
        })
    }

    pub fn render(&mut self, frame: &Frame) -> Result<()> {
        let (pixmap_id, held_buffer) = match &frame.data {
            FrameData::DmaBuf { buffer, dma_info } => {
                (self.import_dmabuf(buffer, dma_info)?, Some(buffer.clone()))
            }
            FrameData::SystemMemory { frame } => (self.import_system_memory(frame)?, None),
        };

        unsafe {
            fl_discard_reply(
                self.client,
                fl_set_surface_pixmap(self.client, self.surface_id, pixmap_id).value,
            );
        }

        self.held.push_back(HeldBuffer {
            buffer: held_buffer,
            pixmap_id,
        });
        while self.held.len() > SCANOUT_HOLD {
            if let Some(old) = self.held.pop_front() {
                unsafe {
                    fl_discard_reply(
                        self.client,
                        fl_destroy_pixmap(self.client, old.pixmap_id).value,
                    );
                }
            }
        }

        let colorimetry = frame_video_colorimetry(frame);
        let transfer = colorimetry.transfer();
        let is_pq = matches!(transfer, gst_video::VideoTransferFunction::Smpte2084);
        let is_hlg = matches!(transfer, gst_video::VideoTransferFunction::AribStdB67);
        self.update_hdr_metadata(frame, colorimetry, is_pq, is_hlg);

        Ok(())
    }

    fn import_dmabuf(
        &self,
        buffer: &gst::Buffer,
        dma_info: &gst_video::VideoInfoDmaDrm,
    ) -> Result<fl_protocol_PixmapId> {
        let vmeta = buffer
            .meta::<gst_video::VideoMeta>()
            .ok_or_else(|| anyhow!("dma-buf frame is missing a VideoMeta"))?;
        let n_planes = vmeta.n_planes() as usize;
        if n_planes == 0 || n_planes > 4 {
            return Err(anyhow!("unsupported dma-buf plane count {n_planes}"));
        }

        let fourcc = dma_info.fourcc();
        let modifier = dma_info.modifier();
        let width = dma_info.width();
        let height = dma_info.height();
        let flags = full_color_range_flag(&dma_info.colorimetry());
        let vmeta_offsets = vmeta.offset();
        let vmeta_strides = vmeta.stride();

        let mut offsets = [0u32; 4];
        let mut pitches = [0u32; 4];
        let modifiers = [modifier; 4];
        let mut fds: Vec<OwnedFd> = Vec::with_capacity(n_planes);

        for plane in 0..n_planes {
            let Some((range, skip)) =
                buffer.find_memory(vmeta_offsets[plane]..(vmeta_offsets[plane] + 1))
            else {
                return Err(anyhow!("no memory backs dma-buf plane {plane}"));
            };
            let mem = buffer.peek_memory(range.start);
            let Some(mem) = mem.downcast_memory_ref::<gst_allocators::DmaBufMemory>() else {
                return Err(anyhow!("dma-buf plane {plane} is not a DmaBufMemory"));
            };
            let dup = unsafe { BorrowedFd::borrow_raw(mem.fd()) }
                .try_clone_to_owned()
                .map_err(|e| anyhow!("failed to dup dma-buf fd: {e}"))?;
            offsets[plane] = (mem.offset() + skip) as u32;
            pitches[plane] = vmeta_strides[plane] as u32;
            fds.push(dup);
        }

        let raw_fds: Vec<i32> = fds.iter().map(|fd| fd.as_raw_fd()).collect();

        let pixmap_id = unsafe {
            let seq = fl_create_pixmap_from_dmabuf(
                self.client,
                width,
                height,
                fourcc,
                n_planes as u8,
                flags,
                offsets.as_ptr(),
                pitches.as_ptr(),
                modifiers.as_ptr(),
                raw_fds.len() as u8,
                raw_fds.as_ptr(),
            );
            drop(fds);
            if seq.value == 0 {
                return Err(anyhow!("fl_create_pixmap_from_dmabuf failed"));
            }

            let mut reply: fl_reply_CreatePixmapFromDmaBuf = std::mem::zeroed();
            if !fl_receive_reply_create_pixmap_from_dma_buf(self.client, seq, &mut reply) {
                return Err(anyhow!("fl_create_pixmap_from_dmabuf reply was null"));
            }
            reply.pixmap_id
        };

        Ok(pixmap_id)
    }

    fn import_system_memory(
        &self,
        frame: &gst_video::VideoFrame<gst_video::video_frame::Readable>,
    ) -> Result<fl_protocol_PixmapId> {
        const YUV420: u32 = fourcc_code(b'Y', b'U', b'1', b'2');
        const YVU420: u32 = fourcc_code(b'Y', b'V', b'1', b'2');
        const NV12: u32 = fourcc_code(b'N', b'V', b'1', b'2');

        let info = frame.info();
        let format = info.format();
        let fourcc = gst_video::dma_drm_fourcc_from_format(format)
            .map_err(|_| anyhow!("no DRM fourcc for video format {format:?}"))?;
        let flags = full_color_range_flag(&info.colorimetry());
        let width = frame.width();
        let height = frame.height();
        let strides = frame.plane_stride();

        if fourcc == YUV420 || fourcc == YVU420 {
            let (u_plane, v_plane) = if fourcc == YUV420 { (1, 2) } else { (2, 1) };
            let y = frame.plane_data(0).map_err(|_| anyhow!("missing Y plane"))?;
            let u = frame
                .plane_data(u_plane as u32)
                .map_err(|_| anyhow!("missing U plane"))?;
            let v = frame
                .plane_data(v_plane as u32)
                .map_err(|_| anyhow!("missing V plane"))?;
            let chroma_w = (width as usize).div_ceil(2);
            let chroma_h = (height as usize).div_ceil(2);
            let u_stride = strides[u_plane] as usize;
            let v_stride = strides[v_plane] as usize;
            let uv_stride = chroma_w * 2;
            let mut uv = vec![0u8; uv_stride * chroma_h];
            for row in 0..chroma_h {
                let u_row = &u[row * u_stride..];
                let v_row = &v[row * v_stride..];
                let dst = &mut uv[row * uv_stride..];
                for x in 0..chroma_w {
                    dst[x * 2] = u_row[x];
                    dst[x * 2 + 1] = v_row[x];
                }
            }
            let planes = plane_uploads(NV12).unwrap();
            let plane_data = [(y, strides[0] as usize), (uv.as_slice(), uv_stride)];
            return self.upload(NV12, width, height, &planes, &plane_data, flags);
        }

        let planes = plane_uploads(fourcc)
            .ok_or_else(|| anyhow!("format {format:?} unsupported for gpu upload"))?;
        let mut plane_data: Vec<(&[u8], usize)> = Vec::with_capacity(planes.len());
        for plane in 0..planes.len() {
            let src = frame
                .plane_data(plane as u32)
                .map_err(|_| anyhow!("missing plane {plane}"))?;
            plane_data.push((src, strides[plane] as usize));
        }

        self.upload(fourcc, width, height, &planes, &plane_data, flags)
    }

    pub(crate) fn upload_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<fl_protocol_PixmapId> {
        let planes = [PlaneUpload {
            sub_fourcc: DRM_FORMAT_ABGR8888,
            gl_format: glow::RGBA,
            gl_type: glow::UNSIGNED_BYTE,
            bytes_per_pixel: 4,
            w_shift: 0,
            h_shift: 0,
        }];
        let plane_data = [(rgba, width as usize * 4)];
        let flags = fl_protocol_PixmapFlags_flags_fl_protocol_PixmapFlags_full_color_range_bit
            as fl_protocol_PixmapFlags;
        self.upload(DRM_FORMAT_ABGR8888, width, height, &planes, &plane_data, flags)
    }

    fn upload(
        &self,
        fourcc: u32,
        width: u32,
        height: u32,
        planes: &[PlaneUpload],
        plane_data: &[(&[u8], usize)],
        flags: fl_protocol_PixmapFlags,
    ) -> Result<fl_protocol_PixmapId> {
        let target = self
            .egl_image_target
            .ok_or_else(|| anyhow!("glEGLImageTargetTexture2DOES unavailable"))?;
        let mut bos: Vec<*mut gbm_bo> = Vec::with_capacity(planes.len());
        let result = (|| -> Result<fl_protocol_PixmapId> {
            for (pu, &(src, src_stride)) in planes.iter().zip(plane_data.iter()) {
                let plane_w = (width + (1 << pu.w_shift) - 1) >> pu.w_shift;
                let plane_h = (height + (1 << pu.h_shift) - 1) >> pu.h_shift;
                let alloc_w = plane_w.next_multiple_of(64);
                let bo = self.gbm.create_scanout_bo(alloc_w, plane_h, pu.sub_fourcc, 1)?;
                bos.push(bo);
                self.upload_plane(bo, pu, src, src_stride, plane_w, plane_h, target)?;
            }
            create_pixmap_from_plane_bos(self.client, &bos, fourcc, width, height, flags)
        })();
        for bo in bos {
            unsafe { gbm_bo_destroy(bo) };
        }
        result
    }

    fn upload_plane(
        &self,
        bo: *mut gbm_bo,
        pu: &PlaneUpload,
        src: &[u8],
        src_stride: usize,
        plane_w: u32,
        plane_h: u32,
        target: EglImageTargetTexture2dOes,
    ) -> Result<()> {
        let modifier = unsafe { gbm_bo_get_modifier(bo) };
        let fd = unsafe { gbm_bo_get_fd(bo) };
        if fd < 0 {
            return Err(anyhow!("gbm_bo_get_fd failed"));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let bo_offset = unsafe { gbm_bo_get_offset(bo, 0) };
        let bo_stride = unsafe { gbm_bo_get_stride_for_plane(bo, 0) };

        let attribs: [egl_sys::bindings::types::EGLAttrib; 17] = [
            egl_sys::bindings::WIDTH as isize,
            plane_w as isize,
            egl_sys::bindings::HEIGHT as isize,
            plane_h as isize,
            egl_sys::bindings::LINUX_DRM_FOURCC_EXT as isize,
            pu.sub_fourcc as isize,
            egl_sys::bindings::DMA_BUF_PLANE0_FD_EXT as isize,
            fd.as_raw_fd() as isize,
            egl_sys::bindings::DMA_BUF_PLANE0_OFFSET_EXT as isize,
            bo_offset as isize,
            egl_sys::bindings::DMA_BUF_PLANE0_PITCH_EXT as isize,
            bo_stride as isize,
            egl_sys::bindings::DMA_BUF_PLANE0_MODIFIER_LO_EXT as isize,
            (modifier & 0xffff_ffff) as isize,
            egl_sys::bindings::DMA_BUF_PLANE0_MODIFIER_HI_EXT as isize,
            (modifier >> 32) as isize,
            egl_sys::bindings::NONE as isize,
        ];

        let image = unsafe {
            egl_sys::bindings::CreateImage(
                self.egl_display as egl_sys::bindings::types::EGLDisplay,
                egl_sys::bindings::NO_CONTEXT,
                egl_sys::bindings::LINUX_DMA_BUF_EXT,
                core::ptr::null::<c_void>() as egl_sys::bindings::types::EGLClientBuffer,
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            let egl_error = unsafe { egl_sys::bindings::GetError() };
            return Err(anyhow!(
                "eglCreateImage failed: egl_error={egl_error:#06x} \
                 sub_fourcc={:#010x} modifier={modifier:#018x} offset={bo_offset} \
                 pitch={bo_stride} {plane_w}x{plane_h}",
                pu.sub_fourcc
            ));
        }

        let row_length = (src_stride / pu.bytes_per_pixel) as i32;

        let upload = (|| -> Result<()> {
            let tex =
                unsafe { self.gl.create_texture() }.map_err(|e| anyhow!("create_texture: {e}"))?;
            unsafe {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                target(glow::TEXTURE_2D, image as *const c_void);
                self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, row_length);
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    plane_w as i32,
                    plane_h as i32,
                    pu.gl_format,
                    pu.gl_type,
                    glow::PixelUnpackData::Slice(Some(src)),
                );
                self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                self.gl.delete_texture(tex);
            }
            Ok(())
        })();

        unsafe {
            egl_sys::bindings::DestroyImage(
                self.egl_display as egl_sys::bindings::types::EGLDisplay,
                image,
            );
        }
        upload?;

        unsafe { self.gl.finish() };
        Ok(())
    }

    fn update_hdr_metadata(
        &mut self,
        frame: &Frame,
        colorimetry: gst_video::VideoColorimetry,
        is_pq: bool,
        is_hlg: bool,
    ) {
        if is_pq || is_hlg {
            let new_hdr_metadata = build_hdr_metadata(frame, colorimetry, is_pq, is_hlg);
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
                        fl_set_surface_hdr_metadata(self.client, self.surface_id, &new_hdr_metadata)
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
    }

    pub fn teardown(&mut self) {
        for held in self.held.drain(..) {
            unsafe {
                fl_discard_reply(
                    self.client,
                    fl_destroy_pixmap(self.client, held.pixmap_id).value,
                );
            }
        }
    }
}

const GBM_BO_USE_SCANOUT: u32 = 1 << 0;
const GBM_BO_USE_RENDERING: u32 = 1 << 2;

struct PlaneUpload {
    sub_fourcc: u32,
    gl_format: u32,
    gl_type: u32,
    bytes_per_pixel: usize,
    w_shift: u32,
    h_shift: u32,
}

fn plane_uploads(fourcc: u32) -> Option<Vec<PlaneUpload>> {
    const NV12: u32 = fourcc_code(b'N', b'V', b'1', b'2');
    const P010: u32 = fourcc_code(b'P', b'0', b'1', b'0');
    const YUV420: u32 = fourcc_code(b'Y', b'U', b'1', b'2');
    const YVU420: u32 = fourcc_code(b'Y', b'V', b'1', b'2');
    const R8: u32 = fourcc_code(b'R', b'8', b' ', b' ');
    const GR88: u32 = fourcc_code(b'G', b'R', b'8', b'8');
    const R16: u32 = fourcc_code(b'R', b'1', b'6', b' ');
    const GR1616: u32 = fourcc_code(b'G', b'R', b'3', b'2');

    match fourcc {
        NV12 => Some(vec![
            PlaneUpload {
                sub_fourcc: R8,
                gl_format: glow::RED,
                gl_type: glow::UNSIGNED_BYTE,
                bytes_per_pixel: 1,
                w_shift: 0,
                h_shift: 0,
            },
            PlaneUpload {
                sub_fourcc: GR88,
                gl_format: glow::RG,
                gl_type: glow::UNSIGNED_BYTE,
                bytes_per_pixel: 2,
                w_shift: 1,
                h_shift: 1,
            },
        ]),
        P010 => Some(vec![
            PlaneUpload {
                sub_fourcc: R16,
                gl_format: glow::RED,
                gl_type: glow::UNSIGNED_SHORT,
                bytes_per_pixel: 2,
                w_shift: 0,
                h_shift: 0,
            },
            PlaneUpload {
                sub_fourcc: GR1616,
                gl_format: glow::RG,
                gl_type: glow::UNSIGNED_SHORT,
                bytes_per_pixel: 4,
                w_shift: 1,
                h_shift: 1,
            },
        ]),
        YUV420 | YVU420 => Some(vec![
            PlaneUpload {
                sub_fourcc: R8,
                gl_format: glow::RED,
                gl_type: glow::UNSIGNED_BYTE,
                bytes_per_pixel: 1,
                w_shift: 0,
                h_shift: 0,
            },
            PlaneUpload {
                sub_fourcc: R8,
                gl_format: glow::RED,
                gl_type: glow::UNSIGNED_BYTE,
                bytes_per_pixel: 1,
                w_shift: 1,
                h_shift: 1,
            },
            PlaneUpload {
                sub_fourcc: R8,
                gl_format: glow::RED,
                gl_type: glow::UNSIGNED_BYTE,
                bytes_per_pixel: 1,
                w_shift: 1,
                h_shift: 1,
            },
        ]),
        _ => None,
    }
}

fn create_pixmap_from_plane_bos(
    client: *mut fl_Client,
    bos: &[*mut gbm_bo],
    fourcc: u32,
    width: u32,
    height: u32,
    flags: fl_protocol_PixmapFlags,
) -> Result<fl_protocol_PixmapId> {
    let mut offsets = [0u32; 4];
    let mut pitches = [0u32; 4];
    let mut modifiers = [0u64; 4];
    let mut fds: Vec<OwnedFd> = Vec::with_capacity(bos.len());
    for (plane, &bo) in bos.iter().enumerate() {
        offsets[plane] = unsafe { gbm_bo_get_offset(bo, 0) };
        pitches[plane] = unsafe { gbm_bo_get_stride_for_plane(bo, 0) };
        modifiers[plane] = unsafe { gbm_bo_get_modifier(bo) };
        let fd = unsafe { gbm_bo_get_fd(bo) };
        if fd < 0 {
            return Err(anyhow!("gbm_bo_get_fd failed"));
        }
        fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let raw_fds: Vec<i32> = fds.iter().map(|fd| fd.as_raw_fd()).collect();

    unsafe {
        let seq = fl_create_pixmap_from_dmabuf(
            client,
            width,
            height,
            fourcc,
            bos.len() as u8,
            flags,
            offsets.as_ptr(),
            pitches.as_ptr(),
            modifiers.as_ptr(),
            raw_fds.len() as u8,
            raw_fds.as_ptr(),
        );
        drop(fds);
        if seq.value == 0 {
            return Err(anyhow!("fl_create_pixmap_from_dmabuf failed"));
        }

        let mut reply: fl_reply_CreatePixmapFromDmaBuf = std::mem::zeroed();
        if !fl_receive_reply_create_pixmap_from_dma_buf(client, seq, &mut reply) {
            return Err(anyhow!("fl_create_pixmap_from_dmabuf reply was null"));
        }
        Ok(reply.pixmap_id)
    }
}

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
    fn gbm_bo_get_offset(bo: *mut gbm_bo, plane: c_int) -> u32;
    fn gbm_bo_get_plane_count(bo: *mut gbm_bo) -> c_int;
    fn gbm_bo_get_stride_for_plane(bo: *mut gbm_bo, plane: c_int) -> u32;
}

const fn fourcc_code(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const DRM_FORMAT_ABGR8888: u32 = fourcc_code(b'A', b'B', b'2', b'4');

pub(crate) struct GbmAllocator {
    _drm_fd: OwnedFd,
    device: *mut gbm_device,
}

impl GbmAllocator {
    pub(crate) fn new(client: *mut fl_Client) -> Result<Self> {
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
                let slice = std::slice::from_raw_parts(path.ptr, path.len as usize);
                std::str::from_utf8(slice)
                    .map_err(|e| anyhow!("invalid utf-8 in render device path: {e}"))?
                    .to_string()
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

    fn create_gpu_scanout_bo(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
        use_flags: u32,
    ) -> Result<*mut gbm_bo> {
        if modifiers.is_empty() {
            return Err(anyhow!("no importable modifiers for fourcc {fourcc:#010x}"));
        }
        let bo = unsafe {
            gbm_bo_create_with_modifiers2(
                self.device,
                width,
                height,
                fourcc,
                modifiers.as_ptr(),
                modifiers.len() as c_uint,
                use_flags,
            )
        };
        if bo.is_null() {
            return Err(anyhow!(
                "gbm_bo_create_with_modifiers2 failed for {width}x{height} fourcc {fourcc:#010x}"
            ));
        }
        Ok(bo)
    }

    fn create_scanout_bo(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        plane_count: usize,
    ) -> Result<*mut gbm_bo> {
        let mut modifiers = rcore::egl::get_importable_modifiers(fourcc);
        if !modifiers.contains(&0) {
            modifiers.push(0);
        }
        modifiers.sort_by_key(|&m| u64::from(m == 0));
        let flag_sets = [
            GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT,
            GBM_BO_USE_SCANOUT,
            GBM_BO_USE_RENDERING,
        ];
        for use_flags in flag_sets {
            for &modifier in &modifiers {
                let Ok(bo) =
                    self.create_gpu_scanout_bo(width, height, fourcc, &[modifier], use_flags)
                else {
                    continue;
                };
                if unsafe { gbm_bo_get_plane_count(bo) } as usize == plane_count {
                    return Ok(bo);
                }
                unsafe { gbm_bo_destroy(bo) };
            }
        }
        Err(anyhow!(
            "no importable {plane_count}-plane modifier for fourcc {fourcc:#010x}"
        ))
    }
}

impl Drop for GbmAllocator {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.device) };
    }
}

fn frame_video_colorimetry(frame: &Frame) -> gst_video::VideoColorimetry {
    match &frame.data {
        FrameData::SystemMemory { frame } => frame.info().colorimetry(),
        FrameData::DmaBuf { dma_info, .. } => dma_info.colorimetry(),
    }
}

fn full_color_range_flag(colorimetry: &gst_video::VideoColorimetry) -> fl_protocol_PixmapFlags {
    if colorimetry.range() == gst_video::VideoColorRange::Range0_255 {
        fl_protocol_PixmapFlags_flags_fl_protocol_PixmapFlags_full_color_range_bit
            as fl_protocol_PixmapFlags
    } else {
        0
    }
}

fn gst_primaries_to_fl_primaries(
    primaries: gst_video::VideoColorPrimaries,
    is_hdr: bool,
) -> fl_protocol_Primaries {
    match primaries {
        gst_video::VideoColorPrimaries::Bt709 => fl_protocol_Primaries_fl_protocol_Primaries_bt_709,
        gst_video::VideoColorPrimaries::Bt2020 => {
            fl_protocol_Primaries_fl_protocol_Primaries_bt_2020
        }
        gst_video::VideoColorPrimaries::Smpterp431 => {
            fl_protocol_Primaries_fl_protocol_Primaries_dci_p3
        }
        gst_video::VideoColorPrimaries::Smpteeg432 => {
            fl_protocol_Primaries_fl_protocol_Primaries_display_p3
        }
        _ => {
            if is_hdr {
                fl_protocol_Primaries_fl_protocol_Primaries_bt_2020
            } else {
                fl_protocol_Primaries_fl_protocol_Primaries_bt_709
            }
        }
    }
}

fn build_hdr_metadata(
    frame: &Frame,
    colorimetry: gst_video::VideoColorimetry,
    is_pq: bool,
    is_hlg: bool,
) -> fl_protocol_HdrMetadata {
    let is_hdr = is_pq || is_hlg;

    let mut hdr_metadata: fl_protocol_HdrMetadata = unsafe { std::mem::zeroed() };
    hdr_metadata.transfer_function = if is_pq {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_pq
    } else if is_hlg {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_hlg
    } else {
        fl_protocol_TransferFunction_fl_protocol_TransferFunction_srgb
    } as u8;

    hdr_metadata.primaries = gst_primaries_to_fl_primaries(colorimetry.primaries(), is_hdr) as u8;

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

    hdr_metadata
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
