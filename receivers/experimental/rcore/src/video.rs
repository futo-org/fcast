use anyhow::Result;
// #[cfg(any(target_os = "macos", target_os = "windows"))]
// use gst_gl::GLVideoFrameExt;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::sync::{
    Arc,
    atomic::{self, AtomicBool, AtomicU32, Ordering},
};

use gst::prelude::*;
use gst_video::prelude::*;
use tracing::{debug, error};

use crate::fcasttextoverlay::meta_imp::TextFormat;

pub enum Resource<T> {
    Eos,
    Cleared,
    Unchanged,
    New(T),
}

#[cfg_attr(target_os = "linux", allow(clippy::large_enum_variant))]
pub enum RawFrame {
    SystemMemory {
        frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    },
    #[cfg(target_os = "linux")]
    DmaBuf {
        buffer: gst::Buffer,
        info: gst_video::VideoInfo,
        dma_info: gst_video::VideoInfoDmaDrm,
    },
    #[cfg(target_os = "macos")]
    Gl {
        buffer: gst::Buffer,
        info: gst_video::VideoInfo,
    },
}

impl RawFrame {
    pub fn width(&self) -> u32 {
        match self {
            RawFrame::SystemMemory { frame } => frame.width(),
            #[cfg(target_os = "linux")]
            RawFrame::DmaBuf { info, .. } => info.width(),
            #[cfg(target_os = "macos")]
            RawFrame::Gl { info, .. } => info.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            RawFrame::SystemMemory { frame } => frame.height(),
            #[cfg(target_os = "linux")]
            RawFrame::DmaBuf { info, .. } => info.height(),
            #[cfg(target_os = "macos")]
            RawFrame::Gl { info, .. } => info.height(),
        }
    }
}

pub type Overlays = Arc<Mutex<Option<Option<SmallVec<[Overlay; 3]>>>>>;
pub type Subtitles = Arc<Mutex<Option<Option<SmallVec<[String; 3]>>>>>;
pub type Frame = RawFrame;

#[derive(Debug)]
pub struct Overlay {
    pub pix_buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    pub x: i32,
    pub y: i32,
}

pub struct SlintOpenGLSink {
    pub appsink: gst_app::AppSink,
    next_frame: Arc<Mutex<Option<Frame>>>,
    next_overlays: Overlays,
    next_subtitles: Subtitles,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub gst_gl_context: Option<gst_gl::GLContext>,
    pub is_eos: Arc<AtomicBool>,
    pub window_width: Arc<AtomicU32>,
    pub window_height: Arc<AtomicU32>,
}

impl SlintOpenGLSink {
    pub fn new() -> Result<Self> {
        let appsink = gst_app::AppSink::builder()
            .caps(&Self::get_caps())
            .enable_last_sample(false)
            .max_buffers(1u32)
            .property("emit-signals", true)
            .build();

        Ok(Self {
            appsink,
            next_frame: Default::default(),
            next_overlays: Default::default(),
            next_subtitles: Default::default(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            gst_gl_context: None,
            is_eos: Arc::new(AtomicBool::new(false)),
            window_width: Arc::new(AtomicU32::new(0)),
            window_height: Arc::new(AtomicU32::new(0)),
        })
    }

    fn get_caps() -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            let formats = [
                gst_video::VideoFormat::Nv12,
                gst_video::VideoFormat::I420,
                gst_video::VideoFormat::P01010le,
                gst_video::VideoFormat::P012Le,
                gst_video::VideoFormat::I42010le,
                gst_video::VideoFormat::I42012le,
                gst_video::VideoFormat::I42212le,
                gst_video::VideoFormat::Y444,
                gst_video::VideoFormat::Y44410le,
                gst_video::VideoFormat::Y44412le,
            ];
            for features in [
                gst::CapsFeatures::new_empty(),
                gst::CapsFeatures::new([
                    "memory:SystemMemory",
                    gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
                ]),
                #[cfg(target_os = "linux")]
                gst::CapsFeatures::new([gst_allocators::CAPS_FEATURE_MEMORY_DMABUF]),
                #[cfg(target_os = "linux")]
                gst::CapsFeatures::new([
                    gst_allocators::CAPS_FEATURE_MEMORY_DMABUF,
                    gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
                ]),
                // #[cfg(target_os = "macos")]
                // gst::CapsFeatures::new([gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY]),
                // #[cfg(target_os = "macos")]
                // gst::CapsFeatures::new([
                //     gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
                //     gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
                // ]),
            ] {
                let mut these_caps = gst_video::VideoCapsBuilder::new()
                    .features(features.iter())
                    // .pixel_aspect_ratio(gst::Fraction::new(1, 1))
                    .width_range(1..i32::MAX)
                    .height_range(1..i32::MAX);
                #[cfg(target_os = "linux")]
                if features.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF) {
                    these_caps = these_caps.format(gst_video::VideoFormat::DmaDrm);
                } else {
                    these_caps = these_caps.format_list(formats);
                }

                #[cfg(target_os = "macos")]
                if features.contains(gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY) {
                    these_caps = these_caps.format_list([
                        gst_video::VideoFormat::Nv12,
                        gst_video::VideoFormat::Ayuv64,
                        gst_video::VideoFormat::P01010le,
                    ]);
                    // these_caps = these_caps.field("texture-target", "rectangle");
                } else {
                    these_caps = these_caps.format_list(formats);
                }

                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    these_caps = these_caps.format_list(formats);
                }

