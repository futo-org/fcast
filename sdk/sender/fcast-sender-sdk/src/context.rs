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
        #[cfg(feature = "fcast")]
        if tokio_rustls::rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            log::error!("Failed to install default crypto provider (ring)");
        }

        Ok(Self {
            runtime: AsyncRuntime::new(Some(1), "cast-context-async-runtime")?,
        })
    }
}

#[cfg(any_protocol)]
impl CastContext {
    /// Concrete [`crate::fcast::FCastDevice`] so callers can use
    /// [`crate::fcast::FCastDevice::companion_resource_registrar`].
    ///
    /// Dynamic FCompanion registration is Rust-only.
    /// [`Self::create_device_from_info`] erases to [`CastingDevice`] and is
    /// what UniFFI/Flutter see; it cannot register dynamic resources.
    #[cfg(feature = "fcast")]
    pub fn create_fcast_device_from_info(
        &self,
        info: DeviceInfo,
    ) -> Arc<crate::fcast::FCastDevice> {
        Arc::new(crate::fcast::FCastDevice::new(
            info,
            self.runtime.handle().clone(),
        ))
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
            ProtocolType::FCast => self.create_fcast_device_from_info(info),
            // Under `__flutter_hacks`, `ProtocolType` carries variants for protocols that were not
            // compiled in. Their device types do not exist, so reject them here. Callers gate on
            // `enabled_protocols`.
            #[cfg(feature = "__flutter_hacks")]
            #[allow(unreachable_patterns)]
            other => panic!("protocol {other:?} is not compiled into this build"),
        }
    }
}

#[cfg(all(feature = "discovery", any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastContext {
    pub fn start_discovery(&self, event_handler: Arc<dyn crate::DeviceDiscovererEventHandler>) {
        self.runtime
            .spawn(discovery::discover_devices(event_handler));
    }
}
