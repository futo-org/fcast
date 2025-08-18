use anyhow::Result;
use clap::{Parser, Subcommand};
use xshell::cmd;
use xtask::{
    kotlin,
    swift::{self, SwiftArgs, SwiftCommand},
};

#[derive(Subcommand)]
enum Command {
    Kotlin(kotlin::KotlinArgs),
    // GenerateIos, // TODO
    // # https://github.com/Tehnix/template-mobile-wasm
    // # export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer/
    // # cargo build -p fcast
    // # cargo run --bin uniffi-bindgen generate --library target/debug/libfcast.dylib --language swift --out-dir uniffi-out
    // # mv uniffi-out/fcastFFI.modulemap uniffi-out/fcast.modulemap
    // # cargo build -p fcast --target=aarch64-apple-ios-sim
    // # cargo run --bin uniffi-bindgen generate --library target/aarch64-apple-ios-sim/debug/libfcast.dylib --language swift --out-dir uniffi-out
    // # mv uniffi-out/fcastFFI.modulemap uniffi-out/fcast.modulemap
    // # rm -rf ios/FCast.xcframework
    // # xcodebuild -create-xcframework -library target/aarch64-apple-ios-sim/debug/libfcast.a -headers uniffi-out \
    //     -output ios/FCast.xcframework
    // # cp uniffi-out/fcast.swift sender-ios/Generated
    Swift(swift::SwiftArgs),
    GenerateIos,
    Hack,
}

#[derive(Parser)]
struct Xtask {
    #[clap(subcommand)]
    cmd: Command,
}

fn main() -> Result<()> {
    match Xtask::parse().cmd {
        Command::Kotlin(cmd) => cmd.run(),
        Command::Hack => {
            let sh = xtask::sh();
            cmd!(sh, "cargo hack check --each-feature").run()?;
            Ok(())
        }
        Command::Swift(cmd) => cmd.run(),
        Command::GenerateIos => {
            SwiftArgs {
                cmd: SwiftCommand::BuildIosLibrary { release: true },
            }
            .run()?;

            Ok(())
        }
    }
}
