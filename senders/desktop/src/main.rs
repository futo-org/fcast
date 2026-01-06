#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// TODO: incremental file listing

use anyhow::{Context, Result, bail};
use clap::Parser;
#[cfg(target_os = "macos")]
use desktop_sender::macos;
use desktop_sender::{FetchEvent, device_info_parser, file_server::FileServer};
use directories::{BaseDirs, UserDirs};
use fcast_sender_sdk::{
    context::CastContext,
    device::{self, DeviceFeature, DeviceInfo},
};
use gst_video::prelude::*;
#[cfg(target_os = "windows")]
use mcore::VideoSource;
use mcore::{
    AudioSource, Event, FileSystemEntry, MediaFileEntry, RootDirType, ShouldQuit,
    transmission::WhepSink,
};
use serde::{Deserialize, Serialize};
use slint::{Model, ToSharedString};
use std::{
    collections::HashMap,
    fmt::Write,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    runtime::Runtime,
    sync::mpsc::{Sender, UnboundedSender, channel},
};
use tracing::{Instrument, debug, error, level_filters::LevelFilter, warn};
use tracing_subscriber::{
    Layer, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
};

use desktop_sender::slint_generated::*;

#[cfg(not(any(target_os = "windows", all(target_arch = "aarch64", target_os = "linux"))))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(any(target_os = "windows", all(target_arch = "aarch64", target_os = "linux"))))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

const MAX_VEC_LOG_ENTRIES: usize = 1500;
const MIN_TIME_BETWEEN_SEEKS: Duration = Duration::from_millis(200);
const MIN_TIME_BETWEEN_VOLUME_CHANGES: Duration = Duration::from_millis(75);
const DEFAULT_FILE_SERVER_PORT: u16 = 0;
const DEFAULT_MIRRORING_SERVER_PORT: u16 = 0;

pub type ProducerId = String;

#[derive(Debug, Clone)]
struct Canceler(Arc<AtomicBool>);

impl Canceler {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(self) {
        self.0.store(true, atomic::Ordering::Relaxed);
    }

    pub fn is_canceled(&self) -> bool {
        self.0.load(atomic::Ordering::Relaxed)
    }
}

async fn list_directory(
    canceler: Canceler,
    id: u32,
    path: PathBuf,
    event_tx: UnboundedSender<Event>,
) -> Result<()> {
    let mut dir_entries = tokio::fs::read_dir(&path).await?;
    let mut entries = Vec::new();
    while let Some(entry) = dir_entries.next_entry().await? {
        if canceler.is_canceled() {
            debug!(?path, "Directory listing was canceled");
            return Ok(());
        }

        let Ok(file_type) = entry.file_type().await else {
            continue;
        };

        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };

        // Ignore all hidden entries
        #[cfg(not(target_os = "windows"))]
        if name.starts_with('.') {
            continue;
        }

        if file_type.is_dir() || file_type.is_file() {
            entries.push(FileSystemEntry {
                name,
                is_file: file_type.is_file(),
            });
        }
    }

    event_tx.send(Event::DirectoryListing { id, entries })?;

    Ok(())
}

async fn process_files(
    canceler: Canceler,
    id: u32,
    mut root_path: PathBuf,
    files: Vec<String>,
    event_tx: UnboundedSender<Event>,
) -> Result<()> {
    let mut media_files = Vec::new();
    for name in files {
        if canceler.is_canceled() {
            debug!(?root_path, "File listing was canceled");
            return Ok(());
        }

        root_path.push(&name);

        let Ok(mut file) = tokio::fs::File::open(&root_path).await else {
            root_path.pop();
            continue;
        };

        let mut buf = [0u8; 64];
        let Ok(bytes_read) = file.read(&mut buf).await else {
            root_path.pop();
            continue;
        };

        if let Some(inferred) = desktop_sender::infer::infer_type(bytes_read, &buf) {
            media_files.push(MediaFileEntry {
                name,
                mime_type: inferred.mime_type,
            });
        }

        root_path.pop();
    }

    if !media_files.is_empty() {
        event_tx.send(Event::FilesListing {
            id,
            entries: media_files,
        })?;
    }

    Ok(())
}

type DirectoryId = i32;
type FileId = i32;

struct IdGenerator(i32);

impl IdGenerator {
    pub fn new() -> Self {
        Self(i32::MIN)
    }

    pub fn next(&mut self) -> i32 {
        self.0 += 1;
        self.0 - 1
    }
}

#[derive(Debug)]
struct LocalMediaDataState {
    pub root: PathBuf,
    pub directories: HashMap<DirectoryId, String>,
    pub files: HashMap<FileId, MediaFileEntry>,
}

use mcore::preview::PreviewPipeline;

#[derive(Debug)]
enum SessionSpecificState {
    Idle,
    Mirroring {
        tx_sink: Option<WhepSink>,
        video_source_fetcher_tx: Sender<FetchEvent>,
        our_source_url: Option<String>,
        video_sources: Vec<(usize, PreviewPipeline)>,
    },
    LocalMedia {
        current_id: u32,
        file_server: FileServer,
        data: LocalMediaDataState,
        listing_canceler: Option<Canceler>,
    },
    YtDlp {
        sources: Option<Vec<mcore::yt_dlp::YtDlpSource>>,
        fetcher_quit_tx: Option<tokio::sync::oneshot::Sender<()>>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename = "file_server")]
struct FileServerSettings {
    pub port: Option<u16>,
}

impl Default for FileServerSettings {
    fn default() -> Self {
        Self {
            port: Some(DEFAULT_FILE_SERVER_PORT),
        }
    }
}

impl FileServerSettings {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_FILE_SERVER_PORT)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename = "mirroring")]
struct MirroringSettings {
    pub server_port: Option<u16>,
    // TODO:
    // pub video_codecs: Option<Vec<VideoCodec>>,
    // pub audio_codecs: Option<Vec<VideoCodec>>,
}

impl Default for MirroringSettings {
    fn default() -> Self {
        Self {
            server_port: Some(DEFAULT_MIRRORING_SERVER_PORT),
        }
    }
}

