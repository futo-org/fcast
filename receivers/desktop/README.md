# FCast Desktop Receiver

## Requirements

Build tools needed on all platforms:

- Rust 1.95 or newer
- meson
- ninja
- cmake
- nasm
- pkgconf (`pkg-config`)
- Python 3
- clang
- git

On Linux the final link also needs a modern multi-pass linker: the static
link line interleaves ~100 archives with dynamic system libs, which bfd
cannot resolve. The repo's `.cargo/config.toml` pins clang with
[wild](https://github.com/davidlattimore/wild), so install wild, or edit
that file to use mold or lld instead.

macOS and Windows build self-contained installers, so only the tools above are needed. Windows also
needs WiX (for the installer).

### Linux dependencies

GStreamer and the media codecs are built and linked statically, but glib, pango, and the platform
libraries are linked from the system. Install the development packages for:

- glib, pango, harfbuzz, fribidi, cairo, pixman, graphene, json-glib, freetype, fontconfig, expat, pcre2, libffi, zlib, libpng, libjpeg
- openssl, libsoup3, libxml2, libpsl, nghttp2
- libogg, libvorbis, libtheora, opus, flac, dav1d, libass, libsrtp2, srt, wavpack, libva, libgudev
- alsa-lib, libpulseaudio, pipewire, libnice, libheif, shaderc, vulkan-loader, libclang
- wayland, libxkbcommon, libX11, libXcursor, libXi, libXrandr, libxcb, libGL

## Building

The receiver statically links a pinned, patched GStreamer (our fork of 1.29.2), built from source
by the [gstreamer-src](https://gitlab.futo.org/fcast/gstreamer-src) crate as a regular cargo
dependency. No manual GStreamer setup is needed: the first build clones the fork and compiles it,
which takes a while; after that it is cached under `target/gst-static` and rebuilds are ordinary
cargo builds.

Release build (`build-static` defaults to a debug build, see
`cargo xtask receiver build-static --help` for all options):

```
$ cargo xtask receiver build-static --release
```

Plain `cargo build -p desktop-receiver` works too, but the xtask wrappers also guard against a
stale binary after gst-only rebuilds, so prefer them.

The xtask also has options for creating self-contained installers for macos and windows.

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

## Testing

`cargo xtask test` runs the receiver-side test lane (uses
[cargo-nextest](https://nexte.st) when installed, which cuts the realtime-paced suites from ~7
minutes to ~2).
