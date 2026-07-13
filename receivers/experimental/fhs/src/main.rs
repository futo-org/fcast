use anyhow::{Result, anyhow};
use fiatlux::*;
use mimalloc::MiMalloc;
use rcore::{
    ImageAnimationFrame, ImageCommand,
    clap::Parser,
    egl, glow,
    tracing::error,
    video::{Frame, Resource},
};
use std::{
    ffi::{CString, c_char, c_void},
    ptr::null,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

mod pixmap_video_sink;
mod placeholder;
mod subtitle_surface;

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
                fl_create_surface(client.client, window.window_id, -1, true),
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

struct ImageAnimation {
    frames: Vec<ImageAnimationFrame>,
    index: usize,
    next_deadline: Instant,
}

const PLACEHOLDER_GRACE: Duration = Duration::from_millis(300);

struct PlaceholderState {
    title: String,
    artist: String,
    size: (u32, u32),
    show_at: Instant,
    shown: bool,
}

fn show_audio_placeholder(
    sink: &mut pixmap_video_sink::FhsPixmapSink,
    client: *mut fl_Client,
    surface_id: fl_protocol_SurfaceId,
    title: &str,
    artist: &str,
    size: (u32, u32),
    scale: f32,
) {
    match placeholder::render(title, artist, size.0, size.1, scale) {
        Ok((rgba, width, height)) => {
            if let Err(err) = sink.show_image(&rgba, width, height) {
                error!(?err, "audio placeholder show failed");
            } else {
                mark_damaged(client, &[surface_id]);
            }
        }
        Err(err) => error!(?err, "audio placeholder render failed"),
    }
}

fn frame_delay(delay_ms: i64) -> Duration {
    let ms = if delay_ms <= 10 { 100 } else { delay_ms };
    Duration::from_millis(ms as u64)
}

fn mark_damaged(client: *mut fl_Client, surface_ids: &[fl_protocol_SurfaceId]) {
    unsafe {
        let damage_seq =
            fl_mark_surfaces_as_damaged(client, surface_ids.as_ptr(), surface_ids.len()).value;
        fl_discard_reply(client, damage_seq);
        fl_wait_for_vsync_finished(client, damage_seq, 0.15);
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
        glutin_egl.MakeCurrent(fl_egl_display, std::ptr::null(), std::ptr::null(), egl_context);
    }

    let gl =
        unsafe { glow::Context::from_loader_function_cstr(|s| gl_load_proc(fl_egl, s.as_ptr())) };

    let egl_image_target: Option<unsafe extern "C" fn(u32, *const c_void)> =
        unsafe { core::mem::transmute(gl_load_proc(fl_egl, c"glEGLImageTargetTexture2DOES".as_ptr())) };

    let drm_formats = egl::get_supported_dma_drm_formats(fl_egl_display)?;

    let mut sink = pixmap_video_sink::FhsPixmapSink::new(
        fl.client.client,
        fl.video_surface_id,
        gl,
        fl_egl_display as *const c_void,
        egl_image_target,
    )?;
    let mut subtitles =
        subtitle_surface::SubtitleSurface::new(fl.client.client, fl.window.window_id)?;

    let render_device_path = pixmap_video_sink::query_render_device_path(fl.client.client);

    let signal = FrameSignal::new();
    let handle =
        rcore::run_with_external_video(cli_args, signal.notifier(), Some(render_device_path))?;
    handle.set_drm_formats(drm_formats);
    handle.set_gui_visible(true);

    let mut cached_frame: Option<Frame> = None;
    let mut animation: Option<ImageAnimation> = None;
    let mut placeholder: Option<PlaceholderState> = None;
    let mut size = (fl.window.width, fl.window.height);
    handle.set_window_resolution(size.0, size.1);

    loop {
        let mut wait = Duration::from_millis(150);
        if let Some(anim) = &animation
            && anim.frames.len() > 1
        {
            wait = wait.min(anim.next_deadline.saturating_duration_since(Instant::now()));
        }
        if let Some(p) = &placeholder
            && !p.shown
        {
            wait = wait.min(p.show_at.saturating_duration_since(Instant::now()));
        }
        signal.wait_timeout(wait);

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
            subtitles.reposition(size);
            if let Some(p) = placeholder.as_mut()
                && p.size != size
            {
                p.size = size;
                if p.shown {
                    show_audio_placeholder(
                        &mut sink,
                        fl.client.client,
                        fl.video_surface_id,
                        &p.title,
                        &p.artist,
                        size,
                        fl.window.display_scale,
                    );
                }
            }
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
                animation = None;
                placeholder = None;
            }
            Some(None) => cached_frame = None,
        }

        if (have_new || resized)
            && let Some(frame) = cached_frame.as_ref()
        {
            if let Err(err) = sink.render(frame) {
                error!(?err, "video sink render failed");
            }

            if have_new {
                let result = match &frame.overlays {
                    Resource::New(overlays) => subtitles.set_overlays(&sink, overlays, size),
                    Resource::Unchanged => Ok(()),
                    Resource::Cleared | Resource::Eos => match &frame.subtitles {
                        Resource::New(lines) => {
                            subtitles.set_subtitles(&sink, lines, size, fl.window.display_scale)
                        }
                        Resource::Unchanged => Ok(()),
                        Resource::Cleared | Resource::Eos => {
                            subtitles.clear();
                            Ok(())
                        }
                    },
                };
                if let Err(err) = result {
                    error!(?err, "subtitle surface update failed");
                }
            }

            let mut surface_ids = vec![fl.video_surface_id];
            if let Some(subtitle_surface_id) = subtitles.surface_id() {
                surface_ids.push(subtitle_surface_id);
            }
            mark_damaged(fl.client.client, &surface_ids);
        }

        match handle.take_image_update() {
            Some(ImageCommand::Set {
                rgba,
                width,
                height,
            }) => {
                animation = None;
                placeholder = None;
                if let Err(err) = sink.show_image(&rgba, width, height) {
                    error!(?err, "image show failed");
                } else {
                    mark_damaged(fl.client.client, &[fl.video_surface_id]);
                }
            }
            Some(ImageCommand::SetAnimation { frames }) => {
                animation = None;
                placeholder = None;
                if let Some(first) = frames.first() {
                    if let Err(err) = sink.show_image(&first.rgba, first.width, first.height) {
                        error!(?err, "image show failed");
                    } else {
                        mark_damaged(fl.client.client, &[fl.video_surface_id]);
                    }
                }
                if frames.len() > 1 {
                    let next_deadline = Instant::now() + frame_delay(frames[0].delay_ms);
                    animation = Some(ImageAnimation {
                        frames,
                        index: 0,
                        next_deadline,
                    });
                }
            }
            Some(ImageCommand::AudioPlaceholder { title, artist }) => {
                animation = None;
                match placeholder.as_mut() {
                    Some(p) if p.title == title && p.artist == artist => {}
                    Some(p) if p.shown => {
                        p.title = title;
                        p.artist = artist;
                        p.size = size;
                        show_audio_placeholder(
                            &mut sink,
                            fl.client.client,
                            fl.video_surface_id,
                            &p.title,
                            &p.artist,
                            size,
                            fl.window.display_scale,
                        );
                    }
                    _ => {
                        placeholder = Some(PlaceholderState {
                            title,
                            artist,
                            size,
                            show_at: Instant::now() + PLACEHOLDER_GRACE,
                            shown: false,
                        });
                    }
                }
            }
            Some(ImageCommand::Clear) => {
                animation = None;
                placeholder = None;
            }
            None => {}
        }

        if let Some(p) = placeholder.as_mut()
            && !p.shown
            && Instant::now() >= p.show_at
        {
            p.shown = true;
            p.size = size;
            show_audio_placeholder(
                &mut sink,
                fl.client.client,
                fl.video_surface_id,
                &p.title,
                &p.artist,
                size,
                fl.window.display_scale,
            );
        }

        if let Some(anim) = &mut animation
            && Instant::now() >= anim.next_deadline
        {
            anim.index = (anim.index + 1) % anim.frames.len();
            let frame = &anim.frames[anim.index];
            if let Err(err) = sink.show_image(&frame.rgba, frame.width, frame.height) {
                error!(?err, "image show failed");
            } else {
                mark_damaged(fl.client.client, &[fl.video_surface_id]);
            }
            anim.next_deadline = Instant::now() + frame_delay(frame.delay_ms);
        }
    }

    handle.set_gui_visible(false);
    handle.send_gui_window_closed_blocking(Duration::from_millis(2500));
    subtitles.clear();
    sink.teardown();
    handle.shutdown();

    Ok(())
}
