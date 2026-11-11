import * as flatbuffers from 'flatbuffers';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { Load } from 'common/v4/generated/fcast/v4/load';
import { MediaSource } from 'common/v4/generated/fcast/v4/media-source';
import { Queue } from 'common/v4/generated/fcast/v4/queue';
import { QueueItem } from 'common/v4/generated/fcast/v4/queue-item';
import { QueuePosition } from 'common/v4/generated/fcast/v4/queue-position';
import { QueueIndex } from 'common/v4/generated/fcast/v4/queue-index';
import { QueueMarkerFront } from 'common/v4/generated/fcast/v4/queue-marker-front';
import { QueueMarkerBack } from 'common/v4/generated/fcast/v4/queue-marker-back';
import { MediaItem } from 'common/v4/generated/fcast/v4/media-item';
import { Metadata } from 'common/v4/generated/fcast/v4/metadata';
import { VideoMetadata } from 'common/v4/generated/fcast/v4/video-metadata';
import { AudioMetadata } from 'common/v4/generated/fcast/v4/audio-metadata';
import { Chapter } from 'common/v4/generated/fcast/v4/chapter';
import { RequestHeader } from 'common/v4/generated/fcast/v4/request-header';
import { MetadataKV } from 'common/v4/generated/fcast/v4/metadata-kv';
import { GenericMetaValue } from 'common/v4/generated/fcast/v4/generic-meta-value';
import { GenericMetaString } from 'common/v4/generated/fcast/v4/generic-meta-string';
import { GenericMetaFloat } from 'common/v4/generated/fcast/v4/generic-meta-float';
import { GenericMetaInt } from 'common/v4/generated/fcast/v4/generic-meta-int';
import { GenericMetaList } from 'common/v4/generated/fcast/v4/generic-meta-list';
import { WrappedGenericMetaValue } from 'common/v4/generated/fcast/v4/wrapped-generic-meta-value';
import { ProgressChanged } from 'common/v4/generated/fcast/v4/progress-changed';
import { VolumeChanged } from 'common/v4/generated/fcast/v4/volume-changed';
import { PlaybackStateChanged } from 'common/v4/generated/fcast/v4/playback-state-changed';
import { PlaybackState } from 'common/v4/generated/fcast/v4/playback-state';
import { SpeedChanged } from 'common/v4/generated/fcast/v4/speed-changed';
import { SenderIntroduction } from 'common/v4/generated/fcast/v4/sender-introduction';
import { DeviceInfo } from 'common/v4/generated/fcast/v4/device-info';
import { StopPlayback } from 'common/v4/generated/fcast/v4/stop-playback';
import { StartMirroringSession } from 'common/v4/generated/fcast/v4/start-mirroring-session';
import { MirroringSessionDescription } from 'common/v4/generated/fcast/v4/mirroring-session-description';
import { QueueInsert } from 'common/v4/generated/fcast/v4/queue-insert';
import { QueueRemove } from 'common/v4/generated/fcast/v4/queue-remove';
import { QueueItemSelected } from 'common/v4/generated/fcast/v4/queue-item-selected';
import { ChangeTrack } from 'common/v4/generated/fcast/v4/change-track';
import { MediaTrackType } from 'common/v4/generated/fcast/v4/media-track-type';
import { AddSubtitleSource } from 'common/v4/generated/fcast/v4/add-subtitle-source';
import { SubtitleTrackMeta } from 'common/v4/generated/fcast/v4/subtitle-track-meta';
import { SetProgressUpdateInterval } from 'common/v4/generated/fcast/v4/set-progress-update-interval';
import { CompanionHelloRequest } from 'common/v4/generated/fcast/v4/companion-hello-request';
import { CompanionHelloResponse } from 'common/v4/generated/fcast/v4/companion-hello-response';
import { CompanionResourceInfoResponse } from 'common/v4/generated/fcast/v4/companion-resource-info-response';
import { CompanionResourceSize } from 'common/v4/generated/fcast/v4/companion-resource-size';
import { KnownResourceSize } from 'common/v4/generated/fcast/v4/known-resource-size';
import { UnknownResourceSize } from 'common/v4/generated/fcast/v4/unknown-resource-size';
import { TracksAvailable } from 'common/v4/generated/fcast/v4/tracks-available';
import { Time } from 'common/v4/generated/fcast/v4/time';

