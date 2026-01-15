import * as net from 'net';
const crypto = require('crypto');
const { Buffer } = require('node:buffer');
import { ListenerService } from 'common/ListenerService';
import { FCastSession } from 'common/FCastSession';
import { Opcode, PROTOCOL_VERSION, VersionMessage } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('TcpListenerService', LoggerType.BACKEND);

export class TcpListenerService extends ListenerService {
    public static readonly PORT = 46899;
    private server: net.Server;
    private encKey: Buffer | null;

    start(password: string | null = null) {
        if (this.server != null) {
            return;
        }

        this.setPassword(password);

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

    setPassword(password: string | null) {
        logger.debug('Setting encryption password, closing existing sessions');

        for (const sessionId in this.sessionMap) {
            this.disconnect(sessionId);
        }

        if (!password || password.length == 0) {
            this.encKey = null;
            return;
        }

        const salt = 'FCAST_SALT';
        const iters = 100000;
        const keyLen = 32;

        crypto.pbkdf2(password.normalize(), salt, iters, keyLen, 'sha256', (err, derivedKey) => {
            if (err) {
                logger.error(err);
            }
            else {
                this.encKey = derivedKey;
            }
        });
    }

    public getSenders(): string[] {
        const senders = [];
        this.sessionMap.forEach((sender) => { senders.push(sender.socket.remoteAddress); });
        return senders;
    }

    private handleConnection(socket: net.Socket) {
        logger.info(`New connection from ${socket.remoteAddress}:${socket.remotePort}`);

        const session = new FCastSession(socket, (data) => {
            socket.write(data);
        });
        session.bindEvents(this.emitter);
        this.sessionMap.set(session.sessionId, session);

        socket.on("error", (err) => {
            logger.warn(`Error from ${socket.remoteAddress}:${socket.remotePort}.`, err);
            this.disconnect(session.sessionId);
        });

        socket.on("data", buffer => {
            try {
                if (this.encKey) {
                    const nonce = buffer.slice(0, 16);
                    const tag_idx = buffer.length - 16;
                    const tag = buffer.slice(tag_idx);

                    const decipher = crypto.createDecipheriv('aes-256-gcm', this.encKey, nonce);
                    decipher.setAuthTag(tag);

                    let decrypted = Buffer.from('');
                    try {
                        decrypted = decipher.update(buffer.slice(16, tag_idx));
                        decrypted = Buffer.concat([decrypted, decipher.final()]);
                    }
                    catch (err) {
                        logger.error(`Error decrypting incoming packet ${err}. Closing socket`);
                        this.disconnect(session.sessionId);
                    }
                    session.processBytes(decrypted);
                }
                else {
                    session.processBytes(buffer);
                }
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
            session.send(Opcode.Version, new VersionMessage(PROTOCOL_VERSION));
        } catch (e) {
            logger.info('Failed to send version', e);
        }
    }
}
