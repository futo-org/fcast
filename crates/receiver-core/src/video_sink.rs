use anyhow::{Result, anyhow};

use crate::{placebo::PlaceboContext, video::RawFrame};

pub trait VideoSink {
    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &RawFrame,
        target_size: (u32, u32),
    ) -> Result<()>;

    fn flush_cache(&mut self, placebo: &mut PlaceboContext) {
        placebo.flush_cache();
    }

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
        frame: &RawFrame,
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
