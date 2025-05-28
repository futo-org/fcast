use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

use crate::{models::{PlaybackUpdateMessage, VolumeUpdateMessage, PlaybackErrorMessage, VersionMessage}, transport::Transport};
use serde::Serialize;

#[derive(Debug)]
enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
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
    Pong = 13
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
            _ => panic!("Unknown value: {}", value),
        }
    }
}

const LENGTH_BYTES: usize = 4;
const MAXIMUM_PACKET_LENGTH: usize = 32000;

pub struct FCastSession<'a> {
    buffer: Vec<u8>,
    bytes_read: usize,
    packet_length: usize,
    stream: Box<dyn Transport + 'a>,
    state: SessionState
}

impl<'a> FCastSession<'a> {
    pub fn new<T: Transport + 'a>(stream: T) -> Self {
        return FCastSession {
            buffer: vec![0; MAXIMUM_PACKET_LENGTH],
            bytes_read: 0,
            packet_length: 0,
            stream: Box::new(stream),
            state: SessionState::Idle
        }
    }
}

impl FCastSession<'_> {
    pub fn send_message<T: Serialize>(&mut self, opcode: Opcode, message: T) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&message)?;
        let data = json.as_bytes();
        let size = 1 + data.len();
        let header_size = LENGTH_BYTES + 1;
        let mut header = vec![0u8; header_size];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;
        
        let packet = [header, data.to_vec()].concat();
        println!("Sent {} bytes with (opcode: {:?}, header size: {}, body size: {}, body: {}).", packet.len(), opcode, header_size, data.len(), json);
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

    pub fn receive_loop(&mut self, running: &Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
        println!("Start receiving.");

        self.state = SessionState::WaitingForLength;

        let mut buffer = [0u8; 1024];
        while running.load(Ordering::SeqCst) {
            let bytes_read = self.stream.transport_read(&mut buffer)?;
            self.process_bytes(&buffer[..bytes_read])?;
        }

        self.state = SessionState::Idle;
        Ok(())
    }

    fn process_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if received_bytes.is_empty() {
            return Ok(());
        }

        println!("{} bytes received", received_bytes.len());

        match self.state {
            SessionState::WaitingForLength => self.handle_length_bytes(received_bytes)?,
            SessionState::WaitingForData => self.handle_packet_bytes(received_bytes)?,
            _ => println!("Data received is unhandled in current session state {:?}", self.state),
        }

        Ok(())
    }


    fn handle_length_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let bytes_to_read = std::cmp::min(LENGTH_BYTES, received_bytes.len());
        let bytes_remaining = received_bytes.len() - bytes_to_read;
        self.buffer[self.bytes_read..self.bytes_read + bytes_to_read]
            .copy_from_slice(&received_bytes[..bytes_to_read]);
        self.bytes_read += bytes_to_read;

        println!("handleLengthBytes: Read {} bytes from packet", bytes_to_read);

        if self.bytes_read >= LENGTH_BYTES {
            self.state = SessionState::WaitingForData;
            self.packet_length = u32::from_le_bytes(self.buffer[..LENGTH_BYTES].try_into()?) as usize;
            self.bytes_read = 0;

            println!("Packet length header received from: {}", self.packet_length);

            if self.packet_length > MAXIMUM_PACKET_LENGTH {
                println!("Maximum packet length is 32kB, killing stream: {}", self.packet_length);

                self.stream.transport_shutdown()?;
                self.state = SessionState::Disconnected;
                return Err(format!("Stream killed due to packet length ({}) exceeding maximum 32kB packet size.", self.packet_length).into());
            }
    
            if bytes_remaining > 0 {
                println!("{} remaining bytes pushed to handlePacketBytes", bytes_remaining);

                self.handle_packet_bytes(&received_bytes[bytes_to_read..])?;
            }
        }

        Ok(())
    }

    fn handle_packet_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let bytes_to_read = std::cmp::min(self.packet_length, received_bytes.len());
        let bytes_remaining = received_bytes.len() - bytes_to_read;
        self.buffer[self.bytes_read..self.bytes_read + bytes_to_read]
            .copy_from_slice(&received_bytes[..bytes_to_read]);
        self.bytes_read += bytes_to_read;
    
        println!("handlePacketBytes: Read {} bytes from packet", bytes_to_read);
    
        if self.bytes_read >= self.packet_length {           
            println!("Packet finished receiving of {} bytes.", self.packet_length);
            self.handle_next_packet()?;

            self.state = SessionState::WaitingForLength;
            self.packet_length = 0;
            self.bytes_read = 0;
    
            if bytes_remaining > 0 {
                println!("{} remaining bytes pushed to handleLengthBytes", bytes_remaining);
                self.handle_length_bytes(&received_bytes[bytes_to_read..])?;
            }
        }

        Ok(())
    }

    fn handle_next_packet(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        println!("Processing packet of {} bytes", self.bytes_read);
    
        let opcode = Opcode::from_u8(self.buffer[0]);
        let packet_length = self.packet_length;
        let body = if packet_length > 1 {
            Some(std::str::from_utf8(&self.buffer[1..packet_length])?.to_string())
        } else {
            None
        };
    
        println!("Received body: {:?}", body);
        self.handle_packet(opcode, body)
    }

    fn handle_packet(&mut self, opcode: Opcode, body: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
        println!("Received message with opcode {:?}.", opcode);

        match opcode {
            Opcode::PlaybackUpdate => {
                if let Some(body_str) = body {
                    if let Ok(playback_update_msg) = serde_json::from_str::<PlaybackUpdateMessage>(body_str.as_str()) {
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
                    if let Ok(volume_update_msg) = serde_json::from_str::<VolumeUpdateMessage>(body_str.as_str()) {
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
                    if let Ok(playback_error_msg) = serde_json::from_str::<PlaybackErrorMessage>(body_str.as_str()) {
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
                    if let Ok(version_msg) = serde_json::from_str::<VersionMessage>(body_str.as_str()) {
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
        return self.stream.transport_shutdown();
    }
}