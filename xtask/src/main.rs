use clap::{Parser, Subcommand};
use xshell::cmd;
use xtask::{android, mdns, receiver, sender, sh, test_corpus, workspace};
#[cfg(feature = "uniffi")]
use xtask::{
    csharp, kotlin,
    swift::{self, SwiftArgs, SwiftCommand},
};

#[derive(Subcommand)]
enum Command {
    #[cfg(feature = "uniffi")]
    Kotlin(kotlin::KotlinArgs),
    #[cfg(feature = "uniffi")]
    Swift(swift::SwiftArgs),
    #[cfg(feature = "uniffi")]
    GenerateIos,
    Hack,
    #[cfg(feature = "uniffi")]
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
        #[cfg(feature = "uniffi")]
        Command::Kotlin(cmd) => cmd.run().unwrap(),
        Command::Hack => {
            let sh = xtask::sh();
            cmd!(sh, "cargo hack check --each-feature").run().unwrap();
        }
        #[cfg(feature = "uniffi")]
        Command::Swift(cmd) => cmd.run().unwrap(),
        #[cfg(feature = "uniffi")]
        Command::GenerateIos => {
            SwiftArgs {
                cmd: SwiftCommand::BuildIosLibrary { release: true },
            }
            .run()
            .unwrap();
        }
        #[cfg(feature = "uniffi")]
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
