use anyhow::{Context, Result};
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::{
    num::NonZero,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
};

use gst::prelude::*;
// use gst_gl::prelude::*;
use gst_video::prelude::*;
use tracing::{debug, error};

pub type Overlays = Arc<Mutex<Option<Option<SmallVec<[Overlay; 3]>>>>>;

// Taken partially from the slint gstreamer example at: https://github.com/slint-ui/slint/blob/2edd97bf8b8dc4dc26b578df6b15ea3297447444/examples/gstreamer-player/egl_integration.rs
pub struct SlintOpenGLSink {
    appsink: gst_app::AppSink,
    // appsink: gst::Element,
    // gl_elems: GlElements,
    // sinkbin: gst::Bin,
    next_frame: Arc<Mutex<Option<(gst_video::VideoInfo, gst::Buffer)>>>,
    next_overlays: Overlays,
    // current_frame: Mutex<Option<gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>>>,
    current_frame: Mutex<
        Option<(
            gst_video::VideoInfo,
            gst_video::VideoFrame<gst_video::video_frame::Readable>,
            // gst_video::VideoFrame<gst_video::video_frame::Writable>,
            // gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>,
        )>,
    >,
    // gst_gl_context: Option<gst_gl::GLContext>,
    pub is_eos: Arc<AtomicBool>,
}

// #[cfg(target_os = "linux")]
// fn is_on_wayland() -> Result<bool> {
//     if std::env::var("WAYLAND_DISPLAY").is_ok() {
//         Ok(true)
//     } else if std::env::var("DISPLAY").is_ok() {
//         Ok(false)
//     } else {
//         anyhow::bail!("Unsupported platform")
//     }
// }

#[derive(Debug)]
pub struct Overlay {
    pub pix_buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    pub x: i32,
    pub y: i32,
}

pub enum FrameData {
    Nv12 { y: NonZero<u32>, uv: NonZero<u32> },
    P01010le { y: NonZero<u32>, uv: NonZero<u32> },
    Gst {
        frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
        // frame: gst_video::VideoFrame<gst_video::video_frame::Writable>,
        info: gst_video::VideoInfo,
    },
}

// TODO: color
pub struct Frame {
    pub external: bool,
    pub width: u32,
    pub height: u32,
    pub color_range: gst_video::VideoColorRange,
    pub color_matrix: gst_video::VideoColorMatrix,
    pub transfer_function: gst_video::VideoTransferFunction,
    pub data: FrameData,
    // TODO: external OES
}

