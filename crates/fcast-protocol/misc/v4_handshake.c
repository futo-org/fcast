/*
 * Generated with claude from the spec
 *
 * Build:
 *   cc v4_handshake.c -o v4_handshake $(pkg-config --cflags --libs openssl)
 *   # or: cc v4_handshake.c -o v4_handshake -lssl -lcrypto
 *
 * Usage:
 *   ./v4_handshake selftest
 *   ./v4_handshake receiver [--host 0.0.0.0] [--port 46899]
 *   ./v4_handshake sender --host <ip> --port 46899 --fp <base64-fp>
 */

#include <arpa/inet.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include <openssl/bio.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/sha.h>
#include <openssl/ssl.h>
#include <openssl/x509.h>
#include <openssl/x509v3.h>

#define DEFAULT_PORT 46899
#define PROTOCOL_VERSION 4
#define MAX_PACKET_SIZE (512 * 1024) /* max value of `Size` (opcode + body) */

#define OPCODE_VERSION 11
#define OPCODE_PING 12
#define OPCODE_PONG 13

static void die_ssl(const char *msg) {
    fprintf(stderr, "%s: ", msg);
    ERR_print_errors_fp(stderr);
    exit(1);
}

static int read_exact_fd(int fd, uint8_t *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        ssize_t r = recv(fd, buf + got, n - got, 0);
        if (r == 0) return -1;          /* peer closed */
        if (r < 0) return -1;           /* error */
        got += (size_t)r;
    }
    return 0;
}

static int write_all_fd(int fd, const uint8_t *buf, size_t n) {
    size_t sent = 0;
    while (sent < n) {
        ssize_t w = send(fd, buf + sent, n - sent, 0);
        if (w <= 0) return -1;
        sent += (size_t)w;
    }
    return 0;
}

static int read_exact_ssl(SSL *ssl, uint8_t *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        int r = SSL_read(ssl, buf + got, (int)(n - got));
        if (r <= 0) return -1;
        got += (size_t)r;
    }
    return 0;
}

static int write_all_ssl(SSL *ssl, const uint8_t *buf, size_t n) {
    size_t sent = 0;
    while (sent < n) {
        int w = SSL_write(ssl, buf + sent, (int)(n - sent));
        if (w <= 0) return -1;
        sent += (size_t)w;
    }
    return 0;
}

/* --- Packet framing (spec: "Overview") -----------------------------------
 *
 *   Size (LE u32) | Opcode (u8) | Body...
 *
 * `Size` is little-endian and counts Opcode + Body (NOT the 4-byte Size field
 * itself), so the body length is `Size - 1`.
 */

/* Encodes a packet into `out` (must hold 5 + body_len bytes). Returns total. */
static size_t encode_packet(uint8_t opcode, const uint8_t *body, size_t body_len,
                            uint8_t *out) {
    uint32_t size = (uint32_t)(1 + body_len); /* opcode + body */
    out[0] = (uint8_t)(size & 0xff);
    out[1] = (uint8_t)((size >> 8) & 0xff);
    out[2] = (uint8_t)((size >> 16) & 0xff);
    out[3] = (uint8_t)((size >> 24) & 0xff);
    out[4] = opcode;
    if (body_len) memcpy(out + 5, body, body_len);
    return 5 + body_len;
}

/* Generic reader parameterised over the transport (fd or SSL). */
typedef int (*read_fn)(void *ctx, uint8_t *buf, size_t n);

static int read_packet(read_fn rd, void *ctx, uint8_t *opcode, uint8_t *body,
                       size_t *body_len, size_t body_cap) {
    uint8_t hdr[4];
    if (rd(ctx, hdr, 4) != 0) return -1;
    uint32_t size = (uint32_t)hdr[0] | ((uint32_t)hdr[1] << 8) |
                    ((uint32_t)hdr[2] << 16) | ((uint32_t)hdr[3] << 24);
    if (size < 1 || size > MAX_PACKET_SIZE) return -1;
    uint8_t op;
    if (rd(ctx, &op, 1) != 0) return -1;
    size_t blen = size - 1;
    if (blen > body_cap) return -1;
    if (blen > 0 && rd(ctx, body, blen) != 0) return -1;
    *opcode = op;
    *body_len = blen;
    return 0;
}

static int rd_fd(void *ctx, uint8_t *buf, size_t n) {
    return read_exact_fd(*(int *)ctx, buf, n);
}
static int rd_ssl(void *ctx, uint8_t *buf, size_t n) {
    return read_exact_ssl((SSL *)ctx, buf, n);
}

