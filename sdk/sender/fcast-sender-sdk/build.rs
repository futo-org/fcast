fn main() {
    println!("cargo:rustc-check-cfg=cfg(any_protocol)");
    if std::env::var_os("CARGO_FEATURE_FCAST").is_some()
        || std::env::var_os("CARGO_FEATURE_CHROMECAST").is_some()
    {
        println!("cargo:rustc-cfg=any_protocol");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
