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

use crate::{sh, workspace, AndroidAbiTarget, BuildMacosInstallerArgs};

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
#[template(path = "Product.wxs.askama", escape = "none")]
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

                let gstreamer_root = Utf8PathBuf::from(
                    sh.var("GSTREAMER_1_0_ROOT_MSVC_X86_64")
                        .expect("GStreamer not found"),
                );
                assert!(
                    gstreamer_root.exists(),
                    "GStreamer installation is likely broken"
                );
                println!("GStreamer root is: {gstreamer_root:?}");

                cmd!(sh, "cargo build --release --package desktop-sender").run()?;

                let build_dir_root = {
                    let mut p = root_path.clone();
                    p.extend(["target", "win-build"]);
                    p
                };

                if sh.remove_path(&build_dir_root).is_ok() {
                    println!("Removed old build dir at `{build_dir_root:?}`")
                }

                sh.create_dir(&build_dir_root)?;

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

                println!("############### Finding DLLs ###############");
                for dll in dlls() {
                    let mut dll_path = gstreamer_root.clone();
                    dll_path.extend(["bin", &dll]);
                    assert!(
                        dll_path.exists(),
                        "DLL `{dll}` is missing full-path={dll_path:?}"
                    );
                    println!("Required library found: `{dll}`");
                    files_to_copy.push((dll_path, dll));
                }

                println!("############### Finding plugins ###############");
                for plugin in plugins() {
                    let mut plugin_path = gstreamer_root.clone();
                    plugin_path.extend(["lib", "gstreamer-1.0", &plugin]);
                    assert!(
                        plugin_path.exists(),
                        "Plugin `{plugin}` is missing full-path={plugin_path:?}"
                    );
                    println!("Required plugin found: `{plugin}`");
                    files_to_copy.push((plugin_path, plugin));
                }

                fn find_vswhere(paths: &[Utf8PathBuf]) -> Option<Utf8PathBuf> {
                    for path in paths {
                        let vswhere = concat_paths(&[
                            path.as_str(),
                            "Microsoft Visual Studio",
                            "Installer",
                            "vswhere.exe",
                        ]);
                        if vswhere.exists() {
                            return Some(vswhere);
                        }
                    }

                    None
                }

                fn get_msvc_installations(sh: &Rc<Shell>) -> Vec<(String, Utf8PathBuf)> {
                    let program_files = Utf8PathBuf::from(
                        sh.var("POGRAMFILES")
                            .unwrap_or("C:\\Program Files".to_string()),
                    );
                    let program_files_x86 = Utf8PathBuf::from(
                        sh.var("ProgramFiles(x86)")
                            .unwrap_or("C:\\Program Files (x86)".to_string()),
                    );

                    let vswhere = find_vswhere(&[program_files, program_files_x86])
                        .expect("Could not find vswhere.exe");

                    let output = Command::new(vswhere)
                        .arg("-format")
                        .arg("json")
                        .arg("-products")
                        .arg("*")
                        .arg("-requires")
                        .arg("Microsoft.VisualStudio.Component.VC.Tools.x86.x64")
                        .arg("-requires")
                        .arg("Microsoft.VisualStudio.Component.Windows*SDK.*")
                        .arg("-utf8")
                        .output()
                        .expect("Failed to execute vswhere command");

                    use serde_json::Value;

                    let output_str = String::from_utf8_lossy(&output.stdout);
                    let installations: Value =
                        serde_json::from_str(&output_str).unwrap_or(Value::Null);

                    let mut msvcs = Vec::new();
                    if let Value::Array(installs) = installations {
                        for install in installs {
                            if let Value::Object(install_obj) = install {
                                let installed_version = install_obj["installationVersion"]
                                    .as_str()
                                    .unwrap_or("")
                                    .split('.')
                                    .next()
                                    .unwrap_or("");
                                let installed_version = format!("{}.0", installed_version);

                                if &installed_version != "17.0" && &installed_version != "18.0" {
                                    continue;
                                }

                                let installation_path = install_obj["installationPath"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                let vc_install_path =
                                    Utf8PathBuf::from(&installation_path).join("VC");

                                msvcs.push((installation_path, vc_install_path));
                            }
                        }
                    }

                    msvcs
                }

                fn find_msvc_redist_dirs(sh: &Rc<Shell>) -> Vec<Utf8PathBuf> {
                    let installations = get_msvc_installations(&sh);
                    let mut results = Vec::new();
                    for (_, installation) in installations {
                        let redist_dir = installation.join("Redist").join("MSVC");
                        if !redist_dir.is_dir() {
                            continue;
                        }

                        let subdirectories = std::fs::read_dir(&redist_dir)
                            .expect("Failed to read redist directory")
                            .filter_map(Result::ok)
                            .collect::<Vec<_>>();

                        for entry in subdirectories.iter().rev() {
                            let redist_path = entry.path();
                            for redist_version in
                                ["VC141", "VC142", "VC143", "VC145", "VC150", "VC160"].iter()
                            {
                                let path1 =
                                    redist_path.join(format!("Microsoft.{}.CRT", redist_version));
                                let path2 = redist_path
                                    .join("onecore")
                                    .join("x64")
                                    .join(format!("Microsoft.{}.CRT", redist_version));

                                for path in vec![path1, path2] {
                                    if path.is_dir() {
                                        results.push(Utf8PathBuf::try_from(path).unwrap());
                                    }
                                }
                            }
                        }
                    }

                    results
                }

                let redist_dirs = find_msvc_redist_dirs(&sh);
                let mut did_find_redists = false;
                for redist_dir in redist_dirs {
                    let maybe_msvcp = redist_dir.join("msvcp140.dll");
                    let maybe_vcruntime = redist_dir.join("vcruntime140.dll");
                    let maybe_vcruntime_1 = redist_dir.join("vcruntime140_1.dll");
                    if maybe_msvcp.exists()
                        && maybe_vcruntime.exists()
                        && maybe_vcruntime_1.exists()
                    {
                        files_to_copy.push((maybe_msvcp, "msvcp140.dll".to_owned()));
                        files_to_copy.push((maybe_vcruntime, "vcruntime140.dll".to_owned()));
                        files_to_copy.push((maybe_vcruntime_1, "vcruntime140_1.dll".to_owned()));
                        did_find_redists = true;
                        break;
                    }
                }

                assert!(did_find_redists, "Couldn't find MSVC redistributables");

                fn find_windows_sdk_installation_path() -> Utf8PathBuf {
                    use winreg::{enums::*, RegKey};
                    let key_path = r"SOFTWARE\Wow6432Node\Microsoft\Microsoft SDKs\Windows\v10.0";
                    let hkml = RegKey::predef(HKEY_LOCAL_MACHINE);
                    let key = hkml.open_subkey(key_path).unwrap();
                    let installation_folder: String = key.get_value("InstallationFolder").unwrap();
                    Utf8PathBuf::from(installation_folder)
                }

                fn find_c_runtime(
                    windows_sdk_dir: Utf8PathBuf,
                ) -> Vec<(Utf8PathBuf, &'static str)> {
                    use std::ffi::OsStr;
                    let crt_dlls = ["api-ms-win-crt-runtime-l1-1-0.dll"];
                    let mut paths = Vec::new();
                    for dll_name in crt_dlls {
                        for entry in walkdir::WalkDir::new(&windows_sdk_dir)
                            .into_iter()
                            .filter_map(Result::ok)
                        {
                            if entry.file_type().is_file()
                                && entry.file_name() == OsStr::new(dll_name)
                            {
                                let path = entry.path();
                                if path
                                    .parent()
                                    .map_or(false, |parent| parent.ends_with("x64"))
                                {
                                    paths.push((
                                        Utf8PathBuf::from(Utf8Path::from_path(path).unwrap()),
                                        dll_name,
                                    ));
                                    break;
                                }
                            }
                        }
                    }

                    assert_eq!(paths.len(), crt_dlls.len(), "One or more CRT librarie(s) was not found (expected: {crt_dlls:?}, got: {paths:?})");

                    paths
                }

                let windows_sdk_dir = find_windows_sdk_installation_path();
                let windows_crt_dlls = find_c_runtime(windows_sdk_dir);
                for (src, dst) in windows_crt_dlls {
                    files_to_copy.push((src, dst.to_owned()));
                }

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
