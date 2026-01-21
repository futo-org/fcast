use anyhow::Result;
use clap::{Args, Subcommand};
use xshell::cmd;

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum ProtocolCommand {
    ExportV4,
}

#[derive(Args)]
pub struct ProtocolArgs {
    #[clap(subcommand)]
    pub cmd: ProtocolCommand,
}

impl ProtocolArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let _p = sh.push_dir(workspace::root_path()?);

        match self.cmd {
            ProtocolCommand::ExportV4 => {
                let _p = sh.push_dir("sdk/common/fcast-protocol");

                cmd!(sh, "cargo r -p fcast-protocol --features __schema").run()?;

                Ok(())
            }
        }
    }
}
