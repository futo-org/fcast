use std::collections::HashMap;
use std::sync::Arc;

pub use fcast_sender_sdk::IpAddr;
use fcast_sender_sdk::context;
pub use fcast_sender_sdk::device::{
    self, ApplicationInfo, CastingDeviceError, DeviceConnectionState, DeviceFeature, DeviceInfo,
    GenericEventSubscriptionGroup, GenericKeyEvent, GenericMediaEvent, LoadRequest, Metadata,
    PlaybackState, PlaylistItem, ProtocolType, Source,
};
use flutter_rust_bridge::frb;

#[frb(mirror(IpAddr))]
pub enum _IpAddr {
    V4 {
        o1: u8,
        o2: u8,
        o3: u8,
        o4: u8,
    },
    V6 {
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

#[frb(mirror(ProtocolType))]
pub enum _ProtocolType {
    Chromecast,
    FCast,
}

#[frb(mirror(DeviceConnectionState))]
pub enum _DeviceConnectionState {
    Disconnected,
    Connecting,
    Connected {
        used_remote_addr: _IpAddr,
        local_addr: _IpAddr,
    },
}

#[frb(mirror(DeviceInfo))]
pub struct _DeviceInfo {
    pub name: String,
    pub protocol: ProtocolType,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

pub fn device_info_from_url(url: String) -> Option<DeviceInfo> {
    device::device_info_from_url(url)
}

#[frb(mirror(PlaybackState))]
#[derive(Default)]
pub enum _PlaybackState {
    #[default]
    Idle,
    Buffering,
    Playing,
    Paused,
}

#[frb(mirror(Source))]
pub enum _Source {
    Url {
        url: String,
        /// MIME content type
        content_type: String,
    },
    Content {
        content: String,
    },
}

#[frb(mirror(PlaylistItem))]
pub struct _PlaylistItem {
    /// MIME type
    pub content_type: String,
    /// URL
    pub content_location: String,
    /// Seconds from beginning of media to start playback
    pub start_time: Option<f64>,
}

#[frb(mirror(GenericEventSubscriptionGroup))]
pub enum _GenericEventSubscriptionGroup {
    Keys,
    Media,
}

#[frb(mirror(GenericKeyEvent))]
pub struct _GenericKeyEvent {
    pub released: bool,
    pub repeat: bool,
    pub handled: bool,
    pub name: String,
}

#[frb(mirror(GenericMediaEvent))]
pub enum _GenericMediaEvent {
    Started,
    Ended,
    Changed,
}

pub trait DeviceEventHandler: Send + Sync {
    fn connection_state_changed(&self, state: _DeviceConnectionState);
    fn volume_changed(&self, volume: f64);
    fn time_changed(&self, time: f64);
    fn playback_state_changed(&self, state: _PlaybackState);
    fn duration_changed(&self, duration: f64);
    fn speed_changed(&self, speed: f64);
    fn source_changed(&self, source: _Source);
    fn key_event(&self, event: _GenericKeyEvent);
    fn media_event(&self, event: _GenericMediaEvent);
    fn playback_error(&self, message: String);
}

#[frb(ignore)]
struct DeviceEventHandlerWrapper(Arc<dyn DeviceEventHandler>);

impl device::DeviceEventHandler for DeviceEventHandlerWrapper {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        #[rustfmt::skip]
        macro_rules! ip_addr_wrapper_to_orig {
            ($addr:expr) => {
                match $addr {
                    IpAddr::V4 { o1, o2, o3, o4 } => _IpAddr::V4 { o1, o2, o3, o4 },
                    IpAddr::V6 {
                        o1, o2, o3, o4, o5, o6, o7, o8, o9, o10, o11, o12, o13, o14, o15, o16,
                        scope_id,
                    } => _IpAddr::V6 {
                        o1, o2, o3, o4, o5, o6, o7, o8, o9, o10, o11, o12, o13, o14, o15, o16,
                        scope_id,
                    },
                }
            };
        }

        self.0.connection_state_changed(match state {
            DeviceConnectionState::Disconnected => _DeviceConnectionState::Disconnected,
            DeviceConnectionState::Connecting => _DeviceConnectionState::Connecting,
            DeviceConnectionState::Connected {
                used_remote_addr,
                local_addr,
            } => _DeviceConnectionState::Connected {
                used_remote_addr: ip_addr_wrapper_to_orig!(used_remote_addr),
                local_addr: ip_addr_wrapper_to_orig!(local_addr),
            },
        });
    }

    fn volume_changed(&self, volume: f64) {
        self.0.volume_changed(volume);
    }

    fn time_changed(&self, time: f64) {
        self.0.time_changed(time);
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        self.0.playback_state_changed(match state {
            PlaybackState::Idle => _PlaybackState::Idle,
            PlaybackState::Buffering => _PlaybackState::Buffering,
            PlaybackState::Playing => _PlaybackState::Playing,
            PlaybackState::Paused => _PlaybackState::Paused,
        });
    }

    fn duration_changed(&self, duration: f64) {
        self.0.duration_changed(duration);
    }

    fn speed_changed(&self, speed: f64) {
        self.0.speed_changed(speed);
    }

    fn source_changed(&self, source: Source) {
        self.0.source_changed(match source {
            Source::Url { url, content_type } => _Source::Url { url, content_type },
            Source::Content { content } => _Source::Content { content },
        });
    }

    fn key_event(&self, event: GenericKeyEvent) {
        self.0.key_event(_GenericKeyEvent {
            released: event.released,
            repeat: event.repeat,
            handled: event.handled,
            name: event.name,
        });
    }

    fn media_event(&self, event: GenericMediaEvent) {
        self.0.media_event(match event {
            GenericMediaEvent::Started => _GenericMediaEvent::Started,
            GenericMediaEvent::Ended => _GenericMediaEvent::Ended,
            GenericMediaEvent::Changed => _GenericMediaEvent::Changed,
        });
    }

    fn playback_error(&self, message: String) {
        self.0.playback_error(message);
    }
}

#[frb(mirror(CastingDeviceError))]
#[derive(thiserror::Error, Debug)]
pub enum _CastingDeviceError {
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

#[frb(mirror(DeviceFeature))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum _DeviceFeature {
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
}

macro_rules! device_error_converter {
    ($result:expr) => {
        match $result {
            Ok(r) => Ok(r),
            Err(err) => Err(match err {
                CastingDeviceError::FailedToSendCommand => _CastingDeviceError::FailedToSendCommand,
                CastingDeviceError::MissingAddresses => _CastingDeviceError::MissingAddresses,
                CastingDeviceError::DeviceAlreadyStarted => {
                    _CastingDeviceError::DeviceAlreadyStarted
                }
                CastingDeviceError::UnsupportedSubscription => {
                    _CastingDeviceError::UnsupportedSubscription
                }
                CastingDeviceError::UnsupportedFeature => _CastingDeviceError::UnsupportedFeature,
            }),
        }
    };
}

#[frb(mirror(Metadata))]
#[derive(Debug, Clone, PartialEq)]
pub struct _Metadata {
    pub title: Option<String>,
    pub thumbnail_url: Option<String>,
}

#[frb(mirror(ApplicationInfo))]
#[derive(Debug)]
pub struct _ApplicationInfo {
    pub name: String,
    pub version: String,
    pub display_name: String,
}

#[frb(mirror(LoadRequest))]
#[derive(Debug, Clone)]
pub enum _LoadRequest {
    Url {
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<_Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Content {
        content_type: String,
        content: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<_Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Video {
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<_Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Image {
        content_type: String,
        url: String,
        metadata: Option<_Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    Playlist {
        items: Vec<PlaylistItem>,
    },
}

#[frb(opaque)]
pub struct CastingDevice(Arc<dyn device::CastingDevice>);

impl CastingDevice {
    fn casting_protocol(&self) -> ProtocolType {
        self.0.casting_protocol()
    }

    fn is_ready(&self) -> bool {
        self.0.is_ready()
    }

    fn supports_feature(&self, feature: DeviceFeature) -> bool {
        self.0.supports_feature(feature)
    }

    fn name(&self) -> String {
        self.0.name()
    }

    fn set_name(&self, name: String) {
        self.0.set_name(name);
    }

    fn seek(&self, time_seconds: f64) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.seek(time_seconds))
    }

    fn stop_playback(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.stop_playback())
    }
    fn pause_playback(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.pause_playback())
    }
    fn resume_playback(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.resume_playback())
    }

    fn load(&self, request: LoadRequest) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.load(request))
    }

    fn playlist_item_next(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.playlist_item_next())
    }

    fn playlist_item_previous(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.playlist_item_previous())
    }

    /// Set the item index for the currently playing playlist.
    ///
    /// # Arguments
    ///   * `index`: zero-based index into the playlist
    fn set_playlist_item_index(&self, index: u32) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.set_playlist_item_index(index))
    }

    fn change_volume(&self, volume: f64) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.change_volume(volume))
    }

    fn change_speed(&self, speed: f64) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.change_speed(speed))
    }

    fn disconnect(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.disconnect())
    }

    fn connect(
        &self,
        app_info: Option<ApplicationInfo>,
        event_handler: Arc<dyn DeviceEventHandler>,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.connect(
            app_info,
            Arc::new(DeviceEventHandlerWrapper(event_handler)),
        ))
    }

    fn get_device_info(&self) -> DeviceInfo {
        self.0.get_device_info()
    }

    fn get_addresses(&self) -> Vec<IpAddr> {
        self.0.get_addresses()
    }

    fn set_addresses(&self, addrs: Vec<IpAddr>) {
        self.0.set_addresses(addrs);
    }

    fn get_port(&self) -> u16 {
        self.0.get_port()
    }

    fn set_port(&self, port: u16) {
        self.0.set_port(port);
    }

    fn subscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.subscribe_event(group))
    }

    fn unsubscribe_event(
        &self,
        group: GenericEventSubscriptionGroup,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.unsubscribe_event(group))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ErrorMessage {
    #[error("{0}")]
    Error(String),
}

#[frb(opaque)]
pub struct CastContext(context::CastContext);

impl CastContext {
    #[frb(sync)]
    pub fn new() -> Result<Self, ErrorMessage> {
        Ok(Self(
            context::CastContext::new().map_err(|err| ErrorMessage::Error(err.to_string()))?,
        ))
    }

    #[frb(sync)]
    pub fn create_device_from_info(&self, info: DeviceInfo) -> CastingDevice {
        CastingDevice(self.0.create_device_from_info(info))
    }
}
