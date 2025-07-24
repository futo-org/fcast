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

To generate the bindings and include them in the `examples/android/` and `examples/android-views/` demos,
execute the following command:

```console
$ cargo xtask generate-android
```

## IOS

### Additional required tools

* The `aarch64-apple-ios-sim` and `aarch64-apple-ios` rustc targets
  (can be installed with [rustup](https://rustup.rs/): `rustup target add aarch64-apple-ios-sim aarch64-apple-ios`)
