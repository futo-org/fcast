# FCast Receiver Changelog

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