impl SlintOpenGLSink {
    pub fn new() -> Result<Self> {
        // let mut caps = gst::Caps::new_empty();
        // // let caps = {
        // {
        //     let caps = caps.get_mut().unwrap();
        //     for features in [
        //         gst::CapsFeatures::new([gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY]),
        //         gst::CapsFeatures::new([
        //             gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
        //             gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
        //         ]),
        //     ] {
        //         let these_caps = gst_video::VideoCapsBuilder::new()
        //             .features(features.iter())
        //             // .format(gst_video::VideoFormat::Nv12)
        //             // .format_list([gst_video::VideoFormat::Nv12, gst_video::VideoFormat::P01010le])
        //             // .format(gst_video::VideoFormat::P01010le)
        //             // .format(gst_video::VideoFormat::Rgba)
        //             // TODO: can we use OES
        //             // .field("texture-target", gst::List::new(["2D", "external-oes"]))
        //             // .field("texture-target", "2D")
        //             .width_range(1..i32::MAX)
        //             .height_range(1..i32::MAX)
        //             .build();
        //         caps.append(these_caps);
        //     }

        //     // gst_video::VideoCapsBuilder::new()
        //     //     .any_features()
        //     // // .features(
        //     // //     gst::CapsFeatures::new([
        //     // //         gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
        //     // //         gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY,
        //     // //         // gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
        //     // //     ]).iter()
        //     // // )
        //     //     .width_range(1..i32::MAX)
        //     //     .height_range(1..i32::MAX)
        //     //     .build()
        // }

        let caps = gst_video::VideoCapsBuilder::new()
            // .format(gst_video::VideoFormat::Nv12)
            // .format(gst_video::VideoFormat::I420)
            .format_list([gst_video::VideoFormat::Nv12, gst_video::VideoFormat::P01010le, gst_video::VideoFormat::I420])
            // .width_range(1..i32::MAX)
            // .height_range(1..i32::MAX)
            .build();

        // TODO: try dmabuf import
        // let mut caps = gst::Caps::new_empty();
        // {
        //     let caps = caps.get_mut().unwrap();

        //     let features = [
        //         Some([gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION]),
        //         None,
        //     ];

        //     let formats = [
        //         gst_video::VideoFormat::Nv12,
        //         gst_video::VideoFormat::Rgba,
        //         // TODO: P010_10LE
        //     ];

        //     // [gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY]),
        //     // gst::CapsFeatures::new([
        //     //     gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
        //     //     gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
        //     // ]),

        //     for feature_set in features {
        //         let these_caps = gst_video::VideoCapsBuilder::new()
        //             .format_list(formats)
        //             .width_range(1..i32::MAX)
        //             .height_range(1..i32::MAX);
        //         let these_caps = if let Some(features) = feature_set {
        //             these_caps.features(features.iter().copied()).build()
        //         } else {
        //             these_caps.build()
        //         };
        //         // .features(features.iter().copied())
        //         // // .format(gst_video::VideoFormat::Nv12)
        //         // .format_list([gst_video::VideoFormat::Nv12, gst_video::VideoFormat::P01010le])
        //         // // .format(gst_video::VideoFormat::P01010le)
        //         // // .format(gst_video::VideoFormat::Rgba)
        //         // // TODO: can we use OES?
        //         // .field("texture-target", gst::List::new(["2D", "external-oes"]))
        //         // // .field("texture-target", "2D")
        //         // .width_range(1..i32::MAX)
        //         // .height_range(1..i32::MAX)
        //         // .build();
        //         caps.append(these_caps);
        //     }
        // }

        // let sink_capsfilter = gst::ElementFactory::make("capsfilter").property("caps", caps).build()?;

        let appsink = gst_app::AppSink::builder()
            .caps(&caps)
            .enable_last_sample(false)
            .max_buffers(1u32)
            // .property("emit-signals", true)
            .build();

        // TODO: this shouldn't be required
        // let bin = gst::Bin::new();
        // let ghost = gst::GhostPad::new(gst::PadDirection::Sink);
        // bin.add_pad(&ghost)?;

        // let appsink = gst::ElementFactory::make("glimagesink").build()?;

        // bin.add(&appsink)?;
        // bin.add(&sink_capsfilter)?;

        // let glupload = gst::ElementFactory::make("glupload").build()?;
        // let glconvert = gst::ElementFactory::make("glcolorconvert").build()?;

        // let capsfilter = gst::ElementFactory::make("capsfilter")
        //     .property(
        //         "caps",
        //         {
        //         }

        //         // gst_video::VideoCapsBuilder::new()
        //         //     .features(
        //         //         gst::CapsFeatures::new([
        //         //             gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY,
        //         //             gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
        //         //         ]).iter()
        //         //     )
        //         //     // .format(gst_video::VideoFormat::Nv12)
        //         //     .format_list([gst_video::VideoFormat::Nv12, gst_video::VideoFormat::P01010le])
        //         //     // .format(gst_video::VideoFormat::P01010le)
        //         //     // .format(gst_video::VideoFormat::Rgba)
        //         //     // TODO: can we use OES?
        //         //     // .field("texture-target", gst::List::new(["2D", "external-oes"]))
        //         //     .field("texture-target", "2D")
        //         //     .width_range(1..i32::MAX)
        //         //     .height_range(1..i32::MAX)
        //         //     .build(),
        //     )
        //     .build()?;

        // bin.add_many([&glupload, &glconvert]).unwrap();
        // bin.add_many([&glupload, &capsfilter, &glconvert]).unwrap();
        // bin.add_many([&glupload, &capsfilter]).unwrap();
        // bin.add_many([&glupload]).unwrap();
        // ghost
        //     .set_target(Some(&glupload.static_pad("sink").unwrap()))
        //     .unwrap();
        // ghost
        //     .set_target(Some(&appsink.static_pad("sink").unwrap()))
        //     .unwrap();
        // gst::Element::link_many([&glupload, &glconvert, appsink.upcast_ref()]).unwrap();
        // gst::Element::link_many([&glupload, &capsfilter, &glconvert, appsink.upcast_ref()]).unwrap();
        // gst::Element::link_many([&glupload, &capsfilter, appsink.upcast_ref()]).unwrap();
        // gst::Element::link_many([&glupload, &capsfilter, &sink_capsfilter, appsink.upcast_ref()]).unwrap();
        // gst::Element::link_many([&glupload, appsink.upcast_ref()]).unwrap();

        Ok(Self {
            appsink,
            next_frame: Default::default(),
            current_frame: Default::default(),
            next_overlays: Default::default(),
            // gst_gl_context: None,
            is_eos: Arc::new(AtomicBool::new(false)),
            // sinkbin: bin,
        })
    }

