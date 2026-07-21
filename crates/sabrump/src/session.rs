//! The SABR session pump.
//!
//! A background async task drives the request/response loop: it builds a
//! `VideoPlaybackAbrRequest`, POSTs it through the [`SabrTransport`], streams the
//! UMP response into per-format [`SabrTrackBuffer`]s, and honours the server's
//! readahead / backoff / redirect / seek directives before issuing the next
//! request.
//!
//! Consumers (e.g. a gstreamer source element) declare what they want via
//! [`SabrSession::set_demand`] and pull completed segments out of the buffers
//! returned by [`SabrSession::buffer_for`].

// Times are kept as plain `i64` micro/milliseconds rather than `time::Duration`
// on purpose. They mirror the wire protocol's integer fields, several use signed
// sentinels (`NO_US = i64::MIN`, live-head `player_time`) that `Duration` can't
// represent, they participate in signed arithmetic (deltas that can go negative),
// and some live in `AtomicI64`. `Duration` would fit none of those cleanly.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use crate::buffer::{NO_US, SabrTrackBuffer};
use crate::error::SabrError;
use crate::format::{SabrFormat, SabrFormatKey};
use crate::http::{SabrBody, SabrTransport};
use crate::proto::{
    BufferedRange, ClientAbrState, ClientInfo, FormatInitializationMetadata, LiveMetadata,
    MediaHeader, MediaType, NextRequestPolicy, SabrContext, SabrContextSendingPolicy,
    SabrContextUpdate, SabrError as SabrErrorPart, SabrRedirect, SabrSeek, SnackbarMessage,
    StreamProtectionStatus, StreamerContext, TimeRange, VideoPlaybackAbrRequest,
};
use crate::segment::SabrSegment;
use crate::spec::{Role, SabrStreamSpec};
use crate::ump::{PartType, UmpReader};
use prost::Message;

// --- tunables ---
const DEFAULT_READAHEAD_MS: i64 = 20_000;
const PUMP_IDLE_POLL_MS: u64 = 250;
const ERROR_BACKOFF_BASE_MS: i64 = 1_000;
const ERROR_BACKOFF_MAX_MS: i64 = 30_000;
const ERROR_BACKOFF_MAX_SHIFT: u32 = 5;
const MAX_CONSECUTIVE_ERRORS: i32 = 4;
const DEFAULT_KEEP_BEHIND_US: i64 = 30_000_000;
const RESTART_TOLERANCE_US: i64 = 1_000_000;
const FRONTIER_EPSILON_US: i64 = 1_000_000;
const NO_PROGRESS_THRESHOLD: i32 = 2;
const MAX_EMPTY_RESPONSES: i32 = 8;
const MAX_SUBSTITUTED_RESPONSES: i32 = 4;
const MAX_REDIRECTS: i32 = 3;
const EMPTY_BACKOFF_BASE_MS: i64 = 500;
const BANDWIDTH_ESTIMATE: i64 = 104_857;
const THROUGHPUT_MIN_BYTES: i64 = 65_536;
const THROUGHPUT_SMOOTHING: i64 = 30;
const THROUGHPUT_MIN_SPEEDUP: i64 = 2;
const LIVE_POLL_MS: i64 = 1_000;
const DEFAULT_LIVE_SEGMENT_US: i64 = 5_000_000;
const SABR_SEEK_SLACK_US: i64 = 30_000_000;
/// Sentinel `player_time` for the initial live-head request (before any
/// `LiveMetadata` arrives). Must be exactly JS `Number.MAX_SAFE_INTEGER`
/// (2^53 - 1) expressed in microseconds. The SABR server rejects any other
/// large value with `sabr.invalid_input_stream`.
const LIVE_HEAD_PLAYER_TIME_US: i64 = 9_007_199_254_740_991 * 1000;
const PROTECTION_STATUS_ATTESTATION_REQUIRED: i32 = 3;
const MICROS_PER_SECOND: i64 = 1_000_000;

