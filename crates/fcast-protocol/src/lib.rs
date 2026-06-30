//! # FCast Protocol
//!
//! Implementation of the data models documented [here](https://gitlab.futo.org/videostreaming/fcast/-/wikis/Protocol-version-3).

use std::collections::HashMap;

use base64::{
    alphabet::URL_SAFE,
    engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig},
    Engine as _,
};
#[cfg(feature = "__schema")]
use get_type_string_derive::GetTypeString;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

pub mod companion;
#[cfg(feature = "tokio-receiver")]
pub mod receiver;
#[cfg(feature = "tokio-sender")]
pub mod sender;
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

    // V3
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

    // V4
    Flatbuf = 20,
    Resource = 21,
}

impl TryFrom<u8> for Opcode {
    type Error = TryFromByteError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::None,
            1 => Self::Play,
            2 => Self::Pause,
            3 => Self::Resume,
            4 => Self::Stop,
            5 => Self::Seek,
            6 => Self::PlaybackUpdate,
            7 => Self::VolumeUpdate,
            8 => Self::SetVolume,
            9 => Self::PlaybackError,
            10 => Self::SetSpeed,
            11 => Self::Version,
            12 => Self::Ping,
            13 => Self::Pong,
            14 => Self::Initial,
            15 => Self::PlayUpdate,
            16 => Self::SetPlaylistItem,
            17 => Self::SubscribeEvent,
            18 => Self::UnsubscribeEvent,
            19 => Self::Event,
            20 => Self::Flatbuf,
            21 => Self::Resource,
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

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlaybackErrorMessage {
    pub message: String,
}

#[cfg_attr(feature = "__schema", derive(GetTypeString))]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FCastService {
    pub port: u16,
    pub r#type: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FCastNetworkConfig {
    pub name: String,
    pub addresses: Vec<String>,
    pub services: Vec<FCastService>,
    pub txt: Option<HashMap<String, String>>,
}

