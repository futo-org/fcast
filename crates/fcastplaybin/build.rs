//! Env-gated `cfg(loom)`, scoped to this package alone, so the `loom_` tests
//! in `src/hands.rs` model-check the real `Hands`:
//!
//! ```sh
//! FCAST_LOOM=1 cargo test -p fcastplaybin --lib --release loom_
//! ```
//!
//! Not `RUSTFLAGS` (reaches hyper-util/tokio, whose `cfg(loom)` branches do
//! not compile) and not a cargo feature (`--all-features` would enable it and
//! loom's primitives panic outside a model). `--lib` is required: the swap
//! needs the `loom` dev-dependency, which only a test target links.
fn main() {
    println!("cargo::rerun-if-env-changed=FCAST_LOOM");
    println!("cargo::rustc-check-cfg=cfg(loom)");
    if std::env::var_os("FCAST_LOOM").is_some() {
        println!("cargo::rustc-cfg=loom");
    }
}
