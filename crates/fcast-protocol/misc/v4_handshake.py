#!/usr/bin/env python3
# Generated with claude from the spec
"""
## Requirements

- Python 3.8+ with a TLS 1.3-capable OpenSSL (`python3 -c "import ssl; print(ssl.OPENSSL_VERSION)"`).
- The [`cryptography`](https://pypi.org/project/cryptography/) package (self-signed
  certificate generation and SPKI fingerprint computation).

```sh
python3 -m venv .venv
. .venv/bin/activate
pip install cryptography
```

## Cross-testing against the Rust reference implementation

Usage:
    # Run a self-contained loopback test (Python sender <-> Python receiver),
    # including a negative test that a wrong fingerprint is rejected:
    python3 v4_handshake.py selftest

    # Run a receiver. Prints the `fp` fingerprint a sender must pin.
    python3 v4_handshake.py receiver [--host 0.0.0.0] [--port 46899]

    # Run a sender against a receiver (Python or the Rust reference receiver).
    python3 v4_handshake.py sender --host <ip> --port 46899 --fp <base64-fp>
"""

import argparse
import base64
import datetime
import hashlib
import json
import socket
import ssl
import struct
import sys
import tempfile
import threading
from pathlib import Path
from typing import Optional, Protocol, Tuple

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec

DEFAULT_PORT = 46899
PROTOCOL_VERSION = 4
MAX_PACKET_SIZE = 512 * 1024  # the max value of the `Size` field (opcode + body)

OPCODE_VERSION = 11
OPCODE_PING = 12
OPCODE_PONG = 13

class SocketLike(Protocol):
    def sendall(self, data: bytes) -> None: ...
    def recv(self, n: int) -> bytes: ...

class ProtocolError(Exception):
    pass


class FingerprintMismatch(Exception):
    pass

def encode_packet(opcode: int, body: bytes = b"") -> bytes:
    size = 1 + len(body)  # opcode + body
    if size > MAX_PACKET_SIZE:
        raise ProtocolError(f"packet too large: {size} > {MAX_PACKET_SIZE}")
    return struct.pack("<I", size) + bytes([opcode]) + body

def _read_exact(sock: SocketLike, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ProtocolError("connection closed mid-packet")
        buf += chunk
    return bytes(buf)

def read_packet(sock: SocketLike) -> Tuple[int, bytes]:
    size = struct.unpack("<I", _read_exact(sock, 4))[0]
    if size < 1:
        raise ProtocolError("invalid packet: size must be >= 1 (opcode byte)")
    if size > MAX_PACKET_SIZE:
        raise ProtocolError(f"invalid packet: size {size} exceeds maximum")
    payload = _read_exact(sock, size)
    return payload[0], payload[1:]

def encode_version(version: int) -> bytes:
    body = json.dumps({"version": version}, separators=(",", ":")).encode("utf-8")
    return encode_packet(OPCODE_VERSION, body)

def parse_version(body: bytes) -> int:
    return int(json.loads(body.decode("utf-8"))["version"])

def spki_fingerprint(cert: x509.Certificate) -> str:
    spki_der = cert.public_key().public_bytes(
        serialization.Encoding.DER,
        serialization.PublicFormat.SubjectPublicKeyInfo,
    )
    return base64.b64encode(hashlib.sha256(spki_der).digest()).decode("ascii")

def fingerprint_from_der(cert_der: bytes) -> str:
    return spki_fingerprint(x509.load_der_x509_certificate(cert_der))

def generate_self_signed_cert() -> Tuple[bytes, bytes, str]:
    """Returns (cert_pem, key_pem, fingerprint). ECDSA P-256, matching the
    reference receiver. The certificate identity is intentionally empty: only
    the public key (via the SPKI fingerprint) is used for trust."""
    key = ec.generate_private_key(ec.SECP256R1())
    empty_name = x509.Name([])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(empty_name)
        .issuer_name(empty_name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(days=1))
        .not_valid_after(now + datetime.timedelta(days=3650))
        .sign(key, hashes.SHA256())
    )
    cert_pem = cert.public_bytes(serialization.Encoding.PEM)
    key_pem = key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    )
    return cert_pem, key_pem, spki_fingerprint(cert)

def make_server_tls_context(cert_pem: bytes, key_pem: bytes) -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.maximum_version = ssl.TLSVersion.TLSv1_3
    ctx.verify_mode = ssl.CERT_NONE  # no client certificate (server-auth only)
    # load_cert_chain only accepts file paths.
    with tempfile.TemporaryDirectory() as d:
        cert_path = Path(d) / "cert.pem"
        key_path = Path(d) / "key.pem"
        cert_path.write_bytes(cert_pem)
        key_path.write_bytes(key_pem)
        ctx.load_cert_chain(certfile=str(cert_path), keyfile=str(key_path))
    return ctx

