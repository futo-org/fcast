#!/usr/bin/env node
/*
 * Generated with claude from the spec
 *
 * Pure Node.js (no npm dependencies). The `openssl` CLI is used once to
 * generate the receiver's self-signed certificate; everything else uses the
 * Node standard library. Requires Node 15.6+ (for `getPeerX509Certificate`).
 *
 * Usage:
 *   node v4_handshake.js selftest
 *   node v4_handshake.js receiver [--host 0.0.0.0] [--port 46899]
 *   node v4_handshake.js sender --host <ip> --port 46899 --fp <base64-fp>
 */

"use strict";

const net = require("net");
const tls = require("tls");
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const DEFAULT_PORT = 46899;
const PROTOCOL_VERSION = 4;
const MAX_PACKET_SIZE = 512 * 1024; // max value of the `Size` field (opcode + body)

const OPCODE_VERSION = 11;
const OPCODE_PING = 12;
const OPCODE_PONG = 13;

class ProtocolError extends Error {}
class FingerprintMismatch extends Error {}

function encodePacket(opcode, body = Buffer.alloc(0)) {
  const size = 1 + body.length; // opcode + body
  if (size > MAX_PACKET_SIZE) {
    throw new ProtocolError(`packet too large: ${size} > ${MAX_PACKET_SIZE}`);
  }
  const header = Buffer.alloc(5);
  header.writeUInt32LE(size, 0);
  header.writeUInt8(opcode, 4);
  return Buffer.concat([header, body]);
}

function readExact(sock, n) {
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (fn, arg) => {
      if (settled) return;
      settled = true;
      sock.removeListener("readable", onReadable);
      sock.removeListener("error", onError);
      sock.removeListener("end", onEnd);
      fn(arg);
    };
    const tryRead = () => {
      const chunk = sock.read(n);
      if (chunk) finish(resolve, chunk);
    };
    const onReadable = () => tryRead();
    const onError = (e) => finish(reject, e);
    const onEnd = () => finish(reject, new ProtocolError("connection closed mid-packet"));
    sock.on("readable", onReadable);
    sock.on("error", onError);
    sock.on("end", onEnd);
    tryRead(); // handle already-buffered data
  });
}

async function readPacket(sock) {
  const header = await readExact(sock, 4);
  const size = header.readUInt32LE(0);
  if (size < 1) throw new ProtocolError("invalid packet: size must be >= 1");
  if (size > MAX_PACKET_SIZE) throw new ProtocolError(`invalid packet: size ${size} too large`);
  const payload = await readExact(sock, size);
  return { opcode: payload.readUInt8(0), body: payload.subarray(1) };
}

function writeAll(sock, buf) {
  return new Promise((resolve, reject) => {
    sock.write(buf, (err) => (err ? reject(err) : resolve()));
  });
}

function encodeVersion(version) {
  const body = Buffer.from(JSON.stringify({ version }), "utf8");
  return encodePacket(OPCODE_VERSION, body);
}

function parseVersion(body) {
  return JSON.parse(body.toString("utf8")).version;
}

function fingerprintFromX509(cert) {
  const spkiDer = cert.publicKey.export({ type: "spki", format: "der" });
  return crypto.createHash("sha256").update(spkiDer).digest("base64");
}

