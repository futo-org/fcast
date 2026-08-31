//! AHardwareBuffer to GL texture, through EGL and nothing else.
//!
//! Deliberately renderer-agnostic: there is no skia type, no slint type and
//! no wgpu type in this file. What comes out is a plain GL texture name and
//! the target it binds to, which is a currency every renderer in the tree
//! already takes. Slint's skia backend consumes it through
//! `BorrowedOpenGLTextureBuilder`, and dodvg's GL executor takes the very
//! same pair through its `adopt_gl_texture`, so the day android moves off
//! skia this module does not move with it.
//!
//! The route is the standard android one:
//!
//! ```text
//! AHardwareBuffer*
//!   -> eglGetNativeClientBufferANDROID   (EGL_ANDROID_get_native_client_buffer)
//!   -> eglCreateImageKHR EGL_NATIVE_BUFFER_ANDROID  (EGL_ANDROID_image_native_buffer)
//!   -> glEGLImageTargetTexture2DOES      (GL_OES_EGL_image_external)
//! ```
//!
//! # Colour, honestly
//!
//! A YUV AHardwareBuffer can only be bound to `GL_TEXTURE_EXTERNAL_OES`, and
//! sampling one returns RGB: the driver has already done the YUV to RGB
//! conversion, with a matrix and a range we did not choose. Our
//! [`crate::video_math`] bridge path does that conversion itself, in BT.601
//! limited range, and is at least explicit about it. The driver's default for
//! an untagged 8-bit YUV gralloc buffer is also BT.601 limited on every
//! implementation seen so far, so the two agree in practice, but HD and UHD
//! content is BT.709 and both are wrong about it in the same direction.
//!
//! Tagging the buffer would fix it: `AHardwareBuffer_setDataSpace` takes a
//! full colour description and the driver would honour it. It is API 34, and
//! this receiver's minimum is 25, so it is a conditional improvement rather
//! than the design. Wiring it in when the device is new enough is the first
//! follow-up, and it is cheap: one call per pooled buffer, at allocation.
//!
//! Keeping the conversion ours instead would mean not using a YUV
//! AHardwareBuffer at all: one `R8_UNORM` buffer per plane, three of them,
//! each imported as an ordinary `GL_TEXTURE_2D`, with our own shader doing
//! the matrix from the caps' colorimetry. That is the only route that gives
//! back full colour control and it is real, but it costs three gralloc
//! allocations per frame slot, needs API 29 for `R8_UNORM`, forces I420 (the
//! NDK has no two-channel format, so NV12's interleaved plane cannot be an
//! AHardwareBuffer), and needs a full GL pipeline of our own inside slint's
//! context. It is the right second step, once the single-buffer route has
//! been shown to work on real hardware.
//!
//! # Threading
//!
//! Every function here must be called with the renderer's GL context
//! current, which in practice means from inside a slint rendering notifier.
//! Nothing here has a `Drop` that touches GL, precisely so a cache entry
//! cannot be freed on a thread with no context.

#![allow(non_snake_case)]

use std::{
    ffi::{CString, c_char, c_void},
    ptr,
    sync::{Mutex, OnceLock},
};

use tracing::{info, warn};

pub const GL_TEXTURE_2D: u32 = 0x0DE1;
pub const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;

const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: i32 = 1;
const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;

type EGLDisplay = *mut c_void;
type EGLClientBuffer = *mut c_void;
type EGLImageKHR = *mut c_void;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetCurrentDisplay() -> EGLDisplay;
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

