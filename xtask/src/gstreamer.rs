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
/// (the ones the receiver's `*-sys` crates bind, plus internal webrtc/dtls
/// deps).
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
/// registers ONLY these elements; the rest are dropped by --gc-sections. NEVER
/// list va or libav, since they register dynamically and would lose everything.
/// A whitelisted plugin also skips plugin-level init, dropping its typefinders
/// and device providers (unused here). Recheck on a gst bump: a newly added
/// element is silently excluded.
const FULL_ELEMENTS: &[(&str, &[&str])] = &[
    // fcastplaybin builds the graph itself out of these four, so playbin,
    // playbin3, playsink, subtitleoverlay and the v1 decodebin/uridecodebin pair
    // (~385K of object code) never link. Subtitle cues are rasterized by the
    // video sink against the display size, so no overlay element is autoplugged.
    // NB the meson trim in xtask/patches/playback-drop-unused-sources.patch
    // assumes this row exists: removing it breaks the link, not just the size.
    (
        "playback",
        &[
            "decodebin3",
            "parsebin",
            "streamsynchronizer",
            "urisourcebin",
        ],
    ),
    // h264parse/h265parse are needed; the niche sibling parsers never link.
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
    // Receive-only: every depayloader (rtsp:// can carry any codec) plus the
    // elements webrtcbin instantiates; the ~40 payloaders never link.
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
            // webrtcbin internals, dropping these breaks WHEP at runtime
            "rtpredenc",
            "rtpreddec",
            "rtpulpfecdec",
            "rtpulpfecenc",
            "rtpstorage",
            "rtphdrextcolorspace",
        ],
    ),
    // demux-only containers: the muxers never link in a playback receiver.
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
    // network sources. The receiver never streams out, so sinks drop.
    ("soup", &["souphttpsrc"]),
    ("rtmp2", &["rtmp2src"]),
    // Nothing autoplugs pango any more, but the plugin must stay BUILT: it is what
    // pulls the pango/cairo wraps into scope=Full. Deleting this row would REGISTER
    // pango whole, not drop it; retiring it means disabling the plugin and
    // provisioning pango for the Rust cue raster instead.
    ("pango", &["textoverlay", "textrender"]),
];

/// Whitelists for plugins built only on LINUX. The generator emits
/// gst_element_register_<e> UNCONDITIONALLY, so naming a plugin that isn't
/// built is an undefined symbol at link, so these must not reach mac/win. Keys
/// are PLUGIN names, not meson option names (`pulse` → `pulseaudio`); a
/// mismatch silently registers the plugin whole.
const FULL_ELEMENTS_LINUX: &[(&str, &[&str])] = &[
    ("srt", &["srtsrc", "srtclientsrc", "srtserversrc"]),
    // decode only; wavpackparse lives in audioparsers, not here.
    ("wavpack", &["wavpackdec"]),
    // output only (pulsesrc is capture); the device provider drops with init.
    ("pulseaudio", &["pulsesink"]),
];

/// Linux: VA-API decode, pulse/pipewire audio. assrender (styled ASS/SSA) needs
/// libass; wavpack is the only WavPack decoder (avdec_wavpack is on gst-libav's
/// skip list). srt/assrender/wavpack have no wrapdb wrap, so the hermetic
/// mac/win builds cannot have them.
const ENABLE_LINUX: &[(Plugins, &str)] = &[
    (Plugins::Bad, "va"),
    // jpegparse feeds vajpegdec, whose sink caps need the parsed fields. The image
    // codecs are stripped in DISABLE_COMMON; ENABLE runs after DISABLE and wins.
    (Plugins::Bad, "jpegformat"),
    (Plugins::Bad, "srt"),
    (Plugins::Bad, "assrender"),
    (Plugins::Good, "wavpack"),
    // Forced, not `auto`: without libnice/libsrtp devel the webrtc stack would
    // silently drop out and WHEP break at runtime. Now it is a configure error.
    (Plugins::Bad, "webrtc"), // webrtcbin, driven directly by fwebrtcsrc
    (Plugins::Bad, "dtls"),
    (Plugins::Bad, "srtp"),
    (Plugins::Bad, "sctp"),
];
const DISABLE_LINUX: &[(Plugins, &str)] = &[(Plugins::Base, "gl")];

/// macOS: VideoToolbox decode + CoreAudio/Cocoa output. `gl` is enabled only
/// because applemedia unconditionally includes <gst/gl/gl.h> (gstglconfig.h
/// exists only when `gl` builds); macOS gstgl links only system frameworks.
const ENABLE_MACOS: &[(Plugins, &str)] = &[
    (Plugins::Bad, "applemedia"),
    (Plugins::Good, "osxaudio"),
    (Plugins::Good, "osxvideo"),
    (Plugins::Base, "gl"),
];
/// macOS must link ONLY OS frameworks (the installer verifies with otool). Each
/// of these pulls an unvendored dylib, or is redundant with libav decode.
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
/// (Kept intentionally: videofilter, audiobuffersplit, proxy, all autoplugged.)
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
    // X11 video (receiver has its own sink). `gl` is NOT disabled here:
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
    // SVG: a discoverable librsvg links dynamically, defeating the static build
    // (its .pc also leaks a bare `-no_compact_unwind` ld flag that breaks clang).
    (Plugins::Bad, "rsvg"),
    // C subtitle parsers. receiver-core registers gst-subparse-rs and ranks
    // subparse/ssaparse to NONE, so decodebin3 already never autoplugs these,
    // and the Rust plugin carries its own rssubparse_typefind at the same
    // MARGINAL rank over the same extension set. Only the cue-IR path the
    // renderer needs comes out of the Rust ones.
    (Plugins::Base, "subparse"),
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
    // tcp: serve-out / socket-IPC elements a playback receiver never uses.
    (Plugins::Base, "tcp"),
    // The gst `gio` element plugin only (files go via filesrc, network via
    // souphttpsrc); the GLib GIO library and its TLS module are unaffected.
    (Plugins::Base, "gio"),
    // v4l2 is capture/output only. Bad's v4l2codecs (the stateless hardware-decode
    // path on SoCs like the Raspberry Pi) is KEPT.
    (Plugins::Good, "v4l2"),
    (Plugins::Base, "alsa"),
    (Plugins::Good, "oss"),
    (Plugins::Good, "oss4"),
    // legacy adaptive streaming: playbin3 autoplugs adaptivedemux2's
    // hlsdemux2/dashdemux2 instead; also home to hlssink/dashsink.
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
    // ASF/WMV/WMA: dead format; the WMV/WMA avdec_* are dropped too.
    (Plugins::Ugly, "asfdemux"),
    // Hermetic auto-plugin exclusions: these sat at meson `auto`, so whether they
    // built depended on the image's -devel packages, and a plugin that registers
    // but does not link statically fails the final link with `undefined symbol:
    // gst_plugin_<x>_register`. Deliberately KEPT at auto: ttml, rtmp2,
    // dvbsuboverlay/dvdspu.
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
    // tracker/module/MIDI music formats, never cast
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
/// and only these re-enabled: the full set (hundreds) is dead weight.
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

