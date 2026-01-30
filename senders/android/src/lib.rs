use anyhow::{bail, Result};
use fcast_sender_sdk::{context::CastContext, device, device::DeviceInfo};
use gst::prelude::{BufferPoolExt, BufferPoolExtManual};
use gst_video::{VideoColorimetry, VideoFrameExt};
use jni::{
    objects::{JByteBuffer, JObject, JString},
    JavaVM,
};
use mcore::{transmission::WhepSink, DeviceEvent, Event, ShouldQuit, SourceConfig};
use parking_lot::{Condvar, Mutex};
use std::{collections::HashMap, net::Ipv6Addr, sync::Arc};
use tracing::{debug, error};

lazy_static::lazy_static! {
    pub static ref GLOB_EVENT_CHAN: (crossbeam_channel::Sender<Event>, crossbeam_channel::Receiver<Event>)
        = crossbeam_channel::bounded(2);
    pub static ref FRAME_PAIR: (Mutex<Option<gst_video::VideoFrame<gst_video::video_frame::Writable>>>, Condvar) = (Mutex::new(None), Condvar::new());
    pub static ref FRAME_POOL: Mutex<gst_video::VideoBufferPool> = Mutex::new(gst_video::VideoBufferPool::new());
}

slint::include_modules!();

macro_rules! log_err {
    ($res:expr, $msg: expr) => {
        if let Err(err) = ($res) {
            error!(?err, $msg);
        }
    };
}

#[derive(Debug)]
enum JavaMethod {
    StopCapture,
    ScanQr,
}

fn call_java_method_no_args(app: &slint::android::AndroidApp, method: JavaMethod) {
    let vm = unsafe {
        let ptr = app.vm_as_ptr() as *mut jni::sys::JavaVM;
        assert!(!ptr.is_null(), "JavaVM ptr is null");
        JavaVM::from_raw(ptr).unwrap()
    };
    let activity = unsafe {
        let ptr = app.activity_as_ptr() as *mut jni::sys::_jobject;
        assert!(!ptr.is_null(), "Activity ptr is null");
        JObject::from_raw(ptr)
    };

    let method_name = match method {
        JavaMethod::StopCapture => "stopCapture",
        JavaMethod::ScanQr => "scanQr",
    };

    match vm.get_env() {
        Ok(mut env) => match env.call_method(activity, method_name, "()V", &[]) {
            Ok(_) => (),
            Err(err) => error!(?err, ?method, "Failed to call java method"),
        },
        Err(err) => error!(?err, "Failed to get env from VM"),
    }
}

struct Application {
    ui_weak: slint::Weak<MainWindow>,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    devices: HashMap<String, DeviceInfo>,
    cast_ctx: CastContext,
    active_device: Option<Arc<dyn device::CastingDevice>>,
    current_device_id: usize,
    local_address: Option<fcast_sender_sdk::IpAddr>,
    android_app: slint::android::AndroidApp,
    tx_sink: Option<WhepSink>,
    our_source_url: Option<String>,
}

