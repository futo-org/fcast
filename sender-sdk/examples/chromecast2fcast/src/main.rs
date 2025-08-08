use std::{net::{IpAddr, Ipv4Addr}, sync::Arc};

use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::net::TcpListener;
use tokio_rustls::rustls;

#[tokio::main]
async fn main() {
    let daemon = mdns_sd::ServiceDaemon::new().unwrap();
    let service = mdns_sd::ServiceInfo::new(
        "_googlecast._tcp.local.",
        "Not-A-Chromecast",
        &"Not-A-Chromecast.local.",
        [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 133)),
        ]
        .as_slice(),
        3003,
        None,
    )
    .unwrap();
    daemon.register(service).unwrap();

    let certs = CertificateDer::pem_file_iter("certs/cert.pem")
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file("certs/cert.key.pem").unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("0.0.0.0:3003").await.unwrap();
    while let Ok((stream, addr)) = listener.accept().await {
        println!("New connection: {stream:?} addr={addr:?}");
        let acceptor = acceptor.clone();
        let fut = async move {
            let _stream = acceptor.accept(stream).await.unwrap();
        };

        tokio::spawn(fut);
    }
}
