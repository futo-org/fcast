//! Android's software video path: decoded frames become slint images on the
//! ui thread.
//!
//! Two arms, picked per frame by where the decoder's pixels landed.
//!
//! * The zero-copy arm. The sink offers the decoder a pool of AHardwareBuffers
//!   ([`crate::android_ahb`]); a decoder that takes it writes straight into
//!   memory the GPU can sample, and the frame reaches the scene as a borrowed
//!   GL texture built by [`crate::android_ahb_gl`]. Nothing is converted and
//!   nothing is uploaded.
//! * The bridge arm, for every frame that did not come out of that pool: a
//!   decoder that built its own buffers, a format with no AHardwareBuffer
//!   mapping, a device whose gralloc refused. NV12/NV21/I420 to RGBA on the
//!   streaming thread, one shared pixel buffer per frame.
//!
//! The whole arm is off under `FCAST_ANDROID_AHB=0`, which is the A/B control
//! the device measurement uses.
//!
//! # Where the import runs
//!
//! On the render thread, inside a slint rendering notifier, because that is
//! the only place the renderer's EGL context is current. The streaming thread
//! only parks the frame and asks for a redraw. That also fixes the lifetime:
//! the parked `gst::Buffer` holds the pool slot, and the buffer that is on
//! screen is held in a second slot until the next one replaces it, so the
//! decoder can never overwrite a picture the GPU is still reading.

use std::sync::{
    Mutex,
    atomic::{AtomicU32, Ordering},
};

use gst::prelude::*;
use gst_video::prelude::*;
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};

use crate::video_math::{Chroma, yuv420_to_rgba};

/// The visible rect inside the frame. Caps carry the coded size when a
/// decoder signals its display window through a crop meta, and the padding
/// rows it hides are decoder scratch, not picture.
fn crop_rect(frame: &gst_video::VideoFrameRef<&gst::BufferRef>) -> (usize, usize, usize, usize) {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let Some(crop) = frame.buffer().meta::<gst_video::VideoCropMeta>() else {
        return (0, 0, w, h);
    };
    let (x, y, cw, ch) = crop.rect();
    let (x, y, cw, ch) = (x as usize, y as usize, cw as usize, ch as usize);
    // a crop that does not fit the frame is not usable, show everything
    if cw == 0 || ch == 0 || x + cw > w || y + ch > h {
        return (0, 0, w, h);
    }
    // even origin keeps the chroma pairing
    (x & !1, y & !1, cw, ch)
}

/// The same rect straight off a buffer, for the zero-copy arm, which never
/// maps the frame and so has no `VideoFrameRef` to ask.
fn crop_rect_of(buffer: &gst::BufferRef, w: u32, h: u32) -> (i32, i32, i32, i32) {
    let Some(crop) = buffer.meta::<gst_video::VideoCropMeta>() else {
        return (0, 0, w as i32, h as i32);
    };
    let (x, y, cw, ch) = crop.rect();
    if cw == 0 || ch == 0 || x + cw > w || y + ch > h {
        return (0, 0, w as i32, h as i32);
    }
    ((x & !1) as i32, (y & !1) as i32, cw as i32, ch as i32)
}

fn frame_to_rgba(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let (x, y, w, h) = crop_rect(frame);
    if w == 0 || h == 0 {
        return None;
    }
    let strides = frame.plane_stride();
    let ystride = strides[0] as usize;
    let ydata = frame.plane_data(0).ok()?.get(y * ystride + x..)?;

    let chroma = match frame.format() {
        gst_video::VideoFormat::Nv12 | gst_video::VideoFormat::Nv21 => {
            let stride = strides[1] as usize;
            Chroma::SemiPlanar {
                data: frame.plane_data(1).ok()?.get(y / 2 * stride + x..)?,
                stride,
                swap: frame.format() == gst_video::VideoFormat::Nv21,
            }
        }
        gst_video::VideoFormat::I420 => {
            let (ustride, vstride) = (strides[1] as usize, strides[2] as usize);
            Chroma::Planar {
                u: frame.plane_data(1).ok()?.get(y / 2 * ustride + x / 2..)?,
                ustride,
                v: frame.plane_data(2).ok()?.get(y / 2 * vstride + x / 2..)?,
                vstride,
            }
        }
        _ => return None,
    };

    let mut pix = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
    yuv420_to_rgba(ydata, ystride, chroma, w, h, pix.make_mut_bytes());
    Some(pix)
}