static size_t encode_version(int version, uint8_t *out) {
    char body[64];
    int n = snprintf(body, sizeof(body), "{\"version\":%d}", version);
    return encode_packet(OPCODE_VERSION, (const uint8_t *)body, (size_t)n, out);
}

static long parse_version(const uint8_t *body, size_t len) {
    char tmp[256];
    if (len >= sizeof(tmp)) return -1;
    memcpy(tmp, body, len);
    tmp[len] = '\0';
    char *p = strstr(tmp, "\"version\"");
    if (!p) return -1;
    p += strlen("\"version\"");
    while (*p && (*p == ':' || *p == ' ')) p++;
    return strtol(p, NULL, 10);
}

static char *base64(const uint8_t *data, size_t len) {
    /* Standard base64 with padding, no newlines. */
    BIO *b64 = BIO_new(BIO_f_base64());
    BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
    BIO *mem = BIO_new(BIO_s_mem());
    b64 = BIO_push(b64, mem);
    BIO_write(b64, data, (int)len);
    BIO_flush(b64);
    BUF_MEM *bptr;
    BIO_get_mem_ptr(b64, &bptr);
    char *out = malloc(bptr->length + 1);
    memcpy(out, bptr->data, bptr->length);
    out[bptr->length] = '\0';
    BIO_free_all(b64);
    return out;
}

static char *spki_fingerprint(EVP_PKEY *pkey) {
    uint8_t *spki_der = NULL;
    int spki_len = i2d_PUBKEY(pkey, &spki_der);
    if (spki_len <= 0) return NULL;
    uint8_t digest[SHA256_DIGEST_LENGTH];
    SHA256(spki_der, (size_t)spki_len, digest);
    OPENSSL_free(spki_der);
    return base64(digest, sizeof(digest));
}

static int generate_self_signed(EVP_PKEY **out_key, X509 **out_cert) {
    EVP_PKEY *pkey = EVP_EC_gen("P-256");
    if (!pkey) return -1;

    X509 *cert = X509_new();
    if (!cert) {
        EVP_PKEY_free(pkey);
        return -1;
    }
    X509_set_version(cert, 2); /* v3 */
    ASN1_INTEGER_set(X509_get_serialNumber(cert), 1);
    X509_gmtime_adj(X509_getm_notBefore(cert), -3600);
    X509_gmtime_adj(X509_getm_notAfter(cert), 60L * 60 * 24 * 3650);
    X509_set_pubkey(cert, pkey);
    /* Empty subject and issuer (self-signed). */
    if (!X509_sign(cert, pkey, EVP_sha256())) {
        X509_free(cert);
        EVP_PKEY_free(pkey);
        return -1;
    }
    *out_key = pkey;
    *out_cert = cert;
    return 0;
}

static SSL_CTX *make_server_ctx(EVP_PKEY *key, X509 *cert) {
    SSL_CTX *ctx = SSL_CTX_new(TLS_server_method());
    if (!ctx) die_ssl("SSL_CTX_new(server)");
    SSL_CTX_set_min_proto_version(ctx, TLS1_3_VERSION);
    SSL_CTX_set_max_proto_version(ctx, TLS1_3_VERSION);
    /* No client certificate (server-auth only). */
    SSL_CTX_set_verify(ctx, SSL_VERIFY_NONE, NULL);
    if (SSL_CTX_use_certificate(ctx, cert) != 1) die_ssl("use_certificate");
    if (SSL_CTX_use_PrivateKey(ctx, key) != 1) die_ssl("use_PrivateKey");
    return ctx;
}

static SSL_CTX *make_client_ctx(void) {
    SSL_CTX *ctx = SSL_CTX_new(TLS_client_method());
    if (!ctx) die_ssl("SSL_CTX_new(client)");
    SSL_CTX_set_min_proto_version(ctx, TLS1_3_VERSION);
    SSL_CTX_set_max_proto_version(ctx, TLS1_3_VERSION);
    /*
     * We pin by SPKI fingerprint, not PKI, so disable chain/hostname checks.
     * IMPORTANT: SSL_VERIFY_NONE only stops a chain-validation failure from
     * aborting the handshake; OpenSSL still verifies the TLS CertificateVerify
     * signature (proof the peer holds the certificate's private key) as part of
     * the handshake. That is exactly what the spec requires.
     */
    SSL_CTX_set_verify(ctx, SSL_VERIFY_NONE, NULL);
    return ctx;
}

