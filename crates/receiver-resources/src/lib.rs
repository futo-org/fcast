#[cfg(any(target_os = "macos", target_os = "windows"))]
pub static GST_PLUGIN_LIBS_COMMON: &[&str] = &[
    "gstrtsp",
    "gstisobmff",
    "gstsoup",
    "gstadaptivedemux2",
    "gstdvdsub",
    "gstdvdspu",
    "gstsubparse",
    "gstassrender",
    "gstcoreelements",
    "gstnice",
    "gstapp",
    "gstaudioconvert",
    "gstaudioresample",
    "gstgio",
    "gstogg",
    "gstopengl",
    "gstopus",
    "gstplayback",
    "gsttheora",
    "gsttypefindfunctions",
    "gstvideoconvertscale",
    "gstvolume",
    "gstvorbis",
    "gstaudiofx",
    "gstaudioparsers",
    "gstautodetect",
    "gstdeinterlace",
    "gstid3demux",
    "gstinterleave",
    "gstisomp4",
    "gstmatroska",
    "gstrtp",
    "gstrtpmanager",
    "gstvideofilter",
    "gstvpx",
    "gstwavparse",
    "gstaudiobuffersplit",
    "gstdtls",
    "gstid3tag",
    "gstproxy",
    "gstvideoparsersbad",
    "gstwebrtc",
    "gstlibav",
    "gstflac",
    "gstsrtp",
    "gstmpegtsdemux",
];

#[cfg(target_os = "macos")]
pub static GST_PLUGIN_LIBS_MACOS: &[&str] = &["gstapplemedia", "gstosxaudio", "gstosxvideo"];

#[cfg(target_os = "windows")]
pub static GST_PLUGIN_LIBS_WIN: &[&str] = &["gstwasapi"];

#[cfg(target_os = "windows")]
pub static GST_BASE_LIBS: &[&str] = &[
    "gstbase",
    "gstnet",
    "gstreamer",
    "gstapp",
    "gstpbutils",
    "gstrtp",
    "gstrtsp",
    "gstsctp",
    "gstsdp",
    "gstvideo",
    "gstwebrtc",
    "gstwebrtcnice",
    "gstd3d11",
    "gstd3d12",
    "gstd3dshader",
    "gstaudio",
    "gsttag",
    "gstdxva",
    "gstcodecs",
    "gstcodecparsers",
];

#[cfg(target_os = "windows")]
pub static GST_WIN_DEPENDENCY_LIBS: &[&str] = &[
    "bz2.dll",
    "ffi-7.dll",
    "gio-2.0-0.dll",
    "glib-2.0-0.dll",
    "gmodule-2.0-0.dll",
    "gobject-2.0-0.dll",
    "intl-8.dll",
    "libcrypto-3-x64.dll",
    "libssl-3-x64.dll",
    "libwinpthread-1.dll",
    "nice-10.dll",
    "orc-0.4-0.dll",
    "pcre2-8-0.dll",
    "z-1.dll",
    "srtp2-1.dll",
    "pango-1.0-0.dll",
    "dav1d.dll",
    "gstgl-1.0-0.dll",
    "harfbuzz.dll",
    "fribidi-0.dll",
    "freetype-6.dll",
    "png16.dll",
    "xml2-16.dll",
    "soup-3.0-0.dll",
    "sqlite3-0.dll",
    "psl-5.dll",
    "nghttp2.dll",
    "ass-9.dll",
    "fontconfig-1.dll",
    "gstriff-1.0-0.dll",
    "vorbis-0.dll",
    "vorbisenc-2.dll",
    "gstfft-1.0-0.dll",
    "FLAC-8.dll",
    "avcodec-61.dll",
    "avfilter-10.dll",
    "avformat-61.dll",
    "avutil-59.dll",
    "gstmpegts-1.0-0.dll",
    "ogg-0.dll",
    "gstcontroller-1.0-0.dll",
    "graphene-1.0-0.dll",
    "jpeg8.dll",
    "opus-0.dll",
    "theoradec-1.dll",
    "theoraenc-1.dll",
    "libiconv-2.dll",
    "libexpat.dll",
    "swresample-5.dll",
    "swscale-8.dll",
];

#[cfg(target_os = "windows")]
pub fn all_plugins_for_win() -> Vec<String> {
    GST_PLUGIN_LIBS_COMMON
        .iter()
        .chain(GST_PLUGIN_LIBS_WIN.iter())
        .map(|s| format!("{s}.dll"))
        .collect()
}

#[cfg(target_os = "macos")]
pub fn all_plugins_for_macos() -> Vec<String> {
    GST_PLUGIN_LIBS_COMMON
        .iter()
        .chain(GST_PLUGIN_LIBS_MACOS.iter())
        .map(|s| format!("lib{s}.dylib"))
        .collect()
}
