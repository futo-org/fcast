use std::{
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
};

use fcast_sender_sdk::{
    DeviceDiscovererEventHandler,
    context::CastContext,
    device::{
        CompanionSource, CompanionSourceDescriptor, DeviceConnectionState, DeviceEventHandler,
        DeviceInfo, KeyEvent, LoadRequest, MediaEvent, MediaTrack, MediaTrackType, PlaybackState,
        QueueItem, QueuePosition, Source,
    },
};
use slint::{ToSharedString, VecModel};

slint::include_modules!();

enum DeviceEvent {
    ConnectionStateChanged(DeviceConnectionState),
}

enum Message {
    DeviceAvailable(DeviceInfo),
    DeviceRemoved(String),
    DeviceChanged(DeviceInfo),
    FromDevice { id: usize, event: DeviceEvent },
    Connect(String),
    StartCast(i32),
}

struct DiscoveryEventHandler {
    msg_tx: Sender<Message>,
}

impl DeviceDiscovererEventHandler for DiscoveryEventHandler {
    fn device_available(&self, device_info: DeviceInfo) {
        self.msg_tx
            .send(Message::DeviceAvailable(device_info))
            .unwrap();
    }

    fn device_removed(&self, device_name: String) {
        self.msg_tx
            .send(Message::DeviceRemoved(device_name))
            .unwrap();
    }

    fn device_changed(&self, device_info: DeviceInfo) {
        self.msg_tx
            .send(Message::DeviceChanged(device_info))
            .unwrap();
    }
}

struct DevEventHandler {
    event_tx: Sender<Message>,
    id: usize,
}

impl DevEventHandler {
    fn send_event(&self, event: DeviceEvent) {
        self.event_tx
            .send(Message::FromDevice { id: self.id, event })
            .unwrap();
    }
}

impl DeviceEventHandler for DevEventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        self.send_event(DeviceEvent::ConnectionStateChanged(state));
    }

    fn volume_changed(&self, _volume: f64) {}

    fn time_changed(&self, _time: f64) {}

    fn playback_state_changed(&self, _state: PlaybackState) {}

    fn duration_changed(&self, _duration: f64) {}

    fn speed_changed(&self, _speed: f64) {}

    fn source_changed(&self, _source: Source) {}

    fn key_event(&self, _event: KeyEvent) {}

    fn media_event(&self, _event: MediaEvent) {}

    fn playback_stopped(&self) {}

    fn playback_error(&self, message: String) {
        println!("Playback error: {message}");
    }

    fn tracks_available(&self, _tracks: Vec<MediaTrack>) {}

    fn track_selected(&self, _id: Option<u32>, _typ: MediaTrackType) {}
}

struct ImageEntry {
    path: PathBuf,
    mime: &'static str,
}

fn find_images() -> std::io::Result<(Vec<ImageEntry>, Vec<UiFileEntry>)> {
    let dirs = directories::UserDirs::new().unwrap();
    let dir = dirs.picture_dir().unwrap();

    let mut images = Vec::new();
    let mut files = Vec::new();

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };

        let Ok(meta) = entry.metadata() else {
            continue;
        };

        if !meta.is_file() {
            continue;
        }

        let path = entry.path();
        let Ok(typ) = infer::get_from_path(&path) else {
            continue;
        };

        if let Some(typ) = typ {
            if typ.matcher_type() == infer::MatcherType::Image {
                let name = path
                    .file_name()
                    .map(|n| n.to_str().unwrap_or(""))
                    .unwrap_or("n/a")
                    .to_shared_string();
                let img = ImageEntry {
                    path,
                    mime: typ.mime_type(),
                };
                let id = images.len() as i32;
                images.push(img);
                let file = UiFileEntry { id, name };
                files.push(file);
            }
        }
    }

    files.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    Ok((images, files))
}

