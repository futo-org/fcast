import * as url from 'url';
import { http, https } from 'modules/follow-redirects';
import { CompanionBroker, isCompanionUrl, parseCompanionUrl, streamCompanionResource } from './Companion';

export const MAX_SUBTITLE_BYTES = 4 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 10000;

// Converts SubRip text to WebVTT. Text that already is WebVTT is returned unchanged.
export function toWebVtt(text: string): string {
    const normalized = text.replace(/^\uFEFF/, '').replace(/\r\n?/g, '\n');
    if (/^WEBVTT/.test(normalized)) {
        return normalized;
    }
    const cues = normalized.replace(/(\d{2}:\d{2}:\d{2}),(\d{3})/g, '$1.$2');
    return 'WEBVTT\n\n' + cues;
}

function httpGet(target: string, headers: { [key: string]: string } | null, maxBytes: number): Promise<{ status: number; body: Buffer }> {
    return new Promise((resolve, reject) => {
        const protocol = target.indexOf('https') === 0 ? https : http;
        const options: any = { ...url.parse(target), headers: headers || {} };
        const request = protocol.get(options, (response: any) => {
            const chunks: Buffer[] = [];
            let size = 0;
            response.on('data', (chunk: Buffer) => {
                size += chunk.length;
                if (size > maxBytes) {
                    request.abort();
                    reject(new Error('response larger than ' + maxBytes + ' bytes'));
                    return;
                }
                chunks.push(chunk);
            });
            response.on('end', () => resolve({ status: response.statusCode, body: Buffer.concat(chunks) }));
            response.on('error', reject);
        });
        request.setTimeout(REQUEST_TIMEOUT_MS, () => {
            request.abort();
            reject(new Error('request timed out'));
        });
        request.on('error', reject);
    });
}

function readCompanion(broker: CompanionBroker, target: string, endInclusive: number | null, maxBytes: number): Promise<Buffer> {
    const parsed = parseCompanionUrl(target);
    if (parsed === null) {
        return Promise.reject(new Error('invalid companion url'));
    }
    const chunks: Buffer[] = [];
    let size = 0;
    const stream = streamCompanionResource(broker, parsed.providerId, parsed.resourceId, 0, endInclusive, (chunk) => {
        size += chunk.length;
        if (size > maxBytes) {
            stream.cancel();
            throw new Error('resource larger than ' + maxBytes + ' bytes');
        }
        chunks.push(chunk);
    });
    return stream.done.then(() => Buffer.concat(chunks));
}

// Retrieves a subtitle file over HTTP or from a companion provider and returns it as WebVTT text.
export function fetchSubtitle(target: string, broker: CompanionBroker): Promise<string> {
    if (isCompanionUrl(target)) {
        return readCompanion(broker, target, null, MAX_SUBTITLE_BYTES).then((body) => toWebVtt(body.toString('utf8')));
    }
    return httpGet(target, null, MAX_SUBTITLE_BYTES).then((response) => {
        if (response.status < 200 || response.status >= 300) {
            throw new Error('subtitle request answered ' + response.status);
        }
        return toWebVtt(response.body.toString('utf8'));
    });
}

// Tells whether the first byte of the media can be retrieved.
export function isReachable(target: string, headers: { [key: string]: string } | null, broker: CompanionBroker): Promise<boolean> {
    if (isCompanionUrl(target)) {
        return readCompanion(broker, target, 0, 1).then(() => true, () => false);
    }
    return new Promise((resolve) => {
        const protocol = target.indexOf('https') === 0 ? https : http;
        const options: any = { ...url.parse(target), headers: { ...(headers || {}), Range: 'bytes=0-0' } };
        const request = protocol.get(options, (response: any) => {
            request.abort();
            resolve(response.statusCode >= 200 && response.statusCode < 400);
        });
        request.setTimeout(REQUEST_TIMEOUT_MS, () => {
            request.abort();
            resolve(false);
        });
        request.on('error', () => resolve(false));
    });
}
