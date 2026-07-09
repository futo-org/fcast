use anyhow::{Result, anyhow};

const BG: (f64, f64, f64) = (0x15 as f64 / 255.0, 0x18 as f64 / 255.0, 0x1F as f64 / 255.0);
const TILE: (f64, f64, f64) = (0x29 as f64 / 255.0, 0x2B as f64 / 255.0, 0x32 as f64 / 255.0);
const NOTE: (f64, f64, f64) = (0x3B as f64 / 255.0, 0x3E as f64 / 255.0, 0x48 as f64 / 255.0);
const TITLE: (f64, f64, f64) = (0xE4 as f64 / 255.0, 0xE6 as f64 / 255.0, 0xED as f64 / 255.0);
const ARTIST: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.6);

pub fn render(
    title: &str,
    artist: &str,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<(Vec<u8>, u32, u32)> {
    use cairo::{Context, Format, ImageSurface};

    let w = width.max(1) as i32;
    let h = height.max(1) as i32;
    let scale = scale.max(0.5) as f64;

    let mut surface = ImageSurface::create(Format::ARgb32, w, h)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    {
        let cr = Context::new(&surface).map_err(|e| anyhow!("cairo context failed: {e}"))?;
        cr.set_source_rgb(BG.0, BG.1, BG.2);
        cr.paint().map_err(|e| anyhow!("cairo paint failed: {e}"))?;

        let tile = ((width.min(height) as f64) * 0.3).clamp(96.0, 420.0);
        let gap = 24.0 * scale;

        let title_layout = (!title.is_empty()).then(|| text_layout(&cr, title, 28.0 * scale, true));
        let artist_layout =
            (!artist.is_empty()).then(|| text_layout(&cr, artist, 18.0 * scale, false));

        let title_h = title_layout.as_ref().map_or(0.0, |l| l.pixel_size().1 as f64);
        let artist_h = artist_layout
            .as_ref()
            .map_or(0.0, |l| l.pixel_size().1 as f64);
        let text_gap = 8.0 * scale;

        let mut group_h = tile;
        if title_layout.is_some() {
            group_h += gap + title_h;
        }
        if artist_layout.is_some() {
            group_h += text_gap + artist_h;
        }

        let cx = w as f64 / 2.0;
        let mut y = ((h as f64) - group_h) / 2.0;

        let tile_x = cx - tile / 2.0;
        rounded_rect(&cr, tile_x, y, tile, tile, 4.0 * scale);
        cr.set_source_rgb(TILE.0, TILE.1, TILE.2);
        cr.fill().map_err(|e| anyhow!("cairo fill failed: {e}"))?;

        let note = text_layout(&cr, "\u{266B}", tile * 0.5, false);
        let (note_w, note_h) = note.pixel_size();
        cr.set_source_rgb(NOTE.0, NOTE.1, NOTE.2);
        cr.move_to(cx - note_w as f64 / 2.0, y + (tile - note_h as f64) / 2.0);
        pangocairo::functions::show_layout(&cr, &note);

        y += tile;
        if let Some(layout) = title_layout {
            y += gap;
            cr.set_source_rgb(TITLE.0, TITLE.1, TITLE.2);
            cr.move_to(cx - layout.pixel_size().0 as f64 / 2.0, y);
            pangocairo::functions::show_layout(&cr, &layout);
            y += title_h;
        }
        if let Some(layout) = artist_layout {
            y += text_gap;
            cr.set_source_rgba(ARTIST.0, ARTIST.1, ARTIST.2, ARTIST.3);
            cr.move_to(cx - layout.pixel_size().0 as f64 / 2.0, y);
            pangocairo::functions::show_layout(&cr, &layout);
        }
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
            dst[x * 4] = src[x * 4 + 2];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4];
            dst[x * 4 + 3] = src[x * 4 + 3];
        }
    }

    Ok((rgba, width, height))
}

fn text_layout(cr: &cairo::Context, text: &str, px: f64, bold: bool) -> pango::Layout {
    let mut font = pango::FontDescription::new();
    font.set_family("sans-serif");
    font.set_weight(if bold {
        pango::Weight::Semibold
    } else {
        pango::Weight::Normal
    });
    font.set_absolute_size(px.max(1.0) * pango::SCALE as f64);
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&font));
    layout.set_text(text);
    layout
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let deg = std::f64::consts::PI / 180.0;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -90.0 * deg, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, 90.0 * deg);
    cr.arc(x + r, y + h - r, r, 90.0 * deg, 180.0 * deg);
    cr.arc(x + r, y + r, r, 180.0 * deg, 270.0 * deg);
    cr.close_path();
}
