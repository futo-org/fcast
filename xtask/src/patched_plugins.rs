//! Build the `playback` and `adaptivedemux2` plugins with xtask/patches
//! applied, from the source nixpkgs pins (so they stay ABI-compatible and
//! GStreamer prefers them over the system copy).
//!
//! `cargo test` links the devshell's GStreamer, which has none of the patches
//! the shipping static build gets: unpatched decodebin3 aborts test binaries
//! (three fuzz seeds did until
//! decodebin3-tolerate-non-update-intermediary-collection).
//!
//! Progress goes to stderr and the env to stdout, so
//! `eval "$(cargo xtask patched-plugins --quiet)"` works from a shell.

use std::rc::Rc;

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args;
use xshell::{cmd, PushEnv, Shell};

use crate::{sh, workspace};

#[derive(Args)]
pub struct PatchedPluginsArgs {
    /// Silence the progress logs. The `export …` lines still go to stdout.
    #[clap(long)]
    quiet: bool,
}

impl PatchedPluginsArgs {
    pub fn run(self) -> Result<()> {
        for (key, value) in build(self.quiet)? {
            println!("export {key}={value}");
        }
        Ok(())
    }
}

struct Plugin {
    /// nixpkgs `gst_all_1` attribute holding the source tarball.
    attr: &'static str,
    /// Workdir under the repo root. Not $TMPDIR: inside `nix develop` that is
    /// per-shell, so it rebuilt every time and the next shell saw nothing.
    work: &'static str,
    /// The built plugin, relative to the unpacked source root.
    so: &'static str,
    meson: &'static [&'static str],
    /// Patch stems in xtask/patches that touch this plugin. The rest belong to
    /// gstreamer core, matroska, multiqueue and friends.
    patches: &'static [&'static str],
    /// Element used to prove the plugin loaded.
    element: &'static str,
}

const PLAYBACK: Plugin = Plugin {
    attr: "gst-plugins-base",
    work: "target/patched-playback",
    so: "build/gst/playback/libgstplayback.so",
    meson: &[
        "-Dauto_features=disabled",
        "-Dintrospection=disabled",
        "-Dexamples=disabled",
        "-Dtests=disabled",
        "-Dtools=disabled",
        "-Ddoc=disabled",
        "-Dplayback=enabled",
    ],
    patches: &[
        "decodebin3-adopt-collection-that-only-adds-streams",
        "decodebin3-auto-select-text-property",
        "decodebin3-post-streams-selected-for-noop-selection",
        "decodebin3-refcount-input-fix-release-uaf",
        "decodebin3-tolerate-non-update-intermediary-collection",
        "decodebin3-outputless-slot-keeps-its-output",
        "decodebin3-serialize-collection-message-posts",
    ],
    element: "decodebin3",
};

// Only a real adaptive demuxer can exercise the D4 output-loop fix. libsoup is
// dlopened at runtime, not a build dep; libxml2 is one (dashdemux2's MPD
// parser) and meson insists on it.
const ADAPTIVEDEMUX2: Plugin = Plugin {
    attr: "gst-plugins-good",
    work: "target/patched-adaptivedemux2",
    so: "build/ext/adaptivedemux2/libgstadaptivedemux2.so",
    meson: &[
        "-Dauto_features=disabled",
        "-Dexamples=disabled",
        "-Dtests=disabled",
        "-Ddoc=disabled",
        "-Dnls=disabled",
        "-Dadaptivedemux2=enabled",
    ],
    patches: &[
        "adaptivedemux2-transient-flushing-no-permanent-pause",
        "adaptivedemux2-track-flush-keeps-its-caps",
    ],
    element: "dashdemux2",
};

