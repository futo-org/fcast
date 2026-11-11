/* End-to-end checks of the v4 receiver against the real service bundle. */
import * as os from 'os';
import * as path from 'path';
import * as util from 'util';
import { encodeFrame, Opcode4 } from 'common/v4/Framing';
import { ErrorKind } from 'common/v4/generated/fcast/v4/error-kind';
import { Message } from 'common/v4/generated/fcast/v4/message';
import { senderIntroduction } from '../unit/builders';
import {
    ClientSession,
    HOST,
    PORT,
    RawPeer,
    errorOf,
    payloadTypeOf,
    flatbufFrame,
    handshake,
    receiverIntroductionBytes,
    receiverIntroductionOf,
    sleep,
    versionFrame,
    waitUntil,
} from './client';

declare const __non_webpack_require__: (id: string) => any;
const harness = __non_webpack_require__(path.join(__dirname, '..', 'e2e', 'harness.js'));

const FAST_CASES = [
    'connect_version_2',
    'connect_version_3',
    'heartbeat',
    'connect_version_4',
    'heartbeat_v4',
    'version_zero_closes',
    'version_downgrade_v5_to_v4',
    'flatbuf_before_handshake_closes',
    'ping_before_version_closes',
    'garbage_flatbuf_closes_v4',
    'truncated_flatbuf_closes_v4',
    'wrong_direction_messages_v4',
    'none_opcode_error_v4',
    'invalid_opcode_error_v4',
    'unsupported_opcode_error_v4',
    'empty_progress_interval_malformed_v4',
];

interface Options {
    serviceDir: string;
    fast: string | null;
    senderFp: string | null;
    sender: string | null;
}

function usage(): string {
    return 'usage: node e2e-v4.js --service-dir <dir> [--fast <fast.exe>] [--sender-fp <fcast.exe>] [--sender <fcast.exe>]';
}

function parseArgs(argv: string[]): Options {
    const opts: Options = { serviceDir: null, fast: null, senderFp: null, sender: null };
    for (let i = 0; i < argv.length; i += 2) {
        const flag = argv[i];
        if (i + 1 >= argv.length) {
            throw new Error(flag + ' needs a value');
        }
        const value = argv[i + 1];
        switch (flag) {
            case '--service-dir': opts.serviceDir = value; break;
            case '--fast': opts.fast = value; break;
            case '--sender-fp': opts.senderFp = value; break;
            case '--sender': opts.sender = value; break;
            default: throw new Error('unknown argument: ' + flag);
        }
    }
    return opts;
}

// Service log lines, captured by wrapping the console methods the service logger writes to.
const logLines: string[] = [];
['log', 'info', 'warn', 'error', 'debug'].forEach((name) => {
    const original = (console as any)[name];
    (console as any)[name] = (...args: any[]) => {
        logLines.push(util.format(...args));
        original.apply(console, args);
    };
});

function countLogs(text: string): number {
    return logLines.filter((line) => line.indexOf(text) !== -1).length;
}

function out(line: string) {
    process.stdout.write(line + '\n');
}

function reasonOf(err: any): string {
    return err && err.message ? err.message : String(err);
}

function check(condition: boolean, message: string) {
    if (!condition) {
        throw new Error(message);
    }
}

let passed = 0;
let total = 0;

async function step(name: string, fn: () => Promise<string | void>): Promise<boolean> {
    total++;
    try {
        const detail = await fn();
        passed++;
        out('PASS ' + name + (detail ? ' ' + detail : ''));
        return true;
    } catch (err) {
        out('FAIL ' + name + ': ' + reasonOf(err));
        return false;
    }
}

function closeQuietly(session: ClientSession | null, peer: RawPeer | null) {
    if (session !== null) {
        session.close();
    } else if (peer !== null) {
        peer.socket.destroy();
    }
}

