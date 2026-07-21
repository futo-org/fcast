use anyhow::{Result, anyhow};
use fiatlux::*;
use rcore::glow::{self, HasContext};
use rcore::video::Overlay as VideoOverlay;
use std::hash::{Hash, Hasher};

use crate::pixmap_video_sink::{FhsPixmapSink, RenderTarget};

const ACCENT: (f32, f32, f32, f32) = (57.0 / 255.0, 137.0 / 255.0, 218.0 / 255.0, 1.0);
const PANEL: (f32, f32, f32, f32) = (0.13, 0.13, 0.13, 0.6);
const ICON: (f32, f32, f32, f32) = (1.0, 1.0, 1.0, 1.0);
const TARGET_POOL: usize = 3;

const VERT_SRC: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_local;
layout(location = 2) in vec2 a_uv;
uniform vec2 u_viewport;
out vec2 v_local;
out vec2 v_uv;
void main() {
    vec2 ndc = vec2(a_pos.x / u_viewport.x * 2.0 - 1.0, a_pos.y / u_viewport.y * 2.0 - 1.0);
    gl_Position = vec4(ndc, 0.0, 1.0);
    v_local = a_local;
    v_uv = a_uv;
}
"#;

const FRAG_SRC: &str = r#"#version 300 es
precision mediump float;
in vec2 v_local;
in vec2 v_uv;
uniform vec4 u_color;
uniform vec2 u_halfsize;
uniform float u_radius;
uniform int u_use_sdf;
uniform int u_use_tex;
uniform int u_use_tex_rgba;
uniform int u_use_triangle;
uniform vec2 u_tri0;
uniform vec2 u_tri1;
uniform vec2 u_tri2;
uniform sampler2D u_tex;
out vec4 frag_color;
float sd_triangle(vec2 p, vec2 p0, vec2 p1, vec2 p2) {
    vec2 e0 = p1 - p0, e1 = p2 - p1, e2 = p0 - p2;
    vec2 v0 = p - p0, v1 = p - p1, v2 = p - p2;
    vec2 pq0 = v0 - e0 * clamp(dot(v0, e0) / dot(e0, e0), 0.0, 1.0);
    vec2 pq1 = v1 - e1 * clamp(dot(v1, e1) / dot(e1, e1), 0.0, 1.0);
    vec2 pq2 = v2 - e2 * clamp(dot(v2, e2) / dot(e2, e2), 0.0, 1.0);
    float s = sign(e0.x * e2.y - e0.y * e2.x);
    vec2 d = min(min(vec2(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
                     vec2(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))),
                     vec2(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x)));
    return -sqrt(d.x) * sign(d.y);
}
void main() {
    if (u_use_tex_rgba == 1) {
        frag_color = texture(u_tex, v_uv);
        return;
    }
    float alpha = u_color.a;
    if (u_use_sdf == 1) {
        vec2 d = abs(v_local) - (u_halfsize - vec2(u_radius));
        float dist = length(max(d, vec2(0.0))) + min(max(d.x, d.y), 0.0) - u_radius;
        alpha *= clamp(0.5 - dist, 0.0, 1.0);
    }
    if (u_use_triangle == 1) {
        float dist = sd_triangle(v_local, u_tri0, u_tri1, u_tri2);
        alpha *= clamp(0.5 - dist, 0.0, 1.0);
    }
    if (u_use_tex == 1) {
        alpha *= texture(u_tex, v_uv).a;
    }
    frag_color = vec4(u_color.rgb * alpha, alpha);
}
"#;

pub struct Playback {
    pub elapsed_s: f64,
    pub duration_s: f64,
    pub paused: bool,
}

struct TextLayer {
    tex: glow::Texture,
    w: i32,
    h: i32,
}

// Content laid out top-to-bottom in the surface-local pixel space.
struct Layout {
    content_w: i32,
    content_h: i32,
    surface_x: i32,
    surface_y: i32,
    sub: Option<(f64, f64)>,
    bar_x0: f64,
    bar_x1: f64,
    bar_y0: f64,
    bar_y1: f64,
    bar_radius: f64,
    label_y: f64,
    label_font: f64,
    button_cx: f64,
    button_cy: f64,
    button_r: f64,
    title: Option<(f64, f64)>,
}

