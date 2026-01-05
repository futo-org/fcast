use crate::{VideoSource, transmission::ExtraVideoContext};
use anyhow::Result;
use gst::prelude::*;
use tracing::{debug, error};

fn scale_res_to_fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let aspect_ratio = (max_width as f32 / width as f32).min(max_height as f32 / height as f32);
    (
        (width as f32 * aspect_ratio) as u32 & !1,
        (height as f32 * aspect_ratio) as u32 & !1,
    )
}

fn make_capture_src(src: VideoSource) -> Result<(gst::Element, Option<ExtraVideoContext>)> {
    Ok(match src {
        VideoSource::TestSrc => (gst::ElementFactory::make("videotestsrc").build()?, None),
        #[cfg(target_os = "linux")]
        VideoSource::PipeWire { node_id, fd } => {
            use std::os::fd::AsRawFd;

            let src = gst::ElementFactory::make("pipewiresrc")
                .property("client-name", "FCast Sender Video Capture")
                .property("fd", fd.as_raw_fd())
                .property("path", node_id.to_string())
                // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/4797
                .property("use-bufferpool", false)
                .build()?;

            let extra = Some(ExtraVideoContext::PipewireVideoSource { _fd: fd });

            (src, extra)
        }
        #[cfg(target_os = "linux")]
        VideoSource::XDisplay {
            id,
            width,
            height,
            x_offset,
            y_offset,
            ..
        } => (
            gst::ElementFactory::make("ximagesrc")
                .property("xid", id as u64)
                .property("startx", x_offset as u32)
                .property("starty", y_offset as u32)
                .property("endx", (x_offset as u32) + (width as u32) - 1)
                .property("endy", (y_offset as u32) + (height as u32) - 1)
                .property("use-damage", false)
                .build()?,
            None,
        ),
        #[cfg(target_os = "macos")]
        VideoSource::CgDisplay { id, .. } => (
            gst::ElementFactory::make("avfvideosrc")
                .property("capture-screen", true)
                .property("capture-screen-cursor", true)
                .property("device-index", id)
                .build()?,
            None,
        ),
        #[cfg(target_os = "windows")]
        VideoSource::D3d11Monitor { handle, .. } => (
            gst::ElementFactory::make("d3d11screencapturesrc")
                .property("show-cursor", true)
                .property("monitor-handle", handle)
                .build()?,
            None,
        ),
    })
}

#[derive(Debug)]
pub struct PreviewElems {
    pub src: gst::Element,
    pub capsfilter: gst::Element,
    pub caps_sink_pad: gst::Pad,
    pub scale_probe: Option<gst::PadProbeId>,
    pub appsink: Option<gst::Element>,
}

pub(crate) fn add_scaling_probe(
    pad: &gst::Pad,
    capsfilter_weak: gst::glib::WeakRef<gst::Element>,
    max_width: u32,
    max_height: u32,
) -> Option<gst::PadProbeId> {
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(event) = info.event() else {
            return gst::PadProbeReturn::Ok;
        };

        use gst::event::EventView;

        if let EventView::Caps(caps) = event.view()
            && let Ok(video_info) = gst_video::VideoInfo::from_caps(caps.caps())
        {
            let width = video_info.width();
            let height = video_info.height();

            if width <= max_width && height <= max_height {
                return gst::PadProbeReturn::Ok;
            }

            let (scaled_width, scaled_height) = if width > height {
                scale_res_to_fit(width, height, max_width, max_height)
            } else {
                scale_res_to_fit(width, height, max_height, max_width)
            };

            debug!(
                width,
                height, scaled_width, scaled_height, "Scaling resolution"
            );

            if let Some(capsfilter) = capsfilter_weak.upgrade() {
                let mut new_caps =
                    match gst_video::VideoInfo::builder_from_info(&video_info).build() {
                        Ok(info) => match info.to_caps() {
                            Ok(caps) => caps,
                            Err(err) => {
                                error!(?err, "Failed to build caps");
                                return gst::PadProbeReturn::Ok;
                            }
                        },
                        Err(err) => {
                            error!(?err, "Failed to build VideoInfo");
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                let new_caps_mut = new_caps.make_mut();
                new_caps_mut.set("width", scaled_width as i32);
                new_caps_mut.set("height", scaled_height as i32);

                capsfilter.set_property("caps", new_caps);
            }
        }

        gst::PadProbeReturn::Ok
    })
}

