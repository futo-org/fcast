//! Prefetch cache for queue items.
//!
//! When a v4 queue is playing, the receiver downloads the first bytes of the
//! items around the current index in the background, so selecting the next or
//! previous item starts from memory instead of paying the network round-trips
//! at load time. This matters most for fcomp companion items, whose gst
//! source pulls one ~64K chunk per request at play time, and for image
//! slideshows flipping through a queue.
//!
//! Only the HEAD of each item is fetched (16M): small items (photos, gifs,
//! clips) end up fully resident and play via `media_source::build_bytes_source`,
//! larger ones keep a partial head that gets injected into the per-load
//! source element (`media_source::build_uri_source_with_head`), which serves
//! it from memory and streams the remainder.
//!
//! Design:
//! - The application recomputes the DESIRED window (neighbors of the current
//!   index, the current item itself is excluded since the pipeline is already
//!   streaming it) after every queue mutation and calls [`Cache::sync`].
//!   Entries outside the window are evicted, missing ones are fetched.
//! - Fetches are fire-and-forget tokio tasks ([`Prefetcher`]) delivering a
//!   [`Event`] back through the application's message loop. Epoch stamps make
//!   results from superseded syncs harmless.
//! - A cache miss (still in flight, too big, failed, disabled) is never an
//!   error: the load falls back to the live network path it always used.
//! - Kill switch: FCAST_NO_QUEUE_CACHE=1 disables everything.

use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use fcast_protocol::companion;
use tracing::{debug, warn};

use crate::fcast::CompanionContext;

/// Bound on one whole prefetch transfer. A hung transfer would otherwise pin
/// its in-flight slot forever and block every retry for that url.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How much of each item to prefetch. Items at most this big become COMPLETE
/// entries, bigger ones keep a partial head (start-from-memory, stream the
/// rest).
const HEAD_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Ceiling for the sum of all cached entries.
const TOTAL_MAX_BYTES: usize = 128 * 1024 * 1024;
/// Window shape around the current index. Ahead is deeper than behind since
/// queues mostly advance forward.
const BEHIND: usize = 1;
const AHEAD: usize = 2;

/// The queue-item indices worth prefetching for `current` in a queue of
/// `len` items: the window around the current index, clamped to the queue
/// bounds, excluding the current item itself. Ordered nearest-first so the
/// most likely flip target starts downloading first.
pub fn window_indices(len: usize, current: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for step in 1..=AHEAD.max(BEHIND) {
        if step <= AHEAD && current + step < len {
            out.push(current + step);
        }
        if step <= BEHIND
            && let Some(idx) = current.checked_sub(step)
        {
            out.push(idx);
        }
    }
    out
}

/// What to fetch for one queue item.
#[derive(Debug, Clone)]
pub struct PrefetchSpec {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
}

/// One resident prefetch result.
#[derive(Debug, Clone)]
pub struct CachedItem {
    pub bytes: Bytes,
    /// The whole resource is resident (it fit within [`HEAD_MAX_BYTES`]).
    pub complete: bool,
    /// Total resource size when the transport reported one. Partial http
    /// heads are only injectable with a known total.
    pub total: Option<u64>,
}

/// Result of one prefetch task, delivered through the application loop.
#[derive(Debug)]
pub enum Event {
    Fetched {
        url: String,
        epoch: u64,
        result: Result<CachedItem, FetchError>,
    },
}

#[derive(Debug)]
pub enum FetchError {
    /// The server ignores range requests and the item is bigger than the
    /// head cap, so a partial head would be unusable: expected, not a
    /// failure.
    HeadUnusable,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeadUnusable => {
                write!(f, "server ignores range requests, no usable head")
            }
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, CachedItem>,
    /// URL -> epoch of the sync that started the fetch. A result with a
    /// stale epoch is discarded (the fetch was superseded).
    in_flight: HashMap<String, u64>,
    desired: HashSet<String>,
    epoch: u64,
    disabled: bool,
}

impl Cache {
    pub fn new() -> Self {
        let disabled = std::env::var("FCAST_NO_QUEUE_CACHE").is_ok_and(|v| v == "1");
        if disabled {
            debug!("Queue prefetch cache disabled by FCAST_NO_QUEUE_CACHE");
        }
        Self {
            disabled,
            ..Default::default()
        }
    }

    /// Cached entry for `url` (complete item or partial head). The entry
    /// stays cached (re-selecting the same neighbor again stays instant),
    /// eviction is window-driven via [`sync`](Self::sync).
    pub fn get(&self, url: &str) -> Option<CachedItem> {
        self.entries.get(url).cloned()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.in_flight.clear();
        self.desired.clear();
    }

