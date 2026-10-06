use jni::objects::JByteBuffer;
use parking_lot::Mutex;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::LazyLock,
};

use rcore::{
    message::Mdns,
    slint,
    tracing::{error, warn},
};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

// The activity's JNI up-calls (network callback, NSD name) start firing
// before android_main runs, so the sender must exist from the first touch
// and early events wait in the channel until rcore::run drains them.
#[allow(clippy::type_complexity)]
static EVENT_CHANNEL: LazyLock<(
    UnboundedSender<rcore::message::Message>,
    Mutex<Option<UnboundedReceiver<rcore::message::Message>>>,
)> = LazyLock::new(|| {
    let (tx, rx) = unbounded_channel();
    (tx, Mutex::new(Some(rx)))
});

/// Once per process, whichever comes first of the core start and a UI.
fn init_logging(settings: &rcore::Settings) {
    log_panics::init();
    // Info unless configured: every debug! line is a logcat write.
    let level = match settings.log_level() {
        Some(level) => log_filter(level),
        None => log::LevelFilter::Info,
    };
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(level)
            .with_filter(
                android_logger::FilterBuilder::new()
                    .filter_level(level)
                    .filter_module("tracing_gstreamer::callsite", log::LevelFilter::Off)
                    .build(),
            ),
    );
}

/// dodvg renders on wgpu, Vulkan unless the GL backend is asked for by name.
/// A box without a Vulkan driver (many GLES 3 ones) gets GL, or the first
/// frame fails and the app closes on launch. Requiring GLES itself is refused
/// by the android backend, only the wgpu API may be named.
fn select_renderer() {
    let selector = rcore::slint::BackendSelector::new();
    let selector = if !gles_forced() && has_vulkan_adapter() {
        selector
    } else {
        warn!("no Vulkan adapter, rendering with GLES");
        let mut settings = slint::wgpu_30::WGPUSettings::default();
        settings.backends = wgpu::Backends::GL;
        selector.require_wgpu_30(slint::wgpu_30::WGPUConfiguration::Automatic(settings))
    };
    selector.select().unwrap();
}

/// `adb shell setprop debug.fcast.renderer gles` takes the GLES path on a
/// device with Vulkan, to test the fallback the boxes without it get.
fn gles_forced() -> bool {
    let mut value = [0u8; 92];
    let len = unsafe {
        libc::__system_property_get(c"debug.fcast.renderer".as_ptr(), value.as_mut_ptr().cast())
    };
    len > 0 && value.get(..len as usize) == Some(b"gles".as_slice())
}

fn has_vulkan_adapter() -> bool {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let mut enumerate = std::pin::pin!(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match enumerate.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(adapters) => adapters
            .iter()
            .any(|a| a.get_info().device_type != wgpu::DeviceType::Cpu),
        // native enumeration is synchronous, keep the default if that changes
        std::task::Poll::Pending => true,
    }
}

#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    // The activity's files dir, where the drawer's settings persist.
    let settings = rcore::Settings::load(app.internal_data_path().as_deref());
    init_logging(&settings);

    slint::android::init(app.clone()).unwrap();

    select_renderer();

    // Only the first activity in a process starts the core, later ones
    // attach a new UI to it (each android_main gets a fresh thread, so slint's
    // thread-local platform is fresh too).
    let event_rx = EVENT_CHANNEL.1.lock().take();

    let keep_core = match rcore::run(app, event_rx, settings) {
        Ok(keep) => keep,
        Err(err) => {
            error!(?err, "receiver UI failed");
            false
        }
    };
    // With no service to keep it, the process ends with its activity, as
    // before: the next launch gets an honest cold start under the splash.
    if !keep_core {
        // a running sticky service would bring the process straight back
        rcore::android_jni::call("stopServiceForExit", "()V", &[]);
        end_process();
    }
}

