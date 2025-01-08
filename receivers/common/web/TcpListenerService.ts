import * as net from 'net';
import { FCastSession, Opcode } from 'common/FCastSession';
import { EventEmitter } from 'events';
import { Main, errorHandler } from 'src/Main';
import { v4 as uuidv4 } from 'modules/uuid';

export class TcpListenerService {
    public static PORT = 46899;
    private static TIMEOUT = 2500;

    emitter = new EventEmitter();

    private server: net.Server;
    private sessions: FCastSession[] = [];

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

    send(opcode: number, message = null) {
        // Main.logger.info(`Sending message ${JSON.stringify(message)}`);
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

    private handleConnection(socket: net.Socket) {
        Main.logger.info(`new connection from ${socket.remoteAddress}:${socket.remotePort}`);

        const session = new FCastSession(socket, (data) => socket.write(data));
        session.bindEvents(this.emitter);
        this.sessions.push(session);

        const connectionId = uuidv4();
        let heartbeatRetries = 0;
        socket.setTimeout(TcpListenerService.TIMEOUT);
        socket.on('timeout', () => {
            try {
                if (heartbeatRetries > 3) {
                    Main.logger.warn(`Could not ping device ${socket.remoteAddress}:${socket.remotePort}. Disconnecting...`);
                    socket.destroy();
                }

                heartbeatRetries += 1;
                session.send(Opcode.Ping);
                this.emitter.emit('ping', { id: connectionId });
            } catch (e) {
                Main.logger.warn(`Error while pinging sender device ${socket.remoteAddress}:${socket.remotePort}.`, e);
                socket.destroy();
            }
        });

        socket.on("error", (err) => {
            Main.logger.warn(`Error from ${socket.remoteAddress}:${socket.remotePort}.`, err);
            socket.destroy();
        });

        socket.on("data", buffer => {
            try {
                heartbeatRetries = 0;
                session.processBytes(buffer);
            } catch (e) {
                Main.logger.warn(`Error while handling packet from ${socket.remoteAddress}:${socket.remotePort}.`, e);
                socket.end();
            }
        });

        socket.on("close", () => {
            const index = this.sessions.indexOf(session);
            if (index != -1) {
                this.sessions.splice(index, 1);
            }
            this.emitter.emit('disconnect', { id: connectionId, type: 'tcp', data: { address: socket.remoteAddress, port: socket.remotePort }});
        });

        this.emitter.emit('connect', { id: connectionId, type: 'tcp', data: { address: socket.remoteAddress, port: socket.remotePort }});

        try {
            Main.logger.info('Sending version');
            session.send(Opcode.Version, {version: 2});
        } catch (e) {
            Main.logger.info('Failed to send version', e);
        }
    }
}
