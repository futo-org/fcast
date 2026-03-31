use rcore::clap::Parser;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> anyhow::Result<()> {
    let args = rcore::CliArgs::parse();

    if std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
        rcore::slint::BackendSelector::new()
            .require_opengl()
            .select()?;
    }

    rcore::run(args)
}
