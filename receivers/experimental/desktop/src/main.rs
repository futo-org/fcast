fn main() -> anyhow::Result<()> {
    // env_logger::Builder::from_default_env()
    //     .filter_module("receiver-desktop", rcore::common::default_log_level())
    //     .filter_module("rcore", rcore::common::default_log_level())
    //     .init();

    if std::env::var("SLINT_BACKEND") == Err(std::env::VarError::NotPresent) {
        rcore::slint::BackendSelector::new()
            .require_opengl()
            .select()?;
    }

    rcore::run()
}
