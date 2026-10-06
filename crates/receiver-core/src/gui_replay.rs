//! The last value of every piece of state the GUI command stream sets, so a
//! UI attaching to a running core (android, ANDROID-BOOT-START-PLAN.md
//! section 4) is caught up in one burst instead of showing defaults.

use std::sync::Arc;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
use crate::ui_types::UiUpdaterState;
use crate::{
    gui::{ImageType, SenderInfo, UpdateGuiCommand},
    image::DecodedImage,
    ui_types::{AppState, GuiPlaybackState, QrCode, UiMediaTrack, UiPlayerVariant},
};

#[derive(Debug, Clone, Default, PartialEq)]
struct Tracks {
    videos: Option<Vec<UiMediaTrack>>,
    audios: Option<Vec<UiMediaTrack>>,
    subtitles: Option<Vec<UiMediaTrack>>,
}

#[derive(Default)]
pub struct GuiSnapshot {
    connected: i32,
    starting_up: Option<bool>,
    port_conflict: Option<u16>,
    system_tray: bool,
    connection: Option<(QrCode, String)>,
    device_name: Option<String>,
    senders: Option<Vec<SenderInfo>>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    updater_state: Option<UiUpdaterState>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    updater_progress: Option<i32>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    updater_error: Option<String>,
    player_type: Option<UiPlayerVariant>,
    tracks: Option<Tracks>,
    track_ids: Option<(i32, i32, i32)>,
    title: Option<String>,
    artist: Option<String>,
    playlist_len: Option<i32>,
    playlist_idx: Option<i32>,
    preview: Option<Arc<DecodedImage>>,
    cover: Option<Arc<DecodedImage>>,
    image_via_player: Option<bool>,
    is_live: Option<bool>,
    rate: Option<f32>,
    volume: Option<f32>,
    progress: Option<(f32, f32)>,
    // Reused buffer: the ranges arrive every progress tick.
    buffered: Vec<(f32, f32)>,
    buffered_set: bool,
    seek_pending: Option<bool>,
    backoff: Option<(u64, u64)>,
    playback_state: Option<GuiPlaybackState>,
    app_state: Option<AppState>,
}

