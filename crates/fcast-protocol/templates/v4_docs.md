# Version 4

## Overview

The protocol is a TCP protocol on port `46899`.

The packet is defined as follows, using the style from
[this document](https://www.ietf.org/archive/id/draft-mcquistin-augmented-ascii-diagrams-13.html):

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           Size (LE)                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    Opcode     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               :
:                             Body                              :
:                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

Where:

  * Size: 32 bits.
  * Opcode: 8 bits.
  * Size is the number of bytes following the Size field, i.e. opcode + body.
  * The size of the body is `Size - sizeof(Opcode)`.
  * The maximum packet (opcode + body) size is 512 KiB.

If a party receives a packet that is missing the opcode (Size = 0) or is larger than the maximum
size, it must disconnect immediately.

The following table describes the opcodes:

| Opcode | Name           | Direction | Description          |
|--------|----------------|-----------|----------------------|
| 11     | Version        | Both      | [↗](#version)        |
| 12     | Ping           | Both      | [↗](#heartbeat)      |
| 13     | Pong           | Both      | [↗](#heartbeat)      |
| 20     | Flatbuf        | Both      | [↗](#flatbuf)        |
| 21     | Resource       | S->R      | [↗](#resource)       |

### Packet sequence number

Every packet on a connection has an implicit, zero-based sequence number that is used to refer to a
specific packet, for example the `packet_num` field of an [`Error`](#flatbuf) message. Each party
numbers the packets it sends independently, starting from zero. The `packet_num` in an `Error`
therefore refers to the offending packet using the numbering of the party that sent it. Only
individual FCast packets are counted and this includes packets sent in plaintext.

### Receiver discovery

Receivers advertise themselves over [mDNS]/[DNS-SD] under the service name `_fcast._tcp`. The
following TXT records are included:

| Name | Example                                      | Value description                       |
|------|----------------------------------------------|-----------------------------------------|
| v    | 4                                            | Highest supported protocol version      |
| fp   | QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc= | Fingerprint (see [Security](#security)) |

### Connection URL

A receiver's connection details can also be encoded into a single URL. This is what is encoded in
the QR code mentioned in the [Threat model](#threat-model) section: it lets a sender connect without
relying on mDNS discovery, and carries the `fp` fingerprint over a channel a network attacker cannot
tamper with.

The URL has the form:

`fcast://r/<connection-info>`

Where `<connection-info>` is the [base64url] encoding of the UTF-8 JSON document described
below. Padding (`=`) is optional and decoders must accept the value whether or not it is present. A
URL that does not begin with the exact `fcast://r/` prefix, whose payload is not valid base64url, or
whose decoded bytes are not the JSON document below, is invalid and must be rejected.

The JSON document has the following fields:

| Field       | Type                            | Description                                                       |
|-------------|---------------------------------|-------------------------------------------------------------------|
| `name`      | `string`                        | Human-readable name of the receiver                               |
| `addresses` | `array` of `string`             | IP addresses (IPv4 or IPv6) the receiver is reachable on          |
| `services`  | `array` of [Service](#service)  | The services the receiver exposes                                 |
| `txt`       | `map<string, string>`, optional | The receiver's [mDNS TXT records](#receiver-discovery), e.g. `fp` |

#### Service

| Field  | Type    | Description                                |
|--------|---------|--------------------------------------------|
| `port` | integer | TCP port number (0–65535)                  |
| `type` | integer | Service type. `0` is the FCast TCP service |

Example document (before base64url encoding):

```json
{
  "name": "Living Room",
  "addresses": ["192.168.1.42", "fe80::1ff:fe23:4567:890a"],
  "services": [{ "port": 46899, "type": 0 }],
  "txt": { "v": "4", "fp": "QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc=" }
}
```

### Connection establishment

When a sender or receiver establishes a connection with the other party, it must send a `Version`
message to indicate which messages and protocol features are supported.

When there is a mismatch of supported protocol versions among devices, the device with the higher
version number must either error out/disconnect or use a downgraded feature set compatible with the
other party's protocol version.

When both parties support version 4, they must proceed to upgrade the connection (see
[Security](#security)).

When the connection is secured, each party must send their introduction message
(`SenderIntroduction` for senders, `ReceiverIntroduction` for receivers).

### Device state synchronization

The protocol allows for multiple senders to connect to a single receiver. To synchronize the
play/control state between all sender devices, the receiver relays most messages that mutates the
state of the receiver. For example, S1 (Sender one) sends `VolumeChanged(50%)` to R (Receiver), once
R has successfully changed the volume it will send that same message to S1 and any other senders
connected. The same applies to `Load`, `PlaybackStateChanged`, `SpeedChanged`, `QueueInsert`,
`QueueRemove`, `QueueItemSelected` and `ChangeTrack`.

### Screen mirroring

A sender can mirror its screen to the receiver over a WebRTC connection that is negotiated through
the FCast control connection. The sender produces the media and is therefore the WebRTC offerer, and
the receiver is the answerer.

The negotiation uses two messages, both sent as [`Flatbuf`](#flatbuf) packets:

1. The sender allocates a `session_id` and sends `StartMirroringSession` to announce the new
   session. The receiver records this `session_id` as its active mirroring session.
1. The sender gathers its ICE candidates, then sends its SDP offer in a `MirroringSessionDescription`
   carrying the same `session_id`.
1. The receiver replies with its own `MirroringSessionDescription` carrying the SDP answer and the
   same `session_id`. A `MirroringSessionDescription` whose `session_id` does not match the active
   session is invalid.

Once the offer/answer exchange completes the WebRTC media flows directly between the two peers.

ICE is non-trickle: each side gathers its candidates fully and embeds them in the SDP before sending
it. The reference implementation configures no STUN or TURN servers and relies on host candidates
only, so mirroring is intended for use on the local network.

## Security

Version 4 requires an encrypted, server-authenticated connection. Once the plaintext `Version`
handshake (see [Connection establishment](#connection-establishment)) has established that both
parties support version 4, the existing TCP connection is upgraded to [TLS 1.3] in place.

### Connection upgrade

The upgrade reuses the same TCP connection and it is not signalled by any dedicated FCast message:

  1. Immediately after the TCP connection is established, both parties send a `Version` message
     (opcode 11) in plaintext.
  1. Once both parties have indicated version 4 support, the stream is switched to TLS. The next
     bytes the connecting party writes are a TLS 1.3 `ClientHello`. The handshake and record layer
     follow [TLS 1.3] unchanged.
  1. The sender, which initiated the TCP connection, is the TLS client, and the receiver is the
     TLS server.
  1. Only TLS 1.3 may be negotiated.
  1. Only the server is authenticated (see below).

After the handshake completes, all subsequent FCast packets are carried inside the TLS session.

### Certificate pinning

The receiver presents a self-signed certificate and it is pinned by its public key. It advertises an
`fp` mDNS TXT record whose value is the standard (padded) base64 encoding of the SHA-256 digest of
the certificate's DER-encoded `SubjectPublicKeyInfo` (SPKI).

When validating the receiver certificate, the sender must compute the SHA-256 digest of the presented
certificate's SPKI and require that it equals the value decoded from `fp`. If they do not match, the
connection should be aborted. The sender must additionally verify the TLS `CertificateVerify`
signature as mandated by [TLS 1.3]. This proves that the peer actually holds the private key for the
pinned public key.

### Threat model

The `fp` fingerprint is the trust anchor for the connection, so a session is only as trustworthy as
the sender's knowledge of the receiver's fingerprint.

When `fp` is learned from the mDNS TXT record it protects against a passive eavesdropper, but not
against an attacker that is able to spoof discovery answers. A QR code displayed by the receiver
removes this weakness because that channel cannot be tampered with by a network attacker. The code
encodes the receiver's connection details together with its `fp` fingerprint and is read directly
off the receiver's screen. The sender also knows in advance that the receiver supports version 4, so
it should refuse to fall back to an unencrypted earlier version. This matters because the `Version`
handshake preceding the upgrade is exchanged in plaintext and is vulnerable to getting downgraded by
an attacker.

## Messages

### Version

JSON encoded body.

{{ version_message }}

### Flatbuf

The majority of the messages are defined in this FlatBuffer schema. Implementers can easily generate
code for many programming languages with the FlatBuffers compiler (see: [the quick start
guide](https://flatbuffers.dev/quick_start/)).

The body of a `Flatbuf` packet is a serialized `Packet` table (the `root_type` below).

```fbs
{{ flatbuffer_source }}
```

### Heartbeat

The heartbeat lets a party detect a dead connection. Either side may probe an idle connection with a
`Ping`, to which the other side must reply with a `Pong`.

Receiving any packet counts as activity and resets the idle timer. The `Pong` message is what keeps
an otherwise-silent connection alive. The reference implementation applies the following policy per
side:

  * If no packet has been received for 3 seconds, send a `Ping`.
  * If a `Ping` has already been sent and still no packet has been received after another 3 seconds
    (6 s of inactivity total), consider the connection dead and end the session.

#### Ping

Sent to probe an idle connection. No body. The peer must respond with a `Pong`.

#### Pong

Sent in response to a `Ping`. No body.

## FCompanion

This section defines the FCompanion protocol used to transfer media data over an FCast
connection. The bodies are in a custom binary format in which all multi-byte integers are encoded
as little-endian.

URLs are defined like this:

`fcomp://<provider-id>.fcast/<resource-id>`

 - `provider-id` is a `U16` and `resource-id` is a `U32`, both rendered as ASCII decimal digits.

A sender that wants to provide resources first sends a `CompanionHelloRequest`. The receiver replies
with a `CompanionHelloResponse` containing the `provider_id` it has assigned to that sender
connection. The sender then constructs `fcomp://` URLs using that `provider_id` and references them
from a `MediaItem`'s `source_url`. To fetch the data the receiver routes a
`CompanionResourceInfoRequest`/`CompanionResourceRequest` to the connection that owns the
`provider_id` and that connection answers with `CompanionResourceInfoResponse` and
[`Resource`](#resource) packets respectively.

The receiver implementation must support the case where a sender plays a companion URL provided
by a different connection to the same receiver. This is to allow more flexibility for sender
developers.

### Resource

The `Resource` opcode carries the bytes of a resource from the providing sender to the receiver.
It is sent in response to a `CompanionResourceRequest`, which is a [`Flatbuf`](#flatbuf) message.

#### Response

Responses can and must be sent as multiple parts if the requested read length is bigger than the
maximum packet size. `Part #` must always start at 0. Because `Total Parts` is a `U8`, the requester
must keep each `ResourceReadHead` range small enough to be delivered in at most 255 parts.

| Arg. # | Type                | Description |
|--------|---------------------|-------------|
| 1      | U32LE               | Request ID  |
| 2      | U8                  | Part #      |
| 3      | U8                  | Total Parts |
| 4      | [GetResourceResult] | Result      |

### Shared types

#### GetResourceResult

A single byte (`U8`) called `variant` with optional extra data.

The values of `variant` are:

| Value | Extra Data | Description                |
|-------|------------|----------------------------|
| 0x00  | NONE       | The resource was not found |
| 0x01  | \[U8\]     | Success                    |

The length of the success array is calculated by subtracting the size of the fixed fields (Request
ID + Part # + Total Parts + variant byte) from the message body length.

[mDNS]: https://www.rfc-editor.org/info/rfc6762/
[DNS-SD]: https://www.rfc-editor.org/info/rfc6763/
[base64url]: https://www.rfc-editor.org/rfc/rfc4648#section-5
[GetResourceResult]: #getresourceresult
[TLS 1.3]: https://www.ietf.org/archive/id/draft-ietf-tls-rfc8446bis-13.html
