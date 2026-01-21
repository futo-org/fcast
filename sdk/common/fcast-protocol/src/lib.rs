//! # FCast Protocol
//!
//! Implementation of the data models documented [here](https://gitlab.futo.org/videostreaming/fcast/-/wikis/Protocol-version-3).

// TODO: most strings should be SmolStr

#[cfg(feature = "__schema")]
use get_type_string_derive::GetTypeString;
#[cfg(feature = "__schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

pub mod v1;
pub mod v2;
pub mod v3;
pub mod v4;

pub const HEADER_LENGTH: usize = 5;

#[derive(Debug)]
pub enum TryFromByteError {
    UnknownOpcode(u8),
}

impl std::error::Error for TryFromByteError {}

impl std::fmt::Display for TryFromByteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryFromByteError::UnknownOpcode(opcode) => write!(f, "Unknown opcode: {opcode}"),
        }
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Opcode {
    /// Not used
    None = 0,
    /// Sender message to play media content, body is [`v3::PlayMessage`]
    Play = 1,
    /// Sender message to pause media content, no body
    Pause = 2,
    /// Sender message to resume media content, no body
    Resume = 3,
    /// Sender message to stop media content, no body
    Stop = 4,
    /// Sender message to seek, body is [`SeekMessage`]
    Seek = 5,
    /// Receiver message to notify an updated playback state, body is [`v3::PlaybackUpdateMessage`]
    PlaybackUpdate = 6,
    /// Receiver message to notify when the volume has changed, body is [`VolumeUpdateMessage`]
    VolumeUpdate = 7,
    /// Sender message to change volume, body is [`SetVolumeMessage`]
    SetVolume = 8,
    /// Server message to notify the sender a playback error happened, body is [`PlaybackErrorMessage`]
    PlaybackError = 9,
    /// Sender message to change playback speed, body is [`SetSpeedMessage`]
    SetSpeed = 10,
    /// Message to notify the other of the current version, body is [`VersionMessage`]
    Version = 11,
    /// Message to get the other party to pong, no body
    Ping = 12,
    /// Message to respond to a ping from the other party, no body
    Pong = 13,
    /// Message to notify the other party of device information and state, body is InitialSenderMessage if receiver or
    /// [`v3::InitialReceiverMessage`] if sender
    Initial = 14,
    /// Receiver message to notify all senders when any device has sent a [`v3::PlayMessage`], body is [`v3::PlayUpdateMessage`]
    PlayUpdate = 15,
    /// Sender message to set the item index in a playlist to play content from, body is [`v3::SetPlaylistItemMessage`]
    SetPlaylistItem = 16,
    /// Sender message to subscribe to a receiver event, body is [`v3::SubscribeEventMessage`]
    SubscribeEvent = 17,
    /// Sender message to unsubscribe to a receiver event, body is [`v3::UnsubscribeEventMessage`]
    UnsubscribeEvent = 18,
    /// Receiver message to notify when a sender subscribed event has occurred, body is [`v3::EventMessage`]
    Event = 19,
}

