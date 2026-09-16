//! The receiver's user interface: the slint window, the render loop, and the
//! consumer half of the GUI command channel.
//!
//! Everything that is not the UI lives in `receiver-core`, which this crate
//! re-exports wholesale, so a receiver binary depends only on this one. Keeping
//! the split this way round is what makes `cargo test -p receiver-core` free of
//! slint (and of compiling the `.slint` sources).

// The `Send`/`Sync` solver walks wgpu's whole context graph to answer for the
// one `OnceLock<SharedDevice>` the lane keeps, and that walk is deeper than
// the default 128. Overrunning it is a future hard error (rust#159228), not a
// style warning, so the limit is raised here rather than left to fire.
#![recursion_limit = "256"]

// Forces the static GStreamer link line and isolates the process from on-disk
// plugins before main.
use gst_static_env as _;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

#[cfg(all(not(target_os = "android"), feature = "systray"))]
use std::cell::RefCell;
#[cfg(not(target_os = "android"))]
use std::{rc::Rc, sync::Arc, time::Duration};

pub use slint;

pub use receiver_core::*;
// The rest arrives through the `receiver_core::*` glob above.
use receiver_core::{gui::GuiController, message::Message};

slint::include_modules!();

mod video_math;

/// The android AHardwareBuffer lane's gating, kept off the FFI so it can be
/// tested on a host with no NDK.
#[cfg(any(target_os = "android", test))]
mod ahb_plan;

/// A gralloc-backed allocator and pool proposed to android's software
/// decoders, so their frames arrive as memory the GPU can sample instead of
/// as pixels the bridge has to convert and upload.
#[cfg(target_os = "android")]
mod android_ahb;
/// The other half of that: AHardwareBuffer to GL texture through EGL, with
/// no renderer type anywhere in it.
#[cfg(target_os = "android")]
mod android_ahb_gl;
#[cfg(target_os = "android")]
mod android_immersive;

/// Activity start/stop from the entry crate. Detaches the video surface
/// from the player before android destroys it and re-adopts a fresh one on
/// return, see android_surface_video.rs.
#[cfg(target_os = "android")]
pub fn android_app_visibility(visible: bool) {
    android_surface_video::app_visibility(visible);
}
#[cfg(target_os = "android")]
mod android_subtitles;
#[cfg(target_os = "android")]
mod android_surface_video;
#[cfg(target_os = "android")]
mod android_video;
/// Bitmap subtitles on the desktop lane, which puts its video in the scene
/// and so has to put the decoded regions in the scene too.
#[cfg(not(target_os = "android"))]
mod bitmap_overlay;
/// Subtitle cues on the desktop lane: the engine's display lists, translated
/// into the dodvg renderer's own scene type.
#[cfg(all(not(target_os = "android"), feature = "scene-cues"))]
mod cue_overlay;
/// Zero-copy import of the decoder's dmabuf planes for the lane below.
#[cfg(target_os = "linux")]
mod desktop_wgpu_dmabuf;
/// The mac counterpart: VideoToolbox's frames are IOSurfaces, imported as
/// metal textures instead of mapped and uploaded.
#[cfg(target_os = "macos")]
mod desktop_wgpu_iosurface;
/// The other half of that import: a udmabuf pool proposed to software
/// decoders, so their frames arrive as dmabufs too instead of as sysmem the
/// lane has to upload.
#[cfg(target_os = "linux")]
mod desktop_wgpu_udmabuf;
/// Desktop presentation through i-slint-video-wgpu.
#[cfg(not(target_os = "android"))]
mod desktop_wgpu_video;

