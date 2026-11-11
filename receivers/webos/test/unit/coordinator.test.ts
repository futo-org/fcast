import * as assert from 'assert';
import * as flatbuffers from 'flatbuffers';
import { test } from './runner';
import * as build from './builders';
import { Opcode, PlaybackState as LegacyState, PlaybackUpdateMessage, PlayMessage } from 'common/Packets';
import { EXTERNAL_TRACK_ID_BASE, PlaybackCoordinator, PlaybackHost, SubtitleTrack } from 'common/PlaybackCoordinator';
import { CompanionBroker } from 'common/v4/Companion';
import { decodePacket } from 'common/v4/Decoder';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { Load } from 'common/v4/generated/fcast/v4/load';
import { MediaSource } from 'common/v4/generated/fcast/v4/media-source';
import { MediaItem } from 'common/v4/generated/fcast/v4/media-item';
import { Queue } from 'common/v4/generated/fcast/v4/queue';
import { QueueItem } from 'common/v4/generated/fcast/v4/queue-item';
import { QueueIndex } from 'common/v4/generated/fcast/v4/queue-index';
import { QueuePosition } from 'common/v4/generated/fcast/v4/queue-position';
import { QueueItemSelected } from 'common/v4/generated/fcast/v4/queue-item-selected';
import { Error as ErrorMessage } from 'common/v4/generated/fcast/v4/error';
import { ErrorKind } from 'common/v4/generated/fcast/v4/error-kind';
import { PlaybackState } from 'common/v4/generated/fcast/v4/playback-state';
import { PlaybackStateChanged } from 'common/v4/generated/fcast/v4/playback-state-changed';
import { VolumeChanged } from 'common/v4/generated/fcast/v4/volume-changed';
import { SpeedChanged } from 'common/v4/generated/fcast/v4/speed-changed';
import { ProgressChanged } from 'common/v4/generated/fcast/v4/progress-changed';
import { ChangeTrack } from 'common/v4/generated/fcast/v4/change-track';
import { TracksAvailable } from 'common/v4/generated/fcast/v4/tracks-available';
import { CompanionHelloResponse } from 'common/v4/generated/fcast/v4/companion-hello-response';
import { MediaTrackType } from 'common/v4/generated/fcast/v4/media-track-type';

const MP4 = 'https://media.example/video.mp4';

function readPacket(bytes: Uint8Array): Packet {
    return Packet.getRootAsPacket(new flatbuffers.ByteBuffer(bytes));
}

// A short text form of a receiver packet, enough to assert on kind, order and key values.
function describe(bytes: Uint8Array): string {
    const packet = readPacket(bytes);
    switch (packet.payloadType()) {
        case Message.Load: {
            const load = packet.payload(new Load()) as Load;
            return load.sourceType() === MediaSource.Queue ? 'Load:queue' : 'Load:single';
        }
        case Message.Error: {
            const error = packet.payload(new ErrorMessage()) as ErrorMessage;
            return 'Error:' + ErrorKind[error.kind()] + ':' + error.packetNum();
        }
        case Message.PlaybackStateChanged:
            return 'State:' + PlaybackState[(packet.payload(new PlaybackStateChanged()) as PlaybackStateChanged).state()];
        case Message.VolumeChanged:
            return 'Volume:' + (packet.payload(new VolumeChanged()) as VolumeChanged).volume();
        case Message.SpeedChanged:
            return 'Speed:' + (packet.payload(new SpeedChanged()) as SpeedChanged).speed();
        case Message.ProgressChanged: {
            const progress = packet.payload(new ProgressChanged()) as ProgressChanged;
            return 'Progress:' + Number(progress.position().micros()) / 1000000;
        }
        case Message.QueueItemSelected: {
            const selected = packet.payload(new QueueItemSelected()) as QueueItemSelected;
            if (selected.positionType() === QueuePosition.Index) {
                return 'Selected:' + (selected.position(new QueueIndex()) as QueueIndex).index();
            }
            return 'Selected:' + QueuePosition[selected.positionType()];
        }
        case Message.ChangeTrack: {
            const change = packet.payload(new ChangeTrack()) as ChangeTrack;
            return 'Track:' + MediaTrackType[change.trackType()] + ':' + change.id();
        }
        case Message.TracksAvailable:
            return 'Tracks:' + (packet.payload(new TracksAvailable()) as TracksAvailable).tracksLength();
        case Message.CompanionHelloResponse:
            return 'Hello:' + (packet.payload(new CompanionHelloResponse()) as CompanionHelloResponse).providerId();
        default:
            return Message[packet.payloadType()];
    }
}

