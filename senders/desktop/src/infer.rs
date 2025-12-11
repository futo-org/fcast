// Derived from https://github.com/bojand/infer to remove all unneeded features and improve performance
//
// MIT License
//
// Copyright (c) 2019 Bojan

use core::convert::TryInto;

type InferenceBuffer<'a> = &'a [u8; 64];

pub fn is_mp3(_len: usize, buf: InferenceBuffer) -> bool {
    (buf[0] == 0x49 && buf[1] == 0x44 && buf[2] == 0x33) // ID3v2
			// Final bit (has crc32) may be or may not be set.
			|| (buf[0] == 0xFF && buf[1] == 0xFB)
}

pub fn is_m4a(_len: usize, buf: InferenceBuffer) -> bool {
    (buf[4] == 0x66
        && buf[5] == 0x74
        && buf[6] == 0x79
        && buf[7] == 0x70
        && buf[8] == 0x4D
        && buf[9] == 0x34
        && buf[10] == 0x41)
        || (buf[0] == 0x4D && buf[1] == 0x34 && buf[2] == 0x41 && buf[3] == 0x20)
}

pub fn is_ogg(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x4F && buf[1] == 0x67 && buf[2] == 0x67 && buf[3] == 0x53
}

pub fn is_flac(_len: usize, buf: InferenceBuffer) -> bool {
    buf.len() > 3 && buf[0] == 0x66 && buf[1] == 0x4C && buf[2] == 0x61 && buf[3] == 0x43
}

pub fn is_wav(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x52
        && buf[1] == 0x49
        && buf[2] == 0x46
        && buf[3] == 0x46
        && buf[8] == 0x57
        && buf[9] == 0x41
        && buf[10] == 0x56
        && buf[11] == 0x45
}

pub fn is_aac(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0xFF && (buf[1] == 0xF1 || buf[1] == 0xF9)
}

pub fn is_jpeg(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0xFF && buf[1] == 0xD8 && buf[2] == 0xFF
}

pub fn is_jpeg2000(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x0
        && buf[1] == 0x0
        && buf[2] == 0x0
        && buf[3] == 0xC
        && buf[4] == 0x6A
        && buf[5] == 0x50
        && buf[6] == 0x20
        && buf[7] == 0x20
        && buf[8] == 0xD
        && buf[9] == 0xA
        && buf[10] == 0x87
        && buf[11] == 0xA
        && buf[12] == 0x0
}

pub fn is_png(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x89 && buf[1] == 0x50 && buf[2] == 0x4E && buf[3] == 0x47
}

pub fn is_gif(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x47 && buf[1] == 0x49 && buf[2] == 0x46
}

pub fn is_webp(_len: usize, buf: InferenceBuffer) -> bool {
    buf[8] == 0x57 && buf[9] == 0x45 && buf[10] == 0x42 && buf[11] == 0x50
}

pub fn is_bmp(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x42 && buf[1] == 0x4D
}

pub fn is_jxr(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x49 && buf[1] == 0x49 && buf[2] == 0xBC
}

pub fn is_ico(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x00 && buf[1] == 0x00 && buf[2] == 0x01 && buf[3] == 0x00
}

pub fn is_jxl(_len: usize, buf: InferenceBuffer) -> bool {
    (buf[0] == 0xFF && buf[1] == 0x0A)
        || (buf[0] == 0x0
            && buf[1] == 0x0
            && buf[2] == 0x0
            && buf[3] == 0x0C
            && buf[4] == 0x4A
            && buf[5] == 0x58
            && buf[6] == 0x4C
            && buf[7] == 0x20
            && buf[8] == 0x0D
            && buf[9] == 0x0A
            && buf[10] == 0x87
            && buf[11] == 0x0A)
}

pub fn is_avif(len: usize, buf: InferenceBuffer) -> bool {
    if buf.is_empty() {
        return false;
    }

    if !is_isobmff(len, buf) {
        return false;
    }

    if let Some((major, _minor, compatible)) = get_ftyp(len, buf) {
        if major == b"avif" || major == b"avis" {
            return true;
        }

        for b in compatible {
            if b == b"avif" || b == b"avis" {
                return true;
            }
        }
    }

    false
}

