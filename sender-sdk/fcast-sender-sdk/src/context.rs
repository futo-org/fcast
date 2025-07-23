#[cfg(any_protocol)]
use crate::casting_device::{CastProtocolType, CastingDevice, CastingDeviceInfo};
#[cfg(all(feature = "discovery", any_protocol))]
use crate::discovery;
use crate::{AsyncRuntime, AsyncRuntimeError};
#[cfg(any_protocol)]
use std::sync::Arc;

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct CastContext {
    runtime: AsyncRuntime,
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastContext {
    #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    pub fn new() -> Result<Self, AsyncRuntimeError> {
        Ok(Self {
            runtime: AsyncRuntime::new(Some(1), "cast-context-async-runtime")?,
        })
    }
}

#[cfg(any_protocol)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastContext {
    pub fn create_device_from_info(&self, info: CastingDeviceInfo) -> Arc<dyn CastingDevice> {
        match info.r#type {
            #[cfg(feature = "chromecast")]
            CastProtocolType::Chromecast => {
                Arc::new(crate::chromecast::ChromecastCastingDevice::new(
                    info,
                    self.runtime.handle().clone(),
                ))
            }
            #[cfg(feature = "airplay1")]
            CastProtocolType::AirPlay => Arc::new(crate::airplay1::AirPlay1CastingDevice::new(
                info,
                self.runtime.handle().clone(),
            )),
            #[cfg(feature = "airplay2")]
            CastProtocolType::AirPlay2 => Arc::new(crate::airplay2::AirPlay2CastingDevice::new(
                info,
                self.runtime.handle().clone(),
            )),
            #[cfg(feature = "fcast")]
            CastProtocolType::FCast => Arc::new(crate::fcast::FCastCastingDevice::new(
                info,
                self.runtime.handle().clone(),
            )),
        }
    }
}

// #[cfg(all(feature = "discovery", any_protocol))]
#[cfg(all(feature = "discovery", any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastContext {
    // pub fn start_discovery(&self, event_handler: Arc<dyn discovery::DeviceDiscovererEventHandler>) {
    pub fn start_discovery(&self, event_handler: Arc<dyn crate::DeviceDiscovererEventHandler>) {
        self.runtime
            .spawn(discovery::discover_devices(event_handler));
    }
}

#[cfg(feature = "http-file-server")]
use crate::file_server;

#[cfg(feature = "http-file-server")]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastContext {
    pub fn start_file_server(&self) -> file_server::FileServer {
        let server = file_server::FileServer::new(self.runtime.handle().clone());
        server.start();
        server
    }
}
