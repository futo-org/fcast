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
    time::Duration,
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

/// Whether the manifest carrying an embedded text AdaptationSet exists.
pub fn has_embedded_text(root: &Path) -> bool {
    root.join("vod/manifest-text.mpd").is_file()
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
    requests: Arc<Mutex<Vec<String>>>,
}

impl FileServer {
    pub fn serve(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fixture server");
        let addr = listener.local_addr().expect("fixture server address");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let accept = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let root = root.clone();
                let log = log.clone();
                // Detached: a client that disappears mid-segment (teardown,
                // a flushing seek) must not hold the accept loop up.
                thread::spawn(move || {
                    let _ = serve_connection(stream, &root, &log);
                });
            }
        });
        Self {
            addr,
            stop,
            accept: Some(accept),
            requests,
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
            .filter(|target| target.contains(needle))
            .count()
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
    log: &Arc<Mutex<Vec<String>>>,
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

        log.lock().expect("request log").push(target.clone());
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
