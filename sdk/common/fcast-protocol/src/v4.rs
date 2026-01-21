use std::collections::HashMap;

#[cfg(feature = "__schema")]
use get_type_string_derive::GetTypeString;
#[cfg(feature = "__schema")]
use schemars::{JsonSchema, JsonSchema_repr};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_repr::{Deserialize_repr, Serialize_repr};
use serde_with::skip_serializing_none;
use smol_str::SmolStr;

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Metadata {
    #[serde(rename = "VIDEO")]
    Video { subtitle_url: Option<String> },
    #[serde(rename = "AUDIO")]
    Audio {
        artist: Option<String>,
        album: Option<String>,
    },
}

#[cfg_attr(feature = "__schema", derive(JsonSchema_repr, GetTypeString))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum PlaybackState {
    Idle = 0,
    Buffering = 1,
    Playing = 2,
    Paused = 3,
    Ended = 4,
    Stopped = 5,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PositionChangedMessage {
    pub position: f64,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct DurationChangedMessage {
    pub duration: f64,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct UpdatePlaybackStateMessage {
    pub state: PlaybackState,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaItem {
    /// The MIME type
    pub container: String,
    pub source_url: String,
    // The time to start playing in seconds
    pub start_time: Option<f64>,
    // The desired volume (0-1)
    pub volume: Option<f64>,
    // Initial playback speed
    pub speed: Option<f64>,
    // HTTP request headers to add to the play request
    pub headers: Option<HashMap<String, String>>,
    // pub info: Option<MediaItemInformation>,
    pub title: Option<String>,
    pub thumbnail_url: Option<String>,
    pub metadata: Option<Metadata>,
    pub extra_metadata: Option<HashMap<String, Value>>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub media_item: MediaItem,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlayMessage {
    #[serde(rename = "SINGLE")]
    Single { media_item: MediaItem },
    #[serde(rename = "QUEUE")]
    Queue {
        items: Vec<QueueItem>,
        start_index: u32,
    },
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueuePosition {
    #[serde(rename = "INDEX")]
    Index { index: u32 },
    #[serde(rename = "FRONT")]
    Front,
    #[serde(rename = "BACK")]
    Back,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueInsertMessage {
    item: QueueItem,
    position: QueuePosition,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueRemoveMessage {
    position: QueuePosition,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItemSelectedMessage {
    pub position: QueuePosition,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaTrackMetadata {
    #[serde(rename = "VIDEO")]
    Video { resolution: Option<VideoResolution> },
    #[serde(rename = "AUDIO")]
    Audio {
        /// ISO 639
        language_code: Option<String>,
    },
    #[serde(rename = "SUBTITLE")]
    Subtitle {
        /// ISO 639
        language_code: Option<String>,
    },
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaTrack {
    pub id: u32,
    pub name: String,
    pub metadata: MediaTrackMetadata,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracksAvailableMessage {
    pub videos: Option<Vec<MediaTrack>>,
    pub audios: Option<Vec<MediaTrack>>,
    pub subtitles: Option<Vec<MediaTrack>>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackType {
    #[serde(rename = "VIDEO")]
    Video,
    #[serde(rename = "AUDIO")]
    Audio,
    #[serde(rename = "SUBTITLE")]
    Subtitle,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeTrackMessage {
    /// When `id` is null, the receiver should disable playback of `track_type` (e.g. turn of subtitles)
    pub id: Option<u32>,
    pub track_type: TrackType,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetSubtitleUrlMessage {
    url: String,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub display_name: Option<String>,
    pub app_name: Option<String>,
    pub app_version: Option<String>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct InitialSenderMessage {
    pub device_info: DeviceInfo,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct DisplayCapabilities {
    pub resolution: Option<VideoResolution>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct MediaCapabilities {
    /// e.g. application/http, application/x-rtsp, application/x-whep
    pub protocols: Vec<SmolStr>,
    /// e.g. video/mp4, video/webm, audio/ogg
    pub containers: Vec<SmolStr>,
    // TODO: find good common format strings for the following fields like
    // https://developer.mozilla.org/en-US/docs/Web/Media/Guides/Formats/codecs_parameter
    pub video_formats: Vec<SmolStr>,
    pub audio_formats: Vec<SmolStr>,
    pub subtitle_formats: Vec<SmolStr>,
    pub hdr_formats: Vec<SmolStr>,
    /// e.g. png, jepg, jp2, heif, heic, avif
    pub image_formats: Vec<SmolStr>,
    pub external_subtitles: bool,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct ReceiverCapabilities {
    pub media: MediaCapabilities,
    pub display: Option<DisplayCapabilities>,
    pub companion: Option<bool>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[skip_serializing_none]
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
pub struct InitialReceiverMessage {
    pub device_info: DeviceInfo,
    pub app_version: Option<String>,
    pub capabilities: Option<ReceiverCapabilities>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SeekMessage {
    pub time: f64,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct UpdateVolumeMessage {
    pub volume: f64,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SetSpeedMessage {
    pub speed: f64,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AddSubtitleSourceMessage {
    pub url: String,
    /// Whether this track should be selected immediately
    pub select: bool,
    pub name: Option<String>,
    /// Should only be an instance of `SUBTITLE`
    pub metadata: Option<MediaTrackMetadata>,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SetStatusUpdateIntervalMessage {
    /// Milliseconds
    pub interval: u32,
}
