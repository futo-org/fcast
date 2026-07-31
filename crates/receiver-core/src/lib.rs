use anyhow::Result;
use gst::prelude::*;
use gst_base::prelude::BaseSinkExt;
#[cfg(target_os = "android")]
use slint::android::android_activity::WindowManagerFlags;
use tokio::sync::mpsc::{self, UnboundedSender};
#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info};

#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

#[cfg(not(target_os = "android"))]
pub use clap;
pub use slint;
pub use tracing;
#[cfg(feature = "airplay")]
mod airplay;
mod application;
mod fcast;
mod fcompsrc;
mod fwebrtcsrc;
mod gcast;
pub mod config;
mod gstreamer;
mod gui;
mod image;
mod imagedec;
mod imagetypefind;
#[cfg(target_os = "linux")]
mod vajpegdec;
mod inspector_graph;
mod logging;
#[cfg(not(target_os = "android"))]
mod mdns;
mod media_formats;
mod media_source;
mod message;
mod player;
mod queue_cache;
mod raop;
mod user_agent;
mod utils;

// The video rendering stack lives in fcast-video. These imports let the render
// loop below keep referring to the modules by their short names.
use fcast_video::{opengl, placebo, render_latency, video};
// Re-exported: the fhs receiver's pixmap sink uses `receiver_core::egl`.
#[cfg(target_os = "linux")]
pub use fcast_video::egl;

pub use glow;
pub use gst;
pub use gst_video;
pub use libplacebo;
pub use fcast_video::{SwapchainSink, VideoSink};
#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
pub use fcast_video::WaylandSubsurfaceSink;

use crate::{fcast::Operation, gui::GuiController, player::PlayerState};

pub use raop::{Configuration, device_name_hash, hash_to_string, txt_properties};

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;
pub type SenderId = u32;

use message::{Mdns, Message, Raop};

pub const FCAST_TCP_PORT: u16 = 46899;
pub const GCAST_TCP_PORT: u16 = 8009;
pub type MediaItemId = u64;

pub use message::MessageSender;

fn video_dbg_info(frame: &video::Frame) -> Option<UiVideoDbgInfo> {
    use slint::ToSharedString;

    let info = frame.data.video_info()?;
    let colorimetry = info.colorimetry();
    let fps = info.fps();
    let par = info.par();

    let framerate = if fps.denom() == 0 {
        String::new()
    } else {
        format!("{:.3} fps", fps.numer() as f64 / fps.denom() as f64)
    };

    let hdr = match frame.mastering_display_info.as_ref() {
        Some(mdi) => {
            let cll = frame
                .content_light_level
                .as_ref()
                .map_or_else(String::new, |cll| {
                    format!(
                        ", CLL {}/{}",
                        cll.max_content_light_level, cll.max_frame_average_light_level
                    )
                });
            format!(
                "mastering {:.0}–{:.0} nits{cll}",
                mdi.min_luminance_as_nits(),
                mdi.max_luminance_as_nits(),
            )
        }
        None => "SDR".to_owned(),
    };

    let rotation = match frame.rotation {
        video::Rotation::Rotate0 => "0°",
        video::Rotation::Rotate90 => "90°",
        video::Rotation::Rotate180 => "180°",
        video::Rotation::Rotate270 => "270°",
    };

    Some(UiVideoDbgInfo {
        format: format!("{:?} ({}-bit)", info.format(), info.comp_depth(0)).to_shared_string(),
        resolution: format!("{}x{}", info.width(), info.height()).to_shared_string(),
        framerate: framerate.to_shared_string(),
        pixel_aspect: format!("{}:{}", par.numer(), par.denom()).to_shared_string(),
        rotation: rotation.to_shared_string(),
        memory: frame.data.memory_kind().to_shared_string(),
        primaries: format!("{:?}", colorimetry.primaries()).to_shared_string(),
        transfer: format!("{:?}", colorimetry.transfer()).to_shared_string(),
        matrix: format!("{:?}", colorimetry.matrix()).to_shared_string(),
        range: format!("{:?}", colorimetry.range()).to_shared_string(),
        hdr: hdr.to_shared_string(),
    })
}

