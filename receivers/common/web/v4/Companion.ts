import { ResourceSizeData } from './Decoder';
import { encodeCompanionResourceInfoRequest, encodeCompanionResourceRequest } from './Encoder';

// Largest byte range asked for in one CompanionResourceRequest. With the 512 KiB packet limit a
// range of this size is answered in a handful of parts, far below the 255 a response may have.
export const COMPANION_WINDOW_BYTES = 1024 * 1024;
export const COMPANION_REQUEST_TIMEOUT_MS = 10000;
const MAX_PROVIDER_ID = 0xFFFF;
const MAX_REQUEST_ID = 0xFFFFFFFF;
const RESOURCE_HEADER_BYTES = 7;

export class CompanionError extends Error {
    constructor(public readonly reason: 'not-found' | 'provider-gone' | 'timeout' | 'cancelled' | 'malformed', message: string) {
        super(message);
        this.name = 'CompanionError';
        Object.setPrototypeOf(this, CompanionError.prototype);
    }
}

export interface CompanionUrl {
    providerId: number;
    resourceId: number;
}

// Parses fcomp://<provider-id>.fcast/<resource-id>. Returns null for any other string.
export function parseCompanionUrl(url: string): CompanionUrl | null {
    const match = /^fcomp:\/\/(\d{1,5})\.fcast\/(\d{1,10})$/.exec(url);
    if (match === null) {
        return null;
    }
    const providerId = Number(match[1]);
    const resourceId = Number(match[2]);
    if (providerId > MAX_PROVIDER_ID || resourceId > MAX_REQUEST_ID) {
        return null;
    }
    return { providerId, resourceId };
}

export function isCompanionUrl(url: string | null | undefined): boolean {
    return typeof url === 'string' && url.indexOf('fcomp://') === 0;
}

export interface ResourcePart {
    requestId: number;
    part: number;
    totalParts: number;
    found: boolean;
    data: Buffer;
}

// Parses the body of a Resource packet: request id, part number, total parts, result variant and,
// on success, the bytes.
export function parseResourcePart(body: Buffer): ResourcePart {
    if (body.length < RESOURCE_HEADER_BYTES) {
        throw new CompanionError('malformed', 'resource packet too short');
    }
    const variant = body.readUInt8(6);
    if (variant !== 0 && variant !== 1) {
        throw new CompanionError('malformed', 'unknown resource result ' + variant);
    }
    if (variant === 0 && body.length !== RESOURCE_HEADER_BYTES) {
        throw new CompanionError('malformed', 'not-found result carries data');
    }
    return {
        requestId: body.readUInt32LE(0),
        part: body.readUInt8(4),
        totalParts: body.readUInt8(5),
        found: variant === 1,
        data: body.slice(RESOURCE_HEADER_BYTES),
    };
}

export interface ResourceInfo {
    contentType: string;
    size: number | null;
}

export interface ReadHandle {
    // Resolves with the number of bytes delivered once the last part has arrived.
    done: Promise<number>;
    cancel(): void;
}

interface PendingRequest {
    sessionId: string;
    timer: any;
    fail(error: CompanionError): void;
    onInfo?(info: ResourceInfo): void;
    onPart?(part: ResourcePart): void;
}

// Routes resource requests to the sender connection that owns a provider id and matches the
// answers by request id. It holds no resource data: parts are handed to the reader as they arrive.
export class CompanionBroker {
    private providerBySession = new Map<string, number>();
    private sessionByProvider = new Map<number, string>();
    private pending = new Map<number, PendingRequest>();
    private nextProviderId = 1;
    private nextRequestId = 1;

    constructor(
        private readonly send: (sessionId: string, bytes: Uint8Array) => void,
        private readonly timeoutMs: number = COMPANION_REQUEST_TIMEOUT_MS,
    ) {}

    // Returns the provider id of the connection, assigning one on first use. Null when all ids are
    // taken.
    registerProvider(sessionId: string): number | null {
        const existing = this.providerBySession.get(sessionId);
        if (existing !== undefined) {
            return existing;
        }
        if (this.sessionByProvider.size >= MAX_PROVIDER_ID) {
            return null;
        }
        while (this.sessionByProvider.has(this.nextProviderId)) {
            this.nextProviderId = this.nextProviderId >= MAX_PROVIDER_ID ? 1 : this.nextProviderId + 1;
        }
        const providerId = this.nextProviderId;
        this.nextProviderId = providerId >= MAX_PROVIDER_ID ? 1 : providerId + 1;
        this.providerBySession.set(sessionId, providerId);
        this.sessionByProvider.set(providerId, sessionId);
        return providerId;
    }

    hasProvider(providerId: number): boolean {
        return this.sessionByProvider.has(providerId);
    }

    // Forgets the connection's provider id and fails every request waiting on it.
    removeSession(sessionId: string): void {
        const providerId = this.providerBySession.get(sessionId);
        if (providerId !== undefined) {
            this.providerBySession.delete(sessionId);
            this.sessionByProvider.delete(providerId);
        }
        for (const [requestId, request] of [...this.pending.entries()]) {
            if (request.sessionId === sessionId) {
                this.finish(requestId);
                request.fail(new CompanionError('provider-gone', 'provider disconnected'));
            }
        }
    }

    getInfo(providerId: number, resourceId: number): Promise<ResourceInfo> {
        return new Promise<ResourceInfo>((resolve, reject) => {
            const sessionId = this.sessionByProvider.get(providerId);
            if (sessionId === undefined) {
                reject(new CompanionError('provider-gone', 'unknown provider ' + providerId));
                return;
            }
            const requestId = this.register({
                sessionId,
                timer: null,
                fail: reject,
                onInfo: resolve,
            });
            this.send(sessionId, encodeCompanionResourceInfoRequest(requestId, resourceId));
        });
    }

