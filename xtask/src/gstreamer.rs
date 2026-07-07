//! Static GStreamer build + link, integrated into xtask.
//!
//! Two phases:
//!   1. `build_gstreamer` — meson `gst-full` static build against a *provided*
//!      source tree (no clone), producing `libgstreamer-full-1.0.a` + the build
//!      tree we link the receiver against (via the generated meson-uninstalled
//!      .pc files).
//!   2. `link_args` + `build_receiver` — resolve the gstreamer-full static link
//!      line, rewrite every `-lgst*` to the in-tree `.a` (so the linker can't
//!      fall back to a dynamic gstreamer), and `cargo rustc --features
//!      static-gstreamer -- <link-args>`.
//!
//! This is the durable version of scripts/build-static-receiver.sh. It assumes a
//! sane build environment provides the pkg-config closure (Flatpak SDK, the Nix
//! flake, brew, …) — it does NOT scavenge /nix/store; that belongs in the flake.
//!
//! Phase 1 targets Linux/Flatpak. macOS/Windows reuse the same meson flow with
//! per-target plugin deltas in [`PluginSet::for_target`]; wire them up once Linux
//! is validated.

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, ValueEnum};
use std::rc::Rc;
use xshell::{Shell, cmd};

use crate::sh;

// ---------------------------------------------------------------------------
// Config as data
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sub {
    Base,
    Good,
    Bad,
    Ugly,
}

