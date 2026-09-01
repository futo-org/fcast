//! Android subtitle overlays. The cue engine rasterizes, the slint layer
//! composites: the window is already a translucent plane over the video
//! surface, so cues cost nothing per video frame, only a small scene
//! repaint when the overlay set changes.
//!
//! The video sink's pad probe stands in for the desktop render sink as the
//! engine's driver: segment, flush and stream-start bookkeeping from the
//! event stream, `overlays_for(running_time)` per buffer for schedule
//! activation/expiry. Every change lands in `on_change`, which repaints on
//! the UI thread through `Bridge.subtitle-overlays`.

use std::sync::{Arc, Mutex};

use fcast_video::{cue::CueEngine, video::OverlaySpace};
use gst::prelude::*;
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tracing::debug;

use crate::{android_surface_video::letterbox, video_math::CueGeometry};

/// One entry per shown overlay: raster identity and placement. Pushes with
/// an unchanged signature are dropped, a texture upload is the expensive
/// part and identical sets need none.
type Signature = Vec<(usize, i32, i32, u32, u32)>;

struct State {
    engine: CueEngine,
    ui: slint::Weak<crate::MainWindow>,
    /// Coded video size from the sink caps, for mapping SrcFrame overlays
    /// (bitmap subtitles) onto the letterboxed picture.
    video_size: Mutex<(u32, u32)>,
    /// Canvas and picture rect as the engine has them, pushed from the UI
    /// thread whenever the window or the video changes shape.
    geometry: CueGeometry,
    last: Mutex<Signature>,
}

/// What the window-geometry callback holds on to, so a resize can re-key the
/// layout without going through the engine's change notification.
#[derive(Clone)]
pub struct Subtitles(Arc<State>);

