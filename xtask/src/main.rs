use clap::{Parser, Subcommand};
use xshell::cmd;
use xtask::{
    android, csharp, kotlin, mdns, receiver, sender, sh,
    swift::{self, SwiftArgs, SwiftCommand},
    test_corpus, workspace,
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
    Receiver(receiver::ReceiverArgs),
    Test,
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
        Command::Receiver(cmd) => cmd.run().unwrap(),
        Command::Test => {
            let sh = sh();
            let root_path = workspace::root_path().unwrap();
            let _p = sh.push_dir(root_path.clone());

            cmd!(sh, "cargo test --all-features --workspace --exclude receiver-android --exclude android-sender").run().unwrap();
        }
    }
}
