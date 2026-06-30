use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{
        self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter, ReadBuf, ReadHalf,
        WriteHalf,
    },
    net::TcpStream,
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tracing::error;

/// A stream that replays a fixed prefix of already-read bytes before delegating
/// to the inner stream. Used so a TLS upgrade can consume handshake bytes that
/// were read past the plaintext `Version` packet. Writes go straight through.
pub struct PrefixedRead<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> PrefixedRead<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedRead<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedRead<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[derive(Default)]
pub enum NetworkStream {
    #[default]
    None,
    Tcp {
        rx: ReadHalf<TcpStream>,
        tx: BufWriter<WriteHalf<TcpStream>>,
    },
    Tls {
        tx: BufWriter<WriteHalf<TlsStream<PrefixedRead<TcpStream>>>>,
        rx: ReadHalf<TlsStream<PrefixedRead<TcpStream>>>,
    },
}

impl NetworkStream {
    pub fn new(stream: TcpStream) -> Self {
        if let Err(err) = stream.set_nodelay(true) {
            error!(?err, "Failed to enable TCP_NODELAY on stream");
        }

        let (rx, tx) = tokio::io::split(stream);
        let tx = BufWriter::new(tx);

        Self::Tcp { rx, tx }
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
            NetworkStream::Tcp { tx, .. } => tx.flush().await?,
            NetworkStream::Tls { tx, .. } => tx.flush().await?,
            _ => (),
        }

        Ok(())
    }

    /// Upgrade a plaintext TCP stream to TLS.
    pub async fn upgrade(&mut self, acceptor: &TlsAcceptor, timeout: Duration) -> io::Result<()> {
        self.upgrade_with_prefix(acceptor, &[], timeout).await
    }

    /// Upgrade a plaintext TCP stream to TLS, replaying `prefix` ahead of the socket.
    ///
    /// `prefix` carries any bytes already read past the plaintext handshake that belong to the TLS
    /// handshake (e.g. a ClientHello coalesced into the same read as the `Version` packet); they
    /// are replayed ahead of the socket so the acceptor sees a complete handshake.
    pub async fn upgrade_with_prefix(
        &mut self,
        acceptor: &TlsAcceptor,
        prefix: &[u8],
        timeout: Duration,
    ) -> io::Result<()> {
        let old = std::mem::take(self);
        *self = match old {
            NetworkStream::Tcp { rx, tx } => {
                let tx = tx.into_inner();
                let stream = rx.unsplit(tx);
                let stream = PrefixedRead::new(prefix.to_vec(), stream);

                let stream = tokio::time::timeout(timeout, acceptor.accept(stream))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "TLS upgrade timed out")
                    })??;
                let (rx, tx) = io::split(stream);
                let tx = BufWriter::new(tx);
                Self::Tls { tx, rx }
            }
            _ => old,
        };

        Ok(())
    }
}
