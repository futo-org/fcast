export interface ConnectionService {
    port: number;
    type: number;
}

export interface ConnectionInfo {
    name: string;
    addresses: string[];
    services: ConnectionService[];
    txt?: Record<string, string>;
}

// Node fallback for runtimes without btoa, webOS browsers always provide btoa
declare const Buffer: { from(data: string, encoding: string): { toString(encoding: string): string } };

const FCAST_TCP_PORT = 46899;
const FCAST_TCP_SERVICE_TYPE = 0;
const CONNECTION_URL_PREFIX = 'fcast://r/';

export function buildConnectionInfo(name: string, addresses: string[], txt: Record<string, string> | null): ConnectionInfo {
    const info: ConnectionInfo = {
        name: name,
        addresses: addresses,
        services: [{ port: FCAST_TCP_PORT, type: FCAST_TCP_SERVICE_TYPE }],
    };
    if (txt) {
        info.txt = txt;
    }
    return info;
}

export function buildConnectionUrl(name: string, addresses: string[], txt: Record<string, string> | null): string {
    const json = JSON.stringify(buildConnectionInfo(name, addresses, txt));
    const base64 = binaryToBase64(utf8ToBinary(json));
    return CONNECTION_URL_PREFIX + base64.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function utf8ToBinary(text: string): string {
    if (typeof TextEncoder === 'function') {
        const bytes = new TextEncoder().encode(text);
        let binary = '';
        for (let i = 0; i < bytes.length; i++) {
            binary += String.fromCharCode(bytes[i]);
        }
        return binary;
    }
    return unescape(encodeURIComponent(text));
}

function binaryToBase64(binary: string): string {
    if (typeof btoa === 'function') {
        return btoa(binary);
    }
    return Buffer.from(binary, 'binary').toString('base64');
}
