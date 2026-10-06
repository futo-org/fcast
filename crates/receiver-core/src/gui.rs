//! The core half of the GUI seam: the command enum the rest of the receiver
//! sends, and the [`GuiController`] facade it sends them through.
//!
//! Applying a command (which needs the generated slint types) is the UI
//! layer's job; see `receiver-ui`'s module of the same name.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
use crate::ui_types::UiUpdaterState;
use crate::{
    image::DecodedImage,
    ui_types::{AppState, GuiPlaybackState, QrCode, UiMediaTrack, UiPlayerVariant, UiToastKind},
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

#[cfg(not(target_os = "android"))]
pub struct GraphDumpData {
    pub trigger: String,
    pub timestamp: String,
    pub scene: crate::inspector_graph::Scene,
}

/// One row of the inspector's track table.
#[cfg(not(target_os = "android"))]
pub struct InspectorTrackRow {
    pub kind: String,
    pub codec: String,
    pub detail: String,
    pub language: String,
    pub selected: bool,
}

/// The inspector's buffering card; `None` when the source can't answer a
/// buffering query.
#[cfg(not(target_os = "android"))]
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
#[cfg(not(target_os = "android"))]
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

/// What a sender's transport command does, for the mini OSD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Pause,
    Resume,
    Seek,
}

/// What a sender said about itself when it introduced its session. Empty
/// strings where it said nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SenderInfo {
    pub display_name: String,
    pub app_name: String,
    pub app_version: String,
    /// The protocol the session spoke when it introduced itself.
    pub protocol: fcast_bug_report::Protocol,
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
    /// A load keeps the audio view up in place of the loading screen.
    SetLoadBehindPlayer(bool),
    UpdatePlaylist {
        start_idx: i32,
        length: i32,
    },
    SetImage {
        typ: ImageType,
        img: IgnoredDebug<Arc<DecodedImage>>,
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
    /// `show` pops the volume overlay, false for a restore (attach replay).
    SetVolume {
        volume: f32,
        show: bool,
    },
    SetPlaylistIndex(i32),
    ShowToastMessage {
        kind: UiToastKind,
        /// The kind's interpolable scrap (codec name, host, caps), never a
        /// full sentence. The wording is slint's.
        detail: Option<String>,
        /// Stable short code (FC-Exx/FC-Wxx), shown beside the text.
        code: &'static str,
    },
    /// The report-bug popup for unexpected failures. The sections are raw
    /// technical text (never localized), the GUI composes them by the user's
    /// checklist and asks for the QR of the result.
    ShowBugReport {
        draft: crate::bug_report::Draft,
        code: &'static str,
    },
    /// Dismisses the report-bug popup so a stale one never sits over the
    /// next item. Sent on every new load.
    HideBugReport,
    /// android: the soft keyboard's visibility, see [`crate::message::Message::SoftKeyboardVisible`].
    SetSoftKeyboardVisible(bool),
    SetPlaybackState(GuiPlaybackState),
    ClearImageState,
    SetImageViaPlayer(bool),
    SetIsLive(bool),
    SetSeekPending(bool),
    /// A transport command that did not come from this UI (the sender's
    /// pause, resume or seek). The player shows its mini OSD for it, where a
    /// click on the receiver's own chrome needs no echo. `by` names the
    /// sender when it introduced itself.
    TransportFromSender {
        kind: TransportKind,
        by: Option<String>,
    },
    /// The senders currently introduced, every time the set changes.
    SetSenders(Vec<SenderInfo>),
    /// Server-directed source backoff countdown ("server busy, retrying in
    /// Ns"). `remaining_ms == 0` clears it, `total_ms` sizes the bar.
    SetSourceBackoff {
        remaining_ms: u64,
        total_ms: u64,
    },
    SetPlaybackRate(f32),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    SetUpdateState(UiUpdaterState),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    SetUpdateDownloadProgress(i32),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
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
    #[cfg(not(target_os = "android"))]
    SetGraphDump(IgnoredDebug<GraphDumpData>),
    #[cfg(not(target_os = "android"))]
    SetInspectorDumping(bool),
    #[cfg(not(target_os = "android"))]
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
    InitSettings {
        config: crate::config::Config,
        config_path: String,
        services: Services,
    },
    QuitLoop,
}

