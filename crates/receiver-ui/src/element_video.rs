//! The desktop video lane, which is the shared `slintvideosink` and this file.
//!
//! Two entry points: the backend selection at startup, and the sink for a
//! running pipeline. What is here is the receiver's own half, which is
//! everything the element deliberately does not own: the cue engine, the
//! `MainWindow` setter, the environment variables, and the render profile the
//! config names. Nothing here describes a video buffer; the element owns the
//! device, the caps, the platform imports and the draw.

use anyhow::{anyhow, Result};
use slint_gstreamer_video::{
    DeviceOptions, Presenter, RenderProfile as ElementProfile, Selection, SlintVideoSink,
    VideoDevice,
};
use std::sync::Arc;
use gst::prelude::*;
use tracing::{info, warn};

/// The device options the receiver's environment asks for. The element takes
/// values; the variable names stay here, where they always were.
fn options() -> DeviceOptions {
    let backend = match std::env::var("FCAST_WGPU_BACKEND").ok().as_deref() {
        Some("gl") => Some(slint_gstreamer_video::wgpu::Backends::GL),
        Some("vulkan") => Some(slint_gstreamer_video::wgpu::Backends::VULKAN),
        Some("metal") => Some(slint_gstreamer_video::wgpu::Backends::METAL),
        Some("dx12") => Some(slint_gstreamer_video::wgpu::Backends::DX12),
        Some(other) => {
            warn!(value = %other, "unknown FCAST_WGPU_BACKEND, using the platform order");
            None
        }
        None => None,
    };
    DeviceOptions {
        backend,
        allow_software: !std::env::var("FCAST_WGPU_SOFTWARE").is_ok_and(|v| v == "0"),
        ..DeviceOptions::default()
    }
}

/// The device this process presents on, once selection has settled.
///
/// `None` until [`select_backend`] succeeds, and on the GL floor until the
/// renderer hands its own device over.
static DEVICE: std::sync::OnceLock<parking_lot::Mutex<Option<Arc<VideoDevice>>>> =
    std::sync::OnceLock::new();

fn device_slot() -> &'static parking_lot::Mutex<Option<Arc<VideoDevice>>> {
    DEVICE.get_or_init(Default::default)
}

/// Opens the device and puts slint on it. The element's own selection, with
/// the receiver's environment read into it.
///
/// False when no device could be opened or slint refused the selection, and
/// the caller then makes its usual OpenGL selection and runs with no video.
pub fn select_backend() -> bool {
    let device = match VideoDevice::create(&options()) {
        Ok(device) => device,
        Err(declined) => {
            // No log subscriber exists yet; this is reported when the sink is
            // built and finds no device.
            *REFUSED.get_or_init(Default::default).lock() = Some(declined.join("; "));
            return false;
        }
    };
    match device.select_slint_backend(Some("dodvg-wgpu")) {
        Ok(Selection::Ready(device)) => {
            *device_slot().lock() = Some(device);
            true
        }
        // The GL floor: slint opens the device on the window's display and the
        // lane adopts it at the first RenderingSetup. See `adopt`.
        Ok(Selection::AdoptLater) => true,
        Err(err) => {
            *REFUSED.get_or_init(Default::default).lock() = Some(err.to_string());
            false
        }
    }
}

static REFUSED: std::sync::OnceLock<parking_lot::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

/// Adopts the device slint's renderer opened, from the window's rendering
/// notifier. The other half of the GL floor.
pub fn adopt(state: slint::RenderingState, api: &slint::GraphicsAPI<'_>) {
    if let Some(device) = VideoDevice::adopt_from(state, api) {
        info!("video lane: adopted the device the renderer opened");
        *device_slot().lock() = Some(Arc::new(device));
    }
}