impl Sub {
    /// meson subproject option prefix, e.g. `gst-plugins-bad`.
    fn prefix(self) -> &'static str {
        match self {
            Sub::Base => "gst-plugins-base",
            Sub::Good => "gst-plugins-good",
            Sub::Bad => "gst-plugins-bad",
            Sub::Ugly => "gst-plugins-ugly",
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

/// Plugins we force ON (hard requirement — meson errors if the dep is missing).
/// vorbis/theora are native decoders gst-libav deliberately refuses to wrap
/// (it expects the native plugins to exist).
const ENABLE_COMMON: &[(Sub, &str)] = &[(Sub::Base, "vorbis"), (Sub::Base, "theora")];

/// Elements kept from the videoparsersbad plugin (via gst-full-elements). The
/// plugin itself must stay (h264parse/h265parse are essential), but it also
/// bundles image/niche parsers (jpeg2000parse, pngparse, diracparse) — listing
/// elements here makes the generated init register ONLY these, so the other
/// parsers' objects are never pulled from the archive at link time.
const VIDEOPARSERS_ELEMENTS: &[&str] = &[
    "av1parse",
    "h263parse",
    "h264parse",
    "h265parse",
    "h266parse",
    "mpeg4videoparse",
    "mpegvideoparse",
    "vc1parse",
    "vp9parse",
];

/// Linux: VA-API hardware decode; audio via pulse/pipewire (auto). srt because
/// the receiver advertises srt:// support via URI-handler introspection.
/// assrender for styled ASS/SSA subtitles — it attaches overlay-composition
/// meta, so it slots into the receiver's libplacebo (pl_overlay) subtitle
/// compositing path instead of burning into frames. Needs libass.
/// wavpack: avdec_wavpack is on gst-libav's hardcoded skip list, so the native
/// plugin is the ONLY WavPack decoder (seen in the wild as A_WAVPACK4 in
/// matroska). libwavpack comes from the environment on Linux; it has no wrapdb
/// wrap, so the hermetic macOS/Windows builds can't provide it → wavpack is
/// Linux-only (disabled on mac/win, like srt/assrender).
const ENABLE_LINUX: &[(Sub, &str)] = &[
    (Sub::Bad, "va"),
    (Sub::Bad, "srt"),
    (Sub::Bad, "assrender"),
    (Sub::Good, "wavpack"),
];
const DISABLE_LINUX: &[(Sub, &str)] = &[(Sub::Base, "gl")];

/// macOS: VideoToolbox decode + CoreAudio/Cocoa output (matches the plugin set
/// receiver-resources bundles for the dynamic build).
/// applemedia (VideoToolbox HW decode) hard-depends on the gstgl library at
/// compile time — its sources (glcontexthelper, videotexturecache-gl) and
/// `#include <gst/gl/gl.h>` are unconditional, and gstglconfig.h is only
/// generated when `gl` is built. So `gl` must be enabled here even though the
/// receiver's own sink is Vulkan/Wayland (glimagesink is never autoplugged).
/// On macOS gstgl only links system Cocoa/OpenGL frameworks — no external deps.
const ENABLE_MACOS: &[(Sub, &str)] = &[
    (Sub::Bad, "applemedia"),
    (Sub::Good, "osxaudio"),
    (Sub::Good, "osxvideo"),
    (Sub::Base, "gl"),
];
/// macOS static build must link ONLY OS frameworks (the installer verifies via
/// otool). These plugins each pull an external dylib that has no vendored wrap
/// (or is a pure encoder / redundant with libav decode), so instead of building
/// them static we drop the plugin. Everything the receiver actually decodes is
/// covered by libav + the native vorbis/theora/opus/flac/dav1d plugins.
const DISABLE_MACOS: &[(Sub, &str)] = &[
    (Sub::Bad, "va"),
    (Sub::Good, "pulse"),
    // encoders (decode-only receiver)
    (Sub::Bad, "aom"),      // AV1 encode; decode via dav1d
    (Sub::Bad, "svtav1"),   // AV1 encoder
    (Sub::Bad, "voaacenc"), // AAC encoder
    (Sub::Bad, "voamrwbenc"),
    // audio decoders redundant with libav (see FFMPEG_DECODERS)
    (Sub::Good, "mpg123"),   // mp3
    (Sub::Good, "amrnb"),    // opencore-amr
    (Sub::Good, "amrwbdec"), // opencore-amr
    (Sub::Good, "speex"),
    (Sub::Good, "wavpack"),
    // image decoders (receiver decodes images itself; libtiff/libjpeg drop with these)
    (Sub::Good, "gdk-pixbuf"),
    (Sub::Bad, "lcevcdecoder"),
    (Sub::Bad, "lcevcencoder"),
    // niche / unused elements dragging external dylibs
    (Sub::Bad, "zbar"),       // barcode
    (Sub::Bad, "sbc"),        // bluetooth audio
    (Sub::Bad, "soundtouch"), // pitch/tempo
    (Sub::Good, "dv"),        // DV video
    (Sub::Bad, "rtmp"),
    (Sub::Bad, "curl"), // http via souphttpsrc / the receiver's own httpsrc
    (Sub::Bad, "sndfile"),
    (Sub::Bad, "spandsp"), // dtmf/fax
    (Sub::Good, "taglib"), // metadata tagging
    (Sub::Good, "bz2"),    // libbz2 support in matroska (bz2-compressed tracks; no wrap)
    // no vendored wrap → can't link static on macOS (kept on Linux)
    (Sub::Bad, "srt"),
    (Sub::Bad, "assrender"),
];

/// Windows: WASAPI audio (matches receiver-resources' bundled set). d3d11 etc.
/// stay `auto`. NOTE: static gst-full on MSVC is upstream-experimental.
const ENABLE_WINDOWS: &[(Sub, &str)] = &[(Sub::Bad, "wasapi")];
const DISABLE_WINDOWS: &[(Sub, &str)] =
    &[(Sub::Bad, "va"), (Sub::Good, "pulse"), (Sub::Base, "gl"), (Sub::Good, "wavpack")];

/// Plugins removed everywhere: unused by a cast receiver, or GPU/vendor codecs
/// whose companion support library gstreamer-full fails to pull statically.
/// (Kept intentionally: videofilter, audiobuffersplit, proxy — autoplugged.)
const DISABLE_COMMON: &[(Sub, &str)] = &[
    // vendor GPU codecs
    (Sub::Bad, "hip"),
    (Sub::Bad, "nvcodec"),
    (Sub::Bad, "qsv"),
    (Sub::Bad, "vulkan"),
    (Sub::Bad, "amfcodec"), // AMD encode-only; even registers on Linux (dlopen)
    // orphan / useless (registered-but-unlinked, or metric/gadget)
    (Sub::Bad, "vmaf"),
    (Sub::Bad, "uvcgadget"),
    // X11 video (receiver uses its own Vulkan/Wayland sink). NOTE: `gl` is NOT
    // disabled here — macOS's applemedia hard-requires the gstgl library, so it
    // is disabled per-target (Linux/Windows) and enabled on macOS instead.
    (Sub::Base, "x11"),
    (Sub::Good, "ximagesrc"),
    // image codecs (receiver decodes images itself)
    (Sub::Good, "jpeg"),
    (Sub::Good, "png"),
    (Sub::Bad, "openjpeg"),
    (Sub::Bad, "webp"),
    (Sub::Bad, "jpegformat"),
    (Sub::Bad, "jp2kdecimator"),
    // SVG decode/overlay: unused (receiver decodes images itself) and, when
    // librsvg is discoverable, links dynamically against a system/dev-kit
    // librsvg — defeating the self-contained static build (and its .pc's
    // Libs.private leaks a bare `-no_compact_unwind` ld flag that breaks clang).
    (Sub::Bad, "rsvg"),
    // redundant codecs (libav provides decode)
    (Sub::Bad, "openh264"),
    (Sub::Bad, "fdkaac"),
    // vp8/vp9 decode comes from FFmpeg's native decoders; the vpx plugin would
    // drag in the libvpx wrap, which force-builds encoders too (~600s cpu +
    // binary bloat for a decode-only receiver)
    (Sub::Good, "vpx"),
    // effects / visualizers
    (Sub::Bad, "gaudieffects"),
    (Sub::Bad, "audiovisualizers"),
    (Sub::Bad, "coloreffects"),
    (Sub::Bad, "geometrictransform"),
    (Sub::Bad, "videofilters"),
    (Sub::Bad, "freeverb"),
    (Sub::Bad, "frei0r"),
    (Sub::Good, "goom"),
    (Sub::Good, "goom2k1"),
    (Sub::Good, "monoscope"),
    (Sub::Good, "spectrum"),
    (Sub::Good, "shapewipe"),
    (Sub::Good, "smpte"),
    (Sub::Good, "videobox"),
    (Sub::Good, "videocrop"),
    (Sub::Good, "videomixer"),
    (Sub::Good, "cutter"),
    (Sub::Good, "imagefreeze"),
    (Sub::Good, "replaygain"),
    // ML / analytics
    (Sub::Bad, "tensordecoders"),
    (Sub::Bad, "analyticsoverlay"),
    (Sub::Bad, "faceoverlay"),
    (Sub::Bad, "fieldanalysis"),
    (Sub::Bad, "videosignal"),
    (Sub::Bad, "bayer"),
    // audio-processing plugins that drag in the huge webrtc-audio-processing
    // C++ subproject (~700 cpu-seconds of build for features we never use)
    (Sub::Bad, "webrtcdsp"),
    (Sub::Bad, "isac"),
    // encoders / muxers (decode-only receiver)
    (Sub::Good, "lame"),
    (Sub::Bad, "adpcmenc"),
    (Sub::Bad, "asfmux"),
    (Sub::Bad, "dvbsubenc"),
    (Sub::Bad, "mpegpsmux"),
    (Sub::Bad, "mpegtsmux"),
    (Sub::Bad, "subenc"),
    (Sub::Good, "wavenc"),
    (Sub::Good, "xingmux"),
    (Sub::Bad, "id3tag"), // id3v2mux/id3mux: ID3 tag *muxer*, encode-side only
    // audio channel interleave/deinterleave: not autoplugged in playback
    (Sub::Good, "interleave"),
    // capture / hardware IO / IPC
    (Sub::Bad, "camerabin2"),
    (Sub::Bad, "decklink"),
    (Sub::Bad, "ipcpipeline"),
    (Sub::Bad, "fbdev"),
    (Sub::Bad, "kms"),
    (Sub::Bad, "shm"),
    (Sub::Bad, "librfb"),
    (Sub::Bad, "unixfd"),
    // v4l2src/v4l2sink/v4l2radio: pure video capture/output — a cast receiver
    // never captures. (Linux HW decode comes from `va`, not v4l2, and the
    // stateless v4l2codecs decoders aren't built.)
    (Sub::Good, "v4l2"),
    (Sub::Base, "alsa"),
    (Sub::Good, "oss"),
    (Sub::Good, "oss4"),
    // legacy adaptive-streaming plugins: hlsdemux/dashdemux are superseded by
    // adaptivedemux2's hlsdemux2/dashdemux2 (what playbin3 autoplugs), and they
    // are also where hlssink/hlssink2/dashsink live — sinks we never use
    (Sub::Bad, "hls"),
    (Sub::Bad, "dash"),
    // test/debug/util elements never autoplugged in playback
    (Sub::Base, "audiotestsrc"),
    (Sub::Base, "videotestsrc"),
    (Sub::Base, "debugutils"),
    (Sub::Good, "debugutils"),
    (Sub::Bad, "debugutils"), // fakeaudiosink/fakevideosink/testsrcbin/…
    (Sub::Good, "effectv"),
    (Sub::Bad, "audiolatency"),
    (Sub::Bad, "festival"),
    (Sub::Bad, "smooth"),
    (Sub::Bad, "speed"),
    (Sub::Bad, "interlace"),
    (Sub::Bad, "codectimestamper"),
    (Sub::Bad, "codecalpha"),
    (Sub::Bad, "closedcaption"),
    // bad's `rtp` option gates the rtpmanagerbad plugin (rtpsrc/rtpsink);
    // distinct from good's `rtp` (the depayloaders), which stays enabled
    (Sub::Bad, "rtp"),
    // mixing/compositing/encoding infrastructure unused by this receiver
    (Sub::Base, "adder"),
    (Sub::Base, "audiomixer"),
    (Sub::Base, "compositor"),
    (Sub::Base, "encoding"),
    (Sub::Base, "rawparse"),
    (Sub::Base, "videorate"),
    (Sub::Base, "audiorate"),
    (Sub::Base, "dsd"),
    (Sub::Bad, "rawparse"), // gates the legacyrawparse plugin
    // audio effects / niche audio IO
    (Sub::Good, "alpha"),
    (Sub::Good, "apetag"),
    (Sub::Good, "auparse"),
    (Sub::Good, "cairo"),
    (Sub::Good, "dtmf"),
    (Sub::Good, "equalizer"),
    (Sub::Good, "jack"),
    (Sub::Good, "y4m"),
    (Sub::Bad, "dvb"),
    // niche demux/parse/format
    (Sub::Bad, "transcode"),
    (Sub::Bad, "bz2"),
    (Sub::Bad, "aes"),
    (Sub::Bad, "segmentclip"),
    (Sub::Bad, "audiofxbad"),
    (Sub::Bad, "audiomixmatrix"),
    (Sub::Bad, "gdp"),
    (Sub::Bad, "midi"),
    (Sub::Bad, "netsim"),
    (Sub::Bad, "onvif"),
    (Sub::Bad, "pcapparse"),
    (Sub::Bad, "pnm"),
    (Sub::Bad, "removesilence"),
    (Sub::Bad, "rist"),
    (Sub::Bad, "siren"),
    (Sub::Bad, "videoframe_audiolevel"),
    (Sub::Bad, "accurip"),
    (Sub::Bad, "adpcmdec"),
    (Sub::Bad, "aiff"),
    (Sub::Bad, "autoconvert"),
    (Sub::Bad, "insertbin"),
    (Sub::Bad, "inter"),
    (Sub::Bad, "ivfparse"),
    (Sub::Bad, "ivtc"),
    (Sub::Bad, "mse"),
    (Sub::Bad, "mxf"),
    (Sub::Bad, "switchbin"),
    (Sub::Bad, "timecode"),
    (Sub::Bad, "vmnc"),
    (Sub::Bad, "smoothstreaming"),
    (Sub::Good, "law"),
    (Sub::Good, "flx"),
    (Sub::Good, "level"),
    (Sub::Good, "multifile"),
    (Sub::Good, "multipart"),
    (Sub::Ugly, "realmedia"),
    // ASF/WMV/WMA (Microsoft, ~1999, deprecated by its own vendor): the asfdemux
    // plugin also carries rtspwms + rtpasfdepay. Nothing in 2026 produces ASF for
    // casting; the WMV/WMA avdec_* decoders are dropped from FFMPEG_DECODERS too.
    (Sub::Ugly, "asfdemux"),
];

/// FFmpeg decoders to keep (via gst-libav's `avdec_*`). We disable ALL FFmpeg
/// decoders/demuxers/protocols and re-enable only these — libavcodec's full
/// decoder set (hundreds, incl. ancient game codecs) is otherwise dead weight.
/// vp8/vp9 use FFmpeg's NATIVE decoders (not the libvpx wrappers) — this
/// replaces the gstvpx plugin + the libvpx wrap subproject entirely (libvpx
/// bundles encoders we can't use: ~600 cpu-seconds of build + binary bloat).
/// Deliberately excluded because native gst plugins cover them: av1 (dav1d),
/// vorbis/theora (native), opus/flac (native). gst-libav also skips registering
/// avdec for vorbis/theora/wavpack/mp1/mp2/av1 regardless (hardcoded skip lists).
/// Demuxing/parsing is done by native gst elements (qtdemux, matroskademux,
/// h264parse, …), so FFmpeg demuxers/parsers/protocols aren't needed.
const FFMPEG_DECODERS: &[&str] = &[
    // video. vc1 is kept: it's also carried in MKV/TS/Blu-ray remuxes (demuxed by
    // matroskademux/tsdemux), so it's not exclusively an ASF/WMV concern.
    "h264", "hevc", "mpeg2video", "mpeg4", "mpeg1video", "msmpeg4v1", "msmpeg4v2",
    "msmpeg4v3", "h263", "h263p", "vc1", "vp6", "vp6f", "flv",
    "mjpeg", "prores", "vp8", "vp9",
    // audio
    "aac", "aac_latm", "ac3", "eac3", "mp3", "mp2", "mp1", "dca", "alac",
    "truehd", "mlp", "amrnb", "amrwb",
    // pcm / adpcm (pcm_bluray = LPCM in .m2ts Blu-ray remuxes)
    "pcm_s16le", "pcm_s16be", "pcm_s24le", "pcm_u8", "pcm_f32le", "pcm_alaw",
    "pcm_mulaw", "pcm_bluray", "adpcm_ima_wav", "adpcm_ms",
];

/// Wraps force-fallbacked in scope=Full so ONE static glib (plus the pango
/// text stack it shares) is built from the monorepo's vendored wraps instead
/// of being found on the system. This is what lets macOS/Windows builds run
/// without the GStreamer dev kit: the kit's only remaining job was providing
/// these as DLLs/dylibs. Forcing (rather than relying on not-found fallback)
/// keeps the build deterministic when a stray brew/system copy exists.
/// freetype2/fontconfig are only looked up on platforms whose text backend
/// wants them (pango uses CoreText on macOS, DirectWrite on Windows) —
/// forcing a dep nobody requests is a no-op.
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
    // Codec + support libraries the receiver's kept plugins pull in. Without
    // these, meson resolves them dynamically from whatever pkg-config finds
    // (on macOS: the GStreamer.framework dev kit), leaving @rpath dylibs that
    // dangle on user machines. Forcing the wraps builds them static into
    // gstreamer-full. Dependency names (not wrap filenames) — must match what
    // each plugin's `dependency('...')` looks up. See each wrap's [provide].
    "ogg",
    "vorbis",
    "vorbisenc",
    "theora",
    "theoradec",
    "theoraenc",
    "opus",
    "flac",
    "dav1d",
    "libsrtp2",   // srtp plugin (webrtc security)
    "json-glib-1.0",
    "graphene-1.0",
    "graphene-gobject-1.0",
    "libjpeg", // gstopengl's gloverlay (gl enabled for applemedia) requires it
    // openssl (glib-networking TLS backend + dtls/srtp). ensure_wrap vendors
    // the .wrap; forcing it builds libcrypto/libssl static instead of linking
    // the dev kit's.
    "openssl",
    "libcrypto",
    "libssl",
    // souphttpsrc stack — http(s) media sources (playbin3/adaptivedemux2).
    "libsoup-3.0",
    "libxml-2.0",
    "libpsl",
    "libnghttp2",
];

/// system-deps entries additionally forced static in scope=Full: the glib the
/// Rust side links must be the SAME static glib compiled into gstreamer-full
/// (two glibs = "cannot register existing type 'GstObject'"). dav1d is built
/// from its wrap in-tree, and dav1d-sys (gstdav1d) must link that archive.
const SYSTEM_DEPS_FULL_SCOPE: &[&str] = &["GLIB_2_0", "GOBJECT_2_0", "GIO_2_0", "DAV1D"];

/// pkg-config modules that must be resolvable for a *Linux* build to succeed.
/// These are provided by a real environment (Flatpak SDK / the Nix flake); we
/// only assert they're present and fail with an actionable message otherwise.
/// On macOS/Windows the codec libs come from meson wraps and the platform
/// plugins from OS frameworks, so no equivalent assertion is needed.
const REQUIRED_BUILD_PC_LINUX: &[&str] = &[
    "vorbis", "vorbisenc", "theora", "theoradec", "ogg", "libva", "libva-drm", "gudev-1.0",
    "srt", "libass", "wavpack",
];

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StaticScope {
    /// gstreamer + codecs static; glib/pango/OS dynamic. For Linux/Flatpak,
    /// where the system/runtime provides (and must provide) glib.
    Gstreamer,
    /// Additionally build glib + the pango stack + TLS static from the
    /// monorepo's vendored wraps → one glib → standalone binary with no
    /// GStreamer dev kit needed. Default for macOS/Windows. NOT for Flatpak
    /// (glib comes from the runtime).
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Lto {
    /// No LTO beyond the cargo profile default.
    Off,
    /// Rust-only fat LTO (the `wild` linker is fine).
    Rust,
    /// Cross-language Rust↔C LTO: clang `-Db_lto` on the C side + rustc
    /// `-Clinker-plugin-lto` + `clang -fuse-ld=lld`. Requires rustc's LLVM and
    /// clang's LLVM to be the same major version.
    Cross,
}

#[derive(Clone)]
struct Profile {
    scope: StaticScope,
    lto: Lto,
    offline: bool,
    target: Option<String>,
    /// Build the receiver in the cargo dev profile (debuggable, faster to build).
    /// GStreamer itself stays a release build — see `gst_buildtype`.
    debug: bool,
    /// meson buildtype for GStreamer (default "release"). `debugoptimized`/`debug`
    /// give symbols if you actually need to step into gstreamer.
    gst_buildtype: String,
    /// Pass --no-default-features to cargo (e.g. no systray on macOS).
    no_default_features: bool,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const GST_REPO: &str = "https://gitlab.freedesktop.org/gstreamer/gstreamer.git";

#[derive(Args)]
pub struct GstreamerArgs {
    /// Path to a GStreamer mono-repo checkout (git source). If omitted, xtask
    /// clones `--gst-ref` into target/ (requires network; incompatible with
    /// `--offline`). For Flatpak/offline builds, provide the source explicitly.
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
    /// Defaults per target OS: `gstreamer` on Linux (glib comes from the
    /// system/Flatpak runtime), `full` on macOS/Windows (glib + the pango
    /// stack are built static from wraps — no GStreamer dev kit needed).
    #[arg(long, value_enum)]
    pub static_scope: Option<StaticScope>,
    #[arg(long, value_enum, default_value_t = Lto::Off)]
    lto: Lto,
    /// Build the receiver as a debug (cargo dev) build. GStreamer stays release
    /// unless you also pass --gst-buildtype.
    #[arg(long)]
    debug: bool,
    /// meson buildtype for GStreamer itself (e.g. release, debugoptimized, debug).
    #[arg(long, default_value = "release")]
    gst_buildtype: String,
    /// Only build gstreamer, don't build the receiver.
    #[arg(long)]
    gstreamer_only: bool,
    /// Build the receiver with --no-default-features (e.g. no systray on macOS).
    #[arg(long)]
    pub no_default_features: bool,
    /// Remove built/downloaded artifacts and exit (like `cargo clean`). Removes
    /// the meson build dir + install prefix; also removes the auto-cloned source,
    /// but never a tree you passed via --source.
    #[arg(long)]
    clean: bool,
}

impl GstreamerArgs {
    pub fn run(self) -> Result<()> {
        self.build().map(|_| ())
    }

    /// Build (or reuse) the static GStreamer and return the pieces needed to
    /// drive cargo against it. Returns `Ok(None)` when `--clean` short-circuits.
    fn prepare(self) -> Result<Option<(Rc<Shell>, Profile, GstBuild)>> {
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
            debug: self.debug,
            gst_buildtype: self.gst_buildtype.clone(),
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
        let build_dir = self
            .build_dir
            .unwrap_or_else(|| source.join("builddir-static"));

        let build = build_gstreamer(&sh, &source, &build_dir, &profile)?;
        Ok(Some((sh, profile, build)))
    }

    /// Build the static gstreamer (+ receiver unless --gstreamer-only) and
    /// return the path to the receiver binary. Used by the installer commands.
    pub fn build(self) -> Result<Option<Utf8PathBuf>> {
        let gstreamer_only = self.gstreamer_only;
        let Some((sh, profile, build)) = self.prepare()? else {
            return Ok(None);
        };
        if gstreamer_only {
            return Ok(None);
        }
        build_receiver(&sh, &build, &profile).map(Some)
    }

    /// Build the static receiver and execute it, forwarding `args` (everything
    /// after `--`). Propagates the receiver's own exit code.
    ///
    /// Mirrors `cargo run`: a fast dev (debug) build by default; `release` opts
    /// into the optimized (fat-LTO) build. An explicit `--debug` also forces
    /// debug and wins over `--release`.
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

    /// `cargo check` the receiver against the static GStreamer.
    pub fn check(self) -> Result<()> {
        self.cargo_subcmd("check")
    }

    /// `cargo clippy` the receiver against the static GStreamer.
    pub fn clippy(self) -> Result<()> {
        self.cargo_subcmd("clippy")
    }

    fn cargo_subcmd(self, subcmd: &str) -> Result<()> {
        let Some((sh, profile, build)) = self.prepare()? else {
            return Ok(());
        };
        receiver_cargo(&sh, &build, &profile, subcmd)
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
    // Absolute path (workspace root + target/…): the shared shell runs git from
    // the pushed root_path, but std::fs uses the process cwd, which isn't
    // guaranteed to be the workspace root — a relative path made the two
    // disagree (git saw the checkout, read_dir didn't → spurious clone).
    let dir = crate::workspace::root_path()?.join("target/gstreamer-src");
    if checkout_present(&dir) {
        // Reuse the existing clone — no network. Warn if it's on a different
        // ref than requested so a stale checkout doesn't surprise anyone.
        // A tag checkout is a detached HEAD ("HEAD" from --abbrev-ref), so
        // also match against an exact tag.
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
            let current = if head.trim() == "HEAD" { tag.trim() } else { head.trim() };
            println!(
                ">> Reusing GStreamer checkout at {dir} (on '{current}', requested '{gst_ref}') — \
                 pass --clean first if you want a fresh clone",
            );
        } else {
            println!(">> Reusing GStreamer checkout at {dir}");
        }
    } else {
        println!(">> Cloning GStreamer {gst_ref} into {dir} …");
        if let Err(e) = cmd!(sh, "git clone --depth 1 --branch {gst_ref} {GST_REPO} {dir}").run() {
            // The presence probe can transiently false-negative on Windows (see
            // checkout_present); the clone then fails because the dir is in fact
            // there ("already exists and is not empty"). Reuse it rather than
            // aborting — only a genuinely-absent dir is a real error.
            if !checkout_present(&dir) {
                return Err(e).context("cloning gstreamer source");
            }
            println!(">> {dir} already present — reusing existing checkout");
        }
    }
    Ok(dir)
}

/// Is the GStreamer checkout present (dir exists and is non-empty)? Retries a
/// few times: listing this large tree can briefly fail on Windows (AV / open
/// handles) right after a prior build's heavy I/O. A false "absent" would clone
/// over a real checkout, so only conclude absence once the retries are spent.
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
}

// ---------------------------------------------------------------------------
// Phase 1: build gstreamer
// ---------------------------------------------------------------------------

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

fn build_gstreamer(
    sh: &Rc<Shell>,
    source: &Utf8Path,
    build_dir: &Utf8Path,
    profile: &Profile,
) -> Result<GstBuild> {
    if !source.join("meson.build").exists() {
        bail!("{source} does not look like a GStreamer source tree (no meson.build)");
    }

    let os = target_os(profile);

    // The env must supply the pkg-config closure (flake / Flatpak SDK / brew).
    // Assert the build deps are present with an actionable error rather than a
    // cryptic meson failure deep in a subproject.
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
                 Provide them via your environment (flake buildInputs / Flatpak SDK).",
                missing.join(", ")
            );
        }
    }

    // scope=Full must be HERMETIC: everything comes from the vendored wraps or
    // OS frameworks, nothing from the host. But a rich Homebrew / GStreamer
    // dev-kit install exposes dozens of optional libs via pkg-config (libde265,
    // openexr, lcms2, libthai, libidn2, x11, …) that `-Dgst-full-plugins=*` plus
    // `auto` plugin/feature flags silently detect and link — leaving dynamic
    // deps that dangle on end-user machines (and whose presence in the final
    // binary flips depending on --gc-sections, so it "works" on one host and
    // fails on another). Blank out pkg-config for the whole gstreamer build so
    // meson can only see the wraps + frameworks: unforced optional deps then
    // fall back to a wrap where one is vendored, or auto-disable. Empty
    // PKG_CONFIG_LIBDIR (a real empty dir) also overrides pkg-config's
    // compiled-in default search path (Homebrew's on this host). The guard is
    // scoped to this fn — build_receiver restores the env and sets its own
    // PKG_CONFIG_PATH to resolve the in-tree gstreamer-full.
    let _pc_isolate = (profile.scope == StaticScope::Full).then(|| {
        let empty = source.join(".xtask-empty-pkgconfig");
        let _ = std::fs::create_dir_all(&empty);
        (
            sh.push_env("PKG_CONFIG_PATH", ""),
            sh.push_env("PKG_CONFIG_LIBDIR", empty.as_str()),
        )
    });

    // Deps always taken from wraps (never the system): the decode-only FFmpeg
    // fork. scope=Full adds the glib/pango closure. NOTE: repeated
    // --force-fallback-for flags override each other — this must stay ONE flag.
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
            if profile.offline { "nodownload" } else { "default" }
        ),
        format!("--force-fallback-for={}", fallback.join(",")),
        "-Dgst-full-target-type=static_library".into(),
        "-Dgst-full-plugins=*".into(),
        // Element-level whitelist: plugins named here register ONLY the listed
        // elements (the rest of that plugin's element objects never link).
        format!(
            "-Dgst-full-elements=videoparsersbad:{}",
            VIDEOPARSERS_ELEMENTS.join(",")
        ),
        format!("-Dgst-full-libraries={}", FULL_LIBRARIES.join(",")),
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
        // decode-only FFmpeg, and only the decoders we actually use (see below).
        // Demuxers/protocols come from native gst elements, not libavformat.
        "-DFFmpeg:encoders=disabled".into(),
        "-DFFmpeg:muxers=disabled".into(),
        "-DFFmpeg:programs=disabled".into(),
        "-DFFmpeg:tests=disabled".into(),
        "-DFFmpeg:decoders=disabled".into(),
        "-DFFmpeg:demuxers=disabled".into(),
        "-DFFmpeg:protocols=disabled".into(),
        // All ~450 avfilters are dead weight (native gst elements filter for us;
        // costs ~7 min cpu + dominates the build's serial tail). libavfilter
        // itself still builds — gst-libav hard-requires the library. Trade-off:
        // gst-libav's avdeinterlace loses its backend; native deinterlace covers it.
        "-DFFmpeg:filters=disabled".into(),
        // FFmpeg auto-detects system bz2 (compressed-matroska support, extremely
        // rare) and links it dynamically — no bz2 wrap exists, so drop it.
        "-DFFmpeg:bzlib=disabled".into(),
        // AV1 decode is the Rust gst-plugin-dav1d (dav1d-sys, pkg-config). With
        // rs=disabled nothing in the meson build requests dav1d, so the wrap is
        // never built and dav1d-sys links the framework's dynamic libdav1d.
        // Enabling FFmpeg's libdav1d makes meson request dependency('dav1d'),
        // which force-fallback (see `fallback`) builds static — producing the
        // libdav1d.a + dav1d-uninstalled.pc that dav1d-sys then links statically.
        "-DFFmpeg:libdav1d=enabled".into(),
    ];
    for dec in FFMPEG_DECODERS {
        args.push(format!("-DFFmpeg:{dec}_decoder=enabled"));
    }

    // vorbis/theora headers include <ogg/ogg.h>, but ogg is only in their .pc's
    // Requires.private and pkgconf does not propagate private include dirs to
    // `--cflags`. On split-prefix systems (Nix) the compiler then can't find
    // ogg.h. Pass the include dirs explicitly; harmless where /usr/include
    // already covers them. Linux-only: on macOS/Windows ogg/vorbis/theora come
    // from meson wraps (meson supplies the include dirs itself), and injecting a
    // system pkg-config path here is both pointless and breaks MSVC — pkg-config
    // backslash-escapes spaces (`C:/Program\ Files/...`), which `cl` mis-splits.
    if os == "linux" {
        let pkgcfg = pkg_config_prog(sh);
        if let Ok(ogg_cflags) = cmd!(sh, "{pkgcfg} --cflags-only-I ogg").quiet().read() {
            let ogg_cflags = ogg_cflags.trim();
            if !ogg_cflags.is_empty() {
                args.push(format!("-Dc_args={ogg_cflags}"));
            }
        }
    }

    let (enable_os, disable_os): (&[(Sub, &str)], &[(Sub, &str)]) = match os {
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

    // Full-static scope: glib + the pango stack come from wraps (see
    // FULL_SCOPE_FALLBACK above) so there is a single, static glib. NOT
    // compatible with Flatpak (the runtime provides glib).
    if profile.scope == StaticScope::Full {
        // With glib internal, the monorepo builds glib-networking as a
        // subproject and statically links its GIO TLS module into
        // gstreamer-full; gst_init_static_plugins() then registers it via
        // g_io_<module>_load() — https for libsoup/adaptivedemux2 works with
        // no runtime GIO modules. gnutls has no wrap, so use the openssl
        // backend (openssl comes from wrapdb, see ensure_wrap).
        args.push("-Dtls=enabled".into());
        args.push("-Dglib-networking:gnutls=disabled".into());
        args.push("-Dglib-networking:openssl=enabled".into());
        args.push("-Dglib-networking:libproxy=disabled".into());
        args.push("-Dglib-networking:gnome_proxy=disabled".into());
        // Keep the glib subproject lean; introspection would drag in the
        // gobject-introspection wrap (unbuildable on mac/win anyway).
        args.push("-Dglib:tests=false".into());
        args.push("-Dglib:introspection=disabled".into());
        args.push("-Dpango:introspection=disabled".into());
        // openssl: glib-networking's TLS backend (gnutls has no wrap).
        ensure_wrap(sh, source, profile, "openssl")?;
        // libnice (WebRTC ICE) picks a DTLS crypto backend; left on `auto` it
        // prefers gnutls, which it finds via meson's cmake fallback in Homebrew
        // (pkg-config isolation doesn't cover cmake) and links dynamically —
        // an @rpath dylib the installer rejects. Force openssl, which resolves
        // to the static openssl wrap we already build (gnutls has no wrap).
        args.push("-Dlibnice:crypto-library=openssl".into());
        // cairo has several `auto` optional features that turn ON whenever the
        // build host happens to expose the lib (XQuartz, Homebrew, …), pulling
        // code/deps a macOS/Windows text stack never needs and that break the
        // build (X11-only sources; a Homebrew lzo with a broken include path).
        // Force them all off for a deterministic build; pango only needs
        // cairo's quartz/image surfaces.
        args.push("-Dcairo:xlib=disabled".into()); // X11 backend (cairo-xlib-screen.c)
        args.push("-Dcairo:xcb=disabled".into());
        args.push("-Dcairo:lzo=disabled".into()); // cairo-script compression (Homebrew lzo)
        args.push("-Dcairo:spectre=disabled".into()); // PS preview (libspectre)
        args.push("-Dcairo:symbol-lookup=disabled".into()); // binutils/bfd
        args.push("-Dcairo:tests=disabled".into());
    }

    // meson captures PKG_CONFIG_PATH at first setup and ignores env changes on
    // --reconfigure, so wipe when the environment changed. And when NOTHING
    // changed (same env + same meson args), skip `meson setup` entirely —
    // re-running configure costs 1-2 minutes per iteration for no effect;
    // `meson compile` (ninja) alone detects source changes just fine.
    let stamp = format!("{}\n{}", pkg_config_path(sh), args.join(" "));
    let configured = build_dir.join("meson-private/coredata.dat").exists();
    let reconf = if configured && stamp_read(build_dir).as_deref() == Some(stamp.as_str()) {
        None
    } else if configured {
        println!(">> Build environment/options changed — wiping build dir");
        Some("--wipe")
    } else {
        Some("--reconfigure") // fresh dir: acts as plain setup
    };

    // Compiler selection for the gstreamer C/C++ build.
    //  - Windows uses MSVC `cl` — the compiler the GStreamer wrap ecosystem
    //    targets on Windows. Countless meson checks (gstreamer's flex
    //    `--nounistd`, glib's SSIZE_T, the openssl wrap, …) gate Windows
    //    behaviour on `cc.get_id() == 'msvc'`, which only `cl` satisfies; clang
    //    /clang-cl fall through those and break. `cl` is found via the vcvars
    //    import below.
    //  - macOS and cross-language LTO use clang/clang++: cross-LTO needs the C
    //    objects to carry LLVM bitcode, and on macOS the C++ runtime must be
    //    libc++ — a non-clang toolchain (a Homebrew gcc/g++ on PATH) makes C++
    //    wraps like harfbuzz resolve libstdc++ and emit `-lstdc++`, which doesn't
    //    exist on macOS. Apple's cc/c++ are already clang, so this is a no-op on
    //    a clean host. (link_args also rewrites any stray -lstdc++ → -lc++.)
    let (cc, cxx) = if os == "windows" {
        (Some("cl"), Some("cl"))
    } else if profile.lto == Lto::Cross || os == "macos" {
        (Some("clang"), Some("clang++"))
    } else {
        (None, None)
    };

    // Assemble the build environment. Each var is pushed exactly once — pushing
    // the same key twice would clobber it — so PATH is fully composed here first.
    let mut build_env: Vec<(String, String)> = Vec::new();
    let mut path = sh.var("PATH").unwrap_or_default();

    // Windows: import the MSVC developer environment from vcvars64 (INCLUDE /
    // LIB / LIBPATH / PATH). This is what puts `cl` (and dumpbin/link, which
    // FFmpeg's makedef step needs) on PATH and points the compiler at the
    // Windows SDK headers (INCLUDE) and libraries (LIB) — meson's
    // find_library('ws2_32', …) and the SDK includes fail without them.
    // Imported before CC/CXX so the compiler inherits it.
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
    if matches!(cc, Some("clang")) && !on_path(sh, "clang") {
        let dir = find_llvm_bin()
            .context("clang not on PATH and no LLVM install found; install LLVM")?;
        path = prepend_env_path(&path, dir.as_str());
    }

    build_env.push(("PATH".to_string(), path));
    let _build_env: Vec<_> = build_env
        .into_iter()
        .map(|(k, v)| sh.push_env(k, v))
        .collect();

    let _cc = cc.map(|c| sh.push_env("CC", c));
    let _cxx = cxx.map(|c| sh.push_env("CXX", c));

    // Windows' installer-detection heuristic flags `patch.exe` — which meson
    // runs to apply wrap diff files (e.g. glib's) — as needing UAC elevation,
    // so CreateProcess fails with `WinError 740` before it can run. RunAsInvoker
    // forces children to inherit our (unelevated) token instead of trying to
    // elevate; patch.exe doesn't actually need elevation. Inherited down through
    // the meson (python) process to its patch.exe grandchild.
    #[cfg(windows)]
    let _no_elevate = sh.push_env("__COMPAT_LAYER", "RunAsInvoker");

    let cross = profile.target.as_ref().map(|t| cross_file(sh, source, t)).transpose()?;
    let cross_args: Vec<String> = cross
        .iter()
        .flat_map(|f| vec!["--cross-file".to_string(), f.to_string()])
        .collect();

    if let Some(reconf) = reconf {
        println!(">> Configuring static GStreamer ({reconf}) …");
        cmd!(sh, "meson setup {build_dir} {source} {reconf} {cross_args...} {args...}").run()?;
    } else {
        println!(">> GStreamer configuration unchanged — skipping meson setup");
    }

    println!(">> Building GStreamer …");
    cmd!(sh, "meson compile -C {build_dir}").run()?;

    // Record env + options so the next run can detect changes.
    stamp_write(build_dir, &stamp)?;

    Ok(GstBuild {
        build_dir: build_dir.to_owned(),
        uninstalled_pc: build_dir.join("meson-uninstalled"),
    })
}


