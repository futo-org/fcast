import * as assert from 'assert';
import { test } from './runner';

const dns = require('dns-js');
const DNSPacket = dns.DNSPacket;
const DNSRecord = dns.DNSRecord;

const SERVICE_TYPE = '_fcast._tcp.local';
const INSTANCE_NAME = 'FCast Receiver webOS._fcast._tcp.local';
const SERVICE_TARGET = 'fcast-receiver.local';
const SERVICE_PORT = 46899;
const FINGERPRINT = 'QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc=';
const INTERFACE_COUNT = 12;

const TXT_STRINGS = [
    'version=3',
    'appName=FCast Receiver webOS',
    'appVersion=2.1.0',
    'v=4',
    'fp=' + FINGERPRINT,
];

function interfaceName(index: number): string {
    return 'fcast-receiver-' + index + '.local';
}

function interfaceAddress(index: number): string {
    return '192.168.' + (index + 1) + '.20';
}

function buildAdvertisement(): any {
    const packet = new DNSPacket();
    packet.header.qr = 1;
    packet.header.aa = 1;

    const ptr = new DNSRecord(SERVICE_TYPE, DNSRecord.Type.PTR, DNSRecord.Class.IN, 4500);
    ptr.data = INSTANCE_NAME;
    packet.answer.push(ptr);

    const srv = new DNSRecord(INSTANCE_NAME, DNSRecord.Type.SRV, DNSRecord.Class.IN, 120);
    srv.priority = 0;
    srv.weight = 0;
    srv.port = SERVICE_PORT;
    srv.target = SERVICE_TARGET;
    packet.answer.push(srv);

    const txt = new DNSRecord(INSTANCE_NAME, DNSRecord.Type.TXT, DNSRecord.Class.IN, 120);
    txt.data = {
        version: 3,
        appName: 'FCast Receiver webOS',
        appVersion: '2.1.0',
        v: '4',
        fp: FINGERPRINT,
    };
    packet.additional.push(txt);

    for (let i = 0; i < INTERFACE_COUNT; i++) {
        const a = new DNSRecord(interfaceName(i), DNSRecord.Type.A, DNSRecord.Class.IN, 120);
        a.address = interfaceAddress(i);
        packet.additional.push(a);
    }

    return packet;
}

// dns-js splits TXT values on '.', so the parts that follow a dotted value carry no '='.
function joinTxtStrings(items: string[]): string[] {
    const entries: string[] = [];
    for (const item of items) {
        if (item.indexOf('=') === -1 && entries.length > 0) {
            entries[entries.length - 1] += '.' + item;
        } else {
            entries.push(item);
        }
    }
    return entries;
}

test('mdns: advertisement over 512 bytes serializes without a RangeError', () => {
    const buffer: Buffer = DNSPacket.toBuffer(buildAdvertisement());
    assert.ok(Buffer.isBuffer(buffer));
    assert.ok(buffer.length > 512, 'packet is ' + buffer.length + ' bytes');
});

test('mdns: advertisement over 512 bytes round-trips TXT and A records', () => {
    const buffer: Buffer = DNSPacket.toBuffer(buildAdvertisement());
    const parsed = DNSPacket.parse(buffer);

    const txt = parsed.additional.filter((rec: any) => rec.type === DNSRecord.Type.TXT);
    assert.strictEqual(txt.length, 1);
    assert.deepStrictEqual(joinTxtStrings(txt[0].data), TXT_STRINGS);

    const a = parsed.additional.filter((rec: any) => rec.type === DNSRecord.Type.A);
    assert.strictEqual(a.length, INTERFACE_COUNT);
    for (let i = 0; i < INTERFACE_COUNT; i++) {
        assert.strictEqual(a[i].name, interfaceName(i));
        assert.strictEqual(a[i].address, interfaceAddress(i));
    }
});
