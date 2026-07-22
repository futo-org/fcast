use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, ValueEnum};
use std::rc::Rc;
use xshell::{cmd, Shell};

use crate::sh;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Plugins {
    Base,
    Good,
    Bad,
    Ugly,
}

impl Plugins {
    /// meson subproject option prefix, e.g. `gst-plugins-bad`.
    fn prefix(self) -> &'static str {
        match self {
            Plugins::Base => "gst-plugins-base",
            Plugins::Good => "gst-plugins-good",
            Plugins::Bad => "gst-plugins-bad",
            Plugins::Ugly => "gst-plugins-ugly",
        }
    }
}

/// The GStreamer libraries whose ABI must be exposed by `gstreamer-full-1.0`
/// (the ones the receiver's `*-sys` crates bind, plus internal webrtc/dtls deps).
const FULL_LIBRARIES: &[&str] = &[
    "gstreamer-app-1.0",
    "gstreamer-video-1.0",
    "gstreamer-base-1.0",
    "gstreamer-audio-1.0",
    "gstreamer-tag-1.0",
    "gstreamer-allocators-1.0",
    "gstreamer-pbutils-1.0",
    "gstreamer-rtp-1.0",
    "gstreamer-rtsp-1.0",
    "gstreamer-sdp-1.0",
    "gstreamer-net-1.0",
    "gstreamer-sctp-1.0",
    "gstreamer-webrtc-1.0",
];

/// gstreamer-rs `*-sys` crates whose system-deps entry we force to static.
const SYSTEM_DEPS: &[&str] = &[
    "GSTREAMER_1_0",
    "GSTREAMER_APP_1_0",
    "GSTREAMER_VIDEO_1_0",
    "GSTREAMER_BASE_1_0",
    "GSTREAMER_AUDIO_1_0",
    "GSTREAMER_TAG_1_0",
    "GSTREAMER_ALLOCATORS_1_0",
    "GSTREAMER_PBUTILS_1_0",
    "GSTREAMER_WEBRTC_1_0",
    "GSTREAMER_SDP_1_0",
    "GSTREAMER_RTP_1_0",
    "GSTREAMER_NET_1_0",
];

/// Plugins forced ON (meson errors if the dep is missing). vorbis/theora:
/// gst-libav refuses to wrap these decoders and expects the native plugins.
const ENABLE_COMMON: &[(Plugins, &str)] = &[(Plugins::Base, "vorbis"), (Plugins::Base, "theora")];

/// Element-level whitelists (`-Dgst-full-elements`): a plugin named here
/// registers ONLY the listed elements — the rest of its element objects are
/// never referenced, so -ffunction-sections + --gc-sections drop them from
/// the binary. The plugin still COMPILES fully (this trims size, not build
/// time). Only valid for plugins using the standard GST_ELEMENT_REGISTER
/// macros — va (per-device registration) and libav (probes FFmpeg at init)
/// register dynamically and MUST NOT be listed; their encode/test elements
/// ride along, not trimmable without patching gst. NB a whitelisted plugin
/// skips plugin-level init entirely, so its typefinders/device providers are
/// dropped too (fine here: container typefinds live in typefindfunctions,
/// and nothing uses GstDeviceMonitor).
/// CAUTION: a gst bump that adds an element (e.g. a new parser) silently
/// excludes it — revisit these lists on version bumps.
const FULL_ELEMENTS: &[(&str, &[&str])] = &[
    // The plugin must stay (h264parse/h265parse), but registering only these
    // keeps its niche sibling parsers (jpeg2000parse, pngparse, …) from ever
    // linking.
    (
        "videoparsersbad",
        &[
            "av1parse",
            "h263parse",
            "h264parse",
            "h265parse",
            "h266parse",
            "mpeg4videoparse",
            "mpegvideoparse",
            "vc1parse",
            "vp9parse",
        ],
    ),
    // Receive-only: keep every depayloader (rtsp:// can carry any codec, so
    // keep them wholesale) plus the elements webrtcbin instantiates
    // internally; the ~40 payloaders (~300K .text) never link.
    (
        "rtp",
        &[
            "rtpac3depay",
            "rtpbvdepay",
            "rtpceltdepay",
            "rtpdvdepay",
            "rtpgstdepay",
            "rtpilbcdepay",
            "rtpg722depay",
            "rtpg723depay",
            "rtpg726depay",
            "rtpg729depay",
            "rtpgsmdepay",
            "rtpamrdepay",
            "rtppcmadepay",
            "rtppcmudepay",
            "rtpmpadepay",
            "rtpmparobustdepay",
            "rtpmpvdepay",
            "rtpopusdepay",
            "rtph261depay",
            "rtph263pdepay",
            "rtph263depay",
            "rtph264depay",
            "rtph265depay",
            "rtpj2kdepay",
            "rtpjpegdepay",
            "rtpklvdepay",
            "rtpL8depay",
            "rtpL16depay",
            "rtpL24depay",
            "rtpmp1sdepay",
            "rtpmp2tdepay",
            "rtpmp4vdepay",
            "rtpmp4adepay",
            "rtpmp4gdepay",
            "rtpqcelpdepay",
            "rtpsbcdepay",
            "rtpsirendepay",
            "rtpspeexdepay",
            "rtpsv3vdepay",
            "rtptheoradepay",
            "rtpvorbisdepay",
            "rtpvp8depay",
            "rtpvp9depay",
            "rtpvrawdepay",
            "rtpstreamdepay",
            "rtpisacdepay",
            // webrtcbin internals — dropping these breaks WHEP at runtime
            "rtpredenc",
            "rtpreddec",
            "rtpulpfecdec",
            "rtpulpfecenc",
            "rtpstorage",
            "rtphdrextcolorspace",
        ],
    ),
    // demux-only containers: qtmux/mp4mux/matroskamux/webmmux/flvmux/avimux
    // never link in a playback-only receiver.
    ("isomp4", &["qtdemux", "rtpxqtdepay"]),
    ("matroska", &["matroskademux", "matroskaparse"]),
    ("flv", &["flvdemux"]),
    ("avi", &["avidemux", "avisubtitle"]),
    (
        "ogg",
        &[
            "oggdemux",
            "oggparse",
            "oggaviparse",
            "ogmaudioparse",
            "ogmvideoparse",
            "ogmtextparse",
        ],
    ),
    // decode-only codecs: encoders + tag writers never link.
    ("opus", &["opusdec"]),
    ("theora", &["theoradec", "theoraparse"]),
    ("vorbis", &["vorbisdec", "vorbisparse", "vorbistag"]),
    ("flac", &["flacdec"]),
    // playbin3's rate-change filter is the only audiofx element it autoplugs.
    ("audiofx", &["scaletempo"]),
    // network sources — the receiver never streams out, so sinks drop.
    ("soup", &["souphttpsrc"]),
    ("rtmp2", &["rtmp2src"]),
    // playsink/subtitleoverlay render via textoverlay/textrender; the
    // time/clock debug overlays never link.
    ("pango", &["textoverlay", "textrender"]),
];

/// Whitelists for plugins that only exist in the LINUX build (srt/wavpack
/// are force-enabled there but have no wrap for the hermetic mac/win scope;
/// pulse is Linux-only). The generator emits gst_element_register_<e> calls
/// UNCONDITIONALLY for whitelisted elements — naming a plugin that isn't
/// built is an undefined symbol at link, so these must not reach mac/win.
/// NB keys are PLUGIN names, not meson option names — the option is `pulse`
/// but the plugin is `pulseaudio` (a mismatch is a SILENT no-op: the plugin
/// just registers fully; caught via the runtime element dump).
const FULL_ELEMENTS_LINUX: &[(&str, &[&str])] = &[
    ("srt", &["srtsrc", "srtclientsrc", "srtserversrc"]),
    // decode only; wavpackparse lives in audioparsers, not here.
    ("wavpack", &["wavpackdec"]),
    // audio output only (pulsesrc is capture); pulsedeviceprovider drops
    // with plugin-level init — nothing uses GstDeviceMonitor.
    ("pulseaudio", &["pulsesink"]),
];

/// Linux: VA-API hardware decode; audio via pulse/pipewire (auto). srt is
/// advertised via URI-handler introspection. assrender (styled ASS/SSA subs)
/// attaches overlay-composition meta → composited by the receiver's libplacebo
/// path; needs libass. wavpack: avdec_wavpack is on gst-libav's skip list, so
/// the native plugin is the only WavPack decoder. srt/assrender/wavpack have
/// no wrapdb wrap, so the hermetic mac/win builds drop them — Linux-only.
const ENABLE_LINUX: &[(Plugins, &str)] = &[
    (Plugins::Bad, "va"),
    (Plugins::Bad, "srt"),
    (Plugins::Bad, "assrender"),
    (Plugins::Good, "wavpack"),
    // Previously `auto`: on an image without libnice/libsrtp devel the webrtc
    // stack silently drops out of the build and fwebrtcsrc/WHEP breaks at
    // runtime. Force it on so a missing dep is a configure-time error.
    (Plugins::Bad, "webrtc"), // webrtcbin — fwebrtcsrc drives it directly
    (Plugins::Bad, "dtls"),
    (Plugins::Bad, "srtp"),
    (Plugins::Bad, "sctp"),
];
const DISABLE_LINUX: &[(Plugins, &str)] = &[(Plugins::Base, "gl")];

/// macOS: VideoToolbox decode + CoreAudio/Cocoa output. applemedia
/// hard-depends on the gstgl library at compile time (unconditional
/// `#include <gst/gl/gl.h>`; gstglconfig.h is only generated when `gl`
/// builds), so `gl` is enabled even though glimagesink is never autoplugged.
/// On macOS gstgl links only system frameworks.
const ENABLE_MACOS: &[(Plugins, &str)] = &[
    (Plugins::Bad, "applemedia"),
    (Plugins::Good, "osxaudio"),
    (Plugins::Good, "osxvideo"),
    (Plugins::Base, "gl"),
];
/// macOS must link ONLY OS frameworks (the installer verifies via otool).
/// Each of these pulls an external dylib with no vendored wrap, or is an
/// encoder / redundant with libav decode. Everything the receiver decodes is
/// covered by libav + the native vorbis/theora/opus/flac/dav1d plugins.
const DISABLE_MACOS: &[(Plugins, &str)] = &[
    (Plugins::Bad, "va"),
    (Plugins::Good, "pulse"),
    // no vendored wrap → can't link static on macOS (kept on Linux)
    (Plugins::Bad, "srt"),
    (Plugins::Bad, "assrender"),
    (Plugins::Good, "wavpack"),
];

/// Windows: WASAPI audio; d3d11 etc. stay `auto`. NOTE: static gst-full on
/// MSVC is upstream-experimental.
const ENABLE_WINDOWS: &[(Plugins, &str)] = &[(Plugins::Bad, "wasapi")];
const DISABLE_WINDOWS: &[(Plugins, &str)] = &[
    (Plugins::Bad, "va"),
    (Plugins::Good, "pulse"),
    (Plugins::Base, "gl"),
    (Plugins::Good, "wavpack"),
];

