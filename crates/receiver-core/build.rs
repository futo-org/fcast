fn main() {
    // rpath the dynamic system libs the static GStreamer pulls in.
    gst_static_link::emit_rpath();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=IOSurface");
        println!("cargo:rustc-link-lib=framework=OpenGL");
    }
}