interface Rig {
    coordinator: PlaybackCoordinator;
    companion: CompanionBroker;
    v4: string[];
    legacy: string[];
    // What each sender received, in order: described v4 packets, or `opcode:json` for legacy ones.
    sent: { [sessionId: string]: string[] };
    raw: { [sessionId: string]: Uint8Array[] };
    player: string[];
    played: PlayMessage[];
    subtitles: SubtitleTrack[];
    reachable: boolean;
    subtitleText: string | null;
    clock: { now: number };
    connectV4(sessionId: string): void;
    send(sessionId: string, packetNum: number, bytes: Uint8Array): void;
    update(state: LegacyState, time: number, duration: number, speed?: number): void;
    clear(): void;
}

function rig(): Rig {
    const r: any = {
        v4: [], legacy: [], sent: {}, raw: {}, player: [], played: [], subtitles: [],
        reachable: true, subtitleText: 'WEBVTT\n', clock: { now: 1000 },
    };
    const record = (sessionId: string, text: string) => {
        (r.sent[sessionId] = r.sent[sessionId] || []).push(text);
    };
    const host: PlaybackHost = {
        v4Sessions: () => r.v4.slice(),
        legacySessions: () => r.legacy.slice(),
        sendV4: (bytes, sessionId) => {
            record(sessionId, describe(bytes));
            (r.raw[sessionId] = r.raw[sessionId] || []).push(bytes);
        },
        sendLegacy: (opcode, message, sessionId) => record(sessionId, opcode + ':' + JSON.stringify(message)),
        startPlayback: (message, options) => {
            r.played.push(message);
            r.player.push('play:' + message.url + (options.queued ? ':queued' : '') + (options.reload ? '' : ':keep'));
        },
        pause: () => r.player.push('pause'),
        resume: () => r.player.push('resume'),
        stop: () => r.player.push('stop'),
        seek: (time) => r.player.push('seek:' + time),
        setVolume: (volume) => r.player.push('volume:' + volume),
        setSpeed: (speed) => r.player.push('speed:' + speed),
        changeTrack: (trackType, id) => r.player.push('track:' + MediaTrackType[trackType] + ':' + id),
        addSubtitle: (track) => {
            r.subtitles.push(track);
            r.player.push('subtitle:' + track.id);
        },
        fetchSubtitle: () => r.subtitleText === null ? Promise.reject(new Error('missing')) : Promise.resolve(r.subtitleText),
        isReachable: () => Promise.resolve(r.reachable),
    };
    r.companion = new CompanionBroker((sessionId, bytes) => host.sendV4(bytes, sessionId));
    r.coordinator = new PlaybackCoordinator(host, r.companion, () => r.clock.now);
    r.connectV4 = (sessionId: string) => {
        r.v4.push(sessionId);
        r.coordinator.onV4Secured(sessionId);
    };
    r.send = (sessionId: string, packetNum: number, bytes: Uint8Array) => {
        r.coordinator.onV4Message(sessionId, packetNum, decodePacket(bytes));
    };
    r.update = (state: LegacyState, time: number, duration: number, speed: number = 1) => {
        r.coordinator.onPlaybackUpdate(new PlaybackUpdateMessage(r.clock.now, state, time, duration, speed));
    };
    r.clear = () => {
        r.sent = {};
        r.raw = {};
        r.player = [];
    };
    return r as Rig;
}

function settle(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
}

function ofKind(list: string[] | undefined, prefix: string): string[] {
    return (list || []).filter((entry) => entry.indexOf(prefix) === 0);
}

// Two v4 senders with a video loaded by A and playing.
function playing(): Rig {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.send('A', 5, build.loadSingle({ sourceUrl: MP4 }));
    r.update(LegacyState.Playing, 0, 100);
    r.clear();
    return r;
}

test('coordinator: a v4 Load is relayed without headers to the other senders only', () => {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.legacy.push('L');
    r.clear();

    r.send('A', 2, build.loadSingle({ sourceUrl: MP4, title: 'Title', headers: [{ key: 'Authorization', value: 'secret' }] }));

    assert.deepStrictEqual(r.sent['A'], ['State:Buffering']);
    assert.deepStrictEqual(r.sent['B'], ['Load:single', 'State:Buffering']);
    const relayed = readPacket(r.raw['B'][0]).payload(new Load()).source(new MediaItem()) as MediaItem;
    assert.strictEqual(relayed.headersLength(), 0);
    assert.strictEqual(relayed.title(), 'Title');
    assert.strictEqual(relayed.sourceUrl(), MP4);

    assert.strictEqual(r.sent['L'].length, 1);
    assert.ok(r.sent['L'][0].indexOf(Opcode.PlayUpdate + ':') === 0);
    assert.ok(r.sent['L'][0].indexOf('secret') === -1, 'legacy sender received the request headers');
    assert.ok(r.sent['L'][0].indexOf(MP4) !== -1);

    // The player still gets the headers: it needs them to fetch the media.
    assert.deepStrictEqual(r.played[0].headers, { Authorization: 'secret' });
});