/// Plugins removed everywhere: unused by a cast receiver, or GPU/vendor codecs
/// whose companion support library gstreamer-full fails to pull statically.
/// (Kept intentionally: videofilter, audiobuffersplit, proxy — autoplugged.)
const DISABLE_COMMON: &[(Plugins, &str)] = &[
    // vendor GPU codecs
    (Plugins::Bad, "hip"),
    (Plugins::Bad, "nvcodec"),
    (Plugins::Bad, "qsv"),
    (Plugins::Bad, "vulkan"),
    (Plugins::Bad, "amfcodec"), // AMD encode-only; even registers on Linux (dlopen)
    // orphan / useless (registered-but-unlinked, or metric/gadget)
    (Plugins::Bad, "vmaf"),
    (Plugins::Bad, "uvcgadget"),
    // X11 video (receiver has its own sink). `gl` is NOT disabled here —
    // applemedia needs the gstgl library, so gl is per-target instead.
    (Plugins::Base, "x11"),
    (Plugins::Good, "ximagesrc"),
    // image codecs (receiver decodes images itself)
    (Plugins::Good, "jpeg"),
    (Plugins::Good, "png"),
    (Plugins::Bad, "openjpeg"),
    (Plugins::Bad, "webp"),
    (Plugins::Bad, "jpegformat"),
    (Plugins::Bad, "jp2kdecimator"),
    // SVG: unused, and a discoverable librsvg links dynamically — defeating
    // the static build (its .pc also leaks a bare `-no_compact_unwind` ld
    // flag that breaks clang).
    (Plugins::Bad, "rsvg"),
    // redundant codecs (libav provides decode)
    (Plugins::Bad, "openh264"),
    (Plugins::Bad, "fdkaac"),
    // vp8/vp9 decode comes from FFmpeg's native decoders; the vpx plugin
    // drags in the libvpx wrap, which force-builds encoders too.
    (Plugins::Good, "vpx"),
    // effects / visualizers
    (Plugins::Bad, "gaudieffects"),
    (Plugins::Bad, "audiovisualizers"),
    (Plugins::Bad, "coloreffects"),
    (Plugins::Bad, "geometrictransform"),
    (Plugins::Bad, "videofilters"),
    (Plugins::Bad, "freeverb"),
    (Plugins::Bad, "frei0r"),
    (Plugins::Good, "goom"),
    (Plugins::Good, "goom2k1"),
    (Plugins::Good, "monoscope"),
    (Plugins::Good, "spectrum"),
    (Plugins::Good, "shapewipe"),
    (Plugins::Good, "smpte"),
    (Plugins::Good, "videobox"),
    (Plugins::Good, "videocrop"),
    (Plugins::Good, "videomixer"),
    (Plugins::Good, "cutter"),
    (Plugins::Good, "imagefreeze"),
    (Plugins::Good, "replaygain"),
    // ML / analytics
    (Plugins::Bad, "tensordecoders"),
    (Plugins::Bad, "analyticsoverlay"),
    (Plugins::Bad, "faceoverlay"),
    (Plugins::Bad, "fieldanalysis"),
    (Plugins::Bad, "videosignal"),
    (Plugins::Bad, "bayer"),
    // drag in the huge webrtc-audio-processing C++ subproject, never used
    (Plugins::Bad, "webrtcdsp"),
    (Plugins::Bad, "isac"),
    // encoders / muxers (decode-only receiver)
    (Plugins::Good, "lame"),
    (Plugins::Bad, "adpcmenc"),
    (Plugins::Bad, "asfmux"),
    (Plugins::Bad, "dvbsubenc"),
    (Plugins::Bad, "mpegpsmux"),
    (Plugins::Bad, "mpegtsmux"),
    (Plugins::Bad, "subenc"),
    (Plugins::Good, "wavenc"),
    (Plugins::Good, "xingmux"),
    (Plugins::Bad, "id3tag"), // id3v2mux/id3mux: ID3 tag *muxer*, encode-side only
    // audio channel interleave/deinterleave: not autoplugged in playback
    (Plugins::Good, "interleave"),
    // capture / hardware IO / IPC
    (Plugins::Bad, "camerabin2"),
    (Plugins::Bad, "decklink"),
    (Plugins::Bad, "ipcpipeline"),
    (Plugins::Bad, "fbdev"),
    (Plugins::Bad, "kms"),
    (Plugins::Bad, "shm"),
    (Plugins::Bad, "librfb"),
    (Plugins::Bad, "unixfd"),
    // TCP: tcpclient/server src+sink, multifdsink, multisocketsink, socketsrc —
    // all serve-out / socket-IPC elements a playback receiver never instantiates.
    (Plugins::Base, "tcp"),
    // GIO IO: giosrc/giosink/giostream*. File IO goes through filesrc and
    // network IO through souphttpsrc / the receiver's own httpsrc, so no cast
    // URI scheme needs a GIO source. Disables only the gst `gio` element plugin;
    // the GLib GIO library + glib-networking TLS module (see gstreamer.rs) are
    // unaffected.
    (Plugins::Base, "gio"),
    // v4l2src/v4l2sink/v4l2radio: pure capture/output — a receiver never
    // captures. Bad's v4l2codecs (v4l2sl*dec) is KEPT: it's the stateless
    // hardware-decode path on SoCs like the Raspberry Pi.
    (Plugins::Good, "v4l2"),
    (Plugins::Base, "alsa"),
    (Plugins::Good, "oss"),
    (Plugins::Good, "oss4"),
    // legacy adaptive streaming: superseded by adaptivedemux2's
    // hlsdemux2/dashdemux2 (what playbin3 autoplugs); also home to
    // hlssink/dashsink, which we never use
    (Plugins::Bad, "hls"),
    (Plugins::Bad, "dash"),
    // test/debug/util elements never autoplugged in playback
    (Plugins::Base, "audiotestsrc"),
    (Plugins::Base, "videotestsrc"),
    (Plugins::Base, "debugutils"),
    (Plugins::Good, "debugutils"),
    (Plugins::Bad, "debugutils"), // fakeaudiosink/fakevideosink/testsrcbin/…
    (Plugins::Good, "effectv"),
    (Plugins::Bad, "audiolatency"),
    (Plugins::Bad, "festival"),
    (Plugins::Bad, "smooth"),
    (Plugins::Bad, "speed"),
    (Plugins::Bad, "interlace"),
    (Plugins::Bad, "codectimestamper"),
    (Plugins::Bad, "codecalpha"),
    (Plugins::Bad, "closedcaption"),
    // gates rtpmanagerbad (rtpsrc/rtpsink); good's `rtp` (depayloaders) stays
    (Plugins::Bad, "rtp"),
    // mixing/compositing/encoding infrastructure unused by this receiver
    (Plugins::Base, "adder"),
    (Plugins::Base, "audiomixer"),
    (Plugins::Base, "compositor"),
    (Plugins::Base, "encoding"),
    (Plugins::Base, "rawparse"),
    (Plugins::Base, "videorate"),
    (Plugins::Base, "audiorate"),
    (Plugins::Base, "dsd"),
    (Plugins::Bad, "rawparse"), // gates the legacyrawparse plugin
    // audio effects / niche audio IO
    (Plugins::Good, "alpha"),
    (Plugins::Good, "apetag"),
    (Plugins::Good, "auparse"),
    (Plugins::Good, "cairo"),
    (Plugins::Good, "dtmf"),
    (Plugins::Good, "equalizer"),
    (Plugins::Good, "jack"),
    (Plugins::Good, "y4m"),
    (Plugins::Bad, "dvb"),
    // niche demux/parse/format
    (Plugins::Bad, "transcode"),
    (Plugins::Bad, "bz2"),
    (Plugins::Bad, "aes"),
    (Plugins::Bad, "segmentclip"),
    (Plugins::Bad, "audiofxbad"),
    (Plugins::Bad, "audiomixmatrix"),
    (Plugins::Bad, "gdp"),
    (Plugins::Bad, "midi"),
    (Plugins::Bad, "netsim"),
    (Plugins::Bad, "onvif"),
    (Plugins::Bad, "pcapparse"),
    (Plugins::Bad, "pnm"),
    (Plugins::Bad, "removesilence"),
    (Plugins::Bad, "rist"),
    (Plugins::Bad, "siren"),
    (Plugins::Bad, "videoframe_audiolevel"),
    (Plugins::Bad, "accurip"),
    (Plugins::Bad, "adpcmdec"),
    (Plugins::Bad, "aiff"),
    (Plugins::Bad, "autoconvert"),
    (Plugins::Bad, "insertbin"),
    (Plugins::Bad, "inter"),
    (Plugins::Bad, "ivfparse"),
    (Plugins::Bad, "ivtc"),
    (Plugins::Bad, "mse"),
    (Plugins::Bad, "mxf"),
    (Plugins::Bad, "switchbin"),
    (Plugins::Bad, "timecode"),
    (Plugins::Bad, "vmnc"),
    (Plugins::Bad, "smoothstreaming"),
    (Plugins::Good, "law"),
    (Plugins::Good, "flx"),
    (Plugins::Good, "level"),
    (Plugins::Good, "multifile"),
    (Plugins::Good, "multipart"),
    (Plugins::Ugly, "realmedia"),
    // ASF/WMV/WMA: dead format, nothing casts it; the WMV/WMA avdec_* are
    // dropped from FFMPEG_DECODERS too
    (Plugins::Ugly, "asfdemux"),
    // ---- hermetic auto-plugin exclusions ----
    // Everything below has an external dep and sat at meson `auto`: whether it
    // built (and registered into gstinitstaticplugins.c) depended on which
    // -devel packages the build image ships. A plugin that registers but does
    // not link statically fails the final link with
    // `undefined symbol: gst_plugin_<x>_register` (first hit: lc3 on the
    // Fedora bootc fhs image). Pin every unneeded one off. Deliberately KEPT
    // at auto: ttml (Subtitle::Ttml is advertised), rtmp2 (serves the
    // advertised rtmp:// protocol, no external dep), dvbsuboverlay/dvdspu
    // (subs in TS captures, no external dep).
    // encoders (decode-only receiver)
    (Plugins::Bad, "lc3"),        // Bluetooth LE audio codec (liblc3)
    (Plugins::Bad, "x265"),       // H.265 encode; rides in via libheif's codec stack
    (Plugins::Bad, "libde265"),   // H.265 decode, redundant with libav; libheif orbit
    (Plugins::Bad, "aom"),        // AV1 encode; decode via dav1d
    (Plugins::Bad, "svtav1"),     // AV1 encoder
    (Plugins::Bad, "svthevcenc"), // HEVC encoder
    (Plugins::Bad, "svtjpegxs"),  // JPEG-XS
    (Plugins::Bad, "faac"),       // AAC encoder
    (Plugins::Bad, "faad"),       // AAC decode, redundant with libav
    (Plugins::Bad, "voaacenc"),   // AAC encoder
    (Plugins::Bad, "voamrwbenc"), // AMR-WB encoder
    (Plugins::Bad, "mpeg2enc"),   // mjpegtools encoder
    (Plugins::Bad, "mplex"),      // mjpegtools muxer
    (Plugins::Good, "twolame"),   // MP2 encoder
    (Plugins::Bad, "lcevcdecoder"),
    (Plugins::Bad, "lcevcencoder"),
    // audio decoders redundant with libav (see FFMPEG_DECODERS)
    (Plugins::Good, "mpg123"),   // mp3
    (Plugins::Good, "amrnb"),    // opencore-amr
    (Plugins::Good, "amrwbdec"), // opencore-amr
    (Plugins::Good, "speex"),
    (Plugins::Bad, "dts"), // libdca
    (Plugins::Bad, "gsm"),
    // tracker/module/MIDI music formats — never cast
    (Plugins::Bad, "modplug"),
    (Plugins::Bad, "musepack"),
    (Plugins::Bad, "gme"),
    (Plugins::Bad, "openmpt"),
    (Plugins::Bad, "wildmidi"),
    (Plugins::Bad, "fluidsynth"),
    // image / overlay / analysis
    (Plugins::Good, "gdk-pixbuf"), // receiver decodes images itself
    (Plugins::Bad, "openexr"),
    (Plugins::Bad, "colormanagement"), // lcms2
    (Plugins::Bad, "zbar"),            // barcode
    (Plugins::Bad, "zxing"),           // barcode/QR
    (Plugins::Bad, "qroverlay"),
    (Plugins::Bad, "iqa"),
    // audio effects / plugin hosts / TTS / spatializers
    (Plugins::Bad, "soundtouch"), // pitch/tempo
    (Plugins::Bad, "spandsp"),    // dtmf/fax
    (Plugins::Bad, "ladspa"),
    (Plugins::Bad, "lv2"),
    (Plugins::Bad, "bs2b"),
    (Plugins::Bad, "flite"), // text-to-speech
    (Plugins::Bad, "openal"),
    // bluetooth audio
    (Plugins::Bad, "bluez"),
    (Plugins::Bad, "sbc"),
    (Plugins::Bad, "ldac"),
    (Plugins::Bad, "openaptx"),
    // capture hardware / physical media a receiver never touches
    (Plugins::Bad, "dc1394"),   // firewire cameras
    (Plugins::Good, "dv1394"),  // firewire DV
    (Plugins::Good, "dv"),      // DV video
    (Plugins::Bad, "resindvd"), // DVD navigation
    // network paths covered elsewhere
    (Plugins::Bad, "curl"), // http via souphttpsrc / the receiver's own httpsrc
    (Plugins::Bad, "neon"), // another http source
    (Plugins::Bad, "rtmp"), // librtmp; rtmp:// is served by rtmp2 (no external dep)
    (Plugins::Good, "shout2"), // icecast streaming sink
    (Plugins::Bad, "microdns"), // mdns via libmicrodns (receiver uses mdns-sd)
    // misc external-dep leftovers
    (Plugins::Bad, "sndfile"),
    (Plugins::Bad, "teletext"), // zvbi
    (Plugins::Good, "taglib"),  // metadata tagging
    (Plugins::Good, "bz2"),     // libbz2 in matroska (bz2-compressed tracks; no wrap)
];