/// One frame waiting for the render thread to import it.
struct Pending {
    /// Holds the pool slot. Dropped when the frame is replaced on screen,
    /// never before, so the decoder cannot write over a picture the GPU is
    /// still sampling.
    buffer: gst::Buffer,
    /// The gralloc allocation's extent, which is what the GL texture covers.
    texture_size: (u32, u32),
    /// The picture inside it, in source pixels.
    clip: (i32, i32, i32, i32),
    external: bool,
}

/// The single-slot mailbox between the streaming thread and the renderer.
/// One slot on purpose: a frame that was never rendered is stale by the time
/// the next one lands, and holding it would only cost the decoder a pool
/// buffer. Replacing rather than queueing is also what keeps the whole lane
/// free of per-frame allocation.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// The frame the scene is currently sampling.
static ON_SCREEN: Mutex<Option<gst::Buffer>> = Mutex::new(None);
/// Bumped per imported frame. A pooled texture id repeats every six frames
/// and its contents change under it, so the scene has to be told the image is
/// not the one it already has.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Installs the render-thread half of the zero-copy arm.
///
/// `BeforeRendering` is the only moment the EGL context is current, so it is
/// where the import, the slint image and the on-screen handover all happen.
/// `RenderingTeardown` is the matching moment for dropping them.
fn attach_renderer(ui: &crate::MainWindow) {
    let weak = ui.as_weak();
    let result = ui
        .window()
        .set_rendering_notifier(move |state, _api| match state {
            slint::RenderingState::BeforeRendering => {
                let Some(pending) = PENDING.lock().ok().and_then(|mut p| p.take()) else {
                    return;
                };
                let Some(ui) = weak.upgrade() else { return };
                present(&ui, pending);
            }
            slint::RenderingState::RenderingTeardown => {
                let _ = PENDING.lock().map(|mut p| p.take());
                let _ = ON_SCREEN.lock().map(|mut p| p.take());
                // still on the render thread with a context, which is the whole
                // reason the cache has no Drop of its own
                unsafe { crate::android_ahb_gl::clear() };
            }
            _ => {}
        });
    if let Err(err) = result {
        tracing::warn!(
            ?err,
            "android ahb lane: no rendering notifier, the bridge carries video"
        );
    }
}

/// Turns a parked frame into the scene's image. Runs on the render thread.
fn present(ui: &crate::MainWindow, pending: Pending) {
    let Some(handle) = crate::android_ahb::hardware_buffer(&pending.buffer) else {
        return;
    };
    let Some(import) = (unsafe { crate::android_ahb_gl::import(handle, pending.external) }) else {
        return;
    };
    let Some(texture_id) = core::num::NonZeroU32::new(import.texture) else {
        return;
    };
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    // Safety: the texture was created by the context that is current here,
    // and the gralloc allocation behind it is held alive both by the import
    // cache's own reference and by the buffer parked below.
    let image = unsafe {
        slint::BorrowedOpenGLTextureBuilder::new_gl_external_oes_texture(
            texture_id,
            [pending.texture_size.0, pending.texture_size.1].into(),
        )
    }
    .generation(generation)
    .build();

    let (x, y, w, h) = pending.clip;
    let bridge = ui.global::<crate::Bridge>();
    bridge.set_sw_video_clip_x(x);
    bridge.set_sw_video_clip_y(y);
    bridge.set_sw_video_clip_width(w);
    bridge.set_sw_video_clip_height(h);
    bridge.set_sw_video_frame(image);
    bridge.set_sw_video_active(true);

    // The old frame goes back to the pool only now, once the scene no longer
    // references it. Dropping it any earlier lets the decoder overwrite what
    // is on screen.
    if let Ok(mut slot) = ON_SCREEN.lock() {
        *slot = Some(pending.buffer);
    }
}

