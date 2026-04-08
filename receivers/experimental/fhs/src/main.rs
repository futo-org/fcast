mod fiatlux;

use anyhow::Result;
use rcore::{
    clap::Parser,
    slint::{self, platform::femtovg_renderer},
};
use std::{
    cell::Cell,
    ffi::CString,
    num::NonZeroU32,
    ptr::null,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

struct FiatLuxGlContext {
    gc: fiatlux::GraphicsContext,
    render_buffer: *mut fiatlux::fl_RenderBuffer,
}

unsafe impl femtovg_renderer::OpenGLInterface for FiatLuxGlContext {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        unsafe {
            fiatlux::fl_egl_window_framebuffer_make_context_active(self.render_buffer);
        }
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    render_buffer: *mut fiatlux::fl_RenderBuffer,
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
            fiatlux::fl_egl_create_window_framebuffer(
                fiatlux::fl_graphics_context_get_egl(gc.gc),
                client.client,
                fiatlux::fl_graphics_context_get_egl_config(gc.gc),
                fiatlux::fl_graphics_context_get_egl_context(gc.gc),
                fl_window.window_id,
                fl_window.width,
                fl_window.height,
            )
            .as_mut()
            .expect("Failed to create window framebuffer")
        };

        unsafe {
            fiatlux::fl_egl_window_framebuffer_make_context_active(render_buffer);
        }

        Ok(Rc::new_cyclic(|w: &Weak<Self>| Self {
            window: slint::Window::new(w.clone()),
            renderer: femtovg_renderer::FemtoVGRenderer::new(FiatLuxGlContext {
                gc: gc,
                render_buffer: render_buffer,
            })
            .unwrap(),
            needs_redraw: Default::default(),
            size: Default::default(),
            client: client,
            fl_window: fl_window,
            render_buffer: render_buffer,
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

impl Drop for FiatLuxWindowAdapter {
    fn drop(&mut self) {
        unsafe {
            fiatlux::fl_egl_destroy_window_framebuffer(self.render_buffer);
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

struct LoopProxy {
    job_sender: mpsc::Sender<Job>,
    quit_event_loop: Arc<AtomicBool>,
}

impl slint::platform::EventLoopProxy for LoopProxy {
    fn quit_event_loop(&self) -> Result<(), rcore::slint::EventLoopError> {
        self.quit_event_loop.store(true, Ordering::Relaxed);
        // Wake up the event loop by sending an empty job
        self.job_sender
            .send(Box::new(move || {}))
            .map_err(|_| rcore::slint::EventLoopError::EventLoopTerminated)
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), rcore::slint::EventLoopError> {
        self.job_sender
            .send(event)
            .map_err(|_| rcore::slint::EventLoopError::EventLoopTerminated)
    }
}

struct FiatLuxPlatform {
    window: Rc<FiatLuxWindowAdapter>,
    timer: Instant,
    job_sender: mpsc::Sender<Job>,
    job_receiver: mpsc::Receiver<Job>,
    quit_event_loop: Arc<AtomicBool>,
}

impl FiatLuxPlatform {
    pub fn new() -> Result<Self> {
        let (job_sender, job_receiver) = mpsc::channel::<Job>();
        Ok(Self {
            window: FiatLuxWindowAdapter::new()?,
            timer: Instant::now(),
            job_sender: job_sender,
            job_receiver: job_receiver,
            quit_event_loop: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl slint::platform::Platform for FiatLuxPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
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
            if self.window.window.has_active_animations() {
                slint::platform::update_timers_and_animations();
            }

            unsafe {
                if !fiatlux::fl_is_connected_to_server(self.window.client.client) {
                    self.quit_event_loop.store(true, Ordering::Relaxed);
                }
            }

            while let Ok(job) = self.job_receiver.try_recv() {
                job();
            }

            if self.quit_event_loop.load(Ordering::Relaxed) {
                break;
            }

            loop {
                unsafe {
                    let mut poll_event_res = fiatlux::fl_poll_event_result_fl_poll_event_success;
                    let event = match fiatlux::fl_poll_events(
                        self.window.client.client,
                        0.0,
                        &mut poll_event_res,
                    )
                    .as_mut()
                    {
                        Some(e) => e,
                        None => break,
                    };

                    const WINDOW_RESIZED: u8 =
                        fiatlux::fl_protocol_EventType_fl_protocol_EventType_window_resized as u8;
                    match event.header.event_type {
                        WINDOW_RESIZED => {
                            fiatlux::fl_egl_window_framebuffer_resize(
                                self.window.render_buffer,
                                event.window_resized.width,
                                event.window_resized.height,
                            );
                            self.window.set_size(slint::PhysicalSize::new(
                                event.window_resized.width,
                                event.window_resized.height,
                            ));
                        }
                        _ => {}
                    }

                    fiatlux::fl_free_event(event);
                }
            }

            self.window.draw_if_needed(|renderer| {
                renderer.render().unwrap();
                unsafe {
                    fiatlux::fl_inhibit_idle(self.window.client.client);
                }
            });

            unsafe {
                fiatlux::fl_egl_window_framebuffer_present_framebuffer_wait_for_vsync(
                    self.window.render_buffer,
                    3.0,
                );
            }
        }

        return Ok(());
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        Some(Box::new(LoopProxy {
            job_sender: self.job_sender.clone(),
            quit_event_loop: self.quit_event_loop.clone(),
        }))
    }
}

fn main() -> Result<()> {
    let args = rcore::CliArgs::parse();
    slint::platform::set_platform(Box::new(FiatLuxPlatform::new()?))?;
    rcore::run(args)
}