// IsISOBMFF checks whether the given buffer represents ISO Base Media File Format data
fn is_isobmff(len: usize, buf: InferenceBuffer) -> bool {
    if len < 16 {
        return false;
    }

    if &buf[4..8] != b"ftyp" {
        return false;
    }

    let ftyp_length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    len >= ftyp_length
}

/// GetFtyp returns the major brand, minor version and compatible brands of the ISO-BMFF data
fn get_ftyp(
    _len: usize,
    buf: &[u8; 64],
) -> Option<(&[u8], &[u8], impl Iterator<Item = &[u8]>)> {
    if buf.len() < 16 {
        return None;
    }

    let ftyp_length = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;

    let major = &buf[8..12];
    let minor = &buf[12..16];
    let compatible = buf[16..]
        .chunks_exact(4)
        .take((ftyp_length / 4).saturating_sub(16 / 4));

    Some((major, minor, compatible))
}

pub fn is_m4v(_len: usize, buf: InferenceBuffer) -> bool {
    buf[4] == 0x66
        && buf[5] == 0x74
        && buf[6] == 0x79
        && buf[7] == 0x70
        && buf[8] == 0x4D
        && buf[9] == 0x34
        && buf[10] == 0x56
}

pub fn is_mkv(_len: usize, buf: InferenceBuffer) -> bool {
    (buf[0] == 0x1A
        && buf[1] == 0x45
        && buf[2] == 0xDF
        && buf[3] == 0xA3
        && buf[4] == 0x93
        && buf[5] == 0x42
        && buf[6] == 0x82
        && buf[7] == 0x88
        && buf[8] == 0x6D
        && buf[9] == 0x61
        && buf[10] == 0x74
        && buf[11] == 0x72
        && buf[12] == 0x6F
        && buf[13] == 0x73
        && buf[14] == 0x6B
        && buf[15] == 0x61)
        || (buf[31] == 0x6D
            && buf[32] == 0x61
            && buf[33] == 0x74
            && buf[34] == 0x72
            && buf[35] == 0x6f
            && buf[36] == 0x73
            && buf[37] == 0x6B
            && buf[38] == 0x61)
}

pub fn is_webm(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x1A && buf[1] == 0x45 && buf[2] == 0xDF && buf[3] == 0xA3
}

pub fn is_mov(_len: usize, buf: InferenceBuffer) -> bool {
    ((buf[4] == b'f' && buf[5] == b't' && buf[6] == b'y' && buf[7] == b'p')
        && (buf[8] == b'q' && buf[9] == b't' && buf[10] == b' ' && buf[11] == b' '))
        || (buf[4] == 0x6d && buf[5] == 0x6f && buf[6] == 0x6f && buf[7] == 0x76)
        || (buf[4] == 0x6d && buf[5] == 0x64 && buf[6] == 0x61 && buf[7] == 0x74)
        || (buf[12] == 0x6d && buf[13] == 0x64 && buf[14] == 0x61 && buf[15] == 0x74)
}

pub fn is_avi(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x52
        && buf[1] == 0x49
        && buf[2] == 0x46
        && buf[3] == 0x46
        && buf[8] == 0x41
        && buf[9] == 0x56
        && buf[10] == 0x49
}

pub fn is_mpeg(_len: usize, buf: InferenceBuffer) -> bool {
    buf[0] == 0x0 && buf[1] == 0x0 && buf[2] == 0x1 && buf[3] >= 0xb0 && buf[3] <= 0xbf
}

