#[cfg(not(target_os = "android"))]
use crate::AudioSource;
use crate::Event;
#[cfg(target_os = "android")]
use crate::{SourceConfig, VideoSource};
use futures::StreamExt;
use gst::{glib, prelude::*};
use std::net::IpAddr;
#[cfg(target_os = "windows")]
use tracing::warn;
use tracing::{debug, error};

#[cfg(not(target_os = "android"))]
use crate::preview::PreviewPipeline;

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(target_os = "linux")]
use std::{cell::RefCell, ops::Deref, rc::Rc};

const MEGA_BIT: u32 = 1024 * 1024;
const WHEP_MIN_BITRATE: u32 = MEGA_BIT / 2;
const WHEP_START_BITRATE: u32 = MEGA_BIT * 4;
const WHEP_MAX_BITRATE: u32 = MEGA_BIT * 15;

fn addr_to_url_string(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(ipv4_addr) => ipv4_addr.to_string(),
        IpAddr::V6(ipv6_addr) => format!("[{ipv6_addr}]"),
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum ExtraVideoContext {
    PipewireVideoSource {
        /// Closes when dropped
        _fd: OwnedFd,
    },
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct ExtraVideoContext(());

/// What came of attaching a system audio source to a transmission pipeline.
// Only the Windows arm can decline, and macOS has no source at all, so off
// Windows one or both variants are never built.
#[cfg(not(target_os = "android"))]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug)]
enum AudioBranch {
    /// Capture is running. The context keeps any OS-side state alive.
    Attached(Option<ExtraAudioContext>),
    /// Capture could not be started; the cast can go on without it.
    Unavailable(String),
}

/// Pre-flight result: the source to attach, or why there is none.
#[cfg(not(target_os = "android"))]
type PreflightAudio = (Option<AudioSource>, Option<String>);

/// Name of the capture element, so the bus handler can tell its messages
/// apart from the rest of the pipeline's.
#[cfg(target_os = "windows")]
const SYSTEM_AUDIO_SRC_NAME: &str = "system-audio-src";

/// How long the pre-flight probe waits for the first captured buffer. Loopback
/// delivers within a couple of 10 ms engine periods once the endpoint is up,
/// but an idle Bluetooth or power-saving USB endpoint can take a few seconds
/// to bring its render path up, and a false negative here silently costs the
/// whole cast its audio.
#[cfg(target_os = "windows")]
const WASAPI_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Pause before re-opening the endpoint after the OS invalidated it (default
/// output switched or unplugged); long enough for the new device to settle.
#[cfg(target_os = "windows")]
const WASAPI_REOPEN_DELAY: std::time::Duration = std::time::Duration::from_millis(1500);

/// Loopback capture of the default render endpoint, i.e. the mix the user
/// hears. `wasapi2src` follows default-device changes; the legacy `wasapisrc`
/// is the fallback for builds that only ship the old plugin.
#[cfg(target_os = "windows")]
fn make_wasapi_loopback_src(continue_on_error: bool) -> anyhow::Result<gst::Element> {
    let factory = ["wasapi2src", "wasapisrc"]
        .into_iter()
        .find(|name| gst::ElementFactory::find(name).is_some())
        .ok_or(anyhow::anyhow!("No WASAPI capture element is available"))?;
    debug!(factory, "Creating WASAPI loopback source");

    // `loopback` is only mutable in READY, so it has to go in at construction.
    // Never combine it with `exclusive` (Windows rejects the pair), and never
    // set `loopback-target-pid`: a non-zero pid silently turns the element
    // back into a microphone capture. `low-latency` is left off: loopback
    // cannot take the IAudioClient3 path it exists for.
    //
    // The pipeline keeps the system clock the preview already runs on rather
    // than the audio device's: an audio master clock stops the moment the
    // capture stalls (endpoint invalidated, headset asleep), and the screen
    // capture paces itself on the pipeline clock, so video would freeze with
    // it. Slaving the source instead (skew, the default) keeps the audio
    // aligned within its drift tolerance and turns a stall into silence.
    let src = gst::ElementFactory::make(factory)
        .name(SYSTEM_AUDIO_SRC_NAME)
        .property("loopback", true)
        .property("provide-clock", false)
        .build()?;

    // wasapi2src >= 1.28: losing the endpoint mid-cast (headset unplugged,
    // exclusive-mode app taking over) turns into silence plus a warning
    // instead of an ERROR. Re-opening is up to us, see `add_bus_handler`.
    if continue_on_error && src.has_property("continue-on-error") {
        src.set_property("continue-on-error", true);
    }

    Ok(src)
}

/// `audioconvert ! audioresample ! capsfilter` pinning what webrtcsink gets.
///
/// Loopback hands out the endpoint's mix format as-is: usually F32LE, and
/// 44.1 kHz or 5.1/7.1 on some outputs. webrtcsink resamples but never
/// downmixes (more than two channels would negotiate MULTIOPUS, which
/// receivers don't take) and needs the audio caps to stay fixed for the whole
/// session, so everything is pinned here, channel mask included so nothing
/// depends on audioconvert's fixation. Convert before resample so the
/// downmix happens first.
#[cfg(target_os = "windows")]
fn make_audio_conversion_chain() -> anyhow::Result<[gst::Element; 3]> {
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let audio_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("layout", "interleaved")
        .field("channels", 2i32)
        .field("channel-mask", gst::Bitmask::new(0x3))
        .field("rate", 48000i32)
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_caps)
        .build()?;
    Ok([convert, resample, capsfilter])
}

#[cfg(target_os = "windows")]
fn describe_error_message(msg: &gst::Message) -> String {
    match msg.view() {
        gst::MessageView::Error(err) => format!(
            "{} ({})",
            err.error(),
            err.debug().map(|d| d.to_string()).unwrap_or_default()
        ),
        _ => "unknown error".to_owned(),
    }
}

/// Runs a throwaway capture chain on its own until it delivers a buffer.
///
/// WASAPI reports most failures (endpoint held in exclusive mode, missing
/// mfplat.dll on Windows N, no output device at all) only when the stream is
/// initialized on the streaming thread, not on the state change. webrtcsink
/// starts codec discovery per pad on its first buffer and the WHEP server
/// only once every pad is discovered, so an audio branch that never produces
/// would leave the whole cast without video as well. Finding out here lets
/// the cast go on without audio instead. The probe runs the same conversion
/// chain as the real branch so a negotiation failure shows up here too.
///
/// Blocks for up to [`WASAPI_PROBE_TIMEOUT`] plus the teardown; run it off
/// the event loop.
#[cfg(target_os = "windows")]
fn probe_wasapi_loopback() -> anyhow::Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn wait_for_first_buffer(
        pipeline: &gst::Pipeline,
        bus: &gst::Bus,
        got_buffer: &AtomicBool,
    ) -> anyhow::Result<()> {
        if pipeline.set_state(gst::State::Playing).is_err() {
            let detail = bus
                .timed_pop_filtered(
                    gst::ClockTime::from_mseconds(100),
                    &[gst::MessageType::Error],
                )
                .map(|msg| describe_error_message(&msg))
                .unwrap_or_else(|| "state change failed".to_owned());
            anyhow::bail!("failed to start: {detail}");
        }

        let deadline = std::time::Instant::now() + WASAPI_PROBE_TIMEOUT;
        while !got_buffer.load(Ordering::Relaxed) {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("no audio arrived within {WASAPI_PROBE_TIMEOUT:?}");
            }
            if let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(50),
                &[gst::MessageType::Error],
            ) {
                anyhow::bail!("{}", describe_error_message(&msg));
            }
        }

        Ok(())
    }

    // No `continue-on-error` here: it would paper over an unopenable
    // endpoint with a silent dummy device.
    let src = make_wasapi_loopback_src(false)?;
    let [convert, resample, capsfilter] = make_audio_conversion_chain()?;
    let fakesink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()?;

    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, &convert, &resample, &capsfilter, &fakesink])?;
    gst::Element::link_many([&src, &convert, &resample, &capsfilter, &fakesink])?;

    let got_buffer = Arc::new(AtomicBool::new(false));
    let probe_added = fakesink
        .static_pad("sink")
        .ok_or(anyhow::anyhow!("fakesink is missing its sink pad"))?
        .add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            {
                let got_buffer = Arc::clone(&got_buffer);
                move |_, _| {
                    got_buffer.store(true, Ordering::Relaxed);
                    gst::PadProbeReturn::Remove
                }
            },
        )
        .is_some();
    if !probe_added {
        anyhow::bail!("Failed to add buffer probe to fakesink");
    }

    let bus = pipeline
        .bus()
        .ok_or(anyhow::anyhow!("Pipeline without bus"))?;

    let result = wait_for_first_buffer(&pipeline, &bus, &got_buffer);

    if let Err(err) = pipeline.set_state(gst::State::Null) {
        error!(?err, "Failed to stop the audio probe pipeline");
    }

    result
}