/// FFmpeg decoders to keep (gst-libav's `avdec_*`). ALL decoders are disabled
/// and only these re-enabled — the full set (hundreds) is dead weight.
const FFMPEG_DECODERS: &[&str] = &[
    // video. vc1 stays: it's also carried in MKV/TS/Blu-ray remuxes, not just ASF.
    "h264",
    "hevc",
    "mpeg2video",
    "mpeg4",
    "mpeg1video",
    "msmpeg4v1",
    "msmpeg4v2",
    "msmpeg4v3",
    "h263",
    "h263p",
    "vc1",
    "vp6",
    "vp6f",
    "flv",
    "mjpeg",
    "prores",
    "vp8",
    "vp9",
    // audio
    "aac",
    "aac_latm",
    "ac3",
    "eac3",
    "mp3",
    "mp2",
    "mp1",
    "dca",
    "alac",
    "truehd",
    "mlp",
    "amrnb",
    "amrwb",
    // pcm / adpcm (pcm_bluray = LPCM in .m2ts Blu-ray remuxes)
    "pcm_s16le",
    "pcm_s16be",
    "pcm_s24le",
    "pcm_u8",
    "pcm_f32le",
    "pcm_alaw",
    "pcm_mulaw",
    "pcm_bluray",
    "adpcm_ima_wav",
    "adpcm_ms",
];

/// FFmpeg parsers/bsfs that kept decoders `select` internally. The groups are
/// disabled wholesale, and the meson port silently CULLS any decoder whose
/// selected component is missing (while still reporting it "enabled") — these
/// are the exact selects of FFMPEG_DECODERS, from configure's
/// `*_decoder_select` lines. Other selects (dsp/helpers) aren't group-gated.
const FFMPEG_COMPONENTS: &[&str] = &[
    "ac3_parser",               // ac3 (eac3 chains through ac3_decoder)
    "aac_latm_parser",          // aac_latm
    "h263_parser",              // h263; h263p/flv/mpeg4/msmpeg4v* chain through h263_decoder
    "mlp_parser",               // mlp, truehd
    "vp9_parser",               // vp9
    "vp9_superframe_split_bsf", // vp9
];

/// Wraps force-fallbacked in scope=Full so ONE static glib (plus the pango
/// stack it shares) is built from vendored wraps — this is what lets mac/win
/// build without the GStreamer dev kit. Forcing (not just not-found fallback)
/// keeps the build deterministic when a stray system copy exists. Forcing a
/// dep no platform requests (freetype2/fontconfig off-Linux) is a no-op.
const FULL_SCOPE_FALLBACK: &[&str] = &[
    "glib",
    "pcre2",
    "libffi",
    "proxy-libintl",
    "zlib",
    "pango",
    "harfbuzz",
    "fribidi",
    "cairo",
    "pixman",
    "libpng",
    "freetype2",
    "fontconfig",
    "expat",
    // Codec + support libs the kept plugins pull in. Unforced, meson resolves
    // them from whatever pkg-config finds → @rpath dylibs that dangle on user
    // machines. These are dependency names (what `dependency('…')` looks up),
    // not wrap filenames — see each wrap's [provide].
    "ogg",
    "vorbis",
    "vorbisenc",
    "theora",
    "theoradec",
    "theoraenc",
    "opus",
    "flac",
    "dav1d",
    "libsrtp2", // srtp plugin (webrtc security)
    "json-glib-1.0",
    "graphene-1.0",
    "graphene-gobject-1.0",
    "libjpeg", // gstopengl's gloverlay (gl enabled for applemedia) requires it
    // openssl (glib-networking TLS backend + dtls/srtp); ensure_wrap vendors
    // the .wrap, forcing builds libcrypto/libssl static.
    "openssl",
    "libcrypto",
    "libssl",
    // souphttpsrc stack — http(s) media sources (playbin3/adaptivedemux2).
    "libsoup-3.0",
    "libxml-2.0",
    "libpsl",
    "libnghttp2",
];

/// system-deps entries additionally forced static in scope=Full: the Rust side
/// must link the SAME static glib compiled into gstreamer-full (two glibs =
/// "cannot register existing type 'GstObject'"). dav1d is built from its wrap
/// in-tree and dav1d-sys must link that archive.
const SYSTEM_DEPS_FULL_SCOPE: &[&str] = &["GLIB_2_0", "GOBJECT_2_0", "GIO_2_0", "DAV1D"];

