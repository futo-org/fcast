use std::collections::HashSet;

use drm_fourcc::{DrmFormat, DrmFourcc, DrmModifier};
use tracing::error;

#[derive(Hash, Eq, PartialEq)]
pub enum Extension {
    ImageDmaBufImport,
    ImageDmaBufImportModifiers,
}

pub fn ensure_init() {
    egl_sys::ensure_init();
}

pub fn get_extensions(egl: &glutin_egl_sys::egl::Egl) -> HashSet<Extension> {
    let display = unsafe { egl.GetCurrentDisplay() };
    let mut extensions = HashSet::new();
    unsafe {
        let res = egl.QueryString(display, glutin_egl_sys::egl::EXTENSIONS as i32);
        if !res.is_null() {
            let res = std::ffi::CStr::from_ptr(res);
            let list = String::from_utf8(res.to_bytes().to_vec()).unwrap_or_else(|_| String::new());
            for ext in list.split(' ') {
                match ext {
                    "EGL_EXT_image_dma_buf_import" => {
                        extensions.insert(Extension::ImageDmaBufImport);
                    }
                    "EGL_EXT_image_dma_buf_import_modifiers" => {
                        extensions.insert(Extension::ImageDmaBufImportModifiers);
                    }
                    _ => (),
                }
            }
        }
    }

    extensions
}

#[derive(Debug, thiserror::Error)]
pub enum EglError {
    #[error("failed to perform query")]
    QueryFailed,
}

pub fn get_supported_dma_drm_formats(
    display: glutin_egl_sys::EGLDisplay,
) -> Result<HashSet<DrmFormat>, EglError> {
    let mut num = 0i32;
    let query_res = unsafe {
        egl_sys::bindings::QueryDmaBufFormatsEXT(
            display,
            0,
            std::ptr::null_mut(),
            &mut num as *mut _,
        )
    };
    if query_res != 1 {
        error!("QueryDmaBufFormatsEXT failed");
        return Err(EglError::QueryFailed);
    }

    let mut formats: Vec<u32> = Vec::with_capacity(num as usize);
    let query_res = unsafe {
        egl_sys::bindings::QueryDmaBufFormatsEXT(
            display,
            num,
            formats.as_mut_ptr() as *mut _,
            &mut num as *mut _,
        )
    };
    if query_res != 1 {
        error!("QueryDmaBufFormatsEXT failed");
        return Err(EglError::QueryFailed);
    }
    unsafe {
        formats.set_len(num as usize);
    }
    let formats = formats
        .into_iter()
        .flat_map(|x| DrmFourcc::try_from(x).ok())
        .collect::<Vec<_>>();
    let mut texture_formats = std::collections::HashSet::new();
    for fourcc in formats {
        let mut num = 0i32;

        let query_res = unsafe {
            egl_sys::bindings::QueryDmaBufModifiersEXT(
                display,
                fourcc as i32,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut num as *mut _,
            )
        };
        if query_res != 1 {
            error!("QueryDmaBufModifiersEXT failed");
            return Err(EglError::QueryFailed);
        }

        if num != 0 {
            let mut mods: Vec<u64> = Vec::with_capacity(num as usize);
            let mut external: Vec<egl_sys::bindings::types::EGLBoolean> =
                Vec::with_capacity(num as usize);
            let query_res = unsafe {
                egl_sys::bindings::QueryDmaBufModifiersEXT(
                    display,
                    fourcc as i32,
                    num,
                    mods.as_mut_ptr(),
                    external.as_mut_ptr(),
                    &mut num as *mut _,
                )
            };
            if query_res != 1 {
                error!("QueryDmaBufModifiersEXT failed");
                return Err(EglError::QueryFailed);
            }

            unsafe {
                mods.set_len(num as usize);
                external.set_len(num as usize);
            }

            for (modifier, external_only) in mods.into_iter().zip(external) {
                if external_only == 1 {
                    texture_formats.insert(DrmFormat {
                        code: fourcc,
                        modifier: DrmModifier::from(modifier),
                    });
                }
            }
        }
    }

    Ok(texture_formats)
}
