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
        CastingDevice, CompanionSource, CompanionSourceDescriptor, DeviceConnectionState,
        DeviceEventHandler, DeviceInfo, LoadRequest, MediaTrack, MediaTrackType, PlaybackState,
        QueueItem, QueuePosition, QueueState, ReceiverError, Source, TrackList,
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

    fn playback_stopped(&self) {}

    fn playback_error(&self, message: String) {
        println!("Playback error: {message}");
    }

    fn tracks_available(&self, _tracks: Vec<MediaTrack>) {}

    fn track_selected(&self, _id: Option<u32>, _typ: MediaTrackType) {}

    fn tracks_changed(&self, _tracks: TrackList) {}

    fn queue_changed(&self, _queue: QueueState) {}

    fn command_error(&self, _error: ReceiverError) {}
}

struct ImageEntry {
    path: PathBuf,
    mime: &'static str,
}

fn find_images() -> std::io::Result<(Vec<ImageEntry>, Vec<UiFileEntry>)> {
    let mut images = Vec::new();
    let mut files = Vec::new();

    // No user dirs or no Pictures directory means there is nothing to list. Return
    // an empty catalog instead of panicking.
    let Some(dirs) = directories::UserDirs::new() else {
        log::warn!("Could not determine user directories, no images to list");
        return Ok((images, files));
    };
    let Some(dir) = dirs.picture_dir() else {
        log::warn!("Could not determine Pictures directory, no images to list");
        return Ok((images, files));
    };

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

    let (images, files) = match find_images() {
        Ok(res) => res,
        Err(e) => {
            log::warn!("Failed to read images: {e}");
            (Vec::new(), Vec::new())
        }
    };
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
                let Some(info) = devices.get(&name) else {
                    log::warn!("Cannot connect: device '{name}' is no longer available");
                    continue;
                };
                let device = cast_context.create_device_from_info(info.clone());
                current_device_id += 1;
                if let Err(e) = device.connect(
                    None,
                    Arc::new(DevEventHandler {
                        event_tx: msg_tx.clone(),
                        id: current_device_id,
                    }),
                    1000,
                ) {
                    log::warn!("Failed to connect to device '{name}': {e}");
                    continue;
                }
                current_device = Some(device);
            }
            Message::StartCast(id) => {
                let Some(device) = &current_device else {
                    log::warn!("Cannot cast: no device connected");
                    continue;
                };
                let id = id as usize;
                let Some(img) = images.get(id) else {
                    log::warn!("Cannot cast: image index {id} is out of range");
                    continue;
                };

                fn create_item(img: &ImageEntry) -> QueueItem {
                    QueueItem::FCompanion {
                        content_type: img.mime.to_owned(),
                        source: CompanionSource {
                            descriptor: CompanionSourceDescriptor::Path(
                                img.path.to_string_lossy().into_owned(),
                            ),
                            content_type: img.mime.to_owned(),
                        },
                        metadata: None,
                    }
                }

                // The prefetch window is the selected image plus its immediate neighbours that
                // actually exist, so it holds 1, 2 or 3 items. Returns the inclusive
                // [lo, hi] range of image indices in the window.
                fn window_range(id: usize, n: usize) -> (usize, usize) {
                    let lo = id.saturating_sub(1);
                    let hi = if id + 1 < n { id + 1 } else { id };
                    (lo, hi)
                }

                // Rebuild the whole window and (re)load it. Used for the first cast and for
                // non-adjacent jumps where the incremental select/add/remove dance cannot
                // express the transition.
                fn load_window(device: &dyn CastingDevice, images: &[ImageEntry], id: usize) {
                    let (lo, hi) = window_range(id, images.len());
                    let items = images[lo..=hi].iter().map(create_item).collect();
                    let start_index = Some((id - lo) as u8);
                    if let Err(e) = device.load(LoadRequest::Queue { items, start_index }, None) {
                        log::warn!("Failed to load queue: {e}");
                    }
                }

                let n = images.len();
                match current_item_idx {
                    Some(prev) if prev == id => continue,
                    // Adjacent moves shift the window by one, so they can be expressed as a select
                    // followed by at most one remove and one add. Anything else is a full reload.
                    Some(prev) if prev.abs_diff(id) == 1 => {
                        let (o_lo, o_hi) = window_range(prev, n);
                        let (n_lo, n_hi) = window_range(id, n);

                        // The new current is always still present in the old window on an adjacent
                        // move, so select it there first. Selecting before removing also keeps us
                        // from ever removing the currently playing item, which the receiver
                        // refuses.
                        let select_idx = (id - o_lo) as u8;
                        if let Err(e) = device.queue_select(QueuePosition::Index(select_idx)) {
                            log::warn!("Failed to select queue item: {e}");
                        }

                        // Drop the old-window item that fell out of the new window. It is always at
                        // an edge: the front when moving right, the back when moving left.
                        if o_lo < n_lo {
                            if let Err(e) = device.queue_remove(QueuePosition::Front) {
                                log::warn!("Failed to remove queue item: {e}");
                            }
                        } else if o_hi > n_hi {
                            if let Err(e) = device.queue_remove(QueuePosition::Back) {
                                log::warn!("Failed to remove queue item: {e}");
                            }
                        }

                        // Add the new-window item that was not already present, again at an edge:
                        // the back when moving right, the front when moving left.
                        if n_hi > o_hi {
                            let item = create_item(&images[n_hi]);
                            if let Err(e) = device.queue_add(item, QueuePosition::Back) {
                                log::warn!("Failed to add queue item: {e}");
                            }
                        } else if n_lo < o_lo {
                            let item = create_item(&images[n_lo]);
                            if let Err(e) = device.queue_add(item, QueuePosition::Front) {
                                log::warn!("Failed to add queue item: {e}");
                            }
                        }

                        current_item_idx = Some(id);
                    }
                    // First cast, or a non-adjacent jump: reload the whole window.
                    _ => {
                        current_item_idx = Some(id);
                        load_window(device.as_ref(), &images, id);
                    }
                }

                let decoded = match image::ImageReader::open(&img.path) {
                    Ok(reader) => reader.decode(),
                    Err(e) => {
                        log::warn!("Failed to open preview image: {e}");
                        continue;
                    }
                };
                match decoded {
                    Ok(img) => {
                        let img = img.to_rgba8();
                        if let Err(e) = ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.global::<Bridge>()
                                .set_current_preview(slint::Image::from_rgba8(
                                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        img.as_raw(),
                                        img.width(),
                                        img.height(),
                                    ),
                                ))
                        }) {
                            log::warn!("Failed to update preview: {e}");
                        }
                    }
                    Err(e) => log::warn!("Failed to decode preview image: {e}"),
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
