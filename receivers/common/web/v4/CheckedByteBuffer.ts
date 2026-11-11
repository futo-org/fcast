import * as flatbuffers from 'flatbuffers';

export class DecodeError extends Error {
    constructor(message: string) {
        super(message);
        this.name = 'DecodeError';
        Object.setPrototypeOf(this, DecodeError.prototype);
    }
}

// flatbuffers 25.9.23 implements readInt8, readInt16, readUint32, readInt64, readUint64,
// readFloat32 and readFloat64 on top of readUint8, readUint16 and readInt32 (js/byte-buffer.js).
// Every typed reader is overridden here, so each read is bounded by the buffer and, when it starts
// at a field recorded by __offset, by the inline area of that field's table.
export class CheckedByteBuffer extends flatbuffers.ByteBuffer {
    // Remaining budget for string bytes. Charged before each string is decoded, so that many
    // references to one shared string cannot multiply the decoding work.
    stringBudget = 0;

    // Absolute start of each field read through __offset, mapped to the end of its table's inline area.
    private fieldEnds = new Map<number, number>();
    // Positions of tables already validated by verifyTable. Shared tables are checked once.
    private verifiedTables = new Set<number>();

    private checkRange(offset: number, length: number): void {
        if (!Number.isInteger(offset) || offset < 0 || length < 0 || offset + length > this.capacity()) {
            throw new DecodeError('out of bounds read at ' + offset + ' (+' + length + ') of ' + this.capacity());
        }
    }

    private checkField(offset: number, length: number): void {
        const end = this.fieldEnds.get(offset);
        if (end !== undefined && offset + length > end) {
            throw new DecodeError('field exceeds table');
        }
    }

    readInt8(offset: number): number {
        this.checkRange(offset, 1);
        this.checkField(offset, 1);
        return super.readInt8(offset);
    }

    readUint8(offset: number): number {
        this.checkRange(offset, 1);
        this.checkField(offset, 1);
        return super.readUint8(offset);
    }

    readInt16(offset: number): number {
        this.checkRange(offset, 2);
        this.checkField(offset, 2);
        return super.readInt16(offset);
    }

    readUint16(offset: number): number {
        this.checkRange(offset, 2);
        this.checkField(offset, 2);
        return super.readUint16(offset);
    }

    readInt32(offset: number): number {
        this.checkRange(offset, 4);
        this.checkField(offset, 4);
        return super.readInt32(offset);
    }

    readUint32(offset: number): number {
        this.checkRange(offset, 4);
        this.checkField(offset, 4);
        return super.readUint32(offset);
    }

    readInt64(offset: number): bigint {
        this.checkRange(offset, 8);
        this.checkField(offset, 8);
        return super.readInt64(offset);
    }

    readUint64(offset: number): bigint {
        this.checkRange(offset, 8);
        this.checkField(offset, 8);
        return super.readUint64(offset);
    }

    readFloat32(offset: number): number {
        this.checkRange(offset, 4);
        this.checkField(offset, 4);
        return super.readFloat32(offset);
    }

    readFloat64(offset: number): number {
        this.checkRange(offset, 8);
        this.checkField(offset, 8);
        return super.readFloat64(offset);
    }

    // Validates the table at absolute position pos: its vtable, its inline area and every field offset.
    verifyTable(pos: number): void {
        if (this.verifiedTables.has(pos)) {
            return;
        }
        if (!Number.isInteger(pos)) {
            throw new DecodeError('bad table position');
        }
        this.checkRange(pos, 4);
        const vtable = pos - this.readInt32(pos);
        this.checkRange(vtable, 4);
        const vtableSize = this.readUint16(vtable);
        const tableSize = this.readUint16(vtable + 2);
        if (vtableSize < 4 || vtableSize % 2 !== 0 || tableSize < 4) {
            throw new DecodeError('bad vtable');
        }
        this.checkRange(vtable, vtableSize);
        this.checkRange(pos, tableSize);
        for (let slot = 4; slot < vtableSize; slot += 2) {
            const field = this.readUint16(vtable + slot);
            if (field !== 0 && (field < 4 || field >= tableSize)) {
                throw new DecodeError('bad field offset');
            }
        }
        this.verifiedTables.add(pos);
    }

    __indirect(offset: number): number {
        const target = super.__indirect(offset);
        this.verifyTable(target);
        return target;
    }

    __union(t: flatbuffers.Table, offset: number): flatbuffers.Table {
        const table = super.__union(t, offset);
        this.verifyTable(table.bb_pos);
        return table;
    }

    __offset(bbPos: number, vtableOffset: number): number {
        const vtable = bbPos - this.readInt32(bbPos);
        const vtableSize = this.readUint16(vtable);
        const tableSize = this.readUint16(vtable + 2);
        if (vtableSize < 4 || vtableSize % 2 !== 0 || tableSize < 4) {
            throw new DecodeError('bad vtable');
        }
        this.checkRange(vtable, vtableSize);
        this.checkRange(bbPos, tableSize);
        if (vtableOffset >= vtableSize) {
            return 0;
        }
        const field = this.readUint16(vtable + vtableOffset);
        if (field === 0) {
            return 0;
        }
        if (field < 4 || field >= tableSize) {
            throw new DecodeError('bad field offset');
        }
        const start = bbPos + field;
        const end = bbPos + tableSize;
        const known = this.fieldEnds.get(start);
        if (known === undefined || end < known) {
            this.fieldEnds.set(start, end);
        }
        return field;
    }

    __string(offset: number, encoding?: any): any {
        const start = offset + this.readInt32(offset);
        const length = this.readInt32(start);
        if (length < 0) {
            throw new DecodeError('negative string length');
        }
        this.checkRange(start + 4, length + 1);
        this.stringBudget -= length;
        if (this.stringBudget < 0) {
            throw new DecodeError('string byte budget');
        }
        if (this.readUint8(start + 4 + length) !== 0) {
            throw new DecodeError('string not NUL terminated');
        }
        return super.__string(offset, encoding);
    }

    __vector_len(offset: number): number {
        const vector = offset + this.readInt32(offset);
        const length = this.readInt32(vector);
        if (length < 0) {
            throw new DecodeError('negative vector length');
        }
        this.checkRange(vector + 4, length * 4);
        return length;
    }
}
