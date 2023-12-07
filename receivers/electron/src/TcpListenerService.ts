import net = require('net');
import { FCastSession } from './FCastSession';
import { EventEmitter } from 'node:events';
import { PlaybackErrorMessage, PlaybackUpdateMessage, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage } from './Packets';
import { dialog } from 'electron';
import Main from './Main';

export class TcpListenerService {
    emitter = new EventEmitter();
    
    private server: net.Server;
    private sessions: FCastSession[] = [];

    start() {
        if (this.server != null) {
            return;
        }

        this.server = net.createServer()
            .listen(46899)
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

    private handleConnection(socket: net.Socket) {
        console.log(`new connection from ${socket.remoteAddress}:${socket.remotePort}`);

        const session = new FCastSession(socket, (data) => socket.write(data));
        session.emitter.on("play", (body: PlayMessage) => { this.emitter.emit("play", body) });
        session.emitter.on("pause", () => { this.emitter.emit("pause") });
        session.emitter.on("resume", () => { this.emitter.emit("resume") });
        session.emitter.on("stop", () => { this.emitter.emit("stop") });
        session.emitter.on("seek", (body: SeekMessage) => { this.emitter.emit("seek", body) });
        session.emitter.on("setvolume", (body: SetVolumeMessage) => { this.emitter.emit("setvolume", body) });
        session.emitter.on("setspeed", (body: SetSpeedMessage) => { this.emitter.emit("setspeed", body) });
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
    }
}