## Building

Install `cargo-ndk`:

```
cargo install --locked cargo-ndk --git https://github.com/bbqsrc/cargo-ndk --rev 9672f442e3524139f3369720cd8d83c8f29b7303
```

Build gstreamer:

```
cargo xtask receiver android build-lib-gst
```

Compile:

```
cargo xtask receiver android build
```

(Add `-r` for optimized build and `-t <ARCH>` for target architecture.)
