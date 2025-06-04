use clap::{App, Arg, SubCommand};
use fcast::models::v3;
use fcast::transport::WebSocket;
use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Instant;
use std::{fs, thread};
use std::{io::Read, net::TcpStream};
use std::{sync::Arc, time::Duration};
use tiny_http::{Header, ListenAddr, Response, Server};
use tungstenite::stream::MaybeTlsStream;
use url::Url;

use fcast::fcastsession::Opcode;
use fcast::{
    fcastsession::FCastSession,
    models::{SeekMessage, SetSpeedMessage, SetVolumeMessage},
};

fn main() {
    if let Err(e) = run() {
        println!("Failed due to error: {}", e)
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::new("Media Control")
        .about("Control media playback")
        .arg(
            Arg::with_name("connection_type")
                .short('c')
                .long("connection_type")
                .value_name("CONNECTION_TYPE")
                .help("Type of connection: tcp or ws (websocket)")
                .required(false)
                .default_value("tcp")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("host")
                .short('h')
                .long("host")
                .value_name("Host")
                .help("The host address to send the command to")
                .default_value("127.0.0.1")
                .required(false)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("port")
                .short('p')
                .long("port")
                .value_name("PORT")
                .help("The port to send the command to")
                .required(false)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("subscribe")
                .short('s')
                .long("subscribe")
                .value_name("EVENTS")
                .help("A comma separated list of events to subscribe to (e.g. MediaItemStart,KeyDown). \
                       Available events: [MediaItemStart, MediaItemEnd, MediaItemChange, KeyDown, KeyUp]")
                .required(false)
                .takes_value(true),
        )
        .subcommand(
            SubCommand::with_name("play")
                .about("Play media")
                .arg(
                    Arg::with_name("mime_type")
                        .short('m')
                        .long("mime_type")
                        .value_name("MIME_TYPE")
                        .help("Mime type (e.g., video/mp4)")
                        .required_unless_present("file")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("file")
                        .short('f')
                        .long("file")
                        .value_name("File")
                        .help("File content to play")
                        .required(false)
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("url")
                        .short('u')
                        .long("url")
                        .value_name("URL")
                        .help("URL to the content")
                        .required(false)
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("content")
                        .short('c')
                        .long("content")
                        .value_name("CONTENT")
                        .help("The actual content")
                        .required(false)
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("timestamp")
                        .short('t')
                        .long("timestamp")
                        .value_name("TIMESTAMP")
                        .help("Timestamp to start playing")
                        .required(false)
                        .default_value("0")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("speed")
                        .short('s')
                        .long("speed")
                        .value_name("SPEED")
                        .help("Factor to multiply playback speed by")
                        .required(false)
                        .default_value("1")
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("header")
                        .short('H')
                        .long("header")
                        .value_name("HEADER")
                        .help("Custom request headers in key:value format")
                        .required(false)
                        .multiple_occurrences(true),
                ),
        )
        .subcommand(
            SubCommand::with_name("seek")
                .about("Seek to a timestamp")
                .arg(
                    Arg::with_name("timestamp")
                        .short('t')
                        .long("timestamp")
                        .value_name("TIMESTAMP")
                        .help("Timestamp to start playing")
                        .required(true)
                        .takes_value(true),
                ),
        )
        .subcommand(SubCommand::with_name("pause").about("Pause media"))
        .subcommand(SubCommand::with_name("resume").about("Resume media"))
        .subcommand(SubCommand::with_name("stop").about("Stop media"))
        .subcommand(SubCommand::with_name("listen").about("Listen to incoming events"))
        .subcommand(
            SubCommand::with_name("setvolume")
                .about("Set the volume")
                .arg(
                    Arg::with_name("volume")
                        .short('v')
                        .long("volume")
                        .value_name("VOLUME")
                        .help("Volume level (0-1)")
                        .required(true)
                        .takes_value(true),
                ),
        )
        .subcommand(
            SubCommand::with_name("setspeed")
                .about("Set the playback speed")
                .arg(
                    Arg::with_name("speed")
                        .short('s')
                        .long("speed")
                        .value_name("SPEED")
                        .help("Factor to multiply playback speed by")
                        .required(true)
                        .takes_value(true),
                ),
        );

    let matches = app.get_matches();

    let host = matches.value_of("host").expect("host has default value");

    let connection_type = matches.value_of("connection_type").unwrap_or("tcp");

    let port = match matches.value_of("port") {
        Some(s) => s,
        _ => match connection_type {
            "tcp" => "46899",
            "ws" => "46898",
            _ => {
                return Err("Unknown connection type, cannot automatically determine port.".into())
            }
        },
    };

    let local_ip: Option<IpAddr>;
    let mut session = match connection_type {
        "tcp" => {
            println!("Connecting via TCP to host={} port={}...", host, port);
            let stream = TcpStream::connect(format!("{}:{}", host, port))?;
            local_ip = Some(stream.local_addr()?.ip());
            FCastSession::connect(stream)?
        }
        "ws" => {
            println!("Connecting via WebSocket to host={} port={}...", host, port);
            let url = Url::parse(format!("ws://{}:{}", host, port).as_str())?;
            let (stream, _) = tungstenite::connect(url)?;
            local_ip = match stream.get_ref() {
                MaybeTlsStream::Plain(ref stream) => Some(stream.local_addr()?.ip()),
                _ => return Err("Established connection type is not plain.".into()),
            };
            let stream = WebSocket::new(stream);
            FCastSession::connect(stream)?
        }
        _ => return Err("Invalid connection type.".into()),
    };

    println!("Connection established.");

    if let Some(subscriptions) = matches.value_of("subscribe") {
        let subs = subscriptions.split(',');
        for sub in subs {
            let event = match sub.to_lowercase().as_str() {
                "mediaitemstart" => v3::EventType::MediaItemStart,
                "mediaitemend" => v3::EventType::MediaItemEnd,
                "mediaitemchange" => v3::EventType::MediaItemChange,
                "keydown" => v3::EventType::KeyDown,
                "keyup" => v3::EventType::KeyUp,
                _ => {
                    println!("Invalid event in subscriptions list: {sub}");
                    continue;
                }
            };
            session.subscribe(event)?;
            println!("Subscribed to {event:?} events");
        }
    }

    let mut join_handle: Option<JoinHandle<Result<(), String>>> = None;
    if let Some(play_matches) = matches.subcommand_matches("play") {
        let file_path = play_matches.value_of("file");

        let mime_type = match play_matches.value_of("mime_type") {
            Some(s) => s.to_string(),
            _ => {
                if file_path.is_none() {
                    return Err("MIME type is required.".into());
                }
                match file_path.unwrap().split('.').next_back() {
                    Some("mkv") => "video/x-matroska".to_string(),
                    Some("mov") => "video/quicktime".to_string(),
                    Some("mp4") | Some("m4v") => "video/mp4".to_string(),
                    Some("mpg") | Some("mpeg") => "video/mpeg".to_string(),
                    Some("webm") => "video/webm".to_string(),
                    _ => return Err("MIME type is required.".into()),
                }
            }
        };

        let time = match play_matches.value_of("timestamp") {
            Some(s) => s.parse::<f64>().ok(),
            _ => None,
        };

        let speed = match play_matches.value_of("speed") {
            Some(s) => s.parse::<f64>().ok(),
            _ => None,
        };

        let headers = play_matches.values_of("header").map(|values| {
            values
                .filter_map(|s| {
                    let mut parts = s.splitn(2, ':');
                    if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                        Some((key.trim().to_string(), value.trim().to_string()))
                    } else {
                        None
                    }
                })
                .collect::<HashMap<String, String>>()
        });

        #[allow(unused_assignments)]
        let mut url = None;
        let mut content = None;

        if let Some(file_path) = file_path {
            match local_ip {
                Some(lip) => {
                    let running = Arc::new(AtomicBool::new(true));
                    let r = running.clone();

                    ctrlc::set_handler(move || {
                        println!(
                            "Ctrl+C triggered, server will stop when onging request finishes..."
                        );
                        r.store(false, Ordering::SeqCst);
                    })
                    .expect("Error setting Ctrl-C handler");

                    println!("Waiting for Ctrl+C...");

                    let result = host_file_and_get_url(&lip, file_path, &mime_type, &running)?;
                    url = Some(result.0);
                    join_handle = Some(result.1);
                }
                _ => return Err("Local IP was not able to be resolved.".into()),
            }
        } else {
            url = play_matches.value_of("url").map(|s| s.to_owned());
            content = play_matches.value_of("content").map(|s| s.to_owned());
        }

        if content.is_none() && url.is_none() {
            println!("Reading content from stdin...");

            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            content = Some(buffer);
        }

        session.send_play_message(mime_type, url, content, time, speed, headers)?;
    } else if let Some(seek_matches) = matches.subcommand_matches("seek") {
        let seek_message = SeekMessage::new(match seek_matches.value_of("timestamp") {
            Some(s) => s.parse::<f64>()?,
            _ => return Err("Timestamp is required.".into()),
        });
        println!("Sent seek {:?}", seek_message);
        session.send_message(Opcode::Seek, &seek_message)?;
    } else if matches.subcommand_matches("pause").is_some() {
        println!("Sent pause");
        session.send_empty(Opcode::Pause)?;
    } else if matches.subcommand_matches("resume").is_some() {
        println!("Sent resume");
        session.send_empty(Opcode::Resume)?;
    } else if matches.subcommand_matches("stop").is_some() {
        println!("Sent stop");
        session.send_empty(Opcode::Stop)?;
    } else if matches.subcommand_matches("listen").is_some() {
        println!("Starter listening to events...");

        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();

        ctrlc::set_handler(move || {
            println!("Ctrl+C triggered...");
            r.store(false, Ordering::SeqCst);
        })
        .expect("Error setting Ctrl-C handler");

        println!("Waiting for Ctrl+C...");

        session.receive_loop(&running)?;

        println!("Ctrl+C received, exiting...");
    } else if let Some(setvolume_matches) = matches.subcommand_matches("setvolume") {
        let setvolume_message = SetVolumeMessage::new(match setvolume_matches.value_of("volume") {
            Some(s) => s.parse::<f64>()?,
            _ => return Err("Timestamp is required.".into()),
        });
        println!("Sent setvolume {:?}", setvolume_message);
        session.send_message(Opcode::SetVolume, &setvolume_message)?;
    } else if let Some(setspeed_matches) = matches.subcommand_matches("setspeed") {
        let setspeed_message = SetSpeedMessage::new(match setspeed_matches.value_of("speed") {
            Some(s) => s.parse::<f64>()?,
            _ => return Err("Speed is required.".into()),
        });
        println!("Sent setspeed {:?}", setspeed_message);
        session.send_message(Opcode::SetSpeed, &setspeed_message)?;
    } else {
        println!("Invalid command. Use --help for more information.");
        std::process::exit(1);
    }

    println!("Waiting on other threads...");
    if let Some(v) = join_handle {
        if v.join().is_err() {
            return Err("Failed to join thread.".into());
        }
    }

    session.shutdown()?;

    Ok(())
}

