use std::{rc::Rc, sync::Arc};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::message;
use crate::{
    Bridge, CompoundImage, GuiPlaybackState, MainWindow, Message, MessageSender, Operation,
    UiMediaTrack, UiMediaTrackType, UiPlayerVariant, application::PacketOrigin,
    image::DecodedImage, log_if_err, utils::sec_to_string,
};
use fcast_protocol::v3;
use parking_lot::{Condvar, Mutex};
use slint::{ComponentHandle, SharedString, ToSharedString, VecModel};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, error};

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
        let msg_tx = msg_tx.clone();
        move |id: i32, variant: UiMediaTrackType| {
            msg_tx.send(Message::SelectTrack { id, variant });
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

#[derive(Debug)]
pub enum ImageType {
    Preview,
    AudioTrackCover,
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

pub struct GraphDumpData {
    pub trigger: String,
    pub timestamp: String,
    pub graph: remote_pipeline_dbg::render::RenderGraph,
}

/// One row of the inspector's track table.
pub struct InspectorTrackRow {
    pub kind: String,
    pub codec: String,
    pub detail: String,
    pub language: String,
    pub selected: bool,
}

/// The inspector's buffering card: a health summary of the current buffering state. `None` when the
/// source can't answer a buffering query (see `Player::dbg_buffering`).
pub struct InspectorBuffering {
    /// Buffer fill (`0.0..=1.0`) for the meter, relative to the watermarks.
    pub fill_fraction: f32,
    /// e.g. "87%" or "87% (busy)".
    pub fill_label: String,
    /// Buffered-ahead duration, e.g. "2.1 s", or empty when unknown.
    pub ahead_label: String,
    /// e.g. "stream", "download".
    pub mode_label: String,
    /// e.g. "full in 3.2 s", or empty when unknown.
    pub eta_label: String,
}

/// One inspector tick's worth of display data (see `Application::inspector_tick`). Bitrate
/// histories are kbit/s, oldest first.
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
    /// Buffered scrubber regions, as `(start, stop)` timeline fractions.
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
    SetSeekPending(bool),
    SetPlaybackRate(f32),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdateState(crate::UiUpdaterState),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdateDownloadProgress(i32),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    SetUpdaterError(SharedString),
    SetWindowVisibility {
        visible: bool,
        prev_tx: oneshot::Sender<bool>,
    },
    SetAnimation {
        frames: IgnoredDebug<Vec<crate::image::AnimationFrame>>,
    },
    SetGraphDump(IgnoredDebug<GraphDumpData>),
    SetInspectorDumping(bool),
    /// One inspector tick's display data. The bitrate histories are
    /// rendered into SVG polylines on the GUI thread.
    SetInspectorSample(IgnoredDebug<InspectorSample>),
    QuitLoop,
}

type RendererMsgSender = std::sync::mpsc::Sender<RendererMessage>;
pub type UpdateGuiSender = UnboundedSender<UpdateGuiCommand>;

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
    is_visible: GuiIsVisible,
}

