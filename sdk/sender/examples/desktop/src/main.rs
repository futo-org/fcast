use std::{rc::Rc, sync::Arc};

use fcast_sender_sdk::{
    context::CastContext,
    device::{
        CastingDevice, DeviceConnectionState, DeviceEventHandler, DeviceInfo, GenericKeyEvent,
        GenericMediaEvent, LoadRequest, PlaybackState, ProtocolType, Source,
    },
    file_server::FileServer,
    url_format_ip_addr, DeviceDiscovererEventHandler, IpAddr,
};
use log::{debug, error};
use rfd::{AsyncFileDialog, FileHandle};
use slint::{Model, SharedString, VecModel};
use tokio::{
    runtime::Runtime,
    sync::mpsc::{channel, Receiver, Sender},
};

slint::include_modules!();

#[derive(Debug)]
enum DeviceEvent {
    ConnectionStateChanged(DeviceConnectionState),
    VolumeChanged(f64),
    TimeChanged(f64),
    PlaybackStateChanged(PlaybackState),
    DurationChanged(f64),
    SpeedChanged(f64),
    SourceChanged(Source),
}

#[derive(Debug)]
enum Event {
    Quit,
    DeviceAvailable(DeviceInfo),
    DeviceRemoved(String),
    DeviceChanged(DeviceInfo),
    Connect(String),
    Disconnect,
    FromDevice {
        id: usize,
        event: DeviceEvent,
    },
    /// User requested that a local file should be casted
    CastLocalRequested,
    CastLocal {
        media_type: infer::Type,
        handle: FileHandle,
    },
    ChangeVolume(f64),
    Seek(f64),
}

struct DiscoveryEventHandler {
    event_tx: Sender<Event>,
}

impl DiscoveryEventHandler {
    pub fn new(event_tx: Sender<Event>) -> Self {
        Self { event_tx }
    }
}

impl DeviceDiscovererEventHandler for DiscoveryEventHandler {
    fn device_available(&self, device_info: DeviceInfo) {
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            event_tx
                .send(Event::DeviceAvailable(device_info))
                .await
                .unwrap();
        });
    }

    fn device_removed(&self, device_name: String) {
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            event_tx
                .send(Event::DeviceRemoved(device_name))
                .await
                .unwrap();
        });
    }

    fn device_changed(&self, device_info: DeviceInfo) {
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            event_tx
                .send(Event::DeviceChanged(device_info))
                .await
                .unwrap();
        });
    }
}

struct DevEventHandler {
    event_tx: Sender<Event>,
    id: usize,
}

impl DevEventHandler {
    pub fn new(event_tx: Sender<Event>, id: usize) -> Self {
        Self { event_tx, id }
    }

    fn send_event(&self, event: DeviceEvent) {
        let id = self.id;
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = event_tx.send(Event::FromDevice { id, event }).await {
                error!("Failed to send event: {err}");
            }
        });
    }
}

impl DeviceEventHandler for DevEventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        self.send_event(DeviceEvent::ConnectionStateChanged(state));
    }

    fn volume_changed(&self, volume: f64) {
        self.send_event(DeviceEvent::VolumeChanged(volume));
    }

    fn time_changed(&self, time: f64) {
        self.send_event(DeviceEvent::TimeChanged(time));
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        self.send_event(DeviceEvent::PlaybackStateChanged(state));
    }

    fn duration_changed(&self, duration: f64) {
        self.send_event(DeviceEvent::DurationChanged(duration));
    }

    fn speed_changed(&self, speed: f64) {
        self.send_event(DeviceEvent::SpeedChanged(speed));
    }

    fn source_changed(&self, source: Source) {
        self.send_event(DeviceEvent::SourceChanged(source));
    }

    fn key_event(&self, _event: GenericKeyEvent) {}

    fn media_event(&self, _event: GenericMediaEvent) {}

    fn playback_error(&self, message: String) {
        error!("Playback error: {message}");
    }
}

struct App {
    ui_weak: slint::Weak<MainWindow>,
    cast_context: CastContext,
    event_tx: Sender<Event>,
    file_server: FileServer,
}

impl App {
    pub async fn new(
        ui_weak: slint::Weak<MainWindow>,
        event_tx: Sender<Event>,
    ) -> anyhow::Result<Self> {
        let cast_context = CastContext::new()?;

        let discovery_event_handler = DiscoveryEventHandler::new(event_tx.clone());
        cast_context.start_discovery(Arc::new(discovery_event_handler));

        let file_server = cast_context.start_file_server();

        Ok(Self {
            ui_weak,
            cast_context,
            event_tx,
            file_server,
        })
    }