/// The cast services this build serves, each one's settings section shows
/// only when it is.
#[derive(Debug, Clone, Copy)]
pub struct Services {
    pub raop: bool,
    pub google_cast: bool,
    pub airplay: bool,
}

struct GuiIsVisibleHandle {
    is_visible: Mutex<bool>,
    cvar: Condvar,
    /// Window shows the application has asked for, see [`await_player_release`].
    shows: AtomicU64,
    /// Of those, the shows the GUI thread has carried out.
    shows_processed: AtomicU64,
}

#[derive(Clone)]
pub struct GuiIsVisible(Arc<GuiIsVisibleHandle>);

impl GuiIsVisible {
    pub fn new() -> Self {
        let handle = GuiIsVisibleHandle {
            is_visible: Mutex::new(false),
            cvar: Condvar::new(),
            shows: AtomicU64::new(0),
            shows_processed: AtomicU64::new(0),
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

    /// Counted before the request is sent, so a teardown blocking the GUI
    /// thread sees it while the application waits on that thread.
    pub fn note_show(&self) {
        self.0.shows.fetch_add(1, Ordering::AcqRel);
    }

    pub fn shows(&self) -> u64 {
        self.0.shows.load(Ordering::Acquire)
    }

    /// Counted by the GUI thread as it carries a show out. A teardown snapshots
    /// this one: a show asked for but not yet carried out is still ahead of it.
    pub fn note_show_processed(&self) {
        self.0.shows_processed.fetch_add(1, Ordering::AcqRel);
    }

    pub fn shows_processed(&self) -> u64 {
        self.0.shows_processed.load(Ordering::Acquire)
    }
}

/// How a renderer teardown's wait for the player to let go ended.
#[derive(Debug, PartialEq, Eq)]
pub enum TeardownWait {
    Released,
    /// A show came in after the teardown began: the application is blocked
    /// on the GUI thread and cannot answer, and the next window is coming.
    ShowRequested,
    TimedOut,
}

const TEARDOWN_POLL: Duration = Duration::from_millis(10);

/// Waits for the player shutdown a teardown asked for, but never for a
/// window that is already being brought back. `shows_processed` is the GUI
/// thread's own count at teardown, so a show requested before the teardown
/// but carried out after it ends the wait too, instead of stalling here and
/// then shutting the player down under that show's load.
pub fn await_player_release(
    released: &oneshot::Receiver<()>,
    visible: &GuiIsVisible,
    shows_processed: u64,
    timeout: Duration,
) -> TeardownWait {
    let deadline = Instant::now() + timeout;
    loop {
        let slice = deadline.saturating_duration_since(Instant::now()).min(TEARDOWN_POLL);
        match released.recv_timeout(slice) {
            // A dropped sender is an application that chose not to shut down.
            Ok(()) | Err(oneshot::RecvTimeoutError::Disconnected) => {
                return TeardownWait::Released;
            }
            Err(oneshot::RecvTimeoutError::Timeout) => {}
        }
        if visible.shows() != shows_processed {
            return TeardownWait::ShowRequested;
        }
        if Instant::now() >= deadline {
            return TeardownWait::TimedOut;
        }
    }
}

pub struct GuiController {
    pub tx: Option<UnboundedSender<UpdateGuiCommand>>,
    playback_state: GuiPlaybackState,
    playback_rate: f32,
    is_live: bool,
    backoff_active: bool,
    is_visible: GuiIsVisible,
    /// Recorded state for a UI that attaches later, `None` where none can.
    replay: Option<Mutex<crate::gui_replay::GuiSnapshot>>,
    /// The attached UI's generation, 0 before the first attach.
    ui_generation: u64,
    /// Runs on every app state set, attached UI or not.
    app_state_hook: Option<Box<dyn Fn(AppState) + Send + Sync>>,
    /// Runs when a pipeline load starts a new item, ahead of its first event.
    item_hook: Option<Box<dyn Fn() + Send + Sync>>,
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
            replay: None,
            ui_generation: 0,
            app_state_hook: None,
            item_hook: None,
        }
    }

