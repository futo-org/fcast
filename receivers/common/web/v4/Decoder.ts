import { CheckedByteBuffer, DecodeError } from './CheckedByteBuffer';
import { Packet } from './generated/fcast/v4/packet';
import { Message } from './generated/fcast/v4/message';
import { Load } from './generated/fcast/v4/load';
import { MediaSource } from './generated/fcast/v4/media-source';
import { Queue } from './generated/fcast/v4/queue';
import { QueueItem } from './generated/fcast/v4/queue-item';
import { QueuePosition } from './generated/fcast/v4/queue-position';
import { QueueIndex } from './generated/fcast/v4/queue-index';
import { QueueMarkerFront } from './generated/fcast/v4/queue-marker-front';
import { QueueMarkerBack } from './generated/fcast/v4/queue-marker-back';
import { MediaItem } from './generated/fcast/v4/media-item';
import { Metadata } from './generated/fcast/v4/metadata';
import { VideoMetadata } from './generated/fcast/v4/video-metadata';
import { AudioMetadata } from './generated/fcast/v4/audio-metadata';
import { Chapter } from './generated/fcast/v4/chapter';
import { RequestHeader } from './generated/fcast/v4/request-header';
import { MetadataKV } from './generated/fcast/v4/metadata-kv';
import { GenericMetaValue } from './generated/fcast/v4/generic-meta-value';
import { GenericMetaString } from './generated/fcast/v4/generic-meta-string';
import { GenericMetaFloat } from './generated/fcast/v4/generic-meta-float';
import { GenericMetaInt } from './generated/fcast/v4/generic-meta-int';
import { GenericMetaList } from './generated/fcast/v4/generic-meta-list';
import { WrappedGenericMetaValue } from './generated/fcast/v4/wrapped-generic-meta-value';
import { ProgressChanged } from './generated/fcast/v4/progress-changed';
import { VolumeChanged } from './generated/fcast/v4/volume-changed';
import { PlaybackStateChanged } from './generated/fcast/v4/playback-state-changed';
import { PlaybackState } from './generated/fcast/v4/playback-state';
import { SpeedChanged } from './generated/fcast/v4/speed-changed';
import { SenderIntroduction } from './generated/fcast/v4/sender-introduction';
import { DeviceInfo } from './generated/fcast/v4/device-info';
import { StopPlayback } from './generated/fcast/v4/stop-playback';
import { StartMirroringSession } from './generated/fcast/v4/start-mirroring-session';
import { MirroringSessionDescription } from './generated/fcast/v4/mirroring-session-description';
import { QueueInsert } from './generated/fcast/v4/queue-insert';
import { QueueRemove } from './generated/fcast/v4/queue-remove';
import { QueueItemSelected } from './generated/fcast/v4/queue-item-selected';
import { ChangeTrack } from './generated/fcast/v4/change-track';
import { MediaTrackType } from './generated/fcast/v4/media-track-type';
import { AddSubtitleSource } from './generated/fcast/v4/add-subtitle-source';
import { SubtitleTrackMeta } from './generated/fcast/v4/subtitle-track-meta';
import { SetProgressUpdateInterval } from './generated/fcast/v4/set-progress-update-interval';
import { CompanionHelloRequest } from './generated/fcast/v4/companion-hello-request';
import { CompanionResourceInfoResponse } from './generated/fcast/v4/companion-resource-info-response';
import { CompanionResourceSize } from './generated/fcast/v4/companion-resource-size';
import { KnownResourceSize } from './generated/fcast/v4/known-resource-size';
import { UnknownResourceSize } from './generated/fcast/v4/unknown-resource-size';

export interface DecodeLimits {
    maxDepth: number;
    maxNodes: number;
    stringSlack: number;
}

export const DEFAULT_LIMITS: DecodeLimits = {
    maxDepth: 16,
    maxNodes: 16384,
    stringSlack: 65536,
};

