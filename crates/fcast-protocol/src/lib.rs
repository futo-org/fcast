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
    /// Start of unconsumed data in `buffer`.
    pos: usize,
    /// End of valid data in `buffer`.
    len: usize,
    max_packet_size: usize,
}

impl PacketReader {
    pub fn new(max_packet_size: usize, padding: usize) -> Self {
        Self {
            buffer: vec![0; size_of::<u32>() + max_packet_size + padding],
            state: ReaderState::MissingLength,
            pos: 0,
            len: 0,
            max_packet_size,
        }
    }

    /// Number of buffered bytes not yet consumed as packets.
    fn buffered(&self) -> usize {
        self.len - self.pos
    }

    /// Resolve a pending [`ReaderState::ShouldClear`]: advance `pos` past the packet that was
    /// returned by the previous `get_packet` call. No bytes move.
    fn discard_consumed(&mut self) {
        if let ReaderState::ShouldClear { body_length } = self.state {
            self.pos += size_of::<u32>() + body_length;
            self.state = ReaderState::MissingLength;
        }
    }

    /// Reclaim the space of already-consumed packets by moving the unconsumed tail to the front of
    /// the buffer. Called once per refill (`push_data`/`spare_capacity_mut`), not per packet.
    fn compact(&mut self) {
        self.discard_consumed();
        if self.pos == 0 {
            return;
        }
        self.buffer.copy_within(self.pos..self.len, 0);
        self.len -= self.pos;
        self.pos = 0;
    }

    fn next_state(&mut self) -> ReadResult<'_> {
        const LEN_SIZE: usize = std::mem::size_of::<u32>();

        match self.state {
            ReaderState::MissingLength => {
                if self.buffered() >= LEN_SIZE {
                    let length = u32::from_le_bytes(
                        self.buffer[self.pos..self.pos + LEN_SIZE]
                            .try_into()
                            .expect("slice is LEN_SIZE bytes"),
                    ) as usize;
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
                if self.buffered().saturating_sub(LEN_SIZE) >= length {
                    self.state = ReaderState::ShouldClear {
                        body_length: length,
                    };
                    let start = self.pos + LEN_SIZE;
                    ReadResult::Read(&self.buffer[start..start + length])
                } else {
                    ReadResult::NeedData
                }
            }
            ReaderState::ShouldClear { .. } => {
                self.discard_consumed();
                self.next_state()
            }
        }
    }

    /// Push data to the reader's internal buffer.
    ///
    /// `get_packet()` should be called to extract packets.
    pub fn push_data(&mut self, data: &[u8]) -> Result<(), PushDataError> {
        if self.len + data.len() > self.buffer.len() {
            self.compact();
            if self.len + data.len() > self.buffer.len() {
                return Err(PushDataError::BufferTooBig);
            }
        }
        self.buffer[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
        Ok(())
    }

    /// Borrow the unused tail of the internal buffer to fill in place.
    ///
    /// This is the zero-copy counterpart to [`push_data`]: instead of reading into a scratch buffer
    /// and copying that in, a transport can write straight into the reassembly buffer and mark the
    /// bytes received with [`commit`]. That removes one copy of every received byte on the hot
    /// receive path.
    ///
    /// Space freed by already-consumed packets is reclaimed here (at most one compaction per
    /// refill), so the returned slice is empty only when unconsumed data fills the whole buffer. A
    /// caller that drains to [`NeedData`] before each read never sees that: a mid-packet reader
    /// holds fewer than `size_of::<u32>() + max_packet_size` bytes, less than the buffer's capacity
    /// even with `padding == 0`. The slice is therefore never empty, so a read into it cannot
    /// return `Ok(0)` and be mistaken for end-of-stream. `padding` only trades memory for fewer,
    /// larger reads.
    ///
    /// # Example
    ///
    /// ```
    /// use std::io::Read;
    ///
    /// use fcast_protocol::{PacketReader, ReadResult};
    ///
    /// // A transport carrying one framed packet: length prefix 3, body [1, 2, 3].
    /// let mut stream: &[u8] = &[3, 0, 0, 0, 1, 2, 3];
    ///
    /// let mut reader = PacketReader::new(1024, 0);
    /// let n = stream.read(reader.spare_capacity_mut())?;
    /// reader.commit(n);
    /// assert_eq!(reader.get_packet(), ReadResult::Read(&[1, 2, 3]));
    /// assert_eq!(reader.get_packet(), ReadResult::NeedData);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// [`push_data`]: Self::push_data
    /// [`commit`]: Self::commit
    /// [`NeedData`]: ReadResult::NeedData
    pub fn spare_capacity_mut(&mut self) -> &mut [u8] {
        self.compact();
        &mut self.buffer[self.len..]
    }

    /// Mark `n` bytes written into the slice returned by [`spare_capacity_mut`] as
    /// received.
    ///
    /// `n` must not exceed the length of that slice (a transport must never report having read more
    /// bytes than the slice could hold). In debug builds this is asserted; in release builds an
    /// out-of-range `n` corrupts the reader's length bookkeeping, so it is a caller bug rather than
    /// defined behaviour.
    ///
    /// [`spare_capacity_mut`]: Self::spare_capacity_mut
    pub fn commit(&mut self, n: usize) {
        debug_assert!(
            self.len + n <= self.buffer.len(),
            "commit({n}) overflows reader buffer (len={}, capacity={})",
            self.len,
            self.buffer.len()
        );
        self.len += n;
    }

    /// Get a packet if it's available.
    ///
    /// This should be called in a loop until `None` is returned which means more data is needed.
    pub fn get_packet(&mut self) -> ReadResult<'_> {
        self.next_state()
    }