                caps.append(these_caps.build());
            }
        }

        caps
    }

    #[cfg(target_os = "linux")]
    fn add_drm_formats_to_caps(
        caps: &mut gst::Caps,
        formats: &std::collections::HashSet<drm_fourcc::DrmFormat>,
    ) {
        let formats = formats
            .iter()
            .map(|fmt| gst_video::dma_drm_fourcc_to_string(fmt.code as u32, fmt.modifier.into()))
            .collect::<Vec<_>>();
        let caps = caps.make_mut();
        for (s, feats) in caps.iter_with_features_mut() {
            if feats.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF) {
                s.set("drm-format", gst::List::new(&formats));
            }
        }
    }

    pub fn video_sink(&self) -> gst::Element {
        self.appsink.clone().upcast()
    }

    fn handle_new_sample<F>(
        sample: gst::Sample,
        next_frame_ref: &Arc<Mutex<Option<Frame>>>,
        next_overlays_ref: &Overlays,
        next_subtitles_ref: &Subtitles,
        next_frame_available_notifier: &Arc<F>,
        is_eos: &Arc<AtomicBool>,
        #[cfg(target_os = "linux")] dma_info: &Arc<Mutex<Option<gst_video::VideoInfoDmaDrm>>>,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        F: Fn() + Send + Sync + 'static,
    {
        is_eos.store(false, atomic::Ordering::Relaxed);

        #[allow(unused_mut)]
        let mut buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;

        #[cfg(target_os = "macos")]
        let mut is_gl = false;

        #[cfg(target_os = "macos")]
        if let Some(context) = (buffer.n_memory() > 0)
            .then(|| buffer.peek_memory(0))
            .and_then(|m| m.downcast_memory_ref::<gst_gl::GLBaseMemory>())
            .map(|m| m.context())
        {
            let context = context.clone();
            if let Some(meta) = buffer.meta::<gst_gl::GLSyncMeta>() {
                meta.set_sync_point(&context);
            } else {
                let buffer = buffer.make_mut();
                let meta = gst_gl::GLSyncMeta::add(buffer, &context);
                meta.set_sync_point(&context);
            }
            is_gl = true;
        }

        let Some(info) = sample
            .caps()
            .and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
        else {
            error!("Got invalid caps");
            return Err(gst::FlowError::NotNegotiated);
        };

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

        #[cfg(target_os = "linux")]
        {
            let dma_info = dma_info.lock();
            if let Some(dma_info) = dma_info.as_ref() {
                *next_frame_ref.lock() = Some(RawFrame::DmaBuf {
                    buffer,
                    info,
                    dma_info: dma_info.clone(),
                });

                next_frame_available_notifier();

                return Ok(gst::FlowSuccess::Ok);
            }
        }

        #[cfg(target_os = "macos")]
        if is_gl {
            *next_frame_ref.lock() = Some(RawFrame::Gl {
                buffer,
                info,
            });
            return Ok(gst::FlowSuccess::Ok);
        }

        match gst_video::VideoFrame::from_buffer_readable(buffer, &info) {
            Ok(frame) => {
                *next_frame_ref.lock() = Some(RawFrame::SystemMemory { frame });
            }
            Err(err) => {
                error!(?err, "Failed to create video frame");
            }
        }

        next_frame_available_notifier();

        Ok(gst::FlowSuccess::Ok)
    }

    #[cfg(target_os = "linux")]
    pub fn set_drm_formats(
        &mut self,
        drm_formats: &std::collections::HashSet<drm_fourcc::DrmFormat>,
    ) {
        let mut caps = Self::get_caps();
        #[cfg(target_os = "linux")]
        Self::add_drm_formats_to_caps(&mut caps, drm_formats);
        self.appsink.set_caps(Some(&caps));
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
        #[cfg(target_os = "linux")]
        let dma_info = Arc::new(Mutex::new(None::<gst_video::VideoInfoDmaDrm>));
        // TODO: create an element instead of using an appsink which has more boilerplate at this point
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
                .new_event({
                    #[cfg(target_os = "linux")]
                    let dma_info = Arc::clone(&dma_info);
                    #[allow(unused_variables)]
                    move |appsink| {
                        #[cfg(target_os = "linux")]
                        {
                            let obj = appsink.pull_object().unwrap();
                            if let Some(event) = obj.downcast_ref::<gst::Event>()
                                && let gst::EventView::Caps(event) = event.view()
                            {
                                *dma_info.lock() =
                                    gst_video::VideoInfoDmaDrm::from_caps(event.caps()).ok();
                            }
                        }

                        false
                    }
                })
                .new_preroll({
                    let next_frame_ref = Arc::clone(&next_frame_ref);
                    let next_overlays_ref = Arc::clone(&self.next_overlays);
                    let next_subtitles_ref = Arc::clone(&self.next_subtitles);
                    let next_frame_available_notifier = Arc::clone(&next_frame_available_notifier);
                    let is_eos = Arc::clone(&is_eos_ref);
                    #[cfg(target_os = "linux")]
                    let dma_info = Arc::clone(&dma_info);
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
                                #[cfg(target_os = "linux")]
                                &dma_info,
                            )
                        } else {
                            Ok(gst::FlowSuccess::Ok)
                        }
                    }
                })
                .new_sample({
                    let is_eos = Arc::clone(&is_eos_ref);
                    #[cfg(target_os = "linux")]
                    let dma_info = Arc::clone(&dma_info);
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
                            #[cfg(target_os = "linux")]
                            &dma_info,
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

    pub fn fetch_next_frame(&self) -> Resource<Frame> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return Resource::Eos;
        }

        if let Some(frame) = self.next_frame.lock().take() {
            #[cfg(target_os = "macos")]
            if let RawFrame::Gl { buffer, .. } = &frame {
                let sync_meta = buffer.meta::<gst_gl::GLSyncMeta>().unwrap();
                sync_meta.wait(self.gst_gl_context.as_ref().unwrap());
            }

            Resource::New(frame)
        } else {
            Resource::Unchanged
        }
    }

    pub fn fetch_next_overlays(&self) -> Resource<SmallVec<[Overlay; 3]>> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return Resource::Eos;
        }

        match self.next_overlays.lock().as_mut() {
            Some(overlays) => {
                match overlays.take() {
                    Some(o) => Resource::New(o),
                    None => Resource::Unchanged,
                }
            }
            None => Resource::Cleared,
        }
    }

    pub fn fetch_next_subtitles(&self) -> Resource<SmallVec<[String; 3]>> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return Resource::Eos;
        }

        match self.next_subtitles.lock().as_mut() {
            Some(subs) => {
                match subs.take() {
                    Some(s) => Resource::New(s),
                    None => Resource::Unchanged,
                }
            }
            None => Resource::Cleared,
        }
    }

    pub fn release_state(&mut self) {
        debug!("Releasing state");
        self.next_frame.lock().take();
        self.next_overlays.lock().take();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.gst_gl_context.take();
    }
}
