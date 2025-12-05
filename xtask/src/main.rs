use clap::{Parser, Subcommand};
use xshell::cmd;
use xtask::{
    android, csharp, kotlin, mdns, sender,
    swift::{self, SwiftArgs, SwiftCommand},
    test_corpus,
};

#[derive(Subcommand)]
enum Command {
    Kotlin(kotlin::KotlinArgs),
    Swift(swift::SwiftArgs),
    GenerateIos,
    Hack,
    CSharp(csharp::CSharpArgs),
    Android(android::AndroidArgs),
    Sender(sender::SenderArgs),
    TestCorpus(test_corpus::TestCorpusArgs),
    Mdns(mdns::MdnsArgs),
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
        Command::Android(cmd) => cmd.run().unwrap(),
        Command::Sender(cmd) => cmd.run().unwrap(),
        Command::TestCorpus(cmd) => cmd.run().unwrap(),
        Command::Mdns(cmd) => cmd.run().unwrap(),
    }
}
