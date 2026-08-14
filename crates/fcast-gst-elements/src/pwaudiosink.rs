//! `fcastpwaudiosink`: a native PipeWire audio sink.
//!
//! Invariant: NEVER park a thread unboundedly or stall silently. A dead
//! daemon/stream and a graph that stops consuming both post an element error
//! (returning -1 alone reaches nobody, see [`imp::StallVerdict::escalates`]),
//! and `reset()` aborts a blocked `write()` immediately.

use gst::{glib, prelude::*};

mod imp {
    use std::{collections::VecDeque, sync::Arc, time::Duration};

    use parking_lot::{Condvar, Mutex};

    use gst::{glib, prelude::ElementExt, subclass::prelude::*};
    use gst_audio::subclass::prelude::*;
    use gst_base::prelude::BaseSinkExt;

    use pipewire as pw;
    use pw::{
        properties::properties,
        stream::{StreamFlags, StreamState},
    };

    use libspa as spa;
    use spa::pod::Pod;

    static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
        gst::DebugCategory::new(
            "fcastpwaudiosink",
            gst::DebugColorFlags::empty(),
            Some("FCast PipeWire audio sink"),
        )
    });

    /// How long `write()` may go without the pw graph freeing any ring space
    /// before it errors out (covers a cold connect and a default-device move).
    const WRITE_STALL_LIMIT: Duration = Duration::from_secs(2);
    const WRITE_STALL_STEP: Duration = Duration::from_millis(100);
    /// How long `write()` tolerates a soft-cork before checking it against the
    /// element state. A cork is legitimate only while PAUSED. One held with the
    /// element PLAYING is stale (lost uncork) and parks the writer forever.
    const CORK_RECONCILE_AFTER: Duration = Duration::from_secs(5);
    /// How long ONE un-corked `write()` may stay blocked in total, even while
    /// the graph frees a trickle of space (which would reset the per-progress
    /// clock forever). An honest block is one segment of audio, see the
    /// `a_healthy_graph_...` test.
    const WRITE_TOTAL_BLOCK_LIMIT: Duration = Duration::from_secs(6);
    /// How long the graph may deliver NO process() callback while the element
    /// is settled at PLAYING and un-corked. Cycles are the only direct "the
    /// graph is running our stream" signal (`BridgeShared::cycles`).
    const CYCLE_STALL_LIMIT: Duration = Duration::from_secs(5);

    /// `FCAST_PW_DELAY_TRACE=1`: eprintln the delay()/process() internals,
    /// rate-limited. Cached, and eprintln rather than gst debug, because the
    /// process callback is RT.
    fn delay_trace() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_DELAY_TRACE").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_DEVICE_LATENCY=1`: fold the graph->device latency back into
    /// `delay()` instead of declaring it as the sink's render delay. An A/B
    /// hatch for devices that misreport it (see [`PwAudioSink::device_delay`]).
    fn no_device_latency() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_DEVICE_LATENCY").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_TOTAL_STALL_CAP=1`: bail out of a blocked `write()` only on
    /// the per-progress clock, not [`WRITE_TOTAL_BLOCK_LIMIT`].
    fn no_total_stall_cap() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_TOTAL_STALL_CAP").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_CYCLE_WATCHDOG=1`: disable the [`CYCLE_STALL_LIMIT`] check.
    fn no_cycle_watchdog() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_CYCLE_WATCHDOG").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_STALL_ELEMENT_ERROR=1`: handle a [`WRITE_STALL_LIMIT`]
    /// stall by returning -1, which is INVISIBLE (the binding drops the error
    /// and gstaudiosink just skips the segment, so nothing reaches the bus).
    fn no_stall_element_error() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_STALL_ELEMENT_ERROR").is_ok_and(|v| v == "1"))
    }

    /// Why a blocked un-corked `write()` gives up, most specific first.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StallVerdict {
        /// Keep waiting, the block is still within every bound.
        Wait,
        /// No process() callback at all, so the graph is not running our
        /// stream.
        NoCycles,
        /// Space freed only in a trickle, blocked past
        /// [`WRITE_TOTAL_BLOCK_LIMIT`].
        TotalBlocked,
        /// Nothing freed at all for [`WRITE_STALL_LIMIT`].
        NoProgress,
    }

    impl StallVerdict {
        fn reason(self) -> &'static str {
            match self {
                StallVerdict::Wait => "not stalled",
                StallVerdict::NoCycles => "the pw graph delivered no process cycle",
                StallVerdict::TotalBlocked => "the pw graph freed ring space only in a trickle",
                StallVerdict::NoProgress => "the pw graph freed no ring space",
            }
        }

        /// Whether to post an element error (and park the writer) instead of
        /// handing -1 back. A negative `write()` is NOT loud. gstaudiosink
        /// skips the segment and calls straight back, and the binding drops the
        /// LoggableError unlogged, so nothing reaches the bus.
        fn escalates(self, stall_errors_levered_off: bool) -> bool {
            match self {
                StallVerdict::Wait => false,
                StallVerdict::NoProgress => !stall_errors_levered_off,
                StallVerdict::NoCycles | StallVerdict::TotalBlocked => true,
            }
        }
    }

    /// Everything the bail-out policy looks at, so the decision is a pure
    /// function (unit-tested, no pw graph or element needed).
    #[derive(Debug, Clone, Copy)]
    struct StallSnapshot {
        current: gst::State,
        pending: gst::State,
        /// Soft-corked. The pause path owns that case entirely.
        corked: bool,
        /// Since the ring last shrank.
        since_progress: Duration,
        /// Since this write() call started blocking.
        blocked_total: Duration,
        /// Since `BridgeShared::cycles` last moved (kept across write() calls).
        since_cycle: Duration,
    }

    /// The limits in force, so the env levers stay out of the decision.
    /// `None` = levered off.
    #[derive(Debug, Clone, Copy)]
    struct StallLimits {
        no_progress: Duration,
        total: Option<Duration>,
        cycle: Option<Duration>,
    }

    fn stall_limits() -> StallLimits {
        StallLimits {
            no_progress: WRITE_STALL_LIMIT,
            total: (!no_total_stall_cap()).then_some(WRITE_TOTAL_BLOCK_LIMIT),
            cycle: (!no_cycle_watchdog()).then_some(CYCLE_STALL_LIMIT),
        }
    }

    /// Decide whether a blocked `write()` may keep waiting.
    fn write_stall_verdict(snap: &StallSnapshot, limits: &StallLimits) -> StallVerdict {
        // A soft-cork legitimately never drains. The paused arms in `write()`
        // own that case.
        if snap.corked {
            return StallVerdict::Wait;
        }
        // The graph-is-stopped verdicts are only knowable with the element
        // settled at PLAYING. Preroll, a transition in flight and a teardown
        // all stop the graph from draining for good reasons.
        if snap.current == gst::State::Playing && snap.pending == gst::State::VoidPending {
            if limits.cycle.is_some_and(|limit| snap.since_cycle >= limit) {
                return StallVerdict::NoCycles;
            }
            if limits
                .total
                .is_some_and(|limit| snap.blocked_total >= limit)
            {
                return StallVerdict::TotalBlocked;
            }
        }
        if snap.since_progress >= limits.no_progress {
            return StallVerdict::NoProgress;
        }
        StallVerdict::Wait
    }

    /// Re-declare the device latency only once it moved this far. Every change
    /// posts a LATENCY message and redistributes the whole pipeline, and the
    /// remainder sits inside the base class's slaving tolerance.
    const RENDER_DELAY_HYSTERESIS: gst::ClockTime = gst::ClockTime::from_mseconds(5);

    /// Sanity cap on a reported device latency. Bluetooth audio tops out near
    /// 300ms, so anything past this is a broken report.
    const MAX_DEVICE_DELAY: gst::ClockTime = gst::ClockTime::from_mseconds(1000);

    /// How long the PLAYING->PAUSED edge may wait for the graph to pick up an
    /// EOS tail still in the bridge (see [`PwAudioSink::drain_eos_tail`]).
    const TAIL_DRAIN_LIMIT: Duration = Duration::from_millis(250);
    const TAIL_DRAIN_STEP: Duration = Duration::from_millis(10);

    /// The `write()` <-> pw-`process` bridge.
    ///
    /// `write()` (audiobasesink's ring thread) blocks while the ring is full,
    /// and that back-pressure paces the base class. The pw process callback
    /// (thread-loop RT thread) drains it. `flushing` must abort a blocked
    /// `write()` IMMEDIATELY.
    struct Bridge {
        ring: VecDeque<u8>,
        /// Capacity in bytes, sized in `prepare()` to ~2 spec segments so the
        /// base class's own ring stays the dominant buffer.
        capacity: usize,
        flushing: bool,
        /// Latched by the error listeners. The stream will never consume again,
        /// so `write()` errors out and `delay()` reports 0 (EOS/drain waits
        /// must not hang on a corpse). Never cleared.
        dead: bool,
        /// Of the negotiated format, for delay math.
        bytes_per_frame: usize,
        /// Channel count of the negotiated format (<=2 by the template).
        channels: usize,
        /// Whether samples are F32LE (else S16LE), for the de-click math.
        is_f32: bool,
        /// The last real frame emitted (f32 per channel), the seed for the
        /// de-click ramp when data stops (a hard cut from non-zero amplitude
        /// pops).
        last_frame: [f32; 2],
        /// A cycle emitted silence, so the next real data gets a gain ramp-in.
        resume_fade: bool,
        /// Soft-cork (pulsesink's pause semantics). process() emits silence
        /// WITHOUT draining and reset() keeps the ring, so delay() (and with it
        /// the audio clock) stays steady across the pause. Real flushes clear
        /// the ring from the FlushStop event instead, because a flush while
        /// paused may never reach reset() at all.
        paused: bool,
        /// PAUSED->READY teardown latch. gst_audio_ring_buffer_activate(FALSE)
        /// JOINS the writer thread with no reset() first, so a `write()`
        /// blocked on a full soft-corked ring would deadlock the state
        /// change. Set before chaining the transition, cleared only by
        /// `prepare()`.
        shutting_down: bool,
        /// EOS reached and not flushed away since, so the tail is owed a
        /// bounded drain on the way out of PLAYING instead of being cut.
        /// Cleared by FlushStop and by every `prepare()`.
        eos: bool,
        /// Bytes process() has taken out of the ring, monotonic. The only way
        /// to wait for a specific piece of audio to reach the graph, because
        /// the ring itself never empties (the writer thread keeps handing us
        /// whole segments, silence included, while its ring is started).
        drained: u64,
        /// Process cycles that found less data than they wanted, idle/paused
        /// silence-fills included. A coarse stat, not an error signal.
        underruns: u64,
        /// A stall was reported for this stream. Post the element error once
        /// and park the writer, rather than failing every segment
        /// (gstaudiosink skips a refused one and calls straight back, a
        /// busy loop). Cleared by the next drain and by every
        /// `prepare()`.
        stalled: bool,
    }

    impl Default for Bridge {
        fn default() -> Self {
            Self {
                ring: VecDeque::new(),
                capacity: 0,
                flushing: false,
                dead: false,
                bytes_per_frame: 4,
                channels: 1,
                is_f32: true,
                last_frame: [0.0; 2],
                resume_fade: false,
                paused: false,
                shutting_down: false,
                eos: false,
                drained: 0,
                underruns: 0,
                stalled: false,
            }
        }
    }

    /// ~5.3ms at 48kHz. Long enough to de-click even pure tones, short enough
    /// to be inaudible as a fade.
    const FADE_FRAMES: usize = 256;

    /// Read one interleaved frame at `offset` as f32 per channel.
    fn read_frame(slice: &[u8], offset: usize, channels: usize, is_f32: bool) -> [f32; 2] {
        let mut out = [0.0f32; 2];
        for (c, out) in out.iter_mut().enumerate().take(channels.min(2)) {
            let base = offset + c * if is_f32 { 4 } else { 2 };
            *out = if is_f32 {
                f32::from_le_bytes(slice[base..base + 4].try_into().unwrap())
            } else {
                i16::from_le_bytes(slice[base..base + 2].try_into().unwrap()) as f32
                    / i16::MAX as f32
            };
        }
        out
    }

    /// Write one interleaved frame at `offset` from f32 per channel.
    fn write_frame(
        slice: &mut [u8],
        offset: usize,
        channels: usize,
        is_f32: bool,
        frame: [f32; 2],
    ) {
        for (c, value) in frame.iter().enumerate().take(channels.min(2)) {
            let base = offset + c * if is_f32 { 4 } else { 2 };
            if is_f32 {
                slice[base..base + 4].copy_from_slice(&value.to_le_bytes());
            } else {
                let v = (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                slice[base..base + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    /// Scale `frames` interleaved frames starting at `offset` by a linear
    /// ramp from `from` to `to` (de-click fade in/out on real data).
    fn apply_gain_ramp(
        slice: &mut [u8],
        offset: usize,
        frames: usize,
        channels: usize,
        is_f32: bool,
        from: f32,
        to: f32,
    ) {
        if frames == 0 {
            return;
        }
        let bpf = channels.min(2) * if is_f32 { 4 } else { 2 };
        for i in 0..frames {
            let gain = from + (to - from) * (i + 1) as f32 / frames as f32;
            let at = offset + i * bpf;
            let mut frame = read_frame(slice, at, channels, is_f32);
            frame[0] *= gain;
            frame[1] *= gain;
            write_frame(slice, at, channels, is_f32, frame);
        }
    }

    /// The bridge halves shared with the RT process callback.
    #[derive(Default)]
    pub struct BridgeShared {
        bridge: Mutex<Bridge>,
        space: Condvar,
        /// Lock-free copy of `Bridge::bytes_per_frame` for the process
        /// callback's no-lock silence path (0 until the first prepare()).
        bytes_per_frame: std::sync::atomic::AtomicUsize,
        /// process() callbacks since `prepare()`, the one direct "the graph is
        /// running our stream" signal (StreamState::Streaming only means the
        /// node is active). Bumped BEFORE the bridge try_lock, so the
        /// watchdog cannot read lock contention as a dead graph.
        cycles: std::sync::atomic::AtomicU64,
    }

    impl BridgeShared {
        /// The stream is gone for good. Unblock and fail every `write()`.
        fn mark_dead(&self) {
            self.bridge.lock().dead = true;
            self.space.notify_all();
        }
    }

    /// Held while the element is OPEN (pw connection up).
    ///
    /// SAFETY (the `unsafe impl Send`): pipewire-rs types are `!Send` because
    /// libpipewire objects are loop-affine. The C contract allows cross-thread
    /// use iff the thread-loop lock is held, so every access to these fields,
    /// plus their construction and drop, takes `thread_loop.lock()` first. One
    /// exception: `pw_stream_get_time_n` is RT- and thread-safe (seqlock read)
    /// and is called lock-free from `delay()`.
    struct PwConn {
        thread_loop: pw::thread_loop::ThreadLoopRc,
        context: pw::context::ContextRc,
        core: pw::core::CoreRc,
        /// Daemon death must post an element error, never leave a silent zombie
        /// stream. Dies (under the loop lock) in `close()`.
        core_listener: pw::core::Listener,
    }
    unsafe impl Send for PwConn {}

    /// Held while PREPARED (stream connected for a concrete format). Same Send
    /// contract as `PwConn` (loop lock).
    struct PwStream {
        stream: pw::stream::StreamRc,
        _listener: pw::stream::StreamListener<()>, // callbacks die if dropped
        rate: u32,
    }
    unsafe impl Send for PwStream {}

    #[derive(Default)]
    pub struct PwAudioSink {
        stream: Mutex<Option<PwStream>>,
        conn: Mutex<Option<PwConn>>,
        shared: Arc<BridgeShared>,
        /// Nanoseconds of device latency last handed to `set_render_delay`, so
        /// a re-check only pays for a bus message when the route changed (see
        /// [`PwAudioSink::sync_render_delay`]).
        announced_device_delay: std::sync::atomic::AtomicU64,
        /// `write()` calls, to keep the device-latency re-check off the hot
        /// path (one segment per call, so every 32nd is a third of a
        /// second).
        writes: std::sync::atomic::AtomicU64,
        /// Cycle-watchdog state, writer thread only: the last observed
        /// `BridgeShared::cycles` and when it last moved. Kept ACROSS `write()`
        /// calls, so a bail-out (which only skips one segment) and a graph that
        /// died during a pause are both still caught.
        cycle_probe: Mutex<Option<(u64, std::time::Instant)>>,
    }

    impl PwAudioSink {
        /// The fixed graph->device latency (`pw_time.delay`): graph filters,
        /// the device's own buffering, and on a Bluetooth route the transport
        /// plus the headset's delay report.
        ///
        /// NOT queueing. It never drains, it is a property of the route. See
        /// `delay()` for the queued half.
        fn device_delay(&self) -> Option<gst::ClockTime> {
            if no_device_latency() {
                return None;
            }
            let stream = self.stream.lock();
            let time = stream.as_ref()?.stream.time().ok()?;
            let rate = time.rate();
            if rate.num == 0 || rate.denom == 0 {
                return None;
            }
            let ns = time.delay().max(0) as u128 * rate.num as u128 * 1_000_000_000u128
                / rate.denom as u128;
            // Saturate rather than panic on a nonsense report; the caller caps it.
            Some(gst::ClockTime::from_nseconds(
                ns.min(gst::ClockTime::MAX.nseconds() as u128) as u64,
            ))
        }

        /// Declare the device latency to the base class whenever it moves
        /// enough to be worth a pipeline-wide redistribution.
        ///
        /// `set_render_delay()` adds it to this sink's LATENCY answer and posts
        /// a LATENCY message, so the pipeline covers the device.
        /// Without it the clock slaving reads the constant offset as
        /// drift and resyncs the ring (dropping audio) forever without
        /// converging.
        fn sync_render_delay(&self) {
            let Some(delay) = self.device_delay() else {
                return;
            };
            let capped = delay.min(MAX_DEVICE_DELAY);
            if capped != delay {
                gst::warning!(
                    CAT,
                    "device reports {delay} of latency, capping at {capped}"
                );
            }
            let announced = self
                .announced_device_delay
                .load(std::sync::atomic::Ordering::Relaxed);
            if capped.nseconds().abs_diff(announced) < RENDER_DELAY_HYSTERESIS.nseconds() {
                return;
            }
            self.announced_device_delay
                .store(capped.nseconds(), std::sync::atomic::Ordering::Relaxed);
            // WARNING, not info: this redistributes the whole pipeline's
            // latency mid-play, and field logs only capture WARNING and above.
            gst::warning!(
                CAT,
                imp = self,
                "device latency {capped}, declaring it as render delay \
                 (posts LATENCY, redistributes the pipeline)"
            );
            self.obj().set_render_delay(capped);
        }

        /// How long the graph has gone without delivering a process() callback,
        /// updating the probe on the way. Reads zero whenever the count moves,
        /// so a `prepare()` (which zeroes `cycles`) restarts the clock.
        fn observe_cycles(&self, cycles: u64) -> Duration {
            let now = std::time::Instant::now();
            let mut probe = self.cycle_probe.lock();
            match *probe {
                Some((seen, at)) if seen == cycles => now.saturating_duration_since(at),
                _ => {
                    *probe = Some((cycles, now));
                    Duration::ZERO
                }
            }
        }

        /// Let the graph take what is left in the bridge after an EOS, before
        /// the sink is corked and the stream torn down. Bounded, and EOS-only.
        /// A Stop or a flush means silence now, which process() ramps out.
        ///
        /// Waits for exactly what is queued right now, NOT for the ring to
        /// empty, which it never does (see `Bridge::drained`).
        fn drain_eos_tail(&self) {
            let mut bridge = self.shared.bridge.lock();
            if !bridge.eos || bridge.paused {
                return;
            }
            let target = bridge.drained + bridge.ring.len() as u64;
            let deadline = std::time::Instant::now() + TAIL_DRAIN_LIMIT;
            while bridge.drained < target && !bridge.dead && !bridge.flushing {
                if std::time::Instant::now() >= deadline {
                    gst::warning!(CAT, "EOS tail still queued after {TAIL_DRAIN_LIMIT:?}");
                    break;
                }
                self.shared.space.wait_for(&mut bridge, TAIL_DRAIN_STEP);
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PwAudioSink {
        const NAME: &'static str = "FCastPwAudioSink";
        type Type = super::PwAudioSink;
        type ParentType = gst_audio::AudioSink;
    }

    impl ObjectImpl for PwAudioSink {}
    impl GstObjectImpl for PwAudioSink {}

    impl ElementImpl for PwAudioSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            Some(METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "FCast PipeWire audio sink",
                    "Sink/Audio",
                    "Plays audio through a native PipeWire stream",
                    "FCast",
                )
            }))
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> =
                std::sync::OnceLock::new();
            TEMPLATES.get_or_init(|| {
                // Only what prepare() maps to spa formats, F32LE first (pw's
                // native mixing format). Capped at stereo until prepare() gets a
                // gst->spa channel-position map.
                let caps = gst::Caps::builder("audio/x-raw")
                    .field("format", gst::List::new(["F32LE", "S16LE"]))
                    .field("rate", gst::IntRange::new(1i32, 384_000))
                    .field("channels", gst::IntRange::new(1i32, 2))
                    .field("layout", "interleaved")
                    .build();
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                ]
            })
        }

        // Never provide a clock, so pipeline election falls through to the
        // monotonic system clock. The base class still runs its internal
        // GstAudioClock off ring position for the skew slaving.
        fn provide_clock(&self) -> Option<gst::Clock> {
            None
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            // Soft-cork bookkeeping, set/cleared BEFORE chaining so the parent's
            // ring pause (which calls reset()) sees it.
            match transition {
                gst::StateChange::PlayingToPaused => {
                    self.drain_eos_tail();
                    self.shared.bridge.lock().paused = true;
                }
                gst::StateChange::PausedToPlaying => {
                    self.shared.bridge.lock().paused = false;
                }
                gst::StateChange::PausedToReady => {
                    // The parent's ring deactivation JOINS the writer thread with
                    // no reset() first, so unblock write() before that join.
                    self.shared.bridge.lock().shutting_down = true;
                    self.shared.space.notify_all();
                }
                _ => {}
            }
            self.parent_change_state(transition)
        }
    }

    impl BaseSinkImpl for PwAudioSink {
        fn event(&self, event: gst::Event) -> bool {
            // Latched BEFORE chaining up. The parent's EOS handling drains and
            // posts without returning here in between (see drain_eos_tail).
            if let gst::EventView::Eos(_) = event.view() {
                self.shared.bridge.lock().eos = true;
            }
            // Real flushes discard the bridge here, reset() can't. The pause path
            // funnels there too, and a flush while paused may skip it entirely.
            if let gst::EventView::FlushStop(_) = event.view() {
                {
                    let mut bridge = self.shared.bridge.lock();
                    bridge.ring.clear();
                    bridge.eos = false;
                }
                self.shared.space.notify_all();
                let conn_slot = self.conn.lock();
                let stream_slot = self.stream.lock();
                if let (Some(conn), Some(s)) = (conn_slot.as_ref(), stream_slot.as_ref()) {
                    let _guard = conn.thread_loop.lock();
                    let _ = s.stream.flush(false);
                }
            }
            self.parent_event(event)
        }
    }
    impl AudioBaseSinkImpl for PwAudioSink {}

    impl AudioSinkImpl for PwAudioSink {
        fn open(&self) -> Result<(), gst::LoggableError> {
            // A missing/broken daemon is a LoggableError here. Fallback policy
            // stays OUT of the element (the receiver probes `is_available`).
            pw::init(); // idempotent

            // SAFETY: the C-side requirement is the loop-lock discipline this
            // module follows throughout (see PwConn).
            let thread_loop =
                unsafe { pw::thread_loop::ThreadLoopRc::new(Some("fcast-pw-sink"), None) }
                    .map_err(|e| gst::loggable_error!(CAT, "pw thread loop: {e}"))?;

            // Construct under the loop lock. The loop thread is live after
            // start() and libpipewire objects are not thread-safe. `?` drops
            // context/core under the guard, so the error paths are safe too.
            thread_loop.start();
            let (context, core, core_listener) = {
                let _guard = thread_loop.lock();
                let context = pw::context::ContextRc::new(&thread_loop, None)
                    .map_err(|e| gst::loggable_error!(CAT, "pw context: {e}"))?;
                let core = context
                    .connect_rc(None)
                    .map_err(|e| gst::loggable_error!(CAT, "pw connect: {e}"))?;

                let shared = Arc::clone(&self.shared);
                let obj = self.obj().downgrade();
                let core_listener = core
                    .add_listener_local()
                    .error(move |id, _seq, res, message| {
                        if id == pw::core::PW_ID_CORE {
                            gst::error!(CAT, "pw core error (res {res}): {message}");
                            shared.mark_dead();
                            if let Some(obj) = obj.upgrade() {
                                gst::element_error!(
                                    obj,
                                    gst::ResourceError::Failed,
                                    ("PipeWire core error: {}", message)
                                );
                            }
                        } else {
                            gst::warning!(CAT, "pw error on object {id} (res {res}): {message}");
                        }
                    })
                    .register();
                (context, core, core_listener)
            };

            *self.conn.lock() = Some(PwConn {
                thread_loop,
                context,
                core,
                core_listener,
            });
            Ok(())
        }

        fn prepare(
            &self,
            spec: &mut gst_audio::AudioRingBufferSpec,
        ) -> Result<(), gst::LoggableError> {
            let info = spec.audio_info();
            let rate = info.rate();
            let channels = info.channels();
            let bytes_per_frame = info.bpf() as usize;

            {
                let mut bridge = self.shared.bridge.lock();
                bridge.bytes_per_frame = bytes_per_frame;
                bridge.channels = channels as usize;
                bridge.is_f32 = info.format() == gst_audio::AudioFormat::F32le;
                bridge.last_frame = [0.0; 2];
                bridge.resume_fade = true; // first data ramps in
                bridge.paused = false;
                bridge.shutting_down = false;
                bridge.flushing = false;
                bridge.eos = false;
                bridge.dead = false;
                bridge.ring.clear();
                bridge.underruns = 0;
                bridge.stalled = false;
                // ~2 segments of headroom, enough that process() never starves
                // between write() wakeups, small enough that the base class's
                // ring stays the dominant buffer.
                bridge.capacity = (spec.segsize() as usize).max(bytes_per_frame * 1024) * 2;
            }
            self.shared
                .bytes_per_frame
                .store(bytes_per_frame, std::sync::atomic::Ordering::Relaxed);
            // A fresh stream starts the cycle watchdog over, never against the
            // previous stream's count.
            self.shared
                .cycles
                .store(0, std::sync::atomic::Ordering::Relaxed);
            *self.cycle_probe.lock() = None;

            let mut audio_info = spa::param::audio::AudioInfoRaw::new();
            audio_info.set_format(match info.format() {
                gst_audio::AudioFormat::F32le => spa::param::audio::AudioFormat::F32LE,
                gst_audio::AudioFormat::S16le => spa::param::audio::AudioFormat::S16LE,
                f => return Err(gst::loggable_error!(CAT, "unmapped format {f:?}")),
            });
            audio_info.set_rate(rate);
            audio_info.set_channels(channels);
            // Required: without positions the stream is UNPOSITIONED, so pw's
            // channel mixer maps by index instead of layout (mono lands on one
            // speaker) and skips up/downmixing to the device layout.
            {
                let mut position =
                    [spa::sys::SPA_AUDIO_CHANNEL_UNKNOWN; spa::param::audio::MAX_CHANNELS];
                match channels {
                    1 => position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO,
                    2 => {
                        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
                        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
                    }
                    n => return Err(gst::loggable_error!(CAT, "unmapped channel count {n}")),
                }
                audio_info.set_position(position);
            }
            gst::info!(
                CAT,
                "preparing: {:?} rate={rate} channels={channels} segsize={} segtotal={}",
                info.format(),
                spec.segsize(),
                spec.segtotal(),
            );

            let values = spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &spa::pod::Value::Object(spa::pod::Object {
                    type_: spa::sys::SPA_TYPE_OBJECT_Format,
                    id: spa::sys::SPA_PARAM_EnumFormat,
                    properties: audio_info.into(),
                }),
            )
            .map_err(|e| gst::loggable_error!(CAT, "format pod: {e:?}"))?
            .0
            .into_inner();
            let mut params = [Pod::from_bytes(&values)
                .ok_or_else(|| gst::loggable_error!(CAT, "format pod parse"))?];

            let conn = self.conn.lock();
            let conn = conn
                .as_ref()
                .ok_or_else(|| gst::loggable_error!(CAT, "prepare() before open()"))?;
            let guard = conn.thread_loop.lock();

            let props = properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Movie",
                *pw::keys::NODE_NAME => "FCast",
                // Ask for our segment size as the quantum, so one process()
                // drains ~one write(). The graph may clamp it.
                *pw::keys::NODE_LATENCY =>
                    format!("{}/{}", spec.segsize() as usize / bytes_per_frame, rate),
            };

            let stream = pw::stream::StreamRc::new(conn.core.clone(), "fcast-audio", props)
                .map_err(|e| gst::loggable_error!(CAT, "pw stream: {e}"))?;

            // The RT callback: memcpy-sized bridge-mutex sections ONLY.
            let shared = Arc::clone(&self.shared);
            let err_shared = Arc::clone(&self.shared);
            let err_obj = self.obj().downgrade();
            let listener = stream
                .add_local_listener::<()>()
                .state_changed(move |_stream, _data, old, new| {
                    if let StreamState::Error(msg) = &new {
                        gst::error!(CAT, "pw stream error: {msg}");
                        err_shared.mark_dead();
                        if let Some(obj) = err_obj.upgrade() {
                            gst::element_error!(
                                obj,
                                gst::ResourceError::Failed,
                                ("PipeWire stream error: {}", msg)
                            );
                        }
                    } else {
                        gst::debug!(CAT, "pw stream state {old:?} -> {new:?}");
                    }
                })
                .process(move |stream, _| {
                    // Unconditionally first. The graph proving it still
                    // schedules us. A cycle that finds no buffer or loses the
                    // try_lock below still ran.
                    shared
                        .cycles
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(mut pwbuf) = stream.dequeue_buffer() else {
                        return;
                    };
                    // The graph's suggested cycle size in frames (0 = unknown).
                    // Filling that rather than the whole mapped buffer keeps
                    // latency tight under a large negotiated buffer.
                    let requested = pwbuf.requested() as usize;
                    let datas = pwbuf.datas_mut();
                    let Some(data) = datas.first_mut() else { return };
                    let Some(slice) = data.data() else { return };

                    // RT discipline: NEVER block on the bridge mutex. It is not
                    // priority-inheriting and write() (normal priority) holds it,
                    // so blocking here stalls the whole graph cycle (priority
                    // inversion). On a miss, emit one cycle of silence.
                    let Some(mut bridge) = shared.bridge.try_lock() else {
                        let bytes_per_frame = shared
                            .bytes_per_frame
                            .load(std::sync::atomic::Ordering::Relaxed)
                            .max(1);
                        let mut want = slice.len();
                        if requested > 0 {
                            want = want.min(requested * bytes_per_frame);
                        }
                        want -= want % bytes_per_frame;
                        slice[..want].fill(0);
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = bytes_per_frame as i32;
                        *chunk.size_mut() = want as u32;
                        return;
                    };
                    let (filled, stride, drained) = {
                        let paused = bridge.paused;
                        let bytes_per_frame = bridge.bytes_per_frame.max(1);
                        if delay_trace() {
                            use std::sync::atomic::{AtomicU64, Ordering};
                            static COUNT: AtomicU64 = AtomicU64::new(0);
                            let n = COUNT.fetch_add(1, Ordering::Relaxed);
                            if n < 5 || n % 500 == 0 {
                                eprintln!(
                                    "pwsink process(): requested={requested} slice={} ring={} bpf={bytes_per_frame}",
                                    slice.len(),
                                    bridge.ring.len(),
                                );
                            }
                        }
                        let mut want = slice.len();
                        if requested > 0 {
                            want = want.min(requested * bytes_per_frame);
                        }
                        want -= want % bytes_per_frame;
                        // The ring only ever holds whole frames (write() appends
                        // whole segments), so `have` stays aligned. While
                        // soft-corked, hold the ring intact and emit silence.
                        let have = if paused {
                            0
                        } else {
                            bridge.ring.len().min(want)
                        };
                        let (a, b) = bridge.ring.as_slices();
                        let n1 = a.len().min(have);
                        slice[..n1].copy_from_slice(&a[..n1]);
                        if have > n1 {
                            slice[n1..have].copy_from_slice(&b[..have - n1]);
                        }
                        bridge.ring.drain(..have);
                        bridge.drained += have as u64;
                        let channels = bridge.channels;
                        let is_f32 = bridge.is_f32;
                        if have > 0 {
                            // Resuming after silence. Ramp the gain back in, a
                            // mid-waveform onset pops.
                            if bridge.resume_fade {
                                let fade = (have / bytes_per_frame).min(FADE_FRAMES);
                                apply_gain_ramp(slice, 0, fade, channels, is_f32, 0.0, 1.0);
                                bridge.resume_fade = false;
                            }
                            bridge.last_frame =
                                read_frame(slice, have - bytes_per_frame, channels, is_f32);
                        }
                        // Ring dry -> emit silence and keep the graph fed, never
                        // stall it waiting for data. Never a hard cut though.
                        // Fade the tail, or decay the held last frame.
                        if have < want {
                            if have > 0 {
                                let fade = (have / bytes_per_frame).min(FADE_FRAMES);
                                apply_gain_ramp(
                                    slice,
                                    have - fade * bytes_per_frame,
                                    fade,
                                    channels,
                                    is_f32,
                                    1.0,
                                    0.0,
                                );
                                slice[have..want].fill(0);
                            } else {
                                let fade = ((want - have) / bytes_per_frame).min(FADE_FRAMES);
                                for i in 0..fade {
                                    let gain = 1.0 - (i + 1) as f32 / fade as f32;
                                    let frame = [
                                        bridge.last_frame[0] * gain,
                                        bridge.last_frame[1] * gain,
                                    ];
                                    write_frame(
                                        slice,
                                        have + i * bytes_per_frame,
                                        channels,
                                        is_f32,
                                        frame,
                                    );
                                }
                                slice[have + fade * bytes_per_frame..want].fill(0);
                            }
                            bridge.last_frame = [0.0; 2];
                            bridge.resume_fade = true;
                            if !paused {
                                bridge.underruns += 1;
                            }
                        }
                        (want, bytes_per_frame as i32, have > 0)
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = stride;
                    *chunk.size_mut() = filled as u32;

                    // Only on progress. A progress-free notify (the paused
                    // silence path) would reset write()'s stall timer forever.
                    if drained {
                        shared.space.notify_all();
                    }
                })
                .register()
                .map_err(|e| gst::loggable_error!(CAT, "pw listener: {e}"))?;

            stream
                .connect(
                    spa::utils::Direction::Output,
                    None,
                    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                    &mut params,
                )
                .map_err(|e| gst::loggable_error!(CAT, "pw stream connect: {e}"))?;

            // BOUNDED wait for the graph to run cycles on our stream. Running
            // time starts at PLAYING regardless, so every ms not consuming by
            // then is instant negative skew. StreamState::Streaming is NOT the
            // signal, only process() callbacks prove consumption.
            drop(guard);
            let deadline = std::time::Instant::now() + Duration::from_millis(1500);
            let died = loop {
                if self.shared.bridge.lock().dead {
                    break true;
                }
                if self
                    .shared
                    .cycles
                    .load(std::sync::atomic::Ordering::Relaxed)
                    >= 2
                {
                    break false;
                }
                if std::time::Instant::now() >= deadline {
                    gst::warning!(CAT, "no pw graph cycles within 1.5s; starting anyway");
                    break false;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            if died {
                // The loop thread is still dispatching, so the stream and
                // listener must die under the loop lock like every other pw
                // object. A bare `return Err` would drop them unlocked.
                let _guard = conn.thread_loop.lock();
                drop(listener);
                drop(stream);
                return Err(gst::loggable_error!(CAT, "pw stream died in prepare()"));
            }

            *self.stream.lock() = Some(PwStream {
                stream,
                _listener: listener,
                rate,
            });

            // Declare the route's latency before the sink prerolls. The pipeline
            // runs its LATENCY query once preroll completes, and this is the only
            // window where the answer is right from the first frame.
            self.sync_render_delay();
            Ok(())
        }

        fn write(&self, data: &[u8]) -> Result<i32, gst::LoggableError> {
            // Cheap re-check for a route that changed under us. The declared
            // latency has to follow it. Before the bridge lock because the
            // stream mutex is always taken first (see delay()).
            let writes = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if writes.is_multiple_of(32) {
                self.sync_render_delay();
            }

            let mut bridge = self.shared.bridge.lock();
            // Never true in practice, but a too-small ring must not livelock.
            if bridge.capacity < data.len() {
                bridge.capacity = data.len() * 2;
            }
            // Progress = the ring actually shrinking. Judging by wakeups lets a
            // drip of progress-free notifies reset the stall clock forever.
            let mut last_progress = std::time::Instant::now();
            let mut last_fill = bridge.ring.len();
            // A trickle of REAL progress resets that clock too, so bound the
            // total time this one call may stay blocked as well.
            let mut blocked_since = last_progress;
            let mut was_corked = bridge.paused;
            let limits = stall_limits();
            loop {
                if bridge.dead {
                    return Err(gst::loggable_error!(CAT, "pw stream is dead"));
                }
                if bridge.flushing || bridge.shutting_down {
                    // Swallow the data. The base class owns flush and teardown
                    // semantics, and returning the full length lets the ring
                    // deactivation join us.
                    return Ok(data.len() as i32);
                }
                if bridge.ring.len() + data.len() <= bridge.capacity {
                    bridge.ring.extend(data);
                    return Ok(data.len() as i32);
                }
                if bridge.ring.len() < last_fill {
                    last_fill = bridge.ring.len();
                    last_progress = std::time::Instant::now();
                    // The graph is consuming again, so a later stall is news.
                    bridge.stalled = false;
                }
                if bridge.paused {
                    was_corked = true;
                } else if was_corked {
                    // Leaving a cork. The cork WAS the reason nothing drained, so
                    // every stall clock restarts here, or any pause longer than
                    // WRITE_STALL_LIMIT would error out on resume.
                    was_corked = false;
                    last_fill = bridge.ring.len();
                    last_progress = std::time::Instant::now();
                    blocked_since = last_progress;
                }
                let blocked = last_progress.elapsed();
                let (_, current, pending) = self.obj().state(Some(gst::ClockTime::ZERO));
                if bridge.paused {
                    // A settled pause legitimately never drains. Block for as long
                    // as the user pauses (resume/flush/teardown/death all notify).
                    // Nothing else may park here:
                    if pending != gst::State::VoidPending {
                        // A transition is waiting on this write to RETURN, so
                        // blocking wedges PLAYING->PAUSED for as long as the cork
                        // holds. Swallow it, one segment lost at worst.
                        if blocked >= WRITE_STALL_LIMIT {
                            gst::warning!(
                                CAT,
                                imp = self,
                                "write blocked {blocked:?} under a pending transition \
                                 (corked, ring {}/{}); handing the segment back",
                                bridge.ring.len(),
                                bridge.capacity
                            );
                            return Ok(data.len() as i32);
                        }
                    } else if current == gst::State::Playing
                        && blocked >= CORK_RECONCILE_AFTER
                        && std::env::var_os("FCAST_PW_NO_CORK_RECONCILE").is_none()
                    {
                        // Corked with the element SETTLED at PLAYING = a lost
                        // uncork, and nothing else ever clears it.
                        gst::warning!(
                            CAT,
                            imp = self,
                            "soft-cork held while the element is settled PLAYING; clearing \
                             the stale cork (ring {}/{} bytes)",
                            bridge.ring.len(),
                            bridge.capacity
                        );
                        bridge.paused = false;
                        // The next iteration restarts the stall clocks (see the
                        // corked-edge reset above), or this recovery would turn
                        // straight into a stall error.
                        continue;
                    }
                } else {
                    // Un-corked. The graph owes us space at the device rate, so
                    // every way of not getting it is bounded. NEVER park here
                    // unbounded.
                    let snap = StallSnapshot {
                        current,
                        pending,
                        corked: false,
                        since_progress: blocked,
                        blocked_total: blocked_since.elapsed(),
                        since_cycle: self.observe_cycles(
                            self.shared
                                .cycles
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                    };
                    let verdict = write_stall_verdict(&snap, &limits);
                    if verdict != StallVerdict::Wait {
                        let detail = format!(
                            "{} (ring {}/{} bytes, blocked {:?}, nothing freed for {:?}, \
                             last process cycle {:?} ago)",
                            verdict.reason(),
                            bridge.ring.len(),
                            bridge.capacity,
                            snap.blocked_total,
                            snap.since_progress,
                            snap.since_cycle,
                        );
                        if !verdict.escalates(no_stall_element_error()) {
                            drop(bridge);
                            return Err(gst::loggable_error!(CAT, "{detail}"));
                        }
                        if !bridge.stalled {
                            bridge.stalled = true;
                            // Report outside the bridge lock. Posting can reach a
                            // sync bus handler, which must never run under it.
                            drop(bridge);
                            gst::error!(CAT, imp = self, "{detail}");
                            gst::element_imp_error!(
                                self,
                                gst::ResourceError::Failed,
                                ("PipeWire playback stalled: {}", detail)
                            );
                            bridge = self.shared.bridge.lock();
                        }
                        // Reported once. Keep waiting at the same cadence
                        // rather than failing the
                        // segment (gstaudiosink skips a refused
                        // one and calls straight back, a busy loop). Teardown,
                        // flush and death still break the loop at the top.
                    }
                }
                let _ = self.shared.space.wait_for(&mut bridge, WRITE_STALL_STEP);
            }
        }

        fn delay(&self) -> u32 {
            // Frames handed over but not yet taken by the graph: the bridge ring
            // plus pw's `buffered` (pw_stream_get_time_n, the one pw call made
            // without the loop lock). Both drain, so EOS waits terminate.
            //
            // Deliberately NOT the fixed graph->device latency, which never
            // drains. The base class subtracts this from its audio clock, so
            // folding a constant in would park that clock behind the pipeline
            // clock and the slaving would read it as drift forever. That part is
            // declared as the render delay instead (see `sync_render_delay`).
            let mut pw_frames: u64 = 0;
            let mut trace: Option<String> = None;
            if let Some(s) = self.stream.lock().as_ref() {
                if let Ok(t) = s.stream.time() {
                    let rate = t.rate();
                    pw_frames += t.buffered();
                    if no_device_latency() && rate.num > 0 && rate.denom > 0 {
                        // Hatch: fold the device latency back in and leave the
                        // compensation to the slaving.
                        pw_frames += t.delay().max(0) as u64 * rate.num as u64 * s.rate as u64
                            / rate.denom as u64;
                    }
                    if delay_trace() {
                        trace = Some(format!(
                            "delay={} rate={}/{} buffered={} queued={} queued_bufs={} pw_frames={} declared={}",
                            t.delay(),
                            rate.num,
                            rate.denom,
                            t.buffered(),
                            t.queued(),
                            t.queued_buffers(),
                            pw_frames,
                            self.announced_device_delay
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ));
                    }
                }
            }
            let bridge = self.shared.bridge.lock();
            if bridge.dead {
                return 0;
            }
            let ring_frames = (bridge.ring.len() / bridge.bytes_per_frame.max(1)) as u64;
            if let Some(trace) = trace {
                use std::sync::atomic::{AtomicU64, Ordering};
                static COUNT: AtomicU64 = AtomicU64::new(0);
                if COUNT.fetch_add(1, Ordering::Relaxed) % 50 == 0 {
                    eprintln!("pwsink delay(): ring_frames={ring_frames} {trace}");
                }
            }
            (ring_frames + pw_frames).min(u32::MAX as u64) as u32
        }

        fn reset(&self) {
            // Called for flushes AND on pause (GstAudioSink funnels both here to
            // unblock a pending write()). Only a real flush discards data, the
            // pause path soft-corks (see Bridge::paused) so delay() holds steady.
            // Must unblock write() immediately either way.
            let clear = {
                let mut bridge = self.shared.bridge.lock();
                bridge.flushing = true;
                let clear = !bridge.paused;
                if clear {
                    // last_frame stays, so the next process() cycle ramps the
                    // discarded waveform down instead of hard-cutting.
                    bridge.ring.clear();
                }
                clear
            };
            self.shared.space.notify_all();
            // LOCK ORDER: never take the thread-loop lock while holding the
            // bridge mutex (process() takes the bridge under the loop thread).
            if clear {
                let conn_slot = self.conn.lock();
                let stream_slot = self.stream.lock();
                if let (Some(conn), Some(s)) = (conn_slot.as_ref(), stream_slot.as_ref()) {
                    let _guard = conn.thread_loop.lock();
                    let _ = s.stream.flush(false);
                }
            }
            let mut bridge = self.shared.bridge.lock();
            bridge.flushing = false;
        }

        fn unprepare(&self) -> Result<(), gst::LoggableError> {
            // No drain here. The EOS tail goes out on the PLAYING->PAUSED edge
            // (see change_state), the only point where the graph is still
            // consuming.
            //
            // LOCK ORDER: conn before stream, as in reset() and FlushStop. An
            // inverted pair here is an ABBA deadlock.
            let conn = self.conn.lock();
            if let Some(s) = self.stream.lock().take() {
                if let Some(conn) = conn.as_ref() {
                    let _guard = conn.thread_loop.lock();
                    let _ = s.stream.disconnect();
                    drop(s); // stream + listener die under the loop lock
                }
            }
            drop(conn);
            let underruns = self.shared.bridge.lock().underruns;
            gst::debug!(CAT, "unprepared; {underruns} underrun/idle process cycles");
            Ok(())
        }

        fn close(&self) -> Result<(), gst::LoggableError> {
            // Drop listener + core + context under the loop lock, and stop the
            // thread_loop LAST (callbacks must be dead first).
            if let Some(conn) = self.conn.lock().take() {
                {
                    let _guard = conn.thread_loop.lock();
                    drop(conn.core_listener);
                    drop(conn.core);
                    drop(conn.context);
                }
                conn.thread_loop.stop();
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn limits() -> StallLimits {
            StallLimits {
                no_progress: WRITE_STALL_LIMIT,
                total: Some(WRITE_TOTAL_BLOCK_LIMIT),
                cycle: Some(CYCLE_STALL_LIMIT),
            }
        }

        /// A write blocked on a healthy graph: settled PLAYING, un-corked,
        /// space freed a moment ago.
        fn healthy() -> StallSnapshot {
            StallSnapshot {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                corked: false,
                since_progress: Duration::from_millis(20),
                blocked_total: Duration::from_millis(20),
                since_cycle: Duration::from_millis(20),
            }
        }

        #[test]
        fn a_normally_blocked_write_keeps_waiting() {
            assert_eq!(
                write_stall_verdict(&healthy(), &limits()),
                StallVerdict::Wait
            );
        }

        #[test]
        fn nothing_freed_bails_out_in_any_state() {
            // Un-corked: the graph owes us space whatever the element is doing.
            for state in [gst::State::Playing, gst::State::Paused] {
                let snap = StallSnapshot {
                    current: state,
                    since_progress: WRITE_STALL_LIMIT,
                    blocked_total: WRITE_STALL_LIMIT,
                    since_cycle: Duration::ZERO,
                    ..healthy()
                };
                assert_eq!(
                    write_stall_verdict(&snap, &limits()),
                    StallVerdict::NoProgress
                );
            }
            // Posts an element error unless levered back to the invisible
            // skipped-segment handling.
            assert!(StallVerdict::NoProgress.escalates(false));
            assert!(!StallVerdict::NoProgress.escalates(true));
        }

        #[test]
        fn a_trickle_that_defeats_the_progress_clock_trips_the_total_cap() {
            // Space freed often enough to reset the per-progress clock, with
            // real time frozen anyway.
            let snap = StallSnapshot {
                since_progress: Duration::from_millis(1_900),
                blocked_total: WRITE_TOTAL_BLOCK_LIMIT,
                since_cycle: Duration::from_millis(1_900),
                ..healthy()
            };
            assert_eq!(
                write_stall_verdict(&snap, &limits()),
                StallVerdict::TotalBlocked
            );
            assert!(StallVerdict::TotalBlocked.escalates(true));
            // One second short of the cap it is still just backpressure.
            let snap = StallSnapshot {
                blocked_total: WRITE_TOTAL_BLOCK_LIMIT - Duration::from_secs(1),
                ..snap
            };
            assert_eq!(write_stall_verdict(&snap, &limits()), StallVerdict::Wait);
        }

        #[test]
        fn a_graph_that_stopped_cycling_trips_the_watchdog() {
            let snap = StallSnapshot {
                since_progress: Duration::from_millis(500),
                blocked_total: Duration::from_millis(500),
                since_cycle: CYCLE_STALL_LIMIT,
                ..healthy()
            };
            assert_eq!(
                write_stall_verdict(&snap, &limits()),
                StallVerdict::NoCycles
            );
            assert!(StallVerdict::NoCycles.escalates(true));
        }

        #[test]
        fn the_most_specific_cause_is_reported() {
            let snap = StallSnapshot {
                since_progress: WRITE_TOTAL_BLOCK_LIMIT,
                blocked_total: WRITE_TOTAL_BLOCK_LIMIT,
                since_cycle: WRITE_TOTAL_BLOCK_LIMIT,
                ..healthy()
            };
            assert_eq!(
                write_stall_verdict(&snap, &limits()),
                StallVerdict::NoCycles
            );
        }

        #[test]
        fn a_cork_never_trips_anything() {
            // A settled pause blocks for as long as the user pauses. The paused
            // arms in write() own that path.
            let snap = StallSnapshot {
                corked: true,
                current: gst::State::Paused,
                since_progress: Duration::from_secs(3_600),
                blocked_total: Duration::from_secs(3_600),
                since_cycle: Duration::from_secs(3_600),
                ..healthy()
            };
            assert_eq!(write_stall_verdict(&snap, &limits()), StallVerdict::Wait);
            // Even a cork held while PLAYING is the reconcile's business.
            let snap = StallSnapshot {
                current: gst::State::Playing,
                ..snap
            };
            assert_eq!(write_stall_verdict(&snap, &limits()), StallVerdict::Wait);
        }

        #[test]
        fn startup_teardown_and_transitions_never_trip_the_new_checks() {
            // prepare()'s own startup wait is the only judge before PLAYING, and
            // a transition or a teardown may legitimately stop the draining.
            for (current, pending) in [
                (gst::State::Null, gst::State::VoidPending),
                (gst::State::Ready, gst::State::Paused),
                (gst::State::Paused, gst::State::Playing),
                (gst::State::Playing, gst::State::Paused),
                (gst::State::Playing, gst::State::Ready),
            ] {
                let snap = StallSnapshot {
                    current,
                    pending,
                    since_progress: Duration::from_millis(500),
                    blocked_total: Duration::from_secs(60),
                    since_cycle: Duration::from_secs(60),
                    ..healthy()
                };
                assert_eq!(
                    write_stall_verdict(&snap, &limits()),
                    StallVerdict::Wait,
                    "{current:?}/{pending:?} must not accuse the graph"
                );
            }
        }

        #[test]
        fn each_lever_restores_the_old_behaviour() {
            let stalled = StallSnapshot {
                since_progress: Duration::from_millis(500),
                blocked_total: Duration::from_secs(60),
                since_cycle: Duration::from_secs(60),
                ..healthy()
            };
            let no_cycle = StallLimits {
                cycle: None,
                ..limits()
            };
            assert_eq!(
                write_stall_verdict(&stalled, &no_cycle),
                StallVerdict::TotalBlocked
            );
            let neither = StallLimits {
                cycle: None,
                total: None,
                ..limits()
            };
            // Only the per-progress clock, which a trickle resets forever.
            assert_eq!(write_stall_verdict(&stalled, &neither), StallVerdict::Wait);
        }

        /// The known silent stall modes and which bail-out (if any) sees each.
        /// The three that read as `Wait` are uncovered BY DESIGN: from inside
        /// the sink they are indistinguishable from a legitimate pause
        /// or from healthy playback, and only the receiver's freeze
        /// watchdog sees them.
        #[test]
        fn the_known_silent_stall_modes_map_to_verdicts() {
            let stopped = Duration::from_secs(10);
            let cases: [(&str, StallSnapshot, StallVerdict); 7] = [
                (
                    "1. the graph stops scheduling a Streaming node",
                    StallSnapshot {
                        since_progress: stopped,
                        blocked_total: stopped,
                        since_cycle: stopped,
                        ..healthy()
                    },
                    StallVerdict::NoCycles,
                ),
                (
                    // Cycles are counted before the dequeue, so the graph still
                    // reads as running. The ring not moving catches this one.
                    "2. dequeue_buffer() returns None every cycle",
                    StallSnapshot {
                        since_progress: WRITE_STALL_LIMIT,
                        blocked_total: WRITE_STALL_LIMIT,
                        since_cycle: Duration::ZERO,
                        ..healthy()
                    },
                    StallVerdict::NoProgress,
                ),
                (
                    "3. the stream is moved to Paused/Unconnected",
                    StallSnapshot {
                        since_progress: stopped,
                        blocked_total: stopped,
                        since_cycle: stopped,
                        ..healthy()
                    },
                    StallVerdict::NoCycles,
                ),
                (
                    "4. the endless 2s-per-segment skip crawl",
                    StallSnapshot {
                        since_progress: WRITE_STALL_LIMIT,
                        blocked_total: WRITE_STALL_LIMIT,
                        since_cycle: Duration::from_millis(20),
                        ..healthy()
                    },
                    StallVerdict::NoProgress,
                ),
                (
                    "5. a settled-PAUSED cork parks the writer for ever",
                    StallSnapshot {
                        corked: true,
                        current: gst::State::Paused,
                        since_progress: stopped,
                        blocked_total: stopped,
                        since_cycle: stopped,
                        ..healthy()
                    },
                    StallVerdict::Wait,
                ),
                (
                    "6. FCAST_PW_NO_CORK_RECONCILE removes the only escape",
                    StallSnapshot {
                        corked: true,
                        since_progress: stopped,
                        blocked_total: stopped,
                        since_cycle: stopped,
                        ..healthy()
                    },
                    StallVerdict::Wait,
                ),
                (
                    "7. the graph consumes normally but nothing is audible",
                    healthy(),
                    StallVerdict::Wait,
                ),
            ];
            for (mode, snap, want) in cases {
                assert_eq!(write_stall_verdict(&snap, &limits()), want, "{mode}");
            }
            // 8. the sink's own pw thread-loop wedges. No callbacks reach us,
            // which is mode 1 seen from the writer's side.
            assert_eq!(
                write_stall_verdict(
                    &StallSnapshot {
                        since_progress: stopped,
                        blocked_total: stopped,
                        since_cycle: stopped,
                        ..healthy()
                    },
                    &limits()
                ),
                StallVerdict::NoCycles
            );
        }

        /// `prepare()`'s ring sizing, so the cap's arithmetic is checked
        /// instead of merely asserted in a comment.
        fn ring_capacity(segsize: usize, bytes_per_frame: usize) -> usize {
            segsize.max(bytes_per_frame * 1024) * 2
        }

        #[test]
        fn a_healthy_graph_never_blocks_one_write_for_seconds() {
            // At 48kHz stereo F32 with the default 10ms segment a FULL ring is
            // 43ms of audio, the steady state, never evidence of a stall.
            assert_eq!(ring_capacity(3840, 8), 16384);

            // A blocked write() needs ONE segment of room and the graph consumes
            // at the device rate, so the wait is one segment of audio however big
            // the ring is.
            for (rate, bpf, segsize) in [
                (48_000usize, 8usize, 3_840usize),
                (48_000, 8, 9_600),
                (8_000, 2, 160),
                (44_100, 4, 3_528),
            ] {
                let segment = Duration::from_secs_f64(segsize as f64 / (bpf * rate) as f64);
                assert!(
                    segment * 20 < WRITE_TOTAL_BLOCK_LIMIT,
                    "{rate}Hz bpf={bpf} seg={segsize}: one segment is {segment:?}, \
                     too close to the {WRITE_TOTAL_BLOCK_LIMIT:?} cap"
                );
                // The whole ring, the most a graph could ever owe us.
                let ring = Duration::from_secs_f64(
                    ring_capacity(segsize, bpf) as f64 / (bpf * rate) as f64,
                );
                assert!(ring * 8 < WRITE_TOTAL_BLOCK_LIMIT, "ring spans {ring:?}");
            }
        }
    }
}

glib::wrapper! {
    pub struct PwAudioSink(ObjectSubclass<imp::PwAudioSink>)
        @extends gst_audio::AudioSink, gst_audio::AudioBaseSink, gst_base::BaseSink,
                 gst::Element, gst::Object;
}

/// Cached probe: is there a reachable PipeWire daemon? Decides once per process
/// whether the receiver builds `fcastpwaudiosink` or falls back to
/// autoaudiosink.
pub fn is_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        pipewire::init();
        // SAFETY: same loop-lock discipline as the element (see PwConn).
        let Ok(thread_loop) =
            (unsafe { pipewire::thread_loop::ThreadLoopRc::new(Some("fcast-pw-probe"), None) })
        else {
            return false;
        };
        thread_loop.start();
        let ok = {
            let _guard = thread_loop.lock();
            // connect_rc dials the daemon socket, so this is a real probe.
            // Context/core drop under the guard, per the loop-lock contract.
            pipewire::context::ContextRc::new(&thread_loop, None)
                .and_then(|ctx| ctx.connect_rc(None).map(|core| (ctx, core)))
                .is_ok()
        };
        thread_loop.stop();
        ok
    })
}

/// Rank NONE. The receiver selects it explicitly, autoplugging never should.
pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcastpwaudiosink",
        gst::Rank::NONE,
        PwAudioSink::static_type(),
    )
}