    pub fn video_sink(&self) -> gst::Element {
        // self.sinkbin.clone().upcast()
        self.appsink.clone().upcast()
    }

    // #[cfg(any(target_os = "linux", target_os = "android"))]
    // fn get_egl_ctx(
    //     graphics_api: &slint::GraphicsAPI<'_>,
    // ) -> Result<(gst_gl::GLContext, gst_gl::GLDisplay)> {
    //     debug!("Creating EGL context");

    //     let egl = match graphics_api {
    //         slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
    //             glutin_egl_sys::egl::Egl::load_with(|symbol| {
    //                 get_proc_address(&std::ffi::CString::new(symbol).unwrap())
    //             })
    //         }
    //         _ => anyhow::bail!("Unsupported graphics API"),
    //     };

    //     let platform = gst_gl::GLPlatform::EGL;

    //     unsafe {
    //         let egl_display = egl.GetCurrentDisplay();
    //         let display = gst_gl_egl::GLDisplayEGL::with_egl_display(egl_display as usize)?;
    //         let native_context = egl.GetCurrentContext();

    //         Ok((
    //             gst_gl::GLContext::new_wrapped(
    //                 &display,
    //                 native_context as _,
    //                 platform,
    //                 gst_gl::GLContext::current_gl_api(platform).0,
    //             )
    //             .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))?,
    //             display.upcast(),
    //         ))
    //     }
    // }

    // #[cfg(target_os = "linux")]
    // fn get_glx_ctx(
    //     graphics_api: &slint::GraphicsAPI<'_>,
    // ) -> Result<(gst_gl::GLContext, gst_gl::GLDisplay)> {
    //     debug!("Creating GLX context");

    //     let glx = match graphics_api {
    //         slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
    //             glutin_glx_sys::glx::Glx::load_with(|symbol| {
    //                 get_proc_address(&std::ffi::CString::new(symbol).unwrap())
    //             })
    //         }
    //         _ => anyhow::bail!("Unsupported graphics API"),
    //     };

    //     let platform = gst_gl::GLPlatform::GLX;

    //     unsafe {
    //         let glx_display = glx.GetCurrentDisplay();
    //         let display = gst_gl_x11::GLDisplayX11::with_display(glx_display as usize)?;
    //         let native_context = glx.GetCurrentContext();

    //         Ok((
    //             gst_gl::GLContext::new_wrapped(
    //                 &display,
    //                 native_context as _,
    //                 platform,
    //                 gst_gl::GLContext::current_gl_api(platform).0,
    //             )
    //             .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))?,
    //             display.upcast(),
    //         ))
    //     }
    // }

    // #[cfg(target_os = "windows")]
    // fn get_wgl_ctx() -> Result<(gst_gl::GLContext, gst_gl::GLDisplay)> {
    //     use anyhow::bail;

