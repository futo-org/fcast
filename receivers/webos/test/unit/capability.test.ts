import * as assert from 'assert';
import { test } from './runner';
import { isV4Capable, receiverMaxVersion, runtimeDescription } from 'common/v4/Capability';

const nodeMajor = Number(process.versions.node.split('.')[0]);

function withEnv(name: string, value: string | undefined, fn: () => void): void {
    const previous = process.env[name];
    if (value === undefined) {
        delete process.env[name];
    }
    else {
        process.env[name] = value;
    }
    try {
        fn();
    }
    finally {
        if (previous === undefined) {
            delete process.env[name];
        }
        else {
            process.env[name] = previous;
        }
    }
}

test('capability: Node >= 12 is v4 capable with max version 4', () => {
    withEnv('FCAST_DISABLE_V4', undefined, () => {
        if (nodeMajor >= 12) {
            assert.strictEqual(isV4Capable(), true);
            assert.strictEqual(receiverMaxVersion(), 4);
        }
    });
});

test('capability: FCAST_DISABLE_V4=1 disables v4 and falls back to max version 3', () => {
    withEnv('FCAST_DISABLE_V4', '1', () => {
        assert.strictEqual(isV4Capable(), false);
        assert.strictEqual(receiverMaxVersion(), 3);
    });
});

test('capability: runtime description names node, openssl and the TLS max version', () => {
    const description = runtimeDescription();
    assert.ok(description.startsWith(`node ${process.version} openssl ${process.versions.openssl} tls-max `), description);
});
