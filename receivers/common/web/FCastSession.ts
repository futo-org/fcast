import * as net from 'net';
import * as tls from 'tls';
import { EventEmitter } from 'events';
import { Opcode, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, InitialSenderMessage, SetPlaylistItemMessage, SubscribeEventMessage, UnsubscribeEventMessage, PROTOCOL_VERSION, InitialReceiverMessage } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { getComputerName, getAppName, getAppVersion, getPlayMessage, getPlaybackUpdateMessage, getV4Context } from 'src/Main';
import { v4 as uuidv4 } from 'modules/uuid';
import { AVCapabilities, LivestreamCapabilities, ReceiverCapabilities } from './Packets';
import { encodeFrame, Frame, FrameReader, Opcode4 } from 'common/v4/Framing';
import { decodePacket, SenderMessage } from 'common/v4/Decoder';
import { encodeError } from 'common/v4/Encoder';
import { ErrorKind } from 'common/v4/generated/fcast/v4/error-kind';
import { createTlsServerSocket } from 'common/v4/V4Transport';
import { V4Context } from 'common/v4/V4Context';
const logger = new Logger('FCastSession', LoggerType.BACKEND);

enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected,
};

const LENGTH_BYTES = 4;
const MAXIMUM_PACKET_LENGTH = 32000;
// Opcodes a sender may use before it sends Version: None through SetVolume.
const LEGACY_MAX_OPCODE = 8;
const V4_HANDSHAKE_TIMEOUT_MS = 10000;
const V4_HEARTBEAT_INTERVAL_MS = 500;
const V4_PING_AFTER_MS = 3000;
const V4_IDLE_TIMEOUT_MS = 6000;
const V4_ERROR_CLOSE_DELAY_MS = 1000;
const PRE_NEGOTIATION_TIMEOUT_MS = 10000;

// Returns the version of a Version body, or null when the body is not a JSON object with a
// non-negative integer `version`.
function parseVersion(body: string | undefined): number | null {
    if (typeof body !== 'string') {
        return null;
    }

    let parsed: any;
    try {
        parsed = JSON.parse(body);
    } catch (e) {
        return null;
    }

    if (parsed === null || typeof parsed !== 'object') {
        return null;
    }

    const version = parsed.version;
    if (typeof version !== 'number' || !Number.isInteger(version) || version < 0) {
        return null;
    }

    return version;
}

export class FCastSession {
    public sessionId: string;
    public protocolVersion: number;
    buffer: Buffer = Buffer.alloc(MAXIMUM_PACKET_LENGTH);
    bytesRead = 0;
    packetLength = 0;
    socket: net.Socket;
    writer: (data: Buffer) => void;
    state: SessionState;
    emitter = new EventEmitter();

    private sentInitialMessage: boolean;
    private packetNum = 0;
    private versionReceived = false;
    private legacyActive = false;
    private upgradeRequested = false;
    private upgradeRemainder: Buffer | null = null;
    private closing = false;
    private v4Context: V4Context | null = null;
    private v4Secure = false;
    private tlsSocket: tls.TLSSocket | null = null;
    private frameReader: FrameReader | null = null;
    private handshakeTimer: any = null;
    private heartbeatTimer: any = null;
    private closeTimer: any = null;
    private lastReceived = 0;
    private pingOutstanding = false;
    private preNegotiationTimer: any = null;

    constructor(socket: net.Socket, writer: (data: Buffer) => void) {
        this.sessionId = uuidv4();
        // Not all senders send a version message to the receiver on connection. Choosing version 2
        // as the base version since most/all current senders support this version.
        this.protocolVersion = 2;
        this.sentInitialMessage = false;
        this.socket = socket;
        this.writer = writer;
        this.state = SessionState.WaitingForLength;
        this.preNegotiationTimer = setTimeout(() => this.onPreNegotiationTimeout(), PRE_NEGOTIATION_TIMEOUT_MS);
    }

    // True once the sender's Version asked for TLS. From then on nothing is written in plaintext.
    get isUpgradingToTls(): boolean {
        return this.upgradeRequested;
    }

    // True once the TLS handshake of a v4 sender has completed.
    get isV4Secure(): boolean {
        return this.v4Secure && !this.closing;
    }

    // True when heartbeat Pings may be sent as plaintext: a Version 2 or 3 sender, or a legacy sender
    // that never sends Version and has already sent a pre-Version opcode.
    get canPing(): boolean {
        return (this.versionReceived && this.protocolVersion >= 2 && this.protocolVersion <= 3) ||
            (this.legacyActive && !this.versionReceived && this.protocolVersion >= 2);
    }

