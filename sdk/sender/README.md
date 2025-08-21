# FCast Sender SDK

## Required tools

* [Rust](https://www.rust-lang.org/)
* [Cargo](https://doc.rust-lang.org/cargo/)
* [protoc](https://protobuf.dev/installation/)

## Android

### Additional required tools

* [cargo-ndk](https://github.com/bbqsrc/cargo-ndk)
* The `aarch64-linux-android`, `i686-linux-android`, `armv7-linux-androideabi` and `x86_64-linux-android` rustc targets
  (can be installed with [rustup](https://rustup.rs/): `rustup target add x86_64-linux-android i686-linux-android armv7-linux-androideabi aarch64-linux-android`)
* `JAVA_HOME` must point to a java implementation

### Building

To build the android library locally you first need to clone [fcast-sdk-jitpack](https://gitlab.futo.org/videostreaming/fcast-sdk-jitpack) locally, build the rust binaries and generate the UniFFI kotlin module:

```console
$ cargo xtask kotlin build-android-library --release --src-dir <path-to-fcast-sdk-jitpack>/src
```

Then follow the `Local testing` section [here](https://gitlab.futo.org/videostreaming/fcast-sdk-jitpack/-/blob/main/README.md?ref_type=heads).

## IOS

If `iphonesimulator SDK` is not found when running the build commands, execute the following:

```console
$ # xcode-select --switch /Applications/Xcode.app/Contents/Developer/
```

### Additional required tools

* The `aarch64-apple-ios-sim` and `aarch64-apple-ios` rustc targets
  (can be installed with [rustup](https://rustup.rs/): `rustup target add aarch64-apple-ios-sim aarch64-apple-ios`)

### Building

Execute:

```console
$ cargo xtask generate-ios
```

You can now import the SDK in your project by drag and dropping `ios-bindings/uniffi/{fcast_sender_sdk.swift, fcast_sender_sdkFFI.h}` and `ios-bindings/fcast_sender_sdk.xcframework` into Xcode.
