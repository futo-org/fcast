use anyhow::Result;
use gst_gl::GLVideoFrameExt;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::{
    num::NonZero,
    sync::{
        Arc,
        atomic::{self, AtomicBool, AtomicU32, Ordering},
    },
};

use gst::prelude::*;
use gst_video::prelude::*;
use tracing::{debug, error};

use crate::fcasttextoverlay::meta_imp::TextFormat;

pub type Overlays = Arc<Mutex<Option<Option<SmallVec<[Overlay; 3]>>>>>;
pub type Subtitles = Arc<Mutex<Option<Option<SmallVec<[String; 3]>>>>>;
type GlVideoFrame = gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>;

pub struct SlintOpenGLSink {
    pub appsink: gst_app::AppSink,
    sinkbin: gst::Element,
    next_frame: Arc<Mutex<Option<(gst_video::VideoInfo, gst::Buffer)>>>,
    next_overlays: Overlays,
    next_subtitles: Subtitles,
    current_frame: Mutex<Option<(gst_video::VideoInfo, GlVideoFrame)>>,
    old_frame: Mutex<Option<GlVideoFrame>>,
    pub gst_gl_context: Option<gst_gl::GLContext>,
    pub is_eos: Arc<AtomicBool>,
    pub window_width: Arc<AtomicU32>,
    pub window_height: Arc<AtomicU32>,
}

#[derive(Debug)]
pub struct Overlay {
    pub pix_buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    pub x: i32,
    pub y: i32,
}

pub struct Frame {
    pub tex_id: NonZero<u32>,
    pub width: u32,
    pub height: u32,
}

impl SlintOpenGLSink {
    pub fn new() -> Result<Self> {
        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            for features in [
                gst::CapsFeatures::new([gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY]),
                gst::CapsFeatures::new([
                    gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
                    gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
                ]),
                gst::CapsFeatures::new([
                    gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
                    crate::fcasttextoverlay::CAPS_FEATURE_FCAST_TEXT_OVERLAY,
                ]),
            ] {
                let these_caps = gst_video::VideoCapsBuilder::new()
                    .features(features.iter())
                    .format(gst_video::VideoFormat::Rgba)
                    .field("texture-target", "2D")
                    .pixel_aspect_ratio(gst::Fraction::new(1, 1))
                    .width_range(1..i32::MAX)
                    .height_range(1..i32::MAX)
                    .build();
                caps.append(these_caps);
            }
        }

        let appsink = gst_app::AppSink::builder()
            .caps(&caps)
            .enable_last_sample(false)
            .max_buffers(1u32)
            .property("emit-signals", true)
            .build();

        let sinkbin = gst::ElementFactory::make("glsinkbin")
            .property("sink", &appsink)
            .build()?;

