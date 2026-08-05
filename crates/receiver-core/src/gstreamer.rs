use std::collections::HashSet;

use gst::glib::{object::Cast, types::StaticType};
use tracing::debug;

use crate::media_formats::*;

pub fn init_and_load_plugins() {
    #[cfg(feature = "static-gstreamer")]
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", "");
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH", "");
        std::env::set_var("GST_PLUGIN_PATH_1_0", "");
        std::env::set_var("GST_PLUGIN_PATH", "");
        std::env::set_var("GST_REGISTRY_DISABLE", "yes");
    }

    // subparse decides "is this UTF-8?" once per push buffer and latches a
    // failure for the rest of the stream, so a multi-byte character that
    // straddles a read boundary (4096 bytes for filesrc, a network chunk for
    // fcasthttpsrc) flips a perfectly valid UTF-8 subtitle file to
    // ISO-8859-15 from that point to EOF. Naming the encoding takes the one
    // conversion path that hands a truncated trailing sequence to the next
    // buffer instead of rejecting the whole block. matroskademux reads the
    // same variable for embedded text. A user's own value wins because it is
    // the only lever left for a legacy cp1252 file. Measurements:
    // subtitle-encoding-findings.md.
    if std::env::var_os("GST_SUBTITLE_ENCODING").is_none() {
        unsafe {
            std::env::set_var("GST_SUBTITLE_ENCODING", "UTF-8");
        }
    }

    gst::init().unwrap();
    debug!(gstreamer_version = %gst::version_string());

    // Dynamic-build path only: load the bundled plugin dylibs/DLLs and point
    // GIO at the bundled TLS module. A static build must NOT do this. The
    // on-disk plugins would drag in a second glib ("cannot register existing
    // type"), and TLS is already compiled in (glib-networking's GIO module is
    // registered by gst_init_static_plugins).
    #[cfg(all(
        any(target_os = "windows", target_os = "macos"),
        not(feature = "static-gstreamer")
    ))]
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

    fcast_gst_elements::fcastwhepsrcbin::plugin_init().unwrap();
    fcast_gst_elements::fcasthttpsrc::plugin_init().unwrap();
    fcast_gst_elements::fcastaudiostretch::plugin_init().unwrap();
    #[cfg(target_os = "linux")]
    fcast_gst_elements::pwaudiosink::plugin_init().unwrap();
    crate::fcompsrc::plugin_init().unwrap();
    fcast_gst_elements::sabrumpsrc::plugin_init().unwrap();
    #[cfg(feature = "airplay")]
    crate::airplay::source::plugin_init().unwrap();
    crate::fwebrtcsrc::plugin_init().unwrap();
    crate::imagetypefind::plugin_init().unwrap();
    crate::imagedec::plugin_init().unwrap();
    // Registers only on a VA-capable Linux box (see the fn); a no-op otherwise,
    // so baseline JPEG falls back to fimagedec.
    #[cfg(target_os = "linux")]
    crate::vajpegdec::plugin_init().unwrap();
    gstrswebrtc::plugin_register_static().unwrap();
    gstrssubparse::plugin_register_static().unwrap();

    // decodebin3/parsebin and subtitleoverlay pick a subtitle parser by rank.
    // rssubparse/rsssaparse register at NONE (never autoplugged) and the C
    // subparse/ssaparse at PRIMARY, so swap the ranks to route every subtitle
    // stream through the Rust elements while keeping the C ones around as a
    // gst-launch escape hatch.
    {
        use gst::prelude::PluginFeatureExtManual;
        let registry = gst::Registry::get();
        for c_name in ["subparse", "ssaparse"] {
            if let Some(feature) = registry.lookup_feature(c_name) {
                feature.set_rank(gst::Rank::NONE);
            }
        }
        for rs_name in ["rssubparse", "rsssaparse"] {
            registry
                .lookup_feature(rs_name)
                .expect("registered by gstrssubparse above")
                .set_rank(gst::Rank::PRIMARY);
        }
    }

    #[cfg(feature = "static-gst-plugins")]
    {
        #[cfg(not(target_os = "android"))]
        gstrsrtp::plugin_register_static().unwrap();
        gstdav1d::plugin_register_static().unwrap();
    }
}

