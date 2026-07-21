use std::collections::HashSet;

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Container {
    Ogg,
    Hls,
    Dash,
    Flv,
    Mp4,
    Quicktime,
    Mkv,
    Webm,
    MpegTs,
    Avi,
    Wav,
}

impl Container {
    pub fn to_str(&self) -> &'static str {
        match self {
            Container::Ogg => "ogg",
            Container::Hls => "hls",
            Container::Dash => "dash",
            Container::Flv => "flv",
            Container::Mp4 => "mp4",
            Container::Quicktime => "quicktime",
            Container::Mkv => "mkv",
            Container::Webm => "webm",
            Container::MpegTs => "mpegts",
            Container::Avi => "avi",
            Container::Wav => "wav",
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
            Video::Vp8 => "vp8",
            Video::Vp9 => "vp9",
            Video::Av1 => "av1",
            Video::H264 => "h264",
            Video::H265 => "h265",
            Video::Theora => "theora",
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub enum Audio {
    Flac,
    Ac3,
    Eac3,
    Dts,
    Opus,
    Vorbis,
    WavPack,
    Mpeg,
    Aac,
    Pcm,
}

impl Audio {
    pub fn to_str(&self) -> &'static str {
        match self {
            Audio::Flac => "flac",
            Audio::Ac3 => "ac3",
            Audio::Eac3 => "eac3",
            Audio::Dts => "dts",
            Audio::Opus => "opus",
            Audio::Vorbis => "vorbis",
            Audio::WavPack => "wavpack",
            Audio::Mpeg => "mp3",
            Audio::Aac => "aac",
            Audio::Pcm => "pcm",
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
            Subtitle::Ass => "ass",
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
    Sabr,
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
            Protocol::Sabr => "sabr",
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
    pub fn to_str(&self) -> &'static str {
        match self {
            Image::ImageLib(fmt) => match fmt {
                image::ImageFormat::Png => "png",
                image::ImageFormat::Jpeg => "jpeg",
                image::ImageFormat::Gif => "gif",
                image::ImageFormat::WebP => "webp",
                image::ImageFormat::Pnm => "pnm",
                image::ImageFormat::Tiff => "tiff",
                image::ImageFormat::Tga => "tga",
                image::ImageFormat::Dds => "dds",
                image::ImageFormat::Bmp => "bmp",
                image::ImageFormat::Ico => "ico",
                image::ImageFormat::Hdr => "radiance-hdr",
                image::ImageFormat::OpenExr => "exr",
                image::ImageFormat::Farbfeld => "farbfeld",
                image::ImageFormat::Avif => "avif",
                image::ImageFormat::Qoi => "qoi",
                _ => "unknown",
            },
            Image::JpegXl => "jxl",
            Image::Jpeg2000 => "jp2",
            #[cfg(all(feature = "extra-imgfmt", target_os = "linux"))]
            Image::Heif => "heif",
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
