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

// One of the two lanes has to be named, and `default = []` names neither: a
// build with no lane has no slint backend, so the generated code's
// `load_image_from_embedded_data` (i-slint-core's `image-decoders`, which
// arrives with a backend) is missing, and `element_cues` is compiled while the
// `cue_overlay` it hops through is behind `scene-cues`. Both surface as
// screenfuls of errors that name neither cause, so say it here instead.
#[cfg(not(any(feature = "desktop", feature = "android", target_os = "android")))]
compile_error!(
    "receiver-ui needs one of its lane features: `desktop` or `android`. \
     `cargo check -p receiver-ui --features desktop` is the desktop one."
);

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

#[cfg(target_os = "android")]
mod android_immersive;

/// Activity start/stop from the entry crate. Detaches the video surface
/// from the player before android destroys it and re-adopts a fresh one on
/// return, see android_surface_video.rs.
#[cfg(target_os = "android")]
pub fn android_app_visibility(visible: bool) {
    android_surface_video::app_visibility(visible);
}
/// The overlay grant and whether a settings page for it exists, from the
/// activity on every focus gain.
#[cfg(target_os = "android")]
pub fn android_overlay_state(granted: bool, can_open_settings: bool) {
    android_overlay::set_state(granted, can_open_settings);
}
#[cfg(target_os = "android")]
mod android_overlay;
#[cfg(target_os = "android")]
mod android_ui;
#[cfg(any(test, target_os = "android"))]
mod android_insets;
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
/// A counting allocator, so a test can put a number on what one frame costs.
#[cfg(test)]
mod alloc_counter;
/// The inspector's stream card, read off the element's stats.
#[cfg(not(target_os = "android"))]
mod element_card;
/// The cue overlay, driven from the element's presented callback.
#[cfg(not(target_os = "android"))]
mod element_cues;
/// Desktop presentation on the shared `slintvideosink`.
#[cfg(not(target_os = "android"))]
mod element_video;

/// What the rendering notifier ticks so a resize with no frame behind it still
/// re-anchors the cues.
#[cfg(not(target_os = "android"))]
type CueTickHandle = std::sync::Arc<element_cues::ElementCues>;

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
    element_video::select_backend()
}

pub mod gui;
pub mod scaling;

