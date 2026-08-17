//! The consumer half of the GUI seam: it takes [`UpdateGuiCommand`]s off the
//! channel and applies them to the slint window.
//!
//! The command enum and the `GuiController` that produces them stay in
//! `receiver-core`; see its module of the same name.

use std::rc::Rc;

use slint::{ComponentHandle, SharedString, ToSharedString, VecModel};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, error};

use fcast_protocol::v3;
use receiver_core::{
    MessageSender,
    application::PacketOrigin,
    fcast::Operation,
    image::DecodedImage,
    log_if_err,
    message::{Message, PortConflictChoice},
    ui_types,
    utils::sec_to_string,
};

// Re-exported so `gui::UpdateGuiCommand` names the same type on both sides.
pub use receiver_core::gui::*;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::UiUpdaterState;
use crate::{
    AppState, Bridge, CompoundImage, GuiPlaybackState, MainWindow, UiMediaTrack, UiMediaTrackType,
    UiPlayerVariant,
};

/// A QR code as slint wants it.
pub type QrCodeImage = slint::SharedPixelBuffer<slint::Rgb8Pixel>;

// ---------------------------------------------------------------------------
// Core's slint-free mirrors -> the types the .slint compiler generated.
// ---------------------------------------------------------------------------

impl From<ui_types::AppState> for AppState {
    fn from(s: ui_types::AppState) -> Self {
        match s {
            ui_types::AppState::Idle => AppState::Idle,
            ui_types::AppState::LoadingMedia => AppState::LoadingMedia,
            ui_types::AppState::Playing => AppState::Playing,
        }
    }
}

impl From<ui_types::GuiPlaybackState> for GuiPlaybackState {
    fn from(s: ui_types::GuiPlaybackState) -> Self {
        match s {
            ui_types::GuiPlaybackState::Idle => GuiPlaybackState::Idle,
            ui_types::GuiPlaybackState::Playing => GuiPlaybackState::Playing,
            ui_types::GuiPlaybackState::Paused => GuiPlaybackState::Paused,
            ui_types::GuiPlaybackState::Loading => GuiPlaybackState::Loading,
        }
    }
}

