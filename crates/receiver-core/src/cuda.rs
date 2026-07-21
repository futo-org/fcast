//! App-side CUDA context provisioning + access. Every CUDA GStreamer element
//! (NVDEC decoders, cudaconvert, ...) must share a single `GstCudaContext`;
//! the CUDA<->Vulkan interop copy also runs under it. We create it once
//! (device 0 = the NVIDIA GPU) and hand the same one to every element that
//! posts `NEED_CONTEXT`, and expose push/pop for our own CUDA calls.
//!
//! Only compiled with the `fhs` feature; `libgstcuda-1.0` is `dlopen`ed at
//! runtime so the binary still links/runs without it (AMD/Intel).

use std::ffi::c_void;
use std::sync::OnceLock;

use gst::prelude::*;

/// Context type string CUDA elements query for (`GST_CUDA_CONTEXT_TYPE`).
pub const CUDA_CONTEXT_TYPE: &str = "gst.cuda.context";

struct GstCuda {
    load_library: unsafe extern "C" fn() -> i32,
    context_new: unsafe extern "C" fn(u32) -> *mut c_void,
    context_new_cuda_context: unsafe extern "C" fn(*mut c_void) -> *mut gst::ffi::GstContext,
    context_push: unsafe extern "C" fn(*mut c_void) -> i32,
    context_pop: unsafe extern "C" fn(*mut *mut c_void) -> i32,
}
unsafe impl Send for GstCuda {}
unsafe impl Sync for GstCuda {}

static GSTCUDA: OnceLock<Option<GstCuda>> = OnceLock::new();

fn gstcuda() -> Option<&'static GstCuda> {
    GSTCUDA
        .get_or_init(|| unsafe {
            let lib = libloading::Library::new("libgstcuda-1.0.so.0")
                .or_else(|_| libloading::Library::new("libgstcuda-1.0.so"))
                .ok()?;
            let g = GstCuda {
                load_library: *lib.get(b"gst_cuda_load_library\0").ok()?,
                context_new: *lib.get(b"gst_cuda_context_new\0").ok()?,
                context_new_cuda_context: *lib.get(b"gst_context_new_cuda_context\0").ok()?,
                context_push: *lib.get(b"gst_cuda_context_push\0").ok()?,
                context_pop: *lib.get(b"gst_cuda_context_pop\0").ok()?,
            };
            std::mem::forget(lib);
            Some(g)
        })
        .as_ref()
}

unsafe fn gst_cuda_load_library() -> i32 {
    match gstcuda() {
        Some(g) => unsafe { (g.load_library)() },
        None => 0,
    }
}
unsafe fn gst_cuda_context_new(device_id: u32) -> *mut c_void {
    match gstcuda() {
        Some(g) => unsafe { (g.context_new)(device_id) },
        None => std::ptr::null_mut(),
    }
}
unsafe fn gst_context_new_cuda_context(cuda_ctx: *mut c_void) -> *mut gst::ffi::GstContext {
    match gstcuda() {
        Some(g) => unsafe { (g.context_new_cuda_context)(cuda_ctx) },
        None => std::ptr::null_mut(),
    }
}
unsafe fn gst_cuda_context_push(ctx: *mut c_void) -> i32 {
    match gstcuda() {
        Some(g) => unsafe { (g.context_push)(ctx) },
        None => 0,
    }
}
unsafe fn gst_cuda_context_pop(cuda_ctx: *mut *mut c_void) -> i32 {
    match gstcuda() {
        Some(g) => unsafe { (g.context_pop)(cuda_ctx) },
        None => 0,
    }
}

struct Shared {
    gst_context: gst::Context,
    /// `GstCudaContext *` — kept alive (extra ref) for push/pop.
    cuda_context: *mut c_void,
}
// The GstCudaContext is thread-safe (GstObject); we only ever push/pop it
// under our own synchronization.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

static SHARED: OnceLock<Option<Shared>> = OnceLock::new();

fn shared() -> Option<&'static Shared> {
    SHARED
        .get_or_init(|| unsafe {
            if gst_cuda_load_library() == 0 {
                return None;
            }
            let cuda_context = gst_cuda_context_new(0);
            if cuda_context.is_null() {
                return None;
            }
            let raw = gst_context_new_cuda_context(cuda_context);
            if raw.is_null() {
                return None;
            }
            // `gst_context_new_cuda_context` takes its own ref; we deliberately
            // keep ours (`cuda_context`) alive for push/pop rather than unref.
            let gst_context: gst::Context = gst::glib::translate::from_glib_full(raw);
            Some(Shared {
                gst_context,
                cuda_context,
            })
        })
        .as_ref()
}

/// Provide the shared CUDA context to an element that posted `NEED_CONTEXT`.
pub fn provide_cuda_context(element: &gst::Element) {
    if let Some(s) = shared() {
        element.set_context(&s.gst_context);
    }
}

/// The shared CUDA `GstContext`, to set on the pipeline up front.
pub fn context() -> Option<gst::Context> {
    shared().map(|s| s.gst_context.clone())
}

/// Push the shared CUDA context current; pops on drop.
pub fn push() -> Option<PushGuard> {
    let s = shared()?;
    if unsafe { gst_cuda_context_push(s.cuda_context) } == 0 {
        return None;
    }
    Some(PushGuard { _priv: () })
}

pub struct PushGuard {
    _priv: (),
}

impl Drop for PushGuard {
    fn drop(&mut self) {
        let mut dummy: *mut c_void = std::ptr::null_mut();
        unsafe {
            gst_cuda_context_pop(&mut dummy);
        }
    }
}