async function expectIntroduction(session: ClientSession): Promise<string> {
    const frame = await session.frames.next([Opcode4.Flatbuf], 5000);
    const intro = receiverIntroductionOf(frame.body);
    check(intro.capabilities().media().mirroring() === false, 'capabilities.media.mirroring is not false');
    const displayName = String(intro.deviceInfo().displayName());
    check(displayName.indexOf('FCast-') === 0, 'deviceInfo.displayName is "' + displayName + '"');
    return 'displayName=' + displayName;
}

// Returns the next Error the receiver sends, skipping the state it shares after its introduction.
async function nextError(session: ClientSession): Promise<{ kind: number; packetNum: number | null }> {
    const deadline = Date.now() + 5000;
    for (;;) {
        const frame = await session.frames.next([Opcode4.Flatbuf], Math.max(1, deadline - Date.now()));
        if (payloadTypeOf(frame.body) === Message.Error) {
            return errorOf(frame.body);
        }
    }
}

function runFastCase(fastExe: string, fp: string, fastCase: string): Promise<void> {
    return harness.runProcess(fastExe, ['--host', HOST, '--port', String(PORT), '--fingerprint', fp, 'run', fastCase], 120000)
        .then((res: any) => {
            const text = (res.stdout + res.stderr).replace(/\x1b\[[0-9;]*m/g, '');
            const okLine = new RegExp('^\\s*test ' + fastCase + ' \\.\\.\\. OK\\s*$', 'm');
            if (!okLine.test(text)) {
                const lines = text.split(/\r?\n/).filter((line: string) => line.indexOf('test ' + fastCase + ' ') !== -1);
                throw new Error('no "test ' + fastCase + ' ... OK" line (exit ' + res.code + ')' + (lines.length ? ': ' + lines.join(' | ').trim() : ''));
            }
        });
}

async function killAfter(proc: any, ms: number) {
    await sleep(ms);
    try {
        proc.child.kill();
    } catch (e) {
        // the sender may already have exited
    }
}

async function main(opts: Options) {
    let service: any = null;
    let fp = '';

    await step('service-load', async () => {
        service = harness.loadService(opts.serviceDir);
        return '';
    });
    if (service === null) {
        return;
    }

    await step('tcp-listen', () => harness.waitForPort(PORT, HOST, 10000).then(() => 'port ' + PORT));

    await step('v4-enabled-log', async () => {
        const line = logLines.find((l) => l.indexOf('v4 enabled fp=') !== -1);
        check(line !== undefined, 'no "v4 enabled fp=" line in the service log');
        fp = /v4 enabled fp=(\S+)/.exec(line)[1];
        return 'fp=' + fp;
    });

    await step('connection-info', async () => {
        const response = await harness.appRequest(service, 'get_connection_info', {});
        const info = response && response.value;
        check(info !== null && info.txt !== null && info.txt.v === '4', 'txt is ' + JSON.stringify(info && info.txt));
        check(info.txt.fp === fp, 'txt.fp ' + info.txt.fp + ' differs from logged fp ' + fp);
        return 'maxVersion=' + info.maxVersion;
    });

    // 2. Version 4 negotiated, TLS 1.3 pinned by fingerprint, ReceiverIntroduction received.
    let peer2: RawPeer | null = null;
    await step('v4-version-negotiated', async () => {
        peer2 = await RawPeer.open();
        const version = JSON.parse(peer2.versionBody).version;
        check(version === 4, 'receiver Version is ' + version);
        return 'version=4';
    });

    let session2: ClientSession | null = null;
    await step('v4-tls13-pinned-fp', async () => {
        check(peer2 !== null, 'no open connection from the previous step');
        session2 = await handshake(peer2, { mode: 'separate' });
        check(session2.protocol === 'TLSv1.3', 'protocol is ' + session2.protocol);
        check(session2.spki === fp, 'peer SPKI ' + session2.spki + ' differs from advertised fp ' + fp);
        return 'protocol=' + session2.protocol;
    });

    await step('v4-receiver-introduction', async () => {
        check(session2 !== null, 'no TLS session from the previous step');
        return expectIntroduction(session2);
    });
    closeQuietly(session2, peer2);

    // 3. Version and ClientHello in one TCP write.
    let session3: ClientSession | null = null;
    let peer3: RawPeer | null = null;
    await step('v4-coalesced-version-clienthello', async () => {
        peer3 = await RawPeer.open();
        session3 = await handshake(peer3, { mode: 'coalesced' });
        return expectIntroduction(session3);
    });
    closeQuietly(session3, peer3);

    // 4. Version one byte per write, then the ClientHello.
    let session4: ClientSession | null = null;
    let peer4: RawPeer | null = null;
    await step('v4-fragmented-version', async () => {
        peer4 = await RawPeer.open();
        session4 = await handshake(peer4, { mode: 'fragmented' });
        return expectIntroduction(session4);
    });
    closeQuietly(session4, peer4);

    // 4b. The receiver heartbeats while the sender has not sent Version yet. Nothing may be written
    // in plaintext before the handshake.
    await step('v4-delayed-version-no-plaintext', async () => {
        const peer = await RawPeer.open();
        let session: ClientSession | null = null;
        try {
            await sleep(3500);
            check(peer.bytesAfterVersion === 0, peer.bytesAfterVersion + ' bytes received after the Version frame during the 3.5 s wait');
            session = await handshake(peer, { mode: 'separate' });
            const detail = await expectIntroduction(session);
            return detail;
        } finally {
            closeQuietly(session, peer);
        }
    });

    // 4c. Broadcasts reach a session that has not sent Version yet. None of them may be written in
    // plaintext before the handshake.
    await step('v4-broadcast-before-version-no-plaintext', async () => {
        const peer = await RawPeer.open();
        let session: ClientSession | null = null;
        try {
            await harness.appRequest(service, 'send_playback_update', {
                generationTime: Date.now(),
                time: 3,
                duration: 30,
                state: 1,
                speed: 1,
            });
            await harness.appRequest(service, 'send_volume_update', { generationTime: Date.now(), volume: 0.5 });
            await sleep(500);
            check(peer.bytesAfterVersion === 0, peer.bytesAfterVersion + ' bytes received after the Version frame following the broadcasts');
            session = await handshake(peer, { mode: 'separate' });
            const detail = await expectIntroduction(session);
            return detail;
        } finally {
            closeQuietly(session, peer);
        }
    });

    // 4d. A sender that sends nothing after Version is closed by the receiver 10 s after connecting,
    // and nothing is written in the meantime.
    await step('pre-negotiation-timeout', async () => {
        const start = Date.now();
        const peer = await RawPeer.open();
        try {
            const closed = await waitUntil(() => peer.closedAt !== null, 12000);
            check(closed, 'connection still open 12 s after connect');
            const at = peer.closedAt - start;
            check(at >= 9000 && at <= 12000, 'closed ' + at + ' ms after connect, expected 9000 to 12000 ms');
            check(peer.bytesAfterVersion === 0, peer.bytesAfterVersion + ' bytes received after the Version frame');
            return 'after ' + at + ' ms';
        } finally {
            peer.socket.destroy();
        }
    });

    // 5. Plaintext after Version 4: the receiver closes and writes nothing after its Version.
    await step('v4-no-plaintext-after-upgrade', async () => {
        const peer = await RawPeer.open();
        const start = Date.now();
        peer.write(versionFrame(4));
        peer.write(encodeFrame(Opcode4.Ping));
        const closed = await waitUntil(() => peer.closedAt !== null, 12000);
        peer.socket.destroy();
        check(closed, 'receiver did not close within 12 s');
        check(peer.bytesAfterVersion === 0, peer.bytesAfterVersion + ' bytes received after the Version frame');
        return 'closed after ' + (peer.closedAt - start) + ' ms';
    });

    // 6. A TLS 1.2 only client is refused.
    await step('v4-tls12-rejected', async () => {
        const peer = await RawPeer.open();
        let session: ClientSession | null = null;
        let failed = false;
        try {
            session = await handshake(peer, { mode: 'separate', tlsVersion: 'TLSv1.2' });
        } catch (e) {
            failed = true;
        }
        const closed = await waitUntil(() => peer.closedAt !== null, 12000);
        closeQuietly(session, peer);
        check(!session, 'TLS 1.2 handshake succeeded');
        check(failed, 'TLS 1.2 handshake did not fail');
        check(closed, 'connection not closed after the failed handshake');
        return '';
    });

    // 7. Heartbeat. The connection stays silent after the handshake: a Ping arrives, then the
    // receiver closes about 6 s after the last client packet.
    let silent: ClientSession | null = null;
    await step('v4-heartbeat-ping', async () => {
        const peer = await RawPeer.open();
        silent = await handshake(peer, { mode: 'separate' });
        await expectIntroduction(silent);
        await silent.frames.next([Opcode4.Ping], 6000);
        const at = Date.now() - silent.secureAt;
        check(at >= 2500 && at <= 4500, 'Ping arrived ' + at + ' ms after the handshake');
        return 'after ' + at + ' ms';
    });

    await step('v4-heartbeat-timeout', async () => {
        check(silent !== null, 'no silent session from the previous step');
        const closed = await waitUntil(() => silent.peer.closedAt !== null, 10000);
        check(closed, 'connection still open 10 s after the handshake');
        const at = silent.peer.closedAt - silent.secureAt;
        check(at >= 5500 && at <= 8000, 'closed ' + at + ' ms after the last client packet');
        return 'after ' + at + ' ms';
    });
    closeQuietly(silent, null);

    await step('v4-heartbeat-answered-open', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            const end = Date.now() + 10000;
            let pings = 0;
            for (;;) {
                const remaining = end - Date.now();
                if (remaining <= 0) {
                    break;
                }
                try {
                    await session.frames.next([Opcode4.Ping], remaining);
                } catch (e) {
                    break;
                }
                pings++;
                session.tls.write(encodeFrame(Opcode4.Pong));
            }
            check(peer.closedAt === null, 'connection closed while Pings were answered');
            check(pings >= 2, 'only ' + pings + ' Pings in 10 s');
            return 'pings=' + pings;
        } finally {
            session.close();
        }
    });

    // 7b. The shared state follows the introduction, starting with the volume.
    await step('v4-state-after-introduction', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            const frame = await session.frames.next([Opcode4.Flatbuf], 5000);
            check(payloadTypeOf(frame.body) === Message.VolumeChanged, 'first message after the introduction has type ' + payloadTypeOf(frame.body));
            return '';
        } finally {
            session.close();
        }
    });

    // 8. Errors on an established v4 session.
    await step('v4-invalid-opcode-error', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            session.tls.write(encodeFrame(0x7f));
            const error = await nextError(session);
            check(error.kind === ErrorKind.InvalidOpcode, 'error kind ' + error.kind);
            check(error.packetNum === 1, 'packet_num ' + error.packetNum + ', expected 1');
            return 'packet_num=' + error.packetNum;
        } finally {
            session.close();
        }
    });

    await step('v4-wrong-direction-keeps-connection', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            session.tls.write(flatbufFrame(receiverIntroductionBytes()));
            const error = await nextError(session);
            check(error.kind === ErrorKind.InvalidPayloadType, 'error kind ' + error.kind);
            check(error.packetNum === 1, 'packet_num ' + error.packetNum + ', expected 1');
            session.tls.write(encodeFrame(Opcode4.Ping));
            await session.frames.next([Opcode4.Pong], 5000);
            return '';
        } finally {
            session.close();
        }
    });

    await step('v4-garbage-flatbuf-closes', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            session.tls.write(flatbufFrame(new Uint8Array([0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4])));
            const error = await nextError(session);
            check(error.kind === ErrorKind.MalformedBody, 'error kind ' + error.kind);
            const closed = await waitUntil(() => peer.closedAt !== null, 3000);
            check(closed, 'connection not closed after MalformedBody');
            return '';
        } finally {
            session.close();
        }
    });

    await step('v4-size-zero-closes', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            session.tls.write(Buffer.from([0, 0, 0, 0]));
            const closed = await waitUntil(() => peer.closedAt !== null, 3000);
            check(closed, 'connection not closed after a zero Size frame');
            return '';
        } finally {
            session.close();
        }
    });

    // 9. SenderIntroduction reaches the service.
    await step('v4-sender-introduction', async () => {
        const peer = await RawPeer.open();
        const session = await handshake(peer, { mode: 'separate' });
        try {
            await expectIntroduction(session);
            const before = countLogs('v4 sender introduced');
            session.tls.write(flatbufFrame(senderIntroduction('FCast e2e sender', 'e2e', '1.0')));
            const seen = await waitUntil(() => countLogs('v4 sender introduced') > before, 5000);
            check(seen, 'no "v4 sender introduced" line in the service log');
            return '';
        } finally {
            session.close();
        }
    });

    // 10. FAST conformance cases. Each case is one FAST run; only its own OK line counts.
    if (opts.fast && fp) {
        for (const fastCase of FAST_CASES) {
            await step('fast-' + fastCase, () => runFastCase(opts.fast, fp, fastCase).then(() => ''));
        }

        await step('fast-negative-wrong-fingerprint', async () => {
            const wrong = Buffer.alloc(32, 0x5a).toString('base64');
            const res = await harness.runProcess(opts.fast, ['--host', HOST, '--port', String(PORT), '--fingerprint', wrong, 'run', 'connect_version_4'], 120000);
            const text = (res.stdout + res.stderr).replace(/\x1b\[[0-9;]*m/g, '');
            check(!/^\s*test connect_version_4 \.\.\. OK\s*$/m.test(text), 'connect_version_4 passed with a wrong fingerprint');
            return '';
        });
    }

    // 11. Real sender, fingerprint injected with --fp.
    if (opts.senderFp && fp) {
        await step('real-sender-fp', async () => {
            const before = countLogs('v4 sender introduced');
            const proc = harness.runProcess(opts.senderFp, ['--host', HOST, '--fp', fp, 'pause'], 20000);
            const seen = await waitUntil(() => countLogs('v4 sender introduced') > before, 15000);
            await killAfter(proc, 0);
            check(seen, 'no "v4 sender introduced" line within 15 s');
            return '';
        });

        await step('real-sender-wrong-fp', async () => {
            const before = countLogs('v4 sender introduced');
            const wrong = Buffer.alloc(32, 0x5a).toString('base64');
            const proc = harness.runProcess(opts.senderFp, ['--host', HOST, '--fp', wrong, 'pause'], 10000);
            await killAfter(proc, 10000);
            check(countLogs('v4 sender introduced') === before, 'a sender was introduced with a wrong fingerprint');
            return '';
        });
    }

    // 12. Real sender found through mDNS, fingerprint read from the TXT record.
    if (opts.sender) {
        await step('real-sender-mdns', async () => {
            const name = 'FCast-' + os.hostname();
            const before = countLogs('v4 sender introduced');
            const proc = harness.runProcess(opts.sender, ['--name', name, '--discovery-timeout', '10', 'pause'], 30000);
            const seen = await waitUntil(() => countLogs('v4 sender introduced') > before, 25000);
            await killAfter(proc, 0);
            check(seen, 'no "v4 sender introduced" line within 25 s');
            return 'name=' + name;
        });
    }
}

let opts: Options = null;
try {
    opts = parseArgs(process.argv.slice(2));
} catch (err) {
    out(reasonOf(err));
    out(usage());
    process.exit(2);
}
if (!opts.serviceDir) {
    out(usage());
    process.exit(2);
}

out('node ' + process.version);

main(opts)
    .catch((err) => {
        out('FAIL harness: ' + reasonOf(err));
    })
    .then(() => {
        const code = total > 0 && passed === total ? 0 : 1;
        process.stdout.write('RESULT ' + passed + '/' + total + '\n', () => process.exit(code));
    });