/// Checks that the system audio source can actually deliver before the cast
/// commits to it. Off the event loop, since the probe blocks.
#[cfg(not(target_os = "android"))]
async fn preflight_audio_src(src: Option<AudioSource>) -> PreflightAudio {
    #[cfg(target_os = "windows")]
    if matches!(src, Some(AudioSource::WasapiLoopback)) {
        return preflight_wasapi_loopback().await;
    }
    (src, None)
}

#[cfg(target_os = "windows")]
async fn preflight_wasapi_loopback() -> PreflightAudio {
    // Leave the probe a moment past its own deadline for the teardown; if it
    // is still stuck, it finishes on its own thread and the elements it holds
    // never touch the real pipeline.
    let deadline = WASAPI_PROBE_TIMEOUT + std::time::Duration::from_secs(2);
    let probed =
        tokio::time::timeout(deadline, tokio::task::spawn_blocking(probe_wasapi_loopback)).await;
    let reason = match probed {
        Ok(Ok(Ok(()))) => return (Some(AudioSource::WasapiLoopback), None),
        Ok(Ok(Err(err))) => err.to_string(),
        Ok(Err(err)) => format!("probe task failed: {err}"),
        Err(_) => "probe did not finish in time".to_owned(),
    };
    warn!(%reason, "System audio capture failed, casting without audio");
    (None, Some(reason))
}

