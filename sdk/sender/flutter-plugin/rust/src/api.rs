use std::{collections::HashMap, sync::Arc};

use fcast_sender_sdk_raw::context;
pub use fcast_sender_sdk_raw::{
    device::{
        self, ApplicationInfo, AudioCapabilities, CastingDeviceError, CompanionSource,
        CompanionSourceDescriptor, DeviceConnectionState, DeviceFeature, DeviceInfo,
        DisplayCapabilities, LoadRequest, MediaCapabilities, MediaItem, MediaLocator, MediaTrack,
        MediaTrackType, Metadata, PlaybackState, PlaylistItem, ProtocolType, Queue, QueueEntry,
        QueueItem, QueuePosition, QueueState, ReceiverCapabilities, ReceiverError, Source,
        SubtitleContent, SubtitleSource, TrackList, VideoResolution,
    },
    IpAddr,
};
use flutter_rust_bridge::{frb, DartFnFuture};

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
    Reconnecting,
    Connected {
        used_remote_addr: IpAddr,
        local_addr: IpAddr,
        capabilities: Option<ReceiverCapabilities>,
    },
}

#[frb(mirror(ReceiverCapabilities))]
pub struct _ReceiverCapabilities {
    pub media: Option<MediaCapabilities>,
    pub display: Option<DisplayCapabilities>,
    pub audio: Option<AudioCapabilities>,
}

#[frb(mirror(MediaCapabilities))]
pub struct _MediaCapabilities {
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

#[frb(mirror(DisplayCapabilities))]
pub struct _DisplayCapabilities {
    pub resolution: Option<VideoResolution>,
}

#[frb(mirror(VideoResolution))]
pub struct _VideoResolution {
    pub width: u32,
    pub height: u32,
}

#[frb(mirror(AudioCapabilities))]
pub struct _AudioCapabilities {
    /// The receiver's volume step granularity, e.g. `0.01` for 1%.
    pub volume_step_interval: f32,
}

#[frb(mirror(DeviceInfo))]
pub struct _DeviceInfo {
    pub name: String,
    pub protocol: ProtocolType,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub txt_records: HashMap<String, String>,
}

#[frb(sync)]
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
    /// The media item played through to its natural end (distinct from being stopped).
    Ended,
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
    CompanionResource {
        id: u32,
        /// MIME content type
        content_type: String,
    },
}

#[frb(mirror(CompanionSourceDescriptor))]
pub enum _CompanionSourceDescriptor {
    Path(String),
    Fd(i32),
    Bytes(Vec<u8>),
}

#[frb(mirror(CompanionSource))]
pub struct _CompanionSource {
    pub descriptor: CompanionSourceDescriptor,
    pub content_type: String,
}