pub struct Overlay {
    client: *mut fl_Client,
    window_id: fl_protocol_WindowId,
    surface_id: Option<fl_protocol_SurfaceId>,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    scratch_tex: glow::Texture,
    fbo: Option<glow::Framebuffer>,
    targets: Vec<RenderTarget>,
    target_size: (u32, u32),
    next_target: usize,
    subtitle: Option<TextLayer>,
    subtitle_gen: u64,
    title: Option<(TextLayer, u64)>,
    last_key: Option<u64>,
    subtitle_key: Option<u64>,
}

impl Overlay {
    pub fn new(
        client: *mut fl_Client,
        window_id: fl_protocol_WindowId,
        gl: &glow::Context,
    ) -> Result<Self> {
        unsafe {
            let program = compile_program(gl)?;
            let vao = gl
                .create_vertex_array()
                .map_err(|e| anyhow!("create_vertex_array: {e}"))?;
            let vbo = gl.create_buffer().map_err(|e| anyhow!("create_buffer: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let stride = 6 * 4;
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 8);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, 16);
            gl.enable_vertex_attrib_array(2);
            gl.bind_vertex_array(None);

            let scratch_tex = gl.create_texture().map_err(|e| anyhow!("create_texture: {e}"))?;

            Ok(Self {
                client,
                window_id,
                surface_id: None,
                program,
                vao,
                vbo,
                scratch_tex,
                fbo: None,
                targets: Vec::new(),
                target_size: (0, 0),
                next_target: 0,
                subtitle: None,
                subtitle_gen: 0,
                title: None,
                last_key: None,
                subtitle_key: None,
            })
        }
    }

    pub fn button_hit(&self, window_size: (u32, u32), x: i32, y: i32) -> bool {
        let Some(l) = self.layout(window_size) else {
            return false;
        };
        if l.button_r <= 0.0 {
            return false;
        }
        let bx = l.surface_x as f64 + l.button_cx;
        let by = l.surface_y as f64 + l.button_cy;
        let dx = x as f64 - bx;
        let dy = y as f64 - by;
        dx * dx + dy * dy <= l.button_r * l.button_r
    }

    pub fn bar_hit(&self, window_size: (u32, u32), x: i32, y: i32) -> Option<f64> {
        let l = self.layout(window_size)?;
        let bar_w = l.bar_x1 - l.bar_x0;
        if bar_w <= 0.0 {
            return None;
        }
        let lx = x as f64 - l.surface_x as f64;
        let ly = y as f64 - l.surface_y as f64;
        let cy = (l.bar_y0 + l.bar_y1) / 2.0;
        let half = ((l.bar_y1 - l.bar_y0) * 2.0).max(l.button_r * 0.5);
        if (ly - cy).abs() > half || lx < l.bar_x0 || lx > l.bar_x1 {
            return None;
        }
        Some(((lx - l.bar_x0) / bar_w).clamp(0.0, 1.0))
    }

