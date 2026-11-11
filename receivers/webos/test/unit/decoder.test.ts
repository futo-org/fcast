import * as assert from 'assert';
import * as flatbuffers from 'flatbuffers';
import { test } from './runner';
import * as b from './builders';
import { decodePacket } from 'common/v4/Decoder';
import type { MediaItemData, QueueItemData, SenderMessage } from 'common/v4/Decoder';
import { DecodeError } from 'common/v4/CheckedByteBuffer';
import { encodeError, encodeReceiverIntroduction } from 'common/v4/Encoder';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Load } from 'common/v4/generated/fcast/v4/load';
import { MediaSource } from 'common/v4/generated/fcast/v4/media-source';
import { MediaItem } from 'common/v4/generated/fcast/v4/media-item';
import { PlaybackState } from 'common/v4/generated/fcast/v4/playback-state';
import { MediaTrackType } from 'common/v4/generated/fcast/v4/media-track-type';
import { StopPlayback } from 'common/v4/generated/fcast/v4/stop-playback';
import { VolumeChanged } from 'common/v4/generated/fcast/v4/volume-changed';
import { QueueRemove } from 'common/v4/generated/fcast/v4/queue-remove';
import { AddSubtitleSource } from 'common/v4/generated/fcast/v4/add-subtitle-source';

function mediaItem(overrides: Partial<MediaItemData> = {}): MediaItemData {
    return Object.assign({
        container: 'video/mp4',
        sourceUrl: 'https://example.com/media.mp4',
        startTime: null,
        volume: null,
        speed: null,
        headers: [],
        title: null,
        thumbnailUrl: null,
        metadata: null,
        extraMetadata: [],
    }, overrides);
}

function queueItem(item: MediaItemData, playbackDuration: number | null = null): QueueItemData {
    return { mediaItem: item, playbackDuration };
}

function expectRejected(bytes: Uint8Array, messagePart?: string): void {
    let error: any = null;
    try {
        decodePacket(bytes);
    } catch (e) {
        error = e;
    }
    if (!(error instanceof DecodeError)) {
        throw new Error('expected DecodeError, got ' + error);
    }
    // A 'malformed:' error means an unexpected exception was caught by the wrapper, i.e. a missing explicit check.
    assert.ok(!error.message.startsWith('malformed: '), 'missing explicit check: ' + error.message);
    if (messagePart !== undefined) {
        assert.ok(error.message.indexOf(messagePart) !== -1, 'expected "' + messagePart + '" in: ' + error.message);
    }
}

function mulberry32(seed: number): () => number {
    let state = seed >>> 0;
    return () => {
        state = (state + 0x6d2b79f5) >>> 0;
        let t = state;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}

const CAPS = {
    media: {
        protocols: ['http', 'https'],
        containers: ['mp4', 'hls'],
        videoFormats: ['h264'],
        audioFormats: ['aac'],
        subtitleFormats: ['vtt'],
        hdrFormats: [],
        imageFormats: ['png'],
        externalSubtitles: true,
        mirroring: false,
    },
    display: { width: 1920, height: 1080 },
    volumeStepInterval: 0.5,
};

test('round trip: ProgressChanged with position and duration', () => {
    assert.deepStrictEqual(decodePacket(b.progressChanged(12000000, 90000000)), {
        type: 'ProgressChanged', position: 12000000, duration: 90000000,
    });
});

test('round trip: ProgressChanged null optional fields stay null', () => {
    assert.deepStrictEqual(decodePacket(b.progressChanged(5000000, null)), {
        type: 'ProgressChanged', position: 5000000, duration: null,
    });
    assert.deepStrictEqual(decodePacket(b.progressChanged(null, null)), {
        type: 'ProgressChanged', position: null, duration: null,
    });
});

test('round trip: VolumeChanged', () => {
    assert.deepStrictEqual(decodePacket(b.volumeChanged(0.75)), { type: 'VolumeChanged', volume: 0.75 });
});

test('round trip: PlaybackStateChanged', () => {
    assert.deepStrictEqual(decodePacket(b.playbackStateChanged(PlaybackState.Paused)), {
        type: 'PlaybackStateChanged', state: PlaybackState.Paused,
    });
});

test('round trip: SpeedChanged', () => {
    assert.deepStrictEqual(decodePacket(b.speedChanged(1.5)), { type: 'SpeedChanged', speed: 1.5 });
});

test('round trip: SenderIntroduction', () => {
    assert.deepStrictEqual(decodePacket(b.senderIntroduction('Living Room', 'FCast Sender', '4.1.0')), {
        type: 'SenderIntroduction',
        deviceInfo: { displayName: 'Living Room', appName: 'FCast Sender', appVersion: '4.1.0' },
    });
});

test('round trip: Load single with headers, title, thumbnail and video chapters', () => {
    assert.deepStrictEqual(decodePacket(b.loadSingle(b.METADATA_FIXTURE)), {
        type: 'Load',
        source: {
            kind: 'single',
            item: mediaItem({
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
                        { start: 60000000, length: null, title: 'Main', skipLabel: null },
                    ],
                },
                extraMetadata: [
                    { key: 'tags', value: { kind: 'list', value: [{ kind: 'string', value: 'a' }, null, { kind: 'int', value: 42 }] } },
                    { key: 'rating', value: { kind: 'float', value: 2.5 } },
                ],
            }),
        },
    });
});

