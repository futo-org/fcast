## Building

GStreamer is built from source and statically linked by the `gstreamer-src`
crate, so there is no prebuilt bundle to fetch and nothing to set up by hand
beyond the SDK, the NDK and `cargo-ndk`.

Install `cargo-ndk`:

```
cargo install --locked cargo-ndk --git https://github.com/bbqsrc/cargo-ndk --rev 9672f442e3524139f3369720cd8d83c8f29b7303
```

Provision the SDK and the NDK into `thirdparty/` (once):

```
cargo xtask android download-sdk
cargo xtask android download-ndk
```

A JDK is needed too, for the java glue the slint android backend compiles and
for gradle. The devshell ships one; `xtask` checks it before anything else.

Compile the native libraries:

```
cargo xtask receiver android build
```

(Add `-r` for the optimized build and `-t <ABI>` for one architecture. The
lane builds arm64 and armv7, the two the apk ships.)

Package them:

```
cargo xtask receiver android package -r                # sideload apk
cargo xtask receiver android package -r --bundle       # playstore aab
```

`--version-code` and `--version-name` are passed through to gradle, and
`--skip-native` packages the libraries already in the tree.
