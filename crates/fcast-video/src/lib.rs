// Forces the static GStreamer link line and isolates the process from on-disk
// plugins before main.
use gst_static_env as _;

pub mod cue;
pub mod cue_ir;
pub mod render_latency;
pub mod render_options;
pub mod subpic;
pub mod video;

// The GPU renderer, behind the off-by-default `render` feature so that test
// and fuzz builds of the cue/subpicture engine never build libplacebo.
#[cfg(feature = "render")]
pub mod opengl;
#[cfg(feature = "render")]
pub mod placebo;
#[cfg(feature = "render")]
pub mod video_sink;

#[cfg(all(target_os = "linux", feature = "render"))]
pub mod dmabuf;
#[cfg(all(target_os = "linux", feature = "render"))]
pub mod egl;
#[cfg(target_os = "macos")]
pub mod iosurface;
#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
pub mod wayland_sink;

#[cfg(feature = "render")]
pub use video_sink::{SwapchainSink, VideoSink};
#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
pub use wayland_sink::WaylandSubsurfaceSink;