static int exchange_version(int fd, const char *who) {
    uint8_t out[64];
    size_t n = encode_version(PROTOCOL_VERSION, out);
    if (write_all_fd(fd, out, n) != 0) {
        fprintf(stderr, "%s: failed to send Version\n", who);
        return -1;
    }
    uint8_t opcode, body[256];
    size_t body_len;
    int fd_copy = fd;
    if (read_packet(rd_fd, &fd_copy, &opcode, body, &body_len, sizeof(body)) != 0) {
        fprintf(stderr, "%s: failed to read Version packet\n", who);
        return -1;
    }
    if (opcode != OPCODE_VERSION) {
        fprintf(stderr, "%s: expected Version (11), got opcode %d\n", who, opcode);
        return -1;
    }
    long peer = parse_version(body, body_len);
    if (peer != PROTOCOL_VERSION) {
        fprintf(stderr, "%s: peer version %ld unsupported (this PoC only does v4)\n",
                who, peer);
        return -1;
    }
    return 0;
}

static SSL *receiver_upgrade(int fd, SSL_CTX *server_ctx) {
    if (exchange_version(fd, "receiver") != 0) return NULL;
    SSL *ssl = SSL_new(server_ctx);
    if (!ssl) return NULL;
    SSL_set_fd(ssl, fd);
    if (SSL_accept(ssl) != 1) {
        SSL_free(ssl);
        return NULL;
    }
    return ssl;
}

static SSL *sender_upgrade(int fd, SSL_CTX *client_ctx, const char *expected_fp,
                           int *rejected) {
    *rejected = 0;
    if (exchange_version(fd, "sender") != 0) return NULL;
    SSL *ssl = SSL_new(client_ctx);
    if (!ssl) return NULL;
    SSL_set_fd(ssl, fd);
    if (SSL_connect(ssl) != 1) {
        SSL_free(ssl);
        return NULL;
    }
    /* The TLS handshake already proved private-key possession; now pin the key. */
    X509 *peer = SSL_get1_peer_certificate(ssl);
    if (!peer) {
        fprintf(stderr, "sender: receiver presented no certificate\n");
        SSL_free(ssl);
        return NULL;
    }
    EVP_PKEY *pubkey = X509_get_pubkey(peer);
    char *got = spki_fingerprint(pubkey);
    EVP_PKEY_free(pubkey);
    X509_free(peer);
    if (!got) {
        SSL_free(ssl);
        return NULL;
    }
    if (strcmp(got, expected_fp) != 0) {
        fprintf(stderr, "sender: fingerprint mismatch: got %s, expected %s\n",
                got, expected_fp);
        free(got);
        *rejected = 1;
        SSL_shutdown(ssl);
        SSL_free(ssl);
        return NULL;
    }
    free(got);
    return ssl;
}

static int sender_ping(SSL *ssl) {
    uint8_t out[8];
    size_t n = encode_packet(OPCODE_PING, NULL, 0, out);
    if (write_all_ssl(ssl, out, n) != 0) return -1;
    uint8_t opcode, body[8];
    size_t body_len;
    if (read_packet(rd_ssl, ssl, &opcode, body, &body_len, sizeof(body)) != 0)
        return -1;
    if (opcode != OPCODE_PONG) {
        fprintf(stderr, "expected Pong (13) after Ping, got opcode %d\n", opcode);
        return -1;
    }
    return 0;
}

static void sender_probe(SSL *ssl) {
    uint8_t out[8];
    size_t n = encode_packet(OPCODE_PING, NULL, 0, out);
    if (write_all_ssl(ssl, out, n) != 0) {
        printf("  (failed to send Ping)\n");
        return;
    }
    uint8_t opcode, body[MAX_PACKET_SIZE];
    size_t body_len;
    if (read_packet(rd_ssl, ssl, &opcode, body, &body_len, sizeof(body)) != 0) {
        printf("  (no post-upgrade packet)\n");
        return;
    }
    if (opcode == OPCODE_PONG) printf("  post-upgrade Ping/Pong ok\n");
    else printf("  post-upgrade packet received inside TLS (opcode %d)\n", opcode);
}

