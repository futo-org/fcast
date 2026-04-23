use std::{ffi::CStr, os::raw::c_void};

use libplacebo_sys::*;

pub use libplacebo_sys;

extern "C" fn on_pl_log(
    _priv: *mut std::os::raw::c_void,
    level: pl_log_level,
    msg: *const std::os::raw::c_char,
) {
    let Ok(msg) = unsafe { CStr::from_ptr(msg) }.to_str() else {
        tracing::error!("Got invalid UTF-8 log message from libplacebo");
        return;
    };

    macro_rules! event {
        ($level:expr) => {
            tracing::event!(target: "libplacebo", $level, "{msg}")
        }
    }

    match level {
        pl_log_level::PL_LOG_NONE => event!(tracing::Level::TRACE),
        pl_log_level::PL_LOG_FATAL | pl_log_level::PL_LOG_ERR => event!(tracing::Level::ERROR),
        pl_log_level::PL_LOG_WARN => event!(tracing::Level::WARN),
        pl_log_level::PL_LOG_INFO => event!(tracing::Level::INFO),
        pl_log_level::PL_LOG_DEBUG => event!(tracing::Level::DEBUG),
        pl_log_level::PL_LOG_TRACE => event!(tracing::Level::TRACE),
    }
}

pub struct Log {
    log: pl_log,
}

impl Log {
    pub fn new() -> Option<Self> {
        unsafe {
            let log = pl_log_create_360(
                PL_API_VER as i32,
                &pl_log_params {
                    log_cb: Some(on_pl_log),
                    log_priv: std::ptr::null_mut(),
                    log_level: libplacebo_sys::pl_log_level::PL_LOG_DEBUG,
                } as *const _,
            );

            if log.is_null() {
                return None;
            }

            Some(Self { log })
        }
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        unsafe {
            pl_log_destroy(&mut self.log);
        }
    }
}

// TODO: rename to Gpu?
pub struct OpenGL {
    pub gl: *const pl_opengl_t,
}

impl OpenGL {
    pub fn new(log: &Log) -> Option<Self> {
        unsafe { Self::new_egl(log, std::ptr::null_mut(), std::ptr::null_mut()) }
    }

    pub unsafe fn new_egl(
        log: &Log,
        egl_display: *mut c_void,
        egl_context: *mut c_void,
    ) -> Option<Self> {
        unsafe {
            let opengl = pl_opengl_create(
                log.log,
                &pl_opengl_params {
                    // TODO: use this
                    get_proc_addr_ex: None,
                    proc_ctx: std::ptr::null_mut(),
                    get_proc_addr: None,
                    debug: true,
                    allow_software: true,
                    no_compute: false,
                    max_glsl_version: 0,
                    egl_display,
                    egl_context,
                    make_current: None,
                    release_current: None,
                    priv_: std::ptr::null_mut(),
                } as *const _,
            );

            if opengl.is_null() {
                return None;
            }

            Some(Self { gl: opengl })
        }
    }

    pub unsafe fn gpu(&self) -> *const pl_gpu_t {
        unsafe { (*self.gl).gpu }
    }
}

impl Drop for OpenGL {
    fn drop(&mut self) {
        unsafe {
            pl_opengl_destroy(&mut self.gl);
        }
    }
}

pub struct Swapchain {
    swapchain: *const pl_swapchain_t,
}

impl Swapchain {
    pub fn new(opengl: &OpenGL) -> Option<Self> {
        unsafe {
            let swapchain = pl_opengl_create_swapchain(
                opengl.gl,
                &pl_opengl_swapchain_params {
                    swap_buffers: None,
                    framebuffer: pl_opengl_framebuffer {
                        id: 0,
                        flipped: false,
                    },
                    max_swapchain_depth: 0,
                    priv_: std::ptr::null_mut(),
                } as *const _,
            );

            if swapchain.is_null() {
                return None;
            }

            Some(Self { swapchain })
        }
    }

    pub fn resize(&self, mut width: i32, mut height: i32) {
        unsafe {
            pl_swapchain_resize(self.swapchain, &mut width, &mut height);
        }
    }

    pub fn swap_buffers(&self) {
        unsafe {
            pl_swapchain_swap_buffers(self.swapchain);
        }
    }

    pub fn start_frame(&self) -> Option<SwapchainFrame> {
        unsafe {
            let mut frame = std::mem::zeroed();
            if pl_swapchain_start_frame(self.swapchain, &mut frame) {
                Some(SwapchainFrame { frame })
            } else {
                None
            }
        }
    }

    pub fn submit_frame(&self) -> bool {
        unsafe {
            pl_swapchain_submit_frame(self.swapchain)
        }
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        unsafe {
            pl_swapchain_destroy(&mut self.swapchain);
        }
    }
}

pub struct SwapchainFrame {
    pub frame: pl_swapchain_frame,
}

pub struct Renderer {
    pub renderer: *mut pl_renderer_t,
}

impl Renderer {
    pub fn new(log: &Log, opengl: &OpenGL) -> Option<Self> {
        unsafe {
            let renderer = pl_renderer_create(log.log, (*opengl.gl).gpu);
            if renderer.is_null() {
                return None;
            }

            Some(Self { renderer })
        }
    }

    pub fn flush_cache(&self) {
        unsafe {
            pl_renderer_flush_cache(self.renderer);
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            pl_renderer_destroy(&mut self.renderer);
        }
    }
}

pub fn new_plane() -> pl_plane {
    pl_plane {
        texture: std::ptr::null(),
        address_mode: pl_tex_address_mode::PL_TEX_ADDRESS_CLAMP,
        flipped: false,
        components: -1,
        component_mapping: [-1; 4],
        shift_x: 0.0,
        shift_y: 0.0,
    }
}

pub fn scale_and_fit(target: &pl_rect2df, frame: &pl_rect2df) -> pl_rect2df {
    let frame_aspect = unsafe { pl_rect2df_aspect(frame) };
    let target_width = target.x1 - target.x0;
    let target_height = target.y1 - target.y0;
    let target_aspect = unsafe { pl_rect2df_aspect(target) };

    let (fit_width, fit_height) = if frame_aspect > target_aspect {
        // scale source to target width
        let w = target_width;
        let h = w / frame_aspect;
        (w, h)
    } else {
        // scale source to target height
        let h = target_height;
        let w = h * frame_aspect;
        (w, h)
    };

    let offset_x = (target_width - fit_width) / 2.0;
    let offset_y = (target_height - fit_height) / 2.0;

    pl_rect2df {
        x0: target.x0 + offset_x,
        y0: target.y0 + offset_y,
        x1: target.x0 + offset_x + fit_width,
        y1: target.y0 + offset_y + fit_height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! rect2df {
        ($width:expr, $height:expr) => {
            pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: $width,
                y1: $height,
            }
        };
    }

    fn assert_rect_eq(a: pl_rect2df, b: pl_rect2df) {
        assert!(
            a.x0 == b.x0 && a.y0 == b.y0 && a.x1 == b.x1 && a.y1 == b.y1,
            "a={a:?} != b={b:?}"
        );
    }

    #[test]
    fn test_scale_and_fit() {
        assert_rect_eq(
            scale_and_fit(&rect2df!(1920.0, 1080.0), &rect2df!(1280.0, 720.0)),
            rect2df!(1920.0, 1080.0),
        );

        assert_rect_eq(
            scale_and_fit(&rect2df!(1915.0, 1075.0), &rect2df!(300.0, 500.0)),
            pl_rect2df {
                x0: 635.0,
                y0: 0.0,
                x1: 1280.0,
                y1: 1075.0,
            },
        );
    }
}