impl FCastNetworkConfig {
    pub fn parse_url(url: &str) -> Option<Self> {
        let connection_info = url.strip_prefix("fcast://r/")?;
        let b64_engine = GeneralPurpose::new(
            &URL_SAFE,
            GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        let json = b64_engine.decode(connection_info).ok()?;
        serde_json::from_slice::<Self>(&json).ok()
    }

    pub fn to_url(&self) -> serde_json::Result<String> {
        let net_config = serde_json::to_string(self)?;
        let url = format!(
            "fcast://r/{}",
            base64::engine::general_purpose::URL_SAFE
                .encode(net_config)
                .as_str(),
        );
        Ok(url)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReaderState {
    MissingLength,
    MissingBody { length: usize },
    ShouldClear { body_length: usize },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadResult<'a> {
    NeedData,
    Read(&'a [u8]),
    PacketTooLarge(usize),
}

#[derive(Debug)]
pub enum PushDataError {
    BufferTooBig,
}

pub struct PacketReader {
    buffer: Vec<u8>,
    state: ReaderState,
    len: usize,
    max_packet_size: usize,
}

impl PacketReader {
    pub fn new(max_packet_size: usize, padding: usize) -> Self {
        Self {
            buffer: vec![0; size_of::<u32>() + max_packet_size + padding],
            state: ReaderState::MissingLength,
            len: 0,
            max_packet_size,
        }
    }

    fn next_state(&mut self) -> ReadResult<'_> {
        const LEN_SIZE: usize = std::mem::size_of::<u32>();

        match self.state {
            ReaderState::MissingLength => {
                if self.len >= LEN_SIZE {
                    let length = u32::from_le_bytes([
                        self.buffer[0],
                        self.buffer[1],
                        self.buffer[2],
                        self.buffer[3],
                    ]) as usize;
                    if length > self.max_packet_size {
                        ReadResult::PacketTooLarge(length)
                    } else {
                        self.state = ReaderState::MissingBody { length };
                        self.next_state()
                    }
                } else {
                    ReadResult::NeedData
                }
            }
            ReaderState::MissingBody { length } => {
                if self.len.saturating_sub(LEN_SIZE) >= length {
                    self.state = ReaderState::ShouldClear {
                        body_length: length,
                    };
                    ReadResult::Read(&self.buffer[LEN_SIZE..LEN_SIZE + length])
                } else {
                    ReadResult::NeedData
                }
            }
            ReaderState::ShouldClear { body_length } => {
                self.buffer.copy_within(LEN_SIZE + body_length..self.len, 0);
                self.len = self
                    .len
                    .saturating_sub(body_length)
                    .saturating_sub(LEN_SIZE);
                self.state = ReaderState::MissingLength;
                self.next_state()
            }
        }
    }

    /// Push data to the reader's internal buffer.
    ///
    /// `get_packet()` should be called to extract packets.
    pub fn push_data(&mut self, data: &[u8]) -> Result<(), PushDataError> {
        if self.len + data.len() > self.buffer.len() {
            return Err(PushDataError::BufferTooBig);
        }
        self.buffer[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
        Ok(())
    }

    /// Get a packet if it's available.
    ///
    /// This should be called in a loop until `None` is returned which means more data is needed.
    pub fn get_packet(&mut self) -> ReadResult<'_> {
        self.next_state()
    }

    /// Take all buffered bytes that are not part of an already-returned packet
    /// and reset the reader.
    ///
    /// This is used when the underlying connection is handed to another
    /// protocol layer (e.g. a TLS upgrade after the plaintext `Version`
    /// exchange): a single read may have pulled in bytes belonging to that
    /// next layer, and those must be replayed there instead of being lost.
    pub fn drain_unparsed(&mut self) -> Vec<u8> {
        const LEN_SIZE: usize = std::mem::size_of::<u32>();
        let start = match self.state {
            ReaderState::ShouldClear { body_length } => LEN_SIZE + body_length,
            _ => 0,
        };
        let data = self.buffer[start..self.len].to_vec();
        self.len = 0;
        self.state = ReaderState::MissingLength;
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_config_url() {
        let samples = [
            FCastNetworkConfig {
                name: "Living Room".to_string(),
                addresses: vec!["192.168.1.42".to_string()],
                services: vec![FCastService {
                    port: 46899,
                    r#type: 0,
                }],
                txt: None,
            },
            FCastNetworkConfig {
                name: "kitchen-tv".to_string(),
                addresses: vec![
                    "10.0.0.5".to_string(),
                    "fe80::1ff:fe23:4567:890a".to_string(),
                ],
                services: vec![FCastService {
                    port: 46899,
                    r#type: 0,
                }],
                txt: Some(HashMap::from([
                    ("version".to_string(), "3".to_string()),
                    ("id".to_string(), "abc-123".to_string()),
                ])),
            },
            FCastNetworkConfig {
                name: "æøå".to_string(),
                addresses: vec![],
                services: vec![],
                txt: Some(HashMap::new()),
            },
        ];

        for config in samples {
            let url = config.to_url().expect("serializing to url should succeed");
            assert!(url.starts_with("fcast://r/"), "unexpected url: {url}");
            let parsed = FCastNetworkConfig::parse_url(&url)
                .unwrap_or_else(|| panic!("parsing url should succeed: {url}"));
            assert_eq!(parsed, config);
        }
    }

    #[test]
    fn test_parse_url_rejects_invalid() {
        assert!(FCastNetworkConfig::parse_url("https://example.com").is_none());
        assert!(FCastNetworkConfig::parse_url("fcast://r/not-valid-base64-$$$").is_none());
    }

    #[test]
    fn packet_reader_single() {
        let mut reader = PacketReader::new(100, 0);
        reader
            .push_data(&[1u32.to_le_bytes().as_slice(), [0u8].as_slice()].concat())
            .unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.len, 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert_eq!(reader.len, 0);
    }

    #[test]
    fn packet_reader_small_push() {
        let mut reader = PacketReader::new(100, 0);
        let length = 1u32.to_le_bytes();
        reader.push_data(&[length[0], length[1]]).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        reader.push_data(&[length[2]]).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.state, ReaderState::MissingLength);
        reader.push_data(&[length[3]]).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        reader.push_data(&[0]).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.len, 0);
    }

    #[rustfmt::skip]
    #[test]
    fn packet_reader_many_packets_single_push() {
        let mut reader = PacketReader::new(100, 0);
        reader.push_data(&[
            1u32.to_le_bytes().as_slice(), [0u8].as_slice(),
            2u32.to_le_bytes().as_slice(), [0u8, 1].as_slice(),
            3u32.to_le_bytes().as_slice(), [0u8, 1, 2].as_slice(),
        ].concat()).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0, 1]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 2 });
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0, 1, 2]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 3 });
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert_eq!(reader.len, 0);
    }

    #[test]
    fn packet_reader_partial_body() {
        let mut reader = PacketReader::new(100, 0);
        reader
            .push_data(&[4u32.to_le_bytes().as_slice(), [0u8, 1].as_slice()].concat())
            .unwrap();
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        reader.push_data(&[2]).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        reader.push_data(&[3]).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0, 1, 2, 3]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 4 });
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert_eq!(reader.len, 0);
    }

    #[test]
    fn packet_reader_large_body() {
        let mut reader = PacketReader::new(100, 0);
        let body = (0..10).collect::<Vec<u8>>();
        reader
            .push_data(&[10u32.to_le_bytes().as_slice(), body.as_slice()].concat())
            .unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&body));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 10 });
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert_eq!(reader.len, 0);
    }

    #[test]
    fn large_body_size() {
        let mut reader = PacketReader::new(65280, 0);
        let body = &[255, 255, 255, 0];
        reader.push_data(body).unwrap();
        assert_eq!(
            reader.get_packet(),
            ReadResult::PacketTooLarge(u32::from_le_bytes(*body) as usize)
        );
    }

    #[test]
    fn drain_unparsed_returns_bytes_after_packet() {
        let mut reader = PacketReader::new(100, 16);
        let trailing = [0x16u8, 0x03, 0x01, 0x02, 0x00, 0x42];
        reader
            .push_data(
                &[
                    1u32.to_le_bytes().as_slice(),
                    [7u8].as_slice(),
                    trailing.as_slice(),
                ]
                .concat(),
            )
            .unwrap();

        assert_eq!(reader.get_packet(), ReadResult::Read(&[7]));
        assert_eq!(reader.state, ReaderState::ShouldClear { body_length: 1 });
        assert_eq!(reader.drain_unparsed(), trailing);
        assert_eq!(reader.len, 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
    }

    #[test]
    fn drain_unparsed_without_reading_returns_everything() {
        let mut reader = PacketReader::new(100, 0);
        let data = [0x16u8, 0x03, 0x01, 0x00];
        reader.push_data(&data).unwrap();
        assert_eq!(reader.drain_unparsed(), data);
        assert_eq!(reader.len, 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
    }

    #[test]
    fn drain_unparsed_with_partial_packet_returns_everything() {
        let mut reader = PacketReader::new(100, 0);
        let data = [4u32.to_le_bytes().as_slice(), [0u8, 1].as_slice()].concat();
        reader.push_data(&data).unwrap();
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.state, ReaderState::MissingBody { length: 4 });

        assert_eq!(reader.drain_unparsed(), data);
        assert_eq!(reader.len, 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
    }

    #[test]
    fn drain_unparsed_when_empty_is_empty() {
        let mut reader = PacketReader::new(100, 0);
        assert!(reader.drain_unparsed().is_empty());
        assert_eq!(reader.state, ReaderState::MissingLength);
    }

    #[test]
    fn reader_is_reusable_after_drain() {
        let mut reader = PacketReader::new(100, 16);
        reader
            .push_data(
                &[
                    1u32.to_le_bytes().as_slice(),
                    [7u8].as_slice(),
                    &[0xaa, 0xbb],
                ]
                .concat(),
            )
            .unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&[7]));
        assert_eq!(reader.drain_unparsed(), [0xaa, 0xbb]);

        reader
            .push_data(&[2u32.to_le_bytes().as_slice(), [8u8, 9].as_slice()].concat())
            .unwrap();
        assert_eq!(reader.get_packet(), ReadResult::Read(&[8, 9]));
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.len, 0);
    }
}
