use std::env;

fn main() {
    slint_build::compile("ui/main.slint").unwrap();

    // assert_eq!(env::var("CARGO_CFG_TARGET_OS"), Ok("android".to_owned()));

    let proj_root = env::current_dir().unwrap();
    let android_ndk_home = env::var("ANDROID_NDK_ROOT").unwrap();
    let gst_libs = env::var("GSTREAMER_ROOT_ANDROID").unwrap();

    let (gst_target_abi, android_target_abi, clang_target_abi) =
        match std::env::var("TARGET").unwrap().as_str() {
            "aarch64-linux-android" => ("arm64", "arm64-v8a", "aarch64"),
            "x86_64-linux-android" => ("x86_64", "x86_64", "x86_64"),
            "i686-linux-android" => ("x86", "x86", "i686"),
            "armv7-linux-androideabi" => ("armv7", "armeabi-v7a", "arm"),
            _ => unimplemented!(),
        };

    let gstreamer_root = gst_libs;

    let search_paths = [
        format!("{gstreamer_root}/{gst_target_abi}/lib"),
        format!("{gstreamer_root}/{gst_target_abi}/lib/gstreamer-1.0"),
        format!("{}/app/libs/{android_target_abi}", proj_root.display()),
        // format!("{android_ndk_home}/toolchains/llvm/prebuilt/linux-x86_64/lib/clang/18/lib/linux/"), // r27d
        format!("{android_ndk_home}/toolchains/llvm/prebuilt/linux-x86_64/lib64/clang/14.0.7/lib/linux/"), // r25c
    ];

    for search_path in search_paths {
        println!("cargo:rustc-link-search=all={search_path}");
    }

    let libs = [
        "gstreamer_android",
        "dylib=c++",
        // "ffi",
        // "iconv",
        // "gstreamer-1.0",
        // "gmodule-2.0",
        // "gobject-2.0",
        // "glib-2.0",
        // "gstvideo-1.0",
        // "gstaudio-1.0",
        // "gstapp-1.0",
        // "gstrtp-1.0",
        // "gstwebrtc-1.0",
        // "gstpbutils-1.0",
        "orc-0.4",
        &format!("clang_rt.builtins-{clang_target_abi}-android"),
    ];

    for lib in libs {
        println!("cargo:rustc-link-lib={lib}");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=app/jni/Android.mk");
}