export type GenericSpec =
    | { kind: 'string'; value: string | null }
    | { kind: 'float'; value: number }
    | { kind: 'int'; value: number }
    | { kind: 'list'; items: (GenericSpec | null)[] }
    | { kind: 'kv'; key: string; value: GenericSpec | null };

export interface ChapterSpec {
    start?: number;
    length?: number;
    title: string;
    skipLabel?: string;
}

export type MetadataSpec =
    | { kind: 'video'; chapters: ChapterSpec[] }
    | { kind: 'audio'; artist?: string; album?: string; chapters: ChapterSpec[] };

export interface HeaderSpec {
    key: string;
    value: string;
}

export interface KeyValueSpec {
    key: string;
    value: GenericSpec | null;
}

export interface MediaItemSpec {
    // Defaults to 'video/mp4'. `null` leaves the required field out.
    container?: string | null;
    sourceUrl?: string;
    startTime?: number;
    volume?: number | null;
    speed?: number | null;
    headers?: HeaderSpec[];
    title?: string;
    thumbnailUrl?: string;
    metadata?: MetadataSpec;
    extraMetadata?: KeyValueSpec[];
}

export interface QueueItemSpec {
    mediaItem: MediaItemSpec;
    playbackDuration?: number | null;
}

export type PositionSpec = { kind: 'index'; index: number } | { kind: 'front' } | { kind: 'back' };

export type ResourceSizeSpec = { kind: 'known'; size: number } | { kind: 'unknown' } | null;

export interface BuildOptions {
    // Replaces the builder's required-field check with a no-op, so that required fields can be left out.
    skipRequiredChecks?: boolean;
}

export function packetBytes(type: number, make: (builder: flatbuffers.Builder) => flatbuffers.Offset, options: BuildOptions = {}): Uint8Array {
    const builder = new flatbuffers.Builder(256);
    if (options.skipRequiredChecks) {
        (builder as any).requiredField = () => {};
    }
    const payload = make(builder);
    builder.finish(Packet.createPacket(builder, type as Message, payload));
    return builder.asUint8Array().slice();
}

function createHeader(builder: flatbuffers.Builder, header: HeaderSpec): flatbuffers.Offset {
    const key = builder.createString(header.key);
    const value = builder.createString(header.value);
    RequestHeader.startRequestHeader(builder);
    RequestHeader.addKey(builder, key);
    RequestHeader.addValue(builder, value);
    return RequestHeader.endRequestHeader(builder);
}

function createGeneric(builder: flatbuffers.Builder, spec: GenericSpec | null): { type: GenericMetaValue; offset: flatbuffers.Offset } {
    if (spec === null) {
        return { type: GenericMetaValue.NONE, offset: 0 };
    }
    switch (spec.kind) {
        case 'string': {
            const value = spec.value === null ? 0 : builder.createString(spec.value);
            GenericMetaString.startGenericMetaString(builder);
            GenericMetaString.addValue(builder, value);
            return { type: GenericMetaValue.String, offset: GenericMetaString.endGenericMetaString(builder) };
        }
        case 'float': {
            GenericMetaFloat.startGenericMetaFloat(builder);
            GenericMetaFloat.addValue(builder, spec.value);
            return { type: GenericMetaValue.Float, offset: GenericMetaFloat.endGenericMetaFloat(builder) };
        }
        case 'int': {
            GenericMetaInt.startGenericMetaInt(builder);
            GenericMetaInt.addValue(builder, BigInt(spec.value));
            return { type: GenericMetaValue.Int, offset: GenericMetaInt.endGenericMetaInt(builder) };
        }
        case 'list': {
            const wrappers = spec.items.map((item) => createWrapped(builder, item));
            const values = GenericMetaList.createValueVector(builder, wrappers);
            GenericMetaList.startGenericMetaList(builder);
            GenericMetaList.addValue(builder, values);
            return { type: GenericMetaValue.List, offset: GenericMetaList.endGenericMetaList(builder) };
        }
        case 'kv':
            return { type: GenericMetaValue.KVPair, offset: createKeyValue(builder, spec.key, spec.value) };
    }
}

