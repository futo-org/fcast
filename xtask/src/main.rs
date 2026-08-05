use clap::{Parser, Subcommand};
use xshell::cmd;
#[cfg(feature = "mdns")]
use xtask::mdns;
use xtask::{android, flutter, gstreamer, protocol, receiver, sender, sh, test_corpus, workspace};
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
    Flutter(flutter::FlutterArgs),
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
        Command::Flutter(cmd) => cmd.run().unwrap(),
        Command::Sender(cmd) => cmd.run().unwrap(),
        Command::TestCorpus(cmd) => cmd.run().unwrap(),
        #[cfg(feature = "mdns")]
        Command::Mdns(cmd) => cmd.run().unwrap(),
        Command::Receiver(cmd) => cmd.run().unwrap(),
        Command::Test => {
            let sh = sh();
            let root_path = workspace::root_path().unwrap();
            let _p = sh.push_dir(root_path.clone());

            //////////////////////////////////////////////////////////////
            //                   DO NOT COMMIT                          //
            //////////////////////////////////////////////////////////////

            // The workspace suites below link against whatever GStreamer the
            // environment provides, which has NONE of xtask/patches applied.
            // That is a configuration the receiver never ships, and it is not
            // merely noisy: unpatched decodebin3 ABORTS the test binary
            // outright on `gstdecodebin3.c:3430: assertion failed:
            // (candidate->is_update || dbin->output_collection == NULL)`,
            // which the receiver fixes with
            // decodebin3-tolerate-non-update-intermediary-collection.patch.
            //
            // So build the patched `playback` plugin and put it ahead of the
            // system one. Every patch that affects the playbin lives there.
            // A failure here is not fatal, since the plugin needs nix, meson
            // and ninja, but it is LOUD: a green run against unpatched
            // GStreamer means much less than it appears to.
            let _patched = match cmd!(sh, "tools/build-patched-playback.sh --env-only")
                .quiet()
                .read()
            {
                Ok(env) => {
                    let guards: Vec<_> = env
                        .lines()
                        .filter_map(|line| line.strip_prefix("export "))
                        .filter_map(|line| line.split_once('='))
                        .map(|(key, value)| sh.push_env(key, value))
                        .collect();
                    println!(">> testing against the patched playback plugin");
                    Some(guards)
                }
                Err(err) => {
                    println!(
                        ">> WARNING: could not build the patched playback plugin ({err}). \
                         The suites below will run against unpatched GStreamer, where some \
                         failures and aborts belong to upstream rather than to this tree. \
                         See tools/build-patched-playback.sh."
                    );
                    None
                }
            };

            cmd!(sh, "cargo test --all-targets --all-features --workspace --exclude receiver-core --exclude desktop-receiver --exclude receiver-android --exclude android-sender --exclude fiatlux-sys --exclude fiatlux --exclude fhs-receiver --exclude xtask-fuzz --exclude libplacebo-sys --exclude libplacebo-vulkan --exclude libplacebo --exclude receiver-resources --exclude egl-sys --exclude fcast-sender-sdk-flutter").run().unwrap();

            gstreamer::GstreamerArgs::with_defaults()
                .test(Vec::new(), false)
                .unwrap();
        }
        Command::Protocol(cmd) => cmd.run().unwrap(),
    }
}
