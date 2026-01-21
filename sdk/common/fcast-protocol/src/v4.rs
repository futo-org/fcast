use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::skip_serializing_none;
use serde_repr::{Deserialize_repr, Serialize_repr};

#[skip_serializing_none]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MetadataObject {
    #[serde(rename = "GENERIC")]
    Generic {
        title: Option<String>,
        thumbnail_url: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        custom: Option<Value>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum PlaybackState {
    Idle = 0,
    Playing = 1,
    Paused = 2,
    Ended = 3,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PlaybackUpdateMessage {
    // The playback state
    pub state: PlaybackState,
    // The current time playing in seconds
    pub time: Option<f64>,
    // The duration in seconds
    pub duration: Option<f64>,
    // The playback speed factor
    pub speed: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaItemSource {
    #[serde(rename = "URL")]
    Url {
        url: String,
    },
    #[serde(rename = "CONTENT")]
    Content {
        content: String,
    },
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaItem {
    /// The MIME type (video/mp4)
    pub container: String,
    pub source: MediaItemSource,

    // The URL to load (optional)
    // pub url: Option<String>,
    // The content to load (i.e. a DASH manifest, optional)
    // TODO: can this be merged with url? (data:<mime>,b64: etc.)
    // pub content: Option<String>,

    // The time to start playing in seconds
    pub time: Option<f64>,
    // The desired volume (0-1)
    pub volume: Option<f64>,
    // Initial playback speed
    pub speed: Option<f64>,
    // HTTP request headers to add to the play request
    pub headers: Option<HashMap<String, String>>,
    pub metadata: Option<MetadataObject>,
}

#[skip_serializing_none]
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PlayUpdateMessage {
    #[serde(rename = "playData")]
    pub play_data: PlayMessage,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub media_item: MediaItem,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlayMessage {
    #[serde(rename = "Single")]
    Single {
        media_item: MediaItem,
    },
    #[serde(rename = "QUEUE")]
    Queue {
        items: Vec<QueueItem>,
        #[serde(rename = "startIndex")]
        start_index: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueuePosition {
    #[serde(rename = "INDEX")]
    Index {
        index: u32,
    },
    #[serde(rename = "FRONT")]
    Front,
    #[serde(rename = "BACK")]
    Back,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueueMessage {
    #[serde(rename = "INSERT")]
    Insert {
        item: QueueItem,
        position: QueuePosition,
    },
    #[serde(rename = "REMOVE")]
    Remove {
        position: QueuePosition,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueUpdatedMessage {
    msg: QueueMessage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItemSelectedMessage {
    pub position: QueuePosition,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTrack {
    pub id: u32,
    pub name: String,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub id: u32,
    pub name: String,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub id: u32,
    pub name: String,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracksAvailableMessage {
    // TODO: include the currently selected one?
    pub videos: Option<Vec<VideoTrack>>,
    pub audios: Option<Vec<AudioTrack>>,
    pub subtitles: Option<Vec<SubtitleTrack>>,
}

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

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeTrackMessage {
    pub id: u32,
    #[serde(rename = "trackType")]
    pub track_type: TrackType,
}

/// Sent when a sender or user changes the track with fcast or in the GUI
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackChangedMessage {
    pub request: ChangeTrackMessage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetSubtitleUrlMessage {
    url: String,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct InitialSenderMessage {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "appName")]
    pub app_name: Option<String>,
    #[serde(rename = "appVersion")]
    pub app_version: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct LivestreamCapabilities {
    /// https://datatracker.ietf.org/doc/draft-murillo-whep/
    pub whep: Option<bool>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct AVCapabilities {
    pub livestream: Option<LivestreamCapabilities>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct ReceiverCapabilities {
    pub av: Option<AVCapabilities>,
}

#[skip_serializing_none]
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
pub struct InitialReceiverMessage {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "appName")]
    pub app_name: Option<String>,
    #[serde(rename = "appVersion")]
    pub app_version: Option<String>,
    #[serde(rename = "playData")]
    pub play_data: Option<PlayMessage>,
    #[serde(rename = "experimentalCapabilities")]
    pub experimental_capabilities: Option<ReceiverCapabilities>,
}