/// An event emitted by the session pump task.
#[derive(Debug)]
pub enum SabrSessionEvent<'a> {
    LiveMetadata(&'a LiveMetadata),
    FormatInitialization(&'a FormatInitializationMetadata),
    SessionError(&'a SabrError),
    /// The server asked us to back off for `delay_ms` while starved.
    Backoff { delay_ms: i64 },
    BackoffEnded,
}

/// Callback invoked by the pump task for each [`SabrSessionEvent`]. Register
/// one with [`SabrSession::set_listener`].
pub type SabrSessionListener = dyn for<'a> Fn(SabrSessionEvent<'a>) + Send + Sync;

/// Deliver an event to the registered listener, if any.
fn emit(shared: &Arc<Shared>, event: SabrSessionEvent) {
    if let Some(listener) = shared.listener.lock().as_deref() {
        listener(event);
    }
}

#[derive(Clone)]
struct Demand {
    format: SabrFormat,
    from_us: i64,
    alternates: Vec<SabrFormat>,
}

/// Cross-thread mutable state, guarded by the `Shared::state` mutex.
struct State {
    streaming_url: String,
    playback_cookie: Option<Vec<u8>>,
    sabr_contexts: HashMap<i32, SabrContext>,
    active_sabr_contexts: BTreeSet<i32>,
    format_initialization: HashMap<SabrFormatKey, FormatInitializationMetadata>,
    format_complete: HashSet<SabrFormatKey>,
    format_no_progress: HashMap<SabrFormatKey, i32>,
    server_chosen: HashMap<Role, SabrFormatKey>,
    demanded_keys: HashSet<SabrFormatKey>,

    video_demand: Option<Demand>,
    audio_demand: Option<Demand>,

    playback_position_us: i64,
    resume_position_us: Option<i64>,
    restart_from_us: i64,
    seek_pending_us: Option<i64>,
    last_sabr_seek_us: i64,
    restart_epoch: i32,

    target_video_readahead_ms: i64,
    target_audio_readahead_ms: i64,

    backoff_until_ms: i64,
    server_backoff_until_ms: i64,
    error_backoff_until_ms: i64,

    aborting: bool,
    last_action_ms: i64,
    last_request_ms: i64,

    media_base_us: i64,
    media_base_set: bool,
    live_metadata: Option<LiveMetadata>,
    reestimated: bool,

    viewport_width: i32,
    viewport_height: i32,
    initial_bandwidth: i64,
    keep_behind_us: i64,
    min_readahead_ms: i64,
    max_readahead_ms: i64,

    fatal: Option<String>,
}

impl State {
    fn demand(&self, role: Role) -> &Option<Demand> {
        match role {
            Role::Video => &self.video_demand,
            Role::Audio => &self.audio_demand,
        }
    }

    fn demand_mut(&mut self, role: Role) -> &mut Option<Demand> {
        match role {
            Role::Video => &mut self.video_demand,
            Role::Audio => &mut self.audio_demand,
        }
    }

    fn set_demand_field(&mut self, role: Role, demand: Option<Demand>) {
        match role {
            Role::Video => self.video_demand = demand,
            Role::Audio => self.audio_demand = demand,
        }
    }
}

/// Values touched only by the pump task, so they need no synchronization.
#[derive(Default)]
struct PumpLocal {
    empty_responses: i32,
    substituted_responses: i32,
    consecutive_redirects: i32,
    consecutive_errors: i32,
    backoff_notified: bool,
    backoff_shown: bool,
    last_wait_log_ms: i64,
    media_bytes: i64,
    media_us_delivered: i64,
    throughput_bytes_per_sec: i64,
    demanded_headers: i32,
    foreign_headers: i32,
}

struct Shared {
    transport: SabrTransport,
    ustreamer_config: Vec<u8>,
    video_id: String,
    client_info: ClientInfo,
    po_token_decoded: Option<Vec<u8>>,
    is_live: bool,
    duration_us: i64,
    created_at_ms: i64,

    state: Mutex<State>,
    /// Wakes the pump task when demand, backoff, or seek state changes. The
    /// `State` mutex above guards the data (held only briefly, never across an
    /// `.await`). This only signals.
    notify: Notify,
    buffers: Mutex<HashMap<SabrFormatKey, Arc<SabrTrackBuffer>>>,
    listener: Mutex<Option<Arc<SabrSessionListener>>>,

    released: AtomicBool,
    request_number: AtomicI32,
    /// Bumped whenever the *server* repositions us (a `SABR_SEEK` part, or a
    /// live seekable-window clamp). Distinct from a client-driven restart.
    /// Consumers watch this to abandon their current read position and re-sync
    /// from the new one, since after a server seek the segment sequence is
    /// discontinuous and a sequence cursor would otherwise wait forever.
    server_seek_generation: AtomicU64,
}

/// A SABR session. Cheaply cloneable handle around shared state. The pump runs
/// as a task via [`SabrSession::run`].
#[derive(Clone)]
pub struct SabrSession {
    shared: Arc<Shared>,
}

impl SabrSession {
    pub fn new(spec: SabrStreamSpec, transport: SabrTransport) -> Self {
        let po_token_decoded = spec.po_token.as_deref().and_then(decode_base64_lenient);
        if spec.po_token.is_some() && po_token_decoded.is_none() {
            log::error!(
                "sabr: po token is not valid base64; requests will be unattested and will be \
                 blocked once the attestation grace period expires"
            );
        }

        let client_info = spec.build_client_info();
        let now = now_ms();
        let state = State {
            streaming_url: spec.server_abr_streaming_url.clone(),
            playback_cookie: None,
            sabr_contexts: HashMap::new(),
            active_sabr_contexts: BTreeSet::new(),
            format_initialization: HashMap::new(),
            format_complete: HashSet::new(),
            format_no_progress: HashMap::new(),
            server_chosen: HashMap::new(),
            demanded_keys: HashSet::new(),
            video_demand: None,
            audio_demand: None,
            playback_position_us: 0,
            resume_position_us: None,
            restart_from_us: NO_US,
            seek_pending_us: None,
            last_sabr_seek_us: NO_US,
            restart_epoch: 0,
            target_video_readahead_ms: DEFAULT_READAHEAD_MS,
            target_audio_readahead_ms: DEFAULT_READAHEAD_MS,
            backoff_until_ms: 0,
            server_backoff_until_ms: 0,
            error_backoff_until_ms: 0,
            aborting: false,
            last_action_ms: now,
            last_request_ms: 0,
            media_base_us: 0,
            media_base_set: false,
            live_metadata: None,
            reestimated: false,
            viewport_width: 0,
            viewport_height: 0,
            initial_bandwidth: 0,
            keep_behind_us: DEFAULT_KEEP_BEHIND_US,
            min_readahead_ms: 0,
            max_readahead_ms: 0,
            fatal: None,
        };

        Self {
            shared: Arc::new(Shared {
                transport,
                ustreamer_config: spec.ustreamer_config,
                video_id: spec.video_id,
                client_info,
                po_token_decoded,
                is_live: spec.is_live,
                duration_us: spec.duration_us,
                created_at_ms: now,
                state: Mutex::new(state),
                notify: Notify::new(),
                buffers: Mutex::new(HashMap::new()),
                listener: Mutex::new(None),
                released: AtomicBool::new(false),
                request_number: AtomicI32::new(0),
                server_seek_generation: AtomicU64::new(0),
            }),
        }
    }

    pub fn video_id(&self) -> &str {
        &self.shared.video_id
    }

    pub fn is_live(&self) -> bool {
        self.shared.is_live
    }

    pub fn duration_us(&self) -> i64 {
        self.shared.duration_us
    }

    pub fn is_released(&self) -> bool {
        self.shared.released.load(Ordering::Acquire)
    }

    /// A counter bumped every time the *server* repositions the stream (a
    /// `SABR_SEEK`, or a live seekable-window clamp). A consumer that pulls
    /// segments in sequence order should snapshot this and restart its read
    /// from the buffer front whenever it changes, since the sequence numbering
    /// is discontinuous across a server seek.
    pub fn server_seek_generation(&self) -> u64 {
        self.shared.server_seek_generation.load(Ordering::Acquire)
    }

    pub fn fatal_error(&self) -> Option<String> {
        self.shared.state.lock().fatal.clone()
    }

    pub fn set_listener(&self, listener: Option<Arc<SabrSessionListener>>) {
        *self.shared.listener.lock() = listener;
    }

    pub fn set_viewport(&self, width: i32, height: i32) {
        let mut state = self.shared.state.lock();
        state.viewport_width = width;
        state.viewport_height = height;
    }

    pub fn set_initial_bandwidth(&self, bytes_per_sec: i64) {
        self.shared.state.lock().initial_bandwidth = bytes_per_sec;
    }

    /// The buffer for a format, creating it on first use.
    pub fn buffer_for(&self, format: &SabrFormat) -> Arc<SabrTrackBuffer> {
        self.buffer_for_key(&format.key())
    }

    pub fn buffer_for_key(&self, key: &SabrFormatKey) -> Arc<SabrTrackBuffer> {
        let mut buffers = self.shared.buffers.lock();
        buffers
            .entry(key.clone())
            .or_insert_with(|| Arc::new(SabrTrackBuffer::new(key.clone())))
            .clone()
    }

    pub fn format_initialization_for(
        &self,
        format: &SabrFormat,
    ) -> Option<FormatInitializationMetadata> {
        self.shared
            .state
            .lock()
            .format_initialization
            .get(&format.key())
            .cloned()
    }

    pub fn active_format(&self, role: Role) -> Option<SabrFormat> {
        self.shared
            .state
            .lock()
            .demand(role)
            .as_ref()
            .map(|d| d.format.clone())
    }

    /// The key of the currently active format for `role`, if any. Cheaper than
    /// [`SabrSession::active_format`] when the caller only needs identity, e.g.
    /// to detect a server-driven ABR switch without cloning the whole format.
    pub fn active_format_key(&self, role: Role) -> Option<SabrFormatKey> {
        self.shared
            .state
            .lock()
            .demand(role)
            .as_ref()
            .map(|d| d.format.key())
    }

    /// Run the pump to completion. The embedder spawns this on its async
    /// runtime (e.g. `runtime.spawn(session.clone().into_pump())`). It returns
    /// once the session is released or hits a fatal error. A no-op if already
    /// released or failed.
    pub async fn run(&self) {
        {
            let state = self.shared.state.lock();
            if self.is_released() || state.fatal.is_some() {
                return;
            }
        }
        pump(self.shared.clone()).await;
    }

    /// Convenience: consume a cloned handle into the pump future, so callers can
    /// `runtime.spawn(session.clone().into_pump())` without borrowing lifetimes.
    pub async fn into_pump(self) {
        self.run().await;
    }

    pub fn release(&self) {
        if self.shared.released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.notify.notify_waiters();
        for buffer in self.shared.buffers.lock().values() {
            buffer.notify_changed();
        }
        *self.shared.listener.lock() = None;
    }

    pub fn set_demand(&self, role: Role, format: SabrFormat, from_us: i64) {
        self.set_demand_alternates(role, vec![format], from_us);
    }

    pub fn set_demand_alternates(&self, role: Role, acceptable: Vec<SabrFormat>, from_us: i64) {
        if acceptable.is_empty() {
            return;
        }
        let mut state = self.shared.state.lock();
        let previous = state.demand(role).clone();

        let active = previous
            .as_ref()
            .and_then(|p| {
                acceptable
                    .iter()
                    .find(|a| a.key() == p.format.key())
                    .cloned()
            })
            .or_else(|| {
                state.server_chosen.get(&role).and_then(|chosen| {
                    acceptable.iter().find(|a| a.key() == *chosen).cloned()
                })
            })
            .unwrap_or_else(|| acceptable[0].clone());

        if let Some(prev) = &previous {
            let same_alternates = prev.alternates.len() == acceptable.len()
                && prev
                    .alternates
                    .iter()
                    .zip(&acceptable)
                    .all(|(a, b)| a.key() == b.key());
            if prev.format.key() == active.key() && prev.from_us == from_us && same_alternates {
                return;
            }
        }

        if previous.as_ref().map(|p| p.format.key()) != Some(active.key()) {
            state.last_action_ms = now_ms();
            state.format_complete.remove(&active.key());
            state.format_no_progress.remove(&active.key());
        }

        for a in &acceptable {
            state.demanded_keys.insert(a.key());
        }

        state.set_demand_field(
            role,
            Some(Demand {
                format: active,
                from_us,
                alternates: acceptable,
            }),
        );
        self.shared.notify.notify_waiters();
    }

    pub fn clear_demand(&self, role: Role) {
        let mut state = self.shared.state.lock();
        state.set_demand_field(role, None);
    }

    /// Advance only the `from_us` of the existing demand for `role`. A cheap
    /// readahead-window bump for a consumer feeding in sequence order, with no
    /// format re-selection and no allocation, unlike
    /// [`SabrSession::set_demand_alternates`]. A no-op if there is no demand for
    /// `role` or `from_us` is unchanged. Format selection stays with the pump
    /// (see `adopt_server_format`).
    pub fn advance_demand(&self, role: Role, from_us: i64) {
        let mut state = self.shared.state.lock();
        match state.demand_mut(role).as_mut() {
            Some(d) if d.from_us != from_us => d.from_us = from_us,
            _ => return,
        }
        self.shared.notify.notify_waiters();
    }

    pub fn set_playback_position(&self, position_us: i64) {
        self.shared.state.lock().playback_position_us = position_us;
    }

    /// Seek to `from_us`. If already buffered, just re-anchor, otherwise restart.
    pub fn seek_to(&self, from_us: i64) {
        let mut state = self.shared.state.lock();
        let demands: Vec<Demand> = [state.video_demand.clone(), state.audio_demand.clone()]
            .into_iter()
            .flatten()
            .collect();
        let buffered = !demands.is_empty()
            && demands.iter().all(|d| {
                let seg = self.buffer_for(&d.format).first_covering(from_us);
                seg.map(|s| s.start_us <= from_us).unwrap_or(false)
            });
        if !buffered {
            drop(state);
            self.restart(from_us, false);
            return;
        }
        state.playback_position_us = from_us;
        reanchor_demands(&mut state, from_us);
        state.last_action_ms = now_ms();
        self.shared.notify.notify_waiters();
    }

    /// Restart fetching from `from_us`. Returns `true` if a restart actually
    /// happened, `false` if it was coalesced with an in-flight restart to the
    /// same position (only when `force` is false).
    pub fn restart(&self, from_us: i64, force: bool) -> bool {
        let mut state = self.shared.state.lock();
        if !force
            && let Some(current) = state.resume_position_us
            && (current - from_us).abs() < RESTART_TOLERANCE_US
        {
            return false;
        }
        state.playback_position_us = from_us;
        state.resume_position_us = Some(from_us);
        state.restart_from_us = from_us;
        state.last_action_ms = now_ms();
        state.restart_epoch += 1;
        state.format_complete.clear();
        state.format_no_progress.clear();

        for buffer in self.shared.buffers.lock().values() {
            buffer.clear();
        }
        reanchor_demands(&mut state, from_us);

        state.backoff_until_ms = state.server_backoff_until_ms.max(state.error_backoff_until_ms);
        state.aborting = true;
        self.shared.notify.notify_waiters();
        true
    }
}

fn reanchor_demands(state: &mut State, from_us: i64) {
    if let Some(d) = state.video_demand.as_mut() {
        d.from_us = from_us;
    }
    if let Some(d) = state.audio_demand.as_mut() {
        d.from_us = from_us;
    }
}

// --- pump task ---

async fn pump(shared: Arc<Shared>) {
    let mut local = PumpLocal::default();
    while !shared.released.load(Ordering::Acquire) {
        match pump_once(&shared, &mut local).await {
            Ok(()) => {
                local.consecutive_errors = 0;
                let mut state = shared.state.lock();
                state.aborting = false;
            }
            Err(PumpStep::Idle) => {}
            Err(PumpStep::Backoff) => {}
            Err(PumpStep::Fatal(err)) => {
                set_fatal(&shared, &err);
                return;
            }
            Err(PumpStep::Error(err)) => {
                if shared.released.load(Ordering::Acquire) {
                    return;
                }
                log::error!("sabr: request failed: {err}");
                notify_error(&shared, &err);
                local.consecutive_errors += 1;
                if local.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    set_fatal(&shared, &err);
                    return;
                }
                let shift = ((local.consecutive_errors - 1) as u32).min(ERROR_BACKOFF_MAX_SHIFT);
                let delay = (ERROR_BACKOFF_BASE_MS << shift).min(ERROR_BACKOFF_MAX_MS);
                let mut state = shared.state.lock();
                state.error_backoff_until_ms = now_ms() + delay;
                state.backoff_until_ms =
                    state.server_backoff_until_ms.max(state.error_backoff_until_ms);
            }
        }
    }
}

