#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::rc::Rc;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::concat_paths;
use anyhow::Result;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use camino::Utf8Path;
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use xshell::cmd;
#[cfg(target_os = "windows")]
use xshell::Shell;

use crate::{sh, workspace, AndroidAbiTarget};

#[cfg(target_os = "macos")]
use crate::BuildMacosInstallerArgs;

#[cfg(target_os = "windows")]
const GSTREAMER_BASE_LIBS: [&'static str; 20] = [
    "gstbase",
    "gstnet",
    "gstreamer",
    "gstapp",
    "gstpbutils",
    "gstrtp",
    "gstrtsp",
    "gstsctp",
    "gstsdp",
    "gstvideo",
    "gstwebrtc",
    "gstwebrtcnice",
    "gstd3d11",
    "gstd3d12",
    "gstd3dshader",
    "gstaudio",
    "gsttag",
    "gstdxva",
    "gstcodecs",
    "gstcodecparsers",
];

#[cfg(target_os = "windows")]
const GSTREAMER_WIN_DEPENDENCY_LIBS: [&'static str; 15] = [
    "bz2.dll",
    "ffi-7.dll",
    "gio-2.0-0.dll",
    "glib-2.0-0.dll",
    "gmodule-2.0-0.dll",
    "gobject-2.0-0.dll",
    "intl-8.dll",
    "libcrypto-3-x64.dll",
    "libssl-3-x64.dll",
    "libwinpthread-1.dll",
    "nice-10.dll",
    "orc-0.4-0.dll",
    "pcre2-8-0.dll",
    "z-1.dll",
    "srtp2-1.dll",
];

#[cfg(any(target_os = "macos", target_os = "windows"))]
const GSTREAMER_PLUGIN_LIBS_COMMON: [&'static str; 13] = [
    "gstcoreelements",
    "gstnice",
    "gstapp",
    "gstvideorate",
    "gstgio",
    "gstvideoconvertscale",
    "gstrtp",
    "gstrtpmanager",
    "gstvpx",
    "gstdtls",
    "gstwebrtc",
    "gstsrtp",
    "gstvideotestsrc",
];

