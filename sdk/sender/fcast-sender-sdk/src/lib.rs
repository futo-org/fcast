//! # FCast Sender SDK
//!
//! An all in one SDK for casting media to [FCast], [Chromecast] and [Google
//! Cast] receiver devices.
//!
//! ## Supported languages
//!
//! + Rust
//! + Kotlin
//! + Swift
//!
//! ## Features
//!
//! + Automatic discovery of devices on the network via [mDNS]
//! + HTTP file server for easy casting of local media files
//!
//! ## Example usage
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use fcast_sender_sdk::context::CastContext;
//! use fcast_sender_sdk::device::{
//!     ApplicationInfo, DeviceConnectionState, DeviceEventHandler, DeviceInfo, KeyEvent,
//!     MediaEvent, LoadRequest, PlaybackState, ProtocolType, Source,
//! };
//! use fcast_sender_sdk::{DeviceDiscovererEventHandler, IpAddr};
//!
//! struct DevEventHandler {}
//!
//! impl DeviceEventHandler for DevEventHandler {
//!     fn connection_state_changed(&self, state: DeviceConnectionState) {
//!         println!("Connection state changed: {state:?}");
//!     }
//!
//!     fn volume_changed(&self, volume: f64) {
//!         println!("Volume changed: {volume}");
//!     }
//!
//!     fn time_changed(&self, time: f64) {
//!         println!("Time changed: {time}");
//!     }
//!
//!     fn playback_state_changed(&self, state: PlaybackState) {
//!         println!("Playback state changed: {state:?}");
//!     }
//!
//!     fn duration_changed(&self, duration: f64) {
//!         println!("Duration changed: {duration}");
//!     }
//!
//!     fn speed_changed(&self, speed: f64) {
//!         println!("Speed changed: {speed}");
//!     }
//!
//!     fn source_changed(&self, source: Source) {
//!         println!("Source changed: {source:?}");
//!     }
//!
//!     fn key_event(&self, event: KeyEvent) {
//!         println!("Key event: {event:?}");
//!     }
//!
//!     fn media_event(&self, event: MediaEvent) {
//!         println!("Media event: {event:?}");
//!     }
//!
//!     fn playback_error(&self, message: String) {
//!         println!("Playback error: {message}");
//!     }
//! }
//!
//! struct DiscovererEventHandler {}
//!
//! impl DeviceDiscovererEventHandler for DiscovererEventHandler {
//!     fn device_available(&self, device_info: DeviceInfo) {
//!         println!("Device available: {device_info:?}");
//!     }
//!
//!     fn device_removed(&self, device_name: String) {
//!         println!("Device removed: {device_name}");
//!     }
//!
//!     fn device_changed(&self, device_info: DeviceInfo) {
//!         println!("Device changed: {device_info:?}");
//!     }
//! }
//!
//! let ctx = CastContext::new().unwrap();
//!
//! ctx.start_discovery(Arc::new(DiscovererEventHandler {}));
//!
//! let dev = ctx.create_device_from_info(DeviceInfo {
//!     name: "FCast device".to_owned(),
//!     protocol: ProtocolType::FCast,
//!     addresses: vec![IpAddr::v4(127, 0, 0, 1)],
//!     port: 46899,
//! });
//!
//! dev.connect(None, Arc::new(DevEventHandler {}), 1000)
//!     .unwrap();
//!
//! dev.load(LoadRequest::Video {
//!     content_type: "video/mp4".to_string(),
//!     url: "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4"
//!         .to_string(),
//!     resume_position: 0.0,
//!     speed: None,
//!     volume: None,
//!     metadata: None,
//!     request_headers: None,
//! })
//! .unwrap();
//! ```
//!
//! [FCast]: https://fcast.org/
//! [Chromecast]: https://en.wikipedia.org/wiki/Chromecast
//! [Google Cast]: https://www.android.com/better-together/#cast
//! [mDNS]: https://en.wikipedia.org/wiki/Multicast_DNS

#[cfg(feature = "chromecast")]
pub mod chromecast;
#[cfg(any(feature = "http-file-server", any_protocol))]
pub mod context;
#[cfg(all(any_protocol, feature = "discovery"))]
pub mod discovery;
#[cfg(feature = "fcast")]
pub mod fcast;
#[cfg(feature = "chromecast")]
pub(crate) mod googlecast_protocol;
#[cfg(feature = "http-file-server")]
pub(crate) mod http;
pub(crate) mod utils;