impl MirroringSettings {
    pub fn server_port(&self) -> u16 {
        self.server_port.unwrap_or(DEFAULT_MIRRORING_SERVER_PORT)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Settings {
    file_server: Option<FileServerSettings>,
    mirroring: Option<MirroringSettings>,
}

impl Settings {
    fn file_server(&self) -> FileServerSettings {
        self.file_server
            .clone()
            .unwrap_or(FileServerSettings::default())
    }

    fn set_file_server_port(&mut self, port: u16) {
        match self.file_server.as_mut() {
            Some(file_server) => file_server.port = Some(port),
            None => {
                let mut file_server = FileServerSettings::default();
                file_server.port = Some(port);
                self.file_server = Some(file_server);
            }
        }
    }

    fn mirroring(&self) -> MirroringSettings {
        self.mirroring
            .clone()
            .unwrap_or(MirroringSettings::default())
    }

    fn set_mirroring_server_port(&mut self, port: u16) {
        match self.mirroring.as_mut() {
            Some(mirroring) => mirroring.server_port = Some(port),
            None => {
                let mut mirroring = MirroringSettings::default();
                mirroring.server_port = Some(port);
                self.mirroring = Some(mirroring);
            }
        }
    }
}

struct SessionState {
    pub device: Arc<dyn device::CastingDevice>,
    pub local_address: Option<fcast_sender_sdk::IpAddr>,
    pub volume: f64,
    pub time: f64,
    pub duration: f64,
    pub speed: f64,
    pub playback_state: UiPlaybackState,
    pub specific: SessionSpecificState,
    pub previous_seek: Instant,
    pub previous_volume_change: Instant,
}

struct Application {
    cast_ctx: CastContext,
    ui_weak: slint::Weak<MainWindow>,
    event_tx: UnboundedSender<Event>,
    devices: HashMap<String, DeviceInfo>,
    current_session_id: usize,
    current_local_media_id: u32,
    user_dirs: Option<UserDirs>,
    base_dirs: Option<BaseDirs>,
    session_state: Option<SessionState>,
    settings: Settings,
}

async fn spawn_video_source_fetcher(event_tx: UnboundedSender<Event>) -> Sender<FetchEvent> {
    #[allow(unused_mut)]
    let (video_source_fetcher_tx, mut video_source_fetcher_rx) = channel::<FetchEvent>(10);

    #[cfg(target_os = "linux")]
    {
        tokio::spawn(async move {
            desktop_sender::linux::video_source_fetch_worker(video_source_fetcher_rx, event_tx)
                .await;
        });
    }

    #[cfg(target_os = "macos")]
    {
        tokio::spawn(async move {
            loop {
                let Some(event) = video_source_fetcher_rx.recv().await else {
                    error!("Failed to receive new video source fetcher event");
                    break;
                };

                match event {
                    FetchEvent::Fetch => match macos::get_video_sources() {
                        Ok(sources) => {
                            event_tx
                                .send(Event::VideosAvailable(
                                    sources
                                        .into_iter()
                                        .enumerate()
                                        .map(|(idx, src)| (idx, src))
                                        .collect(),
                                ))
                                .expect("event loop is not running");
                        }
                        Err(err) => {
                            error!("Failed to get video sources: {err}");
                        }
                    },
                    FetchEvent::Quit => break,
                }
            }

            debug!("Video source fetch loop quit");
        });
    }

    #[cfg(target_os = "windows")]
    {
        tokio::spawn(async move {
            loop {
                let Some(event) = video_source_fetcher_rx.recv().await else {
                    error!("Failed to receive new video source fetcher event");
                    break;
                };

                match event {
                    FetchEvent::Fetch => {
                        use gst::prelude::*;
                        let Some(dev_provider) =
                            gst::DeviceProviderFactory::by_name("d3d11screencapturedeviceprovider")
                        else {
                            error!("Failed to create `d3d11screencapturedeviceprovider`");
                            continue;
                        };

                        if let Err(err) = dev_provider.start() {
                            error!("Failed to start d3d11 device provider: {err}");
                            continue;
                        }
                        let devs = dev_provider.devices();
                        dev_provider.stop();

                        let mut converted_devs = Vec::new();

                        for (idx, dev) in devs.iter().enumerate() {
                            let Some(props) = dev.properties() else {
                                error!("Could not get device properties");
                                continue;
                            };
                            let name = dev.display_name().to_string();
                            let handle = match props.get::<u64>("device.hmonitor") {
                                Ok(handle) => handle,
                                Err(err) => {
                                    error!(
                                        "Failed to get the `device.hmonitor` property from the device: {err}"
                                    );
                                    continue;
                                }
                            };
                            converted_devs.push((idx, VideoSource::D3d11Monitor { name, handle }));
                        }

                        event_tx
                            .send(Event::VideosAvailable(converted_devs))
                            .expect("event loop is not running");
                    }
                    FetchEvent::Quit => break,
                }
            }

            debug!("Video source fetch loop quit");
        });
    }

    video_source_fetcher_tx
}

impl Application {
    /// Must be called from a tokio runtime.
    pub fn new(ui_weak: slint::Weak<MainWindow>, event_tx: UnboundedSender<Event>) -> Result<Self> {
        let cast_ctx = CastContext::new()?;
        cast_ctx.start_discovery(Arc::new(mcore::Discoverer::new(event_tx.clone())));

        Ok(Self {
            cast_ctx,
            ui_weak,
            event_tx,
            devices: HashMap::new(),
            current_session_id: 0,
            current_local_media_id: 0,
            session_state: None,
            user_dirs: UserDirs::new(),
            settings: Settings::default(),
            base_dirs: BaseDirs::new(),
        })
    }

    // TODO: rename to stop_session maybe?
    fn disconnect_device(&mut self, device: Arc<dyn device::CastingDevice>, stop_playback: bool) {
        tokio::spawn(async move {
            if stop_playback {
                if let Err(err) = device.stop_playback() {
                    error!(?err, "Failed to stop playback");
                }
                // NOTE: Instead of waiting for the PlaybackState::Idle event in the main loop we just sleep here
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if let Err(err) = device.disconnect() {
                error!(?err, "Failed to disconnect from device");
            }
        });
    }

    async fn end_session_no_disconnect(&mut self) -> Result<()> {
        if let Some(session) = self.session_state.as_mut() {
            session.device.stop_playback()?;

            if let SessionSpecificState::Mirroring {
                tx_sink,
                video_source_fetcher_tx,
                ..
            } = &mut session.specific
            {
                if let Some(mut tx_sink) = tx_sink.take() {
                    tx_sink.shutdown();
                }

                let _ = video_source_fetcher_tx.send(FetchEvent::Quit).await;
            }

            session.specific = SessionSpecificState::Idle;
        }

        Ok(())
    }

    async fn end_session(&mut self, stop_playback: bool) -> Result<()> {
        if let Some(session) = self.session_state.take() {
            self.disconnect_device(session.device, stop_playback);

            match session.specific {
                SessionSpecificState::Mirroring {
                    video_source_fetcher_tx,
                    mut tx_sink,
                    ..
                } => {
                    if let Some(mut tx_sink) = tx_sink.take() {
                        tx_sink.shutdown();
                    }

                    video_source_fetcher_tx.send(FetchEvent::Quit).await?;
                }
                SessionSpecificState::YtDlp {
                    mut fetcher_quit_tx,
                    ..
                } => {
                    if let Some(quit_tx) = fetcher_quit_tx.take() {
                        let _ = quit_tx.send(());
                    }
                }
                _ => (),
            }

            self.ui_weak.upgrade_in_event_loop(|ui| {
                ui.global::<Bridge>()
                    .invoke_change_state(UiAppState::Disconnected);
            })?;
        }

        Ok(())
    }

    fn update_receivers_in_ui(&mut self) -> Result<()> {
        let receivers = self
            .devices
            .iter()
            .map(|(name, info)| UiDevice {
                name: name.to_shared_string(),
                fcast: info.protocol == fcast_sender_sdk::device::ProtocolType::FCast,
            })
            .collect::<Vec<UiDevice>>();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let model = Rc::new(slint::VecModel::<UiDevice>::from_iter(
                receivers.into_iter(),
            ));
            ui.global::<Bridge>().set_devices(model.into());
        })?;

        Ok(())
    }

    fn add_or_update_device(&mut self, device_info: DeviceInfo) -> Result<()> {
        self.devices.insert(device_info.name.clone(), device_info);
        self.update_receivers_in_ui()?;
        Ok(())
    }

    fn start_directory_listing(&mut self, path: Option<PathBuf>) {
        let path = match path {
            Some(path) => path,
            None => {
                if let Some(video_dir) = self
                    .user_dirs
                    .as_ref()
                    .map(|dirs| dirs.video_dir())
                    .flatten()
                {
                    video_dir.to_owned()
                } else {
                    match std::env::home_dir() {
                        Some(home_dir) => home_dir,
                        None => {
                            error!("Could not get home directory");
                            return;
                        }
                    }
                }
            } // None => match std::env::home_dir() {
              //     Some(home_dir) => home_dir,
              //     None => {
              //         error!("Could not get home directory");
              //         return;
              //     }
              // },
        };

        self.current_local_media_id += 1;
        let id = self.current_local_media_id;
        let event_tx = self.event_tx.clone();
        if let Some(session) = self.session_state.as_mut() {
            match &mut session.specific {
                SessionSpecificState::LocalMedia {
                    data,
                    current_id,
                    listing_canceler,
                    ..
                } => {
                    if let Some(canceler) = listing_canceler.take() {
                        canceler.cancel();
                    }

                    *current_id = id;
                    let root = path.clone();
                    *data = LocalMediaDataState {
                        root: path,
                        directories: HashMap::new(),
                        files: HashMap::new(),
                    };
                    let canceler = Canceler::new();
                    *listing_canceler = Some(canceler.clone());

                    tokio::spawn(async move {
                        if let Err(err) = list_directory(canceler, id, root, event_tx).await {
                            error!(?err, "Failed to list directory");
                        }
                    });
                }
                _ => warn!("Cannot start directory listing in non local media session"),
            }
        }
    }

    fn update_device_state(&mut self, event: mcore::DeviceEvent) -> Result<()> {
        if let Some(session) = self.session_state.as_mut() {
            match event {
                mcore::DeviceEvent::VolumeChanged(new_volume) => session.volume = new_volume,
                mcore::DeviceEvent::TimeChanged(new_time) => session.time = new_time,
                mcore::DeviceEvent::PlaybackStateChanged(new_playback_state) => {
                    session.playback_state = match new_playback_state {
                        device::PlaybackState::Idle => UiPlaybackState::Idle,
                        device::PlaybackState::Buffering => UiPlaybackState::Buffering,
                        device::PlaybackState::Playing => UiPlaybackState::Playing,
                        device::PlaybackState::Paused => UiPlaybackState::Paused,
                    };
                }
                mcore::DeviceEvent::DurationChanged(new_duration) => {
                    session.duration = new_duration
                }
                mcore::DeviceEvent::SpeedChanged(new_speed) => session.speed = new_speed,
                _ => (), // Unreachable
            }

            let volume = session.volume as f32;
            let time = session.time as f32;
            let playback_state = session.playback_state;
            let duration = session.duration as f32;
            let speed = session.speed as f32;

            fn sec_to_str(sec: u32) -> String {
                let h = sec / 60 / 60;
                let m = (sec / 60) % 60;
                let s = sec % 60;

                format!("{h:02}:{m:02}:{s:02}")
            }

            let time_str = sec_to_str(time as u32).to_shared_string();
            let dur_str = sec_to_str(duration as u32).to_shared_string();

            self.ui_weak.upgrade_in_event_loop(move |ui| {
                let bridge = ui.global::<Bridge>();
                bridge.set_volume(volume);
                bridge.set_playback_position(time);
                bridge.set_playback_state(playback_state);
                bridge.set_track_duration(duration);
                bridge.set_playback_rate(speed);
                bridge.set_playback_pos_str(time_str);
                bridge.set_track_dur_str(dur_str);
            })?;
        }

        Ok(())
    }

    fn on_preview_sample(
        id: i32,
        appsink: &gst_app::AppSink,
        ui_weak: &slint::Weak<MainWindow>,
    ) -> std::result::Result<gst::FlowSuccess, gst::FlowError> {
        let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
        let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
        let caps = sample.caps().ok_or(gst::FlowError::Error)?;
        let video_info =
            gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
        let frame = gst_video::VideoFrame::from_buffer_readable(buffer, &video_info)
            .map_err(|_| gst::FlowError::Error)?;
        let slint_frame = match frame.format() {
            gst_video::VideoFormat::Rgb => {
                let mut slint_pixel_buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(
                    frame.width(),
                    frame.height(),
                );
                if let Err(err) = frame
                    .buffer()
                    .copy_to_slice(0, slint_pixel_buffer.make_mut_bytes())
                {
                    error!(?err, "Failed to copy buffer");
                    return Err(gst::FlowError::Error);
                }
                slint_pixel_buffer
            }
            _ => {
                error!(format = ?frame.format(), "Received buffer with invalid format");
                return Err(gst::FlowError::NotSupported);
            }
        };

        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            let sources = bridge.get_video_sources();
            let mut a_idx = None;
            for (idx, src) in sources.iter().enumerate() {
                if src.uid == id {
                    a_idx = Some(idx);
                    break;
                }
            }

            if let Some(idx) = a_idx {
                if let Some(mut item) = sources.row_data(idx) {
                    item.preview = slint::Image::from_rgb8(slint_frame);
                    sources.set_row_data(idx, item);
                    return;
                }
            }
        });

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_yt_dlp_event(&mut self, event: mcore::YtDlpEvent) -> Result<()> {
        let span = tracing::span!(tracing::Level::DEBUG, "yt_dlp");
        let _enter = span.enter();

        fn get_title(src: &mcore::yt_dlp::YtDlpSource) -> String {
            src.title.clone().unwrap_or(src.id.to_string())
        }

        if let Some(session) = &mut self.session_state {
            match &mut session.specific {
                SessionSpecificState::YtDlp { sources, .. } => match event {
                    mcore::YtDlpEvent::SourceAvailable(new_source) => {
                        if let Some(formats) = new_source.formats.as_ref() {
                            if let Some(format) = formats.get(0) {
                                if format.content_type().is_none() {
                                    debug!(
                                        ?format,
                                        "Format does not have a supported content type"
                                    );
                                    return Ok(());
                                }
                            } else {
                                debug!("Source has no formats");
                                return Ok(());
                            }
                        } else {
                            debug!("Source has no formats");
                            return Ok(());
                        }

                        let source_item = UiYtDlpSource {
                            title: get_title(&new_source).to_shared_string(),
                        };

                        self.ui_weak.upgrade_in_event_loop(move |ui| {
                            let bridge = ui.global::<Bridge>();
                            let sources_rc = bridge.get_yt_dlp_sources();
                            let sources = sources_rc
                                .as_any()
                                .downcast_ref::<slint::VecModel<UiYtDlpSource>>()
                                .expect("The model is always a vec");
                            sources.push(source_item);
                            bridge.set_yt_dlp_state(UiYtDlpState::HasDataButFetching);
                        })?;

                        if let Some(sources) = sources.as_mut() {
                            sources.push(*new_source);
                        } else {
                            *sources = Some(vec![*new_source]);
                        }
                    }
                    mcore::YtDlpEvent::Cast(title) => {
                        if let Some(sources) = sources.as_ref() {
                            for src in sources {
                                if title != get_title(src) {
                                    continue;
                                }

                                let Some(formats) = src.formats.as_ref() else {
                                    error!("Missing formats");
                                    break;
                                };

                                let Some(format) = formats.get(0) else {
                                    error!("No formats available");
                                    break;
                                };

                                let Some(content_type) = format.content_type() else {
                                    error!("No content type found for format");
                                    break;
                                };

                                let url = format.src_url();
                                let content_type = content_type.to_owned();
                                session.device.load(
                                    fcast_sender_sdk::device::LoadRequest::Url {
                                        content_type,
                                        url,
                                        resume_position: None,
                                        speed: None,
                                        volume: None,
                                        metadata: Some(fcast_sender_sdk::device::Metadata {
                                            title: Some(title),
                                            thumbnail_url: src
                                                .thumbnails
                                                .as_ref()
                                                .map(|thumbs| {
                                                    let mut chosen = None;
                                                    for thumb in thumbs {
                                                        if thumb.width.unwrap_or(0) >= 500
                                                            || thumb.height.unwrap_or(0) >= 500
                                                        {
                                                            chosen = Some(thumb.url.clone());
                                                            break;
                                                        }
                                                    }
                                                    if chosen.is_some() {
                                                        chosen
                                                    } else {
                                                        thumbs.last().map(|thumb| thumb.url.clone())
                                                    }
                                                })
                                                .flatten(),
                                        }),
                                        request_headers: None,
                                    },
                                )?;

                                break;
                            }
                        }
                    }
                    mcore::YtDlpEvent::Finished => {
                        self.ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.global::<Bridge>()
                                .set_yt_dlp_state(UiYtDlpState::HasData);
                        })?;
                    }
                },
                _ => error!("Invalid state"),
            }
        }

        Ok(())
    }

    fn connect_with_device_info(
        &mut self,
        device_info: fcast_sender_sdk::device::DeviceInfo,
        device_name: &str,
    ) -> Result<()> {
        debug!(?device_info, "Trying to connect");
        let device = self.cast_ctx.create_device_from_info(device_info.clone());
        self.current_session_id += 1;
        if let Err(err) = device.connect(
            None,
            Arc::new(mcore::DeviceHandler::new(
                self.current_session_id,
                self.event_tx.clone(),
            )),
            1000,
        ) {
            error!(?err);
            self.ui_weak.upgrade_in_event_loop(|ui| {
                ui.global::<Bridge>()
                    .invoke_change_state(UiAppState::Disconnected);
            })?;
            return Ok(());
        }
        self.session_state = Some(SessionState {
            device,
            volume: 1.0,
            time: 0.0,
            duration: 0.0,
            speed: 1.0,
            playback_state: UiPlaybackState::Idle,
            local_address: None,
            specific: SessionSpecificState::Idle,
            previous_seek: Instant::now(),
            previous_volume_change: Instant::now(),
        });
        let device_name = slint::SharedString::from(device_name);
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_device_name(device_name);
            bridge.invoke_change_state(UiAppState::Connecting);
        })?;

        Ok(())
    }

    async fn handle_event(&mut self, event: Event) -> Result<ShouldQuit> {
        match event {
            Event::StartCast {
                video_uid,
                include_audio,
                scale_width,
                scale_height,
                max_framerate,
            } => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::Mirroring {
                            tx_sink,
                            video_sources,
                            ..
                        } => {
                            debug!(?video_sources, "Video sources");

                            let video_sources = std::mem::take(video_sources);
                            let video_src = match video_uid {
                                Some(uid) => video_sources
                                    .into_iter()
                                    .find(|(id, _)| uid == *id)
                                    .map(|(_, dev)| dev),
                                None => None,
                            };

                            #[cfg(target_os = "linux")]
                            let audio_src = if include_audio {
                                Some(AudioSource::PulseVirtualSink)
                            } else {
                                None
                            };
                            #[cfg(not(target_os = "linux"))]
                            let audio_src = None;

                            debug!(?video_src, ?audio_src, "Adding WHEP pipeline");
                            *tx_sink = Some(
                                mcore::transmission::WhepSink::from_preview(
                                    self.event_tx.clone(),
                                    tokio::runtime::Handle::current(),
                                    video_src,
                                    audio_src,
                                    scale_width,
                                    scale_height,
                                    max_framerate,
                                    self.settings.mirroring().server_port(),
                                )
                                .await?,
                            );
                        }
                        _ => warn!("Cannot start mirroring in non mirroring session"),
                    }
                } else {
                    bail!("No session to start cast for");
                }

                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(UiAppState::StartingCast);
                })?;
            }
            Event::EndSession { disconnect } => {
                if disconnect {
                    self.end_session(true).await?
                } else {
                    self.end_session_no_disconnect().await?
                }
            }
            Event::ConnectToDevice(device_name) => match self.devices.get(&device_name) {
                Some(device_info) => {
                    if device_info.addresses.is_empty() || device_info.port == 0 {
                        error!(?device_info, "Device is missing an address or port");
                        return Ok(ShouldQuit::No);
                    }

                    self.connect_with_device_info(device_info.clone(), &device_name)?;
                }
                None => error!(device_name, "Device not found"),
            },
            Event::SignallerStarted { bound_port } => {
                if let Some(session) = self.session_state.as_mut() {
                    let local_address = session.local_address;
                    let (content_type, url) = match &mut session.specific {
                        SessionSpecificState::Mirroring {
                            tx_sink,
                            our_source_url,
                            ..
                        } => {
                            let Some(addr) = local_address else {
                                error!("Local address is missing");
                                return Ok(ShouldQuit::No);
                            };

                            let (content_type, url) = tx_sink
                                .as_ref()
                                .unwrap()
                                .get_play_msg((&addr).into(), bound_port);

                            debug!(content_type, url, "Sending play message");

                            *our_source_url = Some(url.clone());

                            (content_type, url)
                        }
                        _ => {
                            warn!("Got signaller started in non mirroring session");
                            return Ok(ShouldQuit::No);
                        }
                    };

                    session.device.load(device::LoadRequest::Url {
                        content_type,
                        url,
                        resume_position: None,
                        speed: None,
                        volume: None,
                        metadata: None,
                        request_headers: None,
                    })?;
                } else {
                    warn!("WHEP signaller was started but we're in a bad state");
                    return Ok(ShouldQuit::No);
                };

                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(UiAppState::Mirroring);
                })?;
            }
            Event::Quit => return Ok(ShouldQuit::Yes),
            Event::VideosAvailable(sources) => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::Mirroring { video_sources, .. } => {
                            let mut srcs = Vec::new();
                            for src in sources {
                                let id = src.0 as i32;
                                let ui_weak = self.ui_weak.clone();
                                srcs.push((
                                    src.0,
                                    PreviewPipeline::new(
                                        src.1.display_name(),
                                        move |appsink| {
                                            Self::on_preview_sample(id, appsink, &ui_weak)
                                        },
                                        src.1,
                                    )?,
                                ));
                            }
                            *video_sources = srcs;

                            self.update_video_sources_in_ui()?;
                        }
                        _ => warn!("Got `VideosAvailable` event in non mirroring session"),
                    }
                }
            }
            Event::ReloadVideoSources => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::Mirroring {
                            video_source_fetcher_tx,
                            ..
                        } => video_source_fetcher_tx.send(FetchEvent::Fetch).await?,
                        _ => warn!("Got `ReloadVideoSources` event in non mirroring session"),
                    }
                }
            }
            Event::DeviceAvailable(device_info) => self.add_or_update_device(device_info)?,
            Event::DeviceRemoved(device_name) => {
                if self.devices.remove(&device_name).is_some() {
                    self.update_receivers_in_ui()?;
                } else {
                    debug!(device_name, "Tried to remove device but it was not found");
                }
            }
            Event::DeviceChanged(device_info) => self.add_or_update_device(device_info)?,
            Event::FromDevice { id, event } if id == self.current_session_id => match event {
                mcore::DeviceEvent::StateChanged(new_state) => match new_state {
                    device::DeviceConnectionState::Disconnected => self.end_session(false).await?,
                    device::DeviceConnectionState::Connecting => (),
                    device::DeviceConnectionState::Reconnecting => {
                        let mut change_to_default_state = false;
                        if let Some(session) = self.session_state.as_mut() {
                            match session.specific {
                                SessionSpecificState::Mirroring {
                                    ref mut tx_sink, ..
                                } => {
                                    if let Some(mut tx_sink) = tx_sink.take() {
                                        tx_sink.shutdown();
                                    }
                                    change_to_default_state = true;
                                }
                                _ => (),
                            }
                        }
                        self.ui_weak.upgrade_in_event_loop(move |ui| {
                            let bridge = ui.global::<Bridge>();
                            bridge.set_is_reconnecting(true);
                            if change_to_default_state {
                                bridge.set_app_state(UiAppState::SelectingInputType);
                            }
                        })?;
                    }
                    device::DeviceConnectionState::Connected {
                        local_addr,
                        used_remote_addr,
                    } => {
                        if let Some(session) = self.session_state.as_mut() {
                            session.local_address = Some(local_addr);
                            let is_mirroring_supported = session
                                .device
                                .supports_feature(DeviceFeature::WhepStreaming);
                            debug!(is_mirroring_supported, "Device connected");
                            let remote_addr: std::net::IpAddr = (&used_remote_addr).into();
                            let remote_addr_str = remote_addr.to_string().to_shared_string();
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                let bridge = ui.global::<Bridge>();
                                bridge.set_is_mirroring_supported(is_mirroring_supported);
                                if !bridge.get_is_reconnecting() {
                                    bridge.invoke_change_state(UiAppState::SelectingInputType);
                                }
                                bridge.set_is_reconnecting(false);
                                bridge.set_device_ip(remote_addr_str);
                            })?;
                        } else {
                            bail!("No session");
                        };
                    }
                },
                mcore::DeviceEvent::SourceChanged(new_source) => {
                    let is_our_url = {
                        if let Some(session) = self.session_state.as_mut() {
                            match &mut session.specific {
                                SessionSpecificState::Mirroring { our_source_url, .. } => {
                                    our_source_url.as_ref().map(|our| match &new_source {
                                        fcast_sender_sdk::device::Source::Url { url, .. } => {
                                            url == our
                                        }
                                        _ => false,
                                    })
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    };

                    if let Some(false) = is_our_url {
                        debug!(
                            ?new_source,
                            "The source on the receiver changed, disconnecting"
                        );
                        self.end_session(false)
                            .await
                            .context("Failed to end session")?;
                    }
                }
                mcore::DeviceEvent::PlaybackError(_) => (),
                _ => self.update_device_state(event)?,
            },
            Event::FromDevice { id, .. } => {
                debug!(
                    id,
                    current = self.current_session_id,
                    "Got event from old device",
                );
            }
            #[cfg(target_os = "linux")]
            Event::UnsupportedDisplaySystem => {
                error!("Unsupported display system");
                return Ok(ShouldQuit::Yes);
            }
            Event::StartLocalMediaSession => {
                let id = self.current_local_media_id;
                if let Some(session) = self.session_state.as_mut() {
                    session.specific = SessionSpecificState::LocalMedia {
                        current_id: id,
                        file_server: FileServer::new(self.settings.file_server().port())
                            .await
                            .context("Failed to create file server")?,
                        data: LocalMediaDataState {
                            root: PathBuf::new(),
                            directories: HashMap::new(),
                            files: HashMap::new(),
                        },
                        listing_canceler: None,
                    };
                }

                self.start_directory_listing(None);

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(UiAppState::LocalMedia);
                })?;
            }
            Event::StartMirroringSession => {
                let event_tx = self.event_tx.clone();
                if let Some(session) = self.session_state.as_mut() {
                    let video_source_fetcher_tx = spawn_video_source_fetcher(event_tx).await;
                    video_source_fetcher_tx
                        .send(FetchEvent::Fetch)
                        .await
                        .context("Failed to send fetch event to video source fetcher")?;

                    session.specific = SessionSpecificState::Mirroring {
                        tx_sink: None,
                        video_source_fetcher_tx,
                        our_source_url: None,
                        video_sources: vec![],
                    };
                }

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(UiAppState::SelectingMirroringSource);
                })?;
            }
            Event::DirectoryListing { id, entries } => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::LocalMedia {
                            current_id,
                            data,
                            listing_canceler,
                            ..
                        } => {
                            if let Some(canceler) = listing_canceler.take() {
                                canceler.cancel();
                            }

                            if id != *current_id {
                                debug!(
                                    id,
                                    self.current_local_media_id, "Got old directory listing"
                                );
                                return Ok(ShouldQuit::No);
                            };

                            let mut files_to_process = Vec::new();
                            let mut id_generator = IdGenerator::new();
                            for entry in entries {
                                if entry.is_file {
                                    files_to_process.push(entry.name);
                                } else {
                                    let _ =
                                        data.directories.insert(id_generator.next(), entry.name);
                                }
                            }

                            let root = data.root.to_string_lossy().to_shared_string();
                            let mut directories = data
                                .directories
                                .iter()
                                .map(|(id, name)| UiDirectoryEntry {
                                    id: *id,
                                    name: name.to_shared_string(),
                                })
                                .collect::<Vec<UiDirectoryEntry>>();
                            directories.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                            self.ui_weak.upgrade_in_event_loop(|ui| {
                                let global = ui.global::<Bridge>();
                                global.set_current_directory(root);
                                global.set_directories(
                                    Rc::new(slint::VecModel::from(directories)).into(),
                                );
                            })?;

                            let event_tx = self.event_tx.clone();
                            let root_id = *current_id;
                            let root_path = data.root.clone();
                            let canceler = Canceler::new();
                            *listing_canceler = Some(canceler.clone());
                            tokio::spawn(async move {
                                if let Err(err) = process_files(
                                    canceler,
                                    root_id,
                                    root_path,
                                    files_to_process,
                                    event_tx,
                                )
                                .await
                                {
                                    error!(?err, "Failed to process files");
                                }
                            });
                        }
                        _ => (),
                    }
                }
            }
            Event::FilesListing { id, entries } => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::LocalMedia {
                            current_id, data, ..
                        } => {
                            if id != *current_id {
                                debug!(id, self.current_local_media_id, "Got old files listing");
                                return Ok(ShouldQuit::No);
                            };

                            let mut id_generator = IdGenerator::new();
                            let mut ui_entries: Vec<UiMediaFileEntry> = Vec::new();
                            for entry in entries {
                                let id = id_generator.next();
                                ui_entries.push(UiMediaFileEntry {
                                    id,
                                    name: entry.name.to_shared_string(),
                                    r#type: if entry.mime_type.starts_with("video") {
                                        UiMediaFileType::Video
                                    } else if entry.mime_type.starts_with("audio") {
                                        UiMediaFileType::Audio
                                    } else {
                                        UiMediaFileType::Image
                                    },
                                });
                                let _ = data.files.insert(id, entry);
                            }

                            self.ui_weak.upgrade_in_event_loop(|ui| {
                                ui.global::<Bridge>()
                                    .set_files(Rc::new(slint::VecModel::from(ui_entries)).into());
                            })?;
                        }
                        _ => (),
                    }
                }
            }
            Event::ChangeDir(dir_id) => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::LocalMedia { data, .. } => {
                            if let Some(dir) = data.directories.get(&dir_id) {
                                let mut full_path = data.root.clone();
                                full_path.push(dir);
                                self.start_directory_listing(Some(full_path));
                            }
                        }
                        _ => (),
                    }
                }
            }
            Event::ChangeDirParent => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::LocalMedia { data, .. } => {
                            let mut path = data.root.clone();
                            path.pop();
                            self.start_directory_listing(Some(path));
                        }
                        _ => (),
                    }
                }
            }
            Event::CastLocalMedia(file_id) => {
                let res = if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::LocalMedia {
                            data, file_server, ..
                        } => {
                            if let Some(file_entry) = data.files.get(&file_id) {
                                let mut path = data.root.clone();
                                path.push(&file_entry.name);
                                debug!(?path, "Getting ready to cast");
                                let Some(local_addr) = session.local_address.as_ref() else {
                                    error!("Missing local address");
                                    return Ok(ShouldQuit::No);
                                };

                                let id = file_server.add_file(path, file_entry.mime_type);
                                let url = file_server.get_url(local_addr, &id);
                                session.device.load(device::LoadRequest::Url {
                                    content_type: file_entry.mime_type.to_string(),
                                    url,
                                    resume_position: None,
                                    speed: None,
                                    volume: Some(session.volume),
                                    metadata: None,
                                    request_headers: None,
                                })
                            } else {
                                warn!(file_id, "No file found");
                                return Ok(ShouldQuit::No);
                            }
                        }
                        _ => return Ok(ShouldQuit::No),
                    }
                } else {
                    return Ok(ShouldQuit::No);
                };

                if let Err(err) = res {
                    error!(?err, "Failed to cast local media");
                    self.end_session(true)
                        .await
                        .context("Failed to end session")?;
                }
            }
            Event::Seek {
                seconds,
                force_complete,
            } => {
                let res = if let Some(session) = self.session_state.as_mut() {
                    if force_complete || session.previous_seek.elapsed() >= MIN_TIME_BETWEEN_SEEKS {
                        session.previous_seek = Instant::now();
                        session.device.seek(seconds)
                    } else {
                        return Ok(ShouldQuit::No);
                    }
                } else {
                    return Ok(ShouldQuit::No);
                };

                if let Err(err) = res {
                    error!(?err, "Failed to seek");
                    self.end_session(true)
                        .await
                        .context("Failed to end session")?;
                }
            }
            Event::ChangePlaybackState(playback_state) => {
                let res = if let Some(session) = self.session_state.as_ref() {
                    match playback_state {
                        device::PlaybackState::Idle => session.device.stop_playback(),
                        device::PlaybackState::Playing => session.device.resume_playback(),
                        device::PlaybackState::Paused => session.device.pause_playback(),
                        _ => return Ok(ShouldQuit::No),
                    }
                } else {
                    return Ok(ShouldQuit::No);
                };

                if let Err(err) = res {
                    error!(?err, "Failed to change playback state");
                    self.end_session(true)
                        .await
                        .context("Failed to end session")?;
                }
            }
            Event::ChangeVolume {
                volume,
                force_complete,
            } => {
                let res = if let Some(session) = self.session_state.as_mut() {
                    if force_complete
                        || session.previous_volume_change.elapsed()
                            >= MIN_TIME_BETWEEN_VOLUME_CHANGES
                    {
                        session.previous_volume_change = Instant::now();
                        session.device.change_volume(volume)
                    } else {
                        return Ok(ShouldQuit::No);
                    }
                } else {
                    return Ok(ShouldQuit::No);
                };

                if let Err(err) = res {
                    error!(?err, "Failed to change volume");
                    self.end_session(true)
                        .await
                        .context("Failed to end session")?;
                }
            }
            Event::CastTestPattern => {
                if let Some(session) = self.session_state.as_mut() {
                    let (video_source_fetcher_tx, _) = channel::<FetchEvent>(10);

                    let preview = PreviewPipeline::new(
                        "Test pattern".to_owned(),
                        move |_| Ok(gst::FlowSuccess::Ok),
                        mcore::VideoSource::TestSrc,
                    )
                    .context("Failed to create preview pipeline")?;

                    let tx_sink = mcore::transmission::WhepSink::from_preview(
                        self.event_tx.clone(),
                        tokio::runtime::Handle::current(),
                        Some(preview),
                        None,
                        720,
                        480,
                        30,
                        self.settings.mirroring().server_port(),
                    )
                    .await
                    .context("Failed to create WHEP sink from preview pipeline")?;

                    session.specific = SessionSpecificState::Mirroring {
                        tx_sink: Some(tx_sink),
                        video_source_fetcher_tx,
                        our_source_url: None,
                        video_sources: vec![],
                    };
                }
            }
            Event::GetSourcesFromUrl(url) => {
                let event_tx = self.event_tx.clone();
                let (quit_tx, quit_rx) = tokio::sync::oneshot::channel::<()>();
                tokio::spawn(async move {
                    if let Err(err) = mcore::yt_dlp::YtDlpSource::try_get(&url, &event_tx, quit_rx)
                        .instrument(tracing::debug_span!("yt_dlp_try_get", url))
                        .await
                    {
                        error!(?err, "Failed to get sources with yt-dlp");
                    };
                });
                if let Some(session) = &mut self.session_state {
                    session.specific = SessionSpecificState::YtDlp {
                        sources: None,
                        fetcher_quit_tx: Some(quit_tx),
                    };
                }
            }
            Event::YtDlp(event) => self.handle_yt_dlp_event(event)?,
            Event::ConnectToDeviceDirect(device_info) => {
                let device_name = device_info.name.clone();
                self.connect_with_device_info(device_info, &device_name)?;
            }
            Event::ChangeRootDir(new_root_dir) => {
                if let Some(user_dirs) = self.user_dirs.as_ref() {
                    let path = match new_root_dir {
                        RootDirType::Pictures => user_dirs.picture_dir(),
                        RootDirType::Videos => user_dirs.video_dir(),
                        RootDirType::Music => user_dirs.audio_dir(),
                    };

                    if let Some(path) = path {
                        if let Some(session) = self.session_state.as_mut() {
                            match &mut session.specific {
                                SessionSpecificState::LocalMedia { .. } => {
                                    self.start_directory_listing(Some(path.to_owned()));
                                }
                                _ => (),
                            }
                        }
                    } else {
                        error!(?new_root_dir, "No directory found");
                    }
                } else {
                    error!("Missing user dirs");
                }
            }
            Event::SetPlaybackRate(new_rate) => {
                if let Some(session) = self.session_state.as_mut() {
                    let _ = session.device.change_speed(new_rate);
                }
            }
            Event::UpdateSettings {
                file_server_port,
                mirroring_server_port,
            } => {
                let has_changes = file_server_port != self.settings.file_server().port()
                    || mirroring_server_port != self.settings.mirroring().server_port();
                self.settings.set_file_server_port(file_server_port);
                self.settings
                    .set_mirroring_server_port(mirroring_server_port);
                // self.settings.file_server.port = port;
                if has_changes {
                    self.write_settings_file()
                        .instrument(tracing::debug_span!("write_settings_file"))
                        .await;
                }
            }
        }

        Ok(ShouldQuit::No)
    }

    fn update_video_sources_in_ui(&mut self) -> Result<()> {
        if let Some(session) = self.session_state.as_mut() {
            match &mut session.specific {
                SessionSpecificState::Mirroring { video_sources, .. } => {
                    video_sources.sort_unstable_by(|a, b| a.1.display_name.cmp(&b.1.display_name));
                    let video_sources = video_sources
                        .iter()
                        .map(|(uid, s)| (*uid, s.display_name.clone()))
                        .collect::<Vec<(usize, String)>>();

                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let video_devs = slint::VecModel::<UiVideoSourceModel>::from_iter(
                            video_sources.iter().map(|dev| UiVideoSourceModel {
                                name: slint::SharedString::from(dev.1.as_str()),
                                uid: dev.0 as i32,
                                preview: slint::Image::default(),
                            }),
                        );

                        ui.global::<Bridge>()
                            .set_video_sources(Rc::new(video_devs).into());
                    })?;
                }
                _ => {
                    bail!(
                        "Attempt to update_video_sources_in_ui in invalid state state={:?}",
                        session.specific
                    );
                }
            }
        } else {
            bail!("No active session for update_video_sources_in_ui");
        };

        Ok(())
    }

    fn get_settings_file_path(&self) -> Option<PathBuf> {
        if let Some(dirs) = self.base_dirs.as_ref() {
            let mut config_dir = dirs.config_dir().to_owned();
            config_dir.extend(["fcast-sender", "config.toml"]);
            Some(config_dir)
        } else {
            None
        }
    }

    async fn write_settings_file(&mut self) {
        let Some(settings_path) = self.get_settings_file_path() else {
            error!("No settings file path available");
            return;
        };

        let mut file = match tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(settings_path)
            .await
        {
            Ok(f) => f,
            Err(err) => {
                error!(?err, "Failed to open settings file");
                return;
            }
        };

        let mut settings_str = String::new();
        if let Err(err) = file.read_to_string(&mut settings_str).await {
            error!(?err, "Failed read settings");
            return;
        }

        let mut settings_doc = match settings_str.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => doc,
            Err(err) => {
                error!(?err, "Failed to parse settings");
                return;
            }
        };

        settings_doc["file_server"]["port"] =
            toml_edit::value(self.settings.file_server().port() as i64);
        settings_doc["mirroring"]["server_port"] =
            toml_edit::value(self.settings.mirroring().server_port() as i64);

        debug!(?settings_doc, "New settings");

        if let Err(err) = file.rewind().await {
            error!(?err, "Failed to rewind settings file");
            return;
        }

        if let Err(err) = file.set_len(0).await {
            error!(?err, "Failed to truncate settings file");
            return;
        }

        let settings_str = settings_doc.to_string();
        if let Err(err) = file.write_all(settings_str.as_bytes()).await {
            error!(?err, "Failed to write new settings");
        }
    }

    async fn write_default_settings_file(&mut self, path: PathBuf) {
        // From https://docs.rs/toml_edit/0.24.0+spec-1.1.0/toml_edit/ser/fn.to_string.html:
        // Serialization can fail if T’s implementation of Serialize decides to fail, if T contains a map
        // with non-string keys, or if T attempts to serialize an unsupported datatype such as an enum, tuple,
        // or tuple struct.
        let settings_str =
            toml_edit::ser::to_string(&self.settings).expect("failed to serialize settings");

        if let Err(err) = tokio::fs::create_dir_all({
            let mut path_no_file = path.clone();
            path_no_file.pop();
            path_no_file
        })
        .await
        {
            error!(?err, "Failed to create the path");
            return;
        }

        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await
        {
            Ok(f) => f,
            Err(err) => {
                error!(?err, "Failed to open config file");
                return;
            }
        };

        if let Err(err) = file.write_all(settings_str.as_bytes()).await {
            error!(?err, "Failed to write settings");
        }

        debug!("Successfully wrote default settings file");
    }

    async fn load_settings(&mut self) -> Result<()> {
        let mut settings_path_str = "unknwon".to_owned();
        if let Some(settings_path) = self.get_settings_file_path() {
            settings_path_str = settings_path.display().to_string();
            if let Ok(mut cfg_file) = tokio::fs::File::open(&settings_path).await {
                let mut config_str = String::new();
                if cfg_file.read_to_string(&mut config_str).await.is_ok() {
                    match toml_edit::de::from_str::<Settings>(&config_str) {
                        Ok(settings) => self.settings = settings,
                        Err(err) => error!(?err, "Failed to parse config as toml"),
                    }
                }
            } else {
                debug!(?settings_path, "Config file does not already exist");
                self.write_default_settings_file(settings_path)
                    .instrument(tracing::debug_span!("write_default_settings_file"))
                    .await;
            }
        }

        debug!(?self.settings, "Using settings");

        let file_server_port = self.settings.file_server().port();
        let mirroring_server_port = self.settings.mirroring().server_port();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_file_server_port(file_server_port.to_shared_string());
            bridge.set_mirroring_server_port(mirroring_server_port.to_shared_string());
            bridge.set_settings_file_path(settings_path_str.to_shared_string());
        })?;

        Ok(())
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    ) -> Result<()> {
        tracing_gstreamer::integrate_events();
        gst::log::remove_default_log_function();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::init()?;
        gstrsrtp::plugin_register_static()?;

        self.load_settings()
            .instrument(tracing::debug_span!("load_settings"))
            .await?;

        tokio::spawn({
            // let ui_weak = self.ui_weak.clone();
            async move {
                let yt_dlp_available = match mcore::yt_dlp::is_yt_dlp_available().await {
                    Ok(p) => p,
                    Err(err) => {
                        error!(?err, "Failed to check if yt-dlp is available");
                        return;
                    }
                };

                debug!(?yt_dlp_available, "yt-dlp status");

                // TODO: include this when ready
                // let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                //     ui.global::<Bridge>()
                //         .set_is_yt_dlp_available(yt_dlp_available);
                // });
            }
        });

        loop {
            let Some(event) = event_rx.recv().await else {
                debug!("No more events");
                break;
            };

            match self
                .handle_event(event)
                .instrument(tracing::debug_span!("handle_event"))
                .await
            {
                Ok(res) => {
                    if res == ShouldQuit::Yes {
                        break;
                    }
                }
                Err(err) => {
                    error!(?err, "Failed to handle event");
                    let _ = self.end_session(true).await;
                    return Err(err);
                }
            }
        }

        debug!("Quitting event loop");

        self.end_session(true).await?;

        let _ = slint::quit_event_loop();

        Ok(())
    }
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {}

