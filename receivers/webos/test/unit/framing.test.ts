import * as assert from 'assert';
import { test } from './runner';
import { encodeFrame, FrameError, FrameReader, MAX_V4_PACKET, Opcode4 } from 'common/v4/Framing';

test('framing: encodeFrame writes a little-endian Size, then the opcode, then the body', () => {
    const frame = encodeFrame(Opcode4.Flatbuf, Buffer.from([1, 2, 3]));
    assert.deepStrictEqual(frame, Buffer.from([4, 0, 0, 0, 20, 1, 2, 3]));
});

test('framing: encodeFrame without a body has Size 1', () => {
    assert.deepStrictEqual(encodeFrame(Opcode4.Ping), Buffer.from([1, 0, 0, 0, 12]));
});

test('framing: encodeFrame round trip', () => {
    const body = Buffer.from('hello, receiver');
    const frames = new FrameReader().push(encodeFrame(Opcode4.Flatbuf, body));
    assert.deepStrictEqual(frames, [{ opcode: Opcode4.Flatbuf, body }]);
});

test('framing: a frame split at every byte boundary is reassembled', () => {
    const body = Buffer.from('split me at every byte');
    const expected = [{ opcode: Opcode4.Flatbuf, body }, { opcode: Opcode4.Pong, body: Buffer.alloc(0) }];
    const stream = Buffer.concat([encodeFrame(Opcode4.Flatbuf, body), encodeFrame(Opcode4.Pong)]);
    for (let split = 0; split <= stream.length; split++) {
        const reader = new FrameReader();
        const frames = reader.push(stream.slice(0, split)).concat(reader.push(stream.slice(split)));
        assert.deepStrictEqual(frames, expected, 'split at ' + split);
    }
});

test('framing: a frame delivered one byte at a time is reassembled', () => {
    const body = Buffer.from([9, 8, 7]);
    const reader = new FrameReader();
    const frames = [];
    const stream = encodeFrame(Opcode4.Flatbuf, body);
    for (let i = 0; i < stream.length; i++) {
        frames.push(...reader.push(stream.slice(i, i + 1)));
    }
    assert.deepStrictEqual(frames, [{ opcode: Opcode4.Flatbuf, body }]);
});

test('framing: several frames in one chunk', () => {
    const stream = Buffer.concat([
        encodeFrame(Opcode4.Ping),
        encodeFrame(Opcode4.Flatbuf, Buffer.from('a')),
        encodeFrame(Opcode4.Resource, Buffer.from('bc')),
    ]);
    assert.deepStrictEqual(new FrameReader().push(stream), [
        { opcode: Opcode4.Ping, body: Buffer.alloc(0) },
        { opcode: Opcode4.Flatbuf, body: Buffer.from('a') },
        { opcode: Opcode4.Resource, body: Buffer.from('bc') },
    ]);
});

test('framing: Size 0 is a FrameError', () => {
    assert.throws(() => new FrameReader().push(Buffer.from([0, 0, 0, 0])), (error: any) => error instanceof FrameError && error.name === 'FrameError');
});

test('framing: Size MAX+1 is a FrameError as soon as the header arrives', () => {
    const size = MAX_V4_PACKET + 1;
    const header = Buffer.from([size & 0xff, (size >> 8) & 0xff, (size >> 16) & 0xff, (size >>> 24) & 0xff]);
    assert.throws(() => new FrameReader().push(header), (error: any) => error instanceof FrameError);
});

test('framing: Size exactly MAX is accepted', () => {
    const body = Buffer.alloc(MAX_V4_PACKET - 1, 0x5a);
    const stream = encodeFrame(Opcode4.Flatbuf, body);
    const reader = new FrameReader();
    const half = Math.floor(stream.length / 2);
    const frames = reader.push(stream.slice(0, half)).concat(reader.push(stream.slice(half)));
    assert.strictEqual(frames.length, 1);
    assert.strictEqual(frames[0].opcode, Opcode4.Flatbuf);
    assert.strictEqual(frames[0].body.length, MAX_V4_PACKET - 1);
});

test('framing: encodeFrame rejects a body that makes Size exceed MAX', () => {
    assert.throws(() => encodeFrame(Opcode4.Flatbuf, Buffer.alloc(MAX_V4_PACKET)), (error: any) => error instanceof FrameError);
});
