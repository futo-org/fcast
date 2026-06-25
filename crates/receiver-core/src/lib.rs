use anyhow::Result;
use fcast::SessionId;
use fcast_protocol::SetVolumeMessage;
use gst::prelude::*;
#[cfg(target_os = "android")]
use slint::android::android_activity::WindowManagerFlags;
use slint::{ToSharedString, VecModel};
use tokio::sync::mpsc::{self, UnboundedSender};
#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info};

#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, LazyLock},
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
pub mod egl;
mod fcast;
mod fcasthttpsrc;
mod fcasttextoverlay;
mod fcastwhepsrcbin;
mod gcast;
mod gstreamer;
mod gui;
mod image;
mod logging;
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

pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get().min(4))
        .thread_name("main-async-worker")
        .build()
        .unwrap()
});

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

#[derive(Debug, serde::Deserialize)]
pub struct DiscoverySettings {
    /// A regex for excluding network interface names to broadcast to.
    pub exclude_interfaces: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct SettingsFile {
    pub discovery: Option<DiscoverySettings>,
}

impl SettingsFile {
    async fn try_load(cli: &CliArgs) -> Option<Self> {
        let base_dirs = directories::BaseDirs::new();
        let paths = [
            cli.settings_file_path
                .as_ref()
                .map(|p| PathBuf::try_from(p).ok())
                .flatten(),
            base_dirs
                .as_ref()
                .map(|b| b.config_dir().to_path_buf().join("fcast-receiver.toml")),
            base_dirs.as_ref().map(|b| {
                b.config_dir()
                    .to_path_buf()
                    .join("fcast-receiver")
                    .join("config.toml")
            }),
            #[cfg(target_os = "linux")]
            Some(PathBuf::from("/etc").join("fcast-receiver.toml")),
            #[cfg(target_os = "linux")]
            Some(
                PathBuf::from("/etc")
                    .join("fcast-receiver")
                    .join("config.toml"),
            ),
        ];

        for path in paths {
            let Some(path) = path else {
                continue;
            };
            if !path.exists() {
                continue;
            };

            let settings_str = match tokio::fs::read_to_string(&path).await {
                Ok(s) => s,
                Err(err) => {
                    error!(?err, ?path, "Failed to read from file");
                    continue;
                }
            };

            match toml_edit::de::from_str::<SettingsFile>(&settings_str) {
                Ok(settings) => {
                    info!(?path, ?settings, "Loaded settings from file");
                    return Some(settings);
                }
                Err(err) => {
                    error!(
                        ?err,
                        ?path,
                        settings_str,
                        "Failed to deserialize settings file"
                    );
                }
            }
        }

        None
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
    /// Path to the settings file to use
    #[arg(long)]
    settings_file_path: Option<String>,
    /// Run without a GUI
    #[arg(long, default_value_t = false)]
    pub headless: bool,
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

pub struct Settings {
    pub cli: CliArgs,
    pub file: Option<SettingsFile>,
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
    let start = std::time::Instant::now();

    logging::init(cli_args.loglevel);

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

    let (msg_tx, event_rx) = mpsc::unbounded_channel::<Message>();
    let msg_tx = MessageSender::new(msg_tx);
    let (fin_tx, fin_rx) = tokio::sync::oneshot::channel::<()>();

    #[cfg(target_os = "android")]
    RUNTIME.spawn({
        let msg_tx = msg_tx.clone();
        async move {
            while let Some(event) = platform_event_rx.recv().await {
                msg_tx.send(event);
            }

            debug!("Platform event proxy finished");
        }
    });

    let is_headless = cli_args.headless;

    let sink_mutex = Arc::new(parking_lot::Mutex::new(None::<video::FSink>));
    let ui = if is_headless {
        None
    } else {
        Some(MainWindow::new()?)
    };
    #[cfg(feature = "systray")]
    let systray = if cli_args.no_systray {
        None
    } else {
        Some(SystemTray::new()?)
    };

    let gui_is_visible = gui::GuiIsVisible::new();
    let mut renderer_tx = None;
    if let Some(ui) = &ui {
        let pl_log = libplacebo::Log::new().unwrap();
        let render_opts = cli_args.rendering_options();

        #[cfg(debug_assertions)]
        ui.global::<Bridge>().set_is_debugging(true);

        let (renderer_chan_tx, renderer_rx) = std::sync::mpsc::channel::<gui::RendererMessage>();
        renderer_tx = Some(renderer_chan_tx);
        ui.window().set_rendering_notifier({
            let ui_weak = ui.as_weak();
            #[cfg(not(target_os = "android"))]
            let mut start_fullscreen = Some(cli_args.fullscreen);
            let mut prev_size = (0, 0);
            let mut sink = None;
            let msg_tx = msg_tx.clone();
            let mut renderer = None;
            let mut pl_context = None;
            #[cfg(target_os = "linux")]
            let mut drm_formats = HashSet::new();
            let gui_is_visible = gui_is_visible.clone();
            let mut video_sink = video_sink;
            let sink_mutex = Arc::clone(&sink_mutex);
            let mut payload_handle = None::<video::imp::VideoPayloadHandle>;
            let mut cached_frame = None;
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    debug!("Got graphics API: {graphics_api:?}");
                    let ui_weak = ui_weak.clone();

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
                                    && extensions
                                        .contains(&egl::Extension::ImageDmaBufImportModifiers)
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
                                    crate::placebo::PlaceboContext::new(&pl_log, &render_opts)
                                        .unwrap(),
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
                                    match renderer.blur_rgba8_image(
                                        img.image.as_raw(),
                                        width,
                                        height,
                                    ) {
                                        Ok(tex) => {
                                            bridge.set_blured_audio_track_cover(CompoundImage {
                                                img: tex.to_borrowed_slint_image(),
                                                rotation: image::orientation_to_degs(
                                                    img.orientation,
                                                ),
                                            });
                                            renderer.blured_audio_cover = Some(tex);
                                        }
                                        Err(err) => {
                                            error!(?err, "Failed to blur audio track cover")
                                        }
                                    }
                                }
                                gui::RendererMessage::ClearBluredAudioTrackCover => {
                                    bridge.set_blured_audio_track_cover(CompoundImage::default());
                                    renderer.blured_audio_cover.take();
                                }
                            }
                        }
                    }

                    let Some(sink) = sink.as_mut() else {
                        if let Some(new_sink) = sink_mutex.lock().take() {
                            #[cfg(target_os = "linux")]
                            new_sink.set_property(
                                "drm-formats",
                                video::imp::DrmFormats(Arc::new(drm_formats.clone())),
                            );
                            payload_handle = Some(new_sink.property("payload-handle"));
                            sink = Some(new_sink);
                        }
                        return;
                    };

                    if let Some(payload_handle) = &payload_handle {
                        if let Some(pay) = payload_handle.0.lock().take() {
                            match pay {
                                Some(frame) => cached_frame = Some(frame),
                                // EOS
                                None => {
                                    cached_frame = None;
                                    bridge.set_overlays(slint::ModelRc::default());
                                    bridge.set_subtitles(slint::ModelRc::default());
                                }
                            }
                        }
                    }

                    let new_size = ui.window().size();
                    let new_size = (new_size.width, new_size.height);
                    if new_size != prev_size {
                        sink.set_property(
                            "window-resolution",
                            video::imp::WindowResolution {
                                width: new_size.0,
                                height: new_size.1,
                            },
                        );
                        prev_size = new_size;
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

                    if let Some(frame) = cached_frame.as_mut() {
                        bridge.set_video_frame_width(frame.data.width() as i32);
                        bridge.set_video_frame_height(frame.data.height() as i32);
                        if let Some(placebo) = pl_context.as_mut()
                            && let Some(renderer) = renderer.as_ref()
                        {
                            if let Err(err) =
                                video_sink.render(placebo, &renderer.gl, frame, prev_size)
                            {
                                error!(?err, "video sink render failed");
                            }
                        }

                        let overlays =
                            std::mem::replace(&mut frame.overlays, video::Resource::Unchanged);
                        match overlays {
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

                        let subtitles =
                            std::mem::replace(&mut frame.subtitles, video::Resource::Unchanged);
                        match subtitles {
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

                    cached_frame.take();

                    if let Some(placebo) = pl_context.as_mut() {
                        video_sink.teardown(placebo);
                    }

                    pl_context.take();
                }
                _ => (),
            }
        })?;
    }

