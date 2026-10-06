use std::collections::HashMap;

use tracing::warn;

pub fn sec_to_string(sec: f64) -> String {
    let time_secs = sec % 60.0;
    let time_mins = (sec / 60.0) % 60.0;
    let time_hours = sec / 60.0 / 60.0;

    format!(
        "{:02}:{:02}:{:02}",
        time_hours as u32, time_mins as u32, time_secs as u32,
    )
}

pub fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("UNIX_EPOCH is always earlier than now")
        .as_millis() as u64
}

pub fn map_to_header_map(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut header_map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) else {
            warn!(k, "Invalid header name");
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) else {
            warn!(v, "Invalid header value");
            continue;
        };
        header_map.insert(name, value);
    }

    header_map
}

/// The response body once it is known to fit in `max` bytes, judged by
/// Content-Length up front and by the running total for a chunked body, so a
/// sender's URL cannot stream into memory forever.
pub async fn read_body_capped(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<bytes::Bytes, BodyError> {
    if resp.content_length().is_some_and(|len| len > max as u64) {
        return Err(BodyError::TooLarge(max));
    }
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(BodyError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("request failed: {0:?}")]
    Request(#[from] reqwest::Error),
    #[error("body larger than {0} bytes")]
    TooLarge(usize),
}

#[cfg(test)]
mod tests {
    use super::{BodyError, read_body_capped};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One connection, answered with `head` and then `body_len` bytes.
    async fn serve_once(head: &'static str, body_len: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(head.as_bytes()).await;
            let chunk = vec![b'x'; 8192];
            let mut left = body_len;
            while left > 0 {
                let n = left.min(chunk.len());
                if sock.write_all(&chunk[..n]).await.is_err() {
                    return;
                }
                left -= n;
            }
        });
        format!("http://{addr}/")
    }

    async fn get(url: &str) -> reqwest::Response {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        reqwest::Client::new().get(url).send().await.unwrap()
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_read() {
        let url = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n", 1000).await;
        assert_eq!(read_body_capped(get(&url).await, 1000).await.unwrap().len(), 1000);
    }

    #[tokio::test]
    async fn an_announced_oversized_body_is_refused_unread() {
        let url = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\nConnection: close\r\n\r\n", 0).await;
        assert!(matches!(read_body_capped(get(&url).await, 1 << 20).await, Err(BodyError::TooLarge(_))));
    }

    #[tokio::test]
    async fn an_unannounced_body_stops_at_the_cap() {
        // no length, so the body runs to the close, far past the cap
        let url = serve_once("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", 4 << 20).await;
        assert!(matches!(read_body_capped(get(&url).await, 1 << 20).await, Err(BodyError::TooLarge(_))));
    }
}
