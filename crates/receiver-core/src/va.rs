use std::path::Path;

pub fn render_node_is_intel(device_path: &str) -> bool {
    let Some(node) = Path::new(device_path).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    match std::fs::read_to_string(format!("/sys/class/drm/{node}/device/vendor")) {
        Ok(vendor) => vendor.trim() == "0x8086",
        Err(_) => false,
    }
}

#[cfg(feature = "fhs")]
pub fn display_context(device_path: &str) -> Option<gst::Context> {
    use std::ffi::CString;

    unsafe extern "C" {
        fn gst_va_display_drm_new_from_path(
            path: *const std::os::raw::c_char,
        ) -> *mut gst::ffi::GstObject;
    }

    let c_path = CString::new(device_path).ok()?;
    let display = unsafe { gst_va_display_drm_new_from_path(c_path.as_ptr()) };
    if display.is_null() {
        return None;
    }
    let display: gst::Object = unsafe { gst::glib::translate::from_glib_full(display) };

    let mut context = gst::Context::new("gst.va.display.handle", true);
    context
        .get_mut()
        .unwrap()
        .structure_mut()
        .set("gst-display", &display);
    Some(context)
}
