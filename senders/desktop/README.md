## Building

### Flatpak

Copy `cargo-sources.json` from https://gitlab.futo.org/-/snippets/4 (internal, TODO: provide it publicly upon release)
and place it in the root of the project (`fcast/`). Execute the following command from `fcast/senders/mirroring/desktop`:

```console
$ flatpak-builder --install ./flatpak-builddir --user org.fcast.sender.yml
```

`org.fcast.sender` should now be available on your system or execute the binary in `./flatpak-builddir/files/bin/fcast-sender`
(I believe this requires all the system dependencies to be available in the environment.)

## Macos

When gstreamer has been installed from [the official website](https://gstreamer.freedesktop.org/download/#macos) some environment variables has to be set:

```console
export PKG_CONFIG_PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig
export PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH
export DYLD_LIBRARY_PATH=/Library/Frameworks/GStreamer.framework/Libraries:$DYLD_LIBRARY_PATH
```
