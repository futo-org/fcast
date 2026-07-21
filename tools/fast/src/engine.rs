use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use fcast_protocol::{
    HEADER_LENGTH, Opcode, PacketReader, PlaybackErrorMessage, PlaybackState, ReadResult,
    SeekMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage, companion,
    sender::{CertVerifier, NetworkStream},
    v2,
    v3::{self, EventSubscribeObject, InitialSenderMessage, PlaylistContent},
    v4::{self, MAX_PACKET_SIZE},
};
use file_server::FileServer;
use rustls_pki_types::ServerName;
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, rustls};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{PlaylistItem, QueueMutationKind, Receive, Send as Op, Step, TrackKind};

const IDLE_TIMEOUT: Duration = Duration::from_secs(4);
// The cap on how long we wait for an expected event (a track-change confirm, a
// state settle) before declaring failure. It must outlast the receiver's OWN
// worst-case recovery, or a correct-but-slow settle reads as a false failure.
//
// The receiver holds a flushing seek's text-restore until the pipeline reports
// steady PLAYING (linking text into subtitleoverlay mid-preroll livelocks), by
// polling every 500ms up to 20 times = ~10s, and only THEN applies a deferred
// track change (another select_streams + StreamsSelected round-trip). Under a
// slow re-preroll, e.g. the FAST shuffle oversubscribing the GPU, which the
// video sink reports as "computer is too slow", that envelope runs ~13-15s.
// 8s gave up mid-settle-poll and failed cases the receiver was recovering from
// cleanly (audio_track_switch_v4, rapid_track_changes_v4). A genuine wedge
// never recovers, so it still fails here, just 8s later.
const MAX_SETTLE: Duration = Duration::from_secs(16);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const TEARDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(5);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug)]
struct Packet {
    opcode: Opcode,
    body: Option<Vec<u8>>,
}

pub struct Connection {
    stream: NetworkStream,
    reader: PacketReader,
    read_buf: Box<[u8]>,
    parsed: std::collections::VecDeque<Packet>,
    local_addr: IpAddr,
    peer_ip: IpAddr,
}

impl Connection {
    pub async fn connect(addr: &SocketAddr) -> Result<Self> {
        let tcp = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to receiver at {addr}"))?;
        let _ = tcp.set_nodelay(true);
        let local_addr = tcp.local_addr()?.ip();
        let read_buf = vec![0u8; 16 * 1024].into_boxed_slice();
        let reader = PacketReader::new(MAX_PACKET_SIZE, read_buf.len());
        let stream = NetworkStream::new(tcp).context("wrapping TCP stream")?;
        Ok(Self {
            stream,
            reader,
            read_buf,
            parsed: std::collections::VecDeque::new(),
            local_addr,
            peer_ip: addr.ip(),
        })
    }

    fn local_ip(&self) -> IpAddr {
        self.local_addr
    }

    async fn upgrade_tls(&mut self, fingerprint: Option<&[u8]>) -> Result<()> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = match fingerprint {
            Some(fp) => CertVerifier::new(fp.to_vec(), provider.clone()),
            None => CertVerifier::new_no_fingerprint_check(provider.clone()),
        };
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .context("selecting TLS protocol versions")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::from(self.peer_ip);
        self.stream
            .upgrade(&connector, server_name, TLS_HANDSHAKE_TIMEOUT)
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::TimedOut {
                    anyhow!(
                        "TLS handshake did not complete within {TLS_HANDSHAKE_TIMEOUT:?} \
                         (receiver accepted the TCP connection but never drove the handshake \
                         - likely wedged by a prior test)"
                    )
                } else {
                    anyhow::Error::new(err).context("upgrading the connection to TLS")
                }
            })?;
        debug!("connection upgraded to TLS 1.3");
        Ok(())
    }

    async fn write(&mut self, opcode: Opcode, body: Option<&[u8]>) -> Result<()> {
        let body_len = body.map_or(0, <[u8]>::len);
        let size = (body_len + 1) as u32;
        let mut header = [0u8; HEADER_LENGTH];
        header[..4].copy_from_slice(&size.to_le_bytes());
        header[4] = opcode as u8;
        self.stream
            .write_all(&header)
            .await
            .context("writing packet header")?;
        if let Some(body) = body {
            self.stream
                .write_all(body)
                .await
                .context("writing packet body")?;
            debug!(?opcode, body = %format_body(opcode, body), "SEND");
        } else {
            debug!(?opcode, "SEND");
        }
        self.stream.flush().await.context("flushing packet")?;
        Ok(())
    }

    async fn write_raw(&mut self, opcode: u8, body: Option<&[u8]>) -> Result<()> {
        let body_len = body.map_or(0, <[u8]>::len);
        let size = (body_len + 1) as u32;
        let mut header = [0u8; HEADER_LENGTH];
        header[..4].copy_from_slice(&size.to_le_bytes());
        header[4] = opcode;
        self.stream
            .write_all(&header)
            .await
            .context("writing packet header")?;
        if let Some(body) = body {
            self.stream
                .write_all(body)
                .await
                .context("writing packet body")?;
        }
        debug!(opcode, "SEND raw");
        self.stream.flush().await.context("flushing packet")?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Packet> {
        loop {
            if let Some(packet) = self.parsed.pop_front() {
                return Ok(packet);
            }

            let n = self
                .stream
                .read(&mut self.read_buf)
                .await
                .context("reading from receiver")?;
            ensure!(n > 0, "connection closed by receiver");

            self.reader
                .push_data(&self.read_buf[..n])
                .map_err(|_| anyhow!("packet reader buffer overrun"))?;

            loop {
                let packet = match self.reader.get_packet() {
                    ReadResult::NeedData => break,
                    ReadResult::PacketTooLarge(size) => {
                        bail!("receiver sent an oversized packet ({size} bytes)")
                    }
                    ReadResult::Read(bytes) => match bytes.split_first() {
                        None => {
                            warn!("ignoring empty packet");
                            continue;
                        }
                        Some((&op, body)) => Packet {
                            opcode: Opcode::try_from(op).map_err(|e| anyhow!("{e}"))?,
                            body: (!body.is_empty()).then(|| body.to_vec()),
                        },
                    },
                };
                self.parsed.push_back(packet);
            }
        }
    }
}

type FileEntry = (String, &'static str, Option<HashMap<String, String>>);

#[derive(Default)]
struct Expectations {
    waiting_opcode: Option<Opcode>,
    volume: Option<(f64, u64)>,
    play_update: Option<v3::PlayMessage>,
    pause: bool,
    resume: bool,
    media_item_start: Option<v3::MediaItem>,
    media_item_changed: Option<v3::MediaItem>,
    media_item_end: Option<v3::MediaItem>,
    receiver_intro: bool,
    volume_v4: Option<f32>,
    speed_v4: Option<f32>,
    state_v4: Option<v4::flat::PlaybackState>,
    companion_hello: bool,
    companion_served: Option<u32>,
    error: Option<v4::flat::ErrorKind>,
    /// Satisfied by a v4 progress update whose position is at least this many seconds.
    progress_v4_at_least: Option<f64>,
    /// The next v4 progress update with a non-zero position must be at least
    /// this many seconds, a lower one fails the test immediately.
    next_progress_floor: Option<f64>,
    /// Waiting for a `TracksAvailable` advertising at least this many tracks
    /// of each kind (indexed by `TrackKind`).
    await_tracks: Option<[usize; 3]>,
    /// Waiting for a relayed `ChangeTrack` per kind (indexed by `TrackKind`)
    /// whose id equals the inner value. That inner option distinguishes a
    /// specific track (`Some`) from the kind having been disabled (`None`).
    change_track: [Option<Option<u32>>; 3],
}

/// Display names per `TrackKind` slot.
const KIND_NAMES: [&str; 3] = ["Video", "Audio", "Subtitle"];

fn track_kind_to_type(kind: TrackKind) -> v4::flat::MediaTrackType {
    match kind {
        TrackKind::Video => v4::flat::MediaTrackType::Video,
        TrackKind::Audio => v4::flat::MediaTrackType::Audio,
        TrackKind::Subtitle => v4::flat::MediaTrackType::Subtitle,
    }
}

impl Expectations {
    fn pending(&self) -> bool {
        self.waiting_opcode.is_some()
            || self.volume.is_some()
            || self.play_update.is_some()
            || self.pause
            || self.resume
            || self.media_item_start.is_some()
            || self.media_item_changed.is_some()
            || self.media_item_end.is_some()
            || self.receiver_intro
            || self.volume_v4.is_some()
            || self.speed_v4.is_some()
            || self.state_v4.is_some()
            || self.companion_hello
            || self.companion_served.is_some()
            || self.error.is_some()
            || self.progress_v4_at_least.is_some()
            || self.next_progress_floor.is_some()
            || self.await_tracks.is_some()
            || self.change_track.iter().any(|c| c.is_some())
    }

    fn describe(&self) -> String {
        let mut out = Vec::new();
        if let Some(op) = self.waiting_opcode {
            out.push(format!("{op:?}"));
        }
        if self.volume.is_some() {
            out.push("VolumeUpdate".into());
        }
        if self.play_update.is_some() {
            out.push("PlayUpdate".into());
        }
        if self.pause {
            out.push("PlaybackUpdate(Paused)".into());
        }
        if self.resume {
            out.push("PlaybackUpdate(Playing)".into());
        }
        if self.media_item_start.is_some() {
            out.push("Event(MediaItemStart)".into());
        }
        if self.media_item_changed.is_some() {
            out.push("Event(MediaItemChanged)".into());
        }
        if self.media_item_end.is_some() {
            out.push("Event(MediaItemEnd)".into());
        }
        if self.receiver_intro {
            out.push("ReceiverIntroduction".into());
        }
        if let Some(v) = self.volume_v4 {
            out.push(format!("VolumeChanged({v})"));
        }
        if let Some(s) = self.speed_v4 {
            out.push(format!("SpeedChanged({s})"));
        }
        if let Some(state) = self.state_v4 {
            out.push(format!("PlaybackStateChanged({state:?})"));
        }
        if self.companion_hello {
            out.push("CompanionHelloResponse".into());
        }
        if let Some(id) = self.companion_served {
            out.push(format!("CompanionResource({id})"));
        }
        if let Some(kind) = self.error {
            out.push(format!("Error({kind:?})"));
        }
        if let Some(secs) = self.progress_v4_at_least {
            out.push(format!("ProgressV4AtLeast({secs})"));
        }
        if let Some(secs) = self.next_progress_floor {
            out.push(format!("NextProgressV4AtLeast({secs})"));
        }
        if let Some([v, a, s]) = self.await_tracks {
            out.push(format!(
                "TracksAvailable(>= {v} video, {a} audio, {s} subtitle)"
            ));
        }
        for (slot, expected) in self.change_track.iter().enumerate() {
            if let Some(expected) = expected {
                let kind = KIND_NAMES[slot];
                match expected {
                    Some(id) => out.push(format!("ChangeTrack({kind}, id={id})")),
                    None => out.push(format!("ChangeTrack({kind}, disabled)")),
                }
            }
        }
        out.join(", ")
    }
}

pub struct Engine<'a> {
    conn: Connection,
    file_server: &'a FileServer,
    sample_media: &'a Path,
    local_ip: IpAddr,
    file_urls: HashMap<u32, FileEntry>,
    subscriptions: HashSet<EventSubscribeObject>,
    expect: Expectations,
    sleep_until: Option<Instant>,
    playlist: Option<PlaylistContent>,
    version: u64,
    fingerprint: Option<Vec<u8>>,
    companion_provider_id: Option<u16>,
    companion_resources: HashMap<u32, CompanionResource>,
    /// Resource ids the provider should report as not found. Maps to the
    /// content type to advertise in the resource-info response, so the receiver
    /// gets past the info stage and then hits the not-found data response.
    companion_missing: HashMap<u32, String>,
    progress_times: Vec<Instant>,
    addr: SocketAddr,
    second: Option<Connection>,
    tls_upgraded: bool,
    /// Ids of the tracks advertised by the most recent `TracksAvailable`,
    /// per kind (indexed by `TrackKind`).
    track_ids: [Vec<u32>; 3],
    /// The most recently relayed `ChangeTrack` id per kind (indexed by
    /// `TrackKind`). `None` = never relayed; `Some(None)` = kind disabled.
    last_track_state: [Option<Option<u32>>; 3],
    /// `track_ids`, but for advertisements broadcast to the second sender.
    /// Only current up to the last packet a second-sender step consumed (the
    /// second connection is not read while `settle` drives the main one).
    second_track_ids: [Vec<u32>; 3],
    /// `last_track_state`, but for `ChangeTrack`s relayed to the second sender.
    second_last_track_state: [Option<Option<u32>>; 3],
    /// The most recently relayed playback state: v4 `PlaybackStateChanged`,
    /// or the state of a legacy `PlaybackUpdate` mapped onto the v4 enum
    /// (so `AwaitPlaybackState` works on every protocol version).
    last_state_v4: Option<v4::flat::PlaybackState>,
}

