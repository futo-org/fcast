//! UMP (Ultra Media Protocol) wire-format reader.
//!
//! A UMP stream is a sequence of parts, each framed as
//! `[type varint][length varint][payload bytes]`. Varints use YouTube's
//! big-endian-ish variable-width encoding where the number of leading 1-bits in
//! the first byte selects the total width (1..=5 bytes).

use std::io;

use bytes::{Buf, BytesMut};
use futures::StreamExt;

use crate::http::SabrBody;

/// UMP part type identifier. Unknown/future wire values round-trip through the
/// [`PartType::Unknown`] variant rather than erroring. `Debug` prints the
/// variant name (and the raw value for unknowns), covering the logging that
/// used to need a hand-written `name()` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartType {
    OnesieHeader,
    OnesieData,
    MediaHeader,
    Media,
    MediaEnd,
    LiveMetadata,
    HostnameChangeHint,
    NextRequestPolicy,
    FormatInitializationMetadata,
    SabrRedirect,
    SabrError,
    SabrSeek,
    ReloadPlayerResponse,
    PlaybackStartPolicy,
    AllowedCachedFormats,
    RequestIdentifier,
    RequestCancellationPolicy,
    TimelineContext,
    SabrContextUpdate,
    StreamProtectionStatus,
    SabrContextSendingPolicy,
    SnackbarMessage,
    Unknown(i32),
}

impl PartType {
    /// Map a raw wire value to a [`PartType`].
    pub fn from_wire(value: i32) -> Self {
        match value {
            10 => Self::OnesieHeader,
            11 => Self::OnesieData,
            20 => Self::MediaHeader,
            21 => Self::Media,
            22 => Self::MediaEnd,
            31 => Self::LiveMetadata,
            32 => Self::HostnameChangeHint,
            35 => Self::NextRequestPolicy,
            42 => Self::FormatInitializationMetadata,
            43 => Self::SabrRedirect,
            44 => Self::SabrError,
            45 => Self::SabrSeek,
            46 => Self::ReloadPlayerResponse,
            47 => Self::PlaybackStartPolicy,
            48 => Self::AllowedCachedFormats,
            52 => Self::RequestIdentifier,
            53 => Self::RequestCancellationPolicy,
            55 => Self::TimelineContext,
            57 => Self::SabrContextUpdate,
            58 => Self::StreamProtectionStatus,
            59 => Self::SabrContextSendingPolicy,
            67 => Self::SnackbarMessage,
            other => Self::Unknown(other),
        }
    }

    /// The raw wire value for this part type.
    pub fn to_wire(self) -> i32 {
        match self {
            Self::OnesieHeader => 10,
            Self::OnesieData => 11,
            Self::MediaHeader => 20,
            Self::Media => 21,
            Self::MediaEnd => 22,
            Self::LiveMetadata => 31,
            Self::HostnameChangeHint => 32,
            Self::NextRequestPolicy => 35,
            Self::FormatInitializationMetadata => 42,
            Self::SabrRedirect => 43,
            Self::SabrError => 44,
            Self::SabrSeek => 45,
            Self::ReloadPlayerResponse => 46,
            Self::PlaybackStartPolicy => 47,
            Self::AllowedCachedFormats => 48,
            Self::RequestIdentifier => 52,
            Self::RequestCancellationPolicy => 53,
            Self::TimelineContext => 55,
            Self::SabrContextUpdate => 57,
            Self::StreamProtectionStatus => 58,
            Self::SabrContextSendingPolicy => 59,
            Self::SnackbarMessage => 67,
            Self::Unknown(v) => v,
        }
    }
}

const MAX_PART_SIZE: i64 = 64 * 1024 * 1024;

/// A single decoded UMP part: its type and its raw payload bytes.
#[derive(Debug, Clone)]
pub struct UmpPart {
    pub ty: PartType,
    pub data: Vec<u8>,
}

