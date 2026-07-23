# fcast_sender_sdk

Flutter/Dart bindings for the FCast Sender SDK. Discover FCast and Google Cast
(Chromecast) receivers on the local network and control playback.

The native (Rust) library is generated with
[`flutter_rust_bridge`](https://cjycode.com/flutter_rust_bridge/) and built &
bundled via Dart's **native assets** build hook (`hook/build.dart`).

## Supported platforms

Android, iOS, Linux, macOS, and Windows. **Web is not supported**.

## Requirements

- [rustup](https://rustup.rs/) — the native build uses it to manage the Rust toolchain
- A Flutter/Dart SDK with native assets enabled:
  ```sh
  flutter config --enable-native-assets
  ```

## Selecting protocols / features (fcast, chromecast)

The underlying `fcast-sender-sdk` crate exposes cargo features. This plugin
forwards them (`rust/Cargo.toml`):

| Feature      | Description                               |
|--------------|-------------------------------------------|
| `fcast`      | FCast protocol support                    |
| `chromecast` | Google Cast (Chromecast) protocol support |
| `logging`    | Native logging                            |

**Default (all three) is used when you configure nothing.**

A consuming app selects a subset by declaring native-assets *user-defines* in its
own `pubspec.yaml`:

```yaml
# app's pubspec.yaml  (workspace-root pubspec.yaml if you use a pub workspace)
hooks:
  user_defines:
    fcast_sender_sdk:
      features: [fcast]   # fcast-only build
```

Trimming features reduces the native binary size and the set of compiled
dependencies.

## Regenerating the bindings

The generated files (`lib/src/rust/*.dart`, `rust/src/frb_generated.rs`) are
git-ignored and regenerated. Run this from anywhere in the monorepo after a
fresh clone or after editing `rust/src/api.rs`:

```sh
cargo xtask flutter generate
```

The xtask ensures that `flutter_rust_bridge_codegen` is always the same version
as the companion flutter package so the generated code can never be invalid at
runtime.