/// Gives up every pooled buffer the lane is holding, without touching the
/// scene. For the item transition in [`crate::gui`], which clears the image
/// itself and would otherwise leave two pool slots parked for the whole of
/// the next item.
pub(crate) fn release_frames() {
    let _ = PENDING.lock().map(|mut p| p.take());
    let _ = ON_SCREEN.lock().map(|mut p| p.take());
}

/// Clears both the scene's frame and everything holding a pool slot.
fn clear_frame(ui: &crate::MainWindow) {
    release_frames();
    let bridge = ui.global::<crate::Bridge>();
    bridge.set_sw_video_frame(slint::Image::default());
    bridge.set_sw_video_active(false);
    bridge.set_sw_video_clip_width(0);
    bridge.set_sw_video_clip_height(0);
}

/// The player's android video sink: a synced appsink whose frames land in the
/// `sw-video-frame` bridge image, zero-copy where the decoder took the pool.
pub fn make_sink(ui: &crate::MainWindow) -> gst::Element {
    attach_renderer(ui);
    let ui = ui.as_weak();
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::Nv12,
                    gst_video::VideoFormat::Nv21,
                    gst_video::VideoFormat::I420,
                ])
                .build(),
        )
        .max_buffers(2)
        .drop(true)
        // The retained last sample is a seventh pool slot the decoder never
        // gets back, and the lane already keeps the frame on screen alive
        // itself. Nothing here ever asks for it.
        .enable_last_sample(false)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // Nothing is proposed on a device whose gralloc has no importable
            // layout, for a format with no AHardwareBuffer mapping, or when
            // the lane is switched off. A decoder is free to ignore what is
            // proposed too. Every one of those leaves the bridge arm carrying
            // the frame exactly as it does today.
            .propose_allocation(|_, query| crate::android_ahb::propose(query))
            .new_sample({
                let ui = ui.clone();
                move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };

                    // The zero-copy arm first: one pointer compare, and if it
                    // says yes nothing here maps or converts anything.
                    if let Some((tw, th, external)) = crate::android_ahb::texture_geometry(buffer) {
                        // publishes the decoder's writes before the GPU reads
                        crate::android_ahb::finish_write(buffer);
                        let s = caps.structure(0);
                        let (w, h) = s
                            .and_then(|s| {
                                Some((s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?))
                            })
                            .unwrap_or((tw as i32, th as i32));
                        let pending = Pending {
                            buffer: buffer.to_owned(),
                            texture_size: (tw, th),
                            clip: crop_rect_of(buffer, w.max(0) as u32, h.max(0) as u32),
                            external,
                        };
                        if let Ok(mut slot) = PENDING.lock() {
                            *slot = Some(pending);
                        }
                        let _ = ui.upgrade_in_event_loop(|ui| ui.window().request_redraw());
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    let Ok(info) = gst_video::VideoInfo::from_caps(caps) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let Ok(frame) =
                        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    if let Some(pix) = frame_to_rgba(&frame) {
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            let bridge = ui.global::<crate::Bridge>();
                            // The conversion already applied the crop, so the
                            // buffer is exactly the picture and the scene
                            // keeps its unclipped element.
                            bridge.set_sw_video_clip_width(0);
                            bridge.set_sw_video_clip_height(0);
                            bridge.set_sw_video_frame(slint::Image::from_rgba8(pix));
                            bridge.set_sw_video_active(true);
                        });
                    }
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .eos(move |_| {
                let _ = ui.upgrade_in_event_loop(|ui| clear_frame(&ui));
            })
            .build(),
    );
    appsink.upcast()
}