function createWrapped(builder: flatbuffers.Builder, spec: GenericSpec | null): flatbuffers.Offset {
    const value = createGeneric(builder, spec);
    WrappedGenericMetaValue.startWrappedGenericMetaValue(builder);
    WrappedGenericMetaValue.addValueType(builder, value.type);
    WrappedGenericMetaValue.addValue(builder, value.offset);
    return WrappedGenericMetaValue.endWrappedGenericMetaValue(builder);
}

function createKeyValue(builder: flatbuffers.Builder, key: string, value: GenericSpec | null): flatbuffers.Offset {
    const keyOffset = builder.createString(key);
    const generic = createGeneric(builder, value);
    MetadataKV.startMetadataKV(builder);
    MetadataKV.addKey(builder, keyOffset);
    MetadataKV.addValueType(builder, generic.type);
    MetadataKV.addValue(builder, generic.offset);
    return MetadataKV.endMetadataKV(builder);
}

function createChapter(builder: flatbuffers.Builder, chapter: ChapterSpec): flatbuffers.Offset {
    const title = builder.createString(chapter.title);
    const skipLabel = chapter.skipLabel === undefined ? 0 : builder.createString(chapter.skipLabel);
    Chapter.startChapter(builder);
    Chapter.addTitle(builder, title);
    // Structs must be written inline, immediately before the field that stores them.
    if (chapter.start !== undefined) {
        Chapter.addStart(builder, Time.createTime(builder, BigInt(chapter.start)));
    }
    if (chapter.length !== undefined) {
        Chapter.addLength(builder, Time.createTime(builder, BigInt(chapter.length)));
    }
    if (skipLabel !== 0) {
        Chapter.addSkipLabel(builder, skipLabel);
    }
    return Chapter.endChapter(builder);
}

function createChapters(builder: flatbuffers.Builder, chapters: ChapterSpec[]): flatbuffers.Offset {
    return VideoMetadata.createChaptersVector(builder, chapters.map((c) => createChapter(builder, c)));
}

function createMetadata(builder: flatbuffers.Builder, spec: MetadataSpec): { type: Metadata; offset: flatbuffers.Offset } {
    if (spec.kind === 'video') {
        const chapters = createChapters(builder, spec.chapters);
        VideoMetadata.startVideoMetadata(builder);
        VideoMetadata.addChapters(builder, chapters);
        return { type: Metadata.Video, offset: VideoMetadata.endVideoMetadata(builder) };
    }
    const artist = spec.artist === undefined ? 0 : builder.createString(spec.artist);
    const album = spec.album === undefined ? 0 : builder.createString(spec.album);
    const chapters = createChapters(builder, spec.chapters);
    AudioMetadata.startAudioMetadata(builder);
    if (artist !== 0) {
        AudioMetadata.addArtist(builder, artist);
    }
    if (album !== 0) {
        AudioMetadata.addAlbum(builder, album);
    }
    AudioMetadata.addChapters(builder, chapters);
    return { type: Metadata.Audio, offset: AudioMetadata.endAudioMetadata(builder) };
}

