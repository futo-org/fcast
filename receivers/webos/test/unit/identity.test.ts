import * as assert from 'assert';
import * as crypto from 'crypto';
import * as fs from 'fs';
import * as net from 'net';
import * as os from 'os';
import * as path from 'path';
import * as tls from 'tls';
import * as forge from 'node-forge';
import { test } from './runner';
import { loadOrCreateIdentity, spkiFingerprintFromCertPem } from 'common/v4/Identity';
import { createV4Context } from 'common/v4/V4Context';

const KEY_FILE = 'receiver-key.pem';
const CERT_FILE = 'receiver-cert.pem';
const HANDSHAKE_TIMEOUT_MS = 10000;

function makeTempRoot(): string {
    return fs.mkdtempSync(path.join(os.tmpdir(), 'fcast-identity-'));
}

function removeTree(target: string): void {
    if (!fs.existsSync(target)) {
        return;
    }
    if (fs.statSync(target).isDirectory()) {
        for (const entry of fs.readdirSync(target)) {
            removeTree(path.join(target, entry));
        }
        fs.rmdirSync(target);
    }
    else {
        fs.unlinkSync(target);
    }
}

function spkiSha256(spki: Buffer): string {
    return crypto.createHash('sha256').update(spki).digest('base64');
}

function peerSpkiFingerprint(certDer: Buffer): string {
    const X509Certificate = (crypto as any).X509Certificate;
    if (typeof X509Certificate === 'function') {
        const spki = new X509Certificate(certDer).publicKey.export({ type: 'spki', format: 'der' });
        return spkiSha256(spki);
    }
    const cert = forge.pki.certificateFromAsn1(forge.asn1.fromDer(certDer.toString('binary')));
    const spki = crypto.createPublicKey(forge.pki.publicKeyToPem(cert.publicKey)).export({ type: 'spki', format: 'der' }) as Buffer;
    return spkiSha256(spki);
}

test('identity: first call creates key and cert files and a matching fingerprint', () => {
    const root = makeTempRoot();
    try {
        const dir = path.join(root, 'identity');
        const identity = loadOrCreateIdentity([{ path: dir, persistent: true }]);

        const keyPath = path.join(dir, KEY_FILE);
        const certPath = path.join(dir, CERT_FILE);
        assert.ok(fs.existsSync(keyPath));
        assert.ok(fs.existsSync(certPath));
        if (process.platform !== 'win32') {
            assert.strictEqual(fs.statSync(keyPath).mode & 0o777, 0o600);
        }

        assert.strictEqual(identity.directory, dir);
        assert.strictEqual(identity.persistent, true);
        assert.strictEqual(identity.created, true);
        assert.strictEqual(identity.fingerprint, spkiFingerprintFromCertPem(identity.certPem));

        const spki = crypto.createPublicKey(identity.keyPem).export({ type: 'spki', format: 'der' }) as Buffer;
        assert.strictEqual(identity.fingerprint, spkiSha256(spki));
    }
    finally {
        removeTree(root);
    }
});

test('identity: second call reuses the stored identity', () => {
    const root = makeTempRoot();
    try {
        const candidates = [{ path: path.join(root, 'identity'), persistent: true }];
        const first = loadOrCreateIdentity(candidates);
        const second = loadOrCreateIdentity(candidates);

        assert.strictEqual(second.created, false);
        assert.strictEqual(second.fingerprint, first.fingerprint);
        assert.strictEqual(second.directory, first.directory);
    }
    finally {
        removeTree(root);
    }
});

test('identity: corrupt cert file is regenerated with a new fingerprint', () => {
    const root = makeTempRoot();
    try {
        const dir = path.join(root, 'identity');
        const first = loadOrCreateIdentity([{ path: dir, persistent: true }]);
        fs.writeFileSync(path.join(dir, CERT_FILE), 'not a certificate');

        const repaired = loadOrCreateIdentity([{ path: dir, persistent: true }]);
        assert.strictEqual(repaired.created, true);
        assert.notStrictEqual(repaired.fingerprint, first.fingerprint);
        assert.strictEqual(repaired.fingerprint, spkiFingerprintFromCertPem(repaired.certPem));
    }
    finally {
        removeTree(root);
    }
});

test('identity: unwritable first candidate falls back to the second', () => {
    const root = makeTempRoot();
    try {
        fs.writeFileSync(path.join(root, 'file.txt'), 'plain file');
        const fallback = path.join(root, 'fallback');

        const identity = loadOrCreateIdentity([
            { path: path.join(root, 'file.txt', 'sub'), persistent: true },
            { path: fallback, persistent: false },
        ]);

        assert.strictEqual(identity.directory, fallback);
        assert.strictEqual(identity.persistent, false);
        assert.strictEqual(identity.created, true);
        assert.ok(fs.existsSync(path.join(fallback, KEY_FILE)));
    }
    finally {
        removeTree(root);
    }
});

test('identity: all candidates unwritable yields an in-memory identity', () => {
    const root = makeTempRoot();
    try {
        fs.writeFileSync(path.join(root, 'file.txt'), 'plain file');

        const identity = loadOrCreateIdentity([
            { path: path.join(root, 'file.txt', 'a'), persistent: true },
            { path: path.join(root, 'file.txt', 'b'), persistent: true },
        ]);

        assert.strictEqual(identity.directory, null);
        assert.strictEqual(identity.persistent, false);
        assert.strictEqual(identity.created, true);
        assert.strictEqual(identity.fingerprint, spkiFingerprintFromCertPem(identity.certPem));
    }
    finally {
        removeTree(root);
    }
});

test('identity: TLS 1.3 server built with createV4Context is pinned by SPKI fingerprint', async () => {
    const root = makeTempRoot();
    const sockets: net.Socket[] = [];
    let server: net.Server = null;

    try {
        const identity = loadOrCreateIdentity([{ path: path.join(root, 'identity'), persistent: true }]);
        const v4 = createV4Context(identity, new Uint8Array([0]));

        server = net.createServer((raw) => {
            sockets.push(raw);
            const serverSocket = new tls.TLSSocket(raw, { isServer: true, secureContext: v4.secureContext });
            serverSocket.on('error', () => undefined);
        });

        await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', () => resolve()));
        const port = (server.address() as net.AddressInfo).port;

        const peer = await new Promise<{ protocol: string, raw: Buffer }>((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error('TLS handshake timed out')), HANDSHAKE_TIMEOUT_MS);
            const client = tls.connect({
                host: '127.0.0.1',
                port: port,
                rejectUnauthorized: false,
                minVersion: 'TLSv1.3',
                maxVersion: 'TLSv1.3',
            }, () => {
                clearTimeout(timer);
                const certificate = client.getPeerCertificate(true);
                resolve({ protocol: client.getProtocol(), raw: certificate.raw });
                client.destroy();
            });
            client.on('error', (err) => {
                clearTimeout(timer);
                reject(err);
            });
        });

        assert.strictEqual(peer.protocol, 'TLSv1.3');
        assert.strictEqual(peerSpkiFingerprint(peer.raw), identity.fingerprint);
    }
    finally {
        sockets.forEach((socket) => socket.destroy());
        if (server) {
            server.close();
        }
        removeTree(root);
    }
});