    /// Reconcile the cache against the desired window: evict entries that
    /// left the window and start fetches (via `fetch`) for missing ones.
    /// `fetch` receives the spec and the epoch to stamp on the result.
    pub fn sync(&mut self, desired: Vec<PrefetchSpec>, mut fetch: impl FnMut(PrefetchSpec, u64)) {
        if self.disabled {
            return;
        }
        self.epoch += 1;
        self.desired = desired.iter().map(|s| s.url.clone()).collect();

        self.entries.retain(|url, item| {
            let keep = self.desired.contains(url);
            if !keep {
                debug!(url, len = item.bytes.len(), "Evicting queue cache entry");
            }
            keep
        });
        // In-flight fetches for evicted urls cannot be cancelled, but their
        // results arrive with an old epoch and get discarded.
        self.in_flight.retain(|url, _| self.desired.contains(url));

        for spec in desired {
            if self.entries.contains_key(&spec.url) || self.in_flight.contains_key(&spec.url) {
                continue;
            }
            debug!(url = spec.url, "Prefetching queue item");
            self.in_flight.insert(spec.url.clone(), self.epoch);
            fetch(spec, self.epoch);
        }
    }

    pub fn on_event(&mut self, event: Event) {
        let Event::Fetched { url, epoch, result } = event;
        if self.in_flight.get(&url) != Some(&epoch) {
            debug!(url, "Discarding a superseded prefetch result");
            return;
        }
        self.in_flight.remove(&url);
        if !self.desired.contains(&url) {
            return;
        }
        match result {
            Ok(item) => {
                let resident: usize = self.entries.values().map(|i| i.bytes.len()).sum();
                if resident + item.bytes.len() > TOTAL_MAX_BYTES {
                    warn!(
                        url,
                        len = item.bytes.len(),
                        resident,
                        "Queue cache full, skipping entry"
                    );
                    return;
                }
                debug!(
                    url,
                    len = item.bytes.len(),
                    complete = item.complete,
                    "Queue item prefetched"
                );
                self.entries.insert(url, item);
            }
            Err(FetchError::HeadUnusable) => {
                debug!(url, "No usable head for the queue item, it will stream live");
            }
            Err(err) => {
                // Non-fatal: the live load path will report a real error if
                // the item is actually selected and still unreachable.
                debug!(url, %err, "Queue item prefetch failed");
            }
        }
    }
}

/// Spawns the background downloads. Mirrors `image::Downloader`'s two
/// transports (http via reqwest, fcomp via the companion channels) but
/// delivers raw size-capped bytes.
pub struct Prefetcher {
    msg_tx: crate::MessageSender,
    client: reqwest::Client,
    companion_ctx: CompanionContext,
}

impl Prefetcher {
    pub fn new(
        msg_tx: crate::MessageSender,
        client: reqwest::Client,
        companion_ctx: CompanionContext,
    ) -> Self {
        Self {
            msg_tx,
            client,
            companion_ctx,
        }
    }

    pub fn fetch(&self, spec: PrefetchSpec, epoch: u64) {
        let Ok(url) = url::Url::parse(&spec.url) else {
            debug!(url = spec.url, "Not prefetching an unparsable url");
            return;
        };
        let tx = self.msg_tx.clone();
        let raw_url = spec.url;
        match url.scheme() {
            "http" | "https" => {
                let client = self.client.clone();
                let headers = spec.headers;
                tokio::spawn(async move {
                    let result = match tokio::time::timeout(FETCH_TIMEOUT, fetch_http(&client, url, headers)).await {
                        Ok(result) => result,
                        Err(_) => Err(FetchError::Other("prefetch timed out".into())),
                    };
                    tx.queue_cache(Event::Fetched {
                        url: raw_url,
                        epoch,
                        result,
                    });
                });
            }
            "fcomp" => {
                let ctx = self.companion_ctx.clone();
                tokio::spawn(async move {
                    let result = match tokio::time::timeout(FETCH_TIMEOUT, fetch_comp(&ctx, &url)).await {
                        Ok(result) => result,
                        Err(_) => Err(FetchError::Other("prefetch timed out".into())),
                    };
                    tx.queue_cache(Event::Fetched {
                        url: raw_url,
                        epoch,
                        result,
                    });
                });
            }
            other => {
                debug!(scheme = other, "Not prefetching an unsupported scheme");
            }
        }
    }
}

