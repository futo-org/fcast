use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use xshell::{cmd, Shell};

use crate::{sh, workspace};

/// Fork of NordSecurity/uniffi-bindgen-cs v0.11.0+v0.31.0 with the callback
/// registration name fix. A relative path resolves against the workspace root.
const BINDGEN_CS_REPO: &str = "https://gitlab.futo.org/fcast/uniffi-bindgen-cs.git";
const BINDGEN_CS_REV: &str = "a6fcd19e36433862f0d8697cc56f4a6400f95897";

#[derive(Subcommand)]
pub enum CSharpCommand {
    BuildCSharpLibrary {
        #[clap(long)]
        release: bool,
        /// Where the generated `fcast_sender_sdk.cs` is written
        #[clap(long, default_value = "bindings-cs")]
        out_dir: Utf8PathBuf,
    },
}

#[derive(Args)]
pub struct CSharpArgs {
    #[clap(subcommand)]
    pub cmd: CSharpCommand,
}

impl CSharpArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let root = workspace::root_path()?;
        let _p = sh.push_dir(&root);

        match self.cmd {
            CSharpCommand::BuildCSharpLibrary { release, out_dir } => {
                let out_dir = root.join(out_dir);
                let bindgen = bindgen_binary(&sh)?;
                // release strips symbols, which hides the uniffi metadata from bindgen
                let profile = if release { "release-dbg" } else { "dev" };
                cmd!(
                    sh,
                    "cargo build -p fcast-sender-sdk --no-default-features --features _uniffi_csharp --profile {profile}"
                )
                .run()?;
                let lib = workspace::target_path()?
                    .join(if release { "release-dbg" } else { "debug" })
                    .join(host_lib_name());
                cmd!(sh, "{bindgen} {lib} --library --config sdk/sender/fcast-sender-sdk/uniffi.toml --out-dir {out_dir}").run()?;
                // bindgen exits 0 without writing anything when it finds no metadata
                let out = out_dir.join("fcast_sender_sdk.cs");
                if std::fs::metadata(&out).map_or(true, |m| m.len() == 0) {
                    bail!("{out} is missing or empty, bindgen found no uniffi metadata in {lib}");
                }
                Ok(())
            }
        }
    }
}

fn host_lib_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "fcast_sender_sdk.dll"
    } else if cfg!(target_os = "macos") {
        "libfcast_sender_sdk.dylib"
    } else {
        "libfcast_sender_sdk.so"
    }
}

/// Rev-pinned bindgen installed under `target/` on first use. Not taken from
/// `PATH`: a bindgen built for another uniffi version generates bindings that
/// fail the contract check at runtime.
fn bindgen_binary(sh: &Shell) -> Result<Utf8PathBuf> {
    let root = workspace::target_path()?
        .join("uniffi-bindgen-cs")
        .join(BINDGEN_CS_REV);
    let bin = root.join(if cfg!(windows) {
        "bin/uniffi-bindgen-cs.exe"
    } else {
        "bin/uniffi-bindgen-cs"
    });
    if !bin.exists() {
        let repo = if BINDGEN_CS_REPO.contains("://") {
            BINDGEN_CS_REPO.to_owned()
        } else {
            let path = workspace::root_path()?.join(BINDGEN_CS_REPO);
            format!("file://{}", path.canonicalize_utf8()?)
        };
        cmd!(
            sh,
            "cargo install uniffi-bindgen-cs --git {repo} --rev {BINDGEN_CS_REV} --locked --root {root}"
        )
        .run()?;
    }
    Ok(bin)
}
