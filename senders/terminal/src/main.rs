use clap::{Parser, Subcommand};
use fcast_sender_sdk::{
    context::CastContext,
    device::{
        DeviceConnectionState, DeviceEventHandler, EventSubscription, KeyEvent, KeyName,
        LoadRequest, MediaEvent, PlaybackState, Source,
    },
};
use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Sender},
        Arc,
    },
    time::Duration,
};

#[derive(Subcommand)]
enum Command {
    /// Play media
    Play {
        /// Mime type (e.g., video/mp4)
        #[arg(long, short)]
        mime_type: Option<String>,
        /// File content to play
        #[arg(long, short)]
        file: Option<String>,
        /// URL to the content
        #[arg(long, short)]
        url: Option<String>,
        /// The actual content
        #[arg(long, short)]
        content: Option<String>,
        /// Timestamp to start playing
        #[arg(long, short)]
        timestamp: Option<f64>,
        /// Factor to multiply playback speed by
        #[arg(long, short)]
        speed: Option<f64>,
        /// Custom request headers in key:value format
        #[arg(long, short('H'))]
        header: Vec<String>,
        /// The desired volume
        #[arg(long, short)]
        volume: Option<f64>,
        /// The port that the file server should bind to
        #[arg(long)]
        file_server_port: Option<u16>,
    },
    /// Seek to a timestamp
    Seek {
        /// Timestamp to start playing
        #[arg(long, short)]
        timestamp: f64,
    },
    /// Pause media
    Pause,
    /// Resume media
    Resume,
    /// Stop media
    Stop,
    /// Listen to incoming events
    Listen,
    /// Set the volume
    SetVolume {
        /// Volume level (0-1)
        #[arg(long, short)]
        volume: f64,
    },
    /// Set the playback speed
    SetSpeed {
        /// Factor to multiply playback speed by
        #[arg(long, short)]
        speed: f64,
    },
    SetPlaylistItem {
        /// Index of the item in the playlist that should be play
        #[arg(long, short)]
        item_index: u32,
    },
}

#[derive(Parser)]
#[command(version)]
struct TerminalSender {
    /// The host address to send the command to
    #[arg(long, short('H'), default_value_t = String::from("127.0.0.1"))]
    host: String,
    /// The port to send the command to
    #[arg(long, short, default_value_t = 46899)]
    port: u16,
    /// A comma separated list of events to subscribe to (e.g. MediaItemStart,KeyDown).
    /// Available events: [MediaItemStart, MediaItemEnd, MediaItemChange, KeyDown, KeyUp]
    #[arg(long, short)]
    subscriptions: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(PartialEq, Eq)]
enum Event {
    Connected(fcast_sender_sdk::IpAddr),
    Disconnected,
}

struct EventHandler {
    tx: Sender<Event>,
}

impl EventHandler {
    pub fn new(tx: Sender<Event>) -> Self {
        Self { tx }
    }
}

impl DeviceEventHandler for EventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        println!("Connection state changed: {state:#?}");
        match state {
            DeviceConnectionState::Disconnected => {
                let _ = self.tx.send(Event::Disconnected);
            }
            DeviceConnectionState::Connected { local_addr, .. } => {
                let _ = self.tx.send(Event::Connected(local_addr));
            }
            _ => (),
        }
    }

    fn volume_changed(&self, volume: f64) {
        println!("Volume changed: {volume}");
    }

    fn time_changed(&self, time: f64) {
        println!("Time changed: {time}");
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        println!("Playback state changed: {state:?}");
    }

    fn duration_changed(&self, duration: f64) {
        println!("Duration changed: {duration}");
    }

    fn speed_changed(&self, speed: f64) {
        println!("Speed changed: {speed}");
    }

    fn source_changed(&self, source: Source) {
        println!("Source changed: {source:#?}");
    }

    fn key_event(&self, event: KeyEvent) {
        println!("Key event: {event:#?}");
    }

    fn media_event(&self, event: MediaEvent) {
        println!("Media event: {event:#?}");
    }

    fn playback_error(&self, message: String) {
        eprintln!("Playback error: {message}");
    }
}

