fn main() {
    let file_descriptors = protox::compile(["src/googlecast.proto"], ["src"]).unwrap();
    prost_build::compile_fds(file_descriptors).unwrap();
}