#[frb(mirror(QueueItem))]
pub enum _QueueItem {
    Url {
        url: String,
        content_type: String,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
    FCompanion {
        content_type: String,
        source: CompanionSource,
        metadata: Option<Metadata>,
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

#[frb(mirror(MediaTrackType))]
pub enum _MediaTrackType {
    Video,
    Audio,
    Subtitle,
}

#[frb(mirror(MediaTrack))]
pub struct _MediaTrack {
    pub id: u32,
    pub title: Option<String>,
    pub language: String,
    pub typ: MediaTrackType,
}

#[frb(mirror(TrackList))]
pub struct _TrackList {
    pub tracks: Vec<MediaTrack>,
    pub selected_video: Option<u32>,
    pub selected_audio: Option<u32>,
    pub selected_subtitle: Option<u32>,
}

#[frb(mirror(MediaLocator))]
pub enum _MediaLocator {
    Url { url: String },
    FCompanion { source: CompanionSource },
}

#[frb(mirror(MediaItem))]
pub struct _MediaItem {
    pub content_type: String,
    pub source: MediaLocator,
    pub start_time: Option<f64>,
    pub volume: Option<f64>,
    pub speed: Option<f64>,
    pub request_headers: Option<HashMap<String, String>>,
    pub title: Option<String>,
    pub thumbnail_url: Option<String>,
}

#[frb(mirror(QueueEntry))]
pub struct _QueueEntry {
    pub item: MediaItem,
    pub playback_duration: Option<f64>,
}

#[frb(mirror(Queue))]
pub struct _Queue {
    pub items: Vec<QueueEntry>,
    pub start_index: Option<u32>,
    pub autoplay: bool,
}

#[frb(mirror(QueueState))]
pub struct _QueueState {
    pub items: Vec<QueueEntry>,
    pub current_index: Option<u32>,
    pub autoplay: bool,
}

#[frb(mirror(QueuePosition))]
pub enum _QueuePosition {
    Front,
    Back,
    Index(u8),
}

#[frb(mirror(SubtitleContent))]
pub enum _SubtitleContent {
    Url { url: String },
    Data { data: Vec<u8>, content_type: String },
}

#[frb(mirror(SubtitleSource))]
pub struct _SubtitleSource {
    pub content: SubtitleContent,
    pub select: bool,
    pub name: Option<String>,
}

#[frb(mirror(ReceiverError))]
pub enum _ReceiverError {
    InvalidOpcode,
    ResourceNotFound,
    SeekOutOfRange,
    VolumeOutOfRange,
    RateOutOfRange,
    UnsupportedFormat,
    MalformedBody,
    InvalidState,
    QueuePositionOutOfRange,
    QueueRemovePlayingItem,
    QueueFull,
    InvalidPayloadType,
    Internal,
    Unknown,
}

#[frb(sync)]
pub fn init_logger() {
    // Init can fail if using flutter's "Hot restart"
    let _ = env_logger::Builder::new()
        .filter(None, log::LevelFilter::Debug)
        .try_init();
}

#[frb(sync)]
pub fn enabled_protocols() -> Vec<ProtocolType> {
    let mut protocols = Vec::new();
    #[cfg(feature = "chromecast")]
    protocols.push(ProtocolType::Chromecast);
    #[cfg(feature = "fcast")]
    protocols.push(ProtocolType::FCast);
    protocols
}

#[frb(non_opaque)]
pub enum DeviceEvent {
    ConnectionStateChanged {
        new_state: DeviceConnectionState,
    },
    VolumeChanged {
        new_volume: f64,
    },
    TimeChanged {
        new_time: f64,
    },
    PlaybackStateChanged {
        new_playback_state: PlaybackState,
    },
    DurationChanged {
        new_duration: f64,
    },
    SpeedChanged {
        new_speed: f64,
    },
    SourceChanged {
        new_source: Source,
    },
    TracksAvailable {
        tracks: Vec<MediaTrack>,
    },
    TrackSelected {
        id: Option<u32>,
        typ: MediaTrackType,
    },
    TracksChanged {
        tracks: TrackList,
    },
    QueueChanged {
        queue: QueueState,
    },
    CommandError {
        error: ReceiverError,
    },
    PlaybackStopped,
    PlaybackError {
        message: String,
    },
}

#[frb(opaque)]
pub struct DeviceEventHandler {
    on_event: Box<dyn Fn(DeviceEvent) -> DartFnFuture<()> + Send + Sync + 'static>,
}

impl DeviceEventHandler {
    #[frb(sync)]
    pub fn new(on_event: impl Fn(DeviceEvent) -> DartFnFuture<()> + Send + Sync + 'static) -> Self {
        Self {
            on_event: Box::new(on_event),
        }
    }
}

impl device::DeviceEventHandler for DeviceEventHandler {
    #[frb(ignore)]
    fn connection_state_changed(&self, new_state: DeviceConnectionState) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::ConnectionStateChanged { new_state }).await;
        });
    }

    #[frb(ignore)]
    fn volume_changed(&self, new_volume: f64) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::VolumeChanged { new_volume }).await;
        });
    }

    #[frb(ignore)]
    fn time_changed(&self, new_time: f64) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::TimeChanged { new_time }).await;
        });
    }

    #[frb(ignore)]
    fn playback_state_changed(&self, new_playback_state: PlaybackState) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::PlaybackStateChanged { new_playback_state }).await;
        });
    }

    #[frb(ignore)]
    fn duration_changed(&self, new_duration: f64) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::DurationChanged { new_duration }).await;
        });
    }

    #[frb(ignore)]
    fn speed_changed(&self, new_speed: f64) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::SpeedChanged { new_speed }).await;
        });
    }

    #[frb(ignore)]
    fn source_changed(&self, new_source: Source) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::SourceChanged { new_source }).await;
        });
    }

    #[frb(ignore)]
    fn tracks_available(&self, tracks: Vec<MediaTrack>) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::TracksAvailable { tracks }).await;
        });
    }

    #[frb(ignore)]
    fn track_selected(&self, id: Option<u32>, typ: MediaTrackType) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::TrackSelected { id, typ }).await;
        });
    }

    #[frb(ignore)]
    fn playback_stopped(&self) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::PlaybackStopped).await;
        });
    }

    #[frb(ignore)]
    fn playback_error(&self, message: String) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::PlaybackError { message }).await;
        });
    }

    #[frb(ignore)]
    fn tracks_changed(&self, tracks: TrackList) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::TracksChanged { tracks }).await;
        });
    }

    #[frb(ignore)]
    fn queue_changed(&self, queue: QueueState) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::QueueChanged { queue }).await;
        });
    }

    #[frb(ignore)]
    fn command_error(&self, error: ReceiverError) {
        futures::executor::block_on(async {
            (self.on_event)(DeviceEvent::CommandError { error }).await;
        });
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
    LoadImage,
    LoadPlaylist,
    PlaylistNextAndPrevious,
    SetPlaylistItemIndex,
    WhepStreaming,
    FCompanion,
    FWRTCSignalling,
    ChangeTrack,
    Queue,
    SetProgressUpdateInterval,
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
#[derive(Debug)]
pub enum _LoadRequest {
    Url {
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
        volume: Option<f64>,
        metadata: Option<Metadata>,
        request_headers: Option<HashMap<String, String>>,
    },
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

#[frb(opaque)]
pub struct CastingDevice(Arc<dyn device::CastingDevice>);

impl CastingDevice {
    #[frb(sync)]
    pub fn casting_protocol(&self) -> ProtocolType {
        self.0.casting_protocol()
    }

    #[frb(sync)]
    pub fn is_ready(&self) -> bool {
        self.0.is_ready()
    }

    #[frb(sync)]
    pub fn supports_feature(&self, feature: DeviceFeature) -> bool {
        self.0.supports_feature(feature)
    }

    #[frb(sync)]
    pub fn name(&self) -> String {
        self.0.name()
    }

    #[frb(sync)]
    pub fn set_name(&self, name: String) {
        self.0.set_name(name);
    }

    #[frb(sync)]
    pub fn seek(&self, time_seconds: f64) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.seek(time_seconds))
    }

    #[frb(sync)]
    pub fn stop_playback(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.stop_playback())
    }

    #[frb(sync)]
    pub fn pause_playback(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.pause_playback())
    }

    #[frb(sync)]
    pub fn resume_playback(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.resume_playback())
    }

    /// Load new media.
    ///
    /// `progress_update_interval_millis`, when set, is applied along with the load (see
    /// [`Self::set_progress_update_interval`]).
    #[frb(sync)]
    pub fn load(
        &self,
        request: LoadRequest,
        progress_update_interval_millis: Option<u64>,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.load(request, progress_update_interval_millis))
    }

    /// Request how often the device reports playback progress, in milliseconds. Settable in any
    /// playback state and persists for the rest of the connection. Values are floored to 100 ms.
    /// FCast v4 and Chromecast.
    #[frb(sync)]
    pub fn set_progress_update_interval(
        &self,
        interval_millis: u64,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.set_progress_update_interval(interval_millis))
    }

    #[frb(sync)]
    pub fn playlist_item_next(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.playlist_item_next())
    }

    #[frb(sync)]
    pub fn playlist_item_previous(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.playlist_item_previous())
    }

    /// Set the item index for the currently playing playlist.
    ///
    /// # Arguments
    ///   * `index`: zero-based index into the playlist
    #[frb(sync)]
    pub fn set_playlist_item_index(&self, index: u32) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.set_playlist_item_index(index))
    }

    #[frb(sync)]
    pub fn change_volume(&self, volume: f64) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.change_volume(volume))
    }

    #[frb(sync)]
    pub fn change_speed(&self, speed: f64) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.change_speed(speed))
    }

    #[frb(sync)]
    pub fn disconnect(&self) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.disconnect())
    }

    #[frb(sync)]
    pub fn connect(
        &self,
        app_info: Option<ApplicationInfo>,
        event_handler: DeviceEventHandler,
        reconnect_interval_millis: u32,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.connect(
            app_info,
            Arc::new(event_handler),
            reconnect_interval_millis as u64
        ))
    }

    #[frb(sync)]
    pub fn get_device_info(&self) -> DeviceInfo {
        self.0.get_device_info()
    }

    #[frb(sync)]
    pub fn get_addresses(&self) -> Vec<IpAddr> {
        self.0.get_addresses()
    }

    #[frb(sync)]
    pub fn set_addresses(&self, addrs: Vec<IpAddr>) {
        self.0.set_addresses(addrs);
    }

    #[frb(sync)]
    pub fn get_port(&self) -> u16 {
        self.0.get_port()
    }

    #[frb(sync)]
    pub fn set_port(&self, port: u16) {
        self.0.set_port(port);
    }

    /// Select (or, with `id = None`, disable) a track of the given type. FCast v4 only.
    #[frb(sync)]
    pub fn change_track(
        &self,
        id: Option<u32>,
        track_type: MediaTrackType,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.change_track(id, track_type))
    }

    /// Load a queue of media items and begin playback. FCast v4 only.
    #[frb(sync)]
    pub fn load_queue(&self, queue: Queue) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.load_queue(queue))
    }

    /// Insert an item into the active queue. FCast v4 only.
    #[frb(sync)]
    pub fn queue_insert(
        &self,
        item: MediaItem,
        playback_duration: Option<f64>,
        position: QueuePosition,
    ) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.queue_insert(item, playback_duration, position))
    }

    /// Remove an item from the active queue. FCast v4 only.
    #[frb(sync)]
    pub fn queue_remove(&self, position: QueuePosition) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.queue_remove(position))
    }

    /// Select (jump to) an item in the active queue. FCast v4 only.
    #[frb(sync)]
    pub fn queue_select(&self, position: QueuePosition) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.queue_select(position))
    }

    /// Add an external subtitle source to the current media. FCast v4 only.
    #[frb(sync)]
    pub fn add_subtitle_source(&self, subtitle: SubtitleSource) -> Result<(), _CastingDeviceError> {
        device_error_converter!(self.0.add_subtitle_source(subtitle))
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
