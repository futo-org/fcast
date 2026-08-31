use std::env;

fn main() {
    // Android-only crate. GStreamer is built and statically linked through
    // the gstreamer-src crate (the bundled -sys forks), no prebuilt
    // gstreamer_android bundle or manual link lines are involved anymore.
    assert_eq!(env::var("CARGO_CFG_TARGET_OS"), Ok("android".to_owned()));

    println!("cargo:rerun-if-changed=build.rs");
}
