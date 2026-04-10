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
Note the maximum total packet size is 128KB. Consequently, the maximum body size is `128000 - 1` unless stated otherwise.

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

{{ play_message }}

### Seek

Protocol

{{ seek_message }}

### UpdateVolume

{{ update_volume_message }}

### UpdatePlaybackState

{{ update_playback_state_message }}

### PlaybackError

{{ playback_error_message}}

### SetSpeed

{{ set_speed_message }}

### Version

{{ version_message }}

### Heartbeat

#### Ping

#### Pong

### Initial

#### From sender

{{ initial_sender_message }}

#### From receiver

{{ initial_receiver_message }}

### PositionChanged

Sent from the receiver after seeks and at regular intervals.

{{ position_changed_message }}

### DurationChanged

{{ duration_changed_message }}

### TracksAvailable

{{ tracks_available_message }}

### ChangeTrack

{{ change_track_message }}

### QueueInsert

{{ queue_insert_message }}

### QueueRemove

{{ queue_remove_message }}

### QueueItemSelected

{{ queue_item_selected_message }}

### AddSubtitleSource

{{ add_subtitle_source_message }}

### SetStatusUpdateInterval

{{ set_status_update_interval_message }}

### StartTLS

Sent from the sender to the receiver when it wish to secure the connection. When the receiver receives the message, it will respond with a `StartTLS` message. When the sender receives this, it should start the TLS handshake.

# Shared types

### MediaItem

{{ media_item }}

### Metadata

{{ metadata }}

### PlaybackState

{{ playback_state }}

### QueueItem

{{ queue_item }}

### QueuePosition

{{ queue_position }}

### MediaTrack

{{ media_track }}

### MediaTrackMetadata

{{ media_track_metadata }}

### TrackType

{{ track_type }}

### DeviceInfo

{{ device_info }}

### ReceiverCapabilities

{{ receiver_capabilities }}

### MediaCapabilities

{{ media_capabilities }}

### DisplayCapabilities

{{ display_capabilities }}

### VideoResolution

{{ video_resolution}}

# FCompanion

TODO: implement this in the desktop sender and maybe immich to verify it's a reasonable protocol.
TODO: support webrtc signaling over FCompanion

This section defines the FCompanion protocol used to transfer media data over an FCast connection. The bodies are in a custom binary format, and they can be of any size not exceeding `2^32 - 2`.

URLs are defined like this:

`fcomp://<provider-id>.fcast/<resource-id>`

 - `provider-id` is a `U16` and `resource-id` is a `U32` encoded as ASCII.

The receiver implementation **must** support the case where a sender plays a companion URL provided by a different connection to the same receiver. This is to allow more flexibility for sender developers.

### CompanionHello

This message is sent from the sender to the receiver to get a companion provider ID. When the receiver gets the message, it will generate a unique ID for the sender's content provider and respond with the same opcode with the following body.

| Arg. # | Type  | Description |
|--------|-------|-------------|
| 1      | U16LE | Provider ID |

### ResourceInfo

#### Request

| Arg. # | Type  | Description |
|--------|-------|-------------|
| 1      | U32LE | Request ID  |
| 2      | U32LE | Resource ID |

#### Response

| Arg. # | Type           | Description    |
|--------|----------------|----------------|
| 1      | U32LE          | Request ID     |
| 2      | [String]       | Content Type   |
| 3      | [ResourceSize] | Resource Size. |

### Resource

#### Request

| Arg. # | Type       | Description |
|--------|------------|-------------|
| 1      | U32LE      | Request ID  |
| 2      | U32LE      | Resource ID |
| 3      | [ReadHead] | Read head   |

#### Response

| Arg. # | Type               | Description |
|--------|--------------------|-------------|
| 1      | U32LE              | Request ID  |
| 2      | [GetResouceResult] | Result      |

## Shared types

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
| 1       | U48LE | Start            |
| 2       | U48LE | Stop (inclusive) |

### GetResouceResult

A single byte (`U8`) called `variant` with optional extra data.

The values of `variant` are:

| Value | Extra Data | Description                |
|-------|------------|----------------------------|
| 0x00  | NONE       | The resource was not found |
| 0x01  | \[U8\]     | Success                    |

The length of the success array is calculated by subtracting the response size from the total message size.

### ResourceSize

A single byte (`U8`) called `variant` with optional extra data.

The values of `variant` are:

| Value | Extra Data | Description                         |
|-------|------------|-------------------------------------|
| 0x00  | NONE       | Resource Size is unknown            |
| 0x01  | U48LE      | A range of bytes from the resource. |

[String]: #string
[ReadHead]: #readhead
[Range]: #range
[ResourceSize]: #resourcesize
[UTF-8]: https://en.wikipedia.org/wiki/UTF-8
[GetResouceResult]: #getresouceresult
[ResourceData]: #resourcedata
[TLS]: https://en.wikipedia.org/wiki/Transport_Layer_Security
