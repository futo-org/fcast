use std::path::Path;

fn render_node_vendor(device_path: &str) -> Option<String> {
    let node = Path::new(device_path).file_name().and_then(|n| n.to_str())?;
    let vendor = std::fs::read_to_string(format!("/sys/class/drm/{node}/device/vendor")).ok()?;
    Some(vendor.trim().to_string())
}

pub fn render_node_is_intel(device_path: &str) -> bool {
    render_node_vendor(device_path).as_deref() == Some("0x8086")
}

#[cfg(feature = "fhs")]
pub fn render_node_is_nvidia(device_path: &str) -> bool {
    render_node_vendor(device_path).as_deref() == Some("0x10de")
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
