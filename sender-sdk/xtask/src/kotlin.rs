use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use uniffi_bindgen::{
    bindings::KotlinBindingGenerator, library_mode::generate_bindings, EmptyCrateConfigSupplier,
};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum KotlinCommand {
    BuildAndroidLibrary {
        #[clap(long)]
        release: bool,
        #[clap(long)]
        src_dir: Utf8PathBuf,
    },
}

#[derive(Args)]
pub struct KotlinArgs {
    #[clap(subcommand)]
    pub cmd: KotlinCommand,
}

fn build_for_android_target(
    target: &str,
    profile: &str,
    dest_dir: &str,
    package_name: &str,
) -> Result<Utf8PathBuf> {
    let sh = sh();
    let _p = sh.push_dir(workspace::root_path()?);
    cmd!(
        sh,
        "cargo ndk --target {target} -o {dest_dir} build --profile {profile} -p {package_name} --no-default-features --features _android_defaults"
    )
    .run()?;

    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    let package_camel = package_name.replace('-', "_");
    let lib_name = format!("lib{package_camel}.so");
    Ok(workspace::target_path()?
        .join(target)
        .join(profile_dir_name)
        .join(lib_name))
}

fn generate_uniffi_bindings(library_path: &Utf8Path, ffi_generated_dir: &Utf8Path) -> Result<()> {
    let config_path = workspace::root_path()?.join("sender-sdk/fcast-sender-sdk/uniffi.toml");

    generate_bindings(
        library_path,
        None,
        &KotlinBindingGenerator,
        &EmptyCrateConfigSupplier,
        Some(&config_path),
        ffi_generated_dir,
        false,
    )?;

    Ok(())
}

fn build_android_library(release: bool, src_dir: Utf8PathBuf) -> Result<()> {
    let package_name = "fcast-sender-sdk";

    let jni_libs_dir = src_dir.join("jniLibs");
    let sh = sh();
    let _p = sh.push_dir(workspace::root_path()?);
    sh.create_dir(&jni_libs_dir)?;
    let jni_libs_dir_str = jni_libs_dir.as_str();

    let kotlin_generated_dir = src_dir.join("kotlin");
    sh.create_dir(&kotlin_generated_dir)?;

    let profile = if release { "release" } else { "dev" };

    build_for_android_target(
        "x86_64-linux-android",
        profile,
        jni_libs_dir_str,
        package_name,
    )?;
    build_for_android_target(
        "i686-linux-android",
        profile,
        jni_libs_dir_str,
        package_name,
    )?;
    build_for_android_target(
        "armv7-linux-androideabi",
        profile,
        jni_libs_dir_str,
        package_name,
    )?;
    let uniffi_lib_path = build_for_android_target(
        "aarch64-linux-android",
        profile,
        jni_libs_dir_str,
        package_name,
    )?;

    generate_uniffi_bindings(&uniffi_lib_path, &kotlin_generated_dir)?;

    Ok(())
}

impl KotlinArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            KotlinCommand::BuildAndroidLibrary { release, src_dir } => {
                build_android_library(release, src_dir)
            }
        }
    }
}