    pub fn set_subtitle_overlays(&mut self, gl: &glow::Context, overlays: &[VideoOverlay]) {
        if overlays.is_empty() {
            self.clear_subtitle(gl);
            self.subtitle_key = None;
            return;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for o in overlays {
            o.seqnum.hash(&mut hasher);
        }
        let key = hasher.finish();
        if self.subtitle_key == Some(key) {
            return;
        }
        if overlays.iter().all(|o| o.pixels.is_empty()) {
            self.clear_subtitle(gl);
            self.subtitle_key = Some(key);
            return;
        }
        if let [only] = overlays {
            self.store_subtitle(gl, &only.pixels, only.width as i32, only.height as i32);
            self.subtitle_key = Some(key);
            return;
        }
        let min_x = overlays.iter().map(|o| o.x).min().unwrap();
        let min_y = overlays.iter().map(|o| o.y).min().unwrap();
        let max_x = overlays
            .iter()
            .map(|o| o.x + o.width as i32)
            .max()
            .unwrap();
        let max_y = overlays
            .iter()
            .map(|o| o.y + o.height as i32)
            .max()
            .unwrap();
        let w = (max_x - min_x).max(1) as u32;
        let h = (max_y - min_y).max(1) as u32;
        let row_bytes = w as usize * 4;
        let mut rgba = vec![0u8; row_bytes * h as usize];
        for overlay in overlays {
            let ow = overlay.width as usize;
            let oh = overlay.height as usize;
            let src = &overlay.pixels;
            let ox = (overlay.x - min_x) as usize;
            let oy = (overlay.y - min_y) as usize;
            for row in 0..oh {
                let dst = ((oy + row) * w as usize + ox) * 4;
                let s = row * ow * 4;
                rgba[dst..dst + ow * 4].copy_from_slice(&src[s..s + ow * 4]);
            }
        }
        self.store_subtitle(gl, &rgba, w as i32, h as i32);
        self.subtitle_key = Some(key);
    }

    pub fn clear_subtitle(&mut self, gl: &glow::Context) {
        if let Some(layer) = self.subtitle.take() {
            unsafe { gl.delete_texture(layer.tex) };
            self.subtitle_gen += 1;
            self.last_key = None;
        }
    }

    fn store_subtitle(&mut self, gl: &glow::Context, rgba: &[u8], w: i32, h: i32) {
        let tex = match self.subtitle.take() {
            Some(layer) => {
                unsafe { upload_texture_into(gl, layer.tex, rgba, w, h) };
                layer.tex
            }
            None => match unsafe { upload_texture(gl, rgba, w, h) } {
                Ok(tex) => tex,
                Err(_) => return,
            },
        };
        self.subtitle = Some(TextLayer { tex, w, h });
        self.subtitle_gen += 1;
        self.last_key = None;
    }

    pub fn set_title(
        &mut self,
        gl: &glow::Context,
        title: &str,
        artist: &str,
        album: &str,
        window_size: (u32, u32),
    ) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        title.hash(&mut hasher);
        artist.hash(&mut hasher);
        album.hash(&mut hasher);
        window_size.hash(&mut hasher);
        let key = hasher.finish();
        if let Some((_, cur)) = &self.title
            && *cur == key
        {
            return;
        }
        if title.is_empty() && artist.is_empty() && album.is_empty() {
            self.clear_title(gl);
            return;
        }
        let Ok((rgba, w, h)) = rasterize_title_row(title, artist, album, window_size) else {
            self.clear_title(gl);
            return;
        };
        let tex = match self.title.take() {
            Some((layer, _)) => {
                unsafe { upload_texture_into(gl, layer.tex, &rgba, w, h) };
                layer.tex
            }
            None => match unsafe { upload_texture(gl, &rgba, w, h) } {
                Ok(tex) => tex,
                Err(_) => return,
            },
        };
        self.title = Some((TextLayer { tex, w, h }, key));
        self.last_key = None;
    }

    pub fn clear_title(&mut self, gl: &glow::Context) {
        if let Some((layer, _)) = self.title.take() {
            unsafe { gl.delete_texture(layer.tex) };
            self.last_key = None;
        }
    }

    // Geometry for hit-testing: the button only exists while the OSD is up.
    fn layout(&self, window_size: (u32, u32)) -> Option<Layout> {
        self.compute(window_size.0 as f64, window_size.1 as f64, true)
    }