    fn init_models(&self) -> anyhow::Result<()> {
        self.ui_weak.upgrade_in_event_loop(|ui| {
            ui.global::<Bridge>()
                .set_devices(Rc::new(VecModel::<Device>::default()).into());
        })?;

        Ok(())
    }

    fn add_device_to_list(&self, device_info: &DeviceInfo) -> anyhow::Result<()> {
        let type_ = match device_info.protocol {
            ProtocolType::Chromecast => DeviceType::Chromecast,
            ProtocolType::FCast => DeviceType::FCast,
        };
        let name = SharedString::from(device_info.name.clone());
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let model = ui.global::<Bridge>().get_devices();
            let model = model
                .as_any()
                .downcast_ref::<slint::VecModel<Device>>()
                .unwrap();
            model.push(Device {
                name,
                r#type: type_,
            })
        })?;

        Ok(())
    }

    fn remove_device_from_list(&self, idx: usize) -> anyhow::Result<()> {
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let model = ui.global::<Bridge>().get_devices();
            let model = model
                .as_any()
                .downcast_ref::<slint::VecModel<Device>>()
                .unwrap();
            model.remove(idx);
        })?;
        Ok(())
    }

    pub async fn run(self, mut event_rx: Receiver<Event>) -> anyhow::Result<()> {
        self.init_models()?;

        let mut devices: Vec<DeviceInfo> = Vec::new();
        let mut active_device: Option<Arc<dyn CastingDevice>> = None;
        let mut current_device_id: usize = 0;
        let mut local_adddress = IpAddr::v4(127, 0, 0, 1);

        loop {
            let Some(event) = event_rx.recv().await else {
                break;
            };

            debug!("Got event: {event:?}");

            match event {
                Event::Quit => break,
                Event::DeviceAvailable(device_info) => {
                    self.add_device_to_list(&device_info)?;
                    devices.push(device_info);
                }
                Event::DeviceRemoved(name) => {
                    let mut idx = None;
                    for (i, device) in devices.iter().enumerate() {
                        if device.name == name {
                            idx = Some(i);
                            break;
                        }
                    }
                    if let Some(idx) = idx {
                        devices.swap_remove(idx);
                        self.remove_device_from_list(idx)?;
                    }
                }
                Event::DeviceChanged(device_info) => {
                    if let Some(device) = devices
                        .iter_mut()
                        .find(|device| device.name == device_info.name)
                    {
                        device.addresses = device_info.addresses;
                        device.port = device_info.port;
                    }
                }
                Event::Connect(device_name) => {
                    if let Some(device_info) = devices
                        .iter()
                        .find(|device| device.name == device_name)
                        .cloned()
                    {
                        let device = self.cast_context.create_device_from_info(device_info);
                        device.connect(
                            None,
                            Arc::new(DevEventHandler::new(
                                self.event_tx.clone(),
                                current_device_id,
                            )),
                        )?;
                        active_device = Some(device);
                    }
                }
                Event::Disconnect => {
                    if let Some(active_device) = active_device.take() {
                        active_device.disconnect()?;
                        current_device_id += 1;
                    }
                }
                Event::FromDevice { id, event } => {
                    if id != current_device_id {
                        debug!(
                            "Received event from old device ({id}, current is {current_device_id})"
                        );
                        continue;
                    }
                    match event {
                        DeviceEvent::ConnectionStateChanged(state) => match state {
                            DeviceConnectionState::Disconnected => (),
                            DeviceConnectionState::Connecting => (),
                            DeviceConnectionState::Reconnecting => {
                                self.ui_weak.upgrade_in_event_loop(|ui| {
                                    ui.global::<Bridge>().set_state(State::Connecting);
                                })?;
                            },
                            DeviceConnectionState::Connected { local_addr, .. } => {
                                local_adddress = local_addr;
                                self.ui_weak.upgrade_in_event_loop(|ui| {
                                    ui.global::<Bridge>().invoke_connected();
                                })?;
                            }
                        },
                        DeviceEvent::VolumeChanged(volume) => {
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>().set_volume(volume as f32);
                            })?
                        }
                        DeviceEvent::TimeChanged(time) => {
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>().set_playback_position(time as f32);
                            })?
                        }
                        DeviceEvent::PlaybackStateChanged(state) => match state {
                            PlaybackState::Idle => (),
                            PlaybackState::Buffering => (),
                            PlaybackState::Playing => (),
                            PlaybackState::Paused => (),
                        },
                        DeviceEvent::DurationChanged(duration) => {
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>().set_playback_duration(duration as f32);
                            })?
                        }
                        DeviceEvent::SpeedChanged(_) => (),
                        DeviceEvent::SourceChanged(source) => (),
                    }
                }
                Event::CastLocalRequested => {
                    let event_tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        let maybe_path = AsyncFileDialog::new()
                            .add_filter(
                                "Media",
                                &[
                                    "png", "jpg", "jpeg", "avif", "mkv", "mp4", "webm", "flac",
                                    "opus", "mp3", "mka", "m4a", "wav", "ogg", "vorbis", "apng",
                                    "gif", "webp",
                                ],
                            )
                            .add_filter("All", &["*"])
                            .pick_file()
                            .await;
                        debug!("User opened: {maybe_path:?}");
                        if let Some(handle) = maybe_path {
                            match infer::get_from_path(handle.path()) {
                                Ok(res) => match res {
                                    Some(type_) => {
                                        event_tx
                                            .send(Event::CastLocal {
                                                media_type: type_,
                                                handle,
                                            })
                                            .await
                                            .unwrap();
                                    }
                                    None => error!("Unable to get file type"),
                                },
                                Err(err) => {
                                    error!("Failed to infer type of file: {err}");
                                }
                            };
                        }
                    });
                }
                Event::CastLocal { media_type, handle } => {
                    let matcher_type = media_type.matcher_type();
                    if !matches!(
                        matcher_type,
                        infer::MatcherType::Audio
                            | infer::MatcherType::Image
                            | infer::MatcherType::Video
                    ) {
                        error!("Unsupported media type {matcher_type:?}");
                        continue;
                    }
                    let file = match std::fs::File::open(handle.path()) {
                        Ok(file) => file,
                        Err(err) => {
                            error!("Failed to open file {handle:?}: {err}");
                            continue;
                        }
                    };
                    match self.file_server.serve_rs_file(file) {
                        Ok(entry) => match active_device.as_ref() {
                            Some(active_device) => {
                                let url = format!(
                                    "http://{}:{}/{}",
                                    url_format_ip_addr(&local_adddress),
                                    entry.port,
                                    entry.location,
                                );
                                active_device
                                    .load(LoadRequest::Url {
                                        content_type: media_type.mime_type().to_string(),
                                        url,
                                        resume_position: None,
                                        speed: None,
                                        volume: None,
                                        metadata: None,
                                        request_headers: None,
                                    })
                                    .unwrap();
                            }
                            None => error!("Not connected"),
                        },
                        Err(err) => error!("Failed to serve file: {err}"),
                    }
                }
                Event::ChangeVolume(new_volume) => {
                    if let Some(active_device) = active_device.as_ref() {
                        active_device.change_volume(new_volume)?;
                    }
                }
                Event::Seek(new_position) => {
                    if let Some(active_device) = active_device.as_ref() {
                        active_device.seek(new_position)?;
                    }
                }
            }
        }

        debug!("Finished");

        if let Some(active_device) = active_device.take() {
            active_device.disconnect()?;
        }

        Ok(())
    }
}