impl Application {
    pub async fn new(
        ui_weak: slint::Weak<MainWindow>,
        event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
        android_app: slint::android::AndroidApp,
    ) -> Result<Self> {
        std::thread::spawn({
            let event_tx = event_tx.clone();
            move || loop {
                match GLOB_EVENT_CHAN.1.recv() {
                    Ok(event) => {
                        if let Err(err) = event_tx.send(event) {
                            error!("Failed to forward event to event loop: {err}");
                            break;
                        }
                    }
                    Err(err) => {
                        error!("Failed to receive event from the global event channel: {err}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            ui_weak,
            event_tx,
            devices: HashMap::new(),
            cast_ctx: CastContext::new()?,
            active_device: None,
            current_device_id: 0,
            local_address: None,
            android_app,
            tx_sink: None,
            our_source_url: None,
        })
    }

    fn update_receivers_in_ui(&mut self) -> Result<()> {
        let receivers = self
            .devices
            .iter()
            .filter(|(_, info)| !info.addresses.is_empty() && info.port != 0)
            .map(|(name, _)| slint::SharedString::from(name))
            .collect::<Vec<slint::SharedString>>();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let model = std::rc::Rc::new(slint::VecModel::<slint::SharedString>::from_iter(
                receivers.into_iter(),
            ));
            ui.global::<Bridge>().set_devices(model.into());
        })?;

        Ok(())
    }

    fn add_or_update_device(&mut self, device_info: DeviceInfo) -> Result<()> {
        self.devices.insert(device_info.name.clone(), device_info);
        self.update_receivers_in_ui()?;
        Ok(())
    }

    async fn stop_cast(&mut self, stop_playback: bool) -> Result<()> {
        let android_app = self.android_app.clone();
        self.ui_weak.upgrade_in_event_loop(move |_| {
            call_java_method_no_args(&android_app, JavaMethod::StopCapture);
        })?;

        if let Some(active_device) = self.active_device.take() {
            tokio::spawn(async move {
                if stop_playback {
                    debug!("Stopping playback");
                    log_err!(active_device.stop_playback(), "Failed to stop playback");
                    // NOTE: Instead of waiting for the PlaybackState::Idle event in the main loop we just sleep here
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                debug!("Disconnecting from active device");
                log_err!(
                    active_device.disconnect(),
                    "Failed to disconnect from active device"
                );
            });
        }

        if let Some(mut tx_sink) = self.tx_sink.take() {
            tx_sink.shutdown();
        }

        Ok(())
    }

    fn connect_with_device_info(&mut self, device_info: DeviceInfo) -> Result<()> {
        let device = self.cast_ctx.create_device_from_info(device_info);
        self.current_device_id += 1;
        device
            .connect(
                None,
                Arc::new(mcore::DeviceHandler::new(
                    self.current_device_id,
                    self.event_tx.clone(),
                )),
                1000,
            )
            .unwrap();
        self.active_device = Some(device);
        self.ui_weak.upgrade_in_event_loop(|ui| {
            ui.global::<Bridge>()
                .invoke_change_state(AppState::Connecting);
        })?;

        Ok(())
    }

    /// Returns `true` if the event loop should quit
    async fn handle_event(&mut self, event: Event) -> Result<ShouldQuit> {
        debug!("Handling event: {event:?}");

        match event {
            Event::EndSession { .. } => {
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(AppState::Disconnected);
                })?;

                self.stop_cast(true).await?;
            }
            Event::ConnectToDevice(device_name) => {
                if let Some(device_info) = self.devices.get(&device_name) {
                    self.connect_with_device_info(device_info.clone())?;
                } else {
                    error!("No device with name `{device_name}` found");
                }
            }
            Event::SignallerStarted { bound_port_v4, bound_port_v6 } => {
                let Some(addr) = self.local_address.as_ref() else {
                    error!("Local address is missing");
                    return Ok(ShouldQuit::No);
                };
                let bound_port = match addr {
                    fcast_sender_sdk::IpAddr::V4 { .. } => bound_port_v4,
                    fcast_sender_sdk::IpAddr::V6 { .. } => bound_port_v6,
                };

                let (content_type, url) = self
                    .tx_sink
                    .as_ref()
                    .unwrap()
                    .get_play_msg(addr.into(), bound_port);

                debug!(content_type, url, "Sending play message");
                self.our_source_url = Some(url.clone());

                match self.active_device.as_ref() {
                    Some(device) => {
                        device.load(device::LoadRequest::Url {
                            content_type,
                            url,
                            resume_position: None,
                            speed: None,
                            volume: None,
                            metadata: None,
                            request_headers: None,
                        })?;
                    }
                    None => error!("Active device is missing, cannot send play message"),
                }

                // self.ui_weak.upgrade_in_event_loop(|ui| {
                //     ui.global::<Bridge>().invoke_change_state(AppState::Casting);
                // })?;
            }
            Event::Quit => return Ok(ShouldQuit::Yes),
            Event::DeviceAvailable(device_info) => self.add_or_update_device(device_info)?,
            Event::DeviceRemoved(device_name) => {
                if self.devices.remove(&device_name).is_some() {
                    self.update_receivers_in_ui()?;
                } else {
                    debug!(device_name, "Tried to remove device but it was not found");
                }
            }
            Event::DeviceChanged(device_info) => self.add_or_update_device(device_info)?,
            Event::FromDevice { id, event } => {
                if id != self.current_device_id {
                    debug!(
                        "Got message from old device (id: {id} current: {})",
                        self.current_device_id
                    );
                } else {
                    match event {
                        DeviceEvent::StateChanged(device_connection_state) => {
                            match device_connection_state {
                                device::DeviceConnectionState::Connected { local_addr, .. } => {
                                    self.local_address = Some(local_addr);

                                    self.ui_weak.upgrade_in_event_loop(|ui| {
                                        ui.global::<Bridge>()
                                            .invoke_change_state(AppState::SelectingSettings);
                                    })?;
                                }
                                _ => (),
                            }
                        }
                        DeviceEvent::SourceChanged(new_source) => {
                            if self.tx_sink.is_some() {
                                match new_source {
                                    fcast_sender_sdk::device::Source::Url { ref url, .. } => {
                                        if Some(url) != self.our_source_url.as_ref() {
                                            // At this point the receiver has stopped playing our stream
                                            debug!(
                                                ?new_source,
                                                "The source on the receiver changed, disconnecting"
                                            );
                                            self.stop_cast(false).await?;
                                        }
                                    }
                                    _ => (),
                                }
                            }
                        }
                    }
                }
            }
            Event::CaptureStopped => (),
            Event::CaptureCancelled => {
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(AppState::Disconnected);
                })?;

                self.stop_cast(false).await?;
            }
            Event::QrScanResult(result) => {
                match fcast_sender_sdk::device::device_info_from_url(result) {
                    Some(device_info) => {
                        self.connect_with_device_info(device_info)?;
                    }
                    None => {
                        error!("QR code scan result is not a valid device");
                    }
                }
            }
            Event::CaptureStarted => {
                let appsrc = gst_app::AppSrc::builder()
                    .caps(
                        &gst_video::VideoCapsBuilder::new()
                            .format(gst_video::VideoFormat::I420)
                            // .framerate(gst::Fraction::new(0, 1))
                            .build(),
                    )
                    .is_live(true)
                    .do_timestamp(true)
                    .format(gst::Format::Time)
                    .max_buffers(1)
                    .build();

                let mut caps = None::<gst::Caps>;
                appsrc.set_callbacks(
                    gst_app::AppSrcCallbacks::builder()
                        .need_data(move |appsrc, _| {
                            let frame = {
                                let (lock, cvar) = &*FRAME_PAIR;
                                let mut frame = lock.lock();
                                while (*frame).is_none() {
                                    cvar.wait(&mut frame);
                                }

                                (*frame).take().unwrap()
                            };

                            use gst_video::prelude::*;

                            let now_caps = gst_video::VideoInfo::builder(
                                frame.format(),
                                frame.width(),
                                frame.height(),
                            )
                            .build()
                            .unwrap()
                            .to_caps()
                            .unwrap();

                            match &caps {
                                Some(old_caps) => {
                                    if *old_caps != now_caps {
                                        appsrc.set_caps(Some(&now_caps));
                                        caps = Some(now_caps);
                                    }
                                }
                                None => {
                                    appsrc.set_caps(Some(&now_caps));
                                    caps = Some(now_caps);
                                }
                            }

                            let _ = appsrc.push_buffer(frame.into_buffer());
                        })
                        .build(),
                );

                let source_config = SourceConfig::Video(mcore::VideoSource::Source(appsrc));

                self.tx_sink = Some(mcore::transmission::WhepSink::new(
                    source_config,
                    self.event_tx.clone(),
                    tokio::runtime::Handle::current(),
                    1920,
                    1080,
                    30,
                )?);

                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>().invoke_change_state(AppState::Casting);
                })?;
            }
            Event::StartCast {
                scale_width,
                scale_height,
                max_framerate,
            } => {
                let android_app = self.android_app.clone();
                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    let vm = unsafe {
                        let ptr = android_app.vm_as_ptr() as *mut jni::sys::JavaVM;
                        assert!(!ptr.is_null(), "JavaVM ptr is null");
                        JavaVM::from_raw(ptr).unwrap()
                    };
                    let activity = unsafe {
                        let ptr = android_app.activity_as_ptr() as *mut jni::sys::_jobject;
                        assert!(!ptr.is_null(), "Activity ptr is null");
                        JObject::from_raw(ptr)
                    };

                    let scale_width = scale_width as jni::sys::jint;
                    let scale_height = scale_height as jni::sys::jint;
                    let max_framerate = max_framerate as jni::sys::jint;

                    match vm.get_env() {
                        Ok(mut env) => match env.call_method(
                            activity,
                            "startScreenCapture",
                            "(III)V",
                            &[
                                scale_width.into(),
                                scale_height.into(),
                                max_framerate.into(),
                            ],
                        ) {
                            Ok(_) => (),
                            Err(err) => error!(
                                ?err,
                                method = "startScreenCapture",
                                "Failed to call java method"
                            ),
                        },
                        Err(err) => error!(?err, "Failed to get env from VM"),
                    }

                    ui.global::<Bridge>()
                        .invoke_change_state(AppState::WaitingForMedia);
                })?;
            }
        }

        Ok(ShouldQuit::No)
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    ) -> Result<()> {
        tracing_gstreamer::integrate_events();
        gst::log::remove_default_log_function();
        gst::log::set_default_threshold(gst::DebugLevel::Fixme);
        gst::init().unwrap();
        debug!("GStreamer version: {:?}", gst::version());

        // self.add_or_update_device(fcast_sender_sdk::device::DeviceInfo::fcast("Localhost for android emulator".to_owned(), vec![fcast_sender_sdk::IpAddr::v4(10, 0, 2, 2)], 46899))?;

        loop {
            let Some(event) = event_rx.recv().await else {
                debug!("No more events");
                break;
            };

            if self.handle_event(event).await? == ShouldQuit::Yes {
                break;
            }
        }

        debug!("Quitting event loop");

        Ok(())
    }
}