pub fn attach(engine: CueEngine, sink: &gst::Element, ui: &crate::MainWindow) -> Subtitles {
    let state = Arc::new(State {
        engine: engine.clone(),
        ui: ui.as_weak(),
        video_size: Mutex::new((0, 0)),
        geometry: CueGeometry::new(),
        last: Mutex::new(Vec::new()),
    });

    // The cue engine lays out in the window's PHYSICAL pixels, so its dp floor
    // needs the display's scale factor to mean anything. Without this the
    // floor is read as physical pixels and never binds on a phone, which is
    // what made portrait subtitles render at 9dp.
    {
        let mut style = engine.style();
        style.px_per_dp = ui.window().scale_factor();
        apply_system_captioning(&mut style);
        engine.set_style(style);
    }

    engine.warm();
    {
        let state = state.clone();
        engine.set_on_change(move || push(&state));
    }

    let Some(pad) = sink.static_pad("sink") else {
        return Subtitles(state);
    };
    let handle = Subtitles(state.clone());
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_, info| {
            let engine = &state.engine;
            match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    // Drives activation and expiry, repaints go via on_change.
                    let rt = engine.video_running_time(buffer.pts());
                    let _ = engine.overlays_for(rt);
                }
                Some(gst::PadProbeData::Event(event)) => match event.view() {
                    gst::EventView::Segment(ev) => engine.set_video_segment(ev.segment()),
                    gst::EventView::FlushStop(_) => engine.flush(),
                    gst::EventView::StreamStart(_) => engine.reset_timeline(),
                    gst::EventView::Caps(ev) => {
                        if let Ok(vinfo) = gst_video::VideoInfo::from_caps(ev.caps()) {
                            *state.video_size.lock().unwrap() = (vinfo.width(), vinfo.height());
                            engine.set_video_size(vinfo.width(), vinfo.height());
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );
    handle
}

/// Re-key the layout after a window geometry change. A canvas change marks
/// the engine dirty, so the repaint arrives through on_change.
pub fn resync(subs: &Subtitles, ui: &crate::MainWindow) {
    let size = ui.window().size();
    let picture = *subs.0.video_size.lock().unwrap();
    subs.0
        .geometry
        .sync(&subs.0.engine, (size.width, size.height), picture);
}

fn push(state: &Arc<State>) {
    let state = state.clone();
    let _ = state.ui.clone().upgrade_in_event_loop(move |ui| {
        let size = ui.window().size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let engine = &state.engine;
        // A canvas or rect change re-keys the rasters, the completions loop
        // back here through on_change until the set is laid out against this
        // geometry. The rect is what anchors positioned cues to the picture
        // instead of the window.
        let picture = *state.video_size.lock().unwrap();
        state
            .geometry
            .sync(engine, (size.width, size.height), picture);
        let overlays = engine.current_overlays();

        let sig: Signature = overlays
            .iter()
            .map(|o| {
                (
                    Arc::as_ptr(&o.pixels) as *const u8 as usize,
                    o.x,
                    o.y,
                    o.render_width,
                    o.render_height,
                )
            })
            .collect();
        {
            let mut last = state.last.lock().unwrap();
            if *last == sig {
                return;
            }
            *last = sig;
        }

        let scale = ui.window().scale_factor();
        let (vw, vh) = picture;
        let mut rows = Vec::with_capacity(overlays.len());
        for o in &overlays {
            if o.width == 0 || o.height == 0 {
                continue;
            }
            // Physical window px first, logical for slint at the end.
            let (x, y, w, h) = match o.space {
                OverlaySpace::Window => (
                    o.x as f32,
                    o.y as f32,
                    o.render_width as f32,
                    o.render_height as f32,
                ),
                // Coded-video px, mapped onto the letterboxed picture.
                OverlaySpace::SrcFrame => {
                    if vw == 0 || vh == 0 {
                        continue;
                    }
                    let (rx, ry, rw, rh) = letterbox(vw, vh, size.width, size.height);
                    let sx = rw as f32 / vw as f32;
                    let sy = rh as f32 / vh as f32;
                    (
                        rx as f32 + o.x as f32 * sx,
                        ry as f32 + o.y as f32 * sy,
                        o.render_width as f32 * sx,
                        o.render_height as f32 * sy,
                    )
                }
            };
            let mut pix = SharedPixelBuffer::<Rgba8Pixel>::new(o.width, o.height);
            pix.make_mut_bytes()
                .copy_from_slice(&o.pixels[..(o.width * o.height * 4) as usize]);
            rows.push(crate::SubtitleOverlay {
                img: slint::Image::from_rgba8(pix),
                x: x / scale,
                y: y / scale,
                w: w / scale,
                h: h / scale,
            });
        }
        debug!(count = rows.len(), "subtitle overlays");
        ui.global::<crate::Bridge>()
            .set_subtitle_overlays(slint::ModelRc::new(VecModel::from(rows)));
    });
}

/// Fold the system's accessibility caption preferences into the house
/// style: font scale always, colors and edge only where the user actually
/// chose them (CaptionStyle's has-checks). Read once at attach; a change
/// applies from the next item.
fn apply_system_captioning(style: &mut fcast_video::cue_ir::CueStyle) {
    let ctx = ndk_context::android_context();
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let context = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
    let result = (|| -> jni::errors::Result<()> {
        let name = env.new_string("captioning")?;
        let manager = env
            .call_method(
                &context,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[jni::objects::JValue::Object(&name)],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(());
        }
        let scale = env.call_method(&manager, "getFontScale", "()F", &[])?.f()?;
        if scale.is_finite() && scale > 0.0 && (scale - 1.0).abs() > 0.01 {
            style.font_height_fraction *= scale;
            style.min_font_dp *= scale;
            tracing::info!(scale, "caption font scale applied");
        }
        let user = env
            .call_method(
                &manager,
                "getUserStyle",
                "()Landroid/view/accessibility/CaptioningManager$CaptionStyle;",
                &[],
            )?
            .l()?;
        if user.is_null() {
            return Ok(());
        }
        let argb = |v: i32| -> fcast_video::cue_ir::Rgba {
            [(v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as u8]
        };
        if env.call_method(&user, "hasBackgroundColor", "()Z", &[])?.z()? {
            let color = env.get_field(&user, "backgroundColor", "I")?.i()?;
            if let Some(bg) = style.background.as_mut() {
                bg.color = argb(color);
            }
        }
        if env.call_method(&user, "hasEdgeType", "()Z", &[])?.z()? {
            // CaptionStyle: 0 none, 1 outline, 2 drop shadow
            match env.get_field(&user, "edgeType", "I")?.i()? {
                0 => style.outline = None,
                1 | 2 => {
                    let mut outline =
                        style.outline.unwrap_or(fcast_video::cue_ir::DEFAULT_OUTLINE);
                    if env.call_method(&user, "hasEdgeColor", "()Z", &[])?.z()? {
                        outline.color = argb(env.get_field(&user, "edgeColor", "I")?.i()?);
                    }
                    style.outline = Some(outline);
                }
                _ => {}
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = env.exception_clear();
        tracing::warn!("caption preference read failed, house style stands");
    }
}
