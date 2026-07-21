fn main() {
    let protos = [
        "proto/common.proto",
        "proto/ump_parts.proto",
        "proto/video_playback_abr_request.proto",
    ];
    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }

    let file_descriptors = protox::compile(protos, ["proto"]).expect("compile sabr protos");
    prost_build::compile_fds(file_descriptors).expect("generate sabr protos");
}
