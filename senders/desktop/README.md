# Building

Platform common dependencies:

 * [Rust](https://rust-lang.org/) and [Cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html)
 * [GStreamer](https://gstreamer.freedesktop.org/download/)
 * [protoc](https://protobuf.dev/installation/)

## Macos

When gstreamer has been installed from [the official website](https://gstreamer.freedesktop.org/download/#macos) some environment variables has to be set:

```console
export PKG_CONFIG_PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig
export PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH
export DYLD_LIBRARY_PATH=/Library/Frameworks/GStreamer.framework/Libraries:$DYLD_LIBRARY_PATH
```

See `desktop_sender_task` in [.cirrus.yml](../../.cirrus.yml) for how to build.

## Windows

Required dependencies:

 * [NASM](https://www.nasm.us/)
 * [CMake](https://cmake.org/)

See the `builSenderForWinX64` job in [.gitlab-ci.yml](./.gitlab-ci.yml) for how to build.

## Linux

Required dependencies:

See the [fcast-sender.nix](./fcast-sender.nix) file or [the flatpak](https://github.com/flathub/org.fcast.Sender) definition for working package declarations.
