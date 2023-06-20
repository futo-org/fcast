mod models;
mod fcastsession;

use clap::{App, Arg, SubCommand};
use std::{io::Read, net::TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::fcastsession::Opcode;
use crate::models::SetVolumeMessage;
use crate::{models::{PlayMessage, SeekMessage}, fcastsession::FCastSession};

fn main() {
    if let Err(e) = run() {
        println!("Failed due to error: {}", e)
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::new("Media Control")
        .about("Control media playback")
        .arg(Arg::with_name("host")
            .short('h')
            .long("host")
            .value_name("Host")
            .help("The host address to send the command to")
            .required(true)
            .takes_value(true))
        .arg(Arg::with_name("port")
            .short('p')
            .long("port")
            .value_name("PORT")
            .help("The port to send the command to")
            .required(false)
            .default_value("46899")
            .takes_value(true))
        .subcommand(SubCommand::with_name("play")
            .about("Play media")
            .arg(Arg::with_name("mime_type")
                .short('m')
                .long("mime_type")
                .value_name("MIME_TYPE")
                .help("Mime type (e.g., video/mp4)")
                .required(true)
                .takes_value(true)
            )
            .arg(Arg::with_name("url")
                .short('u')
                .long("url")
                .value_name("URL")
                .help("URL to the content")
                .required(false)
                .takes_value(true)
            )
            .arg(Arg::with_name("content")
                .short('c')
                .long("content")
                .value_name("CONTENT")
                .help("The actual content")
                .required(false)
                .takes_value(true)
            )
            .arg(Arg::with_name("timestamp")
                .short('t')
                .long("timestamp")
                .value_name("TIMESTAMP")
                .help("Timestamp to start playing")
                .required(false)
                .default_value("0")
                .takes_value(true)
            )
        )
        .subcommand(SubCommand::with_name("seek")
            .about("Seek to a timestamp")
            .arg(Arg::with_name("timestamp")
                .short('t')
                .long("timestamp")
                .value_name("TIMESTAMP")
                .help("Timestamp to start playing")
                .required(true)
                .takes_value(true)
            ),
        )
        .subcommand(SubCommand::with_name("pause").about("Pause media"))
        .subcommand(SubCommand::with_name("resume").about("Resume media"))
        .subcommand(SubCommand::with_name("stop").about("Stop media"))
        .subcommand(SubCommand::with_name("listen").about("Listen to incoming events"))
        .subcommand(SubCommand::with_name("setvolume").about("Set the volume")
            .arg(Arg::with_name("volume")
            .short('v')
            .long("volume")
            .value_name("VOLUME")
            .help("Volume level (0-1)")
            .required(true)
            .takes_value(true))
        );

    let matches = app.get_matches();

    let host = match matches.value_of("host") {
        Some(s) => s,
        _ => return Err("Host is required.".into())
    };
    
    let port = match matches.value_of("port") {
        Some(s) => s,
        _ => return Err("Port is required.".into())
    };

    println!("Connecting to host={} port={}...", host, port);
    let stream = TcpStream::connect(format!("{}:{}", host, port))?;
    let mut session = FCastSession::new(&stream);
    println!("Connection established.");

    if let Some(play_matches) = matches.subcommand_matches("play") {
        let mut play_message = PlayMessage::new(
            match play_matches.value_of("mime_type") {
                Some(s) => s.to_string(),
                _ => return Err("MIME type is required.".into())
            },
            match play_matches.value_of("url") {
                Some(s) => Some(s.to_string()),
                _ => None
            },
            match play_matches.value_of("content") {
                Some(s) => Some(s.to_string()),
                _ => None
            },
            match play_matches.value_of("timestamp") {
                Some(s) => s.parse::<u64>().ok(),
                _ => None
            }
        );

        if play_message.content.is_none() && play_message.url.is_none() {
            println!("Reading content from stdin...");

            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            play_message.content = Some(buffer);
        }

        let json = serde_json::to_string(&play_message);
        println!("Sent play {:?}", json);

        session.send_message(Opcode::Play, Some(play_message))?;
    } else if let Some(seek_matches) = matches.subcommand_matches("seek") {
        let seek_message = SeekMessage::new(match seek_matches.value_of("timestamp") {
            Some(s) => s.parse::<u64>()?,
            _ => return Err("Timestamp is required.".into())
        });
        println!("Sent seek {:?}", seek_message);
        session.send_message(Opcode::Seek, Some(seek_message))?;
    } else if let Some(_) = matches.subcommand_matches("pause") {
        println!("Sent pause");
        session.send_empty(Opcode::Pause)?;
    } else if let Some(_) = matches.subcommand_matches("resume") {
        println!("Sent resume");
        session.send_empty(Opcode::Resume)?;
    } else if let Some(_) = matches.subcommand_matches("stop") {
        println!("Sent stop");
        session.send_empty(Opcode::Stop)?;
    } else if let Some(_) = matches.subcommand_matches("listen") {
        println!("Starter listening to events...");

        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();

        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        }).expect("Error setting Ctrl-C handler");

        println!("Waiting for Ctrl+C...");

        session.receive_loop(&running)?;

        println!("Ctrl+C received, exiting...");
    } else if let Some(setvolume_matches) = matches.subcommand_matches("setvolume") {
        let setvolume_message = SetVolumeMessage::new(match setvolume_matches.value_of("volume") {
            Some(s) => s.parse::<f64>()?,
            _ => return Err("Timestamp is required.".into())
        });
        println!("Sent setvolume {:?}", setvolume_message);
        session.send_message(Opcode::SetVolume, Some(setvolume_message))?;
    } else {
        println!("Invalid command. Use --help for more information.");
        std::process::exit(1);
    }

    session.shutdown()?;

    Ok(())
}