#[cfg(target_os = "windows")]
const GSTREAMER_PLUGIN_LIBS_WIN: [&'static str; 2] = ["gstd3d11", "gstd3d12"];

#[cfg(target_os = "macos")]
const GSTREAMER_PLUGIN_LIBS_MACOS: [&'static str; 1] = ["gstapplemedia"];

#[cfg(target_os = "windows")]
#[derive(askama::Template)]
#[template(path = "sender.Product.wxs.askama", escape = "none")]
struct ProductTemplate {
    version: String,
    dll_components: String,
}

#[cfg(target_os = "macos")]
#[derive(askama::Template)]
#[template(path = "sender.Info.plist.askama")]
struct InfoPlistTemplate {
    version: String,
}

#[derive(Subcommand)]
pub enum AndroidSenderCommand {
    Check,
    Clippy,
    BuildLibGst,
    Build {
        #[clap(short, long)]
        release: bool,
        #[clap(short, long)]
        target: Option<AndroidAbiTarget>,
    },
}

#[derive(Args)]
pub struct AndroidSenderArgs {
    #[clap(subcommand)]
    pub cmd: AndroidSenderCommand,
    #[clap(long)]
    pub android_home_override: Option<String>,
    #[clap(long)]
    pub android_ndk_root_override: Option<String>,
    #[clap(long)]
    pub gstreamer_root_override: Option<String>,
}

#[derive(Subcommand)]
pub enum SenderCommand {
    Android(AndroidSenderArgs),
    #[cfg(target_os = "windows")]
    BuildWindowsInstaller,
    #[cfg(target_os = "macos")]
    BuildMacosInstaller(BuildMacosInstallerArgs),
}

#[derive(Args)]
pub struct SenderArgs {
    #[clap(subcommand)]
    pub cmd: SenderCommand,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn get_sender_version() -> String {
    let sender_toml = std::fs::read_to_string("senders/desktop/Cargo.toml").unwrap();
    let doc = sender_toml.parse::<toml_edit::DocumentMut>().unwrap();
    doc["package"]["version"].as_str().unwrap().to_string()
}

/// `b` must not start with `/`
fn concat_path(a: &Utf8PathBuf, b: &str) -> Utf8PathBuf {
    let mut res = a.clone();
    res.push(b);
    res
}

impl SenderArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let root_path = workspace::root_path()?;
        let _p = sh.push_dir(root_path.clone());

        match self.cmd {
            SenderCommand::Android(args) => {
                let _env_andr_sdk = sh.push_env(
                    "ANDROID_HOME",
                    concat_path(
                        &root_path,
                        &args
                            .android_home_override
                            .unwrap_or(crate::android::ANDROID_HOME_PATH.to_owned()),
                    ),
                );
                let _env_ndk = sh.push_env(
                    "ANDROID_NDK_ROOT",
                    concat_path(
                        &root_path,
                        &args
                            .android_ndk_root_override
                            .clone()
                            .unwrap_or(crate::android::NDK_PATH.to_owned()),
                    ),
                );
                // Needed for some skia stuff on some arm target
                let _env_andr_ndk = sh.push_env(
                    "ANDROID_NDK",
                    concat_path(
                        &root_path,
                        &args
                            .android_ndk_root_override
                            .unwrap_or(crate::android::NDK_PATH.to_owned()),
                    ),
                );
                let _env_gst = sh.push_env(
                    "GSTREAMER_ROOT_ANDROID",
                    concat_path(
                        &root_path,
                        &args
                            .gstreamer_root_override
                            .unwrap_or(crate::android::GST_ANDROID_PATH.to_owned()),
                    ),
                );
                let _env_jar = sh.push_env(
                    "ANDROID_JAR",
                    sh.var("ANDROID_HOME").unwrap() + "/platforms/android-35/android.jar",
                ); // TODO: needed?
                let _env_pkg_config_cross = sh.push_env("PKG_CONFIG_ALLOW_CROSS", "1");
                // let _env_pkg_config_path = sh.push_env(
                //     "PKG_CONFIG_PATH"
                //     "${PKG_CONFIG_SYSROOT_DIR}/lib/pkgconfig"
                // )

                match args.cmd {
                    AndroidSenderCommand::Check => cmd!(
                        sh,
                        "cargo ndk --target x86_64-linux-android check -p android-sender"
                    )
                    .run()?,
                    AndroidSenderCommand::Clippy => cmd!(
                        sh,
                        "cargo ndk --target x86_64-linux-android clippy -p android-sender"
                    )
                    .run()?,
                    AndroidSenderCommand::BuildLibGst => {
                        let _env_build_system = sh.push_env(
                            "BUILD_SYSTEM",
                            sh.var("ANDROID_NDK_ROOT").unwrap() + "/build/core",
                        );
                        let _env_gst_java_src_dir = sh.push_env(
                            "GSTREAMER_JAVA_SRC_DIR",
                            concat_path(&root_path, "senders/android/app/src/main/java"),
                        );
                        let _env_ndk_project_path = sh.push_env(
                            "NDK_PROJECT_PATH",
                            concat_path(&root_path, "senders/android/app/"),
                        );
                        let _env_gst_ndk_build_path = sh.push_env(
                            "GSTREAMER_NDK_BUILD_PATH",
                            sh.var("GSTREAMER_ROOT_ANDROID").unwrap()
                                + "/share/gst-android/ndk-build",
                        );

                        let _t = sh.push_dir("target");

                        let ndk_root = sh.var("ANDROID_NDK_ROOT").unwrap();
                        cmd!(sh, "make -f {ndk_root}/build/core/build-local.mk").run()?;
                    }
                    AndroidSenderCommand::Build { release, target } => {
                        let out_dir =
                            concat_path(&root_path, "senders/android/app/src/main/jniLibs");

                        let targets = target.map(|t| vec![t]).unwrap_or(vec![
                            AndroidAbiTarget::X64,
                            AndroidAbiTarget::X86,
                            AndroidAbiTarget::Arm64,
                            AndroidAbiTarget::Arm32,
                        ]);

                        for target in targets {
                            let gst_root = sh.var("GSTREAMER_ROOT_ANDROID").unwrap();
                            let _env_pkg_config_path = sh.push_env(
                                "PKG_CONFIG_PATH",
                                gst_root
                                    + match target {
                                        AndroidAbiTarget::X64 => "/x86_64",
                                        AndroidAbiTarget::X86 => "/x86",
                                        AndroidAbiTarget::Arm64 => "/arm64",
                                        AndroidAbiTarget::Arm32 => "/armv7",
                                    }
                                    + "/lib/pkgconfig",
                            );

                            let target = target.translate();

                            let mut args = vec![
                                "--target",
                                target,
                                "-o",
                                out_dir.as_str(),
                                "build",
                                "--package",
                                "android-sender",
                            ];
                            if release {
                                args.push("--release");
                            }

                            cmd!(sh, "cargo ndk {args...}").run()?;
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            SenderCommand::BuildWindowsInstaller => {
                // Inspired by https://github.com/servo/servo/tree/main/python/servo

                use askama::Template;

                let gst_root = crate::get_gst_root(&sh);

                cmd!(sh, "cargo build --release --package desktop-sender").run()?;

                let build_dir_root = crate::setup_build_dir(&sh, &root_path);

                let mut files_to_copy = Vec::new();
                files_to_copy.push((
                    concat_paths(&[
                        root_path.as_str(),
                        "target",
                        "release",
                        "desktop-sender.exe",
                    ]),
                    "fcast-sender.exe".to_string(),
                ));

                fn dlls() -> Vec<String> {
                    let mut dlls: Vec<String> = GSTREAMER_WIN_DEPENDENCY_LIBS
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    for lib in GSTREAMER_BASE_LIBS {
                        dlls.push(format!("{lib}-1.0-0.dll"));
                    }
                    dlls
                }

                fn plugins() -> Vec<String> {
                    GSTREAMER_PLUGIN_LIBS_COMMON
                        .iter()
                        .chain(GSTREAMER_PLUGIN_LIBS_WIN.iter())
                        .map(|s| format!("{s}.dll"))
                        .collect()
                }

                files_to_copy.extend(crate::find_dlls(&gst_root, dlls()));
                files_to_copy.extend(crate::find_plugins(&gst_root, plugins()));
                files_to_copy.extend(crate::find_msvc_redists(&sh));
                files_to_copy.extend(crate::find_c_runtime(
                    crate::find_windows_sdk_installation_path(),
                ));
                files_to_copy.push(("senders/extra/fcast.ico".into(), "fcast.ico".to_owned()));

                let mut dll_components = String::new();

                for (src, dst) in files_to_copy {
                    let dst = concat_path(&build_dir_root, &dst);
                    sh.copy_file(&src, &dst)?;
                    println!("Copied `{src}` to `{dst}`");

                    if dst.extension() == Some("dll") {
                        dll_components += &format!(r#"<File Source="{dst}" />"#);
                        dll_components += "\n";
                    }
                }

                let sender_version = get_sender_version();
                let product_wxs = ProductTemplate {
                    version: sender_version.clone(),
                    dll_components,
                }
                .render()?;

                sh.write_file(
                    concat_path(&build_dir_root, &"FCastSenderInstaller.wxs"),
                    product_wxs,
                )?;

                println!("############### Building installer ###############");

                {
                    let output = format!("FCastSender-{sender_version}-win64-installer.msi");
                    let _win_build_p = sh.push_dir(&build_dir_root);
                    cmd!(sh, "wix build -out {output} .\\FCastSenderInstaller.wxs").run()?;
                }
            }
            #[cfg(target_os = "macos")]
            SenderCommand::BuildMacosInstaller(BuildMacosInstallerArgs {
                sign,
                p12_file,
                p12_password_file,
                api_key_file,
            }) => {
                fn plugins() -> Vec<String> {
                    GSTREAMER_PLUGIN_LIBS_COMMON
                        .iter()
                        .chain(GSTREAMER_PLUGIN_LIBS_MACOS.iter())
                        .map(|s| format!("lib{s}.dylib"))
                        .collect()
                }

                let path_to_dmg_dir = root_path.join("target").join("fcast-sender-dmg");
                let app_top_level = path_to_dmg_dir.join("FCast Sender.app");
                let build_dir_root = app_top_level.join("Contents").join("MacOS");

                if sh.remove_path(&path_to_dmg_dir).is_ok() {
                    println!("Removed old build dir at `{path_to_dmg_dir:?}`")
                }

                sh.create_dir(&build_dir_root)?;

                let library_target_directory = build_dir_root.join("lib");
                sh.create_dir(&library_target_directory)?;

                cmd!(
                    sh,
                    "cargo build --profile release-lto --package desktop-sender"
                )
                .run()?;

                let binary_path = concat_paths(&[
                    root_path.as_str(),
                    "target",
                    "release-lto",
                    "desktop-sender",
                ]);

                std::fs::copy(&binary_path, build_dir_root.join("fcast-sender"))?;

                use askama::Template;

                let binary_dependencies = crate::find_libraries(&binary_path, plugins());

                let relative_path = Utf8PathBuf::from("lib/");

                println!("############### Rewriting dependencies to be relative ###############");

                crate::rewrite_dependencies_to_be_relative(
                    &binary_path,
                    &binary_dependencies,
                    &relative_path,
                );

                crate::process_dependencies(&sh, binary_dependencies, library_target_directory);

                println!("############### Writing resources ###############");

                let sender_version = get_sender_version();
                let info_plist = InfoPlistTemplate {
                    version: sender_version.clone(),
                }
                .render()?;
                sh.create_dir(app_top_level.join("Contents").join("Resources"))?;
                sh.copy_file(
                    root_path.join("senders").join("extra").join("fcast.icns"),
                    app_top_level
                        .join("Contents")
                        .join("Resources")
                        .join("fcast.icns"),
                )?;
                sh.write_file(
                    app_top_level.join("Contents").join("Info.plist"),
                    info_plist,
                )?;
                let applications_link_path = path_to_dmg_dir.join("Applications");

                let path_to_dmg = root_path
                    .join("target")
                    .join(format!("fcast-sender-{sender_version}-macos-aarch64.dmg"));
                sh.remove_path(&path_to_dmg)?;

                crate::create_package(
                    &sh,
                    crate::AppType::Sender,
                    sender_version,
                    app_top_level,
                    applications_link_path,
                    path_to_dmg,
                    path_to_dmg_dir,
                    sign,
                    p12_file,
                    p12_password_file,
                    api_key_file,
                );
            }
        }

        Ok(())
    }
}
