use anyhow::{Result, anyhow};
use fiatlux::*;
use rcore::{tracing::debug, video::Overlay};
use std::hash::{Hash, Hasher};

use crate::pixmap_video_sink::{GbmAllocator, MappablePixmap};

// Renders the active subtitle onto its own fiatlux surface, composited on top of
// the video. The surface (and its pixmap) only exist while there is something to
// show. Subtitles arrive either as pre-rendered bitmap overlays (the common case
// via VideoOverlayCompositionMeta) or as plain text; both are written directly
// into a linear, mappable dma-buf pixmap (no libplacebo).
pub struct SubtitleSurface {
    client: *mut fl_Client,
    window_id: fl_protocol_WindowId,
    gbm: GbmAllocator,
    surface_id: Option<fl_protocol_SurfaceId>,
    pixmap: Option<MappablePixmap>,
    last_key: Option<u64>,
}

impl SubtitleSurface {
    pub fn new(client: *mut fl_Client, window_id: fl_protocol_WindowId) -> Result<Self> {
        Ok(Self {
            client,
            window_id,
            gbm: GbmAllocator::new(client)?,
            surface_id: None,
            pixmap: None,
            last_key: None,
        })
    }

    pub fn surface_id(&self) -> Option<fl_protocol_SurfaceId> {
        self.surface_id
    }

    /// Show pre-rendered bitmap overlays. Empty slice hides the surface.
    ///
    /// The caller (via the FSink seqnum dedup) only invokes this when the overlays
    /// actually change, so there's no per-frame work while a subtitle is static.
    pub fn set_overlays(&mut self, overlays: &[Overlay], window_size: (u32, u32)) -> Result<()> {
        if overlays.is_empty() {
            self.clear();
            return Ok(());
        }

        // Composite all overlays into a single pixmap covering their bounding box
        // (positioning of the surface itself is done by the compositor).
        let min_x = overlays.iter().map(|o| o.x).min().unwrap();
        let min_y = overlays.iter().map(|o| o.y).min().unwrap();
        let max_x = overlays
            .iter()
            .map(|o| o.x + o.pix_buffer.width() as i32)
            .max()
            .unwrap();
        let max_y = overlays
            .iter()
            .map(|o| o.y + o.pix_buffer.height() as i32)
            .max()
            .unwrap();
        let width = (max_x - min_x).max(1) as u32;
        let height = (max_y - min_y).max(1) as u32;

        let row_bytes = width as usize * 4;
        let mut rgba = vec![0u8; row_bytes * height as usize];
        for overlay in overlays {
            let ow = overlay.pix_buffer.width() as usize;
            let oh = overlay.pix_buffer.height() as usize;
            let src = overlay.pix_buffer.as_bytes();
            let ox = (overlay.x - min_x) as usize;
            let oy = (overlay.y - min_y) as usize;
            for row in 0..oh {
                let dst = ((oy + row) * width as usize + ox) * 4;
                let s = row * ow * 4;
                rgba[dst..dst + ow * 4].copy_from_slice(&src[s..s + ow * 4]);
            }
        }

        debug!(count = overlays.len(), width, height, "subtitle: rendering overlays");
        self.present(&rgba, width, height, window_size)?;
        self.last_key = None;
        Ok(())
    }

    /// Show plain-text subtitle lines. Empty slice hides the surface.
    pub fn set_subtitles(&mut self, lines: &[String], window_size: (u32, u32), scale: f32) -> Result<()> {
        if lines.is_empty() {
            self.clear();
            return Ok(());
        }

        let max_width = window_size.0;
        let font_px = (30.0 * scale).round().max(1.0) as u32;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        lines.hash(&mut hasher);
        font_px.hash(&mut hasher);
        max_width.hash(&mut hasher);
        let key = hasher.finish();
        if self.pixmap.is_some() && self.surface_id.is_some() && self.last_key == Some(key) {
            return Ok(());
        }

        debug!(?lines, font_px, max_width, "subtitle: rendering text");
        let text = lines.join("\n");
        let (rgba, width, height) = rasterize_text(&text, font_px, max_width)?;
        self.present(&rgba, width, height, window_size)?;
        self.last_key = Some(key);
        Ok(())
    }

    fn present(&mut self, rgba: &[u8], width: u32, height: u32, window_size: (u32, u32)) -> Result<()> {
        let needs_new = match &self.pixmap {
            Some(pixmap) => pixmap.width() != width || pixmap.height() != height,
            None => true,
        };
        if needs_new {
            self.pixmap = None;
            self.pixmap = Some(MappablePixmap::new(self.client, &self.gbm, width, height)?);
        }
        let pixmap = self.pixmap.as_ref().unwrap();

        let row_bytes = width as usize * 4;
        pixmap.write(|dst, stride| {
            for y in 0..height as usize {
                dst[y * stride..y * stride + row_bytes]
                    .copy_from_slice(&rgba[y * row_bytes..y * row_bytes + row_bytes]);
            }
        })?;

        if self.surface_id.is_none() {
            let surface_id = self.create_surface()?;
            debug!(surface_id = surface_id.value, width, height, "subtitle: created surface");
            self.surface_id = Some(surface_id);
        }

        unsafe {
            fl_discard_reply(
                self.client,
                fl_set_surface_pixmap(self.client, self.surface_id.unwrap(), pixmap.pixmap_id())
                    .value,
            );
        }

        self.reposition(window_size);
        Ok(())
    }

