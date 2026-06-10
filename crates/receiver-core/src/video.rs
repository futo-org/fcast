use gst::glib;
use gst_video::prelude::*;
use smallvec::SmallVec;

pub enum Resource<T> {
    Eos,
    Cleared,
    Unchanged,
    New(T),
}

#[cfg_attr(target_os = "linux", allow(clippy::large_enum_variant))]
pub enum FrameData {
    SystemMemory {
        frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    },
    #[cfg(target_os = "linux")]
    DmaBuf {
        buffer: gst::Buffer,
        dma_info: gst_video::VideoInfoDmaDrm,
    },
    #[cfg(target_os = "macos")]
    Gl {
        buffer: gst::Buffer,
        info: gst_video::VideoInfo,
    },
}

impl FrameData {
    pub fn width(&self) -> u32 {
        match self {
            Self::SystemMemory { frame } => frame.width(),
            #[cfg(target_os = "linux")]
            Self::DmaBuf { dma_info, .. } => dma_info.width(),
            #[cfg(target_os = "macos")]
            Self::Gl { info, .. } => info.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::SystemMemory { frame } => frame.height(),
            #[cfg(target_os = "linux")]
            Self::DmaBuf { dma_info, .. } => dma_info.height(),
            #[cfg(target_os = "macos")]
            Self::Gl { info, .. } => info.height(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Coordinate {
    pub x: f32,
    pub y: f32,
}

impl From<gst_video::VideoMasteringDisplayInfoCoordinate> for Coordinate {
    fn from(value: gst_video::VideoMasteringDisplayInfoCoordinate) -> Self {
        Self {
            x: value.x(),
            y: value.y(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct MasteringDisplayInfo {
    /// The xy coordinates of primaries in the CIE 1931 color space.
    ///
    /// The index 0 contains red, 1 is for green and 2 is for blue.
    pub display_primaries: [Coordinate; 3],
    /// The xy coordinates of white point in the CIE 1931 color space.
    pub white_point: Coordinate,
    /// The maximum value of display luminance in unit of 0.0001 nit
    pub max_display_mastering_luminance: u32,
    /// The minimum value of display luminance in unit of 0.0001 nit
    pub min_display_mastering_luminance: u32,
}

#[derive(Debug, Copy, Clone)]
pub struct ContentLightLevel {
    /// The maximum content light level in nits
    pub max_content_light_level: u16,
    /// The maximum frame average light level in nits
    pub max_frame_average_light_level: u16,
}

pub struct Frame {
    pub data: FrameData,
    pub mastering_display_info: Option<MasteringDisplayInfo>,
    pub content_light_level: Option<ContentLightLevel>,
    pub subtitles: Resource<SmallVec<[String; 3]>>,
    pub overlays: Resource<SmallVec<[Overlay; 3]>>,
}

#[derive(Debug)]
pub struct Overlay {
    pub pix_buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    pub x: i32,
    pub y: i32,
    pub render_width: u32,
    pub render_height: u32,
}

pub mod imp {
    use std::sync::{
        Arc, LazyLock,
        atomic::{self, AtomicBool},
    };

    use crate::fcasttextoverlay::meta_imp::TextFormat;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_base::{prelude::*, subclass::prelude::*};
    use gst_video::{prelude::*, subclass::prelude::*};
    use parking_lot::Mutex;
    use smallvec::SmallVec;

    use crate::video::Overlay;

    use super::Resource;

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
            ] {
                let mut these_caps = gst_video::VideoCapsBuilder::new()
                    .features(features.iter())
                    .width_range(1..i32::MAX)
                    .height_range(1..i32::MAX);
                #[cfg(target_os = "linux")]
                if features.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF) {
                    these_caps = these_caps.format(gst_video::VideoFormat::DmaDrm);
                } else {
                    these_caps = these_caps.format_list(formats);
                }

                #[cfg(not(any(target_os = "linux")))]
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

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fcastvideosink",
            gst::DebugColorFlags::empty(),
            Some("FCast video sink"),
        )
    });

    enum VideoInfo {
        DmaDrm(gst_video::VideoInfoDmaDrm),
        Normal(gst_video::VideoInfo),
    }

    #[derive(Clone, Default, Debug, glib::Boxed)]
    #[boxed_type(name = "FCastDrmFormats")]
    pub struct DrmFormats(pub Arc<std::collections::HashSet<drm_fourcc::DrmFormat>>);

    #[derive(Clone, Debug, glib::Boxed)]
    #[boxed_type(name = "FCastWindowResolution")]
    pub struct WindowResolution {
        pub width: u32,
        pub height: u32,
    }

    #[derive(Clone, Debug, Default, glib::Boxed)]
    #[boxed_type(name = "FCastAtomicBoolBox")]
    pub struct AtomicBoolBox(pub Arc<AtomicBool>);

    /// When the inner option is `None` the sink EOS.
    #[derive(Clone, Default, glib::Boxed)]
    #[boxed_type(name = "FCastVideoPayloadHandle")]
    pub struct VideoPayloadHandle(pub Arc<Mutex<Option<Option<super::Frame>>>>);

    #[derive(Default)]
    struct Config {
        video_info: Option<VideoInfo>,
        mastering_display_info: Option<super::MasteringDisplayInfo>,
        content_light_level: Option<super::ContentLightLevel>,
        has_overlay: bool,
    }

    #[derive(Default)]
    pub struct FSink {
        config: Mutex<Config>,
        cached_caps: Mutex<Option<gst::Caps>>,
        window_resolution: Mutex<Option<WindowResolution>>,
        window_resized: AtomicBool,
        is_eos: AtomicBoolBox,
        payload_handle: VideoPayloadHandle,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FSink {
        const NAME: &'static str = "FCastVideoSink";
        type Type = super::FSink;
        type ParentType = gst_video::VideoSink;
    }

    impl ObjectImpl for FSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    #[cfg(target_os = "linux")]
                    glib::ParamSpecBoxed::builder::<DrmFormats>("drm-formats")
                        .nick("DRM formats")
                        .write_only()
                        .build(),
                    glib::ParamSpecBoxed::builder::<WindowResolution>("window-resolution")
                        .nick("Window resolution")
                        .write_only()
                        .build(),
                    glib::ParamSpecBoxed::builder::<AtomicBoolBox>("is-eos")
                        .nick("Is end of stream")
                        .read_only()
                        .build(),
                    glib::ParamSpecBoxed::builder::<VideoPayloadHandle>("payload-handle")
                        .nick("Payload handle")
                        .read_only()
                        .build(),
                ]
            });

            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "is-eos" => self.is_eos.to_value(),
                "payload-handle" => self.payload_handle.to_value(),
                _ => unreachable!(),
            }
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                #[cfg(target_os = "linux")]
                "drm-formats" => {
                    let formats: DrmFormats = value.get().expect("type checked upstream");
                    let mut caps = get_caps();
                    add_drm_formats_to_caps(&mut caps, &formats.0);
                    *self.cached_caps.lock() = Some(caps);
                }
                "window-resolution" => {
                    let resolution: WindowResolution = value.get().expect("type checked upstream");
                    *self.window_resolution.lock() = Some(resolution);
                    self.window_resized.store(true, atomic::Ordering::SeqCst);
                }
                _ => unreachable!(),
            }
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> =
                LazyLock::new(|| vec![glib::subclass::Signal::builder("frame-available").build()]);

