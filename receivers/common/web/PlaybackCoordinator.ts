import {
    GenericMediaMetadata, Opcode, PlaybackErrorMessage, PlaybackState as LegacyState, PlaybackUpdateMessage, PlayMessage,
    PlayUpdateMessage, VolumeUpdateMessage,
} from 'common/Packets';
import { supportedImageTypes, supportedPlayerTypes } from 'common/MimeTypes';
import { MediaItemData, MediaSourceData, QueueItemData, QueuePositionData, SenderMessage } from 'common/v4/Decoder';
import {
    encodeChangeTrack, encodeCompanionHelloResponse, encodeError, encodeLoad, encodePlaybackStateChanged, encodeProgressChanged,
    encodeQueueInsert, encodeQueueItemSelected, encodeQueueRemove, encodeSpeedChanged, encodeStopPlayback, encodeTracksAvailable,
    encodeVolumeChanged, MediaTrackData,
} from 'common/v4/Encoder';
import { CompanionBroker, isCompanionUrl, parseCompanionUrl } from 'common/v4/Companion';
import { ErrorKind } from 'common/v4/generated/fcast/v4/error-kind';
import { MediaTrackType } from 'common/v4/generated/fcast/v4/media-track-type';
import { PlaybackState } from 'common/v4/generated/fcast/v4/playback-state';

export const MAX_QUEUE_ITEMS = 256;
export const MAX_EXTERNAL_SUBTITLES = 64;
export const EXTERNAL_TRACK_ID_BASE = 0x10000000;
export const DEFAULT_PROGRESS_INTERVAL_MS = 500;
export const MIN_PROGRESS_INTERVAL_MS = 100;
export const MIN_SPEED = 1 / 16;
export const MAX_SPEED = 16;
const PROGRESS_TICK_MS = 100;
const PARKED_SUBTITLE_TIMEOUT_MS = 20000;
const MICROS = 1000000;
const ALLOWED_SCHEMES = ['http:', 'https:', 'fcomp:'];

export interface TrackSelection {
    video: number | null;
    audio: number | null;
    subtitle: number | null;
}

export interface TracksReport {
    tracks: MediaTrackData[];
    selected: TrackSelection;
}

export interface PlaybackOptions {
    // Previous and next items exist and are selected through the service.
    queued: boolean;
    // Load the media again even when the player already has the same URL.
    reload: boolean;
}

export interface SubtitleTrack {
    id: number;
    name: string | null;
    vtt: string;
    select: boolean;
}

// Everything the coordinator needs from the service: the sender connections and the player.
export interface PlaybackHost {
    v4Sessions(): string[];
    legacySessions(): string[];
    sendV4(bytes: Uint8Array, sessionId: string): void;
    sendLegacy(opcode: number, message: any, sessionId: string): void;
    // Starts playing the message in the player.
    startPlayback(message: PlayMessage, options: PlaybackOptions): void;
    pause(): void;
    resume(): void;
    stop(): void;
    seek(time: number): void;
    setVolume(volume: number): void;
    setSpeed(speed: number): void;
    changeTrack(trackType: MediaTrackType, id: number | null): void;
    addSubtitle(track: SubtitleTrack): void;
    // Resolves with the subtitle as WebVTT text; rejects when it cannot be retrieved.
    fetchSubtitle(url: string): Promise<string>;
    // Resolves false when the media URL cannot be retrieved at all.
    isReachable(url: string, headers: { [key: string]: string } | null): Promise<boolean>;
}

interface QueueState {
    items: QueueItemData[];
    autoplay: boolean;
    index: number;
}

interface ParkedSubtitle {
    sessionId: string;
    packetNum: number;
    url: string;
    name: string | null;
    select: boolean;
    timer: any;
}

interface SenderProgress {
    intervalMs: number;
    nextAt: number;
}

function schemeOf(url: string): string | null {
    const match = /^([a-zA-Z][a-zA-Z0-9+.-]*:)/.exec(url);
    return match ? match[1].toLowerCase() : null;
}

function isAllowedUrl(url: string): boolean {
    const scheme = schemeOf(url);
    return scheme !== null && ALLOWED_SCHEMES.indexOf(scheme) !== -1;
}

function isImage(container: string): boolean {
    return container.toLowerCase().indexOf('image/') === 0;
}

function isSupportedContainer(container: string): boolean {
    const lower = container.toLowerCase();
    return supportedPlayerTypes.some((type) => type.toLowerCase() === lower) || supportedImageTypes.indexOf(lower) !== -1;
}

function withoutHeaders(message: PlayMessage): PlayMessage {
    const copy: PlayMessage = { ...message, headers: null };
    // A playlist carries its items, and their headers, as JSON text.
    if (copy.container === 'application/json' && typeof copy.content === 'string') {
        try {
            const playlist = JSON.parse(copy.content);
            if (playlist && Array.isArray(playlist.items)) {
                playlist.items = playlist.items.map((item: any) => ({ ...item, headers: null }));
                copy.content = JSON.stringify(playlist);
            }
        } catch (e) {
            // Not JSON: nothing to strip.
        }
    }
    return copy;
}