test('round trip: Load single with audio metadata', () => {
    const withArtist = decodePacket(b.loadSingle({
        metadata: { kind: 'audio', artist: 'Artist', album: 'Album', chapters: [{ start: 0, title: 'Track 1' }] },
    }));
    assert.deepStrictEqual(withArtist, {
        type: 'Load',
        source: {
            kind: 'single',
            item: mediaItem({
                metadata: {
                    kind: 'audio',
                    artist: 'Artist',
                    album: 'Album',
                    chapters: [{ start: 0, length: null, title: 'Track 1', skipLabel: null }],
                },
            }),
        },
    });

    const withoutTags = decodePacket(b.loadSingle({ metadata: { kind: 'audio', chapters: [] } }));
    assert.deepStrictEqual(withoutTags, {
        type: 'Load',
        source: {
            kind: 'single',
            item: mediaItem({ metadata: { kind: 'audio', artist: null, album: null, chapters: [] } }),
        },
    });
});

test('round trip: Load single with nested extra metadata', () => {
    const decoded = decodePacket(b.loadSingle({
        extraMetadata: [
            {
                key: 'outer',
                value: {
                    kind: 'kv',
                    key: 'inner',
                    value: {
                        kind: 'list',
                        items: [{ kind: 'kv', key: 'deep', value: { kind: 'int', value: -7 } }, { kind: 'string', value: null }],
                    },
                },
            },
            { key: 'empty', value: null },
        ],
    }));
    assert.deepStrictEqual(decoded, {
        type: 'Load',
        source: {
            kind: 'single',
            item: mediaItem({
                extraMetadata: [
                    {
                        key: 'outer',
                        value: {
                            kind: 'kv',
                            key: 'inner',
                            value: {
                                kind: 'list',
                                value: [
                                    { kind: 'kv', key: 'deep', value: { kind: 'int', value: -7 } },
                                    { kind: 'string', value: null },
                                ],
                            },
                        },
                    },
                    { key: 'empty', value: null },
                ],
            }),
        },
    });
});

test('round trip: Load single with only the required fields keeps optional scalars null', () => {
    assert.deepStrictEqual(decodePacket(b.loadSingle({})), {
        type: 'Load',
        source: { kind: 'single', item: mediaItem() },
    });
});

test('round trip: Load queue with three items, start index and autoplay', () => {
    assert.deepStrictEqual(decodePacket(b.loadQueue(b.QUEUE_FIXTURE, 1, true)), {
        type: 'Load',
        source: {
            kind: 'queue',
            items: [
                queueItem(mediaItem({
                    sourceUrl: 'https://example.com/1.mp4',
                    title: 'One',
                    headers: [{ key: 'a', value: 'b' }],
                }), 1000000),
                queueItem(mediaItem({ sourceUrl: 'https://example.com/2.mp4', title: 'Two' }), null),
                queueItem(mediaItem({ sourceUrl: 'https://example.com/3.mp4', title: 'Three' }), null),
            ],
            startIndex: 1,
            autoplay: true,
        },
    });
});

