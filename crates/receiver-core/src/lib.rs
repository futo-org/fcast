use anyhow::Result;
use fcast::SessionId;
use fcast_protocol::SetVolumeMessage;
use gst::prelude::*;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use gst_gl::prelude::*;
#[cfg(target_os = "android")]
use slint::android::android_activity::WindowManagerFlags;
use slint::{ToSharedString, VecModel};
use tokio::sync::mpsc::{self, UnboundedSender};
#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info};

use std::{
    collections::HashSet,
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

#[cfg(not(target_os = "android"))]
pub use clap;
pub use slint;
pub use tracing;
mod application;
#[cfg(target_os = "linux")]
mod dmabuf;
#[cfg(target_os = "linux")]
mod egl;
mod fcast;
mod fcasttextoverlay;
mod fcastwhepsrcbin;
mod gcast;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod graphics;
mod gstreamer;
mod gui;
mod image;
#[cfg(all(target_os = "linux", feature = "systray"))]
mod linux_tray;
mod logging;
#[cfg(all(
    not(any(target_os = "android", target_os = "linux")),
    feature = "systray"
))]
mod mac_win_tray;
#[cfg(not(target_os = "android"))]
mod mdns;
mod message;
mod opengl;
pub mod placebo;
mod player;
mod raop;
mod user_agent;
mod utils;
pub mod video;
pub mod video_sink;

pub use glow;
pub use gst;
pub use gst_video;
pub use libplacebo;
pub use video_sink::{SwapchainSink, VideoSink};

use crate::{fcast::Operation, gui::GuiController, player::PlayerState};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use graphics::GraphicsContext;
pub use raop::{Configuration, device_name_hash, hash_to_string, txt_properties};

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

use message::{Mdns, Message, Raop};

pub const FCAST_TCP_PORT: u16 = 46899;
pub const GCAST_TCP_PORT: u16 = 8009;
pub type MediaItemId = u64;

pub use message::MessageSender;

#[macro_export]
macro_rules! log_if_err {
    ($res:expr) => {
        if let Err(err) = $res {
            tracing::error!("{err}");
        }
    };
}

slint::include_modules!();

struct GCastUpdateSender(Option<UnboundedSender<gcast::StatusUpdate>>);

impl GCastUpdateSender {
    fn send(&self, update: gcast::StatusUpdate) {
        if let Some(tx) = self.0.as_ref()
            && let Err(err) = tx.send(update)
        {
            error!(?err, "Failed to send GCast update");
        }
    }
}

#[cfg(not(target_os = "android"))]
#[derive(clap::Parser)]
#[command(name = "FCast Receiver")]
#[command(version)]
pub struct CliArgs {
    /// Start minimized to tray
    #[arg(long, default_value_t = false)]
    no_main_window: bool,
    /// Start application in fullscreen
    #[arg(long, default_value_t = false)]
    fullscreen: bool,
    /// Defines the verbosity level of the logger
    #[arg(long, alias = "log", visible_alias = "log")]
    loglevel: Option<LevelFilter>,
    /// Start player in windowed mode
    #[arg(long, default_value_t = false)]
    no_fullscreen_player: bool,
    /// Disable the system tray icon
    #[arg(long, default_value_t = false)]
    no_systray: bool,
    /// Disable the RAOP receiver
    #[arg(long, default_value_t = false)]
    no_raop: bool,
    /// Disable the Google Cast receiver
    #[arg(long, default_value_t = false)]
    no_google_cast: bool,
    /// Change what video frame render profile should be used
    #[arg(long, value_enum, default_value_t = placebo::RenderProfile::Fast)]
    render_profile: placebo::RenderProfile,
    /// Visualize the color mapping lookup table used for video rendering
    #[arg(long, default_value_t = false)]
    visualize_color_mapping_lut: bool,
    /// Visualize clipped pixels from tone-mapping
    #[arg(long, default_value_t = false)]
    visualize_hdr_clipping: bool,
}

impl CliArgs {
    fn rendering_options(&self) -> placebo::RenderingOptions {
        placebo::RenderingOptions {
            profile: self.render_profile,
            visualize_lut: self.visualize_color_mapping_lut,
            show_clipping: self.visualize_hdr_clipping,
        }
    }
}

