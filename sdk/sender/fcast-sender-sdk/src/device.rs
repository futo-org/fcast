use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;

use crate::IpAddr;

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug)]
pub enum DeviceConnectionState {
    Disconnected,
    Connecting,
    Reconnecting,
    Connected {
        used_remote_addr: IpAddr,
        local_addr: IpAddr,
    },
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolType {
    #[cfg(feature = "chromecast")]
    Chromecast,
    #[cfg(feature = "fcast")]
    FCast,
}

pub(crate) fn ips_to_socket_addrs(ips: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    ips.iter()
        .map(|a| match *a {
            IpAddr::V4 { .. } => SocketAddr::new(a.into(), port),
            IpAddr::V6 {
                scope_id,
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
            } => SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from_bits(u128::from_be_bytes([
                    o1, o2, o3, o4, o5, o6, o7, o8, o9, o10, o11, o12, o13, o14, o15, o16,
                ])),
                port,
                0,
                scope_id,
            )),
        })
        .collect()
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub protocol: ProtocolType,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

macro_rules! dev_info_constructor {
    ($fname:ident, $type:ident) => {
        pub fn $fname(name: String, addresses: Vec<IpAddr>, port: u16) -> DeviceInfo {
            DeviceInfo {
                name,
                protocol: ProtocolType::$type,
                addresses,
                port,
            }
        }
    };
}

/// Attempt to retrieve device info from a URL.
#[cfg(feature = "fcast")]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn device_info_from_url(url: String) -> Option<DeviceInfo> {
    #[derive(serde::Deserialize)]
    struct FCastService {
        port: u16,
        r#type: i32,
    }

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

    use base64::alphabet::URL_SAFE;
    use base64::engine::general_purpose::GeneralPurpose;
    use base64::engine::{DecodePaddingMode, GeneralPurposeConfig};
    use base64::Engine as _;
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

    Some(DeviceInfo::fcast(found_info.name, addrs, tcp_service.port))
}