    /// Take all buffered bytes that are not part of an already-returned packet and reset the
    /// reader.
    ///
    /// This is used when the underlying connection is handed to another protocol layer (e.g. a TLS
    /// upgrade after the plaintext `Version` exchange): a single read may have pulled in bytes
    /// belonging to that next layer, and those must be replayed there instead of being lost.
    pub fn drain_unparsed(&mut self) -> Vec<u8> {
        self.discard_consumed();
        let data = self.buffer[self.pos..self.len].to_vec();
        self.pos = 0;
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
        assert_eq!(reader.buffered(), 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
    }

    #[test]
    fn drain_unparsed_without_reading_returns_everything() {
        let mut reader = PacketReader::new(100, 0);
        let data = [0x16u8, 0x03, 0x01, 0x00];
        reader.push_data(&data).unwrap();
        assert_eq!(reader.drain_unparsed(), data);
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
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
        assert_eq!(reader.buffered(), 0);
    }

    // ---- zero-copy read path: `spare_capacity_mut` + `commit` ----

    const LEN_SIZE: usize = std::mem::size_of::<u32>();

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut v = (body.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(body);
        v
    }

    fn drain_zerocopy(reader: &mut PacketReader, data: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let spare = reader.spare_capacity_mut();
            assert!(
                !spare.is_empty(),
                "spare capacity empty before a read (would be read as EOF)"
            );
            let want = if chunk == 0 {
                spare.len()
            } else {
                chunk.min(spare.len())
            };
            let take = want.min(data.len() - pos);
            spare[..take].copy_from_slice(&data[pos..pos + take]);
            reader.commit(take);
            pos += take;

            loop {
                match reader.get_packet() {
                    ReadResult::Read(p) => out.push(p.to_vec()),
                    ReadResult::NeedData => break,
                    ReadResult::PacketTooLarge(s) => panic!("unexpected PacketTooLarge({s})"),
                }
            }
        }
        out
    }

