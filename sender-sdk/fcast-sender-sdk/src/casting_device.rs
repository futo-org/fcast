use crate::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug)]
pub enum CastConnectionState {
    Disconnected,
    Connecting,
    Connected {
        used_remote_addr: IpAddr,
        local_addr: IpAddr,
    },
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum CastProtocolType {
    #[cfg(feature = "chromecast")]
    Chromecast,
    #[cfg(feature = "airplay1")]
    AirPlay,
    #[cfg(feature = "airplay2")]
    AirPlay2,
    #[cfg(feature = "fcast")]
    FCast,
}

pub(crate) fn ips_to_socket_addrs(ips: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    ips.iter()
        .map(|a| a.into())
        .map(|a| SocketAddr::new(a, port))
        .collect()
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CastingDeviceInfo {
    pub name: String,
    pub r#type: CastProtocolType,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

macro_rules! dev_info_constructor {
    ($fname:ident, $type:ident) => {
        pub fn $fname(name: String, addresses: Vec<IpAddr>, port: u16) -> CastingDeviceInfo {
            CastingDeviceInfo {
                name,
                r#type: CastProtocolType::$type,
                addresses,
                port,
            }
        }
    };
}

#[cfg(feature = "fcast")]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn device_info_from_url(url: String) -> Option<CastingDeviceInfo> {
    #[cfg(feature = "fcast")]
    #[derive(serde::Deserialize)]
    struct FCastService {
        port: u16,
        r#type: i32,
    }

    #[cfg(feature = "fcast")]
    #[derive(serde::Deserialize)]
    struct FCastNetworkConfig {
        name: String,
        addresses: Vec<String>,
        services: Vec<FCastService>,
    }

    let url = match url::Url::parse(&url) {
        Ok(uri) => uri,
        Err(err) => {
            log::error!("Invalid URL: {err}");
            return None;
        }
    };

    if url.scheme() != "fcast" {
        log::error!("Expected URL scheme to be fcast, was {}", url.scheme());
        return None;
    }

    if url.host_str() != Some("r") {
        log::error!("Expected URL type to be r");
        return None;
    }

    let connection_info = url.path_segments()?.next()?;

    use base64::{
        alphabet::URL_SAFE,
        engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig},
        Engine as _,
    };
    let b64_engine = GeneralPurpose::new(
        &URL_SAFE,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let json = match b64_engine.decode(connection_info) {
        Ok(json) => json,
        Err(err) => {
            log::error!("Failed to decode base64: {err}");
            return None;
        }
    };
    let found_info: FCastNetworkConfig = match serde_json::from_slice(&json) {
        Ok(info) => info,
        Err(err) => {
            log::error!("Failed to decode network config json: {err}");
            return None;
        }
    };

    let tcp_service = 'out: {
        for service in found_info.services {
            if service.r#type == 0 {
                break 'out service;
            }
        }
        log::error!("No TCP service found in network config");
        return None;
    };

    let addrs = found_info
        .addresses
        .iter()
        .map(|a| a.parse::<std::net::IpAddr>())
        .map(|a| match a {
            Ok(a) => Some(IpAddr::from(&a)),
            Err(_) => None,
        })
        .collect::<Option<Vec<IpAddr>>>()?;

    Some(CastingDeviceInfo::fcast(
        found_info.name,
        addrs,
        tcp_service.port,
    ))
}

impl CastingDeviceInfo {
    #[cfg(feature = "fcast")]
    dev_info_constructor!(fcast, FCast);
    #[cfg(feature = "chromecast")]
    dev_info_constructor!(chromecast, Chromecast);
    #[cfg(feature = "airplay1")]
    dev_info_constructor!(airplay1, AirPlay);
    #[cfg(feature = "airplay2")]
    dev_info_constructor!(airplay2, AirPlay2);
}

#[derive(Default, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PlaybackState {
    #[default]
    Idle,
    Buffering,
    Playing,
    Paused,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Url {
        url: String,
        /// MIME content type
        content_type: String,
    },
    Content {
        content: String,
    },
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug)]
pub enum GenericEventSubscriptionGroup {
    Keys,
    Media,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug)]
pub struct GenericKeyEvent {
    pub released: bool,
    pub repeat: bool,
    pub handled: bool,
    pub name: String,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug)]
pub enum GenericMediaEvent {
    Started,
    Ended,
    Changed,
}

#[allow(unused_variables)]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait CastingDeviceEventHandler: Send + Sync {
    fn connection_state_changed(&self, state: CastConnectionState);
    fn volume_changed(&self, volume: f64);
    fn time_changed(&self, time: f64);
    fn playback_state_changed(&self, state: PlaybackState);
    fn duration_changed(&self, duration: f64);
    fn speed_changed(&self, speed: f64);
    fn source_changed(&self, source: Source);
    fn key_event(&self, event: GenericKeyEvent);
    fn media_event(&self, event: GenericMediaEvent);
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
#[derive(thiserror::Error, Debug)]
pub enum CastingDeviceError {
    #[error("failed to send command to worker thread")]
    FailedToSendCommand,
    #[error("missing addresses")]
    MissingAddresses,
    #[error("device already started")]
    DeviceAlreadyStarted,
    #[error("unsupported subscription")]
    UnsupportedSubscription,
}

/// # Internal. Do not use.
// #[cfg(any_protocol)]
// pub trait CastingDeviceExt: Send + Sync {
//     fn soft_start(
//         &self,
//         event_handler: Arc<dyn CastingDeviceEventHandler>,
//     ) -> Result<Pin<Box<dyn Future<Output = ()> + Send + 'static>>, CastingDeviceError>;
// }

/// A generic interface for casting devices.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub trait CastingDevice: Send + Sync /* + CastingDeviceExt */ {
    // NOTE: naming it `protocol` causes iOS builds to fail
    fn casting_protocol(&self) -> CastProtocolType;
    fn is_ready(&self) -> bool;
    fn can_set_volume(&self) -> bool;
    fn can_set_speed(&self) -> bool;
    fn support_subscriptions(&self) -> bool;
    fn name(&self) -> String;
    fn set_name(&self, name: String);
    fn stop_casting(&self) -> Result<(), CastingDeviceError>;
    fn seek(&self, time_seconds: f64) -> Result<(), CastingDeviceError>;
    fn stop_playback(&self) -> Result<(), CastingDeviceError>;
    fn pause_playback(&self) -> Result<(), CastingDeviceError>;
    fn resume_playback(&self) -> Result<(), CastingDeviceError>;
    fn load_url(
        &self,
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError>;
    fn load_content(
        &self,
        content_type: String,
        content: String,
        resume_position: f64,
        duration: f64,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError>;
    fn load_video(
        &self,
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError>;
    fn load_image(&self, content_type: String, url: String) -> Result<(), CastingDeviceError>;
    fn change_volume(&self, volume: f64) -> Result<(), CastingDeviceError>;
    fn change_speed(&self, speed: f64) -> Result<(), CastingDeviceError>;
    fn disconnect(&self) -> Result<(), CastingDeviceError>;
    fn connect(
        &self,
        event_handler: Arc<dyn CastingDeviceEventHandler>,
    ) -> Result<(), CastingDeviceError>;
    fn get_device_info(&self) -> CastingDeviceInfo;
    fn get_addresses(&self) -> Vec<IpAddr>;
    fn set_addresses(&self, addrs: Vec<IpAddr>);
    fn get_port(&self) -> u16;
    fn set_port(&self, port: u16);
    fn subscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError>;
    fn unsubscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError>;
}
