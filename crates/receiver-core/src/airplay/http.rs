//! Minimal HTTP/1.1 + RTSP/1.0 request parser and response writer.
//!
//! AirPlay mirroring speaks a hybrid protocol over a single TCP connection: some
//! requests use the `RTSP/1.0` protocol token (OPTIONS, SETUP, RECORD, ...) and
//! others use `HTTP/1.1` (GET /info, POST /feedback, ...). Both share the same
//! request-line + headers + Content-Length body framing, so one small parser
//! handles both. The RAOP module uses `rtsp-types`, but that crate is strict
//! about the `RTSP/` version token and rejects `HTTP/1.1`, which is why this
//! hand-rolled parser exists.

use std::fmt::Write as _;

use anyhow::{Result, bail};
use bytes::{Buf, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    net::TcpStream,
};

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub protocol: String,
    pub headers: Vec<(String, String)>,
    /// Request body (e.g. the binary plist on `SETUP`/`fp-setup`).
    pub body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The path component of the URL, without any `?query`.
    pub fn path(&self) -> &str {
        self.url.split('?').next().unwrap_or(&self.url)
    }
}

#[derive(Debug)]
pub struct Response {
    pub protocol: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(protocol: &str, status: u16, reason: &str) -> Self {
        Self {
            protocol: protocol.to_owned(),
            status,
            reason: reason.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_owned(), content_type.to_owned()));
        self.body = body;
        self
    }

    fn serialize(&self) -> Vec<u8> {
        let mut head = String::new();
        let _ = write!(
            head,
            "{} {} {}\r\n",
            self.protocol, self.status, self.reason
        );
        for (k, v) in &self.headers {
            let _ = write!(head, "{k}: {v}\r\n");
        }
        // Always emit Content-Length so the peer knows the body boundary.
        let _ = write!(head, "Content-Length: {}\r\n\r\n", self.body.len());
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

pub struct Connection {
    stream: BufWriter<TcpStream>,
    buffer: BytesMut,
}

impl Connection {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream: BufWriter::new(stream),
            buffer: BytesMut::with_capacity(2048),
        }
    }

    /// Read the next complete request, or `None` on a clean EOF.
    pub async fn read_request(&mut self) -> Result<Option<Request>> {
        loop {
            if let Some(req) = parse_request(&mut self.buffer)? {
                return Ok(Some(req));
            }
            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                bail!("connection reset by peer with a partial request buffered");
            }
        }
    }

    pub async fn write_response(&mut self, response: &Response) -> Result<()> {
        self.stream.write_all(&response.serialize()).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

/// Attempt to parse a single request from `buf`. On success consumes the request
/// bytes from `buf`. Returns `Ok(None)` when more data is needed.
fn parse_request(buf: &mut BytesMut) -> Result<Option<Request>> {
    let Some(header_end) = find_subslice(buf, b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buf[..header_end])?;
    let mut lines = head.split("\r\n");

    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_owned();
    let url = parts.next().unwrap_or("").to_owned();
    let protocol = parts.next().unwrap_or("").to_owned();
    if method.is_empty() || url.is_empty() || protocol.is_empty() {
        bail!("malformed request line: {request_line:?}");
    }

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            bail!("malformed header line: {line:?}");
        };
        let name = name.trim().to_owned();
        let value = value.trim().to_owned();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name, value));
    }

    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        // Body not fully received yet.
        return Ok(None);
    }

    let body = buf[body_start..body_start + content_length].to_vec();
    buf.advance(body_start + content_length);

    Ok(Some(Request {
        method,
        url,
        protocol,
        headers,
        body,
    }))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_with_query_and_no_body() {
        let mut buf = BytesMut::from(
            &b"GET /info?txtAirPlay HTTP/1.1\r\nCSeq: 0\r\nUser-Agent: AirPlay/x\r\n\r\n"[..],
        );
        let req = parse_request(&mut buf).unwrap().unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "/info?txtAirPlay");
        assert_eq!(req.path(), "/info");
        assert_eq!(req.protocol, "HTTP/1.1");
        assert_eq!(req.header("cseq"), Some("0"));
        assert!(req.body.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn waits_for_full_body() {
        let mut buf =
            BytesMut::from(&b"POST /fp-setup RTSP/1.0\r\nContent-Length: 4\r\n\r\nAB"[..]);
        // Body incomplete -> None, nothing consumed.
        assert!(parse_request(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"CD");
        let req = parse_request(&mut buf).unwrap().unwrap();
        assert_eq!(req.body, b"ABCD");
        assert_eq!(req.protocol, "RTSP/1.0");
    }
}
