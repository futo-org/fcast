use jni::objects::JByteBuffer;
use parking_lot::Mutex;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::LazyLock,
};

use rcore::{MdnsEvent, slint, tracing::error};

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

static EVENT_TX: LazyLock<Mutex<Option<UnboundedSender<rcore::Event>>>> =
    LazyLock::new(|| Mutex::new(None));

#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    log_panics::init();

    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    slint::android::init(app.clone()).unwrap();

    rcore::slint::BackendSelector::new()
        // .require_opengl_es()
        // .backend_name("skia".to_owned())
        .select()
        .unwrap();

    let (event_tx, event_rx) = unbounded_channel();
    *EVENT_TX.lock() = Some(event_tx);

    rcore::run(app, event_rx).unwrap();
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_org_fcast_rsreceiver_android_MainActivity_setMdnsDeviceName<'local>(
    mut env: jni::JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    name: jni::objects::JString,
) {
    let event_tx = EVENT_TX.lock();
    let Some(event_tx) = event_tx.as_ref() else {
        // Unreachable
        return;
    };

    let Ok(device_name) = env.get_string(&name) else {
        return;
    };

    let event = rcore::MdnsEvent::NameSet(device_name.to_string_lossy().to_string());
    let _ = event_tx.send(rcore::Event::Mdns(event));
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
    let event_tx = EVENT_TX.lock();
    let Some(event_tx) = event_tx.as_ref() else {
        // Unreachable
        return;
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
            MdnsEvent::IpAdded(addr)
        } else {
            MdnsEvent::IpRemoved(addr)
        };

        if let Err(err) = event_tx.send(rcore::Event::Mdns(event)) {
            error!(?err, "Failed to send mDNS event");
            return;
        }
    }
}
