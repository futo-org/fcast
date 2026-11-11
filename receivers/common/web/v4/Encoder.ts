import * as flatbuffers from 'flatbuffers';
import { Packet } from './generated/fcast/v4/packet';
import { Message } from './generated/fcast/v4/message';
import { ReceiverIntroduction } from './generated/fcast/v4/receiver-introduction';
import { ReceiverCapabilities } from './generated/fcast/v4/receiver-capabilities';
import { DeviceInfo } from './generated/fcast/v4/device-info';
import { MediaCapabilities } from './generated/fcast/v4/media-capabilities';
import { DisplayCapabilities } from './generated/fcast/v4/display-capabilities';
import { AudioCapabilities } from './generated/fcast/v4/audio-capabilities';
import { VideoResolution } from './generated/fcast/v4/video-resolution';
import { Error as ErrorMessage } from './generated/fcast/v4/error';
import { PlaybackState } from './generated/fcast/v4/playback-state';
import { PlaybackStateChanged } from './generated/fcast/v4/playback-state-changed';
import { VolumeChanged } from './generated/fcast/v4/volume-changed';
import { SpeedChanged } from './generated/fcast/v4/speed-changed';
import { ProgressChanged } from './generated/fcast/v4/progress-changed';
import { Time } from './generated/fcast/v4/time';
import { StopPlayback } from './generated/fcast/v4/stop-playback';
import { Load } from './generated/fcast/v4/load';
import { MediaSource } from './generated/fcast/v4/media-source';
import { MediaItem } from './generated/fcast/v4/media-item';
import { Metadata } from './generated/fcast/v4/metadata';
import { VideoMetadata } from './generated/fcast/v4/video-metadata';
import { AudioMetadata } from './generated/fcast/v4/audio-metadata';
import { Chapter } from './generated/fcast/v4/chapter';
import { MetadataKV } from './generated/fcast/v4/metadata-kv';
import { GenericMetaValue } from './generated/fcast/v4/generic-meta-value';
import { GenericMetaString } from './generated/fcast/v4/generic-meta-string';
import { GenericMetaFloat } from './generated/fcast/v4/generic-meta-float';
import { GenericMetaInt } from './generated/fcast/v4/generic-meta-int';
import { GenericMetaList } from './generated/fcast/v4/generic-meta-list';
import { WrappedGenericMetaValue } from './generated/fcast/v4/wrapped-generic-meta-value';
import { Queue } from './generated/fcast/v4/queue';
import { QueueItem } from './generated/fcast/v4/queue-item';
import { QueuePosition } from './generated/fcast/v4/queue-position';
import { QueueIndex } from './generated/fcast/v4/queue-index';
import { QueueMarkerFront } from './generated/fcast/v4/queue-marker-front';
import { QueueMarkerBack } from './generated/fcast/v4/queue-marker-back';
import { QueueInsert } from './generated/fcast/v4/queue-insert';
import { QueueRemove } from './generated/fcast/v4/queue-remove';
import { QueueItemSelected } from './generated/fcast/v4/queue-item-selected';
import { ChangeTrack } from './generated/fcast/v4/change-track';
import { MediaTrackType } from './generated/fcast/v4/media-track-type';
import { MediaTrack } from './generated/fcast/v4/media-track';
import { MediaTrackMetadata } from './generated/fcast/v4/media-track-metadata';
import { VideoTrackMeta } from './generated/fcast/v4/video-track-meta';
import { AudioTrackMeta } from './generated/fcast/v4/audio-track-meta';
import { SubtitleTrackMeta } from './generated/fcast/v4/subtitle-track-meta';
import { TracksAvailable } from './generated/fcast/v4/tracks-available';
import { CompanionHelloResponse } from './generated/fcast/v4/companion-hello-response';
import { CompanionResourceInfoRequest } from './generated/fcast/v4/companion-resource-info-request';
import { CompanionResourceRequest } from './generated/fcast/v4/companion-resource-request';
import { ResourceReadHead } from './generated/fcast/v4/resource-read-head';
import { ChapterData, GenericMeta, MediaItemData, MediaMetadataData, MediaSourceData, MetadataKVData, QueueItemData, QueuePositionData } from './Decoder';

export interface DeviceInfoInput {
    displayName: string;
    appName: string;
    appVersion: string;
}

export interface MediaCapabilitiesInput {
    protocols: string[];
    containers: string[];
    videoFormats: string[];
    audioFormats: string[];
    subtitleFormats: string[];
    hdrFormats: string[];
    imageFormats: string[];
    externalSubtitles: boolean;
    mirroring: boolean;
}

