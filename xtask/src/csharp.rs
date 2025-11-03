use anyhow::Result;
use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum CSharpCommand {
    BuildCSharpLibrary {
        #[clap(long)]
        release: bool,
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
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            CSharpCommand::BuildCSharpLibrary { .. } => {
                cmd!(
                    sh,
                    "cargo build -p fcast-sender-sdk --features _uniffi_csharp"
                )
                .run()?;
                cmd!(sh, "uniffi-bindgen-cs target/debug/libfcast_sender_sdk.so --library --config sdk/sender/fcast-sender-sdk/uniffi.toml --out-dir bindings-cs").run()?;
                // for target in [
                //     "x86_64-unknown-linux-gnu",
                //     "aarch64-unknown-linux-gnu",
                //     "x86_64-pc-windows-msvc",
                //     "aarch64-apple-darwin",
                //     "x86_64-apple-darwin",
                // ] {
                //     cmd!(sh, "cargo build --release --target {target} -p fcast-sender-sdk --features _uniffi_csharp").run()?;
                // }
                Ok(())
            }
        }
    }
}