function playMessageFromItem(item: MediaItemData): PlayMessage {
    let headers: { [key: string]: string } | null = null;
    if (item.headers.length > 0) {
        headers = {};
        for (const header of item.headers) {
            headers[header.key] = header.value;
        }
    }
    const metadata = item.title !== null || item.thumbnailUrl !== null ? new GenericMediaMetadata(item.title, item.thumbnailUrl) : null;
    return new PlayMessage(
        item.container,
        item.sourceUrl,
        null,
        item.startTime !== null ? item.startTime / MICROS : null,
        item.volume,
        item.speed,
        headers,
        metadata,
    );
}

function itemFromPlayMessage(message: PlayMessage): MediaItemData {
    const metadata = message.metadata as GenericMediaMetadata | null;
    return {
        container: message.container,
        sourceUrl: message.url,
        startTime: typeof message.time === 'number' ? Math.round(message.time * MICROS) : null,
        volume: typeof message.volume === 'number' ? message.volume : null,
        speed: typeof message.speed === 'number' ? message.speed : null,
        headers: [],
        title: metadata && typeof metadata.title === 'string' ? metadata.title : null,
        thumbnailUrl: metadata && typeof metadata.thumbnailUrl === 'string' ? metadata.thumbnailUrl : null,
        metadata: null,
        extraMetadata: [],
    };
}

// Owns the playback state shared by every connected sender. It turns sender requests of any
// protocol version into player commands and reports what the player did back to each sender in
// that sender's protocol. It does no I/O of its own.
export class PlaybackCoordinator {
    // The player was started and has not been stopped since, whether or not its media loaded.
    private playerOpen = false;
    // Media is loaded or loading in the player.
    private mediaActive = false;
    // The current media as a v4 sender cast it; null when a legacy sender or the player started it.
    private v4Source: MediaSourceData | null = null;
    private queue: QueueState | null = null;
    private currentItem: PlayMessage | null = null;
    private legacyPlayMessage: PlayMessage | null = null;
    private loaderSession: string | null = null;
    private loaderPacket: number | null = null;

    private loading = false;
    // When the current item was handed to the player. The player runs on the same device, so
    // its reports carry the same clock.
    private itemStartedAt = 0;
    private pauseWhenLoaded = false;
    // The pause sent to the player when the load finished has not been confirmed yet.
    private awaitingPause = false;
    private pendingSeek: number | null = null;
    private seekPending = false;
    private state: PlaybackState = PlaybackState.Idle;
    private position = 0;
    private duration = 0;
    private positionAt = 0;
    private speed = 1;
    private volume = 1;
    private lastUpdate: PlaybackUpdateMessage | null = null;

    private tracks: TracksReport | null = null;
    private externalSubtitles = 0;
    private parkedSubtitles: ParkedSubtitle[] = [];
    private progress = new Map<string, SenderProgress>();
    private progressTimer: any = null;
    private durationTimer: any = null;
    // Bumped on every load so that late asynchronous results of an earlier item are dropped.
    private generation = 0;

    constructor(
        private readonly host: PlaybackHost,
        private readonly companion: CompanionBroker,
        private readonly now: () => number = Date.now,
    ) {}

    // ---- Sender connections ----

    // Sends the shared state to a v4 sender that has just been introduced to.
    onV4Secured(sessionId: string): void {
        this.progress.set(sessionId, { intervalMs: DEFAULT_PROGRESS_INTERVAL_MS, nextAt: this.now() + DEFAULT_PROGRESS_INTERVAL_MS });
        this.host.sendV4(encodeVolumeChanged(this.volume), sessionId);

        if (this.mediaActive) {
            const source = this.currentV4Source();
            if (source !== null) {
                this.host.sendV4(encodeLoad(source), sessionId);
            }
            if (this.reportsV4State()) {
                this.host.sendV4(encodePlaybackStateChanged(this.state), sessionId);
                if (!this.loading) {
                    this.host.sendV4(this.progressBytes(), sessionId);
                }
                if (this.speed !== 1) {
                    this.host.sendV4(encodeSpeedChanged(this.speed), sessionId);
                }
                if (this.tracks !== null && this.state !== PlaybackState.Ended) {
                    this.sendTracks([sessionId]);
                }
            }
        }
        this.updateProgressTimer();
    }

    onDisconnect(sessionId: string): void {
        this.progress.delete(sessionId);
        this.companion.removeSession(sessionId);
        if (this.loaderSession === sessionId) {
            this.loaderSession = null;
            this.loaderPacket = null;
        }
        this.parkedSubtitles = this.parkedSubtitles.filter((parked) => {
            if (parked.sessionId === sessionId) {
                clearTimeout(parked.timer);
                return false;
            }
            return true;
        });
        this.updateProgressTimer();
    }

    // ---- Legacy (v1 to v3) senders ----

    // Play data for the Initial message of a new legacy sender.
    initialPlayMessage(): PlayMessage | null {
        const message = this.legacyPlayMessage !== null ? this.legacyPlayMessage : this.currentItem;
        if (!this.mediaActive || message === null) {
            return null;
        }
        return { ...withoutHeaders(message), time: this.currentPosition(), volume: this.volume, speed: this.speed };
    }