/// Attaches the (already probed) loopback capture to a pipeline holding
/// `sink`. Any failure past this point is unwound and reported as
/// [`AudioBranch::Unavailable`] rather than propagated, because a stream that
/// webrtcsink knows about but never gets a buffer for would hold up the
/// whole cast: only genuinely programmatic errors bubble up.
#[cfg(target_os = "windows")]
fn add_wasapi_loopback_src(
    pipeline: &gst::Pipeline,
    sink: &gst::Element,
) -> anyhow::Result<AudioBranch> {
    let src = make_wasapi_loopback_src(true)?;
    let [convert, resample, capsfilter] = make_audio_conversion_chain()?;
    // Decouples the WASAPI reader thread from webrtcsink's scheduling, like
    // the video branch's queue does for the screen capture.
    let queue = gst::ElementFactory::make("queue").build()?;
    let elems = [&src, &convert, &resample, &capsfilter, &queue];

    pipeline.add_many(elems)?;

    // Bring the branch up to the parent's state before webrtcsink learns of
    // it, downstream first so the source never pushes into a NULL element.
    // With the parent in READY this is where the device is opened for real,
    // which can still fail in the window since the probe.
    let attach = || -> anyhow::Result<gst::Pad> {
        gst::Element::link_many(elems)?;
        for elem in elems.iter().rev() {
            elem.sync_state_with_parent()?;
        }
        let audio_pad = sink
            .request_pad_simple("audio_%u")
            .ok_or(anyhow::anyhow!("webrtcsink has no audio_%u pad"))?;
        if let Err(err) = queue
            .static_pad("src")
            .ok_or(anyhow::anyhow!("queue is missing its src pad"))?
            .link(&audio_pad)
        {
            sink.release_request_pad(&audio_pad);
            return Err(err.into());
        }
        Ok(audio_pad)
    };

    match attach() {
        Ok(_) => Ok(AudioBranch::Attached(None)),
        Err(err) => {
            warn!(?err, "Could not attach system audio, casting without audio");
            for elem in elems {
                if let Err(err) = elem.set_state(gst::State::Null) {
                    error!(?err, "Failed to stop {}", elem.name());
                }
            }
            if let Err(err) = pipeline.remove_many(elems) {
                error!(?err, "Failed to remove the audio branch");
            }
            Ok(AudioBranch::Unavailable(err.to_string()))
        }
    }
}

