//! `fcastpwaudiosink`: a native PipeWire audio sink.
//!
//! Failure discipline: this element must NEVER park a thread unboundedly or
//! stall silently, those are the diseases it exists to treat. A dead
//! daemon/stream posts an element error (core + stream listeners), a graph
//! that stops consuming posts one from `write()` itself (returning -1 alone
//! reaches nobody, see [`imp::StallVerdict::escalates`]), and `reset()`
//! aborts a blocked `write()` immediately.

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

    /// How long `write()` may go without the pw graph freeing any ring
    /// space before it errors out. Long enough for a cold connect + session
    /// manager routing (~300ms) and default-device moves (<100ms), far
    /// shorter than the settle timeouts a silent stall would otherwise eat.
    const WRITE_STALL_LIMIT: Duration = Duration::from_secs(2);
    const WRITE_STALL_STEP: Duration = Duration::from_millis(100);
    /// How long `write()` tolerates the soft-cork before checking it against
    /// the element's own state. A cork is only legitimate while the element
    /// is PAUSED. Held with the element PLAYING it is stale (a lost uncork)
    /// and parks the writer forever with no error anywhere: the
    /// `forzen-at-the-start.txt` freeze (ring thread in `wait_for`, every
    /// pw loop idle, no stall error because stall accounting is paused).
    const CORK_RECONCILE_AFTER: Duration = Duration::from_secs(5);
    /// How long ONE un-corked `write()` call may stay blocked in total, even
    /// while the graph keeps freeing a little space (which resets the
    /// per-progress clock above, forever, while real time is frozen).
    ///
    /// A healthy graph is nowhere near it: a blocked `write()` needs ONE
    /// segment of room and the graph consumes at the device rate, so the
    /// wait is one segment of audio. The field sizing (`ring 16224/16384`)
    /// is 48kHz stereo F32 with a 10ms segment: the WHOLE ring is 2048
    /// frames = 43ms, one segment 10ms. Even a 200ms buffer-time
    /// negotiation only makes it 200ms, so 6s is >=30x the worst legitimate
    /// block, and it still covers a device suspend/resume or a Bluetooth
    /// route move (<1s). See the `a_healthy_graph_...` test.
    const WRITE_TOTAL_BLOCK_LIMIT: Duration = Duration::from_secs(6);
    /// How long the graph may deliver NO process() callback at all while
    /// the element is settled at PLAYING and un-corked. Cycles are the only
    /// direct "the graph is running our stream" signal (`BridgeShared::cycles`,
    /// what `prepare()` waits on). A stream that stops being cycled plays
    /// nothing and reports nothing, which is the freeze this treats.
    const CYCLE_STALL_LIMIT: Duration = Duration::from_secs(5);

    /// `FCAST_PW_DELAY_TRACE=1`: eprintln the delay()/process() internals
    /// (rate-limited), the A/V-sync debugging view. Cached: the process
    /// callback is RT. Kept as an eprintln hatch because the gst debug
    /// system isn't usable from the RT callback in this binary, and
    /// delay-reporting quirks are device/driver-dependent and recur.
    fn delay_trace() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_DELAY_TRACE").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_DEVICE_LATENCY=1`: stop declaring the graph->device
    /// latency as the sink's render delay and fold it back into `delay()`
    /// (the behaviour before that split, see [`PwAudioSink::device_delay`]).
    /// An A/B hatch: latency reporting is route- and driver-dependent, and a
    /// device that lies about it is better served by the base class's
    /// slaving than by a wrong constant.
    fn no_device_latency() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_DEVICE_LATENCY").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_TOTAL_STALL_CAP=1`: bail out of a blocked `write()` only
    /// on the per-progress clock, the behaviour before
    /// [`WRITE_TOTAL_BLOCK_LIMIT`]. A graph that frees space in a trickle can
    /// then block one write, and playback, for as long as it likes.
    fn no_total_stall_cap() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_TOTAL_STALL_CAP").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_CYCLE_WATCHDOG=1`: stop treating "the graph delivered no
    /// process() callback for [`CYCLE_STALL_LIMIT`]" as a failure.
    fn no_cycle_watchdog() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_CYCLE_WATCHDOG").is_ok_and(|v| v == "1"))
    }

    /// `FCAST_PW_NO_STALL_ELEMENT_ERROR=1`: restore the old, INVISIBLE
    /// handling of the [`WRITE_STALL_LIMIT`] stall. `write()` returned a
    /// LoggableError, the gstreamer-rs trampoline dropped it unlogged
    /// (`imp.write(..).unwrap_or(-1)`), and gstaudiosink turned the -1 into a
    /// debug warning plus a skipped segment: no element error ever reached
    /// the bus, so a stalled graph was a silent freeze by construction.
    fn no_stall_element_error() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_NO_STALL_ELEMENT_ERROR").is_ok_and(|v| v == "1"))
    }

    /// Why a blocked un-corked `write()` gives up, most specific first.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StallVerdict {
        /// Keep waiting, the block is still within every bound.
        Wait,
        /// No process() callback at all: the graph is not running our stream.
        NoCycles,
        /// Space is freed, but so slowly that this write() (and playback)
        /// has been blocked past [`WRITE_TOTAL_BLOCK_LIMIT`].
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
        /// handing -1 back for one skipped segment. A negative `write()`
        /// alone is NOT loud: gstaudiosink logs a debug warning, skips the
        /// segment and calls straight back
        /// (`audioringbuffer_thread_func`), and the binding drops our
        /// LoggableError unlogged, so nothing reaches the bus and the
        /// skip-crawl spins. Only `FCAST_PW_NO_STALL_ELEMENT_ERROR` keeps
        /// that old handling, and only for the plain no-progress stall.
        fn escalates(self, stall_errors_levered_off: bool) -> bool {
            match self {
                StallVerdict::Wait => false,
                StallVerdict::NoProgress => !stall_errors_levered_off,
                StallVerdict::NoCycles | StallVerdict::TotalBlocked => true,
            }
        }
    }

    /// Everything the bail-out policy looks at, snapshotted so the decision
    /// is a pure function (unit-tested, no pw graph or element needed).
    #[derive(Debug, Clone, Copy)]
    struct StallSnapshot {
        current: gst::State,
        pending: gst::State,
        /// Soft-corked: the pause path owns that case entirely.
        corked: bool,
        /// Since the ring last shrank.
        since_progress: Duration,
        /// Since this write() call started blocking.
        blocked_total: Duration,
        /// Since `BridgeShared::cycles` last moved (kept across write() calls).
        since_cycle: Duration,
    }

    /// The limits in force, so the env levers stay out of the decision.
    /// `None` = that check is levered off.
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
        // A soft-cork legitimately never drains and has its own bounded
        // arms (hand the segment back under a pending transition, reconcile
        // a cork held while settled PLAYING).
        if snap.corked {
            return StallVerdict::Wait;
        }
        // The two graph-is-stopped verdicts are only knowable with the
        // element settled at PLAYING: a preroll, a transition in flight and
        // a teardown all stop the graph from draining for good reasons, and
        // `prepare()` owns the startup wait.
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

    /// Re-declare the device latency only once it has moved this far. Every
    /// change posts a LATENCY message and makes the whole pipeline
    /// redistribute, and the leftover rounding sits far inside the base
    /// class's 20ms slaving tolerance, which absorbs it silently.
    const RENDER_DELAY_HYSTERESIS: gst::ClockTime = gst::ClockTime::from_mseconds(5);

    /// Sanity cap on a reported device latency. A2DP tops out near 300ms
    /// (headset delay report plus transport), so anything past this is a
    /// broken report: inflating the pipeline's latency by it would be worse
    /// than ignoring it.
    const MAX_DEVICE_DELAY: gst::ClockTime = gst::ClockTime::from_mseconds(1000);

    /// How long the PLAYING->PAUSED edge may wait for the graph to pick up
    /// an EOS tail still sitting in the bridge (see
    /// [`PwAudioSink::drain_eos_tail`]). A segment or two in practice.
    const TAIL_DRAIN_LIMIT: Duration = Duration::from_millis(250);
    const TAIL_DRAIN_STEP: Duration = Duration::from_millis(10);

    /// The `write()` <-> pw-`process` bridge.
    ///
    /// `write()` (audiobasesink's ring thread) blocks while the ring is
    /// full, that back-pressure is what paces the base class. The pw
    /// process callback (thread-loop RT thread) drains it. `flushing`
    /// aborts a blocked `write()` IMMEDIATELY, a `write()` parked through
    /// a flush is exactly the wedge class this element exists to kill.
    struct Bridge {
        ring: VecDeque<u8>,
        /// Capacity in bytes, sized in `prepare()` to ~2 spec segments so
        /// the base class's own ring stays the dominant buffer.
        capacity: usize,
        flushing: bool,
        /// Latched by the core/stream error listeners: the stream will
        /// never consume again. `write()` errors out, `delay()` reports 0
        /// (so EOS/drain waits can't hang on a corpse). Never cleared,
        /// the receiver builds a fresh sink per load.
        dead: bool,
        /// Of the negotiated format, for delay math.
        bytes_per_frame: usize,
        /// Channel count of the negotiated format (<=2 by the template).
        channels: usize,
        /// Whether samples are F32LE (else S16LE), for the de-click math.
        is_f32: bool,
        /// The last real frame emitted (as f32 per channel): the seed for
        /// the de-click ramp when data stops (underrun, flush, EOS), a
        /// hard cut from non-zero amplitude is an audible pop.
        last_frame: [f32; 2],
        /// Set when a cycle emitted silence, the next real data gets a
        /// short gain ramp-in (resuming mid-waveform pops too).
        resume_fade: bool,
        /// Soft-cork (pulsesink's pause semantics): process() emits
        /// silence WITHOUT draining and reset() keeps the ring. Without
        /// this, the pause-path reset() clears the ring, delay() drops by
        /// the ring fill, the audio clock jumps forward, and the slaving
        /// grinds the jump out as audible skips right after resume. Real
        /// flushes clear the ring via the FlushStop event instead (a
        /// flush while paused may never reach reset() at all).
        paused: bool,
        /// PAUSED->READY teardown latch: gst_audio_ring_buffer_activate(FALSE)
        /// JOINS the writer thread without any reset() first, a write()
        /// blocked on a full soft-corked ring would deadlock the state
        /// change (measured: fcastplaybin worker stuck in the join, whole
        /// receiver wedged). Set before chaining the transition, cleared
        /// only by the next prepare().
        shutting_down: bool,
        /// EOS reached and not flushed away since: the stream ended for
        /// real, so the tail is owed a bounded drain on the way out of
        /// PLAYING instead of being cut. Cleared by FlushStop and by every
        /// `prepare()`.
        eos: bool,
        /// Bytes process() has taken out of the ring, monotonic. The only
        /// way to wait for a specific piece of audio to reach the graph:
        /// the ring itself never empties, because the base class's writer
        /// thread keeps handing us whole segments (silence for the ones
        /// nothing was committed to) for as long as its ring is started.
        drained: u64,
        /// Process cycles that found less data than they wanted. Counts
        /// idle/paused silence-fill cycles too, a coarse stat, logged at
        /// unprepare for quantum sanity-checking, not an error signal.
        underruns: u64,
        /// A stall has been reported for this prepared stream: post the
        /// element error once, and park the writer instead of failing every
        /// segment (gstaudiosink skips a refused segment and calls straight
        /// back, which with an unresponsive graph is a busy loop). Cleared
        /// by the next drain and by every `prepare()`.
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

    /// ~5.3ms at 48kHz, long enough to de-click even pure tones (the
    /// worst case), short enough to be inaudible as a fade.
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
        /// callback's no-lock silence path (0 until first prepare()).
        bytes_per_frame: std::sync::atomic::AtomicUsize,
        /// process() callbacks since `prepare()`, the one direct "the graph
        /// is running our stream" signal (StreamState::Streaming only means
        /// the node is active, a suspended device delays the first cycle by
        /// its resume time). Bumped BEFORE the bridge try_lock, so a cycle
        /// that loses the race still counts: the cycle watchdog must not
        /// read lock contention as a dead graph.
        cycles: std::sync::atomic::AtomicU64,
    }

    impl BridgeShared {
        /// The stream is gone for good: unblock and fail any current or
        /// future `write()`.
        fn mark_dead(&self) {
            self.bridge.lock().dead = true;
            self.space.notify_all();
        }
    }

    /// Held while the element is OPEN (pw connection up). Everything pw
    /// must be constructed AND dropped under the thread-loop lock.
    ///
    /// SAFETY CONTRACT for the `unsafe impl Send`: pipewire-rs types are
    /// deliberately `!Send` because libpipewire objects are loop-affine.
    /// The C contract allows use from other threads iff the thread-loop
    /// lock is held, every access to these fields below takes
    /// `thread_loop.lock()` first (and construction/drop happen under it
    /// too). One documented exception: `pw_stream_get_time_n` is RT- and
    /// thread-safe (seqlock read) and is called lock-free from `delay()`.
    struct PwConn {
        thread_loop: pw::thread_loop::ThreadLoopRc,
        context: pw::context::ContextRc,
        core: pw::core::CoreRc,
        /// Daemon death must post an element error, never leave a silent
        /// zombie stream. Dies (under the loop lock) in `close()`.
        core_listener: pw::core::Listener,
    }
    unsafe impl Send for PwConn {}

    /// Held while PREPARED (stream connected for a concrete format).
    /// Same Send contract as `PwConn` (loop lock).
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
        /// Nanoseconds of device latency last handed to `set_render_delay`,
        /// so a re-check only pays for a bus message when the route actually
        /// changed (see [`PwAudioSink::sync_render_delay`]).
        announced_device_delay: std::sync::atomic::AtomicU64,
        /// `write()` calls, to keep the device-latency re-check off the hot
        /// path (one segment per call, so every 32nd is a third of a second).
        writes: std::sync::atomic::AtomicU64,
        /// Cycle-watchdog state, writer thread only: the last observed
        /// `BridgeShared::cycles` and when it last moved. Kept ACROSS write()
        /// calls, because a bail-out ends the call but gstaudiosink just
        /// skips that segment and calls again, and because a graph that died
        /// during a pause must be caught on the first write after the resume.
        cycle_probe: Mutex<Option<(u64, std::time::Instant)>>,
    }

    impl PwAudioSink {
        /// The fixed graph->device latency: everything between handing a
        /// frame to the pw graph and it becoming audible. Graph filters, the
        /// device's own buffering, and on a Bluetooth route the A2DP/BAP
        /// transport plus the headset's own delay report (pipewire's bluez5
        /// sink publishes all of that as its port latency, which is what
        /// `pw_time.delay` carries).
        ///
        /// This is not queueing: it never drains, it is a property of the
        /// route. That distinction is the whole point, see `delay()` for the
        /// queued half and `sync_render_delay()` for what this half is for.
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
            // Saturate rather than panic on a nonsense report, the caller caps it anyway.
            Some(gst::ClockTime::from_nseconds(
                ns.min(gst::ClockTime::MAX.nseconds() as u128) as u64,
            ))
        }

        /// Declare the device latency to the base class whenever it moves
        /// enough to be worth a pipeline-wide redistribution.
        ///
        /// `set_render_delay()` is GstBaseSink's mechanism for exactly this
        /// case. It adds the value to what this sink answers to the LATENCY
        /// query, so the pipeline configures a latency that covers the
        /// device and every other sink delays its own rendering to match,
        /// and it posts a LATENCY message so the pipeline re-runs the query.
        ///
        /// Without it a Bluetooth route's 100-300ms is invisible: the video
        /// keeps the old timeline, and the base class's clock slaving is
        /// left to discover the offset on its own. It reads a constant
        /// offset as drift and corrects it in drift-tolerance-sized steps,
        /// resyncing the ring (so dropping audio) every time, and it never
        /// converges. Measured against a 200ms device: nine ~22ms
        /// corrections in eight seconds, versus none once the latency is
        /// declared here.
        ///
        /// A live source (AirPlay, WHEP) fares worse still, since the
        /// pipeline never buys the extra lead time the sink needs to run
        /// that far ahead of the device.
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
            // WARNING, not info: this posts a LATENCY message from whatever
            // thread called (the ring writer, every 32nd write), so it
            // redistributes the whole pipeline's latency mid-play. Field logs
            // capture WARNING and above, and a freeze correlating with a
            // latency recalculation has to be visible in them.
            gst::warning!(
                CAT,
                imp = self,
                "device latency {capped}, declaring it as render delay \
                 (posts LATENCY, redistributes the pipeline)"
            );
            self.obj().set_render_delay(capped);
        }

        /// How long the graph has gone without delivering a process()
        /// callback, updating the probe on the way. Reads zero the first
        /// time and whenever the count moves, so a `prepare()` (which zeroes
        /// `cycles`) always starts the clock over.
        ///
        /// A cycle that misses the bridge try_lock does not count, which
        /// cannot persist: a blocked `write()` holds the lock only between
        /// its 100ms waits.
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
        /// the sink is corked and the stream torn down.
        ///
        /// The base class stops waiting once the last sample's sync time
        /// passes, and that timeline ends at the hand-off point rather than
        /// at the device now that the fixed latency is declared as a render
        /// delay (GstBaseSink subtracts it back out of its own waits). So a
        /// ring's worth of real audio can still be sitting here. Bounded,
        /// and only on the EOS path: a Stop or a flush means silence now,
        /// and process() ramps that cut out instead.
        ///
        /// Waits for exactly what is queued right now to be consumed, NOT
        /// for the ring to empty: it never does, the writer thread keeps
        /// pushing silence segments behind the tail (see `Bridge::drained`).
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
                // Only what prepare() actually maps to spa formats. F32LE
                // first (pw native mixing format -> usually zero-convert).
                // Capped at stereo until a gst->spa channel-position map
                // lands in prepare(), fcastplaybin's audioconvert upstream
                // downmixes multichannel content to fit.
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

        // The point of the exercise: never provide a clock, so pipeline
        // election falls through to the monotonic system clock. The base
        // class still runs its internal GstAudioClock off ring position for
        // slaving math, default skew slaving stays as the safety net for
        // genuine device drift (pw rate-matching makes corrections rare,
        // and our clean delay() keeps them honest).
        fn provide_clock(&self) -> Option<gst::Clock> {
            None
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            // Soft-cork bookkeeping, set/cleared BEFORE chaining so the
            // parent's ring pause (which calls reset()) sees it.
            match transition {
                gst::StateChange::PlayingToPaused => {
                    self.drain_eos_tail();
                    self.shared.bridge.lock().paused = true;
                }
                gst::StateChange::PausedToPlaying => {
                    self.shared.bridge.lock().paused = false;
                }
                gst::StateChange::PausedToReady => {
                    // The parent's ring deactivation JOINS the writer
                    // thread with no reset() first, unblock any write()
                    // for good before that join (see Bridge::shutting_down).
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
            // Latched BEFORE chaining up: the parent's EOS handling drains
            // and posts the message without returning here in between, and
            // the teardown path needs to know an EOS is what ended the
            // stream (see drain_eos_tail).
            if let gst::EventView::Eos(_) = event.view() {
                self.shared.bridge.lock().eos = true;
            }
            // Real flushes discard the bridge here, reset() can't (the
            // pause path funnels there too, and a flush-while-paused may
            // skip reset() entirely because the ring is already paused).
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
            // Policy: a missing/broken PipeWire daemon is a LoggableError
            // here, the receiver probes availability up front (see
            // `is_available`) and picks autoaudiosink instead. Fallback
            // policy stays OUT of the element.
            pw::init(); // idempotent

            // SAFETY(new): pipewire-rs marks the Rc constructors unsafe
            // pending documented invariants, the C-side requirement is the
            // loop-lock discipline this module already follows (see PwConn).
            let thread_loop =
                unsafe { pw::thread_loop::ThreadLoopRc::new(Some("fcast-pw-sink"), None) }
                    .map_err(|e| gst::loggable_error!(CAT, "pw thread loop: {e}"))?;

            // Construct under the loop lock: the loop thread is live after
            // start() and libpipewire objects are not thread-safe. Error
            // paths are safe: `?` drops context/core under the guard, and
            // ThreadLoopRc's drop stops the loop itself.
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
                // ~2 segments of headroom: enough that process() never
                // starves between write() wakeups, small enough that the
                // base class's ring (segsize×segtotal) dominates latency.
                bridge.capacity = (spec.segsize() as usize).max(bytes_per_frame * 1024) * 2;
            }
            self.shared
                .bytes_per_frame
                .store(bytes_per_frame, std::sync::atomic::Ordering::Relaxed);
            // A fresh stream starts the cycle watchdog over, never against
            // the previous stream's count.
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
            // Positions matter: without them the stream is UNPOSITIONED and
            // PipeWire's channel mixer maps by index instead of layout,
            // mono lands on one speaker, and up/downmixing to the device
            // layout is skipped. pulsesink always sent a channel map, match
            // it for the layouts the template advertises.
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
                // Ask the graph for our segment size as the quantum, so one
                // process() drains ~one write(). The graph may clamp it,
                // which only shifts where the bridging happens.
                *pw::keys::NODE_LATENCY =>
                    format!("{}/{}", spec.segsize() as usize / bytes_per_frame, rate),
            };

            let stream = pw::stream::StreamRc::new(conn.core.clone(), "fcast-audio", props)
                .map_err(|e| gst::loggable_error!(CAT, "pw stream: {e}"))?;

            // The RT callback: memcpy-sized bridge-mutex sections ONLY, and
            // never a lock write() holds across anything blocking.
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
                    // First thing, unconditionally: this is the graph
                    // proving it still schedules us, and the cycle watchdog
                    // reads it. A cycle that finds no buffer or loses the
                    // try_lock below still ran.
                    shared
                        .cycles
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(mut pwbuf) = stream.dequeue_buffer() else {
                        return;
                    };
                    // The graph's suggested cycle size, in frames (0 =
                    // unknown): fill that rather than the whole mapped
                    // buffer for tighter latency under a large negotiated
                    // buffer.
                    let requested = pwbuf.requested() as usize;
                    let datas = pwbuf.datas_mut();
                    let Some(data) = datas.first_mut() else { return };
                    let Some(slice) = data.data() else { return };

                    // RT discipline: NEVER block on the bridge mutex, it is
                    // not priority-inheriting and write() (normal priority)
                    // holds it, blocking here under CPU pressure stalls the
                    // whole graph cycle (priority inversion). Contention is
                    // rare (both sides hold it for a memcpy), on a miss,
                    // emit one cycle of silence and let the next catch up.
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
                        // The ring only ever holds whole frames (write()
                        // appends whole segments), so `have` stays aligned.
                        // Soft-cork: while paused, hold the ring intact and
                        // emit silence, the kept fill keeps delay() (and
                        // with it the audio clock) steady across the pause.
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
                            // Resuming after a silent stretch: ramp the gain back in (a
                            // mid-waveform onset pops).
                            if bridge.resume_fade {
                                let fade = (have / bytes_per_frame).min(FADE_FRAMES);
                                apply_gain_ramp(slice, 0, fade, channels, is_f32, 0.0, 1.0);
                                bridge.resume_fade = false;
                            }
                            bridge.last_frame =
                                read_frame(slice, have - bytes_per_frame, channels, is_f32);
                        }
                        // Ring dry -> SILENCE and keep the graph fed (mirrors pulse prebuf=0: time
                        // keeps flowing, never stall the graph waiting for data), but never with a
                        // hard cut: fade real data's tail, or decay the held last frame (underrun,
                        // flush and EOS boundaries all pop otherwise).
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

                    // Only on progress: a progress-free notify (e.g. the paused silence path) would
                    // reset write()'s stall timer forever.
                    if drained {
                        shared.space.notify_all(); // write() may proceed
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

            // BOUNDED wait for the graph to actually run CYCLES on our stream. Running time starts
            // at PLAYING no matter what, so every ms the stream isn't consuming by then becomes
            // instant negative skew that the slaving grinds out as audible
            // skips. StreamState::Streaming is NOT the signal (a node goes "streaming" long before
            // a suspended device runs its first cycle), only process() callbacks prove
            // consumption. Waiting here delays preroll completion, silent and correct. A graph that
            // never cycles just loses the head start and falls through to write()'s stall/error
            // handling.
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
                // The loop thread is still dispatching (it just delivered the
                // error that latched `dead`), so the stream and listener must
                // die under the loop lock like every other pw object (see the
                // PwConn Send contract). A bare `return Err` here would drop
                // them unlocked.
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

            // Declare the route's latency before the sink prerolls: the
            // pipeline runs its LATENCY query once preroll completes, and
            // this is the only window where the answer can be right from the
            // first frame instead of being corrected afterwards.
            self.sync_render_delay();
            Ok(())
        }

        fn write(&self, data: &[u8]) -> Result<i32, gst::LoggableError> {
            // Cheap re-check for a route that changed under us (a Bluetooth
            // device connecting, a codec or profile switch, a default-sink
            // move): the declared latency has to follow it. Off the hot path
            // by a counter, and BEFORE the bridge lock, the stream mutex is
            // always taken first (see delay()).
            let writes = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if writes.is_multiple_of(32) {
                self.sync_render_delay();
            }

            let mut bridge = self.shared.bridge.lock();
            // Never true in practice (capacity >= 2 segments, writes are <= 1
            // segment), but a too-small ring must not become a livelock.
            if bridge.capacity < data.len() {
                bridge.capacity = data.len() * 2;
            }
            // Progress = the ring actually shrinking. Judging by wakeups let
            // a drip of progress-free notifies reset the stall clock forever.
            let mut last_progress = std::time::Instant::now();
            let mut last_fill = bridge.ring.len();
            // A trickle of REAL progress resets that clock just as well, so
            // bound the total time this one call may stay blocked too.
            let mut blocked_since = last_progress;
            let mut was_corked = bridge.paused;
            let limits = stall_limits();
            loop {
                if bridge.dead {
                    return Err(gst::loggable_error!(CAT, "pw stream is dead"));
                }
                if bridge.flushing || bridge.shutting_down {
                    // Swallow the data, the base class handles flush and teardown
                    // semantics. Returning the full length keeps it moving (and lets the ring
                    // deactivation join us).
                    return Ok(data.len() as i32);
                }
                if bridge.ring.len() + data.len() <= bridge.capacity {
                    bridge.ring.extend(data);
                    return Ok(data.len() as i32);
                }
                if bridge.ring.len() < last_fill {
                    last_fill = bridge.ring.len();
                    last_progress = std::time::Instant::now();
                    // The graph is consuming again: a later stall is news.
                    bridge.stalled = false;
                }
                if bridge.paused {
                    was_corked = true;
                } else if was_corked {
                    // Leaving a cork (a resume, or the stale-cork reconcile
                    // below): the cork WAS the reason nothing drained, so
                    // every stall clock restarts here. Without this, any
                    // pause longer than WRITE_STALL_LIMIT ends as a stall
                    // error the moment the user resumes.
                    was_corked = false;
                    last_fill = bridge.ring.len();
                    last_progress = std::time::Instant::now();
                    blocked_since = last_progress;
                }
                let blocked = last_progress.elapsed();
                let (_, current, pending) = self.obj().state(Some(gst::ClockTime::ZERO));
                if bridge.paused {
                    // A settled pause legitimately never drains: block for as
                    // long as the user pauses (resume/flush/teardown/death all
                    // notify). Everything else may not park here:
                    if pending != gst::State::VoidPending {
                        // A transition is waiting on this write to RETURN
                        // (the parent parks its ring thread only after it, so
                        // blocking wedged PLAYING->PAUSED for as long as the
                        // cork held, measured pending=paused 5s+). Swallow
                        // like the flush path: one segment lost at worst, the
                        // resume ramp covers the seam.
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
                        // uncork; nothing else ever clears it. Field:
                        // `forzen-at-the-start.txt`.
                        gst::warning!(
                            CAT,
                            imp = self,
                            "soft-cork held while the element is settled PLAYING; clearing \
                             the stale cork (ring {}/{} bytes)",
                            bridge.ring.len(),
                            bridge.capacity
                        );
                        bridge.paused = false;
                        // The next iteration restarts the stall clocks (see
                        // the corked-edge reset above), or this recovery
                        // would turn straight into a stall error.
                        continue;
                    }
                } else {
                    // Un-corked: the graph owes us space at the device rate,
                    // so every way of not getting it is bounded. Never park
                    // unbounded, that is the disease this element treats.
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
                            // Report outside the bridge lock: posting can reach
                            // a sync bus handler, and the error path must never
                            // invite one in here.
                            drop(bridge);
                            gst::error!(CAT, imp = self, "{detail}");
                            gst::element_imp_error!(
                                self,
                                gst::ResourceError::Failed,
                                ("PipeWire playback stalled: {}", detail)
                            );
                            bridge = self.shared.bridge.lock();
                        }
                        // Reported once. Keep waiting at the same 100ms
                        // cadence rather than failing the segment: gstaudiosink
                        // skips a refused one and calls straight back, which
                        // against an unresponsive graph is a busy loop posting
                        // per segment. Teardown, flush and death all still
                        // break the loop at the top, and a graph that recovers
                        // resumes playback.
                    }
                }
                let _ = self.shared.space.wait_for(&mut bridge, WRITE_STALL_STEP);
            }
        }

        fn delay(&self) -> u32 {
            // Frames handed over but not yet taken by the graph: the bridge ring plus what pw
            // still holds in its resampler (`buffered`, from pw_stream_get_time_n, an RT- and
            // thread-safe seqlock read and the one pw call made without the loop lock). Both
            // drain as the stream plays out, so EOS drain waits terminate.
            //
            // Deliberately NOT the fixed graph->device latency (`pw_time.delay`, which on a
            // Bluetooth route is the 100-300ms the headset itself adds). That part never drains,
            // and the base class subtracts whatever this returns from its audio clock to get
            // "what is audible now". Folding a constant in there parks that clock permanently
            // behind the pipeline clock, which the skew slaving reads as drift and keeps
            // resyncing away (see `sync_render_delay` for the measured cost). The fixed part is
            // declared as the sink's render delay instead, which delays the rest of the pipeline
            // to meet the device rather than dragging the audio forward to meet the video.
            let mut pw_frames: u64 = 0;
            let mut trace: Option<String> = None;
            if let Some(s) = self.stream.lock().as_ref() {
                if let Ok(t) = s.stream.time() {
                    let rate = t.rate();
                    pw_frames += t.buffered();
                    if no_device_latency() && rate.num > 0 && rate.denom > 0 {
                        // Hatch: fold the device latency back in and leave the
                        // compensation to the slaving, the pre-split behaviour.
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
            // Called for flushes AND on pause (GstAudioSink funnels both into reset() to unblock a
            // pending write()). Only a real flush discards data, the pause path soft-corks (see
            // Bridge::paused) so delay() holds steady. Must unblock write() immediately either way.
            let clear = {
                let mut bridge = self.shared.bridge.lock();
                bridge.flushing = true;
                let clear = !bridge.paused;
                if clear {
                    // last_frame stays: the next process() cycle ramps the discarded waveform down
                    // instead of hard-cutting.
                    bridge.ring.clear();
                }
                clear
            };
            self.shared.space.notify_all();
            // Lock order: NEVER take the thread-loop lock while holding the bridge mutex (process()
            // takes the bridge under the loop thread).
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
            // No drain here: the EOS tail is flushed out on the PLAYING->PAUSED edge (see
            // change_state), the only point where the graph is still consuming, and the
            // device-side latency lives in the daemon, which keeps playing what it already has
            // after our stream disconnects.
            // conn before stream, the order reset() and FlushStop use (an
            // inverted pair here is an ABBA deadlock waiting for a caller
            // change to make the paths overlap).
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
            // Drop listener + core + context under the loop lock,
            // thread_loop stop LAST (callbacks must be dead first).
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

        // Pause note: AudioSinkImpl has no pause hook, GstAudioSink calls reset() on pause (bridge
        // drops <=2 segments, alsasink-style) and stops calling write(). The pw stream keeps running
        // and silence- fills, which keeps the graph and delay() honest for resume.
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
            // The pre-existing arm, unchanged: un-corked, the graph owes us
            // space whatever the element is doing.
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
            // It posts an element error now, unless levered back to the old
            // (invisible) skipped-segment handling.
            assert!(StallVerdict::NoProgress.escalates(false));
            assert!(!StallVerdict::NoProgress.escalates(true));
        }

        #[test]
        fn a_trickle_that_defeats_the_progress_clock_trips_the_total_cap() {
            // Round 6's shape: space freed often enough to reset the
            // per-progress clock, real time frozen anyway.
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
            // A settled pause blocks for as long as the user pauses, and the
            // paused arms (hand-back, stale-cork reconcile) own that path.
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
            // prepare()'s own startup wait stays the only judge before
            // PLAYING, and a transition in flight or a teardown descent has
            // every right to stop the graph from draining.
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
            // Old behaviour: only the per-progress clock, which a trickle
            // resets forever.
            assert_eq!(write_stall_verdict(&stalled, &neither), StallVerdict::Wait);
        }

        /// The eight silent stall modes enumerated in FREEZE-DIAGN.md
        /// section 4, and which bail-out (if any) sees each one. The three
        /// that read as `Wait` are uncovered BY DESIGN: from inside the sink
        /// they are indistinguishable from a legitimate pause or from
        /// healthy playback, and only the receiver-level freeze watchdog can
        /// see them.
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
                    // Cycles are counted before the dequeue, so the graph
                    // still reads as running: the ring not moving is what
                    // catches this one.
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
                    // The crawl is the OLD handling of this verdict, gone
                    // now that the verdict escalates and parks.
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
            // 8. the sink's own pw thread-loop wedges: no callbacks reach us,
            // which is mode 1 from the writer's side.
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

        /// `prepare()`'s ring sizing, so the cap's justifying arithmetic is
        /// checked instead of just asserted in a comment.
        fn ring_capacity(segsize: usize, bytes_per_frame: usize) -> usize {
            segsize.max(bytes_per_frame * 1024) * 2
        }

        #[test]
        fn a_healthy_graph_never_blocks_one_write_for_seconds() {
            // The field's `ring 16224/16384` is this formula at 48kHz stereo
            // F32 with the default 10ms segment: a FULL ring is 43ms of
            // audio, so a full ring is the steady state, never evidence of a
            // stall.
            assert_eq!(ring_capacity(3840, 8), 16384);

            // A blocked write() needs ONE segment of room and the graph
            // consumes at the device rate, so the wait is one segment of
            // audio however big the ring is. (rate, bytes/frame, segsize):
            // 48k stereo F32 at 10ms and at 200ms buffer-time, 8k mono S16,
            // 44.1k stereo S16.
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

/// Cached probe: is there a reachable PipeWire daemon? Decides once per process whether the
/// receiver builds `fcastpwaudiosink` or falls back to autoaudiosink, probing up front beats
/// failing every load's open().
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
            // connect_rc actually dials the daemon socket, a real probe. Context/core drop under
            // the guard, per the loop-lock contract.
            pipewire::context::ContextRc::new(&thread_loop, None)
                .and_then(|ctx| ctx.connect_rc(None).map(|core| (ctx, core)))
                .is_ok()
        };
        thread_loop.stop();
        ok
    })
}

/// Rank NONE: the receiver selects it explicitly (fcastplaybin `AudioSink::Factory` on Linux),
/// autoplugging never should.
pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcastpwaudiosink",
        gst::Rank::NONE,
        PwAudioSink::static_type(),
    )
}
