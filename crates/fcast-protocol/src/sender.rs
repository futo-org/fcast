use rustls_pki_types::ServerName;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt, BufWriter, ReadHalf, WriteHalf},
    net::TcpStream,
};
use tokio_rustls::{client::TlsStream, rustls, TlsConnector};
use tracing::error;
use x509_parser::prelude::FromDer;

#[derive(Default)]
pub enum NetworkStream {
    #[default]
    None,
    Tcp {
        peer_addr: SocketAddr,
        rx: ReadHalf<TcpStream>,
        tx: BufWriter<WriteHalf<TcpStream>>,
    },
    Tls {
        tx: BufWriter<WriteHalf<TlsStream<TcpStream>>>,
        rx: ReadHalf<TlsStream<TcpStream>>,
    },
}

impl NetworkStream {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        if let Err(err) = stream.set_nodelay(true) {
            error!("Failed to enable TCP_NODELAY on stream: {err:?}");
        }

        let peer_addr = stream.peer_addr()?;

        let (rx, tx) = io::split(stream);
        let tx = BufWriter::new(tx);

        Ok(Self::Tcp { peer_addr, rx, tx })
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp { rx, .. } => rx.read(buf).await,
            Self::Tls { rx, .. } => rx.read(buf).await,
            Self::None => unreachable!(),
        }
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { tx, .. } => tx.write_all(buf).await,
            Self::Tls { tx, .. } => tx.write_all(buf).await,
            Self::None => unreachable!(),
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp { tx, .. } => tx.flush().await?,
            Self::Tls { tx, .. } => tx.flush().await?,
            _ => (),
        }

        Ok(())
    }

    pub async fn upgrade(
        &mut self,
        connector: &TlsConnector,
        server_name: ServerName<'static>,
        timeout: Duration,
    ) -> io::Result<()> {
        let old = std::mem::take(self);
        *self = match old {
            Self::Tcp { tx, rx, .. } => {
                let tx = tx.into_inner();
                let stream = rx.unsplit(tx);

                let tls_stream =
                    tokio::time::timeout(timeout, connector.connect(server_name, stream))
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "TLS upgrade timed out")
                        })??;
                let (rx, tx) = io::split(tls_stream);
                let tx = BufWriter::with_capacity(1024 * 8, tx);
                Self::Tls { tx, rx }
            }
            _ => old,
        };

        Ok(())
    }
}

#[derive(Debug)]
pub struct CertVerifier {
    fingerprint: Vec<u8>,
    crypto_provider: Arc<rustls::crypto::CryptoProvider>,
    skip_fp_check: bool,
}

impl CertVerifier {
    pub fn new(fingerprint: Vec<u8>, crypto_provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self {
            fingerprint,
            crypto_provider,
            skip_fp_check: false,
        }
    }

    pub fn new_no_fingerprint_check(crypto_provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self {
            fingerprint: vec![],
            crypto_provider,
            skip_fp_check: true,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for CertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if self.skip_fp_check {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }

        match x509_parser::prelude::X509Certificate::from_der(end_entity) {
            Ok(cert) => {
                use sha2::Digest;
                let fingerprint = sha2::Sha256::digest(cert.1.subject_pki.raw);
                if fingerprint.as_slice() == self.fingerprint.as_slice() {
                    Ok(rustls::client::danger::ServerCertVerified::assertion())
                } else {
                    Err(rustls::Error::General(format!(
                        "Fingerprints does not match got={fingerprint:?} expected={:?}",
                        self.fingerprint
                    )))
                }
            }
            Err(err) => Err(rustls::Error::General(format!(
                "Failed to parse X509 cert: {err:?}"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.crypto_provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.crypto_provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.crypto_provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
