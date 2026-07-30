//! `fcastpwaudiosink`: a native PipeWire audio sink.
//!
//! Failure discipline: this element must NEVER park a thread unboundedly,
//! that is the disease it exists to treat. A dead daemon/stream posts an
//! element error (core + stream listeners), `write()` gives up after a
//! bounded stall, and `reset()` aborts a blocked `write()` immediately.

use gst::{glib, prelude::*};

mod imp {
    use std::{collections::VecDeque, sync::Arc, time::Duration};

    use parking_lot::{Condvar, Mutex};

    use gst::{glib, subclass::prelude::*};
    use gst_audio::subclass::prelude::*;

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

    /// `FCAST_PW_DELAY_TRACE=1`: eprintln the delay()/process() internals
    /// (rate-limited), the A/V-sync debugging view. Cached: the process
    /// callback is RT. Kept as an eprintln hatch because the gst debug
    /// system isn't usable from the RT callback in this binary, and
    /// delay-reporting quirks are device/driver-dependent and recur.
    fn delay_trace() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("FCAST_PW_DELAY_TRACE").is_ok_and(|v| v == "1"))
    }

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
        /// Process cycles that found less data than they wanted. Counts
        /// idle/paused silence-fill cycles too, a coarse stat, logged at
        /// unprepare for quantum sanity-checking, not an error signal.
        underruns: u64,
        /// Total process() cycles, prepare() waits on this: cycles
        /// running is the real "the graph is consuming" signal
        /// (StreamState::Streaming only means the node is active, a
        /// suspended device delays the first cycle by its resume time).
        cycles: u64,
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
                underruns: 0,
                cycles: 0,
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
            // Real flushes discard the bridge here, reset() can't (the
            // pause path funnels there too, and a flush-while-paused may
            // skip reset() entirely because the ring is already paused).
            if let gst::EventView::FlushStop(_) = event.view() {
                {
                    let mut bridge = self.shared.bridge.lock();
                    bridge.ring.clear();
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
                bridge.dead = false;
                bridge.ring.clear();
                bridge.underruns = 0;
                bridge.cycles = 0;
                // ~2 segments of headroom: enough that process() never
                // starves between write() wakeups, small enough that the
                // base class's ring (segsize×segtotal) dominates latency.
                bridge.capacity = (spec.segsize() as usize).max(bytes_per_frame * 1024) * 2;
            }
            self.shared
                .bytes_per_frame
                .store(bytes_per_frame, std::sync::atomic::Ordering::Relaxed);

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
                        bridge.cycles += 1;
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
            loop {
                {
                    let bridge = self.shared.bridge.lock();
                    if bridge.dead {
                        return Err(gst::loggable_error!(CAT, "pw stream died in prepare()"));
                    }
                    if bridge.cycles >= 2 {
                        break;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    gst::warning!(CAT, "no pw graph cycles within 1.5s; starting anyway");
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }

            *self.stream.lock() = Some(PwStream {
                stream,
                _listener: listener,
                rate,
            });
            Ok(())
        }

        fn write(&self, data: &[u8]) -> Result<i32, gst::LoggableError> {
            let mut bridge = self.shared.bridge.lock();
            // Never true in practice (capacity >= 2 segments, writes are <= 1
            // segment), but a too-small ring must not become a livelock.
            if bridge.capacity < data.len() {
                bridge.capacity = data.len() * 2;
            }
            let mut stalled = Duration::ZERO;
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
                if stalled >= WRITE_STALL_LIMIT {
                    // The graph freed nothing for the whole window and no error listener fired:
                    // fail loud. Never park unbounded, that is the disease this element treats.
                    return Err(gst::loggable_error!(
                        CAT,
                        "pw graph consumed no audio for {stalled:?}; giving up"
                    ));
                }
                let timeout = self.shared.space.wait_for(&mut bridge, WRITE_STALL_STEP);
                // While soft-corked the ring legitimately never drains: a pause must block for as
                // long as the user pauses, interruptible by resume/flush/teardown/death (all of
                // which notify). Stall accounting only runs unpaused, where process() drains a full
                // ring every cycle and only notifies on progress.
                if timeout.timed_out() && !bridge.paused {
                    stalled += WRITE_STALL_STEP;
                } else if !timeout.timed_out() {
                    stalled = Duration::ZERO;
                }
            }
        }

        fn delay(&self) -> u32 {
            // Frames not yet audible = bridge ring + pw-side queue. The pw term comes from
            // pw_stream_get_time_n (RT- and thread-safe seqlock read, the one pw call made without
            // the loop lock): `delay` = graph->device latency in rate ticks, `buffered` = frames
            // sitting in pw's resampler. Both decay/stop naturally as the stream plays out, so EOS
            // drain waits terminate.
            let mut pw_frames: u64 = 0;
            let mut trace: Option<String> = None;
            if let Some(s) = self.stream.lock().as_ref() {
                if let Ok(t) = s.stream.time() {
                    let rate = t.rate();
                    if rate.num > 0 && rate.denom > 0 {
                        pw_frames = t.delay().max(0) as u64 * rate.num as u64 * s.rate as u64
                            / rate.denom as u64;
                    }
                    pw_frames += t.buffered();
                    if delay_trace() {
                        trace = Some(format!(
                            "delay={} rate={}/{} buffered={} queued={} queued_bufs={} pw_frames={}",
                            t.delay(),
                            rate.num,
                            rate.denom,
                            t.buffered(),
                            t.queued(),
                            t.queued_buffers(),
                            pw_frames,
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
            // No drain here: audiobasesink waits out the EOS time (last sample + delay()) on the
            // pipeline clock BEFORE unprepare.
            if let Some(s) = self.stream.lock().take() {
                let conn = self.conn.lock();
                if let Some(conn) = conn.as_ref() {
                    let _guard = conn.thread_loop.lock();
                    let _ = s.stream.disconnect();
                    drop(s); // stream + listener die under the loop lock
                }
            }
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
