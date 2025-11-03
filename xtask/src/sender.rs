use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(clap::ValueEnum, Clone)]
pub enum AbiTarget {
    X64,
    X86,
    Arm64,
    Arm32,
}

impl AbiTarget {
    pub fn translate(&self) -> &'static str {
        match self {
            AbiTarget::X64 => "x86_64-linux-android",
            AbiTarget::X86 => "i686-linux-android",
            AbiTarget::Arm64 => "aarch64-linux-android",
            AbiTarget::Arm32 => "armv7-linux-androideabi",
        }
    }
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
        target: Option<AbiTarget>,
    },
}

#[derive(Args)]
pub struct AndroidSenderArgs {
    #[clap(subcommand)]
    pub cmd: AndroidSenderCommand,
}

#[derive(Subcommand)]
pub enum SenderCommand {
    Android(AndroidSenderArgs),
}

#[derive(Args)]
pub struct SenderArgs {
    #[clap(subcommand)]
    pub cmd: SenderCommand,
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
                    concat_path(&root_path, crate::android::ANDROID_HOME_PATH),
                );
                let _env_ndk = sh.push_env(
                    "ANDROID_NDK_ROOT",
                    concat_path(&root_path, crate::android::NDK_PATH),
                );
                // Needed for some skia stuff on some arm target
                let _env_andr_ndk = sh.push_env(
                    "ANDROID_NDK",
                    concat_path(&root_path, crate::android::NDK_PATH),
                );
                let _env_gst = sh.push_env(
                    "GSTREAMER_ROOT_ANDROID",
                    concat_path(&root_path, crate::android::GST_ANDROID_PATH),
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
                        let out_dir = concat_path(
                            &root_path,
                            "senders/android/app/src/main/jniLibs",
                        );

                        let targets = target.map(|t| vec![t]).unwrap_or(vec![
                            AbiTarget::X64,
                            AbiTarget::X86,
                            AbiTarget::Arm64,
                            AbiTarget::Arm32,
                        ]);

                        for target in targets {
                            let gst_root = sh.var("GSTREAMER_ROOT_ANDROID").unwrap();
                            let _env_pkg_config_path = sh.push_env(
                                "PKG_CONFIG_PATH",
                                gst_root
                                    + match target {
                                        AbiTarget::X64 => "/x86_64",
                                        AbiTarget::X86 => "/x86",
                                        AbiTarget::Arm64 => "/arm64",
                                        AbiTarget::Arm32 => "/armv7",
                                    }
                                    + "/lib/pkgconfig",
                            );

                            let target = target.translate();

                            #[rustfmt::skip]
                            let mut args = vec![
                                "--target", target,
                                "-o", out_dir.as_str(),
                                "build",
                                "--package", "android-sender",
                            ];
                            if release {
                                args.push("--release");
                            }

                            cmd!(sh, "cargo ndk {args...}").run()?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
