import * as net from 'net';
import * as log4js from "modules/log4js";
import { EventEmitter } from 'events';
import { PlaybackErrorMessage, PlaybackUpdateMessage, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, VolumeUpdateMessage } from 'common/Packets';
import { WebSocket } from 'modules/ws';
const logger = log4js.getLogger();

enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected,
};

export enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    Ping = 12,
    Pong = 13
};

const LENGTH_BYTES = 4;
const MAXIMUM_PACKET_LENGTH = 32000;

export class FCastSession {
    buffer: Buffer = Buffer.alloc(MAXIMUM_PACKET_LENGTH);
    bytesRead = 0;
    packetLength = 0;
    socket: net.Socket | WebSocket;
    writer: (data: Buffer) => void;
    state: SessionState;
    emitter = new EventEmitter();

    constructor(socket: net.Socket | WebSocket, writer: (data: Buffer) => void) {
        this.socket = socket;
        this.writer = writer;
        this.state = SessionState.WaitingForLength;
    }

    send(opcode: number, message = null) {
        const json = message ? JSON.stringify(message) : null;
        logger.info(`send (opcode: ${opcode}, body: ${json})`);

        let data: Uint8Array;
        if (json) {
            const utf8Encode = new TextEncoder();
            data = utf8Encode.encode(json);
        } else  {
            data = new Uint8Array(0);
        }

        const size = 1 + data.length;
        const header = Buffer.alloc(4 + 1);

        // Web OS 22 and earlier node versions do not support `writeUint32LE`,
        // so manually checking endianness and writing as LE
        // @ts-ignore
        if (TARGET === 'webOS') {
            let uInt32 = new Uint32Array([0x11223344]);
            let uInt8 = new Uint8Array(uInt32.buffer);

            if(uInt8[0] === 0x44) {
                // LE
                header[0] = size & 0xFF;
                header[1] = size & 0xFF00;
                header[2] = size & 0xFF0000;
                header[3] = size & 0xFF000000;
            } else if (uInt8[0] === 0x11) {
                // BE
                header[0] = size & 0xFF000000;
                header[1] = size & 0xFF0000;
                header[2] = size & 0xFF00;
                header[3] = size & 0xFF;
            }
        } else {
            header.writeUint32LE(size, 0);
        }

        header[4] = opcode;

        let packet: Buffer;
        if (data.length > 0) {
            packet = Buffer.concat([ header, data ]);
        } else {
            packet = header;
        }

        this.writer(packet);
    }

    close() {
        if (this.socket instanceof WebSocket) {
            this.socket.close();
        } else if (this.socket instanceof net.Socket) {
            this.socket.end();
        }
    }

    processBytes(receivedBytes: Buffer) {
        //TODO: Multithreading?

        if (receivedBytes.length == 0) {
            return;
        }

        logger.info(`${receivedBytes.length} bytes received`);

        switch (this.state) {
            case SessionState.WaitingForLength:
                this.handleLengthBytes(receivedBytes);
                break;
            case SessionState.WaitingForData:
                this.handlePacketBytes(receivedBytes);
                break;
            default:
                logger.info(`Data received is unhandled in current session state ${this.state}.`);
                break;
        }
    }

    private handleLengthBytes(receivedBytes: Buffer) {
        const bytesToRead = Math.min(LENGTH_BYTES, receivedBytes.length);
        const bytesRemaining = receivedBytes.length - bytesToRead;
        receivedBytes.copy(this.buffer, this.bytesRead, 0, bytesToRead);
        this.bytesRead += bytesToRead;

        logger.info(`handleLengthBytes: Read ${bytesToRead} bytes from packet`);

        if (this.bytesRead >= LENGTH_BYTES) {
            this.state = SessionState.WaitingForData;
            this.packetLength = this.buffer.readUInt32LE(0);
            this.bytesRead = 0;
            logger.info(`Packet length header received from: ${this.packetLength}`);

            if (this.packetLength > MAXIMUM_PACKET_LENGTH) {
                throw new Error(`Maximum packet length is 32kB: ${this.packetLength}`);
            }

            if (bytesRemaining > 0) {
                logger.info(`${bytesRemaining} remaining bytes pushed to handlePacketBytes`);
                this.handlePacketBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacketBytes(receivedBytes: Buffer) {
        const bytesToRead = Math.min(this.packetLength, receivedBytes.length);
        const bytesRemaining = receivedBytes.length - bytesToRead;
        receivedBytes.copy(this.buffer, this.bytesRead, 0, bytesToRead);
        this.bytesRead += bytesToRead;

        logger.info(`handlePacketBytes: Read ${bytesToRead} bytes from packet`);

        if (this.bytesRead >= this.packetLength) {
            logger.info(`Packet finished receiving from of ${this.packetLength} bytes.`);
            this.handleNextPacket();

            this.state = SessionState.WaitingForLength;
            this.packetLength = 0;
            this.bytesRead = 0;

            if (bytesRemaining > 0) {
                logger.info(`${bytesRemaining} remaining bytes pushed to handleLengthBytes`);
                this.handleLengthBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacket(opcode: number, body: string | undefined) {
        logger.info(`handlePacket (opcode: ${opcode}, body: ${body})`);

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
                case Opcode.SetSpeed:
                    this.emitter.emit("setspeed", JSON.parse(body) as SetSpeedMessage);
                    break;
                case Opcode.Version:
                    this.emitter.emit("version", JSON.parse(body) as VersionMessage);
                    break;
                case Opcode.Ping:
                    this.send(Opcode.Pong);
                    this.emitter.emit("ping");
                    break;
                case Opcode.Pong:
                    this.emitter.emit("pong");
                    break;
            }
        } catch (e) {
            logger.warn(`Error handling packet from.`, e);
        }
    }

    private handleNextPacket() {
        logger.info(`Processing packet of ${this.bytesRead} bytes from`);

        const opcode = this.buffer[0];
        const body = this.packetLength > 1 ? this.buffer.toString('utf8', 1, this.packetLength) : null;
        logger.info('body', body);
        this.handlePacket(opcode, body);
    }

    bindEvents(emitter: EventEmitter) {
        this.emitter.on("play", (body: PlayMessage) => { emitter.emit("play", body) });
        this.emitter.on("pause", () => { emitter.emit("pause") });
        this.emitter.on("resume", () => { emitter.emit("resume") });
        this.emitter.on("stop", () => { emitter.emit("stop") });
        this.emitter.on("seek", (body: SeekMessage) => { emitter.emit("seek", body) });
        this.emitter.on("setvolume", (body: SetVolumeMessage) => { emitter.emit("setvolume", body) });
        this.emitter.on("setspeed", (body: SetSpeedMessage) => { emitter.emit("setspeed", body) });
        this.emitter.on("version", (body: VersionMessage) => { emitter.emit("version", body) });
        this.emitter.on("ping", () => { emitter.emit("ping") });
        this.emitter.on("pong", () => { emitter.emit("pong") });
    }
}
