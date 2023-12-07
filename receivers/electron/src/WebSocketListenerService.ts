import { FCastSession } from './FCastSession';
import { EventEmitter } from 'node:events';
import { PlaybackErrorMessage, PlaybackUpdateMessage, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage } from './Packets';
import { dialog } from 'electron';
import Main from './Main';
import { WebSocket, WebSocketServer } from 'ws';

export class WebSocketListenerService {
    emitter = new EventEmitter();
    
    private server: WebSocketServer;
    private sessions: FCastSession[] = [];

    start() {
        if (this.server != null) {
            return;
        }

        this.server = new WebSocketServer({ port: 46898 })
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

    sendPlaybackError(value: PlaybackErrorMessage) {
        console.info("Sending playback error.", value);

        this.sessions.forEach(session => {
            try {
                session.sendPlaybackError(value);
            } catch (e) {
                console.warn("Failed to send error.", e);
                session.close();
            }
        });
    }

    sendPlaybackUpdate(value: PlaybackUpdateMessage) {
        console.info("Sending playback update.", value);

        this.sessions.forEach(session => {
            try {
                session.sendPlaybackUpdate(value);
            } catch (e) {
                console.warn("Failed to send update.", e);
                session.close();
            }
        });
    }

    sendVolumeUpdate(value: VolumeUpdateMessage) {
        console.info("Sending volume update.", value);

        this.sessions.forEach(session => {
            try {
                session.sendVolumeUpdate(value);
            } catch (e) {
                console.warn("Failed to send update.", e);
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
        session.emitter.on("play", (body: PlayMessage) => { this.emitter.emit("play", body) });
        session.emitter.on("pause", () => { this.emitter.emit("pause") });
        session.emitter.on("resume", () => { this.emitter.emit("resume") });
        session.emitter.on("stop", () => { this.emitter.emit("stop") });
        session.emitter.on("seek", (body: SeekMessage) => { this.emitter.emit("seek", body) });
        session.emitter.on("setvolume", (body: SetVolumeMessage) => { this.emitter.emit("setvolume", body) });
        session.emitter.on("setspeed", (body: SetSpeedMessage) => { this.emitter.emit("setspeed", body) });
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
            session.sendVersion({version: 2});
        } catch (e) {
            console.log('Failed to send version');
        }
    }
}