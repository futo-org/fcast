//! Fixture generation and a file server for `tests/dash_testbed.rs`.
//!
//! Everything here is std-only on purpose: the crate's dev-dependencies carry
//! no HTTP server and this needs to serve a few megabytes of segments to
//! `souphttpsrc`, which is a hundred lines of `TcpListener`.

#![allow(dead_code)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// The generated DASH tree, built on first use.
///
/// `FCAST_DASH_FIXTURES` overrides the location. The default is
/// `<workspace>/target/dash-fixtures`, which is gitignored.
pub fn fixtures() -> PathBuf {
    // Once per process, not once per test. The generator starts by wiping the
    // tree, so several tests entering it at once (the default `cargo test`
    // thread pool) would read a directory another one is rebuilding.
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(generate).clone()
}

fn generate() -> PathBuf {
    let root = std::env::var_os("FCAST_DASH_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/dash-fixtures")
                .to_path_buf()
        });
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/gen-dash.sh");
    // A no-op when the recipe stamp already matches, so a warm tree costs one
    // process spawn.
    let out = Command::new("bash")
        .arg(&script)
        .arg(&root)
        .output()
        .unwrap_or_else(|error| panic!("running {}: {error}", script.display()));
    assert!(
        out.status.success(),
        "{} failed: {}",
        script.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        root.join("vod/manifest.mpd").is_file(),
        "no manifest under {}",
        root.display()
    );
    root
}

/// The generated HLS tree for `tests/regression_hls_codec_family.rs`, built on
/// first use. `FCAST_HLS_FIXTURES` overrides the gitignored default location.
pub fn hls_fixtures() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::var_os("FCAST_HLS_FIXTURES")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/hls-fixtures")
                    .to_path_buf()
            });
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/gen-hls.sh");
        let out = Command::new("bash")
            .arg(&script)
            .arg(&root)
            .output()
            .unwrap_or_else(|error| panic!("running {}: {error}", script.display()));
        assert!(
            out.status.success(),
            "{} failed: {}",
            script.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            root.join("master.m3u8").is_file(),
            "no master playlist under {}",
            root.display()
        );
        root
    })
    .clone()
}

/// Whether the manifest carrying an embedded text AdaptationSet exists.
pub fn has_embedded_text(root: &Path) -> bool {
    root.join("vod/manifest-text.mpd").is_file()
}

/// Whether the SEGMENTED embedded-text manifest exists (SegmentTemplate over
/// per-segment WebVTT).
///
/// The difference from [`has_embedded_text`] is the whole point of the
/// variant: an unsegmented whole-period Representation is pushed ONCE, so the
/// demuxer is idle for the rest of the item and neither a re-select's delivery
/// nor a mid-surgery discard can be observed against it. See the long note in
/// `gen-dash.sh`.
pub fn has_segmented_text(root: &Path) -> bool {
    root.join("vod/manifest-text-seg.mpd").is_file() && root.join("vod/text-00001.vtt").is_file()
}

/// A blocking HTTP/1.1 file server on an ephemeral loopback port.
///
/// Keep-alive and byte ranges are both supported: `souphttpsrc` asks for the
/// manifest with a plain GET and the segments over a reused connection, and
/// `urisourcebin` probes with a range request before it commits to pull mode.
pub struct FileServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    requests: Arc<Mutex<Vec<(String, Instant)>>>,
    delays: Delays,
}

/// The request log: every target this server was asked for, with the instant
/// the request line was read. The timestamp is what makes a fetch ATTRIBUTABLE
/// "the demuxer asked for the subtitle N seconds after load" is a statement
/// about the demuxer's scheduling, and it cannot be made from a count.
type RequestLog = Arc<Mutex<Vec<(String, Instant)>>>;

/// Per-path response delays, keyed by the same substring match [`
/// FileServer::fetches`] uses (see [`FileServer::delay_path`]).
type Delays = Arc<Mutex<Vec<(String, Duration)>>>;

