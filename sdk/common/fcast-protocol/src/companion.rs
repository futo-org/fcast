// TODO: optimize serialize functions

use std::{error, fmt};

pub type ProviderId = u16;

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
pub struct U48(u64);

impl From<u32> for U48 {
    fn from(value: u32) -> Self {
        Self(value.into())
    }
}

impl U48 {
    pub fn size() -> usize {
        6
    }

    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() != Self::size() {
            return Err(ParseError::InvalidSize);
        }

        Ok(Self(u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], 0, 0,
        ])))
    }

    pub fn to_le_bytes(&self) -> [u8; 6] {
        let byts = self.0.to_le_bytes();
        [byts[0], byts[1], byts[2], byts[3], byts[4], byts[5]]
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct HelloResponse {
    pub provider_id: ProviderId,
}

impl HelloResponse {
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() != size_of::<ProviderId>() {
            return Err(ParseError::InvalidSize);
        }

        Ok(Self {
            provider_id: ProviderId::from_le_bytes([buf[0], buf[1]]),
        })
    }

    pub fn serialize(&self) -> [u8; 2] {
        self.provider_id.to_le_bytes()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResourceInfoRequest {
    pub request_id: u32,
    pub resource_id: u32,
}

impl ResourceInfoRequest {
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() != size_of::<u32>() * 2 {
            return Err(ParseError::InvalidSize);
        }

        let request_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let resource_id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

        Ok(Self {
            request_id,
            resource_id,
        })
    }

    pub fn serialize(&self) -> [u8; size_of::<u32>() + size_of::<u32>()] {
        let req = self.request_id.to_le_bytes();
        let res = self.resource_id.to_le_bytes();

        [
            req[0], req[1], req[2], req[3], //
            res[0], res[1], res[2], res[3], //
        ]
    }
}

pub fn parse_str(buf: &[u8]) -> Result<&str, ParseError> {
    if buf.len() < size_of::<u16>() {
        return Err(ParseError::InvalidSize);
    }

    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < len {
        return Err(ParseError::MissingData);
    }

    let start_idx = size_of::<u16>();
    let s =
        str::from_utf8(&buf[start_idx..start_idx + len]).map_err(|_| ParseError::InvalidUtf8Str)?;
    Ok(s)
}

pub fn serialize_str(s: &str) -> Vec<u8> {
    [
        &(s.len() as u16).to_le_bytes(), //
        s.as_bytes(),                    //
    ]
    .concat()
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResourceInfoResponse<'a> {
    pub request_id: u32,
    pub content_type: &'a str,
    pub resource_size: ResourceSize,
}

impl<'a> ResourceInfoResponse<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, ParseError> {
        if buf.len() < size_of::<u32>() + size_of::<u16>() + 1 {
            return Err(ParseError::MissingData);
        }

        let request_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let mut start_idx = 4;
        let content_type = parse_str(&buf[start_idx..])?;
        start_idx += size_of::<u16>() + content_type.len();
        let resource_size = ResourceSize::parse(&buf[start_idx..])?;

        Ok(Self {
            request_id,
            content_type,
            resource_size,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        [
            self.request_id.to_le_bytes().as_slice(),    //
            serialize_str(self.content_type).as_slice(), //
            self.resource_size.serialize().as_slice(),   //
        ]
        .concat()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResourceSize {
    Unknown,
    Known(U48),
}

impl ResourceSize {
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.is_empty() {
            return Err(ParseError::MissingData);
        }

        match buf[0] {
            0x00 => Ok(Self::Unknown),
            0x01 => Ok(Self::Known(U48::parse(&buf[1..])?)),
            v => Err(ParseError::InvalidEnumVariant(v)),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        match self {
            ResourceSize::Unknown => vec![0x00],
            ResourceSize::Known(size) => [&[0x01], size.to_le_bytes().as_slice()].concat(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResourceRequest {
    pub request_id: u32,
    pub resource_id: u32,
    pub read_head: ReadHead,
}

impl ResourceRequest {
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() < size_of::<u32>() + size_of::<u32>() + 1 {
            return Err(ParseError::MissingData);
        }

        let request_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let resource_id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let start_idx = 8;
        let read_head = ReadHead::parse(&buf[start_idx..])?;

        Ok(Self {
            request_id,
            resource_id,
            read_head,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let req = self.request_id.to_le_bytes();
        let res = self.resource_id.to_le_bytes();

        [
            [req[0], req[1], req[2], req[3]].as_slice(), //
            [res[0], res[1], res[2], res[3]].as_slice(), //
            &self.read_head.serialize(),                 //
        ]
        .concat()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResourceResponse<'a> {
    pub request_id: u32,
    pub result: GetResourceResult<'a>,
}

impl<'a> ResourceResponse<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, ParseError> {
        if buf.len() < size_of::<u32>() {
            return Err(ParseError::MissingData);
        }

        let request_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let result = GetResourceResult::parse(&buf[4..])?;

        Ok(Self { request_id, result })
    }

    pub fn serialize(&self) -> Vec<u8> {
        [
            self.request_id.to_le_bytes().as_slice(),
            self.result.serialize().as_slice(),
        ]
        .concat()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadHead {
    Whole,
    Range { start: U48, stop_inclusive: U48 },
}

impl ReadHead {
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.is_empty() {
            return Err(ParseError::MissingData);
        }

        match buf[0] {
            0x00 => Ok(Self::Whole),
            0x01 => {
                let mut start_idx = 1;
                let start = U48::parse(&buf[start_idx..start_idx + U48::size()])?;
                start_idx += U48::size();
                let stop_inclusive = U48::parse(&buf[start_idx..start_idx + U48::size()])?;
                Ok(Self::Range {
                    start,
                    stop_inclusive,
                })
            }
            v => Err(ParseError::InvalidEnumVariant(v)),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        match self {
            ReadHead::Whole => vec![0x00],
            ReadHead::Range {
                start,
                stop_inclusive,
            } => [
                &[0x01],
                start.to_le_bytes().as_slice(),
                stop_inclusive.to_le_bytes().as_slice(),
            ]
            .concat(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GetResourceResult<'a> {
    None,
    Success(&'a [u8]),
}

impl<'a> GetResourceResult<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, ParseError> {
        if buf.is_empty() {
            return Err(ParseError::MissingData);
        }

        match buf[0] {
            0x00 => Ok(Self::None),
            0x01 => Ok(Self::Success(&buf[1..])),
            v => Err(ParseError::InvalidEnumVariant(v)),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        match self {
            GetResourceResult::None => vec![0x00],
            GetResourceResult::Success(buf) => [&[0x01u8], *buf].concat(),
        }
    }
}

pub fn create_url(provider_id: u16, resource_id: u32) -> String {
    format!("fcomp://{provider_id}.fcast/{resource_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_48_int() {
        assert_eq!(
            U48::parse(&12345u64.to_le_bytes()[0..6]).unwrap(),
            U48(12345),
        );
        assert_eq!(
            U48::parse(&0xFFFFFFFFFFFFu64.to_le_bytes()[0..6]).unwrap(),
            U48(0xFFFFFFFFFFFF),
        );
    }

    #[test]
    fn hello_response() {
        assert_eq!(
            HelloResponse::parse(&HelloResponse { provider_id: 123 }.serialize()).unwrap(),
            HelloResponse { provider_id: 123 }
        );
    }

    #[test]
    fn resource_request() {
        let inp = ResourceRequest {
            request_id: 100,
            resource_id: 200,
            read_head: ReadHead::Whole,
        };
        assert_eq!(ResourceRequest::parse(&inp.serialize()).unwrap(), inp,);
        let inp = ResourceRequest {
            request_id: 100,
            resource_id: 200,
            read_head: ReadHead::Range {
                start: 300.into(),
                stop_inclusive: 400.into(),
            },
        };
        assert_eq!(ResourceRequest::parse(&inp.serialize()).unwrap(), inp,);
    }

    #[test]
    fn resource_info_request() {
        let inp = ResourceInfoRequest {
            request_id: 123,
            resource_id: 321,
        };
        assert_eq!(ResourceInfoRequest::parse(&inp.serialize()).unwrap(), inp,);
    }

    #[test]
    fn str_type() {
        assert_eq!(parse_str(&serialize_str("video/mp4")).unwrap(), "video/mp4",);
        assert_eq!(parse_str(&serialize_str("")).unwrap(), "",);
    }

    #[test]
    fn resource_info_response() {
        let inp = ResourceInfoResponse {
            request_id: 100,
            content_type: "video/mp4",
            resource_size: ResourceSize::Unknown,
        };
        assert_eq!(ResourceInfoResponse::parse(&inp.serialize()).unwrap(), inp,);
        let inp = ResourceInfoResponse {
            request_id: 200,
            content_type: "",
            resource_size: ResourceSize::Unknown,
        };
        assert_eq!(ResourceInfoResponse::parse(&inp.serialize()).unwrap(), inp,);
    }

    #[test]
    fn resource_size() {
        let inp = ResourceSize::Unknown;
        assert_eq!(ResourceSize::parse(&inp.serialize()).unwrap(), inp);
        let inp = ResourceSize::Known(1234.into());
        assert_eq!(ResourceSize::parse(&inp.serialize()).unwrap(), inp);
    }

    #[test]
    fn resource_response() {
        let inp = ResourceResponse {
            request_id: 123,
            result: GetResourceResult::None,
        };
        assert_eq!(ResourceResponse::parse(&inp.serialize()).unwrap(), inp,);
        let inp = ResourceResponse {
            request_id: 123,
            result: GetResourceResult::Success(&[1, 2, 3, 4]),
        };
        assert_eq!(ResourceResponse::parse(&inp.serialize()).unwrap(), inp,);
    }

    #[test]
    fn read_head() {
        assert_eq!(
            ReadHead::parse(&ReadHead::Whole.serialize()).unwrap(),
            ReadHead::Whole,
        );
        let inp = ReadHead::Range {
            start: 123.into(),
            stop_inclusive: 321.into(),
        };
        assert_eq!(ReadHead::parse(&inp.serialize()).unwrap(), inp,);
    }

    #[test]
    fn get_resource_result() {
        assert_eq!(
            GetResourceResult::parse(&[0x00]).unwrap(),
            GetResourceResult::None,
        );
        assert_eq!(
            GetResourceResult::parse(&[0x01, 1, 2, 3]).unwrap(),
            GetResourceResult::Success(&[1, 2, 3]),
        );
        assert_eq!(
            GetResourceResult::parse(&[0x01]).unwrap(),
            GetResourceResult::Success(&[]),
        );
    }
}
