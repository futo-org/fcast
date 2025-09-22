#[cfg(any_protocol)]
use std::sync::Arc;

#[cfg(any_protocol)]
use crate::device::{CastingDevice, DeviceInfo, ProtocolType};
#[cfg(all(feature = "discovery", any_protocol))]
use crate::discovery;
use crate::{AsyncRuntime, AsyncRuntimeError};

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
    pub fn create_device_from_info(&self, info: DeviceInfo) -> Arc<dyn CastingDevice> {
        match info.protocol {
            #[cfg(feature = "chromecast")]
            ProtocolType::Chromecast => Arc::new(crate::chromecast::ChromecastDevice::new(
                info,
                self.runtime.handle().clone(),
            )),
            #[cfg(feature = "fcast")]
            ProtocolType::FCast => Arc::new(crate::fcast::FCastDevice::new(info, self.runtime.handle().clone())),
        }
    }
}

#[cfg(all(feature = "discovery", any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastContext {
    pub fn start_discovery(&self, event_handler: Arc<dyn crate::DeviceDiscovererEventHandler>) {
        self.runtime.spawn(discovery::discover_devices(event_handler));
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
