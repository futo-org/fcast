//! Apple binary [plist](https://en.wikipedia.org/wiki/Property_list) parsing utilities.

// References:
//  - https://medium.com/@karaiskc/understanding-apples-binary-property-list-format-281e6da00dbd
//  - https://github.com/Apple-FOSS-Mirror/CF/file/CFBinaryPList.c
//  - http://fileformats.archiveteam.org/wiki/Property_List/Binary

const HEADER_MAGIC_NUMBER: &[u8] = b"bplist";
const HEADER_SIZE: u64 = 8;
const TRAILER_SIZE: usize = 32;
const MAX_OFFSET_TABLE_OFFSET_SIZE: usize = 4;
const MAX_OBJECT_REF_SIZE: usize = 4;
const I8_N_BYTES: u8 = 1 << 0;
const I16_N_BYTES: u8 = 1 << 1;
const I32_N_BYTES: u8 = 1 << 2;
const I64_N_BYTES: u8 = 1 << 3;
const F32_N_BYTES: u8 = 1 << 2;
const F64_N_BYTES: u8 = 1 << 3;
const INT_MARKER: u8 = 0b0001;
const REAL_MARKER: u8 = 0b0010;
const DATA_MARKER: u8 = 0b0100;
const ARRAY_MARKER: u8 = 0b1010;
const ASCII_STR_MARKER: u8 = 0b0101;
const UTF16_STR_MARKER: u8 = 0b0110;
const UID_MARKER: u8 = 0b1000;
const DICT_MARKER: u8 = 0b1101;
const DATE_MARKER: u8 = 0b0011;

const MAX_OBJECTS_IN_LIST: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum PlistReadError {
    #[error("header is missing magic number")]
    MissingMagicNumber,
    #[error("invalid magic number")]
    InvalidMagicNumber,
    #[error("missing byte")]
    MissingByte,
    #[error("unsupported version")]
    UnsupportedVersion,
    #[error("unexpected object type")]
    UnexpectedObjectType,
    #[error("failed to convert int to usize")]
    IntToUsizeFailed,
    #[error("missing trailer")]
    MissingTrailer,
}

#[derive(PartialEq, Debug)]
pub enum Version {
    Zero,
}

#[derive(PartialEq, Debug, Clone)]
pub enum Object {
    Null, // v1+ only but we include it anyways
    Bool(bool),
    Fill,
    Int(i64),
    Real(f64),
    Date(f64),
    Data(Vec<u8>),
    Uid(u64),
    Array(Vec<Object>),
    String(String),
    Dict(Vec<(Object, Object)>),
}

impl Object {
    pub fn try_int_to_usize(&self) -> Result<usize, PlistReadError> {
        Ok(match self {
            Object::Int(n) => (*n)
                .try_into()
                .map_err(|_| PlistReadError::IntToUsizeFailed)?,
            _ => return Err(PlistReadError::UnexpectedObjectType),
        })
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct Trailer {
    pub offset_table_offset_size: usize,
    pub object_ref_size: usize,
    pub num_objects: u64,
    pub top_object_offset: u64,
    pub offset_table_start: u64,
}

#[derive(Debug)]
pub struct PlistReader<'a> {
    i: usize,
    plist: &'a [u8],
}

/// Read n bytes from `self`'s `plist` where `i` is the end ptr, and store the result in a fixed-size array.
/// Not suitable for a large N.
macro_rules! read_n_bytes {
    ($self:expr, $n:expr, $i:expr) => {{
        let mut bytes = [0u8; $n as usize];
        bytes.copy_from_slice(&$self.plist[$i - ($n as usize)..$i]);
        bytes
    }};
}

impl<'a> PlistReader<'a> {
    pub fn new(plist: &'a [u8]) -> Self {
        Self { i: 0, plist }
    }

    pub fn read_magic_number(&mut self) -> Result<(), PlistReadError> {
        if self.plist.len() < HEADER_MAGIC_NUMBER.len() {
            return Err(PlistReadError::MissingMagicNumber);
        }

        if self.plist[0..HEADER_MAGIC_NUMBER.len()] == *HEADER_MAGIC_NUMBER {
            self.i += HEADER_MAGIC_NUMBER.len();
            Ok(())
        } else {
            Err(PlistReadError::InvalidMagicNumber)
        }
    }

    #[inline]
    fn read_next_byte(&mut self) -> Result<u8, PlistReadError> {
        self.i += 1;
        self.plist
            .get(self.i - 1)
            .ok_or(PlistReadError::MissingByte)
            .copied()
    }

