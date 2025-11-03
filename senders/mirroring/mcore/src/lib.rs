use fcast_sender_sdk::device::{self, DeviceInfo};
use tokio::{runtime, sync::mpsc::Sender};
use tracing::error;

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
    XWindow { id: u32, name: String },
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
            VideoSource::XWindow { name, .. } => name.clone(),
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
    AudioVideo {
        video: VideoSource,
        audio: AudioSource,
    },
    Video(VideoSource),
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
}

#[derive(Debug)]
pub enum Event {
    // Common
    StopCast,
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
}

pub struct Discoverer {
    event_tx: Sender<Event>,
    rt_handle: runtime::Handle,
}

impl Discoverer {
    pub fn new(event_tx: Sender<Event>, rt_handle: runtime::Handle) -> Self {
        Self {
            event_tx,
            rt_handle,
        }
    }

    fn send_event_spawned(&self, event: Event) {
        let tx = self.event_tx.clone();
        self.rt_handle.spawn(async move {
            if let Err(err) = tx.send(event).await {
                error!("Failed to send event: {err}");
            }
        });
    }
}

impl fcast_sender_sdk::DeviceDiscovererEventHandler for Discoverer {
    fn device_available(&self, device_info: DeviceInfo) {
        self.send_event_spawned(Event::DeviceAvailable(device_info));
    }

    fn device_removed(&self, device_name: String) {
        self.send_event_spawned(Event::DeviceRemoved(device_name));
    }

    fn device_changed(&self, device_info: DeviceInfo) {
        self.send_event_spawned(Event::DeviceChanged(device_info));
    }
}

pub struct DeviceHandler {
    event_tx: Sender<Event>,
    rt_handle: runtime::Handle,
    id: usize,
}

impl DeviceHandler {
    pub fn new(id: usize, event_tx: Sender<Event>, rt_handle: runtime::Handle) -> Self {
        Self {
            id,
            event_tx,
            rt_handle,
        }
    }

    fn send_event_spawned(&self, event: DeviceEvent) {
        let tx = self.event_tx.clone();
        let id = self.id;
        self.rt_handle.spawn(async move {
            if let Err(err) = tx.send(Event::FromDevice { id, event }).await {
                error!("Failed to send event: {err}");
            }
        });
    }
}

impl device::DeviceEventHandler for DeviceHandler {
    fn connection_state_changed(&self, state: device::DeviceConnectionState) {
        self.send_event_spawned(DeviceEvent::StateChanged(state));
    }

    fn volume_changed(&self, _volume: f64) {}
    fn time_changed(&self, _time: f64) {}
    fn playback_state_changed(&self, _state: device::PlaybackState) {}
    fn duration_changed(&self, _duration: f64) {}
    fn speed_changed(&self, _speed: f64) {}

    fn source_changed(&self, source: device::Source) {
        self.send_event_spawned(DeviceEvent::SourceChanged(source));
    }

    fn key_event(&self, _event: device::KeyEvent) {}
    fn media_event(&self, _event: device::MediaEvent) {}
    fn playback_error(&self, _message: String) {}
}
