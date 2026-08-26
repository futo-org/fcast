//! Marker crate that pulls in `libplacebo` with its `vulkan` feature enabled.
//! Empty off-Linux: vulkan is only used by the wayland-subsurface sink.

#[cfg(target_os = "linux")]
pub use libplacebo;