export interface DeviceInfoData {
    displayName: string | null;
    appName: string | null;
    appVersion: string | null;
}

export interface RequestHeaderData {
    key: string;
    value: string;
}

export interface ChapterData {
    start: number | null;
    length: number | null;
    title: string;
    skipLabel: string | null;
}

export type MediaMetadataData =
    | { kind: 'video'; chapters: ChapterData[] }
    | { kind: 'audio'; artist: string | null; album: string | null; chapters: ChapterData[] };

export type GenericMeta =
    | { kind: 'string'; value: string | null }
    | { kind: 'float'; value: number }
    | { kind: 'int'; value: number }
    | { kind: 'list'; value: (GenericMeta | null)[] }
    | { kind: 'kv'; key: string; value: GenericMeta | null };

export interface MetadataKVData {
    key: string;
    value: GenericMeta | null;
}

export interface MediaItemData {
    container: string;
    sourceUrl: string;
    startTime: number | null;
    volume: number | null;
    speed: number | null;
    headers: RequestHeaderData[];
    title: string | null;
    thumbnailUrl: string | null;
    metadata: MediaMetadataData | null;
    extraMetadata: MetadataKVData[];
}

export interface QueueItemData {
    mediaItem: MediaItemData;
    playbackDuration: number | null;
}

export type MediaSourceData =
    | { kind: 'single'; item: MediaItemData }
    | { kind: 'queue'; items: QueueItemData[]; startIndex: number | null; autoplay: boolean };

export type QueuePositionData = { kind: 'index'; index: number } | { kind: 'front' } | { kind: 'back' };

export type ResourceSizeData = { kind: 'known'; size: number } | { kind: 'unknown' };

export type SenderMessage =
    | { type: 'ProgressChanged'; position: number | null; duration: number | null }
    | { type: 'VolumeChanged'; volume: number }
    | { type: 'PlaybackStateChanged'; state: PlaybackState }
    | { type: 'SpeedChanged'; speed: number }
    | { type: 'SenderIntroduction'; deviceInfo: DeviceInfoData }
    | { type: 'Load'; source: MediaSourceData }
    | { type: 'StopPlayback' }
    | { type: 'StartMirroringSession'; sessionId: number }
    | { type: 'MirroringSessionDescription'; sessionId: number; sdp: string }
    | { type: 'QueueInsert'; item: QueueItemData; position: QueuePositionData }
    | { type: 'QueueRemove'; position: QueuePositionData }
    | { type: 'ChangeTrack'; id: number | null; trackType: MediaTrackType }
    | { type: 'QueueItemSelected'; position: QueuePositionData }
    | { type: 'AddSubtitleSource'; url: string; select: boolean; name: string | null; metadata: {} | null }
    | { type: 'SetProgressUpdateInterval'; interval: number | null }
    | { type: 'CompanionHelloRequest' }
    | { type: 'CompanionResourceInfoResponse'; requestId: number; contentType: string; resourceSize: ResourceSizeData | null }
    | { type: 'WrongDirection'; payload: string };

// Counts every table visited and enforces the nesting and node limits.
class Walk {
    private nodes = 0;

    constructor(private readonly limits: DecodeLimits) {}

    table(depth: number): void {
        if (depth > this.limits.maxDepth) {
            throw new DecodeError('depth limit exceeded');
        }
        if (++this.nodes > this.limits.maxNodes) {
            throw new DecodeError('node budget exceeded');
        }
    }
}

function required<T>(value: T | null | undefined, name: string): T {
    if (value === null || value === undefined) {
        throw new DecodeError('missing required ' + name);
    }
    return value;
}

function finite(value: number): number {
    if (!Number.isFinite(value)) {
        throw new DecodeError('non-finite number');
    }
    return value;
}

function optionalFloat(value: number | null): number | null {
    return value === null ? null : finite(value);
}

