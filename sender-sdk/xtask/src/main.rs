use anyhow::Result;
use clap::{Parser, Subcommand};
use xshell::cmd;
use xtask::kotlin::{self, KotlinArgs, KotlinCommand};

#[derive(Subcommand)]
enum Command {
    Kotlin(kotlin::KotlinArgs),
    GenerateAndroid,
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
        Command::GenerateAndroid => {
            KotlinArgs {
                cmd: KotlinCommand::BuildAndroidLibrary {
                    release: true,
                    src_dir: xtask::workspace::root_path()?
                        .join("sender-sdk/android/src")
                        .into(),
                },
            }
            .run()?;
            let sh = xtask::sh();
            let _p = sh.push_dir(xtask::workspace::root_path()?.join("sender-sdk/android"));
            eprintln!("{:?}", sh.current_dir());
            cmd!(sh, "./gradlew assembleRelease").run()?;
            sh.copy_file(
                "build/outputs/aar/fcast-android-sender-sdk-release.aar",
                "../examples/android/app/aar",
            )?;
            Ok(())
        }
    }
}
