import { FCastSession } from 'common/FCastSession';
import { Opcode } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { EventEmitter } from 'events';
import { WebSocket, WebSocketServer } from 'modules/ws';
import { errorHandler } from 'src/Main';
const logger = new Logger('WebSocketListenerService', LoggerType.BACKEND);

export class WebSocketListenerService {
    public static PORT = 46898;

    emitter = new EventEmitter();

    private server: WebSocketServer;
    private sessionMap = new Map();

    start() {
        if (this.server != null) {
            return;
        }

        this.server = new WebSocketServer({ port: WebSocketListenerService.PORT })
            .on("connection", this.handleConnection.bind(this))
            .on("error", this.handleServerError.bind(this));
    }

    stop() {
        if (this.server == null) {
            return;
        }

        const server = this.server;
        this.server = null;

        server.close();
    }

    send(opcode: number, message = null, sessionId = null) {
        if (sessionId) {
            try {
                this.sessionMap.get(sessionId)?.send(opcode, message);
            } catch (e) {
                logger.warn("Failed to send error.", e);
                this.sessionMap.get(sessionId)?.close();
            }
        }
        else {
            for (const session of this.sessionMap.values()) {
                try {
                    session.send(opcode, message);
                } catch (e) {
                    logger.warn("Failed to send error.", e);
                    session.close();
                }
            }
        }
    }

    disconnect(sessionId: string) {
        this.sessionMap.get(sessionId)?.close();
    }

    public getSessions(): string[] {
        return [...this.sessionMap.keys()];
    }

    private async handleServerError(err: NodeJS.ErrnoException) {
        errorHandler(err);
    }

    private handleConnection(socket: WebSocket, request: any) {
        logger.info('New WebSocket connection');

        const session = new FCastSession(socket, (data) => socket.send(data));
        session.bindEvents(this.emitter);
        this.sessionMap.set(session.sessionId, session);

        socket.on("error", (err) => {
            logger.warn(`Error.`, err);
            this.disconnect(session.sessionId);
        });

        socket.on('message', data => {
            try {
                if (data instanceof Buffer) {
                    session.processBytes(data);
                } else {
                    logger.warn("Received unhandled string message", data);
                }
            } catch (e) {
                logger.warn(`Error while handling packet.`, e);
                session.close();
            }
        });

        socket.on("close", () => {
            this.sessionMap.delete(session.sessionId);
            this.emitter.emit('disconnect', { sessionId: session.sessionId, type: 'ws', data: { url: socket.url }});
        });

        this.emitter.emit('connect', { sessionId: session.sessionId, type: 'ws', data: { url: socket.url }});
        try {
            logger.info('Sending version');
            session.send(Opcode.Version, {version: 2});
        } catch (e) {
            logger.info('Failed to send version');
        }
    }
}