export interface ReceiverCapabilitiesInput {
    media: MediaCapabilitiesInput;
    display: { width: number; height: number } | null;
    volumeStepInterval: number | null;
}

function finishPacket(builder: flatbuffers.Builder, type: Message, payload: flatbuffers.Offset): Uint8Array {
    builder.finish(Packet.createPacket(builder, type, payload));
    return builder.asUint8Array().slice();
}

function toMicros(value: number): bigint {
    return BigInt(Math.round(value));
}

function createStrings(builder: flatbuffers.Builder, values: string[]): flatbuffers.Offset[] {
    return values.map((value) => builder.createString(value));
}

function createDeviceInfo(builder: flatbuffers.Builder, info: DeviceInfoInput): flatbuffers.Offset {
    const displayName = builder.createString(info.displayName);
    const appName = builder.createString(info.appName);
    const appVersion = builder.createString(info.appVersion);
    DeviceInfo.startDeviceInfo(builder);
    DeviceInfo.addDisplayName(builder, displayName);
    DeviceInfo.addAppName(builder, appName);
    DeviceInfo.addAppVersion(builder, appVersion);
    return DeviceInfo.endDeviceInfo(builder);
}

function createMediaCapabilities(builder: flatbuffers.Builder, media: MediaCapabilitiesInput): flatbuffers.Offset {
    const protocols = MediaCapabilities.createProtocolsVector(builder, createStrings(builder, media.protocols));
    const containers = MediaCapabilities.createContainersVector(builder, createStrings(builder, media.containers));
    const videoFormats = MediaCapabilities.createVideoFormatsVector(builder, createStrings(builder, media.videoFormats));
    const audioFormats = MediaCapabilities.createAudioFormatsVector(builder, createStrings(builder, media.audioFormats));
    const subtitleFormats = MediaCapabilities.createSubtitleFormatsVector(builder, createStrings(builder, media.subtitleFormats));
    const hdrFormats = MediaCapabilities.createHdrFormatsVector(builder, createStrings(builder, media.hdrFormats));
    const imageFormats = MediaCapabilities.createImageFormatsVector(builder, createStrings(builder, media.imageFormats));
    MediaCapabilities.startMediaCapabilities(builder);
    MediaCapabilities.addProtocols(builder, protocols);
    MediaCapabilities.addContainers(builder, containers);
    MediaCapabilities.addVideoFormats(builder, videoFormats);
    MediaCapabilities.addAudioFormats(builder, audioFormats);
    MediaCapabilities.addSubtitleFormats(builder, subtitleFormats);
    MediaCapabilities.addHdrFormats(builder, hdrFormats);
    MediaCapabilities.addImageFormats(builder, imageFormats);
    MediaCapabilities.addExternalSubtitles(builder, media.externalSubtitles);
    MediaCapabilities.addMirroring(builder, media.mirroring);
    return MediaCapabilities.endMediaCapabilities(builder);
}

function createDisplayCapabilities(builder: flatbuffers.Builder, display: { width: number; height: number }): flatbuffers.Offset {
    DisplayCapabilities.startDisplayCapabilities(builder);
    // Structs must be written inline, immediately before the field that stores them.
    DisplayCapabilities.addResolution(builder, VideoResolution.createVideoResolution(builder, display.width, display.height));
    return DisplayCapabilities.endDisplayCapabilities(builder);
}

function createAudioCapabilities(builder: flatbuffers.Builder, volumeStepInterval: number): flatbuffers.Offset {
    AudioCapabilities.startAudioCapabilities(builder);
    AudioCapabilities.addVolumeStepInterval(builder, volumeStepInterval);
    return AudioCapabilities.endAudioCapabilities(builder);
}

function createReceiverCapabilities(builder: flatbuffers.Builder, caps: ReceiverCapabilitiesInput): flatbuffers.Offset {
    const media = createMediaCapabilities(builder, caps.media);
    const display = caps.display === null ? 0 : createDisplayCapabilities(builder, caps.display);
    const audio = caps.volumeStepInterval === null ? 0 : createAudioCapabilities(builder, caps.volumeStepInterval);
    ReceiverCapabilities.startReceiverCapabilities(builder);
    ReceiverCapabilities.addMedia(builder, media);
    if (display !== 0) {
        ReceiverCapabilities.addDisplay(builder, display);
    }
    if (audio !== 0) {
        ReceiverCapabilities.addAudio(builder, audio);
    }
    return ReceiverCapabilities.endReceiverCapabilities(builder);
}