/// ReceiverCore's VM and class for the core's calls into Java, from a Java
/// thread whose class loader sees the app's classes.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeCoreInit<'local>(
    mut env: jni::JNIEnv<'local>,
    class: jni::objects::JClass<'local>,
    app: jni::objects::JObject<'local>,
) {
    if let Err(err) = rcore::android_jni::init(&mut env, &class) {
        let _ = env.exception_clear();
        error!(?err, "ReceiverCore init failed, the core cannot call into Java");
    }
    // The core (gst's androidmedia, among others) needs the JavaVM and a
    // context before any activity exists. android-activity sets the same
    // pair again later, which the forked ndk-context accepts.
    let (Ok(vm), Ok(app)) = (env.get_java_vm(), env.new_global_ref(&app)) else {
        let _ = env.exception_clear();
        error!("no JavaVM or app context for ndk-context");
        return;
    };
    let raw = app.as_obj().as_raw();
    // the global ref stays for the life of the process, as android-activity's
    std::mem::forget(app);
    unsafe { ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), raw.cast()) };
}

/// Starts the receiver core without a UI, from ReceiverCore on the Java main
/// thread: before the first activity's native thread, or from the service
/// alone after a boot.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeCoreStart<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString<'local>,
) {
    let Ok(files_dir) = env.get_string(&files_dir) else {
        let _ = env.exception_clear();
        return;
    };
    let files_dir = std::path::PathBuf::from(files_dir.to_string_lossy().into_owned());
    let settings = rcore::Settings::load(Some(files_dir.as_path()));
    init_logging(&settings);
    if let Some(event_rx) = EVENT_CHANNEL.1.lock().take() {
        rcore::android_start_core(event_rx, settings);
    }
}

/// The start-on-boot answer: 1 on, 0 off, -1 never asked.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeStartOnBoot<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString<'local>,
) -> jni::sys::jint {
    let Ok(files_dir) = env.get_string(&files_dir) else {
        let _ = env.exception_clear();
        return -1;
    };
    let files_dir = std::path::PathBuf::from(files_dir.to_string_lossy().into_owned());
    match rcore::android_start_on_boot(&files_dir) {
        Some(true) => 1,
        Some(false) => 0,
        None => -1,
    }
}

/// The first-launch answer. Saved and applied by the core, as a drawer edit.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeSetStartOnBoot<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    on: jni::sys::jboolean,
) {
    let _ = EVENT_CHANNEL.0.send(rcore::message::Message::SetConfigBool {
        key: "interface.start_on_boot".to_owned(),
        value: on != 0,
    });
}

/// The last owner (the service) left with no activity: end the receiver.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeShutdown<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
) {
    rcore::android_shutdown();
    end_process();
}

/// `_exit`, not `exit`: the quit waits a bounded 2 s for a teardown that can
/// take longer, and `exit`'s static destructors would run under MediaCodec
/// threads still releasing, a crash on the way out.
fn end_process() -> ! {
    unsafe { libc::_exit(0) }
}

fn log_filter(level: rcore::tracing::level_filters::LevelFilter) -> log::LevelFilter {
    use rcore::tracing::Level;
    match level.into_level() {
        None => log::LevelFilter::Off,
        Some(Level::ERROR) => log::LevelFilter::Error,
        Some(Level::WARN) => log::LevelFilter::Warn,
        Some(Level::INFO) => log::LevelFilter::Info,
        Some(Level::DEBUG) => log::LevelFilter::Debug,
        Some(Level::TRACE) => log::LevelFilter::Trace,
    }
}

/// The FCast name to register as `{fcast}`, a null element while FCast is
/// disabled and a null array when the call failed. Read from the config
/// before the receiver is up.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeServiceNames<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    files_dir: jni::objects::JString,
    hostname: jni::objects::JString,
) -> jni::sys::jobjectArray {
    // no unwraps in extern "C": a panic here aborts the process. One at a
    // time, a JNI call made with the first's exception pending is undefined.
    let Ok(files_dir) = env.get_string(&files_dir) else {
        return null_names(&env);
    };
    let Ok(hostname) = env.get_string(&hostname) else {
        return null_names(&env);
    };
    let files_dir = std::path::PathBuf::from(files_dir.to_string_lossy().into_owned());
    let fcast = rcore::android_fcast_name(&files_dir, &hostname.to_string_lossy());

    let Ok(names) = env.new_object_array(1, "java/lang/String", jni::objects::JObject::null())
    else {
        return null_names(&env);
    };
    if let Some(name) = fcast {
        let Ok(name) = env.new_string(name) else {
            return null_names(&env);
        };
        if env.set_object_array_element(&names, 0, name).is_err() {
            return null_names(&env);
        }
    }
    names.into_raw()
}