impl FileServer {
    pub fn serve(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fixture server");
        let addr = listener.local_addr().expect("fixture server address");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let delays: Delays = Arc::new(Mutex::new(Vec::new()));
        let schedule = delays.clone();
        let accept = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let root = root.clone();
                let log = log.clone();
                let schedule = schedule.clone();
                // Detached: a client that disappears mid-segment (teardown,
                // a flushing seek) must not hold the accept loop up.
                thread::spawn(move || {
                    let _ = serve_connection(stream, &root, &log, &schedule);
                });
            }
        });
        Self {
            addr,
            stop,
            accept: Some(accept),
            requests,
            delays,
        }
    }

    pub fn url(&self, rel: &str) -> String {
        format!("http://{}/{}", self.addr, rel.trim_start_matches('/'))
    }

    /// How many times `needle` was fetched. Tells a subtitle that never
    /// rendered because nothing re-requested it from one that was fetched and
    /// then dropped somewhere in the pipeline.
    pub fn fetches(&self, needle: &str) -> usize {
        self.requests
            .lock()
            .expect("request log")
            .iter()
            .filter(|(target, _)| target.contains(needle))
            .count()
    }

    /// When the FIRST request whose target contains `needle` was read off the
    /// socket, or `None` if it was never asked for.
    ///
    /// # What this is for
    ///
    /// Attribution. A subtitle that renders late is either late because the
    /// bytes arrived late or late because nothing downstream of the bytes ran,
    /// and the two have completely different owners. This timestamp splits
    /// them: it is the moment the DEMUXER decided it wanted the track, taken
    /// on the server side where no pipeline scheduling can colour it.
    ///
    /// Read before the response hold in [`FileServer::delay_path`], so a
    /// delayed path still reports when it was ASKED for rather than when it
    /// was served.
    pub fn first_fetch_at(&self, needle: &str) -> Option<Instant> {
        self.requests
            .lock()
            .expect("request log")
            .iter()
            .find(|(target, _)| target.contains(needle))
            .map(|(_, at)| *at)
    }

    /// Every request in arrival order, as `(target, seconds since `origin`)`.
    /// The whole download schedule in one value, for a probe that has to say
    /// WHERE in the A/V fetch sequence the subtitle landed.
    pub fn timeline(&self, origin: Instant) -> Vec<(String, f64)> {
        self.requests
            .lock()
            .expect("request log")
            .iter()
            .map(|(target, at)| {
                (
                    target.clone(),
                    at.saturating_duration_since(origin).as_secs_f64(),
                )
            })
            .collect()
    }

    /// Hold the RESPONSE to any request whose target contains `needle` for
    /// `delay` before serving it. The request is logged when it arrives, so
    /// [`FileServer::fetches`] still counts it immediately.
    ///
    /// # What this is for
    ///
    /// Scheduling, not bandwidth. A DASH text AdaptationSet with a bare
    /// `<BaseURL>` is one whole-period Representation: the demuxer fetches it
    /// ONCE and pushes the entire track in a single push. Which second of the
    /// item that push lands in is otherwise decided by the server being
    /// instant and the fixture being 4 KB, it lands during bring-up, before
    /// the driver can be doing anything else. Delaying just that one response
    /// puts the push where the field has it: minutes-scale behind nothing,
    /// seconds behind A/V, arriving at a joined and healthy branch that is
    /// already playing. It is the only knob that makes "a mid-play surgery
    /// races the text track's one and only push" a staged event rather than a
    /// coincidence.
    ///
    /// Substring matching, deliberately the same rule as `fetches`, so a test
    /// names the fixture the same way in both.
    ///
    /// SAFE UNDER LOAD: every connection is served on its own detached thread,
    /// so a held response cannot stall the accept loop, the manifest, or any
    /// A/V segment.
    pub fn delay_path(&self, needle: &str, delay: Duration) {
        self.delays
            .lock()
            .expect("the delay schedule")
            .push((needle.to_owned(), delay));
    }
}