#[link(name = "GLESv2")]
unsafe extern "C" {
    fn glGenTextures(n: i32, textures: *mut u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glTexParameteri(target: u32, pname: u32, param: i32);
    fn glGetError() -> u32;
}

/// The extension entry points, resolved once. `eglGetProcAddress` is the only
/// way to reach them: none is in the NDK's link surface.
struct Ext {
    get_native_client_buffer: unsafe extern "C" fn(*const c_void) -> EGLClientBuffer,
    create_image: unsafe extern "C" fn(
        EGLDisplay,
        *mut c_void,
        u32,
        EGLClientBuffer,
        *const i32,
    ) -> EGLImageKHR,
    destroy_image: unsafe extern "C" fn(EGLDisplay, EGLImageKHR) -> u32,
    image_target_texture_2d: unsafe extern "C" fn(u32, EGLImageKHR),
}

// The pointers are process-global driver entry points.
unsafe impl Send for Ext {}
unsafe impl Sync for Ext {}

fn ext() -> Option<&'static Ext> {
    static EXT: OnceLock<Option<Ext>> = OnceLock::new();
    EXT.get_or_init(|| {
        let get = |name: &str| -> *mut c_void {
            let Ok(c) = CString::new(name) else {
                return ptr::null_mut();
            };
            unsafe { eglGetProcAddress(c.as_ptr()) }
        };
        let a = get("eglGetNativeClientBufferANDROID");
        let b = get("eglCreateImageKHR");
        let c = get("eglDestroyImageKHR");
        let d = get("glEGLImageTargetTexture2DOES");
        if a.is_null() || b.is_null() || c.is_null() || d.is_null() {
            warn!(
                get_native_client_buffer = !a.is_null(),
                create_image = !b.is_null(),
                destroy_image = !c.is_null(),
                image_target_texture_2d = !d.is_null(),
                "android ahb lane: the EGL import extensions are not all there"
            );
            return None;
        }
        info!("android ahb lane: EGL import extensions resolved");
        // The driver's own function pointers, whose signatures are fixed by
        // the extension specs quoted at the top of this file.
        Some(unsafe {
            Ext {
                get_native_client_buffer: std::mem::transmute::<*mut c_void, _>(a),
                create_image: std::mem::transmute::<*mut c_void, _>(b),
                destroy_image: std::mem::transmute::<*mut c_void, _>(c),
                image_target_texture_2d: std::mem::transmute::<*mut c_void, _>(d),
            }
        })
    })
    .as_ref()
}

/// A GL texture that borrows a gralloc allocation's pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Import {
    pub texture: u32,
    /// `GL_TEXTURE_EXTERNAL_OES` for every YUV buffer, `GL_TEXTURE_2D` for
    /// RGBA. The renderer needs to know which: an external texture samples
    /// through a different sampler type and cannot be rendered into.
    pub target: u32,
}

/// One cached import. The gralloc reference is the lane's own, taken so the
/// EGLImage stays valid even if the pool is torn down under it.
struct Entry {
    buffer: *mut ndk_sys::AHardwareBuffer,
    image: EGLImageKHR,
    import: Import,
    /// Bumped on every hit, so the eviction picks the coldest slot.
    last_used: u64,
}

// Only ever touched with the render thread's context current, see the module
// header. The pointers are gralloc and EGL handles, both process-wide.
unsafe impl Send for Entry {}

/// Slots for the pool's whole working set plus slack. The pool asks for six
/// buffers and cycles exactly those, so after the first pass every frame is a
/// hit and nothing is ever allocated per frame.
const SLOTS: usize = 10;

#[derive(Default)]
struct Cache {
    entries: Vec<Entry>,
    clock: u64,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    entries: Vec::new(),
    clock: 0,
});