    initialPlaybackUpdate(): PlaybackUpdateMessage | null {
        if (!this.mediaActive) {
            return null;
        }
        // Before the player's first report the media is still loading.
        return this.lastUpdate !== null ? this.lastUpdate : new PlaybackUpdateMessage(this.now(), LegacyState.Idle, 0, 0, this.speed);
    }

    playLegacy(sessionId: string, message: PlayMessage): void {
        if (typeof message.url === 'string' && message.url.length > 0 && !isAllowedUrl(message.url)) {
            this.host.sendLegacy(Opcode.PlaybackError, new PlaybackErrorMessage('Unsupported URL scheme'), sessionId);
            return;
        }

        // A legacy sender may repeat the Play of what is already playing; playback then goes on.
        const repeated = this.mediaActive && !this.loading && this.v4Source === null && this.state !== PlaybackState.Ended &&
            this.currentItem !== null && typeof message.url === 'string' && message.url === this.currentItem.url &&
            message.container === this.currentItem.container;
        if (repeated) {
            this.relayLegacyPlay(message, sessionId);
            return;
        }

        this.beginMedia(null, null, null);
        this.legacyPlayMessage = message;
        this.startMessage(message, sessionId, false, false);
    }

    // The player asks for an item of a playlist it manages itself.
    playFromPlayer(message: PlayMessage): void {
        if (!this.mediaActive || this.v4Source !== null) {
            return;
        }
        this.generation++;
        this.resetItemState();
        this.startMessage(message, null, false, true);
    }

    pauseLegacy(): void {
        this.requestPause();
    }

    resumeLegacy(): void {
        this.requestResume();
    }

    stopLegacy(): void {
        this.stopMedia();
    }

    seekLegacy(time: number): void {
        if (this.mediaActive && typeof time === 'number' && isFinite(time)) {
            this.applySeek(Math.max(0, time));
        }
    }

    setVolumeLegacy(volume: number): void {
        if (typeof volume === 'number' && isFinite(volume)) {
            this.applyVolume(Math.min(1, Math.max(0, volume)));
        }
    }

    setSpeedLegacy(speed: number): void {
        if (typeof speed === 'number' && isFinite(speed) && speed > 0) {
            this.applySpeed(Math.min(MAX_SPEED, Math.max(MIN_SPEED, speed)));
        }
    }

    // ---- v4 senders ----

    onV4Message(sessionId: string, packetNum: number, message: SenderMessage): void {
        switch (message.type) {
            case 'Load':
                this.loadV4(sessionId, packetNum, message.source);
                break;
            case 'StopPlayback':
                this.stopV4(sessionId);
                break;
            case 'ProgressChanged':
                this.seekV4(sessionId, packetNum, message.position);
                break;
            case 'VolumeChanged': {
                const volume = Math.min(1, Math.max(0, message.volume));
                if (volume !== message.volume) {
                    this.error(sessionId, ErrorKind.VolumeOutOfRange, packetNum);
                }
                this.applyVolume(volume);
                break;
            }
            case 'SpeedChanged': {
                const speed = message.speed > 0 ? Math.min(MAX_SPEED, Math.max(MIN_SPEED, message.speed)) : 1;
                if (speed !== message.speed) {
                    this.error(sessionId, ErrorKind.RateOutOfRange, packetNum);
                }
                this.applySpeed(speed);
                break;
            }
            case 'PlaybackStateChanged':
                if (message.state === PlaybackState.Paused) {
                    this.requestPause();
                } else if (message.state === PlaybackState.Playing) {
                    this.requestResume();
                } else if (message.state === PlaybackState.Idle || message.state === PlaybackState.Ended) {
                    this.stopV4(sessionId);
                }
                break;
            case 'SetProgressUpdateInterval': {
                if (message.interval === null) {
                    this.error(sessionId, ErrorKind.MalformedBody, packetNum);
                    break;
                }
                const intervalMs = Math.max(MIN_PROGRESS_INTERVAL_MS, Math.round(message.interval / 100000) * 100);
                this.progress.set(sessionId, { intervalMs, nextAt: this.now() + intervalMs });
                break;
            }
            case 'QueueInsert':
                this.queueInsert(sessionId, packetNum, message.item, message.position);
                break;
            case 'QueueRemove':
                this.queueRemove(sessionId, packetNum, message.position);
                break;
            case 'QueueItemSelected':
                this.queueSelect(sessionId, packetNum, message.position);
                break;
            case 'ChangeTrack':
                this.changeTrackV4(sessionId, packetNum, message.id, message.trackType);
                break;
            case 'AddSubtitleSource':
                this.addSubtitleV4(sessionId, packetNum, message.url, message.name, message.select);
                break;
            case 'CompanionHelloRequest': {
                const providerId = this.companion.registerProvider(sessionId);
                if (providerId === null) {
                    this.error(sessionId, ErrorKind.Internal, packetNum);
                } else {
                    this.host.sendV4(encodeCompanionHelloResponse(providerId), sessionId);
                }
                break;
            }
            case 'CompanionResourceInfoResponse':
                this.companion.onInfoResponse(sessionId, message.requestId, message.contentType, message.resourceSize);
                break;
            case 'StartMirroringSession':
            case 'MirroringSessionDescription':
                // Mirroring is not offered in the receiver capabilities.
                this.error(sessionId, ErrorKind.InvalidState, packetNum);
                break;
            default:
                break;
        }
    }