async fn fetch_http(
    client: &reqwest::Client,
    url: url::Url,
    headers: Option<HashMap<String, String>>,
) -> Result<CachedItem, FetchError> {
    let other = |msg: String| FetchError::Other(msg);

    let random_user_agent = crate::user_agent::random_browser_user_agent(url.domain());
    // Ask for the head only. A range-capable server answers 206 with the
    // total in Content-Range; one that ignores ranges answers 200 with the
    // whole body, which is only kept when it fits the cap anyway.
    let mut request = client.get(url).header(
        reqwest::header::RANGE,
        format!("bytes=0-{}", HEAD_MAX_BYTES - 1),
    );
    let mut did_set_user_agent = false;
    if let Some(headers) = headers {
        let header_map = crate::utils::map_to_header_map(&headers);
        did_set_user_agent = header_map.contains_key(reqwest::header::USER_AGENT);
        request = request.headers(header_map);
    }
    if !did_set_user_agent {
        request = request.header(reqwest::header::USER_AGENT, random_user_agent);
    }

    let mut resp = request.send().await.map_err(|e| other(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(other(format!("http status {}", resp.status())));
    }

    let ranged = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    // "bytes 0-x/total" (a 206 without a parsable total counts as unranged,
    // a partial head would be unusable without the size).
    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse::<u64>().ok());

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| other(e.to_string()))? {
        if buf.len() + chunk.len() > HEAD_MAX_BYTES {
            // Only possible on a 200 from a server that ignored the range
            // request: without a usable head, drop the transfer.
            return Err(FetchError::HeadUnusable);
        }
        buf.extend_from_slice(&chunk);
    }

    let len = buf.len() as u64;
    let complete = match (ranged, total) {
        // 206: complete when the head covers the reported total.
        (true, Some(total)) => len >= total,
        // 200 that fit within the cap: the body IS the whole resource.
        (false, _) => true,
        // 206 without a parsable total: a body short of the requested
        // range means the resource ended (complete). A full-cap body is
        // ambiguous and gets rejected below.
        (true, None) => (len as usize) < HEAD_MAX_BYTES,
    };
    if !complete && total.is_none() {
        return Err(FetchError::HeadUnusable);
    }
    Ok(CachedItem {
        bytes: Bytes::from_owner(buf),
        complete,
        total: total.or(complete.then_some(len)),
    })
}