/// FFmpeg parsers/bsfs that FFMPEG_DECODERS `select` internally. The groups are
/// disabled wholesale, and the meson port silently CULLS a decoder whose
/// selected component is missing while still reporting it "enabled".
const FFMPEG_COMPONENTS: &[&str] = &[
    "ac3_parser",               // ac3 (eac3 chains through ac3_decoder)
    "aac_latm_parser",          // aac_latm
    "h263_parser",              // h263; h263p/flv/mpeg4/msmpeg4v* chain through h263_decoder
    "mlp_parser",               // mlp, truehd
    "vp9_parser",               // vp9
    "vp9_superframe_split_bsf", // vp9
];

/// Wraps force-fallbacked in scope=Full: ONE static glib (plus the pango stack
/// it shares) built from vendored wraps is what lets mac/win build without the
/// GStreamer dev kit. Forcing (rather than not-found fallback) ignores a stray
/// system copy; forcing a dep no platform requests is a no-op.
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
    // Codec + support libs the kept plugins pull in; unforced, meson resolves them
    // from pkg-config → @rpath dylibs that dangle on user machines. These are
    // dependency names, not wrap filenames (see each wrap's [provide]).
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
    // souphttpsrc stack: http(s) media sources (playbin3/adaptivedemux2).
    "libsoup-3.0",
    "libxml-2.0",
    "libpsl",
    "libnghttp2",
];

/// Forced static in scope=Full too: the Rust side must link the SAME static
/// glib as gstreamer-full (two glibs = "cannot register existing type
/// 'GstObject'"). dav1d-sys must link the in-tree wrap's archive.
const SYSTEM_DEPS_FULL_SCOPE: &[&str] = &["GLIB_2_0", "GOBJECT_2_0", "GIO_2_0", "DAV1D"];

/// HEIC (mac/win, scope=Full): libheif-sys's vendored libheif only parses the
/// container, so it needs a HEVC decoder for pixels: libde265, built
/// statically by [`build_libde265`]. Linux links the system libheif instead.
const LIBDE265_REPO: &str = "https://github.com/strukturag/libde265.git";
const LIBDE265_REF: &str = "v1.0.16";

/// Codecs libheif-sys's embedded build force-enables and then leaves to
/// `find_package` to disable: on a Homebrew/vcpkg host they compile in and leak
/// a dynamic dep into the "hermetic" binary. Killed via
/// CMAKE_DISABLE_FIND_PACKAGE_* in [`write_libheif_toolchain`].
const LIBHEIF_DISABLED_CODECS: &[&str] = &[
    "AOM",
    "DAV1D",
    "FFMPEG",
    "JPEG",
    "OPENJPH",
    "OpenH264",
    "OpenJPEG",
    "RAV1E",
    "SvtEnc",
    "UVG266",
    "X264",
    "X265",
    "kvazaar",
    "libsharpyuv",
    "vvdec",
    "vvenc",
];

/// pkg-config modules a *Linux* build requires from the environment; asserted
/// up front with an actionable error (mac/win get these from wraps/frameworks).
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
    // The webrtc stack (nice/libsrtp2/openssl) is not asserted: wraps exist.
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StaticScope {
    /// gstreamer + codecs static; glib/pango/OS dynamic. For Linux/Flatpak,
    /// where the runtime provides (and must provide) glib.
    Gstreamer,
    /// Also glib + pango + TLS static from wraps → one glib, standalone binary,
    /// no dev kit. Default for macOS/Windows; NOT Flatpak (glib is the
    /// runtime's).
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Lto {
    /// No LTO beyond the cargo profile default.
    Off,
    /// Rust-only fat LTO
    Rust,
    /// Cross-language Rust↔C LTO (`-Db_lto` + `-Clinker-plugin-lto` + lld).
    /// rustc's and clang's LLVM must be the same major version.
    Cross,
}

#[derive(Clone)]
struct Profile {
    scope: StaticScope,
    lto: Lto,
    offline: bool,
    target: Option<String>,
    /// Cargo profile for the receiver build; GStreamer follows `gst_buildtype`.
    cargo_profile: String,
    /// meson buildtype for GStreamer (default "release").
    gst_buildtype: String,
    /// Pass --no-default-features to cargo (e.g. no systray on macOS).
    no_default_features: bool,
}

impl Profile {
    /// The `target/<subdir>` cargo writes this profile's artifacts into.
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
    /// Build directory for the static gstreamer (defaults to
    /// <source>/builddir-static).
    #[arg(long)]
    build_dir: Option<Utf8PathBuf>,
    /// Rust/meson target triple (defaults to host).
    #[arg(long)]
    target: Option<String>,
    /// Offline build: `meson --wrap-mode=nodownload`. Subprojects must be
    /// vendored.
    #[arg(long)]
    offline: bool,
    /// Default: `gstreamer` on Linux, `full` on macOS/Windows.
    #[arg(long, value_enum)]
    pub static_scope: Option<StaticScope>,
    #[arg(long, value_enum, default_value_t = Lto::Off)]
    lto: Lto,
    /// Build the receiver as a cargo dev build (GStreamer stays release).
    #[arg(long)]
    debug: bool,
    /// Cargo profile for the receiver, e.g. `release-dbg`. Wins over --debug.
    #[arg(long)]
    profile: Option<String>,
    /// Profiling preset: receiver in `release-dbg`, GStreamer and its wraps in
    /// `debugoptimized`. An explicit --profile/--gst-buildtype/--debug wins.
    #[arg(long)]
    debug_info: bool,
    /// meson buildtype for GStreamer; `debugoptimized` under --debug-info, else
    /// release.
    #[arg(long)]
    gst_buildtype: Option<String>,
    /// Only build gstreamer, don't build the receiver.
    #[arg(long)]
    gstreamer_only: bool,
    /// Build the receiver with --no-default-features (e.g. no systray on
    /// macOS).
    #[arg(long)]
    pub no_default_features: bool,
    /// Remove built/downloaded artifacts and exit (never a --source tree).
    #[arg(long)]
    clean: bool,
}

impl GstreamerArgs {
    pub fn run(self) -> Result<()> {
        self.build().map(|_| ())
    }

    /// --profile, else --debug-info → `release-dbg`, else --debug → `dev`, else
    /// release.
    fn cargo_profile(&self) -> String {
        match &self.profile {
            Some(p) => p.clone(),
            None if self.debug_info => "release-dbg".to_owned(),
            None if self.debug => "dev".to_owned(),
            None => "release".to_owned(),
        }
    }

    /// --gst-buildtype, else --debug-info → `debugoptimized`, else `release`.
    fn gst_buildtype(&self) -> String {
        match &self.gst_buildtype {
            Some(b) => b.clone(),
            None if self.debug_info => "debugoptimized".to_owned(),
            None => "release".to_owned(),
        }
    }

    /// The args you'd get by passing no flags. Lets other subcommands drive a
    /// cargo build while clap's declared defaults stay the single source of
    /// truth.
    pub fn with_defaults() -> Self {
        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            gst: GstreamerArgs,
        }
        <Wrap as clap::Parser>::parse_from(["xtask"]).gst
    }

    /// Build (or reuse) the static GStreamer and return the pieces needed to
    /// drive cargo against it. Returns `Ok(None)` when `--clean`
    /// short-circuits.
    fn prepare(self) -> Result<Option<(Rc<Shell>, Profile, GstBuild)>> {
        self.prepare_impl(true)
            .map(|o| o.map(|(sh, profile, build, _)| (sh, profile, build)))
    }

    /// Like `prepare`, but `compile: false` only runs `meson setup` (enough for
    /// the uninstalled .pc files) and returns the stamp for a deferred
    /// compile.
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
    /// return the receiver binary path. On Linux/scope=Gstreamer the ninja
    /// build runs CONCURRENTLY with the receiver's Rust dependency graph;
    /// only the final bin link needs the archives, so it happens after the
    /// join.
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
                    // Cargo failed: reap ninja. No stamp is written, so nothing is lost.
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

    /// Build the static receiver and exec it with `args`, propagating its exit
    /// code. Debug build by default; `release` opts into the optimized
    /// profile.
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

    /// `cargo test` receiver-core. Unlike check/clippy this LINKS the test
    /// binary, so it needs the full gstreamer-full link line (see
    /// `link_args`).
    pub fn test(mut self, extra: Vec<String>, release: bool) -> Result<()> {
        self.debug = self.debug || !release;
        let Some((sh, mut profile, build)) = self.prepare()? else {
            return Ok(());
        };
        // Force an explicit --target so the link-arg rustflags scope to the target
        // graph and never reach host build scripts or proc-macros.
        if profile.target.is_none() {
            profile.target = Some(host_triple(&sh)?);
        }
        receiver_test(&sh, &build, &profile, &extra)
    }

    fn cargo_subcmd(mut self, subcmd: &str, extra: Vec<String>, release: bool) -> Result<()> {
        self.debug = self.debug || !release;
        let Some((sh, profile, build)) = self.prepare()? else {
            return Ok(());
        };
        receiver_cargo(&sh, &build, &profile, subcmd, &extra)
    }
}