impl GuiController {
    pub fn new(tx: Option<UnboundedSender<UpdateGuiCommand>>, is_visible: GuiIsVisible) -> Self {
        Self {
            tx,
            playback_state: GuiPlaybackState::default(),
            playback_rate: -1.0,
            is_live: false,
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

    pub fn set_app_state(&self, state: crate::AppState) {
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

    /// Push the scrubber's buffered regions (fractions `0.0..=1.0` of the
    /// timeline, `start` < `stop`).
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

    pub fn set_animation(&self, frames: Vec<crate::image::AnimationFrame>) {
        self.send(UpdateGuiCommand::SetAnimation {
            frames: frames.into(),
        });
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
        UpdateGuiCommand::SetAppState(state) => bridge.set_app_state(state),
        UpdateGuiCommand::UpdatePlaylist { start_idx, length } => {
            bridge.set_playlist_idx(start_idx);
            bridge.set_playlist_idx(length);
        }
        UpdateGuiCommand::SetImage { typ, img } => {
            bridge.set_animation_frames(slint::ModelRc::default());
            match typ {
                ImageType::Preview => bridge.set_image_preview(img.as_compound()),
                ImageType::AudioTrackCover => {
                    bridge.set_audio_track_cover(img.as_compound());
                    let _ = renderer_tx.send(RendererMessage::CreateBluredAudioTrackCover(img.0));
                }
            }
        }
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
        UpdateGuiCommand::SetPlayerType(typ) => bridge.set_player_variant(typ),
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
        UpdateGuiCommand::ClearVideoOverlays => {
            let _ = renderer_tx.send(RendererMessage::ClearVideoOverlays);
            ui.window().request_redraw();
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
            bridge.set_image_preview(CompoundImage::default());
            clear_audio_covers(&bridge, renderer_tx);
            bridge.set_animation_frames(slint::ModelRc::default());
        }
        UpdateGuiCommand::SetIsLive(is_live) => bridge.set_is_live(is_live),
        UpdateGuiCommand::SetSeekPending(pending) => bridge.set_seek_pending(pending),
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
        UpdateGuiCommand::SetAnimation { frames } => {
            bridge.set_image_preview(CompoundImage::default());
            bridge.set_animation_frames(
                Rc::new(slint::VecModel::from_iter(frames.0.into_iter().map(
                    |frame| crate::UiAnimationFrame {
                        img: slint::Image::from_rgba8(frame.image),
                        delay: frame.delay_ms,
                    },
                )))
                .into(),
            );
            bridge.set_current_animation_frame(0);
        }
        UpdateGuiCommand::SetGraphDump(dump) => set_graph_dump(&ui, dump.0),
        UpdateGuiCommand::SetInspectorDumping(dumping) => {
            ui.global::<crate::InspectorState>().set_dumping(dumping);
        }
        UpdateGuiCommand::SetInspectorSample(sample) => set_inspector_sample(&ui, sample.0),
        UpdateGuiCommand::QuitLoop => (),
    }
}

/// Push one inspector sample into the UI: sparkline paths (SVG polylines
/// over the fixed 300x100 viewbox, both scaled to the shared peak so video
/// and audio are comparable), the track table and the info line lists.
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

    let peak = video_kbps
        .iter()
        .chain(audio_kbps)
        .fold(1.0f32, |m, v| m.max(*v));

    let state = ui.global::<crate::InspectorState>();
    state.set_video_bitrate_path(polyline(video_kbps, peak));
    state.set_audio_bitrate_path(polyline(audio_kbps, peak));
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
    state.set_bitrate_peak_label(fmt_rate(peak).into());
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
    use remote_pipeline_dbg::render::TextAlign;

    fn color(rgba: [u8; 4]) -> slint::Color {
        slint::Color::from_argb_u8(rgba[3], rgba[0], rgba[1], rgba[2])
    }

    fn brush(rgba: Option<[u8; 4]>) -> slint::Brush {
        slint::Brush::SolidColor(rgba.map_or(slint::Color::from_argb_u8(0, 0, 0, 0), color))
    }

    let paths: Vec<crate::UiGraphPath> = dump
        .graph
        .paths
        .iter()
        .map(|p| crate::UiGraphPath {
            commands: p.commands.as_str().into(),
            fill: brush(p.fill),
            stroke: brush(p.stroke),
            stroke_width: p.stroke_width,
        })
        .collect();
    let texts: Vec<crate::UiGraphText> = dump
        .graph
        .texts
        .iter()
        .map(|t| crate::UiGraphText {
            x: t.x,
            y: t.y,
            size: t.size,
            text: t.text.as_str().into(),
            color: color(t.color),
            align: match t.align {
                TextAlign::Left => 0,
                TextAlign::Center => 1,
                TextAlign::Right => 2,
            },
        })
        .collect();

    let state = ui.global::<crate::InspectorState>();
    state.set_graph(crate::GraphDump {
        trigger: dump.trigger.into(),
        timestamp: dump.timestamp.into(),
        width: dump.graph.width,
        height: dump.graph.height,
        paths: Rc::new(slint::VecModel::from(paths)).into(),
        texts: Rc::new(slint::VecModel::from(texts)).into(),
    });
    state.set_have_graph(true);
    state.set_dumping(false);
}

pub fn spawn_command_handler(
    ui_weak: slint::Weak<MainWindow>,
    mut cmd_rx: UnboundedReceiver<UpdateGuiCommand>,
    renderer_tx: RendererMsgSender,
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
                handle_command(ui, cmd, &renderer_tx);
            } else {
                debug!("Stopping");
                break;
            }
        }
    })
    .expect("Failed to spawn GUI command handler");
}
