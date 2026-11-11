export function isV4Capable(): boolean {
    try {
        const tls = require('tls');
        const crypto = require('crypto');
        return tls.DEFAULT_MAX_VERSION === 'TLSv1.3'
            && typeof crypto.generateKeyPairSync === 'function'
            && typeof crypto.createPublicKey === 'function'
            && typeof BigInt === 'function'
            && typeof TextDecoder === 'function'
            && process.env.FCAST_DISABLE_V4 !== '1';
    }
    catch (err) {
        return false;
    }
}

export function receiverMaxVersion(): number {
    return isV4Capable() ? 4 : 3;
}

export function runtimeDescription(): string {
    const maxTls = require('tls').DEFAULT_MAX_VERSION || 'none';
    return `node ${process.version} openssl ${process.versions.openssl} tls-max ${maxTls}`;
}