test('round trip: Load queue without start index keeps it null', () => {
    const decoded = decodePacket(b.loadQueue(b.QUEUE_FIXTURE, null, false));
    assert.deepStrictEqual(decoded.type === 'Load' && decoded.source.kind === 'queue' ? [decoded.source.startIndex, decoded.source.autoplay] : null, [null, false]);
});

test('round trip: StopPlayback', () => {
    assert.deepStrictEqual(decodePacket(b.stopPlayback()), { type: 'StopPlayback' });
});

test('round trip: StartMirroringSession and MirroringSessionDescription', () => {
    assert.deepStrictEqual(decodePacket(b.startMirroringSession(7)), { type: 'StartMirroringSession', sessionId: 7 });
    assert.deepStrictEqual(decodePacket(b.mirroringSessionDescription(7, 'v=0\r\no=- 1 2 IN IP4 127.0.0.1')), {
        type: 'MirroringSessionDescription', sessionId: 7, sdp: 'v=0\r\no=- 1 2 IN IP4 127.0.0.1',
    });
});

test('round trip: QueueInsert with each QueuePosition kind', () => {
    const item = queueItem(mediaItem({ sourceUrl: 'https://example.com/insert.mp4', title: 'Inserted' }));
    assert.deepStrictEqual(decodePacket(b.queueInsert(b.INSERT_FIXTURE.item, { kind: 'index', index: 2 })), {
        type: 'QueueInsert', item: queueItem(mediaItem({ sourceUrl: 'https://example.com/insert.mp4', title: 'Inserted' })), position: { kind: 'index', index: 2 },
    });
    assert.deepStrictEqual(decodePacket(b.queueInsert(b.INSERT_FIXTURE.item, { kind: 'front' })), {
        type: 'QueueInsert', item, position: { kind: 'front' },
    });
    assert.deepStrictEqual(decodePacket(b.queueInsert(b.INSERT_FIXTURE.item, { kind: 'back' })), {
        type: 'QueueInsert', item, position: { kind: 'back' },
    });
});

test('round trip: QueueRemove and QueueItemSelected with each QueuePosition kind', () => {
    assert.deepStrictEqual(decodePacket(b.queueRemove({ kind: 'index', index: 0 })), {
        type: 'QueueRemove', position: { kind: 'index', index: 0 },
    });
    assert.deepStrictEqual(decodePacket(b.queueRemove({ kind: 'front' })), { type: 'QueueRemove', position: { kind: 'front' } });
    assert.deepStrictEqual(decodePacket(b.queueRemove({ kind: 'back' })), { type: 'QueueRemove', position: { kind: 'back' } });
    assert.deepStrictEqual(decodePacket(b.queueItemSelected({ kind: 'index', index: 5 })), {
        type: 'QueueItemSelected', position: { kind: 'index', index: 5 },
    });
    assert.deepStrictEqual(decodePacket(b.queueItemSelected({ kind: 'back' })), { type: 'QueueItemSelected', position: { kind: 'back' } });
});

test('round trip: ChangeTrack with and without an id', () => {
    assert.deepStrictEqual(decodePacket(b.changeTrack(3, MediaTrackType.Audio)), {
        type: 'ChangeTrack', id: 3, trackType: MediaTrackType.Audio,
    });
    assert.deepStrictEqual(decodePacket(b.changeTrack(null, MediaTrackType.Subtitle)), {
        type: 'ChangeTrack', id: null, trackType: MediaTrackType.Subtitle,
    });
});