    fn compute(&self, ww: f64, wh: f64, show_ui: bool) -> Option<Layout> {
        let has_sub = self.subtitle.is_some();
        let has_title = show_ui && self.title.is_some();
        if !has_sub && !show_ui {
            return None;
        }

        let pad = (wh * 0.006).max(3.0);
        let bar_h = (wh * 0.006).max(3.0);
        let bar_w = ww * 0.5;
        let label_font = (wh * 0.022).max(11.0);
        let label_h = label_font * 1.3;
        let button_r = (wh * 0.03).max(14.0);
        let gap = wh * 0.012;

        let sub_sz = self.subtitle.as_ref().map(|l| (l.w as f64, l.h as f64));
        let title_sz = if has_title {
            self.title.as_ref().map(|(l, _)| (l.w as f64, l.h as f64))
        } else {
            None
        };

        let mut content_w: f64 = 0.0;
        if let Some((w, _)) = sub_sz {
            content_w = content_w.max(w);
        }
        if show_ui {
            content_w = content_w.max(bar_w);
        }
        if let Some((w, _)) = title_sz {
            content_w = content_w.max(w);
        }
        content_w += pad * 2.0;
        let cx = content_w / 2.0;

        let mut y = pad;
        let sub = sub_sz.map(|(w, h)| {
            let sx = cx - w / 2.0;
            let sy = y;
            y += h + gap;
            (sx, sy)
        });

        let (mut bar_x0, mut bar_x1, mut bar_y0, mut bar_y1) = (0.0, 0.0, 0.0, 0.0);
        let (mut label_y, mut button_cx, mut button_cy, mut br) = (0.0, 0.0, 0.0, 0.0);
        let mut title = None;
        if show_ui {
            label_y = y;
            bar_y0 = y + label_h + gap * 0.4;
            bar_y1 = bar_y0 + bar_h;
            bar_x0 = cx - bar_w / 2.0;
            bar_x1 = cx + bar_w / 2.0;
            button_cx = cx;
            button_cy = bar_y1 + gap + button_r;
            br = button_r;
            y = button_cy + button_r;
            if let Some((tw, th)) = title_sz {
                y += gap;
                title = Some((cx - tw / 2.0, y));
                y += th;
            }
        }
        let content_h = y + pad;

        let content_w_i = content_w.ceil().max(1.0) as i32;
        let content_h_i = content_h.ceil().max(1.0) as i32;
        let bottom_offset = if show_ui { wh * 0.04 } else { wh * 0.05 };
        let surface_x = ((ww - content_w) / 2.0).max(0.0) as i32;
        let surface_y = (wh - content_h - bottom_offset).max(0.0) as i32;

        Some(Layout {
            content_w: content_w_i,
            content_h: content_h_i,
            surface_x,
            surface_y,
            sub,
            bar_x0,
            bar_x1,
            bar_y0,
            bar_y1,
            bar_radius: bar_h / 2.0,
            label_y,
            label_font,
            button_cx,
            button_cy,
            button_r: br,
            title,
        })
    }

    pub fn show(
        &mut self,
        sink: &FhsPixmapSink,
        playback: Option<Playback>,
        window_size: (u32, u32),
    ) -> Result<Option<fl_protocol_SurfaceId>> {
        let (ww, wh) = (window_size.0 as f64, window_size.1 as f64);
        let show_ui = playback.is_some();
        let Some(l) = self.compute(ww, wh, show_ui) else {
            self.clear(sink);
            return Ok(None);
        };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        window_size.hash(&mut hasher);
        self.subtitle.as_ref().map(|_| self.subtitle_gen).hash(&mut hasher);
        show_ui.hash(&mut hasher);
        self.title
            .as_ref()
            .filter(|_| show_ui)
            .map(|(_, k)| *k)
            .hash(&mut hasher);
        if let Some(p) = &playback {
            (p.elapsed_s as u64).hash(&mut hasher);
            (p.duration_s as u64).hash(&mut hasher);
            p.paused.hash(&mut hasher);
        }
        let key = hasher.finish();
        if self.surface_id.is_some() && self.last_key == Some(key) {
            return Ok(None);
        }

        let w = l.content_w.max(1);
        let h = l.content_h.max(1);
        self.ensure_targets(sink, w as u32, h as u32)?;
        let idx = self.next_target;
        self.next_target = (idx + 1) % self.targets.len();
        let texture = self.targets[idx].texture;
        let pixmap_id = self.targets[idx].pixmap_id;

        let gl = sink.gl();
        unsafe {
            let fbo = self.ensure_fbo(gl)?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                return Err(anyhow!("overlay framebuffer incomplete"));
            }
            gl.viewport(0, 0, w, h);
            gl.disable(glow::DEPTH_TEST);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            self.set_uniform_2f(gl, "u_viewport", w as f32, h as f32);

            if let (Some((sx, sy)), Some(layer)) = (l.sub, self.subtitle.as_ref()) {
                self.draw_texture(gl, layer.tex, sx, sy, layer.w as f64, layer.h as f64);
            }

            if let Some(p) = &playback {
                self.draw_playback(gl, &l, p);
                if let (Some((tx, ty)), Some((layer, _))) = (l.title, self.title.as_ref()) {
                    self.draw_texture(gl, layer.tex, tx, ty, layer.w as f64, layer.h as f64);
                }
            }

            gl.bind_vertex_array(None);
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                None,
                0,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.use_program(None);
            gl.disable(glow::BLEND);
            gl.finish();
        }

