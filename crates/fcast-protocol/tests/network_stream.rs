#![cfg(all(feature = "tokio-sender", feature = "tokio-receiver"))]

use std::{sync::Arc, time::Duration};

use fcast_protocol::{
    receiver::NetworkStream as ReceiverStream,
    sender::{CertVerifier, NetworkStream as SenderStream},
    v4, Opcode, PacketReader, PlaybackErrorMessage, ReadResult, SetVolumeMessage, VersionMessage,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{rustls, TlsAcceptor, TlsConnector};

fn encode_packet(opcode: Opcode, body: &[u8]) -> Vec<u8> {
    let size = (body.len() + 1) as u32;
    let mut packet = Vec::with_capacity(fcast_protocol::HEADER_LENGTH + body.len());
    packet.extend_from_slice(&size.to_le_bytes());
    packet.push(opcode as u8);
    packet.extend_from_slice(body);
    packet
}

macro_rules! read_packet {
    ($stream:expr, $reader:expr, $buf:expr) => {{
        loop {
            let ready = match $reader.get_packet() {
                ReadResult::Read(packet) => Some(packet.to_vec()),
                ReadResult::PacketTooLarge(size) => panic!("packet too large: {size}"),
                ReadResult::NeedData => None,
            };

            match ready {
                Some(packet) => break packet,
                None => {
                    let n = $stream.read(&mut $buf).await.expect("read failed");
                    assert_ne!(n, 0, "stream closed before a full packet arrived");
                    $reader.push_data(&$buf[..n]).expect("push_data failed");
                }
            }
        }
    }};
}

/// Like [`read_packet!`] but reads straight into the reader's spare capacity
/// (`spare_capacity_mut` + `commit`) — the zero-copy receive path the receiver uses. No
/// scratch buffer, so there is no `$buf` argument.
macro_rules! read_packet_zerocopy {
    ($stream:expr, $reader:expr) => {{
        loop {
            let ready = match $reader.get_packet() {
                ReadResult::Read(packet) => Some(packet.to_vec()),
                ReadResult::PacketTooLarge(size) => panic!("packet too large: {size}"),
                ReadResult::NeedData => None,
            };

            match ready {
                Some(packet) => break packet,
                None => {
                    let spare = $reader.spare_capacity_mut();
                    assert!(
                        !spare.is_empty(),
                        "spare capacity empty — would read as EOF"
                    );
                    let n = $stream.read(spare).await.expect("read failed");
                    assert_ne!(n, 0, "stream closed before a full packet arrived");
                    $reader.commit(n);
                }
            }
        }
    }};
}

/// Full packet bytes (opcode + payload) as `get_packet` returns them.
fn packet_bytes(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![opcode as u8];
    v.extend_from_slice(payload);
    v
}

fn new_reader() -> PacketReader {
    PacketReader::new(v4::MAX_PACKET_SIZE, 8 * 1024)
}

fn server_tls() -> (TlsAcceptor, Vec<u8>) {
    use rcgen::{date_time_ymd, CertificateParams, DistinguishedName, KeyPair, PublicKeyData};
    use sha2::Digest;

    let mut params: CertificateParams = Default::default();
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    params.distinguished_name = DistinguishedName::new();
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    let fingerprint = sha2::Sha256::digest(key_pair.subject_public_key_info()).to_vec();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().to_owned()], key_pair.into())
        .unwrap();

    (TlsAcceptor::from(Arc::new(config)), fingerprint)
}

fn client_tls(fingerprint: Vec<u8>) -> TlsConnector {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CertVerifier::new(fingerprint, provider)))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

#[tokio::test]
async fn tcp_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let receiver = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut stream = ReceiverStream::new(sock);
        let mut buf = [0u8; 8 * 1024];
        let mut reader = new_reader();

        let packet = read_packet!(stream, reader, buf);
        assert_eq!(packet[0], Opcode::Version as u8);
        let version: VersionMessage = serde_json::from_slice(&packet[1..]).unwrap();
        assert_eq!(version.version, 4);

        stream
            .write_all(&encode_packet(Opcode::Pong, &[]))
            .await
            .unwrap();
        let err = serde_json::to_vec(&PlaybackErrorMessage {
            message: "some error".to_owned(),
        })
        .unwrap();
        stream
            .write_all(&encode_packet(Opcode::PlaybackError, &err))
            .await
            .unwrap();
        stream.flush().await.unwrap();
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let mut stream = SenderStream::new(sock).unwrap();
    let mut buf = [0u8; 8 * 1024];
    let mut reader = new_reader();

    let body = serde_json::to_vec(&VersionMessage { version: 4 }).unwrap();
    stream
        .write_all(&encode_packet(Opcode::Version, &body))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let pong = read_packet!(stream, reader, buf);
    assert_eq!(pong.as_slice(), &[Opcode::Pong as u8]);

    let err = read_packet!(stream, reader, buf);
    assert_eq!(err[0], Opcode::PlaybackError as u8);
    let msg: PlaybackErrorMessage = serde_json::from_slice(&err[1..]).unwrap();
    assert_eq!(msg.message, "some error");

    receiver.await.unwrap();
}

