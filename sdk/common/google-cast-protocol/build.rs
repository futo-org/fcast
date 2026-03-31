fn main() {
    prost_build::compile_protos(&["src/googlecast.proto"], &["src"]).unwrap();
}
