// Forces the static GStreamer link line and isolates the process from on-disk
// plugins before main.
use gst_static_env as _;

pub mod cue;
/// Cue layout and the display list it ends at, which moved to the slint fork
/// so the renderer that draws the scene and the code that builds it live
/// together. Named here under their old paths, since this crate's callers
/// reach them through it.
pub use i_slint_cue::{layout as cue_ir, scene as cue_scene};
pub mod subpic;
pub mod video;
