use clap::{Parser, Subcommand};
use xshell::cmd;
use xtask::{
    csharp, kotlin,
    swift::{self, SwiftArgs, SwiftCommand},
};

#[derive(Subcommand)]
enum Command {
    Kotlin(kotlin::KotlinArgs),
    Swift(swift::SwiftArgs),
    GenerateIos,
    Hack,
    CSharp(csharp::CSharpArgs),
}

#[derive(Parser)]
struct Xtask {
    #[clap(subcommand)]
    cmd: Command,
}

fn main() {
    match Xtask::parse().cmd {
        Command::Kotlin(cmd) => cmd.run().unwrap(),
        Command::Hack => {
            let sh = xtask::sh();
            cmd!(sh, "cargo hack check --each-feature").run().unwrap();
        }
        Command::Swift(cmd) => cmd.run().unwrap(),
        Command::GenerateIos => {
            SwiftArgs {
                cmd: SwiftCommand::BuildIosLibrary { release: true },
            }
            .run()
            .unwrap();
        }
        Command::CSharp(cmd) => cmd.run().unwrap(),
    }
}