struct CompanionResource {
    content_type: String,
    data: Vec<u8>,
}

enum FlatAction {
    None,
    ResourceInfo {
        request_id: u32,
        resource_id: u32,
    },
    Resource {
        request_id: u32,
        resource_id: u32,
        read_head: Option<(u64, u64)>,
    },
}

impl<'a> Engine<'a> {
    pub async fn connect(
        addr: &SocketAddr,
        file_server: &'a FileServer,
        sample_media: &'a Path,
        fingerprint: Option<Vec<u8>>,
    ) -> Result<Self> {
        let conn = Connection::connect(addr).await?;
        let local_ip = conn.local_ip();
        Ok(Self {
            conn,
            file_server,
            sample_media,
            local_ip,
            file_urls: HashMap::new(),
            subscriptions: HashSet::new(),
            expect: Expectations::default(),
            sleep_until: None,
            playlist: None,
            version: 0,
            fingerprint,
            companion_provider_id: None,
            companion_resources: HashMap::new(),
            companion_missing: HashMap::new(),
            progress_times: Vec::new(),
            addr: *addr,
            second: None,
            tls_upgraded: false,
            track_ids: Default::default(),
            last_track_state: [None; 3],
            second_track_ids: Default::default(),
            second_last_track_state: [None; 3],
            last_state_v4: None,
        })
    }

    pub async fn run(mut self, steps: &[Step]) -> Result<()> {
        let result = self.run_steps(steps).await;
        // Best-effort: stop playback on the way out so a failed or short test
        // never leaves the receiver playing into the next one.
        self.best_effort_stop().await;
        result
    }

    async fn run_steps(&mut self, steps: &[Step]) -> Result<()> {
        for (idx, step) in steps.iter().enumerate() {
            self.exec_step(step)
                .await
                .with_context(|| format!("executing step {idx} ({step:?})"))?;
            self.settle()
                .await
                .with_context(|| format!("settling after step {idx} ({step:?})"))?;
        }

        ensure!(
            !self.expect.pending(),
            "test finished with unmet expectations: {}",
            self.expect.describe()
        );
        Ok(())
    }

    async fn best_effort_stop(&mut self) {
        let stop_sent = match self.version {
            // The v4 connection is only writable once the TLS upgrade succeeded.
            4 if self.tls_upgraded => {
                let msg = v4::MessageBuilder::new().stop_playback();
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await.is_ok()
            }
            1..=3 | 5.. => self.conn.write(Opcode::Stop, None).await.is_ok(),
            _ => false,
        };
        if stop_sent {
            self.confirm_stop_delivery().await;
        }
    }

    /// Barrier: make sure the receiver has actually *read* the teardown stop
    /// before the connection is dropped.
    ///
    /// Dropping the connection right after the stop write is not enough. The
    /// receiver broadcasts updates until the very end, so our socket usually
    /// still holds unread data when it closes, turning the close into a TCP
    /// RST, and an RST discards whatever the receiver has not yet read from
    /// its end, the just-written stop included. The receiver then keeps
    /// playing the finished case's media into the next one, whose
    /// `file_server.clear()` turns the orphaned load's next range request
    /// into a "Resource not found" broadcast that fails an innocent test.
    ///
    /// Sessions answer `Ping` in order with the rest of the stream (on every
    /// protocol version), so one ping/pong round-trip after the stop proves
    /// the stop was consumed and handed to the receiver's application loop,
    /// which processes it before it can even register the next case's
    /// connection.
    async fn confirm_stop_delivery(&mut self) {
        if self.conn.write(Opcode::Ping, None).await.is_err() {
            return;
        }
        let deadline = Instant::now() + TEARDOWN_ACK_TIMEOUT;
        loop {
            let now = Instant::now();
            if now >= deadline {
                warn!(
                    "receiver did not acknowledge the teardown stop within \
                     {TEARDOWN_ACK_TIMEOUT:?}; its playback may leak into the next case"
                );
                return;
            }
            match tokio::time::timeout(deadline - now, self.conn.recv()).await {
                Ok(Ok(Packet {
                    opcode: Opcode::Pong,
                    ..
                })) => return,
                Ok(Ok(Packet {
                    opcode: Opcode::Ping,
                    ..
                })) => {
                    let _ = self.conn.write(Opcode::Pong, None).await;
                }
                // Anything else is a late broadcast from the case that just
                // finished (playback updates, errors from the load being torn
                // down), drain it and keep waiting for our pong.
                Ok(Ok(packet)) => debug!(opcode = ?packet.opcode, "drained during teardown"),
                Ok(Err(err)) => {
                    debug!(%err, "connection closed while confirming teardown stop");
                    return;
                }
                Err(_elapsed) => {} // the loop re-checks the deadline
            }
        }
    }

