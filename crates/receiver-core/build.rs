fn main() {
    let config =
        slint_build::CompilerConfiguration::new().with_bundled_translations("translations/");
    slint_build::compile_with_config("ui/main.slint", config).unwrap();

    if std::env::var_os("CARGO_FEATURE_FHS").is_some() {
        pkg_config::Config::new()
            .probe("gstreamer-va-1.0")
            .expect("gstreamer-va-1.0 (libgstva) is required for the fhs feature");
    }
}