export function createMediaItem(builder: flatbuffers.Builder, spec: MediaItemSpec): flatbuffers.Offset {
    const containerName = spec.container === undefined ? 'video/mp4' : spec.container;
    const container = containerName === null ? 0 : builder.createString(containerName);
    const sourceUrl = builder.createString(spec.sourceUrl === undefined ? 'https://example.com/media.mp4' : spec.sourceUrl);
    const headers = (spec.headers || []).map((h) => createHeader(builder, h));
    const title = spec.title === undefined ? 0 : builder.createString(spec.title);
    const thumbnailUrl = spec.thumbnailUrl === undefined ? 0 : builder.createString(spec.thumbnailUrl);
    const metadata = spec.metadata === undefined ? null : createMetadata(builder, spec.metadata);
    const extraMetadata = (spec.extraMetadata || []).map((kv) => createKeyValue(builder, kv.key, kv.value));

    const headersVector = headers.length === 0 ? 0 : MediaItem.createHeadersVector(builder, headers);
    const extraVector = extraMetadata.length === 0 ? 0 : MediaItem.createExtraMetadataVector(builder, extraMetadata);

    MediaItem.startMediaItem(builder);
    if (container !== 0) {
        MediaItem.addContainer(builder, container);
    }
    MediaItem.addSourceUrl(builder, sourceUrl);
    if (spec.startTime !== undefined) {
        MediaItem.addStartTime(builder, Time.createTime(builder, BigInt(spec.startTime)));
    }
    if (spec.volume !== undefined && spec.volume !== null) {
        MediaItem.addVolume(builder, spec.volume);
    }
    if (spec.speed !== undefined && spec.speed !== null) {
        MediaItem.addSpeed(builder, spec.speed);
    }
    if (headersVector !== 0) {
        MediaItem.addHeaders(builder, headersVector);
    }
    if (title !== 0) {
        MediaItem.addTitle(builder, title);
    }
    if (thumbnailUrl !== 0) {
        MediaItem.addThumbnailUrl(builder, thumbnailUrl);
    }
    if (metadata !== null) {
        MediaItem.addMetadataType(builder, metadata.type);
        MediaItem.addMetadata(builder, metadata.offset);
    }
    if (extraVector !== 0) {
        MediaItem.addExtraMetadata(builder, extraVector);
    }
    return MediaItem.endMediaItem(builder);
}

function createQueueItem(builder: flatbuffers.Builder, spec: QueueItemSpec): flatbuffers.Offset {
    const mediaItem = createMediaItem(builder, spec.mediaItem);
    QueueItem.startQueueItem(builder);
    QueueItem.addMediaItem(builder, mediaItem);
    if (spec.playbackDuration !== undefined && spec.playbackDuration !== null) {
        QueueItem.addPlaybackDuration(builder, Time.createTime(builder, BigInt(spec.playbackDuration)));
    }
    return QueueItem.endQueueItem(builder);
}

function createQueue(builder: flatbuffers.Builder, items: QueueItemSpec[], startIndex: number | null, autoplay: boolean): flatbuffers.Offset {
    const itemOffsets = items.map((item) => createQueueItem(builder, item));
    const itemsVector = Queue.createItemsVector(builder, itemOffsets);
    Queue.startQueue(builder);
    Queue.addItems(builder, itemsVector);
    if (startIndex !== null) {
        Queue.addStartIndex(builder, startIndex);
    }
    Queue.addAutoplay(builder, autoplay);
    return Queue.endQueue(builder);
}

function createPosition(builder: flatbuffers.Builder, spec: PositionSpec): { type: QueuePosition; offset: flatbuffers.Offset } {
    switch (spec.kind) {
        case 'index': {
            QueueIndex.startQueueIndex(builder);
            QueueIndex.addIndex(builder, spec.index);
            return { type: QueuePosition.Index, offset: QueueIndex.endQueueIndex(builder) };
        }
        case 'front':
            QueueMarkerFront.startQueueMarkerFront(builder);
            return { type: QueuePosition.Front, offset: QueueMarkerFront.endQueueMarkerFront(builder) };
        case 'back':
            QueueMarkerBack.startQueueMarkerBack(builder);
            return { type: QueuePosition.Back, offset: QueueMarkerBack.endQueueMarkerBack(builder) };
    }
}

export function loadSingle(item: MediaItemSpec, options: BuildOptions = {}): Uint8Array {
    return packetBytes(Message.Load, (builder) => {
        const mediaItem = createMediaItem(builder, item);
        return Load.createLoad(builder, MediaSource.Single, mediaItem);
    }, options);
}

export function loadQueue(items: QueueItemSpec[], startIndex: number | null, autoplay: boolean): Uint8Array {
    return packetBytes(Message.Load, (builder) => {
        const queue = createQueue(builder, items, startIndex, autoplay);
        return Load.createLoad(builder, MediaSource.Queue, queue);
    });
}

