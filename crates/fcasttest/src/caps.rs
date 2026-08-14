//! Caps, URI and stream-id conventions shared by every fcasttest element.

/// Encoded media types owned by the harness. Real decoders never claim them, so
/// ftestdec can register at PRIMARY rank without shadowing anything.
pub const VIDEO_MEDIA_TYPE: &str = "video/x-fcasttest";
pub const AUDIO_MEDIA_TYPE: &str = "audio/x-fcasttest";
/// Text streams leave ftestsrc already parsed, so no parser or decoder is
/// plugged.
pub const TEXT_MEDIA_TYPE: &str = "text/x-raw";

pub const RAW_VIDEO_FORMAT: gst_video::VideoFormat = gst_video::VideoFormat::I420;
pub const RAW_VIDEO_WIDTH: i32 = 16;
pub const RAW_VIDEO_HEIGHT: i32 = 16;

pub const RAW_AUDIO_FORMAT: gst_audio::AudioFormat = gst_audio::AudioFormat::S16le;
pub const RAW_AUDIO_RATE: i32 = 48000;
pub const RAW_AUDIO_CHANNELS: i32 = 2;

/// URI scheme handled by ftestsrc. The host part is the scenario registry key.
pub const URI_SCHEME: &str = "ftest";

/// Marker that starts every fcasttest stream-id: `ftest-<key>-<suffix>`.
pub const STREAM_ID_MARKER: &str = "ftest-";

pub fn video_caps() -> gst::Caps {
    gst::Caps::new_empty_simple(VIDEO_MEDIA_TYPE)
}

pub fn video_caps_at(width: i32, height: i32, framerate: gst::Fraction) -> gst::Caps {
    gst::Caps::builder(VIDEO_MEDIA_TYPE)
        .field("width", width)
        .field("height", height)
        .field("framerate", framerate)
        .build()
}

pub fn audio_caps() -> gst::Caps {
    gst::Caps::new_empty_simple(AUDIO_MEDIA_TYPE)
}

pub fn audio_caps_at(rate: i32, channels: i32) -> gst::Caps {
    gst::Caps::builder(AUDIO_MEDIA_TYPE)
        .field("rate", rate)
        .field("channels", channels)
        .build()
}

pub fn text_caps() -> gst::Caps {
    gst::Caps::builder(TEXT_MEDIA_TYPE)
        .field("format", "utf8")
        .build()
}

/// DVD subpicture (VOBSUB) bitmap subtitles. `subpicture/x-dvd` is in
/// decodebin3's default raw caps, so decodebin3 exposes it directly rather
/// than hunting for a decoder, the production shape for bitmap subtitles.
pub fn subpicture_caps() -> gst::Caps {
    gst::Caps::new_empty_simple("subpicture/x-dvd")
}

/// Blu-ray Presentation Graphic Stream subtitles.
pub fn pgs_caps() -> gst::Caps {
    gst::Caps::new_empty_simple("subpicture/x-pgs")
}

/// DVB subtitles (ETSI EN 300 743).
pub fn dvb_caps() -> gst::Caps {
    gst::Caps::new_empty_simple("subpicture/x-dvb")
}

/// VOBSUB carrying its palette out of band. The `.idx` text reaches the
/// driver as the caps' `codec_data`, as a container delivers it.
pub fn vobsub_caps(codec_data: &[u8]) -> gst::Caps {
    gst::Caps::builder("subpicture/x-dvd")
        .field("codec_data", gst::Buffer::from_slice(codec_data.to_vec()))
        .build()
}

/// Raw ASS/SSA as `text/x-raw` in a format no renderer here speaks. Stands
/// for the case where nothing parsed the subtitle. Classified as text,
/// routed as text, still unrenderable.
pub fn raw_ass_text_caps() -> gst::Caps {
    gst::Caps::builder(TEXT_MEDIA_TYPE)
        .field("format", "ass")
        .build()
}

/// Everything a `text_%u` pad may carry: any `text/x-raw` format plus the
/// three bitmap-subtitle media types above. Must stay a superset of every
/// caps a stream spec can override to, or the pad refuses them. The
/// structures are empty, which admits any fields, so a `codec_data`-bearing
/// override ([`vobsub_caps`]) passes the same template.
pub fn text_template_caps() -> gst::Caps {
    gst::Caps::builder_full()
        .structure(gst::Structure::new_empty(TEXT_MEDIA_TYPE))
        .structure(gst::Structure::new_empty("subpicture/x-dvd"))
        .structure(gst::Structure::new_empty("subpicture/x-pgs"))
        .structure(gst::Structure::new_empty("subpicture/x-dvb"))
        .build()
}