impl GuiSnapshot {
    /// Mirrors what receiver-ui's `handle_command` leaves behind. No wildcard
    /// arm: a new command must decide here whether it is state.
    pub fn record(&mut self, cmd: &UpdateGuiCommand) {
        use UpdateGuiCommand as C;
        match cmd {
            C::DeviceConnected => self.connected += 1,
            C::DeviceDisconnected => self.connected = (self.connected - 1).max(0),
            C::SetAppState(state) => self.app_state = Some(*state),
            C::UpdatePlaylist { start_idx, length } => {
                self.playlist_idx = Some(*start_idx);
                self.playlist_len = Some(*length);
            }
            C::SetPlaylistIndex(idx) => self.playlist_idx = Some(*idx),
            C::SetImage { typ, img } => match typ {
                ImageType::Preview => self.preview = Some(img.0.clone()),
                ImageType::AudioTrackCover => self.cover = Some(img.0.clone()),
            },
            C::UpdatePlaybackProgress {
                progress_s,
                duration_s,
            } => self.progress = Some((*progress_s, *duration_s)),
            C::SetBufferedRanges(ranges) => {
                self.buffered.clone_from(ranges);
                self.buffered_set = true;
            }
            C::SetMediaTitle(title) => self.title = Some(title.clone()),
            C::SetArtistName(name) => self.artist = Some(name.clone()),
            C::ClearAudioCovers => self.cover = None,
            C::ClearCommonPlaybackState => {
                self.cover = None;
                self.progress = Some((0.0, 0.0));
                self.buffered.clear();
                self.buffered_set = true;
            }
            C::ClearImageState => {
                self.preview = None;
                self.cover = None;
            }
            C::SetPlayerType(typ) => self.player_type = Some(*typ),
            C::SetTracks {
                videos,
                audios,
                subtitles,
            } => {
                self.tracks = Some(Tracks {
                    videos: videos.clone(),
                    audios: audios.clone(),
                    subtitles: subtitles.clone(),
                })
            }
            C::SetTrackIds {
                video,
                audio,
                subtitle,
            } => self.track_ids = Some((*video, *audio, *subtitle)),
            C::SetConnectionDetails { qr_code, addrs } => {
                self.connection = Some((qr_code.0.clone(), addrs.clone()))
            }
            C::SetLocalDeviceName(name) => self.device_name = Some(name.clone()),
            C::SetVolume { volume, .. } => self.volume = Some(*volume),
            C::SetPlaybackState(state) => self.playback_state = Some(*state),
            C::SetImageViaPlayer(via) => self.image_via_player = Some(*via),
            C::SetIsLive(live) => self.is_live = Some(*live),
            C::SetSeekPending(pending) => self.seek_pending = Some(*pending),
            C::SetSenders(senders) => self.senders = Some(senders.clone()),
            C::SetSourceBackoff {
                remaining_ms,
                total_ms,
            } => self.backoff = Some((*remaining_ms, *total_ms)),
            C::SetPlaybackRate(rate) => self.rate = Some(*rate),
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
            C::SetUpdateState(state) => self.updater_state = Some(*state),
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
            C::SetUpdateDownloadProgress(p) => self.updater_progress = Some(*p),
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
            C::SetUpdaterError(err) => self.updater_error = Some(err.clone()),
            C::ShowPortConflict { port } => self.port_conflict = Some(*port),
            C::SetStartingUp(starting_up) => {
                self.starting_up = Some(*starting_up);
                // the UI drops the conflict prompt with the startup screen
                if !*starting_up {
                    self.port_conflict = None;
                }
            }
            C::ShowSystemTray => self.system_tray = true,
            // One-offs and requests with a reply channel, nothing to restore.
            // InitSettings is re-pushed from the live config on attach.
            C::SetFullscreen { .. }
            | C::SetWindowVisibility { .. }
            | C::ClearVideoOverlays
            | C::ShowToastMessage { .. }
            | C::ShowBugReport { .. }
            | C::HideBugReport
            | C::SetSoftKeyboardVisible(_)
            | C::TransportFromSender { .. }
            | C::InitSettings { .. }
            | C::QuitLoop => {}
            #[cfg(not(target_os = "android"))]
            C::SetGraphDump(_) | C::SetInspectorDumping(_) | C::SetInspectorSample(_) => {}
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            C::RunOnMainThread(_) => {}
        }
    }

