import * as assert from 'assert';
import { test } from './runner';
import { buildTxtRecord, DiscoveryService } from 'common/DiscoveryService';
import { PROTOCOL_VERSION } from 'common/Packets';

const FINGERPRINT = 'QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc=';

test('discovery: TXT record without v4 keeps only the existing keys', () => {
    assert.deepStrictEqual(buildTxtRecord('FCast Receiver webOS', '2.0.1', null), {
        version: PROTOCOL_VERSION,
        appName: 'FCast Receiver webOS',
        appVersion: '2.0.1',
    });
});

test('discovery: TXT record with v4 adds v and fp', () => {
    assert.deepStrictEqual(buildTxtRecord('FCast Receiver webOS', '2.0.1', { fingerprint: FINGERPRINT }), {
        version: PROTOCOL_VERSION,
        appName: 'FCast Receiver webOS',
        appVersion: '2.0.1',
        v: '4',
        fp: FINGERPRINT,
    });
});

test('discovery: the advertisement is restarted only when the addresses change', () => {
    const log: string[] = [];
    let created = 0;
    const discovery = new DiscoveryService(() => {
        const id = ++created;
        return {
            start: () => { log.push('start ' + id); },
            stop: () => { log.push('stop ' + id); },
        };
    });

    assert.strictEqual(discovery.update(['192.168.1.10']), true);
    // The app reports the same network again each time its main screen loads.
    assert.strictEqual(discovery.update(['192.168.1.10']), false);
    assert.strictEqual(discovery.update(['192.168.1.10']), false);
    assert.deepStrictEqual(log, ['start 1']);

    assert.strictEqual(discovery.update(['192.168.1.10', '10.0.0.2']), true);
    assert.strictEqual(discovery.update(['10.0.0.2', '192.168.1.10']), false);
    assert.strictEqual(discovery.update([]), true);
    assert.deepStrictEqual(log, ['start 1', 'stop 1', 'start 2', 'stop 2', 'start 3']);
});
