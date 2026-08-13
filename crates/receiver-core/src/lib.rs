use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;

#[cfg(not(target_os = "android"))]
pub use clap;
pub use tracing;
#[cfg(feature = "airplay")]
mod airplay;
pub mod application;
pub mod config;
mod external_subtitles;
pub mod fcast;
mod freeze_watchdog;
mod gcast;
pub mod gstreamer;
pub mod gui;
pub mod image;
pub mod inspector_graph;
pub mod logging;
#[cfg(not(target_os = "android"))]
mod mdns;
pub mod media_formats;
mod media_source;
pub mod message;
pub mod player;
mod queue_cache;
mod raop;
pub mod ui_types;
mod user_agent;
pub mod utils;

// These elements moved to fcast-gst-elements so testing them no longer builds
// the receiver (and its UI). Re-exported under their old crate-root paths so
// every existing `crate::fcompsrc::...` call site keeps resolving unchanged.
#[cfg(target_os = "linux")]
pub use fcast_gst_elements::vajpegdec;
pub use fcast_gst_elements::{fcompsrc, fwebrtcsrc, imagedec, imagetypefind};

// Renderer *settings* only: plain data, no libplacebo. This is what the CLI and
// the config store carry, so it stays available with `render` off.
use fcast_video::render_options::{RenderProfile, RenderingOptions};

// Everything below is the GPU render surface, re-exported for the receiver
// binaries. Behind `render` so a test build of this crate never drags in
// libplacebo (and the C library its -sys crate builds).
// Re-exported: the fhs receiver's pixmap sink uses `receiver_core::egl`.
#[cfg(all(target_os = "linux", feature = "render"))]
pub use fcast_video::egl;
#[cfg(feature = "render")]
pub use fcast_video::{SwapchainSink, VideoSink};
#[cfg(feature = "render")]
pub use glow;
#[cfg(feature = "render")]
pub use libplacebo;

pub use gst;
pub use gst_video;

use crate::{fcast::Operation, player::PlayerState};

pub use raop::{Configuration, device_name_hash, hash_to_string, txt_properties};

pub type SenderId = u32;

use message::{Mdns, Raop};

pub const FCAST_TCP_PORT: u16 = 46899;
pub const GCAST_TCP_PORT: u16 = 8009;
pub type MediaItemId = u64;

pub use message::MessageSender;

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

// Own crate so receiver-core and the GStreamer element crate share one thread
// pool.
pub use fcast_runtime::RUNTIME;

struct GCastUpdateSender(Option<UnboundedSender<gcast::StatusUpdate>>);

impl GCastUpdateSender {
    fn send(&mut self, update: gcast::StatusUpdate) {
        let Some(tx) = self.0.as_ref() else {
            return;
        };
        if tx.send(update).is_err() {
            // The gcast server stopped (e.g. its port was taken); make later updates
            // no-ops.
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
    render_profile: Option<RenderProfile>,
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

/// The receiver's effective settings: parsed CLI flags plus the persisted
/// [`config::ConfigStore`], resolved by the accessors below.
///
/// A passed CLI flag always wins; the flags are one-directional, so the CLI can
/// force a behavior on but never off.
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

    /// Frame render profile, resolved from `--render-profile` then `[video]
    /// render_profile`.
    pub fn render_profile(&self) -> RenderProfile {
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
            .unwrap_or(RenderProfile::Fast)
    }

    pub fn rendering_options(&self) -> RenderingOptions {
        RenderingOptions {
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

    /// Broadcast name for RAOP. Defaults to `FCast-<hostname>`.
    pub fn raop_name(&self) -> String {
        self.config
            .get()
            .raop
            .name
            .as_deref()
            .map(expand_name_vars)
            .unwrap_or_else(mdns::fcast_device_name)
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
fn parse_render_profile(value: &str) -> Option<RenderProfile> {
    match <RenderProfile as clap::ValueEnum>::from_str(value, true) {
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

/// Expand `{hostname}`, the only variable allowed in a configured broadcast
/// name.
#[cfg(not(target_os = "android"))]
fn expand_name_vars(template: &str) -> String {
    template.replace("{hostname}", &mdns::hostname())
}

/// Cap the number of glibc malloc arenas: GStreamer's many short-lived worker
/// threads each get an arena that never returns its freed pages, so RSS climbs
/// to the sum of every arena's high-water mark. An explicit MALLOC_ARENA_MAX
/// wins.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn tune_allocator() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_some() {
        return;
    }
    // SAFETY: `mallopt` takes two ints and has no memory-safety preconditions.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn tune_allocator() {}

/// Debug builds only: let any process attach and snapshot thread stacks, which
/// Yama's default `ptrace_scope=1` (ancestor tracers only) otherwise blocks.
#[cfg(all(debug_assertions, target_os = "linux"))]
pub fn allow_ptrace_attach() {
    // SAFETY: prctl(PR_SET_PTRACER, ...) only adjusts this process's Yama tracer
    // allowance.
    unsafe {
        libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
    }
}

#[cfg(not(all(debug_assertions, target_os = "linux")))]
pub fn allow_ptrace_attach() {}
