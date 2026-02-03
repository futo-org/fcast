use clap::Parser;
#[cfg(not(any(
    target_os = "windows",
    all(target_arch = "aarch64", target_os = "linux")
)))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(any(
    target_os = "windows",
    all(target_arch = "aarch64", target_os = "linux")
)))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(clap::Parser)]
#[command(version)]
struct CliArgs {
    /// Start minimized to tray
    #[arg(long, default_value_t = false)]
    no_main_window: bool,
    /// Start application in fullscreen
    #[arg(long, default_value_t = false)]
    fullscreen: bool,
    /// Defines the verbosity level of the logger
    #[arg(long, alias = "log", visible_alias = "log")]
    loglevel: Option<rcore::tracing::level_filters::LevelFilter>,
    /// Start player in windowed mode
    #[arg(long, default_value_t = false)]
    no_fullscreen_player: bool,
    /// Play videos in the main application window
    #[arg(long, default_value_t = false)]
    no_player_window: bool,
}

fn main() -> anyhow::Result<()> {
    let _args = CliArgs::parse();

    if std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
        rcore::slint::BackendSelector::new()
            .require_opengl()
            .select()?;
    }

    rcore::run()
}
