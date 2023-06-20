import net = require('net');
import { EventEmitter } from 'node:events';
import { PlaybackUpdateMessage, PlayMessage, SeekMessage, SetVolumeMessage, VolumeUpdateMessage } from './Packets';

enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected,
};

enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8
};

const LENGTH_BYTES = 4;
const MAXIMUM_PACKET_LENGTH = 32000;

export class FCastSession {
    buffer: Buffer = Buffer.alloc(MAXIMUM_PACKET_LENGTH);
    bytesRead = 0;
    packetLength = 0;
    socket: net.Socket;
    state: SessionState;
    emitter = new EventEmitter();

    constructor(socket: net.Socket) {
        this.socket = socket;
        this.state = SessionState.WaitingForLength;
    }

    sendPlaybackUpdate(value: PlaybackUpdateMessage) {
        this.send(Opcode.PlaybackUpdate, value);
    }

    sendVolumeUpdate(value: VolumeUpdateMessage) {
        this.send(Opcode.VolumeUpdate, value);
    }

    private send(opcode: number, message = null) {
        const json = message ? JSON.stringify(message) : null;
        let data: Uint8Array;
        if (json) {
            const utf8Encode = new TextEncoder();
            data = utf8Encode.encode(json);
        } else  {
            data = new Uint8Array(0);
        }
        
        const size = 1 + data.length;
        const header = Buffer.alloc(4 + 1);
        header.writeUint32LE(size, 0);
        header[4] = opcode;

        let packet: Buffer;
        if (data.length > 0) {
            packet = Buffer.concat([ header, data ]);
        } else {
            packet = header;
        }

        this.socket.write(packet);
    }

    processBytes(receivedBytes: Buffer) {
        //TODO: Multithreading?

        if (receivedBytes.length == 0) {
            return;
        }

        console.log(`${receivedBytes.length} bytes received from ${this.socket.remoteAddress}:${this.socket.remotePort}`);

        switch (this.state) {
            case SessionState.WaitingForLength:
                this.handleLengthBytes(receivedBytes);
                break;
            case SessionState.WaitingForData:
                this.handlePacketBytes(receivedBytes);
                break;
            default:
                console.log(`Data received is unhandled in current session state ${this.state}.`);
                break;
        }
    }

    private handleLengthBytes(receivedBytes: Buffer) {
        const bytesToRead = Math.min(LENGTH_BYTES, receivedBytes.length);
        const bytesRemaining = receivedBytes.length - bytesToRead;
        receivedBytes.copy(this.buffer, this.bytesRead, 0, bytesToRead);
        this.bytesRead += bytesToRead;

        console.log(`handleLengthBytes: Read ${bytesToRead} bytes from packet`);

        if (this.bytesRead >= LENGTH_BYTES) {
            this.state = SessionState.WaitingForData;
            this.packetLength = this.buffer.readUInt32LE(0);
            this.bytesRead = 0;
            console.log(`Packet length header received from ${this.socket.remoteAddress}:${this.socket.remotePort}: ${this.packetLength}`);

            if (this.packetLength > MAXIMUM_PACKET_LENGTH) {
                console.log(`Maximum packet length is 32kB, killing socket ${this.socket.remoteAddress}:${this.socket.remotePort}: ${this.packetLength}`);
                this.socket.end();
                this.state = SessionState.Disconnected;
                return;
            }

            if (bytesRemaining > 0) {
                console.log(`${bytesRemaining} remaining bytes ${this.socket.remoteAddress}:${this.socket.remotePort} pushed to handlePacketBytes`);
                this.handlePacketBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacketBytes(receivedBytes: Buffer) {
        const bytesToRead = Math.min(this.packetLength, receivedBytes.length);
        const bytesRemaining = receivedBytes.length - bytesToRead;
        receivedBytes.copy(this.buffer, this.bytesRead, 0, bytesToRead);
        this.bytesRead += bytesToRead;

        console.log(`handlePacketBytes: Read ${bytesToRead} bytes from packet`);

        if (this.bytesRead >= this.packetLength) {
            console.log(`Packet finished receiving from ${this.socket.remoteAddress}:${this.socket.remotePort} of ${this.packetLength} bytes.`);
            this.handlePacket();

            this.state = SessionState.WaitingForLength;
            this.packetLength = 0;
            this.bytesRead = 0;

            if (bytesRemaining > 0) {
                console.log(`${bytesRemaining} remaining bytes ${this.socket.remoteAddress}:${this.socket.remotePort} pushed to handleLengthBytes`);
                this.handleLengthBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacket() {
        console.log(`Processing packet of ${this.bytesRead} bytes from ${this.socket.remoteAddress}:${this.socket.remotePort}`);

        const opcode = this.buffer[0];
        const body = this.packetLength > 1 ? this.buffer.toString('utf8', 1, this.packetLength) : null;
        console.log('body', body);

        try {
            switch (opcode) {
                case Opcode.Play:
                    this.emitter.emit("play", JSON.parse(body) as PlayMessage);
                    break;
                case Opcode.Pause:
                    this.emitter.emit("pause");
                    break;
                case Opcode.Resume:
                    this.emitter.emit("resume");
                    break;
                case Opcode.Stop:
                    this.emitter.emit("stop");
                    break;
                case Opcode.Seek:
                    this.emitter.emit("seek", JSON.parse(body) as SeekMessage);
                    break;
                case Opcode.SetVolume:
                    this.emitter.emit("setvolume", JSON.parse(body) as SetVolumeMessage);
                    break;
            }
        } catch (e) {
            console.warn(`Error handling packet from ${this.socket.remoteAddress}:${this.socket.remotePort}.`, e);
        }
    }
}