    async fn settle(&mut self) -> Result<()> {
        let started = Instant::now();
        loop {
            let now = Instant::now();
            let sleeping = self.sleep_until.is_some_and(|d| d > now);
            if !sleeping && !self.expect.pending() {
                return Ok(());
            }
            ensure!(
                now.duration_since(started) < MAX_SETTLE,
                "gave up after {MAX_SETTLE:?} still waiting for: {}",
                self.expect.describe()
            );

            let idle_deadline = now + IDLE_TIMEOUT;
            let wake = match self.sleep_until {
                Some(d) if sleeping => d.min(idle_deadline),
                _ => idle_deadline,
            };

            tokio::select! {
                packet = self.conn.recv() => {
                    self.handle_incoming(packet?).await?;
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {
                    if self.sleep_until.is_some_and(|d| d <= Instant::now()) {
                        self.sleep_until = None;
                    }
                    // An idle connection is not failure by itself: a reload
                    // dance can keep the receiver silent for several seconds
                    // (teardown tails stretch on aged instances), with the
                    // expected packet arriving just after. MAX_SETTLE above
                    // bounds the total wait.
                }
            }
        }
    }

    async fn handle_incoming(&mut self, packet: Packet) -> Result<()> {
        let Packet { opcode, body } = packet;
        if let Some(body) = &body {
            debug!(?opcode, body = %format_body(opcode, body), "RECV");
        } else {
            debug!(?opcode, "RECV");
        }

        match opcode {
            Opcode::Ping => self.conn.write(Opcode::Pong, None).await?,
            Opcode::PlaybackError => {
                let msg: PlaybackErrorMessage = parse(opcode, body.as_deref())?;
                bail!("receiver reported a playback error: {}", msg.message);
            }
            Opcode::PlaybackUpdate => self.on_playback_update(body.as_deref())?,
            Opcode::VolumeUpdate => self.on_volume_update(body.as_deref())?,
            Opcode::PlayUpdate => self.on_play_update(body.as_deref())?,
            Opcode::Event => self.on_event(body.as_deref())?,
            Opcode::Flatbuf => self.on_flatbuf(body.as_deref()).await?,
            _ => {}
        }

        if self.expect.waiting_opcode == Some(opcode) {
            self.validate_awaited(opcode, body.as_deref())?;
            self.expect.waiting_opcode = None;
            debug!(?opcode, "awaited packet received");
        }

        Ok(())
    }

    fn on_playback_update(&mut self, body: Option<&[u8]>) -> Result<()> {
        let msg: v3::PlaybackUpdateMessage = parse(Opcode::PlaybackUpdate, body)?;
        self.last_state_v4 = Some(match msg.state {
            PlaybackState::Idle => v4::flat::PlaybackState::Idle,
            PlaybackState::Playing => v4::flat::PlaybackState::Playing,
            PlaybackState::Paused => v4::flat::PlaybackState::Paused,
        });
        if self.expect.pause && msg.state == PlaybackState::Paused {
            self.expect.pause = false;
            info!("paused state confirmed");
        } else if self.expect.resume && msg.state == PlaybackState::Playing {
            self.expect.resume = false;
            info!("playing state confirmed");
        }
        Ok(())
    }

    fn on_volume_update(&mut self, body: Option<&[u8]>) -> Result<()> {
        let msg: v2::VolumeUpdateMessage = parse(Opcode::VolumeUpdate, body)?;
        if let Some((target, sent_at)) = self.expect.volume {
            if msg.generation_time >= sent_at {
                if (msg.volume - target).abs() <= 0.001 {
                    self.expect.volume = None;
                    info!("volume confirmed at {target}");
                } else {
                    debug!(got = msg.volume, want = target, "ignoring interim volume");
                }
            }
        }
        Ok(())
    }

    fn on_play_update(&mut self, body: Option<&[u8]>) -> Result<()> {
        let Some(expected) = self.expect.play_update.as_ref() else {
            return Ok(());
        };
        let msg: v3::PlayUpdateMessage = parse(Opcode::PlayUpdate, body)?;
        let got = msg
            .play_data
            .ok_or_else(|| anyhow!("play update is missing playData"))?;
        // PlayUpdate is broadcast to every connected sender, and the receiver
        // keeps its current media until superseded. A previous test's media can
        // therefore be broadcast onto our connection. Only assert on the update
        // that echoes the URL we just sent, ignore foreign ones and keep waiting.
        if got.url != expected.url {
            debug!(?got, "ignoring play update for a different url");
            return Ok(());
        }
        let expected = self.expect.play_update.take().unwrap();
        ensure!(
            got == expected,
            "play update did not echo what we sent:\n  expected: {expected:?}\n  got:      {got:?}"
        );
        info!("play update confirmed");
        Ok(())
    }

    fn on_event(&mut self, body: Option<&[u8]>) -> Result<()> {
        let msg: v3::EventMessage = parse(Opcode::Event, body)?;
        let v3::EventObject::MediaItem { variant, item } = msg.event else {
            return Ok(());
        };
        let (slot, label) = match variant {
            v3::EventType::MediaItemStart => (&mut self.expect.media_item_start, "MediaItemStart"),
            v3::EventType::MediaItemChange => {
                (&mut self.expect.media_item_changed, "MediaItemChanged")
            }
            v3::EventType::MediaItemEnd => (&mut self.expect.media_item_end, "MediaItemEnd"),
            _ => return Ok(()),
        };
        if let Some(expected) = slot.take() {
            ensure!(
                item == expected,
                "{label} event did not match:\n  expected: {expected:?}\n  got:      {item:?}"
            );
            info!("{label} event confirmed");
        }
        Ok(())
    }

    /// Handle an incoming v4 `Flatbuf` packet, resolving any v4 expectations.
    async fn on_flatbuf(&mut self, body: Option<&[u8]>) -> Result<()> {
        use v4::flat::Message;

        let body = body.ok_or_else(|| anyhow!("Flatbuf message is missing its body"))?;

        let action = {
            let packet =
                v4::flat::root_as_packet(body).map_err(|e| anyhow!("invalid flatbuffer: {e}"))?;

            match packet.payload_type() {
                Message::ProgressChanged => {
                    let progress = packet
                        .payload_as_progress_changed()
                        .ok_or_else(|| anyhow!("malformed ProgressChanged"))?;
                    self.progress_times.push(Instant::now());
                    let pos_secs = progress
                        .position()
                        .map(|t| t.micros() as f64 / 1_000_000.0)
                        .unwrap_or(0.0);
                    if let Some(threshold) = self.expect.progress_v4_at_least {
                        if pos_secs >= threshold {
                            self.expect.progress_v4_at_least = None;
                            info!("v4 progress position {pos_secs:.3}s reached {threshold}s");
                        } else {
                            debug!(pos_secs, threshold, "ignoring interim v4 progress position");
                        }
                    }
                    // Small positions are ignored: the receiver reports ~0
                    // while a (re)load is in flight, and up to ~1s can
                    // elapse between the reload settling and the
                    // position-restore seek (subtitle-branch settle delay),
                    // none of which says anything about where playback
                    // resumes. Playback that genuinely reset to 0 still
                    // trips the floor as soon as its position crosses this
                    // threshold, so cases must use a floor comfortably above
                    // it.
                    if let Some(floor) = self.expect.next_progress_floor
                        && pos_secs > 1.5
                    {
                        ensure!(
                            pos_secs >= floor,
                            "first non-zero progress was {pos_secs:.2}s, expected at least \
                             {floor}s (playback position was not preserved)"
                        );
                        self.expect.next_progress_floor = None;
                        info!("v4 progress resumed at {pos_secs:.3}s (>= {floor}s)");
                    }
                    FlatAction::None
                }
                Message::ReceiverIntroduction => {
                    packet
                        .payload_as_receiver_introduction()
                        .ok_or_else(|| anyhow!("malformed ReceiverIntroduction"))?;
                    if self.expect.receiver_intro {
                        self.expect.receiver_intro = false;
                        info!("receiver introduction received");
                    }
                    FlatAction::None
                }
                Message::VolumeChanged => {
                    let got = packet
                        .payload_as_volume_changed()
                        .ok_or_else(|| anyhow!("malformed VolumeChanged"))?
                        .volume();
                    if let Some(target) = self.expect.volume_v4 {
                        if (got - target).abs() <= 0.001 {
                            self.expect.volume_v4 = None;
                            info!("v4 volume confirmed at {target}");
                        } else {
                            debug!(got, want = target, "ignoring interim v4 volume");
                        }
                    }
                    FlatAction::None
                }
                Message::SpeedChanged => {
                    let got = packet
                        .payload_as_speed_changed()
                        .ok_or_else(|| anyhow!("malformed SpeedChanged"))?
                        .speed();
                    if let Some(target) = self.expect.speed_v4
                        && (got - target).abs() <= 0.001
                    {
                        self.expect.speed_v4 = None;
                        info!("v4 speed confirmed at {target}");
                    }
                    FlatAction::None
                }
                Message::PlaybackStateChanged => {
                    let got = packet
                        .payload_as_playback_state_changed()
                        .ok_or_else(|| anyhow!("malformed PlaybackStateChanged"))?
                        .state();
                    self.last_state_v4 = Some(got);
                    if self.expect.state_v4 == Some(got) {
                        self.expect.state_v4 = None;
                        info!("v4 playback state confirmed: {got:?}");
                    }
                    FlatAction::None
                }
                Message::Error => {
                    let err = packet
                        .payload_as_error()
                        .ok_or_else(|| anyhow!("malformed Error"))?;
                    let kind = err.kind();
                    let packet_num = err.packet_num();
                    if self.expect.error == Some(kind) {
                        self.expect.error = None;
                        info!(?packet_num, "expected v4 error received: {kind:?}");
                    } else {
                        bail!("receiver reported a v4 error: {kind:?} (packet_num={packet_num:?})");
                    }
                    FlatAction::None
                }
                Message::TracksAvailable => {
                    let tracks = packet
                        .payload_as_tracks_available()
                        .ok_or_else(|| anyhow!("malformed TracksAvailable"))?;
                    let ids = advertised_ids(tracks);
                    debug!(?ids, "TracksAvailable track ids");
                    self.track_ids = ids;
                    self.check_await_tracks();
                    FlatAction::None
                }
                Message::ChangeTrack => {
                    let change = packet
                        .payload_as_change_track()
                        .ok_or_else(|| anyhow!("malformed ChangeTrack"))?;
                    let slot = change_track_slot(change)?;
                    let id = change.id();
                    self.last_track_state[slot] = Some(id);
                    if let Some(expected) = self.expect.change_track[slot] {
                        if id == expected {
                            self.expect.change_track[slot] = None;
                            info!(?id, kind = KIND_NAMES[slot], "track change confirmed");
                        } else {
                            debug!(
                                ?id,
                                ?expected,
                                kind = KIND_NAMES[slot],
                                "ignoring interim ChangeTrack"
                            );
                        }
                    }
                    FlatAction::None
                }
                Message::CompanionHelloResponse => {
                    let id = packet
                        .payload_as_companion_hello_response()
                        .ok_or_else(|| anyhow!("malformed CompanionHelloResponse"))?
                        .provider_id();
                    self.companion_provider_id = Some(id);
                    if self.expect.companion_hello {
                        self.expect.companion_hello = false;
                        info!("registered as companion provider {id}");
                    }
                    FlatAction::None
                }
                Message::CompanionResourceInfoRequest => {
                    let msg = packet
                        .payload_as_companion_resource_info_request()
                        .ok_or_else(|| anyhow!("malformed CompanionResourceInfoRequest"))?;
                    FlatAction::ResourceInfo {
                        request_id: msg.request_id(),
                        resource_id: msg.resource_id(),
                    }
                }
                Message::CompanionResourceRequest => {
                    let msg = packet
                        .payload_as_companion_resource_request()
                        .ok_or_else(|| anyhow!("malformed CompanionResourceRequest"))?;
                    FlatAction::Resource {
                        request_id: msg.request_id(),
                        resource_id: msg.resource_id(),
                        read_head: msg.read_head().map(|r| (r.start(), r.stop_inclusive())),
                    }
                }
                _ => FlatAction::None,
            }
        };

        match action {
            FlatAction::None => {}
            FlatAction::ResourceInfo {
                request_id,
                resource_id,
            } => {
                self.send_companion_resource_info(request_id, resource_id)
                    .await?
            }
            FlatAction::Resource {
                request_id,
                resource_id,
                read_head,
            } => {
                self.send_companion_resource(request_id, resource_id, read_head)
                    .await?
            }
        }
        Ok(())
    }

    async fn send_companion_resource_info(
        &mut self,
        request_id: u32,
        resource_id: u32,
    ) -> Result<()> {
        // A resource we will report as not found still advertises a content
        // type, so the receiver gets past the info stage before failing on the
        // actual data fetch.
        if let Some(content_type) = self.companion_missing.get(&resource_id) {
            let msg = v4::MessageBuilder::new().companion_resource_info_response(
                request_id,
                content_type,
                None,
            );
            return self.conn.write(Opcode::Flatbuf, Some(&msg)).await;
        }
        let info = self
            .companion_resources
            .get(&resource_id)
            .map(|r| (r.content_type.clone(), r.data.len() as u64));
        let msg = match info {
            Some((content_type, size)) => v4::MessageBuilder::new()
                .companion_resource_info_response(request_id, &content_type, Some(size)),
            None => v4::MessageBuilder::new().companion_resource_info_response(
                request_id,
                "application/octet-stream",
                None,
            ),
        };
        self.conn.write(Opcode::Flatbuf, Some(&msg)).await
    }

    async fn send_companion_resource(
        &mut self,
        request_id: u32,
        resource_id: u32,
        read_head: Option<(u64, u64)>,
    ) -> Result<()> {
        // Explicitly report not-found resources so the receiver surfaces a
        // ResourceNotFound error.
        if self.companion_missing.contains_key(&resource_id) {
            let body = companion::ResourceResponse {
                request_id,
                part: 1,
                total_parts: 1,
                result: companion::GetResourceResult::NotFound,
            }
            .serialize();
            return self.conn.write(Opcode::Resource, Some(&body)).await;
        }
        let Some(data) = self
            .companion_resources
            .get(&resource_id)
            .map(|r| r.data.clone())
        else {
            let body = companion::ResourceResponse {
                request_id,
                part: 1,
                total_parts: 1,
                result: companion::GetResourceResult::NotFound,
            }
            .serialize();
            return self.conn.write(Opcode::Resource, Some(&body)).await;
        };

        let end = read_head
            .map(|(_, stop_inclusive)| (stop_inclusive as usize + 1).min(data.len()))
            .unwrap_or(data.len());
        let begin = read_head
            .map(|(start, _)| (start as usize).min(end))
            .unwrap_or(0);
        let slice = &data[begin..end];

        let chunks: Vec<&[u8]> = if slice.is_empty() {
            vec![&[]]
        } else {
            slice.chunks(companion::MAX_RESOURCE_READ_SIZE).collect()
        };
        let total_parts = chunks.len() as u8;

        for (i, chunk) in chunks.iter().enumerate() {
            let header =
                companion::ResourceResponse::header_success(request_id, (i + 1) as u8, total_parts);
            let body = [header.as_slice(), chunk].concat();
            self.conn.write(Opcode::Resource, Some(&body)).await?;
        }

        if self.expect.companion_served == Some(resource_id) {
            self.expect.companion_served = None;
            info!(
                "served companion resource {resource_id} ({} bytes)",
                slice.len()
            );
        }
        Ok(())
    }

    fn validate_awaited(&self, opcode: Opcode, body: Option<&[u8]>) -> Result<()> {
        match opcode {
            Opcode::Version => {
                let _: VersionMessage = parse(opcode, body)?;
            }
            Opcode::Initial => {
                let _: v3::InitialReceiverMessage = parse(opcode, body)?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn exec_step(&mut self, step: &Step) -> Result<()> {
        match step {
            Step::Send(send) => self.exec_send(send).await?,
            Step::Receive(receive) => match receive {
                Receive::ReceiverIntroduction => self.expect.receiver_intro = true,
                Receive::Error(kind) => self.expect.error = Some(*kind),
                Receive::Version => self.expect.waiting_opcode = Some(Opcode::Version),
                Receive::Initial => self.expect.waiting_opcode = Some(Opcode::Initial),
                Receive::Pong => self.expect.waiting_opcode = Some(Opcode::Pong),
                Receive::Volume => self.expect.waiting_opcode = Some(Opcode::VolumeUpdate),
                Receive::PlaybackUpdate => {
                    self.expect.waiting_opcode = Some(Opcode::PlaybackUpdate)
                }
                Receive::VolumeChangedV4(volume) => self.expect.volume_v4 = Some(*volume as f32),
                Receive::SpeedChangedV4(speed) => self.expect.speed_v4 = Some(*speed as f32),
                Receive::ProgressV4AtLeast(secs) => self.expect.progress_v4_at_least = Some(*secs),
                Receive::NextProgressV4AtLeast(secs) => {
                    self.expect.next_progress_floor = Some(*secs)
                }
            },
            Step::ServeFile {
                path,
                id,
                mime,
                headers,
            } => self.serve_file(path, *id, mime, *headers)?,
            Step::SleepMillis(ms) => {
                self.sleep_until = Some(Instant::now() + Duration::from_millis(*ms));
            }
            Step::MeasureProgressInterval {
                expected_ms,
                tolerance_ms,
                samples,
            } => {
                self.measure_progress_interval(*expected_ms, *tolerance_ms, *samples)
                    .await?;
            }
            Step::ExpectClosed => self.expect_closed().await?,
            Step::AwaitTracks {
                video,
                audio,
                subtitle,
            } => {
                self.expect.await_tracks = Some([*video, *audio, *subtitle]);
                // The advertisement may already have arrived while settling an
                // earlier step, don't wait for a re-broadcast in that case.
                self.check_await_tracks();
            }
            Step::AssertTrackState {
                video,
                audio,
                subtitle,
            } => {
                for (kind, expected_idx) in [
                    (TrackKind::Video, video),
                    (TrackKind::Audio, audio),
                    (TrackKind::Subtitle, subtitle),
                ] {
                    let slot = kind as usize;
                    let expected = match expected_idx {
                        Some(n) => Some(self.advertised_track_id(kind, *n)?),
                        None => None,
                    };
                    let seen = self.last_track_state[slot].ok_or_else(|| {
                        anyhow!("no ChangeTrack({}) was ever relayed", KIND_NAMES[slot])
                    })?;
                    ensure!(
                        seen == expected,
                        "last relayed {} track is {seen:?}, expected {expected:?}",
                        KIND_NAMES[slot]
                    );
                }
                info!(?video, ?audio, ?subtitle, "relayed track state matches");
            }
            Step::AwaitTrackState {
                video,
                audio,
                subtitle,
            } => {
                self.await_track_state([*video, *audio, *subtitle]).await?;
            }
            Step::AssertTrackCounts {
                video,
                audio,
                subtitle,
            } => {
                let counts = [
                    self.track_ids[0].len(),
                    self.track_ids[1].len(),
                    self.track_ids[2].len(),
                ];
                ensure!(
                    counts == [*video, *audio, *subtitle],
                    "last TracksAvailable advertised {counts:?} (video/audio/subtitle) tracks, \
                     expected [{video}, {audio}, {subtitle}]"
                );
                info!(?counts, "advertised track counts match");
            }
            Step::AssertPlaybackStateV4(expected) => {
                let seen = self
                    .last_state_v4
                    .ok_or_else(|| anyhow!("no v4 PlaybackStateChanged was ever received"))?;
                ensure!(
                    seen == *expected,
                    "last relayed playback state is {seen:?}, expected {expected:?}"
                );
                info!(?expected, "relayed playback state matches");
            }
            Step::AwaitPlaybackState(expected) => {
                self.await_playback_state(*expected).await?;
            }
            Step::OpenSecondSender => self.open_second_sender().await?,
            Step::SetSecondSenderInterval { millis } => {
                let micros = Duration::from_millis(*millis).as_micros() as u64;
                let msg = v4::MessageBuilder::new()
                    .set_progress_update_interval(v4::flat::Time::new(micros));
                self.second_conn()?
                    .write(Opcode::Flatbuf, Some(&msg))
                    .await?;
            }
            Step::AwaitTracksOnSecondSender {
                video,
                audio,
                subtitle,
            } => {
                self.await_tracks_on_second_sender([*video, *audio, *subtitle])
                    .await?
            }
            Step::AddSubtitleSourceOnSecondSenderV4 {
                file_id,
                select,
                name,
            } => {
                self.add_subtitle_on_second_sender(*file_id, *select, *name)
                    .await?
            }
            Step::ChangeTrackOnSecondSender { kind, index } => {
                self.change_track_on_second_sender(*kind, *index).await?
            }
            Step::AssertTrackStateOnSecondSender {
                video,
                audio,
                subtitle,
            } => {
                // Absorb anything already queued on the second connection so
                // the assertion sees the receiver's latest relays.
                self.drain_second().await?;
                for (kind, expected_idx) in [
                    (TrackKind::Video, video),
                    (TrackKind::Audio, audio),
                    (TrackKind::Subtitle, subtitle),
                ] {
                    let slot = kind as usize;
                    let expected = match expected_idx {
                        Some(n) => Some(self.second_advertised_track_id(kind, *n)?),
                        None => None,
                    };
                    let seen = self.second_last_track_state[slot].ok_or_else(|| {
                        anyhow!(
                            "no ChangeTrack({}) was ever relayed to the second sender",
                            KIND_NAMES[slot]
                        )
                    })?;
                    ensure!(
                        seen == expected,
                        "last {} track relayed to the second sender is {seen:?}, \
                         expected {expected:?}",
                        KIND_NAMES[slot]
                    );
                }
                info!(
                    ?video,
                    ?audio,
                    ?subtitle,
                    "second sender's relayed track state matches"
                );
            }
            Step::ExpectLoadOnSecondSender => self.expect_load_on_second_sender().await?,
            Step::ExpectLoadWithMetadataOnSecondSender {
                title,
                thumbnail_url,
            } => {
                self.expect_load_with_metadata_on_second_sender(*title, *thumbnail_url)
                    .await?
            }
            Step::ExpectLoadWithExtraMetadataOnSecondSender { extra } => {
                self.expect_load_with_extra_metadata_on_second_sender(extra)
                    .await?
            }
            Step::ExpectStopOnSecondSender => {
                self.expect_flat_on_second_sender(v4::flat::Message::StopPlayback, "StopPlayback")
                    .await?
            }
            Step::ExpectVolumeOnSecondSender(volume) => {
                self.expect_volume_on_second_sender(*volume as f32).await?
            }
            Step::ExpectQueueMutationOnSecondSender(kind) => {
                let (expected, label) = match kind {
                    QueueMutationKind::Insert => (v4::flat::Message::QueueInsert, "QueueInsert"),
                    QueueMutationKind::Remove => (v4::flat::Message::QueueRemove, "QueueRemove"),
                    QueueMutationKind::Select => {
                        (v4::flat::Message::QueueItemSelected, "QueueItemSelected")
                    }
                };
                self.expect_flat_on_second_sender(expected, label).await?
            }
            Step::MeasureProgressBothSenders {
                a_expected_ms,
                b_expected_ms,
                tolerance_ms,
                samples,
            } => {
                self.measure_progress_both_senders(
                    *a_expected_ms,
                    *b_expected_ms,
                    *tolerance_ms,
                    *samples,
                )
                .await?
            }
        }
        Ok(())
    }

    async fn measure_progress_interval(
        &mut self,
        expected_ms: u64,
        tolerance_ms: u64,
        samples: usize,
    ) -> Result<()> {
        ensure!(samples >= 1, "need at least one sample");
        self.progress_times.clear();
        // Collect one extra sample so the first (possibly stale, buffered at the
        // old interval) gap can be discarded.
        let needed = samples + 2;
        let deadline =
            Instant::now() + Duration::from_millis(expected_ms * (samples as u64 + 3) + 3000);

        while self.progress_times.len() < needed {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "timed out collecting progress updates: got {} of {needed} (expected ~{expected_ms}ms apart)",
                self.progress_times.len()
            );
            if let Ok(packet) = tokio::time::timeout(deadline - now, self.conn.recv()).await {
                self.handle_incoming(packet?).await?;
            }
        }

        let times = self.progress_times.clone();
        check_interval("progress", &times[1..], expected_ms, tolerance_ms)
    }

    /// Assert the receiver closes the connection (e.g. after a fatal protocol
    /// error). Keepalive pings are answered while we wait.
    async fn expect_closed(&mut self) -> Result<()> {
        let deadline = Instant::now() + IDLE_TIMEOUT;
        loop {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "expected the receiver to close the connection, but it stayed open"
            );
            match tokio::time::timeout(deadline - now, self.conn.recv()).await {
                Ok(Ok(pkt)) => {
                    if pkt.opcode == Opcode::Ping {
                        let _ = self.conn.write(Opcode::Pong, None).await;
                    }
                }
                Ok(Err(_)) => return Ok(()),
                Err(_) => {}
            }
        }
    }

    fn second_conn(&mut self) -> Result<&mut Connection> {
        self.second
            .as_mut()
            .ok_or_else(|| anyhow!("no second sender has been opened"))
    }

    async fn open_second_sender(&mut self) -> Result<()> {
        let mut conn = Connection::connect(&self.addr).await?;
        let pkt = conn.recv().await?;
        ensure!(
            pkt.opcode == Opcode::Version,
            "expected Version from receiver, got {:?}",
            pkt.opcode
        );
        let body = serde_json::to_vec(&VersionMessage { version: 4 })?;
        conn.write(Opcode::Version, Some(&body)).await?;
        conn.upgrade_tls(self.fingerprint.as_deref()).await?;
        let info = v4::DeviceInfo {
            display_name: Some("fast-2".to_owned()),
            app_name: Some("fast-2".to_owned()),
            app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        };
        let msg = v4::MessageBuilder::new().sender_introduction(&info);
        conn.write(Opcode::Flatbuf, Some(&msg)).await?;

        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender never received ReceiverIntroduction"
            );
            let pkt = match tokio::time::timeout(deadline - now, conn.recv()).await {
                Ok(p) => p?,
                Err(_) => continue,
            };
            match pkt.opcode {
                Opcode::Ping => conn.write(Opcode::Pong, None).await?,
                Opcode::Flatbuf
                    if flat_payload_type(&pkt) == Some(v4::flat::Message::ReceiverIntroduction) =>
                {
                    break;
                }
                _ => {}
            }
        }
        self.second = Some(conn);
        Ok(())
    }

