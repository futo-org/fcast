mod fiatlux;

use anyhow::{Result, anyhow};
use rcore::{
    clap::Parser,
    slint::{self, platform::femtovg_renderer},
};
use std::{
    cell::{Cell, UnsafeCell},
    ffi::CString,
    num::NonZeroU32,
    ptr::null,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

struct FiatLuxGlContext {
    gc: fiatlux::GraphicsContext,
    render_buffer: UnsafeCell<fiatlux::fl_RenderBuffer>,
}

unsafe impl femtovg_renderer::OpenGLInterface for FiatLuxGlContext {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        unsafe {
            fiatlux::fl_egl_window_framebuffer_make_context_active(self.render_buffer.get());
        }
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        unsafe {
            fiatlux::fl_egl_window_framebuffer_swap(self.render_buffer.get());
        }
        Ok(())
    }

    fn resize(
        &self,
        _width: NonZeroU32,
        _height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // TODO: Resize?
        Ok(())
    }

    fn get_proc_address(&self, name: &std::ffi::CStr) -> *const std::ffi::c_void {
        unsafe {
            let egl = fiatlux::fl_graphics_context_get_egl(self.gc.gc);
            match fiatlux::fl_egl_get_proc_address_func(egl.as_mut().unwrap()) {
                Some(func) => core::mem::transmute(func(name.as_ptr())),
                None => null(),
            }
        }
    }
}

#[allow(unused)]
struct FiatLuxWindowAdapter {
    window: slint::Window,
    renderer: femtovg_renderer::FemtoVGRenderer,
    needs_redraw: Cell<bool>,
    size: Cell<slint::PhysicalSize>,
    client: fiatlux::Client,
    fl_window: fiatlux::Window,
}

impl FiatLuxWindowAdapter {
    pub fn new() -> Result<Rc<Self>> {
        let window_identifier = CString::new("org.futo.Receiver")?;
        let window_title = CString::new("fcast-receiver")?;

        let client = fiatlux::Client::new()?;
        let gc = fiatlux::GraphicsContext::new(&client)?;
        let fl_window =
            fiatlux::Window::new(&client, window_identifier.as_ptr(), window_title.as_ptr())?;

        let render_buffer = unsafe {
            let mut render_buffer: fiatlux::fl_RenderBuffer = std::mem::zeroed();
            let window_fb_created = fiatlux::fl_egl_create_window_framebuffer(
                fiatlux::fl_graphics_context_get_egl(gc.gc),
                client.client,
                fiatlux::fl_graphics_context_get_egl_config(gc.gc),
                fiatlux::fl_graphics_context_get_egl_context(gc.gc),
                fl_window.window_id,
                fl_window.width,
                fl_window.height,
                &mut render_buffer,
            );
            if !window_fb_created {
                return Err(anyhow!("Failed to create window framebuffer"));
            }
            render_buffer
        };

        Ok(Rc::new_cyclic(|w: &Weak<Self>| Self {
            window: slint::Window::new(w.clone()),
            renderer: femtovg_renderer::FemtoVGRenderer::new(FiatLuxGlContext {
                gc: gc,
                render_buffer: UnsafeCell::new(render_buffer),
            })
            .unwrap(),
            needs_redraw: Default::default(),
            size: Default::default(),
            client: client,
            fl_window: fl_window,
        }))
    }

    pub fn draw_if_needed(
        &self,
        render_callback: impl FnOnce(&femtovg_renderer::FemtoVGRenderer),
    ) -> bool {
        if self.needs_redraw.replace(false) {
            render_callback(&self.renderer);
            true
        } else {
            false
        }
    }

    // pub async fn draw_async_if_needed(
    //     &self,
    //     render_callback: impl AsyncFnOnce(&femtovg_renderer::FemtoVGRenderer),
    // ) -> bool {
    //     if self.needs_redraw.replace(false) {
    //         render_callback(&self.renderer).await;
    //         true
    //     } else {
    //         false
    //     }
    // }

    pub fn set_size(&self, size: impl Into<slint::WindowSize>) {
        self.window.set_size(size);
    }
}

impl slint::platform::WindowAdapter for FiatLuxWindowAdapter {
    fn window(&self) -> &slint::Window {
        &self.window
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        &self.renderer
    }

    fn size(&self) -> slint::PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: slint::WindowSize) {
        let sf = self.window.scale_factor();
        self.size.set(size.to_physical(sf));
        let logical_size = size.to_logical(sf);
        self.window
            .dispatch_event(slint::platform::WindowEvent::Resized { size: logical_size });
    }

    fn request_redraw(&self) {
        self.needs_redraw.set(true);
    }
}

struct FiatLuxPlatform {
    window: Rc<FiatLuxWindowAdapter>,
    timer: Instant,
}

impl FiatLuxPlatform {
    pub fn new() -> Result<Self> {
        Ok(Self {
            window: FiatLuxWindowAdapter::new()?,
            timer: Instant::now(),
        })
    }
}

impl slint::platform::Platform for FiatLuxPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        println!("create window adapter!");
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.timer.elapsed()
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        self.window.set_size(slint::PhysicalSize::new(
            self.window.fl_window.width,
            self.window.fl_window.height,
        ));

        loop {
            slint::platform::update_timers_and_animations();

            self.window.draw_if_needed(|renderer| {
                renderer.render().unwrap();
            });
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = rcore::CliArgs::parse();

    slint::platform::set_platform(Box::new(FiatLuxPlatform::new()?)).unwrap();
    // if std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
    //     rcore::slint::BackendSelector::new()
    //         .require_opengl()
    //         .select()?;
    // }

    rcore::run(args)
}
