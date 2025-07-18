import * as net from 'net';
import { EventEmitter } from 'events';
import { Opcode, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, InitialSenderMessage, SetPlaylistItemMessage, SubscribeEventMessage, UnsubscribeEventMessage, PROTOCOL_VERSION, InitialReceiverMessage } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { getComputerName, getAppName, getAppVersion, getPlayMessage } from 'src/Main';
import { WebSocket } from 'modules/ws';
import { v4 as uuidv4 } from 'modules/uuid';
const logger = new Logger('FCastSession', LoggerType.BACKEND);

enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected,
};

const LENGTH_BYTES = 4;
const MAXIMUM_PACKET_LENGTH = 32000;

export class FCastSession {
    public sessionId: string;
    public protocolVersion: number;
    buffer: Buffer = Buffer.alloc(MAXIMUM_PACKET_LENGTH);
    bytesRead = 0;
    packetLength = 0;
    socket: net.Socket | WebSocket;
    writer: (data: Buffer) => void;
    state: SessionState;
    emitter = new EventEmitter();

    private sentInitialMessage: boolean;

    constructor(socket: net.Socket | WebSocket, writer: (data: Buffer) => void) {
        this.sessionId = uuidv4();
        // Not all senders send a version message to the receiver on connection. Choosing version 2
        // as the base version since most/all current senders support this version.
        this.protocolVersion = 2;
        this.sentInitialMessage = false;
        this.socket = socket;
        this.writer = writer;
        this.state = SessionState.WaitingForLength;
    }

    send(opcode: number, message = null) {
        if (!this.isSupportedOpcode(opcode)) {
            return;
        }

        message = this.stripUnsupportedFields(opcode, message);
        const json = message ? JSON.stringify(message) : null;
        logger.info(`send: (session: ${this.sessionId}, opcode: ${opcode}, body: ${json})`);

        let data: Uint8Array;
        if (json) {
            // Do NOT use the TextEncoder utility class, it does not exist in the NodeJS runtime
            // for webOS 6.0 and earlier...
            data = Buffer.from(json, 'utf8');
        } else  {
            data = Buffer.alloc(0);
        }

        const size = 1 + data.length;
        const header = Buffer.alloc(4 + 1);

        // webOS 22 and earlier node versions do not support `writeUint32LE`,
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

        logger.debug(`${receivedBytes.length} bytes received`);

        switch (this.state) {
            case SessionState.WaitingForLength:
                this.handleLengthBytes(receivedBytes);
                break;
            case SessionState.WaitingForData:
                this.handlePacketBytes(receivedBytes);
                break;
            default:
                logger.warn(`Data received is unhandled in current session state ${this.state}.`);
                break;
        }
    }