/// Remove built/downloaded artifacts. Never deletes a user-provided --source
/// tree: only our build dir + prefix inside it.
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
/// into target/gstreamer-src, reusing an existing clone. Refuses when offline.
fn resolve_source(sh: &Rc<Shell>, gst_ref: &str, offline: bool) -> Result<Utf8PathBuf> {
    if offline {
        bail!("--offline requires --source <PATH> (cannot clone without network)");
    }
    // Absolute: the shell runs git from root_path, std::fs uses the process cwd.
    let dir = crate::workspace::root_path()?.join("target/gstreamer-src");
    if checkout_present(&dir) {
        // A tag checkout is a detached HEAD, so match the exact tag too.
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
                ">> Reusing GStreamer checkout at {dir} (on '{current}', requested '{gst_ref}'), \
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
            // The presence probe can transiently false-negative on Windows (see
            // checkout_present); only a genuinely absent dir is a real error.
            if !checkout_present(&dir) {
                return Err(e).context("cloning gstreamer source");
            }
            println!(">> {dir} already present, reusing existing checkout");
        }
    }
    Ok(dir)
}

/// Apply `xtask/patches/*.patch` (every build) and `xtask/patches/<target-os>/`
/// (that OS only, so an OS-specific patch doesn't dirty a checkout shared with
/// another target), idempotently: a reverse-apply check skips ones already
/// present. A patch that neither applies nor is applied warns instead of
/// failing, so a user-provided `--source` on another ref still builds.
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
        // --ignore-whitespace keeps apply and reverse-check EOL-agnostic (a Windows
        // checkout with core.autocrlf turns these LF patches into CRLF). A clean
        // reverse-apply means the patch is already there.
        if cmd!(
            sh,
            "git -C {source} apply --ignore-whitespace --reverse --check {patch}"
        )
        .quiet()
        .ignore_stderr()
        .run()
        .is_ok()
        {
            println!(">> gstreamer patch already applied, skipping: {name}");
            continue;
        }
        // Not applicable to this tree (different ref / already-diverged): warn, don't
        // fail.
        if cmd!(
            sh,
            "git -C {source} apply --ignore-whitespace --check {patch}"
        )
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

/// Is the checkout present (dir exists, non-empty)? Retried: listing this tree
/// can briefly fail on Windows, and a false "absent" would clone over it.
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

/// Target OS ("linux" | "macos" | "windows"), from `--target` if given, else
/// host.
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

/// First `bin` found on PATH (executable regular file), as an absolute path.
fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let cand = dir.join(bin);
        cand.is_file().then(|| cand.to_string_lossy().into_owned())
    })
}

/// rustc args picking the fastest available linker for the Linux builds that
/// would otherwise fall back to bfd, which links the ~0.5 GB static debug
/// binary single threaded. wild/mold are not `-fuse-ld` names, so they go
/// through `clang --ld-path`; gold is. Cross-LTO and non-Linux keep their own
/// wiring.
fn fast_linker_args(profile: &Profile) -> Vec<String> {
    if profile.lto == Lto::Cross || target_os(profile) != "linux" {
        return Vec::new();
    }
    for name in ["wild", "mold"] {
        if let Some(path) = which(name) {
            return vec![
                "-Clinker=clang".into(),
                format!("-Clink-arg=--ld-path={path}"),
            ];
        }
    }
    if which("ld.gold").is_some() {
        return vec!["-Clink-arg=-fuse-ld=gold".into()];
    }
    Vec::new()
}

