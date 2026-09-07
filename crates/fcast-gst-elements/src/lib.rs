// Forces the static GStreamer link line and isolates the process from on-disk
// plugins before main.
use gst_static_env as _;

pub mod companion_ctx;

#[cfg(feature = "textoverlay")]
pub mod fcasttextoverlay;
pub mod fcompsrc;
pub mod imagedec;
pub mod imagetypefind;
pub mod sabrumpsrc;
#[cfg(target_os = "linux")]
pub mod vajpegdec;