    //     debug!("Creating WGL context");

    //     let platform = gst_gl::GLPlatform::WGL;
    //     let gl_api = gst_gl::GLAPI::OPENGL3;
    //     let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

    //     if gl_ctx == 0 {
    //         bail!("Failed to create GL context");
    //     }

    //     let Some(gst_display) = gst_gl::GLDisplay::with_type(gst_gl::GLDisplayType::WIN32) else {
    //         bail!("Failed to create GLDisplay of type WIN32");
    //     };

    //     gst_display.filter_gl_api(gl_api);

    //     unsafe {
    //         Ok((
    //             gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api)
    //                 .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))?,
    //             gst_display,
    //         ))
    //     }
    // }

    // #[cfg(target_os = "macos")]
    // fn get_macos_gl_ctx() -> Result<(gst_gl::GLContext, gst_gl::GLDisplay)> {
    //     use anyhow::bail;

    //     debug!("Creating CGL context");

    //     let platform = gst_gl::GLPlatform::CGL;
    //     let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
    //     let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

    //     if gl_ctx == 0 {
    //         bail!("Failed to get handle from CGL");
    //     }

    //     let gst_display = gst_gl::GLDisplay::new();
    //     unsafe {
    //         let wrapped_context =
    //             gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

    //         let wrapped_context = match wrapped_context {
    //             None => {
    //                 // gst::error!(CAT, imp = self, "Failed to create wrapped GL context");
    //                 bail!("");
    //             }
    //             Some(wrapped_context) => wrapped_context,
    //         };

    //         Ok((wrapped_context, gst_display))
    //     }
    // }

