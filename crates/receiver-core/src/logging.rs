use tracing::level_filters::LevelFilter;

fn default_level() -> LevelFilter {
    if cfg!(debug_assertions) {
        LevelFilter::DEBUG
    } else {
        LevelFilter::OFF
    }
}

pub fn init(loglevel: Option<LevelFilter>) {
    // android installs no subscriber, the tracing `log` bridge feeds the
    // activity's android logger and the level lives there
    #[cfg(target_os = "android")]
    let _ = loglevel;
    let prev_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        tracing_panic::panic_hook(panic_info);
        prev_panic_hook(panic_info);
    }));
    // NOT on android: the tracing bridge needs a subscriber and android
    // installs none (only the tracing macros' own `log` fallback reaches the
    // activity logger, and bridged gst events do not take that path). Hooking
    // gst into it there sent every GStreamer line into the void, which is how
    // both a caps-negotiation failure and the codec probe's own output became
    // undebuggable from logcat. GStreamer's default handler already writes to
    // logcat natively on android, so the right move is to leave it installed.
    #[cfg(not(target_os = "android"))]
    {
        tracing_gstreamer::integrate_events();
        gst::log::remove_default_log_function();
    }

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

        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_timer(tracing_subscriber::fmt::time::Uptime::default());
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        if let Ok(spec) = std::env::var("GST_DEBUG") {
            gst::log::set_threshold_from_string(&spec, false);
        }
        // The `log` bridge the registry installs sets `log::max_level` to
        // TRACE, and the `log` macros build their arguments before the bridge
        // gets to refuse the record. wgpu-core alone formats a label string
        // for every bind group, pipeline and buffer it records, some thirty
        // heap allocations per rendered frame, all for lines nothing prints.
        // Capping the bridge at the level the filter can actually let
        // through stops that at the macro.
        let bridge_level = <EnvFilter as tracing_subscriber::Layer<tracing_subscriber::Registry>>::max_level_hint(&env_filter)
            .unwrap_or(LevelFilter::TRACE);
        let registry = tracing_subscriber::registry()
            .with(fmt_layer)
            .with(env_filter)
            .with(targets);
        #[cfg(feature = "tracy")]
        let registry = registry.with(tracing_tracy::TracyLayer::default());
        registry.init();
        log::set_max_level(log_level(bridge_level));
    }

    #[cfg(target_os = "android")]
    {
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        // GST_DEBUG has no way in from adb (env vars do not cross the
        // zygote), so a system property stands in. Survives reinstalls,
        // read once at startup:
        //   adb shell setprop debug.fcast.gst "GST_PADS:5,GST_CAPS:5"
        //   (restart the app; clear with: setprop debug.fcast.gst '""')
        if let Ok(out) = std::process::Command::new("getprop")
            .arg("debug.fcast.gst")
            .output()
        {
            let spec = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !spec.is_empty() && spec != "\"\"" {
                gst::log::set_threshold_from_string(&spec, false);
            }
        }
        gst::log::set_threshold_for_name("gldebug", gst::DebugLevel::None);
        gst::log::set_threshold_for_name("video-info", gst::DebugLevel::None);
        // the android decoder is young, keep its negotiation visible
        gst::log::set_threshold_for_name("amcviddec", gst::DebugLevel::Info);
        // What the device's MediaCodec can actually decode, which decides
        // whether a stream gets hardware decode or falls through to software.
        // Logged once at startup, and the only evidence that the probe ran at
        // all rather than silently degrading to "assume everything works".
        gst::log::set_threshold_for_name("amccodeclist", gst::DebugLevel::Info);
    }
}

/// The `log` side of a tracing level filter, for the bridge cap above.
#[cfg(not(target_os = "android"))]
fn log_level(level: LevelFilter) -> log::LevelFilter {
    match level {
        LevelFilter::OFF => log::LevelFilter::Off,
        LevelFilter::ERROR => log::LevelFilter::Error,
        LevelFilter::WARN => log::LevelFilter::Warn,
        LevelFilter::INFO => log::LevelFilter::Info,
        LevelFilter::DEBUG => log::LevelFilter::Debug,
        LevelFilter::TRACE => log::LevelFilter::Trace,
    }
}
