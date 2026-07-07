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

#[cfg(feature = "vulkan")]
pub struct Vulkan {
    pub vk: pl_vulkan,
    inst: pl_vk_inst,
    /// The Vulkan loader; its symbols back the instance/device, so it must outlive them.
    _loader: libloading::Library,
}

/// Convert a DRM (major, minor) pair to a `dev_t`, matching glibc's makedev().
#[cfg(feature = "vulkan")]
fn makedev(major: u64, minor: u64) -> u64 {
    ((major & 0xffff_f000) << 32)
        | ((major & 0x0000_0fff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0x0000_00ff)
}

/// Find the physical device whose DRM primary or render node is `target` (a `dev_t`), e.g. the
/// device a Wayland compositor advertised via dmabuf feedback. On multi-GPU systems letting
/// libplacebo pick a device instead can select a GPU whose exported dmabufs (tiling modifiers)
/// the compositor cannot import.
#[cfg(feature = "vulkan")]
unsafe fn find_phys_device_by_drm(inst: pl_vk_inst, target: u64) -> Option<VkPhysicalDevice> {
    unsafe {
        let gpa = (*inst).get_proc_addr?;
        let instance = (*inst).instance;
        let enumerate: PFN_vkEnumeratePhysicalDevices =
            std::mem::transmute(gpa(instance, c"vkEnumeratePhysicalDevices".as_ptr()));
        let get_props2: PFN_vkGetPhysicalDeviceProperties2 =
            std::mem::transmute(gpa(instance, c"vkGetPhysicalDeviceProperties2".as_ptr()));
        let (enumerate, get_props2) = (enumerate?, get_props2?);

        let mut count = 0u32;
        if enumerate(instance, &mut count, std::ptr::null_mut()) != VkResult::VK_SUCCESS {
            return None;
        }
        let mut devices: Vec<VkPhysicalDevice> = vec![std::ptr::null_mut(); count as usize];
        if enumerate(instance, &mut count, devices.as_mut_ptr()) != VkResult::VK_SUCCESS {
            return None;
        }

        for device in devices.into_iter().take(count as usize) {
            let mut drm: VkPhysicalDeviceDrmPropertiesEXT = std::mem::zeroed();
            drm.sType = VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT;
            let mut props: VkPhysicalDeviceProperties2 = std::mem::zeroed();
            props.sType = VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
            props.pNext = &mut drm as *mut _ as *mut c_void;
            // Drivers skip unknown pNext structs, so this stays zeroed (hasPrimary/hasRender
            // false) when VK_EXT_physical_device_drm is unsupported.
            get_props2(device, &mut props);

            let matches = (drm.hasPrimary != 0
                && makedev(drm.primaryMajor as u64, drm.primaryMinor as u64) == target)
                || (drm.hasRender != 0
                    && makedev(drm.renderMajor as u64, drm.renderMinor as u64) == target);
            if matches {
                let name = CStr::from_ptr(props.properties.deviceName.as_ptr());
                tracing::info!(name = ?name, "Matched Vulkan physical device by DRM device");
                return Some(device);
            }
        }
        tracing::warn!(target, "No Vulkan physical device matches the DRM device");
        None
    }
}

#[cfg(feature = "vulkan")]
impl Vulkan {
    /// Create a Vulkan-backed libplacebo context. When `drm_device` (a `dev_t`) is given, the
    /// physical device is chosen by matching its DRM node. Pass the compositor's main device to
    /// guarantee exported dmabufs are importable on multi-GPU systems.
    pub fn new(log: &Log, drm_device: Option<u64>) -> Option<Self> {
        let loader = ["libvulkan.so.1", "libvulkan.so"]
            .into_iter()
            .find_map(|name| unsafe { libloading::Library::new(name) }.ok())?;
        let get_proc_addr: PFN_vkGetInstanceProcAddr = unsafe {
            loader
                .get::<unsafe extern "C" fn()>(b"vkGetInstanceProcAddr\0")
                .ok()
                .map(|sym| std::mem::transmute(sym.into_raw().into_raw()))
        };
        get_proc_addr?;

        unsafe {
            let mut inst_params = pl_vk_inst_default_params;
            inst_params.get_proc_addr = get_proc_addr;
            let mut inst = pl_vk_inst_create(log.log, &inst_params);
            if inst.is_null() {
                return None;
            }

            let mut params = pl_vulkan_default_params;
            params.instance = (*inst).instance;
            params.get_proc_addr = (*inst).get_proc_addr;
            if let Some(target) = drm_device
                && let Some(device) = find_phys_device_by_drm(inst, target)
            {
                params.device = device;
            }
            let vk = pl_vulkan_create(log.log, &params);
            if vk.is_null() {
                pl_vk_inst_destroy(&mut inst);
                return None;
            }

            Some(Self {
                vk,
                inst,
                _loader: loader,
            })
        }
    }

    pub unsafe fn gpu(&self) -> *const pl_gpu_t {
        unsafe { (*self.vk).gpu }
    }
}

#[cfg(feature = "vulkan")]
impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            pl_vulkan_destroy(&mut self.vk);
            pl_vk_inst_destroy(&mut self.inst);
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
        unsafe { pl_swapchain_submit_frame(self.swapchain) }
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
        unsafe { Self::new_from_gpu(log, (*opengl.gl).gpu) }
    }

    pub unsafe fn new_from_gpu(log: &Log, gpu: *const pl_gpu_t) -> Option<Self> {
        unsafe {
            let renderer = pl_renderer_create(log.log, gpu);
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