function generateSelfSignedCert() {
  // ECDSA P-256, self-signed, empty subject -- identity is ignored; only the
  // public key matters. Node has no certificate builder, so we use the openssl
  // CLI (Node's own TLS is built on OpenSSL).
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "v4node-"));
  const keyPath = path.join(dir, "key.pem");
  const certPath = path.join(dir, "cert.pem");
  try {
    execFileSync(
      "openssl",
      [
        "req", "-x509", "-newkey", "ec",
        "-pkeyopt", "ec_paramgen_curve:prime256v1",
        "-nodes", "-keyout", keyPath, "-out", certPath,
        "-days", "3650", "-subj", "/", "-batch",
      ],
      { stdio: "ignore" }
    );
    const certPem = fs.readFileSync(certPath);
    const keyPem = fs.readFileSync(keyPath);
    const cert = new crypto.X509Certificate(certPem);
    return { certPem, keyPem, fp: fingerprintFromX509(cert) };
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

async function exchangeVersion(sock, who) {
  // Both sides send before reading, so the plaintext exchange cannot deadlock.
  await writeAll(sock, encodeVersion(PROTOCOL_VERSION));
  const { opcode, body } = await readPacket(sock);
  if (opcode !== OPCODE_VERSION) {
    throw new ProtocolError(`${who}: expected Version (11), got opcode ${opcode}`);
  }
  const peer = parseVersion(body);
  if (peer !== PROTOCOL_VERSION) {
    // A full implementation would downgrade; this PoC only knows v4.
    throw new ProtocolError(`${who}: peer version ${peer} unsupported (this PoC only does v4)`);
  }
  return peer;
}

async function receiverUpgrade(socket, certInfo) {
  await exchangeVersion(socket, "receiver");
  // Switch the existing connection to TLS, acting as the TLS server.
  return await new Promise((resolve, reject) => {
    const tlsSock = new tls.TLSSocket(socket, {
      isServer: true,
      key: certInfo.keyPem,
      cert: certInfo.certPem,
      minVersion: "TLSv1.3",
      maxVersion: "TLSv1.3",
      requestCert: false, // no client certificate (server-auth only)
    });
    tlsSock.once("secure", () => resolve(tlsSock));
    tlsSock.once("error", reject);
    tlsSock.once("_tlsError", reject);
  });
}

async function senderUpgrade(socket, expectedFp) {
  await exchangeVersion(socket, "sender");
  // Switch the existing connection to TLS, acting as the TLS client. SNI is
  // not required (the receiver ignores it). rejectUnauthorized:false skips the
  // PKI chain/hostname/validity checks, but OpenSSL still verifies the TLS
  // CertificateVerify signature (proof of private-key possession).
  return await new Promise((resolve, reject) => {
    const tlsSock = tls.connect(
      {
        socket,
        minVersion: "TLSv1.3",
        maxVersion: "TLSv1.3",
        rejectUnauthorized: false,
      },
      () => {
        try {
          const cert = tlsSock.getPeerX509Certificate();
          if (!cert) throw new FingerprintMismatch("receiver presented no certificate");
          const got = fingerprintFromX509(cert);
          if (got !== expectedFp) {
            throw new FingerprintMismatch(`fingerprint mismatch: got ${got}, expected ${expectedFp}`);
          }
          resolve(tlsSock);
        } catch (e) {
          tlsSock.destroy();
          reject(e);
        }
      }
    );
    tlsSock.once("error", reject);
  });
}

async function senderPing(tlsSock) {
  // Strict: used against a peer known to be this PoC's receiver.
  await writeAll(tlsSock, encodePacket(OPCODE_PING));
  const { opcode } = await readPacket(tlsSock);
  if (opcode !== OPCODE_PONG) {
    throw new ProtocolError(`expected Pong (13) after Ping, got opcode ${opcode}`);
  }
}

async function senderProbe(tlsSock, timeoutMs = 2000) {
  await writeAll(tlsSock, encodePacket(OPCODE_PING));
  let timer;
  const timeout = new Promise((res) => (timer = setTimeout(() => res(null), timeoutMs)));
  const pkt = await Promise.race([readPacket(tlsSock).catch(() => null), timeout]);
  clearTimeout(timer);
  if (!pkt) console.log("  (no post-upgrade packet within timeout)");
  else if (pkt.opcode === OPCODE_PONG) console.log("  post-upgrade Ping/Pong ok");
  else console.log(`  post-upgrade packet received inside TLS (opcode ${pkt.opcode})`);
}

async function receiverServe(tlsSock) {
  // Respond to Pings with Pongs until the peer disconnects.
  try {
    for (;;) {
      const { opcode } = await readPacket(tlsSock);
      if (opcode === OPCODE_PING) await writeAll(tlsSock, encodePacket(OPCODE_PONG));
    }
  } catch (_) {
    /* peer closed */
  }
}

function runReceiver(host, port) {
  const certInfo = generateSelfSignedCert();
  const server = net.createServer((socket) => {
    const peer = `${socket.remoteAddress}:${socket.remotePort}`;
    console.log(`connection from ${peer}`);
    receiverUpgrade(socket, certInfo)
      .then(async (tlsSock) => {
        console.log(`  TLS upgrade ok: ${tlsSock.getProtocol()} ${tlsSock.getCipher().name}`);
        await receiverServe(tlsSock);
        console.log("  session ended");
      })
      .catch((e) => console.log(`  session failed: ${e.message}`));
  });
  server.listen(port, host, () => {
    console.log(`receiver listening on ${host}:${port}`);
    console.log(`  fp (mDNS TXT record): ${certInfo.fp}`);
    console.log(`  run a sender with:  --host ${host} --port ${port} --fp ${certInfo.fp}`);
  });
}

async function runSender(host, port, expectedFp) {
  const socket = net.connect({ host, port });
  await new Promise((resolve, reject) => {
    socket.once("connect", resolve);
    socket.once("error", reject);
  });
  const tlsSock = await senderUpgrade(socket, expectedFp);
  console.log(`TLS upgrade ok: ${tlsSock.getProtocol()} ${tlsSock.getCipher().name}`);
  console.log(`fingerprint verified: ${expectedFp}`);
  await senderProbe(tlsSock);
  tlsSock.destroy();
}

async function runSelftest() {
  const certInfo = generateSelfSignedCert();
  let failures = 0;

  // 0. Framing must be byte-identical to the Rust reference wire format:
  //    Size(LE u32) = opcode(1) + body ; opcode 11 ; body = compact JSON.
  const expectedWire = Buffer.concat([
    Buffer.from([0x0e, 0x00, 0x00, 0x00, 0x0b]),
    Buffer.from('{"version":4}', "utf8"),
  ]);
  if (encodeVersion(PROTOCOL_VERSION).equals(expectedWire)) {
    console.log("PASS: Version packet framing matches the reference wire format");
  } else {
    failures++;
    console.log(`FAIL: framing mismatch, got ${encodeVersion(PROTOCOL_VERSION).toString("hex")}`);
  }

  const server = net.createServer((socket) => {
    receiverUpgrade(socket, certInfo)
      .then((tlsSock) => receiverServe(tlsSock))
      .catch(() => {});
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;
  console.log(`selftest: receiver fp = ${certInfo.fp}`);
  console.log(`selftest: listening on 127.0.0.1:${port}`);

  // 1. Happy path: correct fingerprint must succeed, and the secured channel
  //    must carry a framed Ping/Pong.
  try {
    const socket = net.connect({ host: "127.0.0.1", port });
    await new Promise((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    const tlsSock = await senderUpgrade(socket, certInfo.fp);
    await senderPing(tlsSock); // strict: requires a Pong over TLS
    tlsSock.destroy();
    console.log("PASS: handshake + Ping/Pong with correct fingerprint");
  } catch (e) {
    failures++;
    console.log(`FAIL: handshake with correct fingerprint raised ${e}`);
  }

  // 2. Negative: a wrong fingerprint must be rejected.
  const wrongFp = Buffer.alloc(32).toString("base64");
  try {
    const socket = net.connect({ host: "127.0.0.1", port });
    await new Promise((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    await senderUpgrade(socket, wrongFp);
    failures++;
    console.log("FAIL: handshake with wrong fingerprint was NOT rejected");
  } catch (e) {
    if (e instanceof FingerprintMismatch) {
      console.log("PASS: handshake with wrong fingerprint rejected");
    } else {
      failures++;
      console.log(`FAIL: wrong fingerprint raised unexpected ${e}`);
    }
  }

  server.close();
  console.log("selftest:", failures === 0 ? "OK" : `${failures} FAILURE(S)`);
  return failures === 0 ? 0 : 1;
}

function parseArgs(argv) {
  const opts = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith("--")) throw new Error(`unexpected argument: ${argv[i]}`);
    opts[argv[i].slice(2)] = argv[i + 1];
  }
  return opts;
}

async function main() {
  const [mode, ...rest] = process.argv.slice(2);
  if (mode === "receiver") {
    const o = parseArgs(rest);
    runReceiver(o.host || "0.0.0.0", parseInt(o.port || DEFAULT_PORT, 10));
    // Never resolve: the listening server keeps the event loop alive. Returning
    // would let `main().then(process.exit)` kill the just-started server.
    return await new Promise(() => {});
  }
  if (mode === "sender") {
    const o = parseArgs(rest);
    if (!o.host || !o.fp) {
      console.error("usage: sender --host <ip> --port <port> --fp <base64-fp>");
      return 1;
    }
    try {
      await runSender(o.host, parseInt(o.port || DEFAULT_PORT, 10), o.fp);
    } catch (e) {
      if (e instanceof FingerprintMismatch) {
        console.error(`REJECTED: ${e.message}`);
        return 2;
      }
      throw e;
    }
    return 0;
  }
  if (mode === "selftest") {
    return await runSelftest();
  }
  console.error("usage: node v4_handshake.js <receiver|sender|selftest> [options]");
  return 1;
}

main().then((code) => process.exit(code)).catch((e) => {
  console.error(e);
  process.exit(1);
});
