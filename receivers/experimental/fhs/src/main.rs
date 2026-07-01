use anyhow::{Result, anyhow};
use fiatlux::*;
use mimalloc::MiMalloc;
use rcore::{
    VideoSink, clap::Parser, egl, glow, libplacebo, placebo::PlaceboContext, tracing::error,
    video::Frame,
};
use std::{
    ffi::{CString, c_char, c_void},
    ptr::null,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

mod pixmap_video_sink;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

unsafe fn gl_load_proc(egl: *mut fl_Egl, name: *const c_char) -> *const c_void {
    unsafe {
        match fl_egl_get_proc_address_func(egl) {
            Some(func) => core::mem::transmute(func(name)),
            None => null(),
        }
    }
}

struct FiatLux {
    client: Client,
    gc: GraphicsContext,
    window: Window,
    video_surface_id: fl_protocol_SurfaceId,
}

impl FiatLux {
    fn new() -> Result<Self> {
        let identifier = CString::new("org.futo.FCastReceiver")?;
        let title = CString::new("fcast-receiver")?;

        let client = Client::new()?;
        let gc = GraphicsContext::new(&client)?;
        let window = Window::new(&client, identifier.as_ptr(), title.as_ptr())?;

        let video_surface_id = unsafe {
            let mut reply: fl_reply_CreateSurface = std::mem::zeroed();
            if !fl_receive_reply_create_surface(
                client.client,
                fl_create_surface(client.client, window.window_id, -1),
                &mut reply,
            ) {
                return Err(anyhow!("Failed to create video surface"));
            }
            reply.surface_id
        };

        Ok(Self {
            client,
            gc,
            window,
            video_surface_id,
        })
    }
}

struct FrameSignal {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl FrameSignal {
    fn new() -> Self {
        Self {
            inner: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn notifier(&self) -> Arc<dyn Fn() + Send + Sync> {
        let inner = self.inner.clone();
        Arc::new(move || {
            let (lock, cvar) = &*inner;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        })
    }

    fn wait_timeout(&self, timeout: Duration) {
        let (lock, cvar) = &*self.inner;
        let mut ready = lock.lock().unwrap();
        if !*ready {
            ready = cvar.wait_timeout(ready, timeout).unwrap().0;
        }
        *ready = false;
    }
}

fn main() -> Result<()> {
    let cli_args = rcore::CliArgs::parse();

    let fl = FiatLux::new()?;
    let fl_egl = unsafe { fl_graphics_context_get_egl(fl.gc.gc) };
    let fl_egl_display = unsafe { fl_graphics_context_get_egl_display(fl.gc.gc) };
    let opts = fl_WindowFramebufferOpts { stencil_size: 0 };
    let mut egl_config: EGLConfig = std::ptr::null_mut();
    let egl_context = unsafe {
        fl_egl_create_context(
            fl_egl,
            fl.client.client,
            fl_PixmapFormat_FL_PIXMAP_FORMAT_RGBA8,
            &opts,
            &mut egl_config,
        )
    };
    if egl_context.is_null() {
        return Err(anyhow!("Failed to create EGL context"));
    }

    egl::ensure_init();

    let glutin_egl = glutin_egl_sys::egl::Egl::load_with(|symbol| {
        let symbol = CString::new(symbol).unwrap();
        unsafe { gl_load_proc(fl_egl, symbol.as_ptr()) }
    });

    unsafe {
        glutin_egl.MakeCurrent(
            fl_egl_display,
            std::ptr::null(),
            std::ptr::null(),
            egl_context,
        );
    }

    let pl_log =
        libplacebo::Log::new().ok_or_else(|| anyhow!("failed to create libplacebo log"))?;
    let render_opts = cli_args.rendering_options();
    let mut placebo =
        unsafe { PlaceboContext::new_egl(&pl_log, &render_opts, fl_egl_display, egl_context)? };

    let gl =
        unsafe { glow::Context::from_loader_function_cstr(|s| gl_load_proc(fl_egl, s.as_ptr())) };

    let drm_formats = egl::get_supported_dma_drm_formats(fl_egl_display)?;

    let mut sink = pixmap_video_sink::FhsPixmapSink::new(fl.client.client, fl.video_surface_id)?;

    let signal = FrameSignal::new();
    let handle = rcore::run_with_external_video(cli_args, signal.notifier())?;
    handle.set_drm_formats(drm_formats);
    handle.set_gui_visible(true);

    let mut cached_frame: Option<Frame> = None;
    let mut size = (fl.window.width, fl.window.height);
    handle.set_window_resolution(size.0, size.1);

    loop {
        signal.wait_timeout(Duration::from_millis(150));

        let mut resized = false;
        loop {
            let mut res = fl_poll_event_result_fl_poll_event_success;
            let event = match unsafe { fl_poll_events(fl.client.client, 0.0, &mut res).as_mut() } {
                Some(event) => event,
                None => break,
            };

            const WINDOW_RESIZED: u8 =
                fl_protocol_EventType_fl_protocol_EventType_window_resized as u8;
            if unsafe { event.header.event_type } == WINDOW_RESIZED {
                let (width, height) =
                    unsafe { (event.window_resized.width, event.window_resized.height) };
                size = (width, height);
                resized = true;
            }

            unsafe {
                fl_free_event(event);
            }
        }

        if resized {
            handle.set_window_resolution(size.0, size.1);
        }

        if handle.should_quit() || unsafe { !fl_is_connected_to_server(fl.client.client) } {
            break;
        }

        let mut have_new = false;
        match handle.take_payload() {
            None => {}
            Some(Some(frame)) => {
                cached_frame = Some(frame);
                have_new = true;
            }
            Some(None) => cached_frame = None,
        }

        if (have_new || resized)
            && let Some(frame) = cached_frame.as_ref()
        {
            if let Err(err) = sink.render(&mut placebo, &gl, frame, size) {
                error!(?err, "video sink render failed");
            }

            unsafe {
                let surface_ids = [fl.video_surface_id];
                let damage_seq = fl_mark_surfaces_as_damaged(
                    fl.client.client,
                    surface_ids.as_ptr(),
                    surface_ids.len(),
                )
                .value;
                fl_discard_reply(fl.client.client, damage_seq);
                fl_wait_for_vsync_finished(fl.client.client, damage_seq, 0.15);
            }
        }
    }

    handle.set_gui_visible(false);
    handle.send_gui_window_closed_blocking(Duration::from_millis(2500));
    sink.teardown(&mut placebo);
    drop(placebo);
    handle.shutdown();

    Ok(())
}