    /// Emits the recorded state, app state last so a view comes up with its
    /// data already in place.
    pub fn replay(&self, mut emit: impl FnMut(UpdateGuiCommand)) {
        use UpdateGuiCommand as C;
        if let Some(starting_up) = self.starting_up {
            emit(C::SetStartingUp(starting_up));
        }
        if let Some(port) = self.port_conflict {
            emit(C::ShowPortConflict { port });
        }
        if self.system_tray {
            emit(C::ShowSystemTray);
        }
        if let Some((qr_code, addrs)) = &self.connection {
            emit(C::SetConnectionDetails {
                qr_code: qr_code.clone().into(),
                addrs: addrs.clone(),
            });
        }
        if let Some(name) = &self.device_name {
            emit(C::SetLocalDeviceName(name.clone()));
        }
        for _ in 0..self.connected {
            emit(C::DeviceConnected);
        }
        if let Some(senders) = &self.senders {
            emit(C::SetSenders(senders.clone()));
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
        {
            if let Some(state) = self.updater_state {
                emit(C::SetUpdateState(state));
            }
            if let Some(p) = self.updater_progress {
                emit(C::SetUpdateDownloadProgress(p));
            }
            if let Some(err) = &self.updater_error {
                emit(C::SetUpdaterError(err.clone()));
            }
        }
        if let Some(typ) = self.player_type {
            emit(C::SetPlayerType(typ));
        }
        if let Some(t) = &self.tracks {
            emit(C::SetTracks {
                videos: t.videos.clone(),
                audios: t.audios.clone(),
                subtitles: t.subtitles.clone(),
            });
        }
        if let Some((video, audio, subtitle)) = self.track_ids {
            emit(C::SetTrackIds {
                video,
                audio,
                subtitle,
            });
        }
        if let Some(title) = &self.title {
            emit(C::SetMediaTitle(title.clone()));
        }
        if let Some(artist) = &self.artist {
            emit(C::SetArtistName(artist.clone()));
        }
        match (self.playlist_idx, self.playlist_len) {
            (Some(start_idx), Some(length)) => emit(C::UpdatePlaylist { start_idx, length }),
            (Some(idx), None) => emit(C::SetPlaylistIndex(idx)),
            _ => {}
        }
        if let Some(img) = &self.preview {
            emit(C::SetImage {
                typ: ImageType::Preview,
                img: img.clone().into(),
            });
        }
        if let Some(img) = &self.cover {
            emit(C::SetImage {
                typ: ImageType::AudioTrackCover,
                img: img.clone().into(),
            });
        }
        if let Some(via) = self.image_via_player {
            emit(C::SetImageViaPlayer(via));
        }
        if let Some(live) = self.is_live {
            emit(C::SetIsLive(live));
        }
        if let Some(rate) = self.rate {
            emit(C::SetPlaybackRate(rate));
        }
        if let Some(volume) = self.volume {
            // restored, not changed: no volume overlay
            emit(C::SetVolume {
                volume,
                show: false,
            });
        }
        if let Some((progress_s, duration_s)) = self.progress {
            emit(C::UpdatePlaybackProgress {
                progress_s,
                duration_s,
            });
        }
        if self.buffered_set {
            emit(C::SetBufferedRanges(self.buffered.clone()));
        }
        if let Some(pending) = self.seek_pending {
            emit(C::SetSeekPending(pending));
        }
        if let Some((remaining_ms, total_ms)) = self.backoff {
            emit(C::SetSourceBackoff {
                remaining_ms,
                total_ms,
            });
        }
        if let Some(state) = self.playback_state {
            emit(C::SetPlaybackState(state));
        }
        if let Some(state) = self.app_state {
            emit(C::SetAppState(state));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The replayable fields of the UI, applied the way receiver-ui's
    /// `handle_command` applies them. Images are compared by allocation.
    #[derive(Debug, Default, PartialEq)]
    struct UiModel {
        connected: i32,
        starting_up: bool,
        port_conflict: Option<u16>,
        system_tray: bool,
        connection: Option<(QrCode, String)>,
        device_name: String,
        senders: Vec<SenderInfo>,
        player_type: UiPlayerVariant,
        tracks: Tracks,
        track_ids: (i32, i32, i32),
        title: String,
        artist: String,
        playlist: (i32, i32),
        preview: Option<usize>,
        cover: Option<usize>,
        image_via_player: bool,
        is_live: bool,
        rate: f32,
        volume: f32,
        volume_overlay: bool,
        progress: (f32, f32),
        buffered: Vec<(f32, f32)>,
        seek_pending: bool,
        backoff: (u64, u64),
        playback_state: GuiPlaybackState,
        app_state: AppState,
    }

    fn img_id(img: &Arc<DecodedImage>) -> usize {
        Arc::as_ptr(img) as usize
    }

    impl UiModel {
        fn apply(&mut self, cmd: UpdateGuiCommand) {
            use UpdateGuiCommand as C;
            match cmd {
                C::DeviceConnected => self.connected += 1,
                C::DeviceDisconnected => self.connected = (self.connected - 1).max(0),
                C::SetAppState(s) => self.app_state = s,
                C::UpdatePlaylist { start_idx, length } => self.playlist = (start_idx, length),
                C::SetPlaylistIndex(idx) => self.playlist.0 = idx,
                C::SetImage { typ, img } => match typ {
                    ImageType::Preview => self.preview = Some(img_id(&img.0)),
                    ImageType::AudioTrackCover => self.cover = Some(img_id(&img.0)),
                },
                C::UpdatePlaybackProgress {
                    progress_s,
                    duration_s,
                } => self.progress = (progress_s, duration_s),
                C::SetBufferedRanges(r) => self.buffered = r,
                C::SetMediaTitle(t) => self.title = t,
                C::SetArtistName(a) => self.artist = a,
                C::ClearAudioCovers => self.cover = None,
                C::ClearCommonPlaybackState => {
                    self.cover = None;
                    self.progress = (0.0, 0.0);
                    self.buffered.clear();
                }
                C::ClearImageState => {
                    self.preview = None;
                    self.cover = None;
                }
                C::SetPlayerType(t) => self.player_type = t,
                C::SetTracks {
                    videos,
                    audios,
                    subtitles,
                } => {
                    self.tracks = Tracks {
                        videos,
                        audios,
                        subtitles,
                    }
                }
                C::SetTrackIds {
                    video,
                    audio,
                    subtitle,
                } => self.track_ids = (video, audio, subtitle),
                C::SetConnectionDetails { qr_code, addrs } => {
                    self.connection = Some((qr_code.0, addrs))
                }
                C::SetLocalDeviceName(n) => self.device_name = n,
                C::SetVolume { volume, show } => {
                    self.volume = volume;
                    self.volume_overlay |= show;
                }
                C::SetPlaybackState(s) => self.playback_state = s,
                C::SetImageViaPlayer(v) => self.image_via_player = v,
                C::SetIsLive(l) => self.is_live = l,
                C::SetSeekPending(p) => self.seek_pending = p,
                C::SetSenders(s) => self.senders = s,
                C::SetSourceBackoff {
                    remaining_ms,
                    total_ms,
                } => self.backoff = (remaining_ms, total_ms),
                C::SetPlaybackRate(r) => self.rate = r,
                C::ShowPortConflict { port } => self.port_conflict = Some(port),
                C::SetStartingUp(s) => {
                    self.starting_up = s;
                    if !s {
                        self.port_conflict = None;
                    }
                }
                C::ShowSystemTray => self.system_tray = true,
                _ => {}
            }
        }
    }

    fn test_image() -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            id: 0,
            image: crate::image::RgbaImage::new(1, 1),
            orientation: ::image::metadata::Orientation::NoTransforms,
            format: "png",
        })
    }

