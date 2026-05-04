use tracing::debug;

pub fn init_and_load_plugins() {
    gst::init().unwrap();
    debug!(gstreamer_version = %gst::version_string());

    // TODO: investigate why certain files leads to crashes when this is added
    // gst::rust_allocator().clone().set_default();

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let mut plugin_dir = std::env::current_exe().unwrap();
        plugin_dir.pop();
        #[cfg(target_os = "macos")]
        plugin_dir.push("lib");
        unsafe {
            std::env::set_var("GIO_MODULE_DIR", plugin_dir.join("gio").join("modules"));
        }
        #[cfg(target_os = "windows")]
        let plugins = receiver_resources::all_plugins_for_win();
        #[cfg(target_os = "macos")]
        let plugins = receiver_resources::all_plugins_for_macos();
        for plugin in plugins {
            use tracing::error;

            let mut path = plugin_dir.clone();
            path.push(&plugin);
            let registry = gst::Registry::get();
            match gst::Plugin::load_file(&path) {
                Ok(plugin) => {
                    let _ = registry.add_plugin(&plugin);
                }
                Err(err) => error!(?err, plugin, "Failed to load gstreamer plugin"),
            }
        }
    }

    crate::fcastwhepsrcbin::plugin_init().unwrap();
    crate::fcasttextoverlay::plugin_init().unwrap();
    crate::fcompsrc::plugin_init().unwrap();
    gstreqwest::plugin_register_static().unwrap();

    #[cfg(feature = "static-gst-plugins")]
    {
        gstwebrtchttp::plugin_register_static().unwrap();
        gstrswebrtc::plugin_register_static().unwrap();
        #[cfg(not(target_os = "android"))]
        gstrsrtp::plugin_register_static().unwrap();
        gstdav1d::plugin_register_static().unwrap();
    }
}