/// Configure (meson setup) the static GStreamer without compiling; returns the
/// build handle plus the stamp to write after a successful compile.
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

    // Assert the pkg-config closure up front, not as a cryptic meson failure.
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

    // scope=Full must be HERMETIC. A rich host exposes dozens of optional libs via
    // pkg-config that `-Dgst-full-plugins=*` + `auto` features silently link,
    // leaving dynamic deps that dangle on user machines. Blank pkg-config out for
    // the whole gstreamer build (LIBDIR at a real empty dir also overrides the
    // compiled-in default search path); build_receiver sets its own.
    let _pc_isolate = (profile.scope == StaticScope::Full).then(|| {
        let empty = source.join(".xtask-empty-pkgconfig");
        let _ = std::fs::create_dir_all(&empty);
        (
            sh.push_env("PKG_CONFIG_PATH", ""),
            sh.push_env("PKG_CONFIG_LIBDIR", empty.as_str()),
        )
    });

    // Always from wraps: the decode-only FFmpeg fork; scope=Full adds the
    // glib/pango closure. NOTE repeated --force-fallback-for flags OVERRIDE
    // each other, so this must stay ONE flag.
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
        // The pinned 1.29.x is a DEV series, where glib_debug (gobject cast checks) and
        // extra-checks default ON: a runtime type walk in every GST_IS_*. Stable
        // releases default them off. glib_assert/glib_checks stay ON: those are
        // behavior-relevant API guards.
        "-Dglib_debug=disabled".into(),
        "-Dextra-checks=disabled".into(),
        "-Dgst-full-target-type=static_library".into(),
        "-Dgst-full-plugins=*".into(),
        // Element whitelist. Generator syntax: plugin:elem,elem;plugin2:elem (see
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
            // macOS zero-copy video needs libgstiosurface-1.0's ABI exported (its .a
            // already builds as an applemedia dep). macOS-only library.
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
        // Decode-only FFmpeg; demuxers/protocols come from native gst elements.
        "-DFFmpeg:encoders=disabled".into(),
        "-DFFmpeg:muxers=disabled".into(),
        "-DFFmpeg:programs=disabled".into(),
        "-DFFmpeg:tests=disabled".into(),
        "-DFFmpeg:decoders=disabled".into(),
        "-DFFmpeg:demuxers=disabled".into(),
        "-DFFmpeg:protocols=disabled".into(),
        // ~450 avfilters are dead weight and dominate the serial build tail;
        // libavfilter itself still builds (gst-libav hard-requires it).
        "-DFFmpeg:filters=disabled".into(),
        // FFmpeg auto-detects system bz2 (compressed-matroska, extremely rare)
        // and links it dynamically, and no bz2 wrap exists, so drop it.
        "-DFFmpeg:bzlib=disabled".into(),
        // gst-libav uses neither (native parse elements feed aligned frames), and both
        // lists are referenced from libavcodec's registry, so unlike unused decoders
        // they would not drop out at link time.
        "-DFFmpeg:parsers=disabled".into(),
        "-DFFmpeg:bsfs=disabled".into(),
        // Wrap deps build their own tests/example programs by default; pure waste.
        "-Dopus:tests=disabled".into(),
        "-Dopus:extra-programs=disabled".into(),
        "-Dopus:docs=disabled".into(),
        // NB libsoup's tests option is a boolean, not a feature. A wrong value TYPE
        // does not error: meson treats the subproject as failed-to-configure and
        // SILENTLY drops everything depending on it (soup + adaptivedemux2).
        "-Dlibsoup:tests=false".into(),
        "-Dlibsoup:docs=disabled".into(),
        "-Dlibsoup:sysprof=disabled".into(),
        // libxml2's only consumer is adaptivedemux2's DASH MPD parser: core parser plus
        // `output`. minimum=true turns off every feature not explicitly enabled.
        "-Dlibxml2:minimum=true".into(),
        "-Dlibxml2:output=enabled".into(),
        "-Dlibxml2:threads=enabled".into(),
        // libsoup hard-requires sqlite but nothing ever reaches it; -O1 only to cut the
        // huge amalgamation TU's compile time.
        "-Dsqlite3:optimization=1".into(),
        // AV1 decode is the Rust dav1d-sys. With rs=disabled nothing requests dav1d, so
        // the wrap never builds and dav1d-sys links a DYNAMIC libdav1d; FFmpeg's
        // libdav1d makes meson request it → static .a + uninstalled .pc.
        "-DFFmpeg:libdav1d=enabled".into(),
    ];
    for dec in FFMPEG_DECODERS {
        args.push(format!("-DFFmpeg:{dec}_decoder=enabled"));
    }
    for comp in FFMPEG_COMPONENTS {
        args.push(format!("-DFFmpeg:{comp}=enabled"));
    }

    // Per-function/data sections let the final --gc-sections drop everything
    // unreferenced; MSVC spells these differently, so skip Windows.
    let mut c_args: Vec<String> = Vec::new();
    let mut cpp_args: Vec<String> = Vec::new();
    if os != "windows" {
        c_args.push("-ffunction-sections".into());
        c_args.push("-fdata-sections".into());
        cpp_args.push("-ffunction-sections".into());
        cpp_args.push("-fdata-sections".into());
    }

    // The profiling build keeps frame pointers in the static C/C++ so `perf
    // --call-graph fp` can walk GStreamer frames (DWARF unwinding fails through
    // this binary). Part of `args`, so the stamp reconfigures on a profile
    // switch.
    if profile.cargo_profile == "release-prof" && os != "windows" {
        c_args.push("-fno-omit-frame-pointer".into());
        cpp_args.push("-fno-omit-frame-pointer".into());
    }

    // vorbis/theora headers include <ogg/ogg.h>, but ogg is only in their .pc's
    // Requires.private, whose include dirs pkgconf omits from --cflags. Linux only:
    // mac/win get these from wraps, and injecting the paths breaks MSVC.
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
    if !cpp_args.is_empty() {
        args.push(format!("-Dcpp_args={}", cpp_args.join(" ")));
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

    // scope=Full: glib + pango from wraps (FULL_SCOPE_FALLBACK), one static glib.
    if profile.scope == StaticScope::Full {
        // glib-networking builds as a subproject and its GIO TLS module links into
        // gstreamer-full (registered by gst_init_static_plugins), so https needs no
        // runtime GIO modules. gnutls has no wrap → openssl backend.
        args.push("-Dtls=enabled".into());
        args.push("-Dglib-networking:gnutls=disabled".into());
        args.push("-Dglib-networking:openssl=enabled".into());
        args.push("-Dglib-networking:libproxy=disabled".into());
        args.push("-Dglib-networking:gnome_proxy=disabled".into());
        // introspection would drag in the gobject-introspection wrap.
        args.push("-Dglib:tests=false".into());
        args.push("-Dglib:introspection=disabled".into());
        args.push("-Dpango:introspection=disabled".into());
        // openssl: glib-networking's TLS backend (gnutls has no wrap).
        ensure_wrap(sh, source, profile, "openssl")?;
        // libnice's `auto` DTLS backend prefers gnutls, which it can still find via
        // meson's cmake fallback (pkg-config isolation doesn't cover cmake) and link
        // dynamically, an @rpath dylib the installer rejects.
        args.push("-Dlibnice:crypto-library=openssl".into());
        // cairo `auto` features turn ON whenever the build host exposes the lib,
        // pulling deps a mac/win text stack never needs. pango needs only
        // quartz/image.
        args.push("-Dcairo:xlib=disabled".into());
        args.push("-Dcairo:xcb=disabled".into());
        args.push("-Dcairo:lzo=disabled".into()); // cairo-script compression
        args.push("-Dcairo:spectre=disabled".into()); // PS preview
        args.push("-Dcairo:symbol-lookup=disabled".into()); // binutils/bfd
        args.push("-Dcairo:tests=disabled".into());
    }

    // Windows must use MSVC `cl`: countless wrap meson checks gate Windows
    // behaviour on `cc.get_id() == 'msvc'`. macOS and cross-LTO use clang
    // (bitcode; and a non-Apple g++ makes C++ wraps emit `-lstdc++`, which
    // doesn't exist on macOS. link_args also rewrites strays to -lc++.
    // Elsewhere an exported CC/CXX is folded in so the ccache wrap applies to
    // it and the stamp sees it change.
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
    // Wrap the compiler in ccache when it is on PATH, so the wipe-on-change path
    // recompiles from cache. meson's own detection can't be relied on (distro
    // patches skip it, an exported CC suppresses it). Not for `cl`.
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

    // Patch subprojects that already exist; fresh downloads get the pass below.
    apply_subproject_patches(sh, source)?;

    // meson captures PKG_CONFIG_PATH and the compilers at first setup and ignores
    // env changes on --reconfigure, so start over when they changed. When nothing
    // changed, skip setup entirely: ninja detects source changes fine.
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
            println!(">> Build environment/options changed, deleting build dir");
            std::fs::remove_dir_all(build_dir)
                .with_context(|| format!("removing stale build dir {build_dir}"))?;
        }
        Some("--reconfigure") // fresh dir: acts as plain setup
    };

    // PATH is composed fully here first: each key may be pushed exactly once.
    let mut build_env: Vec<(String, String)> = Vec::new();
    let mut path = sh.var("PATH").unwrap_or_default();

    // Windows: import the MSVC developer environment from vcvars64, so `cl`,
    // dumpbin/link on PATH plus the SDK INCLUDE/LIB meson's checks need.
    #[cfg(windows)]
    if os == "windows" {
        for (k, v) in vcvars_env(sh)? {
            if k.eq_ignore_ascii_case("PATH") {
                path = v; // already includes our original PATH plus the MSVC
                          // bins
            } else {
                build_env.push((k, v));
            }
        }
    }

    // Cross-LTO clang may need the standalone LLVM bin dir prepended.
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

    // The compiler goes via BOTH CC/CXX and a meson native file: distro-patched
    // mesons can ignore compiler env vars entirely (even CC=/nonexistent configures
    // happily), so the native file is what reliably selects it.
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
            // Write only on change: meson tracks the native file as a regen dependency,
            // so a fresh mtime every run would re-run the generator.
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

    // Windows flags `patch.exe` (which meson runs for wrap diffs) as needing
    // elevation → CreateProcess fails with WinError 740; RunAsInvoker makes
    // children inherit our unelevated token.
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
        // Wraps download at setup: patch anything that just appeared. The changed
        // mtime makes the next `meson compile` regenerate.
        apply_subproject_patches(sh, source)?;
    } else {
        println!(">> GStreamer configuration unchanged, skipping meson setup");
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

/// Apply the Windows MSVC/vcvars build environment to `sh` (PATH + the SDK
/// INCLUDE/LIB + RunAsInvoker), returning the RAII guards. Both `meson setup`
/// AND `meson compile` need it: a compile can trigger a reconfigure whose
/// uncached `find_library` checks re-invoke `cl`.
#[cfg(windows)]
fn push_msvc_build_env(sh: &Rc<Shell>) -> Result<Vec<xshell::PushEnv<'_>>> {
    let mut guards = Vec::new();
    let mut path = sh.var("PATH").unwrap_or_default();
    for (k, v) in vcvars_env(sh)? {
        if k.eq_ignore_ascii_case("PATH") {
            path = v; // vcvars PATH already includes the original plus MSVC
                      // bins
        } else {
            guards.push(sh.push_env(k, v));
        }
    }
    guards.push(sh.push_env("PATH", path));
    // Children must inherit our unelevated token (see configure_gstreamer).
    guards.push(sh.push_env("__COMPAT_LAYER", "RunAsInvoker"));
    Ok(guards)
}