export function encodeReceiverIntroduction(info: DeviceInfoInput, caps: ReceiverCapabilitiesInput): Uint8Array {
    const builder = new flatbuffers.Builder(1024);
    const deviceInfo = createDeviceInfo(builder, info);
    const capabilities = createReceiverCapabilities(builder, caps);
    ReceiverIntroduction.startReceiverIntroduction(builder);
    ReceiverIntroduction.addDeviceInfo(builder, deviceInfo);
    ReceiverIntroduction.addCapabilities(builder, capabilities);
    const introduction = ReceiverIntroduction.endReceiverIntroduction(builder);
    return finishPacket(builder, Message.ReceiverIntroduction, introduction);
}

export function encodeError(kind: number, packetNum: number | null): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    ErrorMessage.startError(builder);
    ErrorMessage.addKind(builder, kind);
    if (packetNum !== null) {
        ErrorMessage.addPacketNum(builder, packetNum);
    }
    const error = ErrorMessage.endError(builder);
    return finishPacket(builder, Message.Error, error);
}

export function encodePlaybackStateChanged(state: PlaybackState): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    PlaybackStateChanged.startPlaybackStateChanged(builder);
    PlaybackStateChanged.addState(builder, state);
    const message = PlaybackStateChanged.endPlaybackStateChanged(builder);
    return finishPacket(builder, Message.PlaybackStateChanged, message);
}

export function encodeStopPlayback(): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    StopPlayback.startStopPlayback(builder);
    return finishPacket(builder, Message.StopPlayback, StopPlayback.endStopPlayback(builder));
}

export function encodeVolumeChanged(volume: number): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    VolumeChanged.startVolumeChanged(builder);
    VolumeChanged.addVolume(builder, volume);
    const message = VolumeChanged.endVolumeChanged(builder);
    return finishPacket(builder, Message.VolumeChanged, message);
}

export function encodeSpeedChanged(speed: number): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    SpeedChanged.startSpeedChanged(builder);
    SpeedChanged.addSpeed(builder, speed);
    const message = SpeedChanged.endSpeedChanged(builder);
    return finishPacket(builder, Message.SpeedChanged, message);
}

export function encodeProgressChanged(positionMicros: number, durationMicros: number | null): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    ProgressChanged.startProgressChanged(builder);
    ProgressChanged.addPosition(builder, Time.createTime(builder, toMicros(positionMicros)));
    if (durationMicros !== null) {
        ProgressChanged.addDuration(builder, Time.createTime(builder, toMicros(durationMicros)));
    }
    const message = ProgressChanged.endProgressChanged(builder);
    return finishPacket(builder, Message.ProgressChanged, message);
}

export interface MediaTrackData {
    id: number;
    trackType: MediaTrackType;
    language: string;
    title: string | null;
}

function createTime(builder: flatbuffers.Builder, micros: number): flatbuffers.Offset {
    return Time.createTime(builder, toMicros(micros));
}

function createChapters(builder: flatbuffers.Builder, chapters: ChapterData[]): flatbuffers.Offset[] {
    return chapters.map((chapter) => {
        const title = builder.createString(chapter.title);
        const skipLabel = chapter.skipLabel === null ? 0 : builder.createString(chapter.skipLabel);
        Chapter.startChapter(builder);
        if (chapter.start !== null) {
            Chapter.addStart(builder, createTime(builder, chapter.start));
        }
        if (chapter.length !== null) {
            Chapter.addLength(builder, createTime(builder, chapter.length));
        }
        Chapter.addTitle(builder, title);
        if (skipLabel !== 0) {
            Chapter.addSkipLabel(builder, skipLabel);
        }
        return Chapter.endChapter(builder);
    });
}

function createMediaMetadata(builder: flatbuffers.Builder, metadata: MediaMetadataData): flatbuffers.Offset {
    if (metadata.kind === 'video') {
        const chapters = VideoMetadata.createChaptersVector(builder, createChapters(builder, metadata.chapters));
        return VideoMetadata.createVideoMetadata(builder, chapters);
    }

    const artist = metadata.artist === null ? 0 : builder.createString(metadata.artist);
    const album = metadata.album === null ? 0 : builder.createString(metadata.album);
    const chapters = AudioMetadata.createChaptersVector(builder, createChapters(builder, metadata.chapters));
    return AudioMetadata.createAudioMetadata(builder, artist, album, chapters);
}

