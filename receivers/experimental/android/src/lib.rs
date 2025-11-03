use receiver_core::slint;

#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    log_panics::init();

    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    rcore::slint::BackendSelector::new()
        .require_opengl()
        .select()
        .unwrap();

    slint::android::init(app).unwrap();

    // TODO: use same as android sender
    // #[cfg(debug_assertions)]
    // unsafe {
    //     std::env::set_var("GST_DEBUG_NO_COLOR", "true");
    //     std::env::set_var("GST_DEBUG", "4");
    // }

    rcore::run().unwrap();
}