#[cfg(all(test, feature = "static-gstreamer"))]
#[ctor::ctor]
fn isolate_gst_registry_for_tests() {
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", "");
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH", "");
        std::env::set_var("GST_PLUGIN_PATH_1_0", "");
        std::env::set_var("GST_PLUGIN_PATH", "");
        std::env::set_var("GST_REGISTRY_DISABLE", "yes");
    }
}

#[cfg(test)]
pub(crate) fn init_for_tests() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        // Elements fcastplaybin builds unconditionally must be registered here
        // too, or a test that constructs a pipeline fails on a missing factory
        // rather than on whatever it meant to assert. `fcastaudiostretch` is in
        // the audio chain of every pipeline.
        fcast_gst_elements::fcastaudiostretch::plugin_init().unwrap();
    });
}

fn caps_field_has_int(structure: &gst::StructureRef, field: &str, target: i32) -> bool {
    let Ok(value) = structure.value(field) else {
        return false;
    };

    if let Ok(v) = value.get::<i32>() {
        v == target
    } else if let Ok(list) = value.get::<gst::ListRef>() {
        list.as_slice()
            .iter()
            .any(|v| v.get::<i32>().is_ok_and(|v| v == target))
    } else {
        false
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

    const MAX_DUMP_ELEMS: usize = 15;
    let mut elems_scratch = Vec::with_capacity(MAX_DUMP_ELEMS);

    let reg = gst::Registry::get();
    for feat in reg.features(gst::ElementFactory::static_type()) {
        let Some(elem) = feat.downcast_ref::<gst::ElementFactory>() else {
            continue;
        };

        use gst::prelude::GstObjectExt;
        elems_scratch.push(elem.name());
        if elems_scratch.len() >= MAX_DUMP_ELEMS {
            debug!(elems = format!("[{}]", elems_scratch.join(",")));
            elems_scratch.clear();
        }

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
                            containers.insert(Container::Mp4);
                            Container::Quicktime
                        }
                        "audio/x-m4a" => Container::Mp4,
                        "audio/x-matroska" => Container::Mkv,
                        "video/x-matroska" => Container::Mkv,
                        "audio/webm" => Container::Webm,
                        "video/webm" => Container::Webm,
                        "video/mpegts" => Container::MpegTs,
                        "video/x-msvideo" => Container::Avi,
                        "audio/x-wav" => Container::Wav,
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
                        match name {
                            "audio/x-flac" => {
                                audios.insert(Audio::Flac);
                            }
                            "audio/x-ac3" | "audio/ac3" => {
                                audios.insert(Audio::Ac3);
                            }
                            "audio/x-eac3" => {
                                audios.insert(Audio::Eac3);
                            }
                            "audio/x-dts" => {
                                audios.insert(Audio::Dts);
                            }
                            "audio/x-opus" => {
                                audios.insert(Audio::Opus);
                            }
                            "audio/x-vorbis" => {
                                audios.insert(Audio::Vorbis);
                            }
                            "audio/x-wavpack" => {
                                audios.insert(Audio::WavPack);
                            }
                            // `audio/mpeg` covers both MPEG-1/2 audio (mp1/2/3,
                            // mpegversion=1) and AAC (mpegversion 2/4); the
                            // `mpegversion` field disambiguates the two.
                            "audio/mpeg" => {
                                if caps_field_has_int(structure, "mpegversion", 1) {
                                    audios.insert(Audio::Mpeg);
                                }
                                if caps_field_has_int(structure, "mpegversion", 2)
                                    || caps_field_has_int(structure, "mpegversion", 4)
                                {
                                    audios.insert(Audio::Aac);
                                }
                            }
                            _ => {}
                        }
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
                "sabrump" => Protocol::Sabr,
                _ => continue,
            };
            protocols.insert(protocol);
        }
    }

    audios.insert(Audio::Pcm);

    if !elems_scratch.is_empty() {
        debug!(elems = format!("[{}]", elems_scratch.join(",")));
    }

    (containers, videos, audios, subtitles, protocols)
}