impl DeviceInfo {
    #[cfg(feature = "fcast")]
    dev_info_constructor!(fcast, FCast);
    #[cfg(feature = "chromecast")]
    dev_info_constructor!(chromecast, Chromecast);
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

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistItem {
    /// MIME type
    pub content_type: String,
    /// URL
    pub content_location: String,
    /// Seconds from beginning of media to start playback
    pub start_time: Option<f64>,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KeyName {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Ok,
}

impl KeyName {
    pub fn all() -> Vec<Self> {
        vec![
            Self::ArrowLeft,
            Self::ArrowRight,
            Self::ArrowUp,
            Self::ArrowDown,
            Self::Ok,
        ]
    }
}

impl ToString for KeyName {
    fn to_string(&self) -> String {
        match self {
            KeyName::ArrowLeft => "ArrowLeft",
            KeyName::ArrowRight => "ArrowRight",
            KeyName::ArrowUp => "ArrowUp",
            KeyName::ArrowDown => "ArrowDown",
            KeyName::Ok => "Ok",
        }
        .to_owned()
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EventSubscription {
    MediaItemStart,
    MediaItemEnd,
    MediaItemChange,
    KeyDown { keys: Vec<KeyName> },
    KeyUp { keys: Vec<KeyName> },
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub released: bool,
    pub repeat: bool,
    pub handled: bool,
    pub name: String,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaItemEventType {
    Start,
    End,
    Change,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug)]
pub struct MediaItem {
    pub content_type: String,
    pub url: Option<String>,
    pub content: Option<String>,
    pub time: Option<f64>,
    pub volume: Option<f64>,
    pub speed: Option<f64>,
    pub show_duration: Option<f64>,
    pub metadata: Option<Metadata>,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug)]
pub struct MediaEvent {
    pub type_: MediaItemEventType,
    pub item: MediaItem,
}

#[allow(unused_variables)]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait DeviceEventHandler: Send + Sync {
    fn connection_state_changed(&self, state: DeviceConnectionState);
    fn volume_changed(&self, volume: f64);
    fn time_changed(&self, time: f64);
    fn playback_state_changed(&self, state: PlaybackState);
    fn duration_changed(&self, duration: f64);
    fn speed_changed(&self, speed: f64);
    fn source_changed(&self, source: Source);
    fn key_event(&self, event: KeyEvent);
    fn media_event(&self, event: MediaEvent);
    fn playback_error(&self, message: String);
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
    #[error("unsupported feature")]
    UnsupportedFeature,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceFeature {
    SetVolume,
    SetSpeed,
    LoadContent,
    LoadUrl,
    KeyEventSubscription,
    MediaEventSubscription,
    LoadImage,
    LoadPlaylist,
    PlaylistNextAndPrevious,
    SetPlaylistItemIndex,
    WhepStreaming,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
    pub title: Option<String>,
    pub thumbnail_url: Option<String>,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug)]
pub struct ApplicationInfo {
    pub name: String,
    pub version: String,
    pub display_name: String,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, Clone)]
pub enum LoadRequest {
    Url {
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    /// Load content, usually a [DASH](https://en.wikipedia.org/wiki/Dynamic_Adaptive_Streaming_over_HTTP) or
    /// [HLS](https://en.wikipedia.org/wiki/HTTP_Live_Streaming) manifest.
    Content {
        content_type: String,
        content: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Video {
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Image {
        content_type: String,
        url: String,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Playlist {
        items: Vec<PlaylistItem>,
    },
}

/// A generic interface for casting devices.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub trait CastingDevice: Send + Sync {
    // NOTE: naming it `protocol` causes iOS builds to fail
    fn casting_protocol(&self) -> ProtocolType;
    /// Returns `true` if the device has the required information needed to
    /// start a connection.
    fn is_ready(&self) -> bool;
    /// Some features may only be present after the device has emitted the
    /// [`Connected`] event.
    ///
    /// [`Connected`]: DeviceConnectionState::Connected
    fn supports_feature(&self, feature: DeviceFeature) -> bool;
    fn name(&self) -> String;
    fn set_name(&self, name: String);
    fn seek(&self, time_seconds: f64) -> Result<(), CastingDeviceError>;
    /// Stop the media that is playing on the receiver.
    ///
    /// This will usually result in the receiver closing the media viewer and
    /// show a default screen.
    fn stop_playback(&self) -> Result<(), CastingDeviceError>;
    fn pause_playback(&self) -> Result<(), CastingDeviceError>;
    fn resume_playback(&self) -> Result<(), CastingDeviceError>;
    /// Load a media item.
    fn load(&self, request: LoadRequest) -> Result<(), CastingDeviceError>;
    /// Try to play the next item in the playlist.
    fn playlist_item_next(&self) -> Result<(), CastingDeviceError>;
    /// Try to play the previous item in the playlist.
    fn playlist_item_previous(&self) -> Result<(), CastingDeviceError>;
    /// Set the item index for the currently playing playlist.
    ///
    /// # Arguments
    ///   * `index`: zero-based index into the playlist
    fn set_playlist_item_index(&self, index: u32) -> Result<(), CastingDeviceError>;
    fn change_volume(&self, volume: f64) -> Result<(), CastingDeviceError>;
    fn change_speed(&self, speed: f64) -> Result<(), CastingDeviceError>;
    fn disconnect(&self) -> Result<(), CastingDeviceError>;
    /// Connect to the device.
    ///
    /// # Arguments
    ///   * `reconnect_interval_millis`: the interval between each reconnect attempt. Setting this
    ///     to `0` indicates that reconnects should not be attempted.
    #[cfg_attr(feature = "uniffi", uniffi::method(default(app_info = None)))]
    fn connect(
        &self,
        app_info: Option<ApplicationInfo>,
        event_handler: Arc<dyn DeviceEventHandler>,
        reconnect_interval_millis: u64,
    ) -> Result<(), CastingDeviceError>;
    fn get_device_info(&self) -> DeviceInfo;
    fn get_addresses(&self) -> Vec<IpAddr>;
    fn set_addresses(&self, addrs: Vec<IpAddr>);
    fn get_port(&self) -> u16;
    fn set_port(&self, port: u16);
    /// Attempt to subscribe to an event group.
    ///
    /// An error will be returned if the device does not support the group. Use
    /// [`supports_feature`] to check if the group is supported.
    ///
    /// [`supports_feature`]: Self::supports_feature
    fn subscribe_event(&self, group: EventSubscription) -> Result<(), CastingDeviceError>;
    fn unsubscribe_event(&self, group: EventSubscription) -> Result<(), CastingDeviceError>;
}
