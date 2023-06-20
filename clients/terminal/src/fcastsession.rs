use std::{net::TcpStream, io::{Write, Read}, sync::{atomic::{AtomicBool, Ordering}, Arc}};

use crate::models::{PlaybackUpdateMessage, VolumeUpdateMessage};
use serde::Serialize;

#[derive(Debug)]
enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected,
}

#[derive(Debug)]
pub enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8
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
    stream: &'a TcpStream,
    state: SessionState
}

impl<'a> FCastSession<'a> {
    pub fn new(stream: &'a TcpStream) -> Self {
        FCastSession {
            buffer: vec![0; MAXIMUM_PACKET_LENGTH],
            bytes_read: 0,
            packet_length: 0,
            stream,
            state: SessionState::Idle
        }
    }
}

impl FCastSession<'_> {
    pub fn send_message<T: Serialize>(&mut self, opcode: Opcode, message: T) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&message)?;
        let data = json.as_bytes();
        let size = 1 + data.len();
        let header_size = LENGTH_BYTES + 1;
        let mut header = vec![0u8; header_size];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;
        
        let packet = [header, data.to_vec()].concat();
        println!("Sent {} bytes with (header size: {}, body size: {}).", packet.len(), header_size, data.len());
        return self.stream.write_all(&packet);
    }

    pub fn send_empty(&mut self, opcode: Opcode) -> Result<(), std::io::Error> {
        let json = String::new();
        let data = json.as_bytes();
        let size = 1 + data.len();
        let mut header = vec![0u8; LENGTH_BYTES + 1];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;
        
        let packet = [header, data.to_vec()].concat();
        return self.stream.write_all(&packet);
    }

    pub fn receive_loop(&mut self, running: &Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
        self.state = SessionState::WaitingForLength;

        let mut buffer = [0u8; 1024];
        while running.load(Ordering::SeqCst) {
            let bytes_read = self.stream.read(&mut buffer)?;
            self.process_bytes(&buffer[..bytes_read])?;
        }

        self.state = SessionState::Idle;
        Ok(())
    }

    pub fn process_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if received_bytes.is_empty() {
            return Ok(());
        }

        let addr = match self.stream.peer_addr() {
            Ok(a) => a.to_string(),
            _ => String::new()
        };

        println!("{} bytes received from {}", received_bytes.len(), addr);

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
            let addr = match self.stream.peer_addr() {
                Ok(a) => a.to_string(),
                _ => String::new()
            };

            self.state = SessionState::WaitingForData;
            self.packet_length = u32::from_le_bytes(self.buffer[..LENGTH_BYTES].try_into()?) as usize;
            self.bytes_read = 0;

            println!("Packet length header received from {}: {}", addr, self.packet_length);

            if self.packet_length > MAXIMUM_PACKET_LENGTH {
                println!("Maximum packet length is 32kB, killing stream {}: {}", addr, self.packet_length);

                self.stream.shutdown(std::net::Shutdown::Both)?;
                self.state = SessionState::Disconnected;
                return Err(format!("Stream killed due to packet length ({}) exceeding maximum 32kB packet size.", self.packet_length).into());
            }
    
            if bytes_remaining > 0 {
                println!("{} remaining bytes {} pushed to handlePacketBytes", bytes_remaining, addr);

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
            let addr = match self.stream.peer_addr() {
                Ok(a) => a.to_string(),
                _ => String::new()
            };
            
            println!("Packet finished receiving from {} of {} bytes.", addr, self.packet_length);
            self.handle_packet()?;

            self.state = SessionState::WaitingForLength;
            self.packet_length = 0;
            self.bytes_read = 0;
    
            if bytes_remaining > 0 {
                println!("{} remaining bytes {} pushed to handleLengthBytes", bytes_remaining, addr);
                self.handle_length_bytes(&received_bytes[bytes_to_read..])?;
            }
        }

        Ok(())
    }

    fn handle_packet(&mut self) -> Result<(), std::str::Utf8Error> {
        let addr = match self.stream.peer_addr() {
            Ok(a) => a.to_string(),
            _ => String::new()
        };

        println!("Processing packet of {} bytes from {}", self.bytes_read, addr);

        let opcode = Opcode::from_u8(self.buffer[0]);
        let body = if self.packet_length > 1 {
            Some(std::str::from_utf8(&self.buffer[1..self.packet_length])?)
        } else {
            None
        };

        println!("Received body: {:?}", body);

        match opcode {
            Opcode::PlaybackUpdate => {
                if let Some(body_str) = body {
                    if let Ok(playback_update_msg) = serde_json::from_str::<PlaybackUpdateMessage>(body_str) {
                        println!("Received playback update {:?}", playback_update_msg);
                    }
                }
            }
            Opcode::VolumeUpdate => {
                if let Some(body_str) = body {
                    if let Ok(volume_update_msg) = serde_json::from_str::<VolumeUpdateMessage>(body_str) {
                        println!("Received volume update {:?}", volume_update_msg);
                    }
                }
            }
            _ => {
                println!("Error handling packet from {}", addr);
            }
        }

        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), std::io::Error> {
        return self.stream.shutdown(std::net::Shutdown::Both);
    }
}