fn main() {
    env_logger::init();

    let app = TerminalSender::parse();

    let context = CastContext::new().unwrap();
    #[allow(unused)]
    let file_server;

    let device_info = fcast_sender_sdk::device::DeviceInfo::fcast(
        "FCast Receiver".to_owned(),
        vec![app.host.parse::<IpAddr>().unwrap().into()],
        app.port,
    );

    let device = context.create_device_from_info(device_info);

    let (tx, rx) = channel();

    device
        .connect(None, Arc::new(EventHandler::new(tx)), 1000)
        .unwrap();

    println!("Connecting...");

    let Event::Connected(local_addr) = rx.recv().unwrap() else {
        eprintln!("Failed to connect");
        std::process::exit(1);
    };

    if let Some(subscriptions) = app.subscriptions {
        let subs = subscriptions.split(',');
        for sub in subs {
            let subscription = match sub.to_lowercase().as_str() {
                "mediaitemstart" => EventSubscription::MediaItemStart,
                "mediaitemend" => EventSubscription::MediaItemEnd,
                "mediaitemchange" => EventSubscription::MediaItemChange,
                "keydown" => EventSubscription::KeyDown {
                    keys: KeyName::all(),
                },
                "keyup" => EventSubscription::KeyUp {
                    keys: KeyName::all(),
                },
                _ => {
                    println!("Invalid event in subscriptions list: {sub}");
                    continue;
                }
            };
            device.subscribe_event(subscription.clone()).unwrap();
            println!("Subscribed to {subscription:?}");
        }
    }

    let quit = Arc::new(AtomicBool::new(true));

    match app.command {
        Command::Play {
            mime_type,
            file,
            url,
            content,
            timestamp,
            speed,
            header,
            volume,
            file_server_port,
        } => {
            fn default_mime_type() -> String {
                println!("No mime type provided via the `--mime_type` argument. Using default (application/octet-stream)");
                "application/octet-stream".to_string()
            }

            let mime_type = match mime_type {
                Some(s) => s.to_string(),
                _ => match &file {
                    Some(path) => match path.split('.').next_back() {
                        Some("mkv") => "video/x-matroska".to_string(),
                        Some("mov") => "video/quicktime".to_string(),
                        Some("mp4") | Some("m4v") => "video/mp4".to_string(),
                        Some("mpg") | Some("mpeg") => "video/mpeg".to_string(),
                        Some("webm") => "video/webm".to_string(),
                        _ => default_mime_type(),
                    },
                    None => default_mime_type(),
                },
            };

            let headers = header
                .iter()
                .filter_map(|s| {
                    let mut parts = s.splitn(2, ':');
                    if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                        Some((key.trim().to_string(), value.trim().to_string()))
                    } else {
                        None
                    }
                })
                .collect::<HashMap<String, String>>();
            let headers = if headers.is_empty() {
                None
            } else {
                Some(headers)
            };

            if file.is_some() || url.is_some() {
                let url = if let Some(file_path) = file {
                    let file = File::open(file_path).unwrap();
                    let server = context.start_file_server(file_server_port);
                    let mut retries = 0;
                    while !server.is_running() {
                        if retries >= 10 {
                            panic!("Failed to serve file");
                        }
                        retries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    let entry = server.serve_rs_file(file).unwrap();
                    file_server = server;
                    let url = format!(
                        "http://{}:{}/{}",
                        fcast_sender_sdk::url_format_ip_addr(&local_addr),
                        entry.port,
                        entry.location,
                    );

                    quit.store(false, Ordering::SeqCst);
                    let quit = Arc::clone(&quit);
                    ctrlc::set_handler(move || {
                        quit.store(true, Ordering::SeqCst);
                    })
                    .unwrap();
                    url
                } else {
                    url.unwrap()
                };
                device
                    .load(LoadRequest::Url {
                        content_type: mime_type,
                        url,
                        resume_position: timestamp,
                        speed,
                        volume,
                        metadata: None,
                        request_headers: headers,
                    })
                    .unwrap();
            } else {
                let content = match content {
                    Some(c) => c,
                    None => {
                        println!("Reading content from stdin...");
                        let mut buffer = String::new();
                        std::io::stdin().read_to_string(&mut buffer).unwrap();
                        buffer
                    }
                };
                device
                    .load(LoadRequest::Content {
                        content_type: mime_type,
                        content,
                        resume_position: timestamp.unwrap_or(0.0),
                        speed,
                        volume,
                        metadata: None,
                        request_headers: headers,
                    })
                    .unwrap();
            }
        }
        Command::Seek { timestamp } => device.seek(timestamp).unwrap(),
        Command::Pause => device.pause_playback().unwrap(),
        Command::Resume => device.resume_playback().unwrap(),
        Command::Stop => device.stop_playback().unwrap(),
        Command::Listen => {
            quit.store(false, Ordering::SeqCst);
            let quit = Arc::clone(&quit);
            ctrlc::set_handler(move || {
                quit.store(true, Ordering::SeqCst);
            })
            .unwrap();
        }
        Command::SetVolume { volume } => device.change_volume(volume).unwrap(),
        Command::SetSpeed { speed } => device.change_speed(speed).unwrap(),
        Command::SetPlaylistItem { item_index } => {
            device.set_playlist_item_index(item_index).unwrap()
        }
    }

    while !quit.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Give the device some time to flush it's message queues
    std::thread::sleep(Duration::from_millis(500));

    device.disconnect().unwrap();

    println!("Disconnecting...");

    let _ = rx.recv().unwrap();
    // Suppress compiler warning
    let _ = file_server;
}
