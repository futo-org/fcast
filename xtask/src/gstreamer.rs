//! Receiver builds for the installer commands.
//!
//! GStreamer itself is built and statically linked by the gstreamer-src
//! crate (with the bundled -sys forks resolving it through the dependency
//! graph), so building the receiver is plain cargo. The one thing cargo
//! cannot see is a gst-only rebuild, covered by the relink guard below.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::Args;
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Args)]
pub struct GstreamerArgs {
    /// Cargo profile for the receiver.
    #[arg(long, default_value = "release")]
    profile: String,
    /// Build with --no-default-features (e.g. no systray on macOS).
    #[arg(long)]
    pub no_default_features: bool,
}

impl GstreamerArgs {
    /// Build the receiver, return the binary path relative to the workspace
    /// root.
    pub fn build(&self) -> Result<Utf8PathBuf> {
        let sh = sh();
        let root = workspace::root_path()?;
        let _p = sh.push_dir(root.clone());
        let bin = receiver_bin_path(&self.profile);
        force_relink_for_fresh_gst(&root.join(&bin))?;
        let profile = &self.profile;
        let ndf: &[&str] = if self.no_default_features {
            &["--no-default-features"]
        } else {
            &[]
        };
        cmd!(
            sh,
            "cargo build -p desktop-receiver --profile {profile} {ndf...}"
        )
        .run()?;
        Ok(bin)
    }
}

/// The relink guard for the plain cargo wrapper commands (build/run).
pub fn guard_receiver_relink(release: bool) -> Result<()> {
    let profile = if release { "release" } else { "dev" };
    let root = workspace::root_path()?;
    force_relink_for_fresh_gst(&root.join(receiver_bin_path(profile)))
}

/// Path of the receiver binary a build with `profile` produces.
fn receiver_bin_path(profile: &str) -> Utf8PathBuf {
    let subdir = match profile {
        "dev" => "debug",
        p => p,
    };
    let bin = if cfg!(windows) {
        "desktop-receiver.exe"
    } else {
        "desktop-receiver"
    };
    Utf8PathBuf::from("target").join(subdir).join(bin)
}

/// cargo fingerprints the link-arg strings, not the archives they name, so a
/// gst-only rebuild (a patch edit plus GSTREAMER_SRC_REBUILD) leaves the
/// receiver binary "fresh" but linked against old code. Deleting it is not
/// enough, cargo restores it from a hardlink sibling without relinking. When
/// any archive under the gstreamer-src build root is newer than the binary,
/// dirty the bin crate's root source to force the relink.
fn force_relink_for_fresh_gst(bin: &Utf8PathBuf) -> Result<()> {
    let Ok(bin_time) = std::fs::metadata(bin.as_std_path()).and_then(|m| m.modified()) else {
        return Ok(());
    };
    let root = workspace::root_path()?;
    let gst_root = std::env::var("GSTREAMER_SRC_BUILD_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| root.join("target/gst-static").into());
    if !newer_archive_under(&gst_root, bin_time) {
        return Ok(());
    }
    println!(">> gstreamer archives are newer than {bin}, forcing a relink");
    let main = root.join("receivers/desktop/src/main.rs");
    std::fs::File::options()
        .write(true)
        .open(main.as_std_path())
        .and_then(|f| f.set_modified(std::time::SystemTime::now()))
        .with_context(|| format!("touching {main}"))?;
    Ok(())
}

/// Whether any `.a` under `dir` (recursively) is newer than `than`.
fn newer_archive_under(dir: &std::path::Path, than: std::time::SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if newer_archive_under(&entry.path(), than) {
                return true;
            }
        } else if entry.path().extension().and_then(|e| e.to_str()) == Some("a")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified > than)
        {
            return true;
        }
    }
    false
}
