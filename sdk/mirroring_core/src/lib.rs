use fcast_sender_sdk::device::{self, DeviceInfo};
use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

#[cfg(not(target_os = "android"))]
pub mod preview;
pub mod transmission;
pub mod whep_signaller;

#[derive(Clone, Debug)]
pub enum AudioSource {
    #[cfg(target_os = "linux")]
    PulseVirtualSink,
    #[cfg(target_os = "android")]
    None,
}

impl AudioSource {
    pub fn display_name(&self) -> String {
        #[cfg(target_os = "linux")]
        match self {
            AudioSource::PulseVirtualSink => "System Audio".to_owned(),
        }
        #[cfg(target_os = "macos")]
        return "n/a".to_string();
        #[cfg(target_os = "windows")]
        return "n/a".to_string();
        #[cfg(target_os = "android")]
        return "n/a".to_string();
    }
}

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

#[derive(Debug)]
pub enum VideoSource {
    #[cfg(target_os = "linux")]
    PipeWire { node_id: u32, fd: OwnedFd },
    #[cfg(target_os = "linux")]
    XDisplay {
        id: u32,
        width: u16,
        height: u16,
        x_offset: i16,
        y_offset: i16,
        name: String,
    },
    #[cfg(target_os = "macos")]
    CgDisplay { id: i32, name: String },
    #[cfg(target_os = "windows")]
    D3d11Monitor { name: String, handle: u64 },
    #[cfg(target_os = "android")]
    Source(gst_app::AppSrc),
}

impl VideoSource {
    pub fn display_name(&self) -> String {
        match self {
            #[cfg(target_os = "linux")]
            VideoSource::PipeWire { .. } => "PipeWire Video Source".to_owned(),
            #[cfg(target_os = "linux")]
            VideoSource::XDisplay { name, .. } => name.clone(),
            #[cfg(target_os = "macos")]
            VideoSource::CgDisplay { name, .. } => name.clone(),
            #[cfg(target_os = "windows")]
            VideoSource::D3d11Monitor { name, .. } => name.clone(),
            #[cfg(target_os = "android")]
            VideoSource::Source(_) => "Default".to_owned(),
        }
    }
}

#[derive(Debug)]
pub enum SourceConfig {
    #[cfg(not(target_os = "android"))]
    AudioVideo {
        video: VideoSource,
        audio: AudioSource,
    },
    Video(VideoSource),
    #[cfg(not(target_os = "android"))]
    Audio(AudioSource),
}

#[derive(PartialEq, Eq)]
pub enum ShouldQuit {
    Yes,
    No,
}

#[derive(Debug)]
pub enum DeviceEvent {
    StateChanged(device::DeviceConnectionState),
    SourceChanged(device::Source),

    #[cfg(not(target_os = "android"))]
    VolumeChanged(f64),
    #[cfg(not(target_os = "android"))]
    TimeChanged(f64),
    #[cfg(not(target_os = "android"))]
    PlaybackStateChanged(device::PlaybackState),
    #[cfg(not(target_os = "android"))]
    DurationChanged(f64),
    #[cfg(not(target_os = "android"))]
    SpeedChanged(f64),
    // fn key_event(&self, _event: device::KeyEvent) {}
    // #[cfg(not(target_os = "android"))]
    // VolumeChanged(f64),
    // fn media_event(&self, _event: device::MediaEvent) {}
    // #[cfg(not(target_os = "android"))]
    // VolumeChanged(f64),
    // fn playback_error(&self, _message: String) {}
    #[cfg(not(target_os = "android"))]
    PlaybackError(String),
}

#[cfg(not(target_os = "android"))]
#[derive(Debug)]
pub struct FileSystemEntry {
    pub name: String,
    pub is_file: bool,
}

#[cfg(not(target_os = "android"))]
#[derive(Debug)]
pub struct MediaFileEntry {
    pub mime_type: &'static str,
    pub name: String,
}

#[derive(Debug)]
pub enum Event {
    // Common
    EndSession,
    ConnectToDevice(String),
    SignallerStarted {
        bound_port: u16,
    },
    Quit,
    DeviceAvailable(DeviceInfo),
    DeviceRemoved(String),
    DeviceChanged(DeviceInfo),
    FromDevice {
        id: usize,
        event: DeviceEvent,
    },

