use std::{
    collections::HashMap,
    convert::Infallible,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    str::FromStr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use log::{debug, error};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    runtime::Handle,
    sync::Mutex,
};
use uuid::Uuid;

const MAX_CHUNK_SIZE: u64 = 1024 * 512;

#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
#[derive(thiserror::Error, Debug)]
pub enum FileServerError {
    #[error("Server is not running")]
    NotRunning,
}

type FileMapLock = Arc<Mutex<HashMap<Uuid, OwnedFd>>>;

/// http://:{port}/{location}
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug)]
pub struct FileStoreEntry {
    pub location: String,
    pub port: u16,
}

#[derive(Debug, thiserror::Error)]
enum FileRequestError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP: {0}")]
    Http(#[from] http::Error),
    #[error("Failed to parse HTTP range")]
    HttpRangeParse,
}

macro_rules! empty_resp {
    ($status:ident) => {
        return Ok(Response::builder()
            .status(StatusCode::$status)
            .body(Full::new(Bytes::new()).boxed())?)
    };
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct FileServer {
    rt_handle: Handle,
    listen_port: Arc<AtomicU16>,
    files: FileMapLock,
}

impl FileServer {
    pub(crate) fn new(rt_handle: Handle) -> Self {
        Self {
            rt_handle,
            listen_port: Arc::new(AtomicU16::new(0)),
            files: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn handle_get_file_request(
        uuid: Uuid,
        files: FileMapLock,
        headers: &HeaderMap<HeaderValue>,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, FileRequestError> {
        let files = files.lock().await;
        let Some(fd) = files.get(&uuid) else {
            debug!("No file found for `{uuid}`");
            empty_resp!(NOT_FOUND);
        };

        let raw_fd = fd.as_raw_fd();
        let dup_fd = unsafe { libc::dup(raw_fd) };
        let mut file = unsafe { tokio::fs::File::from_raw_fd(dup_fd) };
        file.seek(std::io::SeekFrom::Start(0)).await?;

        let file_meta = file.metadata().await?;
        let file_length = file_meta.len();
        if let Some(range) = headers.get(http::header::RANGE) {
            let mut ranges = http_range::HttpRange::parse_bytes(range.as_bytes(), file_length)
                .map_err(|_| FileRequestError::HttpRangeParse)?;
            match ranges.get_mut(0) {
                Some(range) => {
                    range.length = range.length.min(MAX_CHUNK_SIZE);
                    let mut file_part = vec![0; range.length as usize];
                    file.seek(std::io::SeekFrom::Start(range.start)).await?;
                    file.read_exact(&mut file_part).await?;
                    Ok(Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(http::header::CONTENT_TYPE, "application/octet-stream")
                        .header(
                            http::header::CONTENT_RANGE,
                            format!(
                                "bytes {}-{}/{file_length}",
                                range.start,
                                range.start + range.length - 1
                            ),
                        )
                        .body(Full::new(Bytes::from_owner(file_part)).boxed())?)
                }
                None => empty_resp!(INTERNAL_SERVER_ERROR),
            }
        } else {
            let mut file_contents: Vec<u8> = Vec::with_capacity(file_meta.len() as usize);
            file.read_to_end(&mut file_contents).await?;

            Ok(Response::builder()
                .header(http::header::CONTENT_TYPE, "application/octet-stream")
                .body(Full::new(Bytes::from_owner(file_contents)).boxed())?)
        }
    }

    async fn handle_request(
        request: http::Request<hyper::body::Incoming>,
        files: FileMapLock,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, FileRequestError> {
        match *request.method() {
            http::Method::GET => {
                let Some(path) = request.uri().path().strip_prefix('/') else {
                    debug!("Invalid path in URI: {:?}", request.uri());
                    empty_resp!(NOT_FOUND);
                };

                let Ok(uuid) = Uuid::from_str(path) else {
                    debug!("Path is not a valid UUID: {path}");
                    empty_resp!(NOT_FOUND);
                };

                Self::handle_get_file_request(uuid, files, request.headers()).await
            }
            _ => empty_resp!(METHOD_NOT_ALLOWED),
        }
    }

    async fn dispatch_request(
        stream: tokio::net::TcpStream,
        files: FileMapLock,
    ) -> anyhow::Result<()> {
        let res = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
            .serve_connection(
                TokioIo::new(stream),
                service_fn(|request: http::Request<hyper::body::Incoming>| {
                    // debug!("REQUEST: {request:?}");
                    Self::handle_request(request, Arc::clone(&files))
                }),
            )
            .await;

        if let Err(err) = res {
            error!("Failed to handle request: {err}");
        }

        Ok(())
    }

    async fn serve(listen_port: Arc<AtomicU16>, files: FileMapLock) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("[::]:0").await?;
        let bound_port = listener.local_addr()?.port();
        listen_port.store(bound_port, Ordering::Relaxed);

        while let Ok((stream, addr)) = listener.accept().await {
            debug!("Got connection from {addr:?}");
            let files = Arc::clone(&files);
            tokio::spawn(Self::dispatch_request(stream, files));
        }

        Ok(())
    }

    pub(crate) fn start(&self) {
        let listen_port = Arc::clone(&self.listen_port);
        let files = Arc::clone(&self.files);
        self.rt_handle.spawn(Self::serve(listen_port, files));
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl FileServer {
    pub fn serve_file(&self, fd: i32) -> Result<FileStoreEntry, FileServerError> {
        let port = self.listen_port.load(Ordering::Relaxed);
        if port == 0 {
            return Err(FileServerError::NotRunning);
        }

        let id = Uuid::new_v4();
        let files = Arc::clone(&self.files);
        self.rt_handle.spawn(async move {
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut files = files.lock().await;
            files.insert(id, fd);
        });

        Ok(FileStoreEntry {
            location: id.to_string(),
            port,
        })
    }
}