    pub fn read_version(&mut self) -> Result<Version, PlistReadError> {
        if self.read_next_byte()? == b'0' && self.read_next_byte()? == b'0' {
            Ok(Version::Zero)
        } else {
            Err(PlistReadError::UnsupportedVersion)
        }
    }

    pub fn read_trailer(&self) -> Result<Trailer, PlistReadError> {
        if self.plist.len() < TRAILER_SIZE {
            return Err(PlistReadError::MissingTrailer);
        }

        let trailer_start = self.plist.len() - TRAILER_SIZE;
        let offset_table_offset_size = self.plist[trailer_start + 6] as usize;
        let object_ref_size = self.plist[trailer_start + 7] as usize;
        let mut num_objects = [0u8; 8];
        num_objects.copy_from_slice(&self.plist[trailer_start + 8..trailer_start + 16]);
        let mut top_object_offset = [0u8; 8];
        top_object_offset.copy_from_slice(&self.plist[trailer_start + 16..trailer_start + 24]);
        let mut offset_table_start = [0u8; 8];
        offset_table_start.copy_from_slice(&self.plist[trailer_start + 24..trailer_start + 32]);

        Ok(Trailer {
            offset_table_offset_size,
            object_ref_size,
            num_objects: u64::from_be_bytes(num_objects),
            top_object_offset: u64::from_be_bytes(top_object_offset),
            offset_table_start: u64::from_be_bytes(offset_table_start),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlistParseError {
    #[error("read error: {0}")]
    Read(#[from] PlistReadError),
    #[error("offset size is too large")]
    OffsetSizeTooLarge,
    #[error("no more objects")]
    NoMoreObjects,
    #[error("object pointer is out of bounds")]
    ObjectPtrOutOfBounds,
    #[error("unknown object type")]
    UnknownObjectType,
    #[error("int of {0} bytes is too large")]
    TooLargeInt(u8),
    #[error("real value of size {0} is not supported")]
    UnsupportedRealSize(u8),
    #[error("UID of {0} bytes is too large")]
    TooLargeUid(u8),
    #[error("expected int")]
    ExpectedInt,
    #[error("expected real")]
    ExpectedReal,
    #[error("out of bounds")]
    OutOfBounds,
    #[error("first object is out of bounds")]
    StartObjectOob,
    #[error("too many objects in list")]
    TooManyObjectsInList,
    #[error("too large number power")]
    TooLargeNumPow,
    #[error("invalid trailer")]
    InvalidTrailer,
}

#[derive(Debug)]
pub struct PlistParser<'a> {
    plist: &'a [u8],
    trailer: Trailer,
    object_idx: usize,
    parsed_objects: u64,
    object_end: usize,
}

impl<'a> PlistParser<'a> {
    pub fn new(plist: &'a [u8], trailer: Trailer) -> Result<Self, PlistParseError> {
        let (object_idx, overflowed) = trailer
            .offset_table_start
            .overflowing_add(trailer.top_object_offset);
        if overflowed || object_idx >= plist.len() as u64 {
            return Err(PlistParseError::StartObjectOob);
        }

        if trailer.offset_table_offset_size == 0
            || trailer.offset_table_offset_size > MAX_OFFSET_TABLE_OFFSET_SIZE
            || trailer.object_ref_size == 0
            || trailer.object_ref_size > MAX_OBJECT_REF_SIZE
            || trailer.offset_table_start < HEADER_SIZE
        {
            return Err(PlistParseError::InvalidTrailer);
        }

        if trailer.num_objects > MAX_OBJECTS_IN_LIST {
            return Err(PlistParseError::TooManyObjectsInList);
        }

        Ok(Self {
            plist,
            trailer,
            object_idx: object_idx as usize,
            parsed_objects: 0,
            object_end: 0,
        })
    }

    fn parse_object_ref(&self, idx: usize) -> Result<usize, PlistParseError> {
        let ref_size = self.trailer.object_ref_size;
        let mut idx_buf = [0u8; MAX_OFFSET_TABLE_OFFSET_SIZE];
        let bytes = &self
            .plist
            .get(idx..idx + ref_size)
            .ok_or(PlistParseError::OutOfBounds)?;
        for (i, b) in bytes.iter().enumerate() {
            idx_buf[MAX_OFFSET_TABLE_OFFSET_SIZE - bytes.len() + i] = *b;
        }

        Ok(u32::from_be_bytes(idx_buf) as usize)
    }

    fn object_idx_from_offset_table_idx(
        &mut self,
        offset_table_idx: usize,
    ) -> Result<usize, PlistParseError> {
        let offset_size = self.trailer.offset_table_offset_size;
        if offset_size > MAX_OFFSET_TABLE_OFFSET_SIZE {
            return Err(PlistParseError::OffsetSizeTooLarge);
        }

        // if offset_table_idx + offset_size > self.plist.len() + TRAILER_SIZE { // is trailer included or not?
        if offset_table_idx + offset_size > self.plist.len() {
            return Err(PlistParseError::NoMoreObjects);
        }

        let mut idx_buf = [0u8; MAX_OFFSET_TABLE_OFFSET_SIZE];
        let bytes = &self.plist[offset_table_idx..offset_table_idx + offset_size];
        for (i, b) in bytes.iter().enumerate() {
            idx_buf[MAX_OFFSET_TABLE_OFFSET_SIZE - bytes.len() + i] = *b;
        }

        Ok(u32::from_be_bytes(idx_buf) as usize)
    }

    fn next_object_idx(&mut self) -> Result<usize, PlistParseError> {
        let idx = self.object_idx_from_offset_table_idx(self.object_idx);
        self.object_idx += self.trailer.offset_table_offset_size;
        idx
    }

    fn marker(&self, idx: usize) -> (u8, u8) {
        let marker = self.plist[idx];
        (marker >> 4, marker & 0x0F)
    }

    fn parse_uid(&mut self, idx: usize) -> Result<Object, PlistParseError> {
        let (marker_hi, marker_lo) = self.marker(idx);
        if marker_hi != UID_MARKER {
            return Err(PlistParseError::UnknownObjectType);
        }
        let (n, overflowed) = 2u8.overflowing_pow(marker_lo as u32);
        if overflowed {
            return Err(PlistParseError::TooLargeNumPow);
        }
        let idx = idx + 1 + n as usize;
        self.object_end = idx;

        match n {
            I8_N_BYTES => Ok(Object::Uid(
                u8::from_be_bytes(read_n_bytes!(self, I8_N_BYTES, idx)) as u64,
            )),
            I16_N_BYTES => Ok(Object::Uid(
                u16::from_be_bytes(read_n_bytes!(self, I16_N_BYTES, idx)) as u64,
            )),
            I32_N_BYTES => Ok(Object::Uid(
                u32::from_be_bytes(read_n_bytes!(self, I32_N_BYTES, idx)) as u64,
            )),
            I64_N_BYTES => Ok(Object::Uid(u64::from_be_bytes(read_n_bytes!(
                self,
                I64_N_BYTES,
                idx
            )))),
            _ => Err(PlistParseError::TooLargeUid(n)),
        }
    }

    fn parse_int(&mut self, idx: usize) -> Result<Object, PlistParseError> {
        let (marker_hi, marker_lo) = self.marker(idx);
        if marker_hi != INT_MARKER {
            return Err(PlistParseError::ExpectedInt);
        } else if marker_lo >> 3 != 0 {
            return Err(PlistParseError::UnknownObjectType);
        }
        let (n, overflowed) = 2u8.overflowing_pow(marker_lo as u32);
        if overflowed {
            return Err(PlistParseError::TooLargeNumPow);
        }

        let idx = idx + 1 + n as usize;
        self.object_end = idx;
        if idx > self.plist.len() {
            return Err(PlistParseError::OutOfBounds);
        }
        // TODO: are ints actually signed? idk
        match n {
            I8_N_BYTES => Ok(Object::Int(
                u8::from_be_bytes(read_n_bytes!(self, I8_N_BYTES, idx)) as i64,
            )),
            I16_N_BYTES => Ok(Object::Int(
                u16::from_be_bytes(read_n_bytes!(self, I16_N_BYTES, idx)) as i64,
            )),
            I32_N_BYTES => Ok(Object::Int(
                u32::from_be_bytes(read_n_bytes!(self, I32_N_BYTES, idx)) as i64,
            )),
            I64_N_BYTES => Ok(Object::Int(
                u64::from_be_bytes(read_n_bytes!(self, I64_N_BYTES, idx)) as i64,
            )),
            _ => Err(PlistParseError::TooLargeInt(n)),
        }
    }

    fn parse_real(&mut self, idx: usize) -> Result<Object, PlistParseError> {
        let (marker_hi, marker_lo) = self.marker(idx);
        if marker_hi != REAL_MARKER {
            return Err(PlistParseError::ExpectedReal);
        } else if marker_lo >> 3 != 0 {
            return Err(PlistParseError::UnknownObjectType);
        }
        let (n, overflowed) = 2u8.overflowing_pow(marker_lo as u32);
        if overflowed {
            return Err(PlistParseError::TooLargeNumPow);
        }

        let idx = idx + 1 + n as usize;
        self.object_end = idx;
        if idx > self.plist.len() {
            return Err(PlistParseError::OutOfBounds);
        }
        match n {
            F32_N_BYTES => Ok(Object::Real(
                f32::from_be_bytes(read_n_bytes!(self, F32_N_BYTES, idx)) as f64,
            )),
            F64_N_BYTES => Ok(Object::Real(f64::from_be_bytes(read_n_bytes!(
                self,
                F64_N_BYTES,
                idx
            )))),
            _ => Err(PlistParseError::UnsupportedRealSize(n)),
        }
    }

    fn get_count(
        &mut self,
        nnnn: u8,
        read_offset: usize,
    ) -> Result<(usize, usize), PlistParseError> {
        Ok(match nnnn {
            0b1111 => {
                let count = self.parse_int(read_offset)?.try_int_to_usize()?;
                (count, self.object_end)
            }
            count => (count as usize, read_offset),
        })
    }

    fn parse_object(&mut self, object_idx: usize) -> Result<Object, PlistParseError> {
        self.parsed_objects += 1;
        let (marker_hi, marker_lo) = self.marker(object_idx);
        // Format description here: https://github.com/Apple-FOSS-Mirror/CF/blob/a9db511baa36b8a2b75b67a022efdadfae656633/CFBinaryPList.c#L221
        Ok(match (marker_hi, marker_lo) {
            (0b0000, 0b0000) => Object::Null,
            (0b0000, 0b1000) => Object::Bool(false),
            (0b0000, 0b1001) => Object::Bool(true),
            (0b0000, 0b1111) => Object::Fill,
            (INT_MARKER, nnn) => match nnn >> 3 {
                0b0 => self.parse_int(object_idx)?,
                _ => return Err(PlistParseError::UnknownObjectType),
            },
            (REAL_MARKER, nnn) => match nnn >> 3 {
                0b0 => self.parse_real(object_idx)?,
                _ => return Err(PlistParseError::UnknownObjectType),
            },
            (DATE_MARKER, DATE_MARKER) => {
                if object_idx + 1 > self.plist.len() {
                    return Err(PlistParseError::OutOfBounds);
                }
                Object::Date(f64::from_be_bytes(read_n_bytes!(
                    self,
                    F64_N_BYTES,
                    object_idx + 1
                )))
            }
            (DATA_MARKER, nnnn) => {
                let (count, start_offset) = self.get_count(nnnn, object_idx + 1)?;
                if start_offset + count > self.plist.len() {
                    return Err(PlistParseError::OutOfBounds);
                }
                Object::Data(self.plist[start_offset..start_offset + count].to_vec())
            }
            (ASCII_STR_MARKER, nnnn) => {
                let (count, start_offset) = self.get_count(nnnn, object_idx + 1)?;
                if start_offset + count > self.plist.len() {
                    return Err(PlistParseError::OutOfBounds);
                }
                Object::String(
                    String::from_utf8_lossy(&self.plist[start_offset..start_offset + count])
                        .to_string(),
                )
            }
            (UTF16_STR_MARKER, nnnn) => {
                let (count, start_offset) = self.get_count(nnnn, object_idx + 1)?;
                if start_offset + count * 2 > self.plist.len() {
                    return Err(PlistParseError::OutOfBounds);
                }
                let bytes = &self.plist[start_offset..start_offset + count * 2];
                Object::String(String::from_utf16_lossy(
                    &bytes
                        .chunks(2)
                        .map(|b| u16::from_be_bytes([b[0], b[1]]))
                        .collect::<Vec<u16>>(),
                ))
            }
            (UID_MARKER, _) => self.parse_uid(object_idx)?,
            (ARRAY_MARKER, nnnn) => {
                let (count, start_offset) = self.get_count(nnnn, object_idx + 1)?;
                let mut array = Vec::with_capacity(count);
                for i in 0..count {
                    let objref =
                        self.parse_object_ref(start_offset + i * self.trailer.object_ref_size)?;
                    let objidx = self.object_idx_from_offset_table_idx(
                        self.trailer.offset_table_start as usize
                            + objref * self.trailer.offset_table_offset_size,
                    )?;
                    array.push(self.parse_object(objidx)?);
                }
                Object::Array(array)
            }
            (DICT_MARKER, nnnn) => {
                let (count, start_offset) = self.get_count(nnnn, object_idx + 1)?;
                let mut dict = Vec::new();
                let (stride, overflowed) = count.overflowing_mul(self.trailer.object_ref_size);
                if overflowed {
                    return Err(PlistParseError::OutOfBounds);
                }
                for i in 0..count {
                    let keyref =
                        self.parse_object_ref(start_offset + i * self.trailer.object_ref_size)?;
                    let keyidx = self.object_idx_from_offset_table_idx(
                        self.trailer.offset_table_start as usize
                            + keyref * self.trailer.offset_table_offset_size,
                    )?;
                    let key = self.parse_object(keyidx)?;
                    let valref = self.parse_object_ref(
                        start_offset + stride + i * self.trailer.object_ref_size,
                    )?;
                    let validx = self.object_idx_from_offset_table_idx(
                        self.trailer.offset_table_start as usize
                            + valref * self.trailer.offset_table_offset_size,
                    )?;
                    let val = self.parse_object(validx)?;
                    dict.push((key, val));
                }

                Object::Dict(dict)
            }
            _ => return Err(PlistParseError::UnknownObjectType),
        })
    }

    fn parse_next_object(&mut self) -> Result<Object, PlistParseError> {
        let object_idx = self.next_object_idx()?;
        if object_idx >= self.plist.len() {
            return Err(PlistParseError::ObjectPtrOutOfBounds);
        }

        self.parse_object(object_idx)
    }

    pub fn parse(&mut self) -> Result<Vec<Object>, PlistParseError> {
        let mut objects = Vec::with_capacity(self.trailer.num_objects as usize);
        while self.parsed_objects < self.trailer.num_objects {
            objects.push(self.parse_next_object()?);
            self.parsed_objects += 1;
        }

        Ok(objects)
    }
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;

    macro_rules! parse {
        ($plist:expr) => {{
            let reader = PlistReader::new(&$plist);
            let trailer = reader.read_trailer().unwrap();
            let mut parser = PlistParser::new(&$plist, trailer).unwrap();
            parser.parse()
        }};
    }

    macro_rules! strify {
        ($str:expr) => {
            Object::String($str.to_owned())
        };
    }

    #[test]
    fn test_read_header() {
        {
            let mut reader = PlistReader::new(b"bplist00");
            reader.read_magic_number().unwrap();
            assert_eq!(reader.read_version().unwrap(), Version::Zero);
        }
    }

    #[test]
    fn test_parse_int() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps(123, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 10 7b 08 00 00 00 00 00 \
                00 01 01 00 00 00 00 00 00 00 01 00 00 00 00 00 \
                00 00 00 00 00 00 00 00 00 00 0a"
            ))
            .unwrap(),
            vec![Object::Int(123)],
        );
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps(60000, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 11 ea 60 08 00 00 00 00 \
                00 00 01 01 00 00 00 00 00 00 00 01 00 00 00 00 \
                00 00 00 00 00 00 00 00 00 00 00 0b"
            ))
            .unwrap(),
            vec![Object::Int(60000)],
        );
    }

