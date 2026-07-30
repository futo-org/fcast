pub mod opengl;
pub mod placebo;
pub mod render_latency;
pub mod video;
pub mod video_sink;

#[cfg(target_os = "linux")]
pub mod dmabuf;
#[cfg(target_os = "linux")]
pub mod egl;
#[cfg(target_os = "macos")]
pub mod iosurface;
#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
pub mod wayland_sink;

pub use video_sink::{SwapchainSink, VideoSink};
#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
pub use wayland_sink::WaylandSubsurfaceSink;
