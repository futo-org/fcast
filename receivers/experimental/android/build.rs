use std::env;
// use std::env::current_dir;
// use std::process::Command;

// Source: https://github.com/mvvvv/StereoKit-rust-gstreamer/blob/main/build.rs

macro_rules! cargo_link {
    ($feature:expr) => {
        println!("cargo:rustc-link-lib={}", $feature);
    };
}

fn main() {
    assert_eq!(env::var("CARGO_CFG_TARGET_OS"), Ok("android".to_owned()));

    let proj_root = env::current_dir().unwrap();
    let android_ndk_home = env::var("ANDROID_NDK_ROOT").unwrap();
    let gst_libs = env::var("GSTREAMER_ROOT_ANDROID").unwrap();
    // let gst_libs_path = current_dir().unwrap().join(&gst_libs);

    let gst_android_build_path = "../target/gst-android-build";

    // if let Err(_e) = std::fs::create_dir("../target") {};
    // if let Err(_e) = std::fs::create_dir(gst_android_build_path) {};

    // env::set_current_dir("../target")
    //     .expect("Unable to get a build directory for android gstreamer");

    // let stat = Command::new("make")
    //     .env("BUILD_SYSTEM", format!("{}/build/core", android_ndk_home))
    //     .env(
    //         "GSTREAMER_JAVA_SRC_DIR",
    //         "../android-sender/app/src/main/java",
    //     )
    //     .env("NDK_PROJECT_PATH", "../android-sender/app")
    //     .env("GSTREAMER_ROOT_ANDROID", gst_libs_path.to_str().unwrap())
    //     .env(
    //         "GSTREAMER_NDK_BUILD_PATH",
    //         gst_libs_path
    //             .join("share/gst-android/ndk-build/")
    //             .to_str()
    //             .unwrap(),
    //     )
    //     .args([
    //         "-f",
    //         &format!("{}/build/core/build-local.mk", android_ndk_home),
    //     ])
    //     .status()
    //     .expect("failed to make!");

    // assert!(stat.success());

    // env::set_current_dir("../android-sender").unwrap();

    // println!(
    //     "cargo:rustc-link-search=native={}/x86_64",
    //     gst_android_build_path
    // );
    println!(
        // "cargo:rustc-link-search=native={}/arm64-v8a",
        "cargo:rustc-link-search=native={}/x86_64",
        gst_android_build_path
    );
    println!("cargo:rustc-link-search=native={}", gst_libs);
    // println!(
    //     "cargo:rustc-link-search=native={}/app/libs/x86_64",
    //     proj_root.display()
    // );
    println!(
        // "cargo:rustc-link-search=native={}/app/libs/arm64-v8a",
        "cargo:rustc-link-search=native={}/app/libs/x86_64",
        proj_root.display()
    );

    cargo_link!("gstreamer_android");
    cargo_link!("dylib=c++");

    cargo_link!("ffi");
    cargo_link!("iconv");
    cargo_link!("gstreamer-1.0");
    cargo_link!("gmodule-2.0");
    cargo_link!("gobject-2.0");
    cargo_link!("glib-2.0");
    cargo_link!("gstvideo-1.0");
    cargo_link!("gstaudio-1.0");
    cargo_link!("gstapp-1.0");
    cargo_link!("gstrtp-1.0");
    cargo_link!("gstwebrtc-1.0");
    cargo_link!("gstpbutils-1.0");
    cargo_link!("gstgl-1.0");
    cargo_link!("orc-0.4");

    const DEFAULT_CLANG_VERSION: &str = "20";
    let clang_version =
        env::var("NDK_CLANG_VERSION").unwrap_or_else(|_| DEFAULT_CLANG_VERSION.to_owned());
    let linux_x86_64_lib_dir = format!(
        // let linux_arm64_lib_dir = format!(
        "toolchains/llvm/prebuilt/{}-x86_64/lib/clang/{clang_version}/lib/linux/",
        // "toolchains/llvm/prebuilt/{}-arm64/lib/clang/{clang_version}/lib/linux/",
        env::consts::OS
    );
    println!("cargo:rustc-link-search={android_ndk_home}/{linux_x86_64_lib_dir}");
    // println!("cargo:rustc-link-search={android_ndk_home}/{linux_arm64_lib_dir}");
    cargo_link!("clang_rt.builtins-x86_64-android");
    // cargo_link!("clang_rt.builtins-aarch64-android");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=jni/Android.mk");
}
