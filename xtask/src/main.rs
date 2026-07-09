use clap::{Parser, Subcommand};
use xshell::cmd;
#[cfg(feature = "mdns")]
use xtask::mdns;
use xtask::{android, gstreamer, protocol, receiver, sender, sh, test_corpus, workspace};
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
    #[cfg(feature = "mdns")]
    Mdns(mdns::MdnsArgs),
    Receiver(receiver::ReceiverArgs),
    Test,
    Protocol(protocol::ProtocolArgs),
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
        #[cfg(feature = "mdns")]
        Command::Mdns(cmd) => cmd.run().unwrap(),
        Command::Receiver(cmd) => cmd.run().unwrap(),
        Command::Test => {
            let sh = sh();
            let root_path = workspace::root_path().unwrap();
            let _p = sh.push_dir(root_path.clone());

            cmd!(sh, "cargo test --all-targets --all-features --workspace --exclude receiver-core --exclude desktop-receiver --exclude receiver-android --exclude android-sender --exclude fiatlux-sys --exclude fiatlux --exclude fhs-receiver --exclude xtask-fuzz --exclude libplacebo-sys --exclude receiver-resources --exclude get-type-string-derive --exclude egl-sys").run().unwrap();

            gstreamer::GstreamerArgs::with_defaults()
                .test(Vec::new(), false)
                .unwrap();
        }
        Command::Protocol(cmd) => cmd.run().unwrap(),
    }
}