test('coordinator: a legacy Play is echoed unchanged to its sender and without headers to the others', () => {
    const r = rig();
    r.connectV4('A');
    r.legacy.push('L1', 'L2');
    r.clear();

    r.coordinator.playLegacy('L1', new PlayMessage('video/mp4', MP4, null, null, null, null, { Cookie: 'secret' }));

    assert.ok(r.sent['L1'][0].indexOf('secret') !== -1);
    assert.ok(r.sent['L2'][0].indexOf('secret') === -1);
    assert.ok(r.sent['L2'][0].indexOf(MP4) !== -1);
    assert.strictEqual(r.sent['A'][0], 'Load:single');
    const relayed = readPacket(r.raw['A'][0]).payload(new Load()).source(new MediaItem()) as MediaItem;
    assert.strictEqual(relayed.headersLength(), 0);
    assert.strictEqual(r.coordinator.initialPlayMessage().headers, null);
});

test('coordinator: a report the player made before the new item started is ignored', () => {
    const r = playing();
    r.legacy.push('L');
    r.clock.now = 5000;
    r.send('A', 6, build.loadSingle({ sourceUrl: MP4 + '?next' }));
    r.clear();

    // The old item's last report arrives late: the new item is not playing yet.
    r.coordinator.onPlaybackUpdate(new PlaybackUpdateMessage(4990, LegacyState.Playing, 50, 100, 1));
    assert.deepStrictEqual(r.sent, {});

    r.clock.now = 5200;
    r.update(LegacyState.Playing, 0, 30);
    assert.deepStrictEqual(r.sent['A'], ['State:Playing', 'Progress:0']);
    assert.strictEqual(r.sent['L'].length, 1);
});

test('coordinator: a refused Load reports the error and leaves the current media playing', () => {
    const r = playing();

    r.send('A', 7, build.loadSingle({ sourceUrl: 'file:///etc/passwd' }));
    r.send('A', 8, build.loadSingle({ sourceUrl: MP4, container: 'video/x-unknown' }));
    r.send('A', 9, build.loadQueue([{ mediaItem: { sourceUrl: MP4 } }], 1, false));
    r.send('A', 10, build.loadSingle({ sourceUrl: 'fcomp://7.fcast/1' }));

    assert.deepStrictEqual(r.sent['A'], [
        'Error:UnsupportedFormat:7', 'Error:UnsupportedFormat:8', 'Error:QueuePositionOutOfRange:9', 'Error:ResourceNotFound:10',
    ]);
    assert.strictEqual(r.sent['B'], undefined);
    assert.deepStrictEqual(r.player, []);
});

test('coordinator: an out of range volume is reported before the clamped value is broadcast', () => {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.clear();

    r.send('A', 3, build.volumeChanged(1.5));
    assert.deepStrictEqual(r.sent['A'], ['Error:VolumeOutOfRange:3', 'Volume:1']);
    assert.deepStrictEqual(r.sent['B'], ['Volume:1']);
    assert.deepStrictEqual(r.player, ['volume:1']);

    r.clear();
    r.send('B', 4, build.volumeChanged(0.5));
    assert.deepStrictEqual(r.sent['A'], ['Volume:0.5']);
    assert.deepStrictEqual(r.sent['B'], ['Volume:0.5']);
});

test('coordinator: speed is clamped to 1/16..16 and echoed to every sender', () => {
    const r = playing();

    r.send('A', 6, build.speedChanged(64));
    assert.deepStrictEqual(r.sent['A'], ['Error:RateOutOfRange:6', 'Speed:16']);
    assert.deepStrictEqual(r.sent['B'], ['Speed:16']);

    r.clear();
    r.send('A', 7, build.speedChanged(0));
    assert.deepStrictEqual(r.sent['A'], ['Error:RateOutOfRange:7', 'Speed:1']);

    r.clear();
    r.send('A', 8, build.speedChanged(1.5));
    assert.deepStrictEqual(r.sent['A'], ['Speed:1.5']);
    assert.deepStrictEqual(r.player, ['speed:1.5']);
});

test('coordinator: a pause during the load takes effect when the media starts and Playing is never reported', () => {
    const r = rig();
    r.connectV4('A');
    r.legacy.push('L');
    r.send('A', 2, build.loadSingle({ sourceUrl: MP4 }));
    r.clear();

    r.send('A', 3, build.playbackStateChanged(PlaybackState.Paused));
    assert.deepStrictEqual(r.player, []);
    assert.deepStrictEqual(r.sent['A'], ['State:Paused']);

    r.update(LegacyState.Playing, 0, 100);
    assert.deepStrictEqual(r.player, ['pause']);
    // The player reports Playing once more before the pause takes effect.
    r.update(LegacyState.Playing, 0, 100);
    r.update(LegacyState.Paused, 0, 100);

    assert.deepStrictEqual(ofKind(r.sent['A'], 'State:'), ['State:Paused']);
    assert.ok(!r.sent['L'].some((entry) => entry.indexOf('"state":' + LegacyState.Playing) !== -1), 'legacy sender saw Playing');
    assert.ok(r.sent['L'].some((entry) => entry.indexOf('"state":' + LegacyState.Paused) !== -1));
});

