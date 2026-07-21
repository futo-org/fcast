use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use parking_lot::Mutex;

use crate::error::{SabrError, SabrResult};

/// A streaming response body: an async stream of byte chunks, consumed
/// incrementally by the UMP reader as they arrive off the wire.
pub type SabrBody = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

/// Handle to a canned transport's recorded request bodies (for test assertions).
pub type RecordedRequests = Arc<Mutex<Vec<Vec<u8>>>>;

/// How the session sends SABR requests.
pub enum SabrTransport {
    /// Real HTTP via reqwest.
    Http(reqwest::Client),
    /// Replays canned UMP responses in order and records the request bodies it
    /// is asked to send. For tests only.
    Canned {
        responses: Mutex<VecDeque<Vec<u8>>>,
        requests: RecordedRequests,
        status: u16,
    },
}

impl SabrTransport {
    /// Real HTTP transport. The caller supplies a configured `reqwest::Client`
    /// (TLS backend, proxy, timeouts, ...), so transport policy lives with the
    /// embedder rather than in this crate.
    pub fn http(client: reqwest::Client) -> Self {
        SabrTransport::Http(client)
    }

    /// A test transport that replays `responses` (raw UMP bytes) in order. The
    /// returned handle exposes the request bodies the session sent.
    pub fn canned(responses: Vec<Vec<u8>>) -> (Self, RecordedRequests) {
        let requests: RecordedRequests = Arc::new(Mutex::new(Vec::new()));
        let transport = SabrTransport::Canned {
            responses: Mutex::new(responses.into()),
            requests: requests.clone(),
            status: 200,
        };
        (transport, requests)
    }

    /// A test transport that always responds with `status` and an empty body
    /// (for exercising error/HTTP-status handling).
    pub fn canned_status(status: u16) -> Self {
        SabrTransport::Canned {
            responses: Mutex::new(VecDeque::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            status,
        }
    }

    /// Send a SABR POST and return `(status, body_stream)`.
    pub(crate) async fn fetch(
        &self,
        url: String,
        body: Vec<u8>,
        headers: Vec<(&'static str, String)>,
    ) -> SabrResult<(u16, SabrBody)> {
        match self {
            SabrTransport::Http(client) => {
                let mut rb = client.post(&url).body(body);
                for (k, v) in headers {
                    rb = rb.header(k, v);
                }
                let resp = rb.send().await.map_err(|e| SabrError::Http(e.to_string()))?;
                let status = resp.status().as_u16();
                // Adapt reqwest's `chunk()` into a `Stream` (avoids the `stream`
                // feature). The client's read timeout still bounds each `chunk()`,
                // so a stalled server surfaces as an error rather than hanging.
                let stream = futures::stream::unfold(resp, |mut resp| async move {
                    match resp.chunk().await {
                        Ok(Some(chunk)) => Some((Ok(chunk), resp)),
                        Ok(None) => None,
                        Err(e) => Some((Err(io::Error::other(e.to_string())), resp)),
                    }
                });
                let body: SabrBody = Box::pin(stream);
                Ok((status, body))
            }
            SabrTransport::Canned { responses, requests, status } => {
                requests.lock().push(body);
                let bytes = responses.lock().pop_front().unwrap_or_default();
                let stream =
                    futures::stream::once(async move { Ok::<_, io::Error>(Bytes::from(bytes)) });
                let body: SabrBody = Box::pin(stream);
                Ok((*status, body))
            }
        }
    }
}
