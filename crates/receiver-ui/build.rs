fn main() {
    // rpath the dynamic system libs the static GStreamer pulls in.
    gst_static_link::emit_rpath();
    // Set at build time, the slint compiler const-folds it and pins the scale
    // factor with `set_const_scale_factor`, which silently disables the runtime
    // policy in `scaling.rs`. Fail loudly instead of shipping a frozen UI.
    println!("cargo:rerun-if-env-changed=SLINT_SCALE_FACTOR");
    assert!(
        std::env::var_os("SLINT_SCALE_FACTOR").is_none(),
        "SLINT_SCALE_FACTOR is set in the build environment. It bakes a constant \
         scale factor into the generated UI and breaks `[interface] ui_scale`. \
         Unset it to build; to force a factor at runtime, pass --ui-scale."
    );

    let config =
        slint_build::CompilerConfiguration::new().with_bundled_translations("translations/");
    slint_build::compile_with_config("ui/main.slint", config).unwrap();
}