#[tokio::test]
async fn tcp_batched_packets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let receiver = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut stream = ReceiverStream::new(sock);
        let mut buf = [0u8; 8 * 1024];
        let mut reader = new_reader();

        for expected in 0u8..5 {
            let packet = read_packet!(stream, reader, buf);
            assert_eq!(packet[0], Opcode::SetVolume as u8);
            let msg: SetVolumeMessage = serde_json::from_slice(&packet[1..]).unwrap();
            assert_eq!(msg.volume, expected as f64 / 10.0);
        }
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let mut stream = SenderStream::new(sock).unwrap();

    let mut batch = Vec::new();
    for i in 0u8..5 {
        let body = serde_json::to_vec(&SetVolumeMessage {
            volume: i as f64 / 10.0,
        })
        .unwrap();
        batch.extend_from_slice(&encode_packet(Opcode::SetVolume, &body));
    }
    stream.write_all(&batch).await.unwrap();
    stream.flush().await.unwrap();

    receiver.await.unwrap();
}

#[tokio::test]
async fn tls_upgrade_with_over_read_prefix() {
    let (acceptor, fingerprint) = server_tls();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let receiver = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // Simulate over-reading: pull the first chunk of the ClientHello off
        // the raw socket before the TLS upgrade, then replay it as the prefix.
        let mut prefix = [0u8; 8];
        let n = sock.read(&mut prefix).await.unwrap();
        assert_ne!(n, 0, "expected ClientHello bytes");

        let mut stream = ReceiverStream::new(sock);
        stream
            .upgrade_with_prefix(&acceptor, &prefix[..n], Duration::from_secs(5))
            .await
            .unwrap();

        let mut buf = [0u8; 8 * 1024];
        let mut reader = new_reader();
        let packet = read_packet!(stream, reader, buf);
        assert_eq!(packet.as_slice(), &[Opcode::Ping as u8]);
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let connector = client_tls(fingerprint);
    let server_name = rustls_pki_types::ServerName::from(addr.ip());
    let mut tls = connector.connect(server_name, sock).await.unwrap();
    tls.write_all(&encode_packet(Opcode::Ping, &[]))
        .await
        .unwrap();
    tls.flush().await.unwrap();

    receiver.await.unwrap();
}

#[tokio::test]
async fn tls_upgrade_roundtrip() {
    let (acceptor, fingerprint) = server_tls();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let receiver = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut stream = ReceiverStream::new(sock);
        let mut buf = [0u8; 8 * 1024];
        let mut reader = new_reader();

        let packet = read_packet!(stream, reader, buf);
        assert_eq!(packet[0], Opcode::Version as u8);

        let body = serde_json::to_vec(&VersionMessage { version: 4 }).unwrap();
        stream
            .write_all(&encode_packet(Opcode::Version, &body))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        stream
            .upgrade(&acceptor, Duration::from_secs(5))
            .await
            .unwrap();

        let mut reader = new_reader();
        let packet = read_packet!(stream, reader, buf);
        assert_eq!(packet.as_slice(), &[Opcode::Ping as u8]);

        stream
            .write_all(&encode_packet(Opcode::Pong, &[]))
            .await
            .unwrap();
        stream.flush().await.unwrap();
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let mut stream = SenderStream::new(sock).unwrap();
    let mut buf = [0u8; 8 * 1024];
    let mut reader = new_reader();

    let body = serde_json::to_vec(&VersionMessage { version: 4 }).unwrap();
    stream
        .write_all(&encode_packet(Opcode::Version, &body))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let packet = read_packet!(stream, reader, buf);
    assert_eq!(packet[0], Opcode::Version as u8);

    let connector = client_tls(fingerprint);
    let server_name = rustls_pki_types::ServerName::from(addr.ip());
    stream
        .upgrade(&connector, server_name, Duration::from_secs(5))
        .await
        .unwrap();

    let mut reader = new_reader();
    stream
        .write_all(&encode_packet(Opcode::Ping, &[]))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let pong = read_packet!(stream, reader, buf);
    assert_eq!(pong.as_slice(), &[Opcode::Pong as u8]);

    receiver.await.unwrap();
}