    fn track(id: i32) -> UiMediaTrack {
        UiMediaTrack {
            id,
            ..Default::default()
        }
    }

    /// One command of every recordable kind, picked by `k`, values from `v`.
    fn command(k: u32, v: u32, images: &[Arc<DecodedImage>]) -> UpdateGuiCommand {
        use UpdateGuiCommand as C;
        let f = v as f32;
        let i = v as i32;
        let states = [AppState::Idle, AppState::LoadingMedia, AppState::Playing];
        let pb = [
            GuiPlaybackState::Idle,
            GuiPlaybackState::Loading,
            GuiPlaybackState::Playing,
            GuiPlaybackState::Paused,
        ];
        let variants = [UiPlayerVariant::Video, UiPlayerVariant::Audio, UiPlayerVariant::Image];
        let img = images[v as usize % images.len()].clone();
        match k % 33 {
            0 => C::DeviceConnected,
            1 => C::DeviceDisconnected,
            2 => C::SetAppState(states[v as usize % states.len()]),
            3 => C::UpdatePlaylist {
                start_idx: i,
                length: i + 3,
            },
            4 => C::SetPlaylistIndex(i),
            5 => C::SetImage {
                typ: ImageType::Preview,
                img: img.into(),
            },
            6 => C::SetImage {
                typ: ImageType::AudioTrackCover,
                img: img.into(),
            },
            7 => C::UpdatePlaybackProgress {
                progress_s: f,
                duration_s: f * 2.0,
            },
            8 => C::SetBufferedRanges(vec![(0.0, f / 100.0); (v % 3) as usize]),
            9 => C::SetMediaTitle(format!("title {v}")),
            10 => C::SetArtistName(format!("artist {v}")),
            11 => C::ClearAudioCovers,
            12 => C::ClearCommonPlaybackState,
            13 => C::ClearImageState,
            14 => C::SetPlayerType(variants[v as usize % variants.len()]),
            15 => C::SetTracks {
                videos: (v % 2 == 0).then(|| vec![track(i)]),
                audios: Some(vec![track(i), track(i + 1)]),
                subtitles: None,
            },
            16 => C::SetTrackIds {
                video: i,
                audio: i + 1,
                subtitle: -1,
            },
            17 => C::SetConnectionDetails {
                qr_code: QrCode {
                    size: v,
                    dark: vec![v % 2 == 0],
                }
                .into(),
                addrs: format!("10.0.0.{v}"),
            },
            18 => C::SetLocalDeviceName(format!("dev {v}")),
            19 => C::SetVolume {
                volume: f / 100.0,
                show: true,
            },
            20 => C::SetPlaybackState(pb[v as usize % pb.len()]),
            21 => C::SetImageViaPlayer(v % 2 == 0),
            22 => C::SetIsLive(v % 2 == 0),
            23 => C::SetSeekPending(v % 2 == 0),
            24 => C::SetSenders(vec![SenderInfo {
                display_name: format!("s{v}"),
                ..Default::default()
            }]),
            25 => C::SetSourceBackoff {
                remaining_ms: v as u64,
                total_ms: 1000,
            },
            26 => C::SetPlaybackRate(f / 10.0),
            27 => C::ShowPortConflict { port: v as u16 },
            28 => C::SetStartingUp(v % 2 == 0),
            29 => C::ShowSystemTray,
            30 => C::ClearVideoOverlays,
            31 => C::HideBugReport,
            _ => C::TransportFromSender {
                kind: crate::gui::TransportKind::Seek,
                by: None,
            },
        }
    }