/// The failure return of `nativeServiceNames`. A failed JNI call leaves its
/// exception pending, which would throw out of onCreate instead of reaching
/// the activity's null fallback.
fn null_names(env: &jni::JNIEnv) -> jni::sys::jobjectArray {
    let _ = env.exception_clear();
    std::ptr::null_mut()
}

/// The fcast TXT records for the NSD registration. Returns false while the
/// receiver has not minted its TLS identity yet, the activity retries.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_getFCastTxtAttribs<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    attrs: jni::objects::JObject,
) -> jni::sys::jboolean {
    let Some(records) = rcore::fcast_txt_records() else {
        return 0;
    };
    let Ok(attrs) = env.get_map(&attrs) else {
        return 0;
    };
    for (k, v) in records {
        let (Ok(k), Ok(v)) = (env.new_string(k), env.new_string(v)) else {
            return 0;
        };
        if attrs.put(&mut env, &k, &v).is_err() {
            return 0;
        }
    }
    1
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_setMdnsDeviceName<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    name: jni::objects::JString,
) {
    let Ok(device_name) = env.get_string(&name) else {
        return;
    };

    let event = Mdns::NameSet(device_name.to_string_lossy().to_string());
    let _ = EVENT_CHANNEL.0.send(rcore::message::Message::Mdns(event));
}

/// Transport commands from the MediaSession and the notification Stop
/// action: 0 stop, 1 pause, 2 resume.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeMediaCommand<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    code: jni::sys::jint,
) {
    let op = match code {
        0 => rcore::fcast::Operation::Stop,
        1 => rcore::fcast::Operation::Pause,
        2 => rcore::fcast::Operation::Resume,
        other => {
            error!(other, "unknown media command code");
            return;
        }
    };
    let _ = EVENT_CHANNEL.0.send(rcore::message::Message::Op {
        origin: rcore::application::PacketOrigin::Gui,
        op,
    });
}

/// The soft keyboard came up or went away. The activity watches the window
/// insets animation for it; slint's backend consumes the same insets for
/// layout and tells the app nothing.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_nativeSoftKeyboardVisible<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    visible: jni::sys::jboolean,
) {
    let _ = EVENT_CHANNEL
        .0
        .send(rcore::message::Message::SoftKeyboardVisible(visible != 0));
}

/// Absolute seek from the session (lock screen scrubber, BT remote).
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeMediaSeek<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    seconds: jni::sys::jdouble,
) {
    if !seconds.is_finite() || seconds < 0.0 {
        return;
    }
    let _ = EVENT_CHANNEL.0.send(rcore::message::Message::Op {
        origin: rcore::application::PacketOrigin::Gui,
        op: rcore::fcast::Operation::Seek(rcore::gst::ClockTime::from_nseconds(
            (seconds * 1_000_000_000.0) as u64,
        )),
    });
}

/// Audio focus and routing events. Codes match MainActivity: 0 permanent
/// loss, 1 transient loss (ducking folded in), 2 gain, 3 becoming noisy.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeAudioEvent<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    code: jni::sys::jint,
) {
    use rcore::message::AndroidAudio;
    let event = match code {
        0 => AndroidAudio::Loss,
        1 => AndroidAudio::TransientLoss,
        2 => AndroidAudio::Gain,
        3 => AndroidAudio::BecomingNoisy,
        other => {
            error!(other, "unknown audio event code");
            return;
        }
    };
    let _ = EVENT_CHANNEL
        .0
        .send(rcore::message::Message::AndroidAudio(event));
}