#[tokio::test]
async fn tcp_zerocopy_varied_sizes() {
    // Exercises the zero-copy receive path over a real socket: several packets of very
    // different sizes — including one far larger than any single TCP segment — written back
    // to back so they split and coalesce across reads arbitrarily. All must reassemble in
    // order and byte-for-byte.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let items: Vec<(Opcode, Vec<u8>)> = vec![
        (Opcode::Ping, vec![]),
        (
            Opcode::SetVolume,
            serde_json::to_vec(&SetVolumeMessage { volume: 0.5 }).unwrap(),
        ),
        // ~40 KiB body: forces assembly across many reads / TCP segments.
        (
            Opcode::PlaybackError,
            (0..40_000u32).map(|i| (i % 253) as u8).collect(),
        ),
        (Opcode::Pong, vec![]),
    ];
    let expected: Vec<Vec<u8>> = items
        .iter()
        .map(|(op, payload)| packet_bytes(*op, payload))
        .collect();

    let n_packets = expected.len();
    let receiver = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut stream = ReceiverStream::new(sock);
        let mut reader = new_reader();
        let mut got = Vec::new();
        for _ in 0..n_packets {
            got.push(read_packet_zerocopy!(stream, reader));
        }
        got
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let mut stream = SenderStream::new(sock).unwrap();
    let mut batch = Vec::new();
    for (op, payload) in &items {
        batch.extend_from_slice(&encode_packet(*op, payload));
    }
    stream.write_all(&batch).await.unwrap();
    stream.flush().await.unwrap();

    let got = receiver.await.unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn tls_zerocopy_large_packet_after_upgrade() {
    // The receiver's real transport is TLS. Drive the zero-copy path over an upgraded TLS
    // stream with a large packet followed by a tiny one (the reader must keep working after
    // a big read), mirroring `tls_upgrade_roundtrip`'s handshake sequence.
    let (acceptor, fingerprint) = server_tls();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let big: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let big_expected = packet_bytes(Opcode::PlaybackError, &big);

    let receiver = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut stream = ReceiverStream::new(sock);
        let mut buf = [0u8; 8 * 1024];
        let mut reader = new_reader();

        // Plaintext Version exchange, then upgrade — same as tls_upgrade_roundtrip.
        let packet = read_packet!(stream, reader, buf);
        assert_eq!(packet[0], Opcode::Version as u8);
        let body = serde_json::to_vec(&VersionMessage { version: 4 }).unwrap();
        stream
            .write_all(&encode_packet(Opcode::Version, &body))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        stream
            .upgrade(&acceptor, Duration::from_secs(5))
            .await
            .unwrap();

        // Post-upgrade reads via the zero-copy path.
        let mut reader = new_reader();
        let big_packet = read_packet_zerocopy!(stream, reader);
        let ping = read_packet_zerocopy!(stream, reader);
        (big_packet, ping)
    });

    let sock = TcpStream::connect(addr).await.unwrap();
    let mut stream = SenderStream::new(sock).unwrap();
    let mut buf = [0u8; 8 * 1024];
    let mut reader = new_reader();

    let body = serde_json::to_vec(&VersionMessage { version: 4 }).unwrap();
    stream
        .write_all(&encode_packet(Opcode::Version, &body))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let packet = read_packet!(stream, reader, buf);
    assert_eq!(packet[0], Opcode::Version as u8);

    let connector = client_tls(fingerprint);
    let server_name = rustls_pki_types::ServerName::from(addr.ip());
    stream
        .upgrade(&connector, server_name, Duration::from_secs(5))
        .await
        .unwrap();

    stream
        .write_all(&encode_packet(Opcode::PlaybackError, &big))
        .await
        .unwrap();
    stream
        .write_all(&encode_packet(Opcode::Ping, &[]))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let (big_packet, ping) = receiver.await.unwrap();
    assert_eq!(big_packet, big_expected);
    assert_eq!(ping.as_slice(), &[Opcode::Ping as u8]);
}
