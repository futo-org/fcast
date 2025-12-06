package com.futo.fcast.receiver.models

val streamingMediaTypes = arrayListOf(
    "application/vnd.apple.mpegurl",
    "application/x-mpegURL",
    "application/dash+xml",
)

val supportedVideoTypes = arrayListOf(
    "video/mp4",
    "video/mpeg",
    "video/ogg",
    "video/webm",
    "video/x-matroska",
    "video/3gpp",
    "video/3gpp2",
)

val supportedVideoExtensions = arrayListOf(
    ".mp4", ".m4v",
    ".webm",
    ".mkv",
    ".3gp",
    ".3g2",
)

val supportedAudioTypes = arrayListOf(
    "audio/aac",
    "audio/flac",
    "audio/x-flac",
    "audio/mpeg",
    "audio/mp4",
    "audio/ogg",
    "audio/wav",
    "audio/webm",
    "audio/3gpp",
    "audio/3gpp2",
)

val supportedImageTypes = arrayListOf(
    "image/apng",
    "image/avif",
    "image/bmp",
    "image/gif",
    "image/x-icon",
    "image/jpeg",
    "image/png",
    "image/svg+xml",
    "image/vnd.microsoft.icon",
    "image/webp",
)

val supportedImageExtensions = arrayListOf(
    ".apng",
    ".avif",
    ".bmp",
    ".gif",
    ".ico",
    ".jpeg", ".jpg", ".jpe", ".jif", ".jfif", ".jfi",
    ".png",
    ".svg",
    ".webp",
)

val supportedPlayerTypes = streamingMediaTypes + supportedVideoTypes + supportedAudioTypes
