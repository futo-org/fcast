use anyhow::Result;
use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{sh, workspace};

const ANDROID_SDK_PATH: &str = "thirdparty/android-sdk-commandlinetools";
/// Always relative to project root
pub const ANDROID_HOME_PATH: &str = "thirdparty/Android/Sdk";

const GST_ANDROID_URL: &str = "https://gstreamer.freedesktop.org/pkg/android/1.28.3/gstreamer-1.0-android-universal-1.28.3.tar.xz";
const GST_ANDROID_AR_PATH: &str = "thirdparty/gstreamer-1.0-android-universal-1.28.3.tar.xz";
/// Always relative to project root
pub const GST_ANDROID_PATH: &str = "thirdparty/gstreamer-1.0-android-universal-1.28.3";

pub const NDK_PATH: &str = "thirdparty/android-ndk-r25c";

/// The API level the whole android lane compiles against. MUST match
/// `compileSdk` in receivers/experimental/android/app/build.gradle: the java
/// glue in the native lane and gradle build against one android.jar or the
/// two disagree about what the platform offers.
pub const ANDROID_PLATFORM: &str = "android-36";
/// The API level the native libraries link against. cargo-ndk picks the
/// sysroot from it, and its own default of 21 has no libnativewindow and no
/// libaaudio, both of which the video and audio lanes call. Matches the
/// GSTREAMER_SRC_ANDROID_API default in the android-linker-* wrappers, which
/// is what the C build uses, and `minSdk` in the app's build.gradle.
pub const ANDROID_API: u32 = 28;
const ANDROID_BUILD_TOOLS: &str = "36.0.0";

fn sdk_paths() -> (String, String) {
    let os = if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "win"
    } else {
        "linux"
    };
    let filename = format!("commandlinetools-{os}-11076708_latest.zip");
    (
        format!("https://dl.google.com/android/repository/{filename}"),
        format!("thirdparty/{filename}"),
    )
}

fn ndk_paths() -> (String, String) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let filename = format!("android-ndk-r25c-{os}.zip");
    (
        format!("https://dl.google.com/android/repository/{filename}"),
        format!("thirdparty/{filename}"),
    )
}

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
                let (url, zip) = sdk_paths();
                cmd!(sh, "wget {url} -O {zip}").run()?;
                sh.create_dir(ANDROID_SDK_PATH)?;
                cmd!(sh, "unzip -o {zip} -d {ANDROID_SDK_PATH}").run()?;
                sh.remove_path(&zip)?;

                let sdkmanager = if cfg!(windows) {
                    "sdkmanager.bat"
                } else {
                    "sdkmanager"
                };
                let shell_code = format!("yes | {ANDROID_SDK_PATH}/cmdline-tools/bin/{sdkmanager} --sdk_root={ANDROID_HOME_PATH} --licenses");
                cmd!(sh, "sh -c {shell_code}").run()?;

                let platform = format!("platforms;{ANDROID_PLATFORM}");
                let build_tools = format!("build-tools;{ANDROID_BUILD_TOOLS}");
                cmd!(sh, "{ANDROID_SDK_PATH}/cmdline-tools/bin/{sdkmanager} --sdk_root={ANDROID_HOME_PATH} --install {platform}").run()?;
                cmd!(sh, "{ANDROID_SDK_PATH}/cmdline-tools/bin/{sdkmanager} --sdk_root={ANDROID_HOME_PATH} --install {build_tools}").run()?;
            }
            AndroidCommand::DownloadGstreamer => {
                cmd!(sh, "wget {GST_ANDROID_URL} -O {GST_ANDROID_AR_PATH}").run()?;
                sh.create_dir(GST_ANDROID_PATH)?;
                cmd!(sh, "tar -xf {GST_ANDROID_AR_PATH} -C {GST_ANDROID_PATH}").run()?;
                sh.remove_path(GST_ANDROID_AR_PATH)?;
            }
            AndroidCommand::DownloadNdk => {
                let (url, zip) = ndk_paths();
                cmd!(sh, "wget {url} -O {zip}").run()?;
                sh.create_dir(NDK_PATH)?;
                cmd!(sh, "unzip -o {zip} -d thirdparty/").run()?;
                sh.remove_path(&zip)?;
                // TODO: find android-ndk-r25c/ -type f -exec sed -i
                // '1{/^#!\/bin\/bash$/s//#!\/usr\/bin\/env bash/}' {} +
            }
        }

        Ok(())
    }
}
