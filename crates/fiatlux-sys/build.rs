use std::{env, path::PathBuf};

fn main() {
    if let Ok(search) = std::env::var("FL_LIB_PATH") {
        println!("cargo:rustc-link-search={search}");
    } else {
        println!("cargo:rustc-link-search=/usr/local/lib");
    }
    println!("cargo:rustc-link-lib=fiatlux-client");

    let mut bindings = bindgen::Builder::default();
    if let Ok(include) = std::env::var("FL_INCLUDE_PATH") {
        bindings = bindings.clang_arg(format!("-I{include}"))
    }
    let bindings = bindings
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