/// Set when the event loop ends in an error, so the process exits non-zero
/// and a service manager's restart-on-failure sees it.
#[cfg(not(target_os = "android"))]
static EVENT_LOOP_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The receiver's event loop task returned or panicked. Either way nothing
/// handles protocol traffic any more, so the window quits instead of staying
/// up with nothing behind it. A clean return is the quit path itself.
fn event_loop_ended(result: std::result::Result<Result<()>, tokio::task::JoinError>) {
    #[cfg(target_os = "android")]
    {
        ANDROID_CORE_ENDED.store(true, std::sync::atomic::Ordering::Release);
        // no window to quit, nothing else would end the process. The sticky
        // service is left running on purpose so the system restarts it
        if !matches!(result, Ok(Ok(()))) && android_ui::current().is_none() {
            error!(?result, "Receiver event loop ended with no UI up, exiting");
            android_end_process(1);
        }
    }
    #[cfg(not(target_os = "android"))]
    if !matches!(result, Ok(Ok(()))) {
        EVENT_LOOP_FAILED.store(true, std::sync::atomic::Ordering::Release);
    }
    match result {
        Ok(Ok(())) => return,
        Ok(Err(err)) => error!(?err, "Receiver event loop failed"),
        Err(err) if err.is_panic() => error!(?err, "Receiver event loop panicked"),
        Err(err) => error!(?err, "Receiver event loop was cancelled"),
    }
    let _ = slint::quit_event_loop();
}

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
    let cues = Arc::new(parking_lot::Mutex::new(None::<CueTickHandle>));
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
            let mut start_fullscreen = settings.fullscreen();
            let msg_tx = msg_tx.clone();
            let gui_is_visible = gui_is_visible.clone();
            let cues = Arc::clone(&cues);
            let mut cue_tick: Option<CueTickHandle> = None;
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    debug!("Got graphics API: {graphics_api:?}");
                    // The GL floor's device is slint's to open, on the window's
                    // display (see `select_wgpu_video_backend`), and this is
                    // where it is handed over. A no-op on the other backends.
                    element_video::adopt(state, &graphics_api);
                    let Some(ui) = ui_weak.upgrade() else {
                        error!("Failed to upgrade ui");
                        return;
                    };
                    // Only ever a set: a first cast's fullscreen can land before
                    // this first setup, and a startup "false" undid it.
                    if std::mem::take(&mut start_fullscreen) {
                        ui.window().set_fullscreen(true);
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
                    // The GUI thread's own count: a show the application has
                    // asked for but this thread has not carried out is queued
                    // behind this teardown and ends the wait below.
                    let shows = gui_is_visible.shows_processed();
                    msg_tx.send(Message::GuiWindowClosed {
                        shows,
                        feedback: feedback_tx,
                    });
                    match gui::await_player_release(
                        &feedback_rx,
                        &gui_is_visible,
                        shows,
                        Duration::from_millis(2500),
                    ) {
                        gui::TeardownWait::Released => debug!("Player released"),
                        gui::TeardownWait::ShowRequested => {
                            debug!("Window shown again during its teardown, not waiting on the player")
                        }
                        gui::TeardownWait::TimedOut => {
                            error!("Timed out waiting for the player to let go")
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

        #[cfg(not(target_os = "android"))]
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
                    let msg_tx = msg_tx.clone();
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
                            let msg_tx = msg_tx.clone();
                            move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let win = ui.window();
                                    if win.is_visible() {
                                        // a hide, not a close request: the
                                        // core hears of it here
                                        msg_tx.send(Message::GuiWindowHidden);
                                        let _ = win.hide();
                                    } else {
                                        msg_tx.send(Message::GuiWindowShown);
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

        gui::spawn_command_handler(ui.as_weak(), gui_rx, gui_is_visible.clone(), on_show_tray);
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
            let _ = element_video::await_device(Duration::from_secs(5)).await;

            // The lane owns presentation end to end: its appsink renders each
            // frame and pushes a slint image. The engine is built beside it
            // because the lane owns its geometry: the canvas and the picture
            // rect have to be right from the first frame or every raster it
            // builds is keyed to a stale one. No device means no video sink,
            // and the player plays sound only.
            let (video_sink_elem, cue_engine) = match ui_weak {
                Some(ui) => {
                    let engine = fcast_video::cue::CueEngine::new();
                    let built = element_video::make_sink(ui, engine.clone(), render_profile.into())
                        .map(|(sink, tick)| (sink, Some(tick)));
                    match built {
                        Ok((sink, cue_tick)) => {
                            *cues.lock() = cue_tick;
                            (Some(sink), Some(engine))
                        }
                        Err(err) => {
                            error!(%err, "playing without video");
                            (None, None)
                        }
                    }
                }
                None => (None, None),
            };

            // Its own task, awaited: a panic in it must quit too, not leave the
            // Slint loop running a UI with no protocol handling behind it.
            let run = tokio::spawn(async move {
                let app = application::Application::new(
                    gui,
                    video_sink_elem,
                    cue_engine,
                    msg_tx,
                    settings,
                )
                .await?;
                app.run_event_loop(event_rx, fin_tx).await
            });
            event_loop_ended(run.await);
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
                let msg_tx = msg_tx.clone();
                move || {
                    let resolving = ui_weak
                        .upgrade()
                        .is_some_and(|ui| ui.global::<Bridge>().get_show_port_conflict());
                    if resolving {
                        let _ = slint::quit_event_loop();
                    } else {
                        // the core keeps the window hidden past the cast's end
                        msg_tx.send(Message::GuiWindowHidden);
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

    if EVENT_LOOP_FAILED.load(std::sync::atomic::Ordering::Acquire) {
        anyhow::bail!("the receiver event loop failed");
    }
    Ok(())
}

/// What every UI attach needs from the running core.
#[cfg(target_os = "android")]
struct AndroidCore {
    msg_tx: MessageSender,
    gui_is_visible: gui::GuiIsVisible,
    fin_rx: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

/// The core's media half, set once gst registration finished.
#[cfg(target_os = "android")]
struct AndroidMedia {
    subtitles: android_subtitles::Subtitles,
    surface_video: bool,
}

#[cfg(target_os = "android")]
static ANDROID_CORE: std::sync::OnceLock<AndroidCore> = std::sync::OnceLock::new();
#[cfg(target_os = "android")]
static ANDROID_MEDIA: std::sync::OnceLock<AndroidMedia> = std::sync::OnceLock::new();
/// The Application task returned or died, a kept-alive process has no core.
#[cfg(target_os = "android")]
static ANDROID_CORE_ENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Runs once, on the first frame a UI renders. The renderer probe in
/// receiver-android marks its backend as working here: a frame on screen is
/// the one proof a driver got through device, pipeline and present.
#[cfg(target_os = "android")]
static ANDROID_FIRST_FRAME: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

/// Registers the first-frame hook, replacing an unfired one.
#[cfg(target_os = "android")]
pub fn android_on_first_frame(f: impl FnOnce() + Send + 'static) {
    *ANDROID_FIRST_FRAME.lock().unwrap() = Some(Box::new(f));
}

/// `_exit`, not `exit`: static destructors run under MediaCodec threads still
/// releasing, a crash on the way out that hides the real cause. The sticky
/// service stays so the system restarts the receiver.
#[cfg(target_os = "android")]
fn android_end_process(code: i32) -> ! {
    unsafe { libc::_exit(code) }
}

/// Starts the receiver once per process: the application, gst and the
/// player's sinks. It owns no window, UIs attach to it (ANDROID-BOOT-START-PLAN.md).
#[cfg(target_os = "android")]
fn start_core(
    platform_event_rx: mpsc::UnboundedReceiver<Message>,
    settings: Settings,
) -> &'static AndroidCore {
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
        let mut platform_event_rx = platform_event_rx;
        async move {
            while let Some(event) = platform_event_rx.recv().await {
                msg_tx.send(event);
            }
            debug!("Platform event proxy finished");
        }
    });

    // Visible only while a UI is attached, GuiController::attach flips it.
    let gui_is_visible = gui::GuiIsVisible::new();
    let gui = GuiController::new(None, gui_is_visible.clone())
        .with_replay()
        // an item boundary drops the old item's cues, on screen and in the
        // engine, with or without a UI
        .with_app_state_hook(|state| {
            use receiver_core::ui_types::AppState;
            if matches!(state, AppState::LoadingMedia | AppState::Idle) {
                android_subtitles::clear_current();
            }
        })
        .with_item_hook(android_surface_video::item_boundary);

    // GStreamer registration is ~800ms and the idle screen needs none of it,
    // so it runs on a worker and the first frame never waits for it.
    let use_sw_video = std::env::var("FCAST_ANDROID_SW_VIDEO").is_ok_and(|v| v == "1");
    let core_msg_tx = msg_tx.clone();
    std::thread::Builder::new()
        .name("gst-init".to_owned())
        .spawn(move || {
            // Registration unwraps throughout; on this detached worker a
            // device-specific failure would die silently and leave the
            // receiver undiscoverable with every cast failing. Catch it and
            // quit so the failure is loud.
            if std::panic::catch_unwind(gstreamer::init_and_load_plugins).is_err() {
                error!("gstreamer registration failed, the receiver cannot start");
                ANDROID_CORE_ENDED.store(true, std::sync::atomic::Ordering::Release);
                // A UI quits through its loop. Without one (a boot start) the
                // sticky service would keep an empty process up for good.
                if slint::quit_event_loop().is_err() || android_ui::current().is_none() {
                    android_end_process(1);
                }
                return;
            }
            // Zero-copy surface video by default (android_surface_video.rs),
            // FCAST_ANDROID_SW_VIDEO=1 selects the software bridge.
            let surface_sink = (!use_sw_video)
                .then(android_surface_video::make_sink)
                .flatten();
            let surface_video = surface_sink.is_some();
            let video_sink = surface_sink.unwrap_or_else(android_video::make_sink);

            // Subtitles: the engine's cues render as slint overlays above the
            // video hole, driven off the video sink.
            let cue_engine = fcast_video::cue::CueEngine::new();
            let subtitles = android_subtitles::install(cue_engine.clone(), &video_sink);
            let _ = ANDROID_MEDIA.set(AndroidMedia {
                subtitles,
                surface_video,
            });
            // A UI already up gets its view and cue styling on its own thread.
            let _ = slint::invoke_from_event_loop(attach_media);

            RUNTIME.spawn(async move {
                // awaited, so a panic quits too, see event_loop_ended
                let run = RUNTIME.spawn(async move {
                    let app = application::Application::new(
                        gui,
                        Some(video_sink),
                        Some(cue_engine),
                        core_msg_tx,
                        settings,
                    )
                    .await?;
                    app.run_event_loop(event_rx, fin_tx).await
                });
                event_loop_ended(run.await);
            });
        })
        .expect("spawning the gst-init thread");

    let _ = ANDROID_CORE.set(AndroidCore {
        msg_tx,
        gui_is_visible,
        fin_rx: std::sync::Mutex::new(Some(fin_rx)),
    });
    ANDROID_CORE.get().expect("just set")
}

/// The attached UI's media half: its video view and the cue styling for its
/// window. Runs on the UI thread, once per attach, whichever of the attach
/// and the end of gst registration comes second.
#[cfg(target_os = "android")]
fn attach_media() {
    thread_local! {
        static DONE_FOR: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    let (Some(media), Some(generation), Some(ui)) = (
        ANDROID_MEDIA.get(),
        android_ui::generation(),
        android_ui::current().and_then(|ui| ui.upgrade()),
    ) else {
        return;
    };
    if DONE_FOR.get() == generation {
        return;
    }
    DONE_FOR.set(generation);
    if media.surface_video {
        if let Some(app) = android_ui::app() {
            if android_surface_video::SurfaceVideo::attach(&ui, &app).is_none() {
                tracing::warn!("no video surface for this UI, video stays headless");
            }
        }
    }
    android_subtitles::attach(&media.subtitles, &ui);
}

/// Builds a UI for this activity and attaches it to the core.
#[cfg(target_os = "android")]
fn attach_ui(android_app: &slint::android::AndroidApp, core: &AndroidCore) -> Result<(MainWindow, u64)> {
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
    android_immersive::init(android_app);
    let generation = android_ui::attach(&ui, android_app);
    android_overlay::init(&ui);
    // The splash retires on the first rendered frame (the opaque startup art)
    // instead of a timer, see SplashActivity.retire.
    {
        let mut reported = false;
        let notifier = ui.window().set_rendering_notifier(move |state, _| {
            if reported || !matches!(state, slint::RenderingState::AfterRendering) {
                return;
            }
            reported = true;
            android_immersive::with_activity("onReceiverPainted", |env, activity| {
                env.call_method(activity, "onReceiverPainted", "()V", &[])
                    .map(drop)
            });
            if let Some(hook) = ANDROID_FIRST_FRAME.lock().unwrap().take() {
                hook();
            }
        });
        if let Err(err) = notifier {
            tracing::warn!(?err, "no rendering notifier, the splash retires on its backstop");
        }
    }
    // starts with system bars, the player's toggle enters immersive
    ui.global::<Bridge>().set_is_fullscreen(false);

    // A fullscreen toggle resizes the window; re-fit the video rect and the
    // cue canvas together or they drift apart by the inset delta.
    {
        let ui_weak = ui.as_weak();
        ui.global::<Bridge>().on_window_geometry_changed(move || {
            android_surface_video::relayout_current();
            if let (Some(media), Some(ui)) = (ANDROID_MEDIA.get(), ui_weak.upgrade()) {
                android_subtitles::resync(&media.subtitles, &ui);
            }
        });
    }

    let (gui_tx, gui_rx) = mpsc::unbounded_channel();
    // no renderer thread on android, the dropped receiver makes the
    // handler's renderer sends no-ops
    gui::spawn_command_handler(ui.as_weak(), gui_rx, core.gui_is_visible.clone(), Box::new(|| {}));
    core.msg_tx.send(Message::GuiAttached {
        tx: gui_tx,
        generation,
    });
    gui::register_callbacks(&ui, core.msg_tx.clone());
    // the core may have finished its media half before this UI existed
    attach_media();
    Ok((ui, generation))
}

/// The activity's UI went away. The core keeps running without one.
#[cfg(target_os = "android")]
fn detach_ui(core: &AndroidCore, generation: u64) {
    core.msg_tx.send(Message::GuiDetached { generation });
    android_surface_video::detach_view();
    android_ui::detach(generation);
    // the activity jobject dies with this UI
    android_immersive::clear();
}

/// Quits the receiver and waits for it, bounded: a quit inside the gst-init
/// window has no Application task to answer.
#[cfg(target_os = "android")]
fn quit_core(core: &AndroidCore) {
    info!("Shutting down...");
    let fin_rx = core.fin_rx.lock().unwrap().take();
    let msg_tx = core.msg_tx.clone();
    RUNTIME.block_on(async move {
        msg_tx.send(Message::Quit);
        if let Some(fin_rx) = fin_rx {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fin_rx).await;
        }
    });
}

/// Run the app on android: the core once per process, then this activity's
/// UI attached to it. `platform_event_rx` is only taken by the first run.
/// Returns whether the core outlives this UI (the service keeps it).
#[cfg(target_os = "android")]
pub fn run(
    android_app: slint::android::AndroidApp,
    platform_event_rx: Option<mpsc::UnboundedReceiver<Message>>,
    settings: Settings,
) -> Result<bool> {
    let start = std::time::Instant::now();
    let core = match (ANDROID_CORE.get(), platform_event_rx) {
        (Some(core), _) => core,
        (None, Some(rx)) => start_core(rx, settings),
        (None, None) => anyhow::bail!("no receiver core and no event channel to start one"),
    };
    let (ui, generation) = attach_ui(&android_app, core)?;
    info!(initialized_in = ?start.elapsed());
    ui.run()?;
    detach_ui(core, generation);
    drop(ui);

    if !ANDROID_CORE_ENDED.load(std::sync::atomic::Ordering::Acquire)
        && receiver_core::android_jni::keep_alive()
    {
        info!("UI gone, the receiver stays up for its service");
        return Ok(true);
    }
    quit_core(core);
    Ok(false)
}

/// Starts the receiver core once per process, without a UI: ReceiverCore
/// calls this from Java whichever of the activity and the service comes first.
#[cfg(target_os = "android")]
pub fn android_start_core(platform_event_rx: mpsc::UnboundedReceiver<Message>, settings: Settings) {
    if ANDROID_CORE.get().is_none() {
        start_core(platform_event_rx, settings);
    }
}

/// The last owner left with no UI up: quit the receiver. The caller exits.
#[cfg(target_os = "android")]
pub fn android_shutdown() {
    if let Some(core) = ANDROID_CORE.get() {
        quit_core(core);
    }
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