        Ok(Self {
            appsink,
            next_frame: Default::default(),
            current_frame: Default::default(),
            old_frame: Default::default(),
            next_overlays: Default::default(),
            next_subtitles: Default::default(),
            gst_gl_context: None,
            is_eos: Arc::new(AtomicBool::new(false)),
            window_width: Arc::new(AtomicU32::new(0)),
            window_height: Arc::new(AtomicU32::new(0)),
            sinkbin,
        })
    }

    pub fn video_sink(&self) -> gst::Element {
        self.sinkbin.clone().upcast()
    }

    fn handle_new_sample<F>(
        sample: gst::Sample,
        next_frame_ref: &Arc<Mutex<Option<(gst_video::VideoInfo, gst::Buffer)>>>,
        next_overlays_ref: &Overlays,
        next_subtitles_ref: &Subtitles,
        next_frame_available_notifier: &Arc<F>,
        is_eos: &Arc<AtomicBool>,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        F: Fn() + Send + Sync + 'static,
    {
        is_eos.store(false, atomic::Ordering::Relaxed);

        let mut buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;

        let context = match (buffer.n_memory() > 0)
            .then(|| buffer.peek_memory(0))
            .and_then(|m| m.downcast_memory_ref::<gst_gl::GLBaseMemory>())
            .map(|m| m.context())
        {
            Some(context) => context.clone(),
            None => {
                error!("Got non-GL memory");
                return Err(gst::FlowError::Error);
            }
        };

        if let Some(meta) = buffer.meta::<gst_gl::GLSyncMeta>() {
            // debug!("Buffer has sync meta");
            meta.set_sync_point(&context);
        } else {
            // tracing::warn!("Buffer has no sync meta");
            let buffer = buffer.make_mut();
            let meta = gst_gl::GLSyncMeta::add(buffer, &context);
            meta.set_sync_point(&context);
        }

        let Some(info) = sample
            .caps()
            .and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
        else {
            error!("Got invalid caps");
            return Err(gst::FlowError::NotNegotiated);
        };

        // tracing::debug!(video_caps = ?info);

        // https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/blob/main/video/gtk4/src/sink/frame.rs?ref_type=heads
        let overlays: SmallVec<[Overlay; 3]> = buffer
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .flat_map(|meta| {
                meta.overlay()
                    .iter()
                    .filter_map(|rect| {
                        let buffer = rect
                            .pixels_unscaled_argb(gst_video::VideoOverlayFormatFlags::GLOBAL_ALPHA);
                        let (x, y, _width, _height) = rect.render_rectangle();

                        let vmeta = buffer.meta::<gst_video::VideoMeta>().unwrap();

                        if vmeta.format() != gst_video::VideoFormat::Bgra {
                            return None;
                        }

                        let info = gst_video::VideoInfo::builder(
                            vmeta.format(),
                            vmeta.width(),
                            vmeta.height(),
                        )
                        .build()
                        .unwrap();

                        let frame =
                            gst_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;

                        let Ok(plane) = frame.plane_data(0) else {
                            return None;
                        };

                        let mut pix_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
                            frame.width(),
                            frame.height(),
                        );
                        image_swizzle::bgra_to_rgba(plane, pix_buffer.make_mut_bytes());

                        Some(Overlay { pix_buffer, x, y })
                    })
                    .collect::<SmallVec<[_; 3]>>()
            })
            .collect();

        if !overlays.is_empty() {
            *next_overlays_ref.lock() = Some(Some(overlays));
        } else {
            *next_overlays_ref.lock() = None;
        }

        if let Some(meta) = buffer.meta::<crate::fcasttextoverlay::FCastVideoTextOverlayMeta>() {
            let (format, text) = meta.get();

            fn split_subs(subs: &str) -> Option<Option<SmallVec<[String; 3]>>> {
                Some(Some(subs.lines().map(String::from).collect()))
            }

            match format {
                TextFormat::Utf8 => *next_subtitles_ref.lock() = split_subs(text),
                TextFormat::PangoMarkup => match pango::parse_markup(text, '\0') {
                    Ok((_, text, _)) => *next_subtitles_ref.lock() = split_subs(&text),
                    Err(err) => error!(?err, "Failed to parse subtitles as pango markup"),
                },
            }
        } else {
            *next_subtitles_ref.lock() = None;
        }

        *next_frame_ref.lock() = Some((info, buffer));

        next_frame_available_notifier();

        Ok(gst::FlowSuccess::Ok)
    }

    pub fn connect<F>(&mut self, next_frame_available_notifier: F) -> Result<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        debug!("Creating connection between UI and sink");

        let next_frame_ref = Arc::clone(&self.next_frame);
        let next_frame_available_notifier = Arc::new(next_frame_available_notifier);
        let is_eos_ref = Arc::clone(&self.is_eos);
        let next_overlays_ref = Arc::clone(&self.next_overlays);
        let next_subtitles_ref = Arc::clone(&self.next_subtitles);
        let window_width = Arc::clone(&self.window_width);
        let window_height = Arc::clone(&self.window_height);
        self.appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .propose_allocation(move |_, allocation| {
                    allocation.add_allocation_meta::<gst_video::VideoMeta>(None);

                    let width = window_width.load(Ordering::Relaxed);
                    let height = window_height.load(Ordering::Relaxed);
                    debug!(
                        width,
                        height, "Setting window width and height for overlay meta"
                    );

                    let overlay_meta = if width > 0 && height > 0 {
                        Some(
                            gst::Structure::builder("GstVideoOverlayCompositionMeta")
                                .field("width", width)
                                .field("height", height)
                                .build(),
                        )
                    } else {
                        None
                    };

                    allocation.add_allocation_meta::<gst_video::VideoOverlayCompositionMeta>(
                        overlay_meta.as_deref(),
                    );

                    true
                })
                .new_preroll({
                    let next_frame_ref = Arc::clone(&next_frame_ref);
                    let next_overlays_ref = Arc::clone(&self.next_overlays);
                    let next_subtitles_ref = Arc::clone(&self.next_subtitles);
                    let next_frame_available_notifier = Arc::clone(&next_frame_available_notifier);
                    let is_eos = Arc::clone(&is_eos_ref);
                    move |appsink| {
                        let sample = appsink
                            .pull_preroll()
                            .map_err(|_| gst::FlowError::Flushing)?;
                        if !is_eos.load(atomic::Ordering::Relaxed) {
                            Self::handle_new_sample(
                                sample,
                                &next_frame_ref,
                                &next_overlays_ref,
                                &next_subtitles_ref,
                                &next_frame_available_notifier,
                                &is_eos,
                            )
                        } else {
                            Ok(gst::FlowSuccess::Ok)
                        }
                    }
                })
                .new_sample({
                    let is_eos = Arc::clone(&is_eos_ref);
                    move |appsink| {
                        let sample = appsink
                            .pull_sample()
                            .map_err(|_| gst::FlowError::Flushing)?;
                        Self::handle_new_sample(
                            sample,
                            &next_frame_ref,
                            &next_overlays_ref,
                            &next_subtitles_ref,
                            &next_frame_available_notifier,
                            &is_eos,
                        )
                    }
                })
                .eos(move |_| {
                    is_eos_ref.store(true, atomic::Ordering::Relaxed);
                })
                .build(),
        );

        Ok(())
    }

    pub fn fetch_next_frame(&self) -> Option<Option<Frame>> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return None;
        }

        if let Some((info, buffer)) = self.next_frame.lock().take() {
            let sync_meta = buffer.meta::<gst_gl::GLSyncMeta>().unwrap();
            sync_meta.wait(self.gst_gl_context.as_ref().unwrap());

            if let Ok(frame) = gst_gl::GLVideoFrame::from_buffer_readable(buffer, &info) {
                *self.current_frame.lock() = Some((info, frame));
            }
        }

        Some(self.current_frame.lock().take().map(|(_info, frame)| {
            let new = Frame {
                tex_id: frame
                    .texture_id(0)
                    .ok()
                    .and_then(|id| id.try_into().ok())
                    .unwrap(),
                width: frame.width(),
                height: frame.height(),
            };
            *self.old_frame.lock() = Some(frame);
            new
        }))
    }

    pub fn fetch_next_overlays(&self) -> Option<Option<SmallVec<[Overlay; 3]>>> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return None;
        }

        self.next_overlays
            .lock()
            .as_mut()
            .map(|overlays| overlays.take())
    }

    pub fn fetch_next_subtitles(&self) -> Option<Option<SmallVec<[String; 3]>>> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return None;
        }

        self.next_subtitles.lock().as_mut().map(|subs| subs.take())
    }

    pub fn release_state(&mut self) {
        debug!("Releasing state");
        self.next_frame.lock().take();
        self.current_frame.lock().take();
        self.old_frame.lock().take();
        self.next_overlays.lock().take();
        self.gst_gl_context.take();
    }
}