    async fn expect_load_on_second_sender(&mut self) -> Result<()> {
        self.expect_flat_on_second_sender(v4::flat::Message::Load, "Load")
            .await
    }

    /// Wait for the relayed `Load` on the second sender and assert its single
    /// media item carries the expected title and thumbnail metadata. This
    /// proves a party that did not issue the load still receives the full load
    /// broadcast with its metadata attached.
    async fn expect_load_with_metadata_on_second_sender(
        &mut self,
        title: Option<&str>,
        thumbnail_url: Option<&str>,
    ) -> Result<()> {
        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender never received the relayed Load with metadata"
            );
            let Some(pkt) = self.recv_second_strict(deadline - now).await? else {
                continue;
            };
            if pkt.opcode != Opcode::Flatbuf {
                continue;
            }
            let Some(body) = pkt.body.as_deref() else {
                continue;
            };
            let packet =
                v4::flat::root_as_packet(body).map_err(|e| anyhow!("invalid flatbuffer: {e}"))?;
            let Some(load) = packet.payload_as_load() else {
                continue;
            };
            let single = load
                .source_as_single()
                .ok_or_else(|| anyhow!("relayed Load was not a single media source"))?;
            ensure!(
                single.title() == title,
                "relayed Load title mismatch: got {:?}, expected {title:?}",
                single.title()
            );
            ensure!(
                single.thumbnail_url() == thumbnail_url,
                "relayed Load thumbnail_url mismatch: got {:?}, expected {thumbnail_url:?}",
                single.thumbnail_url()
            );
            info!(
                title = ?single.title(),
                thumbnail_url = ?single.thumbnail_url(),
                "second sender received relayed Load with metadata"
            );
            return Ok(());
        }
    }

    /// Wait for the relayed `Load` on the second sender and assert its single
    /// media item carries the expected custom `extra_metadata` key/values. This
    /// proves arbitrary custom metadata fields survive the relay to a party
    /// that did not issue the load.
    async fn expect_load_with_extra_metadata_on_second_sender(
        &mut self,
        expected: &[(&str, &str)],
    ) -> Result<()> {
        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender never received the relayed Load with custom metadata"
            );
            let Some(pkt) = self.recv_second_strict(deadline - now).await? else {
                continue;
            };
            if pkt.opcode != Opcode::Flatbuf {
                continue;
            }
            let Some(body) = pkt.body.as_deref() else {
                continue;
            };
            let packet =
                v4::flat::root_as_packet(body).map_err(|e| anyhow!("invalid flatbuffer: {e}"))?;
            let Some(load) = packet.payload_as_load() else {
                continue;
            };
            let single = load
                .source_as_single()
                .ok_or_else(|| anyhow!("relayed Load was not a single media source"))?;
            let got = v4::read_extra_metadata(&single).unwrap_or_default();
            for (key, value) in expected {
                let entry = got.get(*key).ok_or_else(|| {
                    anyhow!("relayed Load is missing custom metadata key {key:?}; got {got:?}")
                })?;
                ensure!(
                    entry == &v4::MetaValue::String((*value).to_owned()),
                    "relayed Load custom metadata {key:?} mismatch: got {entry:?}, expected {value:?}"
                );
            }
            info!(
                fields = expected.len(),
                "second sender received relayed Load with custom metadata"
            );
            return Ok(());
        }
    }

    /// Receive one packet on the second sender's connection, answering
    /// keepalive pings and folding relayed track advertisements and changes
    /// into the second-sender bookkeeping. Returns `None` if nothing arrived
    /// within `remaining`.
    async fn recv_second(&mut self, remaining: Duration) -> Result<Option<Packet>> {
        let pkt = {
            let conn = self
                .second
                .as_mut()
                .ok_or_else(|| anyhow!("no second sender has been opened"))?;
            match tokio::time::timeout(remaining, conn.recv()).await {
                Ok(p) => p?,
                Err(_) => return Ok(None),
            }
        };
        if pkt.opcode == Opcode::Ping {
            self.second_conn()?.write(Opcode::Pong, None).await?;
        } else {
            self.note_second_packet(&pkt)?;
        }
        Ok(Some(pkt))
    }

    /// `recv_second`, but any relayed v4 error fails the test. Steps that
    /// legitimately race a peer's in-flight operation use `recv_second`
    /// directly and handle `InvalidState` refusals themselves.
    async fn recv_second_strict(&mut self, remaining: Duration) -> Result<Option<Packet>> {
        let pkt = self.recv_second(remaining).await?;
        if let Some(pkt) = &pkt
            && let Some((_, desc)) = flat_error(pkt)
        {
            bail!("receiver reported a v4 error on the second sender: {desc}");
        }
        Ok(pkt)
    }

    /// Fold a packet the receiver sent to the second sender into that
    /// sender's track bookkeeping.
    fn note_second_packet(&mut self, pkt: &Packet) -> Result<()> {
        if pkt.opcode != Opcode::Flatbuf {
            return Ok(());
        }
        let Some(body) = pkt.body.as_deref() else {
            return Ok(());
        };
        let packet =
            v4::flat::root_as_packet(body).map_err(|e| anyhow!("invalid flatbuffer: {e}"))?;
        match packet.payload_type() {
            v4::flat::Message::TracksAvailable => {
                let tracks = packet
                    .payload_as_tracks_available()
                    .ok_or_else(|| anyhow!("malformed TracksAvailable"))?;
                let ids = advertised_ids(tracks);
                debug!(?ids, "second sender TracksAvailable track ids");
                self.second_track_ids = ids;
            }
            v4::flat::Message::ChangeTrack => {
                let change = packet
                    .payload_as_change_track()
                    .ok_or_else(|| anyhow!("malformed ChangeTrack"))?;
                self.second_last_track_state[change_track_slot(change)?] = Some(change.id());
            }
            _ => {}
        }
        Ok(())
    }

    /// Read everything currently queued on the second sender's connection
    /// until it goes quiet, so assertions see the receiver's latest relays.
    async fn drain_second(&mut self) -> Result<()> {
        while self
            .recv_second_strict(Duration::from_millis(80))
            .await?
            .is_some()
        {}
        Ok(())
    }

    /// The protocol id of the `n`th track advertised to the second sender.
    fn second_advertised_track_id(&self, kind: TrackKind, n: usize) -> Result<u32> {
        let slot = kind as usize;
        self.second_track_ids[slot].get(n).copied().ok_or_else(|| {
            anyhow!(
                "{} track index {n} out of range on the second sender ({} advertised); \
                 run AwaitTracksOnSecondSender before acting on tracks",
                KIND_NAMES[slot],
                self.second_track_ids[slot].len()
            )
        })
    }

    /// Wait until the second sender has been advertised at least `min` tracks
    /// of every kind (indexed by `TrackKind`).
    async fn await_tracks_on_second_sender(&mut self, min: [usize; 3]) -> Result<()> {
        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            let counts = [
                self.second_track_ids[0].len(),
                self.second_track_ids[1].len(),
                self.second_track_ids[2].len(),
            ];
            if (0..3).all(|slot| counts[slot] >= min[slot]) {
                info!(?counts, "required tracks advertised to the second sender");
                return Ok(());
            }
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender was never advertised >= {min:?} (video/audio/subtitle) \
                 tracks; its last TracksAvailable had {counts:?}"
            );
            self.recv_second_strict(deadline - now).await?;
        }
    }

    /// Attach an external subtitle from the second sender. The receiver
    /// refuses external-subtitle work while another (re)load is still
    /// applying (`InvalidState`), e.g. the first sender's add mid-dance,
    /// and a sender cannot know a peer's operation is in flight, so a
    /// refusal backs off and retries. Acceptance produces no direct reply,
    /// follow with `AwaitTracksOnSecondSender` to observe the updated
    /// advertisement.
    async fn add_subtitle_on_second_sender(
        &mut self,
        file_id: u32,
        select: bool,
        name: Option<&'static str>,
    ) -> Result<()> {
        let (url, _mime, _headers) = self.file(file_id)?;
        let deadline = Instant::now() + MAX_SETTLE;
        'send: loop {
            let msg = v4::MessageBuilder::new().add_subtitle_source(&url, select, name);
            self.second_conn()?
                .write(Opcode::Flatbuf, Some(&msg))
                .await?;
            // A refusal is sent synchronously while handling the request,
            // watch a short window for one, then treat the add as accepted.
            let watch_until = Instant::now() + Duration::from_millis(600);
            loop {
                let now = Instant::now();
                if now >= watch_until {
                    return Ok(());
                }
                let Some(pkt) = self.recv_second(watch_until - now).await? else {
                    continue;
                };
                if let Some((kind, desc)) = flat_error(&pkt) {
                    ensure!(
                        kind == v4::flat::ErrorKind::InvalidState,
                        "receiver reported a v4 error on the second sender: {desc}"
                    );
                    ensure!(
                        Instant::now() < deadline,
                        "second sender's AddSubtitleSource kept being refused (InvalidState)"
                    );
                    debug!("second sender's AddSubtitleSource refused (InvalidState); retrying");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue 'send;
                }
            }
        }
    }

    /// Send a `ChangeTrack` from the second sender for the `index`th track it
    /// was advertised (`None` = disable the kind) and wait for the relayed
    /// confirmation, interim confirmations for other ids are ignored, like the
    /// main sender's ChangeTrack expectation. An `InvalidState` refusal (a
    /// peer's external-subtitle operation still applying) backs off and
    /// retries.
    async fn change_track_on_second_sender(
        &mut self,
        kind: TrackKind,
        index: Option<usize>,
    ) -> Result<()> {
        let slot = kind as usize;
        let id = match index {
            Some(n) => Some(self.second_advertised_track_id(kind, n)?),
            None => None,
        };
        self.second_last_track_state[slot] = None;
        let msg = v4::MessageBuilder::new().change_track(id, track_kind_to_type(kind));
        self.second_conn()?
            .write(Opcode::Flatbuf, Some(&msg))
            .await?;
        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            if self.second_last_track_state[slot] == Some(id) {
                info!(
                    ?id,
                    kind = KIND_NAMES[slot],
                    "second sender's track change confirmed"
                );
                return Ok(());
            }
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender's ChangeTrack({}, {id:?}) was never confirmed",
                KIND_NAMES[slot]
            );
            let Some(pkt) = self.recv_second(deadline - now).await? else {
                continue;
            };
            if let Some((err_kind, desc)) = flat_error(&pkt) {
                ensure!(
                    err_kind == v4::flat::ErrorKind::InvalidState,
                    "receiver reported a v4 error on the second sender: {desc}"
                );
                debug!("second sender's ChangeTrack refused (InvalidState); retrying");
                tokio::time::sleep(Duration::from_millis(300)).await;
                let msg = v4::MessageBuilder::new().change_track(id, track_kind_to_type(kind));
                self.second_conn()?
                    .write(Opcode::Flatbuf, Some(&msg))
                    .await?;
            }
        }
    }

    /// Wait until the second sender receives a relayed Flatbuf message of the
    /// given payload type, answering keepalive pings while waiting.
    async fn expect_flat_on_second_sender(
        &mut self,
        expected: v4::flat::Message,
        label: &str,
    ) -> Result<()> {
        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender never received the relayed {label}"
            );
            let Some(pkt) = self.recv_second_strict(deadline - now).await? else {
                continue;
            };
            if flat_payload_type(&pkt) == Some(expected) {
                info!("second sender received relayed {label}");
                return Ok(());
            }
        }
    }

    async fn expect_volume_on_second_sender(&mut self, target: f32) -> Result<()> {
        let deadline = Instant::now() + MAX_SETTLE;
        loop {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "second sender never received VolumeChanged({target})"
            );
            let Some(pkt) = self.recv_second_strict(deadline - now).await? else {
                continue;
            };
            if flat_payload_type(&pkt) != Some(v4::flat::Message::VolumeChanged) {
                continue;
            }
            let body = pkt.body.as_deref().unwrap_or_default();
            let packet =
                v4::flat::root_as_packet(body).map_err(|e| anyhow!("invalid flatbuffer: {e}"))?;
            let got = packet
                .payload_as_volume_changed()
                .ok_or_else(|| anyhow!("malformed VolumeChanged"))?
                .volume();
            if (got - target).abs() <= 0.001 {
                info!("second sender received VolumeChanged({target})");
                return Ok(());
            }
        }
    }

    async fn measure_progress_both_senders(
        &mut self,
        a_expected_ms: u64,
        b_expected_ms: u64,
        tolerance_ms: u64,
        samples: usize,
    ) -> Result<()> {
        ensure!(self.second.is_some(), "no second sender has been opened");
        let needed = samples + 2;
        let mut a_times: Vec<Instant> = Vec::new();
        let mut b_times: Vec<Instant> = Vec::new();
        let max_expected = a_expected_ms.max(b_expected_ms);
        let deadline =
            Instant::now() + Duration::from_millis(max_expected * (samples as u64 + 2) + 4000);

        let conn = &mut self.conn;
        let second = self.second.as_mut().unwrap();
        // Flush any progress buffered at the previous interval (the second
        // sender's socket is never read by `settle`, so it can hold a backlog).
        drain_conn(conn).await?;
        drain_conn(second).await?;
        while a_times.len() < needed || b_times.len() < needed {
            let now = Instant::now();
            ensure!(
                now < deadline,
                "timed out: sender A got {} of {needed}, sender B got {} of {needed}",
                a_times.len(),
                b_times.len()
            );
            let remaining = deadline - now;
            tokio::select! {
                r = tokio::time::timeout(remaining, conn.recv()) => {
                    if let Ok(pkt) = r {
                        let pkt = pkt?;
                        match pkt.opcode {
                            Opcode::Ping => conn.write(Opcode::Pong, None).await?,
                            Opcode::Flatbuf if flat_payload_type(&pkt) == Some(v4::flat::Message::ProgressChanged) => {
                                a_times.push(Instant::now());
                            }
                            _ => {}
                        }
                    }
                }
                r = tokio::time::timeout(remaining, second.recv()) => {
                    if let Ok(pkt) = r {
                        let pkt = pkt?;
                        match pkt.opcode {
                            Opcode::Ping => second.write(Opcode::Pong, None).await?,
                            Opcode::Flatbuf if flat_payload_type(&pkt) == Some(v4::flat::Message::ProgressChanged) => {
                                b_times.push(Instant::now());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        check_interval("sender A", &a_times[1..], a_expected_ms, tolerance_ms)?;
        check_interval("sender B", &b_times[1..], b_expected_ms, tolerance_ms)?;
        Ok(())
    }

    /// Clear the pending `AwaitTracks` expectation if the most recent
    /// `TracksAvailable` already advertises enough tracks of every kind.
    fn check_await_tracks(&mut self) {
        if let Some(min) = self.expect.await_tracks
            && (0..3).all(|slot| self.track_ids[slot].len() >= min[slot])
        {
            self.expect.await_tracks = None;
            let counts = [
                self.track_ids[0].len(),
                self.track_ids[1].len(),
                self.track_ids[2].len(),
            ];
            info!(?counts, "required tracks advertised");
        }
    }

    /// Wait until the relayed track state matches `expected_idx` (indices
    /// into the advertised tracks per kind, `None` = kind disabled) and no
    /// contradicting relay arrives for a hold window. Reload dances emit
    /// interim selections (mid-preroll text deselect, auto-select remaps)
    /// before settling, point-in-time asserts race them, and how long a
    /// dance takes varies too much for a fixed sleep. A wanted index that
    /// is not (yet) advertised counts as not-matching, not as an error
    /// (advertisements are re-broadcast during the dance).
    async fn await_track_state(&mut self, expected_idx: [Option<usize>; 3]) -> Result<()> {
        const HOLD: Duration = Duration::from_millis(750);
        let deadline = Instant::now() + MAX_SETTLE;
        let mut matched_since: Option<Instant> = None;
        loop {
            let mut matches = true;
            for kind in [TrackKind::Video, TrackKind::Audio, TrackKind::Subtitle] {
                let slot = kind as usize;
                let want = match expected_idx[slot] {
                    Some(n) => match self.track_ids[slot].get(n).copied() {
                        Some(id) => Some(id),
                        None => {
                            matches = false;
                            break;
                        }
                    },
                    None => None,
                };
                if self.last_track_state[slot] != Some(want) {
                    matches = false;
                    break;
                }
            }
            if matches {
                let since = *matched_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= HOLD {
                    info!(?expected_idx, "relayed track state settled");
                    return Ok(());
                }
            } else {
                matched_since = None;
            }
            let now = Instant::now();
            ensure!(
                now < deadline,
                "track state never settled at {expected_idx:?} (video/audio/subtitle \
                 indices); last relayed ids were {:?}",
                self.last_track_state
            );
            let wait = Duration::from_millis(100).min(deadline - now);
            if let Ok(pkt) = tokio::time::timeout(wait, self.conn.recv()).await {
                self.handle_incoming(pkt?).await?;
            }
        }
    }

    /// Wait until the relayed v4 playback state matches AND holds steady for
    /// a short while. The external-subtitle dance's re-pause lands whenever
    /// playsink finishes its un-signalled text-branch churn, so a fixed
    /// sleep followed by a point-in-time assert races it.
    async fn await_playback_state(&mut self, expected: v4::flat::PlaybackState) -> Result<()> {
        const HOLD: Duration = Duration::from_millis(750);
        let deadline = Instant::now() + MAX_SETTLE;
        let mut matched_since: Option<Instant> = None;
        loop {
            if self.last_state_v4 == Some(expected) {
                let since = *matched_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= HOLD {
                    info!(?expected, "relayed playback state settled");
                    return Ok(());
                }
            } else {
                matched_since = None;
            }
            let now = Instant::now();
            ensure!(
                now < deadline,
                "playback state never settled at {expected:?}; last relayed was {:?}",
                self.last_state_v4
            );
            let wait = Duration::from_millis(100).min(deadline - now);
            if let Ok(pkt) = tokio::time::timeout(wait, self.conn.recv()).await {
                self.handle_incoming(pkt?).await?;
            }
        }
    }

    /// The protocol id of the `n`th advertised track of the given kind.
    fn advertised_track_id(&self, kind: TrackKind, n: usize) -> Result<u32> {
        let slot = kind as usize;
        self.track_ids[slot].get(n).copied().ok_or_else(|| {
            anyhow!(
                "{} track index {n} out of range ({} advertised); \
                 run AwaitTracks before acting on tracks",
                KIND_NAMES[slot],
                self.track_ids[slot].len()
            )
        })
    }

    /// Send a `ChangeTrack` for the `index`th advertised track of `kind`
    /// (`None` = disable the kind), optionally expecting the receiver to
    /// relay the change back.
    async fn send_change_track(
        &mut self,
        kind: TrackKind,
        index: Option<usize>,
        expect: bool,
    ) -> Result<()> {
        let id = match index {
            Some(n) => Some(self.advertised_track_id(kind, n)?),
            None => None,
        };
        if expect {
            self.expect.change_track[kind as usize] = Some(id);
        }
        let msg = v4::MessageBuilder::new().change_track(id, track_kind_to_type(kind));
        self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
        Ok(())
    }

    async fn exec_send(&mut self, send: &Op) -> Result<()> {
        match send {
            Op::Version(version) => {
                self.version = *version;
                self.send_json(Opcode::Version, &VersionMessage { version: *version })
                    .await?;
                // v4 upgrades the plaintext connection to TLS in place,
                // immediately after the `Version` exchange. Versions higher than
                // the receiver implements are clamped to v4, so they upgrade too.
                if *version >= 4 {
                    let fingerprint = self.fingerprint.clone();
                    self.conn.upgrade_tls(fingerprint.as_deref()).await?;
                    self.tls_upgraded = true;
                }
            }
            Op::Initial => {
                let body = InitialSenderMessage {
                    display_name: Some("fast".to_owned()),
                    app_name: Some("fast".to_owned()),
                    app_version: Some("test".to_owned()),
                };
                self.send_json(Opcode::Initial, &body).await?;
            }
            Op::Ping => self.conn.write(Opcode::Ping, None).await?,
            Op::SetVolume(volume) => {
                self.expect.volume = Some((*volume, now_millis()));
                self.send_json(Opcode::SetVolume, &SetVolumeMessage { volume: *volume })
                    .await?;
            }
            Op::SetSpeed(speed) => {
                self.send_json(Opcode::SetSpeed, &SetSpeedMessage { speed: *speed })
                    .await?;
            }
            Op::Seek(time) => {
                self.send_json(Opcode::Seek, &SeekMessage { time: *time })
                    .await?;
            }
            Op::Stop => self.conn.write(Opcode::Stop, None).await?,
            Op::Pause => {
                self.expect.pause = true;
                self.conn.write(Opcode::Pause, None).await?;
            }
            Op::Resume => {
                self.expect.resume = true;
                self.conn.write(Opcode::Resume, None).await?;
            }
            Op::PlayV2 { file_id } => {
                let (url, mime, headers) = self.file(*file_id)?;
                let body = v2::PlayMessage {
                    container: mime.to_owned(),
                    url: Some(url),
                    content: None,
                    time: None,
                    speed: None,
                    headers,
                };
                self.send_json(Opcode::Play, &body).await?;
            }
            Op::PlayV3 { file_id } => {
                let (url, mime, headers) = self.file(*file_id)?;
                self.play_v3(v3::PlayMessage {
                    container: mime.to_owned(),
                    url: Some(url),
                    content: None,
                    time: None,
                    volume: None,
                    speed: None,
                    headers,
                    metadata: None,
                })
                .await?;
            }
            Op::PlayV3WithBody {
                file_id,
                time,
                volume,
                speed,
            } => {
                let (url, mime, headers) = self.file(*file_id)?;
                self.play_v3(v3::PlayMessage {
                    container: mime.to_owned(),
                    url: Some(url),
                    content: None,
                    time: *time,
                    volume: *volume,
                    speed: *speed,
                    headers,
                    metadata: None,
                })
                .await?;
            }
            Op::PlayV3WithMetadata {
                file_id,
                title,
                thumbnail_url,
            } => {
                let (url, mime, headers) = self.file(*file_id)?;
                self.play_v3(v3::PlayMessage {
                    container: mime.to_owned(),
                    url: Some(url),
                    content: None,
                    time: None,
                    volume: None,
                    speed: None,
                    headers,
                    metadata: Some(v3::MetadataObject::Generic {
                        title: title.map(str::to_owned),
                        thumbnail_url: thumbnail_url.map(str::to_owned),
                        custom: None,
                    }),
                })
                .await?;
            }
            Op::PlayContent { mime, content } => {
                self.play_v3(v3::PlayMessage {
                    container: (*mime).to_owned(),
                    url: None,
                    content: Some((*content).to_owned()),
                    time: None,
                    volume: None,
                    speed: None,
                    headers: None,
                    metadata: None,
                })
                .await?;
            }
            Op::SubscribeEvent(event) => {
                self.subscriptions.insert(event.clone());
                let body = v3::SubscribeEventMessage {
                    event: event.clone(),
                };
                self.send_json(Opcode::SubscribeEvent, &body).await?;
            }
            Op::UnsubscribeEvent(event) => {
                self.subscriptions.remove(event);
                let body = v3::UnsubscribeEventMessage {
                    event: event.clone(),
                };
                self.send_json(Opcode::UnsubscribeEvent, &body).await?;
            }
            Op::PlaylistV3 { items } => self.send_playlist(items, None, None, None).await?,
            Op::PlaylistV3WithOptions {
                items,
                offset,
                volume,
                speed,
            } => self.send_playlist(items, *offset, *volume, *speed).await?,
            Op::SetPlaylistItem { index } => {
                let item = self.playlist_item(*index)?;
                self.expect_media_item(item);
                let body = v3::SetPlaylistItemMessage { item_index: *index };
                self.send_json(Opcode::SetPlaylistItem, &body).await?;
            }

            Op::SenderIntroduction => {
                let info = v4::DeviceInfo {
                    display_name: Some("fast".to_owned()),
                    app_name: Some("fast".to_owned()),
                    app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                };
                let msg = v4::MessageBuilder::new().sender_introduction(&info);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::PlayV4 { file_id } => {
                let item = self.media_item_v4(*file_id)?;
                let msg = v4::MessageBuilder::new().load_single(item);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::PlayV4WithMetadata {
                file_id,
                title,
                thumbnail_url,
            } => {
                let mut item = self.media_item_v4(*file_id)?;
                item.title = *title;
                item.thumbnail_url = *thumbnail_url;
                let msg = v4::MessageBuilder::new().load_single(item);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::PlayV4WithExtraMetadata { file_id, extra } => {
                let mut item = self.media_item_v4(*file_id)?;
                item.extra_metadata = Some(
                    extra
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), v4::MetaValue::String((*v).to_owned())))
                        .collect(),
                );
                let msg = v4::MessageBuilder::new().load_single(item);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::PlayFakeUrlV4 { container } => {
                // A well-formed file-server URL pointing at a resource id that
                // was never served, so the receiver gets a 404 when fetching it.
                let url = self.file_server.get_url(&self.local_ip, &Uuid::new_v4());
                let item = v4::MediaItem {
                    container: (*container).to_owned(),
                    source_url: url,
                    start_time: None,
                    volume: None,
                    speed: None,
                    headers: None,
                    title: None,
                    thumbnail_url: None,
                    metadata: None,
                    extra_metadata: None,
                };
                let msg = v4::MessageBuilder::new().load_single(item);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::LoadQueueV4 { items, start_index } => {
                let media_items = items
                    .iter()
                    .map(|it| self.media_item_v4(it.file_id))
                    .collect::<Result<Vec<_>>>()?;
                let msg =
                    v4::MessageBuilder::new().load_queue(media_items.into_iter(), *start_index);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::QueueInsertV4 { file_id, position } => {
                let item = self.media_item_v4(*file_id)?;
                let msg = v4::MessageBuilder::new().queue_insert(item, *position);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::QueueRemoveV4 { position } => {
                let msg = v4::MessageBuilder::new().queue_remove(*position);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::QueueSelectV4 { position } => {
                let msg = v4::MessageBuilder::new().queue_select(*position);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::SetVolumeV4(volume) => {
                self.expect.volume_v4 = Some(*volume as f32);
                let msg = v4::MessageBuilder::new().volume_changed(*volume as f32);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::SetSpeedV4(speed) => {
                self.expect.speed_v4 = Some(*speed as f32);
                let msg = v4::MessageBuilder::new().speed_changed(*speed as f32);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::SetVolumeV4Raw(volume) => {
                let msg = v4::MessageBuilder::new().volume_changed(*volume as f32);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::SetSpeedV4Raw(speed) => {
                let msg = v4::MessageBuilder::new().speed_changed(*speed as f32);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::SetProgressIntervalV4 { millis } => {
                let micros = Duration::from_millis(*millis).as_micros() as u64;
                let msg = v4::MessageBuilder::new()
                    .set_progress_update_interval(v4::flat::Time::new(micros));
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::EmptyProgressIntervalV4 => {
                let msg = v4::MessageBuilder::new().set_progress_update_interval_raw(None);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::LoadQueueRepeatV4 {
                file_id,
                count,
                start_index,
            } => {
                let items = (0..*count)
                    .map(|_| self.media_item_v4(*file_id))
                    .collect::<Result<Vec<_>>>()?;
                let msg = v4::MessageBuilder::new().load_queue(items.into_iter(), *start_index);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::ErrorV4(kind) => {
                let msg = v4::MessageBuilder::new().error(None, *kind);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::CompanionHelloResponseV4 => {
                let msg = v4::MessageBuilder::new().companion_hello_response(1);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::ReceiverIntroductionV4 => {
                let info = v4::DeviceInfo {
                    display_name: Some("fast".to_owned()),
                    app_name: Some("fast".to_owned()),
                    app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                };
                let msg = v4::MessageBuilder::new().receiver_introduction(
                    &info,
                    std::iter::empty(),
                    std::iter::empty(),
                    std::iter::empty(),
                    std::iter::empty(),
                    std::iter::empty(),
                    std::iter::empty(),
                    std::iter::empty(),
                    false,
                    false,
                    0.01,
                );
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::SeekV4(time) => {
                let micros = Duration::from_secs_f64(*time).as_micros() as u64;
                let msg = v4::MessageBuilder::new()
                    .progress_changed(v4::flat::Time::new(micros), v4::flat::Time::new(0));
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::EmptySeekV4 => {
                let msg = v4::MessageBuilder::new().progress_changed_raw(None, None);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::PauseV4 => {
                self.expect.state_v4 = Some(v4::flat::PlaybackState::Paused);
                let msg = v4::MessageBuilder::new()
                    .playback_state_changed(v4::flat::PlaybackState::Paused);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::ResumeV4 => {
                self.expect.state_v4 = Some(v4::flat::PlaybackState::Playing);
                let msg = v4::MessageBuilder::new()
                    .playback_state_changed(v4::flat::PlaybackState::Playing);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::StopV4 => {
                let msg = v4::MessageBuilder::new().stop_playback();
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::AddSubtitleSourceV4 {
                file_id,
                select,
                name,
            } => {
                let (url, _mime, _headers) = self.file(*file_id)?;
                let msg = v4::MessageBuilder::new().add_subtitle_source(&url, *select, *name);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::AddSubtitleSourceFakeUrlV4 { select } => {
                // A well-formed file-server URL for a resource that was never
                // served, so the subtitle fetch gets a 404.
                let url = self.file_server.get_url(&self.local_ip, &Uuid::new_v4());
                let msg = v4::MessageBuilder::new().add_subtitle_source(&url, *select, None);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::AddSubtitleSourceEmptyUrlV4 => {
                let msg = v4::MessageBuilder::new().add_subtitle_source("", false, None);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::ChangeTrack { kind, index } => {
                self.send_change_track(*kind, *index, true).await?;
            }
            Op::ChangeTracks(changes) => {
                for (kind, index) in *changes {
                    self.send_change_track(*kind, *index, true).await?;
                }
            }
            Op::ChangeTrackNoExpect { kind, index } => {
                self.send_change_track(*kind, Some(*index), false).await?;
            }
            Op::ChangeTrackRawId { kind, id } => {
                let msg =
                    v4::MessageBuilder::new().change_track(Some(*id), track_kind_to_type(*kind));
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::ChangeTrackMismatched {
                send_as,
                take_from,
                index,
            } => {
                let id = self.advertised_track_id(*take_from, *index)?;
                let msg =
                    v4::MessageBuilder::new().change_track(Some(id), track_kind_to_type(*send_as));
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::CompanionHello => {
                self.expect.companion_hello = true;
                let msg = v4::MessageBuilder::new().companion_hello_request();
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::ServeCompanionFile {
                resource_id,
                path,
                mime,
            } => {
                let mut file_path = self.sample_media.to_path_buf();
                file_path.push(path);
                let data = std::fs::read(&file_path).with_context(|| {
                    format!("reading companion media file {}", file_path.display())
                })?;
                self.companion_resources.insert(
                    *resource_id,
                    CompanionResource {
                        content_type: (*mime).to_owned(),
                        data,
                    },
                );
            }
            Op::PlayCompanion { resource_id } => {
                let provider_id = self
                    .companion_provider_id
                    .ok_or_else(|| anyhow!("CompanionHello must complete before PlayCompanion"))?;
                let container = self
                    .companion_resources
                    .get(resource_id)
                    .ok_or_else(|| anyhow!("resource {resource_id} was not served"))?
                    .content_type
                    .clone();
                let url = companion::create_url(provider_id, *resource_id);
                let item = v4::MediaItem {
                    container,
                    source_url: url,
                    start_time: None,
                    volume: None,
                    speed: None,
                    headers: None,
                    title: None,
                    thumbnail_url: None,
                    metadata: None,
                    extra_metadata: None,
                };
                let msg = v4::MessageBuilder::new().load_single(item);
                self.expect.companion_served = Some(*resource_id);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::PlayCompanionMissing {
                resource_id,
                container,
            } => {
                let provider_id = self.companion_provider_id.ok_or_else(|| {
                    anyhow!("CompanionHello must complete before PlayCompanionMissing")
                })?;
                // Register the resource as missing without serving any data, so
                // the provider reports it as not found when the receiver fetches.
                self.companion_missing
                    .insert(*resource_id, (*container).to_owned());
                let url = companion::create_url(provider_id, *resource_id);
                let item = v4::MediaItem {
                    container: (*container).to_owned(),
                    source_url: url,
                    start_time: None,
                    volume: None,
                    speed: None,
                    headers: None,
                    title: None,
                    thumbnail_url: None,
                    metadata: None,
                    extra_metadata: None,
                };
                let msg = v4::MessageBuilder::new().load_single(item);
                self.conn.write(Opcode::Flatbuf, Some(&msg)).await?;
            }
            Op::RawOpcode(opcode) => {
                self.conn.write_raw(*opcode, None).await?;
            }
            Op::RawMessage { opcode, body } => {
                self.conn.write_raw(*opcode, Some(body)).await?;
            }
        }
        Ok(())
    }

    async fn play_v3(&mut self, body: v3::PlayMessage) -> Result<()> {
        self.expect.play_update = Some(body.clone());
        self.expect_media_item(body.clone().into());
        self.send_json(Opcode::Play, &body).await
    }

    async fn send_playlist(
        &mut self,
        items: &[PlaylistItem],
        offset: Option<u64>,
        volume: Option<f64>,
        speed: Option<f64>,
    ) -> Result<()> {
        let media_items = items
            .iter()
            .map(|it| {
                let (url, mime, headers) = self.file(it.file_id)?;
                Ok(v3::MediaItem {
                    container: mime.to_owned(),
                    url: Some(url),
                    headers,
                    ..Default::default()
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let start = offset.unwrap_or(0) as usize;
        if let Some(item) = media_items.get(start) {
            self.expect_media_item(item.clone());
        }

        let playlist = PlaylistContent {
            variant: v3::ContentType::Playlist,
            items: media_items,
            offset,
            volume,
            speed,
            ..Default::default()
        };
        self.playlist = Some(playlist.clone());

        let body = v3::PlayMessage {
            container: "application/json".to_owned(),
            url: None,
            content: Some(serde_json::to_string(&playlist)?),
            time: None,
            volume: None,
            speed: None,
            headers: None,
            metadata: None,
        };
        self.expect.play_update = Some(body.clone());
        self.send_json(Opcode::Play, &body).await
    }

    fn expect_media_item(&mut self, item: v3::MediaItem) {
        if self
            .subscriptions
            .contains(&EventSubscribeObject::MediaItemStart)
        {
            self.expect.media_item_start = Some(item.clone());
        }
        if self
            .subscriptions
            .contains(&EventSubscribeObject::MediaItemChanged)
        {
            self.expect.media_item_changed = Some(item.clone());
        }
        if self
            .subscriptions
            .contains(&EventSubscribeObject::MediaItemEnd)
        {
            self.expect.media_item_end = Some(item);
        }
    }

    fn serve_file(
        &mut self,
        path: &'static str,
        id: u32,
        mime: &'static str,
        headers: Option<&'static [(&'static str, &'static str)]>,
    ) -> Result<()> {
        let mut file_path = self.sample_media.to_path_buf();
        file_path.push(path);
        ensure!(
            file_path.exists(),
            "sample media file does not exist: {}",
            file_path.display()
        );

        let (file_id, required_headers) = match headers {
            Some(headers) => {
                let map: HashMap<String, String> = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                (
                    self.file_server
                        .add_file_with_headers(file_path, mime, map.clone()),
                    Some(map),
                )
            }
            None => (self.file_server.add_file(file_path, mime), None),
        };

        let url = self.file_server.get_url(&self.local_ip, &file_id);
        self.file_urls.insert(id, (url, mime, required_headers));
        Ok(())
    }

    fn file(&self, id: u32) -> Result<FileEntry> {
        let (url, mime, headers) = self
            .file_urls
            .get(&id)
            .ok_or_else(|| anyhow!("no file has been served with id {id}"))?;
        Ok((url.clone(), mime, headers.clone()))
    }

    fn media_item_v4(&self, file_id: u32) -> Result<v4::MediaItem<'static>> {
        let (url, mime, headers) = self.file(file_id)?;
        Ok(v4::MediaItem {
            container: mime.to_owned(),
            source_url: url,
            start_time: None,
            volume: None,
            speed: None,
            headers,
            title: None,
            thumbnail_url: None,
            metadata: None,
            extra_metadata: None,
        })
    }

    fn playlist_item(&self, index: u64) -> Result<v3::MediaItem> {
        let playlist = self
            .playlist
            .as_ref()
            .ok_or_else(|| anyhow!("SetPlaylistItem without a preceding playlist"))?;
        playlist
            .items
            .get(index as usize)
            .cloned()
            .ok_or_else(|| anyhow!("playlist item index {index} is out of range"))
    }

    async fn send_json<T: Serialize>(&mut self, opcode: Opcode, msg: &T) -> Result<()> {
        let body = serde_json::to_vec(msg).context("serializing message body")?;
        self.conn.write(opcode, Some(&body)).await
    }

    pub fn dump_file_urls(&self) {
        println!("File urls: {:#?}", self.file_urls);
    }
}

/// Read and discard everything currently buffered on a connection, answering
/// pings, until no packet arrives within a short window.
async fn drain_conn(conn: &mut Connection) -> Result<()> {
    while let Ok(pkt) = tokio::time::timeout(Duration::from_millis(80), conn.recv()).await {
        let pkt = pkt?;
        if pkt.opcode == Opcode::Ping {
            conn.write(Opcode::Pong, None).await?;
        }
    }
    Ok(())
}

/// Track ids carried by a `TracksAvailable` advertisement, per kind
/// (indexed by `TrackKind`).
fn advertised_ids(tracks: v4::flat::TracksAvailable<'_>) -> [Vec<u32>; 3] {
    let mut ids: [Vec<u32>; 3] = Default::default();
    if let Some(list) = tracks.tracks() {
        for track in list {
            let slot = match track.metadata_type() {
                v4::flat::MediaTrackMetadata::Video => TrackKind::Video,
                v4::flat::MediaTrackMetadata::Audio => TrackKind::Audio,
                v4::flat::MediaTrackMetadata::Subtitle => TrackKind::Subtitle,
                _ => continue,
            };
            ids[slot as usize].push(track.id());
        }
    }
    ids
}

/// If the packet is a relayed v4 `Error`, its kind and a description.
fn flat_error(pkt: &Packet) -> Option<(v4::flat::ErrorKind, String)> {
    if flat_payload_type(pkt) != Some(v4::flat::Message::Error) {
        return None;
    }
    let body = pkt.body.as_deref()?;
    let packet = v4::flat::root_as_packet(body).ok()?;
    let err = packet.payload_as_error()?;
    Some((
        err.kind(),
        format!("{:?} (packet_num={:?})", err.kind(), err.packet_num()),
    ))
}

/// The `TrackKind` slot a relayed `ChangeTrack` refers to.
fn change_track_slot(change: v4::flat::ChangeTrack<'_>) -> Result<usize> {
    Ok(match change.track_type() {
        v4::flat::MediaTrackType::Video => TrackKind::Video,
        v4::flat::MediaTrackType::Audio => TrackKind::Audio,
        v4::flat::MediaTrackType::Subtitle => TrackKind::Subtitle,
        typ => bail!("relayed ChangeTrack with unknown track type {typ:?}"),
    } as usize)
}

fn flat_payload_type(pkt: &Packet) -> Option<v4::flat::Message> {
    if pkt.opcode != Opcode::Flatbuf {
        return None;
    }
    let body = pkt.body.as_deref()?;
    v4::flat::root_as_packet(body)
        .ok()
        .map(|p| p.payload_type())
}

fn check_interval(
    label: &str,
    times: &[Instant],
    expected_ms: u64,
    tolerance_ms: u64,
) -> Result<()> {
    ensure!(
        times.len() >= 2,
        "{label}: not enough progress samples ({})",
        times.len()
    );
    let span = times[times.len() - 1].duration_since(times[0]);
    let intervals = (times.len() - 1) as f64;
    let avg_ms = span.as_secs_f64() * 1000.0 / intervals;
    let diff = (avg_ms - expected_ms as f64).abs();
    ensure!(
        diff <= tolerance_ms as f64,
        "{label}: progress interval ~{avg_ms:.0}ms over {intervals} samples, expected {expected_ms}ms (+/-{tolerance_ms}ms)"
    );
    info!("{label}: progress interval ~{avg_ms:.0}ms (expected {expected_ms}ms)");
    Ok(())
}

fn format_body(opcode: Opcode, body: &[u8]) -> String {
    if opcode == Opcode::Flatbuf {
        match v4::flat::root_as_packet(body) {
            Ok(packet) => format!("{packet:?}"),
            Err(e) => format!("<invalid flatbuffer ({e}), {} bytes>", body.len()),
        }
    } else {
        String::from_utf8_lossy(body).into_owned()
    }
}

fn parse<T: DeserializeOwned>(opcode: Opcode, body: Option<&[u8]>) -> Result<T> {
    let body = body.ok_or_else(|| anyhow!("{opcode:?} message is missing its body"))?;
    serde_json::from_slice(body)
        .with_context(|| format!("parsing {opcode:?} body: {}", String::from_utf8_lossy(body)))
}

pub async fn run_case(
    addr: &SocketAddr,
    file_server: &FileServer,
    sample_media: &Path,
    steps: &[Step],
    fingerprint: Option<Vec<u8>>,
) -> Result<()> {
    let engine = Engine::connect(addr, file_server, sample_media, fingerprint).await?;
    engine.run(steps).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_play() -> v3::PlayMessage {
        v3::PlayMessage {
            container: "video/mp4".to_owned(),
            url: Some("http://localhost/x".to_owned()),
            content: None,
            time: None,
            volume: None,
            speed: None,
            headers: None,
            metadata: None,
        }
    }

    #[test]
    fn expectations_default_is_not_pending() {
        assert!(!Expectations::default().pending());
    }

    #[test]
    fn each_expectation_field_marks_pending() {
        let setters: [fn(&mut Expectations); 21] = [
            |e| e.next_progress_floor = Some(2.0),
            |e| e.waiting_opcode = Some(Opcode::Version),
            |e| e.volume = Some((1.0, 0)),
            |e| e.play_update = Some(sample_play()),
            |e| e.pause = true,
            |e| e.resume = true,
            |e| e.media_item_start = Some(v3::MediaItem::default()),
            |e| e.media_item_changed = Some(v3::MediaItem::default()),
            |e| e.media_item_end = Some(v3::MediaItem::default()),
            |e| e.receiver_intro = true,
            |e| e.volume_v4 = Some(1.0),
            |e| e.speed_v4 = Some(1.5),
            |e| e.state_v4 = Some(v4::flat::PlaybackState::Paused),
            |e| e.companion_hello = true,
            |e| e.companion_served = Some(0),
            |e| e.error = Some(v4::flat::ErrorKind::SeekOutOfRange),
            |e| e.progress_v4_at_least = Some(25.0),
            |e| e.await_tracks = Some([1, 1, 3]),
            |e| e.change_track[TrackKind::Video as usize] = Some(Some(0)),
            |e| e.change_track[TrackKind::Audio as usize] = Some(Some(1)),
            |e| e.change_track[TrackKind::Subtitle as usize] = Some(None),
        ];
        for set in setters {
            let mut e = Expectations::default();
            set(&mut e);
            assert!(e.pending(), "expected pending after mutation");
        }
    }

    #[test]
    fn describe_lists_every_outstanding_expectation() {
        let e = Expectations {
            waiting_opcode: Some(Opcode::Initial),
            volume: Some((0.5, 0)),
            play_update: Some(sample_play()),
            pause: true,
            resume: true,
            media_item_start: Some(v3::MediaItem::default()),
            media_item_changed: Some(v3::MediaItem::default()),
            media_item_end: Some(v3::MediaItem::default()),
            receiver_intro: true,
            volume_v4: Some(0.5),
            speed_v4: Some(1.5),
            state_v4: Some(v4::flat::PlaybackState::Paused),
            companion_hello: true,
            companion_served: Some(7),
            error: Some(v4::flat::ErrorKind::VolumeOutOfRange),
            progress_v4_at_least: Some(25.0),
            next_progress_floor: Some(2.0),
            await_tracks: Some([1, 1, 3]),
            change_track: [Some(Some(0)), Some(Some(1)), Some(None)],
        };

        let d = e.describe();
        for needle in [
            "Initial",
            "VolumeUpdate",
            "PlayUpdate",
            "Paused",
            "Playing",
            "MediaItemStart",
            "MediaItemChanged",
            "MediaItemEnd",
            "ReceiverIntroduction",
            "VolumeChanged",
            "SpeedChanged",
            "PlaybackStateChanged",
            "CompanionHelloResponse",
            "CompanionResource(7)",
            "Error(VolumeOutOfRange)",
            "ProgressV4AtLeast(25)",
            "NextProgressV4AtLeast(2)",
            "TracksAvailable(>= 1 video, 1 audio, 3 subtitle)",
            "ChangeTrack(Video, id=0)",
            "ChangeTrack(Audio, id=1)",
            "ChangeTrack(Subtitle, disabled)",
        ] {
            assert!(d.contains(needle), "describe() missing {needle:?}: {d}");
        }
    }

    #[test]
    fn parse_requires_a_body() {
        let err = parse::<VersionMessage>(Opcode::Version, None).unwrap_err();
        assert!(format!("{err:?}").contains("missing"), "{err:?}");
    }

    #[test]
    fn parse_reports_invalid_json() {
        let err = parse::<VersionMessage>(Opcode::Version, Some(b"{ not json")).unwrap_err();
        assert!(
            format!("{err:?}").contains("parsing Version body"),
            "{err:?}"
        );
    }

    #[test]
    fn parse_returns_the_message() {
        let v: VersionMessage = parse(Opcode::Version, Some(br#"{"version":3}"#)).unwrap();
        assert_eq!(v.version, 3);
    }
}
