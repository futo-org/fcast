use clap::{Parser, Subcommand};
use xshell::cmd;
#[cfg(feature = "mdns")]
use xtask::mdns;
use xtask::{
    android, flutter, gstreamer, patched_plugins, protocol, receiver, sender, sh, test_corpus,
    workspace,
};
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
    /// The default lane: the receiver side, against the patched GStreamer.
    /// The sender crates live in `test-sender`.
    Test,
    /// The quick lane: every suite that needs neither the patched GStreamer nor
    /// slint. Pure element tests (they build a factory and drive it) so they
    /// skip the patched-plugin build and the whole UI dependency tree.
    TestQuick,
    /// The sender lane: the senders, the sender SDK and the crates only they
    /// use. Split out of `test` so receiver work never pays for slint + the
    /// uniffi bindings generator.
    TestSender,
    Protocol(protocol::ProtocolArgs),
    /// Build the patched `playback` and `adaptivedemux2` plugins the test lanes
    /// run against, and print the env that selects them. The lanes below do
    /// this themselves; run it by hand for anything else that needs the
    /// same GStreamer: `eval "$(cargo xtask patched-plugins --quiet)"`.
    PatchedPlugins(patched_plugins::PatchedPluginsArgs),
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

            // The receiver suites otherwise link against whatever GStreamer the
            // environment provides, which has NONE of xtask/patches applied:
            // unpatched decodebin3 ABORTS the test binary.
            let _patched = patched_plugins::push_env(&sh);

            // Named packages, not `--workspace --exclude …`: the exclusion list had
            // grown to fifteen entries and still swept in the whole sender tree.
            // This is the receiver side plus the crates it shares (fcast-protocol,
            // sabrump, google-cast-protocol); `test-sender` has the rest.
            //
            // No `--all-features` either. It was pulling `fcast-video/render` +
            // `wayland-subsurface` (libplacebo, which builds the C library
            // from source, plus Vulkan and slint) into a lane whose
            // fcast-video suites are
            // pure CPU rasterization. The three features that actually gate tests
            // are named instead: fcast-protocol's tokio-{sender,receiver} (all of
            // tests/network_stream.rs is `#![cfg]`'d on both) and
            // fcast-gst-elements' textoverlay (the fcasttextoverlay unit tests).
            // sabrump/serde gates no test but is how every dependent builds it.
            cmd!(
                sh,
                "cargo test --all-targets
                 -p fcast-video -p fcastplaybin -p fcast-gst-elements -p fcasttest
                 -p fcast-runtime -p fcast-protocol -p sabrump -p google-cast-protocol
                 -p apple-fairplay -p app-updater -p inhibit-screensaver -p xtask
                 --features fcast-protocol/tokio-receiver,fcast-protocol/tokio-sender,fcast-gst-elements/textoverlay,sabrump/serde"
            )
            .run()
            .unwrap();

            gstreamer::GstreamerArgs::with_defaults()
                .test(Vec::new(), false)
                .unwrap();
        }
        Command::TestSender => {
            let sh = sh();
            let _p = sh.push_dir(workspace::root_path().unwrap());

            // Same patched-plugin guard as `test`: desktop-sender's mirroring
            // drives real pipelines, and this lane used to run inside `test`.
            let _patched = patched_plugins::push_env(&sh);

            // `--all-features` as before, so the uniffi/flutter arms of the SDK
            // still compile here. android-sender (needs the NDK) and
            // fcast-sender-sdk-flutter (checked-in frb_generated.rs, regenerated
            // by `xtask flutter`) stay out, exactly as they were excluded from
            // the old sweep.
            println!(">> sender lane: senders, sender SDK, and the crates only they use");
            cmd!(
                sh,
                "cargo test --all-targets --all-features
                 -p fcast-sender-sdk -p fcast -p desktop-sender -p mcore
                 -p fast -p file-server -p image-viewer"
            )
            .run()
            .unwrap();
        }
        Command::TestQuick => {
            let sh = sh();
            let _p = sh.push_dir(workspace::root_path().unwrap());

            // No patched-GStreamer guard here on purpose: these suites never
            // autoplug decodebin3/playback, so the unpatched plugin they link
            // against cannot abort them. Anything that DOES drive a pipeline
            // belongs in `xtask test`, which builds the patched plugin first.
            println!(">> quick lane: element tests (no patched GStreamer, no slint)");
            cmd!(sh, "cargo test -p fcast-gst-elements --all-targets")
                .run()
                .unwrap();
        }
        Command::Protocol(cmd) => cmd.run().unwrap(),
        Command::PatchedPlugins(cmd) => cmd.run().unwrap(),
    }
}