    // Desktop
    #[cfg(not(target_os = "android"))]
    VideosAvailable(Vec<(usize, VideoSource)>),
    #[cfg(not(target_os = "android"))]
    ReloadVideoSources,
    #[cfg(not(target_os = "android"))]
    StartCast {
        video_uid: Option<usize>,
        audio_uid: Option<usize>,
        scale_width: u32,
        scale_height: u32,
        max_framerate: u32,
    },
    #[cfg(not(target_os = "android"))]
    StartLocalMediaSession,
    #[cfg(not(target_os = "android"))]
    StartMirroringSession,
    #[cfg(not(target_os = "android"))]
    DirectoryListing {
        id: u32,
        entries: Vec<FileSystemEntry>,
    },
    #[cfg(not(target_os = "android"))]
    FilesListing {
        id: u32,
        entries: Vec<MediaFileEntry>,
    },
    #[cfg(not(target_os = "android"))]
    ChangeDir(i32),
    #[cfg(not(target_os = "android"))]
    ChangeDirParent,
    #[cfg(not(target_os = "android"))]
    CastLocalMedia(i32),
    #[cfg(not(target_os = "android"))]
    Seek {
        seconds: f64,
        force_complete: bool,
    },
    #[cfg(not(target_os = "android"))]
    ChangePlaybackState(fcast_sender_sdk::device::PlaybackState),
    #[cfg(not(target_os = "android"))]
    ChangeVolume {
        volume: f64,
        force_complete: bool,
    },

    #[cfg(target_os = "linux")]
    UnsupportedDisplaySystem,

    // Android
    // #[cfg(target_os = "android")]
    // StartCast,
    #[cfg(target_os = "android")]
    CaptureStarted,
    #[cfg(target_os = "android")]
    CaptureStopped,
    #[cfg(target_os = "android")]
    CaptureCancelled,
    #[cfg(target_os = "android")]
    QrScanResult(String),
    #[cfg(target_os = "android")]
    StartCast {
        scale_width: u32,
        scale_height: u32,
        max_framerate: u32,
    },
}

pub struct Discoverer {
    event_tx: UnboundedSender<Event>,
}

impl Discoverer {
    pub fn new(event_tx: UnboundedSender<Event>) -> Self {
        Self {
            event_tx,
        }
    }

    fn send_event(&self, event: Event) {
        if let Err(err) = self.event_tx.send(event) {
            error!("Failed to send event: {err}");
        }
    }
}

impl fcast_sender_sdk::DeviceDiscovererEventHandler for Discoverer {
    fn device_available(&self, device_info: DeviceInfo) {
        self.send_event(Event::DeviceAvailable(device_info));
    }

    fn device_removed(&self, device_name: String) {
        self.send_event(Event::DeviceRemoved(device_name));
    }

    fn device_changed(&self, device_info: DeviceInfo) {
        self.send_event(Event::DeviceChanged(device_info));
    }
}

pub struct DeviceHandler {
    event_tx: UnboundedSender<Event>,
    id: usize,
}

impl DeviceHandler {
    pub fn new(id: usize, event_tx: UnboundedSender<Event>) -> Self {
        Self {
            id,
            event_tx,
        }
    }

    fn send_event(&self, event: DeviceEvent) {
        if let Err(err) = self.event_tx.send(Event::FromDevice { id: self.id, event }) {
            error!("Failed to send event: {err}");
        }
    }
}

impl device::DeviceEventHandler for DeviceHandler {
    fn connection_state_changed(&self, state: device::DeviceConnectionState) {
        self.send_event(DeviceEvent::StateChanged(state));
    }

    fn volume_changed(&self, _volume: f64) {
        #[cfg(not(target_os = "android"))]
        self.send_event(DeviceEvent::VolumeChanged(_volume));
    }

    fn time_changed(&self, _time: f64) {
        #[cfg(not(target_os = "android"))]
        self.send_event(DeviceEvent::TimeChanged(_time));
    }

    fn playback_state_changed(&self, _state: device::PlaybackState) {
        #[cfg(not(target_os = "android"))]
        self.send_event(DeviceEvent::PlaybackStateChanged(_state));
    }

    fn duration_changed(&self, _duration: f64) {
        #[cfg(not(target_os = "android"))]
        self.send_event(DeviceEvent::DurationChanged(_duration));
    }

    fn speed_changed(&self, _speed: f64) {
        #[cfg(not(target_os = "android"))]
        self.send_event(DeviceEvent::SpeedChanged(_speed));
    }

    fn source_changed(&self, source: device::Source) {
        self.send_event(DeviceEvent::SourceChanged(source));
    }

    fn key_event(&self, _event: device::KeyEvent) {}
    fn media_event(&self, _event: device::MediaEvent) {}
    fn playback_error(&self, _message: String) {}
}
