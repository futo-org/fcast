import * as net from 'net';
import { FCastSession } from 'common/FCastSession';
import { Opcode } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { EventEmitter } from 'events';
import { errorHandler } from 'src/Main';
const logger = new Logger('TcpListenerService', LoggerType.BACKEND);

export class TcpListenerService {
    public static PORT = 46899;
    emitter = new EventEmitter();

    private server: net.Server;
    private sessionMap = new Map();

    start() {
        if (this.server != null) {
            return;
        }

        this.server = net.createServer()
            .listen(TcpListenerService.PORT)
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
        // logger.info(`Sending message ${JSON.stringify(message)}`);

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
        this.sessionMap.get(sessionId)?.socket.destroy();
        this.sessionMap.delete(sessionId);
    }

    public getSenders(): string[] {
        const senders = [];
        this.sessionMap.forEach((sender) => { senders.push(sender.socket.remoteAddress); });
        return senders;
    }

    public getSessions(): string[] {
        return [...this.sessionMap.keys()];
    }

    private async handleServerError(err: NodeJS.ErrnoException) {
        errorHandler(err);
    }

    private handleConnection(socket: net.Socket) {
        logger.info(`New connection from ${socket.remoteAddress}:${socket.remotePort}`);

        const session = new FCastSession(socket, (data) => socket.write(data));
        session.bindEvents(this.emitter);
        this.sessionMap.set(session.sessionId, session);

        socket.on("error", (err) => {
            logger.warn(`Error from ${socket.remoteAddress}:${socket.remotePort}.`, err);
            this.disconnect(session.sessionId);
        });

        socket.on("data", buffer => {
            try {
                session.processBytes(buffer);
            } catch (e) {
                logger.warn(`Error while handling packet from ${socket.remoteAddress}:${socket.remotePort}.`, e);
                socket.end();
            }
        });

        socket.on("close", () => {
            this.sessionMap.delete(session.sessionId);
            this.emitter.emit('disconnect', { sessionId: session.sessionId, type: 'tcp', data: { address: socket.remoteAddress, port: socket.remotePort }});
        });

        this.emitter.emit('connect', { sessionId: session.sessionId, type: 'tcp', data: { address: socket.remoteAddress, port: socket.remotePort }});
        try {
            logger.info('Sending version');
            session.send(Opcode.Version, {version: 2});
        } catch (e) {
            logger.info('Failed to send version', e);
        }
    }
}
