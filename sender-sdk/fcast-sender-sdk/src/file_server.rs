use std::{
    collections::HashMap,
    // convert::Infallible,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    str::FromStr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

use anyhow::bail;
// use bytes::Bytes;
// use http::{HeaderMap, HeaderValue, Response, StatusCode};
// use http_body_util::{combinators::BoxBody, BodyExt, Full};
// use hyper::service::service_fn;
// use hyper_util::rt::{TokioExecutor, TokioIo};
use log::{debug, error};
use parsers_common::{find_first_cr_lf, find_first_double_cr_lf, parse_header_map};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    runtime::Handle,
    sync::Mutex,
};
use uuid::Uuid;

const MAX_CHUNK_SIZE: u64 = 1024 * 512;
const DEFAULT_REQUEST_BUF_CAP: usize = 1024;

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
    #[error("Failed to parse HTTP range")]
    HttpRangeParse,
    #[error("Utf8 error")]
    Utf8(#[from] std::str::Utf8Error),
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

    async fn emtpy_response<T: tokio::io::AsyncWrite + std::marker::Unpin>(
        mut writer: T,
        status: my_http::StatusCode,
    ) -> Result<(), FileRequestError> {
        let start_line = my_http::ResponseStartLine {
            protocol: my_http::Protocol::Http11,
            status_code: status,
        }
        .serialize();
        writer.write_all(&start_line).await?;

        writer
            .write_all(my_http::KnownHeaderNames::CONTENT_LENGTH)
            .await?;
        writer.write_all(b": ").await?;
        writer.write_all(b"0").await?;
        writer.write_all(b"\r\n").await?;
        writer.write_all(b"\r\n").await?;

        Ok(())
    }

    async fn handle_get_file_request(
        stream: tokio::net::TcpStream,
        uuid: Uuid,
        files: FileMapLock,
        headers: &[(&[u8], &[u8])],
    ) -> Result<(), FileRequestError> {
        let files = files.lock().await;
        let Some(fd) = files.get(&uuid) else {
            debug!("No file found for `{uuid}`");
            return Self::emtpy_response(stream, my_http::StatusCode::NotFound).await;
        };

        let raw_fd = fd.as_raw_fd();
        let dup_fd = unsafe { libc::dup(raw_fd) };
        let mut file = unsafe { tokio::fs::File::from_raw_fd(dup_fd) };
        file.seek(std::io::SeekFrom::Start(0)).await?;

        let mut writer = tokio::io::BufWriter::new(stream); // NOTE: MUST manually flush

        let file_meta = file.metadata().await?;
        let file_length = file_meta.len();
        let mut reader = tokio::io::BufReader::new(file);
        if let Some(range) = headers.iter().find_map(|(name, val)| {
            if *name == my_http::KnownHeaderNames::RANGE {
                Some(val)
            } else {
                None
            }
        }) {
            let mut ranges = http_range::HttpRange::parse_bytes(range, file_length)
                .map_err(|_| FileRequestError::HttpRangeParse)?;
            if let Some(range) = ranges.get_mut(0) {
                range.length = range.length.min(MAX_CHUNK_SIZE);
                let start_line = my_http::ResponseStartLine {
                    protocol: my_http::Protocol::Http11,
                    status_code: my_http::StatusCode::ParitalContent,
                }
                .serialize();
                writer.write_all(&start_line).await?;

                let headers = [
                    (
                        my_http::KnownHeaderNames::CONTENT_RANGE,
                        format!(
                            "bytes {}-{}/{file_length}",
                            range.start,
                            range.start + range.length - 1
                        ),
                    ),
                    (
                        my_http::KnownHeaderNames::CONTENT_TYPE,
                        "application/octet-stream".to_string(),
                    ),
                    (
                        my_http::KnownHeaderNames::CONTENT_LENGTH,
                        range.length.to_string(),
                    ),
                ];

                // TODO: should be made function
                for header in headers {
                    writer.write_all(header.0).await?;
                    writer.write_all(b": ").await?;
                    writer.write_all(header.1.as_bytes()).await?;
                    writer.write_all(b"\r\n").await?;
                }

                writer.write_all(b"\r\n").await?;

                reader.seek(std::io::SeekFrom::Start(range.start)).await?;
                let mut read_buf = [0u8; 1024 * 8];
                let mut bytes_read = 0;
                while bytes_read < range.length {
                    let n = reader.read(&mut read_buf).await?;
                    let end = read_buf.len().min((range.length - bytes_read) as usize);
                    writer.write_all(&read_buf[..end]).await?;
                    bytes_read += n as u64;
                }

                writer.flush().await?;
            } else {
                return Self::emtpy_response(writer, my_http::StatusCode::BadRequest).await;
            }
        } else {
            let start_line = my_http::ResponseStartLine {
                protocol: my_http::Protocol::Http11,
                status_code: my_http::StatusCode::ParitalContent,
            }
            .serialize();
            writer.write_all(&start_line).await?;

            let headers = [
                (
                    my_http::KnownHeaderNames::CONTENT_TYPE,
                    "application/octet-stream".to_string(),
                ),
                (
                    my_http::KnownHeaderNames::CONTENT_LENGTH,
                    file_length.to_string(),
                ),
            ];

            // TODO: should be made function
            for header in headers {
                writer.write_all(header.0).await?;
                writer.write_all(b": ").await?;
                writer.write_all(header.1.as_bytes()).await?;
                writer.write_all(b"\r\n").await?;
            }

            writer.write_all(b"\r\n").await?;

            tokio::io::copy(&mut reader, &mut writer).await?;

            writer.flush().await?;
        }

        Ok(())
    }

    async fn handle_request(
        stream: tokio::net::TcpStream,
        method: my_http::Method,
        path: &[u8],
        headers: &[(&[u8], &[u8])],
        files: FileMapLock,
    ) -> Result<(), FileRequestError> {
        match method {
            my_http::Method::Get => {
                let Some(path) = str::from_utf8(path)?.strip_prefix('/') else {
                    debug!("Invalid path in URI");
                    return Self::emtpy_response(stream, my_http::StatusCode::NotFound).await;
                };

                let Ok(uuid) = Uuid::from_str(path) else {
                    debug!("Path is not a valid UUID: {path}");
                    return Self::emtpy_response(stream, my_http::StatusCode::NotFound).await;
                };

                Self::handle_get_file_request(stream, uuid, files, headers).await?;
            }
            _ => return Self::emtpy_response(stream, my_http::StatusCode::MethodNotAllowed).await,
        }
        Ok(())
    }

    async fn dispatch_request(
        mut stream: tokio::net::TcpStream,
        files: FileMapLock,
    ) -> anyhow::Result<()> {
        let mut request_buf = Vec::<u8>::with_capacity(DEFAULT_REQUEST_BUF_CAP);
        let mut read_buf = [0u8; 1024];
        let start_line_end = 'out: {
            loop {
                let n = stream.read(&mut read_buf).await?;
                request_buf.extend_from_slice(&read_buf[..n]);
                if let Some(cr_idx) = find_first_cr_lf(&request_buf) {
                    break 'out cr_idx + 2;
                }
                if n < read_buf.len() {
                    break;
                }
            }
            bail!("Missing start line");
        };

        let start_line_buf = request_buf[..start_line_end].to_vec();
        let start_line = my_http::parse_request_start_line(&start_line_buf)?;
        let header_map_end = 'out: {
            loop {
                if let Some(cr_idx) = find_first_double_cr_lf(&request_buf) {
                    break 'out cr_idx + 4;
                }
                let n = stream.read(&mut read_buf).await?;
                request_buf.extend_from_slice(&read_buf[..n]);
                if n < read_buf.len() {
                    break;
                }
            }
            bail!("Missing headers");
        };

        let headers = parse_header_map(&request_buf[start_line_end..header_map_end])?;

        // we don't care about the body

        Self::handle_request(stream, start_line.0, start_line.1, &headers, files).await?;

        Ok(())
    }

    async fn serve(listen_port: Arc<AtomicU16>, files: FileMapLock) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("[::]:0").await?;
        let bound_port = listener.local_addr()?.port();
        listen_port.store(bound_port, Ordering::Relaxed);

        while let Ok((stream, addr)) = listener.accept().await {
            debug!("Got connection from {addr:?}");
            let files = Arc::clone(&files);
            tokio::spawn(async move {
                if let Err(err) = Self::dispatch_request(stream, files).await {
                    error!("Failed to handle request: {err}");
                }
            });
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