/* Respond to Pings with Pongs until the peer disconnects. */
static void receiver_serve(SSL *ssl) {
    for (;;) {
        uint8_t opcode, body[8];
        size_t body_len;
        if (read_packet(rd_ssl, ssl, &opcode, body, &body_len, sizeof(body)) != 0)
            return; /* peer closed */
        if (opcode == OPCODE_PING) {
            uint8_t out[8];
            size_t n = encode_packet(OPCODE_PONG, NULL, 0, out);
            if (write_all_ssl(ssl, out, n) != 0) return;
        }
    }
}

static int tcp_listen(const char *host, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) {
        close(fd);
        return -1;
    }
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0 || listen(fd, 8) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int tcp_connect(const char *host, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) {
        close(fd);
        return -1;
    }
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int run_receiver(const char *host, int port) {
    EVP_PKEY *key;
    X509 *cert;
    if (generate_self_signed(&key, &cert) != 0) die_ssl("generate cert");
    char *fp = spki_fingerprint(key);
    SSL_CTX *ctx = make_server_ctx(key, cert);

    int lfd = tcp_listen(host, port);
    if (lfd < 0) {
        fprintf(stderr, "failed to listen on %s:%d\n", host, port);
        return 1;
    }
    printf("receiver listening on %s:%d\n", host, port);
    printf("  fp (mDNS TXT record): %s\n", fp);
    printf("  run a sender with:  --host %s --port %d --fp %s\n", host, port, fp);
    fflush(stdout);

    for (;;) {
        struct sockaddr_in peer;
        socklen_t plen = sizeof(peer);
        int cfd = accept(lfd, (struct sockaddr *)&peer, &plen);
        if (cfd < 0) continue;
        char ip[INET_ADDRSTRLEN];
        inet_ntop(AF_INET, &peer.sin_addr, ip, sizeof(ip));
        printf("connection from %s:%d\n", ip, ntohs(peer.sin_port));
        fflush(stdout);
        SSL *ssl = receiver_upgrade(cfd, ctx);
        if (ssl) {
            printf("  TLS upgrade ok: %s %s\n", SSL_get_version(ssl),
                   SSL_get_cipher(ssl));
            fflush(stdout);
            receiver_serve(ssl);
            SSL_free(ssl);
            printf("  session ended\n");
        } else {
            printf("  session failed\n");
        }
        fflush(stdout);
        close(cfd);
    }
}

static int run_sender(const char *host, int port, const char *expected_fp) {
    SSL_CTX *ctx = make_client_ctx();
    int fd = tcp_connect(host, port);
    if (fd < 0) {
        fprintf(stderr, "failed to connect to %s:%d\n", host, port);
        return 1;
    }
    int rejected = 0;
    SSL *ssl = sender_upgrade(fd, ctx, expected_fp, &rejected);
    if (!ssl) {
        close(fd);
        return rejected ? 2 : 1;
    }
    printf("TLS upgrade ok: %s %s\n", SSL_get_version(ssl), SSL_get_cipher(ssl));
    printf("fingerprint verified: %s\n", expected_fp);
    fflush(stdout);
    sender_probe(ssl);
    SSL_shutdown(ssl);
    SSL_free(ssl);
    close(fd);
    SSL_CTX_free(ctx);
    return 0;
}

struct selftest_server {
    int lfd;
    SSL_CTX *ctx;
};

static void *selftest_accept_loop(void *arg) {
    struct selftest_server *s = arg;
    for (;;) {
        int cfd = accept(s->lfd, NULL, NULL);
        if (cfd < 0) return NULL;
        SSL *ssl = receiver_upgrade(cfd, s->ctx);
        if (ssl) {
            receiver_serve(ssl);
            SSL_free(ssl);
        }
        close(cfd);
    }
}