    let gui_tx = if let Some(ui) = &ui {
        let (gui_tx, gui_rx) = mpsc::unbounded_channel::<gui::UpdateGuiCommand>();
        gui::spawn_command_handler(ui.as_weak(), gui_rx, renderer_tx.unwrap());
        Some(gui_tx)
    } else {
        None
    };

    let gui = GuiController::new(gui_tx, gui_is_visible.clone());

    #[allow(unused_variables)]
    #[cfg(not(target_os = "android"))]
    let no_main_window = cli_args.no_main_window;
    let event_loop_jh = RUNTIME.spawn({
        let ui_weak = ui.as_ref().map(|ui| ui.as_weak());
        let msg_tx = msg_tx.clone();
        async move {
            gstreamer::init_and_load_plugins();

            let video_sink_elem = if let Some(ui_weak) = ui_weak {
                let sink = video::FSink::new();
                sink.connect("frame-available", false, move |_| {
                    ui_weak
                        .upgrade_in_event_loop(move |ui| {
                            ui.window().request_redraw();
                        })
                        .unwrap();

                    None
                });

                let video_sink_elem = sink.clone();
                *sink_mutex.lock() = Some(sink);
                Some(video_sink_elem)
            } else {
                None
            };

            let settings_file = SettingsFile::try_load(&cli_args).await;
            let settings = Settings {
                cli: cli_args,
                file: settings_file,
            };

            application::Application::new(
                gui,
                video_sink_elem.map(|e| e.upcast()),
                msg_tx,
                #[cfg(target_os = "android")]
                android_app,
                #[cfg(not(target_os = "android"))]
                settings,
            )
            .await
            .unwrap()
            .run_event_loop(event_rx, fin_tx)
            .await
            .unwrap();
        }
    });