struct ServerState {
    active_connections: usize,
    last_request_time: Instant,
}

impl ServerState {
    fn new() -> Self {
        ServerState {
            active_connections: 0,
            last_request_time: Instant::now(),
        }
    }
}

fn host_file_and_get_url(
    local_ip: &IpAddr,
    file_path: &str,
    mime_type: &String,
    running: &Arc<AtomicBool>,
) -> Result<(String, thread::JoinHandle<Result<(), String>>), String> {
    let local_ip_str = if local_ip.is_ipv6() {
        format!("[{}]", local_ip)
    } else {
        format!("{}", local_ip)
    };
    let server = Server::http(format!("{local_ip_str}:0"))
        .map_err(|err| format!("Failed to create server: {err}"))?;

    let url = match server.server_addr() {
        ListenAddr::IP(addr) => format!("http://{local_ip_str}:{}/", addr.port()),
        #[cfg(unix)]
        ListenAddr::Unix(_) => return Err("Unix socket addresses are not supported.".to_string()),
    };

    println!("Server started on {}.", url);

    let state = Mutex::new(ServerState::new());
    let file_path_clone = file_path.to_owned();
    let mime_type_clone = mime_type.to_owned();
    let running_clone = running.to_owned();

    let handle = thread::spawn(move || -> Result<(), String> {
        loop {
            if !running_clone.load(Ordering::SeqCst) {
                println!("Server stopping...");
                break;
            }

            let should_break = {
                let state = state.lock().unwrap();
                state.active_connections == 0
                    && state.last_request_time.elapsed() > Duration::from_secs(300)
            };

            if should_break {
                println!("No activity on server, closing...");
                break;
            }

            match server.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(request)) => {
                    println!("Request received.");

                    let mut state = state.lock().unwrap();
                    state.active_connections += 1;
                    state.last_request_time = Instant::now();

                    let file = fs::File::open(&file_path_clone)
                        .map_err(|_| "Failed to open file.".to_owned())?;

                    let content_type_header =
                        Header::from_str(format!("Content-Type: {}", mime_type_clone).as_str())
                            .map_err(|_| "Failed to open file.".to_owned())?;

                    let response = Response::from_file(file).with_header(content_type_header);

                    if let Err(e) = request.respond(response) {
                        println!("Failed to respond to request: {}", e);
                    }
                    state.active_connections -= 1;
                }
                Ok(None) => {}
                Err(e) => {
                    println!("Error receiving request: {}", e);
                    break;
                }
            }
        }
        Ok(())
    });

    Ok((url, handle))
}