    #[allow(clippy::approx_constant)]
    #[test]
    fn test_parse_real() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps(3.1415, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 23 40 09 21 ca c0 83 12 \
                6f 08 00 00 00 00 00 00 01 01 00 00 00 00 00 00 \
                00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
                00 11"
            ))
            .unwrap(),
            vec![Object::Real(3.1415)],
        );
    }

    #[test]
    fn test_parse_data() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps(b"abc", fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 43 61 62 63 08 00 00 00 \
                00 00 00 01 01 00 00 00 00 00 00 00 01 00 00 00 \
                00 00 00 00 00 00 00 00 00 00 00 00 0c"
            ))
            .unwrap(),
            vec![Object::Data(b"abc".to_vec())],
        );
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps(b"A"*100, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 4f 10 64 41 41 41 41 41 \
                41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 08 \
                00 00 00 00 00 00 01 01 00 00 00 00 00 00 00 01 \
                00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 6f"
            ))
            .unwrap(),
            vec![Object::Data(vec![b'A'; 100])],
        );
    }

    #[test]
    fn test_parse_array() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps([1, 2, 3], fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 a3 01 02 03 10 01 10 02 \
                10 03 08 0c 0e 10 00 00 00 00 00 00 01 01 00 00 \
                00 00 00 00 00 04 00 00 00 00 00 00 00 00 00 00 \
                00 00 00 00 00 12"
            ))
            .unwrap(),
            vec![Object::Array(vec![
                Object::Int(1),
                Object::Int(2),
                Object::Int(3)
            ])],
        );
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps([123]*100, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 af 10 64 01 01 01 01 01 \
                01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 \
                01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 \
                01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 \
                01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 \
                01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 \
                01 01 01 01 01 01 01 01 01 01 01 01 01 01 01 10 \
                7b 08 6f 00 00 00 00 00 00 01 01 00 00 00 00 00 \
                00 00 02 00 00 00 00 00 00 00 00 00 00 00 00 00 \
                00 00 71"
            ))
            .unwrap(),
            vec![Object::Array(vec![Object::Int(123); 100])],
        );
    }

    #[test]
    fn test_parse_ascii_string() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps("abc", fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 53 61 62 63 08 00 00 00 \
                 00 00 00 01 01 00 00 00 00 00 00 00 01 00 00 00 \
                 00 00 00 00 00 00 00 00 00 00 00 00 0c"
            ))
            .unwrap(),
            vec![strify!("abc")],
        );
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps("A"*100, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 5f 10 64 41 41 41 41 41 \
                 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 \
                 41 41 41 41 41 41 41 41 41 41 41 41 41 41 41 08 \
                 00 00 00 00 00 00 01 01 00 00 00 00 00 00 00 01 \
                 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 6f"
            ))
            .unwrap(),
            vec![Object::String(String::from_utf8(vec![b'A'; 100]).unwrap())],
        );
    }

    #[test]
    fn test_parse_utf16_string() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps("æøå", fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 63 00 e6 00 f8 00 e5 08 \
                 00 00 00 00 00 00 01 01 00 00 00 00 00 00 00 01 \
                 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 0f"
            ))
            .unwrap(),
            vec![strify!("æøå")],
        );
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps("Æ"*100, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 6f 10 64 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 c6 00 \
                 c6 00 c6 08 00 00 00 00 00 00 01 01 00 00 00 00 \
                 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 \
                 00 00 00 d3"
            ))
            .unwrap(),
            vec![Object::String(
                String::from_utf16(&[u16::from_be_bytes([0x00, 0xC6]); 100]).unwrap()
            )],
        );
    }

    #[test]
    fn test_parse_dict() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps({"a":"b"}, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 d1 01 02 51 61 51 62 08 \
                 0b 0d 00 00 00 00 00 00 01 01 00 00 00 00 00 00 \
                 00 03 00 00 00 00 00 00 00 00 00 00 00 00 00 00 \
                 00 0f"
            ))
            .unwrap(),
            vec![Object::Dict(vec![(strify!("a"), strify!("b"),)])],
        );
    }

    #[test]
    fn test_parse_nested_objects() {
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps({"a":["b", "c"]}, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 d1 01 02 51 61 a2 03 04 \
                 51 62 51 63 08 0b 0d 10 12 00 00 00 00 00 00 01 \
                 01 00 00 00 00 00 00 00 05 00 00 00 00 00 00 00 \
                 00 00 00 00 00 00 00 00 14"
            ))
            .unwrap(),
            vec![Object::Dict(vec![(
                strify!("a"),
                Object::Array(vec![strify!("b"), strify!("c"),])
            ),])],
        );
        // import plistlib; " ".join(f"{b:02x}" for b in plistlib.dumps({"a":[{"b": "c"}]}, fmt=plistlib.FMT_BINARY))
        assert_eq!(
            parse!(hex!(
                "62 70 6c 69 73 74 30 30 d1 01 02 51 61 a1 03 d1 \
                 04 05 51 62 51 63 08 0b 0d 0f 12 14 00 00 00 00 \
                 00 00 01 01 00 00 00 00 00 00 00 06 00 00 00 00 \
                 00 00 00 00 00 00 00 00 00 00 00 16"
            ))
            .unwrap(),
            vec![Object::Dict(vec![(
                strify!("a"),
                Object::Array(vec![Object::Dict(vec![(strify!("b"), strify!("c"))])])
            ),])],
        );
    }

    #[test]
    fn test_parse_info() {
        parse!(hex!(
            "62 70 6c 69 73 74 30 30 62 79 62 69 70 6c 69 73 \
            74 31 2e 30 de 01 02 03 04 05 06 07 08 09 0a 0b \
            0c 0d 0e 0f 10 11 12 12 13 14 15 16 17 18 19 1a \
            1b 5e 61 75 64 69 6f 4c 61 74 65 6e 63 69 65 73 \
            58 64 65 76 69 63 65 49 44 58 66 65 61 74 75 72 \
            65 73 5f 10 11 6b 65 65 70 41 6c 69 76 65 4c 6f \
            77 50 6f 77 65 72 5f 10 18 6b 65 65 70 41 6c 69 \
            76 65 53 65 6e 64 53 74 61 74 73 41 73 42 6f 64 \
            79 5c 6d 61 6e 75 66 61 63 74 75 72 65 72 55 6d \
            6f 64 65 6c 54 6e 61 6d 65 5f 10 14 6e 61 6d 65 \
            49 73 46 61 63 74 6f 72 79 44 65 66 61 75 6c 74 \
            52 70 69 5f 10 0f 70 72 6f 74 6f 63 6f 6c 56 65 \
            72 73 69 6f 6e 53 73 64 6b 5d 73 6f 75 72 63 65 \
            56 65 72 73 69 6f 6e 5b 73 74 61 74 75 73 46 6c \
            61 67 73 a1 1c d2 1d 1e 1f 20 5f 10 12 69 6e 70 \
            75 74 4c 61 74 65 6e 63 79 4d 69 63 72 6f 73 5f \
            10 13 6f 75 74 70 75 74 4c 61 74 65 6e 63 79 4d \
            69 63 72 6f 73 10 00 12 00 06 1a 80 5f 10 11 32 \
            38 3a 64 30 3a 65 61 3a 35 61 3a 61 32 3a 39 34 \
            13 00 01 c3 00 40 5f 42 00 09 5b 4f 70 65 6e 41 \
            69 72 70 6c 61 79 58 52 65 63 65 69 76 65 72 00 \
            08 5f 10 24 61 61 35 63 62 38 64 66 2d 37 66 31 \
            34 2d 34 32 34 39 2d 39 30 31 61 2d 35 65 37 34 \
            38 63 65 35 37 61 39 33 53 31 2e 31 5d 41 69 72 \
            50 6c 61 79 3b 32 2e 30 2e 32 55 33 36 36 2e 30 \
            53 30 78 34 00 14 00 31 00 40 00 49 00 52 00 66 \
            00 81 00 8e 00 94 00 99 00 b0 00 b3 00 c5 00 c9 \
            00 d7 00 e3 01 1c 01 30 01 39 01 3a 01 46 01 4f \
            01 50 01 51 01 78 01 7c 01 8a 01 90 00 e5 00 ea \
            00 ff 01 15 01 17 00 00 00 00 00 00 02 01 00 00 \
            00 00 00 00 00 21 00 00 00 00 00 00 00 00 00 00 \
            00 00 00 00 01 94"
        ))
        .unwrap();
        parse!(hex!(
            "62 70 6c 69 73 74 30 30 df 10 10 01 03 05 07 09 \
            0b 18 1f 21 23 25 27 29 2b 2d 2e 02 04 06 08 0a \
            0c 19 20 22 24 26 28 2a 2c 02 2f 58 64 65 76 69 \
            63 65 49 44 5f 10 11 33 38 3a 66 33 3a 61 62 3a \
            32 35 3a 36 36 3a 30 32 52 70 6b 4f 10 20 9f dd \
            c2 07 a6 f8 af 95 7f 71 25 9d 4a 75 d2 05 90 05 \
            f3 09 89 f9 27 9a ab 4c f8 03 d3 88 d8 15 5a 74 \
            78 74 41 69 72 50 6c 61 79 4f 10 d7 1a 64 65 76 \
            69 63 65 69 64 3d 33 38 3a 66 33 3a 61 62 3a 32 \
            35 3a 36 36 3a 30 32 17 66 65 61 74 75 72 65 73 \
            3d 30 78 35 32 37 46 46 45 46 37 2c 30 78 30 08 \
            70 77 3d 66 61 6c 73 65 09 66 6c 61 67 73 3d 30 \
            78 34 10 6d 6f 64 65 6c 3d 41 70 70 6c 65 54 56 \
            33 2c 32 43 70 6b 3d 39 66 64 64 63 32 30 37 61 \
            36 66 38 61 66 39 35 37 66 37 31 32 35 39 64 34 \
            61 37 35 64 32 30 35 39 30 30 35 66 33 30 39 38 \
            39 66 39 32 37 39 61 61 62 34 63 66 38 30 33 64 \
            33 38 38 64 38 31 35 27 70 69 3d 32 65 33 38 38 \
            30 30 36 2d 31 33 62 61 2d 34 30 34 31 2d 39 61 \
            36 37 2d 32 35 64 64 34 61 34 33 64 35 33 36 0e \
            73 72 63 76 65 72 73 3d 32 32 30 2e 36 38 04 76 \
            76 3d 32 58 66 65 61 74 75 72 65 73 12 52 7f fe \
            f7 54 6e 61 6d 65 5c 55 78 50 6c 61 79 40 6d 61 \
            72 6b 73 5e 61 75 64 69 6f 4c 61 74 65 6e 63 69 \
            65 73 a2 0d 16 d4 0e 10 12 14 0f 11 13 15 54 74 \
            79 70 65 10 64 5f 10 12 69 6e 70 75 74 4c 61 74 \
            65 6e 63 79 4d 69 63 72 6f 73 10 00 59 61 75 64 \
            69 6f 54 79 70 65 57 64 65 66 61 75 6c 74 5f 10 \
            13 6f 75 74 70 75 74 4c 61 74 65 6e 63 79 4d 69 \
            63 72 6f 73 08 d4 0e 12 10 14 17 13 11 15 10 65 \
            5c 61 75 64 69 6f 46 6f 72 6d 61 74 73 a2 1a 1e \
            d3 1b 0e 1d 1c 0f 1c 5f 10 12 61 75 64 69 6f 4f \
            75 74 70 75 74 46 6f 72 6d 61 74 73 12 03 ff ff \
            fc 5f 10 11 61 75 64 69 6f 49 6e 70 75 74 46 6f \
            72 6d 61 74 73 d3 1b 0e 1d 1c 17 1c 52 70 69 5f \
            10 24 32 65 33 38 38 30 30 36 2d 31 33 62 61 2d \
            34 30 34 31 2d 39 61 36 37 2d 32 35 64 64 34 61 \
            34 33 64 35 33 36 52 76 76 10 02 5b 73 74 61 74 \
            75 73 46 6c 61 67 73 10 44 5f 10 11 6b 65 65 70 \
            41 6c 69 76 65 4c 6f 77 50 6f 77 65 72 10 01 5d \
            73 6f 75 72 63 65 56 65 72 73 69 6f 6e 56 32 32 \
            30 2e 36 38 5f 10 18 6b 65 65 70 41 6c 69 76 65 \
            53 65 6e 64 53 74 61 74 73 41 73 42 6f 64 79 09 \
            55 6d 6f 64 65 6c 5a 41 70 70 6c 65 54 56 33 2c \
            32 5a 6d 61 63 41 64 64 72 65 73 73 58 64 69 73 \
            70 6c 61 79 73 a1 30 dc 31 33 34 35 37 39 3a 3b \
            3c 3e 40 07 32 11 11 36 38 36 38 15 3d 3f 15 41 \
            54 75 75 69 64 5f 10 24 65 30 66 66 38 61 32 37 \
            2d 36 37 33 38 2d 33 64 35 36 2d 38 61 31 36 2d \
            63 63 35 33 61 61 63 65 65 39 32 35 5d 77 69 64 \
            74 68 50 68 79 73 69 63 61 6c 5e 68 65 69 67 68 \
            74 50 68 79 73 69 63 61 6c 55 77 69 64 74 68 11 \
            07 80 56 68 65 69 67 68 74 11 04 38 5b 77 69 64 \
            74 68 50 69 78 65 6c 73 5c 68 65 69 67 68 74 50 \
            69 78 65 6c 73 58 72 6f 74 61 74 69 6f 6e 5b 72 \
            65 66 72 65 73 68 52 61 74 65 23 3f 91 11 11 11 \
            11 11 11 56 6d 61 78 46 50 53 10 1e 5b 6f 76 65 \
            72 73 63 61 6e 6e 65 64 10 0e 00 08 00 2b 00 34 \
            00 48 00 4b 00 6e 00 79 01 53 01 5c 01 61 01 66 \
            01 73 01 82 01 85 01 8e 01 93 01 95 01 aa 01 ac \
            01 b6 01 be 01 d4 01 d5 01 de 01 e0 01 ed 01 f0 \
            01 f7 02 0c 02 11 02 25 02 2c 02 2f 02 56 02 59 \
            02 5b 02 67 02 69 02 7d 02 7f 02 8d 02 94 02 af \
            02 b0 02 b6 02 c1 02 cc 02 d5 02 d7 02 f0 02 f5 \
            03 1c 03 2a 03 39 03 3f 03 42 03 49 03 4c 03 58 \
            03 65 03 6e 03 7a 03 83 03 8a 03 8c 03 98 00 00 \
            00 00 00 00 02 01 00 00 00 00 00 00 00 42 00 00 \
            00 00 00 00 00 00 00 00 00 00 00 00 03 9a"
        ))
        .unwrap();
    }
}