test('round trip: AddSubtitleSource with and without name and metadata', () => {
    assert.deepStrictEqual(decodePacket(b.addSubtitleSource('https://example.com/en.vtt', true, 'English', true)), {
        type: 'AddSubtitleSource', url: 'https://example.com/en.vtt', select: true, name: 'English', metadata: {},
    });
    assert.deepStrictEqual(decodePacket(b.addSubtitleSource('https://example.com/fr.vtt', false, null, false)), {
        type: 'AddSubtitleSource', url: 'https://example.com/fr.vtt', select: false, name: null, metadata: null,
    });
});

test('round trip: SetProgressUpdateInterval with and without an interval', () => {
    assert.deepStrictEqual(decodePacket(b.setProgressUpdateInterval(500000)), {
        type: 'SetProgressUpdateInterval', interval: 500000,
    });
    assert.deepStrictEqual(decodePacket(b.setProgressUpdateInterval(null)), {
        type: 'SetProgressUpdateInterval', interval: null,
    });
});

test('round trip: CompanionHelloRequest', () => {
    assert.deepStrictEqual(decodePacket(b.companionHelloRequest()), { type: 'CompanionHelloRequest' });
});

test('round trip: CompanionResourceInfoResponse with known, unknown and absent size', () => {
    assert.deepStrictEqual(decodePacket(b.companionResourceInfoResponse(3, 'video/mp4', { kind: 'known', size: 123456 })), {
        type: 'CompanionResourceInfoResponse', requestId: 3, contentType: 'video/mp4', resourceSize: { kind: 'known', size: 123456 },
    });
    assert.deepStrictEqual(decodePacket(b.companionResourceInfoResponse(4, 'text/plain', { kind: 'unknown' })), {
        type: 'CompanionResourceInfoResponse', requestId: 4, contentType: 'text/plain', resourceSize: { kind: 'unknown' },
    });
    assert.deepStrictEqual(decodePacket(b.companionResourceInfoResponse(5, 'text/plain', null)), {
        type: 'CompanionResourceInfoResponse', requestId: 5, contentType: 'text/plain', resourceSize: null,
    });
});

test('wrong direction: receiver-only payloads are not traversed', () => {
    const expected = (name: string): SenderMessage => ({ type: 'WrongDirection', payload: name });
    assert.deepStrictEqual(decodePacket(encodeReceiverIntroduction({ displayName: 'Receiver', appName: 'FCast', appVersion: '4' }, CAPS)), expected('ReceiverIntroduction'));
    assert.deepStrictEqual(decodePacket(encodeError(6, 3)), expected('Error'));
    assert.deepStrictEqual(decodePacket(b.companionHelloResponse(1)), expected('CompanionHelloResponse'));
    assert.deepStrictEqual(decodePacket(b.tracksAvailable()), expected('TracksAvailable'));
});

test('malicious: root offset 0x7fffffff', () => {
    const bytes = b.loadSingle(b.METADATA_FIXTURE).slice();
    bytes.set([0xff, 0xff, 0xff, 0x7f], 0);
    expectRejected(bytes);
});

test('malicious: Load truncated by 20 bytes', () => {
    const bytes = b.loadSingle(b.METADATA_FIXTURE);
    expectRejected(bytes.slice(0, bytes.length - 20));
});

test('malicious: buffer that points a vector length at 2^31-1', () => {
    expectRejected(new Uint8Array([4, 0, 0, 0, 255, 255, 255, 127]));
});

test('malicious: unknown union tag 200', () => {
    const bytes = b.packetBytes(200, (builder) => Load.createLoad(builder, MediaSource.Single, b.createMediaItem(builder, {})));
    expectRejected(bytes, 'invalid payload type 200');
});

test('malicious: NONE union tag', () => {
    expectRejected(b.packetBytes(Message.NONE, () => 0), 'invalid payload type 0');
});

test('malicious: Load missing required container', () => {
    expectRejected(b.loadSingle({ container: null }, { skipRequiredChecks: true }), 'missing required container');
});

test('malicious: generic meta list nested 50 deep', () => {
    expectRejected(b.loadSingle({ extraMetadata: [{ key: 'deep', value: b.nestedList(50) }] }), 'depth limit');
});

test('malicious: garbage bytes', () => {
    expectRejected(new Uint8Array([0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]));
});

