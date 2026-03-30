use std::rc::Rc;

use crate::{
    Bridge, CompoundImage, DecodedImage, Event, GuiPlaybackState, MainWindow, Operation,
    SetVolumeMessage, UiMediaTrack, UiMediaTrackType, UiPlayerVariant, log_if_err,
};
use fcast_protocol::v3;
use slint::{ComponentHandle, ToSharedString, VecModel};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, error};

pub fn register_callbacks(ui: &MainWindow, bridge: &Bridge, event_tx: UnboundedSender<Event>) {
    bridge.on_resume_or_pause({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::ResumeOrPause));
        }
    });

    bridge.on_seek_to_percent({
        let event_tx = event_tx.clone();
        move |percent| {
            log_if_err!(event_tx.send(Event::SeekPercent(percent)));
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
        let event_tx = event_tx.clone();
        move |volume| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetVolume(SetVolumeMessage {
                    volume: volume as f64,
                })
            }));
        }
    });

    bridge.on_force_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    bridge.on_debug_toggled({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::ToggleDebug));
        }
    });

    bridge.on_change_playback_rate({
        let event_tx = event_tx.clone();
        move |new_rate: f32| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetSpeed(fcast_protocol::SetSpeedMessage {
                    speed: new_rate as f64
                }),
            }));
        }
    });

    bridge.on_hide_cursor_hack({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks are always called from the event loop");
            let _ = ui
                .window()
                .try_dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(0.0, 0.0),
                    button: slint::platform::PointerEventButton::Other,
                });
        }
    });

    bridge.on_select_track({
        let event_tx = event_tx.clone();
        move |id: i32, variant: UiMediaTrackType| {
            log_if_err!(event_tx.send(Event::SelectTrack { id, variant }));
        }
    });

    bridge.on_select_playlist_item({
        let event_tx = event_tx.clone();
        move |idx: i32| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetPlaylistItem(v3::SetPlaylistItemMessage {
                    item_index: idx as u64
                }),
            }));
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    bridge.on_perform_app_update({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::AppUpdate(crate::AppUpdateEvent::UpdateApplication),));
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    bridge.on_restart_app({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::AppUpdate(crate::AppUpdateEvent::RestartApp),));
        }
    });

    bridge.on_sec_to_string(|sec: i32| -> slint::SharedString {
        crate::sec_to_string(sec as f64).to_shared_string()
    });

    bridge.on_sec_float_to_string(|sec: f32| -> slint::SharedString {
        crate::sec_to_string(sec as f64).to_shared_string()
    });
}

#[derive(Debug)]
pub enum ImageType {
    Preview,
    AudioTrackCover,
    BluredAudioTrackCover,
}

pub type QrCodeImage = slint::SharedPixelBuffer<slint::Rgb8Pixel>;
pub type Seconds = f32;

pub struct IgnoredDebug<T>(pub T);

impl<T> std::fmt::Debug for IgnoredDebug<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[ignored]")
    }
}

impl<T> From<T> for IgnoredDebug<T> {
    fn from(t: T) -> Self {
        Self(t)
    }
}

impl<T> std::ops::Deref for IgnoredDebug<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub enum ToastType {
    Warning,
    Error,
}

#[derive(Debug)]
pub enum UpdateGuiCommand {
    DeviceConnected,
    DeviceDisconnected,
    SetFullscreen(bool),
    SetAppState(crate::AppState),
    UpdatePlaylist {
        start_idx: i32,
        length: i32,
    },
    SetImage {
        typ: ImageType,
        img: IgnoredDebug<DecodedImage>,
    },
    UpdatePlaybackProgress {
        progress_s: Seconds,
        duration_s: Seconds,
    },
    SetMediaTitle(String),
    SetArtistName(String),
    ClearAudioCovers,
    ClearCommonPlaybackState,
    SetPlayerType(UiPlayerVariant),
    // TODO: include hint (e.g. for ensuring window is visibile)
    #[cfg(feature = "systray")]
    ToggleWindow,
    SetTracks {
        videos: Option<Vec<UiMediaTrack>>,
        audios: Option<Vec<UiMediaTrack>>,
        subtitles: Option<Vec<UiMediaTrack>>,
    },
    SetTrackIds {
        video: i32,
        audio: i32,
        subtitle: i32,
    },
    SetConnectionDetails {
        qr_code: IgnoredDebug<QrCodeImage>,
        addrs: String,
    },
    SetLocalDeviceName(String),
    SetVolume(f32),
    SetPlaylistIndex(i32),
    ShowToastMessage {
        msg: String,
        typ: ToastType,
    },
    SetPlaybackState(GuiPlaybackState),
    ClearImageState,
    SetIsLive(bool),
    SetPlaybackRate(f32),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdateState(crate::UiUpdaterState),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdateDownloadProgress(i32),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdaterError(slint::SharedString),
    SetWindowVisibility(bool),
    QuitLoop,
}

pub struct GuiController {
    pub tx: UnboundedSender<UpdateGuiCommand>,
    playback_state: GuiPlaybackState,
    playback_rate: f32,
    is_live: bool,
}