    // ---- Player reports ----

    onPlaybackUpdate(update: PlaybackUpdateMessage): void {
        // After a stop the player may go on reporting until its window is replaced; senders have
        // already been told that nothing is loaded.
        if (!this.mediaActive) {
            return;
        }
        // A report generated before the current item started describes the item it replaced.
        if (typeof update.generationTime === 'number' && update.generationTime < this.itemStartedAt) {
            return;
        }

        if (this.loading) {
            if (update.state !== LegacyState.Playing) {
                this.lastUpdate = update;
                this.sendLegacyToAll(Opcode.PlaybackUpdate, update);
                return;
            }

            this.loading = false;
            this.recordPosition(update);
            if (this.pendingSeek !== null) {
                const time = this.pendingSeek;
                this.pendingSeek = null;
                this.applySeek(time);
            }
            this.applyParkedSubtitles();
            if (this.pauseWhenLoaded) {
                // A pause asked for during the load takes effect now; senders never see Playing.
                this.pauseWhenLoaded = false;
                this.awaitingPause = true;
                this.host.pause();
                return;
            }
        } else {
            this.recordPosition(update);
            if (this.awaitingPause) {
                if (update.state !== LegacyState.Paused) {
                    return;
                }
                this.awaitingPause = false;
            }
        }

        this.lastUpdate = update;
        this.sendLegacyToAll(Opcode.PlaybackUpdate, update);
        if (update.state === LegacyState.Playing) {
            this.startDurationTimer();
        }

        if (!this.reportsV4State()) {
            return;
        }

        const state = update.state === LegacyState.Playing ? PlaybackState.Playing :
            update.state === LegacyState.Paused ? PlaybackState.Paused : this.state;
        const stateChanged = state !== this.state;
        if (stateChanged) {
            this.state = state;
            this.broadcastV4(() => encodePlaybackStateChanged(state));
        }
        if (this.currentItem !== null && isImage(this.currentItem.container)) {
            if (stateChanged && this.tracks === null) {
                this.tracks = {
                    tracks: [{ id: 0, trackType: MediaTrackType.Video, language: 'und', title: null }],
                    selected: { video: 0, audio: null, subtitle: null },
                };
                this.sendTracks(this.host.v4Sessions());
            }
            return;
        }

        if (this.seekPending || stateChanged) {
            this.seekPending = false;
            this.broadcastProgress();
        }
        if (typeof update.speed === 'number' && update.speed > 0 && Math.abs(update.speed - this.speed) > 0.001) {
            this.speed = update.speed;
            this.broadcastV4(() => encodeSpeedChanged(this.speed));
        }
        this.updateProgressTimer();
    }

    onVolumeUpdate(update: VolumeUpdateMessage): void {
        this.sendLegacyToAll(Opcode.VolumeUpdate, update);
        if (typeof update.volume === 'number' && Math.abs(update.volume - this.volume) > 0.001) {
            this.volume = Math.min(1, Math.max(0, update.volume));
            this.broadcastV4(() => encodeVolumeChanged(this.volume));
        }
    }