test('coordinator: a seek past the end is reported, clamped and followed by the new progress', () => {
    const r = playing();

    r.send('A', 6, build.progressChanged(500 * 1000000, null));
    assert.deepStrictEqual(r.sent['A'], ['Error:SeekOutOfRange:6']);
    assert.deepStrictEqual(r.player, ['seek:99.75']);

    r.clear();
    r.update(LegacyState.Playing, 99.75, 100);
    assert.deepStrictEqual(r.sent['A'], ['Progress:99.75']);
    assert.deepStrictEqual(r.sent['B'], ['Progress:99.75']);

    r.clear();
    r.send('A', 7, build.progressChanged(null, null));
    assert.deepStrictEqual(r.sent['A'], ['Error:MalformedBody:7']);
    assert.deepStrictEqual(r.player, []);
});

test('coordinator: a seek while paused reports the new position', () => {
    const r = playing();
    r.update(LegacyState.Paused, 10, 100);
    r.clear();

    r.send('A', 6, build.progressChanged(30 * 1000000, null));
    r.update(LegacyState.Paused, 30, 100);
    assert.deepStrictEqual(r.sent['A'], ['Progress:30']);
});

test('coordinator: stop reports Idle to everyone, then StopPlayback to the other senders', () => {
    const r = playing();
    r.legacy.push('L');

    r.send('A', 6, build.stopPlayback());
    assert.deepStrictEqual(r.sent['A'], ['State:Idle']);
    assert.deepStrictEqual(r.sent['B'], ['State:Idle', 'StopPlayback']);
    assert.deepStrictEqual(r.player, ['stop']);
    assert.ok(r.sent['L'][0].indexOf(Opcode.PlaybackUpdate + ':') === 0 && r.sent['L'][0].indexOf('"state":' + LegacyState.Idle) !== -1);
    assert.strictEqual(r.coordinator.initialPlayMessage(), null);

    // A report from the player that is still closing does not reach the senders.
    r.clear();
    r.update(LegacyState.Playing, 3, 100);
    assert.deepStrictEqual(r.sent, {});

    // Nothing is loaded any more: only the relay remains.
    r.send('A', 7, build.stopPlayback());
    assert.strictEqual(r.sent['A'], undefined);
    assert.deepStrictEqual(r.sent['B'], ['StopPlayback']);
});

test('coordinator: queue operations validate positions and are relayed to the other senders', () => {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.clear();

    r.send('A', 1, build.queueItemSelected({ kind: 'index', index: 0 }));
    r.send('A', 2, build.queueInsert({ mediaItem: { sourceUrl: MP4 } }, { kind: 'back' }));
    r.send('A', 3, build.queueRemove({ kind: 'front' }));
    assert.deepStrictEqual(r.sent['A'], ['Error:InvalidState:1', 'Error:InvalidState:2', 'Error:InvalidState:3']);

    r.clear();
    r.send('A', 4, build.loadQueue([{ mediaItem: { sourceUrl: MP4 + '?0' } }], null, false));
    r.update(LegacyState.Playing, 0, 100);
    r.clear();

    // On a queue of one item every position names the playing item.
    r.send('A', 5, build.queueRemove({ kind: 'index', index: 0 }));
    r.send('A', 6, build.queueRemove({ kind: 'front' }));
    r.send('A', 7, build.queueRemove({ kind: 'back' }));
    r.send('A', 8, build.queueRemove({ kind: 'index', index: 1 }));
    r.send('A', 9, build.queueItemSelected({ kind: 'index', index: 1 }));
    r.send('A', 10, build.queueInsert({ mediaItem: { sourceUrl: MP4 } }, { kind: 'index', index: 2 }));
    assert.deepStrictEqual(r.sent['A'], [
        'Error:QueueRemovePlayingItem:5', 'Error:QueueRemovePlayingItem:6', 'Error:QueueRemovePlayingItem:7',
        'Error:QueuePositionOutOfRange:8', 'Error:QueuePositionOutOfRange:9', 'Error:QueuePositionOutOfRange:10',
    ]);
    assert.strictEqual(r.sent['B'], undefined);

    r.clear();
    r.send('A', 11, build.queueInsert({ mediaItem: { sourceUrl: MP4 + '?front', headers: [{ key: 'K', value: 'secret' }] } }, { kind: 'front' }));
    r.send('A', 12, build.queueInsert({ mediaItem: { sourceUrl: MP4 + '?back' } }, { kind: 'back' }));
    assert.strictEqual(r.sent['A'], undefined);
    assert.deepStrictEqual(r.sent['B'], ['QueueInsert', 'QueueInsert']);

    // The playing item moved to index 1: index 0 can go, index 1 cannot.
    r.clear();
    r.send('A', 13, build.queueRemove({ kind: 'index', index: 1 }));
    r.send('A', 14, build.queueRemove({ kind: 'index', index: 0 }));
    assert.deepStrictEqual(r.sent['A'], ['Error:QueueRemovePlayingItem:13']);
    assert.deepStrictEqual(r.sent['B'], ['QueueRemove']);

    r.clear();
    r.send('A', 15, build.queueItemSelected({ kind: 'back' }));
    assert.deepStrictEqual(r.played[r.played.length - 1].url, MP4 + '?back');
    assert.deepStrictEqual(r.sent['B'], ['Selected:Back', 'State:Buffering']);
    assert.deepStrictEqual(r.sent['A'], ['State:Buffering']);
});

