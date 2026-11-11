import * as net from 'net';
import { ListenerService } from 'common/ListenerService';
import { FCastSession } from 'common/FCastSession';
import { Opcode, VersionMessage } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { getV4Context } from 'src/Main';
const logger = new Logger('TcpListenerService', LoggerType.BACKEND);

export class TcpListenerService extends ListenerService {
    public static readonly PORT = 46899;
    private server: net.Server;

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

    disconnect(sessionId: string) {
        this.sessionMap.get(sessionId)?.socket.destroy();
        this.sessionMap.delete(sessionId);
    }

    public getSenders(): string[] {
        const senders = [];
        this.sessionMap.forEach((sender) => { senders.push(sender.socket.remoteAddress); });
        return senders;
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

        const onData = (buffer: Buffer) => {
            try {
                const upgradeHead = session.processBytes(buffer);
                if (upgradeHead !== null) {
                    socket.removeListener("data", onData);
                    session.upgradeToTls(upgradeHead);
                }
            } catch (e) {
                logger.warn(`Error while handling packet from ${socket.remoteAddress}:${socket.remotePort}.`, e);
                if (session.isUpgradingToTls) {
                    socket.destroy();
                } else {
                    socket.end();
                }
            }
        };
        socket.on("data", onData);

        socket.on("close", () => {
            session.dispose();
            this.sessionMap.delete(session.sessionId);
            this.emitter.emit('disconnect', { sessionId: session.sessionId, type: 'tcp', data: { address: socket.remoteAddress, port: socket.remotePort }});
        });

        this.emitter.emit('connect', { sessionId: session.sessionId, type: 'tcp', data: { address: socket.remoteAddress, port: socket.remotePort }});
        try {
            const maxVersion = getV4Context() !== null ? 4 : 3;
            logger.info(`Sending version ${maxVersion}`);
            session.send(Opcode.Version, new VersionMessage(maxVersion));
        } catch (e) {
            logger.info('Failed to send version', e);
        }
    }
}
