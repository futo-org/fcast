//! # FCast Sender SDK
//!
//! An all in one SDK for casting media to [FCast], [AirPlay] (soon), [Chromecast] and [Google Cast] receiver devices.
//!
//! ## Supported languages
//!
//! + Rust
//! + Kotlin
//! + Swift (soon)
//!
//! ## Features
//!
//! + Automatic discovery of devices on the network via [mDNS]
//! + HTTP file server for easy casting of local media files
//!
//! ## Example usage
//!
//! TODO
//!
//! [FCast]: https://fcast.org/
//! [AirPlay]: https://www.apple.com/airplay/
//! [Chromecast]: https://en.wikipedia.org/wiki/Chromecast
//! [Google Cast]: https://www.android.com/better-together/#cast
//! [mDNS]: https://en.wikipedia.org/wiki/Multicast_DNS

#[cfg(feature = "airplay1")]
pub mod airplay1;
#[cfg(feature = "airplay2")]
pub mod airplay2;
#[cfg(any(feature = "airplay1", feature = "airplay2"))]
pub(crate) mod airplay_common;
#[cfg(feature = "chromecast")]
pub mod chromecast;
#[cfg(any(feature = "http-file-server", any_protocol))]
pub mod context;
#[cfg(all(any_protocol, feature = "discovery"))]
pub mod discovery;
#[cfg(feature = "fcast")]
pub mod fcast;
pub(crate) mod utils;

#[cfg(feature = "http-file-server")]
pub mod file_server;

/// Event handler for device discovery.
#[cfg(all(any_protocol, feature = "discovery_types"))]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait DeviceDiscovererEventHandler: Send + Sync {
    /// Called when a device is found.
    fn device_available(&self, device_info: casting_device::DeviceInfo);
    /// Called when a device is removed or lost.
    fn device_removed(&self, device_name: String);
    /// Called when a device has changed.
    ///
    /// The `name` field of `device_info` will correspond to a device announced from `device_available`.
    fn device_changed(&self, device_info: casting_device::DeviceInfo);
}

#[cfg(all(feature = "discovery", any_protocol))]
use std::future::Future;
#[cfg(any(feature = "http-file-server", any_protocol))]
use tokio::runtime;
#[cfg(any_protocol)]
pub mod casting_device;
#[cfg(any_protocol)]
use log::error;
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
fn url_format_ip_addr(addr: &IpAddr) -> String {
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
        } => {
            let addr = std::net::Ipv6Addr::from_bits(u128::from_be_bytes([
                *o1, *o2, *o3, *o4, *o5, *o6, *o7, *o8, *o9, *o10, *o11, *o12, *o13, *o14, *o15,
                *o16,
            ]));
            format!("[{}]", addr)
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

#[cfg(all(target_os = "android", feature = "logging"))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn init_logger() {
    log_panics::init();
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );
}

#[cfg(all(target_os = "ios", feature = "logging"))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn init_logger() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
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
