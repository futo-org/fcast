use rcore::clap::Parser;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> anyhow::Result<()> {
    let args = rcore::CliArgs::parse();

    if std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
        let selector = rcore::slint::BackendSelector::new();
        #[cfg(not(target_os = "windows"))]
        let selector = selector.require_opengl();
        #[cfg(target_os = "windows")]
        let selector = selector.require_opengl_with_version(4, 0);
        selector.select()?;
    }

    rcore::run(args)
}
