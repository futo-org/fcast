use std::fs::rename;

use anyhow::Result;
use camino::Utf8Path;
use clap::{Args, Subcommand};
use uniffi_bindgen::bindings::{generate, GenerateOptions, TargetLanguage};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum SwiftCommand {
    BuildIosLibrary {
        #[clap(long)]
        release: bool,
    },
}

#[derive(Args)]
pub struct SwiftArgs {
    #[clap(subcommand)]
    pub cmd: SwiftCommand,
}

fn generate_uniffi_bindings(library_path: &Utf8Path, ffi_generated_dir: &Utf8Path) -> Result<()> {
    generate(GenerateOptions {
        languages: vec![TargetLanguage::Swift],
        source: library_path.to_path_buf(),
        out_dir: ffi_generated_dir.to_path_buf(),
        format: false,
        ..Default::default()
    })?;

    Ok(())
}

fn build_ios_library(release: bool) -> Result<()> {
    let package_name = "fcast-sender-sdk";
    let sh = sh();
    let _p = sh.push_dir(workspace::root_path()?);
    let profile = if release { "release" } else { "dev" };

    for target in ["aarch64-apple-ios-sim", "aarch64-apple-ios"] {
        cmd!(
            sh,
            "cargo build -p {package_name} --profile={profile} --target={target} --no-default-features --features _ios_defaults"
        )
        .run()?;
    }

    let package_camel = package_name.replace('-', "_");
    generate_uniffi_bindings(
        Utf8Path::new(&format!(
            "target/aarch64-apple-ios-sim/{}/lib{package_camel}.dylib",
            if release { "release" } else { "debug" }
        )),
        "ios-bindings/uniffi/".into(),
    )?;

    rename(
        format!("ios-bindings/uniffi/{package_camel}FFI.modulemap"),
        format!("ios-bindings/uniffi/module.modulemap"),
    )?;

    sh.remove_path(format!("ios-bindings/{package_camel}.xcframework"))?;

    let profile = if release { "release" } else { "debug" };
    cmd!(
        sh,
        "xcodebuild -create-xcframework -library target/aarch64-apple-ios-sim/{profile}/lib{package_camel}.a -library target/aarch64-apple-ios/{profile}/lib{package_camel}.a -headers ios-bindings/uniffi -output ios-bindings/{package_camel}.xcframework"
    ).run()?;

    Ok(())
}

impl SwiftArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            SwiftCommand::BuildIosLibrary { release } => build_ios_library(release),
        }
    }
}
