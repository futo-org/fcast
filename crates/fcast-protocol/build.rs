use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=flatbuffers/fcast.fbs");
    let out_dir = PathBuf::from(&std::env::var("OUT_DIR").unwrap()).join("flatbuffers");
    flatc_rust::run(flatc_rust::Args {
        inputs: &[Path::new("flatbuffers/fcast.fbs")],
        // out_dir: Path::new("flatbuffers/"),
        out_dir: &out_dir,
        ..Default::default()
    })
    .unwrap();
}