/// `meson compile` + stamp write (only after success, so a failed build re-runs
/// the setup check). Split out so `build()` can spawn/join it instead.
fn compile_gstreamer(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    stamp: &str,
) -> Result<()> {
    // `meson compile` can trigger a regenerate; scope=Full must not see host
    // pkg-config then (same isolation and values as configure, no stamp drift).
    let _pc_isolate = (profile.scope == StaticScope::Full).then(|| {
        let empty = build.source.join(".xtask-empty-pkgconfig");
        let _ = std::fs::create_dir_all(&empty);
        (
            sh.push_env("PKG_CONFIG_PATH", ""),
            sh.push_env("PKG_CONFIG_LIBDIR", empty.as_str()),
        )
    });
    // Windows: that compile-time regenerate re-runs `find_library` link checks,
    // which need `cl` on PATH. configure_gstreamer's guards are gone by now, so
    // without this the reconfigure dies with `CreateProcess … [WinError 2]`.
    #[cfg(windows)]
    let _msvc_env = (target_os(profile) == "windows")
        .then(|| push_msvc_build_env(sh))
        .transpose()?;
    println!(">> Building GStreamer …");
    let build_dir = &build.build_dir;
    cmd!(sh, "meson compile -C {build_dir}").run()?;
    stamp_write(build_dir, stamp)
}

/// Spawn the GStreamer compile in the background (output → xtask-ninja.log).
/// Linux scope=Gstreamer only, where the plain process env is faithful.
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
/// graph: build scripts only need the uninstalled .pc files, and nothing reads
/// the archives until the final link. receiver-core covers every dependency but
/// the bin crate; a feature mismatch costs recompilation, never correctness.
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

/// Subproject source patches applied idempotently around `meson setup`: (dir
/// under xtask/patches/, wrap name). The target directory comes from the wrap's
/// `directory =` key, and a patch that then no longer applies is a HARD error,
/// never silently build unpatched. NB xtask/patches/gstreamer/ is deliberately
/// absent: the playbin3 TEXT-flag patch there hangs preroll (reference only).
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
            continue; // not downloaded yet, the post-setup pass gets it
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
        // Run from the SOURCE ROOT with --directory: inside a git worktree `git apply`
        // resolves patch paths against the repo root and SILENTLY SKIPS (exit 0!)
        // anything outside the cwd, so cd'ing into an extracted subproject no-ops.
        let dir_arg = format!("--directory=subprojects/{dir_name}");
        // --ignore-whitespace makes this EOL-agnostic: a Windows checkout turns these
        // patches into CRLF while tarball-wrap sources (libxml2) stay LF.
        for patch in patches {
            let _d = sh.push_dir(source);
            // A patch that applies cleanly in REVERSE is already present.
            if cmd!(
                sh,
                "git apply --ignore-whitespace --check --reverse {dir_arg} {patch}"
            )
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
                        "applying {patch} to {target}. If the subproject version \
                         changed, re-derive the patch against the new source"
                    )
                })?;
            // Belt and braces against the silent-skip failure mode.
            if cmd!(
                sh,
                "git apply --ignore-whitespace --check --reverse {dir_arg} {patch}"
            )
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

/// Ensure a wrap the monorepo doesn't vendor is present (from wrapdb). `meson
/// wrap install` only drops the .wrap; the source downloads at setup time.
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