#[derive(Debug)]
pub struct ReceiverInfo {
    pub device_info: fcast_protocol::v4::DeviceInfo,
    pub supported_formats: media_formats::SupportedFormats,
}

#[macro_export]
macro_rules! log_if_err {
    ($res:expr) => {
        if let Err(err) = $res {
            tracing::error!("{err}");
        }
    };
}

// The single shared async runtime lives in its own crate so receiver-core and
// the GStreamer element crate spawn on the same thread pool.
pub use fcast_runtime::RUNTIME;

slint::include_modules!();

struct GCastUpdateSender(Option<UnboundedSender<gcast::StatusUpdate>>);

impl GCastUpdateSender {
    fn send(&mut self, update: gcast::StatusUpdate) {
        let Some(tx) = self.0.as_ref() else {
            return;
        };
        if tx.send(update).is_err() {
            // The receiver is gone only when the gcast server has stopped
            // (e.g. its port was taken on a second instance). Drop the sender
            // so later updates are no-ops instead of failing on every call.
            debug!("GCast server not running, disabling status updates");
            self.0 = None;
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
    /// Disable the AirPlay screen-mirroring receiver
    #[cfg(feature = "airplay")]
    #[arg(long, default_value_t = false)]
    no_airplay: bool,
    /// Disable the Google Cast receiver
    #[arg(long, default_value_t = false)]
    no_google_cast: bool,
    /// Disable the FCast receiver
    #[arg(long, default_value_t = false)]
    no_fcast: bool,
    /// Change what video frame render profile should be used
    #[arg(long, value_enum)]
    render_profile: Option<placebo::RenderProfile>,
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
    /// Force HDR content to be tone-mapped to SDR.
    #[arg(long, default_value_t = false)]
    pub disable_hdr_output: bool,
}

/// The receiver's effective settings: the parsed CLI flags plus the persisted
/// [`config::ConfigStore`]. Everything the app actually reads goes through the
/// accessor methods below, which resolve the two sources.
///
/// Resolution rule: a CLI flag that was passed always wins. The existing flags
/// are one-directional (`--no-raop`, `--fullscreen`, ...), so the CLI can force
/// a behavior on but cannot force one off against the config. When the CLI is
/// silent the config value is used, and when both are silent the built-in
/// default applies.
pub struct Settings {
    pub cli: CliArgs,
    pub config: config::ConfigStore,
}

#[cfg(not(target_os = "android"))]
impl Settings {
    /// Parse the CLI flags and load the persisted config they point at.
    pub fn load(cli: CliArgs) -> Self {
        let config = config::ConfigStore::load(cli.settings_file_path.as_deref());
        Self { cli, config }
    }

    /// Log verbosity, resolved from `--loglevel` then `[log] level`.
    pub fn log_level(&self) -> Option<LevelFilter> {
        if let Some(level) = self.cli.loglevel {
            return Some(level);
        }
        self.config
            .get()
            .log
            .level
            .as_deref()
            .and_then(parse_log_level)
    }

    /// The frame render profile, resolved from `--render-profile` then
    /// `[video] render_profile`, defaulting to `Fast`.
    pub fn render_profile(&self) -> placebo::RenderProfile {
        self.cli
            .render_profile
            .or_else(|| {
                self.config
                    .get()
                    .video
                    .render_profile
                    .as_deref()
                    .and_then(parse_render_profile)
            })
            .unwrap_or(placebo::RenderProfile::Fast)
    }

    pub fn rendering_options(&self) -> placebo::RenderingOptions {
        placebo::RenderingOptions {
            profile: self.render_profile(),
            visualize_lut: self.cli.visualize_color_mapping_lut,
            show_clipping: self.cli.visualize_hdr_clipping,
        }
    }

    /// Regex of network interface names to exclude from advertising on.
    pub fn exclude_interfaces(&self) -> Option<&str> {
        self.config.get().discovery.exclude_interfaces.as_deref()
    }

    pub fn fcast_enabled(&self) -> bool {
        !self.cli.no_fcast && self.config.get().fcast.enabled
    }

    pub fn raop_enabled(&self) -> bool {
        !self.cli.no_raop && self.config.get().raop.enabled
    }

    pub fn google_cast_enabled(&self) -> bool {
        !self.cli.no_google_cast && self.config.get().chromecast.enabled
    }

    #[cfg(feature = "airplay")]
    pub fn airplay_enabled(&self) -> bool {
        !self.cli.no_airplay && self.config.get().airplay.enabled
    }

    /// Broadcast name for the FCast service. Defaults to `FCast-<hostname>`.
    pub fn fcast_name(&self) -> String {
        self.config
            .get()
            .fcast
            .name
            .as_deref()
            .map(expand_name_vars)
            .unwrap_or_else(mdns::fcast_device_name)
    }

    /// Broadcast name for RAOP. Defaults to the FCast name.
    pub fn raop_name(&self) -> String {
        self.config
            .get()
            .raop
            .name
            .as_deref()
            .map(expand_name_vars)
            .unwrap_or_else(|| self.fcast_name())
    }

    /// Broadcast name for Google Cast. Defaults to `Chromecast-<hostname>`.
    pub fn chromecast_name(&self) -> String {
        self.config
            .get()
            .chromecast
            .name
            .as_deref()
            .map(expand_name_vars)
            .unwrap_or_else(mdns::chromecast_device_name)
    }

    pub fn headless(&self) -> bool {
        self.cli.headless || self.config.get().interface.headless
    }

    pub fn want_systray(&self) -> bool {
        !self.cli.no_systray && self.config.get().interface.tray
    }

    pub fn no_main_window(&self) -> bool {
        self.cli.no_main_window || !self.config.get().interface.show_window
    }

    pub fn fullscreen(&self) -> bool {
        self.cli.fullscreen || self.config.get().interface.start_fullscreen
    }

    pub fn no_fullscreen_player(&self) -> bool {
        self.cli.no_fullscreen_player || !self.config.get().interface.fullscreen_player
    }

    pub fn disable_hdr_output(&self) -> bool {
        self.cli.disable_hdr_output || !self.config.get().video.hdr_output
    }
}

#[cfg(not(target_os = "android"))]
fn parse_render_profile(value: &str) -> Option<placebo::RenderProfile> {
    match <placebo::RenderProfile as clap::ValueEnum>::from_str(value, true) {
        Ok(profile) => Some(profile),
        Err(_) => {
            tracing::warn!(value, "Unknown render_profile in config, using default");
            None
        }
    }
}

#[cfg(not(target_os = "android"))]
fn parse_log_level(value: &str) -> Option<LevelFilter> {
    match value.parse::<LevelFilter>() {
        Ok(level) => Some(level),
        Err(_) => {
            tracing::warn!(value, "Unknown log level in config, using default");
            None
        }
    }
}

/// Expand the variables allowed in a configured broadcast name. Currently only
/// `{hostname}`, which becomes the local hostname.
#[cfg(not(target_os = "android"))]
fn expand_name_vars(template: &str) -> String {
    template.replace("{hostname}", &mdns::hostname())
}

/// Per-tick video state shared (on the event-loop thread) between the Slint rendering notifier
/// and the event-loop-clocked handlers (`new-video-frame`, `video-obstructed-changed`). The
/// split exists because a subsurface sink stacked above the GUI parks winit's redraw loop
/// (occluded surfaces stop receiving frame callbacks), so frames and obstruction changes must
/// reach the sink without waiting for a repaint.
struct VideoTick<S> {
    video_sink: S,
    payload_handle: Option<video::imp::VideoPayloadHandle>,
    /// The sink element itself (both render paths report their measured render
    /// cost back to it as `render-delay`, see [`render_latency`]).
    sink_elem: Option<video::FSink>,
    cached_frame: Option<video::Frame>,
    /// Render on the next repaint even without a new payload (a standalone render was skipped
    /// or failed after the frame had already been taken off the payload slot).
    force_render: bool,
    /// A standalone EOS couldn't flush the shared GL placebo context (it isn't current outside
    /// the rendering notifier), do it on the next repaint tick.
    pending_gl_flush: bool,
    /// Feeds the real (post-`show_frame`) render cost back into the sink's
    /// `render-delay` so the base sink accounts for it.
    render_latency: render_latency::RenderLatencyTracker,
}

impl<S> VideoTick<S> {
    /// Record one render's measured cost and, on a meaningful change, push the
    /// new `render-delay` to the sink. Posting a LATENCY message makes the
    /// pipeline redistribute latency so the new value takes effect (fcastplaybin
    /// answers it with `recalculate_latency`).
    fn note_render_cost(&mut self, cost: std::time::Duration) {
        self.render_latency.record(cost);
        let Some(delay) = self.render_latency.poll(std::time::Instant::now()) else {
            return;
        };
        let Some(sink) = self.sink_elem.as_ref() else {
            return;
        };
        sink.set_render_delay(gst::ClockTime::from_nseconds(delay.as_nanos() as u64));
        let _ = sink.post_message(gst::message::Latency::builder().src(sink).build());
    }
}

/// Cap the number of glibc malloc arenas.
///
/// GStreamer spawns many short-lived worker threads over a session (one set per
/// load). glibc gives each thread its own arena and never returns an arena's
/// freed pages to the OS, so the process RSS climbs to the sum of every arena's
/// high-water mark even though the live heap stays flat (~120 MB). Capping the
/// arena count pins steady-state RSS (measured: ~800 MB climbing -> ~370 MB flat
/// over hundreds of loads). Skip it if the operator set MALLOC_ARENA_MAX so an
/// explicit environment override always wins.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_allocator() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_some() {
        return;
    }
    // SAFETY: `mallopt` takes two ints and has no memory-safety
    // preconditions. Called on the main thread before any GStreamer worker
    // threads spawn.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_allocator() {}

/// Debug builds: let any process (gdb, eu-stack) attach and snapshot thread
/// stacks. Yama's default `ptrace_scope=1` only allows ancestor tracers, which
/// blocks the "attach to the live receiver when a test harness detects a
/// wedge" workflow, exactly when the stacks matter most. No-op outside
/// debug builds.
#[cfg(all(debug_assertions, target_os = "linux"))]
fn allow_ptrace_attach() {
    // SAFETY: prctl(PR_SET_PTRACER, ...) only adjusts this process's Yama
    // tracer allowance, no memory-safety preconditions.
    unsafe {
        libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
    }
}

#[cfg(not(all(debug_assertions, target_os = "linux")))]
fn allow_ptrace_attach() {}

/// Run the main app.
///
/// Slint and friends are assumed to be initialized by the platform specific target.
pub fn run<S: VideoSink + 'static>(
    #[cfg(not(target_os = "android"))] settings: Settings,
    #[cfg(target_os = "android")] android_app: slint::android::AndroidApp,
    #[cfg(target_os = "android")] mut platform_event_rx: UnboundedReceiver<Message>,
    video_sink: S,
) -> Result<()> {
    let start = std::time::Instant::now();

    tune_allocator();
    allow_ptrace_attach();

    logging::init(settings.log_level());

    if let Err(err) = tokio_rustls::rustls::crypto::ring::default_provider().install_default() {
        error!(
            ?err,
            "Failed to register ring as rustls default crypto provider"
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

    let is_headless = settings.headless();

    let sink_mutex = Arc::new(parking_lot::Mutex::new(None::<video::FSink>));
    let ui = if is_headless {
        None
    } else {
        Some(MainWindow::new()?)
    };
    #[cfg(feature = "systray")]
    let want_systray = settings.want_systray();
    // The tray is created and shown lazily, only once the FCast port is
    // committed (see `on_show_tray`), so a port conflict that ends in quitting
    // never starts a tray at all. Held here for the lifetime of the event loop.
    #[cfg(feature = "systray")]
    let systray_holder: Rc<RefCell<Option<SystemTray>>> = Rc::new(RefCell::new(None));

    let gui_is_visible = gui::GuiIsVisible::new();
    let mut renderer_tx = None;
    let mut _obstruction_watchdog = None;
    if let Some(ui) = &ui {
        let pl_log = libplacebo::Log::new().unwrap();
        let render_opts = settings.rendering_options();

        #[cfg(debug_assertions)]
        ui.global::<Bridge>().set_is_debugging(true);

        let tick = Rc::new(RefCell::new(VideoTick {
            video_sink,
            payload_handle: None,
            sink_elem: None,
            cached_frame: None,
            force_render: false,
            pending_gl_flush: false,
            render_latency: render_latency::RenderLatencyTracker::new(),
        }));

        let (renderer_chan_tx, renderer_rx) = std::sync::mpsc::channel::<gui::RendererMessage>();
        renderer_tx = Some(renderer_chan_tx);
        ui.window().set_rendering_notifier({
            let ui_weak = ui.as_weak();
            #[cfg(not(target_os = "android"))]
            let mut start_fullscreen = Some(settings.fullscreen());
            let mut prev_size = (0, 0);
            let mut sink = None;
            let msg_tx = msg_tx.clone();
            let mut renderer = None;
            let mut pl_context = None;
            #[cfg(target_os = "linux")]
            let mut drm_formats = HashSet::new();
            let gui_is_visible = gui_is_visible.clone();
            let tick = tick.clone();
            let sink_mutex = Arc::clone(&sink_mutex);
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
                                        placebo::PlaceboContext::new_egl(
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
                                    placebo::PlaceboContext::new(&pl_log, &render_opts)
                                        .unwrap(),
                                );
                            }
                        }

                        #[cfg(not(target_os = "linux"))]
                        {
                            pl_context = Some(
                                placebo::PlaceboContext::new(&pl_log, &render_opts).unwrap(),
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

                    // Give the sink a chance to grab native window handles (e.g. the Wayland
                    // surface a subsurface sink parents itself to).
                    if let Some(ui) = ui_weak.upgrade() {
                        tick.borrow_mut().video_sink.setup(ui.window());
                    }

                    gui_is_visible.set(true);
                }
                slint::RenderingState::BeforeRendering => {
                    let Some(ui) = ui_weak.upgrade() else {
                        error!("Failed to upgrade ui");
                        return;
                    };

                    let bridge = ui.global::<Bridge>();

                    let mut clear_video_overlays = false;
                    while let Ok(msg) = renderer_rx.try_recv() {
                        if matches!(msg, gui::RendererMessage::ClearVideoOverlays) {
                            clear_video_overlays = true;
                            continue;
                        }
                        if let Some(renderer) = renderer.as_mut() {
                            match msg {
                                gui::RendererMessage::ClearVideoOverlays => unreachable!(),
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

                    let mut tick_ref = tick.borrow_mut();
                    let t = &mut *tick_ref;

                    if clear_video_overlays
                        && let Some(frame) = t.cached_frame.as_mut()
                        && !frame.overlays.is_empty()
                    {
                        frame.overlays.clear();
                        t.force_render = true;
                    }

                    let Some(sink) = sink.as_mut() else {
                        if let Some(new_sink) = sink_mutex.lock().take() {
                            #[cfg(target_os = "linux")]
                            new_sink.set_property(
                                "drm-formats",
                                video::imp::DrmFormats(Arc::new(drm_formats.clone())),
                            );
                            t.payload_handle = Some(new_sink.property("payload-handle"));
                            t.sink_elem = Some(new_sink.clone());
                            sink = Some(new_sink);
                        }
                        return;
                    };

                    if std::mem::take(&mut t.pending_gl_flush)
                        && let Some(placebo) = pl_context.as_mut()
                    {
                        t.video_sink.flush_cache(placebo);
                    }

                    let mut new_frame = false;
                    if let Some(payload_handle) = &t.payload_handle {
                        if let Some(pay) = payload_handle.0.lock().take() {
                            match pay {
                                Some(frame) => {
                                    t.cached_frame = Some(frame);
                                    new_frame = true;
                                }
                                // EOS
                                None => {
                                    t.cached_frame = None;
                                    t.video_sink.clear();
                                    if let Some(placebo) = pl_context.as_mut() {
                                        t.video_sink.flush_cache(placebo);
                                    }
                                }
                            }
                        }
                    }

                    let new_size = ui.window().size();
                    let new_size = (new_size.width, new_size.height);
                    let size_changed = new_size != prev_size;
                    if size_changed {
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
                        let clear_color = t.video_sink.get_clear_color();
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

                    let force_render = std::mem::take(&mut t.force_render);
                    let mut render_cost = None;
                    if let Some(frame) = t.cached_frame.as_mut() {
                        bridge.set_video_frame_width(frame.data.width() as i32);
                        bridge.set_video_frame_height(frame.data.height() as i32);

                        if bridge.get_show_inspector() && (new_frame || force_render) {
                            match video_dbg_info(frame) {
                                Some(info) => {
                                    bridge.set_video_dbg_info(info);
                                    bridge.set_have_video_dbg_info(true);
                                }
                                None => bridge.set_have_video_dbg_info(false),
                            }
                        }

                        if (new_frame
                            || size_changed
                            || force_render
                            || t.video_sink.needs_render_every_repaint())
                            && let Some(placebo) = pl_context.as_mut()
                            && let Some(renderer) = renderer.as_ref()
                        {
                            let start = std::time::Instant::now();
                            if let Err(err) =
                                t.video_sink.render(placebo, &renderer.gl, frame, prev_size)
                            {
                                error!(?err, "video sink render failed");
                            } else {
                                render_cost = Some(start.elapsed());
                            }
                        }
                    }
                    // After the `cached_frame` borrow ends: feed the measured
                    // cost back into the sink's render-delay (see `render_latency`).
                    if let Some(cost) = render_cost {
                        t.note_render_cost(cost);

                        if bridge.get_show_inspector() {
                            use slint::ToSharedString;
                            let (p95, applied) = t.render_latency.debug_snapshot();
                            let p95 = p95.map_or_else(
                                || "warming up".to_owned(),
                                |d| format!("{:.2} ms p95", d.as_secs_f64() * 1000.0),
                            );
                            bridge.set_render_latency_info(
                                format!(
                                    "render: {:.2} ms, {p95}, delay {:.2} ms",
                                    cost.as_secs_f64() * 1000.0,
                                    applied.as_secs_f64() * 1000.0,
                                )
                                .to_shared_string(),
                            );
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

                    let mut t = tick.borrow_mut();
                    t.cached_frame.take();

                    if let Some(placebo) = pl_context.as_mut() {
                        t.video_sink.teardown(placebo);
                    }

                    pl_context.take();
                }
                _ => (),
            }
        })?;

        // Pushed from MainWindow whenever the GUI starts/stops drawing over the video area.
        ui.global::<Bridge>().on_video_obstructed_changed({
            let tick = tick.clone();
            move |obstructed| {
                debug!(obstructed, "video obstruction changed");
                tick.borrow_mut()
                    .video_sink
                    .set_video_obstructed(obstructed, true);
            }
        });

        // One invocation per decoded frame (proxied from the GStreamer streaming thread).
        // Normally we just schedule a repaint and let `BeforeRendering` take the frame, while
        // the sink presents above the (redraw-parked) GUI, render it directly instead.
        ui.global::<Bridge>().on_new_video_frame({
            let tick = tick.clone();
            let ui_weak = ui.as_weak();
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let mut tick_ref = tick.borrow_mut();
                let t = &mut *tick_ref;
                let bridge = ui.global::<Bridge>();
                // Self-clocked means winit's redraw loop may be parked, and with it Slint's
                // `changed` callbacks (they only run as part of the render/update cycle), so
                // obstruction changes must be *polled*. Reading the property forces a fresh
                // evaluation, restacking below re-exposes the GUI and resumes its redraws.
                if t.video_sink.self_clocked() && bridge.get_video_obstructed() {
                    t.video_sink.set_video_obstructed(true, true);
                }
                if !t.video_sink.self_clocked() {
                    ui.window().request_redraw();
                    return;
                }
                let Some(next_payload) =
                    t.payload_handle.as_ref().and_then(|ph| ph.0.lock().take())
                else {
                    return;
                };
                match next_payload {
                    // EOS: mirror the repaint path's handling. clear() unmaps the subsurface,
                    // un-occluding the GUI, so the requested redraw actually fires.
                    None => {
                        t.cached_frame = None;
                        t.video_sink.clear();
                        t.video_sink.flush_cache_standalone();
                        t.pending_gl_flush = true;
                        ui.window().request_redraw();
                    }
                    Some(frame) => {
                        t.cached_frame = Some(frame);
                        let frame = t.cached_frame.as_mut().unwrap();
                        bridge.set_video_frame_width(frame.data.width() as i32);
                        bridge.set_video_frame_height(frame.data.height() as i32);
                        let size = ui.window().size();
                        let start = std::time::Instant::now();
                        let render_result = t
                            .video_sink
                            .render_standalone(frame, (size.width, size.height));
                        let render_cost = start.elapsed();
                        match render_result {
                            Ok(true) => {
                                t.note_render_cost(render_cost);
                            }
                            // Raced a restack (or failed): the payload slot is already empty,
                            // so flag a forced render and fall back to the repaint path.
                            Ok(false) => {
                                t.force_render = true;
                                ui.window().request_redraw();
                            }
                            Err(err) => {
                                error!(?err, "Standalone video render failed");
                                t.force_render = true;
                                ui.window().request_redraw();
                            }
                        }
                    }
                }
            }
        });

        ui.global::<Bridge>().on_inspector_toggled({
            let ui_weak = ui.as_weak();
            let tick = tick.clone();
            let msg_tx = msg_tx.clone();
            move |active| {
                // Let the app gate inspector work and reset sampling state.
                msg_tx.send(Message::InspectorActive(active));
                if let Some(ui) = ui_weak.upgrade() {
                    tick.borrow_mut().force_render = true;
                    ui.window().request_redraw();

                    // Drop the graph dump and per-tick models on close. A
                    // big pipeline's scene holds thousands of rects, texts
                    // and hit zones that would otherwise sit in memory for
                    // the rest of the session.
                    if !active {
                        let state = ui.global::<InspectorState>();
                        state.set_have_graph(false);
                        state.set_graph(GraphDump::default());
                        state.set_tracks(Rc::new(slint::VecModel::default()).into());
                        state.set_sources_lines(Rc::new(slint::VecModel::default()).into());
                        state.set_internals_lines(Rc::new(slint::VecModel::default()).into());
                        state.set_sink_lines(Rc::new(slint::VecModel::default()).into());
                        state.set_have_bitrate(false);
                        state.set_video_bitrate_path(Default::default());
                        state.set_audio_bitrate_path(Default::default());
                        state.set_have_buffering(false);
                    }
                }
            }
        });

        // Backstop for obstruction changes while the video sits above the GUI but *no frames
        // are flowing* (e.g. a pause racing the last frame's poll).
        let watchdog = slint::Timer::default();
        watchdog.start(slint::TimerMode::Repeated, Duration::from_millis(250), {
            let tick = tick.clone();
            let ui_weak = ui.as_weak();
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let mut t = tick.borrow_mut();
                if t.video_sink.self_clocked() && ui.global::<Bridge>().get_video_obstructed() {
                    t.video_sink.set_video_obstructed(true, true);
                }
            }
        });
        _obstruction_watchdog = Some(watchdog);
    }

    let gui_tx = if let Some(ui) = &ui {
        let (gui_tx, gui_rx) = mpsc::unbounded_channel::<gui::UpdateGuiCommand>();

        // Create, wire and show the tray only once the listening port is
        // committed (see `Application::resolve_listen_port`), so a port conflict
        // that ends in quitting never starts a tray at all.
        let on_show_tray: Box<dyn FnOnce()> = {
            #[cfg(feature = "systray")]
            {
                if want_systray {
                    let ui_weak = ui.as_weak();
                    let holder = systray_holder.clone();
                    Box::new(move || {
                        let systray = match SystemTray::new() {
                            Ok(systray) => systray,
                            Err(err) => {
                                error!(?err, "Failed to create system tray");
                                return;
                            }
                        };
                        systray.on_toggle_window({
                            let ui_weak = ui_weak.clone();
                            move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let win = ui.window();
                                    if win.is_visible() {
                                        let _ = win.hide();
                                    } else {
                                        let _ = win.show();
                                    }
                                }
                            }
                        });
                        systray.on_quit(|| {
                            let _ = slint::quit_event_loop();
                        });
                        log_if_err!(systray.show());
                        // Keep it alive for the rest of the session.
                        *holder.borrow_mut() = Some(systray);
                    })
                } else {
                    Box::new(|| {})
                }
            }
            #[cfg(not(feature = "systray"))]
            {
                Box::new(|| {})
            }
        };

        gui::spawn_command_handler(ui.as_weak(), gui_rx, renderer_tx.unwrap(), on_show_tray);
        Some(gui_tx)
    } else {
        None
    };

    let gui = GuiController::new(gui_tx, gui_is_visible.clone());

    #[allow(unused_variables)]
    #[cfg(not(target_os = "android"))]
    let no_main_window = settings.no_main_window();
    let event_loop_jh = RUNTIME.spawn({
        let ui_weak = ui.as_ref().map(|ui| ui.as_weak());
        let msg_tx = msg_tx.clone();
        async move {
            gstreamer::init_and_load_plugins();

            let video_sink_elem = if let Some(ui_weak) = ui_weak {
                let sink = video::FSink::new();
                sink.connect("frame-available", false, move |_| {
                    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.global::<Bridge>().invoke_new_video_frame();
                    });

                    None
                });

                let video_sink_elem = sink.clone();
                *sink_mutex.lock() = Some(sink);
                Some(video_sink_elem)
            } else {
                None
            };

            let app = application::Application::new(
                gui,
                video_sink_elem.map(|e| e.upcast()),
                msg_tx,
                #[cfg(target_os = "android")]
                android_app,
                #[cfg(not(target_os = "android"))]
                settings,
            )
            .await;

            // This task is detached, so panicking here left the Slint loop running a UI with no
            // protocol handling behind it, no pipeline teardown, and mDNS still advertising. Fail
            // visibly and quit instead, so both halves of the process go down together.
            let result = match app {
                Ok(app) => app.run_event_loop(event_rx, fin_tx).await,
                Err(err) => Err(err),
            };

            if let Err(err) = result {
                error!(?err, "Receiver event loop failed");
                let _ = slint::quit_event_loop();
            }
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

        // Without a tray, `run()` already quits when the window is closed,
        // which is correct both normally and while the conflict dialog is up.
        #[cfg(any(target_os = "android", not(feature = "systray")))]
        ui.run()?;

        #[cfg(feature = "systray")]
        if want_systray {
            // In tray mode a normal window-close hides to the tray, but while
            // the port-conflict dialog is up the app hasn't committed to
            // running, so closing the window then should quit instead.
            ui.window().on_close_requested({
                let ui_weak = ui.as_weak();
                move || {
                    let resolving = ui_weak
                        .upgrade()
                        .is_some_and(|ui| ui.global::<Bridge>().get_show_port_conflict());
                    if resolving {
                        let _ = slint::quit_event_loop();
                    }
                    slint::CloseRequestResponse::HideWindow
                }
            });

            if !no_main_window {
                ui.show()?;
            }
            // The tray is created and shown later, via the `on_show_tray` hook,
            // once the FCast port is committed (see `spawn_command_handler`).
            slint::run_event_loop_until_quit()?;
        } else {
            // No tray: window close quits (default `run()` behavior).
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