#[cfg(feature = "http-file-server")]
pub mod file_server;

/// Event handler for device discovery.
#[cfg(all(any_protocol, feature = "discovery_types"))]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait DeviceDiscovererEventHandler: Send + Sync {
    /// Called when a device is found.
    fn device_available(&self, device_info: device::DeviceInfo);
    /// Called when a device is removed or lost.
    fn device_removed(&self, device_name: String);
    /// Called when a device has changed.
    ///
    /// The `name` field of `device_info` will correspond to a device announced
    /// from `device_available`.
    fn device_changed(&self, device_info: device::DeviceInfo);
}

#[cfg(all(feature = "discovery", any_protocol))]
use std::future::Future;

#[cfg(any(feature = "http-file-server", any_protocol))]
use tokio::runtime;
#[cfg(any_protocol)]
pub mod device;
#[cfg(any_protocol)]
use std::str::FromStr;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

#[cfg(any(feature = "discovery", feature = "http-file-server", any_protocol))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
#[derive(thiserror::Error, Debug)]
pub enum AsyncRuntimeError {
    #[error("failed to build")]
    FailedToBuild(#[from] std::io::Error),
}

#[cfg(any(feature = "http-file-server", any_protocol))]
pub(crate) enum AsyncRuntime {
    Handle(runtime::Handle),
    Runtime(runtime::Runtime),
}

#[cfg(any(feature = "http-file-server", any_protocol))]
impl AsyncRuntime {
    pub fn new(threads: Option<usize>, name: &str) -> Result<Self, AsyncRuntimeError> {
        Ok(match runtime::Handle::try_current() {
            Ok(handle) => Self::Handle(handle),
            Err(_) => Self::Runtime({
                if let Some(threads) = threads {
                    runtime::Builder::new_multi_thread()
                        .worker_threads(threads)
                        .enable_all()
                        .thread_name(name)
                        .build()?
                } else {
                    runtime::Builder::new_multi_thread()
                        .enable_all()
                        .thread_name(name)
                        .build()?
                }
            }),
        })
    }

    #[cfg(all(feature = "discovery", any_protocol))]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            AsyncRuntime::Handle(h) => h.spawn(future),
            AsyncRuntime::Runtime(rt) => rt.spawn(future),
        }
    }

    pub fn handle(&self) -> runtime::Handle {
        match self {
            AsyncRuntime::Handle(handle) => handle.clone(),
            AsyncRuntime::Runtime(runtime) => runtime.handle().clone(),
        }
    }
}

// UniFFI does not support std::net::IpAddr
#[cfg(any_protocol)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IpAddr {
    V4 {
        o1: u8,
        o2: u8,
        o3: u8,
        o4: u8,
    },
    V6 {
        // UniFFI will not accept [u8; 16] here...
        o1: u8,
        o2: u8,
        o3: u8,
        o4: u8,
        o5: u8,
        o6: u8,
        o7: u8,
        o8: u8,
        o9: u8,
        o10: u8,
        o11: u8,
        o12: u8,
        o13: u8,
        o14: u8,
        o15: u8,
        o16: u8,
        scope_id: u32,
    },
}

#[cfg(any_protocol)]
impl IpAddr {
    pub fn v4(o1: u8, o2: u8, o3: u8, o4: u8) -> Self {
        Self::V4 { o1, o2, o3, o4 }
    }
}

#[cfg(any_protocol)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
#[derive(thiserror::Error, Debug)]
pub enum ParseIpAddrError {
    #[error("failed to parse address")]
    FailedToParse(#[from] std::net::AddrParseError),
}

#[allow(dead_code)]
#[cfg(any_protocol)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
fn try_ip_addr_from_str(s: &str) -> Result<IpAddr, ParseIpAddrError> {
    Ok(IpAddr::from(&std::net::IpAddr::from_str(
        s.trim_matches(['[', ']']),
    )?))
}

#[allow(dead_code)]
#[cfg(any_protocol)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn url_format_ip_addr(addr: &IpAddr) -> String {
    match addr {
        IpAddr::V4 { o1, o2, o3, o4 } => format!("{o1}.{o2}.{o3}.{o4}"),
        IpAddr::V6 {
            o1,
            o2,
            o3,
            o4,
            o5,
            o6,
            o7,
            o8,
            o9,
            o10,
            o11,
            o12,
            o13,
            o14,
            o15,
            o16,
            ..
        } => {
            let addr = std::net::Ipv6Addr::from_bits(u128::from_be_bytes([
                *o1, *o2, *o3, *o4, *o5, *o6, *o7, *o8, *o9, *o10, *o11, *o12, *o13, *o14, *o15,
                *o16,
            ]));
            format!("[{addr}]")
        }
    }
}

