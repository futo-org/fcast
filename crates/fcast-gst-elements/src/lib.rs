// Forces the static GStreamer link line and isolates the process from on-disk
// plugins before main.
use gst_static_env as _;

pub mod companion_ctx;

pub mod fcompsrc;
pub mod imagedec;
pub mod imagetypefind;
#[cfg(target_os = "linux")]
pub mod vajpegdec;
