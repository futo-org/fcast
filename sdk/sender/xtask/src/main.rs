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
