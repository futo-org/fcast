//! The receiver's user interface: the slint window, the render loop, and the
//! consumer half of the GUI command channel.
//!
//! Everything that is not the UI lives in `receiver-core`, which this crate
//! re-exports wholesale, so a receiver binary depends only on this one. Keeping
//! the split this way round is what makes `cargo test -p receiver-core` free of
//! slint (and of compiling the `.slint` sources).

use anyhow::Result;
use gst::prelude::*;
use gst_base::prelude::BaseSinkExt;
#[cfg(target_os = "android")]
use slint::android::android_activity::WindowManagerFlags;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

pub use slint;

#[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
pub use fcast_video::WaylandSubsurfaceSink;
pub use receiver_core::*;
// The rest arrives through the `receiver_core::*` glob above.
use receiver_core::{gui::GuiController, message::Message};

use fcast_video::{opengl, placebo, render_latency, video};

slint::include_modules!();

pub mod gui;

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

fn video_dbg_info(frame: &video::Frame) -> Option<UiVideoDbgInfo> {
    use slint::ToSharedString;

    let info = frame.data.video_info()?;
    let colorimetry = info.colorimetry();
    let fps = info.fps();
    let par = info.par();

    let framerate = if fps.denom() == 0 {
        String::new()
    } else {
        format!("{:.3} fps", fps.numer() as f64 / fps.denom() as f64)
    };

    let hdr = match frame.mastering_display_info.as_ref() {
        Some(mdi) => {
            let cll = frame
                .content_light_level
                .as_ref()
                .map_or_else(String::new, |cll| {
                    format!(
                        ", CLL {}/{}",
                        cll.max_content_light_level, cll.max_frame_average_light_level
                    )
                });
            format!(
                "mastering {:.0}–{:.0} nits{cll}",
                mdi.min_luminance_as_nits(),
                mdi.max_luminance_as_nits(),
            )
        }
        None => "SDR".to_owned(),
    };

    let rotation = match frame.rotation {
        video::Rotation::Rotate0 => "0°",
        video::Rotation::Rotate90 => "90°",
        video::Rotation::Rotate180 => "180°",
        video::Rotation::Rotate270 => "270°",
    };

    Some(UiVideoDbgInfo {
        format: format!("{:?} ({}-bit)", info.format(), info.comp_depth(0)).to_shared_string(),
        resolution: format!("{}x{}", info.width(), info.height()).to_shared_string(),
        framerate: framerate.to_shared_string(),
        pixel_aspect: format!("{}:{}", par.numer(), par.denom()).to_shared_string(),
        rotation: rotation.to_shared_string(),
        memory: frame.data.memory_kind().to_shared_string(),
        primaries: format!("{:?}", colorimetry.primaries()).to_shared_string(),
        transfer: format!("{:?}", colorimetry.transfer()).to_shared_string(),
        matrix: format!("{:?}", colorimetry.matrix()).to_shared_string(),
        range: format!("{:?}", colorimetry.range()).to_shared_string(),
        hdr: hdr.to_shared_string(),
    })
}

/// Per-tick video state, shared on the event-loop thread between the Slint
/// rendering notifier and the event-loop-clocked handlers: a subsurface sink
/// stacked above the GUI parks winit's redraw loop, so frames and obstruction
/// changes must reach the sink without a repaint.
struct VideoTick<S> {
    video_sink: S,
    payload_handle: Option<video::imp::VideoPayloadHandle>,
    /// Both render paths report their measured cost back to it as
    /// `render-delay`.
    sink_elem: Option<video::FSink>,
    cached_frame: Option<video::Frame>,
    /// Render on the next repaint even without a new payload.
    force_render: bool,
    /// The GL placebo context isn't current outside the rendering notifier;
    /// flush next tick.
    pending_gl_flush: bool,
    render_latency: render_latency::RenderLatencyTracker,
}

/// Whether an overlay change can be folded into a frame right now, taking the
/// engine's one-shot notification ONLY when it can be applied.
///
/// # `has_frame` comes first, and that is the whole function
///
/// `CueEngine::take_dirty` clears as it reads. Evaluated the other way round,
/// a pass with no cached frame consumes the bit and drops the only notice the
/// engine sends -- and the `&&` short-circuit makes the order load-bearing, so
/// this is a function rather than a condition spelled out at each call site.
///
/// While frames flow, losing it costs nothing: the next frame carries the
/// overlays itself. While PAUSED nothing else is coming, so the cue stays
/// unpainted until the viewer resumes -- which is exactly the "subtitles
/// appear the instant I hit play" report this fixes.
///
/// Leaving the bit up instead is safe in both directions: the engine keeps the
/// overlays, `current_overlays()` re-reads them, and the pass that folds the
/// next payload applies them.
fn overlay_change_applies(has_frame: bool, engine: &fcast_video::cue::CueEngine) -> bool {
    has_frame && engine.take_dirty()
}

