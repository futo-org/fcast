## Building

#### Get android NDK and gstreamer

The uncompressed gstreamer directory is quite large (~14GiB) so make sure you have enough storage space.

```console
$ cargo xtask android download-sdk
$ cargo xtask android download-ndk
$ cargo xtask android download-gstreamer
```

#### Build gstreamer

```console
$ cargo xtask sender android build-lib-gst
```

#### Build the rust library

Add `--target <ARCH>` to specify a single target architecture (possible values: `x64`, `x86`, `arm64`, `arm32`) or `--release` to enable optimizations.

```console
$ cargo xtask sender android build
```

#### Building android the app

Use android studio or gradlew:

```console
$ ./gradlew build
```

Install it for testing:

```console
$ ./gradlew installDebug
```

-----

*For future reference: when encountering errors like `cannot register existing type`, add whatever is missing to `GSTREAMER_EXTRA_DEPS` in `Android.md`.*
