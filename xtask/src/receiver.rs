#[cfg(target_os = "windows")]
use anyhow::anyhow;
use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use xshell::cmd;

#[cfg(target_os = "macos")]
use crate::BuildMacosInstallerArgs;
use crate::{sh, workspace, AndroidAbiTarget};

#[cfg(target_os = "macos")]
#[derive(askama::Template)]
#[template(path = "receiver.Info.plist.askama")]
struct InfoPlistTemplate {
    version: String,
}

#[cfg(target_os = "windows")]
#[derive(askama::Template)]
#[template(path = "receiver.Product.wxs.askama", escape = "none")]
struct ProductTemplate {
    version: String,
    dll_components: String,
}

#[derive(Subcommand)]
pub enum AndroidReceiverCommand {
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
    /// Build a statically-linked GStreamer from a source tree and link the
    /// desktop receiver against it.
    BuildStatic(crate::gstreamer::GstreamerArgs),
    /// Build the statically-linked receiver and run it. Arguments after `--`
    /// are forwarded to the receiver binary.
    Run(RunStaticArgs),
    /// `cargo check` the desktop receiver against a static GStreamer.
    Check(CargoSubcmdArgs),
    /// `cargo clippy` the desktop receiver against a static GStreamer.
    Clippy(CargoSubcmdArgs),
    /// `cargo test` receiver-core against a static GStreamer (links + runs the
    /// test binary). Args after `--` are forwarded to the libtest harness,
    /// e.g. `-- --nocapture` or a test-name filter.
    Test(CargoSubcmdArgs),
    #[cfg(target_os = "windows")]
    BuildWindowsInstaller(crate::gstreamer::GstreamerArgs),
    #[cfg(target_os = "macos")]
    BuildMacosInstaller(BuildMacosInstallerArgs),
}

#[derive(Args)]
pub struct CargoSubcmdArgs {
    #[command(flatten)]
    pub gst: crate::gstreamer::GstreamerArgs,
    /// Check/lint the release profile instead of the default fast debug build.
    #[arg(long)]
    pub release: bool,
    /// Extra args appended to the inner cargo invocation (everything after `--`),
    /// e.g. `-- --message-format=json` for editor integration (rustic/eglot).
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct RunStaticArgs {
    #[command(flatten)]
    pub gst: crate::gstreamer::GstreamerArgs,
    /// Build the receiver in release instead of the default fast debug build
    /// (receiver side only; GStreamer is controlled by --gst-buildtype).
    #[arg(long)]
    pub release: bool,
    /// Arguments forwarded to the receiver binary (everything after `--`).
    #[arg(last = true)]
    pub args: Vec<String>,
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
    let receiver_toml = std::fs::read_to_string("receivers/desktop/Cargo.toml").unwrap();
    let doc = receiver_toml.parse::<toml_edit::DocumentMut>().unwrap();
    doc["package"]["version"].as_str().unwrap().to_string()
}

impl ReceiverArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let root_path = workspace::root_path()?;
        // Run from the workspace root so the receiver build's relative paths
        // (target/gstreamer-src, target/<triple>/…) resolve correctly even when
        // invoked from a subdirectory — e.g. rust-analyzer / rustic launching us
        // inside a crate. push_dir covers the xshell commands; set_current_dir
        // covers the std::fs / std::process::Command calls that bypass the shell
        // (the source-reuse `.git` check and the `run` binary exec).
        std::env::set_current_dir(&root_path)
            .map_err(|e| anyhow::anyhow!("chdir to workspace root {root_path}: {e}"))?;
        let _p = sh.push_dir(root_path.clone());

