use std::collections::HashSet;

use gst::glib::{object::Cast, types::StaticType};
use tracing::debug;

use crate::media_formats::*;

pub fn init_and_load_plugins() {
    gst::init().unwrap();
    debug!(gstreamer_version = %gst::version_string());

    // TODO: investigate why certain files leads to crashes when this is added
    // gst::rust_allocator().clone().set_default();

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let mut plugin_dir = std::env::current_exe().unwrap();
        plugin_dir.pop();
        #[cfg(target_os = "macos")]
        plugin_dir.push("lib");
        unsafe {
            std::env::set_var("GIO_MODULE_DIR", plugin_dir.join("gio").join("modules"));
        }
        #[cfg(target_os = "windows")]
        let plugins = receiver_resources::all_plugins_for_win();
        #[cfg(target_os = "macos")]
        let plugins = receiver_resources::all_plugins_for_macos();
        for plugin in plugins {
            use tracing::error;

            let mut path = plugin_dir.clone();
            path.push(&plugin);
            let registry = gst::Registry::get();
            match gst::Plugin::load_file(&path) {
                Ok(plugin) => {
                    let _ = registry.add_plugin(&plugin);
                }
                Err(err) => error!(?err, plugin, "Failed to load gstreamer plugin"),
            }
        }
    }

    crate::fcastwhepsrcbin::plugin_init().unwrap();
    crate::fcasttextoverlay::plugin_init().unwrap();
    crate::fcasthttpsrc::plugin_init().unwrap();
    crate::fcompsrc::plugin_init().unwrap();
    crate::fwebrtcsrc::plugin_init().unwrap();
    gstrswebrtc::plugin_register_static().unwrap();

    #[cfg(feature = "static-gst-plugins")]
    {
        #[cfg(not(target_os = "android"))]
        gstrsrtp::plugin_register_static().unwrap();
        gstdav1d::plugin_register_static().unwrap();
    }
}

#[tracing::instrument]
pub fn find_formats() -> (
    HashSet<Container>,
    HashSet<Video>,
    HashSet<Audio>,
    HashSet<Subtitle>,
    HashSet<Protocol>,
) {
    let mut containers = HashSet::<Container>::new();
    let mut videos = HashSet::<Video>::new();
    let mut audios = HashSet::<Audio>::new();
    let mut subtitles = HashSet::<Subtitle>::new();
    let mut protocols = HashSet::<Protocol>::new();

    let reg = gst::Registry::get();
    for feat in reg.features(gst::ElementFactory::static_type()) {
        let Some(elem) = feat.downcast_ref::<gst::ElementFactory>() else {
            continue;
        };

        let is_demuxer = elem.has_type(gst::ElementFactoryType::DEMUXER);
        let is_decoder = elem.has_type(gst::ElementFactoryType::DECODER);
        let is_video = elem.has_type(gst::ElementFactoryType::MEDIA_VIDEO);
        let is_audio = elem.has_type(gst::ElementFactoryType::MEDIA_AUDIO);
        let is_subtitle = elem.has_type(gst::ElementFactoryType::MEDIA_SUBTITLE);

        if is_demuxer {
            let templates = elem.static_pad_templates();
            for template in templates {
                if template.direction() != gst::PadDirection::Sink {
                    continue;
                }

                for structure in template.caps().iter() {
                    let name = structure.name().as_str();
                    let container = match name {
                        "application/ogg" | "audio/ogg" | "video/ogg" => Container::Ogg,
                        "application/x-hls" => Container::Hls,
                        "application/dash+xml" => Container::Dash,
                        "video/x-flv" => Container::Flv,
                        "video/quicktime" => {
                            containers.insert(Container::Mp4V);
                            Container::Quicktime
                        }
                        "audio/x-m4a" => Container::Mp4A,
                        "audio/x-matroska" => Container::Mka,
                        "video/x-matroska" => Container::Mkv,
                        "audio/webm" => Container::WebmA,
                        "video/webm" => Container::WebmV,
                        "video/mpegts" => Container::MpegTs,
                        _ => continue,
                    };
                    containers.insert(container);
                }
            }
        }
        if is_decoder {
            let templates = elem.static_pad_templates();
            for template in templates {
                if template.direction() != gst::PadDirection::Sink {
                    continue;
                };

                for structure in template.caps().iter() {
                    let name = structure.name().as_str();
                    if is_video {
                        let video = match name {
                            "video/x-vp8" => Video::Vp8,
                            "video/x-vp9" => Video::Vp9,
                            "video/x-av1" => Video::Av1,
                            "video/x-h264" => Video::H264,
                            "video/x-h265" => Video::H265,
                            "video/x-theora" => Video::Theora,
                            _ => continue,
                        };
                        videos.insert(video);
                    }
                    if is_audio {
                        let audio = match name {
                            "audio/x-flac" => Audio::Flac,
                            "audio/x-ac3" | "audio/ac3" => Audio::Ac3,
                            "audio/x-opus" => Audio::Opus,
                            "audio/x-vorbis" => Audio::Vorbis,
                            "audio/x-wavpack" => Audio::WavPack,
                            "audio/mpeg" => Audio::Mpeg,
                            _ => continue,
                        };
                        audios.insert(audio);
                    }
                    if is_subtitle {
                        let subtitle = match name {
                            "subpicture/x-dvd" => Subtitle::Dvd,
                            "subpicture/x-dvb" => Subtitle::Dvb,
                            "subpicture/x-pgs" => Subtitle::Pgs,
                            "application/x-ssa" => Subtitle::Ssa,
                            "application/x-ass" => Subtitle::Ass,
                            "application/x-subtitle" => Subtitle::Srt,
                            "application/x-subtitle-vtt" => Subtitle::Vtt,
                            "application/ttml+xml" => Subtitle::Ttml,
                            _ => continue,
                        };
                        subtitles.insert(subtitle);
                    }
                }
            }
        }

        for proto in elem.uri_protocols() {
            let protocol = match proto.as_str() {
                "http" => Protocol::Http,
                "https" => Protocol::Https,
                "rtmp" | "rtmpt" | "rtmps" | "rtmpe" | "rtmfp" | "rtmpte" | "rtmpts" => {
                    Protocol::Rtmp
                }
                "data" => Protocol::Data,
                "rtsp" | "rtspu" | "rtspt" | "rtsph" | "rtsp-sdp" | "rtsps" | "rtspsu"
                | "rtspst" | "rtspsh" => Protocol::Rtsp,
                "srt" => Protocol::Srt,
                "fcastwhep" => Protocol::Whep,
                _ => continue,
            };
            protocols.insert(protocol);
        }
    }

    (containers, videos, audios, subtitles, protocols)
}