    /// For state that must follow item boundaries without a UI (android's
    /// cue engine).
    pub fn with_app_state_hook(mut self, hook: impl Fn(AppState) + Send + Sync + 'static) -> Self {
        self.app_state_hook = Some(Box::new(hook));
        self
    }

    /// For state that must reset per item before the pipeline can reach it
    /// (android's video surface).
    pub fn with_item_hook(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.item_hook = Some(Box::new(hook));
        self
    }

    /// A pipeline load is about to start a new item.
    pub fn item_boundary(&self) {
        if let Some(hook) = &self.item_hook {
            hook();
        }
    }

    /// Records every command so [`Self::attach`] can catch a new UI up.
    pub fn with_replay(mut self) -> Self {
        self.replay = Some(Mutex::new(Default::default()));
        self
    }

    /// A UI came up: replays the recorded state into it. Returns false for a
    /// stale attach (an older generation than the current UI).
    pub fn attach(&mut self, tx: UnboundedSender<UpdateGuiCommand>, generation: u64) -> bool {
        if generation < self.ui_generation {
            return false;
        }
        if let Some(replay) = &self.replay {
            replay.lock().replay(|cmd| {
                let _ = tx.send(cmd);
            });
        }
        self.tx = Some(tx);
        self.ui_generation = generation;
        self.is_visible.set(true);
        true
    }

    /// The UI went away. A detach from a UI already replaced is ignored.
    pub fn detach(&mut self, generation: u64) -> bool {
        if generation != self.ui_generation || self.tx.is_none() {
            return false;
        }
        self.tx = None;
        self.is_visible.set(false);
        true
    }