async fn fetch_comp(ctx: &CompanionContext, url: &url::Url) -> Result<CachedItem, FetchError> {
    let other = |msg: &str| FetchError::Other(msg.to_string());

    let url = crate::fcompsrc::FCompUrl::new(url).ok_or_else(|| other("invalid fcomp url"))?;
    let provider = ctx
        .get_provider(url.provider_id)
        .ok_or_else(|| other("companion provider not found"))?;

    // Resource size first, so the transfer can be bounded to the head.
    let mut info_rx = provider
        .get_resource_info(url.resource_id)
        .map_err(|_| other("companion resource info request failed"))?;
    let info = info_rx
        .recv()
        .await
        .ok_or_else(|| other("companion resource info unavailable"))?;
    let total = {
        let info = info.borrow_dependent();
        match info.resource_size_type() {
            fcast_protocol::v4::flat::CompanionResourceSize::Known => {
                info.resource_size_as_known().map(|s| s.size())
            }
            _ => None,
        }
    };

    // Companion read heads are inclusive ranges. With a known size read
    // exactly what is wanted, otherwise read up to the cap and keep the
    // partial head (fcompsrc learns the size itself, no total needed).
    let read_end = match total {
        Some(total) => total.min(HEAD_MAX_BYTES as u64),
        None => HEAD_MAX_BYTES as u64,
    };
    if read_end == 0 {
        return Err(other("companion resource is empty"));
    }
    let read_head = fcast_protocol::v4::flat::ResourceReadHead::new(0, read_end - 1);
    let mut resource_rx = provider
        .get_resource(url.resource_id, Some(read_head))
        .map_err(|_| other("companion resource request failed"))?;

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resource_rx.recv().await {
        match chunk.result {
            companion::GetResourceResult::NotFound => {
                return Err(other("companion resource not found"));
            }
            companion::GetResourceResult::Success(data) => {
                if buf.len() + data.len() > HEAD_MAX_BYTES {
                    // The provider overshot the read head: keep the cap.
                    buf.extend_from_slice(&data[..HEAD_MAX_BYTES - buf.len()]);
                    break;
                }
                buf.extend_from_slice(&data);
            }
        }
    }
    if buf.is_empty() {
        // A closed channel with no data is a failed transfer, not an empty
        // resource (the sender may have disconnected).
        return Err(other("companion transfer produced no data"));
    }
    let len = buf.len() as u64;
    let complete = match total {
        Some(total) => len >= total,
        // Unknown size: complete only when the provider stopped short of the
        // requested head (it ran out of data).
        None => (len as usize) < HEAD_MAX_BYTES,
    };
    Ok(CachedItem {
        bytes: Bytes::from_owner(buf),
        complete,
        total: total.or(complete.then_some(len)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(url: &str) -> PrefetchSpec {
        PrefetchSpec {
            url: url.to_string(),
            headers: None,
        }
    }

    fn enabled_cache() -> Cache {
        Cache {
            disabled: false,
            ..Default::default()
        }
    }

    fn item(data: &'static [u8]) -> CachedItem {
        CachedItem {
            bytes: Bytes::from_static(data),
            complete: true,
            total: Some(data.len() as u64),
        }
    }

    #[test]
    fn window_shape() {
        // Middle of a long queue: next first, then prev, then next+2.
        assert_eq!(window_indices(10, 5), vec![6, 4, 7]);
        // At the front: nothing behind.
        assert_eq!(window_indices(10, 0), vec![1, 2]);
        // At the back: nothing ahead.
        assert_eq!(window_indices(10, 9), vec![8]);
        // Tiny queues.
        assert_eq!(window_indices(1, 0), Vec::<usize>::new());
        assert_eq!(window_indices(2, 0), vec![1]);
        assert_eq!(window_indices(2, 1), vec![0]);
    }

    #[test]
    fn sync_fetches_missing_and_evicts_stale() {
        let mut cache = enabled_cache();
        let mut fetched = Vec::new();
        cache.sync(vec![spec("a"), spec("b")], |s, e| fetched.push((s.url, e)));
        assert_eq!(fetched.len(), 2);

        // Deliver one result.
        let (url, epoch) = fetched[0].clone();
        cache.on_event(Event::Fetched {
            url: url.clone(),
            epoch,
            result: Ok(item(b"data")),
        });
        assert!(cache.get(&url).is_some());

        // New window without "a": entry evicted, "c" fetched, "b" still in
        // flight and not re-fetched.
        fetched.clear();
        cache.sync(vec![spec("b"), spec("c")], |s, e| fetched.push((s.url, e)));
        assert!(cache.get("a").is_none());
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].0, "c");
    }

    #[test]
    fn stale_epoch_results_are_discarded() {
        let mut cache = enabled_cache();
        let mut fetched = Vec::new();
        cache.sync(vec![spec("a")], |s, e| fetched.push((s.url, e)));
        let (url, old_epoch) = fetched[0].clone();

        // "a" leaves the window and comes back: a NEW fetch starts with a
        // newer epoch.
        cache.sync(vec![spec("b")], |_, _| {});
        fetched.clear();
        cache.sync(vec![spec("a")], |s, e| fetched.push((s.url, e)));
        assert_eq!(fetched.len(), 1);
        let new_epoch = fetched[0].1;
        assert_ne!(old_epoch, new_epoch);

        // The superseded task's result must not be stored.
        cache.on_event(Event::Fetched {
            url: url.clone(),
            epoch: old_epoch,
            result: Ok(item(b"stale")),
        });
        assert!(cache.get(&url).is_none());

        // The current one lands.
        cache.on_event(Event::Fetched {
            url: url.clone(),
            epoch: new_epoch,
            result: Ok(item(b"fresh")),
        });
        assert_eq!(cache.get(&url).unwrap().bytes.as_ref(), b"fresh");
    }

    #[test]
    fn failed_fetches_do_not_wedge_the_slot() {
        let mut cache = enabled_cache();
        let mut fetched = Vec::new();
        cache.sync(vec![spec("a")], |s, e| fetched.push((s.url, e)));
        let (url, epoch) = fetched[0].clone();
        cache.on_event(Event::Fetched {
            url: url.clone(),
            epoch,
            result: Err(FetchError::Other("nope".into())),
        });
        assert!(cache.get(&url).is_none());

        // The next sync retries the fetch (it is no longer in flight).
        fetched.clear();
        cache.sync(vec![spec("a")], |s, e| fetched.push((s.url, e)));
        assert_eq!(fetched.len(), 1);
    }

    #[test]
    fn disabled_cache_does_nothing() {
        let mut cache = Cache {
            disabled: true,
            ..Default::default()
        };
        let mut fetched = Vec::new();
        cache.sync(vec![spec("a")], |s, e| fetched.push((s.url, e)));
        assert!(fetched.is_empty());
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn clear_drops_everything() {
        let mut cache = enabled_cache();
        let mut fetched = Vec::new();
        cache.sync(vec![spec("a")], |s, e| fetched.push((s.url, e)));
        let (url, epoch) = fetched[0].clone();
        cache.on_event(Event::Fetched {
            url,
            epoch,
            result: Ok(item(b"data")),
        });
        cache.clear();
        assert!(cache.get("a").is_none());
    }
}
