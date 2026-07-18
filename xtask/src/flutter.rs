use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum FlutterCommand {
    /// Regenerate the flutter_rust_bridge + freezed bindings for the Flutter
    /// plugin at `sdk/sender/flutter-plugin`.
    ///
    /// The generated files (`lib/src/rust/*.dart`, `rust/src/frb_generated.rs`)
    /// are git-ignored; run this after a fresh checkout, after editing
    /// `rust/src/api.rs`, and in CI before `flutter pub publish`.
    Generate {
        /// Comma-separated cargo features to generate bindings for. Must match
        /// the features the app builds with (see the plugin's `hook/build.dart`
        /// and README). Defaults to everything.
        #[clap(long, default_value = "fcast,chromecast,logging")]
        features: String,
    },
}

#[derive(Args)]
pub struct FlutterArgs {
    #[clap(subcommand)]
    pub cmd: FlutterCommand,
}

impl FlutterArgs {
    pub fn run(self) -> Result<()> {
        match self.cmd {
            FlutterCommand::Generate { features } => generate(&features),
        }
    }
}

fn plugin_dir() -> Result<Utf8PathBuf> {
    Ok(workspace::root_path()?.join("sdk/sender/flutter-plugin"))
}

fn generate(features: &str) -> Result<()> {
    let sh = sh();
    let _p = sh.push_dir(plugin_dir()?);

    // Pin flutter_rust_bridge_codegen to the exact flutter_rust_bridge version
    // declared in pubspec.yaml so the generated bindings can never drift from
    // the runtime (a mismatch fails at app startup).
    let pubspec = sh.read_file("pubspec.yaml")?;
    let frb_version = pubspec
        .lines()
        .find_map(|line| line.trim().strip_prefix("flutter_rust_bridge:"))
        .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
        .context("could not read the flutter_rust_bridge version from pubspec.yaml")?;
    println!("Generating bindings for features [{features}] with codegen {frb_version}");

    let installed = cmd!(sh, "flutter_rust_bridge_codegen --version")
        .read()
        .unwrap_or_default();
    if !installed.contains(&frb_version) {
        cmd!(
            sh,
            "cargo install flutter_rust_bridge_codegen --version {frb_version} --locked"
        )
        .run()?;
    }

    cmd!(sh, "flutter pub get").run()?;
    cmd!(
        sh,
        "flutter_rust_bridge_codegen generate --rust-features {features}"
    )
    .run()?;
    cmd!(
        sh,
        "flutter pub run build_runner build --delete-conflicting-outputs"
    )
    .run()?;

    println!("Bindings regenerated.");
    Ok(())
}