function toSafeNumber(value: bigint, what: string): number {
    const n = Number(value);
    if (!Number.isSafeInteger(n)) {
        throw new DecodeError(what + ' out of range');
    }
    return n;
}

function optionalTime(time: { micros(): bigint } | null): number | null {
    return time ? toSafeNumber(time.micros(), 'time') : null;
}

function enumValue(enumObject: any, value: number, what: string): number {
    if (typeof enumObject[value] !== 'string') {
        throw new DecodeError('invalid ' + what + ' ' + value);
    }
    return value;
}

function payload(walk: Walk, packet: Packet, table: any): any {
    walk.table(1);
    return required(packet.payload(table), 'payload');
}

function decodeDeviceInfo(walk: Walk, depth: number, info: DeviceInfo): DeviceInfoData {
    walk.table(depth);
    return {
        displayName: info.displayName(),
        appName: info.appName(),
        appVersion: info.appVersion(),
    };
}

function decodeChapter(walk: Walk, depth: number, chapter: Chapter): ChapterData {
    walk.table(depth);
    return {
        start: optionalTime(chapter.start()),
        length: optionalTime(chapter.length()),
        title: required(chapter.title(), 'chapter title'),
        skipLabel: chapter.skipLabel(),
    };
}

function decodeRequestHeader(walk: Walk, depth: number, header: RequestHeader): RequestHeaderData {
    walk.table(depth);
    return {
        key: required(header.key(), 'header key'),
        value: required(header.value(), 'header value'),
    };
}

function decodeMetadataKV(walk: Walk, depth: number, kv: MetadataKV): MetadataKVData {
    walk.table(depth);
    return {
        key: required(kv.key(), 'metadata key'),
        value: decodeGeneric(walk, depth + 1, kv.valueType(), (obj) => kv.value(obj)),
    };
}

function decodeGeneric(walk: Walk, depth: number, type: GenericMetaValue, get: (obj: any) => any): GenericMeta | null {
    if (type === GenericMetaValue.NONE) {
        return null;
    }
    walk.table(depth);
    switch (type) {
        case GenericMetaValue.String: {
            const s = required(get(new GenericMetaString()), 'string meta') as GenericMetaString;
            return { kind: 'string', value: s.value() };
        }
        case GenericMetaValue.Float: {
            const f = required(get(new GenericMetaFloat()), 'float meta') as GenericMetaFloat;
            return { kind: 'float', value: finite(f.value()) };
        }
        case GenericMetaValue.Int: {
            const n = required(get(new GenericMetaInt()), 'int meta') as GenericMetaInt;
            return { kind: 'int', value: toSafeNumber(n.value(), 'int meta') };
        }
        case GenericMetaValue.List: {
            const list = required(get(new GenericMetaList()), 'list meta') as GenericMetaList;
            const count = list.valueLength();
            const values: (GenericMeta | null)[] = [];
            for (let i = 0; i < count; i++) {
                const wrapper = required(list.value(i, new WrappedGenericMetaValue()), 'list item') as WrappedGenericMetaValue;
                walk.table(depth + 1);
                values.push(decodeGeneric(walk, depth + 1, wrapper.valueType(), (obj) => wrapper.value(obj)));
            }
            return { kind: 'list', value: values };
        }
        case GenericMetaValue.KVPair: {
            const kv = required(get(new MetadataKV()), 'kv meta') as MetadataKV;
            return {
                kind: 'kv',
                key: required(kv.key(), 'kv key'),
                value: decodeGeneric(walk, depth + 1, kv.valueType(), (obj) => kv.value(obj)),
            };
        }
        default:
            throw new DecodeError('invalid generic meta type ' + type);
    }
}

