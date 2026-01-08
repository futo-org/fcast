## Building

## Macos

When gstreamer has been installed from [the official website](https://gstreamer.freedesktop.org/download/#macos) some environment variables has to be set:

```console
export PKG_CONFIG_PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig
export PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH
export DYLD_LIBRARY_PATH=/Library/Frameworks/GStreamer.framework/Libraries:$DYLD_LIBRARY_PATH
```
