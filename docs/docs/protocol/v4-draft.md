# [DRAFT] Version 4

TODO: link to json schema definition

# Overview

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

For opcodes defining a body, the format will be a `JSON` string encoded using `UTF-8`, unless stated otherwise.
Note the total packet size is max 128KB. Consequently, the maximum body size is `128000 - 1` unless stated otherwise.

| Opcode | Name                    | Direction | Description                   |
|--------|-------------------------|-----------|-------------------------------|
| 1      | Play                    | Both      | [↗](#play)                    |
| 5      | Seek                    | S->R      | [↗](#seek)                    |
| 9      | PlaybackError           | R->S      | [↗](#playbackerror)           |
| 10     | SetSpeed                | Both      | [↗](#setspeed)                |
| 11     | Version                 | Both      | [↗](#version)                 |
| 12     | Ping                    | Both      | [↗](#heartbeat)               |
| 13     | Pong                    | Both      | [↗](#heartbeat)               |
| 14     | Initial                 | Both      | [↗](#initial)                 |
| 20     | UpdateVolume            | Both      | [↗](#updatevolume)            |
| 21     | UpdatePlaybackState     | Both      | [↗](#updateplaybackstate)     |
| 22     | PositionChanged         | R->S      | [↗](#positionchanged)         |
| 23     | DurationChanged         | R->S      | [↗](#durationchanged)         |
| 24     | QueueInsert             | Both      | [↗](#queueinsert)             |
| 25     | QueueRemove             | Both      | [↗](#queueremove)             |
| 26     | TracksAvailable         | R->S      | [↗](#tracksavailable)         |
| 27     | ChangeTrack             | Both      | [↗](#changetrack)             |
| 28     | QueueItemSelected       | Both      | [↗](#queueitemselected)       |
| 29     | AddSubtitleSource       | S->R      | [↗](#addsubtitlesource)       |
| 30     | SetStatusUpdateInterval | S->R      | [↗](#setstatusupdateinterval) |
| 50     | CompanionHello          | S->R      | [↗](#hompanionhello)          |
| 51     | ResourceInfo            | Both      | [↗](#resourceinfo)            |
| 52     | Resource                | Both      | [↗](#resource)                |
| 53     | StartTLS                | Both      | [↗](#starttls)                |

### Connection establishment

When a sender or receiver establishes a connection with the other party, it **must** send a `Version` message to indicate which messages and protocol features are supported.

When there is a mismatch of support protocol versions among devices, the device with the higher version number must either error out/disconnect or use a downgraded feature set compatible with the other party's protocol version.

For protocol v3 and above, after determining the supported protocol version from the other party, the `Initial` message **must** be sent to the other party.

### Device state synchronization

The protocol allows for multiple senders to connect to a single receiver. To synchronize the play/control state between all sender devices, the receiver relays most messages that mutates the state of the receiver.
For example, S1 sends UpdateVolume(50%) to R, once R has successfully changed the volume it will send that same message to S1 and any other senders connected. The same applies to `Play`, `UpdatePlaybackState`, `SetSpeed`, `QueueInsert`, `QueueRemove`.

## Security

Based of the [Open Screen Network Protocol](https://www.w3.org/TR/openscreen-network/).

TODO: someone with crypto knowledge must verify that it's actually secure.

Version 4 requires an encrypted connection. [TLS] is used as the cryptographic protocol. The receiver will include a `fp` (fingerprint) DNS TXT record key where the value is the base64 encoded SHA256 hash of the receiver certificate's SPKI fingerprint.

## Messages

### Play

```rust title="Body"
enum PlayMessage {
    #[serde(rename = "SINGLE")]
    Single { media_item: MediaItem },
    #[serde(rename = "QUEUE")]
    Queue { items: Vec<QueueItem>, start_index: u32 },
}
```

### Seek

Protocol

```rust title="Body"
struct SeekMessage {
    time: f64,
}
```

### UpdateVolume

```rust title="Body"
struct UpdateVolumeMessage {
    volume: f64,
}
```

### UpdatePlaybackState

```rust title="Body"
struct UpdatePlaybackStateMessage {
    state: PlaybackState,
}
```

### PlaybackError

```rust title="Body"
struct PlaybackErrorMessage {
    message: String,
}
```

### SetSpeed

```rust title="Body"
struct SetSpeedMessage {
    speed: f64,
}
```

### Version

```rust title="Body"
struct VersionMessage {
    version: u64,
}
```

### Heartbeat

#### Ping

#### Pong

### Initial

#### From sender

```rust title="Body"
struct InitialSenderMessage {
    device_info: DeviceInfo,
}
```

#### From receiver

```rust title="Body"
struct InitialReceiverMessage {
    device_info: DeviceInfo,
    app_version: Option<String>,
    capabilities: Option<ReceiverCapabilities>,
}
```

### PositionChanged

Sent from the receiver after seeks and at regular intervals.

```rust title="Body"
struct PositionChangedMessage {
    position: f64,
}
```

### DurationChanged

```rust title="Body"
struct DurationChangedMessage {
    duration: f64,
}
```

### TracksAvailable

```rust title="Body"
struct TracksAvailableMessage {
    videos: Option<Vec<MediaTrack>>,
    audios: Option<Vec<MediaTrack>>,
    subtitles: Option<Vec<MediaTrack>>,
}
```

### ChangeTrack

```rust title="Body"
struct ChangeTrackMessage {
    /// When `id` is null, the receiver should disable playback of `track_type` (e.g. turn of subtitles)
    id: Option<u32>,
    track_type: TrackType,
}
```

### QueueInsert

```rust title="Body"
struct QueueInsertMessage {
    item: QueueItem,
    position: QueuePosition,
}
```

### QueueRemove

```rust title="Body"
struct QueueRemoveMessage {
    position: QueuePosition,
}
```

### QueueItemSelected

```rust title="Body"
struct QueueItemSelectedMessage {
    position: QueuePosition,
}
```

### AddSubtitleSource

```rust title="Body"
struct AddSubtitleSourceMessage {
    url: String,
    /// Whether this track should be selected immediately
    select: bool,
    name: Option<String>,
    /// Should only be an instance of `SUBTITLE`
    metadata: Option<MediaTrackMetadata>,
}
```

### SetStatusUpdateInterval

```rust title="Body"
struct SetStatusUpdateIntervalMessage {
    /// Milliseconds
    interval: u32,
}
```

### StartTLS

Sent from the sender to the receiver when it wish to secure the connection. When the receiver receives the message, it will respond with a `StartTLS` message. When the sender receives this, it should start the TLS handshake.

# Shared types

### MediaItem

```rust title="Body"
struct MediaItem {
    /// The MIME type
    container: String,
    source_url: String,
    start_time: Option<f64>,
    volume: Option<f64>,
    speed: Option<f64>,
    headers: Option<HashMap<String, String>>,
    title: Option<String>,
    thumbnail_url: Option<String>,
    metadata: Option<Metadata>,
    extra_metadata: Option<HashMap<String, Value>>,
}
```

### Metadata

```rust title="Body"
enum Metadata {
    #[serde(rename = "VIDEO")]
    Video { subtitle_url: Option<String> },
    #[serde(rename = "AUDIO")]
    Audio { artist: Option<String>, album: Option<String> },
}
```

### PlaybackState

```rust title="Body"
enum PlaybackState {
    Idle = 0,
    Buffering = 1,
    Playing = 2,
    Paused = 3,
    Ended = 4,
    Stopped = 5,
}
```

### QueueItem

```rust title="Body"
struct QueueItem {
    media_item: MediaItem,
}
```

### QueuePosition

```rust title="Body"
enum QueuePosition {
    #[serde(rename = "INDEX")]
    Index { index: u32 },
    #[serde(rename = "FRONT")]
    Front,
    #[serde(rename = "BACK")]
    Back,
}
```

### MediaTrack

```rust title="Body"
struct MediaTrack {
    id: u32,
    name: String,
    metadata: MediaTrackMetadata,
}
```

### MediaTrackMetadata

```rust title="Body"
enum MediaTrackMetadata {
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
```

### TrackType

```rust title="Body"
enum TrackType {
    #[serde(rename = "VIDEO")]
    Video,
    #[serde(rename = "AUDIO")]
    Audio,
    #[serde(rename = "SUBTITLE")]
    Subtitle,
}
```

### DeviceInfo

```rust title="Body"
struct DeviceInfo {
    display_name: Option<String>,
    app_name: Option<String>,
    app_version: Option<String>,
}
```

### ReceiverCapabilities

```rust title="Body"
struct ReceiverCapabilities {
    media: MediaCapabilities,
    display: Option<DisplayCapabilities>,
    companion: Option<bool>,
}
```

### MediaCapabilities

```rust title="Body"
struct MediaCapabilities {
    /// e.g. application/http, application/x-rtsp, application/x-whep
    protocols: Vec<SmolStr>,
    /// e.g. video/mp4, video/webm, audio/ogg
    containers: Vec<SmolStr>,
    video_formats: Vec<SmolStr>,
    audio_formats: Vec<SmolStr>,
    subtitle_formats: Vec<SmolStr>,
    hdr_formats: Vec<SmolStr>,
    /// e.g. png, jepg, jp2, heif, heic, avif
    image_formats: Vec<SmolStr>,
    external_subtitles: bool,
}
```

### DisplayCapabilities

```rust title="Body"
struct DisplayCapabilities {
    resolution: Option<VideoResolution>,
}
```

### VideoResolution

```rust title="Body"
struct VideoResolution {
    width: u32,
    height: u32,
}
```

# FCompanion

TODO: implement this in Grayjay to verify it's a reasonable protocol.

This section defines the FCompanion protocol used to transfer media data over an FCast connection. The bodies are in a custom binary format, and they can be of any size not exceeding `2^32 - 2`.

URLs are defined like this:

`fcomp://<server-uuid>.fcast/<resource-uuid>`

The receiver implementation **must** support the case where a sender plays a companion URL provided by a different connection to the same receiver. This is to allow more flexibility for sender developers.

### CompanionHello

This message is sent from the sender to the receiver to notify that the sender provides resources for the specified server ID.

| Arg. # | Type   | Description |
|--------|--------|-------------|
| 1      | [UUID] | Server ID   |

### ResourceInfo

#### Request

| Arg. # | Type   | Description |
|--------|--------|-------------|
| 1      | U32LE  | Request ID  |
| 2      | [UUID] | Resource ID |

#### Response

| Arg. # | Type     | Description                                             |
|--------|----------|---------------------------------------------------------|
| 1      | U32LE    | Request ID                                              |
| 2      | [String] | Content Type                                            |
| 3      | S64LE    | Resource Size. A value of -1 means the size is unknown. |

### Resource

#### Request

| Arg. # | Type       | Description |
|--------|------------|-------------|
| 1      | U32LE      | Request ID  |
| 2      | [UUID]     | Resource ID |
| 3      | [ReadHead] | Read head   |

#### Response

| Arg. # | Type               | Description |
|--------|--------------------|-------------|
| 1      | U32LE              | Request ID  |
| 2      | [GetResouceResult] | Result      |

## Shared types

### UUID

| Field # | Type | Description                                                                               |
|---------|------|-------------------------------------------------------------------------------------------|
| 1       | [U8] | A 16 bytes version 4 [UUID](https://en.wikipedia.org/wiki/Universally_unique_identifier). |

### String

All strings are [UTF-8] encoded.

| Field # | Type   | Description                                             |
|---------|--------|---------------------------------------------------------|
| 1       | U16LE  | Length                                                  |
| 2       | \[U8\] | Array of bytes with the length defined by the field `1` |

### ReadHead

A single byte (`U8`) called `variant` with optional extra data.

The values of `variant` are:

| Value | Extra Data | Description                         |
|-------|------------|-------------------------------------|
| 0x00  | NONE       | The whole resource.                 |
| 0x01  | [Range]    | A range of bytes from the resource. |

### Range

| Field # | Type  | Description      |
|---------|-------|------------------|
| 1       | U64LE | Start            |
| 2       | U64LE | Stop (inclusive) |

### GetResouceResult

A single byte (`U8`) called `variant` with optional extra data.

The values of `variant` are:

| Value | Extra Data     | Description                |
|-------|----------------|----------------------------|
| 0x00  | NONE           | The resource was not found |
| 0x01  | [ResourceData] | Success                    |

### ResourceData

| Field # | Type  | Description                                        |
|---------|-------|----------------------------------------------------|
| 1       | U64LE | Content length                                     |
| 2       | \[U8\]  | Resource data with length defined by argument `1`. |

[UUID]: #uuid
[String]: #string
[ReadHead]: #readhead
[Range]: #range
[UTF-8]: https://en.wikipedia.org/wiki/UTF-8
[GetResouceResult]: #getresouceresult
[ResourceData]: #resourcedata
[TLS]: https://en.wikipedia.org/wiki/Transport_Layer_Security