/// Placeholder: generate/point at a meson cross file for the target.
fn cross_file(_sh: &Rc<Shell>, _source: &Utf8Path, target: &str) -> Result<Utf8PathBuf> {
    bail!("cross-compiling gstreamer to {target} is not wired up yet (host/Linux only)");
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

/// Flags shared by every cargo invocation against the static gstreamer.
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

/// Resolve the libde265 source (clone `LIBDE265_REF` into target/libde265-src,
/// reusing an existing checkout); refuses to clone under `--offline`.
fn resolve_libde265_source(sh: &Rc<Shell>, offline: bool) -> Result<Utf8PathBuf> {
    let dir = crate::workspace::root_path()?.join("target/libde265-src");
    if checkout_present(&dir) {
        println!(">> Reusing libde265 checkout at {dir}");
        return Ok(dir);
    }
    if offline {
        bail!("--offline requires a target/libde265-src checkout (cannot clone without network)");
    }
    println!(">> Cloning libde265 {LIBDE265_REF} into {dir} …");
    if let Err(e) = cmd!(
        sh,
        "git clone --depth 1 --branch {LIBDE265_REF} {LIBDE265_REPO} {dir}"
    )
    .run()
    {
        // Same transient false-negative guard as resolve_source (Windows).
        if !checkout_present(&dir) {
            return Err(e).context("cloning libde265 source");
        }
        println!(">> {dir} already present, reusing existing checkout");
    }
    Ok(dir)
}

/// Patch libde265's source for clang-cl (used for the Windows C deps): v1.0.16
/// assumes `_MSC_VER`/`MSVC` implies real `cl`. Idempotent, and a missing
/// needle is logged rather than fatal, so a fixed `LIBDE265_REF` makes this a
/// no-op.
fn patch_libde265_for_clang_cl(src: &Utf8Path) -> Result<()> {
    // util.h: the `for each (…)` FOR_LOOP branch is a C++/CLI extension real `cl`
    // accepts and clang-cl does not; take the standard C++11 branch instead.
    patch_file(
        &src.join("libde265/util.h"),
        "#if defined(_MSC_VER) || (!__clang__ && __GNUC__ && GCC_VERSION < 40600)",
        "#if (defined(_MSC_VER) && !defined(__clang__)) || (!__clang__ && __GNUC__ && GCC_VERSION < 40600)",
        "util.h FOR_LOOP guard",
    )?;

    // x86/CMakeLists.txt: clang-cl rejects _mm_packus_epi32 without -msse4.1, which
    // upstream passes only `if(NOT MSVC)`, and clang-cl reports as both.
    patch_file(
        &src.join("libde265/x86/CMakeLists.txt"),
        "if(NOT MSVC)",
        "if(NOT MSVC OR CMAKE_CXX_COMPILER_ID MATCHES \"Clang\")",
        "x86 SSE flag guard",
    )?;

    Ok(())
}

/// Replace the first `needle` with `fixed` in `path`, idempotently. A missing
/// needle logs rather than errors, so upstream drift doesn't break the build.
fn patch_file(path: &Utf8Path, needle: &str, fixed: &str, what: &str) -> Result<()> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading libde265 {what}"))?;
    if contents.contains(fixed) {
        return Ok(()); // already patched
    }
    let patched = contents.replacen(needle, fixed, 1);
    if patched == contents {
        println!(">> libde265 {what} not found, skipping clang-cl patch");
        return Ok(());
    }
    std::fs::write(path, patched).with_context(|| format!("patching libde265 {what}"))?;
    println!(">> Patched libde265 {what} for clang-cl");
    Ok(())
}

/// Build a static `libde265` (the HEVC decoder libheif needs for HEIC) and
/// return its install prefix. Idempotent; library only, no example tools.
fn build_libde265(sh: &Rc<Shell>, build: &GstBuild, profile: &Profile) -> Result<Utf8PathBuf> {
    let prefix = build.build_dir.join("libde265-install");
    let libdir = prefix.join("lib");
    // cmake emits `libde265.a` on unix, `libde265.lib` under clang-cl, and
    // alias_de265_lib may have left a `de265.lib` copy, any means it is built.
    let built = ["libde265.a", "libde265.lib", "de265.lib"]
        .iter()
        .any(|n| libdir.join(n).exists());

    if !built {
        let src = resolve_libde265_source(sh, profile.offline)?;
        if target_os(profile) == "windows" {
            patch_libde265_for_clang_cl(&src)?;
        }
        let cmake_build = build.build_dir.join("libde265-build");

        let mut args: Vec<String> = vec![
            "-S".into(),
            src.to_string(),
            "-B".into(),
            cmake_build.to_string(),
            format!("-DCMAKE_INSTALL_PREFIX={prefix}"),
            "-DCMAKE_INSTALL_LIBDIR=lib".into(),
            "-DBUILD_SHARED_LIBS=OFF".into(),
            "-DENABLE_SDL=OFF".into(),
            "-DENABLE_DECODER=OFF".into(), // skip the dec265 tool; we only want the lib
            "-DENABLE_ENCODER=OFF".into(),
            "-DCMAKE_POSITION_INDEPENDENT_CODE=ON".into(),
            "-DCMAKE_BUILD_TYPE=Release".into(),
        ];
        match target_os(profile) {
            "macos" => {
                // Match the receiver's deployment target; pin the arch when explicit.
                args.push("-DCMAKE_OSX_DEPLOYMENT_TARGET=11.0".into());
                if let Some(arch) = macos_osx_arch(sh, profile)? {
                    args.push(format!("-DCMAKE_OSX_ARCHITECTURES={arch}"));
                }
            }
            // with_receiver_env already set vcvars + CC/CXX=clang-cl; Ninja honours those
            // (the default VS generator would ignore them).
            "windows" => args.push("-GNinja".into()),
            _ => {}
        }

        println!(">> Building static libde265 (HEVC decoder for HEIF) …");
        cmd!(sh, "cmake {args...}")
            .run()
            .context("configuring libde265")?;
        cmd!(
            sh,
            "cmake --build {cmake_build} --config Release --target install --parallel"
        )
        .run()
        .context("building libde265")?;
    }

    if target_os(profile) == "windows" {
        alias_de265_lib(&libdir)?;
        stub_stdcxx_lib(sh, &libdir)?;
    }
    Ok(prefix)
}

/// libde265.pc and libheif.pc both declare `Libs.private: -lstdc++`, a GNU-ism
/// with no meaning under MSVC: the rustc link turns it into `stdc++.lib` and
/// fails with LNK1181. Drop a stub `stdc++.lib` on libde265's link-search dir
/// so the reference resolves while the real C++ symbols come from MSVC's CRT.
/// The archive must contain one (empty) object: link.exe rejects a memberless
/// one with LNK1107. Always recreated, so a stale stub is replaced.
fn stub_stdcxx_lib(sh: &Rc<Shell>, libdir: &Utf8Path) -> Result<()> {
    let dst = libdir.join("stdc++.lib");
    let stub_c = libdir.join("xtask-stdcxx-stub.c");
    let stub_obj = libdir.join("xtask-stdcxx-stub.obj");
    std::fs::write(&stub_c, "\n").context("writing stdc++ stub source")?;
    // clang-cl + llvm-lib are already on PATH for the clang-cl libde265 build.
    cmd!(sh, "clang-cl /nologo /c {stub_c} /Fo{stub_obj}")
        .run()
        .context("compiling stdc++ stub object")?;
    cmd!(sh, "llvm-lib /OUT:{dst} {stub_obj}")
        .run()
        .context("archiving stdc++.lib stub")?;
    let _ = std::fs::remove_file(&stub_c);
    let _ = std::fs::remove_file(&stub_obj);
    println!(">> Created stdc++.lib stub (satisfies -lstdc++ on MSVC)");
    Ok(())
}

/// On Windows cmake installs `libde265.lib`, but the link resolves libheif.pc's
/// `Requires.private: libde265` down to `-lde265`, which link.exe looks up as
/// `de265.lib`. Drop a copy under that name on the same `-L` path. Idempotent.
fn alias_de265_lib(libdir: &Utf8Path) -> Result<()> {
    let src = libdir.join("libde265.lib");
    let dst = libdir.join("de265.lib");
    if dst.exists() || !src.exists() {
        return Ok(());
    }
    std::fs::copy(&src, &dst).context("aliasing libde265.lib to de265.lib")?;
    println!(">> Aliased libde265.lib -> de265.lib for the rustc link");
    Ok(())
}

/// `CMAKE_OSX_ARCHITECTURES` for an explicit macOS target; None = let CMake
/// pick.
fn macos_osx_arch(sh: &Rc<Shell>, profile: &Profile) -> Result<Option<String>> {
    let triple = match profile.target.as_deref() {
        Some(t) => t.to_string(),
        None => host_triple(sh)?,
    };
    Ok(if triple.starts_with("aarch64") {
        Some("arm64".into())
    } else if triple.starts_with("x86_64") {
        Some("x86_64".into())
    } else {
        None
    })
}

