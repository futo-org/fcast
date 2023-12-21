import net = require('net');
import * as crypto from 'crypto';
import { EventEmitter } from 'node:events';
import { DecryptedMessage, EncryptedMessage, KeyExchangeMessage, PlaybackErrorMessage, PlaybackUpdateMessage, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, VolumeUpdateMessage } from './Packets';
import { WebSocket } from 'ws';

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
    KeyExchange = 12,
    Encrypted = 13,
    Ping = 14,
    Pong = 15,
    StartEncryption = 16
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
    encryptionStarted = false;

    private aesKey: Buffer;
    private dh: crypto.DiffieHellman;
    private queuedEncryptedMessages: EncryptedMessage[] = [];

    constructor(socket: net.Socket | WebSocket, writer: (data: Buffer) => void) {
        this.socket = socket;
        this.writer = writer;
        this.state = SessionState.WaitingForLength;

        this.dh = generateKeyPair();

        const keyExchangeMessage = getKeyExchangeMessage(this.dh);
        console.log(`Sending KeyExchangeMessage: ${keyExchangeMessage}`);
        this.send(Opcode.KeyExchange, keyExchangeMessage);
    }

    sendVersion(value: VersionMessage) {
        this.send(Opcode.Version, value);
    }

    sendPlaybackError(value: PlaybackErrorMessage) {
        this.send(Opcode.PlaybackError, value);
    }

    sendPlaybackUpdate(value: PlaybackUpdateMessage) {
        this.send(Opcode.PlaybackUpdate, value);
    }

    sendVolumeUpdate(value: VolumeUpdateMessage) {
        this.send(Opcode.VolumeUpdate, value);
    }

    private send(opcode: number, message = null) {
        if (this.encryptionStarted && opcode != Opcode.Encrypted && opcode != Opcode.KeyExchange && opcode != Opcode.StartEncryption) {
            const decryptedMessage: DecryptedMessage = {
                opcode,
                message
            };

            this.send(Opcode.Encrypted, encryptMessage(this.aesKey, decryptedMessage));
            return;
        }

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

        console.log(`${receivedBytes.length} bytes received`);

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
            console.log(`Packet length header received from: ${this.packetLength}`);

            if (this.packetLength > MAXIMUM_PACKET_LENGTH) {
                throw new Error(`Maximum packet length is 32kB: ${this.packetLength}`);
            }

            if (bytesRemaining > 0) {
                console.log(`${bytesRemaining} remaining bytes pushed to handlePacketBytes`);
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
            console.log(`Packet finished receiving from of ${this.packetLength} bytes.`);
            this.handleNextPacket();

            this.state = SessionState.WaitingForLength;
            this.packetLength = 0;
            this.bytesRead = 0;

            if (bytesRemaining > 0) {
                console.log(`${bytesRemaining} remaining bytes pushed to handleLengthBytes`);
                this.handleLengthBytes(receivedBytes.slice(bytesToRead));
            }
        }
    }

    private handlePacket(opcode: number, body: string | undefined) {
        console.log(`handlePacket (opcode: ${opcode}, body: ${body})`);

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
                case Opcode.KeyExchange:
                    const keyExchangeMessage = JSON.parse(body) as KeyExchangeMessage;                    
                    this.aesKey = computeSharedSecret(this.dh, keyExchangeMessage);
                    this.send(Opcode.StartEncryption);

                    for (const encryptedMessage of this.queuedEncryptedMessages) {
                        const decryptedMessage = decryptMessage(this.aesKey, encryptedMessage);
                        this.handlePacket(decryptedMessage.opcode, decryptedMessage.message);
                    }

                    this.queuedEncryptedMessages = [];
                    break;
                case Opcode.Ping:
                    this.send(Opcode.Pong);
                    break;
                case Opcode.Encrypted:
                    const encryptedMessage = JSON.parse(body) as EncryptedMessage;

                    if (this.aesKey) {
                        const decryptedMessage = decryptMessage(this.aesKey, encryptedMessage);
                        this.handlePacket(decryptedMessage.opcode, decryptedMessage.message);
                    } else {
                        if (this.queuedEncryptedMessages.length === 15) {
                            this.queuedEncryptedMessages.shift();
                        }
                        
                        this.queuedEncryptedMessages.push(encryptedMessage);                        
                    }
                    break;
            }
        } catch (e) {
            console.warn(`Error handling packet from.`, e);
        }
    }

    private handleNextPacket() {
        console.log(`Processing packet of ${this.bytesRead} bytes from`);

        const opcode = this.buffer[0];
        const body = this.packetLength > 1 ? this.buffer.toString('utf8', 1, this.packetLength) : null;
        console.log('body', body);
        this.handlePacket(opcode, body);
    }
}

export function getKeyExchangeMessage(dh: crypto.DiffieHellman): KeyExchangeMessage {
    return { version: 1, publicKey: dh.getPublicKey().toString('base64') };
}

export function computeSharedSecret(dh: crypto.DiffieHellman, keyExchangeMessage: KeyExchangeMessage): Buffer {
    console.log("private", dh.getPrivateKey().toString('base64'));

    const theirPublicKey = Buffer.from(keyExchangeMessage.publicKey, 'base64');
    console.log("theirPublicKey", theirPublicKey.toString('base64'));
    const secret = dh.computeSecret(theirPublicKey);
    console.log("secret", secret.toString('base64'));
    const digest = crypto.createHash('sha256').update(secret).digest();
    console.log("digest", digest.toString('base64'));
    return digest;
}

export function encryptMessage(aesKey: Buffer, decryptedMessage: DecryptedMessage): EncryptedMessage {
    const iv = crypto.randomBytes(16);
    const cipher = crypto.createCipheriv('aes-256-cbc', aesKey, iv);
    let encrypted = cipher.update(JSON.stringify(decryptedMessage), 'utf8', 'base64');
    encrypted += cipher.final('base64');
    return {
        version: 1,
        iv: iv.toString('base64'),
        blob: encrypted
    };
}

export function decryptMessage(aesKey: Buffer, encryptedMessage: EncryptedMessage): DecryptedMessage {
    const iv = Buffer.from(encryptedMessage.iv, 'base64');
    const decipher = crypto.createDecipheriv('aes-256-cbc', aesKey, iv);
    let decrypted = decipher.update(encryptedMessage.blob, 'base64', 'utf8');
    decrypted += decipher.final('utf8');
    return JSON.parse(decrypted) as DecryptedMessage;
}

export function generateKeyPair() {
    const dh = createDiffieHellman();
    dh.generateKeys();
    return dh;
}

export function createDiffieHellman(): crypto.DiffieHellman {
    const p = Buffer.from('ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aacaa68ffffffffffffffff', 'hex');
    const g = Buffer.from('02', 'hex');
    return crypto.createDiffieHellman(p, g);
}