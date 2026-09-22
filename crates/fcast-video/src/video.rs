//! The overlay an Android lane draws, and the space it is measured in.
//!
//! Android takes subtitles as pixels: the cue rasterizer's vello_cpu backend
//! and the subpicture decoders both end at one of these, and the bridge turns
//! it into a `slint::Image`. The desktop lane takes neither, it takes display
//! lists and regions and lets the renderer draw them.
//!
//! [`OverlaySpace`] is not defined here. It is `i-slint-video`'s, because the
//! renderer that honours the distinction is the one that gets to say what it
//! means, and two spellings of it drifted apart once already.

pub use i_slint_video::OverlaySpace;
use std::sync::Arc;

/// Pixels to draw over the picture, owned.
///
/// Not [`i_slint_video::Overlay`], which borrows its pixels and is
/// premultiplied because that is what the renderer composites in. This is
/// storage, and it is **straight** alpha because its one consumer builds a
/// `slint::Image` out of it, which is the convention there. Converting is the
/// bridge's job on the day a lane wants both.
#[derive(Debug)]
pub struct Overlay {
    /// RGBA8 pixel data (tightly packed), `width * height * 4` bytes, straight
    /// (non-premultiplied) alpha.
    ///
    /// Refcount-shared rather than owned: a cue strip is megabytes and an
    /// `Overlay` is built per displayed frame, so the buffer is cloned by
    /// pointer. The upload path only reads `&pixels[..]`, which derefs
    /// identically to a `Vec`.
    pub pixels: Arc<Vec<u8>>,
    /// Texture dimensions of `pixels`.
    pub width: u32,
    pub height: u32,
    /// Render rectangle, in the pixels of [`Overlay::space`].
    pub x: i32,
    pub y: i32,
    pub render_width: u32,
    pub render_height: u32,
    /// Which coordinate space `x`/`y`/`render_*` are in.
    pub space: OverlaySpace,
}
