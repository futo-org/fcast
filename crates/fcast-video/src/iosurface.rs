//! Zero-copy import of VideoToolbox IOSurface-backed frames into libplacebo GL textures.
//!
//! `vtdec` decodes into IOSurface-backed CVPixelBuffers and (given the `memory:IOSurface` caps
//! feature) hands them downstream without a CPU readback. This module borrows the `IOSurfaceRef`
//! out of a `GstMemory` and wraps each plane in a `GL_TEXTURE_RECTANGLE` via
//! `CGLTexImageIOSurface2D` — the exact call GStreamer's own `iosurfaceglmemory.c` makes. The
//! resulting texture is then wrapped by libplacebo with `pl_opengl_wrap` (see `placebo.rs`).
//!
//! This mirrors the Linux dma-buf path (`dmabuf.rs` + `placebo::render_dmabuf`): the buffer
//! carries a kernel-shareable handle that the app imports into its own GL context at render time.
//!
//! The `gst_iosurface_*` symbols come from `libgstiosurface-1.0` (unstable GStreamer API, "Since:
//! 1.30"). We hand-declare them because no `gstreamer-iosurface` crate exists for the 0.25 series.
//! If the pinned GStreamer ref is ever bumped, re-diff `gstiosurface.h` against these declarations.

use std::os::raw::{c_uint, c_void};

use gst::glib::translate::from_glib;

/// Caps feature string for IOSurface-backed memory (`GST_CAPS_FEATURE_MEMORY_IOSURFACE`).
pub const CAPS_FEATURE_MEMORY_IOSURFACE: &str = "memory:IOSurface";

/// Opaque `IOSurfaceRef` (a borrowed reference owned by the `GstMemory`).
pub type IOSurfaceRef = *const c_void;
/// Opaque `CGLContextObj`.
type CGLContextObj = *mut c_void;

// --- GL enum constants (macOS OpenGL headers) -----------------------------------------------
// Declared here rather than pulled from `glow` because the import path uses the CGL C API
// directly and never touches the `glow::Context`.
type GLenum = c_uint;
type GLint = i32;
type GLuint = c_uint;
type GLsizei = i32;

const GL_TEXTURE_RECTANGLE: GLenum = 0x84F5;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
const GL_LINEAR: GLint = 0x2601;
const GL_CLAMP_TO_EDGE: GLint = 0x812F;

const GL_R8: GLint = 0x8229;
const GL_RG8: GLint = 0x822B;
const GL_R16: GLint = 0x822A;
const GL_RG16: GLint = 0x822C;
const GL_RGBA8: GLint = 0x8058;
const GL_RGBA: GLint = 0x1908;

const GL_RED: GLenum = 0x1903;
const GL_RG: GLenum = 0x8227;
const GL_BGRA: GLenum = 0x80E1;

const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_UNSIGNED_SHORT: GLenum = 0x1403;
const GL_UNSIGNED_INT_8_8_8_8_REV: GLenum = 0x8367;

unsafe extern "C" {
    // libgstiosurface-1.0 (unstable GStreamer API, matches pinned 1.29.2 source)
    fn gst_is_iosurface_memory(mem: *mut gst::ffi::GstMemory) -> gst::glib::ffi::gboolean;
    fn gst_iosurface_memory_peek_surface(
        mem: *mut gst::ffi::GstMemory,
        surface: *mut IOSurfaceRef,
        plane: *mut c_uint,
    ) -> gst::glib::ffi::gboolean;

    // IOSurface.framework
    fn IOSurfaceGetWidthOfPlane(surface: IOSurfaceRef, plane: usize) -> usize;
    fn IOSurfaceGetHeightOfPlane(surface: IOSurfaceRef, plane: usize) -> usize;

    // OpenGL.framework (CGL)
    fn CGLGetCurrentContext() -> CGLContextObj;
    fn CGLTexImageIOSurface2D(
        ctx: CGLContextObj,
        target: GLenum,
        internal_format: GLint,
        width: GLsizei,
        height: GLsizei,
        format: GLenum,
        type_: GLenum,
        surface: IOSurfaceRef,
        plane: c_uint,
    ) -> i32;

    fn glGenTextures(n: GLsizei, textures: *mut GLuint);
    fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
    fn glBindTexture(target: GLenum, texture: GLuint);
    fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
}

/// The GL format triple used to interpret a single IOSurface plane, plus the libplacebo
/// `iformat` for `pl_opengl_wrap`.
///
/// `cgl_internal` and `pl_iformat` are separate on purpose: `CGLTexImageIOSurface2D` wants the
/// legacy interop formats (unsized `GL_RGBA` for BGRA — sized `GL_RGBA8` is rejected with
/// `kCGLBadValue`; sized `GL_R16`/`GL_RG16` for 16-bit so the plane isn't read as 8-bit), while
/// libplacebo only matches *sized* internal formats. The CGL triples mirror mpv's VideoToolbox
/// interop table (`hwdec_vt.c`).
#[derive(Debug, Clone, Copy)]
pub struct PlaneGlFormat {
    /// Internal format for `CGLTexImageIOSurface2D`.
    pub cgl_internal: GLint,
    /// Sized internal format for `pl_opengl_wrap_params.iformat`.
    pub pl_iformat: GLint,
    /// Client pixel format (GL_RED, GL_RG, GL_BGRA).
    pub format: GLenum,
    /// Client pixel type (GL_UNSIGNED_BYTE, ...).
    pub type_: GLenum,
}