impl GuiController {
    pub fn new(tx: UnboundedSender<UpdateGuiCommand>) -> Self {
        Self {
            tx,
            playback_state: GuiPlaybackState::default(),
            playback_rate: -1.0,
            is_live: false,
        }
    }

    fn send(&self, cmd: UpdateGuiCommand) {
        if let Err(err) = self.tx.send(cmd) {
            error!(?err, "Failed to send update gui command");
        }
    }

    pub fn device_connected(&self) {
        self.send(UpdateGuiCommand::DeviceConnected);
    }

    pub fn device_disconnected(&self) {
        self.send(UpdateGuiCommand::DeviceDisconnected);
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        self.send(UpdateGuiCommand::SetFullscreen(fullscreen));
    }

    pub fn set_app_state(&self, state: crate::AppState) {
        self.send(UpdateGuiCommand::SetAppState(state));
    }

    pub fn update_playlist(&self, start_idx: i32, length: i32) {
        self.send(UpdateGuiCommand::UpdatePlaylist { start_idx, length });
    }

    fn set_image(&self, img: DecodedImage, typ: ImageType) {
        self.send(UpdateGuiCommand::SetImage {
            typ,
            img: img.into(),
        });
    }

    pub fn set_image_preview(&self, img: DecodedImage) {
        self.set_image(img, ImageType::Preview);
    }

    pub fn set_audio_track_cover(&self, img: DecodedImage) {
        self.set_image(img, ImageType::AudioTrackCover);
    }

    pub fn set_blured_audio_track_cover(&self, img: DecodedImage) {
        self.set_image(img, ImageType::BluredAudioTrackCover);
    }

    pub fn update_playback_progress(&self, prog_sec: Seconds, dur_sec: Seconds) {
        self.send(UpdateGuiCommand::UpdatePlaybackProgress {
            progress_s: prog_sec,
            duration_s: dur_sec,
        });
    }

    pub fn set_media_title(&self, title: String) {
        self.send(UpdateGuiCommand::SetMediaTitle(title));
    }

    pub fn set_artist_name(&self, name: String) {
        self.send(UpdateGuiCommand::SetArtistName(name));
    }

    pub fn clear_audio_covers(&self) {
        self.send(UpdateGuiCommand::ClearAudioCovers);
    }

    pub fn clear_common_playback_state(&self) {
        self.send(UpdateGuiCommand::ClearCommonPlaybackState);
    }

    pub fn set_player_type(&self, typ: UiPlayerVariant) {
        self.send(UpdateGuiCommand::SetPlayerType(typ));
    }

    #[cfg(feature = "systray")]
    pub fn toggle_window(&self) {
        self.send(UpdateGuiCommand::ToggleWindow);
    }

    pub fn set_tracks(
        &self,
        videos: Vec<UiMediaTrack>,
        audios: Vec<UiMediaTrack>,
        subtitles: Vec<UiMediaTrack>,
    ) {
        self.send(UpdateGuiCommand::SetTracks {
            videos: Some(videos),
            audios: Some(audios),
            subtitles: Some(subtitles),
        });
    }

    pub fn clear_tracks(&self) {
        self.send(UpdateGuiCommand::SetTracks {
            videos: None,
            audios: None,
            subtitles: None,
        });
    }

    pub fn set_track_ids(&self, video: i32, audio: i32, subtitle: i32) {
        self.send(UpdateGuiCommand::SetTrackIds {
            video,
            audio,
            subtitle,
        });
    }

    pub fn set_connection_details(&self, qr_code: QrCodeImage, addrs: String) {
        self.send(UpdateGuiCommand::SetConnectionDetails {
            qr_code: qr_code.into(),
            addrs,
        });
    }

    pub fn set_local_device_name(&self, name: String) {
        self.send(UpdateGuiCommand::SetLocalDeviceName(name));
    }

    pub fn set_volume(&self, volume: f32) {
        self.send(UpdateGuiCommand::SetVolume(volume));
    }

    pub fn set_playlist_index(&self, index: i32) {
        self.send(UpdateGuiCommand::SetPlaylistIndex(index));
    }

    pub fn show_toast(&self, typ: ToastType, msg: String) {
        self.send(UpdateGuiCommand::ShowToastMessage { msg, typ });
    }

    pub fn set_playback_state(&mut self, state: GuiPlaybackState) {
        if state != self.playback_state {
            self.send(UpdateGuiCommand::SetPlaybackState(state));
            self.playback_state = state;
        }
    }

    pub fn clear_images(&self) {
        self.send(UpdateGuiCommand::ClearImageState);
    }

    pub fn set_is_live(&mut self, is_live: bool) {
        if is_live != self.is_live {
            self.send(UpdateGuiCommand::SetIsLive(is_live));
            self.is_live = is_live;
        }
    }