        if self.surface_id.is_none() {
            self.surface_id = Some(self.create_surface()?);
        }
        let surface_id = self.surface_id.unwrap();
        unsafe {
            fl_discard_reply(
                self.client,
                fl_set_surface_pixmap(self.client, surface_id, pixmap_id).value,
            );
            fl_discard_reply(
                self.client,
                fl_set_surface_position(self.client, surface_id, l.surface_x, l.surface_y).value,
            );
        }
        self.last_key = Some(key);
        Ok(Some(surface_id))
    }

    pub fn clear(&mut self, sink: &FhsPixmapSink) {
        for rt in self.targets.drain(..) {
            sink.destroy_render_target(rt);
        }
        self.target_size = (0, 0);
        self.next_target = 0;
        if let Some(surface_id) = self.surface_id.take() {
            unsafe {
                fl_discard_reply(self.client, fl_destroy_surface(self.client, surface_id).value);
            }
        }
        self.last_key = None;
    }

    fn ensure_targets(&mut self, sink: &FhsPixmapSink, w: u32, h: u32) -> Result<()> {
        if self.target_size == (w, h) && self.targets.len() == TARGET_POOL {
            return Ok(());
        }
        for rt in self.targets.drain(..) {
            sink.destroy_render_target(rt);
        }
        self.target_size = (0, 0);
        self.next_target = 0;
        while self.targets.len() < TARGET_POOL {
            self.targets.push(sink.create_render_target(w, h)?);
        }
        self.target_size = (w, h);
        Ok(())
    }

    unsafe fn ensure_fbo(&mut self, gl: &glow::Context) -> Result<glow::Framebuffer> {
        unsafe {
            if let Some(fbo) = self.fbo {
                return Ok(fbo);
            }
            let fbo = gl
                .create_framebuffer()
                .map_err(|e| anyhow!("create_framebuffer: {e}"))?;
            self.fbo = Some(fbo);
            Ok(fbo)
        }
    }

    fn create_surface(&self) -> Result<fl_protocol_SurfaceId> {
        unsafe {
            let mut reply: fl_reply_CreateSurface = std::mem::zeroed();
            if !fl_receive_reply_create_surface(
                self.client,
                fl_create_surface(self.client, self.window_id, 1, false),
                &mut reply,
            ) {
                return Err(anyhow!("Failed to create overlay surface"));
            }
            Ok(reply.surface_id)
        }
    }

    unsafe fn draw_playback(&self, gl: &glow::Context, l: &Layout, p: &Playback) {
        unsafe {
            self.rounded_rect(gl, l.bar_x0, l.bar_y0, l.bar_x1, l.bar_y1, l.bar_radius, PANEL);

            let frac = if p.duration_s > 0.0 {
                (p.elapsed_s / p.duration_s).clamp(0.0, 1.0)
            } else {
                0.0
            };
            if frac > 0.0 {
                let min_w = (l.bar_y1 - l.bar_y0).max(1.0);
                let progress_w = ((l.bar_x1 - l.bar_x0) * frac).max(min_w);
                self.rounded_rect(
                    gl,
                    l.bar_x0,
                    l.bar_y0,
                    l.bar_x0 + progress_w,
                    l.bar_y1,
                    l.bar_radius,
                    ACCENT,
                );
            }

            self.rounded_rect(
                gl,
                l.button_cx - l.button_r,
                l.button_cy - l.button_r,
                l.button_cx + l.button_r,
                l.button_cy + l.button_r,
                l.button_r,
                PANEL,
            );
            self.draw_icon(gl, l, p.paused);

            self.draw_label(gl, &format_time(p.elapsed_s), l.label_font, l.bar_x0, l.label_y, false);
            self.draw_label(gl, &format_time(p.duration_s), l.label_font, l.bar_x1, l.label_y, true);
        }
    }

    unsafe fn rounded_rect(
        &self,
        gl: &glow::Context,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        radius: f64,
        color: (f32, f32, f32, f32),
    ) {
        let hx = ((x1 - x0) / 2.0) as f32;
        let hy = ((y1 - y0) / 2.0) as f32;
        let r = (radius as f32).min(hx).min(hy).max(0.0);
        let (x0, y0, x1, y1) = (x0 as f32, y0 as f32, x1 as f32, y1 as f32);
        let verts: [f32; 24] = [
            x0, y0, -hx, -hy, 0.0, 0.0, //
            x1, y0, hx, -hy, 1.0, 0.0, //
            x0, y1, -hx, hy, 0.0, 1.0, //
            x1, y1, hx, hy, 1.0, 1.0, //
        ];
        unsafe {
            self.set_uniform_4f(gl, "u_color", color.0, color.1, color.2, color.3);
            self.set_uniform_2f(gl, "u_halfsize", hx, hy);
            self.set_uniform_1f(gl, "u_radius", r);
            self.set_flags(gl, 1, 0, 0, 0);
            self.upload_and_draw(gl, &verts, glow::TRIANGLE_STRIP, 4);
        }
    }

    unsafe fn draw_icon(&self, gl: &glow::Context, l: &Layout, paused: bool) {
        let cx = l.button_cx;
        let cy = l.button_cy;
        let r = l.button_r;
        if paused {
            let tip = cx + r * 0.42;
            let left = cx - r * 0.3;
            let top = cy - r * 0.45;
            let bot = cy + r * 0.45;
            let m = 2.0;
            let (x0, y0, x1, y1) = (
                (left - m) as f32,
                (top - m) as f32,
                (tip + m) as f32,
                (bot + m) as f32,
            );
            let verts: [f32; 24] = [
                x0, y0, x0, y0, 0.0, 0.0, //
                x1, y0, x1, y0, 1.0, 0.0, //
                x0, y1, x0, y1, 0.0, 1.0, //
                x1, y1, x1, y1, 1.0, 1.0, //
            ];
            unsafe {
                self.set_uniform_4f(gl, "u_color", ICON.0, ICON.1, ICON.2, ICON.3);
                self.set_uniform_2f(gl, "u_tri0", left as f32, top as f32);
                self.set_uniform_2f(gl, "u_tri1", left as f32, bot as f32);
                self.set_uniform_2f(gl, "u_tri2", tip as f32, cy as f32);
                self.set_flags(gl, 0, 0, 0, 1);
                self.upload_and_draw(gl, &verts, glow::TRIANGLE_STRIP, 4);
            }
        } else {
            let bar_w = r * 0.24;
            let bar_h = r * 0.9;
            let gap = r * 0.22;
            for sign in [-1.0f64, 1.0] {
                let bx = cx + sign * (gap + bar_w / 2.0);
                unsafe {
                    self.rounded_rect(
                        gl,
                        bx - bar_w / 2.0,
                        cy - bar_h / 2.0,
                        bx + bar_w / 2.0,
                        cy + bar_h / 2.0,
                        bar_w * 0.35,
                        ICON,
                    );
                }
            }
        }
    }

    unsafe fn draw_label(
        &self,
        gl: &glow::Context,
        text: &str,
        font_px: f64,
        anchor_x: f64,
        y: f64,
        right_align: bool,
    ) {
        let Ok((rgba, tw, th)) = rasterize_label(text, font_px) else {
            return;
        };
        if tw == 0 || th == 0 {
            return;
        }
        let x0 = if right_align { anchor_x - tw as f64 } else { anchor_x };
        let (x0, y0, x1, y1) = (x0 as f32, y as f32, (x0 + tw as f64) as f32, (y + th as f64) as f32);
        let verts: [f32; 24] = [
            x0, y0, 0.0, 0.0, 0.0, 0.0, //
            x1, y0, 0.0, 0.0, 1.0, 0.0, //
            x0, y1, 0.0, 0.0, 0.0, 1.0, //
            x1, y1, 0.0, 0.0, 1.0, 1.0, //
        ];
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.scratch_tex));
            tex_params(gl);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                tw,
                th,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&rgba)),
            );
            self.set_flags(gl, 0, 0, 1, 0);
            self.set_uniform_1i(gl, "u_tex", 0);
            self.upload_and_draw(gl, &verts, glow::TRIANGLE_STRIP, 4);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    unsafe fn draw_texture(
        &self,
        gl: &glow::Context,
        tex: glow::Texture,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) {
        let (x0, y0, x1, y1) = (x as f32, y as f32, (x + w) as f32, (y + h) as f32);
        let verts: [f32; 24] = [
            x0, y0, 0.0, 0.0, 0.0, 0.0, //
            x1, y0, 0.0, 0.0, 1.0, 0.0, //
            x0, y1, 0.0, 0.0, 0.0, 1.0, //
            x1, y1, 0.0, 0.0, 1.0, 1.0, //
        ];
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.set_flags(gl, 0, 0, 1, 0);
            self.set_uniform_1i(gl, "u_tex", 0);
            self.upload_and_draw(gl, &verts, glow::TRIANGLE_STRIP, 4);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    unsafe fn set_flags(&self, gl: &glow::Context, sdf: i32, tex: i32, tex_rgba: i32, tri: i32) {
        unsafe {
            self.set_uniform_1i(gl, "u_use_sdf", sdf);
            self.set_uniform_1i(gl, "u_use_tex", tex);
            self.set_uniform_1i(gl, "u_use_tex_rgba", tex_rgba);
            self.set_uniform_1i(gl, "u_use_triangle", tri);
        }
    }

    unsafe fn upload_and_draw(&self, gl: &glow::Context, verts: &[f32], mode: u32, count: i32) {
        unsafe {
            let bytes = core::slice::from_raw_parts(
                verts.as_ptr() as *const u8,
                verts.len() * core::mem::size_of::<f32>(),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::DYNAMIC_DRAW);
            gl.draw_arrays(mode, 0, count);
        }
    }

    unsafe fn set_uniform_2f(&self, gl: &glow::Context, name: &str, a: f32, b: f32) {
        unsafe {
            let loc = gl.get_uniform_location(self.program, name);
            gl.uniform_2_f32(loc.as_ref(), a, b);
        }
    }

    unsafe fn set_uniform_4f(&self, gl: &glow::Context, name: &str, a: f32, b: f32, c: f32, d: f32) {
        unsafe {
            let loc = gl.get_uniform_location(self.program, name);
            gl.uniform_4_f32(loc.as_ref(), a, b, c, d);
        }
    }

    unsafe fn set_uniform_1f(&self, gl: &glow::Context, name: &str, a: f32) {
        unsafe {
            let loc = gl.get_uniform_location(self.program, name);
            gl.uniform_1_f32(loc.as_ref(), a);
        }
    }

    unsafe fn set_uniform_1i(&self, gl: &glow::Context, name: &str, a: i32) {
        unsafe {
            let loc = gl.get_uniform_location(self.program, name);
            gl.uniform_1_i32(loc.as_ref(), a);
        }
    }
}

