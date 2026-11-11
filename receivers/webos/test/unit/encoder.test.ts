import * as assert from 'assert';
import * as flatbuffers from 'flatbuffers';
import { test } from './runner';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { ReceiverIntroduction } from 'common/v4/generated/fcast/v4/receiver-introduction';
import { Error as ErrorMessage } from 'common/v4/generated/fcast/v4/error';
import { PlaybackState } from 'common/v4/generated/fcast/v4/playback-state';
import { PlaybackStateChanged } from 'common/v4/generated/fcast/v4/playback-state-changed';
import { VolumeChanged } from 'common/v4/generated/fcast/v4/volume-changed';
import { SpeedChanged } from 'common/v4/generated/fcast/v4/speed-changed';
import { ProgressChanged } from 'common/v4/generated/fcast/v4/progress-changed';
import {
    encodeError,
    encodePlaybackStateChanged,
    encodeProgressChanged,
    encodeReceiverIntroduction,
    encodeSpeedChanged,
    encodeVolumeChanged,
} from 'common/v4/Encoder';

function readPacket(bytes: Uint8Array): Packet {
    return Packet.getRootAsPacket(new flatbuffers.ByteBuffer(bytes));
}

function readStrings(count: number, get: (index: number) => string | null): (string | null)[] {
    const values: (string | null)[] = [];
    for (let i = 0; i < count; i++) {
        values.push(get(i));
    }
    return values;
}

const CAPS = {
    media: {
        protocols: ['http', 'https', 'whep'],
        containers: ['mp4', 'hls'],
        videoFormats: ['h264', 'vp9'],
        audioFormats: ['aac', 'opus'],
        subtitleFormats: ['vtt'],
        hdrFormats: ['hdr10'],
        imageFormats: ['png', 'jpeg'],
        externalSubtitles: true,
        mirroring: false,
    },
    display: { width: 1920, height: 1080 },
    volumeStepInterval: 0.5,
};

test('encoder: ReceiverIntroduction reads back with device info and capabilities', () => {
    const packet = readPacket(encodeReceiverIntroduction({ displayName: 'Living Room', appName: 'FCast Receiver', appVersion: '2.0.1' }, CAPS));
    assert.strictEqual(packet.payloadType(), Message.ReceiverIntroduction);

    const intro = packet.payload(new ReceiverIntroduction()) as ReceiverIntroduction;
    const info = intro.deviceInfo()!;
    assert.strictEqual(info.displayName(), 'Living Room');
    assert.strictEqual(info.appName(), 'FCast Receiver');
    assert.strictEqual(info.appVersion(), '2.0.1');

    const capabilities = intro.capabilities()!;
    const media = capabilities.media()!;
    assert.deepStrictEqual(readStrings(media.protocolsLength(), (i) => media.protocols(i)), CAPS.media.protocols);
    assert.deepStrictEqual(readStrings(media.containersLength(), (i) => media.containers(i)), CAPS.media.containers);
    assert.deepStrictEqual(readStrings(media.videoFormatsLength(), (i) => media.videoFormats(i)), CAPS.media.videoFormats);
    assert.deepStrictEqual(readStrings(media.audioFormatsLength(), (i) => media.audioFormats(i)), CAPS.media.audioFormats);
    assert.deepStrictEqual(readStrings(media.subtitleFormatsLength(), (i) => media.subtitleFormats(i)), CAPS.media.subtitleFormats);
    assert.deepStrictEqual(readStrings(media.hdrFormatsLength(), (i) => media.hdrFormats(i)), CAPS.media.hdrFormats);
    assert.deepStrictEqual(readStrings(media.imageFormatsLength(), (i) => media.imageFormats(i)), CAPS.media.imageFormats);
    assert.strictEqual(media.externalSubtitles(), true);
    assert.strictEqual(media.mirroring(), false);

    const resolution = capabilities.display()!.resolution()!;
    assert.strictEqual(resolution.width(), 1920);
    assert.strictEqual(resolution.height(), 1080);
    assert.strictEqual(capabilities.audio()!.volumeStepInterval(), 0.5);
});

test('encoder: ReceiverIntroduction without display or audio capabilities omits them', () => {
    const packet = readPacket(encodeReceiverIntroduction({ displayName: 'Receiver', appName: 'FCast', appVersion: '1' }, {
        media: CAPS.media,
        display: null,
        volumeStepInterval: null,
    }));
    const capabilities = (packet.payload(new ReceiverIntroduction()) as ReceiverIntroduction).capabilities()!;
    assert.strictEqual(capabilities.display(), null);
    assert.strictEqual(capabilities.audio(), null);
});

test('encoder: Error keeps packet_num when present and reads back null when absent', () => {
    const withNumber = readPacket(encodeError(6, 42));
    assert.strictEqual(withNumber.payloadType(), Message.Error);
    const error = withNumber.payload(new ErrorMessage()) as ErrorMessage;
    assert.strictEqual(error.kind(), 6);
    assert.strictEqual(error.packetNum(), 42);

    const withoutNumber = (readPacket(encodeError(3, null)).payload(new ErrorMessage()) as ErrorMessage);
    assert.strictEqual(withoutNumber.kind(), 3);
    assert.strictEqual(withoutNumber.packetNum(), null);
});

test('encoder: PlaybackStateChanged reads back the state', () => {
    const packet = readPacket(encodePlaybackStateChanged(PlaybackState.Buffering));
    assert.strictEqual(packet.payloadType(), Message.PlaybackStateChanged);
    assert.strictEqual((packet.payload(new PlaybackStateChanged()) as PlaybackStateChanged).state(), PlaybackState.Buffering);
});

test('encoder: VolumeChanged reads back the volume', () => {
    const packet = readPacket(encodeVolumeChanged(0.25));
    assert.strictEqual(packet.payloadType(), Message.VolumeChanged);
    assert.strictEqual((packet.payload(new VolumeChanged()) as VolumeChanged).volume(), 0.25);
});

test('encoder: SpeedChanged reads back the speed', () => {
    const packet = readPacket(encodeSpeedChanged(2));
    assert.strictEqual(packet.payloadType(), Message.SpeedChanged);
    assert.strictEqual((packet.payload(new SpeedChanged()) as SpeedChanged).speed(), 2);
});

test('encoder: ProgressChanged reads back position and duration in microseconds', () => {
    const packet = readPacket(encodeProgressChanged(1500000, 90000000));
    assert.strictEqual(packet.payloadType(), Message.ProgressChanged);
    const progress = packet.payload(new ProgressChanged()) as ProgressChanged;
    assert.strictEqual(progress.position()!.micros(), BigInt(1500000));
    assert.strictEqual(progress.duration()!.micros(), BigInt(90000000));
});

test('encoder: ProgressChanged rounds to whole microseconds and leaves duration null when absent', () => {
    const progress = readPacket(encodeProgressChanged(1500000.6, null)).payload(new ProgressChanged()) as ProgressChanged;
    assert.strictEqual(progress.position()!.micros(), BigInt(1500001));
    assert.strictEqual(progress.duration(), null);
});
