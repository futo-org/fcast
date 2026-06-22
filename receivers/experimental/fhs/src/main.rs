use anyhow::{Result, anyhow};
use fiatlux::*;
use mimalloc::MiMalloc;
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

mod pixmap_video_sink;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

struct FiatLuxGlContext {
    gc: GraphicsContext,
    render_buffer: *mut fl_RenderBuffer,
}

unsafe impl femtovg_renderer::OpenGLInterface for FiatLuxGlContext {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        unsafe {
            fl_egl_window_framebuffer_make_context_active(self.render_buffer);
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
        Ok(())
    }

    fn get_proc_address(&self, name: &std::ffi::CStr) -> *const std::ffi::c_void {
        unsafe {
            let egl = fl_graphics_context_get_egl(self.gc.gc);
            match fl_egl_get_proc_address_func(egl.as_mut().unwrap()) {
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
    client: Client,
    fl_window: Window,
    render_buffer: *mut fl_RenderBuffer,
}

impl FiatLuxWindowAdapter {
    pub fn new() -> Result<Rc<Self>> {
        let window_identifier = CString::new("org.futo.Receiver")?;
        let window_title = CString::new("fcast-receiver")?;

        let client = Client::new()?;
        let gc = GraphicsContext::new(&client)?;
        let fl_window = Window::new(&client, window_identifier.as_ptr(), window_title.as_ptr())?;

        let render_buffer = unsafe {
            // Slint needs a stencil buffer to render non-rectangular shapes correctly
            let opts = fl_WindowFramebufferOpts { stencil_size: 8 };
            fl_egl_create_window_framebuffer_with_opts(
                fl_graphics_context_get_egl(gc.gc),
                client.client,
                fl_window.window_id,
                fl_window.width,
                fl_window.height,
                fl_PixmapFormat_FL_PIXMAP_FORMAT_RGBA8,
                &opts,
            )
            .as_mut()
            .expect("Failed to create window framebuffer")
        };

        unsafe {
            fl_egl_window_framebuffer_make_context_active(render_buffer);
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

    // Returns the render damage sequence or 0 if no rendering was performed
    pub fn draw_if_needed(
        &self,
        render_callback: impl FnOnce(&femtovg_renderer::FemtoVGRenderer) -> u32,
    ) -> u32 {
        if self.needs_redraw.replace(false) {
            render_callback(&self.renderer)
        } else {
            0
        }
    }

    pub fn set_size(&self, size: impl Into<slint::WindowSize>) {
        self.window.set_size(size);
    }

    pub fn set_scale(&self, scale_factor: f32) {
        self.window
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor });
        self.window.set_size(self.size.get());
    }

    pub fn window_active_changed(&self, active: bool) {
        self.window
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(active));
    }

    pub fn pointer_moved(&self, position: slint::LogicalPosition) {
        self.window
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    }

    pub fn pointer_button(
        &self,
        position: slint::LogicalPosition,
        button: slint::platform::PointerEventButton,
        pressed: bool,
    ) {
        if pressed {
            self.window
                .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
        } else {
            self.window
                .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
        }
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
            fl_egl_destroy_window_framebuffer(self.render_buffer);
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

struct LoopProxy {
    job_sender: mpsc::Sender<Job>,
    quit_event_loop: Arc<AtomicBool>,
}

impl LoopProxy {
    pub fn new(job_sender: mpsc::Sender<Job>, quit_event_loop: Arc<AtomicBool>) -> Self {
        Self {
            job_sender: job_sender,
            quit_event_loop: quit_event_loop,
        }
    }
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
    video_surface_id: fl_protocol_SurfaceId,
}

impl FiatLuxPlatform {
    pub fn new() -> Result<Self> {
        let window = FiatLuxWindowAdapter::new()?;
        let video_surface_id = unsafe {
            let mut reply: fl_reply_CreateSurface = std::mem::zeroed();
            if !fl_receive_reply_create_surface(
                window.client.client,
                fl_create_surface(window.client.client, window.fl_window.window_id, -1),
                &mut reply,
            ) {
                return Err(anyhow!("Failed to create video surface"));
            }
            reply.surface_id
        };

        let (job_sender, job_receiver) = mpsc::channel::<Job>();

        Ok(Self {
            window,
            timer: Instant::now(),
            job_sender,
            job_receiver,
            quit_event_loop: Arc::new(AtomicBool::new(false)),
            video_surface_id,
        })
    }

    fn fl_pointer_button_to_slint_pointer_button(
        fl_button: fl_protocol_PointerButton,
    ) -> slint::platform::PointerEventButton {
        match fl_button {
            fl_protocol_PointerButton_fl_protocol_PointerButton_button1 => {
                slint::platform::PointerEventButton::Left
            }
            fl_protocol_PointerButton_fl_protocol_PointerButton_button2 => {
                slint::platform::PointerEventButton::Middle
            }
            fl_protocol_PointerButton_fl_protocol_PointerButton_button3 => {
                slint::platform::PointerEventButton::Right
            }
            _ => slint::platform::PointerEventButton::Other,
        }
    }

    fn handle_events(&self) {
        let mut new_pointer_position: Option<slint::LogicalPosition> = None;

        loop {
            unsafe {
                let mut poll_event_res = fl_poll_event_result_fl_poll_event_success;
                let event =
                    match fl_poll_events(self.window.client.client, 0.0, &mut poll_event_res)
                        .as_mut()
                    {
                        Some(e) => e,
                        None => break,
                    };

                const WINDOW_RESIZED: u8 =
                    fl_protocol_EventType_fl_protocol_EventType_window_resized as u8;
                const DISPLAY_SCALE_NOTIFY: u8 =
                    fl_protocol_EventType_fl_protocol_EventType_display_scale_notify as u8;
                const WINDOW_VISIBILITY_CHANGED: u8 =
                    fl_protocol_EventType_fl_protocol_EventType_window_visibility_changed as u8;
                const POINTER_MOVED: u8 =
                    fl_protocol_EventType_fl_protocol_EventType_pointer_moved as u8;
                const POINTER_BUTTON: u8 =
                    fl_protocol_EventType_fl_protocol_EventType_pointer_button as u8;

                match event.header.event_type {
                    WINDOW_RESIZED => {
                        fl_egl_window_framebuffer_resize(
                            self.window.render_buffer,
                            event.window_resized.width,
                            event.window_resized.height,
                        );
                        self.window.set_size(slint::PhysicalSize::new(
                            event.window_resized.width,
                            event.window_resized.height,
                        ));
                    }
                    DISPLAY_SCALE_NOTIFY => {
                        self.window
                            .set_scale(event.display_scale_notify.display_scale);
                    }
                    WINDOW_VISIBILITY_CHANGED => {
                        self.window
                            .window_active_changed(event.window_visibility_changed.visible);
                    }
                    POINTER_MOVED => {
                        new_pointer_position = Some(slint::LogicalPosition {
                            x: event.pointer_moved.abs_x as f32,
                            y: event.pointer_moved.abs_y as f32,
                        });
                    }
                    POINTER_BUTTON => {
                        self.window.pointer_button(
                            slint::LogicalPosition {
                                x: event.pointer_button.abs_x as f32,
                                y: event.pointer_button.abs_y as f32,
                            },
                            FiatLuxPlatform::fl_pointer_button_to_slint_pointer_button(event.pointer_button.button as fl_protocol_PointerButton),
                            event.pointer_button.state as fl_protocol_PointerButtonState == fl_protocol_PointerButtonState_fl_protocol_PointerButtonState_pressed,
                        );
                    }
                    _ => {}
                }

                fl_free_event(event);
            }
        }

        if let Some(new_pointer_position) = new_pointer_position {
            self.window.pointer_moved(new_pointer_position);
        }
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
        self.window.set_scale(self.window.fl_window.display_scale);
        self.window.set_size(slint::PhysicalSize::new(
            self.window.fl_window.width,
            self.window.fl_window.height,
        ));

        loop {
            slint::platform::update_timers_and_animations();

            unsafe {
                if !fl_is_connected_to_server(self.window.client.client) {
                    self.quit_event_loop.store(true, Ordering::Relaxed);
                }
            }

            while let Ok(job) = self.job_receiver.try_recv() {
                job();
            }

            if self.quit_event_loop.load(Ordering::Relaxed) {
                break;
            }

            self.handle_events();

            let damage_seq = self.window.draw_if_needed(|renderer| {
                renderer.render().unwrap();

                unsafe {
                    let ui_surface_id =
                        fl_egl_window_framebuffer_swap_buffers(self.window.render_buffer);
                    let surface_ids = [self.video_surface_id, ui_surface_id];
                    let damage_seq = fl_mark_surfaces_as_damaged(
                        self.window.client.client,
                        surface_ids.as_ptr(),
                        surface_ids.len(),
                    )
                    .value;
                    fl_discard_reply(self.window.client.client, damage_seq);
                    damage_seq
                }
            });

            unsafe {
                fl_wait_for_vsync_finished(self.window.client.client, damage_seq, 0.15);
            };
        }

        return Ok(());
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        Some(Box::new(LoopProxy::new(
            self.job_sender.clone(),
            self.quit_event_loop.clone(),
        )))
    }
}

fn main() -> Result<()> {
    let cli_args = rcore::CliArgs::parse();
    let platform = FiatLuxPlatform::new()?;
    let client_ptr = platform.window.client.client;
    let video_surface_id = platform.video_surface_id;

    slint::platform::set_platform(Box::new(platform))?;

    rcore::run(
        cli_args,
        pixmap_video_sink::FhsPixmapSink::new(client_ptr, video_surface_id)?,
    )
}
