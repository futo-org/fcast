use std::sync::Arc;

/// Display rotation a stream carries (image-orientation tag), applied by the
/// video lane when it draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Rotate0,
    /// 90° clockwise.
    Rotate90,
    Rotate180,
    /// 270° clockwise
    Rotate270,
}

/// The coordinate space an [`Overlay`]'s render rectangle is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlaySpace {
    /// Source-frame (video) pixels: the overlay is scaled and rotated with the
    /// video, which is what upstream composition metas expect.
    #[default]
    SrcFrame,
    /// Window (display) pixels: the overlay is composited onto the destination
    /// at native resolution, unscaled and upright.
    Window,
}

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
