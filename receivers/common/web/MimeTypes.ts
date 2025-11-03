
export const streamingMediaTypes = [
    'application/vnd.apple.mpegurl',
    'application/x-mpegURL',
    'application/dash+xml',
    'application/x-whep'
];

export const supportedVideoTypes = [
    'video/mp4',
    'video/mpeg',
    'video/ogg',
    'video/webm',
    'video/x-matroska',
    'video/3gpp',
    'video/3gpp2',
];

export const supportedVideoExtensions = [
    '.mp4', '.m4v',
    '.webm',
    '.mkv',
    '.3gp',
    '.3g2',
];

export const supportedAudioTypes = [
    'audio/aac',
    'audio/flac',
    'audio/mpeg',
    'audio/mp4',
    'audio/ogg',
    'audio/wav',
    'audio/webm',
    'audio/3gpp',
    'audio/3gpp2',
];

export const supportedImageTypes = [
    'image/apng',
    'image/avif',
    'image/bmp',
    'image/gif',
    'image/x-icon',
    'image/jpeg',
    'image/png',
    'image/svg+xml',
    'image/vnd.microsoft.icon',
    'image/webp',
];

export const supportedImageExtensions = [
    '.apng',
    '.avif',
    '.bmp',
    '.gif',
    '.ico',
    '.jpeg', '.jpg', '.jpe', '.jif', '.jfif', '.jfi',
    '.png',
    '.svg',
    '.webp',
];

export const supportedPlayerTypes = streamingMediaTypes.concat(
    supportedVideoTypes,
    supportedAudioTypes,
);
