use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use xshell::cmd;

#[cfg(target_os = "macos")]
use crate::BuildMacosInstallerArgs;
use crate::{concat_paths, sh, workspace, AndroidAbiTarget};

#[cfg(any(target_os = "macos", target_os = "windows"))]
const GSTREAMER_PLUGIN_LIBS_COMMON: [&'static str; 42] = [
    "gstrtsp",
    "gstisobmff",
    "gstadaptivedemux2",
    "gstdvdsub",
    "gstsubparse",
    "gstassrender",
    "gstcoreelements",
    "gstnice",
    "gstapp",
    "gstaudioconvert",
    "gstaudioresample",
    "gstgio",
    "gstogg",
    "gstopengl",
    "gstopus",
    "gstplayback",
    "gsttheora",
    "gsttypefindfunctions",
    "gstvideoconvertscale",
    "gstvolume",
    "gstvorbis",
    "gstaudiofx",
    "gstaudioparsers",
    "gstautodetect",
    "gstdeinterlace",
    "gstid3demux",
    "gstinterleave",
    "gstisomp4",
    "gstmatroska",
    "gstrtp",
    "gstrtpmanager",
    "gstvideofilter",
    "gstvpx",
    "gstwavparse",
    "gstaudiobuffersplit",
    "gstdtls",
    "gstid3tag",
    "gstproxy",
    "gstvideoparsersbad",
    "gstwebrtc",
    "gstlibav",
    "gstflac",
];

#[cfg(target_os = "macos")]
const GSTREAMER_PLUGIN_LIBS_MACOS: [&'static str; 3] =
    ["gstapplemedia", "gstosxaudio", "gstosxvideo"];

#[cfg(target_os = "macos")]
#[derive(askama::Template)]
#[template(path = "receiver.Info.plist.askama")]
struct InfoPlistTemplate {
    version: String,
}

#[derive(Subcommand)]
pub enum AndroidReceiverCommand {
    Check,
    Clippy,
    BuildLibGst,
    Build {
        #[clap(short, long)]
        release: bool,
        #[clap(long)]
        release_lto: bool,
        #[clap(short, long)]
        target: Option<AndroidAbiTarget>,
    },
}

#[derive(Args)]
pub struct AndroidReceiverArgs {
    #[clap(subcommand)]
    pub cmd: AndroidReceiverCommand,
    #[clap(long)]
    pub android_home_override: Option<String>,
    #[clap(long)]
    pub android_ndk_root_override: Option<String>,
    #[clap(long)]
    pub gstreamer_root_override: Option<String>,
}

#[derive(Subcommand)]
pub enum ReceiverCommand {
    Android(AndroidReceiverArgs),
    #[cfg(target_os = "macos")]
    BuildMacosInstaller(BuildMacosInstallerArgs),
}

#[derive(Args)]
pub struct ReceiverArgs {
    #[clap(subcommand)]
    pub cmd: ReceiverCommand,
}

fn concat_path(a: &Utf8PathBuf, b: &str) -> Utf8PathBuf {
    let mut res = a.clone();
    res.push(b);
    res
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn get_receiver_version() -> String {
    let sender_toml = std::fs::read_to_string("receivers/experimental/desktop/Cargo.toml").unwrap();
    let doc = sender_toml.parse::<toml_edit::DocumentMut>().unwrap();
    doc["package"]["version"].as_str().unwrap().to_string()
}

impl ReceiverArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let root_path = workspace::root_path()?;
        let _p = sh.push_dir(root_path.clone());

        match self.cmd {
            ReceiverCommand::Android(args) => {
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

                match args.cmd {
                    AndroidReceiverCommand::Check => cmd!(
                        sh,
                        "cargo ndk --target aarch64-linux-android check -p receiver-android"
                    )
                    .run()?,
                    AndroidReceiverCommand::Clippy => todo!(),
                    AndroidReceiverCommand::BuildLibGst => {
                        let _env_build_system = sh.push_env(
                            "BUILD_SYSTEM",
                            sh.var("ANDROID_NDK_ROOT").unwrap() + "/build/core",
                        );
                        let _env_gst_java_src_dir = sh.push_env(
                            "GSTREAMER_JAVA_SRC_DIR",
                            concat_path(
                                &root_path,
                                "receivers/experimental/android/app/src/main/java",
                            ),
                        );
                        let _env_ndk_project_path = sh.push_env(
                            "NDK_PROJECT_PATH",
                            concat_path(&root_path, "receivers/experimental/android/app/"),
                        );
                        let _env_gst_ndk_build_path = sh.push_env(
                            "GSTREAMER_NDK_BUILD_PATH",
                            sh.var("GSTREAMER_ROOT_ANDROID").unwrap()
                                + "/share/gst-android/ndk-build",
                        );

                        let _t = sh.push_dir("target/");

                        let ndk_root = sh.var("ANDROID_NDK_ROOT").unwrap();
                        cmd!(sh, "make -f {ndk_root}/build/core/build-local.mk").run()?;
                    }
                    AndroidReceiverCommand::Build {
                        release,
                        release_lto,
                        target,
                    } => {
                        let out_dir = concat_path(
                            &root_path,
                            "receivers/experimental/android/app/src/main/jniLibs",
                        );

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
                                "receiver-android",
                            ];
                            if release {
                                args.push("--release");
                                assert!(!release_lto);
                            } else if release_lto {
                                args.extend_from_slice(&["--profile", "release-lto"]);
                            }

                            cmd!(sh, "cargo ndk {args...}").run()?;
                        }
                    }
                }
            }
            ReceiverCommand::BuildMacosInstaller(BuildMacosInstallerArgs {
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

                let path_to_dmg_dir = root_path.join("target").join("fcast-receiver-dmg");
                let app_top_level = path_to_dmg_dir.join("FCast Receiver.app");
                let build_dir_root = app_top_level.join("Contents").join("MacOS");

                if sh.remove_path(&path_to_dmg_dir).is_ok() {
                    println!("Removed old build dir at `{path_to_dmg_dir:?}`")
                }

                let library_target_directory = build_dir_root.join("lib");
                sh.create_dir(&library_target_directory)?;

                cmd!(
                    sh,
                    "cargo build --profile release-lto --package desktop-receiver"
                )
                .run()?;

                let binary_path = concat_paths(&[
                    root_path.as_str(),
                    "target",
                    "release-lto",
                    "desktop-receiver",
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

                let receiver_version = get_receiver_version();
                let info_plist = InfoPlistTemplate {
                    version: receiver_version.clone(),
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
                let path_to_dmg = root_path.join("target").join(format!(
                    "fcast-receiver-{receiver_version}-macos-aarch64.dmg"
                ));
                sh.remove_path(&path_to_dmg)?;

                crate::create_package(
                    &sh,
                    crate::AppType::Receiver,
                    receiver_version,
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