/// Imports a gralloc buffer as a GL texture, reusing the import if this
/// buffer has been seen before.
///
/// `external` picks the binding target and must match the buffer's format:
/// true for anything YUV, false for RGBA. Returns `None` when the extensions
/// are missing or the driver refused the image, which leaves the caller on
/// the conversion path.
///
/// # Safety
///
/// The renderer's GL context must be current on the calling thread, and
/// `buffer` must be a live `AHardwareBuffer`.
pub unsafe fn import(buffer: *mut ndk_sys::AHardwareBuffer, external: bool) -> Option<Import> {
    let ext = ext()?;
    let mut cache = CACHE.lock().ok()?;
    if cache.entries.capacity() == 0 {
        // once, so the steady state never reallocates
        cache.entries.reserve_exact(SLOTS);
    }
    cache.clock += 1;
    let clock = cache.clock;

    if let Some(entry) = cache.entries.iter_mut().find(|e| e.buffer == buffer) {
        entry.last_used = clock;
        return Some(entry.import);
    }

    let display = unsafe { eglGetCurrentDisplay() };
    if display.is_null() {
        warn!("android ahb lane: no current EGL display, the import needs the render thread");
        return None;
    }
    let client = unsafe { (ext.get_native_client_buffer)(buffer as *const c_void) };
    if client.is_null() {
        warn!("android ahb lane: the driver would not wrap the gralloc buffer");
        return None;
    }
    // PRESERVED, because the decoder's pixels are already in there and an
    // import that is free to discard them shows a black frame.
    let attribs = [EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE];
    let image = unsafe {
        (ext.create_image)(
            display,
            ptr::null_mut(),
            EGL_NATIVE_BUFFER_ANDROID,
            client,
            attribs.as_ptr(),
        )
    };
    if image.is_null() {
        warn!("android ahb lane: eglCreateImageKHR refused the buffer");
        return None;
    }

    let target = if external {
        GL_TEXTURE_EXTERNAL_OES
    } else {
        GL_TEXTURE_2D
    };
    let mut texture = 0u32;
    unsafe {
        // clear anything the renderer left behind, so the check below is ours
        while glGetError() != 0 {}
        glGenTextures(1, &mut texture);
        glBindTexture(target, texture);
        (ext.image_target_texture_2d)(target, image);
        // An external texture has no mip levels and cannot repeat, so these
        // are the only sampler states it accepts. Setting them here rather
        // than per frame is why the cache exists at all.
        glTexParameteri(target, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(target, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(target, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(target, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glBindTexture(target, 0);
    }
    let err = unsafe { glGetError() };
    if err != 0 || texture == 0 {
        warn!(
            err,
            "android ahb lane: binding the EGLImage to a texture failed"
        );
        unsafe {
            if texture != 0 {
                glDeleteTextures(1, &texture);
            }
            (ext.destroy_image)(display, image);
        }
        return None;
    }

    // The lane's own gralloc reference. The pool may hand this buffer back to
    // the decoder and even be destroyed; the EGLImage keeps sampling what it
    // was built from until the entry is evicted.
    unsafe { ndk_sys::AHardwareBuffer_acquire(buffer) };

    if cache.entries.len() >= SLOTS {
        evict_coldest(&mut cache, display, ext);
    }
    let import = Import { texture, target };
    cache.entries.push(Entry {
        buffer,
        image,
        import,
        last_used: clock,
    });
    Some(import)
}

fn evict_coldest(cache: &mut Cache, display: EGLDisplay, ext: &Ext) {
    let Some(index) = cache
        .entries
        .iter()
        .enumerate()
        .min_by_key(|(_, e)| e.last_used)
        .map(|(i, _)| i)
    else {
        return;
    };
    let entry = cache.entries.swap_remove(index);
    unsafe {
        glDeleteTextures(1, &entry.import.texture);
        (ext.destroy_image)(display, entry.image);
        ndk_sys::AHardwareBuffer_release(entry.buffer);
    }
}

/// Drops every import, for a stream ending or a pool being replaced.
///
/// # Safety
///
/// Same as [`import`]: the renderer's GL context must be current, because
/// this is where the textures and images are deleted.
pub unsafe fn clear() {
    let Some(ext) = ext() else { return };
    let Ok(mut cache) = CACHE.lock() else { return };
    if cache.entries.is_empty() {
        return;
    }
    let display = unsafe { eglGetCurrentDisplay() };
    for entry in cache.entries.drain(..) {
        unsafe {
            glDeleteTextures(1, &entry.import.texture);
            if !display.is_null() {
                (ext.destroy_image)(display, entry.image);
            }
            ndk_sys::AHardwareBuffer_release(entry.buffer);
        }
    }
}
