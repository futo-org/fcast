use std::{error, fmt, mem::size_of};

use bytes::Bytes;

pub type ProviderId = u16;
pub type ResourceId = u32;
pub type RequestId = u32;

const OPCODE_SIZE: usize = 1;
pub const MAX_RESOURCE_READ_SIZE: usize =
    crate::v4::MAX_PACKET_SIZE - ResourceResponse::max_overhead() - OPCODE_SIZE;

#[derive(Debug)]
pub enum ParseError {
    MissingData,
    InvalidEnumVariant(u8),
}

impl error::Error for ParseError {}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MissingData => write!(f, "Missing data"),
            ParseError::InvalidEnumVariant(v) => write!(f, "Invalid enum variant ({v})"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResourceResponse {
    pub request_id: u32,
    pub part: u8,
    pub total_parts: u8,
    pub result: GetResourceResult,
}

impl ResourceResponse {
    /// Parse a `Resource` packet body.
    ///
    /// Takes the body by shared ownership so the payload is carried out of
    /// the packet without being copied. [`GetResourceResult::Success`] points
    /// straight at the received bytes. This is the media path, so a copy here
    /// would double the cost of every byte received.
    pub fn parse(buf: Bytes) -> Result<Self, ParseError> {
        if buf.len() < Self::max_overhead() {
            return Err(ParseError::MissingData);
        }

        let request_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let part = buf[4];
        let total_parts = buf[5];
        let result = GetResourceResult::parse(buf.slice(6..))?;

        Ok(Self {
            request_id,
            part,
            total_parts,
            result,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        [
            self.request_id.to_le_bytes().as_slice(),
            &[self.part],
            &[self.total_parts],
            self.result.serialize().as_slice(),
        ]
        .concat()
    }

    pub const fn max_overhead() -> usize {
        size_of::<u32>() + size_of::<u8>() * 3
    }

    pub fn header_success(
        request_id: u32,
        part: u8,
        total_parts: u8,
    ) -> [u8; Self::max_overhead()] {
        let id = request_id.to_le_bytes();
        [
            id[0],
            id[1],
            id[2],
            id[3],
            part,
            total_parts,
            GetResourceResult::success_tag(),
        ]
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GetResourceResult {
    NotFound,
    /// The requested bytes, as a refcounted view of the packet they arrived
    /// in, not a copy.
    Success(Bytes),
}

impl GetResourceResult {
    pub fn parse(buf: Bytes) -> Result<Self, ParseError> {
        match buf.first() {
            None => Err(ParseError::MissingData),
            Some(0x00) => Ok(Self::NotFound),
            // A slice sharing `buf`'s allocation. The payload is never copied.
            Some(0x01) => Ok(Self::Success(buf.slice(1..))),
            Some(&v) => Err(ParseError::InvalidEnumVariant(v)),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        match self {
            GetResourceResult::NotFound => vec![0x00],
            GetResourceResult::Success(buf) => [&[Self::success_tag()], &buf[..]].concat(),
        }
    }

    const fn success_tag() -> u8 {
        0x01
    }
}

pub fn create_url(provider_id: u16, resource_id: u32) -> String {
    format!("fcomp://{provider_id}.fcast/{resource_id}")
}

#[derive(Default)]
pub struct RequestIdGenerator(RequestId);

impl RequestIdGenerator {
    pub fn next(&mut self) -> RequestId {
        self.0 += 1;
        self.0 - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_response() {
        let inp = ResourceResponse {
            request_id: 123,
            result: GetResourceResult::NotFound,
            part: 1,
            total_parts: 2,
        };
        assert_eq!(
            ResourceResponse::parse(Bytes::from(inp.serialize())).unwrap(),
            inp,
        );
        let inp = ResourceResponse {
            request_id: 123,
            result: GetResourceResult::Success(Bytes::from_static(&[1, 2, 3, 4])),
            part: 1,
            total_parts: 2,
        };
        assert_eq!(
            ResourceResponse::parse(Bytes::from(inp.serialize())).unwrap(),
            inp,
        );
    }

    #[test]
    fn get_resource_result() {
        assert_eq!(
            GetResourceResult::parse(Bytes::from_static(&[0x00])).unwrap(),
            GetResourceResult::NotFound,
        );
        assert_eq!(
            GetResourceResult::parse(Bytes::from_static(&[0x01, 1, 2, 3])).unwrap(),
            GetResourceResult::Success(Bytes::from_static(&[1, 2, 3])),
        );
        assert_eq!(
            GetResourceResult::parse(Bytes::from_static(&[0x01])).unwrap(),
            GetResourceResult::Success(Bytes::new()),
        );
    }

    /// A `Resource` packet body, `payload` long, and the offset its payload
    /// starts at.
    fn success_frame(payload: &[u8]) -> (Bytes, usize) {
        let mut buf = Vec::new();
        buf.extend_from_slice(&7u32.to_le_bytes()); // request_id
        buf.push(0); // part
        buf.push(1); // total_parts
        buf.push(GetResourceResult::success_tag());
        let offset = buf.len();
        buf.extend_from_slice(payload);
        (Bytes::from(buf), offset)
    }

    #[test]
    fn parsing_a_success_does_not_copy_the_payload() {
        // The payload the parser yields must be the same storage as the packet
        // it was handed, not an equal-valued copy.
        let payload = [0xABu8; 4096];
        let (packet, offset) = success_frame(&payload);
        let payload_ptr = packet[offset..].as_ptr();

        let resp = ResourceResponse::parse(packet.clone()).unwrap();
        let GetResourceResult::Success(body) = resp.result else {
            panic!("expected a success response");
        };

        assert_eq!(&body[..], &payload[..]);
        assert!(
            std::ptr::eq(body.as_ptr(), payload_ptr),
            "payload was copied out of the packet"
        );
    }

    #[test]
    fn parsing_a_success_keeps_the_packet_alive() {
        // The response owns its share of the packet, so it stays valid after
        // the packet handle the parser was given is gone.
        let payload: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
        let (packet, _) = success_frame(&payload);

        let resp = ResourceResponse::parse(packet).unwrap();
        assert_eq!(
            resp.result,
            GetResourceResult::Success(Bytes::from(payload.clone()))
        );
        assert_eq!(resp.request_id, 7);
        assert_eq!((resp.part, resp.total_parts), (0, 1));
    }

    #[test]
    fn success_round_trips_over_the_wire() {
        // Sharing must not change a single byte of the wire format.
        let payload: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let inp = ResourceResponse {
            request_id: u32::MAX,
            part: 3,
            total_parts: 9,
            result: GetResourceResult::Success(Bytes::from(payload)),
        };
        let wire = inp.serialize();
        let (expected, _) = success_frame(&[]);
        assert_eq!(wire[6], expected[6], "success tag moved");
        assert_eq!(ResourceResponse::parse(Bytes::from(wire)).unwrap(), inp);
    }
}