function decodeMediaItem(walk: Walk, depth: number, item: MediaItem): MediaItemData {
    walk.table(depth);

    const headers: RequestHeaderData[] = [];
    const headerCount = item.headersLength();
    for (let i = 0; i < headerCount; i++) {
        headers.push(decodeRequestHeader(walk, depth + 1, required(item.headers(i, new RequestHeader()), 'request header')));
    }

    const extraMetadata: MetadataKVData[] = [];
    const extraCount = item.extraMetadataLength();
    for (let i = 0; i < extraCount; i++) {
        extraMetadata.push(decodeMetadataKV(walk, depth + 1, required(item.extraMetadata(i, new MetadataKV()), 'metadata kv')));
    }

    return {
        container: required(item.container(), 'container'),
        sourceUrl: required(item.sourceUrl(), 'source url'),
        startTime: optionalTime(item.startTime()),
        volume: optionalFloat(item.volume()),
        speed: optionalFloat(item.speed()),
        headers,
        title: item.title(),
        thumbnailUrl: item.thumbnailUrl(),
        metadata: decodeMetadata(walk, depth + 1, item),
        extraMetadata,
    };
}

function decodeChapters(walk: Walk, depth: number, count: number, get: (index: number) => Chapter | null): ChapterData[] {
    const chapters: ChapterData[] = [];
    for (let i = 0; i < count; i++) {
        chapters.push(decodeChapter(walk, depth, required(get(i), 'chapter')));
    }
    return chapters;
}

function decodeMetadata(walk: Walk, depth: number, item: MediaItem): MediaMetadataData | null {
    switch (item.metadataType()) {
        case Metadata.NONE:
            return null;
        case Metadata.Video: {
            const video = required(item.metadata(new VideoMetadata()), 'video metadata') as VideoMetadata;
            walk.table(depth);
            return {
                kind: 'video',
                chapters: decodeChapters(walk, depth + 1, video.chaptersLength(), (i) => video.chapters(i, new Chapter())),
            };
        }
        case Metadata.Audio: {
            const audio = required(item.metadata(new AudioMetadata()), 'audio metadata') as AudioMetadata;
            walk.table(depth);
            return {
                kind: 'audio',
                artist: audio.artist(),
                album: audio.album(),
                chapters: decodeChapters(walk, depth + 1, audio.chaptersLength(), (i) => audio.chapters(i, new Chapter())),
            };
        }
        default:
            throw new DecodeError('invalid metadata type ' + item.metadataType());
    }
}

function decodeQueueItem(walk: Walk, depth: number, queueItem: QueueItem): QueueItemData {
    walk.table(depth);
    return {
        mediaItem: decodeMediaItem(walk, depth + 1, required(queueItem.mediaItem(new MediaItem()), 'media item')),
        playbackDuration: optionalTime(queueItem.playbackDuration()),
    };
}

function decodeQueue(walk: Walk, depth: number, queue: Queue): { kind: 'queue'; items: QueueItemData[]; startIndex: number | null; autoplay: boolean } {
    walk.table(depth);
    // `items` is the first field of Queue, so its vtable slot is 4.
    if (queue.bb!.__offset(queue.bb_pos, 4) === 0) {
        throw new DecodeError('missing required queue items');
    }
    const items: QueueItemData[] = [];
    const count = queue.itemsLength();
    for (let i = 0; i < count; i++) {
        items.push(decodeQueueItem(walk, depth + 1, required(queue.items(i, new QueueItem()), 'queue item')));
    }
    return {
        kind: 'queue',
        items,
        startIndex: queue.startIndex(),
        autoplay: queue.autoplay(),
    };
}

function decodeMediaSource(walk: Walk, depth: number, load: Load): MediaSourceData {
    switch (load.sourceType()) {
        case MediaSource.Single:
            return {
                kind: 'single',
                item: decodeMediaItem(walk, depth, required(load.source(new MediaItem()), 'media item')),
            };
        case MediaSource.Queue:
            return decodeQueue(walk, depth, required(load.source(new Queue()), 'queue'));
        default:
            throw new DecodeError('missing or invalid media source');
    }
}

