use std::{
    collections::HashMap,
    net::{Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::Arc,
};

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
        /// Formats and capabilities the receiver advertised in its v4
        /// introduction. `None` for protocols that don't provide this
        /// information (FCast v2/v3 and Chromecast).
        capabilities: Option<ReceiverCapabilities>,
    },
}

/// Capabilities advertised by an FCast receiver in its v4 `ReceiverIntroduction`.
///
/// Mirrors the `ReceiverCapabilities` flatbuffers table.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReceiverCapabilities {
    pub media: Option<MediaCapabilities>,
    pub display: Option<DisplayCapabilities>,
    pub audio: Option<AudioCapabilities>,
}

/// The media formats a receiver supports.
///
/// Each list holds short, lowercase, canonical format tokens (e.g. `"mp4"`,
/// `"h264"`, `"whep"`) as defined by the FCast protocol. These are distinct
/// from the MIME `container` a sender puts on a loaded item.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaCapabilities {
    pub protocols: Vec<String>,
    pub containers: Vec<String>,
    pub video_formats: Vec<String>,
    pub audio_formats: Vec<String>,
    pub subtitle_formats: Vec<String>,
    pub hdr_formats: Vec<String>,
    pub image_formats: Vec<String>,
    pub external_subtitles: bool,
    pub mirroring: bool,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DisplayCapabilities {
    pub resolution: Option<VideoResolution>,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioCapabilities {
    /// The receiver's volume step granularity, e.g. `0.01` for 1%.
    pub volume_step_interval: f32,
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
    pub txt_records: HashMap<String, String>,
}

macro_rules! dev_info_constructor {
    ($fname:ident, $type:ident) => {
        pub fn $fname(
            name: String,
            addresses: Vec<IpAddr>,
            port: u16,
            txt_records: HashMap<String, String>,
        ) -> DeviceInfo {
            DeviceInfo {
                name,
                protocol: ProtocolType::$type,
                addresses,
                port,
                txt_records,
            }
        }
    };
}

/// Attempt to retrieve device info from a URL.
#[cfg(feature = "fcast")]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn device_info_from_url(url: String) -> Option<DeviceInfo> {
    let Some(network_config) = fcast_protocol::FCastNetworkConfig::parse_url(&url) else {
        log::error!("Failed to parse URL as FCastNetworkConfig");
        return None;
    };

    let tcp_service = 'out: {
        for service in network_config.services {
            if service.r#type == 0 {
                break 'out service;
            }
        }
        log::error!("No TCP service found in network config");
        return None;
    };

    let addrs = network_config
        .addresses
        .iter()
        .map(|a| a.parse::<std::net::IpAddr>())
        .map(|a| match a {
            Ok(a) => Some(IpAddr::from(&a)),
            Err(_) => None,
        })
        .collect::<Option<Vec<IpAddr>>>()?;

    Some(DeviceInfo::fcast(
        network_config.name,
        addrs,
        tcp_service.port,
        network_config.txt.unwrap_or(HashMap::new()),
    ))
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
    /// The media item played through to its natural end.
    ///
    /// This is distinct from being *stopped*: an item that is explicitly
    /// terminated before reaching its end does not produce `Ended` but instead
    /// triggers [`DeviceEventHandler::playback_stopped`]. See that method for
    /// the full ended-vs-stopped breakdown.
    Ended,
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
    CompanionResource {
        id: u32,
        /// MIME content type
        content_type: String,
    },
}

impl Source {
    pub fn content_type(&self) -> Option<&str> {
        match self {
            Source::Url { content_type, .. } | Source::CompanionResource { content_type, .. } => {
                Some(content_type.as_str())
            }
            _ => None,
        }
    }
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

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug)]
pub struct ResourceInfo {}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MediaTrackType {
    Video,
    Audio,
    Subtitle,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Clone, Debug, PartialEq)]
pub struct MediaTrack {
    pub id: u32,
    pub title: Option<String>,
    pub language: String,
    pub typ: MediaTrackType,
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
    /// Called when the current media item's playback is **stopped**, that is,
    /// explicitly terminated before reaching its natural end, either because a
    /// stop was requested (e.g. via [`CastingDevice::stop_playback`]).
    ///
    /// This is deliberately distinct from playback *ending*. A media item that
    /// plays through to completion is reported as [`PlaybackState::Ended`] via
    /// [`DeviceEventHandler::playback_state_changed`].
    fn playback_stopped(&self);
    fn playback_error(&self, message: String);
    fn tracks_available(&self, tracks: Vec<MediaTrack>);
    fn track_selected(&self, id: Option<u32>, typ: MediaTrackType);
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct MirroringOfferSink {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl MirroringOfferSink {
    /// Deliver the SDP offer to the SDK.
    pub fn send_offer(&self, sdp: String) {
        let _ = self.tx.send(sdp);
    }
}

impl MirroringOfferSink {
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self { tx }
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait FWRTCSignaller: Send + Sync + std::fmt::Debug {
    fn set_offer_sink(&self, sink: Arc<MirroringOfferSink>);
    fn on_answer_received(&self, answer: String);
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
#[derive(Debug)]
pub enum CastingDeviceError {
    FailedToSendCommand,
    MissingAddresses,
    DeviceAlreadyStarted,
    UnsupportedFeature,
}

impl std::error::Error for CastingDeviceError {}

impl std::fmt::Display for CastingDeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CastingDeviceError::FailedToSendCommand => {
                write!(f, "failed to send command to worker thread")
            }
            CastingDeviceError::MissingAddresses => write!(f, "missing addresses"),
            CastingDeviceError::DeviceAlreadyStarted => write!(f, "device already started"),
            CastingDeviceError::UnsupportedFeature => write!(f, "unsupported feature"),
        }
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceFeature {
    SetVolume,
    SetSpeed,
    LoadContent,
    LoadUrl,
    LoadImage,
    LoadPlaylist,
    PlaylistNextAndPrevious,
    SetPlaylistItemIndex,
    WhepStreaming,
    FCompanion,
    FWRTCSignalling,
    ChangeTrack,
    Queue,
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

// TODO: should collapse to store in a file when using internally
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq)]
pub enum CompanionSourceDescriptor {
    Path(String),
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, PartialEq)]
pub struct CompanionSource {
    pub descriptor: CompanionSourceDescriptor,
    pub content_type: String,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq)]
