import { EncryptedMessage, DecryptedMessage, KeyExchangeMessage } from '../src/Packets';
import { generateKeyPair, computeSharedSecret, encryptMessage, decryptMessage, createDiffieHellman, Opcode } from '../src/FCastSession';

/*test("testDHEncryptionSelf", () => {
    const keyPair1 = generateKeyPair();
    const keyPair2 = generateKeyPair();

    const aesKey1 = computeSharedSecret(keyPair1, { version:1, publicKey: keyPair2.getPublicKey().toString('base64') });
    const aesKey2 = computeSharedSecret(keyPair2, { version:1, publicKey: keyPair1.getPublicKey().toString('base64') });

    expect(aesKey1.toString('base64')).toBe(aesKey2.toString('base64'));

    const message: DecryptedMessage = { opcode: 1, message: 'text/html' };
    const encryptedMessage: EncryptedMessage = encryptMessage(aesKey1, message);
    const decryptedMessage: DecryptedMessage = decryptMessage(aesKey1, encryptedMessage);

    expect(decryptedMessage.opcode).toBe(message.opcode);
    expect(decryptedMessage.message).toBe(message.message);
});*/

test("testDHEncryptionKnown", () => {
    const encodedPrivateKey1 = "MIIDJwIBADCCAhgGCSqGSIb3DQEDATCCAgkCggEBAJVHXPXZPllsP80dkCrdAvQn9fPHIQMTu0X7TVuy5f4cvWeM1LvdhMmDa+HzHAd3clrrbC/Di4X0gHb6drzYFGzImm+y9wbdcZiYwgg9yNiW+EBi4snJTRN7BUqNgJatuNUZUjmO7KhSoK8S34Pkdapl1OwMOKlWDVZhGG/5i5/J62Du6LAwN2sja8c746zb10/WHB0kdfowd7jwgEZ4gf9+HKVv7gZteVBq3lHtu1RDpWOSfbxLpSAIZ0YXXIiFkl68ZMYUeQZ3NJaZDLcU7GZzBOJh+u4zs8vfAI4MP6kGUNl9OQnJJ1v0rIb/yz0D5t/IraWTQkLdbTvMoqQGywsCggEAQt67naWz2IzJVuCHh+w/Ogm7pfSLiJp0qvUxdKoPvn48W4/NelO+9WOw6YVgMolgqVF/QBTTMl/Hlivx4Ek3DXbRMUp2E355Lz8NuFnQleSluTICTweezy7wnHl0UrB3DhNQeC7Vfd95SXnc7yPLlvGDBhllxOvJPJxxxWuSWVWnX5TMzxRJrEPVhtC+7kMlGwsihzSdaN4NFEQD8T6AL0FG2ILgV68ZtvYnXGZ2yPoOPKJxOjJX/Rsn0GOfaV40fY0c+ayBmibKmwTLDrm3sDWYjRW7rGUhKlUjnPx+WPrjjXJQq5mR/7yXE0Al/ozgTEOZrZZWm+kaVG9JeGk8egSCAQQCggEAECNvEczf0y6IoX/IwhrPeWZ5IxrHcpwjcdVAuyZQLLlOq0iqnYMFcSD8QjMF8NKObfZZCDQUJlzGzRsG0oXsWiWtmoRvUZ9tQK0j28hDylpbyP00Bt9NlMgeHXkAy54P7Z2v/BPCd3o23kzjgXzYaSRuCFY7zQo1g1IQG8mfjYjdE4jjRVdVrlh8FS8x4OLPeglc+cp2/kuyxaVEfXAG84z/M8019mRSfdczi4z1iidPX6HgDEEWsN42Ud60mNKy5jsQpQYkRdOLmxR3+iQEtGFjdzbVhVCUr7S5EORU9B1MOl5gyPJpjfU3baOqrg6WXVyTvMDaA05YEnAHQNOOfA==";
    const keyExchangeMessage2: KeyExchangeMessage = { version: 1, publicKey: "MIIDJTCCAhgGCSqGSIb3DQEDATCCAgkCggEBAJVHXPXZPllsP80dkCrdAvQn9fPHIQMTu0X7TVuy5f4cvWeM1LvdhMmDa+HzHAd3clrrbC/Di4X0gHb6drzYFGzImm+y9wbdcZiYwgg9yNiW+EBi4snJTRN7BUqNgJatuNUZUjmO7KhSoK8S34Pkdapl1OwMOKlWDVZhGG/5i5/J62Du6LAwN2sja8c746zb10/WHB0kdfowd7jwgEZ4gf9+HKVv7gZteVBq3lHtu1RDpWOSfbxLpSAIZ0YXXIiFkl68ZMYUeQZ3NJaZDLcU7GZzBOJh+u4zs8vfAI4MP6kGUNl9OQnJJ1v0rIb/yz0D5t/IraWTQkLdbTvMoqQGywsCggEAQt67naWz2IzJVuCHh+w/Ogm7pfSLiJp0qvUxdKoPvn48W4/NelO+9WOw6YVgMolgqVF/QBTTMl/Hlivx4Ek3DXbRMUp2E355Lz8NuFnQleSluTICTweezy7wnHl0UrB3DhNQeC7Vfd95SXnc7yPLlvGDBhllxOvJPJxxxWuSWVWnX5TMzxRJrEPVhtC+7kMlGwsihzSdaN4NFEQD8T6AL0FG2ILgV68ZtvYnXGZ2yPoOPKJxOjJX/Rsn0GOfaV40fY0c+ayBmibKmwTLDrm3sDWYjRW7rGUhKlUjnPx+WPrjjXJQq5mR/7yXE0Al/ozgTEOZrZZWm+kaVG9JeGk8egOCAQUAAoIBAGlL9EYsrFz3I83NdlwhM241M+M7PA9P5WXgtdvS+pcalIaqN2IYdfzzCUfye7lchVkT9A2Y9eWQYX0OUhmjf8PPKkRkATLXrqO5HTsxV96aYNxMjz5ipQ6CaErTQaPLr3OPoauIMPVVI9zM+WT0KOGp49YMyx+B5rafT066vOVbF/0z1crq0ZXxyYBUv135rwFkIHxBMj5bhRLXKsZ2G5aLAZg0DsVam104mgN/v75f7Spg/n5hO7qxbNgbvSrvQ7Ag/rMk5T3sk7KoM23Qsjl08IZKs2jjx21MiOtyLqGuCW6GOTNK4yEEDF5gA0K13eXGwL5lPS0ilRw+Lrw7cJU=" };

    const dh = createDiffieHellman();
    dh.setPrivateKey(Buffer.from(encodedPrivateKey1, 'base64'));

    const aesKey1 = computeSharedSecret(dh, keyExchangeMessage2);
    expect(aesKey1.toString('base64')).toBe("vI5LGE625zGEG350ggkyBsIAXm2y4sNohiPcED1oAEE=");

    const message = { opcode: 1, message: 'text/html' };
    const serializedBody = JSON.stringify(message);
    const encryptedMessage = encryptMessage(aesKey1, message as DecryptedMessage);
    const decryptedMessage = decryptMessage(aesKey1, encryptedMessage as EncryptedMessage);

    expect(decryptedMessage.opcode).toBe(1);
    expect(decryptedMessage.message).toBe(serializedBody);
});

