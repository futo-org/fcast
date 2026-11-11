import * as assert from 'assert';
import * as flatbuffers from 'flatbuffers';
import * as http from 'http';
import { test } from './runner';
import { NetworkService } from 'common/NetworkService';
import { PlayMessage } from 'common/Packets';
import { COMPANION_WINDOW_BYTES, CompanionBroker } from 'common/v4/Companion';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { CompanionResourceInfoRequest } from 'common/v4/generated/fcast/v4/companion-resource-info-request';
import { CompanionResourceRequest } from 'common/v4/generated/fcast/v4/companion-resource-request';

const PART_BYTES = 300 * 1024;

// A sender connection that provides one resource, the way a v4 sender answers the receiver.
class Provider {
    public ranges: [number, number][] = [];
    public maxParts = 0;
    // When set, the provider stops answering data requests after this many.
    public answerLimit = Infinity;
    // How long the provider takes to answer a data request.
    public dataDelayMs = 0;

    constructor(
        private readonly broker: () => CompanionBroker,
        public readonly sessionId: string,
        private readonly data: Buffer,
        private readonly knownSize: boolean,
    ) {}

    receive(bytes: Uint8Array): void {
        const packet = Packet.getRootAsPacket(new flatbuffers.ByteBuffer(bytes));
        if (packet.payloadType() === Message.CompanionResourceInfoRequest) {
            const info = packet.payload(new CompanionResourceInfoRequest()) as CompanionResourceInfoRequest;
            setImmediate(() => this.broker().onInfoResponse(this.sessionId, info.requestId(), 'video/mp4',
                this.knownSize ? { kind: 'known', size: this.data.length } : { kind: 'unknown' }));
            return;
        }

        const request = packet.payload(new CompanionResourceRequest()) as CompanionResourceRequest;
        const start = Number(request.readHead().start());
        const stop = Number(request.readHead().stopInclusive());
        this.ranges.push([start, stop]);
        if (this.ranges.length > this.answerLimit) {
            return;
        }

        const slice = this.data.slice(start, Math.min(stop + 1, this.data.length));
        const total = Math.max(1, Math.ceil(slice.length / PART_BYTES));
        this.maxParts = Math.max(this.maxParts, total);
        for (let i = 0; i < total; i++) {
            const header = Buffer.alloc(7);
            header.writeUInt32LE(request.requestId(), 0);
            header.writeUInt8(i, 4);
            header.writeUInt8(total, 5);
            header.writeUInt8(1, 6);
            const body = Buffer.concat([header, slice.slice(i * PART_BYTES, (i + 1) * PART_BYTES)]);
            setTimeout(() => this.broker().onResource(this.sessionId, body), this.dataDelayMs);
        }
    }
}

interface Setup {
    broker: CompanionBroker;
    provider: Provider;
    url: string;
    data: Buffer;
}

async function setup(size: number, knownSize: boolean = true): Promise<Setup> {
    const data = Buffer.alloc(size);
    for (let i = 0; i < size; i++) {
        data[i] = (i * 31 + (i >> 8)) & 0xff;
    }
    let provider: Provider = null;
    const broker = new CompanionBroker((sessionId, bytes) => {
        assert.strictEqual(sessionId, provider.sessionId, 'a request went to a connection that is not the provider');
        provider.receive(bytes);
    }, 500);
    // The provider is one connection; the Load that names its resource may come from any other.
    provider = new Provider(() => broker, 'provider-connection', data, knownSize);
    const providerId = broker.registerProvider(provider.sessionId);
    broker.registerProvider('loader-connection');
    NetworkService.companion = broker;
    const url = await NetworkService.proxyPlayIfRequired(new PlayMessage('video/mp4', `fcomp://${providerId}.fcast/7`));
    return { broker, provider, url, data };
}

interface Response {
    status: number;
    headers: http.IncomingHttpHeaders;
    body: Buffer;
    complete: boolean;
}

function get(url: string, headers: { [key: string]: string } = {}): Promise<Response> {
    return new Promise((resolve, reject) => {
        const request = http.get(url, { headers }, (response) => {
            const chunks: Buffer[] = [];
            response.on('data', (chunk) => chunks.push(chunk));
            const finish = () => resolve({ status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks), complete: response.complete });
            response.on('end', finish);
            response.on('aborted', finish);
            response.on('error', finish);
        });
        request.on('error', reject);
    });
}

