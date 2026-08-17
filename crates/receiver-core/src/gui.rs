//! The core half of the GUI seam: the command enum the rest of the receiver
//! sends, and the [`GuiController`] facade it sends them through.
//!
//! Applying a command (which needs the generated slint types) is the UI
//! layer's job; see `receiver-ui`'s module of the same name.

use std::sync::Arc;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::ui_types::UiUpdaterState;
use crate::{
    image::DecodedImage,
    ui_types::{AppState, GuiPlaybackState, QrCode, UiMediaTrack, UiPlayerVariant},
};
use parking_lot::{Condvar, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

#[derive(Debug)]
pub enum ImageType {
    Preview,
    AudioTrackCover,
}

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

pub struct GraphDumpData {
    pub trigger: String,
    pub timestamp: String,
    pub scene: crate::inspector_graph::Scene,
}

/// One row of the inspector's track table.
pub struct InspectorTrackRow {
    pub kind: String,
    pub codec: String,
    pub detail: String,
    pub language: String,
    pub selected: bool,
}

/// The inspector's buffering card; `None` when the source can't answer a
/// buffering query.
pub struct InspectorBuffering {
    /// Buffer fill (`0.0..=1.0`) for the meter, relative to the watermarks.
    pub fill_fraction: f32,
    pub fill_label: String,
    /// Buffered-ahead duration, e.g. "2.1 s", or empty when unknown.
    pub ahead_label: String,
    pub mode_label: String,
    /// e.g. "full in 3.2 s", or empty when unknown.
    pub eta_label: String,
}

/// One inspector tick's display data. Bitrate histories are kbit/s, oldest
/// first.
pub struct InspectorSample {
    pub video_kbps: Vec<f32>,
    pub audio_kbps: Vec<f32>,
    pub tracks: Vec<InspectorTrackRow>,
    pub container: String,
    pub sources: Vec<String>,
    pub internals: Vec<String>,
    pub sinks: Vec<String>,
    pub image: String,
    pub buffering: Option<InspectorBuffering>,
}

#[derive(Debug)]
pub enum UpdateGuiCommand {
    DeviceConnected,
    DeviceDisconnected,
    SetFullscreen {
        fullscreen: bool,
        prev_tx: oneshot::Sender<bool>,
    },
    SetAppState(AppState),
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
    SetBufferedRanges(Vec<(f32, f32)>),
    SetMediaTitle(String),
    SetArtistName(String),
    ClearAudioCovers,
    ClearCommonPlaybackState,
    SetPlayerType(UiPlayerVariant),
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
    ClearVideoOverlays,
    SetConnectionDetails {
        qr_code: IgnoredDebug<QrCode>,
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
    SetImageViaPlayer(bool),
    SetIsLive(bool),
    SetSeekPending(bool),
    /// Server-directed source backoff countdown ("server busy, retrying in
    /// Ns"). `remaining_ms == 0` clears it, `total_ms` sizes the bar.
    SetSourceBackoff {
        remaining_ms: u64,
        total_ms: u64,
    },
    SetPlaybackRate(f32),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdateState(UiUpdaterState),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdateDownloadProgress(i32),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdaterError(String),
    /// Run a closure on the UI thread. Commands are already applied on the
    /// event loop, so this is how non-UI code (the updater) gets there
    /// without slint.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    RunOnMainThread(IgnoredDebug<Box<dyn FnOnce() + Send + Sync + 'static>>),
    SetWindowVisibility {
        visible: bool,
        prev_tx: oneshot::Sender<bool>,
    },
    SetGraphDump(IgnoredDebug<GraphDumpData>),
    SetInspectorDumping(bool),
    SetInspectorSample(IgnoredDebug<InspectorSample>),
    /// Show the "port already in use" modal and force the window visible so the
    /// dialog is seen.
    ShowPortConflict {
        port: u16,
    },
    /// Toggle the startup screen; turning it off also clears the conflict
    /// prompt.
    SetStartingUp(bool),
    /// Reveal the system tray icon. Sent once the listening port is committed,
    /// so a conflict that ends in quitting never leaves a stray tray icon.
    /// Handled in `spawn_command_handler`.
    ShowSystemTray,
    /// Push the current persisted config into the settings drawer's bindings;
    /// sent once at startup.
    #[cfg(not(target_os = "android"))]
    InitSettings {
        config: crate::config::Config,
        config_path: String,
        airplay_available: bool,
    },
    QuitLoop,
}

struct GuiIsVisibleHandle {
    is_visible: Mutex<bool>,
    cvar: Condvar,
}

#[derive(Clone)]
pub struct GuiIsVisible(Arc<GuiIsVisibleHandle>);

impl GuiIsVisible {
    pub fn new() -> Self {
        let handle = GuiIsVisibleHandle {
            is_visible: Mutex::new(false),
            cvar: Condvar::new(),
        };

        Self(Arc::new(handle))
    }

    pub fn set(&self, visible: bool) {
        *self.0.is_visible.lock() = visible;
        self.0.cvar.notify_one();
    }

    pub fn get(&self) -> bool {
        *self.0.is_visible.lock()
    }
}

pub struct GuiController {
    pub tx: Option<UnboundedSender<UpdateGuiCommand>>,
    playback_state: GuiPlaybackState,
    playback_rate: f32,
    is_live: bool,
    backoff_active: bool,
    is_visible: GuiIsVisible,
}

impl GuiController {
    pub fn new(tx: Option<UnboundedSender<UpdateGuiCommand>>, is_visible: GuiIsVisible) -> Self {
        Self {
            tx,
            playback_state: GuiPlaybackState::default(),
            playback_rate: -1.0,
            is_live: false,
            backoff_active: false,
            is_visible,
        }
    }

    fn send(&self, cmd: UpdateGuiCommand) {
        if let Some(tx) = &self.tx
            && let Err(err) = tx.send(cmd)
        {
            error!(?err, "Failed to send update gui command");
        }
    }

    pub fn device_connected(&self) {
        self.send(UpdateGuiCommand::DeviceConnected);
    }

    pub fn device_disconnected(&self) {
        self.send(UpdateGuiCommand::DeviceDisconnected);
    }

    #[cfg(not(target_os = "android"))]
    pub fn init_settings(
        &self,
        config: crate::config::Config,
        config_path: String,
        airplay_available: bool,
    ) {
        self.send(UpdateGuiCommand::InitSettings {
            config,
            config_path,
            airplay_available,
        });
    }

    /// Returns the the previous window fulscreen state.
    pub fn set_fullscreen(&self, fullscreen: bool) -> bool {
        let (prev_tx, prev_rx) = oneshot::channel();
        self.send(UpdateGuiCommand::SetFullscreen {
            fullscreen,
            prev_tx,
        });
        match prev_rx.recv() {
            Ok(p) => p,
            Err(err) => {
                error!(?err, "Failed to receive previous window fullscreen state");
                false
            }
        }
    }

    pub fn set_app_state(&self, state: AppState) {
        self.send(UpdateGuiCommand::SetAppState(state));
    }

    pub fn set_inspector_sample(&self, sample: InspectorSample) {
        self.send(UpdateGuiCommand::SetInspectorSample(sample.into()));
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

    pub fn update_playback_progress(&self, prog_sec: Seconds, dur_sec: Seconds) {
        self.send(UpdateGuiCommand::UpdatePlaybackProgress {
            progress_s: prog_sec,
            duration_s: dur_sec,
        });
    }

    /// Push the scrubber's buffered regions (timeline fractions `0.0..=1.0`,
    /// `start` < `stop`).
    pub fn set_buffered_ranges(&self, ranges: Vec<(f32, f32)>) {
        self.send(UpdateGuiCommand::SetBufferedRanges(ranges));
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

    pub fn clear_video_overlays(&self) {
        self.send(UpdateGuiCommand::ClearVideoOverlays);
    }

    pub fn set_connection_details(&self, qr_code: QrCode, addrs: String) {
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

    /// Mark the load as an animated image decoded through the player pipeline:
    /// the image view then paints nothing opaque so the video sink below
    /// shows through.
    pub fn set_image_via_player(&self, via_player: bool) {
        self.send(UpdateGuiCommand::SetImageViaPlayer(via_player));
    }

    pub fn set_is_live(&mut self, is_live: bool) {
        if is_live != self.is_live {
            self.send(UpdateGuiCommand::SetIsLive(is_live));
            self.is_live = is_live;
        }
    }

    /// Whether the "server busy" countdown is currently shown. The
    /// application's low-on-data gate keeps updating a shown countdown even
    /// after the buffer recovers, rather than letting it freeze mid-bar.
    pub fn source_backoff_shown(&self) -> bool {
        self.backoff_active
    }

    /// Update the "server busy" countdown. `remaining_ms == 0` clears it.
    /// Repeated clears are swallowed so stop/load paths can clear blindly.
    pub fn set_source_backoff(&mut self, remaining_ms: u64, total_ms: u64) {
        let active = remaining_ms > 0;
        if !active && !self.backoff_active {
            return;
        }
        self.backoff_active = active;
        self.send(UpdateGuiCommand::SetSourceBackoff {
            remaining_ms,
            total_ms,
        });
    }

    pub fn set_seek_pending(&self, pending: bool) {
        self.send(UpdateGuiCommand::SetSeekPending(pending));
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

    /// Returns the the previous window visibility state.
    pub fn set_window_visibility(&self, visible: bool) -> bool {
        let (prev_tx, prev_rx) = oneshot::channel();
        self.send(UpdateGuiCommand::SetWindowVisibility { visible, prev_tx });
        match prev_rx.recv() {
            Ok(p) => p,
            Err(err) => {
                error!(?err, "Failed to receive previous window visibility state");
                false
            }
        }
    }

    pub fn show_port_conflict(&self, port: u16) {
        self.send(UpdateGuiCommand::ShowPortConflict { port });
    }

    pub fn set_starting_up(&self, starting_up: bool) {
        self.send(UpdateGuiCommand::SetStartingUp(starting_up));
    }

    pub fn show_system_tray(&self) {
        self.send(UpdateGuiCommand::ShowSystemTray);
    }

    pub fn quit_loop(&mut self) {
        self.send(UpdateGuiCommand::QuitLoop);
    }

    pub fn wait_for_is_visible(&self) -> bool {
        if !self.is_visible.get() {
            let mut is_visible = self.is_visible.0.is_visible.lock();
            self.is_visible
                .0
                .cvar
                .wait_for(&mut is_visible, std::time::Duration::from_millis(200));
            *is_visible
        } else {
            true
        }
    }
}