    send(opcode: number, message = null) {
        if (this.upgradeRequested || this.closing || !this.isSupportedOpcode(opcode)) {
            return;
        }

        // Until the sender's Version is known, only Version may be written: a plaintext message
        // would corrupt the TLS handshake of a v4 sender.
        if (!this.versionReceived && !this.legacyActive && opcode !== Opcode.Version) {
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

        // webOS 22 and earlier node versions do not support `writeUint32LE` despite nodejs stating
        // it should be supported in those versions... `writeUIntLE` however works instead.
        // @ts-ignore
        if (TARGET === 'webOS') {
            header.writeUIntLE(size, 0, 4);
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

    // Sends a v4 Flatbuf packet. Does nothing unless the session is a secured v4 session.
    sendFlatbuf(bytes: Uint8Array) {
        if (!this.v4Secure) {
            return;
        }

        this.writeV4(encodeFrame(Opcode4.Flatbuf, bytes));
    }

    close() {
        if (this.upgradeRequested) {
            this.closeConnection();
            return;
        }

        this.socket.end();
    }

    // Called when the raw socket closes. Stops timers; the caller removes the session.
    dispose() {
        this.closing = true;
        this.clearTimers();
    }

    // Feeds plaintext bytes. Returns the bytes that follow the Version frame when the sender asked
    // for TLS; the caller must then stop feeding plaintext and call upgradeToTls() with them.
    processBytes(receivedBytes: Buffer): Buffer | null {
        if (receivedBytes.length == 0 || this.stopped()) {
            return null;
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

        if (this.upgradeRequested && this.upgradeRemainder !== null) {
            const remainder = this.upgradeRemainder;
            this.upgradeRemainder = null;
            return remainder;
        }

        return null;
    }

    // Starts the server-side TLS handshake on the socket. `head` is the plaintext that followed the
    // Version frame, i.e. the first bytes of the TLS stream.
    upgradeToTls(head: Buffer) {
        const tlsSocket = createTlsServerSocket(this.socket, head, this.v4Context.secureContext);
        if (tlsSocket === null) {
            this.closeConnection();
            return;
        }

        this.tlsSocket = tlsSocket;
        this.frameReader = new FrameReader();
        this.handshakeTimer = setTimeout(() => {
            logger.warn(`TLS handshake timed out (session: ${this.sessionId}).`);
            this.closeConnection();
        }, V4_HANDSHAKE_TIMEOUT_MS);

        tlsSocket.on('secure', () => this.onSecure());
        tlsSocket.on('data', (chunk: Buffer) => this.onTlsData(chunk));
        tlsSocket.on('error', (err: Error) => {
            logger.warn(`TLS error (session: ${this.sessionId}).`, err);
            this.closeConnection();
        });
        tlsSocket.on('close', () => this.closeConnection());
    }

    private stopped(): boolean {
        return this.closing || this.upgradeRequested;
    }

    private handleLengthBytes(receivedBytes: Buffer) {
        const remaining = LENGTH_BYTES - this.bytesRead;
        const bytesToRead = Math.min(remaining, receivedBytes.length);
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
        const remaining = this.packetLength - this.bytesRead;
        const bytesToRead = Math.min(remaining, receivedBytes.length);
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

            if (this.stopped()) {
                if (this.upgradeRequested) {
                    this.upgradeRemainder = receivedBytes.slice(bytesToRead);
                }
                return;
            }

            if (bytesRemaining > 0) {
                logger.debug(`${bytesRemaining} remaining bytes pushed to handleLengthBytes`);
                this.handleLengthBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacket(opcode: number, body: string | undefined) {
        logger.info(`handlePacket: (session: ${this.sessionId}, opcode: ${opcode}, body: ${body})`);

        if (!this.versionReceived && !this.legacyActive && opcode !== Opcode.Version && opcode > LEGACY_MAX_OPCODE) {
            logger.warn(`Opcode ${opcode} received before Version. Closing connection.`);
            this.closeConnection();
            return;
        }

        if (!this.versionReceived && opcode <= LEGACY_MAX_OPCODE) {
            this.legacyActive = true;
            this.clearPreNegotiationTimer();
        }

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
                    this.handleVersion(body);
                    break;
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

    private handleVersion(body: string | undefined) {
        const version = parseVersion(body);
        if (version === null || version === 0) {
            logger.warn(`Invalid Version message: ${body}. Closing connection.`);
            this.closeConnection();
            return;
        }

        const firstVersion = !this.versionReceived;
        this.versionReceived = true;
        this.clearPreNegotiationTimer();

        if (version >= 4 && firstVersion) {
            const v4Context = getV4Context();
            if (v4Context !== null) {
                logger.info(`Sender requested v4, starting TLS (session: ${this.sessionId}).`);
                this.v4Context = v4Context;
                this.protocolVersion = 4;
                this.upgradeRequested = true;
                return;
            }
        }

        this.protocolVersion = version >= 4 ? PROTOCOL_VERSION : version;

        if (!this.sentInitialMessage && this.protocolVersion >= 3) {
            this.send(Opcode.Initial, new InitialReceiverMessage(
                getComputerName(),
                getAppName(),
                getAppVersion(),
                getPlayMessage(),
                new ReceiverCapabilities(
                    new AVCapabilities(
                        new LivestreamCapabilities(true)
                    )
                ),
            ));

            const updateMessage = getPlaybackUpdateMessage();
            if (updateMessage) {
                this.send(Opcode.PlaybackUpdate, updateMessage);
            }
            this.sentInitialMessage = true;
        }

        this.emitter.emit("version", new VersionMessage(version));
    }

    private handleNextPacket() {
        const opcode = this.buffer[0];
        const body = this.packetLength > 1 ? this.buffer.toString('utf8', 1, this.packetLength) : null;
        this.packetNum += 1;
        this.handlePacket(opcode, body);
    }

    private onSecure() {
        clearTimeout(this.handshakeTimer);
        this.handshakeTimer = null;
        this.protocolVersion = 4;
        this.v4Secure = true;
        this.lastReceived = Date.now();
        this.pingOutstanding = false;
        this.heartbeatTimer = setInterval(() => this.checkHeartbeat(), V4_HEARTBEAT_INTERVAL_MS);

        logger.info(`v4 session secured (session: ${this.sessionId}).`);
        this.sendFlatbuf(this.v4Context.receiverIntroduction);
        this.emitter.emit('v4secured', { sessionId: this.sessionId });
    }

    private checkHeartbeat() {
        if (this.closing) {
            return;
        }

        const idle = Date.now() - this.lastReceived;
        if (idle >= V4_IDLE_TIMEOUT_MS) {
            logger.warn(`v4 session ${this.sessionId} idle for ${idle} ms. Closing connection.`);
            this.closeConnection();
        } else if (idle >= V4_PING_AFTER_MS && !this.pingOutstanding) {
            this.pingOutstanding = true;
            this.writeV4(encodeFrame(Opcode4.Ping));
        }
    }

    private onTlsData(chunk: Buffer) {
        if (this.closing) {
            return;
        }

        let frames: Frame[];
        try {
            frames = this.frameReader.push(chunk);
        } catch (e) {
            logger.warn(`Invalid v4 frame (session: ${this.sessionId}). Closing connection.`, e);
            this.closeConnection();
            return;
        }

        for (const frame of frames) {
            if (this.closing) {
                break;
            }

            try {
                this.handleV4Frame(frame);
            } catch (e) {
                logger.warn(`Error handling v4 frame (session: ${this.sessionId}). Closing connection.`, e);
                this.closeConnection();
                return;
            }
        }
    }

    private handleV4Frame(frame: Frame) {
        const packetNum = this.packetNum;
        this.packetNum += 1;
        this.lastReceived = Date.now();
        this.pingOutstanding = false;

        switch (frame.opcode) {
            case Opcode4.Ping:
                this.writeV4(encodeFrame(Opcode4.Pong));
                break;
            case Opcode4.Pong:
                break;
            case Opcode4.Flatbuf:
                this.handleFlatbuf(packetNum, frame.body);
                break;
            case Opcode4.Resource:
                this.emitter.emit('v4resource', { sessionId: this.sessionId, packetNum: packetNum, body: frame.body });
                break;
            default:
                logger.warn(`Unsupported v4 opcode ${frame.opcode} in packet ${packetNum} (session: ${this.sessionId}).`);
                this.sendV4Error(ErrorKind.InvalidOpcode, packetNum);
                break;
        }
    }

    private handleFlatbuf(packetNum: number, body: Buffer) {
        let message: SenderMessage;
        try {
            message = decodePacket(body);
        } catch (e) {
            logger.warn(`Malformed packet ${packetNum} (session: ${this.sessionId}). Closing connection.`, e);
            this.sendV4Error(ErrorKind.MalformedBody, packetNum);
            this.closeAfterError();
            return;
        }

        switch (message.type) {
            case 'WrongDirection':
                logger.warn(`Packet ${packetNum} is a receiver-only payload: ${message.payload}`);
                this.sendV4Error(ErrorKind.InvalidPayloadType, packetNum);
                break;
            case 'SenderIntroduction':
                this.emitter.emit('senderintroduction', { sessionId: this.sessionId, deviceInfo: message.deviceInfo });
                break;
            default:
                this.emitter.emit('v4message', { sessionId: this.sessionId, packetNum: packetNum, message: message });
                break;
        }
    }

    private sendV4Error(kind: ErrorKind, packetNum: number) {
        this.writeV4(encodeFrame(Opcode4.Flatbuf, encodeError(kind, packetNum)));
    }

    private writeV4(frame: Buffer) {
        if (this.tlsSocket === null || !this.v4Secure) {
            return;
        }

        this.tlsSocket.write(frame);
    }

    // Sends the pending error frame, then closes the connection shortly after. Frames received
    // after a malformed packet are ignored.
    private closeAfterError() {
        this.closing = true;
        clearTimeout(this.handshakeTimer);
        clearInterval(this.heartbeatTimer);
        this.tlsSocket.end();
        this.closeTimer = setTimeout(() => this.closeConnection(), V4_ERROR_CLOSE_DELAY_MS);
    }

    private closeConnection() {
        this.closing = true;
        this.clearTimers();
        if (this.tlsSocket !== null) {
            this.tlsSocket.destroy();
        }
        this.socket.destroy();
    }

    // Closes a sender that sent neither Version nor a legacy opcode in time. Nothing is written.
    private onPreNegotiationTimeout() {
        this.preNegotiationTimer = null;
        if (this.versionReceived || this.legacyActive) {
            return;
        }

        logger.warn(`No Version or legacy opcode within ${PRE_NEGOTIATION_TIMEOUT_MS} ms (session: ${this.sessionId}). Closing connection.`);
        this.closeConnection();
    }

    private clearPreNegotiationTimer() {
        clearTimeout(this.preNegotiationTimer);
        this.preNegotiationTimer = null;
    }

    private clearTimers() {
        clearTimeout(this.handshakeTimer);
        clearInterval(this.heartbeatTimer);
        clearTimeout(this.closeTimer);
        this.clearPreNegotiationTimer();
        this.handshakeTimer = null;
        this.heartbeatTimer = null;
        this.closeTimer = null;
    }

    bindEvents(emitter: EventEmitter) {
        this.emitter.on("play", (body: PlayMessage) => { emitter.emit("play", { sessionId: this.sessionId, body: body }) });
        this.emitter.on("pause", () => { emitter.emit("pause") });
        this.emitter.on("resume", () => { emitter.emit("resume") });
        this.emitter.on("stop", () => { emitter.emit("stop") });
        this.emitter.on("seek", (body: SeekMessage) => { emitter.emit("seek", body) });
        this.emitter.on("setvolume", (body: SetVolumeMessage) => { emitter.emit("setvolume", body) });
        this.emitter.on("setspeed", (body: SetSpeedMessage) => { emitter.emit("setspeed", body) });
        this.emitter.on("version", (body: VersionMessage) => { emitter.emit("version", body) });
        this.emitter.on("ping", () => { emitter.emit("ping", this.sessionId) });
        this.emitter.on("pong", () => { emitter.emit("pong", this.sessionId) });
        this.emitter.on("initial", (body: InitialSenderMessage) => { emitter.emit("initial", body) });
        this.emitter.on("setplaylistitem", (body: SetPlaylistItemMessage) => { emitter.emit("setplaylistitem", body) });
        this.emitter.on("subscribeevent", (body: SubscribeEventMessage) => { emitter.emit("subscribeevent", { sessionId: this.sessionId, body: body }) });
        this.emitter.on("unsubscribeevent", (body: UnsubscribeEventMessage) => { emitter.emit("unsubscribeevent", { sessionId: this.sessionId, body: body }) });
        this.emitter.on("v4message", (body: any) => { emitter.emit("v4message", body) });
        this.emitter.on("v4resource", (body: any) => { emitter.emit("v4resource", body) });
        this.emitter.on("senderintroduction", (body: any) => { emitter.emit("senderintroduction", body) });
        this.emitter.on("v4secured", (body: any) => { emitter.emit("v4secured", body) });
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