pub fn add_video_src(
    pipeline: &gst::Pipeline,
    sink: gst::Element,
    src: VideoSource,
    max_width: u32,
    max_height: u32,
    max_framerate: u32,
) -> anyhow::Result<(Option<ExtraVideoContext>, PreviewElems)> {
    let (src, extra) = make_capture_src(src)?;
    let par_filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        )
        .build()?;
    let queue = gst::ElementFactory::make("queue").build()?;
    let rate = gst::ElementFactory::make("videorate")
        .property("drop-only", true)
        .build()?;
    let scale = gst::ElementFactory::make("videoscale")
        .property("add-borders", false)
        .build()?;
    let convert = gst::ElementFactory::make("videoconvert").build()?;
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", gst::Fraction::new(max_framerate as i32, 1))
                .build(),
        )
        .build()?;

    let capsfilter_weak = capsfilter.downgrade();

    // TODO: only queue pipewiresrc?

    pipeline.add_many([
        &src,
        &par_filter,
        &queue,
        &rate,
        &convert,
        &scale,
        &capsfilter,
    ])?;
    gst::Element::link_many([
        &src,
        &par_filter,
        &queue,
        &rate,
        &convert,
        &scale,
        &capsfilter,
        &sink,
    ])?;

    let caps_sink_pad = capsfilter.static_pad("sink").unwrap();

    let scale_probe = add_scaling_probe(
        &caps_sink_pad,
        capsfilter_weak.clone(),
        max_width,
        max_height,
    )
    .ok_or(anyhow::anyhow!(
        "Could not add probe to capsfilter's sink pad"
    ))?;

    Ok((
        extra,
        PreviewElems {
            src,
            capsfilter,
            caps_sink_pad,
            scale_probe: Some(scale_probe),
            appsink: Some(sink),
        },
    ))
}

#[derive(Debug)]
pub struct PreviewPipeline {
    pub pipeline: gst::Pipeline,
    #[cfg(target_os = "linux")]
    _extra_video: Option<crate::transmission::ExtraVideoContext>,
    pub elems: PreviewElems,
    pub display_name: String,
}

impl PreviewPipeline {
    pub fn new<F>(display_name: String, on_new_sample: F, src: VideoSource) -> Result<Self>
    where
        F: FnMut(&gst_app::AppSink) -> std::result::Result<gst::FlowSuccess, gst::FlowError>
            + Send
            + 'static,
    {
        let pipeline = gst::Pipeline::new();

        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst_video::VideoCapsBuilder::new()
                    .format(gst_video::VideoFormat::Rgb)
                    .field("interlace-mode", "progressive")
                    .build(),
            )
            .build();

        pipeline.add(&appsink)?;

        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(on_new_sample)
                .build(),
        );

        let (_extra_video, elems) = add_video_src(&pipeline, appsink.upcast(), src, 300, 400, 5)?;

        pipeline.call_async(|pipeline| {
            pipeline.set_state(gst::State::Playing).unwrap();
        });

        Ok(Self {
            display_name,
            pipeline,
            #[cfg(target_os = "linux")]
            _extra_video,
            elems,
        })
    }
}

impl Drop for PreviewPipeline {
    fn drop(&mut self) {
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            error!(?err, "Failed to set pipeline state to null");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_res_to_fit() {
        assert_eq!(scale_res_to_fit(1920, 1080, 1920, 1080), (1920, 1080));
        assert_eq!(scale_res_to_fit(1920, 3944, 1920, 1080), (524, 1080));
        assert_eq!(scale_res_to_fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(scale_res_to_fit(4096, 2160, 1920, 1080), (1920, 1012));
        assert_eq!(scale_res_to_fit(1440, 2768, 1920, 1080), (560, 1080));
    }
}