        match self.cmd {
            ReceiverCommand::BuildStatic(args) => return args.run(),
            ReceiverCommand::Run(a) => return a.gst.run_binary(a.args, a.release),
            ReceiverCommand::Check(a) => return a.gst.check(a.args, a.release),
            ReceiverCommand::Clippy(a) => return a.gst.clippy(a.args, a.release),
            ReceiverCommand::Test(a) => return a.gst.test(a.args, a.release),
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
                    AndroidReceiverCommand::Build { release, target } => {
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
                            }

                            cmd!(sh, "cargo ndk {args...}").run()?;
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            ReceiverCommand::BuildWindowsInstaller(static_args) => {
                // scope=Full (the mac/win default): gstreamer, the glib/pango
                // stack, codecs and the GIO TLS module are ALL statically
                // linked into the binary — no GStreamer dev kit is involved
                // and no runtime DLLs are bundled beyond the MSVC redists.
                // NEEDS VALIDATION on a Windows box: check the exe imports
                // with `dumpbin /dependents` — anything beyond OS DLLs and
                // the redists indicates a dep that escaped the static build.
                let binary = static_args
                    .build()?
                    .ok_or_else(|| anyhow!("--clean/--gstreamer-only produce no binary"))?;

                let build_dir_root = crate::setup_build_dir(&sh, &root_path);

                let mut files_to_copy = Vec::new();
                files_to_copy.push((
                    concat_path(&root_path, binary.as_str()),
                    "fcast-receiver.exe".to_string(),
                ));

                files_to_copy.extend(crate::find_msvc_redists(&sh));
                files_to_copy.extend(crate::find_c_runtime(
                    crate::find_windows_sdk_installation_path(),
                ));
                files_to_copy.push(("receivers/extra/fcast.ico".into(), "fcast.ico".to_owned()));

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

                use askama::Template;

                let receiver_version = get_receiver_version();
                let product_wxs = ProductTemplate {
                    version: receiver_version.clone(),
                    dll_components,
                }
                .render()?;

                sh.write_file(
                    concat_path(&build_dir_root, &"FCastReceiverInstaller.wxs"),
                    product_wxs,
                )?;

                println!("############### Building installer ###############");

                {
                    let output = format!("FCastReceiver-{receiver_version}-win64-installer.msi");
                    let _win_build_p = sh.push_dir(&build_dir_root);
                    cmd!(sh, "wix build -out {output} .\\FCastReceiverInstaller.wxs").run()?;
                }
            }
            #[cfg(target_os = "macos")]
            ReceiverCommand::BuildMacosInstaller(BuildMacosInstallerArgs {
                sign,
                p12_file,
                p12_password_file,
                api_key_file,
                static_args,
            }) => {
                let path_to_dmg_dir = root_path.join("target").join("fcast-receiver-dmg");
                let app_top_level = path_to_dmg_dir.join("FCast Receiver.app");
                let build_dir_root = app_top_level.join("Contents").join("MacOS");

                if sh.remove_path(&path_to_dmg_dir).is_ok() {
                    println!("Removed old build dir at `{path_to_dmg_dir:?}`")
                }

                sh.create_dir(&build_dir_root)?;

                // scope=Full (the macOS default): gstreamer, the glib/pango
                // stack, codecs and the GIO TLS module are ALL statically
                // linked — no GStreamer.framework dev kit, no dylib bundling,
                // no install_name_tool rewriting. Only OS frameworks may
                // remain dynamic; anything else means a dep escaped the
                // static build, so fail loudly instead of shipping it.
                let mut static_args = static_args;
                static_args.no_default_features = true; // no systray on macOS
                let binary = static_args
                    .build()?
                    .ok_or_else(|| anyhow::anyhow!("--clean/--gstreamer-only produce no binary"))?;
                let binary_path = concat_path(&root_path, binary.as_str());

                let leftover = crate::find_non_system_dependencies_with_otool(&binary_path);
                if !leftover.is_empty() {
                    anyhow::bail!(
                        "static build still links non-system dylibs: {leftover:?}\n\
                         These would dangle on user machines — fix the static build \
                         instead of bundling them."
                    );
                }

                std::fs::copy(&binary_path, build_dir_root.join("fcast-receiver"))?;

                use askama::Template;

                println!("############### Writing resources ###############");

                let receiver_version = get_receiver_version();
                let info_plist = InfoPlistTemplate {
                    version: receiver_version.clone(),
                }
                .render()?;
                sh.create_dir(app_top_level.join("Contents").join("Resources"))?;
                sh.copy_file(
                    root_path.join("receivers").join("extra").join("fcast.icns"),
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
