import { FCastSession, Opcode } from './FCastSession';
import { EventEmitter } from 'node:events';
import { dialog } from 'electron';
import Main from './Main';
import { WebSocket, WebSocketServer } from 'ws';

export class WebSocketListenerService {
    public static PORT = 46898;

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

    private handleConnection(socket: WebSocket) {
        console.log('New WebSocket connection');

        const session = new FCastSession(socket, (data) => socket.send(data));
        session.bindEvents(this.emitter);
        this.sessions.push(session);

        socket.on("error", (err) => {
            console.warn(`Error.`, err);
            session.close();
        });

        socket.on('message', data => {
            try {
                if (data instanceof Buffer) {
                    session.processBytes(data);
                } else {
                    console.warn("Received unhandled string message", data);
                }
            } catch (e) {
                console.warn(`Error while handling packet.`, e);
                session.close();
            }
        });

        socket.on("close", () => {
            console.log('WebSocket connection closed');

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