def make_client_tls_context() -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.maximum_version = ssl.TLSVersion.TLSv1_3
    # We pin by SPKI fingerprint, not PKI, so disable chain + hostname checks.
    # IMPORTANT: OpenSSL still verifies the TLS CertificateVerify signature
    # (proof that the peer holds the certificate's private key) regardless of
    # verify_mode -- CERT_NONE only skips chain/hostname/validity checks. This
    # is exactly what the spec requires.
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx

def _exchange_version(sock: SocketLike, who: str) -> int:
    """Send our Version and read the peer's, in plaintext. Returns the peer's
    version. Both sides send before reading, so there is no deadlock."""
    sock.sendall(encode_version(PROTOCOL_VERSION))
    opcode, body = read_packet(sock)
    if opcode != OPCODE_VERSION:
        raise ProtocolError(f"{who}: expected Version (11), got opcode {opcode}")
    peer = parse_version(body)
    if peer != PROTOCOL_VERSION:
        # A full implementation would downgrade to a shared feature set; this
        # PoC only knows how to upgrade for v4.
        raise ProtocolError(
            f"{who}: peer version {peer} unsupported (this PoC only does v4)"
        )
    return peer

def receiver_upgrade(conn: socket.socket, server_ctx: ssl.SSLContext) -> ssl.SSLSocket:
    _exchange_version(conn, "receiver")
    # Switch the existing connection to TLS, acting as the TLS server.
    return server_ctx.wrap_socket(conn, server_side=True)

def sender_upgrade(
    sock: socket.socket, client_ctx: ssl.SSLContext, expected_fp: str
) -> ssl.SSLSocket:
    _exchange_version(sock, "sender")
    # Switch the existing connection to TLS, acting as the TLS client. SNI is
    # not required (the receiver ignores it), so server_hostname is None.
    tls = client_ctx.wrap_socket(sock, server_hostname=None)
    # The TLS handshake already proved private-key possession; now pin the key.
    der = tls.getpeercert(binary_form=True)
    if der is None:
        tls.close()
        raise FingerprintMismatch("receiver presented no certificate")
    got = fingerprint_from_der(der)
    if got != expected_fp:
        tls.close()
        raise FingerprintMismatch(
            f"fingerprint mismatch: got {got!r}, expected {expected_fp!r}"
        )
    return tls

def sender_ping(tls: ssl.SSLSocket) -> None:
    tls.sendall(encode_packet(OPCODE_PING))
    opcode, _ = read_packet(tls)
    if opcode != OPCODE_PONG:
        raise ProtocolError(f"expected Pong (13) after Ping, got opcode {opcode}")

def sender_probe(tls: ssl.SSLSocket, timeout: float = 2.0) -> None:
    """Lenient post-upgrade liveness check that interoperates with any v4 peer.

    Sends a Ping and reports the first packet that comes back. The reference
    Rust receiver does not Pong here: once secured it sends a `ReceiverIntroduction`
    (a Flatbuf message, opcode 20), so we just report whatever opcode arrives to
    show that framed packets ride inside the TLS session."""
    tls.sendall(encode_packet(OPCODE_PING))
    tls.settimeout(timeout)
    try:
        opcode, _ = read_packet(tls)
    except (socket.timeout, TimeoutError):
        print("  (no post-upgrade packet within timeout)")
        return
    finally:
        tls.settimeout(None)
    if opcode == OPCODE_PONG:
        print("  post-upgrade Ping/Pong ok")
    else:
        print(f"  post-upgrade packet received inside TLS (opcode {opcode})")

def receiver_serve(tls: ssl.SSLSocket) -> None:
    """Respond to Pings with Pongs until the peer disconnects."""
    try:
        while True:
            opcode, _ = read_packet(tls)
            if opcode == OPCODE_PING:
                tls.sendall(encode_packet(OPCODE_PONG))
    except (ProtocolError, OSError):
        pass

def run_receiver(host: str, port: int) -> None:
    cert_pem, key_pem, fp = generate_self_signed_cert()
    server_ctx = make_server_tls_context(cert_pem, key_pem)

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((host, port))
    listener.listen()

    print(f"receiver listening on {host}:{port}")
    print(f"  fp (mDNS TXT record): {fp}")
    print(f"  run a sender with:  --host {host} --port {port} --fp {fp}")

    while True:
        conn, peer = listener.accept()
        print(f"connection from {peer[0]}:{peer[1]}")
        try:
            tls = receiver_upgrade(conn, server_ctx)
            print(f"  TLS upgrade ok: {tls.version()} {tls.cipher()[0]}")
            receiver_serve(tls)
            print("  session ended")
        except (ProtocolError, ssl.SSLError, OSError) as e:
            print(f"  session failed: {e}")
        finally:
            try:
                conn.close()
            except OSError:
                pass

