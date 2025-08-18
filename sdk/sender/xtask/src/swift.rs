use std::fs::rename;

use anyhow::Result;
use camino::Utf8Path;
use clap::{Args, Subcommand};
use uniffi_bindgen::{
    bindings::SwiftBindingGenerator, library_mode::generate_bindings, EmptyCrateConfigSupplier,
};
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
    generate_bindings(
        library_path,
        None,
        &SwiftBindingGenerator,
        &EmptyCrateConfigSupplier,
        None,
        ffi_generated_dir,
        false,
    )?;

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
        "ios-bindings".into(),
    )?;

    rename(
        format!("ios-bindings/{package_camel}FFI.modulemap"),
        format!("ios-bindings/{package_camel}.modulemap"),
    )?;

    sh.remove_path("ios-build")?;

    let profile = if release { "release" } else { "debug" };
    cmd!(
        sh,
        "xcodebuild -create-xcframework -library target/aarch64-apple-ios-sim/{profile}/lib{package_camel}.a -library target/aarch64-apple-ios/{profile}/lib{package_camel}.a -headers ios-bindings -output ios-build/{package_camel}.xcframework"
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