/// A step outcome that is not a normal "advanced" success.
enum PumpStep {
    Idle,
    Backoff,
    Fatal(SabrError),
    Error(SabrError),
}

fn set_fatal(shared: &Arc<Shared>, err: &SabrError) {
    shared.state.lock().fatal = Some(err.to_string());
    notify_error(shared, err);
    for buffer in shared.buffers.lock().values() {
        buffer.notify_changed();
    }
}

fn notify_error(shared: &Arc<Shared>, err: &SabrError) {
    emit(shared, SabrSessionEvent::SessionError(err));
}

async fn pump_once(shared: &Arc<Shared>, local: &mut PumpLocal) -> Result<(), PumpStep> {
    // Wait for demand. Register interest (`enable`) before checking so a demand
    // change racing the check still wakes us promptly.
    loop {
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = shared.state.lock();
            if shared.released.load(Ordering::Acquire) || needs_data(shared, &state) {
                break;
            }
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(PUMP_IDLE_POLL_MS),
            notified.as_mut(),
        )
        .await;
    }
    if shared.released.load(Ordering::Acquire) {
        return Err(PumpStep::Idle);
    }

    // Honour backoff. Listener events are emitted *after* the state guard is
    // dropped: emitting under the lock lets a re-entrant listener (any callback
    // that touches a state-locking `SabrSession` method) deadlock the pump.
    let mut emit_backoff_delay: Option<i64> = None;
    let mut emit_backoff_ended = false;
    let mut backoff_wait_ms: Option<u64> = None;
    {
        let state = shared.state.lock();
        let now = now_ms();
        let wait_ms = state.backoff_until_ms - now;
        let server_wait_ms = state.server_backoff_until_ms - now;
        if wait_ms > 0 {
            if server_wait_ms > 0 {
                if !local.backoff_notified {
                    local.backoff_notified = true;
                    if starved(shared, &state) {
                        local.backoff_shown = true;
                        emit_backoff_delay = Some(server_wait_ms);
                    }
                }
            } else if now - local.last_wait_log_ms > 1_000 {
                local.last_wait_log_ms = now;
            }
            backoff_wait_ms = Some(wait_ms.min(PUMP_IDLE_POLL_MS as i64).max(0) as u64);
        } else if local.backoff_notified {
            local.backoff_notified = false;
            if local.backoff_shown {
                local.backoff_shown = false;
                emit_backoff_ended = true;
            }
        }
    }
    if let Some(delay_ms) = emit_backoff_delay {
        emit(shared, SabrSessionEvent::Backoff { delay_ms });
    }
    if emit_backoff_ended {
        emit(shared, SabrSessionEvent::BackoffEnded);
    }
    if let Some(capped) = backoff_wait_ms {
        // A shorter-than-`capped` wake (e.g. a restart notifying us) just
        // re-enters the loop and re-checks. A missed wake costs at most `capped`.
        let _ = tokio::time::timeout(Duration::from_millis(capped), shared.notify.notified()).await;
        return Err(PumpStep::Backoff);
    }

    evict_consumed_segments(shared);
    perform_request(shared, local).await
}

