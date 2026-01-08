fn main() {
    slint_build::compile("ui/main.slint").unwrap();

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();

    // https://github.com/servo/servo/blob/613f2ec869cc72d9dfa6641fba51f99f856e2e95/ports/servoshell/build.rs#L88
    if target_os == "macos" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/lib/");
    }

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../extra/fcast.ico");
        res.compile().unwrap();
    }
}
