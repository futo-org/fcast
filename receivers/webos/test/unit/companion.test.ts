import * as assert from 'assert';
import * as flatbuffers from 'flatbuffers';
import { test } from './runner';
import {
    COMPANION_WINDOW_BYTES, CompanionBroker, CompanionError, parseCompanionUrl, parseResourcePart, streamCompanionResource,
} from 'common/v4/Companion';
import { toWebVtt } from 'common/v4/ResourceFetch';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { CompanionResourceInfoRequest } from 'common/v4/generated/fcast/v4/companion-resource-info-request';
import { CompanionResourceRequest } from 'common/v4/generated/fcast/v4/companion-resource-request';

interface Request {
    sessionId: string;
    kind: 'info' | 'data';
    requestId: number;
    resourceId: number;
    start?: number;
    stop?: number;
}

function readRequest(sessionId: string, bytes: Uint8Array): Request {
    const packet = Packet.getRootAsPacket(new flatbuffers.ByteBuffer(bytes));
    if (packet.payloadType() === Message.CompanionResourceInfoRequest) {
        const info = packet.payload(new CompanionResourceInfoRequest()) as CompanionResourceInfoRequest;
        return { sessionId, kind: 'info', requestId: info.requestId(), resourceId: info.resourceId() };
    }
    const data = packet.payload(new CompanionResourceRequest()) as CompanionResourceRequest;
    return {
        sessionId, kind: 'data', requestId: data.requestId(), resourceId: data.resourceId(),
        start: Number(data.readHead().start()), stop: Number(data.readHead().stopInclusive()),
    };
}

function part(requestId: number, index: number, total: number, data: Buffer | null): Buffer {
    const header = Buffer.alloc(7);
    header.writeUInt32LE(requestId, 0);
    header.writeUInt8(index, 4);
    header.writeUInt8(total, 5);
    header.writeUInt8(data === null ? 0 : 1, 6);
    return data === null ? header : Buffer.concat([header, data]);
}

function setup(timeoutMs?: number): { broker: CompanionBroker; requests: Request[] } {
    const requests: Request[] = [];
    const broker = new CompanionBroker((sessionId, bytes) => requests.push(readRequest(sessionId, bytes)), timeoutMs);
    return { broker, requests };
}

async function rejection(promise: Promise<any>): Promise<CompanionError> {
    try {
        await promise;
    } catch (e) {
        return e;
    }
    throw new Error('promise did not reject');
}

test('companion: URLs name a 16 bit provider and a 32 bit resource', () => {
    assert.deepStrictEqual(parseCompanionUrl('fcomp://12.fcast/345'), { providerId: 12, resourceId: 345 });
    assert.deepStrictEqual(parseCompanionUrl('fcomp://65535.fcast/4294967295'), { providerId: 65535, resourceId: 4294967295 });
    for (const url of ['fcomp://65536.fcast/1', 'fcomp://1.fcast/4294967296', 'fcomp://1.fcast/', 'fcomp://1.other/2', 'http://1.fcast/2', 'fcomp://1.fcast/2/3', 'fcomp://-1.fcast/2']) {
        assert.strictEqual(parseCompanionUrl(url), null, url);
    }
});

test('companion: resource parts are parsed and malformed ones rejected', () => {
    assert.deepStrictEqual(parseResourcePart(part(9, 1, 3, Buffer.from('ab'))), { requestId: 9, part: 1, totalParts: 3, found: true, data: Buffer.from('ab') });
    assert.strictEqual(parseResourcePart(part(9, 0, 1, null)).found, false);
    assert.throws(() => parseResourcePart(Buffer.alloc(6)), CompanionError);
    const badVariant = part(9, 0, 1, Buffer.alloc(0));
    badVariant.writeUInt8(2, 6);
    assert.throws(() => parseResourcePart(badVariant), CompanionError);
    assert.throws(() => parseResourcePart(Buffer.concat([part(9, 0, 1, null), Buffer.from('x')])), CompanionError);
});

test('companion: a provider id is stable per connection and released on disconnect', () => {
    const { broker } = setup();
    assert.strictEqual(broker.registerProvider('A'), 1);
    assert.strictEqual(broker.registerProvider('B'), 2);
    assert.strictEqual(broker.registerProvider('A'), 1);
    broker.removeSession('A');
    assert.strictEqual(broker.hasProvider(1), false);
    assert.strictEqual(broker.hasProvider(2), true);
});

test('companion: requests go to the providing connection and only its answers count', async () => {
    const { broker, requests } = setup();
    const providerA = broker.registerProvider('A');
    broker.registerProvider('B');

    const info = broker.getInfo(providerA, 77);
    assert.deepStrictEqual(requests.map((r) => [r.sessionId, r.kind, r.resourceId]), [['A', 'info', 77]]);
    // The same request id from another connection is not an answer.
    assert.strictEqual(broker.onInfoResponse('B', requests[0].requestId, 'video/mp4', { kind: 'known', size: 10 }), false);
    assert.strictEqual(broker.onInfoResponse('A', requests[0].requestId, 'video/mp4', { kind: 'known', size: 10 }), true);
    assert.deepStrictEqual(await info, { contentType: 'video/mp4', size: 10 });

    const unknown = broker.getInfo(providerA, 78);
    broker.onInfoResponse('A', requests[1].requestId, 'audio/mpeg', { kind: 'unknown' });
    assert.deepStrictEqual(await unknown, { contentType: 'audio/mpeg', size: null });
    assert.strictEqual(broker.pendingCount, 0);
});

