import * as assert from 'assert';
import { test } from './runner';
import { buildConnectionInfo, buildConnectionUrl } from 'common/ConnectionUrl';

const PREFIX = 'fcast://r/';
const FINGERPRINT = 'QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc=';

function decodeConnectionUrl(url: string): any {
    assert.ok(url.startsWith(PREFIX), url);
    let payload = url.slice(PREFIX.length).replace(/-/g, '+').replace(/_/g, '/');
    while (payload.length % 4 !== 0) {
        payload += '=';
    }
    return JSON.parse(Buffer.from(payload, 'base64').toString('utf8'));
}

test('connection url: spec example decodes to the documented document', () => {
    const url = buildConnectionUrl('Living Room', ['192.168.1.42', 'fe80::1ff:fe23:4567:890a'], { v: '4', fp: FINGERPRINT });
    assert.deepStrictEqual(decodeConnectionUrl(url), {
        name: 'Living Room',
        addresses: ['192.168.1.42', 'fe80::1ff:fe23:4567:890a'],
        services: [{ port: 46899, type: 0 }],
        txt: { v: '4', fp: FINGERPRINT },
    });
});

test('connection url: payload is base64url without padding or reserved characters', () => {
    const url = buildConnectionUrl('Living Room', ['192.168.1.42', 'fe80::1ff:fe23:4567:890a'], { v: '4', fp: FINGERPRINT });
    const payload = url.slice(PREFIX.length);
    assert.ok(!/[=+/]/.test(payload), payload);
});

test('connection url: non-ASCII name round-trips as UTF-8', () => {
    const url = buildConnectionUrl('Salotto è', ['10.0.0.2'], null);
    assert.strictEqual(decodeConnectionUrl(url).name, 'Salotto è');
});

test('connection url: null txt omits the txt key', () => {
    const decoded = decodeConnectionUrl(buildConnectionUrl('Living Room', ['10.0.0.2'], null));
    assert.deepStrictEqual(Object.keys(decoded), ['name', 'addresses', 'services']);
});

test('connection info: txt is omitted when null and included otherwise', () => {
    assert.deepStrictEqual(buildConnectionInfo('TV', ['10.0.0.2'], null), {
        name: 'TV',
        addresses: ['10.0.0.2'],
        services: [{ port: 46899, type: 0 }],
    });
    assert.deepStrictEqual(buildConnectionInfo('TV', ['10.0.0.2'], { v: '4' }).txt, { v: '4' });
});