/// Re-opens the capture endpoint after Windows invalidated it, which
/// `continue-on-error` reports as a warning (default output switched or
/// unplugged). wasapi2src only re-opens from a property write, and setting
/// `device` to its current value is that write once the element has flagged
/// the loss. If the endpoint is still gone the re-open warns again and lands
/// back here, which paces the retries.
#[cfg(target_os = "windows")]
fn maybe_reopen_system_audio(msg: &gst::message::Warning, rt_handle: &tokio::runtime::Handle) {
    let Some(src) = msg
        .src()
        .and_then(|obj| obj.downcast_ref::<gst::Element>())
        .filter(|elem| elem.name().as_str() == SYSTEM_AUDIO_SRC_NAME)
        .filter(|elem| elem.has_property("continue-on-error"))
    else {
        return;
    };

    let src = src.clone();
    rt_handle.spawn(async move {
        tokio::time::sleep(WASAPI_REOPEN_DELAY).await;
        // The cast may have ended meanwhile.
        if src.current_state() < gst::State::Paused {
            return;
        }
        debug!("Re-opening the system audio endpoint");
        src.set_property("device", None::<&str>);
    });
}

// macOS has no audio source variant: the match below is empty there (type
// `!`, so no trailing expression is needed) and the parameters go unused.
#[cfg(not(target_os = "android"))]
#[cfg_attr(target_os = "macos", allow(unused_variables))]
fn add_audio_src(
    pipeline: &gst::Pipeline,
    sink: &gst::Element,
    src: AudioSource,
) -> anyhow::Result<AudioBranch> {
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
            let from_main_pair =
                std::sync::Arc::new((parking_lot::Mutex::new(true), parking_lot::Condvar::new()));
            let from_main_pair_clone = std::sync::Arc::clone(&from_main_pair);

            let jh = std::thread::spawn(move || {
                use libpulse_binding::{context::Context, mainloop::threaded::Mainloop};

                fn set_and_notify(
                    pair: &std::sync::Arc<(parking_lot::Mutex<PulseResult>, parking_lot::Condvar)>,
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
                let set_default_op = context.borrow_mut().set_default_sink("fcast_sender_sink", {
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

                while set_default_op.get_state() == libpulse_binding::operation::State::Running {
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

            pipeline.add_many([&src, &capsfilter])?;
            gst::Element::link_many([&src, &capsfilter, sink])?;

            // Downstream first, so the source never pushes into a NULL element.
            capsfilter.sync_state_with_parent()?;
            src.sync_state_with_parent()?;

            let extra = Some(ExtraAudioContext::PulseVirtualSink {
                jh: Some(jh),
                pair: from_main_pair,
            });

            Ok(AudioBranch::Attached(extra))
        }
        #[cfg(target_os = "windows")]
        AudioSource::WasapiLoopback => add_wasapi_loopback_src(pipeline, sink),
    }
}

fn add_bus_handler(
    pipeline: &gst::Pipeline,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    rt_handle: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    rt_handle.clone().spawn({
        let bus = pipeline
            .bus()
            .ok_or(anyhow::anyhow!("Pipeline without bus"))?;
        // The task gets no finish signal. A failed upgrade of the weak
        // pipeline ref is the cue to quit.
        let pipeline_weak = pipeline.downgrade();

        async move {
            let mut messages = bus.stream();
            while let Some(msg) = messages.next().await {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(..) => if let Err(err) = event_tx.send(Event::EndSession { disconnect: true }) {
                        error!(?err, "Failed to send event");
                    },
                    MessageView::Error(err) => {
                        error!(
                            src = ?err.src().map(|s| s.path_string()),
                            err = ?err.error(),
                            debug = ?err.debug(),
                            "Error",
                        );
                        // Not ended here since webrtcbin per-consumer errors land on
                        // this bus too. Fatal signaller failures send EndSession directly.
                        // if let Err(err) = event_tx.send(Event::EndSession { disconnect: true }) {
                        //     error!(?err, "Failed to send event");
                        // }
                    }
                    MessageView::Warning(warning) => {
                        debug!(
                            src = ?warning.src().map(|s| s.path_string()),
                            err = ?warning.error(),
                            debug = ?warning.debug(),
                            "Warning",
                        );
                        #[cfg(target_os = "windows")]
                        maybe_reopen_system_audio(warning, &rt_handle);
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

    Ok(())
}

fn configure_webrtcsink(sink: &gstrswebrtc::webrtcsink::BaseWebRTCSink) {
    sink.set_property("min-bitrate", WHEP_MIN_BITRATE);
    sink.set_property("start-bitrate", WHEP_START_BITRATE);
    sink.set_property("max-bitrate", WHEP_MAX_BITRATE);
    sink.set_property_from_str("enable-mitigation-modes", "downsampled");
    sink.set_property_from_str("stun-server", ""); // We don't care about internet connections
    // VP8 only. It is widely available, and offering fewer formats reduces
    // startup time before streaming.
    sink.set_property("video-caps", gst::Caps::builder("video/x-vp8").build());
}

fn create_webrtcsink(
    server_port: u16,
    rt_handle: tokio::runtime::Handle,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
) -> anyhow::Result<gstrswebrtc::webrtcsink::BaseWebRTCSink> {
    let signaller = crate::whep_signaller::WhepServerSignaller::default();
    // Binding is async so the sink was already built Ok. End the session so the
    // sender leaves its casting state instead of waiting for SignallerStarted.
    signaller.connect(
        crate::whep_signaller::ON_SERVER_FAILED_SIGNAL_NAME,
        false,
        {
            let event_tx = event_tx.clone();
            move |vals| {
                let msg = vals.get(1).and_then(|v| v.get::<String>().ok());
                error!(?msg, "WHEP server failed, ending the session");
                if let Err(err) = event_tx.send(Event::EndSession { disconnect: true }) {
                    error!(?err, "Failed to send event");
                }
                None
            }
        },
    );
    signaller.connect(
        crate::whep_signaller::ON_SERVER_STARTED_SIGNAL_NAME,
        false,
        move |vals| {
            let Some(bound_ipv4_port_val) = vals.get(1) else {
                error!("Could not get bound ipv4 port parameter");
                return None;
            };
            let Some(bound_ipv6_port_val) = vals.get(2) else {
                error!("Could not get bound ipv6 port parameter");
                return None;
            };

            fn to_port(val: &glib::Value) -> Option<u16> {
                match val.get::<u32>() {
                    Ok(port) => Some(port as u16),
                    Err(err) => {
                        error!(?err, "Failed to get value as u32");
                        None
                    }
                }
            }

            let bound_port_v4 = to_port(bound_ipv4_port_val)?;
            let bound_port_v6 = to_port(bound_ipv6_port_val)?;
            let event_tx = event_tx.clone();
            rt_handle.spawn(async move {
                event_tx
                    .send(Event::SignallerStarted {
                        bound_port_v4,
                        bound_port_v6,
                    })
                    .unwrap();
            });

            None
        },
    );
    signaller.set_property("server-port", server_port as u32);
    let sink = gstrswebrtc::webrtcsink::BaseWebRTCSink::with_signaller(
        gstrswebrtc::signaller::Signallable::from(signaller),
    );
    configure_webrtcsink(&sink);

    Ok(sink)
}

fn create_f_webrtcsink(
    _rt_handle: tokio::runtime::Handle,
    _event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
) -> anyhow::Result<(
    gstrswebrtc::webrtcsink::BaseWebRTCSink,
    crate::fsignaller::FSignaller,
)> {
    let signaller = crate::fsignaller::FSignaller::default();
    let signaller_ref = signaller.clone();
    let sink = gstrswebrtc::webrtcsink::BaseWebRTCSink::with_signaller(
        gstrswebrtc::signaller::Signallable::from(signaller),
    );
    configure_webrtcsink(&sink);
    Ok((sink, signaller_ref))
}

pub enum SinkConfig {
    Whep { server_port: u16 },
    FCast,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum ExtraAudioContext {
    PulseVirtualSink {
        jh: Option<std::thread::JoinHandle<()>>,
        pair: std::sync::Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
    },
}

// Never constructed off Linux: WASAPI loopback is passive, nothing to undo.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
#[derive(Debug)]
struct ExtraAudioContext(());

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

#[derive(Debug)]
pub enum Pipeline {
    Simple(gst::Pipeline),
    #[cfg(not(target_os = "android"))]
    Preview(PreviewPipeline),
}

impl Pipeline {
    pub fn shutdown(&self) {
        let pipeline = match self {
            Pipeline::Simple(p) => p,
            #[cfg(not(target_os = "android"))]
            Pipeline::Preview(preview) => &preview.pipeline,
        };
        pipeline.call_async(|pipeline| {
            if let Err(err) = pipeline.set_state(gst::State::Null) {
                error!("Failed to stop pipeline: {err}");
            }
        });
    }
}

#[cfg(not(target_os = "android"))]
fn sink_from_preview(
    sink: gstrswebrtc::webrtcsink::BaseWebRTCSink,
    preview_pipeline: Option<PreviewPipeline>,
    (audio_src, audio_unavailable): PreflightAudio,
    max_width: u32,
    max_height: u32,
    max_framerate: u32,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    rt_handle: tokio::runtime::Handle,
) -> anyhow::Result<(Pipeline, Option<ExtraAudioContext>, Option<String>)> {
    if let Some(mut preview_pipeline) = preview_pipeline {
        let elems = &mut preview_pipeline.elems;

        let capsfilter_src_pad = elems.capsfilter.static_pad("src").unwrap();

        // TODO: it seems that all sources are fine to be set to ready, do we still need
        // to block upstream?
        let needs_ready = {
            let name = elems
                .src
                .factory()
                .ok_or(anyhow::anyhow!("Source element is missing factory"))?
                .name();
            name == "ximagesrc"
                || name == "d3d12screencapturesrc"
                || name == "avfvideosrc"
                || name == "pipewiresrc"
                || name == "videotestsrc"
        };

        if needs_ready {
            preview_pipeline.pipeline.set_state(gst::State::Ready)?;
        }

        let block_probe = capsfilter_src_pad
            .add_probe(gst::PadProbeType::BLOCK, |_, _| gst::PadProbeReturn::Drop)
            .ok_or(anyhow::anyhow!(
                "Failed to add blocking probe to capsfilter's src pad"
            ))?;
        debug!("Added blocking probe to capsfilter's sink pad");

        if let Some(scale_probe) = elems.scale_probe.take() {
            elems.caps_sink_pad.remove_probe(scale_probe);
            debug!("Removed scaling probe from capsfilter");
        }

        if let Some(appsink) = elems.appsink.take() {
            elems.capsfilter.unlink(&appsink);
            preview_pipeline.pipeline.remove(&appsink)?;
            appsink.set_state(gst::State::Null)?;
            debug!("Removed appsink");
        }

        elems.scale_probe = Some(
            crate::preview::add_scaling_probe(
                &elems.caps_sink_pad,
                elems.capsfilter.downgrade(),
                max_width,
                max_height,
            )
            .unwrap(),
        );
        debug!("Added new scaling probe to capsfilter");

        elems.capsfilter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", gst::Fraction::new(max_framerate as i32, 1))
                .field("interlace-mode", "progressive")
                .field("width", gst::IntRange::new(1, 16383))
                .field("height", gst::IntRange::new(1, 16383))
                .build(),
        );

        preview_pipeline.pipeline.add(&sink)?;

        let sink_video_pad = sink.request_pad_simple("video_%u").unwrap();
        capsfilter_src_pad.link(&sink_video_pad)?;
        debug!("Added and synced webrtc sink");

        capsfilter_src_pad.remove_probe(block_probe);
        debug!("Removed capsfilter blocking probe");

        let mut extra_audio = None;
        let mut audio_unavailable = audio_unavailable;
        if let Some(audio_src) = audio_src {
            match add_audio_src(&preview_pipeline.pipeline, sink.upcast_ref(), audio_src)? {
                AudioBranch::Attached(extra) => extra_audio = extra,
                // Video is still worth casting on its own.
                AudioBranch::Unavailable(reason) => audio_unavailable = Some(reason),
            }
        }

        sink.sync_state_with_parent()?;

        if needs_ready {
            preview_pipeline.pipeline.set_state(gst::State::Playing)?;
        }

        add_bus_handler(&preview_pipeline.pipeline, event_tx, rt_handle)?;

        Ok((
            Pipeline::Preview(preview_pipeline),
            extra_audio,
            audio_unavailable,
        ))
    } else if let Some(audio_src) = audio_src {
        let pipeline = gst::Pipeline::new();

        pipeline.add(&sink)?;

        let extra_audio = match add_audio_src(&pipeline, sink.upcast_ref(), audio_src)? {
            AudioBranch::Attached(extra) => extra,
            // There is nothing else to cast.
            AudioBranch::Unavailable(reason) => {
                anyhow::bail!("System audio capture is unavailable: {reason}")
            }
        };

        pipeline.call_async(|pipeline| {
            pipeline.set_state(gst::State::Playing).unwrap();
        });

        add_bus_handler(&pipeline, event_tx, rt_handle)?;

        Ok((Pipeline::Simple(pipeline), extra_audio, None))
    } else if let Some(reason) = audio_unavailable {
        // Audio was all there was to cast. Reaches the crash window with the
        // log, which is where the reason ends up being read.
        anyhow::bail!("System audio capture is unavailable: {reason}");
    } else {
        anyhow::bail!("Missing source");
    }
}

#[derive(Debug)]
pub struct WhepSink {
    // pub pipeline: gst::Pipeline,
    pub pipeline: Pipeline,
    /// Keeps RAII guards alive so stream sources are not prematurely torn down
    #[cfg(not(target_os = "android"))]
    _extra_audio: Option<ExtraAudioContext>,
    /// Set when system audio was asked for but capture could not start, so
    /// the cast carries video only. Holds the reason.
    #[cfg(not(target_os = "android"))]
    pub audio_unavailable: Option<String>,
}

impl WhepSink {
    #[cfg(target_os = "android")]
    pub fn new(
        source_config: SourceConfig,
        event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
        rt_handle: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let pipeline = gst::Pipeline::new();

        let sink = create_webrtcsink(0, rt_handle.clone(), event_tx.clone())?;
        let sink = sink.upcast::<gst::Element>();
        pipeline.add(&sink)?;

        let self_ = Self {
            pipeline: Pipeline::Simple(pipeline.clone()),
        };

        match source_config {
            SourceConfig::Video(src) => {
                let VideoSource::Source(appsrc) = src;
                pipeline.add_many([&appsrc])?;
                gst::Element::link_many([appsrc.upcast_ref(), &sink])?;
            }
        }

        pipeline.call_async(|pipeline| {
            debug!("Starting pipeline...");

            if let Err(err) = pipeline.set_state(gst::State::Playing) {
                error!("Failed to start pipeline: {err}");
            } else {
                debug!("Pipeline started");
            }
        });

        add_bus_handler(&pipeline, event_tx, rt_handle)?;

        Ok(self_)
    }

    #[cfg(not(target_os = "android"))]
    pub async fn from_preview(
        sink_config: SinkConfig,
        event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
        rt_handle: tokio::runtime::Handle,
        preview_pipeline: Option<PreviewPipeline>,
        audio_src: Option<AudioSource>,
        max_width: u32,
        max_height: u32,
        max_framerate: u32,
    ) -> anyhow::Result<Self> {
        let sink = match sink_config {
            SinkConfig::Whep { server_port } => {
                create_webrtcsink(server_port, rt_handle.clone(), event_tx.clone())?
            }
            _ => todo!(),
        };
        let audio = preflight_audio_src(audio_src).await;
        let (pipeline, _extra_audio, audio_unavailable) = sink_from_preview(
            sink,
            preview_pipeline,
            audio,
            max_width,
            max_height,
            max_framerate,
            event_tx,
            rt_handle,
        )?;
        Ok(Self {
            pipeline,
            _extra_audio,
            audio_unavailable,
        })
    }

    pub fn get_play_msg(&self, addr: IpAddr, port: u16) -> (String, String) {
        (
            "application/x-whep".to_owned(),
            format!("http://{}:{port}/endpoint", addr_to_url_string(addr)),
        )
    }

    pub fn shutdown(&mut self) {
        self.pipeline.shutdown();
    }
}

#[derive(Debug)]
pub struct FSink {
    // pub pipeline: gst::Pipeline,
    pub pipeline: Pipeline,
    pub signaller: crate::fsignaller::FSignaller,
    /// Keeps RAII guards alive so stream sources are not prematurely torn down
    #[cfg(not(target_os = "android"))]
    _extra_audio: Option<ExtraAudioContext>,
    /// Set when system audio was asked for but capture could not start, so
    /// the cast carries video only. Holds the reason.
    #[cfg(not(target_os = "android"))]
    pub audio_unavailable: Option<String>,
}

impl FSink {
    // TODO: fixme https://github.com/GStreamer/gst-plugins-rs/commit/19b8a0f602e5852c77dd6e8a3e0e536521b1f7aa?
    #[cfg(target_os = "android")]
    pub fn new(
        source_config: SourceConfig,
        event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
        rt_handle: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let (sink, signaller) = create_f_webrtcsink(rt_handle.clone(), event_tx.clone())?;
        let sink = sink.upcast::<gst::Element>();
        let pipeline = gst::Pipeline::new();
        pipeline.add(&sink)?;

        match source_config {
            SourceConfig::Video(src) => {
                let VideoSource::Source(appsrc) = src;
                pipeline.add_many([&appsrc])?;
                gst::Element::link_many([appsrc.upcast_ref(), &sink])?;
            }
        }

        pipeline.call_async(|pipeline| {
            debug!("Starting pipeline...");

            if let Err(err) = pipeline.set_state(gst::State::Playing) {
                error!("Failed to start pipeline: {err}");
            } else {
                debug!("Pipeline started");
            }
        });

        add_bus_handler(&pipeline, event_tx, rt_handle)?;

        Ok(Self {
            pipeline: Pipeline::Simple(pipeline),
            signaller,
        })
    }

    #[cfg(not(target_os = "android"))]
    pub async fn from_preview(
        sink_config: SinkConfig,
        event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
        rt_handle: tokio::runtime::Handle,
        preview_pipeline: Option<PreviewPipeline>,
        audio_src: Option<AudioSource>,
        max_width: u32,
        max_height: u32,
        max_framerate: u32,
    ) -> anyhow::Result<Self> {
        let (sink, signaller) = match sink_config {
            SinkConfig::FCast => create_f_webrtcsink(rt_handle.clone(), event_tx.clone())?,
            _ => unreachable!(),
        };
        let audio = preflight_audio_src(audio_src).await;
        let (pipeline, _extra_audio, audio_unavailable) = sink_from_preview(
            sink,
            preview_pipeline,
            audio,
            max_width,
            max_height,
            max_framerate,
            event_tx,
            rt_handle,
        )?;
        Ok(Self {
            pipeline,
            signaller,
            _extra_audio,
            audio_unavailable,
        })
    }

    pub fn shutdown(&mut self) {
        self.pipeline.shutdown();
    }
}
