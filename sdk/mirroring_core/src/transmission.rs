use crate::{AudioSource, Event, SourceConfig, VideoSource};
use futures::StreamExt;
use gst::prelude::*;
use std::net::IpAddr;
use tracing::{debug, error};

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(target_os = "linux")]
use std::{cell::RefCell, ops::Deref, rc::Rc};

const MEGA_BIT: u32 = 1024 * 1024;
const WHEP_MIN_BITRATE: u32 = MEGA_BIT / 2;
const WHEP_START_BITRATE: u32 = MEGA_BIT * 16;
const WHEP_MAX_BITRATE: u32 = MEGA_BIT * 48;

fn addr_to_url_string(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(ipv4_addr) => ipv4_addr.to_string(),
        IpAddr::V6(ipv6_addr) => format!("[{ipv6_addr}]"),
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum ExtraVideoContext {
    PipewireVideoSource {
        /// Closes when dropped
        _fd: OwnedFd,
    },
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum ExtraAudioContext {
    PulseVirtualSink {
        jh: Option<std::thread::JoinHandle<()>>,
        pair: std::sync::Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
    },
}

#[cfg(target_os = "linux")]
impl Drop for ExtraAudioContext {
    fn drop(&mut self) {
        match self {
            #[cfg(target_os = "linux")]
            ExtraAudioContext::PulseVirtualSink { jh, pair } => {
                debug!("Telling pulse thread to quit");

                *pair.0.lock() = false;
                pair.1.notify_one();

                if let Some(jh) = jh.take() {
                    if jh.join().is_err() {
                        error!("Failed to join pulse thread");
                    } else {
                        debug!("Pulse thread finished");
                    }
                }
            }
        }
    }
}

fn scale_res_to_fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let aspect_ratio = (max_width as f32 / width as f32).min(max_height as f32 / height as f32);
    (
        (width as f32 * aspect_ratio) as u32 & !1,
        (height as f32 * aspect_ratio) as u32 & !1,
    )
}

#[derive(Debug)]
pub struct WhepSink {
    pub pipeline: gst::Pipeline,
    /// Used to keep connections and similar stuff alive for later use or for keeping RAII guards
    /// from not prematurely terminating stream sources
    #[cfg(target_os = "linux")]
    extra_audio: Option<ExtraAudioContext>,
    #[cfg(target_os = "linux")]
    extra_video: Option<ExtraVideoContext>,
}

impl WhepSink {
    fn add_video_src(
        &mut self,
        sink: &gst::Element,
        src: VideoSource,
        max_width: u32,
        max_height: u32,
        max_framerate: u32,
    ) -> anyhow::Result<()> {
        let src_element = match src {
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

                self.extra_video = Some(ExtraVideoContext::PipewireVideoSource { _fd: fd });

                src
            }
            #[cfg(target_os = "linux")]
            VideoSource::XWindow { id, .. } => gst::ElementFactory::make("ximagesrc")
                .property("xid", id as u64)
                .property("use-damage", false)
                .build()?,
            #[cfg(target_os = "linux")]
            VideoSource::XDisplay {
                id,
                width,
                height,
                x_offset,
                y_offset,
                ..
            } => gst::ElementFactory::make("ximagesrc")
                .property("xid", id as u64)
                .property("startx", x_offset as u32)
                .property("starty", y_offset as u32)
                .property("endx", (x_offset as u32) + (width as u32) - 1)
                .property("endy", (y_offset as u32) + (height as u32) - 1)
                .property("use-damage", false)
                .build()?,
            #[cfg(target_os = "macos")]
            VideoSource::CgDisplay { id, .. } => gst::ElementFactory::make("avfvideosrc")
                .property("capture-screen", true)
                .property("capture-screen-cursor", true)
                .property("device-index", id)
                .build()?,
            #[cfg(target_os = "windows")]
            VideoSource::D3d11Monitor { handle, .. } => {
                gst::ElementFactory::make("d3d11screencapturesrc")
                    .property("show-cursor", true)
                    .property("monitor-handle", handle)
                    .build()?
            }
            #[cfg(target_os = "android")]
            VideoSource::Source(appsrc) => appsrc.upcast(),
        };

        let scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", false)
            .build()?;
        let rate = gst::ElementFactory::make("videorate")
            .property("drop-only", true)
            .build()?;
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property_from_str("caps-change-mode", "delayed")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(max_framerate as i32, 1))
                    .field("interlace-mode", "progressive")
                    .build(),
            )
            .build()?;

        let capsfilter_weak = capsfilter.downgrade();

        self.pipeline
            .add_many([&src_element, &scale, &rate, &capsfilter])?;
        gst::Element::link_many([&src_element, &scale, &rate, &capsfilter, sink])?;

        let caps_sink_pad = capsfilter.static_pad("sink").unwrap();
        caps_sink_pad
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
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
            .ok_or(anyhow::anyhow!(
                "Could not add probe to capsfilter's sink pad"
            ))?;

        Ok(())
    }

    fn add_audio_src(&mut self, sink: &gst::Element, src: AudioSource) -> anyhow::Result<()> {
        match src {
            #[cfg(target_os = "linux")]
            AudioSource::PulseVirtualSink => {
                #[derive(PartialEq)]
                enum PulseResult {
                    None,
                    Failed,
                    Ok,
                }

                let from_pulse_pair = std::sync::Arc::new((
                    parking_lot::Mutex::new(PulseResult::None),
                    parking_lot::Condvar::new(),
                ));
                let from_pulse_pair_clone = std::sync::Arc::clone(&from_pulse_pair);
                let from_main_pair = std::sync::Arc::new((
                    parking_lot::Mutex::new(true),
                    parking_lot::Condvar::new(),
                ));
                let from_main_pair_clone = std::sync::Arc::clone(&from_main_pair);

                let jh = std::thread::spawn(move || {
                    use libpulse_binding::{context::Context, mainloop::threaded::Mainloop};

                    fn set_and_notify(
                        pair: &std::sync::Arc<(
                            parking_lot::Mutex<PulseResult>,
                            parking_lot::Condvar,
                        )>,
                        result: PulseResult,
                    ) {
                        *pair.0.lock() = result;
                        pair.1.notify_one();
                    }

                    let mainloop = Rc::new(RefCell::new(match Mainloop::new() {
                        Some(ml) => ml,
                        None => {
                            error!("Failed to create pulse audio mainloop");
                            set_and_notify(&from_pulse_pair_clone, PulseResult::Failed);
                            return;
                        }
                    }));

                    let context = Rc::new(RefCell::new(
                        match Context::new(mainloop.borrow().deref(), "fcast sender") {
                            Some(ctx) => ctx,
                            None => {
                                error!("Failed to create pulse audio context");
                                set_and_notify(&from_pulse_pair_clone, PulseResult::Failed);
                                return;
                            }
                        },
                    ));

                    {
                        let ml_ref = Rc::clone(&mainloop);
                        let context_ref = Rc::clone(&context);
                        context
                            .borrow_mut()
                            .set_state_callback(Some(Box::new(move || {
                                let state = unsafe { (*context_ref.as_ptr()).get_state() };
                                debug!(?state, "New pulse state");
                                match state {
                                    libpulse_binding::context::State::Ready
                                    | libpulse_binding::context::State::Failed
                                    | libpulse_binding::context::State::Terminated => unsafe {
                                        (*ml_ref.as_ptr()).signal(false);
                                    },
                                    _ => {}
                                }
                            })));
                    }

                    if let Err(err) = context.borrow_mut().connect(
                        None,
                        libpulse_binding::context::FlagSet::NOFLAGS,
                        None,
                    ) {
                        error!(?err, "Failed to connect to pulse");
                        set_and_notify(&from_pulse_pair_clone, PulseResult::Failed);
                        return;
                    }

                    mainloop.borrow_mut().lock();

                    debug!("Starting pulse mainloop...");
                    if let Err(err) = mainloop.borrow_mut().start() {
                        error!(?err, "Failed to start mainloop");
                        set_and_notify(&from_pulse_pair_clone, PulseResult::Failed);
                        return;
                    }

                    debug!("Connecting to pulse...");

                    loop {
                        match context.borrow().get_state() {
                            libpulse_binding::context::State::Ready => {
                                break;
                            }
                            libpulse_binding::context::State::Failed
                            | libpulse_binding::context::State::Terminated => {
                                error!("Context state failed/terminated, quitting...");
                                mainloop.borrow_mut().unlock();
                                mainloop.borrow_mut().stop();
                                set_and_notify(&from_pulse_pair_clone, PulseResult::Failed);
                                return;
                            }
                            _ => {
                                mainloop.borrow_mut().wait();
                            }
                        }
                    }
                    context.borrow_mut().set_state_callback(None);

                    debug!("Successfully connected");

                    let mut pulse_introspector = context.borrow_mut().introspect();
                    let module_idx = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
                    debug!("Trying to load `module-null-sink`...");
                    let load_op = pulse_introspector.load_module(
                        "module-null-sink",
                        "sink_name='fcast_sender_sink' formats='float32le, format.rate=\"[48000]\" format.channels=\"2\"; pcm'",
                        {
                            let ml_ref = Rc::clone(&mainloop);
                            let module_idx = std::sync::Arc::clone(&module_idx);
                            move |idx| {
                                debug!("Got pulse module index: {idx}");
                                module_idx.store(idx, std::sync::atomic::Ordering::Relaxed);
                                unsafe { (*ml_ref.as_ptr()).signal(false); }
                        }});

                    while load_op.get_state() == libpulse_binding::operation::State::Running {
                        mainloop.borrow_mut().wait();
                    }

                    if load_op.get_state() == libpulse_binding::operation::State::Cancelled {
                        error!("Load module-null-sink failed due to the operation being cancelled");
                        set_and_notify(&from_pulse_pair_clone, PulseResult::Failed);
                        return;
                    }

                    debug!("Setting default sink");
                    let set_default_op =
                        context.borrow_mut().set_default_sink("fcast_sender_sink", {
                            let ml_ref = Rc::clone(&mainloop);
                            move |ok| {
                                if ok {
                                    debug!("Successfully set the default pulse sink");
                                } else {
                                    error!("Failed to set the default pulse sink");
                                }
                                unsafe {
                                    (*ml_ref.as_ptr()).signal(false);
                                }
                            }
                        });

                    while set_default_op.get_state() == libpulse_binding::operation::State::Running
                    {
                        mainloop.borrow_mut().wait();
                    }

                    mainloop.borrow_mut().unlock();

                    // Send a signal that the device is available for `pulsesrc`
                    set_and_notify(&from_pulse_pair_clone, PulseResult::Ok);

                    // Wait for quit
                    let (main_lock, main_cvar) = &*from_main_pair_clone;
                    let mut should_run = main_lock.lock();
                    while *should_run {
                        main_cvar.wait(&mut should_run);
                    }

                    debug!("Got quit signal");

                    mainloop.borrow_mut().lock();

                    let unload_op = pulse_introspector.unload_module(
                        module_idx.load(std::sync::atomic::Ordering::Relaxed),
                        {
                            let ml_ref = Rc::clone(&mainloop);
                            move |_ok| unsafe {
                                (*ml_ref.as_ptr()).signal(false);
                            }
                        },
                    );

                    while unload_op.get_state() == libpulse_binding::operation::State::Running {
                        mainloop.borrow_mut().wait();
                    }

                    context.borrow_mut().disconnect();

                    mainloop.borrow_mut().unlock();

                    mainloop.borrow_mut().stop();
                });

                let (pulse_lock, pulse_cvar) = &*from_pulse_pair;
                let mut pulse_res = pulse_lock.lock();
                while *pulse_res == PulseResult::None {
                    pulse_cvar.wait(&mut pulse_res);
                }

                match *pulse_res {
                    PulseResult::Failed => panic!("Pulse failed"),
                    PulseResult::Ok => debug!("Pulse finished OK"),
                    _ => unreachable!(),
                }

                let src = gst::ElementFactory::make("pulsesrc")
                    .property("device", "fcast_sender_sink.monitor")
                    .build()?;
                let audio_caps = gst::Caps::builder("audio/x-raw")
                    .field("channels", 2i32)
                    .field("rate", 48000i32)
                    .build();
                let capsfilter = gst::ElementFactory::make("capsfilter")
                    .property("caps", audio_caps.clone())
                    .build()?;

                self.pipeline.add_many([&src, &capsfilter])?;
                gst::Element::link_many([&src, &capsfilter, sink])?;

                self.extra_audio = Some(ExtraAudioContext::PulseVirtualSink {
                    jh: Some(jh),
                    pair: from_main_pair,
                });
            }
            #[cfg(target_os = "android")]
            _ => todo!(),
        }

        Ok(())
    }

    pub fn new(
        source_config: SourceConfig,
        event_tx: tokio::sync::mpsc::Sender<Event>,
        rt_handle: tokio::runtime::Handle,
        max_width: u32,
        max_height: u32,
        max_framerate: u32,
    ) -> anyhow::Result<Self> {
        let pipeline = gst::Pipeline::new();

        let signaller = crate::whep_signaller::WhepServerSignaller::default();
        let rt_handle_clone = rt_handle.clone();
        let event_tx_clone = event_tx.clone();
        signaller.connect(
            crate::whep_signaller::ON_SERVER_STARTED_SIGNAL_NAME,
            false,
            move |vals| {
                let Some(bound_port_val) = vals.get(1) else {
                    error!("Could not get bound port parameter");
                    return None;
                };
                let bound_port = match bound_port_val.get::<u32>() {
                    Ok(port) => port as u16,
                    Err(err) => {
                        error!(?err, "Failed to get `bound_port_val` as u32");
                        return None;
                    }
                };
                let event_tx = event_tx_clone.clone();
                rt_handle_clone.spawn(async move {
                    event_tx
                        .send(Event::SignallerStarted { bound_port })
                        .await
                        .unwrap();
                });

                None
            },
        );
        let sink = gst_rs_webrtc::webrtcsink::BaseWebRTCSink::with_signaller(
            gst_rs_webrtc::signaller::Signallable::from(signaller),
        );
        sink.set_property("min-bitrate", WHEP_MIN_BITRATE);
        sink.set_property("start-bitrate", WHEP_START_BITRATE);
        sink.set_property("max-bitrate", WHEP_MAX_BITRATE);
        sink.set_property_from_str("enable-mitigation-modes", "downsampled");
        sink.set_property_from_str("stun-server", ""); // We don't care about internet connections
        // NOTE: we ask for VP8 only because it's widely available and having few possible formats
        //       reduces the startup time before streaming
        sink.set_property("video-caps", gst::Caps::builder("video/x-vp8").build());

        let sink = sink.upcast();

        pipeline.add(&sink)?;

        let mut self_ = Self {
            pipeline,
            #[cfg(target_os = "linux")]
            extra_audio: None,
            #[cfg(target_os = "linux")]
            extra_video: None,
        };

        match source_config {
            SourceConfig::Video(src) => {
                self_.add_video_src(&sink, src, max_width, max_height, max_framerate)?
            }
            SourceConfig::Audio(audio) => self_.add_audio_src(&sink, audio)?,
            SourceConfig::AudioVideo { video, audio } => {
                self_.add_video_src(&sink, video, max_width, max_height, max_framerate)?;
                self_.add_audio_src(&sink, audio)?;
            }
        }

        self_.pipeline.call_async(|pipeline| {
            debug!("Starting pipeline...");
            if let Err(err) = pipeline.set_state(gst::State::Playing) {
                error!("Failed to start pipeline: {err}");
            } else {
                debug!("Pipeline started");
            }
        });

        rt_handle.spawn({
            let bus = self_
                .pipeline
                .bus()
                .ok_or(anyhow::anyhow!("Pipeline without bus"))?;
            // We keep weak pipeline ref because the thread does not receive a finish signal,
            // therefore when we can't upgrade the ref, we know to quit
            let pipeline_weak = self_.pipeline.downgrade();

            async move {
                let mut messages = bus.stream();
                while let Some(msg) = messages.next().await {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Eos(..) => if let Err(err) = event_tx.send(Event::EndSession).await {
                            error!(?err, "Failed to send event");
                        },
                        MessageView::Error(err) => {
                            error!(
                                src = ?err.src().map(|s| s.path_string()),
                                err = ?err.error(),
                                debug = ?err.debug(),
                                "Error",
                            );
                            if let Err(err) = event_tx.send(Event::EndSession).await {
                                error!(?err, "Failed to send event");
                            }
                        }
                        MessageView::StateChanged(state_changed) => {
                            let Some(pipeline) = pipeline_weak.upgrade() else {
                                debug!("Failed to handle state change bus message because pipeline is missing");
                                return;
                            };

                            if state_changed.src() == Some(pipeline.upcast_ref())
                                && state_changed.old() == gst::State::Paused
                                && state_changed.current() == gst::State::Playing
                            {
                                debug!("Pipeline is playing");
                            }
                        }
                        _ => (),
                    }
                }

                debug!("Bus watcher quit");
            }
        });

        Ok(self_)
    }

    pub fn get_play_msg(&self, addr: IpAddr, port: u16) -> (String, String) {
        (
            "application/x-whep".to_owned(),
            format!("http://{}:{port}/endpoint", addr_to_url_string(addr)),
        )
    }

    pub fn shutdown(&mut self) {
        self.pipeline.call_async(|pipeline| {
            if let Err(err) = pipeline.set_state(gst::State::Null) {
                error!("Failed to stop pipeline: {err}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::transmission::scale_res_to_fit;

    #[test]
    fn test_scale_res_to_fit() {
        assert_eq!(scale_res_to_fit(1920, 1080, 1920, 1080), (1920, 1080));
        assert_eq!(scale_res_to_fit(1920, 3944, 1920, 1080), (524, 1080));
        assert_eq!(scale_res_to_fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(scale_res_to_fit(4096, 2160, 1920, 1080), (1920, 1012));
        assert_eq!(scale_res_to_fit(1440, 2768, 1920, 1080), (560, 1080));
    }
}
