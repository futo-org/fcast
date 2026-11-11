// Maximum value of the Size field, which counts the opcode and the body.
export const MAX_V4_PACKET = 512 * 1024;

export const Opcode4 = {
    Version: 11,
    Ping: 12,
    Pong: 13,
    Flatbuf: 20,
    Resource: 21,
};

export class FrameError extends Error {
    constructor(message: string) {
        super(message);
        this.name = 'FrameError';
        Object.setPrototypeOf(this, FrameError.prototype);
    }
}

export interface Frame {
    opcode: number;
    body: Buffer;
}

export function encodeFrame(opcode: number, body?: Uint8Array): Buffer {
    const bodyLength = body ? body.length : 0;
    const size = 1 + bodyLength;
    if (size > MAX_V4_PACKET) {
        throw new FrameError('packet too large: ' + size);
    }
    const frame = Buffer.alloc(4 + size);
    frame.writeUIntLE(size, 0, 4);
    frame.writeUInt8(opcode, 4);
    if (body) {
        frame.set(body, 5);
    }
    return frame;
}

// Reassembles frames from a byte stream. Chunks may split frames at any byte and may contain
// several frames. Throws FrameError on a Size of 0 or above the maximum; the caller must
// disconnect.
export class FrameReader {
    private pending: Buffer = Buffer.alloc(0);

    push(chunk: Buffer): Frame[] {
        this.pending = Buffer.concat([this.pending, chunk]);

        const frames: Frame[] = [];
        let offset = 0;
        while (this.pending.length - offset >= 4) {
            const size = this.pending.readUIntLE(offset, 4);
            if (size === 0) {
                throw new FrameError('missing opcode');
            }
            if (size > MAX_V4_PACKET) {
                throw new FrameError('packet too large: ' + size);
            }
            if (this.pending.length - offset - 4 < size) {
                break;
            }
            const opcode = this.pending.readUInt8(offset + 4);
            const body = Buffer.from(this.pending.slice(offset + 5, offset + 4 + size));
            frames.push({ opcode, body });
            offset += 4 + size;
        }
        this.pending = Buffer.from(this.pending.slice(offset));
        return frames;
    }
}