def connect_and_upgrade(
    host: str, port: int, expected_fp: str
) -> ssl.SSLSocket:
    client_ctx = make_client_tls_context()
    sock = socket.create_connection((host, port))
    return sender_upgrade(sock, client_ctx, expected_fp)

def run_sender(host: str, port: int, expected_fp: str) -> None:
    tls = connect_and_upgrade(host, port, expected_fp)
    print(f"TLS upgrade ok: {tls.version()} {tls.cipher()[0]}")
    print(f"fingerprint verified: {expected_fp}")
    sender_probe(tls)
    tls.close()

def run_selftest() -> int:
    cert_pem, key_pem, fp = generate_self_signed_cert()
    server_ctx = make_server_tls_context(cert_pem, key_pem)

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    port = listener.getsockname()[1]
    print(f"selftest: receiver fp = {fp}")
    print(f"selftest: listening on 127.0.0.1:{port}")

    def accept_loop() -> None:
        while True:
            try:
                conn, _ = listener.accept()
            except OSError:
                return
            try:
                tls = receiver_upgrade(conn, server_ctx)
                receiver_serve(tls)
            except (ProtocolError, ssl.SSLError, OSError):
                pass
            finally:
                try:
                    conn.close()
                except OSError:
                    pass

    server_thread = threading.Thread(target=accept_loop, daemon=True)
    server_thread.start()

    failures = 0

    # 0. Framing must be byte-identical to the Rust reference wire format:
    #    Size(LE u32) = opcode(1) + body ; opcode 11 ; body = compact JSON.
    expected_wire = b"\x0e\x00\x00\x00\x0b" + b'{"version":4}'
    if encode_version(PROTOCOL_VERSION) == expected_wire:
        print("PASS: Version packet framing matches the reference wire format")
    else:
        failures += 1
        print(
            f"FAIL: framing mismatch, got {encode_version(PROTOCOL_VERSION)!r} "
            f"expected {expected_wire!r}"
        )

    # 1. Happy path: correct fingerprint must succeed, and the secured channel
    #    must carry a framed Ping/Pong.
    try:
        tls = connect_and_upgrade("127.0.0.1", port, fp)
        sender_ping(tls)  # strict: requires a Pong over TLS
        tls.close()
        print("PASS: handshake + Ping/Pong with correct fingerprint")
    except Exception as e:  # noqa: BLE001 - test harness
        failures += 1
        print(f"FAIL: handshake with correct fingerprint raised {e!r}")

    # 2. Negative: a wrong fingerprint must be rejected.
    wrong_fp = base64.b64encode(b"\x00" * 32).decode("ascii")
    try:
        run_sender("127.0.0.1", port, wrong_fp)
        failures += 1
        print("FAIL: handshake with wrong fingerprint was NOT rejected")
    except FingerprintMismatch:
        print("PASS: handshake with wrong fingerprint rejected")
    except Exception as e:
        failures += 1
        print(f"FAIL: wrong fingerprint raised unexpected {e!r}")

    listener.close()
    print("selftest:", "OK" if failures == 0 else f"{failures} FAILURE(S)")
    return 1 if failures else 0


def main(argv: Optional[list] = None) -> int:
    parser = argparse.ArgumentParser(description="FCast v4 handshake PoC")
    sub = parser.add_subparsers(dest="mode", required=True)

    p_recv = sub.add_parser("receiver", help="run a receiver")
    p_recv.add_argument("--host", default="0.0.0.0")
    p_recv.add_argument("--port", type=int, default=DEFAULT_PORT)

    p_send = sub.add_parser("sender", help="run a sender against a receiver")
    p_send.add_argument("--host", required=True)
    p_send.add_argument("--port", type=int, default=DEFAULT_PORT)
    p_send.add_argument("--fp", required=True, help="receiver SPKI fingerprint (base64)")

    sub.add_parser("selftest", help="run a loopback sender<->receiver test")

    args = parser.parse_args(argv)

    try:
        sys.stdout.reconfigure(line_buffering=True)
    except (AttributeError, ValueError):
        pass

    if args.mode == "receiver":
        run_receiver(args.host, args.port)
        return 0
    if args.mode == "sender":
        try:
            run_sender(args.host, args.port, args.fp)
        except FingerprintMismatch as e:
            print(f"REJECTED: {e}", file=sys.stderr)
            return 2
        return 0
    if args.mode == "selftest":
        return run_selftest()
    return 1


if __name__ == "__main__":
    sys.exit(main())