test('companion: parts are delivered in order and a read completes with the last part', async () => {
    const { broker, requests } = setup();
    const provider = broker.registerProvider('A');
    const received: string[] = [];

    const read = broker.read(provider, 5, 100, 105, (chunk) => received.push(chunk.toString()));
    const request = requests[0];
    assert.deepStrictEqual([request.kind, request.resourceId, request.start, request.stop], ['data', 5, 100, 105]);
    assert.strictEqual(broker.onResource('B', part(request.requestId, 0, 2, Buffer.from('abc'))), false);
    broker.onResource('A', part(request.requestId, 0, 2, Buffer.from('abc')));
    broker.onResource('A', part(request.requestId, 1, 2, Buffer.from('def')));
    assert.strictEqual(await read.done, 6);
    assert.deepStrictEqual(received, ['abc', 'def']);
    assert.strictEqual(broker.pendingCount, 0);
});

test('companion: a read fails on not found, wrong part order, excess data, disconnect, timeout and cancel', async () => {
    const { broker, requests } = setup(30);
    const provider = broker.registerProvider('A');
    const ignore = () => undefined;

    const notFound = broker.read(provider, 1, 0, 9, ignore);
    broker.onResource('A', part(requests[0].requestId, 0, 1, null));
    assert.strictEqual((await rejection(notFound.done)).reason, 'not-found');

    const outOfOrder = broker.read(provider, 1, 0, 9, ignore);
    broker.onResource('A', part(requests[1].requestId, 1, 2, Buffer.from('x')));
    assert.strictEqual((await rejection(outOfOrder.done)).reason, 'malformed');

    const tooMuch = broker.read(provider, 1, 0, 1, ignore);
    broker.onResource('A', part(requests[2].requestId, 0, 1, Buffer.from('abc')));
    assert.strictEqual((await rejection(tooMuch.done)).reason, 'malformed');

    const timedOut = broker.read(provider, 1, 0, 9, ignore);
    assert.strictEqual((await rejection(timedOut.done)).reason, 'timeout');

    const cancelled = broker.read(provider, 1, 0, 9, ignore);
    cancelled.cancel();
    assert.strictEqual((await rejection(cancelled.done)).reason, 'cancelled');

    const gone = broker.read(provider, 1, 0, 9, ignore);
    const info = broker.getInfo(provider, 1);
    broker.removeSession('A');
    assert.strictEqual((await rejection(gone.done)).reason, 'provider-gone');
    assert.strictEqual((await rejection(info)).reason, 'provider-gone');
    assert.strictEqual((await rejection(broker.getInfo(provider, 1))).reason, 'provider-gone');
    assert.strictEqual(broker.pendingCount, 0);
});

test('companion: a stream asks one window at a time and waits for the writer', async () => {
    const { broker, requests } = setup();
    const provider = broker.registerProvider('A');
    const total = COMPANION_WINDOW_BYTES + 10;
    const written: number[] = [];
    let release: () => void = null;

    const stream = streamCompanionResource(broker, provider, 3, 0, total - 1, (chunk) => {
        written.push(chunk.length);
        return new Promise<void>((resolve) => { release = resolve; });
    });

    assert.deepStrictEqual([requests[0].start, requests[0].stop], [0, COMPANION_WINDOW_BYTES - 1]);
    const half = COMPANION_WINDOW_BYTES / 2;
    broker.onResource('A', part(requests[0].requestId, 0, 2, Buffer.alloc(half)));
    broker.onResource('A', part(requests[0].requestId, 1, 2, Buffer.alloc(half)));
    await new Promise((resolve) => setTimeout(resolve, 0));

    // The first chunk is with the writer; the second window is not requested until it is done.
    assert.deepStrictEqual(written, [half]);
    assert.strictEqual(requests.length, 1);
    release();
    await new Promise((resolve) => setTimeout(resolve, 0));
    release();
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.deepStrictEqual([requests[1].start, requests[1].stop], [COMPANION_WINDOW_BYTES, total - 1]);
    broker.onResource('A', part(requests[1].requestId, 0, 1, Buffer.alloc(10)));
    await new Promise((resolve) => setTimeout(resolve, 0));
    release();
    assert.strictEqual(await stream.done, total);
    assert.deepStrictEqual(written, [half, half, 10]);
});

test('companion: a stream of unknown length ends at the first short window and can be cancelled', async () => {
    const { broker, requests } = setup();
    const provider = broker.registerProvider('A');
    const chunks: Buffer[] = [];

    const stream = streamCompanionResource(broker, provider, 3, 0, null, (chunk) => { chunks.push(chunk); });
    broker.onResource('A', part(requests[0].requestId, 0, 1, Buffer.from('short')));
    assert.strictEqual(await stream.done, 5);
    assert.strictEqual(requests.length, 1);

    const cancelled = streamCompanionResource(broker, provider, 3, 0, null, () => undefined);
    cancelled.cancel();
    assert.strictEqual((await rejection(cancelled.done)).reason, 'cancelled');
    assert.strictEqual(broker.pendingCount, 0);
});

test('companion: SubRip text becomes WebVTT and WebVTT is kept', () => {
    assert.strictEqual(toWebVtt('1\r\n00:00:01,000 --> 00:00:02,500\r\nHello\r\n'), 'WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500\nHello\n');
    assert.strictEqual(toWebVtt('﻿WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n'), 'WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n');
});
