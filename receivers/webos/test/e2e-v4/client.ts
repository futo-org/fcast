import * as crypto from 'crypto';
import * as net from 'net';
import * as stream from 'stream';
import * as tls from 'tls';
import * as flatbuffers from 'flatbuffers';
import * as forge from 'node-forge';
import { encodeFrame, Frame, FrameReader, Opcode4 } from 'common/v4/Framing';
import { encodeReceiverIntroduction } from 'common/v4/Encoder';
import { Packet } from 'common/v4/generated/fcast/v4/packet';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { ReceiverIntroduction } from 'common/v4/generated/fcast/v4/receiver-introduction';
import { Error as ErrorMessage } from 'common/v4/generated/fcast/v4/error';

export const HOST = '127.0.0.1';
export const PORT = 46899;

export function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function waitUntil(predicate: () => boolean, timeoutMs: number, stepMs = 50): Promise<boolean> {
    const deadline = Date.now() + timeoutMs;
    while (!predicate()) {
        if (Date.now() >= deadline) {
            return false;
        }
        await sleep(stepMs);
    }
    return true;
}

export function versionFrame(version: number): Buffer {
    return encodeFrame(Opcode4.Version, Buffer.from(JSON.stringify({ version: version }), 'utf8'));
}

// SHA-256 of the DER SubjectPublicKeyInfo, base64, as advertised in the fp TXT field.
export function spkiFingerprint(certDer: Buffer): string {
    const cert = forge.pki.certificateFromAsn1(forge.asn1.fromDer(certDer.toString('binary')));
    const spki = crypto.createPublicKey(forge.pki.publicKeyToPem(cert.publicKey)).export({ type: 'spki', format: 'der' }) as Buffer;
    return crypto.createHash('sha256').update(spki).digest('base64');
}

// Plaintext side of a connection. Reads the receiver's Version frame and hands every byte that
// follows it to the attached sink, which is the TLS client's transport.
export class RawPeer {
    readonly socket: net.Socket;
    versionBody: string | null = null;
    bytesAfterVersion = 0;
    closedAt: number | null = null;
    private header: Buffer = Buffer.alloc(0);
    private sink: ((chunk: Buffer) => void) | null = null;
    private sinkEnd: (() => void) | null = null;
    private early: Buffer[] = [];

    private constructor(socket: net.Socket) {
        this.socket = socket;
        socket.on('data', (chunk: Buffer) => this.onData(chunk));
        socket.on('end', () => this.onEnd());
        socket.on('error', () => undefined);
        socket.on('close', () => {
            if (this.closedAt === null) {
                this.closedAt = Date.now();
            }
            this.onEnd();
        });
    }

    static async open(timeoutMs = 5000): Promise<RawPeer> {
        const socket = net.connect({ host: HOST, port: PORT });
        const peer = new RawPeer(socket);
        const ok = await waitUntil(() => peer.versionBody !== null, timeoutMs);
        if (!ok) {
            socket.destroy();
            throw new Error('no Version frame from the receiver within ' + timeoutMs + ' ms');
        }
        return peer;
    }

    write(bytes: Buffer, callback?: (err?: Error) => void) {
        this.socket.write(bytes, callback);
    }

    attachSink(sink: (chunk: Buffer) => void, end: () => void) {
        this.sink = sink;
        this.sinkEnd = end;
        const pending = this.early;
        this.early = [];
        pending.forEach((chunk) => sink(chunk));
        if (this.closedAt !== null) {
            this.onEnd();
        }
    }

    private onData(chunk: Buffer) {
        let rest = chunk;
        if (this.versionBody === null) {
            this.header = Buffer.concat([this.header, chunk]);
            if (this.header.length < 4) {
                return;
            }
            const size = this.header.readUIntLE(0, 4);
            if (this.header.length < 4 + size) {
                return;
            }
            this.versionBody = this.header.toString('utf8', 5, 4 + size);
            rest = this.header.slice(4 + size);
            this.header = Buffer.alloc(0);
        }

        if (rest.length > 0) {
            this.bytesAfterVersion += rest.length;
            if (this.sink !== null) {
                this.sink(rest);
            } else {
                this.early.push(rest);
            }
        }
    }

    private onEnd() {
        if (this.sinkEnd !== null) {
            const end = this.sinkEnd;
            this.sinkEnd = null;
            end();
        }
    }
}

// Duplex the TLS client uses as its socket. Writes are passed to `hook`.
class ClientTransport extends stream.Duplex {
    private peer: RawPeer;
    private hook: (chunk: Buffer, callback: (error?: Error | null) => void) => void;

    constructor(peer: RawPeer, hook: (chunk: Buffer, callback: (error?: Error | null) => void) => void) {
        super();
        this.peer = peer;
        this.hook = hook;
        peer.attachSink((chunk: Buffer) => { this.push(chunk); }, () => { this.push(null); });
    }

    _read() {
        return undefined;
    }

    _write(chunk: Buffer, encoding: string, callback: (error?: Error | null) => void) {
        this.hook(chunk, callback);
    }

    _destroy(error: Error | null, callback: (error?: Error | null) => void) {
        this.peer.socket.destroy();
        callback(error);
    }
}

// How the Version frame and the ClientHello reach the receiver:
// separate: Version in its own write, then the ClientHello.
// coalesced: Version and the ClientHello in one write.
// fragmented: Version one byte per write with 5 ms gaps, then the ClientHello.
export type HandshakeMode = 'separate' | 'coalesced' | 'fragmented';

export interface HandshakeOptions {
    mode: HandshakeMode;
    tlsVersion?: 'TLSv1.2' | 'TLSv1.3';
}