    fn drain_pushdata(reader: &mut PacketReader, data: &[u8], scratch: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; scratch];
        let mut pos = 0;
        while pos < data.len() {
            let n = scratch.min(data.len() - pos);
            buf[..n].copy_from_slice(&data[pos..pos + n]);
            reader.push_data(&buf[..n]).expect("push_data overflowed");
            pos += n;
            loop {
                match reader.get_packet() {
                    ReadResult::Read(p) => out.push(p.to_vec()),
                    ReadResult::NeedData => break,
                    ReadResult::PacketTooLarge(s) => panic!("unexpected PacketTooLarge({s})"),
                }
            }
        }
        out
    }

    #[test]
    fn spare_capacity_starts_at_full_buffer() {
        let mut reader = PacketReader::new(100, 16);
        assert_eq!(reader.spare_capacity_mut().len(), LEN_SIZE + 100 + 16);
    }

    #[test]
    fn commit_zero_is_noop() {
        let mut reader = PacketReader::new(100, 16);
        let before = reader.spare_capacity_mut().len();
        reader.commit(0);
        assert_eq!(reader.spare_capacity_mut().len(), before);
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn spare_capacity_shrinks_by_commit_and_regrows_after_consume() {
        let mut reader = PacketReader::new(100, 16);
        let cap = LEN_SIZE + 100 + 16;

        // Write a whole framed packet plus the length prefix of a second, in place.
        let first = frame(&[0xAA, 0xBB, 0xCC]);
        let second_prefix = 2u32.to_le_bytes();
        let n = {
            let spare = reader.spare_capacity_mut();
            spare[..first.len()].copy_from_slice(&first);
            spare[first.len()..first.len() + LEN_SIZE].copy_from_slice(&second_prefix);
            first.len() + LEN_SIZE
        };
        reader.commit(n);
        assert_eq!(reader.spare_capacity_mut().len(), cap - n);

        // Consuming the first packet frees its bytes; the next `spare_capacity_mut` compacts
        // (leftover prefix moves to the front) and the spare grows back to
        // all-but-the-leftover.
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0xAA, 0xBB, 0xCC]));
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        assert_eq!(reader.buffered(), LEN_SIZE);
        assert_eq!(reader.spare_capacity_mut().len(), cap - LEN_SIZE);
    }

    #[test]
    fn consuming_packets_does_not_move_buffered_data() {
        // Consumption must be a cursor advance, not a per-packet memmove of the tail -
        // otherwise a read that batches K packets costs O(K^2) byte moves.
        let mut reader = PacketReader::new(100, 16);
        let stream: Vec<u8> = [frame(&[0]), frame(&[0, 1]), frame(&[0, 1, 2])].concat();
        reader.push_data(&stream).unwrap();

        assert_eq!(reader.get_packet(), ReadResult::Read(&[0]));
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0, 1]));
        assert_eq!(reader.get_packet(), ReadResult::Read(&[0, 1, 2]));
        assert_eq!(reader.get_packet(), ReadResult::NeedData);

        // All three packets were consumed purely by advancing the cursor: the write end
        // never moved back, so no bytes were copied while draining.
        assert_eq!(reader.pos, stream.len());
        assert_eq!(reader.len, stream.len());

        // The next refill reclaims the whole buffer in one step - and since nothing is
        // buffered mid-packet, it is a free cursor reset rather than a copy.
        assert_eq!(
            reader.spare_capacity_mut().len(),
            LEN_SIZE + 100 + 16,
            "refill should reclaim all consumed space"
        );
        assert_eq!((reader.pos, reader.len), (0, 0));
    }

    #[test]
    fn zerocopy_single_packet() {
        let mut reader = PacketReader::new(100, 16);
        let body = [7u8, 8, 9];
        let out = drain_zerocopy(&mut reader, &frame(&body), 0);
        assert_eq!(out, vec![body.to_vec()]);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn zerocopy_reassembles_across_all_chunk_sizes() {
        let bodies: Vec<Vec<u8>> = vec![
            vec![0], // opcode-only packet
            vec![1, 2],
            (0..37u8).collect(),
            vec![0xFF; 90], // near max
            vec![42],
            (0..64u8).rev().collect(),
        ];
        let mut stream = Vec::new();
        for b in &bodies {
            stream.extend_from_slice(&frame(b));
        }

        for chunk in [1usize, 2, 3, 4, 5, 6, 7, 8, 13, 31, 64, 100, 8192, 0] {
            let mut reader = PacketReader::new(100, 8192);
            let out = drain_zerocopy(&mut reader, &stream, chunk);
            assert_eq!(out, bodies, "mismatch at chunk size {chunk}");
            assert_eq!(
                reader.buffered(),
                0,
                "buffer not drained at chunk size {chunk}"
            );
        }
    }

    #[test]
    fn zerocopy_large_packet_split_byte_by_byte() {
        let mut reader = PacketReader::new(100_000, 8192);
        let body: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let out = drain_zerocopy(&mut reader, &frame(&body), 1);
        assert_eq!(out, vec![body]);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn zerocopy_matches_push_data_path() {
        // Same input, same chunking, two APIs - identical extracted packets.
        let bodies: Vec<Vec<u8>> = vec![vec![1], (0..50u8).collect(), vec![9; 80], vec![2, 3]];
        let mut stream = Vec::new();
        for b in &bodies {
            stream.extend_from_slice(&frame(b));
        }

        for chunk in [1usize, 3, 7, 64, 128] {
            let mut zc = PacketReader::new(100, 8192);
            let mut pd = PacketReader::new(100, 8192);
            let zc_out = drain_zerocopy(&mut zc, &stream, chunk);
            let pd_out = drain_pushdata(&mut pd, &stream, chunk);
            assert_eq!(
                zc_out, pd_out,
                "zero-copy vs push_data diverged at chunk {chunk}"
            );
            assert_eq!(zc_out, bodies);
        }
    }

    #[test]
    fn zerocopy_full_buffer_still_yields_a_packet() {
        // buffer = 4 + 64 + 16 = 84. Fill it exactly: one max-size (64B) packet plus 16
        // bytes of the next. A full buffer must still surface the complete packet - the
        // invariant that guarantees the spare never stays empty.
        let max = 64usize;
        let padding = 16usize;
        let mut reader = PacketReader::new(max, padding);
        let big = frame(&vec![0x5Au8; max]); // 4 + 64 = 68 bytes
                                             // 16 trailing bytes forming the *start* of a second packet: a length prefix of 16
                                             // but only 12 of those 16 body bytes present, so it stays incomplete (NeedData)
                                             // rather than parsing as another packet.
        let mut trailing = 16u32.to_le_bytes().to_vec();
        trailing.extend_from_slice(&[0xEE; 12]);
        assert_eq!(trailing.len(), 16);
        let n = {
            let spare = reader.spare_capacity_mut();
            assert_eq!(spare.len(), LEN_SIZE + max + padding);
            spare[..big.len()].copy_from_slice(&big);
            spare[big.len()..big.len() + trailing.len()].copy_from_slice(&trailing);
            big.len() + trailing.len()
        };
        reader.commit(n);
        assert_eq!(
            reader.spare_capacity_mut().len(),
            0,
            "buffer should be exactly full"
        );

        assert_eq!(reader.get_packet(), ReadResult::Read(&[0x5A; 64]));
        assert_eq!(reader.get_packet(), ReadResult::NeedData);
        // Refilling compacts the 16 leftover bytes to the front; spare recovered (no
        // deadlock).
        assert_eq!(reader.buffered(), 16);
        assert_eq!(
            reader.spare_capacity_mut().len(),
            LEN_SIZE + max + padding - 16
        );
    }

    #[test]
    fn zerocopy_never_false_eof_under_back_to_back_max_packets() {
        // Greedy reads (chunk = 0) over a stream of max-size packets: the assertion inside
        // `drain_zerocopy` fails if the spare is ever empty before a read.
        let max = 200usize;
        let mut reader = PacketReader::new(max, 64);
        let bodies: Vec<Vec<u8>> = (0..15)
            .map(|i| vec![i as u8; max]) // each body exactly max_packet_size
            .collect();
        let mut stream = Vec::new();
        for b in &bodies {
            stream.extend_from_slice(&frame(b));
        }
        let out = drain_zerocopy(&mut reader, &stream, 0);
        assert_eq!(out, bodies);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn zerocopy_drain_unparsed_recovers_tls_prefix() {
        // Mirrors the receiver's TLS upgrade: a single read pulls in the plaintext `Version`
        // packet plus the first bytes of the following TLS ClientHello, committed in place.
        let mut reader = PacketReader::new(100, 16);
        let version = frame(&[Opcode::Version as u8, b'{', b'}']);
        let handshake = [0x16u8, 0x03, 0x01, 0x02, 0x00, 0x42];
        let n = {
            let spare = reader.spare_capacity_mut();
            spare[..version.len()].copy_from_slice(&version);
            spare[version.len()..version.len() + handshake.len()].copy_from_slice(&handshake);
            version.len() + handshake.len()
        };
        reader.commit(n);

        assert_eq!(
            reader.get_packet(),
            ReadResult::Read(&[Opcode::Version as u8, b'{', b'}'])
        );
        assert_eq!(reader.drain_unparsed(), handshake);
        assert_eq!(reader.buffered(), 0);
        assert_eq!(reader.state, ReaderState::MissingLength);
    }

    #[test]
    fn zerocopy_too_large_prefix_is_reported() {
        let mut reader = PacketReader::new(64, 16);
        // Length prefix of 65 (> max 64). Write just the prefix and commit.
        let prefix = 65u32.to_le_bytes();
        reader.spare_capacity_mut()[..LEN_SIZE].copy_from_slice(&prefix);
        reader.commit(LEN_SIZE);
        assert_eq!(reader.get_packet(), ReadResult::PacketTooLarge(65));
    }

    #[test]
    fn zerocopy_randomized_reassembly() {
        // Deterministic xorshift PRNG, random packet counts, sizes and read chunking. The
        // extracted packets must always equal the framed input, regardless of segmentation.
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let max = 300usize;
        for _ in 0..400 {
            let n_packets = (next() % 12) as usize + 1;
            let bodies: Vec<Vec<u8>> = (0..n_packets)
                .map(|_| {
                    let len = (next() as usize % max) + 1; // 1..=max
                    (0..len).map(|_| next() as u8).collect()
                })
                .collect();
            let mut stream = Vec::new();
            for b in &bodies {
                stream.extend_from_slice(&frame(b));
            }
            let chunk = (next() as usize % 40) + 1; // 1..=40 (also exercises tiny reads)
            let mut reader = PacketReader::new(max, 8192);
            let out = drain_zerocopy(&mut reader, &stream, chunk);
            assert_eq!(
                out, bodies,
                "randomized reassembly mismatch (chunk={chunk})"
            );
            assert_eq!(reader.buffered(), 0);
        }
    }
}
