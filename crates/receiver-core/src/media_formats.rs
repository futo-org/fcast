use std::collections::HashSet;

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Container {
    Ogg,
    Hls,
    Dash,
    Flv,
    Mp4V,
    Mp4A,
    Quicktime,
    Mkv,
    Mka,
    WebmV,
    WebmA,
    MpegTs,
}

impl Container {
    pub fn to_str(&self) -> &'static str {
        match self {
            Container::Ogg => "application/ogg",
            Container::Hls => "application/vnd.apple.mpegurl",
            Container::Dash => "application/dash+xml",
            Container::Flv => "video/x-flv",
            Container::Mp4V => "video/mp4",
            Container::Mp4A => "audio/mp4",
            Container::Quicktime => "video/quicktime",
            Container::Mkv => "video/matroska",
            Container::Mka => "audio/matroska",
            Container::WebmV => "video/webm",
            Container::WebmA => "audio/webm",
            Container::MpegTs => "video/MP2T",
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Video {
    Vp8,
    Vp9,
    Av1,
    H264,
    H265,
    Theora,
}

impl Video {
    pub fn to_str(&self) -> &'static str {
        match self {
            Video::Vp8 => "video/VP8",
            Video::Vp9 => "video/VP9",
            Video::Av1 => "video/AV1",
            Video::H264 => "avc",
            Video::H265 => "hevc",
            Video::Theora => "theora",
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Audio {
    Flac,
    Ac3,
    Opus,
    Vorbis,
    WavPack,
    Mpeg,
    Aac,
}

impl Audio {
    pub fn to_str(&self) -> &'static str {
        match self {
            Audio::Flac => "audio/flac",
            Audio::Ac3 => "audio/ac3",
            Audio::Opus => "audio/opus",
            Audio::Vorbis => "audio/vorbis",
            Audio::WavPack => "audio/x-wavpack",
            Audio::Mpeg => "audio/mpeg",
            Audio::Aac => "audio/aac",
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Subtitle {
    Dvd,
    Dvb,
    Pgs,
    Ssa,
    Ass,
    Srt,
    Vtt,
    Ttml,
}

impl Subtitle {
    pub fn to_str(&self) -> &'static str {
        match self {
            Subtitle::Dvd => "dvd",
            Subtitle::Dvb => "dvb",
            Subtitle::Pgs => "pgs",
            Subtitle::Ssa => "ssa",
            Subtitle::Ass => "aas",
            Subtitle::Srt => "srt",
            Subtitle::Vtt => "vtt",
            Subtitle::Ttml => "ttml",
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Https,
    Rtmp,
    Data,
    Rtsp,
    Srt,
    Whep,
}

impl Protocol {
    pub fn to_str(&self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Https => "https",
            Protocol::Rtmp => "rtmp",
            Protocol::Data => "data",
            Protocol::Rtsp => "rtsp",
            Protocol::Srt => "srt",
            Protocol::Whep => "whep",
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Image {
    ImageLib(image::ImageFormat),
    JpegXl,
    Jpeg2000,
    #[cfg(all(feature = "extra-imgfmt", target_os = "linux"))]
    Heif,
}

impl Image {
    pub fn mime_type(&self) -> &'static str {
        match self {
            Image::ImageLib(fmt) => fmt.to_mime_type(),
            Image::JpegXl => "image/jxl",
            Image::Jpeg2000 => "image/jp2",
            #[cfg(all(feature = "extra-imgfmt", target_os = "linux"))]
            Image::Heif => "image/heif",
        }
    }
}

#[derive(Debug)]
pub enum Hdr {
    Hdr10,
    Hdr10Plus,
    DoVi,
}

impl Hdr {
    pub fn to_str(&self) -> &'static str {
        match self {
            Hdr::Hdr10 => "hdr10",
            Hdr::Hdr10Plus => "hdr10+",
            Hdr::DoVi => "dolby-vision",
        }
    }
}

#[derive(Debug)]
pub struct SupportedFormats {
    pub containers: HashSet<Container>,
    pub videos: HashSet<Video>,
    pub audios: HashSet<Audio>,
    pub subtitles: HashSet<Subtitle>,
    pub protocols: HashSet<Protocol>,
    pub images: HashSet<Image>,
    pub hdrs: HashSet<Hdr>,
}

impl SupportedFormats {
    pub fn get_all() -> Self {
        let (containers, videos, audios, subtitles, protocols) = crate::gstreamer::find_formats();
        let images = crate::image::find_formats();

        Self {
            containers,
            videos,
            audios,
            subtitles,
            protocols,
            images,
            hdrs: HashSet::new(),
        }
    }
}