    // Requests the inclusive byte range and passes each part's bytes to `onData` in order.
    read(providerId: number, resourceId: number, start: number, stopInclusive: number, onData: (chunk: Buffer) => void): ReadHandle {
        let requestId: number | null = null;
        let rejectDone: (error: CompanionError) => void = () => undefined;

        const done = new Promise<number>((resolve, reject) => {
            rejectDone = reject;
            const sessionId = this.sessionByProvider.get(providerId);
            if (sessionId === undefined) {
                reject(new CompanionError('provider-gone', 'unknown provider ' + providerId));
                return;
            }

            let expectedPart = 0;
            let delivered = 0;
            const id = this.register({
                sessionId,
                timer: null,
                fail: reject,
                onPart: (part: ResourcePart) => {
                    if (!part.found) {
                        this.finish(id);
                        reject(new CompanionError('not-found', 'resource not found'));
                        return;
                    }
                    if (part.totalParts === 0 || part.part !== expectedPart || part.part >= part.totalParts) {
                        this.finish(id);
                        reject(new CompanionError('malformed', 'unexpected resource part ' + part.part + '/' + part.totalParts));
                        return;
                    }
                    expectedPart++;
                    delivered += part.data.length;
                    if (delivered > stopInclusive - start + 1) {
                        this.finish(id);
                        reject(new CompanionError('malformed', 'resource response larger than the requested range'));
                        return;
                    }
                    if (part.data.length > 0) {
                        onData(part.data);
                    }
                    if (expectedPart === part.totalParts) {
                        this.finish(id);
                        resolve(delivered);
                    } else {
                        this.restartTimer(id);
                    }
                },
            });
            requestId = id;
            this.send(sessionId, encodeCompanionResourceRequest(id, resourceId, start, stopInclusive));
        });

        return {
            done,
            cancel: () => {
                if (requestId !== null && this.pending.has(requestId)) {
                    this.finish(requestId);
                    rejectDone(new CompanionError('cancelled', 'request cancelled'));
                }
            },
        };
    }

    // Handles a CompanionResourceInfoResponse. Returns false when no request of that connection is
    // waiting for it.
    onInfoResponse(sessionId: string, requestId: number, contentType: string, resourceSize: ResourceSizeData | null): boolean {
        const request = this.pending.get(requestId);
        if (!request || request.sessionId !== sessionId || !request.onInfo) {
            return false;
        }
        this.finish(requestId);
        request.onInfo({
            contentType,
            size: resourceSize !== null && resourceSize.kind === 'known' ? resourceSize.size : null,
        });
        return true;
    }

    // Handles the body of a Resource packet. Returns false when no request of that connection is
    // waiting for it. Throws CompanionError when the body is malformed.
    onResource(sessionId: string, body: Buffer): boolean {
        const part = parseResourcePart(body);
        const request = this.pending.get(part.requestId);
        if (!request || request.sessionId !== sessionId || !request.onPart) {
            return false;
        }
        request.onPart(part);
        return true;
    }

    get pendingCount(): number {
        return this.pending.size;
    }

    private register(request: PendingRequest): number {
        while (this.pending.has(this.nextRequestId)) {
            this.nextRequestId = this.nextRequestId >= MAX_REQUEST_ID ? 1 : this.nextRequestId + 1;
        }
        const requestId = this.nextRequestId;
        this.nextRequestId = requestId >= MAX_REQUEST_ID ? 1 : requestId + 1;
        this.pending.set(requestId, request);
        this.restartTimer(requestId);
        return requestId;
    }

    private restartTimer(requestId: number): void {
        const request = this.pending.get(requestId);
        if (!request) {
            return;
        }
        clearTimeout(request.timer);
        request.timer = setTimeout(() => {
            if (this.pending.get(requestId) === request) {
                this.finish(requestId);
                request.fail(new CompanionError('timeout', 'provider did not answer in time'));
            }
        }, this.timeoutMs);
    }

    private finish(requestId: number): void {
        const request = this.pending.get(requestId);
        if (request) {
            clearTimeout(request.timer);
            this.pending.delete(requestId);
        }
    }
}

export interface CompanionStream {
    // Resolves with the number of bytes written once the range, or the resource, is exhausted.
    done: Promise<number>;
    cancel(): void;
}

// Reads a resource from `start` to `endInclusive` (or to its end when null) one window at a time and
// hands the bytes to `write`. The next window is requested only after `write` has settled, so at
// most one window is held in memory.
export function streamCompanionResource(
    broker: CompanionBroker,
    providerId: number,
    resourceId: number,
    start: number,
    endInclusive: number | null,
    write: (chunk: Buffer) => Promise<void> | void,
): CompanionStream {
    let cancelled = false;
    let current: ReadHandle | null = null;

    const run = async (): Promise<number> => {
        let position = start;
        while (endInclusive === null || position <= endInclusive) {
            if (cancelled) {
                throw new CompanionError('cancelled', 'request cancelled');
            }
            const stop = endInclusive === null ? position + COMPANION_WINDOW_BYTES - 1 : Math.min(position + COMPANION_WINDOW_BYTES - 1, endInclusive);
            const chunks: Buffer[] = [];
            current = broker.read(providerId, resourceId, position, stop, (chunk) => chunks.push(chunk));
            const received = await current.done;
            current = null;
            for (const chunk of chunks) {
                if (cancelled) {
                    throw new CompanionError('cancelled', 'request cancelled');
                }
                await write(chunk);
            }
            position += received;
            if (received < stop - (position - received) + 1) {
                break;
            }
        }
        return position - start;
    };

    return {
        done: run(),
        cancel: () => {
            cancelled = true;
            if (current !== null) {
                current.cancel();
            }
        },
    };
}