    private handleLengthBytes(receivedBytes: Buffer) {
        const bytesToRead = Math.min(LENGTH_BYTES, receivedBytes.length);
        const bytesRemaining = receivedBytes.length - bytesToRead;
        receivedBytes.copy(this.buffer, this.bytesRead, 0, bytesToRead);
        this.bytesRead += bytesToRead;

        logger.debug(`handleLengthBytes: Read ${bytesToRead} bytes from packet`);

        if (this.bytesRead >= LENGTH_BYTES) {
            this.state = SessionState.WaitingForData;
            this.packetLength = this.buffer.readUInt32LE(0);
            this.bytesRead = 0;
            logger.debug(`Packet length header received from: ${this.packetLength}`);

            if (this.packetLength > MAXIMUM_PACKET_LENGTH) {
                throw new Error(`Maximum packet length is 32kB: ${this.packetLength}`);
            }

            if (bytesRemaining > 0) {
                logger.debug(`${bytesRemaining} remaining bytes pushed to handlePacketBytes`);
                this.handlePacketBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacketBytes(receivedBytes: Buffer) {
        const bytesToRead = Math.min(this.packetLength, receivedBytes.length);
        const bytesRemaining = receivedBytes.length - bytesToRead;
        receivedBytes.copy(this.buffer, this.bytesRead, 0, bytesToRead);
        this.bytesRead += bytesToRead;

        logger.debug(`handlePacketBytes: Read ${bytesToRead} bytes from packet`);

        if (this.bytesRead >= this.packetLength) {
            logger.debug(`handlePacketBytes: Finished handling packet with ${this.packetLength} bytes. Total bytes read ${this.bytesRead}.`);
            this.handleNextPacket();

            this.state = SessionState.WaitingForLength;
            this.packetLength = 0;
            this.bytesRead = 0;

            if (bytesRemaining > 0) {
                logger.debug(`${bytesRemaining} remaining bytes pushed to handleLengthBytes`);
                this.handleLengthBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacket(opcode: number, body: string | undefined) {
        logger.info(`handlePacket: (session: ${this.sessionId}, opcode: ${opcode}, body: ${body})`);

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
                case Opcode.Version: {
                    const versionMessage = JSON.parse(body) as VersionMessage;
                    this.protocolVersion = (versionMessage.version > 0 && versionMessage.version <= PROTOCOL_VERSION) ?  versionMessage.version : this.protocolVersion;
                    if (!this.sentInitialMessage && this.protocolVersion >= 3) {
                        this.send(Opcode.Initial, new InitialReceiverMessage(
                            getComputerName(),
                            getAppName(),
                            getAppVersion(),
                            getPlayMessage(),
                        ));

                        this.sentInitialMessage = true;
                    }

                    this.emitter.emit("version", versionMessage);
                    break;
                }
                case Opcode.Ping:
                    this.send(Opcode.Pong);
                    this.emitter.emit("ping");
                    break;
                case Opcode.Pong:
                    this.emitter.emit("pong");
                    break;
                case Opcode.Initial:
                    this.emitter.emit("initial", JSON.parse(body) as InitialSenderMessage);
                    break;
                case Opcode.SetPlaylistItem:
                    this.emitter.emit("setplaylistitem", JSON.parse(body) as SetPlaylistItemMessage);
                    break;
                case Opcode.SubscribeEvent:
                    this.emitter.emit("subscribeevent", JSON.parse(body) as SubscribeEventMessage);
                    break;
                case Opcode.UnsubscribeEvent:
                    this.emitter.emit("unsubscribeevent", JSON.parse(body) as UnsubscribeEventMessage);
                    break;
            }
        } catch (e) {
            logger.warn(`Error handling packet from.`, e);
        }
    }

    private handleNextPacket() {
        const opcode = this.buffer[0];
        const body = this.packetLength > 1 ? this.buffer.toString('utf8', 1, this.packetLength) : null;
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
        this.emitter.on("ping", () => { emitter.emit("ping", { sessionId: this.sessionId }) });
        this.emitter.on("pong", () => { emitter.emit("pong", { sessionId: this.sessionId }) });
        this.emitter.on("initial", (body: InitialSenderMessage) => { emitter.emit("initial", body) });
        this.emitter.on("setplaylistitem", (body: SetPlaylistItemMessage) => { emitter.emit("setplaylistitem", body) });
        this.emitter.on("subscribeevent", (body: SubscribeEventMessage) => { emitter.emit("subscribeevent", { sessionId: this.sessionId, body: body }) });
        this.emitter.on("unsubscribeevent", (body: UnsubscribeEventMessage) => { emitter.emit("unsubscribeevent", { sessionId: this.sessionId, body: body }) });
    }

    private isSupportedOpcode(opcode: number) {
        switch (this.protocolVersion) {
            case 1:
                return opcode <= 8;

            case 2:
                return opcode <= 13;

            case 3:
                return opcode <= 19;

            default:
                return false;
        }
    }

    private stripUnsupportedFields(opcode: number, message: any = null): any {
        switch (this.protocolVersion) {
            case 1: {
                switch (opcode) {
                    case Opcode.Play:
                        delete message.speed;
                        delete message.headers;
                        break;
                    case Opcode.PlaybackUpdate:
                        delete message.generationTime;
                        delete message.duration;
                        delete message.speed;

                        message.time = message.time !== null ? message.time : 0;
                        break;
                    case Opcode.VolumeUpdate:
                        delete message.generationTime;
                        break;
                    default:
                        break;
                }

                // fallthrough
            }
            case 2: {
                switch (opcode) {
                    case Opcode.Play:
                        delete message.volume;
                        delete message.metadata;
                        break;
                    case Opcode.PlaybackUpdate:
                        delete message.itemIndex;

                        message.time = message.time !== null ? message.time : 0;
                        message.duration = message.duration !== null ? message.duration : 0;
                        message.speed = message.speed !== null ? message.speed : 1;
                        break;
                    default:
                        break;
                }

                // fallthrough
            }
            case 3:
                break;

            default:
                break;
        }

        return message;
    }
}