    /// Positions the subtitle surface at the bottom-center of the window, raised
    /// slightly off the bottom edge. No-op if there's no surface yet.
    pub fn reposition(&self, window_size: (u32, u32)) {
        let (Some(surface_id), Some(pixmap)) = (self.surface_id, self.pixmap.as_ref()) else {
            return;
        };
        let margin = (window_size.1 as f32 * 0.05).round() as i32;
        let x = ((window_size.0 as i32 - pixmap.width() as i32) / 2).max(0);
        let y = (window_size.1 as i32 - pixmap.height() as i32 - margin).max(0);
        unsafe {
            fl_discard_reply(
                self.client,
                fl_set_surface_position(self.client, surface_id, x, y).value,
            );
        }
    }

    pub fn clear(&mut self) {
        if let Some(surface_id) = self.surface_id.take() {
            debug!(surface_id = surface_id.value, "subtitle: destroying surface");
            unsafe {
                fl_discard_reply(
                    self.client,
                    fl_destroy_surface(self.client, surface_id).value,
                );
            }
        }
        self.pixmap = None;
        self.last_key = None;
    }

    fn create_surface(&self) -> Result<fl_protocol_SurfaceId> {
        unsafe {
            let mut reply: fl_reply_CreateSurface = std::mem::zeroed();
            // z_index 0: above the video surface (which is at -1)
            if !fl_receive_reply_create_surface(
                self.client,
                fl_create_surface(self.client, self.window_id, 0, false),
                &mut reply,
            ) {
                return Err(anyhow!("Failed to create subtitle surface"));
            }
            Ok(reply.surface_id)
        }
    }
}

impl Drop for SubtitleSurface {
    fn drop(&mut self) {
        self.clear();
    }
}

// Rasterizes subtitle text into a tightly-packed R,G,B,A buffer (premultiplied
// alpha) sized to the text plus padding.
fn rasterize_text(text: &str, font_px: u32, max_width: u32) -> Result<(Vec<u8>, u32, u32)> {
    use cairo::{Context, Format, ImageSurface, Operator};

    const PAD_X: i32 = 10;
    const PAD_Y: i32 = 5;

    let mut font = pango::FontDescription::new();
    font.set_family("sans-serif");
    font.set_absolute_size(font_px as f64 * pango::SCALE as f64);

    let measure = ImageSurface::create(Format::ARgb32, 1, 1)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    let measure_cr = Context::new(&measure).map_err(|e| anyhow!("cairo context failed: {e}"))?;
    let layout = pangocairo::functions::create_layout(&measure_cr);
    layout.set_font_description(Some(&font));
    layout.set_alignment(pango::Alignment::Center);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_width((max_width as i32 - PAD_X * 2).max(1) * pango::SCALE);
    layout.set_text(text);
    let (text_w, text_h) = layout.pixel_size();

    let width = (text_w + PAD_X * 2).max(1) as u32;
    let height = (text_h + PAD_Y * 2).max(1) as u32;

    let mut surface = ImageSurface::create(Format::ARgb32, width as i32, height as i32)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    {
        let cr = Context::new(&surface).map_err(|e| anyhow!("cairo context failed: {e}"))?;
        cr.set_operator(Operator::Source);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
        cr.paint().map_err(|e| anyhow!("cairo paint failed: {e}"))?;

        cr.set_operator(Operator::Over);
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.move_to(PAD_X as f64, PAD_Y as f64);
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&font));
        layout.set_alignment(pango::Alignment::Center);
        layout.set_wrap(pango::WrapMode::WordChar);
        layout.set_width(text_w * pango::SCALE);
        layout.set_text(text);
        pangocairo::functions::show_layout(&cr, &layout);
    }
    surface.flush();

    let stride = surface.stride() as usize;
    let data = surface
        .data()
        .map_err(|e| anyhow!("cairo surface data borrow failed: {e}"))?;

    let row_bytes = width as usize * 4;
    let mut rgba = vec![0u8; row_bytes * height as usize];
    for y in 0..height as usize {
        let src = &data[y * stride..];
        let dst = &mut rgba[y * row_bytes..];
        for x in 0..width as usize {
            // cairo ARGB32 is native-endian premultiplied; on little-endian the
            // bytes are [B, G, R, A]. Swap to R,G,B,A for FL_PIXMAP_FORMAT_RGBA8.
            dst[x * 4] = src[x * 4 + 2];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4];
            dst[x * 4 + 3] = src[x * 4 + 3];
        }
    }

    Ok((rgba, width, height))
}