// A Load whose headers vector holds `refs` references to a single RequestHeader table.
// FlatBuffers allows this aliasing, so the decoder has to budget each reference separately.
export function loadSingleSharedHeader(refs: number, key: string, value: string): Uint8Array {
    return packetBytes(Message.Load, (builder) => {
        const header = createHeader(builder, { key, value });
        const headers: flatbuffers.Offset[] = [];
        for (let i = 0; i < refs; i++) {
            headers.push(header);
        }
        const headersVector = MediaItem.createHeadersVector(builder, headers);
        const container = builder.createString('video/mp4');
        const sourceUrl = builder.createString('https://example.com/media.mp4');
        MediaItem.startMediaItem(builder);
        MediaItem.addContainer(builder, container);
        MediaItem.addSourceUrl(builder, sourceUrl);
        MediaItem.addHeaders(builder, headersVector);
        const mediaItem = MediaItem.endMediaItem(builder);
        return Load.createLoad(builder, MediaSource.Single, mediaItem);
    });
}

export function progressChanged(position: number | null, duration: number | null): Uint8Array {
    return packetBytes(Message.ProgressChanged, (builder) => {
        ProgressChanged.startProgressChanged(builder);
        if (position !== null) {
            ProgressChanged.addPosition(builder, Time.createTime(builder, BigInt(position)));
        }
        if (duration !== null) {
            ProgressChanged.addDuration(builder, Time.createTime(builder, BigInt(duration)));
        }
        return ProgressChanged.endProgressChanged(builder);
    });
}

export function volumeChanged(volume: number): Uint8Array {
    return packetBytes(Message.VolumeChanged, (builder) => {
        VolumeChanged.startVolumeChanged(builder);
        VolumeChanged.addVolume(builder, volume);
        return VolumeChanged.endVolumeChanged(builder);
    });
}

export function playbackStateChanged(state: PlaybackState): Uint8Array {
    return packetBytes(Message.PlaybackStateChanged, (builder) => {
        PlaybackStateChanged.startPlaybackStateChanged(builder);
        PlaybackStateChanged.addState(builder, state);
        return PlaybackStateChanged.endPlaybackStateChanged(builder);
    });
}

export function speedChanged(speed: number): Uint8Array {
    return packetBytes(Message.SpeedChanged, (builder) => {
        SpeedChanged.startSpeedChanged(builder);
        SpeedChanged.addSpeed(builder, speed);
        return SpeedChanged.endSpeedChanged(builder);
    });
}

export function senderIntroduction(displayName: string, appName: string, appVersion: string): Uint8Array {
    return packetBytes(Message.SenderIntroduction, (builder) => {
        const displayNameOffset = builder.createString(displayName);
        const appNameOffset = builder.createString(appName);
        const appVersionOffset = builder.createString(appVersion);
        DeviceInfo.startDeviceInfo(builder);
        DeviceInfo.addDisplayName(builder, displayNameOffset);
        DeviceInfo.addAppName(builder, appNameOffset);
        DeviceInfo.addAppVersion(builder, appVersionOffset);
        const deviceInfo = DeviceInfo.endDeviceInfo(builder);
        SenderIntroduction.startSenderIntroduction(builder);
        SenderIntroduction.addDeviceInfo(builder, deviceInfo);
        return SenderIntroduction.endSenderIntroduction(builder);
    });
}

export function stopPlayback(): Uint8Array {
    return packetBytes(Message.StopPlayback, (builder) => {
        StopPlayback.startStopPlayback(builder);
        return StopPlayback.endStopPlayback(builder);
    });
}

export function startMirroringSession(sessionId: number): Uint8Array {
    return packetBytes(Message.StartMirroringSession, (builder) => {
        StartMirroringSession.startStartMirroringSession(builder);
        StartMirroringSession.addSessionId(builder, sessionId);
        return StartMirroringSession.endStartMirroringSession(builder);
    });
}