/// pkg-config modules a *Linux* build requires from the environment; asserted
/// up front with an actionable error. On mac/win the codecs come from wraps
/// and the platform plugins from OS frameworks — no assertion needed.
const REQUIRED_BUILD_PC_LINUX: &[&str] = &[
    "vorbis",
    "vorbisenc",
    "theora",
    "theoradec",
    "ogg",
    "libva",
    "libva-drm",
    "gudev-1.0",
    "srt",
    "libass",
    "wavpack",
    // NB the force-enabled webrtc stack (nice/libsrtp2/openssl) is NOT
    // asserted here: the monorepo carries wraps for all three, so meson
    // falls back to building them when the system lacks the .pc.
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StaticScope {
    /// gstreamer + codecs static; glib/pango/OS dynamic. For Linux/Flatpak,
    /// where the runtime provides (and must provide) glib.
    Gstreamer,
    /// Additionally build glib + pango + TLS static from vendored wraps → one
    /// glib → standalone binary, no dev kit. Default for macOS/Windows.
    /// NOT for Flatpak (glib comes from the runtime).
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Lto {
    /// No LTO beyond the cargo profile default.
    Off,
    /// Rust-only fat LTO
    Rust,
    /// Cross-language Rust↔C LTO: `-Db_lto` on the C side + rustc
    /// `-Clinker-plugin-lto` + `clang -fuse-ld=lld`. rustc's and clang's LLVM
    /// must be the same major version.
    Cross,
}

#[derive(Clone)]
struct Profile {
    scope: StaticScope,
    lto: Lto,
    offline: bool,
    target: Option<String>,
    /// Cargo profile for the receiver build (`dev`, `release`, or a custom
    /// profile like `release-dbg`); GStreamer stays release (see
    /// `gst_buildtype`).
    cargo_profile: String,
    /// meson buildtype for GStreamer (default "release").
    gst_buildtype: String,
    /// Pass --no-default-features to cargo (e.g. no systray on macOS).
    no_default_features: bool,
}

impl Profile {
    /// The `target/<subdir>` directory cargo writes this profile's build
    /// artifacts into: `dev` → `debug`, `release` → `release`, and any custom
    /// profile uses its own name (e.g. `release-dbg` → `release-dbg`).
    fn target_subdir(&self) -> &str {
        match self.cargo_profile.as_str() {
            "dev" | "test" => "debug",
            "release" | "bench" => "release",
            other => other,
        }
    }
}

const GST_REPO: &str = "https://gitlab.freedesktop.org/gstreamer/gstreamer.git";

#[derive(Args)]
pub struct GstreamerArgs {
    /// GStreamer mono-repo checkout. If omitted, xtask clones `--gst-ref`
    /// into target/ (needs network; incompatible with `--offline`).
    #[arg(long)]
    source: Option<Utf8PathBuf>,
    /// Git ref to clone when `--source` is not given.
    #[arg(long, default_value = "1.29.2")]
    gst_ref: String,
    /// Build directory for the static gstreamer (defaults to <source>/builddir-static).
    #[arg(long)]
    build_dir: Option<Utf8PathBuf>,
    /// Rust/meson target triple (defaults to host).
    #[arg(long)]
    target: Option<String>,
    /// Offline build: `meson --wrap-mode=nodownload`. Subprojects must be vendored.
    #[arg(long)]
    offline: bool,
    /// Defaults per target OS: `gstreamer` on Linux (glib from the runtime),
    /// `full` on macOS/Windows (glib/pango static from wraps, no dev kit).
    #[arg(long, value_enum)]
    pub static_scope: Option<StaticScope>,
    #[arg(long, value_enum, default_value_t = Lto::Off)]
    lto: Lto,
    /// Build the receiver as a debug (cargo dev) build. GStreamer stays release
    /// unless you also pass --gst-buildtype.
    #[arg(long)]
    debug: bool,
    /// Cargo profile for the receiver build, e.g. `release-dbg` for an
    /// optimized build that keeps full debug symbols (ideal under
    /// heaptrack/perf). Overrides --debug/--release; GStreamer is still
    /// controlled by --gst-buildtype.
    #[arg(long)]
    profile: Option<String>,
    /// Debug-info preset for profiling/debugging: builds the receiver in the
    /// `release-dbg` cargo profile (optimized + full unstripped debug symbols)
    /// AND builds GStreamer and all its vendored dependencies with debug info
    /// (`--buildtype=debugoptimized`). An explicit --profile / --gst-buildtype /
    /// --debug still wins over this preset.
    #[arg(long)]
    debug_info: bool,
    /// meson buildtype for GStreamer itself (e.g. release, debugoptimized, debug).
    /// Defaults to `debugoptimized` under --debug-info, otherwise `release`.
    #[arg(long)]
    gst_buildtype: Option<String>,
    /// Only build gstreamer, don't build the receiver.
    #[arg(long)]
    gstreamer_only: bool,
    /// Build the receiver with --no-default-features (e.g. no systray on macOS).
    #[arg(long)]
    pub no_default_features: bool,
    /// Remove built/downloaded artifacts and exit: the meson build dir +
    /// install prefix, and the auto-cloned source — never a --source tree.
    #[arg(long)]
    clean: bool,
}

impl GstreamerArgs {
    pub fn run(self) -> Result<()> {
        self.build().map(|_| ())
    }

    /// The cargo profile the receiver is built with: an explicit `--profile`
    /// wins, then `--debug-info` selects `release-dbg`, then `--debug` selects
    /// the `dev` profile, and the default is `release`. (The
    /// run/check/clippy/test subcommands force `--debug` on unless `--release`
    /// is passed, so those default to `dev` here.)
    fn cargo_profile(&self) -> String {
        match &self.profile {
            Some(p) => p.clone(),
            None if self.debug_info => "release-dbg".to_owned(),
            None if self.debug => "dev".to_owned(),
            None => "release".to_owned(),
        }
    }

    /// The meson buildtype GStreamer (and its vendored subprojects) is built
    /// with: an explicit `--gst-buildtype` wins, then `--debug-info` selects
    /// `debugoptimized` (debug=true, optimization=2 — propagated to every
    /// subproject), and the default is `release`.
    fn gst_buildtype(&self) -> String {
        match &self.gst_buildtype {
            Some(b) => b.clone(),
            None if self.debug_info => "debugoptimized".to_owned(),
            None => "release".to_owned(),
        }
    }

    /// The args you'd get by passing no flags — host target, release GStreamer,
    /// per-OS scope. Used to drive a cargo subcommand programmatically (e.g.
    /// `xtask test`) while keeping clap's declared defaults the single source
    /// of truth (gst_ref, buildtype, …).
    pub fn with_defaults() -> Self {
        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            gst: GstreamerArgs,
        }
        <Wrap as clap::Parser>::parse_from(["xtask"]).gst
    }

    /// Build (or reuse) the static GStreamer and return the pieces needed to
    /// drive cargo against it. Returns `Ok(None)` when `--clean` short-circuits.
    fn prepare(self) -> Result<Option<(Rc<Shell>, Profile, GstBuild)>> {
        self.prepare_impl(true)
            .map(|o| o.map(|(sh, profile, build, _)| (sh, profile, build)))
    }

    /// Like `prepare`, but `compile: false` only CONFIGURES GStreamer (`meson
    /// setup` — enough for the uninstalled .pc files to exist) and defers
    /// `meson compile`, returning the deferred stamp; the caller must run
    /// `compile_gstreamer` (or spawn/join) with it. Lets `build()` overlap the
    /// ninja build with the receiver's Rust dependency graph.
    fn prepare_impl(
        self,
        compile: bool,
    ) -> Result<Option<(Rc<Shell>, Profile, GstBuild, Option<String>)>> {
        let sh = sh();
        if self.clean {
            clean(self.source.as_deref(), self.build_dir.as_deref())?;
            return Ok(None);
        }
        let profile = Profile {
            scope: self.static_scope.unwrap_or_else(|| {
                if os_from_target(self.target.as_deref()) == "linux" {
                    StaticScope::Gstreamer
                } else {
                    StaticScope::Full
                }
            }),
            lto: self.lto,
            offline: self.offline,
            target: self.target.clone(),
            cargo_profile: self.cargo_profile(),
            gst_buildtype: self.gst_buildtype(),
            no_default_features: self.no_default_features,
        };
        let source = match self.source {
            Some(s) => s,
            None => resolve_source(&sh, &self.gst_ref, self.offline)?,
        };
        // meson requires absolute paths for --prefix (and relative build dirs
        // break once we push_dir elsewhere), so canonicalize up front.
        let source = canonicalize_no_verbatim(&source)
            .with_context(|| format!("canonicalizing source path {source}"))?;
        apply_gst_patches(&sh, &source, target_os(&profile))?;
        let build_dir = self
            .build_dir
            .unwrap_or_else(|| source.join("builddir-static"));

        let (build, stamp) = configure_gstreamer(&sh, &source, &build_dir, &profile)?;
        if compile {
            compile_gstreamer(&sh, &build, &profile, &stamp)?;
            Ok(Some((sh, profile, build, None)))
        } else {
            Ok(Some((sh, profile, build, Some(stamp))))
        }
    }

    /// Build the static gstreamer (+ receiver unless --gstreamer-only) and
    /// return the path to the receiver binary. Used by the installer commands.
    ///
    /// On Linux (scope=Gstreamer) the ninja build and the receiver's Rust
    /// dependency graph build CONCURRENTLY; only the final bin link needs the
    /// GStreamer archives, so it runs after the join. Both sides assume all
    /// cores and briefly oversubscribe — total CPU is unchanged.
    pub fn build(self) -> Result<Option<Utf8PathBuf>> {
        let gstreamer_only = self.gstreamer_only;
        let Some((sh, profile, build, stamp)) = self.prepare_impl(false)? else {
            return Ok(None);
        };
        let stamp = stamp.expect("prepare_impl(false) always defers the compile");
        let parallel = !gstreamer_only
            && target_os(&profile) == "linux"
            && profile.scope == StaticScope::Gstreamer;
        if parallel {
            let child = spawn_gst_compile(&build)?;
            match prebuild_receiver_deps(&sh, &build, &profile) {
                Ok(()) => join_gst_compile(child, &build, &stamp)?,
                Err(e) => {
                    // Cargo failed: reap ninja (safe to interrupt). No stamp
                    // is written, so the next run re-checks everything.
                    let mut child = child;
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            }
        } else {
            compile_gstreamer(&sh, &build, &profile, &stamp)?;
        }
        if gstreamer_only {
            return Ok(None);
        }
        build_receiver(&sh, &build, &profile).map(Some)
    }

    /// Build the static receiver and execute it, forwarding `args` (everything
    /// after `--`); propagates its exit code. Mirrors `cargo run`: debug build
    /// by default, `release` opts into the fat-LTO build, `--debug` wins.
    pub fn run_binary(mut self, args: Vec<String>, release: bool) -> Result<()> {
        self.debug = self.debug || !release;
        let Some((sh, profile, build)) = self.prepare()? else {
            return Ok(());
        };
        let bin = build_receiver(&sh, &build, &profile)?;
        println!(">> Running {bin} …");
        let status = std::process::Command::new(bin.as_std_path())
            .args(&args)
            .status()
            .with_context(|| format!("spawning receiver {bin}"))?;
        match status.code() {
            Some(0) | None => Ok(()),
            Some(code) => std::process::exit(code),
        }
    }

    /// `cargo check` the receiver against the static GStreamer. `extra` is
    /// appended to the inner cargo invocation (e.g. `--message-format=json`).
    pub fn check(self, extra: Vec<String>, release: bool) -> Result<()> {
        self.cargo_subcmd("check", extra, release)
    }

    /// `cargo clippy` the receiver against the static GStreamer.
    pub fn clippy(self, extra: Vec<String>, release: bool) -> Result<()> {
        self.cargo_subcmd("clippy", extra, release)
    }

    /// `cargo test` receiver-core against the static GStreamer. Unlike
    /// check/clippy this LINKS the test binary, so it needs the full
    /// gstreamer-full link line (see `link_args`) — not just the compile-time
    /// env. `extra` is forwarded to the inner cargo invocation (e.g. a test
    /// name filter or `-- --nocapture`).
    pub fn test(mut self, extra: Vec<String>, release: bool) -> Result<()> {
        // Like `cargo`, default to a fast debug build; `--release` opts into
        // the optimized profile (an explicit `--debug` also forces debug).
        self.debug = self.debug || !release;
        let Some((sh, mut profile, build)) = self.prepare()? else {
            return Ok(());
        };
        // Force an explicit --target so cargo splits the host/target build
        // graphs; the link-arg rustflags below then scope to the test binary
        // (and its target-side deps) and never touch host build scripts or
        // proc-macros — which must NOT be linked against the gstreamer archives.
        if profile.target.is_none() {
            profile.target = Some(host_triple(&sh)?);
        }
        receiver_test(&sh, &build, &profile, &extra)
    }

    fn cargo_subcmd(mut self, subcmd: &str, extra: Vec<String>, release: bool) -> Result<()> {
        // Like `cargo`, check/clippy default to a fast debug build; `--release`
        // opts into the optimized profile (an explicit `--debug` also forces debug).
        self.debug = self.debug || !release;
        let Some((sh, profile, build)) = self.prepare()? else {
            return Ok(());
        };
        receiver_cargo(&sh, &build, &profile, subcmd, &extra)
    }
}

/// Remove built/downloaded artifacts. Never deletes a user-provided --source
/// tree — only the build dir + prefix inside it. The auto-cloned source (which
/// we own) is removed wholesale.
fn clean(source: Option<&Utf8Path>, build_dir: Option<&Utf8Path>) -> Result<()> {
    let mut targets: Vec<Utf8PathBuf> = Vec::new();
    match source {
        // We own the auto-clone: nuke source + its builddir/prefix/subprojects.
        None => targets.push(Utf8PathBuf::from("target/gstreamer-src")),
        // User's tree: only our artifacts, never their checkout.
        Some(src) => {
            targets.push(src.join("builddir-static"));
            targets.push(src.join("prefix-static"));
        }
    }
    // An explicitly-set --build-dir may live outside the source tree.
    if let Some(bd) = build_dir {
        if !targets.iter().any(|t| t == bd) {
            targets.push(bd.to_owned());
        }
    }

    let mut removed = 0;
    for t in &targets {
        if t.exists() {
            std::fs::remove_dir_all(t).with_context(|| format!("removing {t}"))?;
            println!("removed {t}");
            removed += 1;
        }
    }
    if removed == 0 {
        println!("nothing to clean");
    }
    Ok(())
}

/// Resolve the GStreamer source when `--source` wasn't given: clone `gst_ref`
/// into target/gstreamer-src (reusing an existing clone). Refuses in offline
/// mode, where a source must be provided (and its subprojects vendored).
fn resolve_source(sh: &Rc<Shell>, gst_ref: &str, offline: bool) -> Result<Utf8PathBuf> {
    if offline {
        bail!("--offline requires --source <PATH> (cannot clone without network)");
    }
    // Absolute path: the shell runs git from the pushed root_path but std::fs
    // uses the process cwd — a relative path makes the two disagree.
    let dir = crate::workspace::root_path()?.join("target/gstreamer-src");
    if checkout_present(&dir) {
        // Reuse the clone; warn when it's on a different ref than requested.
        // A tag checkout is a detached HEAD, so also match the exact tag.
        let head = cmd!(sh, "git -C {dir} rev-parse --abbrev-ref HEAD")
            .quiet()
            .read()
            .unwrap_or_default();
        let tag = cmd!(sh, "git -C {dir} describe --tags --exact-match")
            .quiet()
            .ignore_stderr()
            .read()
            .unwrap_or_default();
        if head.trim() != gst_ref && tag.trim() != gst_ref {
            let current = if head.trim() == "HEAD" {
                tag.trim()
            } else {
                head.trim()
            };
            println!(
                ">> Reusing GStreamer checkout at {dir} (on '{current}', requested '{gst_ref}') — \
                 pass --clean first if you want a fresh clone",
            );
        } else {
            println!(">> Reusing GStreamer checkout at {dir}");
        }
    } else {
        println!(">> Cloning GStreamer {gst_ref} into {dir} …");
        if let Err(e) = cmd!(
            sh,
            "git clone --depth 1 --branch {gst_ref} {GST_REPO} {dir}"
        )
        .run()
        {
            // The presence probe can transiently false-negative on Windows
            // (see checkout_present) and the clone then fails on the existing
            // dir — reuse it; only a genuinely-absent dir is a real error.
            if !checkout_present(&dir) {
                return Err(e).context("cloning gstreamer source");
            }
            println!(">> {dir} already present — reusing existing checkout");
        }
    }
    Ok(dir)
}

/// Apply the in-repo GStreamer source patches to `source`, idempotently. These fix bugs in the
/// pinned release that we can't otherwise avoid — see each patch's header.
///
/// `xtask/patches/*.patch` apply to every build; `xtask/patches/<target-os>/*.patch` only when
/// building for that OS, so an OS-specific patch (e.g. applemedia) doesn't dirty a checkout used
/// for another OS's build and force needless rebuilds.
///
/// A reused checkout keeps the applied patch, so a reverse-apply check skips ones already present.
/// A patch that neither applies nor is already applied (e.g. a user-provided `--source` on a
/// different ref) is warned about and skipped rather than fatal, so custom source trees still build.
fn apply_gst_patches(sh: &Rc<Shell>, source: &Utf8Path, os: &str) -> Result<()> {
    let patches_root = crate::workspace::root_path()?.join("xtask/patches");

    let mut patches: Vec<Utf8PathBuf> = Vec::new();
    for dir in [patches_root.clone(), patches_root.join(os)] {
        if !dir.exists() {
            continue;
        }
        patches.extend(
            std::fs::read_dir(&dir)
                .with_context(|| format!("reading patches dir {dir}"))?
                .filter_map(|e| e.ok())
                .filter_map(|e| Utf8PathBuf::from_path_buf(e.path()).ok())
                .filter(|p| p.extension() == Some("patch")),
        );
    }
    patches.sort();

    for patch in patches {
        let name = patch.file_name().unwrap_or("<patch>");
        // --ignore-whitespace: a Windows CI checkout (core.autocrlf=true) turns
        // these LF patches into CRLF; ignoring the trailing CR keeps the apply
        // and the reverse-check idempotency EOL-agnostic against either source.
        // Already applied (reused checkout): reverse-apply must succeed cleanly.
        if cmd!(sh, "git -C {source} apply --ignore-whitespace --reverse --check {patch}")
            .quiet()
            .ignore_stderr()
            .run()
            .is_ok()
        {
            println!(">> gstreamer patch already applied, skipping: {name}");
            continue;
        }
        // Not applicable to this tree (different ref / already-diverged): warn, don't fail.
        if cmd!(sh, "git -C {source} apply --ignore-whitespace --check {patch}")
            .quiet()
            .ignore_stderr()
            .run()
            .is_err()
        {
            println!(">> WARNING: gstreamer patch does not apply cleanly, skipping: {name}");
            continue;
        }
        println!(">> Applying gstreamer patch: {name}");
        cmd!(sh, "git -C {source} apply --ignore-whitespace {patch}")
            .run()
            .with_context(|| format!("applying gstreamer patch {name}"))?;
    }

    Ok(())
}

/// Is the checkout present (dir exists, non-empty)? Retries: listing this
/// large tree can briefly fail on Windows (AV / open handles), and a false
/// "absent" would clone over a real checkout.
fn checkout_present(dir: &Utf8Path) -> bool {
    for i in 0..6 {
        match std::fs::read_dir(dir) {
            Ok(mut entries) => return entries.next().is_some(),
            Err(_) => {
                if i < 5 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }
    false
}

/// Result of a successful gstreamer build: the build tree we link against.
struct GstBuild {
    build_dir: Utf8PathBuf,
    /// dir holding the generated *-uninstalled.pc files.
    uninstalled_pc: Utf8PathBuf,
    /// the GStreamer source tree (for compile-time env recreation).
    source: Utf8PathBuf,
}

/// Target OS ("linux" | "macos" | "windows"), from `--target` if given, else host.
fn target_os(profile: &Profile) -> &'static str {
    os_from_target(profile.target.as_deref())
}

fn os_from_target(target: Option<&str>) -> &'static str {
    if let Some(t) = target {
        if t.contains("darwin") || t.contains("apple") {
            return "macos";
        }
        if t.contains("windows") {
            return "windows";
        }
        return "linux";
    }
    std::env::consts::OS // "linux" | "macos" | "windows"
}

/// Configure (meson setup) the static GStreamer without compiling. Returns
/// the build handle plus the config stamp to write after a successful
/// compile. The uninstalled .pc files exist once this returns.
fn configure_gstreamer(
    sh: &Rc<Shell>,
    source: &Utf8Path,
    build_dir: &Utf8Path,
    profile: &Profile,
) -> Result<(GstBuild, String)> {
    if !source.join("meson.build").exists() {
        bail!("{source} does not look like a GStreamer source tree (no meson.build)");
    }

    let os = target_os(profile);

    // The environment must supply the pkg-config closure; assert it up front
    // rather than dying with a cryptic meson failure deep in a subproject.
    if os == "linux" {
        let pkgcfg = pkg_config_prog(sh);
        let mut missing = Vec::new();
        for pc in REQUIRED_BUILD_PC_LINUX {
            if cmd!(sh, "{pkgcfg} --exists {pc}").quiet().run().is_err() {
                missing.push(*pc);
            }
        }
        if !missing.is_empty() {
            bail!(
                "missing pkg-config deps for the gstreamer build: {}\n\
                 Provide them via your build environment (devshell / Flatpak SDK).",
                missing.join(", ")
            );
        }
    }

    // scope=Full must be HERMETIC: everything from vendored wraps or OS
    // frameworks. A rich host install exposes dozens of optional libs via
    // pkg-config that `-Dgst-full-plugins=*` + `auto` features silently link —
    // dynamic deps that dangle on end-user machines. Blank out pkg-config for
    // the whole gstreamer build: unforced deps fall back to a vendored wrap or
    // auto-disable. PKG_CONFIG_LIBDIR pointed at a real empty dir also
    // overrides pkg-config's compiled-in default search path. Scoped to this
    // fn — build_receiver sets its own PKG_CONFIG_PATH.
    let _pc_isolate = (profile.scope == StaticScope::Full).then(|| {
        let empty = source.join(".xtask-empty-pkgconfig");
        let _ = std::fs::create_dir_all(&empty);
        (
            sh.push_env("PKG_CONFIG_PATH", ""),
            sh.push_env("PKG_CONFIG_LIBDIR", empty.as_str()),
        )
    });

    // Always from wraps: the decode-only FFmpeg fork; scope=Full adds the
    // glib/pango closure. NOTE: repeated --force-fallback-for flags override
    // each other — this must stay ONE flag.
    let mut fallback: Vec<&str> = vec!["libavcodec", "libavformat", "libavutil", "libavfilter"];
    if profile.scope == StaticScope::Full {
        fallback.extend(FULL_SCOPE_FALLBACK);
    }

    let mut args: Vec<String> = vec![
        "--prefix".into(),
        source.join("prefix-static").to_string(),
        format!("--buildtype={}", profile.gst_buildtype),
        "--default-library=static".into(),
        format!(
            "--wrap-mode={}",
            if profile.offline {
                "nodownload"
            } else {
                "default"
            }
        ),
        format!("--force-fallback-for={}", fallback.join(",")),
        "-Dgst-full-target-type=static_library".into(),
        "-Dgst-full-plugins=*".into(),
        // Element-level whitelist: plugins named here register ONLY the listed
        // elements (the rest of that plugin's element objects never link).
        // Generator syntax: plugin:elem,elem;plugin2:elem (see
        // scripts/generate_init_static_plugins.py).
        format!(
            "-Dgst-full-elements={}",
            FULL_ELEMENTS
                .iter()
                .chain(if os == "linux" {
                    FULL_ELEMENTS_LINUX
                } else {
                    &[]
                })
                .map(|(plugin, elems)| format!("{plugin}:{}", elems.join(",")))
                .collect::<Vec<_>>()
                .join(";")
        ),
        {
            // macOS zero-copy video needs libgstiosurface-1.0's ABI exposed by
            // gstreamer-full (its static .a is already built as an applemedia dep; this just
            // exports the symbols the receiver's hand-written FFI binds). macOS-only: the
            // library does not exist on linux/windows.
            let mut full_libraries: Vec<&str> = FULL_LIBRARIES.to_vec();
            if target_os(profile) == "macos" {
                full_libraries.push("gstreamer-iosurface-1.0");
            }
            format!("-Dgst-full-libraries={}", full_libraries.join(","))
        },
        "-Dlibav=enabled".into(),
        // subsystems we never need
        "-Drs=disabled".into(),
        "-Dgpl=disabled".into(),
        "-Dges=disabled".into(),
        "-Drtsp_server=disabled".into(),
        "-Ddevtools=disabled".into(),
        "-Dexamples=disabled".into(),
        "-Dtests=disabled".into(),
        "-Dbenchmarks=disabled".into(),
        "-Dtools=disabled".into(),
        "-Ddoc=disabled".into(),
        "-Dintrospection=disabled".into(),
        "-Dnls=disabled".into(),
        "-Dqt5=disabled".into(),
        "-Dqt6=disabled".into(),
        "-Dgtk_doc=disabled".into(),
        // Decode-only FFmpeg with a decoder whitelist (see below);
        // demuxers/protocols come from native gst elements.
        "-DFFmpeg:encoders=disabled".into(),
        "-DFFmpeg:muxers=disabled".into(),
        "-DFFmpeg:programs=disabled".into(),
        "-DFFmpeg:tests=disabled".into(),
        "-DFFmpeg:decoders=disabled".into(),
        "-DFFmpeg:demuxers=disabled".into(),
        "-DFFmpeg:protocols=disabled".into(),
        // All ~450 avfilters are dead weight (native gst elements filter for
        // us) and dominate the build's serial tail. libavfilter itself still
        // builds — gst-libav hard-requires the library; avdeinterlace loses
        // its backend, native deinterlace covers it.
        "-DFFmpeg:filters=disabled".into(),
        // FFmpeg auto-detects system bz2 (compressed-matroska, extremely rare)
        // and links it dynamically — no bz2 wrap exists, so drop it.
        "-DFFmpeg:bzlib=disabled".into(),
        // gst-libav uses neither parsers nor bsfs (native parse elements feed
        // the decoders aligned frames); both lists are referenced from
        // libavcodec's registry, so unlike unused decoders they would not
        // drop out at link time.
        "-DFFmpeg:parsers=disabled".into(),
        "-DFFmpeg:bsfs=disabled".into(),
        // Wrap-built deps compile their own tests/example programs by default
        // — pure waste for libraries we only link. Harmless when the dep
        // resolves from the system: meson just warns about the unused option.
        "-Dopus:tests=disabled".into(),
        "-Dopus:extra-programs=disabled".into(),
        "-Dopus:docs=disabled".into(),
        // NB: libsoup's tests option is a boolean, not a feature. A wrong
        // value TYPE doesn't error the build — meson treats the subproject as
        // failed-to-configure and SILENTLY disables everything depending on
        // it (soup + adaptivedemux2 vanish from the binary).
        "-Dlibsoup:tests=false".into(),
        "-Dlibsoup:docs=disabled".into(),
        "-Dlibsoup:sysprof=disabled".into(),
        // libxml2 has exactly one consumer: adaptivedemux2's DASH MPD parser,
        // which needs the core tree/parser API plus the `output` module.
        // minimum=true turns off every feature not explicitly enabled,
        // roughly halving compile time and footprint.
        "-Dlibxml2:minimum=true".into(),
        "-Dlibxml2:output=enabled".into(),
        "-Dlibxml2:threads=enabled".into(),
        // sqlite: libsoup hard-requires it but nothing ever reaches it (the
        // cookie-jar/HSTS-DB objects are unreferenced, so sqlite never links
        // into the binary). Its only cost is compiling the huge amalgamation
        // TU — much faster at -O1, and the code never runs anyway.
        "-Dsqlite3:optimization=1".into(),
        // AV1 decode is the Rust gst-plugin-dav1d (dav1d-sys). With
        // rs=disabled nothing in the meson build requests dav1d, so the wrap
        // never builds and dav1d-sys links a dynamic libdav1d. Enabling
        // FFmpeg's libdav1d makes meson request dependency('dav1d'), which
        // force-fallback builds static — the libdav1d.a + dav1d-uninstalled.pc
        // that dav1d-sys then links.
        "-DFFmpeg:libdav1d=enabled".into(),
    ];
    for dec in FFMPEG_DECODERS {
        args.push(format!("-DFFmpeg:{dec}_decoder=enabled"));
    }
    for comp in FFMPEG_COMPONENTS {
        args.push(format!("-DFFmpeg:{comp}=enabled"));
    }

    // Per-function/data sections let the final link's --gc-sections (rustc
    // passes it by default) drop everything unreferenced; without them GC
    // granularity is a whole object file. Skipped for MSVC (`cl` spells it
    // /Gy//Gw; the experimental Windows path is left untouched).
    let mut c_args: Vec<String> = Vec::new();
    if os != "windows" {
        c_args.push("-ffunction-sections".into());
        c_args.push("-fdata-sections".into());
        args.push("-Dcpp_args=-ffunction-sections -fdata-sections".into());
    }

    // vorbis/theora headers include <ogg/ogg.h>, but ogg is only in their
    // .pc's Requires.private, whose include dirs pkgconf doesn't propagate to
    // `--cflags` — on split-prefix systems the compiler can't find ogg.h, so
    // pass the include dirs explicitly (harmless elsewhere). Linux-only: on
    // mac/win these come from wraps, and injecting a pkg-config path breaks
    // MSVC (backslash-escaped spaces that `cl` mis-splits).
    if os == "linux" {
        let pkgcfg = pkg_config_prog(sh);
        if let Ok(ogg_cflags) = cmd!(sh, "{pkgcfg} --cflags-only-I ogg").quiet().read() {
            let ogg_cflags = ogg_cflags.trim();
            if !ogg_cflags.is_empty() {
                c_args.push(ogg_cflags.to_string());
            }
        }
    }
    if !c_args.is_empty() {
        args.push(format!("-Dc_args={}", c_args.join(" ")));
    }

    let (enable_os, disable_os): (&[(Plugins, &str)], &[(Plugins, &str)]) = match os {
        "macos" => (ENABLE_MACOS, DISABLE_MACOS),
        "windows" => (ENABLE_WINDOWS, DISABLE_WINDOWS),
        _ => (ENABLE_LINUX, DISABLE_LINUX),
    };
    for (sub, plugin) in DISABLE_COMMON.iter().chain(disable_os) {
        args.push(format!("-D{}:{plugin}=disabled", sub.prefix()));
    }
    for (sub, plugin) in ENABLE_COMMON.iter().chain(enable_os) {
        args.push(format!("-D{}:{plugin}=enabled", sub.prefix()));
    }

    // Cross-language LTO: emit LLVM bitcode on the C side.
    if profile.lto == Lto::Cross {
        args.push("-Db_lto=true".into());
        args.push("-Db_lto_mode=thin".into());
    }

    // Full-static scope: glib + pango from wraps (FULL_SCOPE_FALLBACK) so
    // there is a single static glib. NOT Flatpak-compatible.
    if profile.scope == StaticScope::Full {
        // With glib internal, the monorepo builds glib-networking as a
        // subproject and statically links its GIO TLS module into
        // gstreamer-full; gst_init_static_plugins() registers it via
        // g_io_<module>_load() — https works with no runtime GIO modules.
        // gnutls has no wrap, so use the openssl backend.
        args.push("-Dtls=enabled".into());
        args.push("-Dglib-networking:gnutls=disabled".into());
        args.push("-Dglib-networking:openssl=enabled".into());
        args.push("-Dglib-networking:libproxy=disabled".into());
        args.push("-Dglib-networking:gnome_proxy=disabled".into());
        // Keep glib lean; introspection would drag in the
        // gobject-introspection wrap (unbuildable on mac/win anyway).
        args.push("-Dglib:tests=false".into());
        args.push("-Dglib:introspection=disabled".into());
        args.push("-Dpango:introspection=disabled".into());
        // openssl: glib-networking's TLS backend (gnutls has no wrap).
        ensure_wrap(sh, source, profile, "openssl")?;
        // libnice's DTLS backend on `auto` prefers gnutls, which it can find
        // via meson's cmake fallback (pkg-config isolation doesn't cover
        // cmake) and link dynamically — an @rpath dylib the installer
        // rejects. Force openssl → the static wrap we already build.
        args.push("-Dlibnice:crypto-library=openssl".into());
        // cairo `auto` features turn ON whenever the build host exposes the
        // lib, pulling deps a mac/win text stack never needs (and X11-only
        // sources / broken host libs). Force off; pango only needs the
        // quartz/image surfaces.
        args.push("-Dcairo:xlib=disabled".into());
        args.push("-Dcairo:xcb=disabled".into());
        args.push("-Dcairo:lzo=disabled".into()); // cairo-script compression
        args.push("-Dcairo:spectre=disabled".into()); // PS preview
        args.push("-Dcairo:symbol-lookup=disabled".into()); // binutils/bfd
        args.push("-Dcairo:tests=disabled".into());
    }

    // Compiler selection for the gstreamer C/C++ build.
    //  - Windows: MSVC `cl` (via the vcvars import below). Countless meson
    //    checks in the wrap ecosystem gate Windows behaviour on
    //    `cc.get_id() == 'msvc'`, which only `cl` satisfies.
    //  - macOS and cross-LTO: clang/clang++. Cross-LTO needs LLVM bitcode,
    //    and on macOS the C++ runtime must be libc++ — a non-Apple gcc/g++ on
    //    PATH makes C++ wraps emit `-lstdc++`, which doesn't exist there.
    //    (link_args also rewrites any stray -lstdc++ → -lc++.)
    //  - elsewhere (Linux): an exported CC/CXX is folded in rather than left
    //    for meson to read, so the ccache wrap below applies to it and the
    //    stamp sees it change.
    let (cc, cxx) = if os == "windows" {
        (Some("cl".to_string()), Some("cl".to_string()))
    } else if profile.lto == Lto::Cross || os == "macos" {
        (Some("clang".to_string()), Some("clang++".to_string()))
    } else {
        (
            sh.var("CC").ok().filter(|v| !v.is_empty()),
            sh.var("CXX").ok().filter(|v| !v.is_empty()),
        )
    };
    // Wrap the compiler in ccache when it's on PATH: the wipe-on-change path
    // below then recompiles from cache instead of from scratch. meson's own
    // ccache auto-detection can't be relied on (distro-patched mesons skip
    // it, and an exported CC suppresses it), so wrap explicitly, falling back
    // to the first default compiler on PATH when nothing chose one. Not for
    // `cl` (the experimental MSVC path is left untouched).
    let ccache = os != "windows" && on_path(sh, "ccache");
    let ccache_wrap = |c: Option<String>, defaults: &[&str]| match (c, ccache) {
        (Some(c), true) if !c.contains("ccache") => Some(format!("ccache {c}")),
        (None, true) => defaults
            .iter()
            .find(|c| on_path(sh, c))
            .map(|c| format!("ccache {c}")),
        (c, _) => c,
    };
    let (cc, cxx) = (
        ccache_wrap(cc, &["cc", "gcc", "clang"]),
        ccache_wrap(cxx, &["c++", "g++", "clang++"]),
    );

    // Patch subproject checkouts that already exist so setup sees them;
    // wraps that setup downloads fresh are covered by the second pass below.
    apply_subproject_patches(sh, source)?;

    // meson captures PKG_CONFIG_PATH and the compilers at first setup and
    // ignores env changes on --reconfigure, so start over when they changed.
    // When NOTHING changed, skip `meson setup` entirely — reconfiguring costs
    // minutes for no effect; ninja alone detects source changes fine.
    let stamp = format!(
        "{}\n{}\n{}\n{}",
        pkg_config_path(sh),
        cc.as_deref().unwrap_or_default(),
        cxx.as_deref().unwrap_or_default(),
        args.join(" ")
    );
    let configured = build_dir.join("meson-private/coredata.dat").exists();
    let reconf = if configured && stamp_read(build_dir).as_deref() == Some(stamp.as_str()) {
        None
    } else {
        if configured {
            // Delete rather than `meson setup --wipe`: --wipe restores the
            // ORIGINAL configure's environment (CC/CXX/PKG_CONFIG_PATH), so
            // e.g. a compiler change would silently not take effect.
            println!(">> Build environment/options changed — deleting build dir");
            std::fs::remove_dir_all(build_dir)
                .with_context(|| format!("removing stale build dir {build_dir}"))?;
        }
        Some("--reconfigure") // fresh dir: acts as plain setup
    };

    // Assemble the build environment. Each var is pushed exactly once — pushing
    // the same key twice would clobber it — so PATH is fully composed here first.
    let mut build_env: Vec<(String, String)> = Vec::new();
    let mut path = sh.var("PATH").unwrap_or_default();

    // Windows: import the MSVC developer environment from vcvars64. This puts
    // `cl` (and dumpbin/link, needed by FFmpeg's makedef) on PATH and points
    // the compiler at the Windows SDK headers/libs (INCLUDE/LIB) — meson's
    // find_library and the SDK includes fail without them.
    #[cfg(windows)]
    if os == "windows" {
        for (k, v) in vcvars_env(sh)? {
            if k.eq_ignore_ascii_case("PATH") {
                path = v; // already includes our original PATH plus the MSVC bins
            } else {
                build_env.push((k, v));
            }
        }
    }

    // clang (cross-LTO) may need the standalone LLVM bin prepended; `cl` comes
    // from the vcvars import above, and macOS uses Apple's clang already on PATH.
    if cc.as_deref().is_some_and(|c| c.ends_with("clang")) && !on_path(sh, "clang") {
        let dir =
            find_llvm_bin().context("clang not on PATH and no LLVM install found; install LLVM")?;
        path = prepend_env_path(&path, dir.as_str());
    }

    build_env.push(("PATH".to_string(), path));
    let _build_env: Vec<_> = build_env
        .into_iter()
        .map(|(k, v)| sh.push_env(k, v))
        .collect();

    // The compiler is passed BOTH via CC/CXX env and a meson native file:
    // distro-patched mesons can ignore compiler env vars entirely (verified —
    // even CC=/nonexistent configures happily), so the native file is what
    // reliably selects the compiler; env covers unpatched mesons and the
    // tools meson invokes underneath. Not on Windows: `cl` comes from the
    // vcvars environment.
    let native_file = match (&cc, &cxx) {
        (None, None) => None,
        _ if os == "windows" => None,
        (cc, cxx) => {
            let mut ini = String::from("[binaries]\n");
            for (key, val) in [("c", cc), ("cpp", cxx)] {
                if let Some(v) = val {
                    let words: Vec<String> =
                        v.split_whitespace().map(|w| format!("'{w}'")).collect();
                    ini.push_str(&format!("{key} = [{}]\n", words.join(", ")));
                }
            }
            let path = source.join(".xtask-native.ini");
            // Write only on change: meson tracks the native file as a regen
            // dependency, so a fresh mtime every run would make ninja
            // needlessly regenerate build files.
            if std::fs::read_to_string(&path).ok().as_deref() != Some(&ini) {
                std::fs::write(&path, ini).context("writing meson native file")?;
            }
            Some(path)
        }
    };
    let native_args: Vec<String> = native_file
        .iter()
        .flat_map(|f| vec!["--native-file".to_string(), f.to_string()])
        .collect();

    let _cc = cc.map(|c| sh.push_env("CC", c));
    let _cxx = cxx.map(|c| sh.push_env("CXX", c));

    // Windows' installer-detection heuristic flags `patch.exe` (which meson
    // runs to apply wrap diffs) as needing UAC elevation → CreateProcess
    // fails with WinError 740. RunAsInvoker makes children inherit our
    // unelevated token; patch.exe doesn't actually need elevation.
    #[cfg(windows)]
    let _no_elevate = sh.push_env("__COMPAT_LAYER", "RunAsInvoker");

    let cross = profile
        .target
        .as_ref()
        .map(|t| cross_file(sh, source, t))
        .transpose()?;
    let cross_args: Vec<String> = cross
        .iter()
        .flat_map(|f| vec!["--cross-file".to_string(), f.to_string()])
        .collect();

    if let Some(reconf) = reconf {
        println!(">> Configuring static GStreamer ({reconf}) …");
        cmd!(
            sh,
            "meson setup {build_dir} {source} {reconf} {native_args...} {cross_args...} {args...}"
        )
        .run()?;
        // Wraps download at setup: patch anything that just appeared. The
        // changed meson.build mtime makes the next `meson compile`
        // regenerate, so a patch landing here still takes effect this build.
        apply_subproject_patches(sh, source)?;
    } else {
        println!(">> GStreamer configuration unchanged — skipping meson setup");
    }

    Ok((
        GstBuild {
            build_dir: build_dir.to_owned(),
            uninstalled_pc: build_dir.join("meson-uninstalled"),
            source: source.to_owned(),
        },
        stamp,
    ))
}

/// `meson compile` + stamp write (only after success, so a failed build
/// re-runs the setup check next time). Split from `configure_gstreamer` so
/// `build()` can instead run the compile in the background (spawn/join).
fn compile_gstreamer(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    stamp: &str,
) -> Result<()> {
    // `meson compile` can trigger a regenerate; scope=Full must not see host
    // pkg-config then (same isolation and values as configure — no stamp drift).
    let _pc_isolate = (profile.scope == StaticScope::Full).then(|| {
        let empty = build.source.join(".xtask-empty-pkgconfig");
        let _ = std::fs::create_dir_all(&empty);
        (
            sh.push_env("PKG_CONFIG_PATH", ""),
            sh.push_env("PKG_CONFIG_LIBDIR", empty.as_str()),
        )
    });
    println!(">> Building GStreamer …");
    let build_dir = &build.build_dir;
    cmd!(sh, "meson compile -C {build_dir}").run()?;
    // Record env + options so the next run can detect changes.
    stamp_write(build_dir, stamp)
}

/// Spawn the GStreamer compile in the background (output → xtask-ninja.log).
/// Only used on Linux scope=Gstreamer, where the plain process env is
/// faithful (no pkg-config isolation, no MSVC dev env).
fn spawn_gst_compile(build: &GstBuild) -> Result<std::process::Child> {
    let log_path = build.build_dir.join("xtask-ninja.log");
    println!(">> Building GStreamer in the background … (log: {log_path})");
    let log = std::fs::File::create(&log_path).with_context(|| format!("creating {log_path}"))?;
    std::process::Command::new("meson")
        .args(["compile", "-C", build.build_dir.as_str()])
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .context("spawning background meson compile")
}

/// Wait for the background compile; on failure surface the log tail. Writes
/// the stamp only on success (mirrors `compile_gstreamer`).
fn join_gst_compile(mut child: std::process::Child, build: &GstBuild, stamp: &str) -> Result<()> {
    let status = child
        .wait()
        .context("waiting for background meson compile")?;
    if !status.success() {
        let log_path = build.build_dir.join("xtask-ninja.log");
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(50).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "GStreamer build failed (full log: {log_path}):\n{}",
            tail.join("\n")
        );
    }
    stamp_write(&build.build_dir, stamp)
}

/// While ninja builds GStreamer, pre-build the receiver's Rust dependency
/// graph. Build scripts only need the uninstalled .pc files (written at
/// `meson setup`); nothing reads the .a archives until the final `cargo
/// rustc` link of the bin crate, which `build_receiver` runs after the join.
/// receiver-core covers every dependency except the bin crate itself. The
/// features must mirror what the desktop-receiver build enables on rcore —
/// a mismatch only costs recompilation, never correctness.
fn prebuild_receiver_deps(sh: &Rc<Shell>, build: &GstBuild, profile: &Profile) -> Result<()> {
    let mut features = String::from("static-gstreamer,desktop");
    if !profile.no_default_features {
        features.push_str(",systray");
    }
    let mut flags: Vec<String> = vec!["--profile".into(), profile.cargo_profile.clone()];
    flags.extend([
        "-p".into(),
        "receiver-core".into(),
        "--features".into(),
        features,
    ]);
    if let Some(t) = &profile.target {
        flags.push("--target".into());
        flags.push(t.clone());
    }
    with_receiver_env(sh, build, profile, || {
        println!(">> Pre-building receiver deps (concurrent with GStreamer) …");
        cmd!(sh, "cargo build {flags...}").run()?;
        Ok(())
    })
}

fn pkg_config_path(sh: &Rc<Shell>) -> String {
    sh.var("PKG_CONFIG_PATH").unwrap_or_default()
}

/// The pkg-config program to invoke: `$PKG_CONFIG` (system-deps' knob; may be
/// `pkgconf` where no `pkg-config` binary exists), else `pkg-config`.
fn pkg_config_prog(sh: &Rc<Shell>) -> String {
    sh.var("PKG_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "pkg-config".to_string())
}

/// Subproject source patches, applied idempotently around `meson setup`:
/// (dir under xtask/patches/, wrap name). The target directory comes from
/// the wrap's `directory =` key so wrap version bumps keep resolving; a
/// patch that then no longer applies is a HARD error — re-derive it against
/// the new source instead of silently building unpatched.
/// NB xtask/patches/gstreamer/ is deliberately not listed: the playbin3
/// TEXT-flag patch kept there hangs preroll (reference only).
const SUBPROJECT_PATCHES: &[(&str, &str)] = &[
    ("ffmpeg", "FFmpeg"),   // no nasm DWARF in release (~300s CPU, tail item)
    ("libxml2", "libxml2"), // skip tools/tests/examples (~110s CPU)
    ("flac", "flac"),       // skip the flac/metaflac command-line tools
    ("libnice", "libnice"), // skip the stund/stunbdc tools
];

fn apply_subproject_patches(sh: &Rc<Shell>, source: &Utf8Path) -> Result<()> {
    let patches_root = crate::workspace::root_path()?.join("xtask/patches");
    for (patch_dir, wrap) in SUBPROJECT_PATCHES {
        let patch_dir = patches_root.join(patch_dir);
        if !patch_dir.is_dir() {
            continue;
        }
        let wrap_file = source.join(format!("subprojects/{wrap}.wrap"));
        let dir_name = std::fs::read_to_string(&wrap_file)
            .ok()
            .and_then(|w| {
                w.lines().find_map(|l| {
                    let (k, v) = l.split_once('=')?;
                    (k.trim() == "directory").then(|| v.trim().to_string())
                })
            })
            .unwrap_or_else(|| wrap.to_string());
        let target = source.join("subprojects").join(&dir_name);
        if !target.is_dir() {
            continue; // not downloaded yet — the post-setup pass gets it
        }
        let mut patches: Vec<Utf8PathBuf> = Vec::new();
        for entry in
            std::fs::read_dir(&patch_dir).with_context(|| format!("reading {patch_dir}"))?
        {
            let p = Utf8PathBuf::try_from(entry?.path())
                .map_err(|e| anyhow::anyhow!("non-UTF8 patch path: {e}"))?;
            if p.extension() == Some("patch") {
                patches.push(p);
            }
        }
        patches.sort();
        // Run from the SOURCE ROOT with --directory: inside a git worktree,
        // `git apply` resolves patch paths against the repo root and SILENTLY
        // SKIPS (exit 0!) anything outside the cwd — cd'ing into an extracted
        // (non-repo) subproject dir therefore no-ops. --directory pins the
        // prefix explicitly and behaves the same for git checkouts (FFmpeg)
        // and extracted archives.
        let dir_arg = format!("--directory=subprojects/{dir_name}");
        // --ignore-whitespace treats a trailing CR as whitespace, so an
        // LF-only patch matches regardless of the source's line endings. On a
        // Windows CI the repo checks out with core.autocrlf=true, turning these
        // patch files into CRLF; git-wrap sources (FFmpeg) are CRLF too and
        // match, but tarball-wrap sources (libxml2) stay LF and the CRLF patch
        // then fails to apply. Ignoring the CR makes it EOL-agnostic both ways.
        for patch in patches {
            let _d = sh.push_dir(source);
            // A patch that applies cleanly in REVERSE is already present.
            if cmd!(sh, "git apply --ignore-whitespace --check --reverse {dir_arg} {patch}")
                .quiet()
                .ignore_stderr()
                .run()
                .is_ok()
            {
                continue;
            }
            cmd!(sh, "git apply --ignore-whitespace {dir_arg} {patch}")
                .quiet()
                .run()
                .with_context(|| {
                    format!(
                        "applying {patch} to {target} — if the subproject version \
                         changed, re-derive the patch against the new source"
                    )
                })?;
            // Belt and suspenders against the silent-skip failure mode: after
            // a successful apply the reverse-check must pass.
            if cmd!(sh, "git apply --ignore-whitespace --check --reverse {dir_arg} {patch}")
                .quiet()
                .ignore_stderr()
                .run()
                .is_err()
            {
                bail!(
                    "{patch} reported success but did not modify {target} \
                     (git apply silently skipped it)"
                );
            }
            println!(
                ">> Patched {dir_name}: {}",
                patch.file_name().unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// Ensure a wrap the monorepo doesn't vendor is present (from wrapdb).
/// `meson wrap install` only drops the .wrap file; the source download
/// happens at setup time like every other wrap.
fn ensure_wrap(sh: &Rc<Shell>, source: &Utf8Path, profile: &Profile, name: &str) -> Result<()> {
    if source.join(format!("subprojects/{name}.wrap")).exists() {
        return Ok(());
    }
    if profile.offline {
        bail!(
            "subprojects/{name}.wrap is required but missing; vendor it \
             (`meson wrap install {name}`) before an --offline build"
        );
    }
    let _d = sh.push_dir(source);
    cmd!(sh, "meson wrap install {name}")
        .run()
        .with_context(|| format!("installing the {name} wrap from wrapdb"))?;
    Ok(())
}

fn stamp_path(build_dir: &Utf8Path) -> Utf8PathBuf {
    build_dir.join(".xtask-pkgconfig-path")
}
fn stamp_read(build_dir: &Utf8Path) -> Option<String> {
    std::fs::read_to_string(stamp_path(build_dir)).ok()
}
fn stamp_write(build_dir: &Utf8Path, value: &str) -> Result<()> {
    std::fs::write(stamp_path(build_dir), value).context("writing pkgconfig stamp")
}

/// Placeholder for cross builds: generate/point at a meson cross file for the
/// target. Native builds return None. (Fill in when adding mac/win/cross.)
fn cross_file(_sh: &Rc<Shell>, _source: &Utf8Path, target: &str) -> Result<Utf8PathBuf> {
    bail!("cross-compiling gstreamer to {target} is not wired up yet (phase 1 = host/Linux)");
}

/// Path of the receiver binary a build with `profile` produces.
fn receiver_bin_path(profile: &Profile) -> Utf8PathBuf {
    let mut bin = Utf8PathBuf::from("target");
    if let Some(t) = &profile.target {
        bin.push(t);
    }
    bin.push(profile.target_subdir());
    bin.push(if target_os(profile) == "windows" {
        "desktop-receiver.exe"
    } else {
        "desktop-receiver"
    });
    bin
}

/// Common `--features`/`--target`/profile flags shared by every cargo
/// invocation that targets the receiver against the static gstreamer.
fn receiver_cargo_flags(profile: &Profile, package: &str) -> Vec<String> {
    let mut flags = vec!["--profile".to_owned(), profile.cargo_profile.clone()];
    flags.extend([
        "-p".into(),
        package.to_owned(),
        "--features".into(),
        "static-gstreamer".into(),
    ]);
    if profile.no_default_features {
        flags.push("--no-default-features".into());
    }
    if let Some(t) = &profile.target {
        flags.push("--target".into());
        flags.push(t.clone());
    }
    flags
}

/// Set up the env cargo needs against the static gstreamer (PKG_CONFIG_PATH
/// to the meson-uninstalled .pc + stubs, `SYSTEM_DEPS_*_LINK=static`), then
/// run `f`. Shared by build/run/check/clippy so build-script fingerprints
/// match and cargo's cache is shared.
fn with_receiver_env<T>(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    // Windows: build the receiver's C deps with clang-cl inside the MSVC dev
    // env. libplacebo passes gcc-style flags that `cl` rejects but clang-cl
    // accepts; gstreamer itself is built separately with `cl` (its wraps key
    // off the `msvc` compiler id) — both emit MSVC-ABI archives that link
    // together. Applied here so check/clippy/run share the same env.
    #[cfg(windows)]
    let _msvc_env: Vec<_> = if target_os(profile) == "windows" {
        let mut env = vcvars_env(sh)?;
        // The standalone LLVM installer isn't on PATH by default; prepend it to
        // the (vcvars) PATH so meson/cc find clang-cl.
        if !on_path(sh, "clang-cl") {
            if let Some(dir) = find_llvm_bin() {
                for (k, v) in env.iter_mut() {
                    if k.eq_ignore_ascii_case("PATH") {
                        *v = prepend_env_path(v, dir.as_str());
                    }
                }
            }
        }
        env.push(("CC".to_string(), "clang-cl".to_string()));
        env.push(("CXX".to_string(), "clang-cl".to_string()));
        env.into_iter().map(|(k, v)| sh.push_env(k, v)).collect()
    } else {
        Vec::new()
    };

    // Link against the BUILD TREE via meson-uninstalled .pc (the install tree
    // omits per-plugin .pc, so the gstreamer-full aggregate can't resolve there).
    let mut pkg_path = prepend_env_path(&pkg_config_path(sh), build.uninstalled_pc.as_str());

    // LINK PHASE ONLY: some distros ship a glib-2.0.pc whose Requires.private
    // lists sysprof-capture-4 without shipping its .pc or lib (the code is in
    // libglib). `pkg-config --static` recurses Requires.private, so resolving
    // gstreamer-full fails without it; an empty stub satisfies the resolver
    // with zero link impact. It MUST NOT be visible during the meson build —
    // subprojects treat sysprof-capture-4 as a real optional feature and try
    // to compile against its (nonexistent) headers.
    let pkgcfg = pkg_config_prog(sh);
    if cmd!(sh, "{pkgcfg} --exists sysprof-capture-4")
        .quiet()
        .run()
        .is_err()
    {
        let stub_dir = build.build_dir.join("xtask-pc-stubs");
        std::fs::create_dir_all(&stub_dir).context("creating pc stub dir")?;
        std::fs::write(
            stub_dir.join("sysprof-capture-4.pc"),
            "Name: sysprof-capture-4\n\
             Description: Stub to satisfy glib-2.0 Requires.private (no separate lib exists)\n\
             Version: 3.38.0\n\
             Libs:\n\
             Cflags:\n",
        )
        .context("writing sysprof-capture-4 stub")?;
        pkg_path = prepend_env_path(&pkg_path, stub_dir.as_str());
    }
    let _pc = sh.push_env("PKG_CONFIG_PATH", &pkg_path);

    // Debug/profiling profiles keep frame pointers so `perf record
    // --call-graph fp` resolves Rust frames (rustc omits them even at
    // opt-level 0; the static gstreamer C side already keeps them). Appended
    // via RUSTFLAGS because a `cargo rustc` arg after `--` would only cover
    // the final crate, and applied here so build/check/clippy share unit
    // fingerprints. Plain `cargo build/test` outside xtask doesn't set this,
    // so alternating the two rebuilds shared dev-profile deps.
    let _fp = matches!(profile.cargo_profile.as_str(), "dev" | "release-dbg").then(|| {
        let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
        if !flags.contains("force-frame-pointers") {
            if !flags.is_empty() {
                flags.push(' ');
            }
            flags.push_str("-Cforce-frame-pointers=yes");
        }
        sh.push_env("RUSTFLAGS", flags)
    });

    // Tell system-deps to link the gstreamer libs statically.
    let mut guards = Vec::new();
    for dep in SYSTEM_DEPS {
        guards.push(sh.push_env(format!("SYSTEM_DEPS_{dep}_LINK"), "static"));
    }
    if profile.scope == StaticScope::Full {
        for dep in SYSTEM_DEPS_FULL_SCOPE {
            guards.push(sh.push_env(format!("SYSTEM_DEPS_{dep}_LINK"), "static"));
        }
        // dav1d-sys resolves dav1d via pkg-config and would pick up a DYNAMIC
        // libdav1d — a runtime dep the static build must not have. Pin it to
        // the libdav1d.a we already built: NO_PKG_CONFIG bypasses resolution
        // entirely (dav1d-sys ships pregenerated bindings, needs no headers,
        // and its version check would reject the wrap's older dav1d). The
        // fresh env vars also re-fingerprint its build script.
        let archives = find_archives(&build.build_dir)?;
        if let Some(a) = archives.get("libdav1d.a") {
            let search = if target_os(profile) == "windows" {
                // rustc's `static=dav1d` links `dav1d.lib`, but meson named the
                // archive `libdav1d.a`; hand link.exe a copy under that name.
                let libdir = build.build_dir.join("xtask-dav1d-lib");
                std::fs::create_dir_all(&libdir).context("creating dav1d lib dir")?;
                std::fs::copy(a, libdir.join("dav1d.lib"))
                    .context("copying libdav1d.a to dav1d.lib")?;
                libdir
            } else {
                Utf8Path::new(a)
                    .parent()
                    .map(|p| p.to_owned())
                    .unwrap_or_else(|| build.build_dir.clone())
            };
            guards.push(sh.push_env("SYSTEM_DEPS_DAV1D_NO_PKG_CONFIG", "1"));
            guards.push(sh.push_env("SYSTEM_DEPS_DAV1D_SEARCH_NATIVE", search.as_str()));
            guards.push(sh.push_env("SYSTEM_DEPS_DAV1D_LIB", "dav1d"));
        }
    }

    f()
}

/// Build the receiver against the static gstreamer; returns the binary path.
fn build_receiver(sh: &Rc<Shell>, build: &GstBuild, profile: &Profile) -> Result<Utf8PathBuf> {
    with_receiver_env(sh, build, profile, || {
        let link_args = link_args(sh, build, profile)?;

        // cargo rustc scopes the link args to the FINAL binary (RUSTFLAGS
        // would hit every crate incl. build scripts / proc-macros).
        let mut cargo: Vec<String> = vec!["rustc".into()];
        cargo.extend(receiver_cargo_flags(profile, "desktop-receiver"));
        cargo.push("--".into());

        // Args for the final crate's rustc (after `--`). Cross-LTO drives the
        // LLVM LTO plugin via clang/lld; rust-only keeps the workspace's fat LTO.
        let mut rustc_args: Vec<String> = Vec::new();
        if profile.lto == Lto::Cross {
            rustc_args.push("-Clinker-plugin-lto".into());
            rustc_args.push("-Clinker=clang".into());
            rustc_args.push("-Clink-arg=-fuse-ld=lld".into());
        }
        for a in &link_args {
            rustc_args.push(format!("-Clink-arg={a}"));
        }

        // Windows caps a command line near 32 KiB; the static link line blows
        // past it. Hand the rustc args off via `@argfile` (one arg per line);
        // rustc in turn response-files the linker itself.
        if target_os(profile) == "windows" {
            let argfile = build.build_dir.join("xtask-rustc-args.txt");
            std::fs::write(&argfile, rustc_args.join("\n")).context("writing rustc argfile")?;
            cargo.push(format!("@{argfile}"));
        } else {
            cargo.extend(rustc_args);
        }

        // The link line carries ~100 `-Clink-arg=<abspath>.a` tokens — echoing
        // it every build is noise. Print the cargo flags up to `--` and
        // summarise the rest; `.quiet()` only hides the echo, not cargo output.
        let hidden = cargo
            .iter()
            .position(|a| a == "--")
            .map_or(0, |i| cargo.len() - i - 1);
        let shown = cargo
            .iter()
            .take_while(|a| a.as_str() != "--")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        println!(">> Building desktop-receiver (static gstreamer) …");
        println!(">> cargo {shown} -- <{hidden} link args hidden>");
        cmd!(sh, "cargo {cargo...}").quiet().run()?;
        Ok(())
    })?;
    Ok(receiver_bin_path(profile))
}

/// Run `cargo <subcmd>` (check/clippy) against the static gstreamer. No link
/// args: these don't produce the final binary, only the compile-time env.
fn receiver_cargo(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    subcmd: &str,
    extra: &[String],
) -> Result<()> {
    with_receiver_env(sh, build, profile, || {
        let mut cargo: Vec<String> = vec![subcmd.to_owned()];
        cargo.extend(receiver_cargo_flags(profile, "desktop-receiver"));
        cargo.extend(extra.iter().cloned());
        // stderr, so it can't interleave with a `--message-format=json` stream.
        eprintln!(">> cargo {subcmd} (static gstreamer) …");
        cmd!(sh, "cargo {cargo...}").run()?;
        Ok(())
    })
}

/// `cargo test` the receiver-core crate against the static gstreamer. The test
/// binary references gstreamer symbols, so it must be linked with the same
/// gstreamer-full aggregate line as the final receiver binary. `cargo test`
/// builds several targets (no single `cargo rustc`), so the link args are fed
/// through a `[target.<triple>].rustflags` config file rather than a scoped
/// `cargo rustc --` tail: with an explicit `--target`, cargo applies those
/// flags to the target artifacts only, leaving host build scripts/proc-macros
/// (which don't reference gstreamer) untouched.
fn receiver_test(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    extra: &[String],
) -> Result<()> {
    with_receiver_env(sh, build, profile, || {
        let link_args = link_args(sh, build, profile)?;

        let mut rustflags: Vec<String> = Vec::new();
        // Cross-LTO drives the LLVM plugin via clang/lld (matches build_receiver);
        // the default debug test build keeps the workspace linker.
        if profile.lto == Lto::Cross {
            rustflags.push("-Clinker-plugin-lto".into());
            rustflags.push("-Clinker=clang".into());
            rustflags.push("-Clink-arg=-fuse-ld=lld".into());
        }
        for a in &link_args {
            rustflags.push(format!("-Clink-arg={a}"));
        }

        // Feed the link line through CARGO_ENCODED_RUSTFLAGS, not a
        // `[target].rustflags` config file. Cargo picks ONE rustflags source and
        // env wins over config, so with_receiver_env's own RUSTFLAGS (frame
        // pointers) would silently shadow the config file and drop every link
        // arg, leaving gst_init_static_plugins undefined at link. This env var
        // has the highest precedence, and its \x1f separator keeps abspaths with
        // spaces intact where a space-split RUSTFLAGS would not. The ambient
        // RUSTFLAGS is merged in so the frame-pointer flag survives. The forced
        // --target scopes all of this to the test binary and its target-side
        // deps, never host build scripts or proc-macros (which must NOT link the
        // gstreamer archives).
        let mut encoded: Vec<String> = std::env::var("RUSTFLAGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        encoded.extend(rustflags);
        let _rf = sh.push_env("CARGO_ENCODED_RUSTFLAGS", encoded.join("\x1f"));

        let mut cargo: Vec<String> = vec!["test".into()];
        cargo.extend(receiver_cargo_flags(profile, "receiver-core"));
        // Trailing args go to the libtest harness (filters, --nocapture,
        // --list, --test-threads), matching plain `cargo test -- …`.
        if !extra.is_empty() {
            cargo.push("--".into());
            cargo.extend(extra.iter().cloned());
        }
        eprintln!(">> cargo test (static gstreamer) …");
        cmd!(sh, "cargo {cargo...}").run()?;
        Ok(())
    })
}

/// The host target triple, parsed from `rustc -vV` (the `host:` line). Used to
/// force an explicit `--target` on `cargo test` so link-arg rustflags don't
/// leak into host build scripts/proc-macros.
fn host_triple(sh: &Rc<Shell>) -> Result<String> {
    let out = cmd!(sh, "rustc -vV").read().context("running rustc -vV")?;
    out.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("could not parse host triple from `rustc -vV`"))
}

/// Compute the gstreamer-full static link line, rewriting every `-lX` whose
/// archive was built in-tree to the `.a`'s absolute path so the linker can't
/// fall back to a same-named dynamic library (→ mixed static/dynamic binary).
/// Non-built libs keep their `-l` and stay dynamic. Also appends the internal
/// helper libraries gstreamer-full's pkg-config omits.
fn link_args(sh: &Rc<Shell>, build: &GstBuild, profile: &Profile) -> Result<Vec<String>> {
    let pkgcfg = pkg_config_prog(sh);
    let raw = cmd!(sh, "{pkgcfg} --static --libs gstreamer-full-1.0")
        .read()
        .context(
            "resolving gstreamer-full-1.0 statically (a private-dep .pc is missing from \
             PKG_CONFIG_PATH — provide it via your environment)",
        )?;

    // Index every built lib*.a so `-lX` can be rewritten to its abspath.
    let archives = find_archives(&build.build_dir)?;
    let macos = target_os(profile) == "macos";

    let mut out = Vec::new();
    for tok in raw.split_whitespace() {
        // `-pthread` is a compile-time flag pkg-config repeats per static lib;
        // at link it's a no-op and clang warns for each copy. Drop them.
        if tok == "-pthread" {
            continue;
        }
        if let Some(name) = tok.strip_prefix("-l") {
            // macOS' C++ runtime is libc++; a .pc generated against a
            // non-clang toolchain drags in `-lstdc++`, which doesn't exist
            // there. Rewrite to the platform runtime (ABI-compatible for a
            // plain link).
            if macos && name == "stdc++" {
                out.push("-lc++".to_string());
                continue;
            }
            // meson names static libs `lib<name>.a`, but also `<name>.a` on
            // MSVC — a bare `-l<name>` left behind is silently ignored by
            // link.exe, so try both forms.
            let candidates = [format!("lib{name}.a"), format!("{name}.a")];
            match candidates.iter().find_map(|f| archives.get(f)) {
                Some(path) => out.push(path.to_string()),
                None => out.push(tok.to_string()), // non-built -l stays dynamic
            }
        } else {
            out.push(tok.to_string());
        }
    }

    // gstreamer-full's pkg-config omits the internal helper libs (riff, fft,
    // adaptivedemux, codecparsers, …) many plugins reference. Add every built
    // libgst*-1.0.a; --gc-sections drops the unreferenced ones.
    for (name, path) in &archives {
        if name.ends_with("-1.0.a") {
            out.push(path.to_string());
        }
    }

    // Windows: the dshow/mediafoundation/winks/dmo plugins reference COM GUID
    // constants living in Windows SDK GUID libs, which gstreamer-full's
    // pkg-config doesn't propagate — name them explicitly; link.exe resolves
    // them from LIB (vcvars).
    if cfg!(windows) {
        for lib in [
            "strmiids.lib",       // DirectShow IIDs/CLSIDs
            "mfuuid.lib",         // Media Foundation IIDs
            "ksuser.lib",         // KS category/property GUIDs
            "dmoguids.lib",       // DMO category GUIDs
            "wmcodecdspuuid.lib", // WM codec DMO CLSIDs
            "msdmo.lib",          // DMO helper entry points
        ] {
            out.push(lib.to_string());
        }
    }
    Ok(out)
}

/// Map every `.a` basename -> absolute path across the build tree. Includes
/// non-`lib*` archives — meson drops the prefix for some libs on MSVC.
fn find_archives(build_dir: &Utf8Path) -> Result<std::collections::HashMap<String, String>> {
    let mut map = std::collections::HashMap::new();
    for entry in walk(build_dir) {
        if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
            if name.ends_with(".a") {
                map.entry(name.to_string())
                    .or_insert_with(|| entry.to_string_lossy().into_owned());
            }
        }
    }
    Ok(map)
}

fn walk(root: &Utf8Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.as_std_path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Canonicalize, stripping Windows' `\\?\` verbatim prefix: Rust's
/// `canonicalize` returns verbatim paths, which meson (Python) mishandles
/// when joining with forward-slash relative paths (EINVAL on open). On
/// non-Windows this is just `canonicalize_utf8`.
fn canonicalize_no_verbatim(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let canonical = path.canonicalize_utf8()?;
    #[cfg(windows)]
    if let Some(rest) = canonical.as_str().strip_prefix(r"\\?\") {
        // `\\?\UNC\server\share` → `\\server\share`; `\\?\C:\…` → `C:\…`.
        let stripped = match rest.strip_prefix("UNC\\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        };
        return Ok(Utf8PathBuf::from(stripped));
    }
    Ok(canonical)
}

/// Prepend `dir` to a PATH-style variable using the OS's separator.
fn prepend_env_path(existing: &str, dir: &str) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    if existing.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}{sep}{existing}")
    }
}

/// Is `bin` resolvable on PATH? Probed by running `bin --version`.
fn on_path(sh: &Rc<Shell>, bin: &str) -> bool {
    cmd!(sh, "{bin} --version")
        .quiet()
        .ignore_stdout()
        .ignore_stderr()
        .run()
        .is_ok()
}

/// Locate a standalone LLVM `bin` dir (the Windows installer doesn't add it
/// to PATH); None elsewhere / when not installed.
fn find_llvm_bin() -> Option<Utf8PathBuf> {
    [
        "C:/Program Files/LLVM/bin",
        "C:/Program Files (x86)/LLVM/bin",
    ]
    .into_iter()
    .map(Utf8PathBuf::from)
    .find(|p| p.join("clang.exe").exists())
}

/// Import the x64 MSVC developer environment by running `vcvars64.bat` and
/// capturing the env it sets (PATH included). Errors if Visual Studio / the
/// Windows SDK is missing.
#[cfg(windows)]
fn vcvars_env(sh: &Rc<Shell>) -> Result<Vec<(String, String)>> {
    let vswhere =
        Utf8PathBuf::from("C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe");
    if !vswhere.exists() {
        bail!("vswhere.exe not found — install Visual Studio (with the C++ workload) to build on Windows");
    }
    let install = cmd!(sh, "{vswhere} -latest -property installationPath")
        .quiet()
        .read()?;
    let install = install.trim();
    if install.is_empty() {
        bail!("vswhere found no Visual Studio installation with the C++ workload");
    }
    let vcvars = Utf8PathBuf::from(install).join("VC/Auxiliary/Build/vcvars64.bat");
    if !vcvars.exists() {
        bail!("vcvars64.bat not found at {vcvars} (install the MSVC C++ build tools)");
    }
    // Run vcvars and dump the env via `set`. A wrapper .bat avoids the nested
    // quoting an inline `cmd /c "call …"` mangles; vcvars also wants a
    // backslash path.
    let vcvars_win = vcvars.as_str().replace('/', "\\");
    let wrapper = std::env::temp_dir().join("xtask-vcvars-dump.bat");
    std::fs::write(
        &wrapper,
        format!("@echo off\r\ncall \"{vcvars_win}\" >nul\r\nset\r\n"),
    )
    .context("writing vcvars wrapper batch")?;
    let wrapper = wrapper.to_string_lossy().to_string();
    let dump = cmd!(sh, "cmd /c {wrapper}")
        .quiet()
        .read()
        .context("running vcvars64.bat to import the MSVC environment")?;
    let _ = std::fs::remove_file(&wrapper);
    let env: Vec<(String, String)> = dump
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if !env.iter().any(|(k, _)| k.eq_ignore_ascii_case("LIB")) {
        bail!("vcvars64.bat did not set LIB — the Windows SDK may be missing");
    }
    Ok(env)
}
