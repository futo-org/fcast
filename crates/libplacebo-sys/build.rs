use std::{env, fs::File, io::Write, path::PathBuf};

mod build {
    // https://github.com/rust-av/dav1d-rs/blob/master/dav1d-sys/build.rs

    use super::*;
    use std::{
        fs,
        path::Path,
        process::{Command, Stdio},
    };

    const TAG: &str = "v7.360.1";

    macro_rules! runner {
        ($cmd:expr, $($arg:expr),*) => {{
            let status = Command::new($cmd)
                $(.arg($arg))*
                .stderr(Stdio::inherit())
                .stdout(Stdio::inherit())
                .status()
                .expect(concat!($cmd, " failed to spawn"));
            assert!(status.success(), concat!($cmd, " exited with a failure status"));
        }};
    }

    pub fn build_from_src(
        lib: &str,
        _: &str,
    ) -> Result<system_deps::Library, system_deps::BuildInternalClosureError> {
        let build_dir = "build";
        let release_dir = "release";

        let libplacebo_source =
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("libplacebo");

        let source = PathBuf::from(env::var("OUT_DIR").unwrap()).join("libplacebo");
        let build_path = source.join(build_dir);
        let release_path = source.join(release_dir);

        fn apply_patch(root: &Path, patch_path: &Path) {
            let status = Command::new("git")
                .current_dir(root)
                .arg("apply")
                .arg("-p1")
                .arg(patch_path)
                .status()
                .expect(concat!("`git apply` failed to spawn"));
            assert!(status.success(), "`git apply` exited with a failure status");
        }

        fn copy_dir(dst: &PathBuf, root: &Path) {
            fs::create_dir_all(dst).unwrap();
            for entry in fs::read_dir(root).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.ends_with(".git") {
                    continue;
                }
                if path.is_file() {
                    let name = path.file_name().unwrap();
                    let mut dst = dst.clone();
                    dst.push(name);
                    eprintln!("name={name:?} dst={dst:?}");
                    std::fs::copy(path, dst).unwrap();
                } else {
                    let mut next = dst.clone();
                    next.push(path.components().last().unwrap());
                    copy_dir(&next, &path);
                }
            }
        }

        // Remove possible stale files
        let _ = fs::remove_dir_all(&source);
        copy_dir(&source, &libplacebo_source);
        let rgb10a2_patch_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("patches/opengl-rgb10a2.patch");
        apply_patch(&source, &rgb10a2_patch_path);

        let (vulkan, shaderc, vk_proc_addr) = if cfg!(feature = "vulkan") {
            (
                "-Dvulkan=enabled",
                "-Dshaderc=enabled",
                "-Dvk-proc-addr=disabled",
            )
        } else {
            (
                "-Dvulkan=disabled",
                "-Dshaderc=disabled",
                "-Dvk-proc-addr=disabled",
            )
        };

        runner!(
            "meson",
            "setup",
            "-Dbuildtype=release",
            "-Ddefault_library=static",
            "-Dglslang=disabled",
            vulkan,
            shaderc,
            vk_proc_addr,
            "-Dd3d11=disabled",
            "-Ddemos=false",
            "-Ddovi=disabled",
            "-Dlcms=disabled",
            "-Dxxhash=disabled",
            "-Dunwind=disabled",
            "--prefix",
            release_path.to_str().unwrap(),
            build_path.to_str().unwrap(),
            source.to_str().unwrap()
        );

        runner!("ninja", "-C", build_path.to_str().unwrap());
        runner!("meson", "install", "-C", build_path.to_str().unwrap());

        let pkg_dir = build_path.join("meson-private");
        system_deps::Library::from_internal_pkg_config(pkg_dir, lib, TAG)
    }
}

fn format_write(builder: bindgen::Builder) -> String {
    builder
        .generate()
        .unwrap()
        .to_string()
        .replace("/**", "/*")
        .replace("/*!", "/*")
}

fn main() {
    println!("cargo:rerun-if-changed=patches/opengl-rgb10a2.patch");

    unsafe { std::env::set_var("SYSTEM_DEPS_LIBPLACEBO_BUILD_INTERNAL", "always") };

    let libs = system_deps::Config::new()
        .add_build_internal("libplacebo", build::build_from_src)
        .probe()
        .unwrap();

    // https://github.com/rust-av/libplacebo-rs/blob/master/libplacebo-sys/build.rs

    let headers = libs
        .get_by_name("libplacebo")
        .unwrap()
        .include_paths
        .clone();

    let mut builder = bindgen::builder()
        .header("placebo.h")
        .constified_enum("pl_handle_type");

    for header in headers {
        builder = builder.clang_arg("-I").clang_arg(header.to_str().unwrap());
    }

    if cfg!(feature = "vulkan") {
        builder = builder.clang_arg("-DFCAST_ENABLE_VULKAN");
        // libplacebo/vulkan.h includes <vulkan/vulkan.h>; use the same vendored headers
        // the meson build itself prefers, so the bindings match the compiled library.
        let vk_headers = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("libplacebo/3rdparty/Vulkan-Headers/include");
        builder = builder
            .clang_arg("-I")
            .clang_arg(vk_headers.to_str().unwrap());
    }

    builder = builder.default_enum_style(bindgen::EnumVariation::Rust {
        non_exhaustive: false,
    });

    // Manually fix the comment so rustdoc won't try to pick them
    let s = format_write(builder);

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());

    let mut file = File::create(out_path.join("placebo.rs")).unwrap();

    let _ = file.write(s.as_bytes());
}