/// GL format table for the plane of a given video format. `None` for formats/planes the importer
/// does not implement (caller must fall back to system memory). Mirrors the table in the plan.
pub fn plane_gl_format(format: gst_video::VideoFormat, plane: usize) -> Option<PlaneGlFormat> {
    use gst_video::VideoFormat;
    let f = |cgl_internal, pl_iformat, format, type_| {
        Some(PlaneGlFormat {
            cgl_internal,
            pl_iformat,
            format,
            type_,
        })
    };
    match (format, plane) {
        (VideoFormat::Nv12, 0) => f(GL_RED as GLint, GL_R8, GL_RED, GL_UNSIGNED_BYTE),
        (VideoFormat::Nv12, 1) => f(GL_RG as GLint, GL_RG8, GL_RG, GL_UNSIGNED_BYTE),
        (VideoFormat::P01010le, 0) => f(GL_R16, GL_R16, GL_RED, GL_UNSIGNED_SHORT),
        (VideoFormat::P01010le, 1) => f(GL_RG16, GL_RG16, GL_RG, GL_UNSIGNED_SHORT),
        (VideoFormat::Bgra, 0) => f(GL_RGBA, GL_RGBA8, GL_BGRA, GL_UNSIGNED_INT_8_8_8_8_REV),
        _ => None,
    }
}

/// Whether a `GstMemory` is IOSurface-backed.
pub fn is_iosurface_memory(mem: &gst::MemoryRef) -> bool {
    unsafe { from_glib(gst_is_iosurface_memory(mem.as_ptr() as *mut _)) }
}

/// Borrow the `IOSurfaceRef` (and its plane index) out of an IOSurface-backed `GstMemory`. The
/// returned surface is owned by `mem` and stays valid as long as `mem` (and therefore the
/// owning `gst::Buffer`) is alive.
pub fn peek_surface(mem: &gst::MemoryRef) -> Option<(IOSurfaceRef, u32)> {
    let mut surface: IOSurfaceRef = std::ptr::null();
    let mut plane: c_uint = 0;
    let ok: bool = unsafe {
        from_glib(gst_iosurface_memory_peek_surface(
            mem.as_ptr() as *mut _,
            &mut surface,
            &mut plane,
        ))
    };
    if ok && !surface.is_null() {
        Some((surface, plane))
    } else {
        None
    }
}

/// A GL texture (`GL_TEXTURE_RECTANGLE`) bound to one IOSurface plane. Deletes the texture on
/// drop. The libplacebo `pl_tex` wrapper created from `id` does *not* own the GL object, so this
/// guard must outlive the wrapper and be dropped only after `pl_tex_destroy`.
pub struct PlaneTexture {
    pub id: GLuint,
    /// Plane dimensions in texels (device-aligned; from the IOSurface, not the video info).
    pub width: i32,
    pub height: i32,
    pub gl_format: PlaneGlFormat,
}

impl PlaneTexture {
    /// The `GL_TEXTURE_RECTANGLE` target the texture is bound to (for `pl_opengl_wrap_params`).
    pub const TARGET: GLenum = GL_TEXTURE_RECTANGLE;
}

impl Drop for PlaneTexture {
    fn drop(&mut self) {
        unsafe { glDeleteTextures(1, &self.id) }
    }
}

/// Import one plane of `surface` into a fresh `GL_TEXTURE_RECTANGLE`, using the currently-current
/// CGL context (Slint's, when called from `SwapchainSink::render`). Returns `None` if there is no
/// current context or the CGL import fails.
///
/// # Safety
///
/// A CGL/OpenGL context must be current on the calling thread and `surface` must be a live
/// IOSurface with at least `plane + 1` planes.
pub unsafe fn import_plane(
    surface: IOSurfaceRef,
    plane: u32,
    gl_format: PlaneGlFormat,
) -> Option<PlaneTexture> {
    unsafe {
        let ctx = CGLGetCurrentContext();
        if ctx.is_null() {
            return None;
        }

        let width = IOSurfaceGetWidthOfPlane(surface, plane as usize) as i32;
        let height = IOSurfaceGetHeightOfPlane(surface, plane as usize) as i32;

        let mut id: GLuint = 0;
        glGenTextures(1, &mut id);
        if id == 0 {
            return None;
        }
        let tex = PlaneTexture {
            id,
            width,
            height,
            gl_format,
        };

        glBindTexture(GL_TEXTURE_RECTANGLE, id);
        glTexParameteri(GL_TEXTURE_RECTANGLE, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_RECTANGLE, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_RECTANGLE, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_RECTANGLE, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

        let err = CGLTexImageIOSurface2D(
            ctx,
            GL_TEXTURE_RECTANGLE,
            gl_format.cgl_internal,
            width,
            height,
            gl_format.format,
            gl_format.type_,
            surface,
            plane,
        );
        glBindTexture(GL_TEXTURE_RECTANGLE, 0);

        if err != 0 {
            // Surfaces the exact CGL error (e.g. legacy OpenGL rejecting GL_R16/GL_RG16 for
            // 10-bit P010) instead of a generic import failure. `tex` drops here, freeing the
            // GL texture.
            tracing::warn!(
                cgl_error = err,
                internal = gl_format.cgl_internal,
                format = gl_format.format,
                gl_type = gl_format.type_,
                width,
                height,
                plane,
                "CGLTexImageIOSurface2D failed",
            );
            return None;
        }

        Some(tex)
    }
}
