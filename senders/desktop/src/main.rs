#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// TODO: incremental file listing

use anyhow::{Result, bail};
use clap::Parser;
#[cfg(target_os = "macos")]
use desktop_sender::macos;
use desktop_sender::{FetchEvent, file_server::FileServer};
use fcast_sender_sdk::{
    context::CastContext,
    device::{self, DeviceFeature, DeviceInfo},
};
use mcore::{
    AudioSource, Event, FileSystemEntry, MediaFileEntry, ShouldQuit, SourceConfig, VideoSource,
    transmission::WhepSink,
};
use slint::ToSharedString;
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    time::{Duration, Instant},
};
use std::{fmt::Write, path::PathBuf};
use tokio::{
    io::AsyncReadExt,
    runtime::Runtime,
    sync::mpsc::{Sender, channel},
};
use tracing::{debug, error, level_filters::LevelFilter, warn};
use tracing_subscriber::{
    Layer, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
};

slint::include_modules!();

const MAX_VEC_LOG_ENTRIES: usize = 1500;
const MIN_TIME_BETWEEN_SEEKS: Duration = Duration::from_millis(200);
const MIN_TIME_BETWEEN_VOLUME_CHANGES: Duration = Duration::from_millis(75);

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
    event_tx: Sender<Event>,
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

    event_tx
        .send(Event::DirectoryListing { id, entries })
        .await?;

    Ok(())
}