/// Raw video template caps. Framerate stays unfixed.
pub fn raw_video_caps() -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format(RAW_VIDEO_FORMAT)
        .width(RAW_VIDEO_WIDTH)
        .height(RAW_VIDEO_HEIGHT)
        .build()
}

pub fn raw_video_caps_at(framerate: gst::Fraction) -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format(RAW_VIDEO_FORMAT)
        .width(RAW_VIDEO_WIDTH)
        .height(RAW_VIDEO_HEIGHT)
        .framerate(framerate)
        .build()
}

/// Raw audio template caps. Rate stays unfixed.
pub fn raw_audio_caps() -> gst::Caps {
    gst_audio::AudioCapsBuilder::new_interleaved()
        .format(RAW_AUDIO_FORMAT)
        .channels(RAW_AUDIO_CHANNELS)
        .build()
}

pub fn raw_audio_caps_at(rate: i32) -> gst::Caps {
    gst_audio::AudioCapsBuilder::new_interleaved()
        .format(RAW_AUDIO_FORMAT)
        .rate(rate)
        .channels(RAW_AUDIO_CHANNELS)
        .fallback_channel_mask()
        .build()
}

pub fn uri_for_key(key: &str) -> String {
    format!("{URI_SCHEME}://{key}")
}

/// Extracts the scenario key from an `ftest://` URI.
pub fn key_from_uri(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix(URI_SCHEME)?.strip_prefix("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let key = &rest[..end];
    (!key.is_empty()).then_some(key)
}

/// Builds a stream-id. Keys must not contain `-`, suffixes may.
pub fn stream_id(key: &str, suffix: &str) -> String {
    debug_assert!(!key.contains('-'), "scenario keys must not contain '-'");
    format!("{STREAM_ID_MARKER}{key}-{suffix}")
}

/// Splits a stream-id into its key and suffix. Tolerates parsebin and
/// decodebin3 prefixing (`<upstream>/ftest-key-suffix`) and trailing appended
/// components.
pub fn split_stream_id(stream_id: &str) -> Option<(&str, &str)> {
    let marker = stream_id.find(STREAM_ID_MARKER)?;
    let rest = &stream_id[marker + STREAM_ID_MARKER.len()..];
    let (key, suffix) = rest.split_once('-')?;
    if key.is_empty() {
        return None;
    }
    let end = suffix.find(['/', ':']).unwrap_or(suffix.len());
    let suffix = &suffix[..end];
    (!suffix.is_empty()).then_some((key, suffix))
}

pub fn key_from_stream_id(stream_id: &str) -> Option<&str> {
    split_stream_id(stream_id).map(|(key, _)| key)
}

pub fn suffix_from_stream_id(stream_id: &str) -> Option<&str> {
    split_stream_id(stream_id).map(|(_, suffix)| suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trip() {
        let uri = uri_for_key("scen1");
        assert_eq!(uri, "ftest://scen1");
        assert_eq!(key_from_uri(&uri), Some("scen1"));
        assert_eq!(key_from_uri("ftest://scen1/ignored?x=1"), Some("scen1"));
        assert_eq!(key_from_uri("file:///tmp/a.mkv"), None);
        assert_eq!(key_from_uri("ftest://"), None);
    }

    #[test]
    fn stream_id_round_trip() {
        let id = stream_id("scen1", "video_0");
        assert_eq!(id, "ftest-scen1-video_0");
        assert_eq!(split_stream_id(&id), Some(("scen1", "video_0")));
    }

    #[test]
    fn stream_id_tolerates_prefix_and_suffix() {
        assert_eq!(
            split_stream_id("abc123/ftest-scen1-video_0"),
            Some(("scen1", "video_0"))
        );
        assert_eq!(
            split_stream_id("ftest-scen1-video_0/parsed:0"),
            Some(("scen1", "video_0"))
        );
        assert_eq!(split_stream_id("some-other-id"), None);
    }
}
