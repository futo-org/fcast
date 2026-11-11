import * as crypto from 'crypto';
import * as fs from 'fs';
import * as path from 'path';
import * as forge from 'node-forge';

const KEY_FILE_NAME = 'receiver-key.pem';
const CERT_FILE_NAME = 'receiver-cert.pem';
const COMMON_NAME = 'FCast Receiver';
const KEY_FILE_MODE = 0o600;
const CERT_FILE_MODE = 0o644;
const ONE_DAY_MS = 24 * 60 * 60 * 1000;
const VALIDITY_YEARS = 20;

export interface ReceiverIdentity {
    keyPem: string;
    certPem: string;
    fingerprint: string;
    directory: string | null;
    persistent: boolean;
    created: boolean;
}

interface IdentityCandidate {
    path: string;
    persistent: boolean;
}

interface IdentityMaterial {
    keyPem: string;
    certPem: string;
    fingerprint: string;
}

export function loadOrCreateIdentity(candidateDirs: IdentityCandidate[]): ReceiverIdentity {
    let generated: IdentityMaterial = null;

    for (const candidate of candidateDirs) {
        const keyPath = path.join(candidate.path, KEY_FILE_NAME);
        const certPath = path.join(candidate.path, CERT_FILE_NAME);

        if (fs.existsSync(keyPath) && fs.existsSync(certPath)) {
            const existing = readIdentity(keyPath, certPath);
            if (existing) {
                return toReceiverIdentity(existing, candidate, false);
            }
        }

        if (!generated) {
            generated = generateIdentity();
        }

        try {
            ensureDirectory(candidate.path);
            writeFileAtomic(keyPath, generated.keyPem, KEY_FILE_MODE);
            writeFileAtomic(certPath, generated.certPem, CERT_FILE_MODE);
            return toReceiverIdentity(generated, candidate, true);
        }
        catch (err) {
            continue;
        }
    }

    if (!generated) {
        generated = generateIdentity();
    }
    return { ...generated, directory: null, persistent: false, created: true };
}

export function spkiFingerprintFromCertPem(certPem: string): string {
    const cert = forge.pki.certificateFromPem(certPem);
    return sha256Base64(spkiDerFromPem(forge.pki.publicKeyToPem(cert.publicKey)));
}

function toReceiverIdentity(material: IdentityMaterial, candidate: IdentityCandidate, created: boolean): ReceiverIdentity {
    return {
        keyPem: material.keyPem,
        certPem: material.certPem,
        fingerprint: material.fingerprint,
        directory: candidate.path,
        persistent: candidate.persistent,
        created: created,
    };
}

function readIdentity(keyPath: string, certPath: string): IdentityMaterial | null {
    try {
        const keyPem = fs.readFileSync(keyPath, 'utf8');
        const certPem = fs.readFileSync(certPath, 'utf8');

        crypto.createPrivateKey(keyPem);
        const privateSpki = spkiDerFromPem(keyPem);
        const cert = forge.pki.certificateFromPem(certPem);
        const certSpki = spkiDerFromPem(forge.pki.publicKeyToPem(cert.publicKey));
        if (!privateSpki.equals(certSpki)) {
            return null;
        }

        const now = Date.now();
        if (now < cert.validity.notBefore.getTime() || now > cert.validity.notAfter.getTime()) {
            return null;
        }

        return { keyPem: keyPem, certPem: certPem, fingerprint: sha256Base64(privateSpki) };
    }
    catch (err) {
        return null;
    }
}

function generateIdentity(): IdentityMaterial {
    const pair = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const pkcs1Pem = pair.privateKey.export({ type: 'pkcs1', format: 'pem' }) as string;
    const forgePrivateKey = forge.pki.privateKeyFromPem(pkcs1Pem);

    const now = Date.now();
    const notAfter = new Date(now);
    notAfter.setFullYear(notAfter.getFullYear() + VALIDITY_YEARS);

    const cert = forge.pki.createCertificate();
    cert.publicKey = forge.pki.setRsaPublicKey(forgePrivateKey.n, forgePrivateKey.e);
    cert.serialNumber = randomSerialNumber();
    cert.validity.notBefore = new Date(now - ONE_DAY_MS);
    cert.validity.notAfter = notAfter;
    const subject = [{ name: 'commonName', value: COMMON_NAME }];
    cert.setSubject(subject);
    cert.setIssuer(subject);
    cert.sign(forgePrivateKey, forge.md.sha256.create());

    const publicSpki = pair.publicKey.export({ type: 'spki', format: 'der' }) as Buffer;
    return {
        keyPem: pair.privateKey.export({ type: 'pkcs8', format: 'pem' }) as string,
        certPem: forge.pki.certificateToPem(cert),
        fingerprint: sha256Base64(publicSpki),
    };
}

function randomSerialNumber(): string {
    const bytes = crypto.randomBytes(16);
    bytes[0] &= 0x7f;
    if (bytes[0] === 0) {
        bytes[0] = 0x01;
    }
    return bytes.toString('hex');
}

function spkiDerFromPem(pem: string): Buffer {
    return crypto.createPublicKey(pem).export({ type: 'spki', format: 'der' }) as Buffer;
}

function sha256Base64(data: Buffer): string {
    return crypto.createHash('sha256').update(data).digest('base64');
}

function ensureDirectory(dir: string): void {
    if (fs.existsSync(dir)) {
        if (!fs.statSync(dir).isDirectory()) {
            throw new Error(`Not a directory: ${dir}`);
        }
        return;
    }

    const parent = path.dirname(dir);
    if (parent !== dir) {
        ensureDirectory(parent);
    }
    fs.mkdirSync(dir);
}

function writeFileAtomic(filePath: string, content: string, mode: number): void {
    const tempPath = `${filePath}.${process.pid}.${crypto.randomBytes(4).toString('hex')}.tmp`;
    try {
        fs.writeFileSync(tempPath, content, { mode: mode });
        fs.renameSync(tempPath, filePath);
    }
    catch (err) {
        if (fs.existsSync(tempPath)) {
            fs.unlinkSync(tempPath);
        }
        throw err;
    }
}