    // The player reached the end of the current item.
    onMediaEnded(): void {
        if (!this.mediaActive || this.loading) {
            return;
        }
        clearTimeout(this.durationTimer);
        this.durationTimer = null;

        if (this.queue !== null && this.queue.autoplay && this.queue.index + 1 < this.queue.items.length) {
            this.queue.index++;
            this.broadcastV4(() => encodeQueueItemSelected({ kind: 'index', index: this.queue.index }));
            this.startQueueItem();
            return;
        }

        if (this.duration > 0) {
            this.position = this.duration;
            this.positionAt = this.now();
        }
        if (this.reportsV4State() && this.state !== PlaybackState.Ended) {
            this.state = PlaybackState.Ended;
            this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Ended));
        }
        this.updateProgressTimer();
    }

    onPlaybackError(error: PlaybackErrorMessage): void {
        this.sendLegacyToAll(Opcode.PlaybackError, error);
        if (!this.mediaActive) {
            return;
        }

        const generation = this.generation;
        const wasLoading = this.loading;
        const item = this.currentItem;
        const sessionId = this.loaderSession;
        const packetNum = this.loaderPacket;
        const fail = (kind: ErrorKind) => {
            if (generation !== this.generation || !this.mediaActive) {
                return;
            }
            if (sessionId !== null) {
                this.error(sessionId, kind, packetNum);
            }
            if (!wasLoading) {
                // The player may recover from an error in the middle of the media, so the media
                // stays; until the player reports again, it is not playing.
                if (this.reportsV4State() && this.state !== PlaybackState.Buffering) {
                    this.position = this.currentPosition();
                    this.state = PlaybackState.Buffering;
                    this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Buffering));
                    this.updateProgressTimer();
                }
                return;
            }
            const parked = this.parkedSubtitles;
            this.clearMedia();
            this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Idle));
            for (const subtitle of parked) {
                this.error(subtitle.sessionId, ErrorKind.InvalidState, subtitle.packetNum);
            }
        };

        if (item === null || typeof item.url !== 'string') {
            fail(ErrorKind.Internal);
            return;
        }
        if (!wasLoading) {
            fail(ErrorKind.Internal);
            return;
        }
        this.host.isReachable(item.url, item.headers).then(
            (reachable) => fail(reachable ? ErrorKind.UnsupportedFormat : ErrorKind.ResourceNotFound),
            () => fail(ErrorKind.ResourceNotFound),
        );
    }

    onTracks(report: TracksReport): void {
        if (!this.mediaActive || !this.reportsV4State()) {
            return;
        }
        const listChanged = this.tracks === null || JSON.stringify(this.tracks.tracks) !== JSON.stringify(report.tracks);
        this.tracks = report;
        const sessions = this.host.v4Sessions();
        if (listChanged) {
            this.sendTracks(sessions);
        } else {
            this.sendSelection(sessions);
        }
    }

    // Previous or next asked for with the receiver's own controls.
    selectFromPlayer(offset: number): void {
        if (this.queue === null) {
            return;
        }
        const index = this.queue.index + offset;
        if (index < 0 || index >= this.queue.items.length) {
            return;
        }
        this.queue.index = index;
        this.broadcastV4(() => encodeQueueItemSelected({ kind: 'index', index }));
        this.startQueueItem();
    }

    // ---- v4 requests ----

    private loadV4(sessionId: string, packetNum: number, source: MediaSourceData): void {
        const items = source.kind === 'single' ? [source.item] : source.items.map((queued) => queued.mediaItem);
        if (items.length === 0 || items.length > MAX_QUEUE_ITEMS || items.some((item) => item.sourceUrl.length === 0)) {
            this.error(sessionId, ErrorKind.MalformedBody, packetNum);
            return;
        }
        const startIndex = source.kind === 'queue' && source.startIndex !== null ? source.startIndex : 0;
        if (startIndex >= items.length) {
            this.error(sessionId, ErrorKind.QueuePositionOutOfRange, packetNum);
            return;
        }
        for (const item of items) {
            const refusal = this.refusalOf(item);
            if (refusal !== null) {
                this.error(sessionId, refusal, packetNum);
                return;
            }
        }

        this.beginMedia(source, sessionId, packetNum);
        this.relayV4(() => encodeLoad(source), sessionId);
        if (source.kind === 'queue') {
            this.queue = { items: source.items.slice(), autoplay: source.autoplay, index: startIndex };
            this.startQueueItem();
        } else {
            this.startMessage(playMessageFromItem(source.item), null, false, true);
        }
    }

    private refusalOf(item: MediaItemData): ErrorKind | null {
        if (!isAllowedUrl(item.sourceUrl) || !isSupportedContainer(item.container)) {
            return ErrorKind.UnsupportedFormat;
        }
        if (isCompanionUrl(item.sourceUrl)) {
            const parsed = parseCompanionUrl(item.sourceUrl);
            if (parsed === null || !this.companion.hasProvider(parsed.providerId)) {
                return ErrorKind.ResourceNotFound;
            }
        }
        return null;
    }

    private stopV4(sessionId: string): void {
        this.stopMedia();
        this.relayV4(() => encodeStopPlayback(), sessionId);
    }

    private seekV4(sessionId: string, packetNum: number, position: number | null): void {
        if (position === null) {
            this.error(sessionId, ErrorKind.MalformedBody, packetNum);
            return;
        }
        if (!this.mediaActive || (this.currentItem !== null && isImage(this.currentItem.container))) {
            return;
        }

        let time = position / MICROS;
        if (this.duration > 0 && time > this.duration) {
            this.error(sessionId, ErrorKind.SeekOutOfRange, packetNum);
            time = this.duration - Math.min(0.25, this.duration / 2);
        }
        this.applySeek(time);
    }

    private resolvePosition(position: QueuePositionData, length: number, forInsert: boolean): number {
        switch (position.kind) {
            case 'index': return position.index;
            case 'front': return 0;
            default: return forInsert ? length : length - 1;
        }
    }

    private queueInsert(sessionId: string, packetNum: number, item: QueueItemData, position: QueuePositionData): void {
        if (this.queue === null) {
            this.error(sessionId, ErrorKind.InvalidState, packetNum);
            return;
        }
        if (this.queue.items.length >= MAX_QUEUE_ITEMS) {
            this.error(sessionId, ErrorKind.QueueFull, packetNum);
            return;
        }
        if (item.mediaItem.sourceUrl.length === 0) {
            this.error(sessionId, ErrorKind.MalformedBody, packetNum);
            return;
        }
        const refusal = this.refusalOf(item.mediaItem);
        if (refusal !== null) {
            this.error(sessionId, refusal, packetNum);
            return;
        }
        const index = this.resolvePosition(position, this.queue.items.length, true);
        if (index > this.queue.items.length) {
            this.error(sessionId, ErrorKind.QueuePositionOutOfRange, packetNum);
            return;
        }

        this.queue.items.splice(index, 0, item);
        if (index <= this.queue.index) {
            this.queue.index++;
        }
        this.relayV4(() => encodeQueueInsert(item, position), sessionId);
    }

    private queueRemove(sessionId: string, packetNum: number, position: QueuePositionData): void {
        if (this.queue === null) {
            this.error(sessionId, ErrorKind.InvalidState, packetNum);
            return;
        }
        const index = this.resolvePosition(position, this.queue.items.length, false);
        if (index >= this.queue.items.length) {
            this.error(sessionId, ErrorKind.QueuePositionOutOfRange, packetNum);
            return;
        }
        if (index === this.queue.index) {
            this.error(sessionId, ErrorKind.QueueRemovePlayingItem, packetNum);
            return;
        }

        this.queue.items.splice(index, 1);
        if (index < this.queue.index) {
            this.queue.index--;
        }
        this.relayV4(() => encodeQueueRemove(position), sessionId);
    }

    private queueSelect(sessionId: string, packetNum: number, position: QueuePositionData): void {
        if (this.queue === null) {
            this.error(sessionId, ErrorKind.InvalidState, packetNum);
            return;
        }
        const index = this.resolvePosition(position, this.queue.items.length, false);
        if (index >= this.queue.items.length) {
            this.error(sessionId, ErrorKind.QueuePositionOutOfRange, packetNum);
            return;
        }

        this.queue.index = index;
        this.loaderSession = sessionId;
        this.loaderPacket = packetNum;
        this.relayV4(() => encodeQueueItemSelected(position), sessionId);
        this.startQueueItem();
    }

    private changeTrackV4(sessionId: string, packetNum: number, id: number | null, trackType: MediaTrackType): void {
        if (!this.mediaActive || this.tracks === null) {
            this.error(sessionId, ErrorKind.MalformedBody, packetNum);
            return;
        }
        if (id !== null && !this.tracks.tracks.some((track) => track.id === id && track.trackType === trackType)) {
            this.error(sessionId, ErrorKind.MalformedBody, packetNum);
            return;
        }
        if (trackType === MediaTrackType.Subtitle && id !== null && this.tracks.selected.video === null &&
            this.tracks.tracks.some((track) => track.trackType === MediaTrackType.Video)) {
            this.error(sessionId, ErrorKind.InvalidState, packetNum);
            return;
        }
        this.host.changeTrack(trackType, id);
    }

    private addSubtitleV4(sessionId: string, packetNum: number, url: string, name: string | null, select: boolean): void {
        if (url.length === 0) {
            this.error(sessionId, ErrorKind.MalformedBody, packetNum);
            return;
        }
        if (!isAllowedUrl(url)) {
            this.error(sessionId, ErrorKind.UnsupportedFormat, packetNum);
            return;
        }
        if (isCompanionUrl(url)) {
            const parsed = parseCompanionUrl(url);
            if (parsed === null || !this.companion.hasProvider(parsed.providerId)) {
                this.error(sessionId, ErrorKind.ResourceNotFound, packetNum);
                return;
            }
        }
        if (!this.mediaActive || this.currentItem === null || isImage(this.currentItem.container) ||
            this.externalSubtitles + this.parkedSubtitles.length >= MAX_EXTERNAL_SUBTITLES) {
            this.error(sessionId, ErrorKind.InvalidState, packetNum);
            return;
        }

        if (this.loading) {
            const parked: ParkedSubtitle = { sessionId, packetNum, url, name, select, timer: null };
            parked.timer = setTimeout(() => {
                const index = this.parkedSubtitles.indexOf(parked);
                if (index !== -1) {
                    this.parkedSubtitles.splice(index, 1);
                    this.error(sessionId, ErrorKind.InvalidState, packetNum);
                }
            }, PARKED_SUBTITLE_TIMEOUT_MS);
            this.parkedSubtitles.push(parked);
            return;
        }
        this.fetchSubtitle(sessionId, packetNum, url, name, select);
    }

    private applyParkedSubtitles(): void {
        const parked = this.parkedSubtitles;
        this.parkedSubtitles = [];
        for (const subtitle of parked) {
            clearTimeout(subtitle.timer);
            this.fetchSubtitle(subtitle.sessionId, subtitle.packetNum, subtitle.url, subtitle.name, subtitle.select);
        }
    }

    private fetchSubtitle(sessionId: string, packetNum: number, url: string, name: string | null, select: boolean): void {
        const generation = this.generation;
        const id = EXTERNAL_TRACK_ID_BASE + this.externalSubtitles;
        this.externalSubtitles++;
        this.host.fetchSubtitle(url).then(
            (vtt) => {
                if (generation === this.generation && this.mediaActive) {
                    this.host.addSubtitle({ id, name, vtt, select });
                }
            },
            () => {
                if (generation === this.generation) {
                    this.error(sessionId, ErrorKind.ResourceNotFound, packetNum);
                }
            },
        );
    }

    // ---- Shared behaviour ----

    private requestPause(): void {
        if (!this.mediaActive) {
            return;
        }
        if (this.loading) {
            this.pauseWhenLoaded = true;
            if (this.reportsV4State()) {
                this.state = PlaybackState.Paused;
                this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Paused));
            }
            return;
        }
        if (this.state === PlaybackState.Paused) {
            this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Paused));
        }
        this.host.pause();
    }

    private requestResume(): void {
        if (!this.mediaActive) {
            return;
        }
        if (this.loading) {
            this.pauseWhenLoaded = false;
            return;
        }
        if (this.state === PlaybackState.Playing) {
            this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Playing));
        }
        this.host.resume();
    }

    private applySeek(time: number): void {
        if (this.loading) {
            this.pendingSeek = time;
            return;
        }
        this.position = time;
        this.positionAt = this.now();
        this.seekPending = true;
        this.host.seek(time);
    }

    private applyVolume(volume: number): void {
        this.volume = volume;
        this.host.setVolume(volume);
        this.broadcastV4(() => encodeVolumeChanged(volume));
    }

    private applySpeed(speed: number): void {
        this.position = this.currentPosition();
        this.positionAt = this.now();
        this.speed = speed;
        this.host.setSpeed(speed);
        this.broadcastV4(() => encodeSpeedChanged(speed));
    }

    private stopMedia(): void {
        if (this.playerOpen) {
            // Also closes a player left open by media that failed to load.
            this.playerOpen = false;
            this.host.stop();
        }
        if (!this.mediaActive) {
            return;
        }
        this.clearMedia();
        this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Idle));
        this.sendLegacyToAll(Opcode.PlaybackUpdate, new PlaybackUpdateMessage(this.now(), LegacyState.Idle, 0, 0, 0));
    }

    private beginMedia(source: MediaSourceData | null, sessionId: string | null, packetNum: number | null): void {
        this.generation++;
        this.mediaActive = true;
        this.v4Source = source;
        this.queue = null;
        this.legacyPlayMessage = null;
        this.loaderSession = sessionId;
        this.loaderPacket = packetNum;
        this.speed = 1;
        this.resetItemState();
    }

    private resetItemState(): void {
        this.loading = true;
        this.pauseWhenLoaded = false;
        this.awaitingPause = false;
        this.pendingSeek = null;
        this.seekPending = false;
        this.position = 0;
        this.duration = 0;
        this.positionAt = this.now();
        this.lastUpdate = null;
        this.tracks = null;
        this.externalSubtitles = 0;
        for (const parked of this.parkedSubtitles) {
            clearTimeout(parked.timer);
        }
        this.parkedSubtitles = [];
        clearTimeout(this.durationTimer);
        this.durationTimer = null;
    }

    private clearMedia(): void {
        this.generation++;
        this.resetItemState();
        this.loading = false;
        this.mediaActive = false;
        this.v4Source = null;
        this.queue = null;
        this.currentItem = null;
        this.legacyPlayMessage = null;
        this.loaderSession = null;
        this.loaderPacket = null;
        this.state = PlaybackState.Idle;
        this.speed = 1;
        this.updateProgressTimer();
    }

    private startQueueItem(): void {
        this.generation++;
        this.resetItemState();
        this.startMessage(playMessageFromItem(this.queue.items[this.queue.index].mediaItem), null, true, true);
    }

    private relayLegacyPlay(message: PlayMessage, legacyOrigin: string | null): void {
        const relayed = new PlayUpdateMessage(this.now(), withoutHeaders(message));
        for (const sessionId of this.host.legacySessions()) {
            // The sender that asked gets its own message back unchanged.
            this.host.sendLegacy(Opcode.PlayUpdate, sessionId === legacyOrigin ? new PlayUpdateMessage(relayed.generationTime, message) : relayed, sessionId);
        }
    }

    // Starts the item in the player and tells the senders that did not ask for it.
    private startMessage(message: PlayMessage, legacyOrigin: string | null, queued: boolean, reload: boolean): void {
        this.currentItem = message;
        if (typeof message.speed === 'number' && message.speed > 0) {
            this.speed = message.speed;
        }

        this.relayLegacyPlay(message, legacyOrigin);
        if (this.v4Source === null && message.container !== 'application/json' && typeof message.url === 'string') {
            this.broadcastV4(() => encodeLoad({ kind: 'single', item: itemFromPlayMessage(message) }));
        }
        if (this.reportsV4State()) {
            this.state = PlaybackState.Buffering;
            this.broadcastV4(() => encodePlaybackStateChanged(PlaybackState.Buffering));
        }
        this.playerOpen = true;
        this.itemStartedAt = this.now();
        this.host.startPlayback(message, { queued, reload });
        this.updateProgressTimer();
    }

    // A still image has no playback state in v4; an animated one plays like a video.
    private reportsV4State(): boolean {
        if (this.currentItem === null) {
            return false;
        }
        const container = this.currentItem.container.toLowerCase();
        return !isImage(container) || container === 'image/gif' || container === 'image/apng';
    }

    // The media as a v4 sender would have to load it now to reach the current state.
    private currentV4Source(): MediaSourceData | null {
        if (this.v4Source === null) {
            return null;
        }
        if (this.queue !== null) {
            return { kind: 'queue', items: this.queue.items, startIndex: this.queue.index, autoplay: this.queue.autoplay };
        }
        return this.v4Source;
    }

    private recordPosition(update: PlaybackUpdateMessage): void {
        if (typeof update.time === 'number' && isFinite(update.time)) {
            this.position = update.time;
            this.positionAt = this.now();
        }
        if (typeof update.duration === 'number' && isFinite(update.duration) && update.duration > 0) {
            this.duration = update.duration;
        }
    }

    private currentPosition(): number {
        if (this.state !== PlaybackState.Playing) {
            return this.position;
        }
        const position = this.position + ((this.now() - this.positionAt) / 1000) * this.speed;
        return this.duration > 0 ? Math.min(position, this.duration) : position;
    }

    private progressBytes(): Uint8Array {
        return encodeProgressChanged(Math.max(0, this.currentPosition()) * MICROS, this.duration > 0 ? this.duration * MICROS : null);
    }

    private broadcastProgress(): void {
        const sessions = this.host.v4Sessions();
        if (sessions.length === 0) {
            return;
        }
        const bytes = this.progressBytes();
        const now = this.now();
        for (const sessionId of sessions) {
            this.host.sendV4(bytes, sessionId);
            const progress = this.progress.get(sessionId);
            if (progress) {
                progress.nextAt = now + progress.intervalMs;
            }
        }
    }

    private updateProgressTimer(): void {
        const wanted = this.mediaActive && this.state === PlaybackState.Playing && this.progress.size > 0;
        if (wanted && this.progressTimer === null) {
            this.progressTimer = setInterval(() => this.progressTick(), PROGRESS_TICK_MS);
        } else if (!wanted && this.progressTimer !== null) {
            clearInterval(this.progressTimer);
            this.progressTimer = null;
        }
    }

    private progressTick(): void {
        const now = this.now();
        let bytes: Uint8Array | null = null;
        for (const sessionId of this.host.v4Sessions()) {
            const progress = this.progress.get(sessionId);
            if (!progress || now < progress.nextAt) {
                continue;
            }
            if (bytes === null) {
                bytes = this.progressBytes();
            }
            this.host.sendV4(bytes, sessionId);
            progress.nextAt = Math.max(progress.nextAt + progress.intervalMs, now);
        }
    }

    // A queue item with a playback duration ends after being shown for that long.
    private startDurationTimer(): void {
        if (this.queue === null || this.durationTimer !== null) {
            return;
        }
        const duration = this.queue.items[this.queue.index].playbackDuration;
        if (duration !== null && duration > 0) {
            this.durationTimer = setTimeout(() => {
                this.durationTimer = null;
                this.onMediaEnded();
            }, duration / 1000);
        }
    }

    private sendTracks(sessions: string[]): void {
        if (sessions.length === 0) {
            return;
        }
        const bytes = encodeTracksAvailable(this.tracks.tracks);
        for (const sessionId of sessions) {
            this.host.sendV4(bytes, sessionId);
        }
        this.sendSelection(sessions);
    }

    private sendSelection(sessions: string[]): void {
        if (sessions.length === 0) {
            return;
        }
        const selected = this.tracks.selected;
        const messages = [
            encodeChangeTrack(selected.video, MediaTrackType.Video),
            encodeChangeTrack(selected.audio, MediaTrackType.Audio),
            encodeChangeTrack(selected.subtitle, MediaTrackType.Subtitle),
        ];
        for (const sessionId of sessions) {
            for (const bytes of messages) {
                this.host.sendV4(bytes, sessionId);
            }
        }
    }

    private error(sessionId: string, kind: ErrorKind, packetNum: number | null): void {
        this.host.sendV4(encodeError(kind, packetNum), sessionId);
    }

    // A v4 message is built only when a v4 sender will receive it. A runtime without v4 (webOS 5
    // and 6) never has one, and lacks what the encoder needs.
    private broadcastV4(encode: () => Uint8Array): void {
        const sessions = this.host.v4Sessions();
        if (sessions.length === 0) {
            return;
        }
        const bytes = encode();
        for (const sessionId of sessions) {
            this.host.sendV4(bytes, sessionId);
        }
    }

    private relayV4(encode: () => Uint8Array, exceptSessionId: string): void {
        const sessions = this.host.v4Sessions().filter((sessionId) => sessionId !== exceptSessionId);
        if (sessions.length === 0) {
            return;
        }
        const bytes = encode();
        for (const sessionId of sessions) {
            this.host.sendV4(bytes, sessionId);
        }
    }

    private sendLegacyToAll(opcode: number, message: any): void {
        for (const sessionId of this.host.legacySessions()) {
            this.host.sendLegacy(opcode, message, sessionId);
        }
    }
}