/// Number of bytes a varint occupies given its first byte.
pub fn size_of(first_byte: u8) -> usize {
    match first_byte {
        b if b < 128 => 1,
        b if b < 192 => 2,
        b if b < 224 => 3,
        b if b < 240 => 4,
        _ => 5,
    }
}

/// Decode a varint from `bytes` starting at `offset`.
///
/// Returns `(value, next_offset)`. On truncation, returns `(-1, ..)` mirroring
/// the reference implementation so callers can defensively handle short buffers.
pub fn decode_varint(bytes: &[u8], offset: usize) -> (i64, usize) {
    if offset >= bytes.len() {
        return (-1, offset);
    }
    let first = bytes[offset] as usize;
    let size = size_of(bytes[offset]);
    if size == 1 {
        return (first as i64, offset + 1);
    }
    if offset + size > bytes.len() {
        return (-1, bytes.len());
    }

    let mut trailing: i64 = 0;
    for i in 1..size {
        trailing |= (bytes[offset + i] as i64) << (8 * (i - 1));
    }

    if size == 5 {
        return (trailing, offset + 5);
    }
    let value_bits = 8 - size;
    let head = (first & ((1 << value_bits) - 1)) as i64;
    (head | (trailing << value_bits), offset + size)
}

/// Streaming reader over an async UMP byte source.
///
/// Chunks are pulled off the [`SabrBody`] stream into an internal buffer and
/// parts are parsed out of it once fully buffered, so parts may span any number
/// of chunks and a chunk may hold several parts.
pub struct UmpReader {
    stream: SabrBody,
    buf: BytesMut,
    eof: bool,
}

impl UmpReader {
    pub fn new(stream: SabrBody) -> Self {
        Self {
            stream,
            buf: BytesMut::new(),
            eof: false,
        }
    }

    /// Pull one more chunk into `buf`. Returns `false` once the stream is
    /// exhausted.
    async fn fill(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        match self.stream.next().await {
            Some(Ok(chunk)) => {
                self.buf.extend_from_slice(&chunk);
                Ok(true)
            }
            Some(Err(e)) => Err(e),
            None => {
                self.eof = true;
                Ok(false)
            }
        }
    }

    /// Ensure at least `n` bytes are buffered. Returns `false` if the stream
    /// ends first (fewer than `n` bytes will ever be available).
    async fn ensure(&mut self, n: usize) -> io::Result<bool> {
        while self.buf.len() < n {
            if !self.fill().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Read the next part, or `None` at a clean end of stream.
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> io::Result<Option<UmpPart>> {
        // Type varint: a clean EOF before the first byte ends the stream.
        if !self.ensure(1).await? {
            return Ok(None);
        }
        let ty = match self.take_varint().await? {
            Some(ty) => ty,
            None => return Ok(None),
        };
        let length = self
            .take_varint()
            .await?
            .ok_or_else(|| eof(&format!("UMP part {ty} truncated before length")))?;
        if !(0..=MAX_PART_SIZE).contains(&length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UMP part {ty} has implausible length {length}"),
            ));
        }

        let length = length as usize;
        if !self.ensure(length).await? {
            return Err(eof(&format!(
                "UMP part {ty} truncated: got {} of {length}",
                self.buf.len()
            )));
        }
        let data = self.buf.split_to(length).to_vec();
        Ok(Some(UmpPart {
            ty: PartType::from_wire(ty as i32),
            data,
        }))
    }

    /// Consume a varint from the front of `buf`, buffering more as needed.
    /// Returns `None` only on a clean EOF before any byte of the varint.
    async fn take_varint(&mut self) -> io::Result<Option<i64>> {
        if !self.ensure(1).await? {
            return Ok(None);
        }
        let size = size_of(self.buf[0]);
        if !self.ensure(size).await? {
            return Err(eof("Unexpected end of UMP stream"));
        }
        let (value, _) = decode_varint(&self.buf, 0);
        self.buf.advance(size);
        Ok(Some(value))
    }
}

fn eof(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, msg.to_string())
}
