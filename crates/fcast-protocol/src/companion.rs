use std::{error, fmt, mem::size_of};

pub type ProviderId = u16;
pub type ResourceId = u32;
pub type RequestId = u32;

const OPCODE_SIZE: usize = 1;
pub const MAX_RESOURCE_READ_SIZE: usize =
    crate::v4::MAX_PACKET_SIZE - ResourceResponse::max_overhead() - OPCODE_SIZE;

#[derive(Debug)]
pub enum ParseError {
    InvalidSize,
    MissingData,
    InvalidEnumVariant(u8),
    InvalidUtf8Str,
}

impl error::Error for ParseError {}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InvalidSize => write!(f, "Invalid size"),
            ParseError::MissingData => write!(f, "Missing data"),
            ParseError::InvalidEnumVariant(v) => write!(f, "Invalid enum variant ({v})"),
            ParseError::InvalidUtf8Str => write!(f, "Invalid UTF-8 string"),
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
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() < Self::max_overhead() {
            return Err(ParseError::MissingData);
        }

        let request_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let part = buf[4];
        let total_parts = buf[5];
        let result = GetResourceResult::parse(&buf[6..])?;

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
    Success(Vec<u8>),
}

impl GetResourceResult {
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.is_empty() {
            return Err(ParseError::MissingData);
        }

        match buf[0] {
            0x00 => Ok(Self::NotFound),
            0x01 => Ok(Self::Success(buf[1..].to_vec())),
            v => Err(ParseError::InvalidEnumVariant(v)),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        match self {
            GetResourceResult::NotFound => vec![0x00],
            GetResourceResult::Success(buf) => [&[Self::success_tag()], buf.as_slice()].concat(),
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
        assert_eq!(ResourceResponse::parse(&inp.serialize()).unwrap(), inp,);
        let inp = ResourceResponse {
            request_id: 123,
            result: GetResourceResult::Success(vec![1, 2, 3, 4]),
            part: 1,
            total_parts: 2,
        };
        assert_eq!(ResourceResponse::parse(&inp.serialize()).unwrap(), inp,);
    }

    #[test]
    fn get_resource_result() {
        assert_eq!(
            GetResourceResult::parse(&[0x00]).unwrap(),
            GetResourceResult::NotFound,
        );
        assert_eq!(
            GetResourceResult::parse(&[0x01, 1, 2, 3]).unwrap(),
            GetResourceResult::Success(vec![1, 2, 3]),
        );
        assert_eq!(
            GetResourceResult::parse(&[0x01]).unwrap(),
            GetResourceResult::Success(vec![]),
        );
    }
}
