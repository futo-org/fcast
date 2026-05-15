//! Abstraction over where decoded video frames end up on screen.
//!
//! Each receiver target (desktop, android, fhs) provides an implementation;
//! the rcore rendering loop just hands frames over. The default
//! [`SwapchainSink`] renders straight into libplacebo's GL swapchain, which
//! is what the X11/Wayland/macOS/Windows desktop receiver and the android
//! receiver want. Specialized targets (the fhs receiver, which presents to a
//! fiatlux compositor as a separate pixmap) implement the trait themselves.

use anyhow::{Result, anyhow};

use crate::{placebo::PlaceboContext, video::RawFrame};

pub trait VideoSink: 'static {
    /// Render a decoded video frame.
    ///
    /// `placebo` is the rcore-owned libplacebo context (already set up
    /// against slint's GL context). `target_size` is the current window size
    /// in physical pixels. `caps` is the upstream gst caps for the frame and
    /// may carry HDR metadata such as `mastering-display-info`.
    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &RawFrame,
        target_size: (u32, u32),
        caps: Option<&gst::CapsRef>,
    ) -> Result<()>;

    /// EOS / clear / source-change hook. The default drops libplacebo's
    /// per-frame caches; implementations that hold extra GPU state should
    /// override and free it too.
    fn flush_cache(&mut self, placebo: &mut PlaceboContext) {
        placebo.flush_cache();
    }

    /// Whether the slint UI should be cleared with `alpha = 0` (a transparent
    /// background) before each frame. Returns `false` by default, which
    /// gives the historical opaque-black behavior used by the desktop and
    /// android paths. Sinks that present the window framebuffer as a
    /// separate compositor layer on top of the video (the fhs path) want
    /// `true` so the video shows through wherever slint hasn't drawn.
    fn wants_transparent_clear(&self) -> bool {
        false
    }

    /// Called from slint's `RenderingTeardown`. Release any GPU resources
    /// tied to `placebo` here; `placebo` will be destroyed immediately
    /// afterwards.
    fn teardown(&mut self, _placebo: &mut PlaceboContext) {}
}

/// Default sink: render the video frame through libplacebo's OpenGL
/// swapchain, which writes directly into slint's window framebuffer.
pub struct SwapchainSink {
    last_size: (u32, u32),
}

impl SwapchainSink {
    pub fn new() -> Self {
        Self {
            last_size: (0, 0),
        }
    }
}

impl Default for SwapchainSink {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoSink for SwapchainSink {
    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        _gl: &glow::Context,
        frame: &RawFrame,
        target_size: (u32, u32),
        _caps: Option<&gst::CapsRef>,
    ) -> Result<()> {
        if target_size != self.last_size {
            placebo.resize_swapchain(target_size.0 as i32, target_size.1 as i32);
            self.last_size = target_size;
        }
        let Some(swframe) = placebo.start_frame() else {
            return Ok(());
        };
        placebo
            .render_frame(&swframe, frame)
            .map_err(|err| anyhow!("placebo swapchain render failed: {err}"))?;
        placebo.submit_frame();
        Ok(())
    }

    fn wants_transparent_clear(&self) -> bool {
        false
    }
}