function genericType(value: GenericMeta | null): GenericMetaValue {
    if (value === null) {
        return GenericMetaValue.NONE;
    }
    switch (value.kind) {
        case 'string': return GenericMetaValue.String;
        case 'float': return GenericMetaValue.Float;
        case 'int': return GenericMetaValue.Int;
        case 'list': return GenericMetaValue.List;
        default: return GenericMetaValue.KVPair;
    }
}

function createGeneric(builder: flatbuffers.Builder, value: GenericMeta | null): flatbuffers.Offset {
    if (value === null) {
        return 0;
    }
    switch (value.kind) {
        case 'string': {
            const text = value.value === null ? 0 : builder.createString(value.value);
            return GenericMetaString.createGenericMetaString(builder, text);
        }
        case 'float':
            return GenericMetaFloat.createGenericMetaFloat(builder, value.value);
        case 'int':
            return GenericMetaInt.createGenericMetaInt(builder, BigInt(value.value));
        case 'list': {
            const items = value.value.map((item) => {
                const inner = createGeneric(builder, item);
                return WrappedGenericMetaValue.createWrappedGenericMetaValue(builder, genericType(item), inner);
            });
            return GenericMetaList.createGenericMetaList(builder, GenericMetaList.createValueVector(builder, items));
        }
        default:
            return createMetadataKV(builder, { key: value.key, value: value.value });
    }
}

function createMetadataKV(builder: flatbuffers.Builder, kv: MetadataKVData): flatbuffers.Offset {
    const key = builder.createString(kv.key);
    const value = createGeneric(builder, kv.value);
    return MetadataKV.createMetadataKV(builder, key, genericType(kv.value), value);
}

// Request headers are never encoded: a relayed item must not carry another sender's credentials.
function createMediaItem(builder: flatbuffers.Builder, item: MediaItemData): flatbuffers.Offset {
    const container = builder.createString(item.container);
    const sourceUrl = builder.createString(item.sourceUrl);
    const title = item.title === null ? 0 : builder.createString(item.title);
    const thumbnailUrl = item.thumbnailUrl === null ? 0 : builder.createString(item.thumbnailUrl);
    const metadata = item.metadata === null ? 0 : createMediaMetadata(builder, item.metadata);
    const extraMetadata = item.extraMetadata.length === 0 ? 0 :
        MediaItem.createExtraMetadataVector(builder, item.extraMetadata.map((kv) => createMetadataKV(builder, kv)));

    MediaItem.startMediaItem(builder);
    MediaItem.addContainer(builder, container);
    MediaItem.addSourceUrl(builder, sourceUrl);
    if (item.startTime !== null) {
        MediaItem.addStartTime(builder, createTime(builder, item.startTime));
    }
    if (item.volume !== null) {
        MediaItem.addVolume(builder, item.volume);
    }
    if (item.speed !== null) {
        MediaItem.addSpeed(builder, item.speed);
    }
    if (title !== 0) {
        MediaItem.addTitle(builder, title);
    }
    if (thumbnailUrl !== 0) {
        MediaItem.addThumbnailUrl(builder, thumbnailUrl);
    }
    if (item.metadata !== null) {
        MediaItem.addMetadataType(builder, item.metadata.kind === 'video' ? Metadata.Video : Metadata.Audio);
        MediaItem.addMetadata(builder, metadata);
    }
    if (extraMetadata !== 0) {
        MediaItem.addExtraMetadata(builder, extraMetadata);
    }
    return MediaItem.endMediaItem(builder);
}

function createQueueItem(builder: flatbuffers.Builder, item: QueueItemData): flatbuffers.Offset {
    const mediaItem = createMediaItem(builder, item.mediaItem);
    QueueItem.startQueueItem(builder);
    QueueItem.addMediaItem(builder, mediaItem);
    if (item.playbackDuration !== null) {
        QueueItem.addPlaybackDuration(builder, createTime(builder, item.playbackDuration));
    }
    return QueueItem.endQueueItem(builder);
}

function createQueuePosition(builder: flatbuffers.Builder, position: QueuePositionData): { type: QueuePosition; offset: flatbuffers.Offset } {
    switch (position.kind) {
        case 'index':
            return { type: QueuePosition.Index, offset: QueueIndex.createQueueIndex(builder, position.index) };
        case 'front':
            QueueMarkerFront.startQueueMarkerFront(builder);
            return { type: QueuePosition.Front, offset: QueueMarkerFront.endQueueMarkerFront(builder) };
        default:
            QueueMarkerBack.startQueueMarkerBack(builder);
            return { type: QueuePosition.Back, offset: QueueMarkerBack.endQueueMarkerBack(builder) };
    }
}