async fn perform_request(shared: &Arc<Shared>, local: &mut PumpLocal) -> Result<(), PumpStep> {
    let start_epoch;
    let video;
    let audio;
    let requested_resume;
    let position_us;
    let streaming_url;
    {
        let mut state = shared.state.lock();
        state.aborting = false;
        start_epoch = state.restart_epoch;
        video = state.video_demand.as_ref().map(|d| d.format.clone());
        audio = state.audio_demand.as_ref().map(|d| d.format.clone());
        if video.is_none() && audio.is_none() {
            return Err(PumpStep::Idle);
        }
        requested_resume = state.resume_position_us;
        position_us = request_position_us(shared, &state);
        streaming_url = state.streaming_url.clone();
    }
    local.demanded_headers = 0;
    local.foreign_headers = 0;

    log::debug!(
        "sabr: request live={} playerTimeMs={} video={:?} audio={:?}",
        shared.is_live,
        position_us / 1000,
        video.as_ref().map(|f| f.itag),
        audio.as_ref().map(|f| f.itag),
    );

    let body = build_request(shared, local, &video, &audio, position_us / 1000).encode_to_vec();
    let url = append_request_number(shared, &streaming_url);

    let headers = vec![
        ("Content-Type", "application/x-protobuf".to_owned()),
        ("Accept", "application/vnd.yt-ump".to_owned()),
        ("Accept-Encoding", "identity".to_owned()),
        ("Origin", "https://www.youtube.com".to_owned()),
        ("Referer", "https://www.youtube.com/".to_owned()),
    ];

    let (video_count_before, video_init_before) = count_and_init(shared, &video);
    let (audio_count_before, audio_init_before) = count_and_init(shared, &audio);

    shared.state.lock().last_request_ms = now_ms();
    let sent_ms = now_ms();

    let (status, resp_body) = shared
        .transport
        .fetch(url, body, headers)
        .await
        .map_err(PumpStep::Error)?;

    if shared.released.load(Ordering::Acquire) {
        return Err(PumpStep::Idle);
    }
    if !(200..300).contains(&status) {
        return Err(if status == 403 {
            PumpStep::Fatal(SabrError::Blocked("SABR request returned HTTP 403".into()))
        } else {
            PumpStep::Error(SabrError::Http(format!(
                "SABR request returned HTTP {status}"
            )))
        });
    }

    {
        let mut state = shared.state.lock();
        if state.resume_position_us == requested_resume {
            state.resume_position_us = None;
        }
    }

    let accepted_keys = {
        let state = shared.state.lock();
        let mut keys = HashSet::new();
        for d in [&state.video_demand, &state.audio_demand].into_iter().flatten() {
            for a in &d.alternates {
                keys.insert(a.key());
            }
        }
        keys
    };

    let bytes_before = local.media_bytes;
    let media_us_before = local.media_us_delivered;
    let consume_result = consume(
        shared,
        local,
        resp_body,
        position_us,
        &accepted_keys,
        start_epoch,
    )
    .await;
    let elapsed = now_ms() - sent_ms;
    record_throughput(
        local,
        local.media_bytes - bytes_before,
        elapsed,
        local.media_us_delivered - media_us_before,
    );
    let redirected = consume_result?;

    clear_seek_if_landed(shared);

    {
        let state = shared.state.lock();
        if state.restart_epoch != start_epoch {
            drop(state);
            for buffer in shared.buffers.lock().values() {
                buffer.clear();
            }
            local.empty_responses = 0;
            return Ok(());
        }
    }

    let advanced = has_advanced(
        shared,
        &video,
        video_count_before,
        video_init_before,
        &audio,
        audio_count_before,
        audio_init_before,
    );

    log::debug!(
        "sabr: response advanced={advanced} redirected={redirected} mediaBytes={} demandedHeaders={} foreignHeaders={}",
        local.media_bytes - bytes_before,
        local.demanded_headers,
        local.foreign_headers,
    );

    if advanced {
        local.empty_responses = 0;
        local.substituted_responses = 0;
        local.consecutive_redirects = 0;
        let mut state = shared.state.lock();
        state.error_backoff_until_ms = 0;
        state.backoff_until_ms = state.server_backoff_until_ms;
    }

    let aborting = shared.state.lock().aborting;

    if shared.is_live {
        if !aborting && !redirected {
            clamp_to_seekable_window(shared);
        }
        if !advanced && !redirected {
            let mut state = shared.state.lock();
            state.backoff_until_ms = state.backoff_until_ms.max(now_ms() + LIVE_POLL_MS);
        }
        return Ok(());
    }

    if !advanced && !aborting && !redirected {
        update_progress(shared, &video, position_us);
        update_progress(shared, &audio, position_us);
        if (video.is_none() || is_complete(shared, video.as_ref().unwrap()))
            && (audio.is_none() || is_complete(shared, audio.as_ref().unwrap()))
        {
            return Ok(());
        }

        if local.demanded_headers == 0 && local.foreign_headers > 0 {
            local.substituted_responses += 1;
            if local.substituted_responses >= MAX_SUBSTITUTED_RESPONSES {
                return Err(PumpStep::Fatal(SabrError::FormatSubstituted(format!(
                    "server served a different format for {} consecutive requests; the caller's \
                     format list is out of sync",
                    local.substituted_responses
                ))));
            }
        }

        local.empty_responses += 1;
        let shift = ((local.empty_responses - 1) as u32).min(ERROR_BACKOFF_MAX_SHIFT);
        let delay = (EMPTY_BACKOFF_BASE_MS << shift).min(ERROR_BACKOFF_MAX_MS);
        {
            let mut state = shared.state.lock();
            state.backoff_until_ms = state.backoff_until_ms.max(now_ms() + delay);
        }
        if local.empty_responses >= MAX_EMPTY_RESPONSES {
            return Err(PumpStep::Error(SabrError::Protocol(format!(
                "server returned no media for {} consecutive requests",
                local.empty_responses
            ))));
        }
    } else if !aborting && !redirected {
        update_progress(shared, &video, position_us);
        update_progress(shared, &audio, position_us);
    }

    Ok(())
}

