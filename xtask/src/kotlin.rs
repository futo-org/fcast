use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use uniffi_bindgen::bindings::{generate, GenerateOptions, TargetLanguage};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum KotlinCommand {
    /// Build the native libraries and generate the UniFFI Kotlin bindings into
    /// `<src_dir>` (`jniLibs/` + `kotlin/`). This is the raw input consumed by
    /// the `sdk/sender/android` Gradle project; it does not produce an `.aar`.
    BuildAndroidLibrary {
        #[clap(long)]
        release: bool,
        #[clap(long)]
        src_dir: Utf8PathBuf,
    },
    /// Drive the `sdk/sender/android` Gradle project: generate the native libs +
    /// bindings into `<module_dir>/src`, run its Gradle wrapper, then optionally
    /// copy the resulting `.aar` to `--out` (e.g. grayjay's `app/aar/`).
    ///
    /// Pass `--gradle-task publishToMavenCentral` to build and publish the SDK to
    /// Maven Central instead of just assembling a drop-in aar.
    BuildAar {
        #[clap(long)]
        release: bool,
        /// Path to the Gradle module (defaults to the in-repo `sdk/sender/android`).
        #[clap(long, default_value = "sdk/sender/android")]
        module_dir: Utf8PathBuf,
        /// Optional destination for the produced `.aar`: a directory (the aar
        /// keeps its Gradle-assigned name) or an explicit `*.aar` file path.
        /// Omit when the Gradle task publishes rather than assembles.
        #[clap(long)]
        out: Option<Utf8PathBuf>,
        /// Gradle task to run.
        #[clap(long, default_value = "assembleRelease")]
        gradle_task: String,
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

    built_lib_path(target, profile, package_name)
}

/// Path to the `.so` a given profile/target produces under the workspace target dir.
fn built_lib_path(target: &str, profile: &str, package_name: &str) -> Result<Utf8PathBuf> {
    let profile_dir_name = if profile == "dev" { "debug" } else { profile };
    let package_camel = package_name.replace('-', "_");
    Ok(workspace::target_path()?
        .join(target)
        .join(profile_dir_name)
        .join(format!("lib{package_camel}.so")))
}

fn generate_uniffi_bindings(library_path: &Utf8Path, ffi_generated_dir: &Utf8Path) -> Result<()> {
    let config_path = workspace::root_path()?.join("sdk/sender/fcast-sender-sdk/uniffi.toml");

    generate(GenerateOptions {
        languages: vec![TargetLanguage::Kotlin],
        source: library_path.to_path_buf(),
        out_dir: ffi_generated_dir.to_path_buf(),
        config_override: Some(config_path),
        format: false,
        ..Default::default()
    })?;

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

    let profile = if release { "release-small" } else { "dev" };

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
    let release_aarch64 = build_for_android_target(
        "aarch64-linux-android",
        profile,
        jni_libs_dir_str,
        package_name,
    )?;

    // uniffi_bindgen reconstructs the interface from the `UNIFFI_META_*` symbols
    // in the library. The `release-small` profile has `strip = "symbols"`, which
    // removes them (the FFI functions survive, so the app loads but fails the
    // API-checksum check for any changed method). Generate from an unstripped
    // build instead: for release, a separate (unstripped, un-optimized) aarch64
    // lib; for dev the (already unstripped) aarch64 lib we just built.
    let uniffi_lib_path = if release {
        build_unstripped_meta_lib(package_name)?
    } else {
        release_aarch64
    };

    generate_uniffi_bindings(&uniffi_lib_path, &kotlin_generated_dir)?;

    Ok(())
}

/// Build an unstripped aarch64 library purely so `uniffi_bindgen` can read its
/// metadata. Uses the `dev` profile: the `UNIFFI_META_*` symbols are emitted the
/// same regardless of optimization, so this needs neither LTO nor `opt-level`
/// (unlike a `release`-derived profile, whose fat LTO dominated build time).
/// Not copied into `jniLibs`.
fn build_unstripped_meta_lib(package_name: &str) -> Result<Utf8PathBuf> {
    let sh = sh();
    let _p = sh.push_dir(workspace::root_path()?);
    // A throwaway `-o` dir keeps the debug `.so` out of the aar's jniLibs.
    let scratch = workspace::target_path()?.join("uniffi-meta");
    cmd!(
        sh,
        "cargo ndk --target aarch64-linux-android -o {scratch} build --profile dev -p {package_name} --no-default-features --features _android_defaults"
    )
    .run()?;

    built_lib_path("aarch64-linux-android", "dev", package_name)
}

/// Drive the `sdk/sender/android` Gradle project to assemble (or publish) the aar.
fn build_aar(
    release: bool,
    module_dir: Utf8PathBuf,
    out: Option<Utf8PathBuf>,
    gradle_task: String,
) -> Result<()> {
    let sh = sh();

    // Resolve workspace-relative paths up front, while the cwd is still the
    // workspace root — the Gradle step below pushes into the module dir, where
    // `cargo metadata` (used by `workspace::*`) would fail.
    let fallback_android_home = workspace::root_path()?.join(crate::android::ANDROID_HOME_PATH);

    // 1. Generate the native libs + UniFFI bindings into the layout the Gradle
    //    project consumes (`<module_dir>/src`). This overwrites the generated
    //    `.so` + `fcast_sender_sdk.kt` and leaves hand-written sources (e.g.
    //    `Discovery.kt`) untouched.
    build_android_library(release, module_dir.join("src"))?;

    // 2. Run the requested Gradle task with the project's own wrapper.
    let gradlew = module_dir.join("gradlew");
    if !gradlew.exists() {
        anyhow::bail!(
            "no gradlew found at {gradlew}; --module-dir should point at the sdk/sender/android Gradle project"
        );
    }
    {
        let _dir = sh.push_dir(&module_dir);
        // Point the Android Gradle plugin at the SDK xtask can provision, unless
        // the environment already sets ANDROID_HOME.
        let _env = (std::env::var_os("ANDROID_HOME").is_none() && fallback_android_home.exists())
            .then(|| sh.push_env("ANDROID_HOME", fallback_android_home.as_str()));
        cmd!(sh, "./gradlew {gradle_task}").run()?;
    }

    // 3. Optionally locate the produced aar and copy it to `out`. Skipped when
    //    the task publishes rather than assembles a drop-in aar.
    if let Some(out) = out {
        let aar = find_built_aar(&module_dir)?;
        let dest = if out.extension() == Some("aar") {
            if let Some(parent) = out.parent() {
                sh.create_dir(parent)?;
            }
            out
        } else {
            sh.create_dir(&out)?;
            out.join(aar.file_name().expect("aar has a file name"))
        };
        sh.copy_file(&aar, &dest)?;
        println!("Wrote {dest}");
    }

    Ok(())
}

/// Find the most recently built `.aar` under `<root>/**/build/outputs/aar/`,
/// preferring a `release` variant. Searching the output dir avoids hard-coding
/// the Gradle module name.
fn find_built_aar(root: &Utf8Path) -> Result<Utf8PathBuf> {
    fn collect(dir: &Utf8Path, out: &mut Vec<Utf8PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension() == Some("aar")
                && path
                    .as_str()
                    .replace('\\', "/")
                    .contains("build/outputs/aar/")
            {
                out.push(path);
            }
        }
    }

    let mut candidates = Vec::new();
    collect(root, &mut candidates);

    if candidates.is_empty() {
        anyhow::bail!("no .aar found under {root}/**/build/outputs/aar/ after the Gradle build");
    }

    // Newest last, then prefer a release-variant filename.
    candidates.sort_by_key(|p| p.as_std_path().metadata().and_then(|m| m.modified()).ok());
    let pick = candidates
        .iter()
        .rev()
        .find(|p| p.file_name().is_some_and(|n| n.contains("release")))
        .or_else(|| candidates.last())
        .cloned()
        .expect("candidates is non-empty");
    Ok(pick)
}

impl KotlinArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            KotlinCommand::BuildAndroidLibrary { release, src_dir } => {
                build_android_library(release, src_dir)
            }
            KotlinCommand::BuildAar {
                release,
                module_dir,
                out,
                gradle_task,
            } => build_aar(release, module_dir, out, gradle_task),
        }
    }
}
