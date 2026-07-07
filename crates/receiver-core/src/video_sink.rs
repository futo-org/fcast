use anyhow::{Result, anyhow};

use crate::{placebo::PlaceboContext, video::Frame};

pub trait VideoSink {
    /// Called once during `RenderingSetup`, on the render/event-loop thread, after the GL and
    /// libplacebo contexts have been created.
    fn setup(&mut self, _window: &slint::Window) {}

    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &Frame,
        target_size: (u32, u32),
    ) -> Result<()>;

    /// Whether [`render`](Self::render) must be called on *every* Slint repaint, or only when the
    /// frame or target size actually changes.
    fn needs_render_every_repaint(&self) -> bool {
        true
    }

    fn flush_cache(&mut self, placebo: &mut PlaceboContext) {
        placebo.flush_cache();
    }

    /// GUI hint: whether any UI element (playback controls, subtitles, toasts, ...) is currently
    /// drawn over the video area.
    fn set_video_obstructed(&mut self, _obstructed: bool, _commit_parent: bool) {}

    /// Whether the sink currently presents independently of the GUI render loop (e.g. its
    /// subsurface is stacked above the GUI, whose redraw cycle may therefore be parked). When
    /// true, new frames are delivered via [`render_standalone`](Self::render_standalone) from the
    /// event loop instead of waiting for a repaint.
    fn self_clocked(&self) -> bool {
        false
    }

    /// Render a frame without the GUI's GL context (event-loop clocked, not repaint clocked).
    /// Returns `Ok(false)` if the sink can't (caller should fall back to a repaint-driven
    /// render). Only meaningful for sinks that return `true` from
    /// [`self_clocked`](Self::self_clocked). Default: `Ok(false)`.
    fn render_standalone(&mut self, _frame: &Frame, _target_size: (u32, u32)) -> Result<bool> {
        Ok(false)
    }

    /// Like [`flush_cache`](Self::flush_cache) but without the GUI's libplacebo context, for use
    /// from the event loop (the GL context isn't current there). Sinks with their own context
    /// flush it here; the caller still owes a regular `flush_cache` on the next repaint tick.
    fn flush_cache_standalone(&mut self) {}

    /// Called when the current stream ends (EOS) and there is no longer a frame to display.  Sinks
    /// that own a separate presentation surface must detach their last buffer here, otherwise the
    /// stale final frame lingers on screen across a stop/play transition.
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