fn log_level() -> LevelFilter {
    match std::env::var("FCAST_LOG") {
        Ok(level) => match level.to_ascii_lowercase().as_str() {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            "trace" => LevelFilter::TRACE,
            _ => LevelFilter::OFF,
        },
        #[cfg(debug_assertions)]
        Err(_) => LevelFilter::DEBUG,
        #[cfg(not(debug_assertions))]
        Err(_) => LevelFilter::OFF,
    }
}

struct StringVisitor {
    res: String,
}

macro_rules! write_event {
    ($res:expr, $field:expr, $value:expr) => {
        let _ = write!(&mut $res, " {}={}", $field.name(), $value);
    };
}

impl tracing::field::Visit for StringVisitor {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        write_event!(self.res, field, value);
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        write_event!(self.res, field, value);
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        write_event!(self.res, field, value);
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        write_event!(self.res, field, value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        write_event!(self.res, field, value);
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        write_event!(self.res, field, value);
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(&mut self.res, " {}={:?}", field.name(), value);
    }
}

struct VecLayer {
    events: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
}

impl VecLayer {
    pub fn new(events: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>) -> Self {
        Self { events }
    }
}

impl<S: tracing::Subscriber> Layer<S> for VecLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut events = self.events.lock();

        let mut event_line = if events.len() >= MAX_VEC_LOG_ENTRIES {
            match events.pop_front() {
                Some(mut old_line) => {
                    old_line.clear();
                    old_line
                }
                None => String::new(),
            }
        } else {
            String::new()
        };

        let meta = event.metadata();
        let _ = write!(
            &mut event_line,
            "{} {}:",
            meta.level(),
            meta.module_path().unwrap_or("n/a")
        );
        let mut visitor = StringVisitor { res: event_line };
        event.record(&mut visitor);
        events.push_back(visitor.res);
    }
}