// TODO: handle errs
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    let app_clone = app.clone();

    slint::android::init(app).unwrap();

    let ui = MainWindow::new().unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

    ui.global::<Bridge>().on_connect_receiver({
        let event_tx = event_tx.clone();
        move |device_name| {
            event_tx
                .send(Event::ConnectToDevice(device_name.to_string()))
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_start_casting({
        let event_tx = event_tx.clone();
        move |scale_width: i32, scale_height: i32, max_framerate: i32| {
            event_tx
                .send(Event::StartCast {
                    scale_width: scale_width as u32,
                    scale_height: scale_height as u32,
                    max_framerate: max_framerate as u32,
                })
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_stop_casting({
        let event_tx = event_tx.clone();
        move || {
            event_tx
                .send(Event::EndSession { disconnect: true })
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_scan_qr({
        let android_app = app_clone.clone();
        move || {
            call_java_method_no_args(&android_app, JavaMethod::ScanQr);
        }
    });

    let ui_weak = ui.as_weak();

    let event_tx_clone = event_tx.clone();
    let app_jh = runtime.spawn(async move {
        Application::new(ui_weak, event_tx_clone, app_clone)
            .await
            .unwrap()
            .run_event_loop(event_rx)
            .await
            .unwrap();
    });

    ui.run().unwrap();

    runtime.spawn(async move {
        event_tx.send(Event::Quit).unwrap();
        app_jh.await.unwrap();
    });

    debug!("Finished");
}

fn jstring_to_string<'local>(env: &mut jni::JNIEnv<'local>, s: &JString<'local>) -> Result<String> {
    Ok(env.get_string(s)?.to_string_lossy().to_string())
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_FCastDiscoveryListener_serviceFound<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    name: JString<'local>,
    addrs: jni::objects::JObject,
    port: jni::sys::jint,
) {
    let name = match jstring_to_string(&mut env, &name) {
        Ok(name) => name,
        Err(err) => {
            error!(?err, "Failed to convert jstring to string");
            return;
        }
    };
    let port = port as u16;
    let addrs = match jni::objects::JList::from_env(&mut env, &addrs) {
        Ok(addrs) => addrs,
        Err(err) => {
            error!(?err, "Failed to get address list from env");
            return;
        }
    };
    let mut ip_addrs = Vec::<fcast_sender_sdk::IpAddr>::new();
    let n_addrs = match addrs.size(&mut env) {
        Ok(n) => n,
        Err(err) => {
            error!(?err, "Failed to get JList size");
            return;
        }
    };
    for i in 0..n_addrs {
        let Ok(Some(addr)) = addrs.get(&mut env, i) else {
            continue;
        };
        let buffer = unsafe { JByteBuffer::from_raw(*addr) };

        let buffer_cap = match env.get_direct_buffer_capacity(&buffer) {
            Ok(cap) => cap,
            Err(err) => {
                error!(?err, "Failed to get capacity of the byte buffer");
                continue;
            }
        };

        debug!(buffer_cap);

        let buffer_ptr = match env.get_direct_buffer_address(&buffer) {
            Ok(ptr) => {
                assert!(!ptr.is_null());
                ptr
            }
            Err(err) => {
                error!(?err, "Failed to get buffer address");
                continue;
            }
        };

        let buffer_slice: &[u8] = unsafe { std::slice::from_raw_parts(buffer_ptr, buffer_cap) };

        ip_addrs.push(match buffer_slice.len() {
            4 => fcast_sender_sdk::IpAddr::v4(
                buffer_slice[0],
                buffer_slice[1],
                buffer_slice[2],
                buffer_slice[3],
            ),
            20 => {
                let mut addr_slice = [0; 16];
                for i in 0..addr_slice.len() {
                    addr_slice[i] = buffer_slice[i];
                }
                let addr = Ipv6Addr::from(addr_slice);
                let scope_id_slice = &buffer_slice[16..20];
                let this_scope_id = i32::from_le_bytes([
                    scope_id_slice[0],
                    scope_id_slice[1],
                    scope_id_slice[2],
                    scope_id_slice[3],
                ]) as u32;
                let mut ip = fcast_sender_sdk::IpAddr::from(std::net::IpAddr::V6(addr));
                match &mut ip {
                    fcast_sender_sdk::IpAddr::V6 { scope_id, .. } => *scope_id = this_scope_id,
                    _ => (),
                }
                ip
            }
            len => {
                error!(len, "Invalid address buffer length");
                continue;
            }
        });
    }

    let device_info = fcast_sender_sdk::device::DeviceInfo::fcast(name, ip_addrs, port);
    debug!(?device_info, "Found device");

    log_err!(
        GLOB_EVENT_CHAN.0.send(Event::DeviceAvailable(device_info)),
        "Failed to send device available event"
    );
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_FCastDiscoveryListener_serviceLost<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    name: jni::objects::JString<'local>,
) {
    match jstring_to_string(&mut env, &name) {
        Ok(name) => log_err!(
            GLOB_EVENT_CHAN.0.send(Event::DeviceRemoved(name)),
            "Failed to send device removed event"
        ),
        Err(err) => error!(?err, "Failed to convert jstring to string"),
    }
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_MainActivity_nativeCaptureStarted<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
) {
    debug!("Screen capture was started");
    log_err!(
        GLOB_EVENT_CHAN.0.send(Event::CaptureStarted),
        "Failed to send capture started event"
    );
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_MainActivity_nativeCaptureStopped<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
) {
    debug!("Screen capture was stopped");
    log_err!(
        GLOB_EVENT_CHAN.0.send(Event::CaptureStopped),
        "Failed to send capture stopped event"
    );
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_MainActivity_nativeCaptureCancelled<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
) {
    debug!("Screen capture was cancelled");
    log_err!(
        GLOB_EVENT_CHAN.0.send(Event::CaptureCancelled),
        "Failed to send capture cancelled event"
    );
}

fn process_frame<'local>(
    env: jni::JNIEnv<'local>,
    width: jni::sys::jint,
    height: jni::sys::jint,
    buffer_y: JByteBuffer<'local>,
    buffer_u: JByteBuffer<'local>,
    buffer_v: JByteBuffer<'local>,
) -> Result<()> {
    let width = width as usize;
    let height = height as usize;

    fn buffer_as_slice<'local>(
        env: &jni::JNIEnv<'local>,
        buffer: &JByteBuffer<'local>,
        size: usize,
    ) -> Result<&'local [u8]> {
        let buffer_cap = match env.get_direct_buffer_capacity(&buffer) {
            Ok(cap) => cap,
            Err(err) => {
                bail!("Failed to get capacity of the byte buffer: {err}");
            }
        };

        if buffer_cap < size {
            bail!("buffer_cap < size: {buffer_cap} < {size}");
        }

        let buffer_ptr = match env.get_direct_buffer_address(&buffer) {
            Ok(ptr) => {
                assert!(!ptr.is_null());
                ptr
            }
            Err(err) => {
                bail!("Failed to get buffer address: {err}");
            }
        };

        unsafe { Ok(std::slice::from_raw_parts(buffer_ptr, buffer_cap)) }
    }

    let slice_y = buffer_as_slice(&env, &buffer_y, width * height)?;
    let slice_u = buffer_as_slice(&env, &buffer_u, (width / 2) * (height / 2))?;
    let slice_v = buffer_as_slice(&env, &buffer_v, (width / 2) * (height / 2))?;

    let info = match gst_video::VideoInfo::builder(
        gst_video::VideoFormat::I420,
        width as u32,
        height as u32,
    )
    .colorimetry(&VideoColorimetry::new(
        gst_video::VideoColorRange::Range0_255,
        gst_video::VideoColorMatrix::Bt709,
        gst_video::VideoTransferFunction::Bt709,
        gst_video::VideoColorPrimaries::Bt709,
    ))
    .build()
    {
        Ok(info) => info,
        Err(err) => {
            bail!("Failed to crate video info: {err}");
        }
    };

    let new_caps = match info.to_caps() {
        Ok(caps) => caps,
        Err(err) => {
            bail!("Failed to create caps from video info: {err}");
        }
    };

    fn init_frame_pool(
        pool: &gst_video::VideoBufferPool,
        mut old_config: gst::BufferPoolConfig,
        new_caps: &gst::Caps,
        frame_size: u32,
    ) -> Result<()> {
        pool.set_config({
            old_config.set_params(Some(&new_caps), frame_size, 1, 30);
            old_config
        })?;
        pool.set_active(true)?;
        Ok(())
    }

    let mut frame_pool = FRAME_POOL.lock();
    let old_config = frame_pool.config();
    let frame_size = width * height + 2 * ((width / 2) * (height / 2));
    if !frame_pool.is_active() {
        init_frame_pool(&frame_pool, old_config, &new_caps, frame_size as u32)?;
    } else {
        let _ = frame_pool.set_active(false);
        let new_frame_pool = gst_video::VideoBufferPool::new();
        init_frame_pool(&new_frame_pool, old_config, &new_caps, frame_size as u32)?;
        *frame_pool = new_frame_pool;
    }

    let buffer = match frame_pool.acquire_buffer(None) {
        Ok(buffer) => buffer,
        Err(err) => {
            bail!("Failed to acquire buffer from pool: {err}");
        }
    };
    let Ok(mut vframe) = gst_video::VideoFrame::from_buffer_writable(buffer, &info) else {
        bail!("Failed to crate VideoFrame from buffer");
    };

    fn copy(
        vframe: &mut gst_video::VideoFrame<gst_video::video_frame::Writable>,
        plane_idx: u32,
        src_plane: &[u8],
    ) -> Result<()> {
        let dest_y_stride = *vframe
            .plane_stride()
            .get(plane_idx as usize)
            .ok_or(anyhow::anyhow!("Could not get plane stride"))?
            as usize;
        let dest_y = vframe.plane_data_mut(plane_idx)?;
        for (dest, src) in dest_y
            .chunks_exact_mut(dest_y_stride)
            .zip(src_plane.chunks_exact(dest_y_stride))
        {
            dest[..dest_y_stride].copy_from_slice(&src[..dest_y_stride]);
        }

        Ok(())
    }

    copy(&mut vframe, 0, slice_y)?;
    copy(&mut vframe, 1, slice_u)?;
    copy(&mut vframe, 2, slice_v)?;

    let (lock, cvar) = &*FRAME_PAIR;
    let mut frame = lock.lock();
    *frame = Some(vframe);
    cvar.notify_one();

    Ok(())
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_MainActivity_nativeProcessFrame<'local>(
    env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    width: jni::sys::jint,
    height: jni::sys::jint,
    buffer_y: JByteBuffer<'local>,
    buffer_u: JByteBuffer<'local>,
    buffer_v: JByteBuffer<'local>,
) {
    if let Err(err) = process_frame(env, width, height, buffer_y, buffer_u, buffer_v) {
        error!(?err, "Failed to process frame");
    }
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_android_sender_MainActivity_nativeQrScanResult<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    result: jni::objects::JString<'local>,
) {
    match jstring_to_string(&mut env, &result) {
        Ok(result) => log_err!(
            GLOB_EVENT_CHAN.0.send(Event::QrScanResult(result)),
            "Failed to send device removed event"
        ),
        Err(err) => error!(?err, "Failed to convert jstring to string"),
    }
}
