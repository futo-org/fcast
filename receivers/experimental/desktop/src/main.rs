use rcore::clap::Parser;
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

fn main() -> anyhow::Result<()> {
    let args = rcore::CliArgs::parse();

    if std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
        rcore::slint::BackendSelector::new()
            // .require_vulkan()
            // .require_opengl()
            .select()?;
    }

    rcore::run(args)
}
