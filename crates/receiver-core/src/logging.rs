#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;

#[cfg(not(target_os = "android"))]
fn log_level() -> LevelFilter {
    match std::env::var("FCAST_LOG") {
        Ok(level) => match level.to_ascii_lowercase().as_str() {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            "trace" => LevelFilter::TRACE,
            _ => LevelFilter::OFF,
        },
        #[cfg(debug_assertions)]
        Err(_) => LevelFilter::DEBUG,
        #[cfg(not(debug_assertions))]
        Err(_) => LevelFilter::OFF,
    }
}

pub fn init(loglevel: Option<LevelFilter>) {
    let prev_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        tracing_panic::panic_hook(panic_info);
        prev_panic_hook(panic_info);
    }));
    tracing_gstreamer::integrate_events();
    gst::log::remove_default_log_function();

    #[cfg(not(target_os = "android"))]
    {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        let log_level = loglevel.unwrap_or(log_level());
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("tracing_gstreamer::callsite", LevelFilter::OFF)
            .with_target("mdns_sd", LevelFilter::INFO)
            .with_target("hyper_util", LevelFilter::INFO)
            .with_target("h2", LevelFilter::INFO)
            .with_target("winit", LevelFilter::INFO)
            .with_default(log_level);
        let fmt_layer = tracing_subscriber::fmt::layer();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        let registry = tracing_subscriber::registry().with(fmt_layer).with(filter);
        #[cfg(feature = "tracy")]
        let registry = registry.with(tracing_tracy::TracyLayer::default());
        registry.init();
    }

    #[cfg(target_os = "android")]
    {
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::log::set_threshold_for_name("gldebug", gst::DebugLevel::None);
        gst::log::set_threshold_for_name("video-info", gst::DebugLevel::None);
    }
}
