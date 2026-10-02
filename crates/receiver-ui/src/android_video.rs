//! Android's software video path, the fallback when the zero-copy surface
//! (android_surface_video.rs) cannot be set up: NV12/NV21/I420 to RGBA on the
//! streaming thread, one shared pixel buffer per frame, shown as a slint image.

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

fn clear_frame(ui: &crate::MainWindow) {
    let bridge = ui.global::<crate::Bridge>();
    bridge.set_sw_video_frame(slint::Image::default());
    bridge.set_sw_video_active(false);
}

/// The player's android video sink: a synced appsink whose frames land in the
/// attached UI's `sw-video-frame` bridge image, dropped while none is.
pub fn make_sink() -> gst::Element {
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
        // nothing here ever asks for the retained last sample
        .enable_last_sample(false)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample({
                move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };

                    let Ok(info) = gst_video::VideoInfo::from_caps(caps) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let Ok(frame) =
                        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let Some(ui) = crate::android_ui::current() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    if let Some(pix) = frame_to_rgba(&frame) {
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            let bridge = ui.global::<crate::Bridge>();
                            bridge.set_sw_video_frame(slint::Image::from_rgba8(pix));
                            bridge.set_sw_video_active(true);
                        });
                    }
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .eos(move |_| {
                if let Some(ui) = crate::android_ui::current() {
                    let _ = ui.upgrade_in_event_loop(|ui| clear_frame(&ui));
                }
            })
            .build(),
    );
    appsink.upcast()
}