test('malicious: two bytes', () => {
    expectRejected(new Uint8Array([1, 2]), 'too short');
});

test('malicious: empty buffer', () => {
    expectRejected(new Uint8Array(0), 'too short');
});

test('malicious: PlaybackStateChanged with state 200', () => {
    expectRejected(b.playbackStateChanged(200 as PlaybackState), 'invalid playback state 200');
});

test('malicious: VolumeChanged with NaN', () => {
    expectRejected(b.volumeChanged(NaN), 'non-finite');
});

function viewOf(bytes: Uint8Array): DataView {
    return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

function rootPacket(bytes: Uint8Array): Packet {
    return Packet.getRootAsPacket(new flatbuffers.ByteBuffer(bytes));
}

// Absolute position of the vtable slot `slot` of the table at `table`, which must have that field.
function fieldPosition(bb: flatbuffers.ByteBuffer, table: number, slot: number): number {
    const offset = bb.__offset(table, slot);
    if (offset === 0) {
        throw new Error('field missing at vtable slot ' + slot);
    }
    return table + offset;
}

test('malicious: StopPlayback payload offset points past the packet', () => {
    const bytes = b.stopPlayback();
    const packet = rootPacket(bytes);
    const slot = fieldPosition(packet.bb!, packet.bb_pos, 6);
    const target = bytes.length + 4096;
    const copy = bytes.slice();
    viewOf(copy).setInt32(slot, target - slot, true);
    expectRejected(copy, 'out of bounds');
});

test('malicious: StopPlayback table whose vtable starts outside the packet', () => {
    const bytes = b.stopPlayback();
    const packet = rootPacket(bytes);
    const stop = packet.payload(new StopPlayback()) as StopPlayback;
    assert.ok(stop.bb_pos < bytes.length, 'payload table is not inside the packet');
    const copy = bytes.slice();
    viewOf(copy).setInt32(stop.bb_pos, stop.bb_pos - (bytes.length + 100), true);
    expectRejected(copy, 'out of bounds');
});

test('malicious: QueueRemove front marker redirected past the packet', () => {
    const bytes = b.queueRemove({ kind: 'front' });
    const packet = rootPacket(bytes);
    const remove = packet.payload(new QueueRemove()) as QueueRemove;
    const slot = fieldPosition(remove.bb!, remove.bb_pos, 6);
    const target = bytes.length + 4096;
    assert.ok(target >= bytes.length);
    const copy = bytes.slice();
    viewOf(copy).setInt32(slot, target - slot, true);
    expectRejected(copy, 'out of bounds');
});

test('malicious: AddSubtitleSource metadata offset points past the packet', () => {
    const bytes = b.addSubtitleSource('https://example.com/en.vtt', true, 'English', true);
    const packet = rootPacket(bytes);
    const subtitle = packet.payload(new AddSubtitleSource()) as AddSubtitleSource;
    const slot = fieldPosition(subtitle.bb!, subtitle.bb_pos, 10);
    const target = bytes.length + 4096;
    const copy = bytes.slice();
    viewOf(copy).setInt32(slot, target - slot, true);
    expectRejected(copy, 'out of bounds');
});

test('malicious: Load container string terminator is not zero', () => {
    const bytes = b.loadSingle({ container: 'video/x-matroska' });
    const packet = rootPacket(bytes);
    const load = packet.payload(new Load()) as Load;
    const item = load.source(new MediaItem()) as MediaItem;
    const slot = fieldPosition(item.bb!, item.bb_pos, 4);
    const str = slot + item.bb!.readInt32(slot);
    const nul = str + 4 + item.bb!.readInt32(str);
    assert.strictEqual(bytes[nul], 0, 'fixture terminator is not NUL');
    const copy = bytes.slice();
    copy[nul] = 0x41;
    expectRejected(copy, 'not NUL terminated');
});

test('malicious: VolumeChanged declared inline size too small for its field', () => {
    const bytes = b.volumeChanged(0.75);
    const packet = rootPacket(bytes);
    const volume = packet.payload(new VolumeChanged()) as VolumeChanged;
    const vtable = volume.bb_pos - volume.bb!.readInt32(volume.bb_pos);
    const packetVtable = packet.bb_pos - packet.bb!.readInt32(packet.bb_pos);
    assert.notStrictEqual(vtable, packetVtable, 'VolumeChanged shares the Packet vtable');
    const copy = bytes.slice();
    viewOf(copy).setUint16(vtable + 2, 5, true);
    expectRejected(copy, 'field exceeds table');
});

test('aliasing: 1000 references to one 8192-char header exhaust the string budget', () => {
    expectRejected(b.loadSingleSharedHeader(1000, 'k', 'x'.repeat(8192)), 'budget');
});

test('aliasing: as many references as fit in 512 KiB are rejected with a small heap growth', () => {
    const refs = Math.floor((512 * 1024 - 9000) / 4);
    const bytes = b.loadSingleSharedHeader(refs, 'k', 'x'.repeat(8192));
    assert.ok(bytes.length <= 512 * 1024, 'fixture is ' + bytes.length + ' bytes');

    const before = process.memoryUsage().heapUsed;
    expectRejected(bytes);
    const growth = process.memoryUsage().heapUsed - before;
    assert.ok(growth < 8 * 1024 * 1024, 'heap grew by ' + growth + ' bytes');
});

test('aliasing: more header references than the node budget are rejected', () => {
    expectRejected(b.loadSingleSharedHeader(20000, 'k', ''), 'node budget');
});

test('malicious: time above the safe integer range', () => {
    expectRejected(b.progressChanged(Math.pow(2, 53), null), 'time out of range');
});

test('legit: one header with a 400000-char value is accepted', () => {
    const value = 'v'.repeat(400000);
    const decoded: any = decodePacket(b.loadSingle({ headers: [{ key: 'Cookie', value }] }));
    assert.strictEqual(decoded.source.item.headers[0].value.length, 400000);
});

test('legit: queue of 256 items with two headers and a title each is accepted', () => {
    const items: b.QueueItemSpec[] = [];
    for (let i = 0; i < 256; i++) {
        items.push({
            mediaItem: {
                sourceUrl: 'https://example.com/' + i + '.mp4',
                title: 'Item ' + i,
                headers: [{ key: 'Authorization', value: 'token-' + i }, { key: 'Referer', value: 'https://example.com/' }],
            },
        });
    }
    const decoded: any = decodePacket(b.loadQueue(items, 0, false));
    assert.strictEqual(decoded.source.items.length, 256);
    assert.strictEqual(decoded.source.items[255].mediaItem.title, 'Item 255');
    assert.strictEqual(decoded.source.items[255].mediaItem.headers.length, 2);
});

test('fuzz: 50000 seeded mutations decode or are rejected with DecodeError', () => {
    const fixtures = [
        b.loadSingle(b.METADATA_FIXTURE),
        b.loadQueue(b.QUEUE_FIXTURE, 1, true),
        b.queueInsert(b.INSERT_FIXTURE.item, b.INSERT_FIXTURE.position),
    ];
    const random = mulberry32(0xfca57);
    let ok = 0;
    let rejected = 0;
    let malformed = 0;
    for (let i = 0; i < 50000; i++) {
        const bytes = fixtures[i % fixtures.length].slice();
        const mutations = 1 + Math.floor(random() * 4);
        for (let m = 0; m < mutations; m++) {
            bytes[Math.floor(random() * bytes.length)] = Math.floor(random() * 256);
        }
        try {
            decodePacket(bytes);
            ok++;
        } catch (error) {
            if (!(error instanceof DecodeError)) {
                throw new Error('mutation ' + i + ' threw ' + error);
            }
            if (error.message.startsWith('malformed: ')) {
                malformed++;
            } else {
                rejected++;
            }
        }
    }
    console.log('fuzz: ok ' + ok + ' rejected ' + rejected + ' malformed ' + malformed);
    assert.strictEqual(malformed, 0, 'unchecked decoder paths reached: ' + malformed);
});