test('coordinator: a full queue refuses another item', () => {
    const r = rig();
    r.connectV4('A');
    const items = [];
    for (let i = 0; i < 256; i++) {
        items.push({ mediaItem: { sourceUrl: MP4 + '?' + i } });
    }
    r.send('A', 1, build.loadQueue(items, 0, false));
    r.clear();

    r.send('A', 2, build.queueInsert({ mediaItem: { sourceUrl: MP4 } }, { kind: 'back' }));
    assert.deepStrictEqual(r.sent['A'], ['Error:QueueFull:2']);
});

test('coordinator: an autoplay queue advances at the end of an item and ends after the last', () => {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.send('A', 1, build.loadQueue([{ mediaItem: { sourceUrl: MP4 + '?0' } }, { mediaItem: { sourceUrl: MP4 + '?1' } }], 0, true));
    r.update(LegacyState.Playing, 0, 10);
    r.clear();

    r.coordinator.onMediaEnded();
    assert.deepStrictEqual(r.player, ['play:' + MP4 + '?1:queued']);
    assert.deepStrictEqual(r.sent['A'], ['Selected:1', 'State:Buffering']);
    assert.deepStrictEqual(r.sent['B'], ['Selected:1', 'State:Buffering']);

    r.update(LegacyState.Playing, 0, 10);
    r.clear();
    r.coordinator.onMediaEnded();
    assert.deepStrictEqual(r.player, []);
    assert.deepStrictEqual(r.sent['A'], ['State:Ended']);
});

test('coordinator: without autoplay the queue stops at the end of the item', () => {
    const r = rig();
    r.connectV4('A');
    r.send('A', 1, build.loadQueue([{ mediaItem: { sourceUrl: MP4 + '?0' } }, { mediaItem: { sourceUrl: MP4 + '?1' } }], 0, false));
    r.update(LegacyState.Playing, 0, 10);
    r.clear();

    r.coordinator.onMediaEnded();
    assert.deepStrictEqual(r.player, []);
    assert.deepStrictEqual(r.sent['A'], ['State:Ended']);
});

test('coordinator: previous and next from the receiver move within the queue and tell the senders', () => {
    const r = rig();
    r.connectV4('A');
    r.send('A', 1, build.loadQueue([{ mediaItem: { sourceUrl: MP4 + '?0' } }, { mediaItem: { sourceUrl: MP4 + '?1' } }], 1, false));
    r.update(LegacyState.Playing, 0, 10);
    r.clear();

    r.coordinator.selectFromPlayer(1);
    assert.deepStrictEqual(r.player, []);
    r.coordinator.selectFromPlayer(-1);
    assert.deepStrictEqual(r.player, ['play:' + MP4 + '?0:queued']);
    assert.deepStrictEqual(r.sent['A'], ['Selected:0', 'State:Buffering']);
});

