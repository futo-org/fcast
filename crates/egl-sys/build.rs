use gl_generator::{Api, Fallbacks, Profile, Registry};
use std::{env, fs::File, path::PathBuf};

fn main() {
    let dest = PathBuf::from(&env::var("OUT_DIR").unwrap());
    let mut file = File::create(dest.join("egl.rs")).unwrap();
    Registry::new(
        Api::Egl,
        (1, 5),
        Profile::Core,
        Fallbacks::All,
        [
            "EGL_EXT_image_dma_buf_import",
            "EGL_EXT_image_dma_buf_import_modifiers",
        ],
    )
    .write_bindings(gl_generator::GlobalGenerator, &mut file)
    .unwrap();
}
