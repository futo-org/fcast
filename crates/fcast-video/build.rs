fn main() {
    // rpath the dynamic system libs the static GStreamer pulls in.
    gst_static_link::emit_rpath();
}