unsafe fn tex_params(gl: &glow::Context) {
    unsafe {
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    }
}

unsafe fn upload_texture(gl: &glow::Context, rgba: &[u8], w: i32, h: i32) -> Result<glow::Texture> {
    unsafe {
        let tex = gl.create_texture().map_err(|e| anyhow!("create_texture: {e}"))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        tex_params(gl);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(rgba)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(tex)
    }
}

unsafe fn upload_texture_into(gl: &glow::Context, tex: glow::Texture, rgba: &[u8], w: i32, h: i32) {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(rgba)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
}

unsafe fn compile_program(gl: &glow::Context) -> Result<glow::Program> {
    unsafe {
        let program = gl.create_program().map_err(|e| anyhow!("create_program: {e}"))?;
        for (ty, src) in [
            (glow::VERTEX_SHADER, VERT_SRC),
            (glow::FRAGMENT_SHADER, FRAG_SRC),
        ] {
            let shader = gl.create_shader(ty).map_err(|e| anyhow!("create_shader: {e}"))?;
            gl.shader_source(shader, src);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(anyhow!("shader compile failed: {}", gl.get_shader_info_log(shader)));
            }
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(anyhow!("program link failed: {}", gl.get_program_info_log(program)));
        }
        Ok(program)
    }
}

fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn rasterize_label(text: &str, font_px: f64) -> Result<(Vec<u8>, i32, i32)> {
    use cairo::{Context, Format, ImageSurface};

    let outline = (font_px * 0.07).max(1.5);
    let pad = outline.ceil() as i32 + 1;

    let mut font = pango::FontDescription::new();
    font.set_family("sans-serif");
    font.set_weight(pango::Weight::Bold);
    font.set_absolute_size(font_px.max(1.0) * pango::SCALE as f64);

    let measure = ImageSurface::create(Format::ARgb32, 1, 1)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    let measure_cr = Context::new(&measure).map_err(|e| anyhow!("cairo context failed: {e}"))?;
    let layout = pangocairo::functions::create_layout(&measure_cr);
    layout.set_font_description(Some(&font));
    layout.set_text(text);
    let (tw, th) = layout.pixel_size();

    let width = (tw + pad * 2).max(1);
    let height = (th + pad * 2).max(1);

    let mut surface = ImageSurface::create(Format::ARgb32, width, height)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    {
        let cr = Context::new(&surface).map_err(|e| anyhow!("cairo context failed: {e}"))?;
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&font));
        layout.set_text(text);
        outline_text(&cr, &layout, pad as f64, outline)?;
    }
    surface.flush();

    Ok((swizzle(&mut surface, width, height)?, width, height))
}

