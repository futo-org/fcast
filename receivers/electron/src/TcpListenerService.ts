import * as net from 'net';
import { FCastSession, Opcode } from './FCastSession';
import { EventEmitter } from 'node:events';
import { dialog } from 'electron';
import Main from './Main';

export class TcpListenerService {
    public static PORT = 46899;

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
        this.sessions.forEach(session => {
            try {
                session.send(opcode, message);
            } catch (e) {
                console.warn("Failed to send error.", e);
                session.close();
            }
        });
    }

    private async handleServerError(err: NodeJS.ErrnoException) {
        console.error("Server error:", err);

        const restartPrompt = await dialog.showMessageBox({
            type: 'error',
            title: 'Failed to start',
            message: 'The application failed to start properly.',
            buttons: ['Restart', 'Close'],
            defaultId: 0,
            cancelId: 1
        });

        if (restartPrompt.response === 0) {
            Main.application.relaunch();
            Main.application.exit(0);
        } else {
            Main.application.exit(0);
        }
    }

    private handleConnection(socket: net.Socket) {
        console.log(`new connection from ${socket.remoteAddress}:${socket.remotePort}`);

        const session = new FCastSession(socket, (data) => socket.write(data));
        session.bindEvents(this.emitter);
        this.sessions.push(session);

        socket.on("error", (err) => {
            console.warn(`Error from ${socket.remoteAddress}:${socket.remotePort}.`, err);
            socket.destroy();
        });

        socket.on("data", buffer => {
            try {
                session.processBytes(buffer);
            } catch (e) {
                console.warn(`Error while handling packet from ${socket.remoteAddress}:${socket.remotePort}.`, e);
                socket.end();
            }
        });

        socket.on("close", () => {
            const index = this.sessions.indexOf(session);
            if (index != -1) {
                this.sessions.splice(index, 1);
            }
        });

        try {
            console.log('Sending version');
            session.send(Opcode.Version, {version: 2});
        } catch (e) {
            console.log('Failed to send version');
        }
    }
}