impl Drop for FileServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop, which is parked in `incoming()`.
        let _ = TcpStream::connect(self.addr);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn serve_connection(
    stream: TcpStream,
    root: &Path,
    log: &RequestLog,
    delays: &Delays,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut request = String::new();
        if reader.read_line(&mut request)? == 0 {
            return Ok(());
        }
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let target = parts.next().unwrap_or("/").to_owned();

        let mut range = None;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = match header.split_once(':') {
                Some(split) => split,
                None => continue,
            };
            if name.eq_ignore_ascii_case("range") {
                range = parse_range(value.trim());
            }
        }

        log.lock()
            .expect("request log")
            .push((target.clone(), Instant::now()));
        // The scheduled hold (see `FileServer::delay_path`). Logged before,
        // slept after: a test that waits for the fetch to be OBSERVED and then
        // acts is timing itself against the request, which is the event it can
        // actually see.
        let hold = delays
            .lock()
            .expect("the delay schedule")
            .iter()
            .find(|(needle, _)| target.contains(needle.as_str()))
            .map(|(_, delay)| *delay);
        if let Some(hold) = hold {
            thread::sleep(hold);
        }
        if method != "GET" && method != "HEAD" {
            // Ends the connection rather than looping: an unread request body
            // would be misparsed as the next request line.
            return respond_status(&mut writer, 405, "Method Not Allowed");
        }
        match resolve(root, &target) {
            Some(path) => send_file(&mut writer, &path, range, method == "HEAD")?,
            None => respond_status(&mut writer, 404, "Not Found")?,
        }
    }
}

/// `bytes=start-` / `bytes=start-end`. Suffix ranges are not used by any
/// GStreamer source here and are refused as a whole-body read.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = end.trim();
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((start, end))
}

/// Map a request target onto a file under `root`, refusing anything that
/// escapes it.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let mut resolved = root.to_path_buf();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\\') {
            return None;
        }
        resolved.push(segment);
    }
    resolved.is_file().then_some(resolved)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mpd") => "application/dash+xml",
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("ts") => "video/mp2t",
        Some("m4s") | Some("mp4") | Some("m4a") | Some("m4v") => "video/mp4",
        Some("vtt") => "text/vtt",
        Some("srt") => "application/x-subrip",
        _ => "application/octet-stream",
    }
}

fn send_file(
    writer: &mut TcpStream,
    path: &Path,
    range: Option<(u64, Option<u64>)>,
    head_only: bool,
) -> std::io::Result<()> {
    let mut file = fs::File::open(path)?;
    let total = file.metadata()?.len();
    let (status, reason, start, end) = match range {
        Some((start, end)) if start < total => {
            let end = end.unwrap_or(total - 1).min(total - 1);
            (206, "Partial Content", start, end)
        }
        Some(_) => {
            // Unsatisfiable: say so rather than hand back a short body.
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\n\
                 Content-Length: 0\r\n\r\n"
            );
            return writer.write_all(head.as_bytes());
        }
        None => (200, "OK", 0, total.saturating_sub(1)),
    };
    let length = if total == 0 { 0 } else { end - start + 1 };

    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nContent-Length: {length}\r\n\
         Accept-Ranges: bytes\r\n",
        content_type(path)
    );
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    if head_only || length == 0 {
        return Ok(());
    }

    file.seek(SeekFrom::Start(start))?;
    let mut remaining = length;
    let mut buffer = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        // A client that went away mid-body is normal here (teardown, a
        // flushing seek), so end the connection instead of failing.
        if writer.write_all(&buffer[..read]).is_err() {
            let _ = writer.shutdown(Shutdown::Both);
            return Ok(());
        }
        remaining -= read as u64;
    }
    writer.flush()
}

fn respond_status(writer: &mut TcpStream, status: u16, reason: &str) -> std::io::Result<()> {
    writer.write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n").as_bytes())
}
