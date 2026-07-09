fn main() {
    let config =
        slint_build::CompilerConfiguration::new().with_bundled_translations("translations/");
    slint_build::compile_with_config("ui/main.slint", config).unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=IOSurface");
        println!("cargo:rustc-link-lib=framework=OpenGL");
    }
}