#[allow(dead_code)]
#[cfg(any_protocol)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
fn octets_from_ip_addr(addr: &IpAddr) -> Vec<u8> {
    match addr {
        IpAddr::V4 { o1, o2, o3, o4 } => vec![*o1, *o2, *o3, *o4],
        IpAddr::V6 {
            o1,
            o2,
            o3,
            o4,
            o5,
            o6,
            o7,
            o8,
            o9,
            o10,
            o11,
            o12,
            o13,
            o14,
            o15,
            o16,
            ..
        } => {
            vec![
                *o1, *o2, *o3, *o4, *o5, *o6, *o7, *o8, *o9, *o10, *o11, *o12, *o13, *o14, *o15,
                *o16,
            ]
        }
    }
}

#[cfg(any_protocol)]
impl From<&std::net::IpAddr> for IpAddr {
    fn from(value: &std::net::IpAddr) -> Self {
        match value {
            std::net::IpAddr::V4(v4) => {
                let octets = v4.octets();
                Self::V4 {
                    o1: octets[0],
                    o2: octets[1],
                    o3: octets[2],
                    o4: octets[3],
                }
            }
            std::net::IpAddr::V6(v6) => {
                let octets = v6.octets();
                Self::V6 {
                    o1: octets[0],
                    o2: octets[1],
                    o3: octets[2],
                    o4: octets[3],
                    o5: octets[4],
                    o6: octets[5],
                    o7: octets[6],
                    o8: octets[7],
                    o9: octets[8],
                    o10: octets[9],
                    o11: octets[10],
                    o12: octets[11],
                    o13: octets[12],
                    o14: octets[13],
                    o15: octets[14],
                    o16: octets[15],
                    scope_id: 0,
                }
            }
        }
    }
}

#[cfg(any_protocol)]
impl From<&IpAddr> for std::net::IpAddr {
    fn from(value: &IpAddr) -> Self {
        match value {
            IpAddr::V4 { o1, o2, o3, o4 } => {
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(*o1, *o2, *o3, *o4))
            }
            IpAddr::V6 {
                o1,
                o2,
                o3,
                o4,
                o5,
                o6,
                o7,
                o8,
                o9,
                o10,
                o11,
                o12,
                o13,
                o14,
                o15,
                o16,
                ..
            } => std::net::IpAddr::V6(std::net::Ipv6Addr::from_bits(u128::from_be_bytes([
                *o1, *o2, *o3, *o4, *o5, *o6, *o7, *o8, *o9, *o10, *o11, *o12, *o13, *o14, *o15,
                *o16,
            ]))),
        }
    }
}

#[cfg(any_protocol)]
impl From<std::net::IpAddr> for IpAddr {
    fn from(value: std::net::IpAddr) -> Self {
        Self::from(&value)
    }
}

#[cfg(any_protocol)]
impl From<std::net::SocketAddr> for IpAddr {
    fn from(addr: std::net::SocketAddr) -> Self {
        match addr {
            std::net::SocketAddr::V4(_) => addr.ip().into(),
            std::net::SocketAddr::V6(v6) => {
                let this_scope_id = v6.scope_id();
                let mut ip: Self = addr.ip().into();
                match &mut ip {
                    IpAddr::V6 { scope_id, .. } => *scope_id = this_scope_id,
                    _ => (),
                }
                ip
            }
        }
    }
}

