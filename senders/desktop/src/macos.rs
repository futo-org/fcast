use core_foundation::{
    base::{CFType, TCFType},
    dictionary::{CFDictionary, CFDictionaryRef},
    string::{CFString, CFStringRef},
};
use mcore::VideoSource;
use objc2_core_graphics::{CGDirectDisplayID, CGGetOnlineDisplayList};

#[link(name = "CoreDisplay", kind = "framework")]
unsafe extern "C" {
    unsafe fn CoreDisplay_DisplayCreateInfoDictionary(
        display_id: CGDirectDisplayID,
    ) -> CFDictionaryRef;
}

// https://github.com/haimgel/ddc-macos-rs
fn product_name(id: CGDirectDisplayID) -> Option<String> {
    let info: CFDictionary<CFString, CFType> = unsafe {
        CFDictionary::wrap_under_create_rule(CoreDisplay_DisplayCreateInfoDictionary(id))
    };

    let display_product_name_key = CFString::from_static_string("DisplayProductName");
    let display_product_names_dict = info
        .find(&display_product_name_key)?
        .downcast::<CFDictionary>()?;
    let (locales, localized_product_names) = display_product_names_dict.get_keys_and_values();
    for (idx, locale) in locales.iter().enumerate() {
        let locale_name = unsafe { CFString::wrap_under_get_rule(*locale as CFStringRef) };
        if locale_name == "en_US" {
            return localized_product_names.get(idx).map(|name| {
                unsafe { CFString::wrap_under_get_rule(*name as CFStringRef) }.to_string()
            });
        }
    }

    None
}

pub fn get_video_sources() -> anyhow::Result<Vec<VideoSource>> {
    const MAX: u32 = 32;
    let mut displays: [CGDirectDisplayID; MAX as usize] = [0; MAX as usize];
    let mut count: u32 = 0;
    let err = unsafe { CGGetOnlineDisplayList(MAX, displays.as_mut_ptr(), &mut count) };
    if err.0 == 0 {
        let mut sources = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let display_id = displays[i];
            tracing::debug!(display_id, idx = i, "Found video device");
            sources.push(VideoSource::CgDisplay {
                id: i as i32,
                name: product_name(display_id).unwrap_or(format!("Display {}", i + 1)),
            });
        }
        Ok(sources)
    } else {
        anyhow::bail!("Failed to get display list")
    }
}
