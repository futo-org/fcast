# [DRAFT] Version 4

TODO: Generate schemas from rust code with https://docs.rs/schemars/latest/schemars/

## Overview

The protocol is a TCP protocol on port `46899`.

The header packet structure is defined as:

```c
struct HEADER {
   uint32_t size; //Little Endian
   uint8_t opcode;
} header;
```

For a packet with no body, only the header is sent. In this case, `size = 1`.

When a body is attached, it has the following format.

```c
struct BODY {
   uint8_t bytes[header.size - 1];
};
```

The body is a `JSON` string, encoded using `UTF-8`. Note the total packet size is max 32KB. Consequently, the maximum body size is `32000 - 1`.

| Name           | Opcode | Description                                                                                                                                         |
|----------------|--------|-----------------------------------------------------------------------------------------------------------------------------------------------------|
| None           | 0      | Not used                                                                                                                                            |
| Play           | 1      | Sender message to play media content, body is `PlayMessage`                                                                                         |
| Pause          | 2      | Sender message to pause media content, no body                                                                                                      |
| Resume         | 3      | Sender message to resume media content, no body                                                                                                     |
| Stop           | 4      | Sender message to stop media content, no body                                                                                                       |
| Seek           | 5      | Sender message to seek, body is `SeekMessage`                                                                                                       |
| PlaybackUpdate | 6      | Receiver message to notify an updated playback state, body is `PlaybackUpdateMessage`                                                               |
| VolumeUpdate   | 7      | Receiver message to notify when the volume has changed, body is `VolumeUpdateMessage`                                                               |
| SetVolume      | 8      | Sender message to change volume, body is `SetVolumeMessage`                                                                                         |
| PlaybackError  | 9      | Server message to notify the sender a playback error happened, body is `PlaybackErrorMessage`                                                       |
| SetSpeed       | 10     | Sender message to change playback speed, body is `SetSpeedMessage`                                                                                  |
| Version        | 11     | Message to notify the other of the current version, body is `VersionMessage`                                                                        |
| Ping           | 12     | Message to get the other party to pong, no body                                                                                                     |
| Pong           | 13     | Message to respond to a ping from the other party, no body                                                                                          |
| Initial        | 14     | Message to notify the other party of device information and state, body is `InitialSenderMessage` if receiver or `InitialReceiverMessage` if sender |
| PlayUpdate     | 15     | Receiver message to notify all senders when any device has sent a `Play` message, body is `PlayUpdateMessage`                                       |
| Queue          | 16     |                                                                                                                                                     |

#### Connection establishment

When a sender or receiver establishes a connection with the other party, it **must** send a `Version` message to indicate which messages and protocol features are supported.

When there is a mismatch of support protocol versions among devices, the device with the higher version number must either error out/disconnect or use a downgraded feature set compatible with the other party's protocol version.

For protocol v3 and above, after determining the supported protocol version from the other party, the `Initial` message **must** be sent to the other party synchronize the connection state.

#### Device state synchronization

The protocol allows for multiple senders to connect to a single receiver. To synchronize the play/control state between all sender devices, the following messages are used:

* `Initial`: Sent by the receiver upon connection establishment with a sender. Likewise the sender also sends the same message to the receiver upon connection establishment
* `PlayUpdate`: Sent by the receiver when the played content has changed
* `PlaybackUpdate`: Sent by the receiver whenever there is a change in the playback state of the media item
* `VolumeUpdate`: Sent by the receiver when a change of volume has been made on the receiver or requested from another sender
* `PlaybackError`: Sent by the receiver when a receiver-side play error has occurred

## Bodies

#### `PlayMessage`

```rust
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueueItem {
    pub media_item: MediaItem,
    pub show_duration: Option<u32>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum PlayMessage {
    #[serde(rename = "Single")]
    Single {
        item: MediaItem,
    },
    #[serde(rename = "QUEUE")]
    Queue {
        items: Vec<QueueItem>,
        #[serde(rename = "startIndex")]
        start_index: u32,
    },
}
```

#### `SeekMessage`

Protocol

```rust
struct SeekMessage {
    time: f64,
}
```

#### `PlaybackUpdateMessage`

```rust
struct PlaybackUpdateMessage {
    /// The time the packet was generated (unix time milliseconds)
    generationTime: u64,
    /// The playback state
    state: crate::PlaybackState,
    /// The current time playing in seconds
    time: Option<f64>,
    /// The duration in seconds
    duration: Option<f64>,
    /// The playback speed factor
    speed: Option<f64>,
    /// The playlist item index currently being played on receiver
    itemIndex: Option<u64>,
}
```

The playback state are defined as follows.
```rust
enum PlaybackState {
    Idle = 0,
    Playing = 1,
    Paused = 2,
    Ended = 3,
}
```

#### `VolumeUpdateMessage`

```rust
struct VolumeUpdateMessage {
    generationTime: u64,
    volume: f64, //(0-1)
}
```

#### `SetVolumeMessage`

```rust
struct SetVolumeMessage {
    volume: f64,
}
```

#### `PlaybackErrorMessage`

```rust
struct PlaybackErrorMessage {
    message: String,
}
```

#### `SetSpeedMessage`

```rust
struct SetSpeedMessage {
    speed: f64,
}
```

#### `VersionMessage`

```rust
struct VersionMessage {
    version: u64,
}
```

#### `InitialSenderMessage`

```rust
struct InitialSenderMessage {
    displayName: Option<String>,
    appName: Option<String>,
    appVersion: Option<String>,
}
```

#### `InitialReceiverMessage`

```rust
struct LivestreamCapabilities {
    /// https://datatracker.ietf.org/doc/draft-murillo-whep/
    whep: Option<bool>,
}

struct AVCapabilities {
    livestream: Option<LivestreamCapabilities>,
}

struct ReceiverCapabilities {
    av: Option<AVCapabilities>,
}
```

```rust
struct InitialReceiverMessage {
    displayName: Option<String>,
    appName: Option<String>,
    appVersion: Option<String>,
    playData: Option<PlayMessage>,
    experimentalCapabilities: Option<ReceiverCapabilities>,
}
```

#### `PlayUpdateMessage`

```rust
struct PlayUpdateMessage {
    generationTime: Option<u64>,
    playData: Option<PlayMessage>,
}
```

#### `QueueMessage`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum QueuePosition {
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
enum QueueMessage {
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
```

#### `TracksAvailableMessage`

```rust
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
```

```rust
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
```