fn main() -> Result<()> {
    let init_start = std::time::Instant::now();

    let _cli = Cli::parse();

    #[cfg(target_os = "windows")]
    let _ = enable_ansi_support::enable_ansi_support();

    #[cfg(target_os = "windows")]
    {
        let mut plugin_dir = std::env::current_exe()?;
        plugin_dir.pop();
        unsafe { std::env::set_var("GST_PLUGIN_PATH", plugin_dir) };
    }

    #[cfg(target_os = "macos")]
    {
        let mut plugin_dir = std::env::current_exe()?;
        plugin_dir.pop();
        plugin_dir.push("lib");
        unsafe { std::env::set_var("GST_PLUGIN_PATH", plugin_dir) };
    }

    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(log_level());
    let tracing_events: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>> =
        Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
    let vec_layer = VecLayer::new(Arc::clone(&tracing_events)).with_filter(LevelFilter::DEBUG);

    let prev_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        tracing_panic::panic_hook(panic_info);
        prev_panic_hook(panic_info);
    }));

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(vec_layer)
        .init();

    let runtime = Runtime::new()?;

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    let ui = MainWindow::new()?;

    let event_loop_jh = runtime.spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        async move {
            // NOTE: spawn so panics bouble up to us and we're able to quit the GUI
            let res = tokio::task::spawn(async move {
                Application::new(ui_weak, event_tx)
                    .unwrap()
                    .run_event_loop(event_rx)
                    .await
            })
            .await;
            let _ = slint::quit_event_loop();
            res
        }
    });

    let bridge = ui.global::<Bridge>();
    bridge.on_connect_to_device({
        let event_tx = event_tx.clone();
        move |device_name| {
            if let Err(err) = event_tx.send(Event::ConnectToDevice(device_name.to_string())) {
                error!("on_connect_to_device: failed to send event: {err}");
            }
        }
    });

    bridge.on_start_cast({
        let event_tx = event_tx.clone();
        move |video_uid, include_audio, scale_width: i32, scale_height: i32, max_framerate: i32| {
            event_tx
                .send(Event::StartCast {
                    video_uid: if video_uid >= 0 {
                        Some(video_uid as usize)
                    } else {
                        None
                    },
                    include_audio,
                    scale_width: scale_width.max(1) as u32,
                    scale_height: scale_height.max(1) as u32,
                    max_framerate: max_framerate.max(1) as u32,
                })
                .unwrap();
        }
    });

    bridge.on_stop_cast({
        let event_tx = event_tx.clone();
        move |disconnect: bool| {
            event_tx.send(Event::EndSession { disconnect }).unwrap();
        }
    });

    bridge.on_reload_video_sources({
        let event_tx = event_tx.clone();
        move || {
            event_tx.send(Event::ReloadVideoSources).unwrap();
        }
    });

    bridge.on_select_input_type({
        let event_tx = event_tx.clone();
        move |input_type| match input_type {
            UiInputType::LocalMedia => event_tx.send(Event::StartLocalMediaSession).unwrap(),
            UiInputType::Mirroring => event_tx.send(Event::StartMirroringSession).unwrap(),
        }
    });

    bridge.on_change_dir_child({
        let event_tx = event_tx.clone();
        move |dir_id| {
            event_tx.send(Event::ChangeDir(dir_id)).unwrap();
        }
    });

    bridge.on_change_dir_parent({
        let event_tx = event_tx.clone();
        move || {
            event_tx.send(Event::ChangeDirParent).unwrap();
        }
    });

    bridge.on_cast_local_media({
        let event_tx = event_tx.clone();
        move |file_id| {
            event_tx.send(Event::CastLocalMedia(file_id)).unwrap();
        }
    });

    bridge.on_seek({
        let event_tx = event_tx.clone();
        move |seconds: f32, force_complete: bool| {
            event_tx
                .send(Event::Seek {
                    seconds: seconds as f64,
                    force_complete,
                })
                .unwrap();
        }
    });

    bridge.on_change_playback_state({
        let event_tx = event_tx.clone();
        move |state: UiPlaybackState| {
            event_tx
                .send(Event::ChangePlaybackState(match state {
                    UiPlaybackState::Idle => device::PlaybackState::Idle,
                    UiPlaybackState::Playing => device::PlaybackState::Playing,
                    UiPlaybackState::Paused => device::PlaybackState::Paused,
                    UiPlaybackState::Buffering => device::PlaybackState::Buffering,
                }))
                .unwrap();
        }
    });

    bridge.on_change_volume({
        let event_tx = event_tx.clone();
        move |volume: f32, force_complete: bool| {
            event_tx
                .send(Event::ChangeVolume {
                    volume: volume as f64,
                    force_complete,
                })
                .unwrap();
        }
    });

    bridge.on_disconnect({
        let event_tx = event_tx.clone();
        move || {
            event_tx
                .send(Event::EndSession { disconnect: true })
                .unwrap();
        }
    });

    bridge.on_connect_manually({
        let event_tx = event_tx.clone();
        move |url: slint::SharedString| {
            let Some(dev_info) = device_info_parser::parse(&url) else {
                // NOTE: should be unreachable because the url is being checked
                error!(?url, "Invalid device info");
                return;
            };
            event_tx
                .send(Event::ConnectToDeviceDirect(dev_info))
                .unwrap();
        }
    });

    bridge.on_reload_log_string({
        let ui_weak = ui.as_weak();
        let tracing_events = Arc::clone(&tracing_events);
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("Callback handlers are always called from the ui thread");
            let events = tracing_events.lock();
            let (front, back) = events.as_slices();
            let log_string = [front.join("\n"), back.join("\n")]
                .join("\n")
                .to_shared_string();
            ui.global::<Bridge>().set_log_string(log_string);
        }
    });

    bridge.on_start_test_pattern_cast({
        let event_tx = event_tx.clone();
        move || {
            event_tx.send(Event::CastTestPattern).unwrap();
        }
    });

    bridge.on_is_device_info_valid(|info: slint::SharedString| -> bool {
        device_info_parser::parse(info.as_str()).is_some()
    });

    bridge.on_open_url(|url: slint::SharedString| {
        debug!(?url, "Trying to open URL");
        if let Err(err) = webbrowser::open(&url) {
            error!(?err, "Failed to open URL");
        }
    });

    bridge.on_try_play_url({
        let event_tx = event_tx.clone();
        move |url: slint::SharedString| {
            event_tx
                .send(Event::GetSourcesFromUrl(url.to_string()))
                .unwrap();
        }
    });

    bridge.on_cast_yt_dlp({
        let event_tx = event_tx.clone();
        move |title: slint::SharedString| {
            event_tx
                .send(Event::YtDlp(mcore::YtDlpEvent::Cast(title.to_string())))
                .unwrap();
        }
    });

    bridge.on_change_root_dir({
        let event_tx = event_tx.clone();
        move |dir_type: UiRootDirType| {
            event_tx
                .send(Event::ChangeRootDir(match dir_type {
                    UiRootDirType::Pictures => RootDirType::Pictures,
                    UiRootDirType::Videos => RootDirType::Videos,
                    UiRootDirType::Music => RootDirType::Music,
                }))
                .unwrap();
        }
    });

    bridge.on_change_playback_rate({
        let event_tx = event_tx.clone();
        move |rate: f32| {
            event_tx.send(Event::SetPlaybackRate(rate as f64)).unwrap();
        }
    });

    bridge.on_update_settings({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("Callback handlers are always called from the ui thread");
            let bridge = ui.global::<Bridge>();
            let file_server_port = bridge.get_file_server_port();
            let Ok(file_server_port) = file_server_port.parse::<u16>() else {
                error!(?file_server_port, "Invalid port");
                return;
            };
            let mirroring_server_port = bridge.get_mirroring_server_port();
            let Ok(mirroring_server_port) = mirroring_server_port.parse::<u16>() else {
                error!(?mirroring_server_port, "Invalid port");
                return;
            };
            event_tx
                .send(Event::UpdateSettings {
                    file_server_port,
                    mirroring_server_port,
                })
                .unwrap();
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    bridge.set_is_audio_supported(false);

    #[cfg(target_os = "linux")]
    bridge.set_is_audio_supported(true);

    bridge.set_app_version(env!("CARGO_PKG_VERSION").to_shared_string());

    let init_finished_in = init_start.elapsed();
    debug!(?init_finished_in, "Initialization finished");

    ui.run()?;

    let res = runtime.block_on(async move {
        let _ = event_tx.send(Event::Quit);
        event_loop_jh.await
    });

    if matches!(res, Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_)) {
        let crash_window = CrashWindow::new().unwrap();

        let log_string = {
            let events = tracing_events.lock();
            let (front, back) = events.as_slices();
            [front.join("\n"), back.join("\n")].join("\n")
        }
        .to_shared_string();

        debug!("Starting crash window");

        crash_window.set_log(log_string);

        crash_window.global::<Bridge>().on_open_url(|url| {
            if let Err(err) = webbrowser::open(&url) {
                error!(?err, "Failed to open URL");
            }
        });

        let _ = slint::run_event_loop();
        crash_window.run().unwrap();
    }

    Ok(())
}