// Encodes a Load without request headers, as relayed to senders.
export function encodeLoad(source: MediaSourceData): Uint8Array {
    const builder = new flatbuffers.Builder(1024);
    let load: flatbuffers.Offset;
    if (source.kind === 'single') {
        load = Load.createLoad(builder, MediaSource.Single, createMediaItem(builder, source.item));
    } else {
        const items = Queue.createItemsVector(builder, source.items.map((item) => createQueueItem(builder, item)));
        load = Load.createLoad(builder, MediaSource.Queue, Queue.createQueue(builder, items, source.startIndex, source.autoplay));
    }
    return finishPacket(builder, Message.Load, load);
}

export function encodeQueueInsert(item: QueueItemData, position: QueuePositionData): Uint8Array {
    const builder = new flatbuffers.Builder(1024);
    const queueItem = createQueueItem(builder, item);
    const encoded = createQueuePosition(builder, position);
    return finishPacket(builder, Message.QueueInsert, QueueInsert.createQueueInsert(builder, queueItem, encoded.type, encoded.offset));
}

export function encodeQueueRemove(position: QueuePositionData): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    const encoded = createQueuePosition(builder, position);
    return finishPacket(builder, Message.QueueRemove, QueueRemove.createQueueRemove(builder, encoded.type, encoded.offset));
}

export function encodeQueueItemSelected(position: QueuePositionData): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    const encoded = createQueuePosition(builder, position);
    return finishPacket(builder, Message.QueueItemSelected, QueueItemSelected.createQueueItemSelected(builder, encoded.type, encoded.offset));
}

export function encodeChangeTrack(id: number | null, trackType: MediaTrackType): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    return finishPacket(builder, Message.ChangeTrack, ChangeTrack.createChangeTrack(builder, id, trackType));
}

export function encodeTracksAvailable(tracks: MediaTrackData[]): Uint8Array {
    const builder = new flatbuffers.Builder(256);
    const offsets = tracks.map((track) => {
        const language = builder.createString(track.language);
        const title = track.title === null ? 0 : builder.createString(track.title);
        let metadataType: MediaTrackMetadata;
        let metadata: flatbuffers.Offset;
        if (track.trackType === MediaTrackType.Video) {
            metadataType = MediaTrackMetadata.Video;
            VideoTrackMeta.startVideoTrackMeta(builder);
            metadata = VideoTrackMeta.endVideoTrackMeta(builder);
        } else if (track.trackType === MediaTrackType.Audio) {
            metadataType = MediaTrackMetadata.Audio;
            AudioTrackMeta.startAudioTrackMeta(builder);
            metadata = AudioTrackMeta.endAudioTrackMeta(builder);
        } else {
            metadataType = MediaTrackMetadata.Subtitle;
            SubtitleTrackMeta.startSubtitleTrackMeta(builder);
            metadata = SubtitleTrackMeta.endSubtitleTrackMeta(builder);
        }
        return MediaTrack.createMediaTrack(builder, track.id, language, title, metadataType, metadata);
    });
    const vector = TracksAvailable.createTracksVector(builder, offsets);
    return finishPacket(builder, Message.TracksAvailable, TracksAvailable.createTracksAvailable(builder, vector));
}

export function encodeCompanionHelloResponse(providerId: number): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    return finishPacket(builder, Message.CompanionHelloResponse, CompanionHelloResponse.createCompanionHelloResponse(builder, providerId));
}

export function encodeCompanionResourceInfoRequest(requestId: number, resourceId: number): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    return finishPacket(builder, Message.CompanionResourceInfoRequest,
        CompanionResourceInfoRequest.createCompanionResourceInfoRequest(builder, requestId, resourceId));
}

// `start` and `stopInclusive` are byte offsets into the resource.
export function encodeCompanionResourceRequest(requestId: number, resourceId: number, start: number, stopInclusive: number): Uint8Array {
    const builder = new flatbuffers.Builder(64);
    CompanionResourceRequest.startCompanionResourceRequest(builder);
    CompanionResourceRequest.addRequestId(builder, requestId);
    CompanionResourceRequest.addResourceId(builder, resourceId);
    CompanionResourceRequest.addReadHead(builder, ResourceReadHead.createResourceReadHead(builder, BigInt(start), BigInt(stopInclusive)));
    return finishPacket(builder, Message.CompanionResourceRequest, CompanionResourceRequest.endCompanionResourceRequest(builder));
}
