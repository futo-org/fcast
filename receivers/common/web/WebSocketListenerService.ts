import { ListenerService } from 'common/ListenerService';
import { FCastSession } from 'common/FCastSession';
import { Opcode, PROTOCOL_VERSION, VersionMessage } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { WebSocket, WebSocketServer } from 'modules/ws';
const logger = new Logger('WebSocketListenerService', LoggerType.BACKEND);

export class WebSocketListenerService extends ListenerService {
    public static readonly PORT = 46898;
    private server: WebSocketServer;

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

    disconnect(sessionId: string) {
        this.sessionMap.get(sessionId)?.close();
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
            session.send(Opcode.Version, new VersionMessage(PROTOCOL_VERSION));
        } catch (e) {
            logger.info('Failed to send version', e);
        }
    }
}