test('coordinator: a sender that joins later receives the current queue, state and progress in order', () => {
    const r = rig();
    r.connectV4('A');
    r.send('A', 1, build.loadQueue([
        { mediaItem: { sourceUrl: MP4 + '?0', headers: [{ key: 'K', value: 'secret' }] } },
        { mediaItem: { sourceUrl: MP4 + '?1' } },
    ], 0, false));
    r.send('A', 2, build.queueItemSelected({ kind: 'index', index: 1 }));
    r.update(LegacyState.Playing, 0, 100);
    r.update(LegacyState.Paused, 12, 100);
    r.coordinator.onTracks({
        tracks: [{ id: 0, trackType: MediaTrackType.Video, language: 'und', title: null }, { id: 1, trackType: MediaTrackType.Audio, language: 'und', title: null }],
        selected: { video: 0, audio: 1, subtitle: null },
    });

    r.connectV4('B');
    assert.deepStrictEqual(r.sent['B'], [
        'Volume:1', 'Load:queue', 'State:Paused', 'Progress:12', 'Tracks:2', 'Track:Video:0', 'Track:Audio:1', 'Track:Subtitle:null',
    ]);
    const queue = readPacket(r.raw['B'][1]).payload(new Load()).source(new Queue()) as Queue;
    assert.strictEqual(queue.itemsLength(), 2);
    assert.strictEqual(queue.startIndex(), 1);
    assert.strictEqual((queue.items(0, new QueueItem()) as QueueItem).mediaItem(new MediaItem()).headersLength(), 0);
});

test('coordinator: a load failure goes to the loading sender, then Idle to all, then the parked subtitle is refused', async () => {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.send('A', 4, build.loadSingle({ sourceUrl: MP4 }));
    r.send('B', 9, build.addSubtitleSource('https://media.example/sub.vtt', true, null, false));
    r.clear();
    r.reachable = false;

    r.coordinator.onPlaybackError({ message: 'code=4' });
    await settle();

    assert.deepStrictEqual(r.sent['A'], ['Error:ResourceNotFound:4', 'State:Idle']);
    assert.deepStrictEqual(r.sent['B'], ['State:Idle', 'Error:InvalidState:9']);
    assert.deepStrictEqual(r.subtitles, []);
});

test('coordinator: media that is reachable but fails to load is an unsupported format', async () => {
    const r = rig();
    r.connectV4('A');
    r.send('A', 4, build.loadSingle({ sourceUrl: MP4 }));
    r.clear();

    r.coordinator.onPlaybackError({ message: 'code=4' });
    await settle();
    assert.deepStrictEqual(r.sent['A'], ['Error:UnsupportedFormat:4', 'State:Idle']);

    // The player that could not load is still open; a stop closes it without another Idle.
    r.clear();
    r.send('A', 5, build.stopPlayback());
    assert.deepStrictEqual(r.player, ['stop']);
    assert.strictEqual(r.sent['A'], undefined);
});

test('coordinator: an error in the middle of the media keeps it and reports buffering until the player recovers', async () => {
    const r = playing();
    r.reachable = false;

    r.coordinator.onPlaybackError({ message: 'decode' });
    await settle();
    assert.deepStrictEqual(r.sent['A'], ['Error:Internal:5', 'State:Buffering']);
    assert.deepStrictEqual(r.sent['B'], ['State:Buffering']);

    r.clear();
    r.update(LegacyState.Playing, 20, 100);
    assert.deepStrictEqual(r.sent['B'], ['State:Playing', 'Progress:20']);
});

test('coordinator: a legacy sender repeating the Play of the current media does not restart it', () => {
    const r = rig();
    r.legacy.push('L');
    const message = new PlayMessage('video/mp4', MP4);
    r.coordinator.playLegacy('L', message);
    assert.deepStrictEqual(r.player, ['play:' + MP4 + ':keep']);
    r.update(LegacyState.Playing, 5, 100);
    r.clear();

    r.coordinator.playLegacy('L', message);
    assert.deepStrictEqual(r.player, []);
    assert.ok(r.sent['L'][0].indexOf(Opcode.PlayUpdate + ':') === 0);
    assert.strictEqual(r.coordinator.initialPlaybackUpdate().time, 5);

    // Other media replaces it.
    r.coordinator.playLegacy('L', new PlayMessage('video/mp4', MP4 + '?other'));
    assert.deepStrictEqual(r.player, ['play:' + MP4 + '?other:keep']);
});

test('coordinator: tracks reported by the player are announced with the selection of each kind', () => {
    const r = playing();
    const tracks = [
        { id: 0, trackType: MediaTrackType.Video, language: 'und', title: null },
        { id: 1, trackType: MediaTrackType.Audio, language: 'en', title: null },
    ];

    r.coordinator.onTracks({ tracks, selected: { video: 0, audio: 1, subtitle: null } });
    assert.deepStrictEqual(r.sent['A'], ['Tracks:2', 'Track:Video:0', 'Track:Audio:1', 'Track:Subtitle:null']);
    assert.deepStrictEqual(r.sent['B'], r.sent['A']);

    // The same list with another selection only repeats the selection.
    r.clear();
    r.coordinator.onTracks({ tracks, selected: { video: null, audio: 1, subtitle: null } });
    assert.deepStrictEqual(r.sent['A'], ['Track:Video:null', 'Track:Audio:1', 'Track:Subtitle:null']);
});