function wait(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

test('proxy: a companion resource is served whole over loopback HTTP', async () => {
    const s = await setup(100000);
    assert.ok(s.url.indexOf('http://127.0.0.1:') === 0, s.url);
    assert.strictEqual(NetworkService.proxyServerAddress.address, '127.0.0.1');

    const response = await get(s.url);
    assert.strictEqual(response.status, 200);
    assert.strictEqual(response.headers['content-length'], '100000');
    assert.strictEqual(response.headers['content-type'], 'video/mp4');
    assert.strictEqual(response.headers['accept-ranges'], 'bytes');
    assert.ok(response.body.equals(s.data));
    assert.strictEqual(s.broker.pendingCount, 0);
});

test('proxy: a Range request gets that part of the companion resource', async () => {
    const s = await setup(100000);

    const middle = await get(s.url, { Range: 'bytes=1000-1999' });
    assert.strictEqual(middle.status, 206);
    assert.strictEqual(middle.headers['content-range'], 'bytes 1000-1999/100000');
    assert.strictEqual(middle.headers['content-length'], '1000');
    assert.ok(middle.body.equals(s.data.slice(1000, 2000)));
    assert.deepStrictEqual(s.provider.ranges, [[1000, 1999]]);

    const tail = await get(s.url, { Range: 'bytes=99990-' });
    assert.strictEqual(tail.status, 206);
    assert.strictEqual(tail.headers['content-range'], 'bytes 99990-99999/100000');
    assert.ok(tail.body.equals(s.data.slice(99990)));

    const clamped = await get(s.url, { Range: 'bytes=99990-200000' });
    assert.strictEqual(clamped.headers['content-range'], 'bytes 99990-99999/100000');

    const beyond = await get(s.url, { Range: 'bytes=100000-' });
    assert.strictEqual(beyond.status, 416);
    assert.strictEqual(beyond.headers['content-range'], 'bytes */100000');
});

test('proxy: a large companion resource is asked in windows, each answered in few parts', async () => {
    const size = Math.floor(COMPANION_WINDOW_BYTES * 2.5);
    const s = await setup(size);

    const response = await get(s.url);
    assert.strictEqual(response.status, 200);
    assert.ok(response.body.equals(s.data));
    assert.deepStrictEqual(s.provider.ranges, [
        [0, COMPANION_WINDOW_BYTES - 1],
        [COMPANION_WINDOW_BYTES, 2 * COMPANION_WINDOW_BYTES - 1],
        [2 * COMPANION_WINDOW_BYTES, size - 1],
    ]);
    assert.ok(s.provider.maxParts <= 255, 'a window needed ' + s.provider.maxParts + ' parts');
});

test('proxy: a companion resource of unknown size is streamed to its end', async () => {
    const s = await setup(COMPANION_WINDOW_BYTES + 5, false);

    const response = await get(s.url);
    assert.strictEqual(response.status, 200);
    assert.strictEqual(response.headers['content-length'], undefined);
    assert.ok(response.body.equals(s.data));

    const ranged = await get(s.url, { Range: 'bytes=10-' });
    assert.strictEqual(ranged.status, 416);
});

test('proxy: closing the HTTP request cancels the read in flight and stops asking the provider', async () => {
    const s = await setup(COMPANION_WINDOW_BYTES * 6);
    s.provider.dataDelayMs = 300;

    // The reader takes the whole first window and leaves. By then the proxy is waiting for the
    // second window, which the provider takes a while to answer.
    await new Promise<void>((resolve, reject) => {
        let received = 0;
        const request = http.get(s.url, (response) => {
            response.on('data', (chunk) => {
                received += chunk.length;
                if (received >= COMPANION_WINDOW_BYTES) {
                    request.destroy();
                    resolve();
                }
            });
        });
        request.on('error', () => resolve());
        setTimeout(() => reject(new Error('the first window did not arrive')), 3000);
    });
    await wait(100);
    assert.strictEqual(s.provider.ranges.length, 2);
    assert.strictEqual(s.broker.pendingCount, 0, 'the read in flight was not cancelled when the request was closed');

    // The provider's late answer changes nothing.
    await wait(500);
    assert.strictEqual(s.provider.ranges.length, 2, 'the provider was asked again after the request was closed');
    assert.strictEqual(s.broker.pendingCount, 0);
});

test('proxy: the response ends when the provider disconnects in the middle', async () => {
    const s = await setup(COMPANION_WINDOW_BYTES * 3);
    s.provider.answerLimit = 1;
    setTimeout(() => s.broker.removeSession(s.provider.sessionId), 150);

    const response = await get(s.url);
    assert.strictEqual(response.complete, false);
    assert.ok(response.body.length < s.data.length);
    assert.strictEqual(s.broker.pendingCount, 0);
});

test('proxy: a resource whose provider is gone or silent is not found', async () => {
    const s = await setup(1000);
    s.broker.removeSession(s.provider.sessionId);
    assert.strictEqual((await get(s.url)).status, 404);

    const silent = await setup(1000);
    silent.provider.receive = () => undefined;
    assert.strictEqual((await get(silent.url)).status, 504);
});
