use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::{
    models::v2::PlaybackUpdateMessage,
    models::{PlaybackErrorMessage, VersionMessage, VolumeUpdateMessage},
    transport::Transport,
};
use serde::Serialize;

#[derive(Debug)]
enum SessionState {
    Idle = 0,
    Disconnected,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    Ping = 12,
    Pong = 13,

    Initial = 14,
    PlayUpdate = 15,
    SetPlaylistItem = 16,
    SubscribeEvent = 17,
    UnsubscribeEvent = 18,
    Event = 19,
}

impl Opcode {
    fn from_u8(value: u8) -> Opcode {
        match value {
            0 => Opcode::None,
            1 => Opcode::Play,
            2 => Opcode::Pause,
            3 => Opcode::Resume,
            4 => Opcode::Stop,
            5 => Opcode::Seek,
            6 => Opcode::PlaybackUpdate,
            7 => Opcode::VolumeUpdate,
            8 => Opcode::SetVolume,
            9 => Opcode::PlaybackError,
            10 => Opcode::SetSpeed,
            11 => Opcode::Version,
            12 => Opcode::Ping,
            13 => Opcode::Pong,

            14 => Opcode::Initial,
            15 => Opcode::PlayUpdate,
            16 => Opcode::SetPlaylistItem,
            17 => Opcode::SubscribeEvent,
            18 => Opcode::UnsubscribeEvent,
            19 => Opcode::Event,
            _ => panic!("Unknown value: {}", value),
        }
    }
}

const LENGTH_BYTES: usize = 4;
const MAXIMUM_PACKET_LENGTH: usize = 32000;

pub struct FCastSession<'a> {
    buffer: Vec<u8>,
    stream: Box<dyn Transport + 'a>,
    state: SessionState,
}

impl<'a> FCastSession<'a> {
    pub fn new<T: Transport + 'a>(stream: T) -> Self {
        Self {
            buffer: vec![0; MAXIMUM_PACKET_LENGTH],
            stream: Box::new(stream),
            state: SessionState::Idle,
        }
    }

    pub fn connect<T: Transport + 'a>(stream: T) -> Result<Self, Box<dyn std::error::Error>> {
        let mut session = Self::new(stream);

        session.send_message(
            Opcode::Version,
            crate::models::VersionMessage { version: 3 },
        )?;

        let (opcode, body) = session.read_packet()?;

        if opcode != Opcode::Version {
            return Err(format!("Expected Opcode::Version, got {opcode:?}").into());
        }

        let msg: VersionMessage =
            serde_json::from_str(&body.ok_or("Version messages required body".to_owned())?)?;

        if msg.version == 3 {
            todo!("Send/recv initial message");
        }

        Ok(session)
    }

    fn read_packet(&mut self) -> Result<(Opcode, Option<String>), Box<dyn std::error::Error>> {
        let mut header_buf = [0u8; 5];
        self.stream.transport_read_exact(&mut header_buf)?;

        let opcode = Opcode::from_u8(header_buf[4]);
        let body_length =
            u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]])
                as usize
                - 1;

        if body_length > MAXIMUM_PACKET_LENGTH {
            println!(
                "Maximum packet length is 32kB, killing stream: {}",
                body_length,
            );

            self.stream.transport_shutdown()?;
            self.state = SessionState::Disconnected;
            return Err(format!(
                "Stream killed due to packet length ({}) exceeding maximum 32kB packet size.",
                body_length,
            )
            .into());
        }

        self.stream
            .transport_read_exact(&mut self.buffer[0..body_length])?;

        let body_json = if body_length > 0 {
            Some(String::from_utf8(self.buffer[0..body_length].to_vec())?)
        } else {
            None
        };

        Ok((opcode, body_json))
    }

    pub fn send_message<T: Serialize>(
        &mut self,
        opcode: Opcode,
        message: T,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&message)?;
        let data = json.as_bytes();
        let size = 1 + data.len();
        let header_size = LENGTH_BYTES + 1;
        let mut header = vec![0u8; header_size];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;

        let packet = [header, data.to_vec()].concat();
        println!(
            "Sent {} bytes with (opcode: {:?}, header size: {}, body size: {}, body: {}).",
            packet.len(),
            opcode,
            header_size,
            data.len(),
            json
        );
        self.stream.transport_write(&packet)?;
        Ok(())
    }

    pub fn send_empty(&mut self, opcode: Opcode) -> Result<(), Box<dyn std::error::Error>> {
        let json = String::new();
        let data = json.as_bytes();
        let size = 1 + data.len();
        let mut header = vec![0u8; LENGTH_BYTES + 1];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;

        let packet = [header, data.to_vec()].concat();
        self.stream.transport_write(&packet)?;
        Ok(())
    }

    pub fn receive_loop(
        &mut self,
        running: &Arc<AtomicBool>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!("Start receiving.");

        while running.load(Ordering::SeqCst) {
            let (opcode, body) = self.read_packet()?;
            self.handle_packet(opcode, body)?;
        }

        Ok(())
    }

    fn handle_packet(
        &mut self,
        opcode: Opcode,
        body: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!("Received message with opcode {:?}.", opcode);

        match opcode {
            Opcode::PlaybackUpdate => {
                if let Some(body_str) = body {
                    if let Ok(playback_update_msg) =
                        serde_json::from_str::<PlaybackUpdateMessage>(body_str.as_str())
                    {
                        println!("Received playback update {:?}", playback_update_msg);
                    } else {
                        println!("Received playback update with malformed body.");
                    }
                } else {
                    println!("Received playback update with no body.");
                }
            }
            Opcode::VolumeUpdate => {
                if let Some(body_str) = body {
                    if let Ok(volume_update_msg) =
                        serde_json::from_str::<VolumeUpdateMessage>(body_str.as_str())
                    {
                        println!("Received volume update {:?}", volume_update_msg);
                    } else {
                        println!("Received volume update with malformed body.");
                    }
                } else {
                    println!("Received volume update with no body.");
                }
            }
            Opcode::PlaybackError => {
                if let Some(body_str) = body {
                    if let Ok(playback_error_msg) =
                        serde_json::from_str::<PlaybackErrorMessage>(body_str.as_str())
                    {
                        println!("Received playback error {:?}", playback_error_msg);
                    } else {
                        println!("Received playback error with malformed body.");
                    }
                } else {
                    println!("Received playback error with no body.");
                }
            }
            Opcode::Version => {
                if let Some(body_str) = body {
                    if let Ok(version_msg) =
                        serde_json::from_str::<VersionMessage>(body_str.as_str())
                    {
                        println!("Received version {:?}", version_msg);
                    } else {
                        println!("Received version with malformed body.");
                    }
                } else {
                    println!("Received version with no body.");
                }
            }
            Opcode::Ping => {
                println!("Received ping");
                self.send_empty(Opcode::Pong)?;
                println!("Sent pong");
            }
            _ => {
                println!("Error handling packet");
            }
        }

        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), std::io::Error> {
        self.stream.transport_shutdown()
    }
}