impl From<ui_types::UiPlayerVariant> for UiPlayerVariant {
    fn from(s: ui_types::UiPlayerVariant) -> Self {
        match s {
            ui_types::UiPlayerVariant::Unknown => UiPlayerVariant::Unknown,
            ui_types::UiPlayerVariant::Image => UiPlayerVariant::Image,
            ui_types::UiPlayerVariant::Audio => UiPlayerVariant::Audio,
            ui_types::UiPlayerVariant::Video => UiPlayerVariant::Video,
            ui_types::UiPlayerVariant::Raop => UiPlayerVariant::Raop,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl From<ui_types::UiUpdaterState> for UiUpdaterState {
    fn from(s: ui_types::UiUpdaterState) -> Self {
        match s {
            ui_types::UiUpdaterState::None => UiUpdaterState::None,
            ui_types::UiUpdaterState::ShowingDialog => UiUpdaterState::ShowingDialog,
            ui_types::UiUpdaterState::Downloading => UiUpdaterState::Downloading,
            ui_types::UiUpdaterState::DownloadFailed => UiUpdaterState::DownloadFailed,
            ui_types::UiUpdaterState::InstallFailed => UiUpdaterState::InstallFailed,
            ui_types::UiUpdaterState::InstallSuccessful => UiUpdaterState::InstallSuccessful,
        }
    }
}

fn track_kind(variant: UiMediaTrackType) -> receiver_core::player::TrackKind {
    use receiver_core::player::TrackKind;
    match variant {
        UiMediaTrackType::Video => TrackKind::Video,
        UiMediaTrackType::Audio => TrackKind::Audio,
        UiMediaTrackType::Subtitle => TrackKind::Subtitle,
    }
}

fn slint_color(c: ui_types::Color) -> slint::Color {
    slint::Color::from_argb_u8(c.alpha, c.red, c.green, c.blue)
}

fn track_model(tracks: Vec<ui_types::UiMediaTrack>) -> slint::ModelRc<UiMediaTrack> {
    Rc::new(VecModel::from(
        tracks
            .into_iter()
            .map(|t| UiMediaTrack {
                id: t.id,
                name: t.name.into(),
            })
            .collect::<Vec<_>>(),
    ))
    .into()
}

/// The module grid core sent, rasterised one pixel per module.
fn qr_pixbuf(qr: &ui_types::QrCode) -> QrCodeImage {
    let mut pixbuf = QrCodeImage::new(qr.size, qr.size);
    let pixels = pixbuf.make_mut_slice();
    for (idx, dark) in qr.dark.iter().take(pixels.len()).enumerate() {
        pixels[idx] = if *dark {
            slint::Rgb8Pixel::new(0x00, 0x00, 0x00)
        } else {
            slint::Rgb8Pixel::new(0xFF, 0xFF, 0xFF)
        };
    }
    pixbuf
}

fn to_slint_pixbuf(img: &receiver_core::image::RgbaImage) -> crate::SlintRgba8Pixbuf {
    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        img.as_raw(),
        img.width(),
        img.height(),
    )
}

/// A decoded image as the UI's compound (bitmap + rotation) struct.
fn as_compound(img: &DecodedImage) -> CompoundImage {
    CompoundImage {
        img: slint::Image::from_rgba8(to_slint_pixbuf(&img.image)),
        rotation: receiver_core::image::orientation_to_degs(img.orientation),
    }
}

pub fn register_callbacks(ui: &MainWindow, msg_tx: MessageSender) {
    let bridge = ui.global::<Bridge>();
    bridge.on_resume_or_pause({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.operation(PacketOrigin::Gui, Operation::ResumeOrPause);
        }
    });

    bridge.on_seek_to_percent({
        let msg_tx = msg_tx.clone();
        move |percent| {
            msg_tx.send(Message::SeekPercent(percent));
        }
    });

    bridge.on_toggle_fullscreen({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks always get called from the event loop");
            let is_fullscreen = !ui.window().is_fullscreen();
            ui.window().set_fullscreen(is_fullscreen);
            ui.global::<Bridge>().set_is_fullscreen(is_fullscreen);
        }
    });

    bridge.on_set_volume({
        let msg_tx = msg_tx.clone();
        move |volume| {
            msg_tx.operation(PacketOrigin::Gui, Operation::SetVolume(volume));
        }
    });