impl<S> VideoTick<S> {
    /// Fold an overlay change into the cached frame, and say whether that
    /// leaves something to draw.
    ///
    /// The ordering that makes it correct is [`overlay_change_applies`].
    fn fold_overlay_change(&mut self, engine: &fcast_video::cue::CueEngine) -> bool {
        if !overlay_change_applies(self.cached_frame.is_some(), engine) {
            return false;
        }
        let frame = self
            .cached_frame
            .as_mut()
            .expect("`overlay_change_applies` answered true, so there is a frame");
        frame.overlays = engine.current_overlays().into_iter().collect();
        self.force_render = true;
        true
    }

    /// Record one render's cost and, on a meaningful change, push the new
    /// `render-delay` to the sink. The LATENCY message is what makes it take
    /// effect.
    fn note_render_cost(&mut self, cost: std::time::Duration) {
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *OFF.get_or_init(|| std::env::var_os("FCAST_NO_RENDER_DELAY_FEEDBACK").is_some()) {
            return;
        }
        self.render_latency.record(cost);
        let Some(delay) = self.render_latency.poll(std::time::Instant::now()) else {
            return;
        };
        let Some(sink) = self.sink_elem.as_ref() else {
            return;
        };
        sink.set_render_delay(gst::ClockTime::from_nseconds(delay.as_nanos() as u64));
        let _ = sink.post_message(gst::message::Latency::builder().src(sink).build());
    }
}