/// Build both plugins and return the env that puts them ahead of the system
/// ones. adaptivedemux2 is best-effort: without it only the suites gated on
/// `FCAST_PATCHED_ADAPTIVEDEMUX2` skip themselves.
pub fn build(quiet: bool) -> Result<Vec<(String, String)>> {
    let sh = sh();
    let root = workspace::root_path()?;
    let _p = sh.push_dir(&root);

    let playback = build_plugin(&sh, &root, &PLAYBACK, quiet)?;
    let adaptive = match build_plugin(&sh, &root, &ADAPTIVEDEMUX2, quiet) {
        Ok(so) => Some(so),
        Err(err) => {
            // Not log(): --quiet must not silence this one.
            eprintln!(
                "WARNING: the patched adaptivedemux2 plugin did not build ({err:#}). The\n\
                 playback env is still valid, and the suites gated on\n\
                 FCAST_PATCHED_ADAPTIVEDEMUX2 will skip themselves."
            );
            None
        }
    };

    let mut dirs = parent(&playback)?.to_string();
    if let Some(so) = &adaptive {
        dirs = format!("{dirs}:{}", parent(so)?);
    }

    let work = root.join(PLAYBACK.work);
    verify_loads(&sh, &work, &dirs, PLAYBACK.element, &playback)?;
    if let Some(so) = &adaptive {
        verify_loads(&sh, &work, &dirs, ADAPTIVEDEMUX2.element, so)?;
    }

    log(quiet, &format!("built {playback} (verified loadable)"));
    let mut env = vec![
        ("GST_PLUGIN_PATH".to_owned(), dirs),
        (
            "GST_REGISTRY".to_owned(),
            work.join("registry.bin").to_string(),
        ),
    ];
    if let Some(so) = adaptive {
        log(quiet, &format!("built {so} (verified loadable)"));
        env.push(("FCAST_PATCHED_ADAPTIVEDEMUX2".to_owned(), so.to_string()));
    }
    Ok(env)
}

/// Env guards for the test lanes, held for the duration of the run. Building
/// needs nix, meson and ninja, so a failure is LOUD rather than fatal: a green
/// run against unpatched GStreamer means much less than it appears to.
#[must_use]
pub fn push_env(sh: &Rc<Shell>) -> Option<Vec<PushEnv<'_>>> {
    match build(false) {
        Ok(env) => {
            let guards = env
                .into_iter()
                .map(|(key, value)| sh.push_env(key, value))
                .collect();
            println!(">> testing against the patched playback plugin");
            Some(guards)
        }
        Err(err) => {
            println!(
                ">> WARNING: could not build the patched plugins ({err:#}). The suites \
                 below will run against unpatched GStreamer, where some failures and \
                 aborts belong to upstream rather than to this tree. See \
                 xtask/src/patched_plugins.rs."
            );
            None
        }
    }
}

/// Build `plugin` if the .so is missing or the patch set changed.
fn build_plugin(
    sh: &Rc<Shell>,
    root: &Utf8Path,
    plugin: &Plugin,
    quiet: bool,
) -> Result<Utf8PathBuf> {
    let work = root.join(plugin.work);
    let src = work.join("src");
    let so = src.join(plugin.so);
    let stamp = work.join("patches.stamp");
    let want = patch_stamp(root, plugin.patches)?;

    if so.exists() && std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return Ok(so);
    }
    if so.exists() {
        log(
            quiet,
            &format!("the {} patch set changed, rebuilding", plugin.attr),
        );
    }

    log(
        quiet,
        &format!("fetching the {} source nixpkgs pins", plugin.attr),
    );
    unpack_and_patch(sh, root, plugin, &src, quiet)?;

    log(quiet, &format!("building {}", plugin.attr));
    {
        let _d = sh.push_dir(&src);
        cmd!(sh, "meson setup build")
            .args(plugin.meson)
            .quiet()
            .ignore_stdout()
            .run()
            .context("meson setup")?;
        cmd!(sh, "ninja -C build")
            .quiet()
            .ignore_stdout()
            .run()
            .context("ninja")?;
    }
    if !so.exists() {
        bail!("{} built but {so} is missing", plugin.attr);
    }
    // After the build, so an interrupted one rebuilds instead of passing for
    // the current patch set.
    std::fs::write(&stamp, &want).with_context(|| format!("writing {stamp}"))?;
    Ok(so)
}