// White glyphs with a black outline: stroke the glyph path (centered on the
// edge, so line width = 2x the outline) in black, then fill white on top.
fn outline_text(cr: &cairo::Context, layout: &pango::Layout, pad: f64, outline: f64) -> Result<()> {
    cr.move_to(pad, pad);
    pangocairo::functions::layout_path(cr, layout);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_line_width(outline * 2.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);
    cr.stroke_preserve().map_err(|e| anyhow!("cairo stroke failed: {e}"))?;
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.fill().map_err(|e| anyhow!("cairo fill failed: {e}"))?;
    Ok(())
}

fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn rasterize_title_row(
    title: &str,
    artist: &str,
    album: &str,
    window_size: (u32, u32),
) -> Result<(Vec<u8>, i32, i32)> {
    use cairo::{Context, Format, ImageSurface};

    let (window_width, window_height) = window_size;
    let base = window_height as f64;
    let font_px = (base * 0.026).max(12.0);
    let outline = (font_px * 0.07).max(1.5);
    let pad = outline.ceil() as i32 + 2;
    let max_width = (window_width as f64 * 0.9) as i32;

    let sep = "   \u{2022}   ";
    let mut parts: Vec<String> = Vec::new();
    if !title.is_empty() {
        parts.push(escape_markup(title));
    }
    if !artist.is_empty() {
        parts.push(escape_markup(artist));
    }
    if !album.is_empty() {
        parts.push(escape_markup(album));
    }
    let markup = parts.join(sep);

    let mut font = pango::FontDescription::new();
    font.set_family("sans-serif");
    font.set_weight(pango::Weight::Bold);
    font.set_absolute_size(font_px * pango::SCALE as f64);

    let measure = ImageSurface::create(Format::ARgb32, 1, 1)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    let measure_cr = Context::new(&measure).map_err(|e| anyhow!("cairo context failed: {e}"))?;
    let layout = pangocairo::functions::create_layout(&measure_cr);
    layout.set_font_description(Some(&font));
    layout.set_width(max_width.max(1) * pango::SCALE);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_markup(&markup);
    let (text_w, text_h) = layout.pixel_size();

    let width = (text_w + pad * 2).max(1);
    let height = (text_h + pad * 2).max(1);

    let mut surface = ImageSurface::create(Format::ARgb32, width, height)
        .map_err(|e| anyhow!("cairo surface create failed: {e}"))?;
    {
        let cr = Context::new(&surface).map_err(|e| anyhow!("cairo context failed: {e}"))?;
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&font));
        layout.set_width(max_width.max(1) * pango::SCALE);
        layout.set_ellipsize(pango::EllipsizeMode::End);
        layout.set_markup(&markup);

        outline_text(&cr, &layout, pad as f64, outline)?;
    }
    surface.flush();

    Ok((swizzle(&mut surface, width, height)?, width, height))
}

fn swizzle(surface: &mut cairo::ImageSurface, width: i32, height: i32) -> Result<Vec<u8>> {
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
    Ok(rgba)
}