/// Write the CMake toolchain that makes libheif-sys's embedded build hermetic:
/// every codec but libde265 force-disabled, and our static libde265 on the
/// prefix path so libheif's own `find_package(LIBDE265)` resolves it.
fn write_libheif_toolchain(build: &GstBuild, de265_prefix: &Utf8Path) -> Result<Utf8PathBuf> {
    let path = build.build_dir.join("xtask-libheif-toolchain.cmake");
    let disables: String = LIBHEIF_DISABLED_CODECS
        .iter()
        .map(|c| format!("set(CMAKE_DISABLE_FIND_PACKAGE_{c} ON)\n"))
        .collect();
    // CMake reads the prefix with forward slashes on every platform.
    let prefix = de265_prefix.as_str().replace('\\', "/");
    std::fs::write(
        &path,
        format!(
            "# Generated by xtask: decode-only, hermetic libheif for the receiver.\n\
             # Keep libde265 (HEVC decode); drop every other codec so no host\n\
             # library (Homebrew/vcpkg) is detected and linked in.\n\
             {disables}list(PREPEND CMAKE_PREFIX_PATH \"{prefix}\")\n"
        ),
    )
    .context("writing libheif toolchain file")?;
    Ok(path)
}

/// Set up the env cargo needs against the static gstreamer (PKG_CONFIG_PATH to
/// the meson-uninstalled .pc + stubs, `SYSTEM_DEPS_*_LINK=static`), then run
/// `f`. Shared by build/run/check/clippy so build-script fingerprints match.
fn with_receiver_env<T>(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    // Windows: the receiver's C deps build with clang-cl inside the MSVC dev env,
    // libplacebo passes gcc-style flags `cl` rejects, while gstreamer itself is
    // built with `cl`; both emit MSVC-ABI archives that link together.
    #[cfg(windows)]
    let _msvc_env: Vec<_> = if target_os(profile) == "windows" {
        let mut env = vcvars_env(sh)?;
        // The standalone LLVM installer isn't on PATH by default.
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

    // HEIC decode (scope=Full only): libheif-sys's vendored libheif needs a HEVC
    // decoder for pixels, so build a static libde265, hand it to libheif's CMake
    // via a toolchain file (which also disables every other codec), and put its
    // .pc on PKG_CONFIG_PATH. After the MSVC env (Windows needs it), before
    // pkg_path. Linux links the system libheif instead. Idempotent.
    let mut heif_guards: Vec<xshell::PushEnv<'_>> = Vec::new();
    let mut libde265_pc: Option<Utf8PathBuf> = None;
    if profile.scope == StaticScope::Full {
        let de265 = build_libde265(sh, build, profile)?;
        let toolchain = write_libheif_toolchain(build, &de265)?;
        heif_guards.push(sh.push_env("CMAKE_TOOLCHAIN_FILE", toolchain.as_str()));
        heif_guards.push(sh.push_env("SYSTEM_DEPS_LIBHEIF_LINK", "static"));
        libde265_pc = Some(de265.join("lib/pkgconfig"));
    }

    // Link against the BUILD TREE via meson-uninstalled .pc (the install tree
    // omits per-plugin .pc, so the gstreamer-full aggregate can't resolve there).
    let mut pkg_path = prepend_env_path(&pkg_config_path(sh), build.uninstalled_pc.as_str());
    if let Some(pc) = &libde265_pc {
        pkg_path = prepend_env_path(&pkg_path, pc.as_str());
    }

    // LINK PHASE ONLY: some distros ship a glib-2.0.pc whose Requires.private lists
    // sysprof-capture-4 without shipping its .pc, and `pkg-config --static`
    // recurses that. An empty stub satisfies the resolver with zero link
    // impact. It MUST NOT be visible during the meson build: subprojects treat
    // it as a real feature.
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

    // Debug/profiling profiles keep frame pointers so `perf --call-graph fp`
    // resolves Rust frames (release-prof's C side gets the matching meson flag).
    // Via RUSTFLAGS so build/check/clippy share unit fingerprints; a plain `cargo
    // build` outside xtask doesn't set it, so alternating the two rebuilds deps.
    let _fp = matches!(
        profile.cargo_profile.as_str(),
        "dev" | "release-dbg" | "release-prof"
    )
    .then(|| {
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
        // dav1d-sys would resolve a DYNAMIC libdav1d via pkg-config. Pin it to the
        // libdav1d.a we already built: NO_PKG_CONFIG bypasses resolution entirely
        // (pregenerated bindings, and its version check would reject the wrap).
        let archives = find_archives(&build.build_dir)?;
        if let Some(a) = archives.get("libdav1d.a") {
            let search = if target_os(profile) == "windows" {
                // rustc links `static=dav1d` as `dav1d.lib`; meson named it libdav1d.a.
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

        // Cross-LTO drives the LLVM plugin via clang/lld; rust-only keeps fat LTO.
        let mut rustc_args: Vec<String> = Vec::new();
        if profile.lto == Lto::Cross {
            rustc_args.push("-Clinker-plugin-lto".into());
            rustc_args.push("-Clinker=clang".into());
            rustc_args.push("-Clink-arg=-fuse-ld=lld".into());
        }
        // Non-cross builds otherwise default to bfd (slow on this binary).
        rustc_args.extend(fast_linker_args(profile));
        for a in &link_args {
            rustc_args.push(format!("-Clink-arg={a}"));
        }

        // Windows caps a command line near 32 KiB and the static link line blows past
        // it; hand rustc an `@argfile` (it response-files the linker itself).
        if target_os(profile) == "windows" {
            let argfile = build.build_dir.join("xtask-rustc-args.txt");
            std::fs::write(&argfile, rustc_args.join("\n")).context("writing rustc argfile")?;
            cargo.push(format!("@{argfile}"));
        } else {
            cargo.extend(rustc_args);
        }

        // The link line carries ~100 `-Clink-arg=<abspath>.a` tokens; print the cargo
        // flags up to `--` and summarise the rest.
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

/// `cargo test` receiver-core against the static gstreamer. The test binary
/// references gstreamer symbols, so it needs the same link line as the
/// receiver; `cargo test` builds several targets, so the args go through
/// rustflags with an explicit --target, keeping host build scripts/proc-macros
/// out of them.
fn receiver_test(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    extra: &[String],
) -> Result<()> {
    with_receiver_env(sh, build, profile, || {
        let link_args = link_args(sh, build, profile)?;

        let mut rustflags: Vec<String> = Vec::new();
        // Cross-LTO drives the LLVM plugin via clang/lld; otherwise pick the fastest
        // available linker the same way build_receiver does.
        if profile.lto == Lto::Cross {
            rustflags.push("-Clinker-plugin-lto".into());
            rustflags.push("-Clinker=clang".into());
            rustflags.push("-Clink-arg=-fuse-ld=lld".into());
        }
        rustflags.extend(fast_linker_args(profile));
        for a in &link_args {
            rustflags.push(format!("-Clink-arg={a}"));
        }

        // CARGO_ENCODED_RUSTFLAGS, not a `[target].rustflags` config file: cargo picks
        // ONE rustflags source and env wins, so with_receiver_env's own RUSTFLAGS would
        // silently shadow the config and drop every link arg (gst_init_static_plugins
        // undefined at link). Its \x1f separator also survives abspaths with spaces,
        // and the ambient RUSTFLAGS is merged in. The forced --target scopes it
        // all to the test binary and its target-side deps.
        let mut encoded: Vec<String> = std::env::var("RUSTFLAGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        encoded.extend(rustflags);
        let _rf = sh.push_env("CARGO_ENCODED_RUSTFLAGS", encoded.join("\x1f"));

        let mut cargo: Vec<String> = vec!["test".into()];
        cargo.extend(receiver_cargo_flags(profile, "receiver-core"));
        // Trailing args go to the libtest harness, like plain `cargo test -- …`.
        if !extra.is_empty() {
            cargo.push("--".into());
            cargo.extend(extra.iter().cloned());
        }
        eprintln!(">> cargo test (static gstreamer) …");
        cmd!(sh, "cargo {cargo...}").run()?;
        Ok(())
    })
}

/// The host target triple from `rustc -vV`. Used to force an explicit --target
/// so link-arg rustflags don't leak into host build scripts/proc-macros.
fn host_triple(sh: &Rc<Shell>) -> Result<String> {
    let out = cmd!(sh, "rustc -vV").read().context("running rustc -vV")?;
    out.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("could not parse host triple from `rustc -vV`"))
}

/// The gstreamer-full static link line: every `-lX` whose archive was built
/// in-tree is rewritten to the `.a`'s absolute path, so the linker cannot fall
/// back to a same-named dynamic library. Also appends the internal helper
/// libraries gstreamer-full's pkg-config omits.
fn link_args(sh: &Rc<Shell>, build: &GstBuild, profile: &Profile) -> Result<Vec<String>> {
    let pkgcfg = pkg_config_prog(sh);
    let raw = cmd!(sh, "{pkgcfg} --static --libs gstreamer-full-1.0")
        .read()
        .context(
            "resolving gstreamer-full-1.0 statically (a private-dep .pc is missing from \
             PKG_CONFIG_PATH, provide it via your environment)",
        )?;

    // Index every built lib*.a so `-lX` can be rewritten to its abspath.
    let archives = find_archives(&build.build_dir)?;
    let macos = target_os(profile) == "macos";

    let mut out = Vec::new();
    for tok in raw.split_whitespace() {
        // `-pthread` is compile-time only; at link it just makes clang warn per copy.
        if tok == "-pthread" {
            continue;
        }
        if let Some(name) = tok.strip_prefix("-l") {
            // macOS' C++ runtime is libc++; a .pc generated against a non-clang toolchain
            // asks for `-lstdc++`, which doesn't exist there.
            if macos && name == "stdc++" {
                out.push("-lc++".to_string());
                continue;
            }
            // meson names static libs `lib<name>.a`, but `<name>.a` on MSVC, and a
            // leftover bare `-l<name>` is silently ignored by link.exe.
            let candidates = [format!("lib{name}.a"), format!("{name}.a")];
            match candidates.iter().find_map(|f| archives.get(f)) {
                Some(path) => out.push(path.to_string()),
                None => out.push(tok.to_string()), // non-built -l stays dynamic
            }
        } else if let Some(dir) = tok.strip_prefix("-R").filter(|d| !d.is_empty()) {
            // Solaris/BSD-ld style rpath (`-R<dir>`, an alias for `-rpath`).
            out.push(format!("-Wl,-rpath,{dir}"));
        } else {
            out.push(tok.to_string());
        }
    }

    // gstreamer-full's pkg-config omits the internal helper libs (riff, fft,
    // codecparsers, …) many plugins reference; --gc-sections drops the unused.
    // Order must be stable across runs: these land in `cargo rustc -- <args>`,
    // which cargo hashes into the unit fingerprint. A varying order mints a new
    // build-dir per build and leaks a full copy of the binary every time.
    for (name, path) in &archives {
        if name.ends_with("-1.0.a") {
            out.push(path.to_string());
        }
    }

    // aarch64 GCC emits outline-atomics calls whose stubs exist only in the STATIC
    // libgcc.a, and rustc places its own -lgcc_s BEFORE these appended archives, so
    // FFmpeg's objects cannot resolve them without a trailing -lgcc.
    let triple = match profile.target.as_deref() {
        Some(t) => t.to_string(),
        None => host_triple(sh)?,
    };
    if triple.starts_with("aarch64") && target_os(profile) == "linux" {
        out.push("-lgcc".to_string());
    }

    // Windows: the dshow/mediafoundation/winks/dmo plugins need COM GUID libs that
    // gstreamer-full's pkg-config doesn't propagate; link.exe finds them in LIB.
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
/// non-`lib*` archives: meson drops the prefix for some libs on MSVC.
/// BTreeMap, not HashMap: callers iterate this to build link args, and
/// HashMap's per-process random order would change them on every run.
fn find_archives(build_dir: &Utf8Path) -> Result<std::collections::BTreeMap<String, String>> {
    let mut map = std::collections::BTreeMap::new();
    // Sorted so a duplicate basename resolves to the same path every run:
    // read_dir order is unspecified, and the winner ends up in the link args.
    let mut entries = walk(build_dir);
    entries.sort_unstable();
    for entry in entries {
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

/// Canonicalize, stripping Windows' verbatim `\\?\` prefix: meson (Python)
/// mishandles those when joining forward-slash relative paths (EINVAL on open).
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

/// A standalone LLVM `bin` dir (the Windows installer doesn't add it to PATH).
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
/// capturing the env it sets (PATH included).
#[cfg(windows)]
fn vcvars_env(sh: &Rc<Shell>) -> Result<Vec<(String, String)>> {
    let vswhere =
        Utf8PathBuf::from("C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe");
    if !vswhere.exists() {
        bail!("vswhere.exe not found, install Visual Studio (with the C++ workload) to build on Windows");
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
    // quoting an inline `cmd /c "call …"` mangles; vcvars wants a backslash path.
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
        bail!("vcvars64.bat did not set LIB, the Windows SDK may be missing");
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paths from `find_archives` end up in `cargo rustc -- <args>`, which
    /// cargo folds into the unit fingerprint. Any run-to-run variation mints a
    /// fresh build directory and leaks a full copy of the receiver binary.
    #[test]
    fn find_archives_is_order_stable() {
        let root = std::env::temp_dir().join(format!("xtask-archives-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Two dirs holding the same basename, plus enough distinct names that a
        // randomized hash order would show up.
        for dir in ["a", "b", "c/d"] {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            for n in 0..8 {
                std::fs::write(d.join(format!("libgst{n}-1.0.a")), b"").unwrap();
            }
            std::fs::write(d.join("libdup.a"), b"").unwrap();
        }
        let root = Utf8PathBuf::from_path_buf(root).unwrap();

        let first = find_archives(&root).unwrap();
        let names: Vec<&str> = first.keys().map(String::as_str).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "keys must iterate in a stable sorted order");

        for _ in 0..4 {
            let again = find_archives(&root).unwrap();
            assert_eq!(
                first.iter().collect::<Vec<_>>(),
                again.iter().collect::<Vec<_>>(),
                "repeated scans must yield identical name -> path pairs"
            );
        }

        // A duplicate basename resolves to the lexicographically first path.
        assert_eq!(first["libdup.a"], root.join("a/libdup.a").as_str());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