export function mirroringSessionDescription(sessionId: number, sdp: string): Uint8Array {
    return packetBytes(Message.MirroringSessionDescription, (builder) => {
        const sdpOffset = builder.createString(sdp);
        MirroringSessionDescription.startMirroringSessionDescription(builder);
        MirroringSessionDescription.addSessionId(builder, sessionId);
        MirroringSessionDescription.addSdp(builder, sdpOffset);
        return MirroringSessionDescription.endMirroringSessionDescription(builder);
    });
}

export function queueInsert(item: QueueItemSpec, position: PositionSpec): Uint8Array {
    return packetBytes(Message.QueueInsert, (builder) => {
        const itemOffset = createQueueItem(builder, item);
        const pos = createPosition(builder, position);
        QueueInsert.startQueueInsert(builder);
        QueueInsert.addItem(builder, itemOffset);
        QueueInsert.addPositionType(builder, pos.type);
        QueueInsert.addPosition(builder, pos.offset);
        return QueueInsert.endQueueInsert(builder);
    });
}

export function queueRemove(position: PositionSpec): Uint8Array {
    return packetBytes(Message.QueueRemove, (builder) => {
        const pos = createPosition(builder, position);
        QueueRemove.startQueueRemove(builder);
        QueueRemove.addPositionType(builder, pos.type);
        QueueRemove.addPosition(builder, pos.offset);
        return QueueRemove.endQueueRemove(builder);
    });
}

export function queueItemSelected(position: PositionSpec): Uint8Array {
    return packetBytes(Message.QueueItemSelected, (builder) => {
        const pos = createPosition(builder, position);
        QueueItemSelected.startQueueItemSelected(builder);
        QueueItemSelected.addPositionType(builder, pos.type);
        QueueItemSelected.addPosition(builder, pos.offset);
        return QueueItemSelected.endQueueItemSelected(builder);
    });
}

export function changeTrack(id: number | null, trackType: MediaTrackType): Uint8Array {
    return packetBytes(Message.ChangeTrack, (builder) => {
        ChangeTrack.startChangeTrack(builder);
        if (id !== null) {
            ChangeTrack.addId(builder, id);
        }
        ChangeTrack.addTrackType(builder, trackType);
        return ChangeTrack.endChangeTrack(builder);
    });
}

export function addSubtitleSource(url: string, select: boolean, name: string | null, withMetadata: boolean): Uint8Array {
    return packetBytes(Message.AddSubtitleSource, (builder) => {
        const urlOffset = builder.createString(url);
        const nameOffset = name === null ? 0 : builder.createString(name);
        let metadataOffset = 0;
        if (withMetadata) {
            SubtitleTrackMeta.startSubtitleTrackMeta(builder);
            metadataOffset = SubtitleTrackMeta.endSubtitleTrackMeta(builder);
        }
        AddSubtitleSource.startAddSubtitleSource(builder);
        AddSubtitleSource.addUrl(builder, urlOffset);
        AddSubtitleSource.addSelect(builder, select);
        if (nameOffset !== 0) {
            AddSubtitleSource.addName(builder, nameOffset);
        }
        if (metadataOffset !== 0) {
            AddSubtitleSource.addMetadata(builder, metadataOffset);
        }
        return AddSubtitleSource.endAddSubtitleSource(builder);
    });
}

export function setProgressUpdateInterval(interval: number | null): Uint8Array {
    return packetBytes(Message.SetProgressUpdateInterval, (builder) => {
        SetProgressUpdateInterval.startSetProgressUpdateInterval(builder);
        if (interval !== null) {
            SetProgressUpdateInterval.addInterval(builder, Time.createTime(builder, BigInt(interval)));
        }
        return SetProgressUpdateInterval.endSetProgressUpdateInterval(builder);
    });
}

export function companionHelloRequest(): Uint8Array {
    return packetBytes(Message.CompanionHelloRequest, (builder) => {
        CompanionHelloRequest.startCompanionHelloRequest(builder);
        return CompanionHelloRequest.endCompanionHelloRequest(builder);
    });
}

