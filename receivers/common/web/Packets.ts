// Protocol Documentation: https://gitlab.futo.org/videostreaming/fcast/-/wikis/Protocol-version-3
export const PROTOCOL_VERSION = 3;

export enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    Ping = 12,
    Pong = 13,
    Initial = 14,
    PlayUpdate = 15,
    SetPlaylistItem = 16,
    SubscribeEvent = 17,
    UnsubscribeEvent = 18,
    Event = 19,
};

export enum PlaybackState {
    Idle = 0,
    Playing = 1,
    Paused = 2,
}

export enum ContentType {
    Playlist = 0,
}

export enum MetadataType {
    Generic = 0,
}

export enum EventType {
    MediaItemStart = 0,
    MediaItemEnd = 1,
    MediaItemChange = 2,
    KeyDown = 3,
    KeyUp = 4,
}

// Required supported keys for listener events defined below.
// Optionally supported key values list: https://developer.mozilla.org/en-US/docs/Web/API/UI_Events/Keyboard_event_key_values
export enum KeyNames {
    Left = 'ArrowLeft',
    Right = 'ArrowRight',
    Up = 'ArrowUp',
    Down = 'ArrowDown',
    Ok = 'Enter',
}

export interface MetadataObject {
    type: MetadataType;
}

export class GenericMediaMetadata implements MetadataObject {
    readonly type = MetadataType.Generic;

    constructor(
        public title: string = null,
        public thumbnailUrl: string = null,
        public custom: any = null,
    ) {}
}

export class PlayMessage {
    constructor(
        public container: string,           // The MIME type (video/mp4)
        public url: string = null,          // The URL to load (optional)
        public content: string = null,      // The content to load (i.e. a DASH manifest, json content, optional)
        public time: number = null,         // The time to start playing in seconds
        public volume: number = null,       // The desired volume (0-1)
        public speed: number = null,        // The factor to multiply playback speed by (defaults to 1.0)
        public headers: { [key: string]: string } = null,  // HTTP request headers to add to the play request Map<string, string>
        public metadata: MetadataObject = null,
    ) {}
}

export class SeekMessage {
    constructor(
        public time: number,                // The time to seek to in seconds
    ) {}
}

export class PlaybackUpdateMessage {
    constructor(
        public generationTime: number,      // The time the packet was generated (unix time milliseconds)
        public state: number,               // The playback state
        public time: number = null,         // The current time playing in seconds
        public duration: number = null,     // The duration in seconds
        public speed: number = null,        // The playback speed factor
        public itemIndex: number = null,    // The playlist item index currently being played on receiver
    ) {}
}

export class VolumeUpdateMessage {
    constructor(
        public generationTime: number,      // The time the packet was generated (unix time milliseconds)
        public volume: number,              // The current volume (0-1)
    ) {}
}

export class SetVolumeMessage {
    constructor(
        public volume: number,              // The desired volume (0-1)
    ) {}
}

export class PlaybackErrorMessage {
    constructor(
        public message: string
    ) {}
}

export class SetSpeedMessage {
    constructor(
        public speed: number,               // The factor to multiply playback speed by
    ) {}
}

export class VersionMessage {
    constructor(
        public version: number,             // Protocol version number (integer)
    ) {}
}

export interface ContentObject {
    contentType: ContentType;
}

export class MediaItem {
    constructor(
        public container: string,           // The MIME type (video/mp4)
        public url: string = null,          // The URL to load (optional)
        public content: string = null,      // The content to load (i.e. a DASH manifest, json content, optional)
        public time: number = null,         // The time to start playing in seconds
        public volume: number = null,       // The desired volume (0-1)
        public speed: number = null,        // The factor to multiply playback speed by (defaults to 1.0)
        public cache: boolean = null,       // Indicates if the receiver should preload the media item
        public showDuration: number = null, // Indicates how long the item content is presented on screen in seconds
        public headers: { [key: string]: string } = null,  // HTTP request headers to add to the play request Map<string, string>
        public metadata: MetadataObject = null,
    ) {}
}

export class PlaylistContent implements ContentObject {
    readonly contentType = ContentType.Playlist;

    constructor(
        public items: MediaItem[],
        public offset: number = null,         // Start position of the first item to play from the playlist
        public volume: number = null,         // The desired volume (0-1)
        public speed: number = null,          // The factor to multiply playback speed by (defaults to 1.0)
        public forwardCache: number = null,   // Count of media items should be pre-loaded forward from the current view index
        public backwardCache: number = null,  // Count of media items should be pre-loaded backward from the current view index
        public metadata: MetadataObject = null,
    ) {}
}

export class InitialSenderMessage {
    constructor(
        public displayName: string = null,
        public appName: string = null,
        public appVersion: string = null,
    ) {}
}

export class InitialReceiverMessage {
    constructor(
        public displayName: string = null,
        public appName: string = null,
        public appVersion: string = null,
        public playData: PlayMessage = null,
    ) {}
}

export class PlayUpdateMessage {
    constructor(
        public generationTime: number,
        public playData: PlayMessage = null,
    ) {}
}

export class SetPlaylistItemMessage {
    constructor(
        public itemIndex: number,          // The playlist item index to play on receiver
    ) {}
}

export interface EventSubscribeObject {
    type: EventType;
}

export interface EventObject {
    type: EventType;
}

export class MediaItemStartEvent implements EventSubscribeObject {
    readonly type = EventType.MediaItemStart;

    constructor() {}
}

export class MediaItemEndEvent implements EventSubscribeObject {
    readonly type = EventType.MediaItemEnd;

    constructor() {}
}

export class MediaItemChangeEvent implements EventSubscribeObject {
    readonly type = EventType.MediaItemChange;

    constructor() {}
}

export class KeyDownEvent implements EventSubscribeObject {
    readonly type = EventType.KeyDown;

    constructor(
        public keys: string[],
    ) {}
}

export class KeyUpEvent implements EventSubscribeObject {
    readonly type = EventType.KeyUp;

    constructor(
        public keys: string[],
    ) {}
}

export class SubscribeEventMessage {
    constructor(
        public event: EventSubscribeObject,
    ) {}
}

export class UnsubscribeEventMessage {
    constructor(
        public event: EventSubscribeObject,
    ) {}
}

export class MediaItemEvent implements EventObject {
    constructor(
        public type: EventType,
        public item: MediaItem,
    ) {}
}

export class KeyEvent implements EventObject {
    constructor(
        public type: EventType,
        public key: string,
        public repeat: boolean,
        public handled: boolean,
    ) {}
}

export class EventMessage {
    constructor(
        public generationTime: number,
        public event: EventObject,
    ) {}
}