/// Puts slint's renderer on the same wgpu device the video lane presents
/// with, so decoded frames reach the scene as textures instead of a readback.
/// Must run before any slint window exists, which is why the binary calls it
/// where it picks a backend.
///
/// False when no gpu device could be built or slint refused the selection.
/// The caller then makes its usual OpenGL selection and the receiver runs
/// with no video sink at all.
#[cfg(not(target_os = "android"))]
pub fn select_wgpu_video_backend() -> bool {
    let Some(shared) = desktop_wgpu_video::create_shared_device() else {
        return false;
    };
    // A device opened here on a native API is handed to slint as it is. The
    // GL floor is different: a GL instance presents only on the display it
    // was opened with, which is the window's and does not exist yet, so slint
    // opens that device itself, asked for GL by name, and the lane adopts it
    // at the first RenderingSetup. The startup device only proved the
    // adapter and goes.
    let adapter = shared
        .adapter
        .clone()
        .filter(|_| !desktop_wgpu_video::is_gl_floor(&shared));
    let (config, adopt_later) = match adapter {
        Some(adapter) => (
            slint::wgpu_30::WGPUConfiguration::Manual {
                instance: shared.instance.clone(),
                adapter,
                device: shared.device.clone(),
                queue: shared.queue.clone(),
            },
            false,
        ),
        None => (
            slint::wgpu_30::WGPUConfiguration::Automatic(desktop_wgpu_video::gl_floor_settings(
                &shared,
            )),
            true,
        ),
    };
    // Named, not left to the backend's default order: dodvg's OpenGL lane is
    // compiled in too and wins that order, so the wgpu one has to be asked for.
    match slint::BackendSelector::new()
        .renderer_name("dodvg-wgpu".into())
        .require_wgpu_30(config)
        .select()
    {
        // Nothing is logged here either way: this runs before the subscriber
        // is installed. The sink reports which lane it got.
        Ok(()) if adopt_later => {
            drop(shared);
            desktop_wgpu_video::expect_renderer_device();
            true
        }
        Ok(()) => {
            desktop_wgpu_video::adopt_shared_device(shared);
            true
        }
        Err(err) => {
            // No log subscriber exists yet, so hand the reason to the sink,
            // which reports it once there is one.
            desktop_wgpu_video::note_shared_device_refused(err.to_string());
            false
        }
    }
}
pub mod gui;
pub mod scaling;

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// Run the main app. Slint is assumed to be initialized by the platform
/// specific target.
#[cfg(not(target_os = "android"))]
pub fn run(settings: Settings) -> Result<()> {
    let start = std::time::Instant::now();

    receiver_core::tune_allocator();
    receiver_core::allow_ptrace_attach();

    logging::init(settings.log_level());

    if let Err(err) = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default() {
        error!(
            ?err,
            "Failed to register aws-lc-rs as rustls default crypto provider"
        );
    }

    let (msg_tx, event_rx) = mpsc::unbounded_channel::<Message>();
    let msg_tx = MessageSender::new(msg_tx);
    let (fin_tx, fin_rx) = tokio::sync::oneshot::channel::<()>();

    let is_headless = settings.headless();

    // The video lane's cue geometry tick. Built with the sink, on the event
    // loop task, and read by the rendering notifier, which is the only thing
    // that runs on a resize with no frame behind it.
    let cues = Arc::new(parking_lot::Mutex::new(None::<desktop_wgpu_video::CueTick>));
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
    if let Some(ui) = &ui {
        #[cfg(debug_assertions)]
        ui.global::<Bridge>().set_is_debugging(true);

        // Owned by the window from here on: the geometry callback holds the
        // only strong reference, and it holds the window weakly.
        let _ui_scaler = scaling::install(ui, settings.ui_scale(), settings.ui_scale_forced());

        ui.window().set_rendering_notifier({
            let ui_weak = ui.as_weak();
            let mut start_fullscreen = Some(settings.fullscreen());
            let msg_tx = msg_tx.clone();
            let gui_is_visible = gui_is_visible.clone();
            let cues = Arc::clone(&cues);
            let mut cue_tick: Option<desktop_wgpu_video::CueTick> = None;
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    debug!("Got graphics API: {graphics_api:?}");
                    // The GL floor's device is slint's to open, on the window's
                    // display (see `select_wgpu_video_backend`), and this is
                    // where it is handed over. A no-op on the other backends.
                    if let slint::GraphicsAPI::WGPU30 {
                        instance,
                        device,
                        queue,
                        ..
                    } = &graphics_api
                    {
                        desktop_wgpu_video::adopt_renderer_device(instance, device, queue);
                    }
                    let Some(ui) = ui_weak.upgrade() else {
                        error!("Failed to upgrade ui");
                        return;
                    };
                    if let Some(fullscreen) = start_fullscreen.take() {
                        ui.window().set_fullscreen(fullscreen);
                    }
                    // Where the cue overlay goes in the item tree: right
                    // after the marker rectangle the player view paints with
                    // this color, which is above the video and below the
                    // controls. Named once, from the single definition in
                    // globals.slint.
                    #[cfg(feature = "scene-cues")]
                    {
                        use slint::winit_030::DodvgWindowAccessor;
                        let anchor = ui.global::<Bridge>().get_cue_anchor();
                        let dodvg = ui
                            .window()
                            .with_dodvg_renderer(|r| r.set_cue_anchor(Some(anchor)))
                            .is_some();
                        debug!(dodvg, "cue overlay slot");
                    }
                    gui_is_visible.set(true);
                }
                slint::RenderingState::BeforeRendering => {
                    let Some(ui) = ui_weak.upgrade() else {
                        error!("Failed to upgrade ui");
                        return;
                    };
                    // The lane's whole cue path hangs off its appsink, so a
                    // resize with no frame behind it (paused, or between two
                    // frames) reaches the engine nowhere else. This runs on
                    // every render pass and costs a compare when the window
                    // did not move.
                    if cue_tick.is_none() {
                        cue_tick = cues.lock().clone();
                    }
                    if let Some(tick) = cue_tick.as_ref() {
                        tick.on_render(&ui);
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
                    // The overlay's scene pool outlives the renderer that
                    // holds the other half of it otherwise.
                    cue_tick = None;
                    cues.lock().take();
                }
                _ => (),
            }
        })?;

        ui.global::<Bridge>().on_inspector_toggled({
            let ui_weak = ui.as_weak();
            let msg_tx = msg_tx.clone();
            move |active| {
                msg_tx.send(Message::InspectorActive(active));
                if let Some(ui) = ui_weak.upgrade() {
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

        gui::spawn_command_handler(ui.as_weak(), gui_rx, on_show_tray);
        Some(gui_tx)
    } else {
        None
    };

    let gui = GuiController::new(gui_tx, gui_is_visible.clone());

    #[allow(unused_variables)]
    #[cfg(not(target_os = "android"))]
    let no_main_window = settings.no_main_window();
    // The lane fixes its quality knobs at startup off the resolved profile.
    let render_profile = settings.render_profile();
    let event_loop_jh = RUNTIME.spawn({
        let ui_weak = ui.as_ref().map(|ui| ui.as_weak());
        let msg_tx = msg_tx.clone();
        let cues = Arc::clone(&cues);
        async move {
            gstreamer::init_and_load_plugins();

            // On the GL floor the presenting device is slint's to open, at its
            // first render setup, so the sink waits for the handover instead
            // of declining before the device exists. Immediate on every other
            // backend, and bounded for a window that never comes up.
            desktop_wgpu_video::await_renderer_device(Duration::from_secs(5)).await;

            // The lane owns presentation end to end: its appsink renders each
            // frame and pushes a slint image. The engine is built beside it
            // because the lane owns its geometry: the canvas and the picture
            // rect have to be right from the first frame or every raster it
            // builds is keyed to a stale one. No device means no video sink,
            // and the player plays sound only.
            let (video_sink_elem, cue_engine) = match ui_weak {
                Some(ui) => {
                    let engine = fcast_video::cue::CueEngine::new();
                    match desktop_wgpu_video::make_sink(ui, engine.clone(), render_profile) {
                        Some((sink, cue_tick)) => {
                            *cues.lock() = Some(cue_tick);
                            (Some(sink), Some(engine))
                        }
                        None => {
                            error!("no gpu device for the video lane, playing without video");
                            (None, None)
                        }
                    }
                }
                None => (None, None),
            };

            let app =
                application::Application::new(gui, video_sink_elem, cue_engine, msg_tx, settings)
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
            // The finished signal is sent from inside the task, before the
            // application (and with it the player, the pipeline and the video
            // sink's GPU resources) is dropped at the task's end. Returning on
            // the signal alone let main exit under that drop, and on the GL
            // floor the loader's exit-time teardown then pulled the EGL
            // display out from under it: a panic in a worker at every quit.
            if let Err(err) = event_loop_jh.await {
                error!(?err, "Failed to join event loop task");
            }
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

/// Run the app on android: the slint UI and the receiver application,
/// without the desktop render loop. The player and the audio path are fully
/// live; video decodes headless until the direct-surface integration lands.
/// Platform events (mdns name, network changes, raop config) arrive from
/// the activity's JNI bridges.
#[cfg(target_os = "android")]
pub fn run(
    android_app: slint::android::AndroidApp,
    mut platform_event_rx: mpsc::UnboundedReceiver<Message>,
) -> Result<()> {
    let start = std::time::Instant::now();

    receiver_core::tune_allocator();
    receiver_core::allow_ptrace_attach();
    // Installs the panic hook and the gst log integration; no fmt
    // subscriber on android, tracing's `log` bridge forwards everything to
    // the android logger the activity installed.
    logging::init(None);
    receiver_core::install_default_crypto_provider();

    let (msg_tx, event_rx) = mpsc::unbounded_channel::<Message>();
    let msg_tx = MessageSender::new(msg_tx);
    let (fin_tx, fin_rx) = tokio::sync::oneshot::channel::<()>();

    RUNTIME.spawn({
        let msg_tx = msg_tx.clone();
        async move {
            while let Some(event) = platform_event_rx.recv().await {
                msg_tx.send(event);
            }
            debug!("Platform event proxy finished");
        }
    });

    let ui = MainWindow::new()?;
    // TV means dpad-first: controls must not auto-hide away from a focused
    // remote, so touch mode stays off there.
    ui.global::<Bridge>()
        .set_touch_mode(!android_immersive::is_television());
    ui.global::<Bridge>()
        .set_tv_mode(android_immersive::is_television());
    // the hole punch is an android constant, whatever the input style
    ui.global::<Bridge>().set_behind_window_video(true);
    ui.global::<Bridge>().set_android(true);
    android_immersive::init(&android_app);
    // starts with system bars, the player's toggle enters immersive
    ui.global::<Bridge>().set_is_fullscreen(false);
    let gui_is_visible = gui::GuiIsVisible::new();
    // one window, no tray: visible for the app's whole life
    gui_is_visible.set(true);

    let (gui_tx, gui_rx) = mpsc::unbounded_channel();
    {
        // no renderer thread on android, the dropped receiver makes the
        // handler's renderer sends no-ops
        gui::spawn_command_handler(ui.as_weak(), gui_rx, Box::new(|| {}));
    }
    let gui = GuiController::new(Some(gui_tx), gui_is_visible);

    // GStreamer registration is ~800ms and the idle screen needs none of it,
    // only the video sink does and that waits for the first cast. Register on a
    // worker so ui.run() paints the idle screen immediately, then finish the
    // sink and receiver wiring back on the event-loop thread. The desktop lane
    // already backgrounds this (see the non-android run); android was the last
    // one doing it inline on the UI thread. The sink and Application still come
    // up at the same wall-clock moment as before (~800ms in), so port binding
    // and cast handling are unchanged, only the first frame moves earlier.
    let use_sw_video = std::env::var("FCAST_ANDROID_SW_VIDEO").is_ok_and(|v| v == "1");
    {
        let ui_weak = ui.as_weak();
        let msg_tx = msg_tx.clone();
        std::thread::Builder::new()
            .name("gst-init".to_owned())
            .spawn(move || {
                // Registration unwraps throughout; on this detached worker a
                // device-specific failure would die silently and leave the
                // receiver at the idle screen with no sink and no bound port,
                // undiscoverable with every cast failing. Catch it and quit so
                // the failure is loud, the way it was when this ran inline on
                // the event-loop thread.
                if std::panic::catch_unwind(gstreamer::init_and_load_plugins).is_err() {
                    error!("gstreamer registration failed, the receiver cannot start");
                    let _ = slint::quit_event_loop();
                    return;
                }
                // The sink setup and callback registration touch the live ui,
                // so finish on the event-loop thread. The sink itself only
                // needs a weak ui and marshals its own ui work; the idle screen
                // is opaque so the video SurfaceView going up behind it now is
                // invisible until the first cast punches the hole.
                let finish = move || {
                    let Some(ui) = ui_weak.upgrade() else { return };

                    // Zero-copy surface video by default (see
                    // android_surface_video.rs); FCAST_ANDROID_SW_VIDEO=1
                    // selects the software bridge instead.
                    let mut surface_video = None;
                    let video_sink = if use_sw_video {
                        android_video::make_sink(&ui)
                    } else {
                        match android_surface_video::SurfaceVideo::setup(&ui, &android_app) {
                            Some((sv, sink)) => {
                                surface_video = Some(sv);
                                sink
                            }
                            None => android_video::make_sink(&ui),
                        }
                    };

                    // Subtitles: the engine's cues render as slint overlays
                    // above the video hole, driven off the video sink.
                    let cue_engine = fcast_video::cue::CueEngine::new();
                    let subtitles =
                        android_subtitles::attach(cue_engine.clone(), &video_sink, &ui);

                    // A fullscreen toggle resizes the window; re-fit the video
                    // rect and the cue canvas together or they drift apart by
                    // the inset delta.
                    {
                        let ui_weak = ui.as_weak();
                        ui.global::<Bridge>().on_window_geometry_changed(move || {
                            if let Some(surface_video) = &surface_video {
                                surface_video.relayout(&ui_weak);
                            }
                            if let Some(ui) = ui_weak.upgrade() {
                                android_subtitles::resync(&subtitles, &ui);
                            }
                        });
                    }

                    RUNTIME.spawn(async move {
                        let app = application::Application::new(
                            gui,
                            Some(video_sink),
                            Some(cue_engine),
                            msg_tx,
                            android_app,
                        )
                        .await;

                        // Detached: fail visibly and quit rather than leave the
                        // slint loop running with no protocol handling behind it.
                        let result = match app {
                            Ok(app) => app.run_event_loop(event_rx, fin_tx).await,
                            Err(err) => Err(err),
                        };
                        if let Err(err) = result {
                            error!(?err, "Receiver event loop failed");
                            let _ = slint::quit_event_loop();
                        }
                    });
                };
                if slint::invoke_from_event_loop(finish).is_err() {
                    error!("event loop ended before gstreamer finished loading");
                }
            })
            .expect("spawning the gst-init thread");
    }

    gui::register_callbacks(&ui, msg_tx.clone());
    info!(initialized_in = ?start.elapsed());
    ui.run()?;

    info!("Shutting down...");
    RUNTIME.block_on(async move {
        msg_tx.send(Message::Quit);
        // Bounded: a quit inside the gst-init window means the Application task
        // never started, so fin_tx is stranded in the abandoned android event
        // queue and would never fire. Cap the wait rather than hang the thread.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fin_rx).await;
    });

    Ok(())
}

#[cfg(test)]
mod tests {
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

    /// A cue submitted against a shown frame reports a change. Without one
    /// there is nothing for the paused repaint to be triggered by: the lane's
    /// `set_on_change` hook fires off this same bit.
    #[test]
    fn the_paused_submit_raises_the_change_notification() {
        let engine = dirty_engine();
        assert!(engine.take_dirty());
    }
}