    pub fn set_playback_rate(&mut self, rate: f32) {
        if rate != self.playback_rate {
            self.send(UpdateGuiCommand::SetPlaybackRate(rate));
            self.playback_rate = rate;
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_updater_state(&self, state: crate::UiUpdaterState) {
        self.send(UpdateGuiCommand::SetUpdateState(state));
    }

    pub fn set_window_visibility(&self, visible: bool) {
        self.send(UpdateGuiCommand::SetWindowVisibility(visible));
    }

    pub fn quit_loop(&mut self) {
        self.send(UpdateGuiCommand::QuitLoop);
    }
}

fn set_playback_progress(bridge: &Bridge, prog_sec: Seconds, dur_sec: Seconds) {
    if !bridge.get_is_scrubbing_position() {
        bridge.set_progress_secs(prog_sec);
    }
    bridge.set_duration_secs(dur_sec);
}

fn clear_audio_covers(bridge: &Bridge) {
    bridge.set_audio_track_cover(CompoundImage::default());
    bridge.set_blured_audio_track_cover(CompoundImage::default());
}

fn handle_command(ui: MainWindow, cmd: UpdateGuiCommand) {
    let bridge = ui.global::<Bridge>();

    match cmd {
        UpdateGuiCommand::DeviceConnected => ui.invoke_device_connected(),
        UpdateGuiCommand::DeviceDisconnected => bridge.invoke_device_disconnected(),
        UpdateGuiCommand::SetFullscreen(fullscreen) => ui.window().set_fullscreen(fullscreen),
        UpdateGuiCommand::SetAppState(state) => bridge.set_app_state(state),
        UpdateGuiCommand::UpdatePlaylist { start_idx, length } => {
            bridge.set_playlist_idx(start_idx);
            bridge.set_playlist_idx(length);
        }
        UpdateGuiCommand::SetImage { typ, img } => {
            let img = img.0.to_compound();
            match typ {
                ImageType::Preview => bridge.set_image_preview(img),
                ImageType::AudioTrackCover => bridge.set_audio_track_cover(img),
                ImageType::BluredAudioTrackCover => bridge.set_blured_audio_track_cover(img),
            }
        }
        UpdateGuiCommand::UpdatePlaybackProgress {
            progress_s,
            duration_s,
        } => {
            set_playback_progress(&bridge, progress_s, duration_s);
        }
        UpdateGuiCommand::SetMediaTitle(title) => bridge.set_media_title(title.to_shared_string()),
        UpdateGuiCommand::SetArtistName(name) => bridge.set_artist_name(name.to_shared_string()),
        UpdateGuiCommand::ClearAudioCovers => clear_audio_covers(&bridge),
        UpdateGuiCommand::ClearCommonPlaybackState => {
            clear_audio_covers(&bridge);
            set_playback_progress(&bridge, 0.0, 0.0);
        }
        UpdateGuiCommand::SetPlayerType(typ) => bridge.set_player_variant(typ),
        #[cfg(feature = "systray")]
        UpdateGuiCommand::ToggleWindow => {
            let window = ui.window();
            if let Err(err) = if window.is_visible() {
                window.hide()
            } else {
                window.show()
            } {
                error!(?err, "Failed to toggle window visibility");
            }
        }
        UpdateGuiCommand::SetTracks {
            videos,
            audios,
            subtitles,
        } => {
            macro_rules! wrap_or_default {
                ($tracks:expr) => {
                    $tracks
                        .map(|t| Rc::new(VecModel::from(t)).into())
                        .unwrap_or(slint::ModelRc::default())
                        .into()
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
        UpdateGuiCommand::SetConnectionDetails { qr_code, addrs } => {
            bridge.set_qr_code(slint::Image::from_rgb8(qr_code.0));
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
        UpdateGuiCommand::SetPlaybackState(state) => bridge.set_playback_state(state),
        UpdateGuiCommand::ClearImageState => {
            bridge.set_video_frame(slint::Image::default());
            bridge.set_image_preview(CompoundImage::default());
            bridge.set_audio_track_cover(CompoundImage::default());
            bridge.set_blured_audio_track_cover(CompoundImage::default());
            bridge.set_overlays(slint::ModelRc::default());
        }
        UpdateGuiCommand::SetIsLive(is_live) => bridge.set_is_live(is_live),
        UpdateGuiCommand::SetPlaybackRate(rate) => bridge.set_playback_rate(rate),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        UpdateGuiCommand::SetUpdateState(state) => bridge.set_updater_state(state),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        UpdateGuiCommand::SetUpdateDownloadProgress(progress) => {
            bridge.set_update_download_progress(progress)
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        UpdateGuiCommand::SetUpdaterError(err) => bridge.set_updater_error_msg(err),
        UpdateGuiCommand::SetWindowVisibility(visible) => {
            let window = ui.window();
            if let Err(err) = if visible {
                window.show()
            } else {
                window.hide()
            } {
                error!(?err, visible, "Failed to set window visibility");
            }
        }
        UpdateGuiCommand::QuitLoop => (),
    }
}

pub fn spawn_command_handler(
    ui_weak: slint::Weak<MainWindow>,
    mut cmd_rx: UnboundedReceiver<UpdateGuiCommand>,
) {
    slint::spawn_local(async move {
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
                handle_command(ui, cmd);
            } else {
                debug!("Stopping");
                break;
            }
        }
    })
    .expect("Failed to spawn GUI command handler");
}
