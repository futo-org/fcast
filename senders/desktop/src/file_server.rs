#[cfg(target_os = "windows")]
use std::net::Ipv4Addr;
use std::{
    collections::HashMap,
    mem::MaybeUninit,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

use anyhow::{Result, bail};
use bytes::Bytes;
use http_range::HttpRange;
use hyper::{Request, Response, StatusCode, header};
use parking_lot::RwLock;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncSeek},
    net::TcpListener,
    sync::oneshot::{Receiver, Sender, channel},
};
use tracing::{debug, error};
use uuid::Uuid;

const MAX_PARTIAL_CONTENT_SIZE: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq)]
enum FileSeekState {
    NeedSeek,
    Seeking,
    Reading,
}

enum FileBody {
    Empty,
    Full {
        file: File,
        remaining: u64,
    },
    Range {
        file: File,
        start_offset: u64,
        remaining: u64,
        seek_state: FileSeekState,
    },
}

fn poll_read(
    file: &mut File,
    remaining: &mut u64,
    cx: &mut task::Context<'_>,
) -> Poll<std::result::Result<Option<Bytes>, std::io::Error>> {
    let mut buf = [MaybeUninit::uninit(); 16 * 1024];
    let rem_len = buf.len().min(*remaining as usize);
    let mut read_buf = tokio::io::ReadBuf::uninit(&mut buf[0..rem_len]);
    match Pin::new(file).poll_read(cx, &mut read_buf) {
        Poll::Ready(Ok(())) => {
            let filled = read_buf.filled();
            *remaining -= filled.len() as u64;
            if filled.is_empty() {
                Poll::Ready(Ok(None))
            } else {
                Poll::Ready(Ok(Some(Bytes::copy_from_slice(filled))))
            }
        }
        Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        Poll::Pending => Poll::Pending,
    }
}

impl hyper::body::Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let opt = task::ready!(match *self {
            FileBody::Empty => return Poll::Ready(None),
            FileBody::Full {
                ref mut file,
                ref mut remaining,
            } => {
                poll_read(file, remaining, cx)
            }
            FileBody::Range {
                ref mut file,
                ref start_offset,
                ref mut remaining,
                ref mut seek_state,
            } => {
                if *seek_state == FileSeekState::NeedSeek {
                    *seek_state = FileSeekState::Seeking;
                    if let Err(err) =
                        Pin::new(&mut *file).start_seek(std::io::SeekFrom::Start(*start_offset))
                    {
                        return Poll::Ready(Some(Err(err)));
                    }
                }

                if *seek_state == FileSeekState::Seeking {
                    match Pin::new(&mut *file).poll_complete(cx) {
                        Poll::Ready(Ok(..)) => *seek_state = FileSeekState::Reading,
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Pending => return Poll::Pending,
                    }
                }

                poll_read(file, remaining, cx)
            }
        });

        match opt {
            Ok(res) => match res {
                Some(res) => Poll::Ready(Some(Ok(hyper::body::Frame::data(res)))),
                None => Poll::Ready(None),
            },
            Err(err) => Poll::Ready(Some(Err(err))),
        }
    }
}

fn empty(status: StatusCode) -> Result<Response<FileBody>, hyper::http::Error> {
    Response::builder().status(status).body(FileBody::Empty)
}

fn not_found() -> Result<Response<FileBody>, hyper::http::Error> {
    empty(StatusCode::NOT_FOUND)
}

fn internal_server_error() -> Result<Response<FileBody>, hyper::http::Error> {
    empty(StatusCode::INTERNAL_SERVER_ERROR)
}

fn bad_request() -> Result<Response<FileBody>, hyper::http::Error> {
    empty(StatusCode::BAD_REQUEST)
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: PathBuf,
    content_type: &'static str,
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    files: Arc<RwLock<HashMap<Uuid, FileEntry>>>,
) -> Result<Response<FileBody>, hyper::http::Error> {
    debug!(?req, "Got request");

    if req.method() != hyper::Method::GET {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }

    let uri = req.uri();
    let path = uri.path();
    debug!(path, "Handling request");
    let Some(maybe_uuid) = path.strip_prefix('/') else {
        error!("Invalid path");
        return not_found();
    };

    let Ok(uuid) = uuid::Uuid::parse_str(maybe_uuid) else {
        error!("Path is not valid uuid");
        return not_found();
    };

    let (file, content_type) = {
        let entry = {
            let files = files.read();
            let Some(entry) = files.get(&uuid) else {
                error!(?uuid, "File not found");
                return not_found();
            };

            entry.clone()
        };

        let Ok(file) = File::open(&entry.path).await else {
            return not_found();
        };

        (file, entry.content_type)
    };

    let Ok(meta) = file.metadata().await else {
        return internal_server_error();
    };
    let file_len = meta.len();

    let headers = req.headers();
    match headers.get(hyper::http::header::RANGE) {
        Some(range) => {
            let Ok(mut ranges) = HttpRange::parse_bytes(range.as_bytes(), file_len) else {
                return bad_request();
            };

            if let Some(range) = ranges.get_mut(0) {
                range.length = range.length.min(MAX_PARTIAL_CONTENT_SIZE);

                let bytes_range_str = format!(
                    "bytes {}-{}/{file_len}",
                    range.start,
                    range.start + range.length - 1
                );

                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CONTENT_RANGE, bytes_range_str)
                    .header(header::CONTENT_LENGTH, range.length)
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(FileBody::Range {
                        file,
                        remaining: range.length,
                        start_offset: range.start,
                        seek_state: FileSeekState::NeedSeek,
                    })
            } else {
                bad_request()
            }
        }
        None => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, file_len)
            .header(header::ACCEPT_RANGES, "bytes")
            .body(FileBody::Full {
                file,
                remaining: file_len,
            }),
    }
}

