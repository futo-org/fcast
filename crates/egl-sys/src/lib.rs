#![allow(non_camel_case_types)]
#![allow(unsafe_op_in_unsafe_fn)]
// Lints
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::missing_transmute_annotations)]

use std::ffi::{CString, c_long, c_void};

pub type khronos_utime_nanoseconds_t = khronos_uint64_t;
pub type khronos_uint64_t = u64;
pub type khronos_ssize_t = c_long;
pub type EGLint = i32;
pub type EGLNativeDisplayType = NativeDisplayType;
pub type EGLNativePixmapType = NativePixmapType;
pub type EGLNativeWindowType = NativeWindowType;
pub type NativeDisplayType = *const c_void;
pub type NativePixmapType = *const c_void;
pub type NativeWindowType = *const c_void;

pub mod bindings {
    // https://github.com/Smithay/smithay/blob/27af99ef492ab4d7dc5cd2e625374d2beb2772f7/src/backend/egl/ffi.rs

    use super::*;

    use libloading::Library;
    use std::sync::{LazyLock, Once};

    pub static LIB: LazyLock<Library> =
        LazyLock::new(|| unsafe { Library::new("libEGL.so.1") }.expect("Failed to load LibEGL"));

    pub static LOAD: Once = Once::new();

    include!(concat!(env!("OUT_DIR"), "/egl.rs"));
}

pub fn ensure_init() {
    bindings::LOAD.call_once(|| unsafe {
        bindings::load_with(|sym| {
            let name = CString::new(sym).unwrap();
            let symbol = bindings::LIB.get::<*mut c_void>(name.as_bytes());
            match symbol {
                Ok(x) => *x as *const _,
                Err(_) => std::ptr::null(),
            }
        });
        bindings::load_with(|sym| {
            let addr = CString::new(sym.as_bytes()).unwrap();
            let addr = addr.as_ptr();
            bindings::GetProcAddress(addr) as *const _
        });
    });
}
