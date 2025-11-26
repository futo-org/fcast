use anyhow::Result;
use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{sh, workspace};

const ANDROID_SDK_URL: &str =
    "https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip";
const SDK_ZIP_PATH: &str = "thirdparty/commandlinetools-linux-11076708_latest.zip";
const ANDROID_SDK_PATH: &str = "thirdparty/android-sdk-commandlinetools";
/// Always relative to project root
pub const ANDROID_HOME_PATH: &str = "thirdparty/Android/Sdk";

const GST_ANDROID_URL: &str = "https://gstreamer.freedesktop.org/data/pkg/android/1.26.8/gstreamer-1.0-android-universal-1.26.8.tar.xz";
const GST_ANDROID_AR_PATH: &str = "thirdparty/gstreamer-1.0-android-universal-1.26.8.tar.xz";
/// Always relative to project root
pub const GST_ANDROID_PATH: &str = "thirdparty/gstreamer-1.0-android-universal-1.26.8";

const NDK_URL: &str = "https://dl.google.com/android/repository/android-ndk-r25c-linux.zip";
/// Always relative to project root
const NDK_ZIP_PATH: &str = "thirdparty/android-ndk-r25c-linux.zip";
pub const NDK_PATH: &str = "thirdparty/android-ndk-r25c";

#[derive(Subcommand)]
pub enum AndroidCommand {
    DownloadSdk,
    DownloadGstreamer,
    DownloadNdk,
}

#[derive(Args)]
pub struct AndroidArgs {
    #[clap(subcommand)]
    pub cmd: AndroidCommand,
}

impl AndroidArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);
        sh.create_dir("thirdparty")?;

        match self.cmd {
            AndroidCommand::DownloadSdk => {
                cmd!(sh, "wget {ANDROID_SDK_URL} -O {SDK_ZIP_PATH}").run()?;
                sh.create_dir(ANDROID_SDK_PATH)?;
                cmd!(sh, "unzip {SDK_ZIP_PATH} -d {ANDROID_SDK_PATH}").run()?;
                sh.remove_path(SDK_ZIP_PATH)?;

                let shell_code = format!("yes | {ANDROID_SDK_PATH}/cmdline-tools/bin/sdkmanager --sdk_root={ANDROID_HOME_PATH} --licenses");
                cmd!(sh, "sh -c {shell_code}").run()?;

                cmd!(sh, "{ANDROID_SDK_PATH}/cmdline-tools/bin/sdkmanager --sdk_root={ANDROID_HOME_PATH} --install platforms;android-35").run()?;
                cmd!(sh, "{ANDROID_SDK_PATH}/cmdline-tools/bin/sdkmanager --sdk_root={ANDROID_HOME_PATH} --install build-tools;35.0.0").run()?;
            }
            AndroidCommand::DownloadGstreamer => {
                cmd!(sh, "wget {GST_ANDROID_URL} -O {GST_ANDROID_AR_PATH}").run()?;
                sh.create_dir(GST_ANDROID_PATH)?;
                cmd!(sh, "tar -xf {GST_ANDROID_AR_PATH} -C {GST_ANDROID_PATH}").run()?;
                sh.remove_path(GST_ANDROID_AR_PATH)?;
            }
            AndroidCommand::DownloadNdk => {
                cmd!(sh, "wget {NDK_URL} -O {NDK_ZIP_PATH}").run()?;
                sh.create_dir(NDK_PATH)?;
                cmd!(sh, "unzip {NDK_ZIP_PATH} -d thirdparty/").run()?;
                sh.remove_path(NDK_ZIP_PATH)?;
                // TODO: find android-ndk-r25c/ -type f -exec sed -i '1{/^#!\/bin\/bash$/s//#!\/usr\/bin\/env bash/}' {} +
            }
        }

        Ok(())
    }
}