export class FrameQueue {
    private reader = new FrameReader();
    private frames: Frame[] = [];
    private waiters: Array<() => void> = [];
    private ended = false;
    private failure: Error | null = null;

    feed(chunk: Buffer) {
        try {
            this.reader.push(chunk).forEach((frame) => this.frames.push(frame));
        } catch (e) {
            this.failure = e;
        }
        this.wake();
    }

    end() {
        this.ended = true;
        this.wake();
    }

    // Resolves with the next frame whose opcode is one of `opcodes`. Other frames stay queued.
    async next(opcodes: number[], timeoutMs: number): Promise<Frame> {
        const deadline = Date.now() + timeoutMs;
        for (;;) {
            const index = this.frames.findIndex((frame) => opcodes.indexOf(frame.opcode) !== -1);
            if (index !== -1) {
                return this.frames.splice(index, 1)[0];
            }
            if (this.failure !== null) {
                throw this.failure;
            }
            if (this.ended) {
                throw new Error('connection closed while waiting for opcode ' + opcodes.join('|'));
            }
            const remaining = deadline - Date.now();
            if (remaining <= 0) {
                throw new Error('timed out waiting for opcode ' + opcodes.join('|'));
            }
            await new Promise<void>((resolve) => {
                const timer = setTimeout(resolve, remaining);
                this.waiters.push(() => {
                    clearTimeout(timer);
                    resolve();
                });
            });
        }
    }

    private wake() {
        const waiters = this.waiters;
        this.waiters = [];
        waiters.forEach((wake) => wake());
    }
}

export interface ClientSession {
    peer: RawPeer;
    tls: tls.TLSSocket;
    frames: FrameQueue;
    secureAt: number;
    spki: string;
    protocol: string | null;
    close(): void;
}

// Runs the TLS client over an open peer. Resolves on 'secure'; rejects when the handshake fails.
export function handshake(peer: RawPeer, options: HandshakeOptions): Promise<ClientSession> {
    const tlsVersion = options.tlsVersion || 'TLSv1.3';
    let versionSent = options.mode === 'separate';
    if (versionSent) {
        peer.write(versionFrame(4));
    }

    const hook = (chunk: Buffer, callback: (error?: Error | null) => void) => {
        if (!versionSent && options.mode === 'coalesced') {
            versionSent = true;
            peer.write(Buffer.concat([versionFrame(4), chunk]), callback);
            return;
        }
        if (!versionSent && options.mode === 'fragmented') {
            versionSent = true;
            const frame = versionFrame(4);
            (async () => {
                for (let i = 0; i < frame.length; i++) {
                    peer.write(frame.slice(i, i + 1));
                    await sleep(5);
                }
                peer.write(chunk, callback);
            })();
            return;
        }
        peer.write(chunk, callback);
    };

    const transport = new ClientTransport(peer, hook);
    const socket = tls.connect({
        socket: transport as any,
        rejectUnauthorized: false,
        minVersion: tlsVersion,
        maxVersion: tlsVersion,
    } as any);

    const frames = new FrameQueue();
    socket.on('data', (chunk: Buffer) => frames.feed(chunk));
    socket.on('end', () => frames.end());
    socket.on('close', () => frames.end());
    socket.on('error', () => frames.end());

    return new Promise<ClientSession>((resolve, reject) => {
        const timer = setTimeout(() => {
            reject(new Error('TLS handshake did not complete within 10 s'));
        }, 10000);

        socket.once('secure', () => {
            clearTimeout(timer);
            const cert = socket.getPeerCertificate(true);
            resolve({
                peer: peer,
                tls: socket,
                frames: frames,
                secureAt: Date.now(),
                spki: spkiFingerprint(cert.raw),
                protocol: socket.getProtocol(),
                close: () => {
                    socket.destroy();
                    peer.socket.destroy();
                },
            });
        });
        socket.once('error', (err: Error) => {
            clearTimeout(timer);
            reject(err);
        });
        socket.once('close', () => {
            clearTimeout(timer);
            reject(new Error('connection closed during the TLS handshake'));
        });
    });
}

export function decodePacketBytes(body: Buffer): Packet {
    return Packet.getRootAsPacket(new flatbuffers.ByteBuffer(new Uint8Array(body.buffer, body.byteOffset, body.length)));
}

export function receiverIntroductionOf(body: Buffer): ReceiverIntroduction {
    const packet = decodePacketBytes(body);
    if (packet.payloadType() !== Message.ReceiverIntroduction) {
        throw new Error('expected ReceiverIntroduction, got message type ' + packet.payloadType());
    }
    return packet.payload(new ReceiverIntroduction()) as ReceiverIntroduction;
}

export function payloadTypeOf(body: Buffer): number {
    return decodePacketBytes(body).payloadType();
}

export function errorOf(body: Buffer): { kind: number; packetNum: number | null } {
    const packet = decodePacketBytes(body);
    if (packet.payloadType() !== Message.Error) {
        throw new Error('expected Error, got message type ' + packet.payloadType());
    }
    const error = packet.payload(new ErrorMessage()) as ErrorMessage;
    return { kind: error.kind(), packetNum: error.packetNum() };
}

export function receiverIntroductionBytes(): Uint8Array {
    return encodeReceiverIntroduction(
        { displayName: 'FCast e2e sender', appName: 'e2e', appVersion: '1.0' },
        {
            media: {
                protocols: [],
                containers: [],
                videoFormats: [],
                audioFormats: [],
                subtitleFormats: [],
                hdrFormats: [],
                imageFormats: [],
                externalSubtitles: false,
                mirroring: false,
            },
            display: null,
            volumeStepInterval: null,
        },
    );
}

export function flatbufFrame(bytes: Uint8Array): Buffer {
    return encodeFrame(Opcode4.Flatbuf, bytes);
}