/// Run the main app. Slint is assumed to be initialized by the platform
/// specific target.
pub fn run<S: VideoSink + 'static>(
    #[cfg(not(target_os = "android"))] settings: Settings,
    #[cfg(target_os = "android")] android_app: slint::android::AndroidApp,
    #[cfg(target_os = "android")] mut platform_event_rx: UnboundedReceiver<Message>,
    video_sink: S,
) -> Result<()> {
    let start = std::time::Instant::now();

    receiver_core::tune_allocator();
    receiver_core::allow_ptrace_attach();

    logging::init(settings.log_level());

    if let Err(err) = tokio_rustls::rustls::crypto::ring::default_provider().install_default() {
        error!(
            ?err,
            "Failed to register ring as rustls default crypto provider"
        );
    }

    let (msg_tx, event_rx) = mpsc::unbounded_channel::<Message>();
    let msg_tx = MessageSender::new(msg_tx);
    let (fin_tx, fin_rx) = tokio::sync::oneshot::channel::<()>();

    #[cfg(target_os = "android")]
    RUNTIME.spawn({
        let msg_tx = msg_tx.clone();
        async move {
            while let Some(event) = platform_event_rx.recv().await {
                msg_tx.send(event);
            }

            debug!("Platform event proxy finished");
        }
    });

    let is_headless = settings.headless();

    let sink_mutex = Arc::new(parking_lot::Mutex::new(None::<video::FSink>));
    let ui = if is_headless {
        None
    } else {
        Some(MainWindow::new()?)
    };
    #[cfg(feature = "systray")]
    let want_systray = settings.want_systray();
    #[cfg(feature = "systray")]
    let systray_holder: Rc<RefCell<Option<SystemTray>>> = Rc::new(RefCell::new(None));

    let gui_is_visible = gui::GuiIsVisible::new();
    let mut renderer_tx = None;
    let mut _obstruction_watchdog = None;
    if let Some(ui) = &ui {
        let pl_log = libplacebo::Log::new().unwrap();
        let render_opts = settings.rendering_options();

        #[cfg(debug_assertions)]
        ui.global::<Bridge>().set_is_debugging(true);

        let tick = Rc::new(RefCell::new(VideoTick {
            video_sink,
            payload_handle: None,
            sink_elem: None,
            cached_frame: None,
            force_render: false,
            pending_gl_flush: false,
            render_latency: render_latency::RenderLatencyTracker::new(),
        }));

        let (renderer_chan_tx, renderer_rx) = std::sync::mpsc::channel::<gui::RendererMessage>();
        renderer_tx = Some(renderer_chan_tx);
        ui.window().set_rendering_notifier({
            let ui_weak = ui.as_weak();
            #[cfg(not(target_os = "android"))]
            let mut start_fullscreen = Some(settings.fullscreen());
            let mut prev_size = (0, 0);
            let mut sink = None;
            let msg_tx = msg_tx.clone();
            let mut renderer = None;
            let mut pl_context = None;
            #[cfg(target_os = "linux")]
            let mut drm_formats = HashSet::new();
            let gui_is_visible = gui_is_visible.clone();
            let tick = tick.clone();
            let sink_mutex = Arc::clone(&sink_mutex);
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    debug!("Got graphics API: {graphics_api:?}");
                    let ui_weak = ui_weak.clone();

                    // The controls reveal must be input-driven: while the GUI is redraw-parked
                    // its `changed` callbacks never run, so pointer activity has to restack the
                    // video directly. Registered here because on_winit_window_event silently
                    // no-ops unless the winit window adapter exists.
                    #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
                    if let Some(ui) = ui_weak.upgrade() {
                        use i_slint_backend_winit::WinitWindowAccessor;
                        debug!("Installing winit input-reveal filter");
                        ui.window().on_winit_window_event({
                            let tick = tick.clone();
                            move |_window, event| {
                                use i_slint_backend_winit::winit::event::WindowEvent;
                                if matches!(
                                    event,
                                    WindowEvent::CursorEntered { .. }
                                        | WindowEvent::CursorMoved { .. }
                                ) {
                                    // try_borrow: stay panic-free if input ever
                                    // races the other tick users.
                                    if let Ok(mut t) = tick.try_borrow_mut() {
                                        if t.video_sink.self_clocked() {
                                            debug!("Pointer activity while parked: revealing GUI");
                                            t.video_sink.set_video_obstructed(true, true);
                                        }
                                    }
                                }
                                i_slint_backend_winit::EventResult::Propagate
                            }
                        });
                    }

                    #[cfg(not(target_os = "android"))]
                    if let Some(fullscreen) = start_fullscreen.take() {
                        ui_weak
                            .upgrade()
                            .unwrap()
                            .window()
                            .set_fullscreen(fullscreen);
                    }

                    if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api {
                        #[cfg(target_os = "linux")]
                        {
                            egl::ensure_init();
                            let egl = glutin_egl_sys::egl::Egl::load_with(|symbol| {
                                get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                            });

                            let display = unsafe { egl.GetCurrentDisplay() };
                            let err = unsafe { egl.GetError() };
                            if !display.is_null() && err == glutin_egl_sys::egl::SUCCESS as i32 {
                                pl_context = unsafe {
                                    Some(
                                        placebo::PlaceboContext::new_egl(
                                            &pl_log,
                                            &render_opts,
                                            display as *mut _,
                                            egl.GetCurrentContext() as *mut _,
                                        )
                                        .unwrap(),
                                    )
                                };

                                let extensions = egl::get_extensions(&egl);
                                if extensions.contains(&egl::Extension::ImageDmaBufImport)
                                    && extensions
                                        .contains(&egl::Extension::ImageDmaBufImportModifiers)
                                {
                                    match egl::get_supported_dma_drm_formats(display) {
                                        Ok(formats) => {
                                            debug!(
                                                formats = formats
                                                    .iter()
                                                    .map(|fmt| format!(
                                                        "{}:{:?}",
                                                        fmt.code, fmt.modifier
                                                    ))
                                                    .collect::<Vec<_>>()
                                                    .join(" "),
                                                "Got supported DMA DRM formats"
                                            );
                                            drm_formats = formats;
                                        }
                                        Err(err) => {
                                            error!(?err, "Failed to get supported DMA DRM formats");
                                        }
                                    }
                                }
                            } else {
                                pl_context = Some(
                                    placebo::PlaceboContext::new(&pl_log, &render_opts).unwrap(),
                                );
                            }
                        }

                        #[cfg(not(target_os = "linux"))]
                        {
                            pl_context =
                                Some(placebo::PlaceboContext::new(&pl_log, &render_opts).unwrap());
                        }

                        let gl = unsafe {
                            glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                        };
                        match opengl::Renderer::new(gl) {
                            Ok(r) => renderer = Some(r),
                            Err(err) => error!(?err, "Failed to create renderer"),
                        }
                    }

                    // Let the sink grab native window handles (e.g. the surface to parent to).
                    if let Some(ui) = ui_weak.upgrade() {
                        tick.borrow_mut().video_sink.setup(ui.window());
                    }

                    gui_is_visible.set(true);
                }
                slint::RenderingState::BeforeRendering => {
                    let Some(ui) = ui_weak.upgrade() else {
                        error!("Failed to upgrade ui");
                        return;
                    };

                    let bridge = ui.global::<Bridge>();

                    let mut clear_video_overlays = false;
                    while let Ok(msg) = renderer_rx.try_recv() {
                        if matches!(msg, gui::RendererMessage::ClearVideoOverlays) {
                            clear_video_overlays = true;
                            continue;
                        }
                        if let Some(renderer) = renderer.as_mut() {
                            match msg {
                                gui::RendererMessage::ClearVideoOverlays => unreachable!(),
                                gui::RendererMessage::CreateBluredAudioTrackCover(img) => {
                                    let (width, height) = img.image.dimensions();
                                    match renderer.blur_rgba8_image(
                                        img.image.as_raw(),
                                        width,
                                        height,
                                    ) {
                                        Ok(tex) => {
                                            bridge.set_blured_audio_track_cover(CompoundImage {
                                                img: tex.to_borrowed_slint_image(),
                                                rotation: image::orientation_to_degs(
                                                    img.orientation,
                                                ),
                                            });
                                            renderer.blured_audio_cover = Some(tex);
                                        }
                                        Err(err) => {
                                            error!(?err, "Failed to blur audio track cover")
                                        }
                                    }
                                }
                                gui::RendererMessage::ClearBluredAudioTrackCover => {
                                    bridge.set_blured_audio_track_cover(CompoundImage::default());
                                    renderer.blured_audio_cover.take();
                                }
                            }
                        }
                    }

                    let mut tick_ref = tick.borrow_mut();
                    let t = &mut *tick_ref;

                    if clear_video_overlays
                        && let Some(frame) = t.cached_frame.as_mut()
                        && !frame.overlays.is_empty()
                    {
                        frame.overlays.clear();
                        t.force_render = true;
                    }

                    let Some(sink) = sink.as_mut() else {
                        if let Some(new_sink) = sink_mutex.lock().take() {
                            #[cfg(target_os = "linux")]
                            new_sink.set_property(
                                "drm-formats",
                                video::imp::DrmFormats(Arc::new(drm_formats.clone())),
                            );
                            t.payload_handle = Some(new_sink.property("payload-handle"));
                            t.sink_elem = Some(new_sink.clone());
                            sink = Some(new_sink);
                        }
                        return;
                    };

                    if std::mem::take(&mut t.pending_gl_flush)
                        && let Some(placebo) = pl_context.as_mut()
                    {
                        t.video_sink.flush_cache(placebo);
                    }

                    // `video-scene-clean` also requires that no idle/loading view is still
                    // fading over the player: parking the GUI on one flashes it on reveal.
                    t.video_sink
                        .set_gui_scene_is_player(ui.get_video_scene_clean());

                    let mut new_frame = false;
                    if let Some(payload_handle) = &t.payload_handle {
                        if let Some(pay) = payload_handle.0.lock().take() {
                            match pay {
                                Some(frame) => {
                                    t.cached_frame = Some(frame);
                                    new_frame = true;
                                }
                                // EOS
                                None => {
                                    t.cached_frame = None;
                                    t.video_sink.clear();
                                    if let Some(placebo) = pl_context.as_mut() {
                                        t.video_sink.flush_cache(placebo);
                                    }
                                    // Undo the player's winit-level cursor hide.
                                    bridge.invoke_set_cursor_hidden(false);
                                }
                            }
                        }
                    }

                    // The cue engine changed without a frame carrying it. A new frame already
                    // arrives with the engine's overlays on it, so this is for the frames that
                    // are NOT coming: everything visible while PAUSED.
                    let engine = sink.cue_engine();
                    if t.fold_overlay_change(&engine) {
                        debug!("paused-repaint: overlay change folded in a render pass");
                    }

                    let new_size = ui.window().size();
                    let new_size = (new_size.width, new_size.height);
                    // A window mid-create or mid-minimize reports a zero dimension, which
                    // reaches basetextoverlay and assrender through the allocation query and
                    // aborts them. Skip the property set WITHOUT updating `prev_size`, so the
                    // restore to a real size still registers.
                    let size_changed = new_size != prev_size && new_size.0 != 0 && new_size.1 != 0;
                    if size_changed {
                        sink.set_property(
                            "window-resolution",
                            video::imp::WindowResolution {
                                width: new_size.0,
                                height: new_size.1,
                            },
                        );
                        prev_size = new_size;
                    }

                    if let Some(renderer) = renderer.as_mut() {
                        use glow::HasContext;
                        let clear_color = t.video_sink.get_clear_color();
                        unsafe {
                            renderer.gl.clear_color(
                                clear_color[0],
                                clear_color[1],
                                clear_color[2],
                                clear_color[3],
                            );
                            renderer.gl.clear(glow::COLOR_BUFFER_BIT);
                        }
                    }

                    let force_render = std::mem::take(&mut t.force_render);
                    let mut render_cost = None;
                    if let Some(frame) = t.cached_frame.as_mut() {
                        bridge.set_video_frame_width(frame.data.width() as i32);
                        bridge.set_video_frame_height(frame.data.height() as i32);

                        if bridge.get_show_inspector() && (new_frame || force_render) {
                            match video_dbg_info(frame) {
                                Some(info) => {
                                    bridge.set_video_dbg_info(info);
                                    bridge.set_have_video_dbg_info(true);
                                }
                                None => bridge.set_have_video_dbg_info(false),
                            }
                        }

                        if (new_frame
                            || size_changed
                            || force_render
                            || t.video_sink.needs_render_every_repaint())
                            && let Some(placebo) = pl_context.as_mut()
                            && let Some(renderer) = renderer.as_ref()
                        {
                            let start = std::time::Instant::now();
                            if let Err(err) =
                                t.video_sink.render(placebo, &renderer.gl, frame, prev_size)
                            {
                                error!(?err, "video sink render failed");
                            } else {
                                render_cost = Some(start.elapsed());
                            }
                        }
                    }
                    if let Some(cost) = render_cost {
                        t.note_render_cost(cost);

                        if bridge.get_show_inspector() {
                            use slint::ToSharedString;
                            let (p95, applied) = t.render_latency.debug_snapshot();
                            let p95 = p95.map_or_else(
                                || "warming up".to_owned(),
                                |d| format!("{:.2} ms p95", d.as_secs_f64() * 1000.0),
                            );
                            bridge.set_render_latency_info(
                                format!(
                                    "render: {:.2} ms, {p95}, delay {:.2} ms",
                                    cost.as_secs_f64() * 1000.0,
                                    applied.as_secs_f64() * 1000.0,
                                )
                                .to_shared_string(),
                            );
                        }
                    }
                }
                slint::RenderingState::RenderingTeardown => {
                    gui_is_visible.set(false);

                    let (feedback_tx, feedback_rx) = oneshot::channel::<()>();

                    msg_tx.send(Message::GuiWindowClosed(feedback_tx));
                    match feedback_rx.recv_timeout(Duration::from_millis(2500)) {
                        Ok(_) => debug!("Player shutdown successfully"),
                        Err(err) => {
                            error!(?err, "Failed to receive feedback of player shutdown")
                        }
                    }

                    let mut t = tick.borrow_mut();
                    t.cached_frame.take();

                    if let Some(placebo) = pl_context.as_mut() {
                        t.video_sink.teardown(placebo);
                    }

                    pl_context.take();
                }
                _ => (),
            }
        })?;

        // Pushed from MainWindow whenever the GUI starts/stops drawing over the video
        // area.
        ui.global::<Bridge>().on_video_obstructed_changed({
            let tick = tick.clone();
            move |obstructed| {
                debug!(obstructed, "video obstruction changed");
                tick.borrow_mut()
                    .video_sink
                    .set_video_obstructed(obstructed, true);
            }
        });

        // One invocation per decoded frame, proxied from the GStreamer streaming
        // thread. It normally just schedules a repaint, but renders directly
        // while the GUI is parked.
        ui.global::<Bridge>().on_new_video_frame({
            let tick = tick.clone();
            let ui_weak = ui.as_weak();
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let mut tick_ref = tick.borrow_mut();
                let t = &mut *tick_ref;
                let bridge = ui.global::<Bridge>();
                // Self-clocked means the redraw loop may be parked, and with it Slint's
                // `changed` callbacks, so obstruction changes must be polled instead.
                if t.video_sink.self_clocked() && bridge.get_video_obstructed() {
                    t.video_sink.set_video_obstructed(true, true);
                }
                if !t.video_sink.self_clocked() {
                    // On this path an overlay-only change (no frame payload behind the
                    // invoke) is entirely at the mercy of the requested redraw actually
                    // producing a render pass on a static, paused surface. The pass
                    // logs "folded in a render pass" when it runs; a request with no
                    // fold after it is the smoking gun.
                    let overlay_only = t
                        .payload_handle
                        .as_ref()
                        .is_none_or(|ph| ph.0.lock().is_none());
                    if overlay_only {
                        debug!("paused-repaint: overlay change, no payload; redraw requested (not self-clocked)");
                    }
                    ui.window().request_redraw();
                    return;
                }
                let Some(next_payload) =
                    t.payload_handle.as_ref().and_then(|ph| ph.0.lock().take())
                else {
                    // NO PAYLOAD, which is what `overlays-changed` looks like: the
                    // engine's set changed and no frame is carrying it. Returning here
                    // is what left a paused seek's cue unpainted until the viewer
                    // resumed -- self-clocked parks winit's redraw loop, so nothing
                    // else would come back to ask.
                    let Some(engine) = t.sink_elem.as_ref().map(|sink| sink.cue_engine()) else {
                        return;
                    };
                    if !t.fold_overlay_change(&engine) {
                        return;
                    }
                    let frame = t
                        .cached_frame
                        .as_mut()
                        .expect("fold_overlay_change only answers true with a frame");
                    let size = ui.window().size();
                    let start = std::time::Instant::now();
                    let render_result = t
                        .video_sink
                        .render_standalone(frame, (size.width, size.height));
                    let render_cost = start.elapsed();
                    match render_result {
                        Ok(true) => {
                            t.force_render = false;
                            t.note_render_cost(render_cost);
                        }
                        // Raced a restack, or the renderer refused: fall back to the
                        // repaint path, which still has `force_render` armed.
                        Ok(false) => ui.window().request_redraw(),
                        Err(err) => {
                            error!(?err, "Standalone overlay repaint failed");
                            ui.window().request_redraw();
                        }
                    }
                    return;
                };
                match next_payload {
                    // EOS: clear() unmaps the subsurface, so the requested redraw fires.
                    None => {
                        t.cached_frame = None;
                        t.video_sink.clear();
                        t.video_sink.flush_cache_standalone();
                        t.pending_gl_flush = true;
                        // Undo the player's winit-level cursor hide.
                        bridge.invoke_set_cursor_hidden(false);
                        ui.window().request_redraw();
                    }
                    Some(frame) => {
                        t.cached_frame = Some(frame);
                        let frame = t.cached_frame.as_mut().unwrap();
                        bridge.set_video_frame_width(frame.data.width() as i32);
                        bridge.set_video_frame_height(frame.data.height() as i32);
                        let size = ui.window().size();
                        let start = std::time::Instant::now();
                        let render_result = t
                            .video_sink
                            .render_standalone(frame, (size.width, size.height));
                        let render_cost = start.elapsed();
                        match render_result {
                            Ok(true) => {
                                t.note_render_cost(render_cost);
                            }
                            // Raced a restack: the payload slot is already empty, so fall
                            // back to the repaint path with a forced render.
                            Ok(false) => {
                                t.force_render = true;
                                ui.window().request_redraw();
                            }
                            Err(err) => {
                                error!(?err, "Standalone video render failed");
                                t.force_render = true;
                                ui.window().request_redraw();
                            }
                        }
                    }
                }
            }
        });

        ui.global::<Bridge>().on_inspector_toggled({
            let ui_weak = ui.as_weak();
            let tick = tick.clone();
            let msg_tx = msg_tx.clone();
            move |active| {
                msg_tx.send(Message::InspectorActive(active));
                if let Some(ui) = ui_weak.upgrade() {
                    tick.borrow_mut().force_render = true;
                    ui.window().request_redraw();

                    // Drop the graph dump and per-tick models: a big pipeline's scene
                    // holds thousands of rects, texts and hit zones.
                    if !active {
                        let state = ui.global::<InspectorState>();
                        state.set_have_graph(false);
                        state.set_graph(GraphDump::default());
                        state.set_tracks(Rc::new(slint::VecModel::default()).into());
                        state.set_sources_lines(Rc::new(slint::VecModel::default()).into());
                        state.set_internals_lines(Rc::new(slint::VecModel::default()).into());
                        state.set_sink_lines(Rc::new(slint::VecModel::default()).into());
                        state.set_have_bitrate(false);
                        state.set_video_bitrate_path(Default::default());
                        state.set_audio_bitrate_path(Default::default());
                        state.set_have_buffering(false);
                    }
                }
            }
        });

        // Backstop for obstruction changes while the video sits above the GUI and no
        // frames are flowing (e.g. a pause racing the last frame's poll).
        let watchdog = slint::Timer::default();
        watchdog.start(slint::TimerMode::Repeated, Duration::from_millis(250), {
            let tick = tick.clone();
            let ui_weak = ui.as_weak();
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let mut t = tick.borrow_mut();
                if t.video_sink.self_clocked() && ui.global::<Bridge>().get_video_obstructed() {
                    t.video_sink.set_video_obstructed(true, true);
                }
            }
        });
        _obstruction_watchdog = Some(watchdog);
    }

    let gui_tx = if let Some(ui) = &ui {
        let (gui_tx, gui_rx) = mpsc::unbounded_channel::<gui::UpdateGuiCommand>();

        // Shown only once the listening port is committed, so a port conflict that
        // ends in quitting never starts a tray at all.
        let on_show_tray: Box<dyn FnOnce()> = {
            #[cfg(feature = "systray")]
            {
                if want_systray {
                    let ui_weak = ui.as_weak();
                    let holder = systray_holder.clone();
                    Box::new(move || {
                        let systray = match SystemTray::new() {
                            Ok(systray) => systray,
                            Err(err) => {
                                error!(?err, "Failed to create system tray");
                                return;
                            }
                        };
                        systray.on_toggle_window({
                            let ui_weak = ui_weak.clone();
                            move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let win = ui.window();
                                    if win.is_visible() {
                                        let _ = win.hide();
                                    } else {
                                        let _ = win.show();
                                    }
                                }
                            }
                        });
                        systray.on_quit(|| {
                            let _ = slint::quit_event_loop();
                        });
                        log_if_err!(systray.show());
                        // Keep it alive for the rest of the session.
                        *holder.borrow_mut() = Some(systray);
                    })
                } else {
                    Box::new(|| {})
                }
            }
            #[cfg(not(feature = "systray"))]
            {
                Box::new(|| {})
            }
        };

        gui::spawn_command_handler(ui.as_weak(), gui_rx, renderer_tx.unwrap(), on_show_tray);
        Some(gui_tx)
    } else {
        None
    };

    let gui = GuiController::new(gui_tx, gui_is_visible.clone());

    #[allow(unused_variables)]
    #[cfg(not(target_os = "android"))]
    let no_main_window = settings.no_main_window();
    let event_loop_jh = RUNTIME.spawn({
        let ui_weak = ui.as_ref().map(|ui| ui.as_weak());
        let msg_tx = msg_tx.clone();
        async move {
            gstreamer::init_and_load_plugins();

            let (video_sink_elem, cue_engine) = if let Some(ui_weak) = ui_weak {
                let sink = video::FSink::new();
                // Cloned out here because the player only ever sees the bare `gst::Element`.
                let cue_engine = sink.cue_engine();
                {
                    let ui_weak = ui_weak.clone();
                    sink.connect("frame-available", false, move |_| {
                        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.global::<Bridge>().invoke_new_video_frame();
                        });

                        None
                    });
                }
                // The overlay set changed without a new frame behind it. While PAUSED this
                // is the only thing that can put a newly selected track's cue on screen.
                sink.connect("overlays-changed", false, move |_| {
                    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.global::<Bridge>().invoke_new_video_frame();
                    });

                    None
                });

                let video_sink_elem = sink.clone();
                *sink_mutex.lock() = Some(sink);
                (Some(video_sink_elem), Some(cue_engine))
            } else {
                (None, None)
            };

            let app = application::Application::new(
                gui,
                video_sink_elem.map(|e| e.upcast()),
                cue_engine,
                msg_tx,
                #[cfg(target_os = "android")]
                android_app,
                #[cfg(not(target_os = "android"))]
                settings,
            )
            .await;

            // This task is detached: fail visibly and quit rather than leave the Slint loop
            // running a UI with no protocol handling behind it.
            let result = match app {
                Ok(app) => app.run_event_loop(event_rx, fin_tx).await,
                Err(err) => Err(err),
            };

            if let Err(err) = result {
                error!(?err, "Receiver event loop failed");
                let _ = slint::quit_event_loop();
            }
        }
    });

    #[cfg(not(target_os = "android"))]
    RUNTIME.spawn({
        let msg_tx = msg_tx.clone();
        async move {
            if let Err(err) = tokio::signal::ctrl_c().await {
                error!(?err, "Failed to listen for ctrl+c event");
            } else {
                debug!("Got Ctrl+C");
                if is_headless {
                    msg_tx.send(Message::Quit);
                } else {
                    let _ = slint::quit_event_loop();
                }
            }
        }
    });

    if let Some(ui) = ui {
        gui::register_callbacks(&ui, msg_tx.clone());
        info!(initialized_in = ?start.elapsed());

        // Without a tray, `run()` quits when the window is closed.
        #[cfg(any(target_os = "android", not(feature = "systray")))]
        ui.run()?;

        #[cfg(feature = "systray")]
        if want_systray {
            // Tray mode hides on close, except while the port-conflict dialog is
            // up: the app hasn't committed to running yet, so quit instead.
            ui.window().on_close_requested({
                let ui_weak = ui.as_weak();
                move || {
                    let resolving = ui_weak
                        .upgrade()
                        .is_some_and(|ui| ui.global::<Bridge>().get_show_port_conflict());
                    if resolving {
                        let _ = slint::quit_event_loop();
                    }
                    slint::CloseRequestResponse::HideWindow
                }
            });

            if !no_main_window {
                ui.show()?;
            }
            slint::run_event_loop_until_quit()?;
        } else {
            ui.run()?;
        }

        info!("Shutting down...");

        RUNTIME.block_on(async move {
            msg_tx.send(Message::Quit);
            let _ = fin_rx.await;
        });
    } else {
        info!(initialized_in = ?start.elapsed());
        RUNTIME.block_on(async move {
            if let Err(err) = event_loop_jh.await {
                error!(?err, "Failed to join event loop task");
            }
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::overlay_change_applies;
    use fcast_video::cue::{CueEngine, CueInput, TextFormat};

    /// An engine with its change notification raised, by the route the paused
    /// path actually uses: a frame has been shown (so the engine has a running
    /// time to schedule against) and a cue covering it arrives afterwards.
    /// That is `CueEngine::submit`'s publish-side evaluate, which is what a
    /// post-seek cue does while no further frame is coming.
    fn dirty_engine() -> CueEngine {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        // Establishes `last_shown_rt`, the way a preroll frame does.
        engine.overlays_for(Some(gst::ClockTime::ZERO));
        let _ = engine.take_dirty();
        engine.submit(CueInput {
            format: TextFormat::Utf8,
            text: "SUBTITLE".to_owned(),
            start_rt: gst::ClockTime::ZERO,
            end_rt: Some(gst::ClockTime::from_seconds(2)),
        });
        engine
    }

    /// The helper's own premise: that route really does raise the bit. If this
    /// fails the others prove nothing.
    #[test]
    fn the_paused_submit_raises_the_change_notification() {
        let engine = dirty_engine();
        assert!(
            engine.take_dirty(),
            "a cue submitted against a shown frame must report a change; without one \
             there is nothing for the repaint to be triggered by"
        );
    }

    /// THE BUG, stated as a test: a pass with no frame must not eat the bit.
    /// It used to, and while PAUSED nothing raises it again.
    #[test]
    fn a_pass_with_no_frame_leaves_the_notification_up() {
        let engine = dirty_engine();
        assert!(!overlay_change_applies(false, &engine));
        assert!(
            overlay_change_applies(true, &engine),
            "the notification was consumed by a pass that had no frame to apply it to, \
             so the cue is lost until something else happens to raise it -- and while \
             paused nothing will"
        );
    }

    /// The ordinary case: a frame is cached and the engine has something new.
    #[test]
    fn a_pass_with_a_frame_applies_and_consumes_it() {
        let engine = dirty_engine();
        assert!(overlay_change_applies(true, &engine));
        assert!(
            !overlay_change_applies(true, &engine),
            "the notification is one-shot; a second pass has nothing to apply"
        );
    }

    /// A quiet engine asks for nothing, frame or no frame.
    #[test]
    fn a_quiet_engine_asks_for_no_repaint() {
        let engine = dirty_engine();
        assert!(overlay_change_applies(true, &engine));
        assert!(!overlay_change_applies(true, &engine));
        assert!(!overlay_change_applies(false, &engine));
    }

    /// The paused-seek interleaving, both orders. The cue's notification can
    /// arrive before the post-seek preroll frame is folded or after it, and
    /// either way exactly one pass must come away with something to draw.
    #[test]
    fn either_seek_ordering_still_paints_once() {
        // Notification first, frame second -- the order that used to lose it.
        let engine = dirty_engine();
        assert!(!overlay_change_applies(false, &engine), "no frame yet");
        assert!(overlay_change_applies(true, &engine), "the frame arrives");
        assert!(!overlay_change_applies(true, &engine), "and only once");

        // Frame first, notification second.
        let engine = dirty_engine();
        assert!(overlay_change_applies(true, &engine));
        assert!(!overlay_change_applies(true, &engine));
    }
}