    bridge.on_force_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    bridge.on_port_conflict_retry({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.send(Message::PortConflictChoice(PortConflictChoice::Retry));
        }
    });

    bridge.on_port_conflict_use_different_port({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.send(Message::PortConflictChoice(
                PortConflictChoice::UseDifferentPort,
            ));
        }
    });

    // Ending the Slint loop drives the normal `Message::Quit` shutdown.
    bridge.on_port_conflict_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    bridge.on_debug_toggled({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.send(Message::ToggleDebug);
        }
    });

    bridge.on_change_playback_rate({
        let msg_tx = msg_tx.clone();
        move |new_rate: f32| {
            msg_tx.operation(PacketOrigin::Gui, Operation::SetSpeed(new_rate));
        }
    });

    bridge.on_set_cursor_hidden({
        let ui_weak = ui.as_weak();
        move |hidden| {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks are always called from the event loop");
            if hidden {
                let _ =
                    ui.window()
                        .try_dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position: slint::LogicalPosition::new(0.0, 0.0),
                            button: slint::platform::PointerEventButton::Other,
                        });
            }

            // The subsurface sink occludes the winit window once controls hide, so Slint's
            // redraw-applied cursor change never lands: set it on the winit window, hide
            // and unhide.
            #[cfg(all(target_os = "linux", feature = "wayland-subsurface"))]
            {
                use i_slint_backend_winit::WinitWindowAccessor;
                ui.window().with_winit_window(|win| {
                    win.set_cursor_visible(!hidden);
                });
            }

            #[cfg(target_os = "macos")]
            {
                let _ = &ui;
                // Callback runs on the Slint event loop, i.e. the main thread.
                objc2_app_kit::NSCursor::setHiddenUntilMouseMoves(hidden);
            }
        }
    });

    bridge.on_select_track({
        let msg_tx = msg_tx.clone();
        move |id: i32, variant: UiMediaTrackType| {
            msg_tx.send(Message::SelectTrack {
                id,
                variant: track_kind(variant),
            });
        }
    });

    bridge.on_set_bool_setting({
        let msg_tx = msg_tx.clone();
        move |key: SharedString, value: bool| {
            msg_tx.send(Message::SetConfigBool {
                key: key.to_string(),
                value,
            });
        }
    });

    bridge.on_set_string_setting({
        let msg_tx = msg_tx.clone();
        move |key: SharedString, value: SharedString| {
            msg_tx.send(Message::SetConfigString {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    });

    bridge.on_select_playlist_item({
        let msg_tx = msg_tx.clone();
        move |idx: i32| {
            msg_tx.operation(
                PacketOrigin::Gui,
                Operation::SetPlaylistItem(v3::SetPlaylistItemMessage {
                    item_index: idx as u64,
                }),
            );
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    bridge.on_perform_app_update({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.app_update(message::AppUpdate::UpdateApplication);
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    bridge.on_restart_app({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.app_update(message::AppUpdate::RestartApp);
        }
    });

    bridge.on_refresh_pipeline_graph({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.send(Message::InspectorRefresh);
        }
    });

    bridge.on_inspector_tick({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.send(Message::InspectorBitrateTick);
        }
    });

    bridge.on_stop_playback({
        let msg_tx = msg_tx.clone();
        move || {
            msg_tx.operation(PacketOrigin::Gui, Operation::Stop);
        }
    });

    bridge.on_sec_to_string(|sec: i32| -> SharedString {
        sec_to_string(sec as f64).to_shared_string()
    });

    bridge.on_sec_float_to_string(|sec: f32| -> SharedString {
        sec_to_string(sec as f64).to_shared_string()
    });
}

pub enum RendererMessage {
    CreateBluredAudioTrackCover(DecodedImage),
    ClearBluredAudioTrackCover,
    ClearVideoOverlays,
}

type RendererMsgSender = std::sync::mpsc::Sender<RendererMessage>;

fn set_playback_progress(bridge: &Bridge, prog_sec: Seconds, dur_sec: Seconds) {
    if !bridge.get_is_scrubbing_position() && !bridge.get_seek_pending() {
        bridge.set_progress_secs(prog_sec);
    }
    bridge.set_duration_secs(dur_sec);
}

fn set_buffered_ranges(bridge: &Bridge, ranges: Vec<(f32, f32)>) {
    let model: Vec<crate::UiBufferedRange> = ranges
        .into_iter()
        .map(|(start, stop)| crate::UiBufferedRange { start, stop })
        .collect();
    bridge.set_buffered_ranges(Rc::new(VecModel::from(model)).into());
}

fn clear_audio_covers(bridge: &Bridge, renderer_tx: &RendererMsgSender) {
    bridge.set_audio_track_cover(CompoundImage::default());
    let _ = renderer_tx.send(RendererMessage::ClearBluredAudioTrackCover);
}

/// Re-assert a visible cursor whenever the video scene is NOT up.
///
/// The video player is the only writer of the winit-level cursor hide, and
/// Slint's cursor cache is blind to those direct calls (it re-applies only
/// when the cursor it computes on a mouse event differs from its cache). A
/// hide that lands after the teardown unhide but before the view unmounts
/// (the 2s auto-hide timer) therefore left the cursor invisible at the idle
/// menu with nothing ever re-showing it. The scene condition mirrors
/// main.slint's VideoPlayerView mount condition, and this runs after every
/// write to either property, so a falling edge always ends on "visible".
fn unhide_cursor_outside_video_scene(ui: &MainWindow) {
    let bridge = ui.global::<Bridge>();
    let video_scene = bridge.get_app_state() == AppState::Playing
        && bridge.get_player_variant() == UiPlayerVariant::Video;
    if !video_scene {
        bridge.invoke_set_cursor_hidden(false);
    }
}

fn handle_command(ui: MainWindow, cmd: UpdateGuiCommand, renderer_tx: &RendererMsgSender) {
    let bridge = ui.global::<Bridge>();

    match cmd {
        UpdateGuiCommand::DeviceConnected => ui.invoke_device_connected(),
        UpdateGuiCommand::DeviceDisconnected => bridge.invoke_device_disconnected(),
        UpdateGuiCommand::SetFullscreen {
            fullscreen,
            prev_tx,
        } => {
            let window = ui.window();
            let _ = prev_tx.send(window.is_fullscreen());
            window.set_fullscreen(fullscreen);
        }
        UpdateGuiCommand::SetAppState(state) => {
            bridge.set_app_state(state.into());
            unhide_cursor_outside_video_scene(&ui);
        }
        UpdateGuiCommand::UpdatePlaylist { start_idx, length } => {
            bridge.set_playlist_idx(start_idx);
            bridge.set_playlist_idx(length);
        }
        UpdateGuiCommand::SetImage { typ, img } => match typ {
            ImageType::Preview => bridge.set_image_preview(as_compound(&img.0)),
            ImageType::AudioTrackCover => {
                bridge.set_audio_track_cover(as_compound(&img.0));
                let _ = renderer_tx.send(RendererMessage::CreateBluredAudioTrackCover(img.0));
            }
        },
        UpdateGuiCommand::UpdatePlaybackProgress {
            progress_s,
            duration_s,
        } => {
            set_playback_progress(&bridge, progress_s, duration_s);
        }
        UpdateGuiCommand::SetBufferedRanges(ranges) => set_buffered_ranges(&bridge, ranges),
        UpdateGuiCommand::SetMediaTitle(title) => bridge.set_media_title(title.to_shared_string()),
        UpdateGuiCommand::SetArtistName(name) => bridge.set_artist_name(name.to_shared_string()),
        UpdateGuiCommand::ClearAudioCovers => clear_audio_covers(&bridge, renderer_tx),
        UpdateGuiCommand::ClearCommonPlaybackState => {
            clear_audio_covers(&bridge, renderer_tx);
            set_playback_progress(&bridge, 0.0, 0.0);
            set_buffered_ranges(&bridge, Vec::new());
        }
        UpdateGuiCommand::SetPlayerType(typ) => {
            bridge.set_player_variant(typ.into());
            unhide_cursor_outside_video_scene(&ui);
        }
        UpdateGuiCommand::SetTracks {
            videos,
            audios,
            subtitles,
        } => {
            macro_rules! wrap_or_default {
                ($tracks:expr) => {
                    $tracks.map(track_model).unwrap_or_default()
                };
            }

            bridge.set_video_tracks(wrap_or_default!(videos));
            bridge.set_audio_tracks(wrap_or_default!(audios));
            bridge.set_subtitle_tracks(wrap_or_default!(subtitles));
        }
        UpdateGuiCommand::SetTrackIds {
            video,
            audio,
            subtitle,
        } => {
            bridge.set_current_video_track(video);
            bridge.set_current_audio_track(audio);
            bridge.set_current_subtitle_track(subtitle);
        }
        UpdateGuiCommand::ClearVideoOverlays => {
            let _ = renderer_tx.send(RendererMessage::ClearVideoOverlays);
            ui.window().request_redraw();
        }
        UpdateGuiCommand::SetConnectionDetails { qr_code, addrs } => {
            bridge.set_qr_code(slint::Image::from_rgb8(qr_pixbuf(&qr_code.0)));
            bridge.set_local_ip_addrs(addrs.to_shared_string());
        }
        UpdateGuiCommand::SetLocalDeviceName(name) => {
            bridge.set_device_name(name.to_shared_string())
        }
        UpdateGuiCommand::SetVolume(volume) => {
            bridge.set_volume(volume);
            bridge.set_volume_set_at(1.0);
        }
        UpdateGuiCommand::SetPlaylistIndex(idx) => bridge.set_playlist_idx(idx),
        UpdateGuiCommand::ShowToastMessage { msg, typ } => match typ {
            ToastType::Warning => {
                bridge.set_warning_message(msg.to_shared_string());
                bridge.set_is_showing_warning_message(true);
            }
            ToastType::Error => {
                bridge.set_error_message(msg.to_shared_string());
                bridge.set_is_showing_error_message(true);
            }
        },
        UpdateGuiCommand::SetPlaybackState(state) => bridge.set_playback_state(state.into()),
        UpdateGuiCommand::ClearImageState => {
            bridge.set_image_preview(CompoundImage::default());
            clear_audio_covers(&bridge, renderer_tx);
        }
        UpdateGuiCommand::SetImageViaPlayer(via_player) => bridge.set_image_via_player(via_player),
        UpdateGuiCommand::SetIsLive(is_live) => bridge.set_is_live(is_live),
        UpdateGuiCommand::SetSeekPending(pending) => bridge.set_seek_pending(pending),
        UpdateGuiCommand::SetSourceBackoff {
            remaining_ms,
            total_ms,
        } => {
            bridge.set_source_backoff_remaining_ms(remaining_ms.min(i32::MAX as u64) as i32);
            bridge.set_source_backoff_total_ms(total_ms.min(i32::MAX as u64) as i32);
        }
        UpdateGuiCommand::SetPlaybackRate(rate) => bridge.set_playback_rate(rate),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        UpdateGuiCommand::SetUpdateState(state) => bridge.set_updater_state(state),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        UpdateGuiCommand::SetUpdateDownloadProgress(progress) => {
            bridge.set_update_download_progress(progress)
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        UpdateGuiCommand::SetUpdaterError(err) => bridge.set_updater_error_msg(err),
        UpdateGuiCommand::SetWindowVisibility { visible, prev_tx } => {
            let window = ui.window();
            let _ = prev_tx.send(window.is_visible());
            let res = if visible {
                window.show()
            } else {
                window.hide()
            };
            if let Err(err) = res {
                error!(?err, visible, "Failed to set window visibility");
            }
        }
        UpdateGuiCommand::SetGraphDump(dump) => set_graph_dump(&ui, dump.0),
        UpdateGuiCommand::SetInspectorDumping(dumping) => {
            ui.global::<crate::InspectorState>().set_dumping(dumping);
        }
        UpdateGuiCommand::SetInspectorSample(sample) => set_inspector_sample(&ui, sample.0),
        UpdateGuiCommand::ShowPortConflict { port } => {
            let bridge = ui.global::<Bridge>();
            bridge.set_conflicting_port(port as i32);
            bridge.set_show_port_conflict(true);
            // Force visible so the dialog is seen even under `--no-main-window`
            // (`resolve_listen_port` restores the hidden state afterwards).
            log_if_err!(ui.window().show());
        }
        UpdateGuiCommand::SetStartingUp(starting_up) => {
            let bridge = ui.global::<Bridge>();
            bridge.set_starting_up(starting_up);
            if !starting_up {
                bridge.set_show_port_conflict(false);
            }
        }
        #[cfg(not(target_os = "android"))]
        UpdateGuiCommand::InitSettings {
            config,
            config_path,
            airplay_available,
        } => {
            let bridge = ui.global::<Bridge>();
            bridge.set_cfg_fcast_enabled(config.fcast.enabled);
            bridge.set_cfg_fcast_name(config.fcast.name.clone().unwrap_or_default().into());
            bridge.set_cfg_raop_enabled(config.raop.enabled);
            bridge.set_cfg_raop_name(config.raop.name.clone().unwrap_or_default().into());
            bridge.set_cfg_chromecast_enabled(config.chromecast.enabled);
            bridge
                .set_cfg_chromecast_name(config.chromecast.name.clone().unwrap_or_default().into());
            bridge.set_cfg_airplay_enabled(config.airplay.enabled);
            bridge.set_cfg_interface_show_window(config.interface.show_window);
            bridge.set_cfg_interface_tray(config.interface.tray);
            bridge.set_cfg_interface_start_fullscreen(config.interface.start_fullscreen);
            bridge.set_cfg_interface_fullscreen_player(config.interface.fullscreen_player);
            bridge.set_cfg_interface_headless(config.interface.headless);
            bridge.set_cfg_video_hdr_output(config.video.hdr_output);
            bridge.set_cfg_video_render_profile(
                config
                    .video
                    .render_profile
                    .clone()
                    .unwrap_or_else(|| "Default".to_owned())
                    .into(),
            );
            bridge.set_cfg_discovery_exclude_interfaces(
                config
                    .discovery
                    .exclude_interfaces
                    .clone()
                    .unwrap_or_default()
                    .into(),
            );
            bridge.set_cfg_log_level(
                config
                    .log
                    .level
                    .clone()
                    .unwrap_or_else(|| "Default".to_owned())
                    .into(),
            );
            bridge.set_settings_config_path(config_path.into());
            bridge.set_settings_airplay_available(airplay_available);
        }
        // Handled in `spawn_command_handler`, which holds the tray handle.
        UpdateGuiCommand::ShowSystemTray => (),
        UpdateGuiCommand::QuitLoop => (),
    }
}

/// Push one inspector sample into the UI. Sparklines are SVG polylines over a
/// fixed 300x100 viewbox; each series is scaled to its own peak because video
/// dwarfs audio by 10-20x.
fn set_inspector_sample(ui: &MainWindow, sample: InspectorSample) {
    use std::fmt::Write;

    let video_kbps: &[f32] = &sample.video_kbps;
    let audio_kbps: &[f32] = &sample.audio_kbps;

    fn fmt_rate(kbps: f32) -> String {
        if kbps >= 1000.0 {
            format!("{:.1} Mbit/s", kbps / 1000.0)
        } else {
            format!("{kbps:.0} kbit/s")
        }
    }

    fn polyline(history: &[f32], peak: f32) -> SharedString {
        let mut commands = String::new();
        let last = history.len().saturating_sub(1).max(1) as f32;
        for (i, kbps) in history.iter().enumerate() {
            let x = i as f32 / last * 300.0;
            let y = 100.0 - (kbps / peak) * 95.0;
            let op = if i == 0 { 'M' } else { 'L' };
            let _ = write!(commands, "{op} {x:.1} {y:.1} ");
        }
        commands.into()
    }

    let video_peak = video_kbps.iter().fold(1.0f32, |m, v| m.max(*v));
    let audio_peak = audio_kbps.iter().fold(1.0f32, |m, v| m.max(*v));

    let state = ui.global::<crate::InspectorState>();
    state.set_video_bitrate_path(polyline(video_kbps, video_peak));
    state.set_audio_bitrate_path(polyline(audio_kbps, audio_peak));
    state.set_video_bitrate_label(
        format!(
            "Video {}",
            fmt_rate(video_kbps.last().copied().unwrap_or(0.0))
        )
        .into(),
    );
    state.set_audio_bitrate_label(
        format!(
            "Audio {}",
            fmt_rate(audio_kbps.last().copied().unwrap_or(0.0))
        )
        .into(),
    );
    state.set_video_bitrate_peak_label(fmt_rate(video_peak).into());
    state.set_audio_bitrate_peak_label(fmt_rate(audio_peak).into());
    state.set_have_bitrate(true);

    let tracks: Vec<crate::UiInspectorTrack> = sample
        .tracks
        .into_iter()
        .map(|t| crate::UiInspectorTrack {
            kind: t.kind.into(),
            codec: t.codec.into(),
            detail: t.detail.into(),
            language: t.language.into(),
            selected: t.selected,
        })
        .collect();
    state.set_tracks(Rc::new(VecModel::from(tracks)).into());
    state.set_container(sample.container.into());
    let lines = |v: Vec<String>| -> slint::ModelRc<SharedString> {
        Rc::new(VecModel::from(
            v.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        ))
        .into()
    };
    state.set_sources_lines(lines(sample.sources));
    state.set_internals_lines(lines(sample.internals));
    state.set_sink_lines(lines(sample.sinks));
    state.set_image_info(sample.image.into());

    match sample.buffering {
        Some(buffering) => {
            state.set_buffering_fill(buffering.fill_fraction);
            state.set_buffering_fill_label(buffering.fill_label.into());
            state.set_buffering_ahead_label(buffering.ahead_label.into());
            state.set_buffering_mode_label(buffering.mode_label.into());
            state.set_buffering_eta_label(buffering.eta_label.into());
            state.set_have_buffering(true);
        }
        None => state.set_have_buffering(false),
    }
}

fn set_graph_dump(ui: &MainWindow, dump: GraphDumpData) {
    let rects: Vec<crate::UiGraphRect> = dump
        .scene
        .rects
        .iter()
        .map(|rect| crate::UiGraphRect {
            x: rect.x,
            y: rect.y,
            width: rect.w,
            height: rect.h,
            fill: slint_color(rect.fill),
            stroke: slint_color(rect.stroke),
        })
        .collect();
    // Wire geometry is denormalized onto every hit-zone and chip (shared refcounted
    // strings), so the UI can highlight without model indexing.
    let edge_commands: Vec<(slint::SharedString, slint::SharedString)> = dump
        .scene
        .edge_paths
        .iter()
        .map(|e| (e.commands.as_str().into(), e.arrow.as_str().into()))
        .collect();
    let labels: Vec<crate::UiGraphLabel> = dump
        .scene
        .labels
        .iter()
        .map(|label| crate::UiGraphLabel {
            x: label.x,
            y: label.y,
            width: label.w,
            height: label.h,
            summary: label.summary.as_str().into(),
            detail: label.detail.as_str().into(),
            detail_width: label.detail_w,
            detail_height: label.detail_h,
            edge: label.edge as i32,
            commands: edge_commands[label.edge].0.clone(),
            arrow: edge_commands[label.edge].1.clone(),
        })
        .collect();
    let hits: Vec<crate::UiEdgeHit> = dump
        .scene
        .edge_hits
        .iter()
        .map(|hit| crate::UiEdgeHit {
            x: hit.x,
            y: hit.y,
            width: hit.w,
            height: hit.h,
            edge: hit.edge as i32,
            commands: edge_commands[hit.edge].0.clone(),
            arrow: edge_commands[hit.edge].1.clone(),
        })
        .collect();
    let texts: Vec<crate::UiGraphText> = dump
        .scene
        .texts
        .iter()
        .map(|text| crate::UiGraphText {
            x: text.x,
            y: text.y,
            size: text.size,
            text: text.text.as_str().into(),
            color: slint_color(text.color),
        })
        .collect();

    let state = ui.global::<crate::InspectorState>();
    state.set_graph(crate::GraphDump {
        trigger: dump.trigger.into(),
        timestamp: dump.timestamp.into(),
        width: dump.scene.width,
        height: dump.scene.height,
        rects: Rc::new(slint::VecModel::from(rects)).into(),
        labels: Rc::new(slint::VecModel::from(labels)).into(),
        hits: Rc::new(slint::VecModel::from(hits)).into(),
        texts: Rc::new(slint::VecModel::from(texts)).into(),
        edges: dump.scene.edges.as_str().into(),
        arrows: dump.scene.arrows.as_str().into(),
    });
    state.set_have_graph(true);
    state.set_dumping(false);
}

pub fn spawn_command_handler(
    ui_weak: slint::Weak<MainWindow>,
    mut cmd_rx: UnboundedReceiver<UpdateGuiCommand>,
    renderer_tx: RendererMsgSender,
    // Runs on the event-loop thread; the tray handle is `!Send`. A no-op when there is no tray.
    on_show_tray: Box<dyn FnOnce()>,
) {
    slint::spawn_local(async move {
        let mut on_show_tray = Some(on_show_tray);
        loop {
            if let Some(cmd) = cmd_rx.recv().await
                && let Some(ui) = ui_weak.upgrade()
            {
                // Ignore frequently sent updates to reduce log size
                if !matches!(cmd, UpdateGuiCommand::UpdatePlaybackProgress { .. }) {
                    debug!(?cmd, "received command");
                }
                if matches!(cmd, UpdateGuiCommand::QuitLoop) {
                    break;
                }
                if matches!(cmd, UpdateGuiCommand::ShowSystemTray) {
                    if let Some(show) = on_show_tray.take() {
                        show();
                    }
                    continue;
                }
                handle_command(ui, cmd, &renderer_tx);
            } else {
                debug!("Stopping");
                break;
            }
        }
    })
    .expect("Failed to spawn GUI command handler");
}