pub fn is_mp4(_len: usize, buf: InferenceBuffer) -> bool {
    (buf[4] == b'f' && buf[5] == b't' && buf[6] == b'y' && buf[7] == b'p')
        && ((buf[8] == b'a' && buf[9] == b'v' && buf[10] == b'c' && buf[11] == b'1')
            || (buf[8] == b'd' && buf[9] == b'a' && buf[10] == b's' && buf[11] == b'h')
            || (buf[8] == b'i' && buf[9] == b's' && buf[10] == b'o' && buf[11] == b'2')
            || (buf[8] == b'i' && buf[9] == b's' && buf[10] == b'o' && buf[11] == b'3')
            || (buf[8] == b'i' && buf[9] == b's' && buf[10] == b'o' && buf[11] == b'4')
            || (buf[8] == b'i' && buf[9] == b's' && buf[10] == b'o' && buf[11] == b'5')
            || (buf[8] == b'i' && buf[9] == b's' && buf[10] == b'o' && buf[11] == b'6')
            || (buf[8] == b'i' && buf[9] == b's' && buf[10] == b'o' && buf[11] == b'm')
            || (buf[8] == b'm' && buf[9] == b'm' && buf[10] == b'p' && buf[11] == b'4')
            || (buf[8] == b'm' && buf[9] == b'p' && buf[10] == b'4' && buf[11] == b'1')
            || (buf[8] == b'm' && buf[9] == b'p' && buf[10] == b'4' && buf[11] == b'2')
            || (buf[8] == b'm' && buf[9] == b'p' && buf[10] == b'4' && buf[11] == b'v')
            || (buf[8] == b'm' && buf[9] == b'p' && buf[10] == b'7' && buf[11] == b'1')
            || (buf[8] == b'M' && buf[9] == b'S' && buf[10] == b'N' && buf[11] == b'V')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'A' && buf[11] == b'S')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'S' && buf[11] == b'C')
            || (buf[8] == b'N' && buf[9] == b'S' && buf[10] == b'D' && buf[11] == b'C')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'S' && buf[11] == b'H')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'S' && buf[11] == b'M')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'S' && buf[11] == b'P')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'S' && buf[11] == b'S')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'X' && buf[11] == b'C')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'X' && buf[11] == b'H')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'X' && buf[11] == b'M')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'X' && buf[11] == b'P')
            || (buf[8] == b'N' && buf[9] == b'D' && buf[10] == b'X' && buf[11] == b'S')
            || (buf[8] == b'F' && buf[9] == b'4' && buf[10] == b'V' && buf[11] == b' ')
            || (buf[8] == b'F' && buf[9] == b'4' && buf[10] == b'P' && buf[11] == b' '))
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum MatcherType {
    Audio,
    Image,
    Video,
}

pub type Matcher = fn(len: usize, buf: InferenceBuffer) -> bool;

// This is needed until function pointers can be used in `const fn`.
// See trick and discussion at https://github.com/rust-lang/rust/issues/63997#issuecomment-616666309
#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct WrapMatcher(pub Matcher);

#[derive(Copy, Clone)]
pub struct Type {
    pub mime_type: &'static str,
    matcher: WrapMatcher,
}

impl Type {
    pub(crate) const fn new_static(mime_type: &'static str, matcher: WrapMatcher) -> Self {
        Self { mime_type, matcher }
    }
}

macro_rules! matcher_map {
    ($(($mime_type:literal, $matcher:expr)),*) => {
        pub const MATCHER_MAP: &[Type] = &[
            $(Type::new_static($mime_type, WrapMatcher($matcher)),)*
        ];
    };
}

matcher_map!(
    // Image
    ("image/jpeg", is_jpeg),
    ("image/jp2", is_jpeg2000),
    ("image/png", is_png),
    ("image/gif", is_gif),
    ("image/webp", is_webp),
    ("image/bmp", is_bmp),
    ("image/vnd.microsoft.icon", is_ico),
    ("image/avif", is_avif),
    ("image/jxl", is_jxl),
    // Video
    ("video/mp4", is_mp4),
    ("video/x-m4v", is_m4v),
    ("video/x-matroska", is_mkv),
    ("video/webm", is_webm),
    ("video/quicktime", is_mov),
    ("video/x-msvideo", is_avi),
    ("video/mpeg", is_mpeg),
    // Audio
    ("audio/mpeg", is_mp3),
    ("audio/m4a", is_m4a),
    ("audio/ogg", is_ogg),
    ("audio/flac", is_flac),
    ("audio/wav", is_wav),
    ("audio/aac", is_aac)
);

pub fn infer_type(len: usize, buf: InferenceBuffer) -> Option<Type> {
    MATCHER_MAP
        .iter()
        .find(|kind| (kind.matcher.0)(len, buf))
        .copied()
}