test('coordinator: a track change must name a known track of its kind', async () => {
    const r = playing();
    r.send('A', 6, build.addSubtitleSource('https://media.example/sub.vtt', false, 'English', false));
    await settle();
    const external = EXTERNAL_TRACK_ID_BASE;
    r.coordinator.onTracks({
        tracks: [
            { id: 0, trackType: MediaTrackType.Video, language: 'und', title: null },
            { id: 1, trackType: MediaTrackType.Audio, language: 'und', title: null },
            { id: external, trackType: MediaTrackType.Subtitle, language: 'und', title: 'English' },
        ],
        selected: { video: null, audio: 1, subtitle: null },
    });
    r.clear();

    r.send('A', 7, build.changeTrack(99, MediaTrackType.Audio));
    r.send('A', 8, build.changeTrack(0, MediaTrackType.Audio));
    r.send('A', 9, build.changeTrack(external, MediaTrackType.Subtitle));
    assert.deepStrictEqual(r.sent['A'], ['Error:MalformedBody:7', 'Error:MalformedBody:8', 'Error:InvalidState:9']);
    assert.deepStrictEqual(r.player, []);

    r.send('A', 10, build.changeTrack(0, MediaTrackType.Video));
    r.send('A', 11, build.changeTrack(null, MediaTrackType.Audio));
    assert.deepStrictEqual(r.player, ['track:Video:0', 'track:Audio:null']);
});

test('coordinator: an external subtitle is fetched, numbered and handed to the player', async () => {
    const r = playing();

    r.send('A', 6, build.addSubtitleSource('https://media.example/a.vtt', true, 'English', false));
    r.send('A', 7, build.addSubtitleSource('https://media.example/b.vtt', false, null, false));
    await settle();

    assert.deepStrictEqual(r.subtitles, [
        { id: EXTERNAL_TRACK_ID_BASE, name: 'English', vtt: 'WEBVTT\n', select: true },
        { id: EXTERNAL_TRACK_ID_BASE + 1, name: null, vtt: 'WEBVTT\n', select: false },
    ]);
    assert.strictEqual(r.sent['A'], undefined);
});

test('coordinator: a subtitle is refused without media, with an empty or local URL, or when it cannot be fetched', async () => {
    const r = rig();
    r.connectV4('A');
    r.clear();

    r.send('A', 1, build.addSubtitleSource('https://media.example/a.vtt', true, null, false));
    r.send('A', 2, build.addSubtitleSource('', true, null, false));
    r.send('A', 3, build.addSubtitleSource('file:///a.vtt', true, null, false));
    assert.deepStrictEqual(r.sent['A'], ['Error:InvalidState:1', 'Error:MalformedBody:2', 'Error:UnsupportedFormat:3']);

    r.send('A', 4, build.loadSingle({ sourceUrl: MP4 }));
    r.update(LegacyState.Playing, 0, 100);
    r.clear();
    r.subtitleText = null;
    r.send('A', 5, build.addSubtitleSource('https://media.example/missing.vtt', true, null, false));
    await settle();
    assert.deepStrictEqual(r.sent['A'], ['Error:ResourceNotFound:5']);
    assert.deepStrictEqual(r.subtitles, []);
});

test('coordinator: a subtitle added during the load is applied once the media plays', async () => {
    const r = rig();
    r.connectV4('A');
    r.send('A', 1, build.loadSingle({ sourceUrl: MP4 }));
    r.send('A', 2, build.addSubtitleSource('https://media.example/a.vtt', true, null, false));
    await settle();
    assert.deepStrictEqual(r.subtitles, []);

    r.update(LegacyState.Playing, 0, 100);
    await settle();
    assert.strictEqual(r.subtitles.length, 1);
    assert.strictEqual(r.subtitles[0].id, EXTERNAL_TRACK_ID_BASE);
});

test('coordinator: each sender gets progress at its own interval while playing', async () => {
    const r = rig();
    const realNow = Date.now;
    const coordinator = r.coordinator as any;
    coordinator.now = realNow;
    r.connectV4('A');
    r.connectV4('B');
    r.send('A', 1, build.loadSingle({ sourceUrl: MP4 }));
    r.send('A', 2, build.setProgressUpdateInterval(0));
    // This test runs on the real clock, so the player's reports are stamped with it too.
    r.clock.now = realNow() + 1;
    r.update(LegacyState.Playing, 0, 100);
    r.clear();

    await new Promise((resolve) => setTimeout(resolve, 1050));
    r.clock.now = realNow();
    r.update(LegacyState.Paused, 1, 100);
    const fast = ofKind(r.sent['A'], 'Progress:').length;
    const slow = ofKind(r.sent['B'], 'Progress:').length;
    r.send('A', 3, build.stopPlayback());

    // A asked for 0, which means the 100 ms minimum; B keeps the 500 ms default.
    assert.ok(fast >= 7 && fast <= 12, 'sender at 100 ms got ' + fast + ' progress messages in about one second');
    assert.ok(slow >= 2 && slow <= 4, 'sender at 500 ms got ' + slow + ' progress messages in about one second');
});