export function companionHelloResponse(providerId: number): Uint8Array {
    return packetBytes(Message.CompanionHelloResponse, (builder) => {
        CompanionHelloResponse.startCompanionHelloResponse(builder);
        CompanionHelloResponse.addProviderId(builder, providerId);
        return CompanionHelloResponse.endCompanionHelloResponse(builder);
    });
}

export function tracksAvailable(): Uint8Array {
    return packetBytes(Message.TracksAvailable, (builder) => {
        const tracks = TracksAvailable.createTracksVector(builder, []);
        TracksAvailable.startTracksAvailable(builder);
        TracksAvailable.addTracks(builder, tracks);
        return TracksAvailable.endTracksAvailable(builder);
    });
}

export function companionResourceInfoResponse(requestId: number, contentType: string, size: ResourceSizeSpec): Uint8Array {
    return packetBytes(Message.CompanionResourceInfoResponse, (builder) => {
        const contentTypeOffset = builder.createString(contentType);
        let sizeType = CompanionResourceSize.NONE;
        let sizeOffset = 0;
        if (size === null) {
            sizeType = CompanionResourceSize.NONE;
        } else if (size.kind === 'unknown') {
            UnknownResourceSize.startUnknownResourceSize(builder);
            sizeOffset = UnknownResourceSize.endUnknownResourceSize(builder);
            sizeType = CompanionResourceSize.Unknown;
        } else {
            KnownResourceSize.startKnownResourceSize(builder);
            KnownResourceSize.addSize(builder, BigInt(size.size));
            sizeOffset = KnownResourceSize.endKnownResourceSize(builder);
            sizeType = CompanionResourceSize.Known;
        }
        CompanionResourceInfoResponse.startCompanionResourceInfoResponse(builder);
        CompanionResourceInfoResponse.addRequestId(builder, requestId);
        CompanionResourceInfoResponse.addContentType(builder, contentTypeOffset);
        CompanionResourceInfoResponse.addResourceSizeType(builder, sizeType);
        CompanionResourceInfoResponse.addResourceSize(builder, sizeOffset);
        return CompanionResourceInfoResponse.endCompanionResourceInfoResponse(builder);
    });
}

// Generic meta value fixtures used by the decoder tests.
export function nestedList(depth: number): GenericSpec {
    let spec: GenericSpec = { kind: 'string', value: 'leaf' };
    for (let i = 0; i < depth; i++) {
        spec = { kind: 'list', items: [spec] };
    }
    return spec;
}

export const METADATA_FIXTURE: MediaItemSpec = {
    container: 'video/x-matroska',
    sourceUrl: 'https://example.com/movie.mkv',
    startTime: 2500000,
    volume: 0.5,
    speed: 1.25,
    headers: [{ key: 'Authorization', value: 'Bearer token' }],
    title: 'Movie',
    thumbnailUrl: 'https://example.com/thumb.png',
    metadata: {
        kind: 'video',
        chapters: [
            { start: 0, length: 60000000, title: 'Intro', skipLabel: 'Skip intro' },
            { start: 60000000, title: 'Main' },
        ],
    },
    extraMetadata: [
        { key: 'tags', value: { kind: 'list', items: [{ kind: 'string', value: 'a' }, null, { kind: 'int', value: 42 }] } },
        { key: 'rating', value: { kind: 'float', value: 2.5 } },
    ],
};

export const QUEUE_FIXTURE: QueueItemSpec[] = [
    { mediaItem: { sourceUrl: 'https://example.com/1.mp4', title: 'One', headers: [{ key: 'a', value: 'b' }] }, playbackDuration: 1000000 },
    { mediaItem: { sourceUrl: 'https://example.com/2.mp4', title: 'Two' } },
    { mediaItem: { sourceUrl: 'https://example.com/3.mp4', title: 'Three' }, playbackDuration: null },
];

export const INSERT_FIXTURE: { item: QueueItemSpec; position: PositionSpec } = {
    item: { mediaItem: { sourceUrl: 'https://example.com/insert.mp4', title: 'Inserted' } },
    position: { kind: 'index', index: 2 },
};
