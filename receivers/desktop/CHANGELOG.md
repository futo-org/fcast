# FCast Receiver Changelog

## 3.0.3 - UNRELEASED

### New Features

 - YouTube (UMP/SABR) stream playback support
 - Support for FCast protocol v4
 - HDR10 metadata passthrough to the video renderer
 - Zero-copy video import on macOS
 - Wayland subsurface video sink (experimental, enable with `FCAST_VIDEO_SINK=wayland-subsurface`)
 - Debug inspector view (Ctrl+Shift+I)
 - Enable system tray icon on macOS

### Fixes

 - Rework track selection and fix subtitle deselect freeze
 - Improve RAOP synchronization and fix RAOP playback crashes
 - Fix Google Cast optional volume handling
 - Fix rustls crypto panic on macOS/Windows
 - Fix subtitle timing when the end time is unknown
 - Use the new request headers for image downloads

## 3.0.2 - 2026-06-16

 - Downgrade session version if sender is higher than receiver instead of rejecting the connection
 - Fix subtitles flickering in certain situations
 - Added `--headless` option to run without a GUI and only play audio

## 3.0.1 - 2026-06-02

 - Fix crash when running on X11
 - inhibit screensaver only when playing media

## 3.0.0 - 2026-05-22

## 0.1.3-beta - 2026-05-14

 - Fix freezing when disabling subtitles
 - Update dependencies

## 0.1.2-beta - 2026-04-30

 - New video renderer that uses [libplacebo]
 - Fixed a bug where playback would freeze if the sender changed playback states quickly

[libplacebo]: https://code.videolan.org/videolan/libplacebo

## 0.1.1-beta - 2026-04-16

### New Features

 - Animated images
 - Support for more image formats (heif, jxl, jp2)
 - Windows support

## 0.1.0-beta - 2026-04-06

### New Features

  - Updated UI
  - [RAOP] support
  - [Google Cast] support compatible with the [immich] mobile app
  - Wider media format support compared to the electron receiver

[RAOP]: https://en.wikipedia.org/wiki/Remote_Audio_Output_Protocol
[Google Cast]: https://en.wikipedia.org/wiki/Google_Cast
[immich]: https://immich.app/