function decodeQueuePosition(walk: Walk, depth: number, type: QueuePosition, get: (obj: any) => any): QueuePositionData {
    switch (type) {
        case QueuePosition.Index: {
            walk.table(depth);
            const index = required(get(new QueueIndex()), 'queue index') as QueueIndex;
            return { kind: 'index', index: index.index() };
        }
        case QueuePosition.Front:
            walk.table(depth);
            required(get(new QueueMarkerFront()), 'queue front marker');
            return { kind: 'front' };
        case QueuePosition.Back:
            walk.table(depth);
            required(get(new QueueMarkerBack()), 'queue back marker');
            return { kind: 'back' };
        default:
            throw new DecodeError('invalid queue position ' + type);
    }
}

function decodeResourceSize(walk: Walk, depth: number, info: CompanionResourceInfoResponse): ResourceSizeData | null {
    switch (info.resourceSizeType()) {
        case CompanionResourceSize.NONE:
            return null;
        case CompanionResourceSize.Unknown:
            walk.table(depth);
            required(info.resourceSize(new UnknownResourceSize()), 'unknown resource size');
            return { kind: 'unknown' };
        case CompanionResourceSize.Known: {
            walk.table(depth);
            const known = required(info.resourceSize(new KnownResourceSize()), 'known resource size') as KnownResourceSize;
            return { kind: 'known', size: toSafeNumber(known.size(), 'resource size') };
        }
        default:
            throw new DecodeError('invalid resource size type ' + info.resourceSizeType());
    }
}

function decodeSenderMessage(walk: Walk, packet: Packet, type: number): SenderMessage {
    switch (type) {
        case Message.ProgressChanged: {
            const m = payload(walk, packet, new ProgressChanged()) as ProgressChanged;
            return {
                type: 'ProgressChanged',
                position: optionalTime(m.position()),
                duration: optionalTime(m.duration()),
            };
        }
        case Message.VolumeChanged: {
            const m = payload(walk, packet, new VolumeChanged()) as VolumeChanged;
            return { type: 'VolumeChanged', volume: finite(m.volume()) };
        }
        case Message.PlaybackStateChanged: {
            const m = payload(walk, packet, new PlaybackStateChanged()) as PlaybackStateChanged;
            return { type: 'PlaybackStateChanged', state: enumValue(PlaybackState, m.state(), 'playback state') };
        }
        case Message.SpeedChanged: {
            const m = payload(walk, packet, new SpeedChanged()) as SpeedChanged;
            return { type: 'SpeedChanged', speed: finite(m.speed()) };
        }
        case Message.SenderIntroduction: {
            const m = payload(walk, packet, new SenderIntroduction()) as SenderIntroduction;
            return {
                type: 'SenderIntroduction',
                deviceInfo: decodeDeviceInfo(walk, 2, required(m.deviceInfo(), 'device info')),
            };
        }
        case Message.Load: {
            const m = payload(walk, packet, new Load()) as Load;
            return { type: 'Load', source: decodeMediaSource(walk, 2, m) };
        }
        case Message.StopPlayback: {
            payload(walk, packet, new StopPlayback());
            return { type: 'StopPlayback' };
        }
        case Message.StartMirroringSession: {
            const m = payload(walk, packet, new StartMirroringSession()) as StartMirroringSession;
            return { type: 'StartMirroringSession', sessionId: m.sessionId() };
        }
        case Message.MirroringSessionDescription: {
            const m = payload(walk, packet, new MirroringSessionDescription()) as MirroringSessionDescription;
            return {
                type: 'MirroringSessionDescription',
                sessionId: m.sessionId(),
                sdp: required(m.sdp(), 'sdp'),
            };
        }
        case Message.QueueInsert: {
            const m = payload(walk, packet, new QueueInsert()) as QueueInsert;
            return {
                type: 'QueueInsert',
                item: decodeQueueItem(walk, 2, required(m.item(new QueueItem()), 'queue item')),
                position: decodeQueuePosition(walk, 2, m.positionType(), (obj) => m.position(obj)),
            };
        }
        case Message.QueueRemove: {
            const m = payload(walk, packet, new QueueRemove()) as QueueRemove;
            return {
                type: 'QueueRemove',
                position: decodeQueuePosition(walk, 2, m.positionType(), (obj) => m.position(obj)),
            };
        }
        case Message.ChangeTrack: {
            const m = payload(walk, packet, new ChangeTrack()) as ChangeTrack;
            return {
                type: 'ChangeTrack',
                id: m.id(),
                trackType: enumValue(MediaTrackType, m.trackType(), 'track type'),
            };
        }
        case Message.QueueItemSelected: {
            const m = payload(walk, packet, new QueueItemSelected()) as QueueItemSelected;
            return {
                type: 'QueueItemSelected',
                position: decodeQueuePosition(walk, 2, m.positionType(), (obj) => m.position(obj)),
            };
        }
        case Message.AddSubtitleSource: {
            const m = payload(walk, packet, new AddSubtitleSource()) as AddSubtitleSource;
            const metadata = m.metadata(new SubtitleTrackMeta());
            if (metadata) {
                walk.table(2);
            }
            return {
                type: 'AddSubtitleSource',
                url: required(m.url(), 'subtitle url'),
                select: m.select(),
                name: m.name(),
                metadata: metadata ? {} : null,
            };
        }
        case Message.SetProgressUpdateInterval: {
            const m = payload(walk, packet, new SetProgressUpdateInterval()) as SetProgressUpdateInterval;
            return { type: 'SetProgressUpdateInterval', interval: optionalTime(m.interval()) };
        }
        case Message.CompanionHelloRequest: {
            payload(walk, packet, new CompanionHelloRequest());
            return { type: 'CompanionHelloRequest' };
        }
        case Message.CompanionResourceInfoResponse: {
            const m = payload(walk, packet, new CompanionResourceInfoResponse()) as CompanionResourceInfoResponse;
            return {
                type: 'CompanionResourceInfoResponse',
                requestId: m.requestId(),
                contentType: required(m.contentType(), 'content type'),
                resourceSize: decodeResourceSize(walk, 2, m),
            };
        }
        default:
            throw new DecodeError('unsupported payload type ' + type);
    }
}