    #[cfg(not(target_os = "android"))]
    RUNTIME.spawn({
        let msg_tx = msg_tx.clone();
        async move {
            if let Err(err) = tokio::signal::ctrl_c().await {
                error!(?err, "Failed to listen for ctrl+c event");
            } else {
                debug!("Got Ctrl+C");
                if is_headless {
                    msg_tx.send(Message::Quit);
                } else {
                    let _ = slint::quit_event_loop();
                }
            }
        }
    });

    if let Some(ui) = ui {
        gui::register_callbacks(&ui, msg_tx.clone());
        info!(initialized_in = ?start.elapsed());

        #[cfg(any(target_os = "android", not(feature = "systray")))]
        ui.run()?;

        #[cfg(feature = "systray")]
        if let Some(systray) = systray.as_ref() {
            let ui_weak = ui.as_weak();
            systray.on_toggle_window(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let win = ui.window();
                    if win.is_visible() {
                        let _ = win.hide();
                    } else {
                        let _ = win.show();
                    }
                }
            });

            systray.on_quit(|| {
                let _ = slint::quit_event_loop();
            });

            if !no_main_window {
                ui.show()?;
            }
            systray.show()?;
            slint::run_event_loop_until_quit()?;
        } else {
            ui.run()?;
        }


        info!("Shutting down...");

        RUNTIME.block_on(async move {
            msg_tx.send(Message::Quit);
            let _ = fin_rx.await;
        });
    } else {
        info!(initialized_in = ?start.elapsed());
        RUNTIME.block_on(async move {
            if let Err(err) = event_loop_jh.await {
                error!(?err, "Failed to join event loop task");
            }
        });
    }

    Ok(())
}