/// Consume the UMP response stream. Returns whether a redirect was issued.
async fn consume(
    shared: &Arc<Shared>,
    local: &mut PumpLocal,
    body: SabrBody,
    requested_position_us: i64,
    requested_keys: &HashSet<SabrFormatKey>,
    start_epoch: i32,
) -> Result<bool, PumpStep> {
    let mut reader = UmpReader::new(body);
    // Value carries the destination buffer alongside the segment so the hot
    // `MEDIA` / `MEDIA_END` path doesn't re-lock `shared.buffers` and clone an
    // `Arc` for every chunk.
    let mut pending: HashMap<i32, (Arc<SabrSegment>, Arc<SabrTrackBuffer>)> = HashMap::new();
    let mut redirect: Option<String> = None;
    let mut seek_to_us: Option<i64> = None;

    let result: Result<(), PumpStep> = async {
        loop {
            if shared.released.load(Ordering::Acquire) {
                break;
            }
            if shared.state.lock().restart_epoch != start_epoch {
                break;
            }
            let part = match reader.next().await.map_err(|e| PumpStep::Error(SabrError::Io(e)))? {
                Some(p) => p,
                None => break,
            };
            match part.ty {
                PartType::MediaHeader => {
                    let header = MediaHeader::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    on_media_header(shared, local, header, &mut pending, requested_keys);
                }
                PartType::Media => {
                    let (header_id, offset) = crate::ump::decode_varint(&part.data, 0);
                    local.media_bytes += (part.data.len() - offset) as i64;
                    if let Some((segment, buffer)) = pending.get(&(header_id as i32)) {
                        segment.append(&part.data[offset..]);
                        buffer.notify_changed();
                    }
                }
                PartType::MediaEnd => {
                    let (header_id, _) = crate::ump::decode_varint(&part.data, 0);
                    if let Some((segment, buffer)) = pending.remove(&(header_id as i32)) {
                        if segment.content_length > 0
                            && segment.size() != segment.content_length as usize
                        {
                            log::debug!(
                                "sabr: dropping truncated seq={} itag={} got={} want={}",
                                segment.sequence_number,
                                segment.format_key.itag,
                                segment.size(),
                                segment.content_length
                            );
                            buffer.discard(&segment);
                        } else {
                            segment.mark_complete();
                            buffer.notify_changed();
                        }
                    }
                }
                PartType::NextRequestPolicy => {
                    let policy = NextRequestPolicy::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    on_next_request_policy(shared, policy);
                }
                PartType::FormatInitializationMetadata => {
                    let metadata = FormatInitializationMetadata::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    let fid = metadata.format_id.clone().unwrap_or_default();
                    let key = SabrFormatKey::of(fid.itag, fid.lmt, Some(&fid.xtags));
                    shared
                        .state
                        .lock()
                        .format_initialization
                        .insert(key, metadata.clone());
                    emit(shared, SabrSessionEvent::FormatInitialization(&metadata));
                }
                PartType::LiveMetadata => {
                    let metadata = LiveMetadata::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    on_live_metadata(shared, metadata);
                }
                PartType::SabrContextUpdate => {
                    let update = SabrContextUpdate::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    let mut state = shared.state.lock();
                    state.sabr_contexts.insert(
                        update.r#type,
                        SabrContext {
                            r#type: update.r#type,
                            value: update.value.clone(),
                        },
                    );
                    if update.send_by_default {
                        state.active_sabr_contexts.insert(update.r#type);
                    }
                }
                PartType::SabrContextSendingPolicy => {
                    let policy = SabrContextSendingPolicy::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    let mut state = shared.state.lock();
                    for t in policy.start_policy {
                        state.active_sabr_contexts.insert(t);
                    }
                    for t in policy.stop_policy {
                        state.active_sabr_contexts.remove(&t);
                    }
                    for t in policy.discard_policy {
                        state.sabr_contexts.remove(&t);
                        state.active_sabr_contexts.remove(&t);
                    }
                }
                PartType::StreamProtectionStatus => {
                    let status = StreamProtectionStatus::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    if status.status == PROTECTION_STATUS_ATTESTATION_REQUIRED {
                        return Err(PumpStep::Fatal(SabrError::Blocked(
                            "po token rejected (attestation required)".into(),
                        )));
                    }
                }
                PartType::SabrRedirect => {
                    redirect = Some(
                        SabrRedirect::decode(part.data.as_slice())
                            .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?
                            .url,
                    );
                }
                PartType::SabrSeek => {
                    let seek = SabrSeek::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    let scale = seek.seek_media_timescale;
                    seek_to_us = if scale > 0 {
                        Some(ticks_to_us(seek.seek_media_time, scale))
                    } else {
                        None
                    };
                    log::debug!(
                        "sabr: SABR_SEEK source={} time={} scale={} -> {:?}us",
                        seek.seek_source,
                        seek.seek_media_time,
                        scale,
                        seek_to_us,
                    );
                }
                PartType::SabrError => {
                    let error = SabrErrorPart::decode(part.data.as_slice())
                        .map_err(|e| PumpStep::Error(SabrError::Decode(e)))?;
                    return Err(PumpStep::Error(SabrError::Protocol(format!(
                        "SABR error {} {}",
                        error.code, error.r#type
                    ))));
                }
                PartType::ReloadPlayerResponse => {
                    return Err(PumpStep::Fatal(SabrError::ReloadRequired(
                        "server asked for a fresh player response".into(),
                    )));
                }
                PartType::SnackbarMessage => {
                    let _ = SnackbarMessage::decode(part.data.as_slice());
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;

    // Discard any still-pending (incomplete) segments.
    for (segment, buffer) in pending.values() {
        buffer.discard(segment);
        buffer.notify_changed();
    }

    result?;

    if let Some(url) = redirect {
        log::info!("sabr: redirect issued");
        local.consecutive_redirects += 1;
        {
            let mut state = shared.state.lock();
            state.streaming_url = url;
            state.backoff_until_ms = state.server_backoff_until_ms;
            state.resume_position_us = Some(requested_position_us);
        }
        if local.consecutive_redirects >= MAX_REDIRECTS {
            return Err(PumpStep::Error(SabrError::Protocol(format!(
                "SABR redirected {} times without delivering media",
                local.consecutive_redirects
            ))));
        }
        return Ok(true);
    }

    if let Some(seek) = seek_to_us {
        apply_sabr_seek(shared, seek, requested_position_us);
    }
    Ok(false)
}

fn on_media_header(
    shared: &Arc<Shared>,
    local: &mut PumpLocal,
    header: MediaHeader,
    pending: &mut HashMap<i32, (Arc<SabrSegment>, Arc<SabrTrackBuffer>)>,
    requested_keys: &HashSet<SabrFormatKey>,
) {
    let key = SabrFormatKey::of(header.itag, header.lmt, Some(&header.xtags));
    let buffer = self_buffer(shared, &key);

    log::debug!(
        "sabr: MediaHeader itag={} lmt={} seq={} init={} startMs={} durMs={} len={}",
        header.itag,
        header.lmt,
        header.sequence_number,
        header.is_init_segment,
        header.start_ms,
        header.duration_ms,
        header.content_length,
    );

    if requested_keys.contains(&key) {
        local.demanded_headers += 1;
        adopt_server_format(shared, &key);
    } else if !shared.state.lock().demanded_keys.contains(&key) {
        local.foreign_headers += 1;
    }

    let existing = if header.is_init_segment {
        buffer.init_segment()
    } else {
        buffer.get(header.sequence_number)
    };
    if let Some(existing) = existing
        && existing.is_complete()
    {
        let placeholder = Arc::new(SabrSegment::new(
            key,
            header.sequence_number,
            header.is_init_segment,
            0,
            0,
            0,
            0,
            0,
        ));
        pending.insert(header.header_id, (placeholder, buffer));
        return;
    }

    let tr = header.time_range.unwrap_or_default();
    let timescale = tr.timescale;
    let (start_us, mut duration_us) = if timescale > 0 {
        (
            ticks_to_us(tr.start_ticks, timescale),
            ticks_to_us(tr.duration_ticks, timescale),
        )
    } else {
        (header.start_ms * 1000, header.duration_ms * 1000)
    };
    let exact = duration_us > 0;
    if !header.is_init_segment {
        back_patch_previous(shared, &buffer, header.sequence_number, start_us);
        if !exact {
            duration_us = estimate_segment_us(shared, header.sequence_number, start_us, &buffer);
        }
        // Single lock: chaining two `shared.state.lock()` calls in one
        // expression keeps the first guard alive across the `.or_else`, so the
        // second lock self-deadlocks (parking_lot is non-reentrant). This path
        // is only hit when `video_demand` is `None`, i.e. audio-only streams.
        let clock = {
            let state = shared.state.lock();
            state
                .video_demand
                .as_ref()
                .map(|d| d.format.key())
                .or_else(|| state.audio_demand.as_ref().map(|d| d.format.key()))
        };
        if clock.as_ref() == Some(&key) {
            local.media_us_delivered += duration_us;
        }
    }

    let segment = Arc::new(SabrSegment::new(
        key,
        header.sequence_number,
        header.is_init_segment,
        start_us,
        duration_us,
        header.content_length as i32,
        if timescale > 0 { tr.start_ticks } else { 0 },
        timescale,
    ));
    if exact {
        segment.set_duration(duration_us, true);
    }
    pending.insert(header.header_id, (segment.clone(), buffer.clone()));
    buffer.announce(segment);
}

fn estimate_segment_us(
    shared: &Arc<Shared>,
    sequence: i32,
    start_us: i64,
    buffer: &SabrTrackBuffer,
) -> i64 {
    let cadence = live_cadence_us(shared, buffer);
    if cadence > 0 {
        return cadence;
    }
    if let Some(lm) = shared.state.lock().live_metadata
        && lm.head_sequence_number > sequence
    {
        let segments = (lm.head_sequence_number - sequence) as i64;
        let span_us = lm.head_sequence_time_ms * 1000 - start_us;
        if span_us > 0 {
            return (span_us / segments).max(1);
        }
    }
    if let Some(prev) = buffer.get(sequence - 1)
        && prev.duration_us() > 0
    {
        return prev.duration_us();
    }
    DEFAULT_LIVE_SEGMENT_US
}

fn live_cadence_us(shared: &Arc<Shared>, buffer: &SabrTrackBuffer) -> i64 {
    let mut observed = buffer.recent_start_deltas_us(8);
    if !observed.is_empty() {
        observed.sort_unstable();
        return observed[observed.len() / 2].max(1);
    }
    let lm = match shared.state.lock().live_metadata {
        Some(lm) => lm,
        None => return -1,
    };
    let anchor = match buffer.first_at_or_after(-1) {
        Some(a) => a,
        None => return -1,
    };
    if lm.head_sequence_number <= anchor.sequence_number {
        return -1;
    }
    let span_us = lm.head_sequence_time_ms * 1000 - anchor.start_us;
    if span_us <= 0 {
        return -1;
    }
    (span_us / (lm.head_sequence_number - anchor.sequence_number) as i64).max(1)
}

fn back_patch_previous(
    shared: &Arc<Shared>,
    buffer: &SabrTrackBuffer,
    sequence: i32,
    start_us: i64,
) {
    let prev = match buffer.get(sequence - 1) {
        Some(p) => p,
        None => return,
    };
    if prev.duration_exact() {
        return;
    }
    let delta_us = start_us - prev.start_us;
    if delta_us <= 0 {
        return;
    }
    let cadence = {
        let c = live_cadence_us(shared, buffer);
        if c > 0 { c } else { prev.duration_us() }
    };
    if cadence > 0 && delta_us > cadence * 3 / 2 {
        return;
    }
    prev.set_duration(delta_us, true);
}

fn on_live_metadata(shared: &Arc<Shared>, metadata: LiveMetadata) {
    let first;
    {
        let mut state = shared.state.lock();
        first = !state.reestimated;
        state.reestimated = true;
        state.live_metadata = Some(metadata);
        if shared.is_live && !state.media_base_set && metadata.min_seekable_timescale > 0 {
            state.media_base_us = ticks_to_us(
                metadata.min_seekable_time_ticks,
                metadata.min_seekable_timescale,
            );
            state.media_base_set = true;
        }
    }
    if first {
        reestimate_inexact_durations(shared);
    }
    if metadata.min_seekable_timescale > 0 && metadata.max_seekable_timescale > 0 {
        log::debug!(
            "sabr: live metadata window=[{}us,{}us] headSeq={} headTimeMs={}",
            ticks_to_us(metadata.min_seekable_time_ticks, metadata.min_seekable_timescale),
            ticks_to_us(metadata.max_seekable_time_ticks, metadata.max_seekable_timescale),
            metadata.head_sequence_number,
            metadata.head_sequence_time_ms,
        );
    }
    emit(shared, SabrSessionEvent::LiveMetadata(&metadata));
}

fn reestimate_inexact_durations(shared: &Arc<Shared>) {
    let buffers: Vec<Arc<SabrTrackBuffer>> =
        shared.buffers.lock().values().cloned().collect();
    for buffer in buffers {
        for segment in buffer.snapshot() {
            if segment.duration_exact() || segment.is_init {
                continue;
            }
            if let Some(next) = buffer.get(segment.sequence_number + 1) {
                back_patch_previous(shared, &buffer, next.sequence_number, next.start_us);
            }
            if segment.duration_exact() {
                continue;
            }
            let est = estimate_segment_us(shared, segment.sequence_number, segment.start_us, &buffer);
            segment.set_duration(est, false);
        }
    }
}

fn on_next_request_policy(shared: &Arc<Shared>, policy: NextRequestPolicy) {
    let mut state = shared.state.lock();
    if !policy.playback_cookie.is_empty() {
        state.playback_cookie = Some(policy.playback_cookie);
    }
    if policy.target_video_readahead_ms > 0 {
        state.target_video_readahead_ms = policy.target_video_readahead_ms as i64;
    }
    if policy.target_audio_readahead_ms > 0 {
        state.target_audio_readahead_ms = policy.target_audio_readahead_ms as i64;
    }
    if policy.backoff_time_ms > 0 {
        state.server_backoff_until_ms = now_ms() + policy.backoff_time_ms as i64;
        state.backoff_until_ms = state.backoff_until_ms.max(state.server_backoff_until_ms);
    }
}

fn adopt_server_format(shared: &Arc<Shared>, key: &SabrFormatKey) {
    let mut state = shared.state.lock();
    for role in [Role::Video, Role::Audio] {
        let demand = match state.demand(role).clone() {
            Some(d) => d,
            None => continue,
        };
        if demand.format.key() == *key || demand.alternates.len() <= 1 {
            continue;
        }
        if let Some(chosen) = demand.alternates.iter().find(|a| a.key() == *key).cloned() {
            state.server_chosen.insert(role, key.clone());
            state.set_demand_field(
                role,
                Some(Demand {
                    format: chosen,
                    from_us: demand.from_us,
                    alternates: demand.alternates,
                }),
            );
            return;
        }
    }
}

fn build_request(
    shared: &Arc<Shared>,
    local: &PumpLocal,
    video: &Option<SabrFormat>,
    audio: &Option<SabrFormat>,
    position_ms: i64,
) -> VideoPlaybackAbrRequest {
    let now = now_ms();
    let state = shared.state.lock();

    let bandwidth = if local.throughput_bytes_per_sec > 0 {
        local.throughput_bytes_per_sec
    } else if state.initial_bandwidth > 0 {
        state.initial_bandwidth
    } else {
        BANDWIDTH_ESTIMATE
    };

    let mut abr = ClientAbrState {
        player_time_ms: Some(position_ms),
        bandwidth_estimate: bandwidth,
        network_latency_ms: rand::random_range(7..97),
        time_since_last_action_ms: now - state.last_action_ms,
        time_since_last_manual_format_selection_ms: now - shared.created_at_ms,
        last_manual_direction: 0,
        drc_enabled: true,
        visibility: Some(0),
        prefer_vp9: Some(false),
        ..Default::default()
    };
    if state.last_request_ms > 0 {
        abr.time_since_last_request_ms = now - state.last_request_ms;
    }

    let video_alternates = state
        .video_demand
        .as_ref()
        .map(|d| d.alternates.clone())
        .unwrap_or_default();
    let audio_alternates = state
        .audio_demand
        .as_ref()
        .map(|d| d.alternates.clone())
        .unwrap_or_default();

    if let Some(v) = video {
        let cap = video_alternates
            .iter()
            .max_by_key(|f| f.height)
            .cloned()
            .unwrap_or_else(|| v.clone());
        abr.client_viewport_width =
            if state.viewport_width > 0 { state.viewport_width } else { cap.width } as i64;
        abr.client_viewport_height =
            if state.viewport_height > 0 { state.viewport_height } else { cap.height } as i64;
        if video_alternates.len() <= 1 {
            abr.last_manual_selected_resolution = v.height as i64;
            abr.sticky_resolution = v.height as i64;
            abr.selected_quality_height = v.height as i64;
        }
        if audio.is_none() {
            abr.enabled_track_types_bitfield = MediaType::Video as i32;
        }
    } else if audio.is_some() {
        abr.enabled_track_types_bitfield = MediaType::Audio as i32;
    }

    let mut streamer = StreamerContext {
        client_info: Some(shared.client_info.clone()),
        ..Default::default()
    };
    for (t, ctx) in &state.sabr_contexts {
        if state.active_sabr_contexts.contains(t) {
            streamer.sabr_contexts.push(ctx.clone());
        } else {
            streamer.unsent_sabr_contexts.push(*t);
        }
    }
    if let Some(token) = &shared.po_token_decoded {
        streamer.po_token = token.clone();
    }
    if let Some(cookie) = &state.playback_cookie {
        streamer.playback_cookie = cookie.clone();
    }

    let mut request = VideoPlaybackAbrRequest {
        client_abr_state: Some(abr),
        video_playback_ustreamer_config: shared.ustreamer_config.clone(),
        streamer_context: Some(streamer),
        ..Default::default()
    };

    if video_alternates.is_empty() {
        if let Some(v) = video {
            request.preferred_video_format_ids.push(v.to_format_id());
        }
    } else {
        for f in &video_alternates {
            request.preferred_video_format_ids.push(f.to_format_id());
        }
    }
    if audio_alternates.is_empty() {
        if let Some(a) = audio {
            request.preferred_audio_format_ids.push(a.to_format_id());
        }
    } else {
        for f in &audio_alternates {
            request.preferred_audio_format_ids.push(f.to_format_id());
        }
    }

    let held: Vec<SabrFormat> = if video_alternates.is_empty() && audio_alternates.is_empty() {
        [video.clone(), audio.clone()].into_iter().flatten().collect()
    } else {
        video_alternates.iter().chain(&audio_alternates).cloned().collect()
    };

    for format in &held {
        if !state.format_initialization.contains_key(&format.key()) {
            continue;
        }
        let buffer = self_buffer(shared, &format.key());
        if buffer.segment_count() == 0 {
            continue;
        }
        let from_us = demand_from_us(&state, &buffer, &format.key());
        let last_sequence = buffer.last_completed_sequence(from_us);
        if last_sequence < 0 {
            continue;
        }

        let start_seq = first_sequence_of_run(&buffer, from_us);
        let start_us = match buffer.get(start_seq) {
            Some(s) => s.start_us,
            None => continue,
        };
        let exact_end = if shared.is_live {
            buffer.exact_end_from_sequence(start_seq)
        } else {
            NO_US
        };
        let end = if exact_end != NO_US {
            exact_end
        } else {
            buffer.buffered_end_us(from_us)
        };
        let duration_us = if end == NO_US { 0 } else { (end - start_us).max(0) };

        let mut range = BufferedRange {
            format_id: Some(format.to_format_id()),
            start_segment_index: start_seq as i64,
            end_segment_index: last_sequence as i64,
            start_time_ms: start_us / 1000,
            duration_ms: duration_us / 1000,
            ..Default::default()
        };
        if let Some(first) = buffer.get(start_seq)
            && first.timescale > 0
        {
            range.time_range = Some(TimeRange {
                start_ticks: first.start_ticks,
                duration_ticks: us_to_ticks(duration_us, first.timescale),
                timescale: first.timescale,
            });
        }
        request.selected_format_ids.push(format.to_format_id());
        request.buffered_ranges.push(range);
    }

    request
}

// --- demand / progress helpers ---

fn needs_data(shared: &Arc<Shared>, state: &State) -> bool {
    if state.video_demand.is_none() && state.audio_demand.is_none() {
        return false;
    }
    if state.resume_position_us.is_some() {
        return true;
    }
    if let Some(v) = &state.video_demand
        && needs_data_for(shared, state, v, state.target_video_readahead_ms)
    {
        return true;
    }
    if let Some(a) = &state.audio_demand
        && needs_data_for(shared, state, a, state.target_audio_readahead_ms)
    {
        return true;
    }
    false
}

fn needs_data_for(shared: &Arc<Shared>, state: &State, demand: &Demand, target_ms: i64) -> bool {
    if is_complete_locked(shared, state, &demand.format) {
        return false;
    }
    let buffer = self_buffer(shared, &demand.format.key());
    let from = effective_from_us(&buffer, demand.from_us);
    let end = buffer.buffered_end_us(from);
    if end == NO_US {
        return true;
    }
    let mut target = target_ms.max(state.min_readahead_ms);
    if state.max_readahead_ms > 0 {
        target = target.min(state.max_readahead_ms);
    }
    end - from < target * 1000
}

fn effective_from_us(buffer: &SabrTrackBuffer, from_us: i64) -> i64 {
    match buffer.first_at_or_after(-1) {
        Some(first) => from_us.max(first.start_us),
        None => from_us,
    }
}

fn is_complete(shared: &Arc<Shared>, format: &SabrFormat) -> bool {
    let state = shared.state.lock();
    is_complete_locked(shared, &state, format)
}

fn is_complete_locked(shared: &Arc<Shared>, state: &State, format: &SabrFormat) -> bool {
    if shared.is_live {
        return false;
    }
    if state.format_complete.contains(&format.key()) {
        return true;
    }
    let end_segment = state
        .format_initialization
        .get(&format.key())
        .map(|m| m.end_segment_number)
        .unwrap_or(0);
    if end_segment <= 0 {
        return false;
    }
    let buffer = self_buffer(shared, &format.key());
    let from_us = demand_from_us(state, &buffer, &format.key());
    buffer.last_completed_sequence(from_us) >= end_segment
}

fn demand_from_us(state: &State, buffer: &SabrTrackBuffer, key: &SabrFormatKey) -> i64 {
    let raw = if state
        .video_demand
        .as_ref()
        .is_some_and(|d| d.alternates.iter().any(|a| a.key() == *key))
    {
        state.video_demand.as_ref().map(|d| d.from_us)
    } else if state
        .audio_demand
        .as_ref()
        .is_some_and(|d| d.alternates.iter().any(|a| a.key() == *key))
    {
        state.audio_demand.as_ref().map(|d| d.from_us)
    } else {
        None
    };
    match raw {
        Some(raw) => effective_from_us(buffer, raw),
        None => NO_US,
    }
}

fn request_position_us(shared: &Arc<Shared>, state: &State) -> i64 {
    if let Some(r) = state.resume_position_us {
        return r;
    }
    if let Some(s) = state.seek_pending_us {
        return s;
    }
    if shared.is_live && state.live_metadata.is_none() {
        return LIVE_HEAD_PLAYER_TIME_US;
    }
    let mut earliest = i64::MAX;
    for demand in [&state.video_demand, &state.audio_demand].into_iter().flatten() {
        let buffer = self_buffer(shared, &demand.format.key());
        let effective = effective_from_us(&buffer, demand.from_us);
        let end = buffer.buffered_end_us(effective);
        let from = if end == NO_US { effective } else { effective.max(end) };
        earliest = earliest.min(from);
    }
    if earliest == i64::MAX {
        return state.playback_position_us;
    }
    if !shared.is_live {
        return earliest;
    }
    state.playback_position_us.min(earliest)
}

fn update_progress(shared: &Arc<Shared>, format: &Option<SabrFormat>, requested_us: i64) {
    let format = match format {
        Some(f) => f,
        None => return,
    };
    let buffer = self_buffer(shared, &format.key());
    let mut state = shared.state.lock();
    if state
        .format_initialization
        .get(&format.key())
        .is_some_and(|m| m.end_segment_number > 0)
    {
        return;
    }
    // No prior segment info to compare against: only meaningful once we have
    // format init metadata.
    if !state.format_initialization.contains_key(&format.key()) {
        return;
    }
    let end_us = state
        .format_initialization
        .get(&format.key())
        .and_then(|m| (m.end_time_ms > 0).then_some(m.end_time_ms * 1000))
        .unwrap_or(shared.duration_us);
    let buffered_end = buffer.buffered_end_from_front_us();
    if end_us > 0 && buffered_end < end_us - FRONTIER_EPSILON_US {
        return;
    }
    if end_us > 0 && requested_us < end_us - FRONTIER_EPSILON_US {
        return;
    }
    let at_frontier = buffered_end <= requested_us + FRONTIER_EPSILON_US;
    if !at_frontier {
        return;
    }
    let n = state.format_no_progress.get(&format.key()).copied().unwrap_or(0) + 1;
    state.format_no_progress.insert(format.key(), n);
    if n >= NO_PROGRESS_THRESHOLD {
        state.format_complete.insert(format.key());
    }
}

fn starved(shared: &Arc<Shared>, state: &State) -> bool {
    let demands: Vec<&Demand> =
        [&state.video_demand, &state.audio_demand].into_iter().flatten().collect();
    if demands.is_empty() {
        return false;
    }
    for demand in demands {
        let buffer = self_buffer(shared, &demand.format.key());
        let end = buffer.buffered_end_us(effective_from_us(&buffer, demand.from_us));
        if end == NO_US {
            return true;
        }
        if end - state.playback_position_us < 3_000_000 {
            return true;
        }
    }
    false
}

fn evict_consumed_segments(shared: &Arc<Shared>) {
    let (threshold, buffers) = {
        let state = shared.state.lock();
        let floor = if state.restart_from_us != NO_US {
            state.restart_from_us
        } else {
            state.playback_position_us
        };
        let threshold = state.playback_position_us.min(floor) - state.keep_behind_us;
        (
            threshold,
            shared.buffers.lock().values().cloned().collect::<Vec<_>>(),
        )
    };
    if threshold <= 0 {
        return;
    }
    for buffer in buffers {
        buffer.evict_before(threshold);
    }
}

fn apply_sabr_seek(shared: &Arc<Shared>, seek_to_us: i64, _requested_position_us: i64) {
    let mut state = shared.state.lock();
    if shared.is_live && let Some(lm) = state.live_metadata {
        let min_scale = lm.min_seekable_timescale;
        let max_scale = lm.max_seekable_timescale;
        if min_scale <= 0 || max_scale <= 0 {
            return;
        }
        let window_start = ticks_to_us(lm.min_seekable_time_ticks, min_scale);
        let window_end = ticks_to_us(lm.max_seekable_time_ticks, max_scale);
        if seek_to_us < window_start || seek_to_us > window_end + SABR_SEEK_SLACK_US {
            return;
        }
    }

    // Is this a genuine reposition, or the live server just keeping us pinned at
    // the newest servable segment? A live stream re-issues SABR_SEEK to ~the edge
    // on nearly every request as a keep-alive. If we're already at (or already
    // have buffered) that position, honouring it (rewinding the playhead,
    // re-anchoring demands, and worst of all clearing the buffer) starves the
    // stream. The segment covering the seek can never survive, so `seek_pending`
    // never lands, the request stays pinned, and the server keeps replying with
    // empty (len=0) headers + another seek. Only a real jump (far from us and not
    // already buffered) discards media and restarts the feeders.
    let near_position = (seek_to_us - state.playback_position_us).abs() < SABR_SEEK_SLACK_US;
    let already_buffered = [&state.video_demand, &state.audio_demand]
        .into_iter()
        .flatten()
        .any(|d| self_buffer(shared, &d.format.key()).first_covering(seek_to_us).is_some());
    if near_position || already_buffered {
        state.last_sabr_seek_us = seek_to_us;
        return;
    }

    if seek_to_us == state.last_sabr_seek_us && state.seek_pending_us.is_none() {
        return;
    }
    state.last_sabr_seek_us = seek_to_us;
    state.seek_pending_us = Some(seek_to_us);
    state.playback_position_us = seek_to_us;
    state.restart_from_us = seek_to_us;
    reanchor_demands(&mut state, seek_to_us);
    state.last_action_ms = now_ms();

    // A genuine server seek makes the timeline discontinuous: drop buffered
    // segments, bump the pump's restart epoch so any in-flight consume for the
    // old position is abandoned, and bump the externally-visible seek generation
    // so consumers re-sync from the new front instead of waiting for a sequence
    // that will never arrive.
    state.restart_epoch += 1;
    state.format_complete.clear();
    state.format_no_progress.clear();
    for buffer in shared.buffers.lock().values() {
        buffer.clear();
    }
    shared.server_seek_generation.fetch_add(1, Ordering::AcqRel);
    self_notify(shared);
}

fn clear_seek_if_landed(shared: &Arc<Shared>) {
    let (pending, demands) = {
        let state = shared.state.lock();
        let pending = match state.seek_pending_us {
            Some(p) => p,
            None => return,
        };
        let demands: Vec<Demand> = [state.video_demand.clone(), state.audio_demand.clone()]
            .into_iter()
            .flatten()
            .collect();
        (pending, demands)
    };
    let landed = demands
        .iter()
        .all(|d| self_buffer(shared, &d.format.key()).first_covering(pending).is_some());
    if landed {
        shared.state.lock().seek_pending_us = None;
    }
}

fn clamp_to_seekable_window(shared: &Arc<Shared>) {
    let (position, window_start, window_end) = {
        let state = shared.state.lock();
        if state.seek_pending_us.is_some() {
            return;
        }
        let lm = match state.live_metadata {
            Some(lm) => lm,
            None => return,
        };
        if lm.min_seekable_timescale <= 0 || lm.max_seekable_timescale <= 0 {
            return;
        }
        (
            state.playback_position_us,
            ticks_to_us(lm.min_seekable_time_ticks, lm.min_seekable_timescale),
            ticks_to_us(lm.max_seekable_time_ticks, lm.max_seekable_timescale),
        )
    };
    if position < window_start {
        apply_sabr_seek(shared, window_start, position);
    } else if position > window_end + SABR_SEEK_SLACK_US {
        apply_sabr_seek(shared, window_end, position);
    }
}

fn first_sequence_of_run(buffer: &SabrTrackBuffer, from_us: i64) -> i32 {
    let last = buffer.last_completed_sequence(from_us);
    if last < 0 {
        return buffer.lowest_sequence().max(1);
    }
    let mut first = last;
    while first > 0 && buffer.get(first - 1).is_some_and(|s| s.is_complete()) {
        first -= 1;
    }
    first.max(0)
}

// --- small helpers ---

fn self_buffer(shared: &Arc<Shared>, key: &SabrFormatKey) -> Arc<SabrTrackBuffer> {
    let mut buffers = shared.buffers.lock();
    buffers
        .entry(key.clone())
        .or_insert_with(|| Arc::new(SabrTrackBuffer::new(key.clone())))
        .clone()
}

fn self_notify(shared: &Arc<Shared>) {
    shared.notify.notify_waiters();
}

fn count_and_init(
    shared: &Arc<Shared>,
    format: &Option<SabrFormat>,
) -> (usize, Option<Arc<SabrSegment>>) {
    match format {
        Some(f) => {
            let buffer = self_buffer(shared, &f.key());
            (buffer.segment_count(), buffer.init_segment())
        }
        None => (0, None),
    }
}

#[allow(clippy::too_many_arguments)]
fn has_advanced(
    shared: &Arc<Shared>,
    video: &Option<SabrFormat>,
    video_count_before: usize,
    video_init_before: Option<Arc<SabrSegment>>,
    audio: &Option<SabrFormat>,
    audio_count_before: usize,
    audio_init_before: Option<Arc<SabrSegment>>,
) -> bool {
    let check = |format: &Option<SabrFormat>, count_before: usize, init_before: &Option<_>| {
        let f = match format {
            Some(f) => f,
            None => return false,
        };
        let buffer = self_buffer(shared, &f.key());
        buffer.segment_count() > count_before
            || (init_before.is_none() && buffer.init_segment().is_some())
    };
    check(video, video_count_before, &video_init_before)
        || check(audio, audio_count_before, &audio_init_before)
}

fn record_throughput(local: &mut PumpLocal, bytes: i64, elapsed_ms: i64, media_us: i64) {
    if bytes < THROUGHPUT_MIN_BYTES || elapsed_ms <= 0 {
        return;
    }
    if media_us < elapsed_ms * 1000 * THROUGHPUT_MIN_SPEEDUP {
        return;
    }
    let sample = bytes * 1000 / elapsed_ms;
    local.throughput_bytes_per_sec = if local.throughput_bytes_per_sec == 0 {
        sample
    } else {
        (local.throughput_bytes_per_sec * (100 - THROUGHPUT_SMOOTHING) + sample * THROUGHPUT_SMOOTHING)
            / 100
    };
}

fn append_request_number(shared: &Arc<Shared>, url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let rn = shared.request_number.fetch_add(1, Ordering::AcqRel) + 1;
    format!("{url}{separator}rn={rn}")
}

fn ticks_to_us(ticks: i64, timescale: i32) -> i64 {
    let ts = timescale as i64;
    ticks / ts * MICROS_PER_SECOND + (ticks % ts) * MICROS_PER_SECOND / ts
}

fn us_to_ticks(us: i64, timescale: i32) -> i64 {
    let ts = timescale as i64;
    us / MICROS_PER_SECOND * ts + (us % MICROS_PER_SECOND) * ts / MICROS_PER_SECOND
}

fn now_ms() -> i64 {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    START.elapsed().as_millis() as i64
}

fn decode_base64_lenient(value: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value) {
        return Some(bytes);
    }
    let standardized = value.replace('-', "+").replace('_', "/");
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(&standardized)
        .ok()
        .or_else(|| base64::engine::general_purpose::STANDARD.decode(&standardized).ok())
}