async fn process_files(
    canceler: Canceler,
    id: u32,
    mut root_path: PathBuf,
    files: Vec<String>,
    event_tx: Sender<Event>,
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

        if let Some(infered) = desktop_sender::infer::infer_type(bytes_read, &buf) {
            media_files.push(MediaFileEntry {
                name,
                mime_type: infered.mime_type,
            });
        }

        root_path.pop();
    }

    if !media_files.is_empty() {
        event_tx
            .send(Event::FilesListing {
                id,
                entries: media_files,
            })
            .await?;
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

#[derive(Debug)]
enum SessionSpecificState {
    Idle,
    Mirroring {
        tx_sink: Option<WhepSink>,
        video_source_fetcher_tx: Sender<FetchEvent>,
        our_source_url: Option<String>,
        video_sources: Vec<(usize, VideoSource)>,
        audio_sources: Vec<(usize, AudioSource)>,
    },
    LocalMedia {
        current_id: u32,
        file_server: FileServer,
        data: LocalMediaDataState,
        listing_canceler: Option<Canceler>,
    },
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
    event_tx: Sender<Event>,
    devices: HashMap<String, DeviceInfo>,
    current_session_id: usize,
    current_local_media_id: u32,
    session_state: Option<SessionState>,
}

async fn spawn_video_source_fetcher(event_tx: Sender<Event>) -> Sender<FetchEvent> {
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
                                .await
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
                            .await
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
    pub fn new(ui_weak: slint::Weak<MainWindow>, event_tx: Sender<Event>) -> Result<Self> {
        let cast_ctx = CastContext::new()?;
        cast_ctx.start_discovery(Arc::new(mcore::Discoverer::new(
            event_tx.clone(),
            tokio::runtime::Handle::current(),
        )));

        Ok(Self {
            cast_ctx,
            ui_weak,
            event_tx,
            devices: HashMap::new(),
            current_session_id: 0,
            current_local_media_id: 0,
            session_state: None,
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
            .keys()
            .map(slint::SharedString::from)
            .collect::<Vec<slint::SharedString>>();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let model = Rc::new(slint::VecModel::<slint::SharedString>::from_iter(
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
            None => match std::env::home_dir() {
                Some(home_dir) => home_dir,
                None => {
                    error!("Could not get home directory");
                    return;
                }
            },
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

            self.ui_weak.upgrade_in_event_loop(move |ui| {
                let bridge = ui.global::<Bridge>();
                bridge.set_volume(volume);
                bridge.set_playback_position(time);
                bridge.set_playback_state(playback_state);
                bridge.set_track_duration(duration);
                bridge.set_playback_rate(speed);
            })?;
        }

        Ok(())
    }

    async fn handle_event(&mut self, event: Event) -> Result<ShouldQuit> {
        let span = tracing::span!(tracing::Level::DEBUG, "handle_event");
        let _enter = span.enter();

        match event {
            Event::StartCast {
                video_uid,
                audio_uid,
                scale_width,
                scale_height,
                max_framerate,
            } => {
                if let Some(session) = self.session_state.as_mut() {
                    match &mut session.specific {
                        SessionSpecificState::Mirroring {
                            tx_sink,
                            video_sources,
                            audio_sources,
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

                            debug!(?audio_sources, "Audio sources");

                            let audio_sources = std::mem::take(audio_sources);
                            let audio_src = match audio_uid {
                                Some(uid) => audio_sources
                                    .into_iter()
                                    .find(|(id, _)| uid == *id)
                                    .map(|(_, dev)| dev),
                                None => None,
                            };

                            let source_config = match (video_src, audio_src) {
                                (Some(video), Some(audio)) => {
                                    SourceConfig::AudioVideo { video, audio }
                                }
                                (Some(video), None) => SourceConfig::Video(video),
                                (None, Some(audio)) => SourceConfig::Audio(audio),
                                _ => unreachable!(),
                            };

                            debug!("Adding WHEP pipeline");
                            *tx_sink = Some(mcore::transmission::WhepSink::new(
                                source_config,
                                self.event_tx.clone(),
                                tokio::runtime::Handle::current(),
                                scale_width,
                                scale_height,
                                max_framerate,
                            )?);
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
            Event::EndSession => self.end_session(true).await?,
            Event::ConnectToDevice(device_name) => match self.devices.get(&device_name) {
                Some(device_info) => {
                    if device_info.addresses.is_empty() || device_info.port == 0 {
                        error!(?device_info, "Device is missing an address or port");
                        return Ok(ShouldQuit::No);
                    }

                    debug!(?device_info, "Trying to connect");
                    let device = self.cast_ctx.create_device_from_info(device_info.clone());
                    self.current_session_id += 1;
                    if let Err(err) = device.connect(
                        None,
                        Arc::new(mcore::DeviceHandler::new(
                            self.current_session_id,
                            self.event_tx.clone(),
                            tokio::runtime::Handle::current(),
                        )),
                        1000,
                    ) {
                        error!(?err);
                        self.ui_weak.upgrade_in_event_loop(|ui| {
                            ui.global::<Bridge>()
                                .invoke_change_state(UiAppState::Disconnected);
                        })?;
                        return Ok(ShouldQuit::No);
                    }
                    self.session_state = Some(SessionState {
                        device,
                        volume: 0.0,
                        time: 0.0,
                        duration: 0.0,
                        speed: 0.0,
                        playback_state: UiPlaybackState::Idle,
                        local_address: None,
                        specific: SessionSpecificState::Idle,
                        previous_seek: Instant::now(),
                        previous_volume_change: Instant::now(),
                    });
                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let bridge = ui.global::<Bridge>();
                        bridge.set_device_name(slint::SharedString::from(device_name));
                        bridge.invoke_change_state(UiAppState::Connecting);
                    })?;
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
                            *video_sources = sources;
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
                self.devices.remove(&device_name);
            }
            Event::DeviceChanged(device_info) => self.add_or_update_device(device_info)?,
            Event::FromDevice { id, event } if id == self.current_session_id => match event {
                mcore::DeviceEvent::StateChanged(new_state) => match new_state {
                    device::DeviceConnectionState::Disconnected => self.end_session(false).await?,
                    device::DeviceConnectionState::Connecting => (),
                    device::DeviceConnectionState::Reconnecting => {
                        // TODO: I'm sure we can handle this more gracefully
                        if let Some(session) = self.session_state.as_mut() {
                            match session.specific {
                                SessionSpecificState::Mirroring {
                                    ref mut tx_sink, ..
                                } => {
                                    if let Some(mut tx_sink) = tx_sink.take() {
                                        tx_sink.shutdown();
                                    }
                                }
                                _ => (),
                            }
                        }
                    }
                    device::DeviceConnectionState::Connected { local_addr, .. } => {
                        if let Some(session) = self.session_state.as_mut() {
                            session.local_address = Some(local_addr);
                            let is_mirroring_supported = session
                                .device
                                .supports_feature(DeviceFeature::WhepStreaming);
                            debug!(is_mirroring_supported, "Device connected");
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                let bridge = ui.global::<Bridge>();
                                bridge.set_is_mirroring_supported(is_mirroring_supported);
                                bridge.invoke_change_state(UiAppState::SelectingInputType);
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
                        self.end_session(false).await?;
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
                        file_server: FileServer::new().await?,
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
                    video_source_fetcher_tx.send(FetchEvent::Fetch).await?;

                    #[cfg(target_os = "linux")]
                    let audio_sources = vec![(0, AudioSource::PulseVirtualSink)];

                    session.specific = SessionSpecificState::Mirroring {
                        tx_sink: None,
                        video_source_fetcher_tx,
                        our_source_url: None,
                        video_sources: vec![],
                        #[cfg(target_os = "linux")]
                        audio_sources,
                        #[cfg(not(target_os = "linux"))]
                        audio_sources: vec![],
                    };
                }

                #[cfg(target_os = "linux")]
                self.update_audio_sources_in_ui()?;

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
                            directories.sort_by(|a, b| a.name.cmp(&b.name));
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
                                    volume: None,
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
                    self.end_session(true).await?;
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
                    self.end_session(true).await?;
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
                    self.end_session(true).await?;
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
                    self.end_session(true).await?;
                }
            }
        }

        Ok(ShouldQuit::No)
    }

    fn update_audio_sources_in_ui(&self) -> Result<()> {
        if let Some(session) = &self.session_state {
            match &session.specific {
                SessionSpecificState::Mirroring { audio_sources, .. } => {
                    let audio_sources = audio_sources.clone();
                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let audio_devs =
                            slint::VecModel::<slint::ModelRc<UiAudioSourceModel>>::from_iter(
                                audio_sources.chunks(3).map(|row| {
                                    slint::ModelRc::<UiAudioSourceModel>::new(slint::VecModel::<
                                        UiAudioSourceModel,
                                    >::from_iter(
                                        row.iter().map(|dev| UiAudioSourceModel {
                                            name: slint::SharedString::from(
                                                dev.1.display_name().as_str(),
                                            ),
                                            uid: dev.0 as i32,
                                        }),
                                    ))
                                }),
                            );

                        ui.global::<Bridge>()
                            .set_audio_sources(Rc::new(audio_devs).into());
                    })?;
                }
                _ => {
                    bail!(
                        "Attempt to update_audio_sources_in_ui in invalid state state={:?}",
                        session.specific
                    );
                }
            }
        } else {
            bail!("No active session for update_audio_sources_in_ui");
        };

        Ok(())
    }

    fn update_video_sources_in_ui(&mut self) -> Result<()> {
        if let Some(session) = self.session_state.as_mut() {
            match &mut session.specific {
                SessionSpecificState::Mirroring { video_sources, .. } => {
                    video_sources.sort_by(|a, b| a.1.display_name().cmp(&b.1.display_name()));
                    let video_sources = video_sources
                        .iter()
                        .map(|(uid, s)| (*uid, s.display_name()))
                        .collect::<Vec<(usize, String)>>();

                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let video_devs =
                            slint::VecModel::<slint::ModelRc<UiVideoSourceModel>>::from_iter(
                                video_sources.chunks(3).map(|row| {
                                    slint::ModelRc::<UiVideoSourceModel>::new(slint::VecModel::<
                                        UiVideoSourceModel,
                                    >::from_iter(
                                        row.iter().map(|dev| UiVideoSourceModel {
                                            name: slint::SharedString::from(dev.1.as_str()),
                                            uid: dev.0 as i32,
                                        }),
                                    ))
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

    pub async fn run_event_loop(
        mut self,
        mut event_rx: tokio::sync::mpsc::Receiver<Event>,
    ) -> Result<()> {
        tracing_gstreamer::integrate_events();
        gst::log::remove_default_log_function();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::init()?;
        gstrsrtp::plugin_register_static()?;

        loop {
            let Some(event) = event_rx.recv().await else {
                debug!("No more events");
                break;
            };

            match self.handle_event(event).await {
                Ok(res) => {
                    if res == ShouldQuit::Yes {
                        break;
                    }
                }
                Err(err) => {
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

    let (event_tx, event_rx) = channel::<Event>(100);

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

    ui.global::<Bridge>().on_connect_to_device({
        let event_tx = event_tx.clone();
        move |device_name| {
            if let Err(err) =
                event_tx.blocking_send(Event::ConnectToDevice(device_name.to_string()))
            {
                error!("on_connect_to_device: failed to send event: {err}");
            }
        }
    });

    ui.global::<Bridge>().on_start_cast({
        let event_tx = event_tx.clone();
        move |video_uid, audio_uid, scale_width: i32, scale_height: i32, max_framerate: i32| {
            event_tx
                .blocking_send(Event::StartCast {
                    video_uid: if video_uid >= 0 {
                        Some(video_uid as usize)
                    } else {
                        None
                    },
                    audio_uid: if audio_uid >= 0 {
                        Some(audio_uid as usize)
                    } else {
                        None
                    },
                    scale_width: scale_width.max(1) as u32,
                    scale_height: scale_height.max(1) as u32,
                    max_framerate: max_framerate.max(1) as u32,
                })
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_stop_cast({
        let event_tx = event_tx.clone();
        move || {
            event_tx.blocking_send(Event::EndSession).unwrap();
        }
    });

    ui.global::<Bridge>().on_reload_video_sources({
        let event_tx = event_tx.clone();
        move || {
            event_tx.blocking_send(Event::ReloadVideoSources).unwrap();
        }
    });

    ui.global::<Bridge>().on_select_input_type({
        let event_tx = event_tx.clone();
        move |input_type| match input_type {
            UiInputType::LocalMedia => event_tx
                .blocking_send(Event::StartLocalMediaSession)
                .unwrap(),
            UiInputType::Mirroring => event_tx
                .blocking_send(Event::StartMirroringSession)
                .unwrap(),
        }
    });

    ui.global::<Bridge>().on_change_dir_child({
        let event_tx = event_tx.clone();
        move |dir_id| {
            event_tx.blocking_send(Event::ChangeDir(dir_id)).unwrap();
        }
    });

    ui.global::<Bridge>().on_change_dir_parent({
        let event_tx = event_tx.clone();
        move || {
            event_tx.blocking_send(Event::ChangeDirParent).unwrap();
        }
    });

    ui.global::<Bridge>().on_cast_local_media({
        let event_tx = event_tx.clone();
        move |file_id| {
            event_tx
                .blocking_send(Event::CastLocalMedia(file_id))
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_str_fmt_seconds(|seconds: f32| {
        let total_seconds = seconds as u32;
        let hours = total_seconds / 60 / 60;
        let minutes = (total_seconds / 60) % 60;
        let seconds = total_seconds % 60;

        format!("{hours:02}:{minutes:02}:{seconds:02}").to_shared_string()
    });

    ui.global::<Bridge>().on_seek({
        let event_tx = event_tx.clone();
        move |seconds: f32, force_complete: bool| {
            event_tx
                .blocking_send(Event::Seek {
                    seconds: seconds as f64,
                    force_complete,
                })
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_change_playback_state({
        let event_tx = event_tx.clone();
        move |state: UiPlaybackState| {
            event_tx
                .blocking_send(Event::ChangePlaybackState(match state {
                    UiPlaybackState::Idle => device::PlaybackState::Idle,
                    UiPlaybackState::Playing => device::PlaybackState::Playing,
                    UiPlaybackState::Paused => device::PlaybackState::Paused,
                    UiPlaybackState::Buffering => device::PlaybackState::Buffering,
                }))
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_change_volume({
        let event_tx = event_tx.clone();
        move |volume: f32, force_complete: bool| {
            event_tx
                .blocking_send(Event::ChangeVolume {
                    volume: volume as f64,
                    force_complete,
                })
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_disconnect({
        let event_tx = event_tx.clone();
        move || {
            event_tx.blocking_send(Event::EndSession).unwrap();
        }
    });

    ui.global::<Bridge>()
        .on_is_valid_ip_address(|address: slint::SharedString| {
            address.as_str().parse::<std::net::IpAddr>().is_ok()
        });

    ui.global::<Bridge>().on_connect_manually({
        let event_tx = event_tx.clone();
        move |proto: UiCastProtocol,
              name_shared: slint::SharedString,
              port: i32,
              address: slint::SharedString| {
            let name = name_shared.to_string();
            let port = port as u16;
            let addresses = match address.as_str().parse::<std::net::IpAddr>() {
                Ok(addr) => vec![fcast_sender_sdk::IpAddr::from(addr)],
                Err(err) => {
                    // NOTE: should be unreachable
                    error!(?err, "Failed to parse address {address}");
                    return;
                }
            };

            let device_info = match proto {
                UiCastProtocol::FCast => {
                    fcast_sender_sdk::device::DeviceInfo::fcast(name, addresses, port)
                }
                UiCastProtocol::GCast => {
                    fcast_sender_sdk::device::DeviceInfo::chromecast(name, addresses, port)
                }
            };

            event_tx
                .blocking_send(Event::DeviceAvailable(device_info))
                .unwrap();
            event_tx
                .blocking_send(Event::ConnectToDevice(name_shared.to_string()))
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_reload_log_string({
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

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    ui.global::<Bridge>().set_is_audio_supported(false);

    #[cfg(target_os = "linux")]
    ui.global::<Bridge>().set_is_audio_supported(true);

    let init_finished_in = init_start.elapsed();
    debug!(?init_finished_in, "Initialization finished");

    ui.run()?;

    let res = runtime.block_on(async move {
        let _ = event_tx.send(Event::Quit).await;
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

        let _ = slint::run_event_loop();
        crash_window.run().unwrap();
    }

    Ok(())
}