test('coordinator: an interval without a value is malformed', () => {
    const r = rig();
    r.connectV4('A');
    r.clear();
    r.send('A', 3, build.setProgressUpdateInterval(null));
    assert.deepStrictEqual(r.sent['A'], ['Error:MalformedBody:3']);
});

test('coordinator: a still image has no v4 playback state, an animated one plays as a video track', () => {
    const r = rig();
    r.connectV4('A');
    r.clear();

    r.send('A', 1, build.loadSingle({ sourceUrl: 'https://media.example/a.jpg', container: 'image/jpeg' }));
    r.update(LegacyState.Playing, 0, 0);
    assert.strictEqual(r.sent['A'], undefined);

    r.send('A', 2, build.loadSingle({ sourceUrl: 'https://media.example/a.gif', container: 'image/gif' }));
    r.update(LegacyState.Playing, 0, 0);
    assert.deepStrictEqual(r.sent['A'], [
        'State:Buffering', 'State:Playing', 'Tracks:1', 'Track:Video:0', 'Track:Audio:null', 'Track:Subtitle:null',
    ]);
});

test('coordinator: with legacy senders only, nothing of v4 is built (webOS 5 and 6 runtime)', async () => {
    // Node 8.12 has neither TextEncoder nor BigInt, which the v4 encoder needs.
    const scope = global as any;
    const saved = { TextEncoder: scope.TextEncoder, BigInt: scope.BigInt };
    scope.TextEncoder = undefined;
    scope.BigInt = undefined;
    try {
        const r = rig();
        r.legacy.push('L');
        r.coordinator.playLegacy('L', new PlayMessage('video/mp4', MP4, null, 3, 0.5, 1.5, { Cookie: 'c' }));
        r.update(LegacyState.Idle, 0, 0);
        r.update(LegacyState.Playing, 3, 100, 1.5);
        r.coordinator.onTracks({
            tracks: [{ id: 0, trackType: MediaTrackType.Video, language: 'und', title: null }],
            selected: { video: 0, audio: null, subtitle: null },
        });
        r.coordinator.onVolumeUpdate({ generationTime: 1000, volume: 0.5 });
        r.coordinator.pauseLegacy();
        r.update(LegacyState.Paused, 4, 100, 1.5);
        r.coordinator.resumeLegacy();
        r.coordinator.seekLegacy(50);
        r.update(LegacyState.Playing, 50, 100, 1.5);
        r.coordinator.setVolumeLegacy(0.2);
        r.coordinator.setSpeedLegacy(2);
        r.coordinator.onMediaEnded();
        r.coordinator.onPlaybackError({ message: 'late error' });
        await settle();
        r.coordinator.playLegacy('L', new PlayMessage('image/gif', 'https://media.example/a.gif'));
        r.update(LegacyState.Playing, 0, 0);
        r.coordinator.stopLegacy();

        assert.deepStrictEqual(r.player, [
            'play:' + MP4 + ':keep', 'pause', 'resume', 'seek:50', 'volume:0.2', 'speed:2',
            'play:https://media.example/a.gif:keep', 'stop',
        ]);
        assert.ok(r.sent['L'].length > 0);
        assert.deepStrictEqual(Object.keys(r.sent), ['L']);
    } finally {
        scope.TextEncoder = saved.TextEncoder;
        scope.BigInt = saved.BigInt;
    }
});

test('coordinator: each connection gets one provider id and mirroring is refused', () => {
    const r = rig();
    r.connectV4('A');
    r.connectV4('B');
    r.clear();

    r.send('A', 1, build.companionHelloRequest());
    r.send('A', 2, build.companionHelloRequest());
    r.send('B', 1, build.companionHelloRequest());
    r.send('B', 2, build.startMirroringSession(1));
    assert.deepStrictEqual(r.sent['A'], ['Hello:1', 'Hello:1']);
    assert.deepStrictEqual(r.sent['B'], ['Hello:2', 'Error:InvalidState:2']);

    // A companion URL of a registered provider is accepted, also from the other connection.
    r.clear();
    r.send('B', 3, build.loadSingle({ sourceUrl: 'fcomp://1.fcast/5' }));
    assert.deepStrictEqual(r.player, ['play:fcomp://1.fcast/5']);

    // When the provider disconnects its id is gone.
    r.v4 = ['B'];
    r.coordinator.onDisconnect('A');
    r.clear();
    r.send('B', 4, build.loadSingle({ sourceUrl: 'fcomp://1.fcast/5' }));
    assert.deepStrictEqual(r.sent['B'], ['Error:ResourceNotFound:4']);
});
