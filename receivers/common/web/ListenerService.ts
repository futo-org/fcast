import { FCastSession } from 'common/FCastSession';
import { Opcode, EventSubscribeObject, EventObject, EventType, KeyEvent, KeyDownEvent, KeyUpEvent } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { deepEqual } from 'common/UtilityBackend';
import { EventEmitter } from 'events';
import { errorHandler } from 'src/Main';
const logger = new Logger('ListenerService', LoggerType.BACKEND);

export abstract class ListenerService {
    public emitter: EventEmitter = new EventEmitter();
    protected sessionMap: Map<string, FCastSession> = new Map();
    private eventSubscribers: Map<string, EventSubscribeObject[]> = new Map();

    public abstract start(): void;
    public abstract stop(): void;
    public abstract disconnect(sessionId: string): void;

    public send(opcode: number, message = null, sessionId = null) {
        // logger.info(`Sending message ${JSON.stringify(message)}`);

        if (sessionId) {
            if (opcode === Opcode.Event.valueOf() && !this.isSubscribedToEvent(sessionId, message.event)) {
                return;
            }

            try {
                this.sessionMap.get(sessionId)?.send(opcode, message);
            } catch (e) {
                logger.warn("Failed to send error.", e);
                this.sessionMap.get(sessionId)?.close();
            }
        }
        else {
            for (const session of this.sessionMap.values()) {
                if (opcode === Opcode.Event.valueOf() && !this.isSubscribedToEvent(session.sessionId, message.event)) {
                    continue;
                }

                try {
                    session.send(opcode, message);
                } catch (e) {
                    logger.warn("Failed to send error.", e);
                    session.close();
                }
            }
        }
    }

    // Sends a v4 Flatbuf packet to the given session, or to every v4 session when no id is given.
    public sendV4(bytes: Uint8Array, sessionId: string = null) {
        const sessions = sessionId ? [this.sessionMap.get(sessionId)] : [...this.sessionMap.values()];

        for (const session of sessions) {
            if (!session) {
                continue;
            }

            try {
                session.sendFlatbuf(bytes);
            } catch (e) {
                logger.warn("Failed to send v4 message.", e);
                session.close();
            }
        }
    }

    public subscribeEvent(sessionId: string, event: EventSubscribeObject) {
        if (!this.eventSubscribers.has(sessionId)) {
            this.eventSubscribers.set(sessionId, []);
        }

        let sessionSubscriptions = this.eventSubscribers.get(sessionId);
        sessionSubscriptions.push(event);
        this.eventSubscribers.set(sessionId, sessionSubscriptions);
    }

    public unsubscribeEvent(sessionId: string, event: EventSubscribeObject) {
        if (this.eventSubscribers.has(sessionId)) {
            let sessionSubscriptions = this.eventSubscribers.get(sessionId);

            const index = sessionSubscriptions.findIndex((obj) => deepEqual(obj, event));
            if (index != -1) {
                sessionSubscriptions.splice(index, 1);
            }

            this.eventSubscribers.set(sessionId, sessionSubscriptions);
        }
    }

    public getSessions(): string[] {
        return [...this.sessionMap.keys()];
    }

    // Ids of the sessions that completed the v4 handshake.
    public getV4Sessions(): string[] {
        return [...this.sessionMap.values()].filter((session) => session.isV4Secure).map((session) => session.sessionId);
    }

    // Ids of the sessions that speak protocol version 1 to 3.
    public getLegacySessions(): string[] {
        return [...this.sessionMap.values()].filter((session) => session.protocolVersion < 4 && !session.isUpgradingToTls).map((session) => session.sessionId);
    }

    public getSessionProtocolVersion(sessionId: string) {
        return this.sessionMap.get(sessionId)?.protocolVersion;
    }

    public isSessionPingable(sessionId: string): boolean {
        return this.sessionMap.get(sessionId)?.canPing ?? false;
    }

    public getAllSubscribedKeys(): { keyDown: Set<string>, keyUp: Set<string> } {
        let keyDown = new Set<string>();
        let keyUp = new Set<string>();

        for (const session of this.eventSubscribers.values()) {
            for (const event of session) {
                switch (event.type) {
                    case EventType.KeyDown:
                        keyDown = new Set([...keyDown, ...(event as KeyDownEvent).keys]);
                        break;

                    case EventType.KeyUp:
                        keyUp = new Set([...keyUp, ...(event as KeyUpEvent).keys]);
                        break;

                    default:
                        break;
                }
            }
        }

        return { keyDown: keyDown, keyUp: keyUp };
    }

    private isSubscribedToEvent(sessionId: string, event: EventObject): boolean {
        let isSubscribed = false;

        if (this.eventSubscribers.has(sessionId)) {
            // Iterates the array itself: Array.prototype.values() is missing in the Node.js of webOS 5 and 6.
            for (const e of this.eventSubscribers.get(sessionId)) {
                if (e.type === event.type) {
                    if (e.type === EventType.KeyDown.valueOf() || e.type === EventType.KeyUp.valueOf()) {
                        const subscribeEvent = e.type === EventType.KeyDown.valueOf() ? e as KeyDownEvent : e as KeyUpEvent;
                        const keyEvent = event as KeyEvent;

                        if (!subscribeEvent.keys.includes(keyEvent.key)) {
                            continue;
                        }
                    }

                    isSubscribed = true;
                    break;
                }
            }
        }

        return isSubscribed;
    }

    protected async handleServerError(err: NodeJS.ErrnoException) {
        errorHandler(err);
    }
}
