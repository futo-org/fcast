import * as net from 'net';
import * as stream from 'stream';
import * as tls from 'tls';

// First byte of a TLS handshake record. Any other first byte cannot start a TLS ClientHello.
const TLS_HANDSHAKE_RECORD = 0x16;

// Duplex over an accepted socket. Writes go to the socket. Reads yield the bytes that were
// already consumed from the socket, then the socket's data.
class SocketDuplex extends stream.Duplex {
    constructor(private socket: net.Socket) {
        super();
    }

    _read() {
        this.socket.resume();
    }

    _write(chunk: Buffer, encoding: string, callback: (error?: Error | null) => void) {
        this.socket.write(chunk, (err?: Error) => callback(err));
    }

    _final(callback: (error?: Error | null) => void) {
        this.socket.end();
        callback();
    }

    _destroy(error: Error | null, callback: (error?: Error | null) => void) {
        this.socket.destroy();
        callback(error);
    }
}

// Wraps an accepted socket in a server-side TLS session. `head` holds the bytes read from the
// socket after the Version frame; they are the beginning of the TLS stream. When the stream does
// not start with a TLS handshake record the socket is destroyed and null is returned, so no TLS
// alert is ever written to a peer that is not speaking TLS.
export function createTlsServerSocket(socket: net.Socket, head: Buffer, secureContext: tls.SecureContext): tls.TLSSocket | null {
    if (head.length > 0 && head[0] !== TLS_HANDSHAKE_RECORD) {
        socket.destroy();
        return null;
    }

    const duplex = new SocketDuplex(socket);
    let admitted = head.length > 0;
    duplex.on('error', () => socket.destroy());

    if (head.length > 0) {
        duplex.push(head);
    }

    socket.on('data', (chunk: Buffer) => {
        if (!admitted) {
            admitted = true;
            if (chunk[0] !== TLS_HANDSHAKE_RECORD) {
                socket.destroy();
                return;
            }
        }

        if (!duplex.push(chunk)) {
            socket.pause();
        }
    });
    socket.on('end', () => duplex.push(null));
    socket.on('error', (err: Error) => duplex.destroy(err));
    socket.on('close', () => duplex.destroy());

    return new tls.TLSSocket(duplex, { isServer: true, secureContext: secureContext });
}