fn pkg_config_path(sh: &Rc<Shell>) -> String {
    sh.var("PKG_CONFIG_PATH").unwrap_or_default()
}

/// The pkg-config program to invoke. Honors `$PKG_CONFIG` (system-deps' knob,
/// which the Windows CI sets to `pkgconf` — there may be no `pkg-config`
/// binary), falling back to `pkg-config`.
fn pkg_config_prog(sh: &Rc<Shell>) -> String {
    sh.var("PKG_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "pkg-config".to_string())
}

/// Make sure a wrap the monorepo doesn't vendor is present (from wrapdb).
/// `meson wrap install` only drops the .wrap file into <source>/subprojects —
/// the actual source download happens at setup time like every other wrap.
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

// ---------------------------------------------------------------------------
// Phase 2: link the receiver
// ---------------------------------------------------------------------------

/// Path of the receiver binary a build with `profile` produces.
fn receiver_bin_path(profile: &Profile) -> Utf8PathBuf {
    let mut bin = Utf8PathBuf::from("target");
    if let Some(t) = &profile.target {
        bin.push(t);
    }
    bin.push(if profile.debug { "debug" } else { "release" });
    bin.push(if target_os(profile) == "windows" {
        "desktop-receiver.exe"
    } else {
        "desktop-receiver"
    });
    bin
}

/// Common `--features`/`--target`/profile flags shared by every cargo
/// invocation that targets the receiver against the static gstreamer.
fn receiver_cargo_flags(profile: &Profile) -> Vec<String> {
    let mut flags = Vec::new();
    if !profile.debug {
        flags.push("--release".into());
    }
    flags.extend([
        "-p".into(),
        "desktop-receiver".into(),
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

/// Set up the environment cargo needs to compile against the static gstreamer
/// (PKG_CONFIG_PATH to the meson-uninstalled .pc + the sysprof stub, and the
/// `SYSTEM_DEPS_*_LINK=static` overrides), then run `f` with it in scope. The
/// same env is used by build/run/check/clippy so their build-script
/// fingerprints match and cargo's cache is shared between them.
fn with_receiver_env<T>(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    // Windows: build the receiver's C deps with clang-cl inside the MSVC dev
    // environment. libplacebo-sys runs its own meson build, and libplacebo
    // passes gcc-style flags (-Wundef, -fno-math-errno, …) that `cl` rejects but
    // clang-cl (the MSVC-compatible clang driver) accepts. GStreamer itself is
    // built separately with `cl` (its wrap ecosystem keys off the `msvc`
    // compiler id); both emit MSVC-ABI archives that link together. Needs the
    // vcvars env (INCLUDE/LIB, headers/libs) plus clang-cl on PATH. Applied here
    // (not just in build_receiver) so check/clippy/run share the same env.
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

    // nixpkgs glib workaround, LINK PHASE ONLY: glib-2.0.pc lists
    // `Requires.private: sysprof-capture-4`, but nixpkgs ships neither its .pc
    // nor a library (the code is compiled into libglib). `pkg-config --static`
    // recurses Requires.private, so resolving gstreamer-full fails without it.
    // An empty stub satisfies the resolver with zero link impact. It MUST NOT be
    // visible during the meson build: subprojects (libsoup, glib) treat
    // sysprof-capture-4 as a real optional feature and would try to compile
    // against its (nonexistent) headers. Hence it is created here, scoped to the
    // cargo link, and not in build_gstreamer.
    let pkgcfg = pkg_config_prog(sh);
    if cmd!(sh, "{pkgcfg} --exists sysprof-capture-4").quiet().run().is_err() {
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

    // Tell system-deps to link the gstreamer libs statically.
    let mut guards = Vec::new();
    for dep in SYSTEM_DEPS {
        guards.push(sh.push_env(format!("SYSTEM_DEPS_{dep}_LINK"), "static"));
    }
    if profile.scope == StaticScope::Full {
        for dep in SYSTEM_DEPS_FULL_SCOPE {
            guards.push(sh.push_env(format!("SYSTEM_DEPS_{dep}_LINK"), "static"));
        }
        // dav1d (AV1) is the Rust gst-plugin-dav1d → dav1d-sys, which resolves
        // dav1d via pkg-config and otherwise picks up the platform's DYNAMIC
        // libdav1d (an @rpath dylib on macOS, a dav1d.dll import lib on Windows)
        // — a runtime dep the static build must not have. Pin it to the static
        // libdav1d.a we already built (FFmpeg's libdav1d wrap): NO_PKG_CONFIG
        // bypasses resolution entirely (dav1d-sys ships pregenerated bindings so
        // it needs no headers, and its `dav1d >= 1.5.0` check would otherwise
        // reject our 1.4.1 — on Windows the gstreamer dav1d.pc on
        // PKG_CONFIG_PATH also shadows any from-source build). These fresh env
        // vars also change dav1d-sys's build-script fingerprint, forcing a re-run.
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

        // cargo rustc scopes the extra link args to the FINAL binary only
        // (RUSTFLAGS would apply them to every crate incl. build scripts /
        // proc-macros).
        let mut cargo: Vec<String> = vec!["rustc".into()];
        cargo.extend(receiver_cargo_flags(profile));
        cargo.push("--".into());

        // Args for the final crate's rustc (after `--`). LTO: cross uses
        // clang/lld (drives the LLVM LTO plugin); rust-only keeps the
        // workspace's fat LTO + default/wild linker.
        let mut rustc_args: Vec<String> = Vec::new();
        if profile.lto == Lto::Cross {
            rustc_args.push("-Clinker-plugin-lto".into());
            rustc_args.push("-Clinker=clang".into());
            rustc_args.push("-Clink-arg=-fuse-ld=lld".into());
        }
        for a in &link_args {
            rustc_args.push(format!("-Clink-arg={a}"));
        }

        // Windows caps a command line near 32 KiB; the static link line (dozens
        // of absolute archive paths) blows past it. Hand the rustc args off via
        // a `@argfile` (rustc reads one arg per line) so the `cargo` command
        // line stays short; rustc in turn response-files the linker itself.
        if target_os(profile) == "windows" {
            let argfile = build.build_dir.join("xtask-rustc-args.txt");
            std::fs::write(&argfile, rustc_args.join("\n")).context("writing rustc argfile")?;
            cargo.push(format!("@{argfile}"));
        } else {
            cargo.extend(rustc_args);
        }

        // The link line carries ~100 `-Clink-arg=<abspath>.a` tokens; echoing it
        // on every build is pure noise. Print the meaningful cargo flags (up to
        // `--`) with the rest summarised, and silence xshell's own command echo
        // with `.quiet()` (this only hides the echoed command, not cargo output).
        let hidden = cargo.iter().position(|a| a == "--").map_or(0, |i| cargo.len() - i - 1);
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

/// Run `cargo <subcmd>` (e.g. check/clippy) for the receiver against the static
/// gstreamer. No link args: these subcommands don't produce the final binary,
/// so only the compile-time env (headers/version via pkg-config) is needed.
fn receiver_cargo(
    sh: &Rc<Shell>,
    build: &GstBuild,
    profile: &Profile,
    subcmd: &str,
) -> Result<()> {
    with_receiver_env(sh, build, profile, || {
        let mut cargo: Vec<String> = vec![subcmd.to_owned()];
        cargo.extend(receiver_cargo_flags(profile));
        println!(">> cargo {subcmd} (static gstreamer) …");
        cmd!(sh, "cargo {cargo...}").run()?;
        Ok(())
    })
}

/// Compute the gstreamer-full static link line, rewriting every `-lX` whose
/// archive we BUILT in-tree (gst plugins/libs, and in scope=Full also
/// glib/pango/ffmpeg/…) to the `.a`'s absolute path, so the linker can't fall
/// back to a same-named dynamic library elsewhere on the system (that's how we
/// once got a mixed static/dynamic-gstreamer binary). Libraries we did NOT
/// build keep their `-l` and stay dynamic. Also appends the internal helper
/// libraries gstreamer-full's pkg-config omits.
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
        // `-pthread` is a *compile*-time flag that pkg-config repeats for every
        // static lib in the closure. At link it's a no-op (pthread lives in libc
        // on modern glibc, and rustc's std links it anyway), so clang emits a
        // "-pthread unused during compilation" warning for each copy. Drop them.
        if tok == "-pthread" {
            continue;
        }
        if let Some(name) = tok.strip_prefix("-l") {
            // macOS' C++ runtime is libc++, not libstdc++. A C++ dep whose .pc
            // was generated against libstdc++ (or picked up from a non-clang
            // toolchain on the host) drags in `-lstdc++`, which doesn't exist on
            // macOS — `ld: library 'stdc++' not found`. Rewrite to the platform
            // runtime; the two are ABI-compatible for the plain link we need.
            if macos && name == "stdc++" {
                out.push("-lc++".to_string());
                continue;
            }
            // meson names static libs `lib<name>.a` (Unix) but also `<name>.a`
            // on MSVC (e.g. `ogg.a`), so a plain `lib`-prefix lookup misses those
            // and leaves a bare `-l<name>` that link.exe silently ignores. Try
            // both forms.
            let candidates = [format!("lib{name}.a"), format!("{name}.a")];
            match candidates.iter().find_map(|f| archives.get(f)) {
                Some(path) => out.push(path.to_string()),
                None => out.push(tok.to_string()), // non-built -l stays dynamic
            }
        } else {
            out.push(tok.to_string());
        }
    }

    // gstreamer-full's pkg-config doesn't pull the internal helper libs (riff,
    // fft, gl, adaptivedemux, codecparsers, …) many plugins reference. Add every
    // built libgst*-1.0.a; --gc-sections drops the unreferenced ones.
    for (name, path) in &archives {
        if name.ends_with("-1.0.a") {
            out.push(path.to_string());
        }
    }

    // Windows: gstreamer's default-enabled dshow / mediafoundation / winks / dmo
    // plugins reference COM GUID constants (IID_*, CLSID_*, DMOCATEGORY_*, KS*)
    // that live in Windows SDK GUID libs. gstreamer-full's pkg-config doesn't
    // propagate them, so name them explicitly — link.exe resolves them from LIB
    // (set by the vcvars import).
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

/// Map every `.a` basename -> absolute path, across the build tree. Indexes all
/// archives (not just `lib*.a`) because meson drops the `lib` prefix for some
/// libs on MSVC (`ogg.a`), and the `-l` rewrite needs to find those too.
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

/// Canonicalize, stripping Windows' `\\?\` extended-length ("verbatim")
/// prefix. Rust's `canonicalize` returns verbatim paths (`\\?\C:\…`), but
/// meson (Python) mishandles them: it joins the build dir with a forward-slash
/// relative path and `open()` rejects the resulting mixed
/// `\\?\C:\…\meson-private/meson.lock` with EINVAL. A plain `C:\…` path avoids
/// this. On non-Windows this is just `canonicalize_utf8`.
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

/// Prepend `dir` to a PATH-style variable using the OS's separator (`;` on
/// Windows, where `:` would collide with drive letters; `:` elsewhere).
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

/// Locate a standalone LLVM `bin` dir (the Windows installer doesn't add it to
/// PATH). None on other platforms / when not installed.
fn find_llvm_bin() -> Option<Utf8PathBuf> {
    ["C:/Program Files/LLVM/bin", "C:/Program Files (x86)/LLVM/bin"]
        .into_iter()
        .map(Utf8PathBuf::from)
        .find(|p| p.join("clang.exe").exists())
}

/// Import the x64 MSVC developer environment by running `vcvars64.bat` and
/// capturing the environment it sets. Returns the full `NAME=VALUE` set (PATH
/// included), which the caller applies before building. Errors if Visual Studio
/// / the Windows SDK isn't installed — a native Windows build can't proceed
/// without them.
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
    // Run vcvars and dump the resulting environment (`set` prints one NAME=VALUE
    // per line). This goes through a wrapper .bat rather than an inline
    // `cmd /c "call \"…\" && set"`: cmd.exe doesn't understand the
    // backslash-escaped quotes the inline arg picks up, so it mangles the nested
    // quoting. A wrapper file needs no inline quotes. vcvars also wants a
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
