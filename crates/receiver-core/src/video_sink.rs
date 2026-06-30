use anyhow::{Result, anyhow};

use crate::{placebo::PlaceboContext, video::Frame};

pub trait VideoSink {
    /// Called once during `RenderingSetup`, on the render/event-loop thread, after the GL and
    /// libplacebo contexts have been created. This gives the sink a chance to grab native window
    /// handles (e.g. the Wayland `wl_surface` it needs to parent a subsurface to). The default
    /// implementation does nothing, which is correct for sinks that render into Slint's own
    /// surface (e.g. [`SwapchainSink`]).
    fn setup(&mut self, _window: &slint::Window) {}

    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &Frame,
        target_size: (u32, u32),
    ) -> Result<()>;

    /// Whether [`render`](Self::render) must be called on *every* Slint repaint, or only when the
    /// frame or target size actually changes. Sinks that composite into Slint's own GL surface
    /// must return `true`: Slint clears that surface every repaint, so the video has to be redrawn
    /// each time (e.g. on focus/cursor repaints) or it would vanish. A sink that commits to an
    /// independent presentation surface (a Wayland subsurface) can return `false` — the compositor
    /// keeps showing the last committed buffer across Slint repaints, so re-rendering an unchanged
    /// frame would be wasted GPU work. Default: `true`.
    fn needs_render_every_repaint(&self) -> bool {
        true
    }

    fn flush_cache(&mut self, placebo: &mut PlaceboContext) {
        placebo.flush_cache();
    }

    /// Called when the current stream ends (EOS) and there is no longer a frame to display.
    /// Sinks that own a separate presentation surface (e.g. a Wayland subsurface) must detach
    /// their last buffer here, otherwise the stale final frame lingers on screen across a
    /// stop/play transition. Sinks that render into Slint's own surface need do nothing — the
    /// rendering loop clears that surface every frame. Default: no-op.
    fn clear(&mut self) {}

    fn get_clear_color(&self) -> [f32; 4];

    fn teardown(&mut self, _placebo: &mut PlaceboContext) {}
}

pub struct SwapchainSink {
    size: (u32, u32),
}

impl SwapchainSink {
    pub fn new() -> Self {
        Self { size: (0, 0) }
    }
}

impl VideoSink for SwapchainSink {
    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        _gl: &glow::Context,
        frame: &Frame,
        target_size: (u32, u32),
    ) -> Result<()> {
        if target_size != self.size {
            placebo.resize_swapchain(target_size.0 as i32, target_size.1 as i32);
            self.size = target_size;
        }
        let Some(swframe) = placebo.start_frame() else {
            // start_frame can fail if the window is invisible or inaccessible, so it's not
            // really an error condition or important that it failed
            return Ok(());
        };
        placebo
            .render_frame(&swframe, frame)
            .map_err(|err| anyhow!("placebo swapchain render failed: {err}"))?;
        placebo.submit_frame();
        Ok(())
    }

    fn get_clear_color(&self) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }
}
