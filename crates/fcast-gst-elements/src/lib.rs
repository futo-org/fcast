// Forces the static GStreamer link line and isolates the process from on-disk
// plugins before main.
use gst_static_env as _;

pub mod companion_ctx;

#[cfg(feature = "textoverlay")]
pub mod fcasttextoverlay;
// The C webrtcbin receive bins, android only: desktop mirrors through
// fcast-webrtc on webrtcbin2, which registers the same element names.
#[cfg(target_os = "android")]
pub mod fcastwhepsrcbin;
pub mod fcompsrc;
#[cfg(target_os = "android")]
pub mod fwebrtcsrc;
pub mod imagedec;
pub mod imagetypefind;
pub mod sabrumpsrc;
#[cfg(target_os = "linux")]
pub mod vajpegdec;
