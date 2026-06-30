#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;

#[cfg(not(target_os = "android"))]
fn default_level() -> LevelFilter {
    if cfg!(debug_assertions) {
        LevelFilter::DEBUG
    } else {
        LevelFilter::OFF
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
        use tracing_subscriber::{
            EnvFilter, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt,
        };

        let default = loglevel.unwrap_or_else(default_level);
        let builder = EnvFilter::builder().with_default_directive(default.into());
        let env_filter = match loglevel {
            Some(_) => builder.parse_lossy(""),
            None => builder.with_env_var("FCAST_LOG").from_env_lossy(),
        };

        let targets = Targets::new()
            .with_target("tracing_gstreamer::callsite", LevelFilter::OFF)
            .with_target("mdns_sd", LevelFilter::INFO)
            .with_target("hyper_util", LevelFilter::INFO)
            .with_target("h2", LevelFilter::INFO)
            .with_target("winit", LevelFilter::INFO)
            .with_default(LevelFilter::TRACE);

        let fmt_layer = tracing_subscriber::fmt::layer();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        let registry = tracing_subscriber::registry()
            .with(fmt_layer)
            .with(env_filter)
            .with(targets);
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
