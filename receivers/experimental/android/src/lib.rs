use jni::objects::JByteBuffer;
use parking_lot::Mutex;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::LazyLock,
};

use rcore::{message::Mdns, slint, tracing::error};

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

#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    log_panics::init();

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_filter(
                android_logger::FilterBuilder::new()
                    .filter_level(log::LevelFilter::Debug)
                    .filter_module("tracing_gstreamer::callsite", log::LevelFilter::Off)
                    .build(),
            ),
    );

    slint::android::init(app.clone()).unwrap();

    // No graphics API requirement. This asked for OpenGL ES, which was right
    // while the android backend rendered with skia, but it now renders with
    // dodvg on wgpu 30 and that lane is Vulkan. The request is carried all the
    // way to surface creation, where anything other than the wgpu API is
    // refused outright ("does not implement renderer selection by graphics
    // API"), so requiring GLES here panicked the receiver on the first frame.
    rcore::slint::BackendSelector::new().select().unwrap();

    let event_rx = EVENT_CHANNEL.1.lock().take().expect("android_main ran twice");

    rcore::run(app, event_rx).unwrap();

    // The activity is gone (Destroy broke the event loop), but android keeps
    // the process around and serves the NEXT activity from it, which runs
    // android_main again. The receiver is a per-process singleton: the slint
    // platform is set once, gst is initialized once, and the event channel's
    // receiver was taken above, so a second run can only hit that expect and
    // die on a background thread with the new activity left blank. Exit
    // instead. The next launch gets a fresh process and an honest cold start,
    // which the splash window already covers.
    std::process::exit(0);
}

/// The fcast TXT records for the NSD registration. Returns false while the
/// receiver has not minted its TLS identity yet, the activity retries.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_getFCastTxtAttribs<'local>(
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
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_setMdnsDeviceName<'local>(
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

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_getDeviceNameRaopHash<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    name: jni::objects::JString,
) -> jni::sys::jstring {
    let name = env.get_string(&name).unwrap();
    let name = name.to_str().unwrap();
    let hash = rcore::device_name_hash(&name);
    let hash_str = rcore::hash_to_string(&hash);
    let _ = EVENT_CHANNEL.0.send(rcore::message::Message::Raop(
        rcore::message::Raop::ConfigAvailable(rcore::Configuration { hw_addr: hash }),
    ));

    env.new_string(hash_str).unwrap().into_raw()
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_getRaopTxtAttribs<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    attrs: jni::objects::JObject,
) {
    let attrs = env.get_map(&attrs).unwrap();
    for (k, v) in rcore::txt_properties() {
        let k = env.new_string(k).unwrap();
        let v = env.new_string(v).unwrap();
        attrs.put(&mut env, &k, &v).unwrap();
    }
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
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_nativeNetworkEvent<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    available: jni::sys::jboolean,
    addrs: jni::objects::JObject,
) {
    let available = available != 0;
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

        let event = if available {
            Mdns::IpAdded(addr)
        } else {
            Mdns::IpRemoved(addr)
        };

        if let Err(err) = EVENT_CHANNEL.0.send(rcore::message::Message::Mdns(event)) {
            error!(?err, "Failed to send mDNS event");
            return;
        }
    }
}
