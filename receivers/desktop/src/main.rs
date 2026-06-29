#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mimalloc::MiMalloc;
use rcore::clap::Parser;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> anyhow::Result<()> {
    let args = rcore::CliArgs::parse();

    #[cfg(target_os = "windows")]
    let _ = enable_ansi_support::enable_ansi_support();

    if !args.headless && std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
        let selector = rcore::slint::BackendSelector::new();
        #[cfg(not(target_os = "windows"))]
        let selector = selector.require_opengl_with_version(3, 30);
        #[cfg(target_os = "windows")]
        let selector = selector.require_opengl_with_version(4, 0);
        selector.select()?;
    }

    if let Err(err) = rcore::slint::set_xdg_app_id("org.fcast.Receiver") {
        rcore::tracing::warn!(?err, "Failed to set XDG app id");
    }

    rcore::run(args, rcore::SwapchainSink::new())
}