    fn send(&self, cmd: UpdateGuiCommand) {
        if let Some(replay) = &self.replay {
            replay.lock().record(&cmd);
        }
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

    pub fn init_settings(
        &self,
        config: crate::config::Config,
        config_path: String,
        services: Services,
    ) {
        self.send(UpdateGuiCommand::InitSettings {
            config,
            config_path,
            services,
        });
    }

    /// Returns the the previous window fulscreen state.
    pub fn set_fullscreen(&self, fullscreen: bool) -> bool {
        // no UI to answer (android, detached)
        if self.tx.is_none() {
            return false;
        }
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
        if let Some(hook) = &self.app_state_hook {
            hook(state);
        }
        self.send(UpdateGuiCommand::SetAppState(state));
    }

    pub fn set_load_behind_player(&self, behind: bool) {
        self.send(UpdateGuiCommand::SetLoadBehindPlayer(behind));
    }

    #[cfg(not(target_os = "android"))]
    pub fn set_inspector_sample(&self, sample: InspectorSample) {
        self.send(UpdateGuiCommand::SetInspectorSample(sample.into()));
    }

    pub fn update_playlist(&self, start_idx: i32, length: i32) {
        self.send(UpdateGuiCommand::UpdatePlaylist { start_idx, length });
    }

    fn set_image(&self, img: DecodedImage, typ: ImageType) {
        self.send(UpdateGuiCommand::SetImage {
            typ,
            img: Arc::new(img).into(),
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
        self.send(UpdateGuiCommand::SetVolume { volume, show: true });
    }

    pub fn set_playlist_index(&self, index: i32) {
        self.send(UpdateGuiCommand::SetPlaylistIndex(index));
    }

    pub fn show_toast(&self, kind: UiToastKind, detail: Option<String>, code: &'static str) {
        self.send(UpdateGuiCommand::ShowToastMessage { kind, detail, code });
    }

    pub fn show_bug_report(&self, draft: crate::bug_report::Draft, code: &'static str) {
        self.send(UpdateGuiCommand::ShowBugReport { draft, code });
    }

    pub fn hide_bug_report(&self) {
        self.send(UpdateGuiCommand::HideBugReport);
    }

    pub fn set_soft_keyboard_visible(&self, visible: bool) {
        self.send(UpdateGuiCommand::SetSoftKeyboardVisible(visible));
    }

    pub fn set_playback_state(&mut self, state: GuiPlaybackState) {
        if state != self.playback_state {
            self.send(UpdateGuiCommand::SetPlaybackState(state));
            self.playback_state = state;
        }
    }

    pub fn end_buffering(&mut self) {
        if self.playback_state == GuiPlaybackState::Buffering {
            self.set_playback_state(GuiPlaybackState::Loading);
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

    pub fn transport_from_sender(&self, kind: TransportKind, by: Option<String>) {
        self.send(UpdateGuiCommand::TransportFromSender { kind, by });
    }

    pub fn set_senders(&self, senders: Vec<SenderInfo>) {
        self.send(UpdateGuiCommand::SetSenders(senders));
    }

    pub fn set_playback_rate(&mut self, rate: f32) {
        if rate != self.playback_rate {
            self.send(UpdateGuiCommand::SetPlaybackRate(rate));
            self.playback_rate = rate;
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    pub fn set_updater_state(&self, state: UiUpdaterState) {
        self.send(UpdateGuiCommand::SetUpdateState(state));
    }

    #[cfg(target_os = "android")]
    pub fn set_updater_error(&self, msg: String) {
        self.send(UpdateGuiCommand::SetUpdaterError(msg));
    }

    #[cfg(target_os = "android")]
    pub fn set_update_download_progress(&self, percent: i32) {
        self.send(UpdateGuiCommand::SetUpdateDownloadProgress(percent));
    }

    /// Returns the the previous window visibility state.
    pub fn set_window_visibility(&self, visible: bool) -> bool {
        if visible {
            self.is_visible.note_show();
        }
        // no window, so nothing for a later restore to hide
        if self.tx.is_none() {
            return true;
        }
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

    /// [`Self::set_fullscreen`] without waiting for the answer, see
    /// [`Self::set_window_visibility_detached`].
    pub fn set_fullscreen_detached(&self, fullscreen: bool) {
        if self.tx.is_none() {
            return;
        }
        let (prev_tx, _) = oneshot::channel();
        self.send(UpdateGuiCommand::SetFullscreen {
            fullscreen,
            prev_tx,
        });
    }

    /// [`Self::set_window_visibility`] without waiting for the answer. The
    /// app thread must not block on a GUI thread that may be stuck in a
    /// present or already gone at quit.
    pub fn set_window_visibility_detached(&self, visible: bool) {
        if visible {
            self.is_visible.note_show();
        }
        if self.tx.is_none() {
            return;
        }
        let (prev_tx, _) = oneshot::channel();
        self.send(UpdateGuiCommand::SetWindowVisibility { visible, prev_tx });
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

    /// See [`GuiIsVisible::note_show`].
    pub fn shows(&self) -> u64 {
        self.is_visible.shows()
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

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(5);

    #[test]
    fn a_shutdown_answer_releases_the_teardown() {
        let visible = GuiIsVisible::new();
        let (tx, rx) = oneshot::channel();
        tx.send(()).unwrap();
        assert_eq!(await_player_release(&rx, &visible, visible.shows_processed(), LONG), TeardownWait::Released);
    }

    #[test]
    fn a_declined_shutdown_releases_the_teardown() {
        let visible = GuiIsVisible::new();
        let (tx, rx) = oneshot::channel::<()>();
        drop(tx);
        assert_eq!(await_player_release(&rx, &visible, visible.shows_processed(), LONG), TeardownWait::Released);
    }

    #[test]
    fn a_show_during_the_teardown_ends_the_wait() {
        // The item-change deadlock: the application asked for a show and blocks
        // on the GUI thread, which blocks here on the application.
        let visible = GuiIsVisible::new();
        let shows = visible.shows_processed();
        let (_tx, rx) = oneshot::channel::<()>();
        let app = {
            let visible = visible.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                visible.note_show();
            })
        };
        let start = Instant::now();
        assert_eq!(await_player_release(&rx, &visible, shows, LONG), TeardownWait::ShowRequested);
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
        app.join().unwrap();
    }

    #[test]
    fn a_show_requested_before_the_teardown_but_not_yet_carried_out_ends_the_wait() {
        // The GUI thread reads its count after the application already asked for
        // the show that is queued behind this teardown.
        let visible = GuiIsVisible::new();
        visible.note_show();
        let (_tx, rx) = oneshot::channel::<()>();
        let wait = await_player_release(&rx, &visible, visible.shows_processed(), LONG);
        assert_eq!(wait, TeardownWait::ShowRequested);
    }

    #[test]
    fn a_carried_out_show_is_no_show_in_flight() {
        let visible = GuiIsVisible::new();
        visible.note_show();
        visible.note_show_processed();
        let (_tx, rx) = oneshot::channel::<()>();
        let short = Duration::from_millis(50);
        let wait = await_player_release(&rx, &visible, visible.shows_processed(), short);
        assert_eq!(wait, TeardownWait::TimedOut);
    }

    #[test]
    fn an_unanswered_teardown_times_out() {
        let visible = GuiIsVisible::new();
        let (_tx, rx) = oneshot::channel::<()>();
        let wait = await_player_release(&rx, &visible, visible.shows_processed(), Duration::from_millis(50));
        assert_eq!(wait, TeardownWait::TimedOut);
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UpdateGuiCommand>) -> Vec<UpdateGuiCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn an_attach_catches_the_ui_up_on_what_it_missed() {
        let visible = GuiIsVisible::new();
        let mut gui = GuiController::new(None, visible.clone()).with_replay();
        gui.set_media_title("headless".into());
        gui.set_app_state(AppState::Playing);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(gui.attach(tx, 1));
        assert!(visible.get());
        let cmds = drain(&mut rx);
        assert!(matches!(&cmds[..], [UpdateGuiCommand::SetMediaTitle(t), UpdateGuiCommand::SetAppState(AppState::Playing)] if t == "headless"));
        gui.set_media_title("live".into());
        assert!(matches!(&drain(&mut rx)[..], [UpdateGuiCommand::SetMediaTitle(t)] if t == "live"));
    }

    #[test]
    fn a_detach_from_a_replaced_ui_is_ignored() {
        // The new activity attached before the old one's detach arrived.
        let visible = GuiIsVisible::new();
        let mut gui = GuiController::new(None, visible.clone()).with_replay();
        let (old_tx, _old_rx) = tokio::sync::mpsc::unbounded_channel();
        let (new_tx, mut new_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(gui.attach(old_tx, 1));
        assert!(gui.attach(new_tx, 2));
        assert!(!gui.detach(1));
        assert!(visible.get());
        gui.set_media_title("still here".into());
        assert_eq!(drain(&mut new_rx).len(), 1);
        assert!(gui.detach(2));
        assert!(!visible.get());
        assert!(!gui.detach(2), "a second detach is a no-op");
    }

    #[test]
    fn a_stale_attach_is_refused() {
        let mut gui = GuiController::new(None, GuiIsVisible::new()).with_replay();
        let (new_tx, _new_rx) = tokio::sync::mpsc::unbounded_channel();
        let (old_tx, _old_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(gui.attach(new_tx, 2));
        assert!(!gui.attach(old_tx, 1));
    }

    #[test]
    fn a_volume_restore_shows_no_overlay() {
        let mut gui = GuiController::new(None, GuiIsVisible::new()).with_replay();
        gui.set_volume(0.5);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        gui.attach(tx, 1);
        assert!(matches!(&drain(&mut rx)[..], [UpdateGuiCommand::SetVolume { show: false, .. }]));
    }

    #[test]
    fn only_a_show_counts() {
        let visible = GuiIsVisible::new();
        let gui = GuiController::new(None, visible.clone());
        gui.set_window_visibility(false);
        assert_eq!(visible.shows(), 0);
        gui.set_window_visibility(true);
        assert_eq!(visible.shows(), 1);
        assert_eq!(visible.shows_processed(), 0, "only the GUI thread counts these");
    }
}