    fn handle_new_sample<F>(
        sample: gst::Sample,
        next_frame_ref: &Arc<Mutex<Option<(gst_video::VideoInfo, gst::Buffer)>>>,
        next_overlays_ref: &Overlays,
        next_frame_available_notifier: &Arc<F>,
        is_eos: &Arc<AtomicBool>,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        F: Fn() + Send + Sync + 'static,
    {
        // TODO: can this be done just on a new preroll sample?
        is_eos.store(false, atomic::Ordering::Relaxed);

        // {
        //     if let Some(buffer_list) = sample.buffer_list() {
        //         let len = buffer_list.len();
        //         tracing::debug!(number_of_buffers = len);
        //     }
        // }

        let mut buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;

        // return Ok(gst::FlowSuccess::Ok);

        // tracing::debug!(number_of_memory= buffer.n_memory());
        // match (buffer.n_memory() > 0)
        //     .then(|| buffer.peek_memory(0))
        //     .and_then(|m| m.downcast_memory_ref::<gst_gl::GLBaseMemory>())
        //     .map(|m| m.context())
        // {
        //     Some(_context) => {
        //         // Sync point to ensure that the rendering in this context will be complete by the time the
        //         // Slint created GL context needs to access the texture.
        //         // if let Some(meta) = buffer.meta::<gst_gl::GLSyncMeta>() {
        //         //     debug!("Buffer has sync meta");
        //         //     meta.set_sync_point(context);
        //         // } else {
        //         //     tracing::warn!("Buffer has no sync meta");
        //         //     let buffer = buffer.make_mut();
        //         //     let meta = gst_gl::GLSyncMeta::add(buffer, &context);
        //         //     meta.set_sync_point(context);
        //         // }
        //         todo!();
        //     }
        //     None => {
        //         // error!("Got non-GL memory");
        //         // return Err(gst::FlowError::Error);
        //         // return Ok(gst::FlowSuccess::Ok);
        //     }
        // }

        let Some(info) = sample
            .caps()
            .and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
        else {
            error!("Got invalid caps");
            return Err(gst::FlowError::NotNegotiated);
        };

        // tracing::debug!(video_caps = ?info);

        // https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/blob/main/video/gtk4/src/sink/frame.rs?ref_type=heads
        // let overlays: SmallVec<[Overlay; 3]> = buffer
        //     .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
        //     .flat_map(|meta| {
        //         meta.overlay()
        //             .iter()
        //             .filter_map(|rect| {
        //                 let buffer = rect
        //                     .pixels_unscaled_argb(gst_video::VideoOverlayFormatFlags::GLOBAL_ALPHA);
        //                 let (x, y, _width, _height) = rect.render_rectangle();

        //                 let vmeta = buffer.meta::<gst_video::VideoMeta>().unwrap();

        //                 if vmeta.format() != gst_video::VideoFormat::Bgra {
        //                     return None;
        //                 }

        //                 let info = gst_video::VideoInfo::builder(
        //                     vmeta.format(),
        //                     vmeta.width(),
        //                     vmeta.height(),
        //                 )
        //                 .build()
        //                 .unwrap();

        //                 let frame =
        //                     gst_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;

        //                 let Ok(plane) = frame.plane_data(0) else {
        //                     return None;
        //                 };

        //                 let mut pix_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
        //                     frame.width(),
        //                     frame.height(),
        //                 );
        //                 image_swizzle::bgra_to_rgba(plane, pix_buffer.make_mut_bytes());

        //                 Some(Overlay { pix_buffer, x, y })
        //             })
        //             .collect::<SmallVec<[_; 3]>>()
        //     })
        //     .collect();

        // if !overlays.is_empty() {
        //     *next_overlays_ref.lock() = Some(Some(overlays));
        // } else {
        //     *next_overlays_ref.lock() = None;
        // }

        *next_frame_ref.lock() = Some((info, buffer));

        next_frame_available_notifier();

        Ok(gst::FlowSuccess::Ok)
    }

    pub fn connect<F>(
        &mut self,
        graphics_api: &slint::GraphicsAPI<'_>,
        next_frame_available_notifier: F,
        // contexts: &Arc<std::sync::Mutex<Option<(gst_gl::GLDisplay, gst_gl::GLContext)>>>,
    ) -> Result<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        debug!("Creating connection between UI and sink");

        // if let Some(old_gl_context) = self.gst_gl_context.take() {
        //     match old_gl_context.activate(false) {
        //         Ok(_) => debug!("Deactivated old GL context"),
        //         Err(err) => error!(?err, "Failed to deactivate old GL context"),
        //     }
        // }

        // #[cfg(target_os = "linux")]
        // let (gst_gl_context, gst_gl_display) = {
        //     match is_on_wayland() {
        //         // NOTE: If error: assume KMS
        //         Ok(true) | Err(_) => Self::get_egl_ctx(graphics_api)?,
        //         Ok(false) => Self::get_glx_ctx(graphics_api)?,
        //     }
        // };
        // #[cfg(target_os = "android")]
        // let (gst_gl_context, gst_gl_display) = Self::get_egl_ctx(graphics_api)?;
        // #[cfg(target_os = "windows")]
        // let (gst_gl_context, gst_gl_display) = Self::get_wgl_ctx()?;
        // #[cfg(target_os = "macos")]
        // let (gst_gl_context, gst_gl_display) = Self::get_macos_gl_ctx()?;

        // gst_gl_context
        //     .activate(true)
        //     .context("could not activate GStreamer GL context")?;
        // gst_gl_context
        //     .fill_info()
        //     .context("failed to fill GL info for wrapped context")?;

        // *contexts.lock().unwrap() = Some((gst_gl_display.clone(), gst_gl_context.clone()));

        // self.gst_gl_context = Some(gst_gl_context);

        let next_frame_ref = Arc::clone(&self.next_frame);
        let next_frame_available_notifier = Arc::new(next_frame_available_notifier);
        let is_eos_ref = Arc::clone(&self.is_eos);
        let next_overlays_ref = Arc::clone(&self.next_overlays);

        self.appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                // .propose_allocation(|_, allocation| {
                //     allocation.add_allocation_meta::<gst_video::VideoMeta>(None);
                //     debug!("##################### Propose allocation #######################");
                //     true
                // })
                // .new_event(|appsink| {
                //     if let Ok(obj) = appsink.pull_object() {
                //         if let Some(event) = obj.downcast_ref::<gst::Event>() {
                //             debug!(?event, "New event");
                //         }
                //     }
                //     false
                // })
                .new_preroll({
                    let next_frame_ref = Arc::clone(&next_frame_ref);
                    let next_overlays_ref = Arc::clone(&self.next_overlays);
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
                        // tracing::debug!("New sample available");
                        let sample = appsink
                            .pull_sample()
                            .map_err(|_| gst::FlowError::Flushing)?;
                        // tracing::debug!(video_caps = ?sample.caps());
                        Self::handle_new_sample(
                            sample,
                            &next_frame_ref,
                            &next_overlays_ref,
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

    /// Returns (texture id, [width, height])
    // pub fn fetch_next_frame_as_texture(&self) -> Option<(NonZero<u32>, [u32; 2])> {
    pub fn fetch_next_frame_as_texture(&self) -> Option<Option<Frame>> {
        if self.is_eos.load(atomic::Ordering::Relaxed) {
            return None;
        }

        if let Some((info, buffer)) = self.next_frame.lock().take() {
            let frame = gst_video::VideoFrame::from_buffer_readable(buffer, &info).unwrap();
            // let frame = gst_video::VideoFrame::from_buffer_writable(buffer, &info).unwrap();
            // let sync_meta = buffer.meta::<gst_gl::GLSyncMeta>().unwrap();
            // sync_meta.wait(self.gst_gl_context.as_ref().unwrap());

            *self.current_frame.lock() = Some((info, frame));

            // if let Ok(frame) = gst_gl::GLVideoFrame::from_buffer_readable(buffer, &info) {
            //     *self.current_frame.lock() = Some((info, frame));
            // } else {
            //     return None;
            // }
            // return None;
        }

        Some(self.current_frame
            .lock()
            .take()
            // .as_ref()
            .and_then(|(info, frame)| {
                // tracing::debug!(n_planes = frame.n_planes());
                // let external = match frame.texture_target(0).unwrap() {
                //     gst_gl::GLTextureTarget::_2d => false,
                //     gst_gl::GLTextureTarget::ExternalOes => true,
                //     _ => todo!(),
                // };
                // tracing::debug!(external, video_format = ?info.format(), gl_format_of_buffer = ?frame.format());
                // // tracing::debug!(gl_format = ?frame.format_info());
                // // frame.colorimetry();
                let external = false;

                let (width, height) = (frame.width(), frame.height());
                // let data = match frame.format() {
                //     gst_video::VideoFormat::Nv12 => {
                //         // FrameData::Nv12 {
                //         //     y: frame.texture_id(0).unwrap().try_into().unwrap(),
                //         //     uv: frame.texture_id(1).unwrap().try_into().unwrap(),
                //         // }
                //         todo!()
                //     }
                //     gst_video::VideoFormat::P01010le => {
                //         // FrameData::P01010le {
                //         //     y: frame.texture_id(0).unwrap().try_into().unwrap(),
                //         //     uv: frame.texture_id(1).unwrap().try_into().unwrap(),
                //         // }
                //         todo!()
                //     }
                //     _ => return None,
                // };

                let colorimetry = info.colorimetry();
                let data = FrameData::Gst {
                    frame,
                    info,
                };

                Some(Frame {
                    external,
                    width,
                    height,
                    data,
                    color_range: colorimetry.range(),
                    color_matrix: colorimetry.matrix(),
                    transfer_function: colorimetry.transfer(),
                })
            }))

        // self.current_frame
        //     .lock()
        //     .as_ref()
        //     .and_then(|frame| {
        //         frame
        //             .texture_id(0)
        //             .ok()
        //             .and_then(|id| id.try_into().ok())
        //             .map(|texture| (frame, texture))
        //     })
        //     .map(|(frame, texture)| (texture, [frame.width(), frame.height()]))

        // None
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
}