    /// xorshift, so the sequences are reproducible without a dependency
    fn next(state: &mut u64) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 16) as u32
    }

    #[test]
    fn a_replay_rebuilds_the_ui_the_full_stream_built() {
        let images: Vec<_> = (0..3).map(|_| test_image()).collect();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..2000 {
            let mut live = UiModel::default();
            let mut snapshot = GuiSnapshot::default();
            let len = 1 + next(&mut seed) % 60;
            for _ in 0..len {
                let (k, v) = (next(&mut seed), next(&mut seed) % 50);
                snapshot.record(&command(k, v, &images));
                live.apply(command(k, v, &images));
            }
            let mut attached = UiModel::default();
            snapshot.replay(|cmd| attached.apply(cmd));
            // The live UI may have shown the volume overlay, a restore never does.
            assert!(!attached.volume_overlay, "round {round}");
            attached.volume_overlay = live.volume_overlay;
            assert_eq!(attached, live, "round {round}");
        }
    }

    #[test]
    fn one_offs_are_never_replayed() {
        let mut snapshot = GuiSnapshot::default();
        snapshot.record(&UpdateGuiCommand::HideBugReport);
        snapshot.record(&UpdateGuiCommand::ClearVideoOverlays);
        snapshot.record(&UpdateGuiCommand::SetSoftKeyboardVisible(true));
        snapshot.record(&UpdateGuiCommand::TransportFromSender {
            kind: crate::gui::TransportKind::Pause,
            by: None,
        });
        let mut emitted = 0;
        snapshot.replay(|_| emitted += 1);
        assert_eq!(emitted, 0);
    }

    #[test]
    fn the_app_state_comes_last() {
        let mut snapshot = GuiSnapshot::default();
        snapshot.record(&UpdateGuiCommand::SetAppState(AppState::Playing));
        snapshot.record(&UpdateGuiCommand::SetMediaTitle("t".into()));
        snapshot.record(&UpdateGuiCommand::SetPlayerType(UiPlayerVariant::Audio));
        let mut last = None;
        snapshot.replay(|cmd| last = Some(cmd));
        assert!(matches!(last, Some(UpdateGuiCommand::SetAppState(AppState::Playing))));
    }
}