function decodeBody(bytes: Uint8Array, limits: DecodeLimits): SenderMessage {
    if (bytes.length < 8) {
        throw new DecodeError('packet too short');
    }
    const buffer = new CheckedByteBuffer(bytes);
    buffer.stringBudget = bytes.length + limits.stringSlack;
    const walk = new Walk(limits);
    walk.table(0);

    const packet = Packet.getRootAsPacket(buffer);
    buffer.verifyTable(packet.bb_pos);
    const type: number = packet.payloadType();
    const name: string | undefined = (Message as any)[type];
    if (type === Message.NONE || typeof name !== 'string') {
        throw new DecodeError('invalid payload type ' + type);
    }

    switch (type) {
        case Message.ReceiverIntroduction:
        case Message.TracksAvailable:
        case Message.CompanionHelloResponse:
        case Message.CompanionResourceInfoRequest:
        case Message.CompanionResourceRequest:
        case Message.Error:
            return { type: 'WrongDirection', payload: name };
        default:
            return decodeSenderMessage(walk, packet, type);
    }
}

// Decodes a Packet that a sender sent to the receiver. Any failure is reported as a DecodeError.
export function decodePacket(bytes: Uint8Array, limits: DecodeLimits = DEFAULT_LIMITS): SenderMessage {
    try {
        return decodeBody(bytes, limits);
    } catch (e) {
        if (e instanceof DecodeError) {
            throw e;
        }
        throw new DecodeError('malformed: ' + (e && e.message ? e.message : String(e)));
    }
}