/// Run the main app.
///
/// Slint and friends are assumed to be initialized by the platform specific target.
pub fn run<S: VideoSink + 'static>(
    #[cfg(not(target_os = "android"))] cli_args: CliArgs,
    #[cfg(target_os = "android")] android_app: slint::android::AndroidApp,
    #[cfg(target_os = "android")] mut platform_event_rx: UnboundedReceiver<Message>,
    video_sink: S,
) -> Result<()> {
    logging::init(cli_args.loglevel);

    let start = std::time::Instant::now();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get().min(4))
        .thread_name("main-async-worker")
        .build()
        .unwrap();

    #[cfg(target_os = "linux")]
    if let Err(err) = rustls::crypto::ring::default_provider().install_default() {
        error!(
            ?err,
            "Failed to register ring as rustls default crypto provider"
        );
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Err(err) = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default()
    {
        error!(
            ?err,
            "Failed to register aws_lc_rs as rustls default crypto provider"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let gst_gl_contexts = graphics::GlContext::new();

    let (msg_tx, event_rx) = mpsc::unbounded_channel::<Message>();
    let msg_tx = MessageSender::new(msg_tx);
    let (fin_tx, fin_rx) = tokio::sync::oneshot::channel::<()>();

    #[cfg(target_os = "android")]
    runtime.spawn({
        let msg_tx = msg_tx.clone();
        async move {
            while let Some(event) = platform_event_rx.recv().await {
                msg_tx.send(event);
            }

            debug!("Platform event proxy finished");
        }
    });

    let slint_sink_mutex = Arc::new(parking_lot::Mutex::new(None::<video::SlintOpenGLSink>));

    let ui = MainWindow::new()?;

    let bridge = ui.global::<Bridge>();

    let pl_log = libplacebo::Log::new().unwrap();
    let render_opts = cli_args.rendering_options();

    let gui_is_visible = gui::GuiIsVisible::new();

    #[cfg(debug_assertions)]
    bridge.set_is_debugging(true);

    let (renderer_tx, renderer_rx) = std::sync::mpsc::channel::<gui::RendererMessage>();
    ui.window().set_rendering_notifier({
        let ui_weak = ui.as_weak();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let gst_gl_contexts = gst_gl_contexts.clone();
        #[cfg(not(target_os = "android"))]
        let mut start_fullscreen = Some(cli_args.fullscreen);
        let mut prev_size = (0, 0);
        let mut slint_sink = None;
        let slint_sink_mutex = Arc::clone(&slint_sink_mutex);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let mut graphics_context = GraphicsContext::None;
        let msg_tx = msg_tx.clone();
        let mut renderer = None;
        let mut pl_context = None;
        let mut cached_frame = None;
        #[cfg(target_os = "linux")]
        let mut drm_formats = HashSet::new();
        let gui_is_visible = gui_is_visible.clone();
        let mut video_sink = video_sink;
        move |state, graphics_api| match state {
            slint::RenderingState::RenderingSetup => {
                debug!("Got graphics API: {graphics_api:?}");
                let ui_weak = ui_weak.clone();

                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    graphics_context = GraphicsContext::from_slint(graphics_api).unwrap();
                }

                #[cfg(not(target_os = "android"))]
                if let Some(fullscreen) = start_fullscreen.take() {
                    ui_weak
                        .upgrade()
                        .unwrap()
                        .window()
                        .set_fullscreen(fullscreen);
                }

                if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api {
                    #[cfg(target_os = "linux")]
                    {
                        egl::ensure_init();
                        let egl = glutin_egl_sys::egl::Egl::load_with(|symbol| {
                            get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                        });

                        let display = unsafe { egl.GetCurrentDisplay() };
                        let err = unsafe { egl.GetError() };
                        if !display.is_null() && err == glutin_egl_sys::egl::SUCCESS as i32 {
                            pl_context = unsafe {
                                Some(
                                    crate::placebo::PlaceboContext::new_egl(
                                        &pl_log,
                                        &render_opts,
                                        display as *mut _,
                                        egl.GetCurrentContext() as *mut _,
                                    )
                                    .unwrap(),
                                )
                            };

                            let extensions = egl::get_extensions(&egl);
                            if extensions.contains(&egl::Extension::ImageDmaBufImport)
                                && extensions.contains(&egl::Extension::ImageDmaBufImportModifiers)
                            {
                                match egl::get_supported_dma_drm_formats(display) {
                                    Ok(formats) => {
                                        debug!(
                                            formats = formats
                                                .iter()
                                                .map(|fmt| format!(
                                                    "{}:{:?}",
                                                    fmt.code, fmt.modifier
                                                ))
                                                .collect::<Vec<_>>()
                                                .join(" "),
                                            "Got supported DMA DRM formats"
                                        );
                                        drm_formats = formats;
                                    }
                                    Err(err) => {
                                        error!(?err, "Failed to get supported DMA DRM formats");
                                    }
                                }
                            }
                        } else {
                            pl_context = Some(
                                crate::placebo::PlaceboContext::new(&pl_log, &render_opts).unwrap(),
                            );
                        }
                    }

                    #[cfg(not(target_os = "linux"))]
                    {
                        pl_context = Some(
                            crate::placebo::PlaceboContext::new(&pl_log, &render_opts).unwrap(),
                        );
                    }

                    let gl = unsafe {
                        glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                    };
                    match opengl::Renderer::new(gl) {
                        Ok(r) => renderer = Some(r),
                        Err(err) => error!(?err, "Failed to create renderer"),
                    }
                }

                gui_is_visible.set(true);
            }
            slint::RenderingState::BeforeRendering => {
                let Some(ui) = ui_weak.upgrade() else {
                    error!("Failed to upgrade ui");
                    return;
                };

                let bridge = ui.global::<Bridge>();

                while let Ok(msg) = renderer_rx.try_recv() {
                    if let Some(renderer) = renderer.as_mut() {
                        match msg {
                            gui::RendererMessage::CreateBluredAudioTrackCover(img) => {
                                let (width, height) = img.image.dimensions();
                                match renderer.blur_rgba8_image(img.image.as_raw(), width, height) {
                                    Ok(tex) => {
                                        bridge.set_blured_audio_track_cover(CompoundImage {
                                            img: tex.to_borrowed_slint_image(),
                                            rotation: image::orientation_to_degs(img.orientation),
                                        });
                                        renderer.blured_audio_cover = Some(tex);
                                    }
                                    Err(err) => error!(?err, "Failed to blur audio track cover"),
                                }
                            }
                            gui::RendererMessage::ClearBluredAudioTrackCover => {
                                bridge.set_blured_audio_track_cover(CompoundImage::default());
                                renderer.blured_audio_cover.take();
                            }
                        }
                    }
                }

                let Some(slint_sink) = slint_sink.as_mut() else {
                    #[allow(unused_mut)]
                    if let Some(mut sink) = slint_sink_mutex.lock().take() {
                        #[cfg(target_os = "linux")]
                        sink.set_drm_formats(&drm_formats);
                        slint_sink = Some(sink);
                    }
                    return;
                };

                #[cfg(any(target_os = "macos", target_os = "windows"))]
                if let Some((gst_gl_context, gst_gl_display)) = graphics_context.get_gst_contexts()
                {
                    gst_gl_context
                        .activate(true)
                        .expect("could not activate GStreamer GL context");
                    gst_gl_context
                        .fill_info()
                        .expect("failed to fill GL info for wrapped context");

                    slint_sink.gst_gl_context = Some(gst_gl_context.clone());

                    gst_gl_contexts.set_contexts(gst_gl_display, gst_gl_context);
                }

                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    graphics_context = GraphicsContext::Initialized;
                }

                let new_size = ui.window().size();
                let new_size = (new_size.width, new_size.height);
                if new_size != prev_size {
                    slint_sink.window_width.store(new_size.0, Ordering::Relaxed);
                    slint_sink
                        .window_height
                        .store(new_size.1, Ordering::Relaxed);
                    prev_size = new_size;
                    if let Some(sink_pad) = slint_sink.appsink.static_pad("sink") {
                        sink_pad.push_event(gst::event::Reconfigure::builder().build());
                    }
                }

                if let Some(renderer) = renderer.as_mut() {
                    use glow::HasContext;
                    let clear_color = video_sink.get_clear_color();
                    unsafe {
                        renderer.gl.clear_color(
                            clear_color[0],
                            clear_color[1],
                            clear_color[2],
                            clear_color[3],
                        );
                        renderer.gl.clear(glow::COLOR_BUFFER_BIT);
                    }
                }

                match slint_sink.fetch_next_frame() {
                    video::Resource::Eos | video::Resource::Cleared => {
                        if cached_frame.is_some()
                            && let Some(placebo) = pl_context.as_mut()
                        {
                            video_sink.flush_cache(placebo);
                        }
                        cached_frame.take();
                    }
                    video::Resource::Unchanged => (),
                    video::Resource::New(frame) => {
                        bridge.set_video_frame_width(frame.data.width() as i32);
                        bridge.set_video_frame_height(frame.data.height() as i32);
                        cached_frame = Some(frame);
                    }
                }

                match slint_sink.fetch_next_overlays() {
                    video::Resource::Eos | video::Resource::Cleared => {
                        bridge.set_overlays(slint::ModelRc::default());
                    }
                    video::Resource::Unchanged => (),
                    video::Resource::New(overlays) => {
                        let overlays: VecModel<UiSubOverlay> = overlays
                            .into_iter()
                            .map(|overlay| UiSubOverlay {
                                img: slint::Image::from_rgba8(overlay.pix_buffer),
                                x: overlay.x as f32,
                                y: overlay.y as f32,
                                render_width: overlay.render_width as f32,
                                render_height: overlay.render_height as f32,
                            })
                            .collect();
                        bridge.set_overlays(Rc::new(overlays).into());
                    }
                }

                match slint_sink.fetch_next_subtitles() {
                    video::Resource::Eos | video::Resource::Cleared => {
                        bridge.set_subtitles(slint::ModelRc::default());
                    }
                    video::Resource::Unchanged => (),
                    video::Resource::New(subs) => {
                        let subs: VecModel<slint::SharedString> =
                            subs.into_iter().map(|s| s.to_shared_string()).collect();
                        bridge.set_subtitles(Rc::new(subs).into());
                    }
                }

                if let Some(frame) = cached_frame.as_ref()
                    && let Some(placebo) = pl_context.as_mut()
                    && let Some(renderer) = renderer.as_ref()
                {
                    if let Err(err) = video_sink.render(placebo, &renderer.gl, frame, prev_size) {
                        error!(?err, "video sink render failed");
                    }
                }
            }
            slint::RenderingState::RenderingTeardown => {
                gui_is_visible.set(false);

                let (feedback_tx, feedback_rx) = oneshot::channel::<()>();

                msg_tx.send(Message::GuiWindowClosed(feedback_tx));
                match feedback_rx.recv_timeout(Duration::from_millis(2500)) {
                    Ok(_) => debug!("Player shutdown successfully"),
                    Err(err) => {
                        error!(?err, "Failed to receive feedback of player shutdown")
                    }
                }

                #[cfg(any(target_os = "macos", target_os = "windows"))]
                gst_gl_contexts.deactivate_and_clear();

                if let Some(sink) = slint_sink.as_mut() {
                    sink.release_state();
                }

                if let Some(placebo) = pl_context.as_mut() {
                    video_sink.teardown(placebo);
                }

                pl_context.take();
            }
            _ => (),
        }
    })?;

    #[cfg(all(
        not(any(target_os = "android", target_os = "linux")),
        feature = "systray"
    ))]
    let _tray_icon = if !cli_args.no_systray {
        let (tray, ids) = mac_win_tray::create_tray_icon();
        mac_win_tray::set_event_handler(msg_tx.clone(), ids);
        Some(tray)
    } else {
        None
    };

    let (gui_tx, gui_rx) = mpsc::unbounded_channel::<gui::UpdateGuiCommand>();

    gui::spawn_command_handler(ui.as_weak(), gui_rx, renderer_tx);

    let gui = GuiController::new(gui_tx, gui_is_visible.clone());

    #[allow(unused_variables)]
    #[cfg(not(target_os = "android"))]
    let (no_main_window, no_systray) = (cli_args.no_main_window, cli_args.no_systray);
    runtime.spawn({
        let ui_weak = ui.as_weak();
        let msg_tx = msg_tx.clone();
        let slint_sink_mutex = Arc::clone(&slint_sink_mutex);
        async move {
            gstreamer::init_and_load_plugins();

            let mut slint_sink = video::SlintOpenGLSink::new().unwrap();
            let slint_appsink = slint_sink.video_sink();
            let video_sink_is_eos = Arc::clone(&slint_sink.is_eos);
            let request_redraw_cb = move || {
                ui_weak
                    .upgrade_in_event_loop(move |ui| {
                        ui.window().request_redraw();
                    })
                    .unwrap();
            };
            slint_sink.connect(request_redraw_cb).unwrap();

            *slint_sink_mutex.lock() = Some(slint_sink);

            application::Application::new(
                gui,
                slint_appsink,
                msg_tx,
                video_sink_is_eos,
                #[cfg(target_os = "android")]
                android_app,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                gst_gl_contexts,
                #[cfg(not(target_os = "android"))]
                cli_args,
            )
            .await
            .unwrap()
            .run_event_loop(event_rx, fin_tx)
            .await
            .unwrap();
        }
    });

    gui::register_callbacks(&ui, &bridge, msg_tx.clone());

    info!(initialized_in = ?start.elapsed());

    #[cfg(not(target_os = "android"))]
    runtime.spawn(async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            error!(?err, "Failed to listen for ctrl+c event");
        } else {
            debug!("Got Ctrl+C");
            let _ = slint::quit_event_loop();
        }
    });

    #[cfg(any(target_os = "android", not(feature = "systray")))]
    ui.run()?;

    #[cfg(feature = "systray")]
    if no_systray {
        ui.run()?;
    } else {
        if !no_main_window {
            ui.show()?;
        }
        slint::run_event_loop_until_quit()?;
    }

    info!("Shutting down...");

    runtime.block_on(async move {
        msg_tx.send(Message::Quit);
        let _ = fin_rx.await;
    });

    Ok(())
}
