# Protocol

This section documents the FCast protocol details, which can be found in the latest version page.

## Version History

-----

#### [Version 3](v3)

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

-----

#### [Version 2](v2)

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

-----

#### [Version 1](v1)

Initial Version