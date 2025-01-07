import { FCastSession, Opcode } from 'common/FCastSession';
import { EventEmitter } from 'events';
import { WebSocket, WebSocketServer } from 'modules/ws';
import { Main, errorHandler } from 'src/Main';
import { v4 as uuidv4 } from 'modules/uuid';

export class WebSocketListenerService {
    public static PORT = 46898;
    private static TIMEOUT = 2500;

    emitter = new EventEmitter();

    private server: WebSocketServer;
    private sessions: FCastSession[] = [];

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

    send(opcode: number, message = null) {
        this.sessions.forEach(session => {
            try {
                session.send(opcode, message);
            } catch (e) {
                Main.logger.warn("Failed to send error.", e);
                session.close();
            }
        });
    }

    private async handleServerError(err: NodeJS.ErrnoException) {
        errorHandler(err);
    }

    private handleConnection(socket: WebSocket) {
        Main.logger.info('New WebSocket connection');

        const session = new FCastSession(socket, (data) => socket.send(data));
        session.bindEvents(this.emitter);
        this.sessions.push(session);

        const connectionId = uuidv4();
        let heartbeatRetries = 0;
        socket.setTimeout(WebSocketListenerService.TIMEOUT);
        socket.on('timeout', () => {
            try {
                if (heartbeatRetries > 3) {
                    Main.logger.warn(`Could not ping device ${socket.remoteAddress}:${socket.remotePort}. Disconnecting...`);
                    socket.destroy();
                }

                heartbeatRetries += 1;
                session.send(Opcode.Ping);
            } catch (e) {
                Main.logger.warn(`Error while pinging sender device ${socket.remoteAddress}:${socket.remotePort}.`, e);
                socket.destroy();
            }
        });

        socket.on("error", (err) => {
            Main.logger.warn(`Error.`, err);
            session.close();
        });

        socket.on('message', data => {
            try {
                heartbeatRetries = 0;
                if (data instanceof Buffer) {
                    session.processBytes(data);
                } else {
                    Main.logger.warn("Received unhandled string message", data);
                }
            } catch (e) {
                Main.logger.warn(`Error while handling packet.`, e);
                session.close();
            }
        });

        socket.on("close", () => {
            Main.logger.info('WebSocket connection closed');

            const index = this.sessions.indexOf(session);
            if (index != -1) {
                this.sessions.splice(index, 1);
            }
            this.emitter.emit('disconnect', { id: connectionId, type: 'ws', data: { url: socket.url() }});
        });

        this.emitter.emit('connect', { id: connectionId, type: 'ws', data: { url: socket.url() }});

        try {
            Main.logger.info('Sending version');
            session.send(Opcode.Version, {version: 2});
        } catch (e) {
            Main.logger.info('Failed to send version');
        }
    }
}