fn run(ui_weak: slint::Weak<MainWindow>, msg_tx: Sender<Message>, msg_rx: Receiver<Message>) {
    let cast_context = CastContext::new().unwrap();

    let discovery_event_handler = DiscoveryEventHandler {
        msg_tx: msg_tx.clone(),
    };
    cast_context.start_discovery(Arc::new(discovery_event_handler));

    let (images, files) = find_images().unwrap();
    ui_weak
        .upgrade_in_event_loop(move |ui| {
            ui.global::<Bridge>()
                .set_files(Rc::new(VecModel::from(files)).into());
        })
        .unwrap();

    fn update_devices(ui_weak: &slint::Weak<MainWindow>, devices: &HashMap<String, DeviceInfo>) {
        let devs = devices
            .iter()
            .map(|(n, _v)| n.to_shared_string())
            .collect::<Vec<_>>();
        ui_weak
            .upgrade_in_event_loop(move |ui| {
                ui.global::<Bridge>()
                    .set_devices(Rc::new(VecModel::from(devs)).into());
            })
            .unwrap();
    }

    let mut current_device_id = 0;
    let mut current_device = None;
    let mut devices = HashMap::<String, DeviceInfo>::new();
    let mut current_item_idx = None::<usize>;
    while let Ok(msg) = msg_rx.recv() {
        match msg {
            Message::DeviceAvailable(device_info) => {
                devices.insert(device_info.name.clone(), device_info);
                update_devices(&ui_weak, &devices);
            }
            Message::DeviceRemoved(name) => {
                devices.remove(&name);
                update_devices(&ui_weak, &devices);
            }
            Message::DeviceChanged(device_info) => {
                devices.insert(device_info.name.clone(), device_info);
                update_devices(&ui_weak, &devices);
            }
            Message::FromDevice { id, event } => {
                if id != current_device_id {
                    continue;
                }

                match event {
                    DeviceEvent::ConnectionStateChanged(state) => {
                        let new_state = match state {
                            DeviceConnectionState::Disconnected => {
                                current_device = None;
                                UiDeviceState::Disconnected
                            }
                            DeviceConnectionState::Connecting
                            | DeviceConnectionState::Reconnecting => UiDeviceState::Connecting,
                            DeviceConnectionState::Connected { .. } => UiDeviceState::Connected,
                        };
                        ui_weak
                            .upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>().set_device_state(new_state);
                            })
                            .unwrap();
                    }
                }
            }
            Message::Connect(name) => {
                let info = devices.get(&name).unwrap();
                let device = cast_context.create_device_from_info(info.clone());
                current_device_id += 1;
                device
                    .connect(
                        None,
                        Arc::new(DevEventHandler {
                            event_tx: msg_tx.clone(),
                            id: current_device_id,
                        }),
                        1000,
                    )
                    .unwrap();
                current_device = Some(device);
            }
            Message::StartCast(id) => {
                if let Some(device) = &current_device {
                    let id = id as usize;
                    let img = &images[id];

                    fn create_item(img: &ImageEntry) -> QueueItem {
                        QueueItem::FCompanion {
                            content_type: img.mime.to_owned(),
                            source: CompanionSource {
                                descriptor: CompanionSourceDescriptor::Path(
                                    img.path.to_str().unwrap().to_owned(),
                                ),
                                content_type: img.mime.to_owned(),
                            },
                            metadata: None,
                        }
                    }

                    if let Some(current_idx) = current_item_idx.as_mut() {
                        if *current_idx == id {
                            continue;
                        };

                        let go_left = id < *current_idx;
                        let id_to_queue = if go_left { id - 1 } else { id + 1 };
                        let new_item = create_item(&images[id_to_queue]);

                        let next_pos = if go_left { 0 } else { 2 };

                        *current_idx = id;
                        device.queue_select(QueuePosition::Index(next_pos)).unwrap();

                        if go_left {
                            device.queue_remove(QueuePosition::Back).unwrap();
                            device.queue_add(new_item, QueuePosition::Front).unwrap();
                        } else {
                            device.queue_remove(QueuePosition::Front).unwrap();
                            device.queue_add(new_item, QueuePosition::Back).unwrap();
                        }
                    } else {
                        current_item_idx = Some(id);
                        let items = vec![
                            create_item(&images[id - 1]),
                            create_item(&images[id]),
                            create_item(&images[id + 1]),
                        ];
                        device
                            .load(LoadRequest::Queue {
                                items,
                                start_index: Some(1),
                            })
                            .unwrap();
                    }

                    if let Ok(img) = image::ImageReader::open(&img.path).unwrap().decode() {
                        let img = img.to_rgba8();
                        ui_weak
                            .upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>()
                                    .set_current_preview(
                                        slint::Image::from_rgba8(slint::SharedPixelBuffer::<
                                            slint::Rgba8Pixel,
                                        >::clone_from_slice(
                                            img.as_raw(),
                                            img.width(),
                                            img.height(),
                                        )),
                                    )
                            })
                            .unwrap();
                    }
                } else {
                    panic!("No device");
                }
            }
        }
    }
}

fn main() {
    env_logger::Builder::new()
        .filter(Some("fcast_sender_sdk"), log::LevelFilter::Debug)
        .init();

    let ui = MainWindow::new().unwrap();

    let bridge = ui.global::<Bridge>();

    let (msg_tx, msg_rx) = channel();

    let ui_weak = ui.as_weak();
    std::thread::spawn({
        let msg_tx = msg_tx.clone();
        move || {
            run(ui_weak, msg_tx, msg_rx);
        }
    });

    bridge.on_connect({
        let msg_tx = msg_tx.clone();
        move |name| {
            msg_tx.send(Message::Connect(name.to_string())).unwrap();
        }
    });

    bridge.on_start_cast({
        let msg_tx = msg_tx.clone();
        move |id| {
            msg_tx.send(Message::StartCast(id)).unwrap();
        }
    });

    ui.run().unwrap();
}