/// Unpack the source tarball into `dest` and apply the plugin's patches.
fn unpack_and_patch(
    sh: &Rc<Shell>,
    root: &Utf8Path,
    plugin: &Plugin,
    dest: &Utf8Path,
    quiet: bool,
) -> Result<()> {
    let attr = plugin.attr;
    let expr = format!(
        "let f = builtins.getFlake \"github:NixOS/nixpkgs/nixos-unstable\"; \
             p = import f.outPath {{ system = builtins.currentSystem; }}; \
         in p.gst_all_1.{attr}.src"
    );
    let tarball = cmd!(
        sh,
        "nix build --no-link --print-out-paths --impure --expr {expr}"
    )
    .quiet()
    .read()
    .with_context(|| format!("fetching the {attr} source"))?;
    let tarball = tarball.trim();

    sh.remove_path(dest)?;
    sh.create_dir(dest)?;
    cmd!(sh, "tar -xf {tarball} -C {dest} --strip-components=1")
        .quiet()
        .run()?;

    let _d = sh.push_dir(dest);
    for name in plugin.patches {
        // -p3 strips a/subprojects/<module>/ down to the tarball's own layout.
        let patch = std::fs::read(patch_path(root, name))?;
        if cmd!(sh, "patch -p3 --dry-run --silent")
            .stdin(&patch)
            .quiet()
            .ignore_stdout()
            .ignore_stderr()
            .run()
            .is_err()
        {
            log(
                quiet,
                &format!("WARNING: does not apply to this source, skipping: {name}"),
            );
            continue;
        }
        cmd!(sh, "patch -p3 --silent").stdin(&patch).quiet().run()?;
        log(quiet, &format!("applied {name}"));
    }
    Ok(())
}

/// Cache key for the patch set. Keyed on the .so alone, adding a patch silently
/// did nothing and handed back a plugin built without it. FNV-1a so the value
/// stays stable across toolchains.
fn patch_stamp(root: &Utf8Path, patches: &[&str]) -> Result<String> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in patches {
        let path = patch_path(root, name);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok(format!("{hash:016x}"))
}

fn patch_path(root: &Utf8Path, name: &str) -> Utf8PathBuf {
    root.join("xtask/patches").join(format!("{name}.patch"))
}

fn parent(so: &Utf8Path) -> Result<&Utf8Path> {
    so.parent().with_context(|| format!("{so} has no parent"))
}

/// A plugin that fails to load is not an error to GStreamer: it warns on stderr
/// and falls back to the system copy, so every suite runs UNPATCHED while
/// looking healthy (seen for real, a 1.29 build against a 1.28 core). Compared
/// against the FULL path, so one dir cannot quietly displace another's.
fn verify_loads(
    sh: &Rc<Shell>,
    work: &Utf8Path,
    dirs: &str,
    element: &str,
    want: &Utf8Path,
) -> Result<()> {
    let registry = work.join("verify-registry.bin");
    let out = cmd!(sh, "gst-inspect-1.0 --gst-plugin-path={dirs} {element}")
        .env("GST_REGISTRY", &registry)
        .quiet()
        .ignore_stderr()
        .read()
        .unwrap_or_default();
    let _ = std::fs::remove_file(&registry);

    let resolved = out
        .lines()
        .find_map(|line| line.strip_prefix("  Filename"))
        .map(str::trim)
        .unwrap_or("<nothing>");
    if resolved != want.as_str() {
        bail!(
            "the patched plugin did not load; {element} resolved to\n  \
             {resolved}\ninstead of\n  {want}\nRe-run `gst-inspect-1.0 \
             --gst-plugin-path={dirs} {element}` to see why. Until this passes, every \
             suite runs against UNPATCHED GStreamer."
        );
    }
    Ok(())
}

fn log(quiet: bool, message: &str) {
    if !quiet {
        eprintln!(">> {message}");
    }
}
