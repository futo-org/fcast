# Protocol

FCast is an open source protocol for multimedia content streaming and playback control between devices on a local network. A **sender** controls what is played, while a **receiver** is the device that displays the casted media. The protocol is media-format agnostic. Any content type the receiver supports can be cast, such as streaming formats like DASH and HLS, or media content like video, audio, and images.

Unlike proprietary protocols like Chromecast and AirPlay, FCast is fully open — anyone can implement a sender or receiver on any platform. Receivers exist for Android, Linux, Windows, macOS, webOS, and Tizen.

## Architecture

The sender discovers a receiver on the local network, establishes a connection, and directs it to play media from a remote source. The receiver fetches the media directly and reports playback state back to the sender.

``` mermaid
graph TD
  M@{ shape: cloud, label: "Media Source" }
  S(Sender)
  R(Receiver)

  M -.->|media| R
  S -->|playback control| R
  R -->|state updates| S
```

The sender can also serve as the media source itself, proxying streams or local files directly to the receiver. This is also how screen mirroring works, as the sender captures and streams its display to the receiver in real time.

## Protocol Components

The protocol is organized into the following areas, each covered in detail in the version specification:

- **Connecting**: Finding receivers on the local network via mDNS, connecting via QR codes and direct IP
- **Session Management**: Establishing connections, connection liveness, and error signaling
- **Playback Control**: Play, pause, seek, volume, speed, and more
- **Queue Management**: Media item playlists and modification
- **State Synchronization**: Receiver state updates and multi-sender state synchronization

## Versions

### [Version 4](v4.md)

Summary: TODO

temp changes

- protocol overhaul simplification (compatibility broken, using new port)
- removed event system (replicated at sdk level)
- changed packet structure format
- replaced playlist with queue
- other enhancements (device capabilities, metadata improvements)

??? note "Changelog"

    **Breaking Changes**

    - TODO

    **New Features**

    - TODO

    **Message Changes**

    - TODO

### Previous Versions (Deprecated)

!!! warning
    Versions 1-3 are deprecated. Support may be removed in later releases. New implementations should target Version 4.

- [Version 3](v3.md)
- [Version 2](v2.md)
- [Version 1](v1.md)

??? note "Changelog"

    #### Version 3

    **General Changes**

    - Added support for media item metadata
    - Added support for media playlists
    - Added support for receiver event subscription of media and keypress events
    - Improved multi-device state synchronization

    **Message Changes**

    - `PlayMessage`: Added `volume` and `metadata` fields
    - `PlaybackUpdateMessage`: Added `itemIndex` field and allow other fields except `generationTime` and `state` to be `null`

    **New Opcodes**

    - 14: `Initial`
    - 15: `PlayUpdate`
    - 16: `SetPlaylistItem`
    - 17: `SubscribeEvent`
    - 18: `UnsubscribeEvent`
    - 19: `EventMessage`

    #### Version 2

    **Message Changes**

    - `PlayMessage`: Added `speed` and `headers` fields
    - `PlaybackUpdateMessage`: Added `generationTime`, `duration`, and `speed` fields
    - `VolumeUpdateMessage`: Added `generationTime` field

    **New Opcodes**

    - 9: `PlaybackError`
    - 10: `SetSpeed`
    - 11: `Version`
    - 12: `Ping`
    - 13: `Pong`

    #### Version 1

    Initial version