#[derive(Debug)]
struct BoundPortPair {
    ipv6: u16,
    ipv4: u16,
}

async fn run_server(
    files: Arc<RwLock<HashMap<Uuid, FileEntry>>>,
    bound_port_tx: Sender<BoundPortPair>,
    mut quit_rx: Receiver<()>,
) -> Result<()> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).await?;
    #[cfg(target_os = "windows")]
    let ipv4_listener =
        TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).await?;

    let bound_port = listener.local_addr()?.port();
    #[allow(unused_mut)]
    let mut bound_port_pair = BoundPortPair {
        ipv6: bound_port,
        ipv4: bound_port,
    };
    #[cfg(target_os = "windows")]
    {
        let ipv4_port = ipv4_listener.local_addr()?.port();
        bound_port_pair.ipv4 = ipv4_port;
    }
    if bound_port_tx.send(bound_port_pair).is_err() {
        bail!("Could not send bound port");
    }

    async fn handle_connection(
        files: &Arc<RwLock<HashMap<Uuid, FileEntry>>>,
        conn: std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
    ) {
        let (stream, _) = match conn {
            Ok(stream) => stream,
            Err(err) => {
                error!(?err, "Accept error");
                return;
            }
        };

        let files = Arc::clone(&files);
        tokio::spawn(async move {
            let stream = hyper_util::rt::TokioIo::new(Box::pin(stream));
            let server =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());

            let conn = server.serve_connection_with_upgrades(
                stream,
                hyper::service::service_fn({
                    |req| {
                        let files = Arc::clone(&files);
                        async move { handle_request(req, files).await }
                    }
                }),
            );

            if let Err(err) = conn.await {
                error!(?err, "Failed to handle connection");
            }
        });
    }

    loop {
        // We create 2 separate select blocks because the select! macro does not allow adding cfg attributes on branch arms
        #[cfg(not(target_os = "windows"))]
        tokio::select! {
            conn = listener.accept() => handle_connection(&files, conn).await,
            _ = &mut quit_rx => {
                debug!("Got quit signal");
                break;
            }
        }

        #[cfg(target_os = "windows")]
        tokio::select! {
            conn = listener.accept() => handle_connection(&files, conn).await,
            conn = ipv4_listener.accept() => handle_connection(&files, conn).await,
            _ = &mut quit_rx => {
                debug!("Got quit signal");
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct FileServer {
    files: Arc<RwLock<HashMap<Uuid, FileEntry>>>,
    bound_ports: BoundPortPair,
    quit_tx: Option<Sender<()>>,
}

impl FileServer {
    pub async fn new() -> Result<Self> {
        let files = Arc::new(RwLock::new(HashMap::new()));

        let (quit_tx, quit_rx) = channel();
        let (bound_port_tx, bound_port_rx) = channel::<BoundPortPair>();

        tokio::spawn({
            let files = Arc::clone(&files);
            async move {
                if let Err(err) = run_server(files, bound_port_tx, quit_rx).await {
                    error!(?err);
                }
            }
        });

        let bound_ports = bound_port_rx.await?;
        debug!(?bound_ports, "Received ports used by file server");

        Ok(Self {
            files,
            bound_ports,
            quit_tx: Some(quit_tx),
        })
    }

    pub fn add_file(&self, path: PathBuf, content_type: &'static str) -> Uuid {
        let id = Uuid::new_v4();
        let mut files = self.files.write();
        debug!(?id, ?path, "Adding file");
        let _ = files.insert(id, FileEntry { path, content_type });
        id
    }

    // pub fn remove_file(&self, id: &Uuid) {
    //     let mut files = self.files.write();
    //     let path = files.remove(id);
    //     debug!(?path, ?id, "Removed file");
    // }

    pub fn get_url(&self, local_addr: &fcast_sender_sdk::IpAddr, file_id: &Uuid) -> String {
        let port = match local_addr {
            fcast_sender_sdk::IpAddr::V4 { .. } => self.bound_ports.ipv4,
            fcast_sender_sdk::IpAddr::V6 { .. } => self.bound_ports.ipv6,
        };
        format!(
            "http://{}:{}/{}",
            fcast_sender_sdk::url_format_ip_addr(local_addr),
            port,
            file_id,
        )
    }
}

impl Drop for FileServer {
    fn drop(&mut self) {
        if let Some(quit_tx) = self.quit_tx.take() {
            let _ = quit_tx.send(());
        }
    }
}