/*test("testAESKeyGeneration", () => {
    const testCases = [
        {
            publicKey: "MIIBHzCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECA4GEAAKBgEnOS0oHteVA+3kND3u4yXe7GGRohy1LkR9Q5tL4c4ylC5n4iSwWSoIhcSIvUMWth6KAhPhu05sMcPY74rFMSS2AGTNCdT/5KilediipuUMdFVvjGqfNMNH1edzW5mquIw3iXKdfQmfY/qxLTI2wccyDj4hHFhLCZL3Y+shsm3KF",
            privateKey: "MIIBIQIBADCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECBIGDAoGAeo/ceIeH8Jt1ZRNKX5aTHkMi23GCV1LtcS2O6Tktn9k8DCv7gIoekysQUhMyWtR+MsZlq2mXjr1JFpAyxl89rqoEPU6QDsGe9q8R4O8eBZ2u+48mkUkGSh7xPGRQUBvmhH2yk4hIEA8aK4BcYi1OTsCZtmk7pQq+uaFkKovD/8M=",
            expectedAES: "7dpl1/6KQTTooOrFf2VlUOSqgrFHi6IYxapX0IxFfwk="
        },
        {
            publicKey: "MIIBHzCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECA4GEAAKBgGvIlCP/S+xpAuNEHSn4cEDOL1esUf+uMuY2Kp5J10a7HGbwzNd+7eYsgEc4+adddgB7hJgTvjsGg7lXUhHQ7WbfbCGgt7dbkx8qkic6Rgq4f5eRYd1Cgidw4MhZt7mEIOKrHweqnV6B9rypbXjbqauc6nGgtwx+Gvl6iLpVATRK",
            privateKey: "MIIBIQIBADCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECBIGDAoGAMXmiIgWyutbaO+f4UiMAb09iVVSCI6Lb6xzNyD2MpUZyk4/JOT04Daj4JeCKFkF1Fq79yKhrnFlXCrF4WFX00xUOXb8BpUUUH35XG5ApvolQQLL6N0om8/MYP4FK/3PUxuZAJz45TUsI/v3u6UqJelVTNL83ltcFbZDIfEVftRA=",
            expectedAES: "a2tUSxnXifKohfNocAQHkAlPffDv6ReihJ7OojBGt0Q="
        }
    ];

    testCases.forEach(({ publicKey, privateKey, expectedAES }) => {
        const theirPublicKey = Buffer.from(publicKey, 'base64');
        const dh = createDiffieHellman();
        dh.setPrivateKey(Buffer.from(privateKey, 'base64'));
        const aesKey = computeSharedSecret(dh, { version: 1, publicKey: theirPublicKey.toString('base64') });
        expect(aesKey.toString('base64')).toBe(expectedAES);
    });
});*/

/*test("testDecryptMessageKnown", () => {
    const encryptedMessage: EncryptedMessage = {
        version: 1,
        iv: "C4H70VC5FWrNtkty9/cLIA==",
        blob: "K6/N7JMyi1PFwKhU0mFj7ZJmd/tPp3NCOMldmQUtDaQ7hSmPoIMI5QNMOj+NFEiP4qTgtYp5QmBPoQum6O88pA=="
    };
    const aesKeyBase64 = "+hr9Jg8yre7S9WGUohv2AUSzHNQN514JPh6MoFAcFNU=";

    const aesKey = Buffer.from(aesKeyBase64, 'base64');
    const decryptedMessage = decryptMessage(aesKey, encryptedMessage);

    expect(decryptedMessage.opcode).toBe(Opcode.Play);
    expect(decryptedMessage.message).toBe("{\"container\":\"text/html\"}");
});*/