pub enum QueueItem {
    Url {
        url: String,
        content_type: String,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    // Named `FCompanion` (not `Companion`) because uniffi maps enum variants to
    // Kotlin sealed-class members, and `Companion` collides with Kotlin's
    // reserved `companion object`.
    FCompanion {
        content_type: String,
        source: CompanionSource,
        metadata: Option<Metadata>,
    },
}

impl QueueItem {
    pub fn content_type(&self) -> &str {
        match self {
            QueueItem::Url { content_type, .. } | QueueItem::FCompanion { content_type, .. } => {
                &content_type
            }
        }
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq)]
pub enum QueuePosition {
    Front,
    Back,
    Index(u8),
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug)]
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
    CompanionResource {
        content_type: String,
        source: CompanionSource,
        resume_position: Option<f64>,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
    },
    Queue {
        items: Vec<QueueItem>,
        start_index: Option<u8>,
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
    fn start_mirroring_session(
        &self,
        signaller: Arc<dyn FWRTCSignaller>,
    ) -> Result<(), CastingDeviceError>;
    fn change_track(
        &self,
        id: Option<u32>,
        track_type: MediaTrackType,
    ) -> Result<(), CastingDeviceError>;
    fn queue_remove(&self, position: QueuePosition) -> Result<(), CastingDeviceError>;
    fn queue_add(&self, item: QueueItem, position: QueuePosition)
        -> Result<(), CastingDeviceError>;
    fn queue_select(&self, position: QueuePosition) -> Result<(), CastingDeviceError>;
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "fcast")]
    #[test]
    fn test_device_info_from_url() {
        use super::{device_info_from_url, DeviceInfo, ProtocolType};
        use crate::IpAddr;
        use fcast_protocol::{FCastNetworkConfig, FCastService};
        use std::collections::HashMap;

        let config = FCastNetworkConfig {
            name: "Living Room".to_string(),
            addresses: vec!["192.168.1.42".to_string(), "10.0.0.5".to_string()],
            services: vec![
                FCastService {
                    port: 8009,
                    r#type: 1,
                },
                FCastService {
                    port: 46899,
                    r#type: 0,
                },
            ],
            txt: Some(HashMap::from([("version".to_string(), "3".to_string())])),
        };
        let url = config.to_url().unwrap();
        let info = device_info_from_url(url).expect("should produce device info");
        assert_eq!(
            info,
            DeviceInfo {
                name: "Living Room".to_string(),
                protocol: ProtocolType::FCast,
                addresses: vec![IpAddr::v4(192, 168, 1, 42), IpAddr::v4(10, 0, 0, 5)],
                port: 46899,
                txt_records: HashMap::from([("version".to_string(), "3".to_string())]),
            }
        );

        let config = FCastNetworkConfig {
            name: "No Txt".to_string(),
            addresses: vec!["127.0.0.1".to_string()],
            services: vec![FCastService {
                port: 1234,
                r#type: 0,
            }],
            txt: None,
        };
        let info = device_info_from_url(config.to_url().unwrap()).unwrap();
        assert_eq!(info.port, 1234);
        assert_eq!(info.addresses, vec![IpAddr::v4(127, 0, 0, 1)]);
        assert!(info.txt_records.is_empty());

        let config = FCastNetworkConfig {
            name: "v6".to_string(),
            addresses: vec!["fe80::1".to_string()],
            services: vec![FCastService {
                port: 46899,
                r#type: 0,
            }],
            txt: None,
        };
        let info = device_info_from_url(config.to_url().unwrap()).unwrap();
        let expected_v6 = IpAddr::from(&"fe80::1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(info.addresses, vec![expected_v6]);

        let config = FCastNetworkConfig {
            name: "No TCP".to_string(),
            addresses: vec!["192.168.1.1".to_string()],
            services: vec![FCastService {
                port: 8009,
                r#type: 1,
            }],
            txt: None,
        };
        assert!(device_info_from_url(config.to_url().unwrap()).is_none());

        let config = FCastNetworkConfig {
            name: "Bad Addr".to_string(),
            addresses: vec!["not-an-ip".to_string()],
            services: vec![FCastService {
                port: 46899,
                r#type: 0,
            }],
            txt: None,
        };
        assert!(device_info_from_url(config.to_url().unwrap()).is_none());
        assert!(device_info_from_url("https://example.com".to_string()).is_none());
    }
}