fn main() {
    env_logger::Builder::new()
        .filter(None, log::LevelFilter::Debug)
        .init();

    let runtime = Runtime::new().unwrap();

    let (event_tx, event_rx) = channel::<Event>(100);

    let ui = MainWindow::new().unwrap();

    let ui_weak = ui.as_weak();
    let event_tx_clone = event_tx.clone();
    let app_jh = runtime.spawn(async move {
        let app = App::new(ui_weak, event_tx_clone).await?;
        app.run(event_rx).await
    });

    {
        let event_tx = event_tx.clone();
        ui.global::<Bridge>().on_connect(move |device_name| {
            event_tx
                .blocking_send(Event::Connect(device_name.to_string()))
                .unwrap();
        });
    }

    {
        let event_tx = event_tx.clone();
        ui.global::<Bridge>().on_disconnect(move || {
            event_tx.blocking_send(Event::Disconnect).unwrap();
        });
    }

    {
        let event_tx = event_tx.clone();
        ui.global::<Bridge>().on_cast_local(move || {
            event_tx.blocking_send(Event::CastLocalRequested).unwrap();
        });
    }

    {
        let event_tx = event_tx.clone();
        ui.global::<Bridge>().on_change_volume(move |new_volume| {
            event_tx
                .blocking_send(Event::ChangeVolume(new_volume as f64))
                .unwrap();
        });
    }

    {
        let event_tx = event_tx.clone();
        ui.global::<Bridge>().on_seek(move |new_position| {
            event_tx
                .blocking_send(Event::Seek(new_position as f64))
                .unwrap();
        });
    }

    ui.run().unwrap();

    runtime.block_on(async move {
        event_tx.send(Event::Quit).await.unwrap();
        if let Err(err) = app_jh.await {
            error!("Error occured when running: {err}");
        }
    });
}