            SIGNALS.as_ref()
        }
    }

    impl GstObjectImpl for FSink {}

    impl ElementImpl for FSink {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &get_caps(),
                    )
                    .unwrap(),
                ]
            });

            PAD_TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            match transition {
                gst::StateChange::PausedToReady => {
                    let mut config = self.config.lock();
                    config.video_info.take();
                    config.mastering_display_info.take();
                    config.content_light_level.take();
                    config.has_overlay = false;
                    self.is_eos.0.store(true, atomic::Ordering::Relaxed);
                    self.payload_handle.0.lock().replace(None);
                    self.obj().emit_by_name::<()>("frame-available", &[]);
                }
                _ => (),
            }

            self.parent_change_state(transition)
        }
    }

    impl BaseSinkImpl for FSink {
        fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
            let cached_caps = self.cached_caps.lock().clone();
            let mut tmp_caps = cached_caps.unwrap_or_else(|| {
                let templ = Self::pad_templates();
                templ[0].caps().clone()
            });

            gst::debug!(CAT, imp = self, "Advertising our own caps: {tmp_caps:?}");

            if let Some(filter_caps) = filter {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Intersecting with filter caps: {filter_caps:?}",
                );

                tmp_caps =
                    filter_caps.intersect_with_mode(&tmp_caps, gst::CapsIntersectMode::First);
            };

            gst::debug!(CAT, imp = self, "Returning caps: {tmp_caps:?}");
            Some(tmp_caps)
        }

        fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
            let mut config = self.config.lock();

            #[cfg(target_os = "linux")]
            {
                config.video_info = gst_video::VideoInfoDmaDrm::from_caps(caps)
                    .map(VideoInfo::DmaDrm)
                    .ok();
            }

            if config.video_info.is_none() {
                config.video_info = Some(
                    gst_video::VideoInfo::from_caps(caps)
                        .map(VideoInfo::Normal)
                        .map_err(|_| gst::loggable_error!(CAT, "Invalid caps"))?,
                );
            }

            config.mastering_display_info = gst_video::VideoMasteringDisplayInfo::from_caps(caps)
                .map(|mdi| super::MasteringDisplayInfo {
                    display_primaries: mdi.display_primaries().map(super::Coordinate::from),
                    white_point: mdi.white_point().into(),
                    max_display_mastering_luminance: mdi.max_display_mastering_luminance(),
                    min_display_mastering_luminance: mdi.min_display_mastering_luminance(),
                })
                .ok();

            config.content_light_level = gst_video::VideoContentLightLevel::from_caps(caps)
                .map(|cll| super::ContentLightLevel {
                    max_content_light_level: cll.max_content_light_level(),
                    max_frame_average_light_level: cll.max_frame_average_light_level(),
                })
                .ok();

            config.has_overlay = caps
                .features(0)
                .unwrap()
                .contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);

            Ok(())
        }

        fn propose_allocation(
            &self,
            query: &mut gst::query::Allocation,
        ) -> Result<(), gst::LoggableError> {
            query.add_allocation_meta::<gst_video::VideoMeta>(None);

            let overlay_meta = if let Some(win) = self.window_resolution.lock().as_ref() {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Setting window width and height for overlay meta {win:?}"
                );
                Some(
                    gst::Structure::builder("GstVideoOverlayCompositionMeta")
                        .field("width", win.width)
                        .field("height", win.height)
                        .build(),
                )
            } else {
                None
            };

            query.add_allocation_meta::<gst_video::VideoOverlayCompositionMeta>(
                overlay_meta.as_deref(),
            );

            Ok(())
        }

        // TODO: handle rotation?
        //       https://github.com/GStreamer/gst-plugins-rs/blob/e5ff6f0a272e179d4472acf037273367c6e8511b/video/gtk4/src/sink/imp.rs#L759
        // fn event(&self, event: gst::Event) -> bool {
        //     match event.view() {
        //         gst::EventView::StreamStart(_) => {}
        //         _ => (),
        //     }

        //     self.parent_event(event)
        // }
    }

    impl VideoSinkImpl for FSink {
        fn show_frame(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let reconfigure = self.window_resized.swap(false, atomic::Ordering::SeqCst)
                && self.config.lock().has_overlay;
            if reconfigure {
                gst::info!(CAT, imp = self, "Window size changed, needs to reconfigure");
                let obj = self.obj();
                obj.sink_pad()
                    .push_event(gst::event::Reconfigure::builder().build());
            }

            if buffer.n_memory() == 0 {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Empty buffer, nothing to render. Returning."
                );
                return Ok(gst::FlowSuccess::Ok);
            };

            self.is_eos.0.store(false, atomic::Ordering::Relaxed);
            let buffer = buffer.clone();

            let overlays: SmallVec<[Overlay; 3]> = buffer
                .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
                .flat_map(|meta| {
                    meta.overlay()
                        .iter()
                        .filter_map(|rect| {
                            let buffer = rect.pixels_unscaled_argb(
                                gst_video::VideoOverlayFormatFlags::GLOBAL_ALPHA,
                            );
                            let (x, y, render_width, render_height) = rect.render_rectangle();

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

                            Some(Overlay {
                                pix_buffer,
                                x,
                                y,
                                render_width,
                                render_height,
                            })
                        })
                        .collect::<SmallVec<[_; 3]>>()
                })
                .collect();
            let overlays = if !overlays.is_empty() {
                Resource::New(overlays)
            } else {
                Resource::Cleared
            };

            let mut subtitles = Resource::Cleared;
            if let Some(meta) = buffer.meta::<crate::fcasttextoverlay::FCastVideoTextOverlayMeta>()
            {
                let (format, text) = meta.get();

                fn split_subs(subs: &str) -> SmallVec<[String; 3]> {
                    subs.lines().map(String::from).collect()
                }

                match format {
                    TextFormat::Utf8 => subtitles = Resource::New(split_subs(text)),
                    TextFormat::PangoMarkup => match pango::parse_markup(text, '\0') {
                        Ok((_, text, _)) => subtitles = Resource::New(split_subs(&text)),
                        Err(err) => gst::error!(
                            CAT,
                            imp = self,
                            "Failed to parse subtitles as pango markup err={err:?}"
                        ),
                    },
                }
            }

            let config = self.config.lock();
            let mdi = config.mastering_display_info;
            let cll = config.content_light_level;
            if let Some(video_info) = config.video_info.as_ref() {
                let data = match video_info {
                    VideoInfo::DmaDrm(dma_info) => super::FrameData::DmaBuf {
                        buffer,
                        dma_info: dma_info.clone(),
                    },
                    VideoInfo::Normal(info) => {
                        match gst_video::VideoFrame::from_buffer_readable(buffer, &info) {
                            Ok(frame) => super::FrameData::SystemMemory { frame },
                            Err(err) => {
                                gst::error!(
                                    CAT,
                                    imp = self,
                                    "Failed to create video frame: {err:?}"
                                );
                                return Err(gst::FlowError::Flushing);
                            }
                        }
                    }
                };

                let frame = super::Frame {
                    data,
                    mastering_display_info: mdi,
                    content_light_level: cll,
                    subtitles,
                    overlays,
                };

                self.payload_handle.0.lock().replace(Some(frame));
                self.obj().emit_by_name::<()>("frame-available", &[]);
            }

            Ok(gst::FlowSuccess::Ok)
        }
    }
}

glib::wrapper! {
    pub struct FSink(ObjectSubclass<imp::FSink>)
        @extends gst_video::VideoSink, gst_base::BaseSink, gst::Element, gst::Object;
}

impl FSink {
    pub fn new() -> Self {
        gst::Object::builder().build().unwrap()
    }
}
