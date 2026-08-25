## 0.1.0

- Update to `fcast-sender-sdk` 0.3.0
- Subtitle data over the FCast companion channel: `addSubtitleSource` can now
  carry raw subtitle bytes via `SubtitleContent.data`, so no receiver-reachable
  URL is required
- In-memory companion payloads via `CompanionSourceDescriptor.bytes`
- Fix device discovery on Bonsoir 7 / macOS
- Discovered devices are deduplicated by a stable key so add and remove events line up
- Fix builds when not using default features
- BREAKING: `SubtitleSource` now takes a `content` (`SubtitleContent`) instead of
  a `url` string. Wrap an existing URL as `SubtitleContent.url(url: ...)`
- BREAKING: `DiscoveryEventDeviceRemoved.name` is renamed to `storageKey`.
  `DiscoveryEventDeviceAdded` and `DiscoveryEventDeviceUpdated` now expose a
  matching `storageKey`. Index your device list by `storageKey` (from
  `deviceStorageKey`), not `DeviceInfo.name`

## 0.0.5

- Update to `fcast-sender-sdk` 0.2.0
- Queue support: `loadQueue`, `queueInsert`, `queueRemove`, `queueSelect`, and
  `DeviceEvent.queueChanged` carrying full queue snapshots
- Track selection: `changeTrack` and the aggregated `DeviceEvent.tracksChanged`
- External subtitles: `addSubtitleSource`
- Receiver-rejected commands are surfaced via `DeviceEvent.commandError`
- Progress update interval: `setProgressUpdateInterval` plus an optional
  `progressUpdateIntervalMillis` parameter on `load`
- File-descriptor companion sources (`CompanionSourceDescriptor.fd`) for
  Android SAF / iOS picker flows
- `supportsFeature` covers the new capabilities (queue, track selection,
  companion, WHEP, progress interval)
- BREAKING: exhaustive `switch`es over `DeviceEvent` must handle the new
  variants

## 0.0.4

- Update to `fcast-sender-sdk` 0.1.4
- Add `PlaybackState.ended`, emitted when media reaches its natural end
- Native build no longer requires `flatc` (still needs `rustup`)
- Raise minimum Flutter to 3.35

## 0.0.1 - 0.0.3

- Initial releases