/// Whether a device is ready.
///
/// The GL floor hands its device over at the first `RenderingSetup`, which is
/// after the pipeline task starts, so the caller waits on this rather than
/// building a sink that would decline.
pub async fn await_device(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if device_slot().lock().is_some() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Renderer and staging buffer for [`downscale_cover`], kept so a track change
/// after the first pays no pipeline compile and no staging allocation.
static COVER: slint_gstreamer_video::Scaler = slint_gstreamer_video::Scaler::new();

/// Antialiased downscale of a decoded cover on the presenting device, feeding
/// the audio-cover blur.
///
/// The full-res upload and the lanczos reduction are the part that costs real
/// time at 4K and up; the caller blurs the small result on the cpu in
/// microseconds. `None` when the lane has no device or the reduction failed,
/// and the caller falls back to the cpu downscale.
pub fn downscale_cover(
    img: &receiver_core::image::RgbaImage,
    thumb_dim: u32,
) -> Option<receiver_core::image::RgbaImage> {
    let device = device_slot().lock().clone()?;
    let size = img.dimensions();
    let small = COVER
        .fit(&device, img.as_raw(), size, thumb_dim)
        .map_err(|err| warn!(?err, "gpu cover downscale failed, cpu fallback"))
        .ok()?;
    receiver_core::image::RgbaImage::from_raw(small.width, small.height, small.pixels)
}

/// Whether the lane has a device, which is what says the reduction above is
/// worth asking for.
pub fn has_device() -> bool {
    device_slot().lock().is_some()
}

/// The video sink, and the cue engine wired to it.
///
/// `None` when no device was selected, and then the player plays sound only,
/// exactly as the old lane behaves.
pub fn make_sink(
    ui: slint::Weak<crate::MainWindow>,
    cues: fcast_video::cue::CueEngine,
    profile: ElementProfile,
) -> Result<(gst::Element, Arc<crate::element_cues::ElementCues>)> {
    let Some(device) = device_slot().lock().clone() else {
        let why = REFUSED
            .get()
            .and_then(|r| r.lock().clone())
            .unwrap_or_else(|| "no device was selected".to_owned());
        return Err(anyhow!("no gpu device for the video lane: {why}"));
    };

    // The setter and the cue tick are the receiver's own: the element does not
    // know about MainWindow, and the cue engine is scheduled against the
    // picture rather than the clock, so it ticks when a frame is really up.
    let overlay = Arc::new(crate::element_cues::ElementCues::new(cues));
    let card_ui = ui.clone();
    let ticker = ui.clone();
    let engine = Arc::clone(&overlay);
    let presenter = Presenter::builder(ui, |ui: &crate::MainWindow, image| {
        use slint::ComponentHandle;
        let bridge = ui.global::<crate::Bridge>();
        match image {
            Some(image) => {
                bridge.set_sw_video_frame(image);
                bridge.set_sw_video_active(true);
            }
            None => {
                bridge.set_sw_video_frame(slint::Image::default());
                bridge.set_sw_video_active(false);
            }
        }
    })
    .on_presented(move |shown| {
        // Already on the UI thread, so the upgrade is a refcount bump.
        let Some(ui) = ticker.upgrade() else {
            return;
        };
        engine.presented(&ui, shown);
    })
    .build();

    let element: gst::Element = SlintVideoSink::new(device, presenter).upcast();
    element.set_property("render-profile", profile);
    // The receiver's own A/B switches, which were environment variables on the
    // old lane and are properties here.
    if std::env::var("FCAST_DESKTOP_WGPU_DMABUF").is_ok_and(|v| v == "0") {
        element.set_property("enable-dmabuf", false);
    }
    if std::env::var("FCAST_DESKTOP_WGPU_UDMABUF").is_ok_and(|v| v == "0") {
        element.set_property("enable-udmabuf", false);
    }
    if std::env::var("FCAST_DESKTOP_WGPU_IOSURFACE").is_ok_and(|v| v == "0") {
        element.set_property("enable-iosurface", false);
    }
    if std::env::var("FCAST_DESKTOP_WGPU_D3D12").is_ok_and(|v| v == "0") {
        element.set_property("enable-d3d12", false);
    }
    // The old lane cleared at end of stream; the element keeps the last frame
    // by default, so it is asked for the old behaviour explicitly.
    element.set_property("keep-last-frame", false);
    // The inspector's stream card. The element says when its description
    // moved, which is a caps change or a route change and nothing per frame,
    // so the card is rebuilt then and never on the frame path.
    element.connect_notify(Some("stats"), {
        let ui = card_ui;
        move |element, _| {
            let stats = element.property::<gst::Structure>("stats");
            // The same description in the log, because a report of a wrong
            // picture is answered by the route and the colorimetry and the
            // reporter cannot open the inspector for us.
            info!(
                width = stats.get::<u32>("width").unwrap_or(0),
                height = stats.get::<u32>("height").unwrap_or(0),
                format = stats.get::<String>("format").unwrap_or_default(),
                arm = stats.get::<String>("arm").unwrap_or_default(),
                matrix = stats.get::<String>("matrix").unwrap_or_default(),
                range = stats.get::<String>("range").unwrap_or_default(),
                transfer = stats.get::<String>("transfer").unwrap_or_default(),
                hdr = stats.get::<bool>("hdr").unwrap_or(false),
                "video lane: stream"
            );
            let _ = ui.upgrade_in_event_loop(move |ui| crate::element_card::publish(&ui, &stats));
        }
    });

    info!(profile = ?profile, "video lane: slintvideosink");
    Ok((element, overlay))
}