/// Activity start/stop. The video surface dies with the activity, the
/// player must let go of it first and re-adopt a fresh one on return.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_nativeAppVisibility<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    visible: jni::sys::jboolean,
) {
    rcore::android_app_visibility(visible != 0);
    use rcore::message::AndroidWindow;
    let event = if visible != 0 {
        AndroidWindow::Shown
    } else {
        AndroidWindow::Hidden
    };
    let _ = EVENT_CHANNEL
        .0
        .send(rcore::message::Message::AndroidWindow(event));
}

/// An ended cast will send the task to the back. The core keeps the ended
/// item on screen until onStop instead of showing the idle screen on the way out.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeLeavingForEndedCast<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
) {
    let _ = EVENT_CHANNEL.0.send(rcore::message::Message::AndroidWindow(
        rcore::message::AndroidWindow::Leaving,
    ));
}

/// The committed fcast listen port, 0 while the listeners are not bound yet.
/// The activity polls this alongside the TXT records before advertising.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_getFCastPort<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
) -> jni::sys::jint {
    rcore::fcast_committed_port().map_or(0, i32::from)
}

/// The full current address set, replacing whatever was known. The activity
/// sweeps interfaces on every network change, so removals need no per-network
/// bookkeeping on either side.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_ReceiverCore_nativeSetAddresses<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    addrs: jni::objects::JObject,
) {
    let addrs = match jni::objects::JList::from_env(&mut env, &addrs) {
        Ok(addrs) => addrs,
        Err(err) => {
            error!(?err, "Failed to get address list from env");
            return;
        }
    };
    let n_addrs = match addrs.size(&mut env) {
        Ok(n) => n,
        Err(err) => {
            error!(?err, "Failed to get JList size");
            return;
        }
    };
    let mut ips = Vec::with_capacity(n_addrs as usize);
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

        let buffer_ptr = match env.get_direct_buffer_address(&buffer) {
            Ok(ptr) if ptr.is_null() => {
                error!("Null address for the byte buffer");
                continue;
            }
            Ok(ptr) => ptr,
            Err(err) => {
                error!(?err, "Failed to get buffer address");
                continue;
            }
        };

        let buffer_slice: &[u8] = unsafe { std::slice::from_raw_parts(buffer_ptr, buffer_cap) };

        let addr = match buffer_slice.len() {
            4 => {
                let mut addr_slice = [0; 4];
                for i in 0..addr_slice.len() {
                    addr_slice[i] = buffer_slice[i];
                }
                IpAddr::V4(Ipv4Addr::from_octets(addr_slice))
            }
            16 => {
                let mut addr_slice = [0; 16];
                for i in 0..addr_slice.len() {
                    addr_slice[i] = buffer_slice[i];
                }
                IpAddr::V6(Ipv6Addr::from(addr_slice))
            }
            len => {
                error!(len, "Invalid address buffer length");
                continue;
            }
        };
        ips.push(addr);
    }

    if let Err(err) = EVENT_CHANNEL
        .0
        .send(rcore::message::Message::Mdns(Mdns::SetIps(ips)))
    {
        error!(?err, "Failed to send mDNS event");
    }
}

/// PackageInstaller refused the update, or the user cancelled it.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_Updater_nativeInstallFailed<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    message: jni::objects::JString<'local>,
) {
    let message = env
        .get_string(&message)
        .map(String::from)
        .unwrap_or_default();
    let _ = EVENT_CHANNEL
        .0
        .send(rcore::message::Message::AppUpdate(
            rcore::message::AppUpdate::InstallFailed(message),
        ));
}

/// The overlay grant, pushed by the activity on every window focus gain.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_nativeOverlayState<'local>(
    _env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    granted: jni::sys::jboolean,
    can_open_settings: jni::sys::jboolean,
) {
    rcore::android_overlay_state(granted != 0, can_open_settings != 0);
}