// Copy of https://doc.rust-lang.org/std/net/struct.Ipv6Addr.html#method.is_unicast_global to not have to force the use of a nightly toolchain
#[cfg(all(any_protocol, not(feature = "uniffi")))]
pub fn ipv6_is_global(v6: std::net::Ipv6Addr) -> bool {
    !(v6.is_unspecified()
        || v6.is_loopback()
        || matches!(v6.segments(), [0, 0, 0, 0, 0, 0xffff, _, _])
        || matches!(v6.segments(), [0x64, 0xff9b, 1, _, _, _, _, _])
        || matches!(v6.segments(), [0x100, 0, 0, 0, _, _, _, _])
        || (matches!(v6.segments(), [0x2001, b, _, _, _, _, _, _] if b < 0x200)
            && !(
                u128::from_be_bytes(v6.octets()) == 0x2001_0001_0000_0000_0000_0000_0000_0001
                || u128::from_be_bytes(v6.octets()) == 0x2001_0001_0000_0000_0000_0000_0000_0002
                || matches!(v6.segments(), [0x2001, 3, _, _, _, _, _, _])
                || matches!(v6.segments(), [0x2001, 4, 0x112, _, _, _, _, _])
                || matches!(v6.segments(), [0x2001, b, _, _, _, _, _, _] if b >= 0x20 && b <= 0x3F)
            ))
        || matches!(v6.segments(), [0x2002, _, _, _, _, _, _, _])
        || matches!(v6.segments(), [0x5f00, ..])
        || v6.is_unique_local()
        || v6.is_unicast_link_local())
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[cfg(all(
    any(target_os = "android", target_os = "ios", feature = "_uniffi_csharp"),
    feature = "logging"
))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogLevelFilter {
    Debug,
    Info,
}

#[cfg(all(
    any(target_os = "android", target_os = "ios", feature = "_uniffi_csharp"),
    feature = "logging"
))]
impl LogLevelFilter {
    pub fn to_log_compat(&self) -> log::LevelFilter {
        match self {
            LogLevelFilter::Debug => log::LevelFilter::Debug,
            LogLevelFilter::Info => log::LevelFilter::Debug,
        }
    }
}

#[cfg(all(target_os = "android", feature = "logging"))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn init_logger(level_filter: LogLevelFilter) {
    log_panics::init();
    android_logger::init_once(
        android_logger::Config::default().with_max_level(level_filter.to_log_compat()),
    );
}

#[cfg(all(target_os = "ios", feature = "logging"))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn init_logger(level_filter: LogLevelFilter) {
    env_logger::Builder::new()
        .filter(None, level_filter.to_log_compat())
        .init();
}

#[cfg(all(feature = "_uniffi_csharp", feature = "logging"))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[cfg(all(feature = "_uniffi_csharp", feature = "logging"))]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait LogHandler: Send + Sync {
    fn log(&self, level: LogLevel, tag: String, message: String);
}

#[cfg(all(feature = "_uniffi_csharp", feature = "logging"))]
pub struct CustomLogger {
    handler: std::sync::Arc<dyn LogHandler>,
}

#[cfg(all(feature = "_uniffi_csharp", feature = "logging"))]
impl CustomLogger {
    pub fn init(handler: std::sync::Arc<dyn LogHandler>) -> anyhow::Result<()> {
        log::set_max_level(log::LevelFilter::Debug);
        Ok(log::set_boxed_logger(Box::new(Self { handler }))?)
    }
}

#[cfg(all(feature = "_uniffi_csharp", feature = "logging"))]
impl log::Log for CustomLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        self.handler.log(
            match record.level() {
                log::Level::Error => LogLevel::Error,
                log::Level::Warn => LogLevel::Warn,
                log::Level::Info => LogLevel::Info,
                log::Level::Debug => LogLevel::Debug,
                log::Level::Trace => LogLevel::Trace,
            },
            record.module_path().unwrap_or("n/a").to_string(),
            record.args().to_string(),
        );
    }

    fn flush(&self) {}
}

#[cfg(all(feature = "_uniffi_csharp", feature = "logging"))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn init_custom_logger(handler: std::sync::Arc<dyn LogHandler>) {
    let _ = CustomLogger::init(handler);
}

#[cfg(test)]
mod tests {
    use crate::AsyncRuntime;

    #[tokio::test]
    async fn async_runtime_spawn() {
        let rt = AsyncRuntime::new(Some(1), "test-runtime").unwrap();
        let jh = rt.spawn(async {
            async fn test() -> u8 {
                0
            }
            test().await
        });
        assert_eq!(jh.await.unwrap(), 0u8);
    }
}