static int run_selftest(void) {
    int failures = 0;

    /* 0. Framing must be byte-identical to the Rust reference wire format:
     *    Size(LE u32) = opcode(1) + body ; opcode 11 ; body = compact JSON. */
    static const uint8_t expected_wire[] = {0x0e, 0x00, 0x00, 0x00, 0x0b,
                                            '{', '"', 'v', 'e', 'r', 's', 'i',
                                            'o', 'n', '"', ':', '4', '}'};
    uint8_t wire[64];
    size_t wire_len = encode_version(PROTOCOL_VERSION, wire);
    if (wire_len == sizeof(expected_wire) &&
        memcmp(wire, expected_wire, wire_len) == 0) {
        printf("PASS: Version packet framing matches the reference wire format\n");
    } else {
        failures++;
        printf("FAIL: framing mismatch\n");
    }

    EVP_PKEY *key;
    X509 *cert;
    if (generate_self_signed(&key, &cert) != 0) die_ssl("generate cert");
    char *fp = spki_fingerprint(key);
    SSL_CTX *server_ctx = make_server_ctx(key, cert);

    int lfd = tcp_listen("127.0.0.1", 0);
    if (lfd < 0) {
        fprintf(stderr, "selftest: failed to listen\n");
        return 1;
    }
    struct sockaddr_in bound;
    socklen_t blen = sizeof(bound);
    getsockname(lfd, (struct sockaddr *)&bound, &blen);
    int port = ntohs(bound.sin_port);
    printf("selftest: receiver fp = %s\n", fp);
    printf("selftest: listening on 127.0.0.1:%d\n", port);

    struct selftest_server srv = {lfd, server_ctx};
    pthread_t thread;
    pthread_create(&thread, NULL, selftest_accept_loop, &srv);

    SSL_CTX *client_ctx = make_client_ctx();

    /* 1. Happy path: correct fingerprint must succeed + Ping/Pong over TLS. */
    {
        int fd = tcp_connect("127.0.0.1", port);
        int rejected = 0;
        SSL *ssl = fd >= 0 ? sender_upgrade(fd, client_ctx, fp, &rejected) : NULL;
        if (ssl && sender_ping(ssl) == 0) {
            printf("PASS: handshake + Ping/Pong with correct fingerprint\n");
        } else {
            failures++;
            printf("FAIL: handshake with correct fingerprint\n");
        }
        if (ssl) {
            SSL_shutdown(ssl);
            SSL_free(ssl);
        }
        if (fd >= 0) close(fd);
    }

    /* 2. Negative: a wrong fingerprint must be rejected. */
    {
        const char *wrong_fp = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        int fd = tcp_connect("127.0.0.1", port);
        int rejected = 0;
        SSL *ssl = fd >= 0 ? sender_upgrade(fd, client_ctx, wrong_fp, &rejected) : NULL;
        if (ssl == NULL && rejected) {
            printf("PASS: handshake with wrong fingerprint rejected\n");
        } else {
            failures++;
            printf("FAIL: handshake with wrong fingerprint was NOT rejected\n");
        }
        if (ssl) {
            SSL_free(ssl);
        }
        if (fd >= 0) close(fd);
    }

    pthread_cancel(thread);
    close(lfd);
    printf("selftest: %s\n", failures == 0 ? "OK" : "FAILURE(S)");

    free(fp);
    SSL_CTX_free(client_ctx);
    SSL_CTX_free(server_ctx);
    X509_free(cert);
    EVP_PKEY_free(key);
    return failures == 0 ? 0 : 1;
}

static const char *get_opt(int argc, char **argv, const char *name) {
    for (int i = 0; i + 1 < argc; i += 2) {
        if (strcmp(argv[i], name) == 0) return argv[i + 1];
    }
    return NULL;
}

int main(int argc, char **argv) {
    /* A peer that closes mid-write would otherwise terminate us with SIGPIPE;
     * we handle write errors via return values instead. */
    signal(SIGPIPE, SIG_IGN);

    if (argc < 2) {
        fprintf(stderr, "usage: %s <receiver|sender|selftest> [options]\n", argv[0]);
        return 1;
    }
    const char *mode = argv[1];
    int rest_argc = argc - 2;
    char **rest_argv = argv + 2;

    if (strcmp(mode, "receiver") == 0) {
        const char *host = get_opt(rest_argc, rest_argv, "--host");
        const char *port = get_opt(rest_argc, rest_argv, "--port");
        return run_receiver(host ? host : "0.0.0.0", port ? atoi(port) : DEFAULT_PORT);
    }
    if (strcmp(mode, "sender") == 0) {
        const char *host = get_opt(rest_argc, rest_argv, "--host");
        const char *port = get_opt(rest_argc, rest_argv, "--port");
        const char *fp = get_opt(rest_argc, rest_argv, "--fp");
        if (!host || !fp) {
            fprintf(stderr, "usage: %s sender --host <ip> --port <port> --fp <base64-fp>\n",
                    argv[0]);
            return 1;
        }
        return run_sender(host, port ? atoi(port) : DEFAULT_PORT, fp);
    }
    if (strcmp(mode, "selftest") == 0) {
        return run_selftest();
    }
    fprintf(stderr, "unknown mode: %s\n", mode);
    return 1;
}
