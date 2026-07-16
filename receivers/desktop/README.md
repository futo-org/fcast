# FCast Desktop Receiver

## Requirements

Build tools needed on all platforms:

- Rust nightly
- meson
- ninja
- cmake
- nasm
- pkgconf (`pkg-config`)
- flatbuffers (`flatc`)
- Python 3
- clang
- git

macOS and Windows build self-contained installers, so only the tools above are needed. Windows also
needs WiX (for the installer).

### Linux dependencies

The xtask builds GStreamer and the media codecs statically, but glib, pango, and the platform
libraries are linked from the system. Install the development packages for:

- glib, pango, harfbuzz, fribidi, cairo, pixman, graphene, json-glib, freetype, fontconfig, expat, pcre2, libffi, zlib, libpng, libjpeg
- openssl, libsoup3, libxml2, libpsl, nghttp2
- libogg, libvorbis, libtheora, opus, flac, dav1d, libass, libsrtp2, srt, wavpack, libva, libgudev
- alsa-lib, libpulseaudio, pipewire, libnice, libheif, shaderc, vulkan-loader, libclang
- wayland, libxkbcommon, libX11, libXcursor, libXi, libXrandr, libxcb, libGL

## Building

The receiver requires an unstable version of GStreamer, the downloading, building, and linking of
gstreamer, it's dependencies, and the receiver is taken care of by an xtask. Here is the command for
a release build (run `cargo xtask receiver build-static --help` for a list of all options):

```
$ cargo xtask receiver build-static
```

The xtask also have options for creating self-contained installers for macos and windows.

Macos:

```
$ cargo xtask receiver build-macos-installer
```

Windows:

```
$ cargo xtask receiver build-windows-installer
```

Single command to build and run in debug mode:

```
$ cargo xtask receiver run
```

and quickly check for errors:

```
$ cargo xtask receiver check
```
