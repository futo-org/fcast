use crate::proto::FormatId;

/// Identity of a format: the `(itag, lmt, xtags)` triple keyed on everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SabrFormatKey {
    pub itag: i32,
    pub last_modified: u64,
    pub xtags: String,
}

impl SabrFormatKey {
    pub fn new(itag: i32, last_modified: u64, xtags: impl Into<String>) -> Self {
        Self {
            itag,
            last_modified,
            xtags: xtags.into(),
        }
    }

    pub fn of(itag: i32, lmt: u64, xtags: Option<&str>) -> Self {
        Self::new(itag, lmt, xtags.unwrap_or("").to_owned())
    }
}

/// A selectable audio or video format.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SabrFormat {
    pub itag: i32,
    pub last_modified: u64,
    pub xtags: String,
    pub mime_type: String,
    pub codecs: String,
    pub bitrate: i32,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub audio_channels: i32,
    pub audio_sample_rate: i32,
    pub language: Option<String>,
    pub is_original_audio: bool,
    pub is_drc: bool,
}

impl SabrFormat {
    pub fn key(&self) -> SabrFormatKey {
        SabrFormatKey::new(self.itag, self.last_modified, self.xtags.clone())
    }

    pub fn is_video(&self) -> bool {
        self.mime_type.starts_with("video/")
    }

    pub fn is_audio(&self) -> bool {
        self.mime_type.starts_with("audio/")
    }

    /// The container mime type, e.g. `video/mp4`, with any `;codecs=...` stripped.
    pub fn container_mime_type(&self) -> String {
        self.mime_type
            .split(';')
            .next()
            .unwrap_or(&self.mime_type)
            .trim()
            .to_owned()
    }

    pub fn to_format_id(&self) -> FormatId {
        FormatId {
            itag: self.itag,
            lmt: self.last_modified,
            xtags: self.xtags.clone(),
        }
    }
}

impl PartialEq for SabrFormat {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for SabrFormat {}

/// Codec-name / sample-mime helpers.
pub mod codecs {
    /// Short human-readable codec name (e.g. `H.264`, `AV1`, `Opus`).
    pub fn codec_name(codecs: &str) -> String {
        let c = codecs.to_ascii_lowercase();
        let name = if c.starts_with("avc1") || c.starts_with("avc3") {
            "H.264"
        } else if c.starts_with("hev1") || c.starts_with("hvc1") {
            "H.265"
        } else if c.starts_with("av01") {
            "AV1"
        } else if c.starts_with("vp9") || c.starts_with("vp09") {
            "VP9"
        } else if c.starts_with("vp8") || c.starts_with("vp08") {
            "VP8"
        } else if c.starts_with("mp4a") {
            "AAC"
        } else if c.starts_with("opus") {
            "Opus"
        } else if c.starts_with("vorbis") {
            "Vorbis"
        } else if c.starts_with("ec-3") {
            "EAC3"
        } else if c.starts_with("ac-3") {
            "AC3"
        } else {
            return codecs.split('.').next().unwrap_or(codecs).trim().to_owned();
        };
        name.to_owned()
    }

    /// Android-style sample mime type for the codec, or `None` if unknown.
    pub fn sample_mime_type(codecs: &str) -> Option<&'static str> {
        let c = codecs.to_ascii_lowercase();
        let mime = if c.starts_with("avc1") || c.starts_with("avc3") {
            "video/avc"
        } else if c.starts_with("hev1") || c.starts_with("hvc1") {
            "video/hevc"
        } else if c.starts_with("av01") {
            "video/av01"
        } else if c.starts_with("vp9") || c.starts_with("vp09") {
            "video/x-vnd.on2.vp9"
        } else if c.starts_with("vp8") || c.starts_with("vp08") {
            "video/x-vnd.on2.vp8"
        } else if c.starts_with("mp4a") {
            "audio/mp4a-latm"
        } else if c.starts_with("opus") {
            "audio/opus"
        } else if c.starts_with("vorbis") {
            "audio/vorbis"
        } else if c.starts_with("ec-3") {
            "audio/eac3"
        } else if c.starts_with("ac-3") {
            "audio/ac3"
        } else {
            return None;
        };
        Some(mime)
    }
}