impl TryFrom<u8> for Opcode {
    type Error = TryFromByteError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
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
            _ => return Err(TryFromByteError::UnknownOpcode(value)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum PlaybackState {
    Idle = 0,
    Playing = 1,
    Paused = 2,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlaybackErrorMessage {
    pub message: String,
}

#[cfg_attr(feature = "__schema", derive(JsonSchema, GetTypeString))]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VersionMessage {
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SetSpeedMessage {
    pub speed: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SetVolumeMessage {
    pub volume: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SeekMessage {
    pub time: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FCastService {
    pub port: u16,
    pub r#type: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FCastNetworkConfig {
    pub name: String,
    pub addresses: Vec<String>,
    pub services: Vec<FCastService>,
}

#[derive(Debug, PartialEq, Eq)]
enum ReaderState {
    MissingLength,
    MissingBody { length: usize },
    ShouldClear { body_length: usize },
}

pub struct PacketReader {
    buffer: Vec<u8>,
    state: ReaderState,
}

impl PacketReader {
    // TODO: honor max packet size? (shring_to_fit() when?)
    pub fn new(max_packet_size: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(max_packet_size),
            state: ReaderState::MissingLength,
        }
    }

    fn next_state(&mut self) -> Option<&[u8]> {
        const LEN_SIZE: usize = std::mem::size_of::<u32>();

        match self.state {
            ReaderState::MissingLength => {
                if self.buffer.len() >= LEN_SIZE {
                    let length = u32::from_le_bytes([
                        self.buffer[0],
                        self.buffer[1],
                        self.buffer[2],
                        self.buffer[3],
                    ]) as usize;
                    self.state = ReaderState::MissingBody { length };
                    self.next_state()
                } else {
                    None
                }
            }
            ReaderState::MissingBody { length } => {
                if self.buffer.len().saturating_sub(LEN_SIZE) >= length {
                    self.state = ReaderState::ShouldClear {
                        body_length: length,
                    };
                    Some(&self.buffer[LEN_SIZE..LEN_SIZE + length])
                } else {
                    None
                }
            }
            ReaderState::ShouldClear { body_length } => {
                let full_length = self.buffer.len().min(LEN_SIZE + body_length);
                self.buffer.drain(0..full_length);
                self.state = ReaderState::MissingLength;
                self.next_state()
            }
        }
    }

    /// Push data to the reader's internal buffer.
    ///
    /// `get_packet()` should be called to extract packets.
    pub fn push_data(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Get a packet if it's available.
    ///
    /// This should be called in a loop until `None` is returned which means more data is needed.
    pub fn get_packet(&mut self) -> Option<&[u8]> {
        self.next_state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_reader_single() {
        let mut reader = PacketReader::new(100);
        reader.push_data(&[1u32.to_le_bytes().as_slice(), [0u8].as_slice()].concat());
        assert_eq!(reader.get_packet().unwrap(), &[0]);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.get_packet(), None);
        assert_eq!(reader.buffer, Vec::<u8>::new());
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert!(reader.buffer.is_empty());
    }

    #[test]
    fn packet_reader_small_push() {
        let mut reader = PacketReader::new(100);
        let length = 1u32.to_le_bytes();
        reader.push_data(&[length[0], length[1]]);
        assert_eq!(reader.get_packet(), None);
        reader.push_data(&[length[2]]);
        assert_eq!(reader.get_packet(), None);
        assert_eq!(reader.state, ReaderState::MissingLength);
        reader.push_data(&[length[3]]);
        assert_eq!(reader.get_packet(), None);
        reader.push_data(&[0]);
        assert_eq!(reader.get_packet().unwrap(), &[0]);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.get_packet(), None);
        assert!(reader.buffer.is_empty());
    }

    #[rustfmt::skip]
    #[test]
    fn packet_reader_many_packets_single_push() {
        let mut reader = PacketReader::new(100);
        reader.push_data(&[
            1u32.to_le_bytes().as_slice(), [0u8].as_slice(),
            2u32.to_le_bytes().as_slice(), [0u8, 1].as_slice(),
            3u32.to_le_bytes().as_slice(), [0u8, 1, 2].as_slice(),
        ].concat());
        assert_eq!(reader.get_packet().unwrap(), &[0]);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.get_packet().unwrap(), &[0, 1]);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 2 });
        assert_eq!(reader.get_packet().unwrap(), &[0, 1, 2]);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 3 });
        assert_eq!(reader.get_packet(), None);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert!(reader.buffer.is_empty());
    }

    #[test]
    fn packet_reader_partial_body() {
        let mut reader = PacketReader::new(100);
        reader.push_data(&[4u32.to_le_bytes().as_slice(), [0u8, 1].as_slice()].concat());
        assert_eq!(reader.get_packet(), None);
        reader.push_data(&[2]);
        assert_eq!(reader.get_packet(), None);
        reader.push_data(&[3]);
        assert_eq!(reader.get_packet().unwrap(), &[0, 1, 2, 3]);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 4 });
        assert_eq!(reader.get_packet(), None);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert!(reader.buffer.is_empty());
    }

    #[test]
    fn packet_reader_large_body() {
        let mut reader = PacketReader::new(100);
        let body = (0..10).collect::<Vec<u8>>();
        reader.push_data(&[10u32.to_le_bytes().as_slice(), body.as_slice()].concat());
        assert_eq!(reader.get_packet().unwrap(), &body);
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 10 });
        assert_eq!(reader.get_packet(), None);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert!(reader.buffer.is_empty());
    }
}
