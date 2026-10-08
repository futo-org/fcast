//! fcast and flapjack each pin the GStreamer fork in their own `.cargo/config.toml`
//! `[env]`, and cargo reads only the invoking workspace's. Nothing tied the two, and
//! the receiver ran weeks of flapjack commits against an older fork than flapjack's
//! own tests had (found 2026-10-08). The receiver builds and the test lane refuse a
//! mismatch.

use std::env;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use xshell::cmd;

use crate::{sh, workspace};

const PIN: &str = "GSTREAMER_SRC_REF";

pub fn check_matches_flapjack() -> Result<()> {
    let root = workspace::root_path()?;
    let ours_at = root.join(".cargo/config.toml");
    let ours = pin_in(&ours_at)?;
    // `cargo run` exports the config's `[env]` to us, so only a value that differs from
    // the config is a real override, and that is the caller's business, the same
    // variable overrides the config for gstreamer-src too.
    if let Ok(forced) = env::var(PIN) {
        if forced != ours {
            println!(">> {PIN}={forced} from the environment, not comparing the pins");
            return Ok(());
        }
    }
    let flapjack = flapjack_root()?;
    let theirs_at = flapjack.join(".cargo/config.toml");
    let theirs = pin_in(&theirs_at)?;
    if ours != theirs {
        bail!(
            "the GStreamer fork pin differs:\n  {ours_at}: {ours}\n  {theirs_at}: {theirs}\n\
             flapjack's tests ran against its pin, the receiver would run against ours. \
             Set {PIN} in {ours_at} to flapjack's, or bump flapjack first."
        );
    }
    Ok(())
}

fn pin_in(config: &Utf8Path) -> Result<String> {
    let text = std::fs::read_to_string(config.as_std_path())
        .with_context(|| format!("reading {config}"))?;
    let doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {config}"))?;
    doc.get("env")
        .and_then(|env| env.get(PIN))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .with_context(|| format!("{config} has no [env] {PIN}"))
}

/// The flapjack checkout the receiver builds against, the git checkout cargo holds or
/// the path a `[patch]` points at. Its workspace root carries the `.cargo` directory.
fn flapjack_root() -> Result<Utf8PathBuf> {
    #[derive(serde::Deserialize)]
    struct Metadata {
        packages: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
        manifest_path: Utf8PathBuf,
    }
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let sh = sh();
    let json = cmd!(sh, "{cargo} metadata --format-version 1").read()?;
    let metadata: Metadata = serde_json::from_str(&json)?;
    let manifest = metadata
        .packages
        .into_iter()
        .find(|package| package.name == "flapjack")
        .map(|package| package.manifest_path)
        .context("flapjack is not in the dependency graph")?;
    // <root>/flapjack/Cargo.toml
    manifest
        .parent()
        .and_then(Utf8Path::parent)
        .map(Utf8Path::to_path_buf)
        .with_context(|| format!("no workspace root above {manifest}"))
}
