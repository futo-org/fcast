//! Sink-side subtitle cue state: which cue is on screen right now
//! ([`CueEngine`]) and what it looks like.
//!
//! What a cue LOOKS like is [`i_slint_cue::engine`]'s: the cache, the worker
//! and the per-frame probe live beside the layout they drive, so an
//! application that draws subtitles gets them without writing any of it. What
//! is here is the half that is a video sink's: the running-time schedule, the
//! bitmap subtitle decoder, and the stacking that turns a set of cues into a
//! set of overlays.
//!
//! The engine is fed running-time-scheduled cues from outside the sink and is
//! evaluated per displayed frame, so a cue's visibility is a pure function of
//! the frame's running time and the cue's window. No pipeline clock, no
//! waiting, and no dependency on a new video buffer to change what is shown,
//! which is what makes a paused subtitle switch possible.
//!
//! Timing semantics are the retired `fcasttextoverlay` element's
//! `wait_for_text_buf`, as the pure functions [`cue_is_too_old`] and
//! [`cue_is_in_future`]. Its blocking handoff is deliberately not lifted.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};
use smallvec::SmallVec;
use tracing::{debug, warn};

use flapjack::cue::{MAX_ACTIVE_CUES, Schedule, opening_rank};
use i_slint_cue::engine::{CueRaster, Painted, RasterOptions, SceneSlot};
/// One cue on screen, as a display list plus where it ended up: what a SCENE
/// consumer gets instead of an [`Overlay`]. The dodvg lanes draw the scene
/// straight into their frame, so nothing in it has been painted.
pub use i_slint_cue::engine::Shown as ShownScene;

/// The scheduling vocabulary, which is the driver's. Named here too so a
/// consumer of this crate reaches one set of types.
pub use flapjack::cue::{
    BITMAP_PENDING_PIXEL_BUDGET, CueInput, PENDING_LIMIT, SubtitleTextFormat, cue_is_in_future,
    cue_is_too_old,
};
/// What this crate called the format before the schedule moved.
pub use flapjack::cue::SubtitleTextFormat as TextFormat;

use crate::{
    cue_ir::{CueStyle, VideoRect},
    subpic::{BitmapSubFormat, BitmapPacket, DisplayUpdate, SubpicDecoder},
    video::{Overlay, OverlaySpace},
};

/// Undecoded bitmap packets allowed to wait for the decode worker. A burst
/// backstop, not a working bound: a bitmap stream is demuxer-paced, so a
/// healthy pipeline keeps one or two packets here.
///
/// At the limit the queue is drained whole and the epoch bumps (see
/// [`CueEngine::submit_bitmap`]), because these packets feed a stateful
/// decoder. Dropping one and keeping the rest hands the decoder a stream with
/// a hole in it, and the resulting corruption is silent and permanent. A reset
/// is loud, counted, and recovers at the next complete set.
const BITMAP_QUEUE_LIMIT: usize = 64;

/// How many decode costs are kept for [`CueEngine::bitmap_decode_latencies`].
const BITMAP_LATENCY_WINDOW: usize = 256;

/// How long the decode worker waits with nothing to do before it retires.
///
/// Both engine workers are lazily spawned and lazily unspawned, so an idle
/// sink does not keep a thread parked for the process lifetime. The cost of
/// retiring too eagerly is one thread spawn off the streaming thread; the
/// timeout is long enough that a normal subtitle cadence never retires the
/// worker mid-track. The raster worker's twin is
/// [`i_slint_cue::engine::IDLE_TIMEOUT`], and it is the same twenty seconds.
///
/// Fixed, unlike that twin, which a test shortens through
/// [`RasterOptions::idle_timeout`]. The decode retirement test drives its
/// worker through [`CueEngine::hold_decode_for_test`] instead.
const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Inline capacity of the overlay set a raster consumer reads per frame.
///
/// Text cues alone are bounded by [`MAX_ACTIVE_CUES`], but a bitmap set adds a
/// region per subpicture on top, so this cannot be made spill-proof the way the
/// scene set is. Four covers a text cue beside a two-region display set, which
/// is what real sources carry; past it the frame pays one allocation, which is
/// what it used to pay at two.
const MAX_OVERLAYS: usize = 4;

#[derive(Default)]
struct State {
    /// Which cues and display sets are on screen, as a function of the running
    /// time of the frame being shown. The driver's, because the clock is.
    ///
    /// Its payload is the raster engine's, and nothing here ever reads one:
    /// everything about WHEN a cue is on screen is the schedule's, everything
    /// about what it looks like is [`CueRaster`]'s.
    sched: Schedule<SceneSlot>,
    /// Coded video size decoders pre-scale their regions to (see
    /// [`CueEngine::set_video_size`]). `(0, 0)` until the sink negotiates caps.
    video_size: (u32, u32),
}

/// The slot a cue activates with: nothing built, and nothing keyed until the
/// next [`CueRaster::resolve`] keys it.
fn slot(_: &CueInput) -> SceneSlot {
    SceneSlot::default()
}

/// A markup cue becomes its IR before it is scheduled.
///
/// The raster engine reads the IR and nothing else, so the one place that
/// knows what a transport calls its format has to do the conversion, and this
/// is it. Parsing here also means a markup cue is parsed once rather than once
/// per layout.
///
/// It is reachable: matroskademux emits `format=pango-markup` directly for
/// S_TEXT/UTF8 tracks, so cues arrive that never passed through a parser
/// element. The parser is tolerant by design and never rejects a cue, so
/// broken markup degrades to its words instead of reaching the screen as tags.
fn parsed(mut cue: CueInput) -> CueInput {
    if matches!(cue.format, SubtitleTextFormat::PangoMarkup) {
        let ir = gstrssubparse::pango_markup::markup_to_cue_ir(&cue.text);
        cue.text = ir.plain_text();
        cue.format = SubtitleTextFormat::CueIr { ir: Arc::new(ir), pts_start: None };
    }
    cue
}

/// Whether the engine shows one cue at a time.
///
/// Lever: `FCAST_SINGLE_ACTIVE_CUE=1` (set = on). It restores the retired
/// `fcasttextoverlay` behaviour of holding exactly one text buffer: a cue
/// whose turn comes replaces whatever is showing. Overlapping cues then show
/// one at a time, but existing pixel and timing expectations were written
/// against this, so it stays reachable.
///
/// Read once, on first use. The schedule keeps per-cue state whose shape
/// depends on the answer, and a lever changed under a running pipeline would
/// leave that state describing a policy no longer in force.
fn single_active_cues() -> bool {
    static SINGLE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("FCAST_SINGLE_ACTIVE_CUE").is_some_and(|value| value == "1")
    });
    *SINGLE
}

/// The paused gap tolerance in force: the driver's default, or none at all.
///
/// Lever: `FCAST_NO_PAUSED_CUE_LOOKAHEAD` (set = off). With it set the paused
/// schedule is exact again and a frame frozen in a gap stays blank. Read once,
/// like every other lever here.
fn paused_cue_lookahead() -> gst::ClockTime {
    static OFF: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("FCAST_NO_PAUSED_CUE_LOOKAHEAD").is_some());
    if *OFF {
        gst::ClockTime::ZERO
    } else {
        flapjack::cue::PAUSED_CUE_LOOKAHEAD
    }
}

type OnChange = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    state: Mutex<State>,
    /// What every cue on screen looks like: the layout, the caches, the
    /// worker. Told what is on screen once per frame and never told when.
    raster: CueRaster,
    on_change: Mutex<Option<OnChange>>,
    dirty: AtomicBool,

    // ---- the bitmap side ----
    /// The `fvid-sub-decode` worker, spawned on the first bitmap packet.
    decode_worker: Mutex<Option<DecodeHandle>>,
    /// Times the packet inbox overflowed and reset the decoder. Pathological:
    /// the phase gate asserts this stays 0 across the whole battery.
    bitmap_overflow_resets: AtomicU64,
    /// Packets the decoder refused (a panic caught at the worker, or a format
    /// with no decoder).
    bitmap_decode_errors: AtomicU64,
    /// Display sets the decoder produced.
    bitmap_sets_decoded: AtomicU64,
    /// What each `push` cost, newest last, bounded. The `raster_latencies`
    /// twin.
    bitmap_decode_latencies: Mutex<VecDeque<Duration>>,
    /// The decoder factory the worker builds from. Tests install their own to
    /// drive the engine without a real format decoder; production leaves it
    /// `None` and [`build_decoder`] falls through to
    /// [`crate::subpic::decoder_for`].
    ///
    /// Not `cfg(test)`: integration tests in `tests/` link the library as an
    /// ordinary dependent and cannot see anything gated on the crate's own
    /// test cfg.
    decoder_factory: Mutex<Option<Arc<DecoderFactory>>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(handle) = self.decode_worker.lock().take() {
            handle.stop();
        }
    }
}

/// Called when the overlay set changes without a frame flowing: a raster
/// landing, an activation or expiry, a clear. Runs on whichever thread noticed
/// and never with an engine lock held.
fn mark_changed(shared: &Shared) {
    shared.dirty.store(true, Ordering::Release);
    let callback = shared.on_change.lock().clone();
    if let Some(callback) = callback {
        callback();
    }
}

type DecoderFactory = dyn Fn(BitmapSubFormat) -> Option<Box<dyn SubpicDecoder>> + Send + Sync;

/// Sink-side cue scheduler. Cheap to clone (an `Arc` handle); every method is
/// non-blocking.
#[derive(Clone)]
pub struct CueEngine {
    shared: Arc<Shared>,
}

impl Default for CueEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CueEngine {
    /// An engine for a consumer that takes [`Overlay`]s: the raster engine
    /// paints every cue, and [`CueEngine::overlays_for`] hands the pixels over.
    pub fn new() -> Self {
        Self::build(RasterOptions::default())
    }

    /// An engine for a consumer that draws the display lists itself, which
    /// switches the paint lane off.
    ///
    /// Fixed here rather than settable, because the pixel cache remembers a
    /// failed paint as a tombstone and a lane switched mid-stream has
    /// tombstones in it that were never a refusal. No consumer switches.
    ///
    /// [`CueEngine::overlays_for`] and [`CueEngine::current_overlays`] stay
    /// callable and stay correct: they answer with the bitmap subtitle set and
    /// nothing else, because [`active_overlays`] refuses to composite a text
    /// cue whose pixels the consumer is drawing itself. A subpicture has no
    /// display list and is the one thing a scene consumer cannot draw, so it
    /// stays on this path on every lane.
    pub fn for_scene_consumer() -> Self {
        Self::build(RasterOptions { scenes_only: true, ..RasterOptions::default() })
    }

    /// An engine whose raster worker retires after `idle` rather than after
    /// [`WORKER_IDLE_TIMEOUT`].
    ///
    /// Tests only. A test that wants to watch a retirement cannot wait twenty
    /// seconds for one.
    #[doc(hidden)]
    pub fn with_worker_idle_for_test(idle: Duration) -> Self {
        Self::build(RasterOptions { idle_timeout: idle, ..RasterOptions::default() })
    }

    fn build(options: RasterOptions) -> Self {
        // Cyclic because the raster engine's change hook has to reach back
        // here: it means "ask again", and only this side knows what is on
        // screen to ask about. Weak, so the hook is not what keeps the engine
        // alive.
        let shared = Arc::new_cyclic(|back: &Weak<Shared>| {
            let raster = CueRaster::with_options(options);
            let back = back.clone();
            raster.on_change(move || {
                if let Some(shared) = back.upgrade() {
                    mark_changed(&shared);
                }
            });
            Shared {
                state: Mutex::default(),
                raster,
                on_change: Mutex::default(),
                dirty: AtomicBool::new(false),
                decode_worker: Mutex::default(),
                bitmap_overflow_resets: AtomicU64::new(0),
                bitmap_decode_errors: AtomicU64::new(0),
                bitmap_sets_decoded: AtomicU64::new(0),
                bitmap_decode_latencies: Mutex::default(),
                decoder_factory: Mutex::default(),
            }
        });
        {
            let mut state = shared.state.lock();
            state.sched.set_single_active(single_active_cues());
            state.sched.set_paused_lookahead(paused_cue_lookahead());
        }
        Self { shared }
    }

    /// One pass of the raster engine over what is on screen: what each active
    /// cue wants built, and the next cue's work before its turn comes.
    ///
    /// Everything the engine needs is here and nothing it does not: the cue's
    /// words, its IR when it has one, and the rank its reveal has reached.
    /// Where the cue came from and when it is due stay on this side.
    fn drive(&self, state: &mut State) -> bool {
        let rate = state.sched.rate();
        let (active, next) = state.sched.active_and_next_mut();
        let mut cues: SmallVec<[i_slint_cue::engine::Cue<'_>; MAX_ACTIVE_CUES]> = active
            .iter_mut()
            .map(|active| i_slint_cue::engine::Cue {
                text: &active.cue.text,
                ir: flapjack::cue::cue_ir(&active.cue.format),
                rank: active.rank,
                slot: &mut active.payload,
            })
            .collect();
        let next = next.map(|cue| i_slint_cue::engine::Next {
            text: &cue.text,
            ir: flapjack::cue::cue_ir(&cue.format),
            rank: opening_rank(cue, rate),
        });
        self.shared
            .raster
            .resolve(i_slint_cue::engine::Frame { active: &mut cues, next })
    }

    /// Schedule a cue. Called from the text delivery thread; never blocks and
    /// never rasterizes inline.
    pub fn submit(&self, cue: CueInput) {
        // Before the lock: a markup cue is parsed here, and no lock is worth
        // holding across a parse.
        let cue = parsed(cue);
        let mut changed;
        {
            let mut state = self.shared.state.lock();

            // Ordered by start time (delivery is normally in order; a re-send
            // after a seek may not be). `partition_point` because a whole-file
            // burst is mostly sorted and the insert point must not cost a walk
            // over thousands of queued cues.
            state.sched.submit(cue);

            // A cue that covers the frame already on screen becomes visible
            // without a new frame. This is the paused path, so it evaluates
            // with the gap tolerance.
            changed = match state.sched.last_shown_rt() {
                Some(rt) => state.sched.advance_paused(rt, slot),
                None => false,
            };
            changed |= self.drive(&mut state);
        }

        if changed {
            mark_changed(&self.shared);
        }
    }

    /// Hand one bitmap subtitle packet to the decoder. Called from the text
    /// delivery thread, exactly like [`CueEngine::submit`]: it never blocks,
    /// never maps the buffer and never decodes inline.
    ///
    /// Two things happen here and nothing else. First the
    /// consecutive-duplicate check: the transport's appsink can hand the same
    /// buffer object over twice in a row (preroll then render). The text path
    /// absorbs that in its latest-wins scheduling; a stateful reassembler
    /// cannot, because the second copy of a fragment corrupts the object being
    /// assembled. Buffer identity is the test, so a genuinely re-delivered
    /// packet after a seek is a different object and passes. See
    /// [`same_buffer`].
    ///
    /// Then the enqueue, with the overflow policy [`BITMAP_QUEUE_LIMIT`]
    /// describes: drain whole, bump the epoch, count, and admit the new packet
    /// on the far side of the reset.
    pub fn submit_bitmap(&self, packet: BitmapPacket) {
        // In an `Option` so the retry loop gives the packet up exactly once,
        // on the iteration that finds a live inbox.
        let mut packet = Some(packet);
        loop {
            // Before the state lock: this may spawn the worker thread, and no
            // engine lock may be held across a spawn.
            let inbox = self.decode_inbox();

            // State lock outside, inbox lock inside, the one order this pair
            // is ever taken in. The worker takes them apart: inbox lock only
            // to pop, state lock only to publish.
            let mut state = self.shared.state.lock();
            let mut slot = inbox.slot.lock();
            // The retirement check comes first, before a single field is
            // written. The dedupe remembers this buffer, so checking after the
            // duplicate check would make the retry see its own packet as the
            // previous one and drop it.
            if slot.retired {
                continue;
            }
            if packet
                .as_ref()
                .is_some_and(|p| state.sched.is_repeat_bitmap(&p.data))
            {
                debug!(
                    rt = ?packet.as_ref().map(|p| p.rt),
                    "dropping a repeat of the packet just submitted (preroll then render)"
                );
                return;
            }
            let Some(packet) = packet.take() else { return };
            state.sched.note_bitmap(packet.data.clone());

            if slot.queue.len() >= BITMAP_QUEUE_LIMIT {
                let dropped = slot.queue.len();
                slot.queue.clear();
                let epoch = state.sched.bump_bitmap_epoch();
                let total = self
                    .shared
                    .bitmap_overflow_resets
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                warn!(
                    dropped,
                    epoch,
                    total,
                    "bitmap decode inbox full; reset the decoder rather than decode a stream with a \
                     hole in it -- subtitles resume at the next complete display set"
                );
            }
            slot.queue.push_back((state.sched.bitmap_epoch(), packet));
            inbox.cv.notify_all();
            return;
        }
    }

    /// Drop everything scheduled and everything showing. The raster cache is
    /// deliberately kept: a clear is usually a prelude to re-delivery of the
    /// same cues (a flushing seek, a track restart).
    ///
    /// Both sides go: this is the track-switch primitive, and a switch away
    /// from a bitmap track must not leave its last page painted on the frame.
    /// The epoch bump makes that true for work already in flight. Packets in
    /// the decode inbox still get decoded, but their sets are dropped at
    /// publish instead of appearing after the switch.
    pub fn clear(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            state.sched.clear()
        };
        if changed {
            mark_changed(&self.shared);
        }
    }

    /// Set the display size cues are laid out against, from the sink's
    /// `window-resolution` property.
    ///
    /// A zero dimension is ignored. A window mid-create or mid-minimize
    /// reports 0x0 and there is nothing to lay out against.
    pub fn set_canvas(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            debug!(width, height, "ignoring zero canvas size");
            return;
        }

        if self.shared.raster.canvas() == (width, height) {
            return;
        }
        // Every active cue's scene was laid out against the old size, and the
        // next `drive` re-keys them. Keeping the old scene up meanwhile is
        // what stops a resize blanking every line on screen for as long as
        // the worker takes; the old-canvas scenes stay cached, so a canvas
        // that returns to a previous size is instant.
        self.shared.raster.set_canvas((width, height));
        if self.drive(&mut self.shared.state.lock()) {
            mark_changed(&self.shared);
        }
    }

    /// Set the rectangle the video occupies inside the window, in window
    /// coordinates (`None` = unknown; the whole window then doubles as the
    /// picture). Update it
    /// wherever the sink recomputes its scaled destination rect (resize,
    /// rotation, aspect change).
    ///
    /// Only the cue-IR arm can use it. It anchors positioned cues to the
    /// picture rather than the window, and sizes their text against the
    /// picture height. Default-placed subtitles may still use the window bars
    /// (see [`CueStyle::use_window_margins`]).
    pub fn set_video_rect(&self, rect: Option<VideoRect>) {
        if let Some(r) = rect
            && (r.width == 0 || r.height == 0)
        {
            debug!(?rect, "ignoring zero-sized video rect");
            return;
        }

        if self.shared.raster.video_rect() == rect {
            return;
        }
        // The active cues were laid out against the old picture rect, so their
        // placement is now slightly wrong. They re-key and keep showing
        // meanwhile, as in `set_canvas`: a resize while playing must not
        // strobe the line.
        self.shared.raster.set_video_rect(rect);
        if self.drive(&mut self.shared.state.lock()) {
            mark_changed(&self.shared);
        }
    }

    /// Set the CODED size of the video, from the sink's caps.
    ///
    /// Three geometries live in this engine: [`CueEngine::set_canvas`] is the
    /// window size text cues are laid out against;
    /// [`CueEngine::set_video_rect`] is where the picture sits inside that
    /// window, anchoring positioned cue-IR cues; this is the picture's own
    /// pixel grid, which bitmap subtitle decoders scale their regions to,
    /// because a bitmap region is composited in source-frame space.
    ///
    /// A zero dimension is ignored. A size change mid-set does not re-scale
    /// the set already showing; the next set picks the new size up. A
    /// coded-size change mid-stream is a renegotiation, not a resize, and a
    /// window resize does not reach here at all.
    pub fn set_video_size(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            debug!(width, height, "ignoring zero video size");
            return;
        }
        let mut state = self.shared.state.lock();
        if state.video_size != (width, height) {
            debug!(width, height, "coded video size for bitmap subtitles");
            state.video_size = (width, height);
        }
    }

    /// Change the house style (see [`CueStyle`]); the active cue re-rasters.
    /// Callable at any time from any thread, including while paused.
    pub fn set_style(&self, style: CueStyle) {
        // The active cues were drawn with the old style; they keep showing
        // until the re-styled ones land, rather than blinking blank on a
        // settings toggle. Old-style scenes stay cached, so a style toggled
        // back is instant as well.
        self.shared.raster.set_style(style);
        if self.drive(&mut self.shared.state.lock()) {
            mark_changed(&self.shared);
        }
    }

    /// The house style in force (see [`CueStyle`]).
    pub fn style(&self) -> CueStyle {
        (*self.shared.raster.style()).clone()
    }

    /// Record the video segment the sink is running, so frame pts can be turned
    /// into the running time cues are scheduled in.
    pub fn set_video_segment(&self, segment: &gst::Segment) {
        self.shared.state.lock().sched.set_video_segment(segment);
    }

    /// FLUSH_STOP. Both sides of the comparison are invalid: cues from before
    /// the flush must not be shown after it, and the timeline anchor is gone.
    pub fn flush(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            state.sched.flush()
        };
        if changed {
            mark_changed(&self.shared);
        }
    }

    /// STREAM_START: a new stream's segment is about to arrive; forget the old
    /// timeline anchor. Scheduled cues are left alone. Dropping them is the
    /// producer's decision, since it knows whether the text stream restarted.
    ///
    /// Decoded bitmap sets with an end are left alone for the same reason, and
    /// the epoch still bumps: this is the video sink's STREAM_START, and a
    /// decoder's half-assembled display set belongs to whatever was playing
    /// before it. The text branch's own restart reaches the engine as a Clear
    /// from the transport probe, which is the call that does drop pending.
    ///
    /// **Open-ended sets are the exception, deliberately.** An open-ended set
    /// means "show this until something replaces it", and the thing that would
    /// have replaced it belonged to the item that just ended. Left alone, a
    /// page from the previous item would stay painted over the new one until
    /// its first subtitle arrived, possibly forever. A bounded set carries its
    /// own end and cannot outlive it, so it keeps the text side's survival
    /// rule.
    pub fn reset_timeline(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            let mut changed = state.sched.forget_timeline();

            let stranded = state
                .sched
                .retain_bitmap_pending(|update| update.end_rt.is_some());
            if stranded > 0 {
                debug!(
                    stranded,
                    "dropped open-ended bitmap sets at a new stream; they had no end of their own \
                     and nothing in the next item would have superseded them"
                );
            }
            changed |= state
                .sched
                .clear_bitmap_active_if(|active| active.end_rt.is_none());
            changed
        };
        if changed {
            mark_changed(&self.shared);
        }
    }

    /// Running time of a frame with this pts, under the captured video segment.
    pub fn video_running_time(&self, pts: Option<gst::ClockTime>) -> Option<gst::ClockTime> {
        let pts = pts?;
        let state = self.shared.state.lock();
        let segment = state.sched.video_segment()?;
        match segment.to_running_time(pts) {
            gst::GenericFormattedValue::Time(time) => time,
            _ => None,
        }
    }

    /// The overlays a frame at `frame_rt` should carry. Called per frame from
    /// the sink's streaming thread.
    ///
    /// `None` means the frame has no usable running time (no segment yet, no
    /// pts): the cue state is left exactly as it is and whatever is on screen
    /// stays there, since there is no information to schedule against.
    ///
    /// Exact, always. A frame arriving is evidence that the clock is running,
    /// so this path never takes the paused gap tolerance
    /// ([`PAUSED_CUE_LOOKAHEAD`]). A cue whose start is a few frames away
    /// arrives on its own, on time, and showing it early here would move every
    /// cue boundary in the file.
    pub fn overlays_for(
        &self,
        frame_rt: Option<gst::ClockTime>,
    ) -> SmallVec<[Overlay; MAX_OVERLAYS]> {
        let mut changed = false;
        let overlays;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = frame_rt {
                changed = state.sched.advance(rt, slot);
            }
            changed |= self.drive(&mut state);
            overlays = active_overlays(&state, self.shared.raster.draws_pixels());
        }

        if changed {
            mark_changed(&self.shared);
        }
        overlays
    }

    /// The overlays for the frame already on screen, re-evaluated against the
    /// frozen `last_shown_rt`. This is the paused path: read from the render
    /// thread, it produces the answer `overlays_for` would without needing a
    /// frame to flow.
    ///
    /// Not quite the same answer, deliberately. Being the paused path, this
    /// evaluates the text schedule with the gap tolerance
    /// [`PAUSED_CUE_LOOKAHEAD`] describes, so a playhead frozen in the hole
    /// between two cues shows the one it is about to reach rather than
    /// nothing. The bitmap schedule is read exactly, as it is everywhere.
    pub fn current_overlays(&self) -> SmallVec<[Overlay; MAX_OVERLAYS]> {
        let mut changed = false;
        let overlays;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = state.sched.last_shown_rt() {
                changed = state.sched.advance_paused(rt, slot);
            }
            changed |= self.drive(&mut state);
            overlays = active_overlays(&state, self.shared.raster.draws_pixels());
        }

        if changed {
            mark_changed(&self.shared);
        }
        overlays
    }

    /// [`Self::overlays_for`] for a consumer that draws the display lists
    /// itself: the same schedule advance, the same stacking, no pixels.
    ///
    /// Requires [`CueEngine::for_scene_consumer`], or the engine keeps
    /// painting cues nothing will ever read.
    pub fn scenes_for(
        &self,
        frame_rt: Option<gst::ClockTime>,
    ) -> SmallVec<[ShownScene; MAX_ACTIVE_CUES]> {
        let mut changed = false;
        let shown;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = frame_rt {
                changed = state.sched.advance(rt, slot);
            }
            changed |= self.drive(&mut state);
            shown = active_scenes(&state);
        }

        if changed {
            mark_changed(&self.shared);
        }
        shown
    }

    /// [`Self::current_overlays`] for a scene consumer: the paused path, with
    /// the same gap tolerance, as display lists.
    pub fn current_scenes(&self) -> SmallVec<[ShownScene; MAX_ACTIVE_CUES]> {
        let mut changed = false;
        let shown;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = state.sched.last_shown_rt() {
                changed = state.sched.advance_paused(rt, slot);
            }
            changed |= self.drive(&mut state);
            shown = active_scenes(&state);
        }

        if changed {
            mark_changed(&self.shared);
        }
        shown
    }

    /// The display lists on screen right now, evaluating nothing.
    ///
    /// For a consumer whose schedule is advanced elsewhere: one that still
    /// calls [`Self::overlays_for`] per frame on the streaming thread, because
    /// its bitmap subtitle set has no display list and stays on the raster
    /// path, with [`Self::current_overlays`] advancing it while paused.
    /// Reaching for [`Self::scenes_for`] from such a lane's repaint would
    /// evaluate the schedule a second time, at the repaint clock rather than
    /// the frame's, and move every cue boundary with it.
    pub fn shown_scenes(&self) -> SmallVec<[ShownScene; MAX_ACTIVE_CUES]> {
        // Evaluating nothing means no CLOCK is read: no cue starts or ends
        // here. Reconciling the cues already on screen against what has been
        // built is not scheduling, and it is the only place a display list
        // reaches the slot a consumer draws from.
        let changed;
        let shown;
        {
            let mut state = self.shared.state.lock();
            changed = self.drive(&mut state);
            shown = active_scenes(&state);
        }
        if changed {
            mark_changed(&self.shared);
        }
        shown
    }

    /// The bitmap subtitle set on screen right now, evaluating nothing.
    ///
    /// The subpicture twin of [`Self::shown_scenes`], and what a scene consumer
    /// reads instead of [`Self::current_overlays`]. A display set has no
    /// display list, so it can never come back on the scene lane; it is
    /// decoded pixels and the consumer has to composite them itself.
    ///
    /// By closure rather than by value because that is the whole point: a
    /// consumer that only wants to know whether the set moved gets its answer
    /// without building a `SmallVec` of [`Overlay`]s and cloning an `Arc` per
    /// region every frame. The closure runs under the engine's state lock, so
    /// it must stay short. Composite outside it.
    ///
    /// The slice is empty when nothing is showing, which is also how a cue end,
    /// a scheduled clear, [`Self::clear`] and [`Self::flush`] surface here.
    ///
    /// Bitmap sets bypass the raster engine entirely, so unlike
    /// [`Self::shown_scenes`] this one reads and nothing else.
    pub fn with_shown_bitmaps<R>(
        &self,
        read: impl FnOnce(&[crate::subpic::BitmapRegion]) -> R,
    ) -> R {
        let state = self.shared.state.lock();
        read(match state.sched.bitmap_active() {
            Some(update) => &update.regions,
            None => &[],
        })
    }

    /// Whether the overlay set changed since the last call, and clears the
    /// flag.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// Called when the overlay set changes without a frame flowing: raster
    /// completion, activation/expiry, clear. Invoked from the raster worker or
    /// from whichever thread submitted, never with an engine lock held.
    pub fn set_on_change(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.shared.on_change.lock() = Some(Arc::new(callback));
    }

    /// Cues dropped because the pending list was full.
    pub fn dropped_cues(&self) -> u64 {
        self.shared.state.lock().sched.dropped()
    }

    /// Cues currently laid out and held in the scene cache.
    pub fn cached_rasters(&self) -> usize {
        self.shared.raster.cached_scenes()
    }

    /// Paints held for the overlay lanes: one per (cue, reveal rank) shown.
    pub fn cached_pixels(&self) -> usize {
        self.shared.raster.cached_pixels()
    }

    /// How many cues the worker has LAID OUT, cache hits excluded.
    ///
    /// The karaoke gate: a reveal sweep repaints one scene at a rising
    /// threshold, so this must not move while a syllable fires. Doc-hidden and
    /// public rather than `cfg(test)` because the suites in `tests/` link this
    /// crate as any dependent does.
    #[doc(hidden)]
    pub fn scene_builds(&self) -> u64 {
        self.shared.raster.scene_builds()
    }

    /// Times the bitmap decode inbox overflowed and reset the decoder.
    /// Pathological by construction. See [`BITMAP_QUEUE_LIMIT`].
    pub fn bitmap_overflow_resets(&self) -> u64 {
        self.shared.bitmap_overflow_resets.load(Ordering::Relaxed)
    }

    /// Decoded display sets given up because the pending store was full.
    pub fn bitmap_dropped_sets(&self) -> u64 {
        self.shared.state.lock().sched.bitmap_dropped()
    }

    /// Packets the decoder could not take: a caught panic, or a format with no
    /// decoder behind it.
    pub fn bitmap_decode_errors(&self) -> u64 {
        self.shared.bitmap_decode_errors.load(Ordering::Relaxed)
    }

    /// Display sets the decoder has produced.
    pub fn bitmap_sets_decoded(&self) -> u64 {
        self.shared.bitmap_sets_decoded.load(Ordering::Relaxed)
    }

    /// What each bitmap packet cost the decoder, in order, bounded.
    pub fn bitmap_decode_latencies(&self) -> Vec<Duration> {
        self.shared
            .bitmap_decode_latencies
            .lock()
            .iter()
            .copied()
            .collect()
    }

    /// Start the raster worker and have it build its fontmap now.
    ///
    /// First-use fontconfig/fontmap construction can cost seconds, which is
    /// why it happens here, on a dedicated thread, at sink construction. Never
    /// on a streaming or event-loop thread, and never in the middle of a cue.
    pub fn warm(&self) {
        self.shared.raster.warm();
    }

    /// What the last rasters cost, in order, from the request that reached the
    /// worker to the pixels being ready.
    ///
    /// A warm engine must put a cue on screen well inside a frame. Cache hits
    /// never reach the worker, so this measures the rasterizer rather than the
    /// cache in front of it.
    pub fn raster_latencies(&self) -> Vec<Duration> {
        self.shared.raster.latencies()
    }

    pub fn warm_up_time(&self) -> Option<Duration> {
        self.shared.raster.warm_cost()
    }

    /// The same, for the bitmap decode inbox.
    fn with_decode_inbox<R>(
        &self,
        mut f: impl FnMut(&Arc<BitmapInbox>, &mut BitmapSlot) -> R,
    ) -> R {
        loop {
            let inbox = self.decode_inbox();
            let mut slot = inbox.slot.lock();
            if slot.retired {
                continue;
            }
            return f(&inbox, &mut slot);
        }
    }

    /// The decode worker is spawned on the first bitmap packet, so a pipeline
    /// that never carries one never pays for the thread.
    fn decode_inbox(&self) -> Arc<BitmapInbox> {
        let mut worker = self.shared.decode_worker.lock();
        if let Some(handle) = worker.as_ref() {
            return handle.inbox.clone();
        }
        let inbox = Arc::new(BitmapInbox::default());
        let weak = Arc::downgrade(&self.shared);
        let thread_inbox = inbox.clone();
        let spawned = std::thread::Builder::new()
            .name("fvid-sub-decode".to_owned())
            .spawn(move || decode_worker_main(weak, thread_inbox));
        match spawned {
            Ok(_) => {
                *worker = Some(DecodeHandle {
                    inbox: inbox.clone(),
                })
            }
            Err(err) => warn!(%err, "failed to spawn the bitmap subtitle decode thread"),
        }
        inbox
    }

    /// Stop the decode worker before it takes its next packet, until the
    /// returned guard is dropped.
    ///
    /// Exists so a test can fill the inbox to a known depth without racing the
    /// worker that is draining it. The overflow-reset behaviour is otherwise
    /// only observable by luck. Nothing in production calls this.
    #[doc(hidden)]
    pub fn hold_decode_for_test(&self) -> DecodeHold {
        let inbox = self.with_decode_inbox(|inbox, slot| {
            slot.held = true;
            // A held worker cannot retire (retirement wants an idle AND unheld
            // slot), so this is the live inbox for the guard's lifetime.
            inbox.clone()
        });
        DecodeHold { inbox }
    }

    /// Whether each engine worker thread currently exists.
    ///
    /// `(raster, decode)`. Doc-hidden and for the retirement tests: the
    /// handles are what a submitter consults, so this is the engine's own
    /// answer to "is there a worker", beside the operating system's.
    #[doc(hidden)]
    pub fn workers_live(&self) -> (bool, bool) {
        (self.shared.raster.worker_live(), self.shared.decode_worker.lock().is_some())
    }

    /// Install the decoder factory the worker builds from.
    ///
    /// TESTS ONLY. Production never calls this, and with nothing installed
    /// the worker builds from [`crate::subpic::decoder_for`], which is the one
    /// place the implemented format set is written down.
    ///
    /// Doc-hidden `pub` rather than `cfg(test)`, and deliberately the same
    /// visibility as [`CueEngine::hold_decode_for_test`]: the phase's
    /// integration tests live in `tests/`, which links this crate the way any
    /// dependent does and cannot see a `cfg(test)` item at all. A seam only the
    /// unit tests can reach is a seam the end-to-end tests have to work around.
    #[doc(hidden)]
    pub fn set_decoder_factory(
        &self,
        factory: impl Fn(BitmapSubFormat) -> Option<Box<dyn SubpicDecoder>> + Send + Sync + 'static,
    ) {
        *self.shared.decoder_factory.lock() = Some(Arc::new(factory));
    }

}

/// Everything on screen right now, as overlays: one per active text cue, then
/// the bitmap set's regions.
///
/// THE STACK. Each cue's raster already carries the placement its content asked
/// for (bottom-centre of the picture by house policy, or wherever the file put
/// it on the cue-IR arm: `line:`/`position:`, SSA `\pos`, an `{\an8}` anchor).
/// That placement is honoured as the cue's first choice, and cues are placed
/// newest first, so the latest-starting cue keeps the spot it asked for and an
/// earlier one still showing moves up out of its way. Read top to bottom the
/// stack is start order, the roll-up rule: new text enters at the bottom and
/// old text climbs. Files split one sentence across two overlapping cues
/// expecting exactly that, and the seniority rule (earliest keeps the bottom,
/// which is what libass does) renders those sentences in reverse.
///
/// Two known limits, both deliberate:
///
///  * a stack tall enough to run off the top of the canvas clamps at 0 and the
///    cues there do overlap. [`MAX_ACTIVE_CUES`] keeps that out of reach for
///    real files.
///  * an unpositioned cue moving up may still land in a positioned cue's space
///    if the positioned cue starts earlier (it has not been placed yet when the
///    later one is). Fixing that means placing positioned cues first, which
///    reorders the stack, and the ordering is worth more.
///
/// `text` is false for a scene consumer, whose text cues are drawn from their
/// display lists and must not also be composited here. Authoritative rather
/// than emergent: a cache still holding a paint from before the switch would
/// otherwise put that cue on screen twice.
fn active_overlays(state: &State, text: bool) -> SmallVec<[Overlay; MAX_OVERLAYS]> {
    let mut overlays: SmallVec<[Overlay; MAX_OVERLAYS]> = SmallVec::new();
    for active in state.sched.active().iter().filter(|_| text) {
        // Whatever the last `drive` decided is drawable, which includes a
        // stale scene and a paint one rank behind: both beat blanking the line
        // while the replacement is built.
        let (Some(painted), Some(at)) = (active.payload.painted(), active.payload.placed()) else {
            continue;
        };
        overlays.push(to_overlay(painted, at));
    }
    // The bitmap set rides beside the text cue, not instead of it: a source
    // can carry a subpicture track and a text track at once, and the
    // compositor already mixes the two spaces per overlay. These bypass the
    // raster path entirely. They are pixels already, so there is no key, cache
    // or worker between the decoder and the screen.
    if let Some(update) = state.sched.bitmap_active().as_ref() {
        overlays.extend(
            update.regions.iter().map(crate::subpic::region_overlay),
        );
    }
    overlays
}

/// [`active_overlays`] for the scene lanes: the same cues, in the same stacking
/// order, as display lists rather than pixels.
///
/// No paint is read, so a cue shows the frame its LAYOUT is ready
/// rather than the frame its paint is. That is the whole point of the lane: the
/// consumer paints it itself, in its own frame.
///
/// Bitmap subtitle sets are deliberately absent. They are decoded pixels with
/// no display list behind them, so a scene consumer cannot draw them and
/// pretending otherwise here would silently drop them.
fn active_scenes(state: &State) -> SmallVec<[ShownScene; MAX_ACTIVE_CUES]> {
    state
        .sched
        .active()
        .iter()
        .filter_map(|active| active.payload.shown())
        .collect()
}

/// One painted cue as an overlay.
///
/// Cheap. The pixel buffer is refcount-shared with the paint the engine
/// cached, so this is a handful of scalar copies per frame, not a memcpy of a
/// cue strip.
fn to_overlay(painted: &Painted, (x, y): (i32, i32)) -> Overlay {
    Overlay {
        pixels: painted.pixels.clone(),
        width: painted.width,
        height: painted.height,
        x,
        y,
        render_width: painted.width,
        render_height: painted.height,
        // Window space: the paint was laid out at display resolution, so it
        // must not be scaled (or rotated) with the video.
        space: OverlaySpace::Window,
    }
}

/// Retire the bitmap decode worker if it is STILL idle. Answers whether the
/// caller should end its thread.
///
/// # The handshake
///
/// The only thing that could go wrong is LOSING WORK, a submitter that hands a
/// packet to an inbox nobody will ever read again, and the flag plus the lock
/// order is the whole answer to it:
///
///  * a submitter takes the inbox lock to write and this takes the same lock
///    to retire, so the two cannot interleave. Whoever gets there first wins;
///  * `retired` is set in the SAME critical section that clears the engine's
///    handle, so a submitter holding a stale `Arc` sees the flag, and every
///    submitter that arrives afterwards gets a freshly spawned worker from the
///    now-empty handle. [`CueEngine::with_decode_inbox`] is where it is read;
///  * the handle is cleared only if it still points at THIS inbox, so a worker
///    that somehow outlived its own replacement cannot unregister it.
///
/// The raster worker next door runs the same handshake, in
/// [`i_slint_cue::engine`].
fn retire_decode_worker(shared: &Weak<Shared>, inbox: &Arc<BitmapInbox>) -> bool {
    let Some(shared) = shared.upgrade() else {
        return true;
    };
    let mut handle = shared.decode_worker.lock();
    let mut slot = inbox.slot.lock();
    if slot.quit {
        return true;
    }
    // A HELD worker is not an idle one: the test latch stops it draining a
    // queue it is meant to let fill up.
    if !slot.queue.is_empty() || slot.held {
        return false;
    }
    if handle
        .as_ref()
        .is_some_and(|live| Arc::ptr_eq(&live.inbox, inbox))
    {
        *handle = None;
    }
    slot.retired = true;
    debug!("the bitmap subtitle decode worker retired after an idle period");
    true
}

/// The decode worker's inbox: an ORDERED FIFO, and that is the whole point.
///
/// The raster worker next door keeps a single newest-wins slot, because a
/// raster request is a pure function of a key and an older one is worthless.
/// Bitmap packets are the opposite: they are the input to a state machine, so
/// order is meaning and an unread packet is a hole. Nothing in here may be
/// borrowed from `Slot`.
#[derive(Default)]
struct BitmapSlot {
    /// `(epoch, packet)`: the epoch the packet was submitted under, checked
    /// again at publish so a reset that happens mid-decode discards the result.
    queue: VecDeque<(u64, BitmapPacket)>,
    /// Test latch (see [`CueEngine::hold_decode_for_test`]).
    held: bool,
    quit: bool,
    /// See [`Slot::retired`], the same flag and the same protocol.
    retired: bool,
}

#[derive(Default)]
struct BitmapInbox {
    slot: Mutex<BitmapSlot>,
    cv: Condvar,
}

struct DecodeHandle {
    inbox: Arc<BitmapInbox>,
}

impl DecodeHandle {
    fn stop(&self) {
        let mut slot = self.inbox.slot.lock();
        slot.quit = true;
        self.inbox.cv.notify_all();
    }
}

/// Guard returned by [`CueEngine::hold_decode_for_test`]; releases the worker
/// when dropped.
#[doc(hidden)]
pub struct DecodeHold {
    inbox: Arc<BitmapInbox>,
}

impl Drop for DecodeHold {
    fn drop(&mut self) {
        let mut slot = self.inbox.slot.lock();
        slot.held = false;
        self.inbox.cv.notify_all();
    }
}

/// The `fvid-sub-decode` thread: pop a packet, decode it, publish what came
/// out.
///
/// Everything expensive about bitmap subtitles happens here and nowhere else
/// (mapping the buffer, RLE expansion, palette conversion, the persistent
/// region buffers DVB paints into), which is what keeps `submit_bitmap` a
/// pointer copy on the delivery thread.
fn decode_worker_main(shared: Weak<Shared>, inbox: Arc<BitmapInbox>) {
    let mut decoder: Option<(BitmapSubFormat, Box<dyn SubpicDecoder>)> = None;
    let mut applied_codec_data: Option<gst::Buffer> = None;
    let mut applied_size: Option<(u32, u32)> = None;
    let mut current_epoch: Option<u64> = None;
    // The `(format, epoch)` the "no decoder" warning was last raised for. The
    // COUNTER stays per packet, since it measures the defect, but the log line
    // does not: an unwired format would otherwise print once per packet for the
    // whole stream.
    let mut warned_undecodable: Option<(BitmapSubFormat, u64)> = None;

    'work: loop {
        let (epoch, packet) = {
            let mut slot = inbox.slot.lock();
            loop {
                if slot.quit {
                    break 'work;
                }
                if let Some(packet) = (!slot.held).then(|| slot.queue.pop_front()).flatten() {
                    break packet;
                }
                // See the raster worker: a track that is deselected for the
                // rest of a film should not keep a decode thread parked on this
                // condvar. A HELD worker never retires (the test latch is not
                // idleness), and neither does one with packets waiting.
                if inbox
                    .cv
                    .wait_for(&mut slot, WORKER_IDLE_TIMEOUT)
                    .timed_out()
                {
                    drop(slot);
                    if retire_decode_worker(&shared, &inbox) {
                        return;
                    }
                    slot = inbox.slot.lock();
                }
            }
        };
        let Some(shared) = shared.upgrade() else {
            break;
        };

        // A reset happened behind this packet: whatever the decoder has half
        // assembled describes a timeline that no longer exists.
        //
        // BOTH setup memories go with it, and that symmetry is the point:
        // [`SubpicDecoder::reset`] is contracted to return the decoder to its
        // JUST-CONSTRUCTED state, so after it the decoder knows neither the
        // codec_data nor the video size, exactly as a freshly built one does.
        // Remembering the size across a reset left post-reset decoders scaling
        // their regions onto the default grid for the rest of the stream. The
        // panic path below already cleared both, which is where the asymmetry
        // showed.
        if current_epoch != Some(epoch) {
            current_epoch = Some(epoch);
            if let Some((_, decoder)) = decoder.as_mut() {
                decoder.reset();
            }
            applied_codec_data = None;
            applied_size = None;
        }

        if decoder
            .as_ref()
            .is_none_or(|(format, _)| *format != packet.format)
        {
            match build_decoder(&shared, packet.format) {
                Some(built) => decoder = Some((packet.format, built)),
                None => {
                    // The driver's caps gate only admits formats this crate can
                    // decode, so reaching here is a wiring bug rather than a
                    // stream property -- counted, never fatal. Counted per
                    // PACKET (the counter is the measurement); logged once per
                    // format per epoch, because a mis-wired stream delivers
                    // packets for as long as it plays.
                    if warned_undecodable != Some((packet.format, epoch)) {
                        warned_undecodable = Some((packet.format, epoch));
                        warn!(
                            format = ?packet.format,
                            epoch,
                            "no decoder for this bitmap subtitle format; every packet of it is \
                             counted, this line is not repeated until the format or the epoch \
                             changes"
                        );
                    }
                    shared.bitmap_decode_errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            applied_codec_data = None;
            applied_size = None;
        }
        let (_, active) = decoder.as_mut().expect("built just above");

        let video_size = shared.state.lock().video_size;
        if video_size != (0, 0) && applied_size != Some(video_size) {
            active.set_video_size(video_size.0, video_size.1);
            applied_size = Some(video_size);
        }
        // CONTENT comparison, the opposite of the packet dedupe next door (see
        // [`same_buffer`]), and here that is the semantic wanted. What
        // matters about `codec_data` is the SETUP BYTES it carries: a caps
        // renegotiation that hands over a fresh buffer holding the same VOBSUB
        // palette must not re-apply it, while the same object cannot carry
        // different bytes. `!=` on `gst::Buffer` is gstreamer-rs's size +
        // memcmp of the mapped bytes, which is exactly that question.
        //
        // One caveat, accepted rather than worked around: `BufferRef::eq` maps
        // both buffers and answers FALSE when a map fails, so equality is not
        // even reflexive for an unmappable buffer, so such a `codec_data` would
        // be re-applied on every packet. That costs one `map_readable` attempt
        // and one `set_codec_data` per packet, both of which the decoder must
        // tolerate anyway (applying the same setup twice is idempotent by the
        // trait's contract), and a `codec_data` that cannot be mapped fails the
        // `map_readable` below in any case, so nothing reaches the decoder.
        if let Some(codec_data) = packet.codec_data.as_ref()
            && applied_codec_data.as_ref() != Some(codec_data)
        {
            match codec_data.map_readable() {
                Ok(map) => {
                    active.set_codec_data(map.as_slice());
                    applied_codec_data = Some(codec_data.clone());
                }
                Err(_) => warn!("bitmap subtitle codec_data could not be mapped"),
            }
        }

        // Malformed input is the decoder's own business and must come back as a
        // counted reset, never a panic. This is the backstop for
        // when that discipline fails: a panic on this thread would leave the
        // handle held and every later packet going to a dead worker, i.e.
        // subtitles silently off for the process lifetime. Same shape as the
        // raster worker's catch_unwind.
        let started = Instant::now();
        let decoded =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| active.push(&packet)));
        record_bitmap_latency(&shared, started.elapsed());
        // What the decoder threw away, drained per packet: malformed input is a
        // counted reset inside the decoder, and this is the count
        // reaching the engine's own instrument. Taken before the panic arm so a
        // decoder that counted a reset and then panicked reports both.
        let dropped = active.take_decode_errors();
        if dropped > 0 {
            shared
                .bitmap_decode_errors
                .fetch_add(dropped, Ordering::Relaxed);
        }
        let updates = match decoded {
            Ok(updates) => updates,
            Err(_) => {
                warn!(format = ?packet.format, "bitmap subtitle decoder panicked; rebuilding it");
                shared.bitmap_decode_errors.fetch_add(1, Ordering::Relaxed);
                decoder = None;
                applied_codec_data = None;
                applied_size = None;
                continue;
            }
        };
        if updates.is_empty() {
            continue;
        }
        shared
            .bitmap_sets_decoded
            .fetch_add(updates.len() as u64, Ordering::Relaxed);
        if publish_bitmap(&shared, epoch, updates) {
            mark_changed(&shared);
        }
    }
}

/// Build the decoder for a format. Production reads the implemented set from
/// [`crate::subpic::decoder_for`]; tests may install their own factory through
/// [`CueEngine::set_decoder_factory`].
fn build_decoder(shared: &Arc<Shared>, format: BitmapSubFormat) -> Option<Box<dyn SubpicDecoder>> {
    let factory = shared.decoder_factory.lock().clone();
    match factory {
        Some(factory) => factory(format),
        None => crate::subpic::decoder_for(format),
    }
}

/// Hand decoded sets to the engine. Returns whether what is on screen changed.
///
/// The epoch check is the ONE serialization point between the decode worker and
/// the rest of the engine: a `clear`, `flush` or overflow that happened while
/// this packet was being decoded bumped the epoch, and the set it produced
/// belongs to a track or timeline that is gone. Checking it under the state
/// lock (the same lock a reset takes) is what makes "dropped" and "adopted"
/// the only two outcomes.
fn publish_bitmap(shared: &Arc<Shared>, epoch: u64, updates: Vec<DisplayUpdate>) -> bool {
    let mut state = shared.state.lock();
    if state.sched.bitmap_epoch() != epoch {
        debug!(
            epoch,
            current = state.sched.bitmap_epoch(),
            sets = updates.len(),
            "dropping bitmap sets decoded before a reset"
        );
        return false;
    }

    // Insert-sorted and trimmed by the schedule, which owns the backlog
    // policy for both sides.
    for update in updates {
        state.sched.submit_bitmap(epoch, update);
    }

    // A set that covers the frame already on screen becomes visible without a
    // new frame: the paused path, identical to the text one.
    match state.sched.last_shown_rt() {
        Some(rt) => state.sched.advance_bitmap(rt),
        None => false,
    }
}

/// Record what one packet cost the decoder, oldest dropped.
fn record_bitmap_latency(shared: &Arc<Shared>, cost: Duration) {
    let mut latencies = shared.bitmap_decode_latencies.lock();
    if latencies.len() == BITMAP_LATENCY_WINDOW {
        latencies.pop_front();
    }
    latencies.push_back(cost);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue_ir::{self, RasterCtx};
    use crate::cue_scene::ALL_REVEALED;

    fn ms(value: u64) -> gst::ClockTime {
        gst::ClockTime::from_mseconds(value)
    }

    fn cue(text: &str, start: u64, duration: u64) -> CueInput {
        CueInput {
            format: SubtitleTextFormat::Utf8,
            text: text.to_owned(),
            start_rt: ms(start),
            end_rt: Some(ms(start + duration)),
        }
    }

    /// The TOPMOST cue showing, without needing a raster: the timing tests are
    /// about scheduling, not pixels.
    ///
    /// Topmost = latest start = last in the active set, which is the cue the
    /// engine would have shown when it could only show one. Tests about
    /// overlap read [`showing_all`] instead, and the ones that predate
    /// multi-active keep asking the question they always asked.
    fn showing(engine: &CueEngine) -> Option<String> {
        showing_all(engine).pop()
    }

    /// Every cue on screen, bottom of the stack first (earliest start first).
    fn showing_all(engine: &CueEngine) -> Vec<String> {
        engine
            .shared
            .state
            .lock()
            .sched.active()
            .iter()
            .map(|active| active.cue.text.clone())
            .collect()
    }

    fn advance(engine: &CueEngine, rt: gst::ClockTime) -> Option<String> {
        engine.overlays_for(Some(rt));
        showing(engine)
    }

    /// What a PAUSED read puts on screen: the render thread's
    /// `current_overlays` against the frozen frame, then the active set it
    /// produced. The paused twin of [`advance`], and the only way to see
    /// [`PAUSED_CUE_LOOKAHEAD`]. Reading `showing_all` alone would report
    /// the schedule as the last FRAME left it.
    fn showing_paused(engine: &CueEngine) -> Vec<String> {
        engine.current_overlays();
        showing_all(engine)
    }

    fn wait_for<F: Fn() -> bool>(condition: F) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    // ---- timing, ported from the retired fcasttextoverlay's harness tests ----

    /// Its `test_basic_passthrough`: a frame with no cue in flight carries
    /// nothing.
    #[test]
    fn no_cue_means_no_overlay() {
        let engine = CueEngine::new();
        assert!(engine.overlays_for(Some(gst::ClockTime::ZERO)).is_empty());
        assert_eq!(showing(&engine), None);
    }

    /// `test_basic_video_with_subtitle` (`:1054`): a cue covering the frame is
    /// the active cue.
    #[test]
    fn cue_covering_the_frame_is_active() {
        let engine = CueEngine::new();
        engine.submit(cue("Hello", 0, 1000));
        assert_eq!(
            advance(&engine, gst::ClockTime::ZERO).as_deref(),
            Some("Hello")
        );
    }

    /// The end is exclusive: `text_running_time_end <= vid_running_time` pops.
    #[test]
    fn cue_is_not_active_at_its_exact_end() {
        let engine = CueEngine::new();
        engine.submit(cue("Hello", 0, 1000));
        assert_eq!(advance(&engine, ms(1000)), None);
    }

    /// `test_multiple_frames_and_subs` (`:1114`): the 100ms frame cadence walk,
    /// including the end boundary and the gap between cues.
    #[test]
    fn multiple_frames_and_cues() {
        let engine = CueEngine::new();

        engine.submit(cue("One", 0, 300));
        for frame in 0..3u64 {
            assert_eq!(
                advance(&engine, ms(frame * 100)).as_deref(),
                Some("One"),
                "frame at {}ms",
                frame * 100
            );
        }

        // Exactly at the end boundary "One" no longer applies.
        assert_eq!(advance(&engine, ms(300)), None);
        // Gap frame, still nothing.
        assert_eq!(advance(&engine, ms(350)), None);

        engine.submit(cue("Two", 400, 200));
        for frame in 4..6u64 {
            assert_eq!(
                advance(&engine, ms(frame * 100)).as_deref(),
                Some("Two"),
                "frame at {}ms",
                frame * 100
            );
        }

        assert_eq!(advance(&engine, ms(600)), None);
    }

    #[test]
    fn cue_in_the_future_is_held_until_its_start() {
        let engine = CueEngine::new();
        engine.submit(cue("Later", 400, 200));
        assert_eq!(advance(&engine, ms(350)), None);
        assert_eq!(advance(&engine, ms(399)), None);
        assert_eq!(advance(&engine, ms(400)).as_deref(), Some("Later"));
    }

    #[test]
    fn a_cue_that_expired_between_frames_is_never_shown() {
        let engine = CueEngine::new();
        engine.submit(cue("Blink", 100, 50));
        engine.submit(cue("Seen", 200, 100));
        // The frame jumps over "Blink" entirely.
        assert_eq!(advance(&engine, ms(250)).as_deref(), Some("Seen"));
    }

    /// RE-BASELINED at the multi-active change. This test was
    /// `latest_start_wins_when_two_cues_cover_the_frame` and pinned the
    /// inherited limitation: the newer cue REPLACED the older one, so a file
    /// with two overlapping cues showed one of them at a time.
    ///
    /// Latest-start-wins survives as ORDERING (the later cue takes the bottom
    /// slot, the earlier one climbs above it) and `showing()` still answers
    /// with the latest cue, which is why every test that predates this one
    /// reads the same as it did.
    #[test]
    fn two_cues_covering_the_frame_both_show_latest_at_the_bottom() {
        let engine = CueEngine::new();
        engine.submit(cue("First", 0, 1000));
        engine.submit(cue("Second", 500, 1000));

        engine.overlays_for(Some(ms(600)));
        assert_eq!(
            showing_all(&engine),
            vec!["First".to_owned(), "Second".to_owned()],
            "the second cue replaced the first instead of joining it"
        );
        assert_eq!(
            showing(&engine).as_deref(),
            Some("Second"),
            "the latest start is the newest entry of the stack"
        );

        // Each leaves on its OWN end -- the half the single-active rule could
        // not express, since there the arrival of a cue ended what was showing.
        assert_eq!(advance(&engine, ms(1_000)).as_deref(), Some("Second"));
        assert_eq!(
            showing_all(&engine),
            vec!["Second".to_owned()],
            "the first cue outlived its own end"
        );
        assert_eq!(advance(&engine, ms(1_500)), None);
    }

    /// An out-of-order delivery whose start is BEHIND a cue already showing
    /// slots in front of it, above it on screen: the stack is ordered by start
    /// time, not by arrival.
    #[test]
    fn a_late_delivered_earlier_cue_slots_by_its_start() {
        let engine = CueEngine::new();
        engine.submit(cue("Later start", 500, 1000));
        engine.overlays_for(Some(ms(600)));
        assert_eq!(showing_all(&engine), vec!["Later start".to_owned()]);

        engine.submit(cue("Earlier start", 0, 1000));
        engine.overlays_for(Some(ms(600)));
        assert_eq!(
            showing_all(&engine),
            vec!["Earlier start".to_owned(), "Later start".to_owned()],
            "the stack is ordered by start time, not by when the cue was handed over"
        );
    }

    /// The backstop: more overlapping cues than the screen holds gives up the
    /// OLDEST START, which is the one that has been readable the longest.
    #[test]
    fn more_overlapping_cues_than_the_cap_gives_up_the_oldest() {
        let engine = CueEngine::new();
        const CUES: u64 = MAX_ACTIVE_CUES as u64 + 3;
        for index in 0..CUES {
            engine.submit(cue(&format!("cue {index}"), index * 10, 10_000));
        }
        engine.overlays_for(Some(ms(CUES * 10)));

        let showing = showing_all(&engine);
        assert_eq!(showing.len(), MAX_ACTIVE_CUES, "the cap did not bite");
        assert_eq!(
            showing.first().map(String::as_str),
            Some("cue 3"),
            "the cap gave up the newest cues instead of the oldest: {showing:?}"
        );
        assert_eq!(
            showing.last().map(String::as_str),
            Some(&*format!("cue {}", CUES - 1)),
            "the cue that just arrived is not on screen: {showing:?}"
        );
    }

    /// Redelivery idempotence is unchanged by multi-active: the merge finds the
    /// repeated cue wherever it is, including underneath another one.
    #[test]
    fn a_redelivery_merges_into_the_cue_it_repeats_even_under_another() {
        let engine = CueEngine::new();
        engine.submit(cue("bottom", 0, 2000));
        engine.submit(cue("top", 500, 2000));
        engine.overlays_for(Some(ms(600)));
        assert_eq!(showing_all(&engine).len(), 2);

        // The replay: the anchor goes, the cues stay, the file comes again.
        engine.reset_timeline();
        engine.submit(cue("bottom", 0, 2000));
        engine.submit(cue("top", 500, 2000));
        assert_eq!(
            engine.shared.state.lock().sched.pending().len(),
            0,
            "a cue on screen was queued a second time"
        );
        assert_eq!(
            showing_all(&engine),
            vec!["bottom".to_owned(), "top".to_owned()],
            "the redelivery disturbed the stack"
        );

        // ...and a zero-length twin of the BOTTOM cue cannot shorten it.
        engine.submit(CueInput {
            format: SubtitleTextFormat::Utf8,
            text: "bottom".to_owned(),
            start_rt: ms(0),
            end_rt: Some(ms(0)),
        });
        assert_eq!(engine.shared.state.lock().sched.pending().len(), 0);
        engine.overlays_for(Some(ms(1_500)));
        assert_eq!(
            showing_all(&engine).len(),
            2,
            "the twin expired the cue it repeats"
        );
    }

    #[test]
    fn out_of_order_submission_is_ordered_by_start() {
        let engine = CueEngine::new();
        engine.submit(cue("Second", 500, 500));
        engine.submit(cue("First", 0, 500));
        assert_eq!(advance(&engine, ms(100)).as_deref(), Some("First"));
        assert_eq!(advance(&engine, ms(600)).as_deref(), Some("Second"));
    }

    #[test]
    fn an_open_ended_cue_never_expires() {
        let engine = CueEngine::new();
        engine.submit(CueInput {
            format: SubtitleTextFormat::Utf8,
            text: "Forever".to_owned(),
            start_rt: gst::ClockTime::ZERO,
            end_rt: None,
        });
        assert_eq!(advance(&engine, ms(0)).as_deref(), Some("Forever"));
        assert_eq!(
            advance(&engine, gst::ClockTime::from_seconds(3600)).as_deref(),
            Some("Forever")
        );
    }

    #[test]
    fn clear_drops_pending_and_active() {
        let engine = CueEngine::new();
        engine.submit(cue("Showing", 0, 1000));
        engine.submit(cue("Queued", 2000, 1000));
        assert_eq!(advance(&engine, ms(10)).as_deref(), Some("Showing"));

        engine.clear();
        assert_eq!(showing(&engine), None);
        assert_eq!(advance(&engine, ms(2500)), None);
    }

    #[test]
    fn flush_drops_the_timeline_anchor_too() {
        let engine = CueEngine::new();
        engine.submit(cue("Showing", 0, 1000));
        advance(&engine, ms(10));

        engine.flush();
        assert_eq!(showing(&engine), None);
        assert_eq!(engine.shared.state.lock().sched.last_shown_rt(), None);
    }

    /// The paused path in miniature: no frame flows, but a cue covering the
    /// frame already on screen becomes active and fires the change callback.
    #[test]
    fn a_cue_arriving_while_paused_activates_against_the_last_shown_frame() {
        let engine = CueEngine::new();
        let fired = Arc::new(AtomicU64::new(0));
        let counter = fired.clone();
        engine.set_on_change(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        // A frame goes by with no cues, then playback stops.
        advance(&engine, ms(5000));
        assert_eq!(showing(&engine), None);
        assert!(!engine.take_dirty());

        engine.submit(cue("Instant", 4000, 2000));
        assert_eq!(showing(&engine).as_deref(), Some("Instant"));
        assert!(engine.take_dirty());
        assert!(fired.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn a_frame_without_a_running_time_leaves_the_state_alone() {
        let engine = CueEngine::new();
        engine.submit(cue("Showing", 0, 1000));
        advance(&engine, ms(10));

        engine.overlays_for(None);
        assert_eq!(showing(&engine).as_deref(), Some("Showing"));
        assert_eq!(engine.shared.state.lock().sched.last_shown_rt(), Some(ms(10)));
    }

    /// The whole-file burst, at engine scale: an external subtitle arrives as
    /// one burst of the whole file, and every cue in it has to survive to
    /// its turn.
    #[test]
    fn a_whole_file_burst_is_retained_and_shows_at_its_times() {
        let engine = CueEngine::new();
        const CUES: u64 = 1000;
        // One per second, as a real file is; delivered in a tight loop, as an
        // unsynced branch delivers it.
        for index in 0..CUES {
            engine.submit(cue(&format!("cue {index}"), 10_000 + index * 1000, 800));
        }
        assert_eq!(engine.dropped_cues(), 0, "a whole file must fit");
        assert_eq!(engine.shared.state.lock().sched.pending().len(), CUES as usize);

        // First, middle and last each show at their own time. The middle and
        // the last are the ones the old drop-oldest bound discarded.
        assert_eq!(advance(&engine, ms(10_100)).as_deref(), Some("cue 0"));
        assert_eq!(
            advance(&engine, ms(10_000 + 500 * 1000 + 100)).as_deref(),
            Some("cue 500")
        );
        assert_eq!(
            advance(&engine, ms(10_000 + 999 * 1000 + 100)).as_deref(),
            Some("cue 999")
        );
        assert_eq!(engine.dropped_cues(), 0);
    }

    /// Overflow gives up the FURTHEST FUTURE, never the next cue up. The
    /// inversion of the drop-oldest policy this replaces.
    #[test]
    fn the_backlog_gives_up_the_furthest_future_and_counts_it() {
        let engine = CueEngine::new();
        for index in 0..(PENDING_LIMIT as u64 + 4) {
            engine.submit(cue(&format!("cue {index}"), 10_000 + index * 100, 50));
        }
        assert_eq!(engine.dropped_cues(), 4);
        let state = engine.shared.state.lock();
        assert_eq!(state.sched.pending_len(), PENDING_LIMIT);
        // The survivors are the SOONEST ones: the next cue up is still there,
        // and it is the four furthest out that went.
        assert_eq!(state.sched.pending().front().unwrap().text, "cue 0");
        assert_eq!(
            state.sched.pending().back().unwrap().text,
            format!("cue {}", PENDING_LIMIT - 1)
        );
    }


    /// Converter output carries every cue twice: a zero-length
    /// record (`start == end`) and then the real one. Both are faithful
    /// deliveries; the engine is where they become one cue.
    #[test]
    fn a_zero_length_twin_merges_into_the_cue_it_repeats() {
        let engine = CueEngine::new();
        let degenerate = CueInput {
            format: SubtitleTextFormat::Utf8,
            text: "twin".to_owned(),
            start_rt: ms(2965),
            end_rt: Some(ms(2965)),
        };
        let real = CueInput {
            end_rt: Some(ms(4185)),
            ..degenerate.clone()
        };
        engine.submit(degenerate.clone());
        engine.submit(real.clone());

        {
            let state = engine.shared.state.lock();
            assert_eq!(state.sched.pending_len(), 1, "the twins are one cue");
            assert_eq!(state.sched.pending()[0].end_rt, Some(ms(4185)));
        }
        // And it is shown for its REAL window rather than expiring on arrival.
        assert_eq!(advance(&engine, ms(3000)).as_deref(), Some("twin"));

        // The other order merges too: a degenerate copy arriving second must
        // not shorten what it repeats.
        let engine = CueEngine::new();
        engine.submit(real);
        engine.submit(degenerate);
        let state = engine.shared.state.lock();
        assert_eq!(state.sched.pending_len(), 1);
        assert_eq!(state.sched.pending()[0].end_rt, Some(ms(4185)));
    }

    /// The redelivery may also repeat the cue that is ON SCREEN, and the
    /// field's sequence puts the engine in the one state where that is not
    /// absorbed anyway: a replay's STREAM_START drops the timeline anchor
    /// ([`CueEngine::reset_timeline`]) while leaving the cue showing, so the
    /// schedule is not advanced on submit and a repeat of the active cue would
    /// sit in `pending` waiting to re-activate the cue already on screen.
    #[test]
    fn a_redelivery_of_the_cue_on_screen_is_absorbed_by_it() {
        let engine = CueEngine::new();
        engine.submit(cue("showing", 1000, 2000));
        assert_eq!(advance(&engine, ms(1500)).as_deref(), Some("showing"));

        // The replay: the anchor goes, the cue stays, the file comes again.
        engine.reset_timeline();
        engine.submit(cue("showing", 1000, 2000));
        assert_eq!(
            engine.shared.state.lock().sched.pending().len(),
            0,
            "the cue on screen was queued a second time"
        );
        assert_eq!(showing(&engine).as_deref(), Some("showing"));

        // A zero-length twin of it lands the same way, and cannot shorten it.
        engine.submit(CueInput {
            format: SubtitleTextFormat::Utf8,
            text: "showing".to_owned(),
            start_rt: ms(1000),
            end_rt: Some(ms(1000)),
        });
        assert_eq!(engine.shared.state.lock().sched.pending().len(), 0);
        assert_eq!(advance(&engine, ms(2500)).as_deref(), Some("showing"));
    }

    /// A replay re-delivers the whole file. The second burst must change
    /// nothing at all: same cues, same count, nothing dropped.
    #[test]
    fn a_replays_whole_file_redelivery_is_a_no_op() {
        let engine = CueEngine::new();
        let burst = || {
            for index in 0..500u64 {
                engine.submit(cue(&format!("cue {index}"), 10_000 + index * 1000, 800));
            }
        };
        burst();
        let after_first = engine.shared.state.lock().sched.pending().len();
        assert_eq!(after_first, 500);

        burst();
        assert_eq!(
            engine.shared.state.lock().sched.pending().len(),
            after_first,
            "the redelivery queued a second copy of the file"
        );
        assert_eq!(engine.dropped_cues(), 0);
        assert_eq!(advance(&engine, ms(10_100)).as_deref(), Some("cue 0"));
    }

    #[test]
    fn the_timing_predicates_match_the_element() {
        // `text_running_time_end <= vid_running_time`
        assert!(cue_is_too_old(Some(ms(300)), ms(300)));
        assert!(cue_is_too_old(Some(ms(300)), ms(301)));
        assert!(!cue_is_too_old(Some(ms(300)), ms(299)));
        assert!(!cue_is_too_old(None, ms(u64::from(u32::MAX))));

        assert!(cue_is_in_future(ms(400), ms(399)));
        assert!(!cue_is_in_future(ms(400), ms(400)));
        assert!(!cue_is_in_future(ms(400), ms(401)));
    }

    #[test]
    fn running_time_comes_from_the_captured_video_segment() {
        gst::init().unwrap();

        let engine = CueEngine::new();
        assert_eq!(engine.video_running_time(Some(ms(500))), None);

        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(ms(1000));
        segment.set_time(ms(1000));
        segment.set_position(ms(1000));
        engine.set_video_segment(segment.upcast_ref());

        assert_eq!(engine.video_running_time(Some(ms(1500))), Some(ms(500)));
        assert_eq!(engine.video_running_time(None), None);

        engine.reset_timeline();
        assert_eq!(engine.video_running_time(Some(ms(1500))), None);
    }

    // ---- rasterization ----

    /// Lay a cue out the way the engine does: the IR when the format carries
    /// one, the plain text when it does not, and the one conversion this crate
    /// owns in between (see [`parsed`]).
    ///
    /// Takes the context, so a test that lays several cues out does not
    /// rebuild the font stack per cue.
    fn render_with(
        ctx: &mut RasterCtx,
        text: &str,
        format: SubtitleTextFormat,
        canvas: (u32, u32),
        style: &CueStyle,
    ) -> Option<crate::cue_scene::RasterOut> {
        let cue = parsed(CueInput {
            format,
            text: text.to_owned(),
            start_rt: gst::ClockTime::ZERO,
            end_rt: None,
        });
        let plain;
        let ir = match flapjack::cue::cue_ir(&cue.format) {
            Some(ir) => &**ir,
            None => {
                plain = crate::cue_ir::CueIr::from_plain_text(&cue.text);
                &plain
            }
        };
        ctx.render(ir, style, canvas, None, ALL_REVEALED as usize)
    }

    /// pangocairo draws into an image surface: no display server, no GL, no
    /// window. These tests pass under `env -u DISPLAY -u WAYLAND_DISPLAY`.
    #[test]
    fn raster_smoke() {
        let mut ctx = RasterCtx::new();
        let canvas = (1920, 1080);
        let raster = render_with(&mut ctx, "Hello, subtitles", SubtitleTextFormat::Utf8, canvas, &CueStyle::default())
            .expect("a plain cue rasterizes");

        let (width, height) = (raster.width, raster.height);
        assert!(width > 0 && height > 0);
        assert_eq!(raster.pixels.len(), width as usize * height as usize * 4);
        // Sized against the canvas, not the video, and it fits inside it. One
        // line is roughly `FONT_HEIGHT_FRACTION` of the canvas height plus the
        // outline padding, 49px of font at 1080p.
        assert!(width <= canvas.0, "raster {width} wider than the canvas");
        assert!(
            (40..160).contains(&height),
            "unexpected line height {height}"
        );

        // Bottom-centre placement, inside the canvas.
        let (x, y) = (raster.x, raster.y);
        assert!(x >= 0 && x as u32 + width <= canvas.0);
        assert!(y as u32 > canvas.1 / 2);
        assert!(y as u32 + height <= canvas.1);

        // Actual glyphs: some pixels are opaque, some are fully transparent.
        let alphas: Vec<u8> = raster
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[3])
            .collect();
        assert!(alphas.iter().any(|&a| a > 200), "no opaque pixels");
        assert!(alphas.contains(&0), "no transparent pixels");
    }

    #[test]
    fn an_empty_cue_rasterizes_to_nothing() {
        let mut ctx = RasterCtx::new();
        assert!(
            render_with(&mut ctx, "", SubtitleTextFormat::Utf8, (1920, 1080), &CueStyle::default())
                .is_none()
        );
    }

    #[test]
    fn markup_renders_differently_from_the_same_source_as_utf8() {
        let mut ctx = RasterCtx::new();
        let canvas = (1280, 720);
        let source = "<i>Bonjour</i>";

        let markup = render_with(&mut ctx, source, SubtitleTextFormat::PangoMarkup, canvas, &CueStyle::default())
            .expect("markup rasterizes");
        let utf8 = render_with(&mut ctx, source, SubtitleTextFormat::Utf8, canvas, &CueStyle::default())
            .expect("utf8 rasterizes");

        // The utf8 rendering shows the tags literally, so it is wider.
        assert_ne!((markup.width, markup.height), (utf8.width, utf8.height));
        assert!(utf8.width > markup.width);
    }

    #[test]
    fn invalid_markup_renders_its_words_with_styling_instead_of_dropping_the_cue() {
        let mut ctx = RasterCtx::new();
        let canvas = (1280, 720);
        let broken = "<b>unclosed";

        let rendered = render_with(&mut ctx, broken, SubtitleTextFormat::PangoMarkup, canvas, &CueStyle::default())
            .expect("a cue is never dropped for bad markup");
        // The tolerant parser closes the tag at end of input, so the cue
        // renders as its words WITH the styling honoured (pango used to
        // reject the whole string and the fallback discarded the bold).
        let ir = Arc::new(gstrssubparse::pango_markup::markup_to_cue_ir(broken));
        let styled = render_with(
            &mut ctx,
            broken,
            SubtitleTextFormat::CueIr { ir, pts_start: None },
            canvas,
            &CueStyle::default(),
        )
        .expect("the parsed IR rasterizes");
        let raw = render_with(&mut ctx, broken, SubtitleTextFormat::Utf8, canvas, &CueStyle::default())
            .expect("utf8 rasterizes");

        assert_eq!((rendered.width, rendered.height), (styled.width, styled.height));
        assert_eq!(rendered.pixels, styled.pixels);
        assert_ne!(
            rendered.pixels,
            raw.pixels,
            "the markup source must not reach the screen"
        );
    }

    /// THE FIELD CASE: a WebVTT voice span reaches this arm as
    /// `<v Voice1>...</v>`, which strict pango parsing rejects ("expected a
    /// `=` after attribute name"), and before the sanitizer the viewer read
    /// the tags themselves. The tolerant parser treats the unknown tag as
    /// transparent, so the words render unstyled.
    ///
    /// It arrives here for two independent reasons, so fixing only one would
    /// not do: `subparse-formats` keeps `v` in the pango-markup output to stay
    /// byte-identical to the C `subparse`, AND matroskademux emits
    /// pango-markup directly for S_TEXT/UTF8 tracks without any parser element
    /// in the chain -- the second can never be switched to cue-ir.
    #[test]
    fn a_webvtt_voice_span_renders_its_words_not_its_tags() {
        let mut ctx = RasterCtx::new();
        let canvas = (1280, 720);
        let voiced = "<v Voice1>Hello there</v> and more";

        // Precondition: strict pango-parity parsing really does refuse this,
        // so the test is about the tolerant path and not about the strict
        // parser quietly coping.
        assert!(
            gstrssubparse::pango_markup::parse_markup(voiced, None).is_err(),
            "the premise is that strict parsing rejects a voice span"
        );

        let rendered = render_with(&mut ctx, voiced, SubtitleTextFormat::PangoMarkup, canvas, &CueStyle::default())
            .expect("a cue is never dropped for bad markup");
        let words =
            render_with(&mut ctx, "Hello there and more", SubtitleTextFormat::Utf8, canvas, &CueStyle::default())
                .expect("utf8 rasterizes");
        let with_tags = render_with(&mut ctx, voiced, SubtitleTextFormat::Utf8, canvas, &CueStyle::default())
            .expect("utf8 rasterizes");

        assert_eq!(
            rendered.pixels,
            words.pixels,
            "the viewer must read only the words"
        );
        // The tags are strictly wider than the words, so this is also a
        // guard against the two rasters coinciding by accident.
        assert!(with_tags.width > words.width);
        assert_ne!(rendered.pixels, with_tags.pixels);
    }

    // ---- karaoke: one layout, a threshold per step ----

    /// A `\k`-style cue as the cue-IR arm delivers one: three syllables, the
    /// last two revealing a second apart, each in its own colour so the
    /// difference is visible in the pixels.
    fn karaoke_cue() -> (CueInput, Arc<cue_ir::CueIr>) {
        let mut ir = cue_ir::CueIr::from_plain_text("");
        let mut spans = Vec::new();
        for (index, word) in ["first ", "second ", "third"].into_iter().enumerate() {
            let mut span = cue_ir::ir::Span::plain(word);
            if index > 0 {
                span.reveal_ns = Some(index as u64 * 1_000_000_000);
            }
            span.style.foreground = Some(cue_ir::ir::Color::rgb(255, (80 * index) as u8, 0));
            spans.push(span);
        }
        ir.lines[0].spans = spans;
        let ir = Arc::new(ir);
        (
            CueInput {
                format: SubtitleTextFormat::CueIr {
                    ir: ir.clone(),
                    pts_start: Some(gst::ClockTime::ZERO),
                },
                text: "first second third".to_owned(),
                start_rt: gst::ClockTime::ZERO,
                end_rt: Some(ms(10_000)),
            },
            ir,
        )
    }


    /// THE WAVE 5 GATE: a karaoke line lays out exactly ONCE, however many
    /// syllables fire.
    ///
    /// Before this wave a reveal step was part of the raster key, so every
    /// syllable was a new key, a new parley layout and a new vello render of
    /// the whole cue -- several times a second on a fast line, at whatever the
    /// display resolution is. The step is now a threshold over one scene, so
    /// the layout counter must not move across the sweep and the cue on screen
    /// must be the same scene allocation throughout.
    #[test]
    fn a_karaoke_sweep_lays_the_cue_out_once() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        let (cue, _) = karaoke_cue();
        engine.submit(cue);

        engine.overlays_for(Some(ms(100)));
        assert!(wait_for(|| !engine.current_overlays().is_empty()));

        // The scene the whole sweep is drawn from, by pointer.
        let scene = {
            let state = engine.shared.state.lock();
            state
                .sched
                .active()
                .first()
                .expect("a cue is active")
                .payload
                .showing()
                .expect("the cue never got a display list")
                .clone()
        };
        assert_eq!(
            scene.max_rank(),
            2,
            "two timed syllables are two ranks: {scene:?}"
        );
        let builds = engine.scene_builds();
        assert_eq!(builds, 1, "the cue was laid out {builds} times, not once");

        let opening = engine.current_overlays().remove(0);
        let ink = |overlay: &Overlay| {
            overlay
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|px| px[3] > 200 && px[0] > 200)
                .count()
        };
        let mut seen = vec![ink(&opening)];

        // Cross both thresholds.
        for (rank, rt) in [(1u16, 1_500u64), (2, 2_500)] {
            engine.overlays_for(Some(ms(rt)));
            assert!(
                !engine.current_overlays().is_empty(),
                "crossing a threshold blanked the line; the rank below it was the \
                 right thing to keep showing"
            );
            assert!(
                wait_for(|| ink(&engine.current_overlays()[0]) > *seen.last().expect("a sweep")),
                "syllable {rank} never lit up"
            );

            let overlay = engine.current_overlays().remove(0);
            assert_eq!(
                (overlay.width, overlay.height),
                (opening.width, opening.height),
                "revealing a syllable resized the cue"
            );
            let live = engine.shared.state.lock();
            let active = live.sched.active().first().expect("a cue is active");
            assert_eq!(active.rank, rank, "the reveal rank did not track the clock");
            let now = active.payload.showing().expect("the sweep dropped the scene");
            assert!(
                Arc::ptr_eq(now, &scene),
                "the cue was re-keyed at a syllable, so it laid out again"
            );
            drop(live);
            assert_eq!(
                engine.scene_builds(),
                builds,
                "a syllable cost a parley layout, which is the whole thing this wave removed"
            );
            seen.push(ink(&overlay));
        }
        // One scene, one cache entry, whatever the reveal did; the paints are
        // where the steps show up, one per rank the clock passed.
        assert_eq!(
            engine.cached_rasters(),
            1,
            "a step keyed a scene of its own"
        );
        assert_eq!(
            engine.cached_pixels(),
            seen.len(),
            "the overlay lane holds one paint per reveal step and nothing else"
        );
    }

    /// THE COMPATIBILITY PATH: the pixels the engine hands the overlay lanes at
    /// reveal step N are the pixels the one-piece rasterizer produces at step
    /// N, byte for byte.
    ///
    /// [`cue_ir::RasterCtx::render`] is untouched by this wave and is what
    /// every pixel expectation in the tree was written against, so it is the
    /// pre-surgery oracle. What is under test is the engine's half of the
    /// step-to-rank mapping: an off-by-one between "thresholds passed" and
    /// "rank painted" would show a syllable early or late and nothing else
    /// would catch it.
    #[test]
    fn the_overlay_lane_paints_what_the_step_used_to_render() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        let (cue, ir) = karaoke_cue();
        engine.submit(cue);

        let mut oracle = cue_ir::RasterCtx::new();
        let style = engine.style();
        // Frame times either side of each threshold, and the step each one
        // means: 0 before the first, then one and two.
        for (step, rt) in [(0usize, 100u64), (1, 1_500), (2, 2_500)] {
            let want = oracle
                .render(&ir, &style, (1280, 720), None, step)
                .expect("the karaoke cue rasterizes");
            engine.overlays_for(Some(ms(rt)));
            assert!(
                wait_for(|| engine
                    .current_overlays()
                    .first()
                    .is_some_and(|overlay| *overlay.pixels == want.pixels)),
                "step {step}: the engine painted something other than the raster this \
                 step used to produce"
            );
            let overlay = engine.current_overlays().remove(0);
            assert_eq!(
                (overlay.width, overlay.height, overlay.x),
                (want.width, want.height, want.x),
                "step {step}: the surface moved"
            );
        }
    }

    // ---- the default readability box ----

    /// A pixel of a paint, as straight RGBA.
    fn px_at(raster: &crate::cue_scene::RasterOut, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * raster.width + x) * 4) as usize;
        raster.pixels[at..at + 4].try_into().expect("in bounds")
    }

    /// The cue-IR form of the same plain text.
    fn ir_format(text: &str) -> SubtitleTextFormat {
        SubtitleTextFormat::CueIr {
            ir: Arc::new(cue_ir::CueIr::from_plain_text(text)),
            pts_start: None,
        }
    }

    /// THE DEFAULT LOOK: both arms draw a tinted rounded box behind the cue,
    /// and they draw it in the SAME PLACE with the SAME TINT.
    ///
    /// The sample points are chosen from the geometry both rasterizers share:
    /// the box is inset one pixel from the raster edge (padding is
    /// `max(outline, box_pad) + 1`), so a pixel a few columns in at mid-height
    /// is inside the box and still well left of the glyph ink, while the very
    /// corner is outside the corner radius and must stay clear.
    #[test]
    fn the_default_style_draws_a_readability_box_on_both_arms() {
        let mut ctx = RasterCtx::new();
        let canvas = (640, 360);

        for (arm, format) in [
            ("pango", SubtitleTextFormat::Utf8),
            ("cue-ir", ir_format("Hello, subtitles")),
        ] {
            let raster = render_with(&mut ctx, "Hello, subtitles", format, canvas, &CueStyle::default())
                .unwrap_or_else(|| panic!("{arm}: rasterizes"));
            let (_, h) = (raster.width, raster.height);

            // Inside the box, outside the ink: the tint, at its own alpha.
            let inside = px_at(&raster, 3, h / 2);
            assert!(
                inside[3] > 120 && inside[3] < 200,
                "{arm}: expected the box tint inside the box, got {inside:?}"
            );
            assert!(
                inside[0] < 40 && inside[1] < 40 && inside[2] < 40,
                "{arm}: the box must be black, got {inside:?}"
            );

            // Outside the rounded corner: nothing at all.
            let corner = px_at(&raster, 0, 0);
            assert_eq!(
                corner[3], 0,
                "{arm}: the rounded corner must stay transparent, got {corner:?}"
            );

            // The glyphs still read white on top of it.
            let white = raster
                .pixels
                .chunks_exact(4)
                .filter(|px| px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200)
                .count();
            assert!(white > 50, "{arm}: expected white glyph fill, got {white}");
        }
    }

    /// The box is governed by ONE struct across both arms, so turning it off
    /// through `set_style`'s `CueStyle` turns it off everywhere -- and turns
    /// the raster back into the bare-outline cue this renderer used to make.
    #[test]
    fn the_box_can_be_turned_off_through_the_house_style() {
        let mut ctx = RasterCtx::new();
        let canvas = (640, 360);

        for (arm, format) in [
            ("pango", SubtitleTextFormat::Utf8),
            ("cue-ir", ir_format("Hello, subtitles")),
        ] {
            let boxed = render_with(&mut ctx, "Hello, subtitles", format.clone(), canvas, &CueStyle::default())
                .unwrap_or_else(|| panic!("{arm}: rasterizes"));
            let bare = render_with(&mut ctx, "Hello, subtitles", format, canvas, &CueStyle::outline_only())
                .unwrap_or_else(|| panic!("{arm}: rasterizes"));

            // Nothing is painted where the box was.
            let (_, h) = (bare.width, bare.height);
            assert_eq!(
                px_at(&bare, 3, h / 2)[3],
                0,
                "{arm}: no box means nothing behind the text"
            );
            // ...and the cue is physically smaller without the box padding.
            assert!(
                bare.width < boxed.width && bare.height < boxed.height,
                "{arm}: the box adds padding, so {:?} must be smaller than {:?}",
                (bare.width, bare.height),
                (boxed.width, boxed.height)
            );
        }
    }

    /// A cue that carries its OWN background (a WebVTT `::cue { background }`,
    /// an SSA `BorderStyle=3`) keeps that colour: the house tint is a default,
    /// not an override. The house GEOMETRY still applies, which is what keeps
    /// such a cue looking like the rest of them.
    #[test]
    fn a_cue_with_its_own_background_does_not_get_the_house_tint() {
        let mut ctx = RasterCtx::new();
        let canvas = (640, 360);

        let mut ir = cue_ir::CueIr::from_plain_text("Hello, subtitles");
        ir.layout.background = Some(cue_ir::ir::Color::rgb(0, 0, 255));
        let raster = render_with(&mut ctx, "Hello, subtitles", SubtitleTextFormat::CueIr { ir: Arc::new(ir), pts_start: None, }, canvas, &CueStyle::default())
            .expect("rasterizes");

        let (_, h) = (raster.width, raster.height);
        let inside = px_at(&raster, 3, h / 2);
        assert!(
            inside[2] > 200 && inside[0] < 60 && inside[1] < 60,
            "the cue's own blue background must win over the house black, got {inside:?}"
        );
        assert_eq!(inside[3], 255, "an opaque cue background stays opaque");
    }

    /// SPAN colours are not a cue background: text that merely happens to be
    /// coloured still gets the house box behind it.
    #[test]
    fn coloured_text_still_gets_the_house_box() {
        let mut ctx = RasterCtx::new();
        let canvas = (640, 360);

        let mut ir = cue_ir::CueIr::from_plain_text("Hello, subtitles");
        ir.base.foreground = Some(cue_ir::ir::Color::rgb(255, 0, 0));
        let raster = render_with(&mut ctx, "Hello, subtitles", SubtitleTextFormat::CueIr { ir: Arc::new(ir), pts_start: None, }, canvas, &CueStyle::default())
            .expect("rasterizes");

        let (_, h) = (raster.width, raster.height);
        let inside = px_at(&raster, 3, h / 2);
        assert!(
            inside[3] > 120 && inside[3] < 200 && inside[0] < 40,
            "a coloured-text cue still gets the black house box, got {inside:?}"
        );
        let red = raster
            .pixels
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] > 180 && px[1] < 60 && px[2] < 60)
            .count();
        assert!(red > 50, "the text is still red, got {red} px");
    }

    #[test]
    fn a_larger_canvas_rasters_larger_text() {
        let mut ctx = RasterCtx::new();
        let small = render_with(&mut ctx, "Same text", SubtitleTextFormat::Utf8, (640, 360), &CueStyle::default())
            .expect("rasterizes");
        let large = render_with(&mut ctx, "Same text", SubtitleTextFormat::Utf8, (1920, 1080), &CueStyle::default())
            .expect("rasterizes");

        assert!(large.width > small.width);
        assert!(large.height > small.height);
    }

    // ---- the engine driving the worker ----

    #[test]
    fn a_submitted_cue_reaches_the_screen_as_a_window_space_overlay() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("On screen", 0, 1000));

        // The first frame may render bare: the raster is in flight, never waited on.
        engine.overlays_for(Some(ms(10)));
        assert!(wait_for(|| !engine.current_overlays().is_empty()));

        let overlays = engine.current_overlays();
        assert_eq!(overlays.len(), 1);
        let overlay = &overlays[0];
        assert_eq!(overlay.space, OverlaySpace::Window);
        assert_eq!(overlay.render_width, overlay.width);
        assert_eq!(overlay.render_height, overlay.height);
        assert_eq!(
            overlay.pixels.len(),
            overlay.width as usize * overlay.height as usize * 4
        );

        // Expiry takes it away again.
        assert!(engine.overlays_for(Some(ms(1000))).is_empty());
    }

    /// The second cue in a stack gets its raster WITHOUT anything asking for
    /// the overlay set again, which, while paused, nothing does.
    ///
    /// The worker inbox is a single newest-wins slot, so one pass over the
    /// active set hands over one key; the rest of the stack arrives because the
    /// worker asks for the next one after every publish. A test that polled
    /// `current_overlays()` would perform that resolution itself and pass with
    /// the pump deleted (the B7 lesson), so this one polls the raster CACHE,
    /// which no scheduling path runs through.
    #[test]
    fn a_second_cue_rasters_without_a_frame_to_ask_for_it() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("The bottom line", 0, 8_000));
        engine.submit(cue("The line above it", 1_000, 8_000));

        // ONE frame, and then nothing ever again: this is the paused viewer.
        engine.overlays_for(Some(ms(2_000)));

        assert!(
            wait_for(|| engine.cached_rasters() == 2),
            "only {} of the two cues on screen was ever rastered; the rest of the stack waits for \
             a frame that never comes while paused",
            engine.cached_rasters()
        );
    }

    /// CONTIGUOUS CUES NEVER BLANK BETWEEN.
    ///
    /// A telemetry-style embedded track (mp4/tx3g out of qtdemux) has cues that
    /// ABUT: pts 1s/2s/3s… each with a duration of exactly 1s, so
    /// `end(N) == start(N+1)` to the nanosecond and there is no gap for a frame
    /// to fall into. The predicates handle that seam correctly (one `evaluate`
    /// expires N and adopts N+1) and the cue was already in `pending` a full
    /// second early, because the text branch is unsynced and runs ~1.2s ahead
    /// of the clock.
    ///
    /// The blank came from the RASTER, not the schedule: the cue was adopted
    /// `Pending`, `active_overlays` skips anything that is not Ready or Stale,
    /// and the boundary frame therefore carried nothing at all until the worker
    /// published one frame later. Exactly one blank frame, at every boundary,
    /// which on a continuous track is a visible flash.
    ///
    /// So this test asserts on PIXELS across the seam, not on the active set:
    /// reading `active` would have passed throughout.
    #[test]
    fn contiguous_cues_hand_over_without_a_blank_frame() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        // Three cues, two seams, since "every boundary" is the claim.
        engine.submit(cue("ALT 121m", 1_000, 1_000));
        engine.submit(cue("ALT 124m", 2_000, 1_000));
        engine.submit(cue("ALT 127m", 3_000, 1_000));

        // 29.97 fps, the file's rate: 1001/30000 s per frame.
        let frame_rt =
            |frame: u64| gst::ClockTime::from_nseconds(frame * 1_001_000_000_000 / 30_000);

        // Frame 30 is the first inside cue one. Its raster is in flight, so this
        // frame may legitimately render bare. The FIRST cue of a run has
        // nothing before it to hide behind, and no policy can raster it before
        // it is known.
        engine.overlays_for(Some(frame_rt(30)));
        assert!(
            wait_for(|| !engine.current_overlays().is_empty()),
            "the first cue never rastered"
        );

        // Now walk forward, one frame at a time, exactly as the sink does, and
        // never jump: crossing a seam is the whole question, and a walk that
        // skipped ahead would raster the next cue early by accident.
        //
        // ONE evaluation per frame, and the assert is on THAT answer. A frame is
        // drawn once. A retry loop here would let the worker publish and then
        // pass on the second look, which is exactly the one-frame blank under
        // test and would make this test blind to it.
        //
        // The sleep is the real 33.4 ms frame period. It is what gives the
        // prefetch its chance, and the player has ~30 of them between one cue's
        // start and the next; the raster itself costs about a millisecond.
        // Without the prefetch there is nothing in flight when the seam arrives
        // and no amount of frame period helps.
        for frame in 31..=95u64 {
            std::thread::sleep(Duration::from_nanos(1_001_000_000_000 / 30_000));
            let rt = frame_rt(frame);
            assert!(
                !engine.overlays_for(Some(rt)).is_empty(),
                "frame {frame} (rt {rt}) carried no overlay: a contiguous cue boundary blanked the \
                 screen for exactly one frame, which is a visible flash"
            );
        }
    }

    /// The mechanism behind the test above, pinned on its own: while a cue is
    /// on screen, the NEXT one's raster is already in the cache. Asserted
    /// against the cache rather than the overlays because no scheduling
    /// path runs through it, so this cannot pass on a lucky repaint.
    #[test]
    fn the_next_cue_is_rastered_before_its_turn_comes() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Showing now", 0, 1_000));
        engine.submit(cue("Up next", 1_000, 1_000));

        // ONE frame, well inside the first cue and nowhere near the second's
        // start. Only the first is active; the second is still pending.
        engine.overlays_for(Some(ms(100)));

        assert!(
            wait_for(|| engine.cached_rasters() == 2),
            "only {} raster(s) cached: the cue that is up next was not warmed while there was an \
             idle worker to warm it, so its first frame will be blank",
            engine.cached_rasters()
        );
        assert_eq!(
            showing_all(&engine),
            vec!["Showing now".to_owned()],
            "warming the next cue must not put it on screen early"
        );
    }

    /// THE STACK, IN PIXELS: two overlapping cues reach the screen as two
    /// overlays at two heights, neither covers the other, and they read
    /// top-to-bottom in start order.
    ///
    /// Both rasters are laid out bottom-centre by the house policy, so they ask
    /// for the SAME strip; what separates them is `active_overlays`, and the
    /// separation has to be visible in the numbers a compositor uploads rather
    /// than in engine state.
    ///
    /// The cue pair is a real one, a converted stream splitting one sentence
    /// across two cues 41 ms apart. The seniority rule kept the earlier cue at
    /// the bottom and the sentence read "something to live for. / It helps if
    /// they have"; the later start must take the bottom slot.
    #[test]
    fn two_cues_on_screen_are_two_overlays_at_two_heights() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("It helps if they have", 0, 8_000));
        engine.submit(cue("something to live for.", 41, 2_000));

        engine.overlays_for(Some(ms(1_000)));
        assert!(
            wait_for(|| engine.current_overlays().len() == 2),
            "two cues cover this frame and {} overlay(s) reached the screen",
            engine.current_overlays().len()
        );

        let overlays = engine.current_overlays();
        let (first, second) = (&overlays[0], &overlays[1]);
        assert!(
            overlays.iter().all(|o| o.space == OverlaySpace::Window),
            "text cues are laid out at display resolution and stay in window space"
        );
        assert!(
            second.y > first.y,
            "the later-starting cue must be the LOWER one: first at y={}, second at y={}",
            first.y,
            second.y
        );
        assert!(
            first.y + first.height as i32 <= second.y,
            "the two cues overlap vertically: first spans {}..{}, second starts at {}",
            first.y,
            first.y + first.height as i32,
            second.y
        );
        assert!(
            second.y + second.height as i32 <= 720,
            "the bottom cue hangs off the canvas"
        );
        // Both are real pictures, not empty strips.
        for overlay in overlays.iter() {
            assert!(
                overlay.pixels.iter().skip(3).step_by(4).any(|a| *a > 0),
                "an overlay carries no ink at all"
            );
        }

        // The one at the BOTTOM ends first. THE OTHER STAYS -- under the
        // single-active rule the second cue's arrival ended the first, so this
        // frame showed nothing at all -- without re-rastering, in the same
        // allocation, and it drops back into the freed bottom slot.
        let first_pixels = first.pixels.clone();
        let first_y = first.y;
        engine.overlays_for(Some(ms(4_000)));
        let after = engine.current_overlays();
        assert_eq!(
            after.len(),
            1,
            "the surviving cue went with the expired one"
        );
        assert!(
            Arc::ptr_eq(&after[0].pixels, &first_pixels),
            "the surviving cue was re-rastered rather than left alone"
        );
        assert!(
            after[0].y > first_y,
            "the surviving cue did not reclaim the bottom slot: y={} was {}",
            after[0].y,
            first_y
        );
    }

    #[test]
    fn a_re_shown_cue_comes_back_from_the_cache_without_the_worker() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Cached", 0, 1000));
        engine.overlays_for(Some(ms(10)));
        assert!(wait_for(|| !engine.current_overlays().is_empty()));
        assert_eq!(engine.cached_rasters(), 1);

        engine.clear();
        assert!(engine.current_overlays().is_empty());

        // Same text, same canvas: served synchronously from the cache, so it is
        // on screen the instant the frame is evaluated.
        engine.submit(cue("Cached", 2000, 1000));
        assert!(
            !engine.overlays_for(Some(ms(2000))).is_empty(),
            "a cached re-show must not wait for the worker"
        );
    }

    #[test]
    fn a_canvas_change_re_rasters_the_active_cue() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("Resize me", 0, 10_000));
        engine.overlays_for(Some(ms(10)));
        assert!(wait_for(|| !engine.current_overlays().is_empty()));
        let small = engine.current_overlays()[0].width;

        engine.set_canvas(1920, 1080);
        assert!(wait_for(|| engine
            .current_overlays()
            .first()
            .is_some_and(|overlay| overlay.width != small)));

        let large = engine.current_overlays()[0].width;
        assert!(large > small, "{large} should exceed {small}");
        assert_eq!(engine.cached_rasters(), 2, "both canvas sizes are cached");
    }

    // ---- the paused switch and the latency gate ----

    /// The renderer's half. A cue that covers the FROZEN frame becomes a
    /// non-empty overlay set with no new frame anywhere.
    ///
    /// Its other half is `flapjack`'s
    /// `sink_subtitles::a_paused_embedded_switch_shows_its_cue_without_resuming`,
    /// which pins that a paused track switch really delivers such a cue,
    /// covering the frame the sink is showing, through the whole transport.
    /// The two are split because the driver crate cannot depend on this one,
    /// and they are joined by the quantity both sides use: the cue's
    /// `[start_rt, end_rt)` against the frame's running time. Here that frame
    /// is the last one `overlays_for` was given, exactly as while paused, and
    /// the cue arrives after it, the order the transport produces.
    ///
    /// `current_overlays`, not `overlays_for`: the paused path reads the
    /// engine from the event loop WITHOUT advancing it, because advancing it
    /// would need a frame it does not have.
    #[test]
    fn a_paused_cue_covering_the_frozen_frame_reaches_the_screen() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        let changed = Arc::new(AtomicU64::new(0));
        let counter = changed.clone();
        engine.set_on_change(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        // A frame goes by with no cue selected, and then playback stops. This
        // is the receiver at a settled PAUSED with subtitles off, or on a
        // track whose cue has expired.
        engine.overlays_for(Some(ms(4_100)));
        assert!(engine.current_overlays().is_empty());

        // The switch: the driver's refresh seek re-emits the incoming track's
        // cue covering where the item already is, and it arrives at the
        // consumer, which submits it. No frame follows.
        engine.submit(cue("Incoming", 4_000, 500));
        assert!(
            wait_for(|| !engine.current_overlays().is_empty()),
            "a cue covering the frozen frame never reached the screen: the paused \
             switch renders nothing until playback resumes, which is the contract \
             the consumer transport exists to replace"
        );
        assert!(
            changed.load(Ordering::Relaxed) >= 1,
            "the renderer was never told to repaint, so a paused viewer would keep \
             looking at the old frame however ready the overlay is"
        );

        // And it is the right one, laid out for this canvas.
        let overlays = engine.current_overlays();
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].space, OverlaySpace::Window);
        assert_eq!(showing(&engine).as_deref(), Some("Incoming"));

        // A cue that does NOT cover the frozen frame changes nothing on
        // screen, which is what makes the assertion above about COVERING
        // rather than about arrival.
        engine.clear();
        engine.submit(cue("Later", 9_000, 500));
        assert!(
            engine.current_overlays().is_empty(),
            "a cue starting after the frozen frame was put on screen anyway"
        );
    }

    // ---- the paused gap tolerance (`PAUSED_CUE_LOOKAHEAD`) ----
    //
    // THE SHAPE, shared by every test below: a caption converter leaves a 70 ms
    // hole between one cue's end (20.900) and the next one's start (20.970), and
    // the viewer pauses inside it at 20.930, 40 ms short of a cue that no frame
    // will ever arrive to bring in.

    /// A frozen frame in the hole shows the cue it is about to reach.
    ///
    /// Both sides of the tolerance in one test, because the number is the whole
    /// policy: 40 ms ahead is shown, 300 ms ahead is not. The far cue is what
    /// keeps this about NEARNESS rather than about "paused shows anything
    /// pending": the same read, the same frozen frame, opposite answers.
    #[test]
    fn a_paused_frame_in_a_gap_shows_a_cue_just_ahead_of_it() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Before", 20_000, 900));

        // The frame that lands in the hole. `overlays_for` is the exact path,
        // so it leaves the screen blank: "Before" expired at 20.900.
        engine.overlays_for(Some(ms(20_930)));
        assert_eq!(
            showing(&engine),
            None,
            "the frame path is exact and nothing covers 20.930"
        );

        // 40 ms ahead: inside the tolerance.
        engine.submit(cue("After", 20_970, 2_000));
        assert_eq!(
            showing_paused(&engine),
            vec!["After"],
            "a paused frame 40 ms short of the next cue stayed blank, which is the \
             correct-but-useless answer this tolerance exists to replace"
        );

        // 300 ms ahead: outside it. A fresh engine, frozen at the same instant.
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Before", 20_000, 900));
        engine.overlays_for(Some(ms(20_930)));
        engine.submit(cue("Far", 21_230, 2_000));
        assert!(
            showing_paused(&engine).is_empty(),
            "a cue 300 ms away was pulled onto the frozen frame; the tolerance is a \
             gap filler, not a licence to show whatever is pending"
        );
    }

    /// While frames flow the schedule is EXACT: the same hole stays blank, and
    /// the cue appears at the running time its file gives it.
    ///
    /// This is the half that keeps the policy from being visible during
    /// playback. `overlays_for` runs per frame, so a tolerance applied here
    /// would move every cue boundary in the file up to 200 ms early against the
    /// audio.
    #[test]
    fn while_frames_flow_the_gap_stays_blank() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Before", 20_000, 900));
        engine.submit(cue("After", 20_970, 2_000));

        assert_eq!(advance(&engine, ms(20_500)), Some("Before".to_owned()));
        // Three frames' worth of the hole, at 30 fps.
        for rt in [20_910, 20_930, 20_960] {
            assert_eq!(
                advance(&engine, ms(rt)),
                None,
                "the frame at {rt} ms showed the next cue early; while playing the \
                 schedule is exact"
            );
        }
        // And it arrives on time, by itself.
        assert_eq!(advance(&engine, ms(20_970)), Some("After".to_owned()));
    }

    /// The repaint half: a cue for the far side of the hole arriving while the
    /// clock is stopped fires the change signal, with no frame anywhere.
    ///
    /// The twin above pins this for a cue that COVERS the frozen frame. The
    /// tolerance extends it to one that does not cover it yet, and the signal
    /// matters just as much: an overlay nothing repaints for is an overlay the
    /// paused viewer never sees.
    #[test]
    fn a_cue_arriving_just_ahead_of_a_frozen_frame_fires_the_repaint() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        let changed = Arc::new(AtomicU64::new(0));
        let counter = changed.clone();
        engine.set_on_change(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        engine.submit(cue("Before", 20_000, 900));
        engine.overlays_for(Some(ms(20_930)));
        assert!(engine.current_overlays().is_empty());
        let before = changed.load(Ordering::Relaxed);

        // The delivery, after the frame rather than before it: this is the
        // publish-side paused path, not the render-side one.
        engine.submit(cue("After", 20_970, 2_000));
        assert!(
            wait_for(|| !engine.current_overlays().is_empty()),
            "the cue on the far side of the hole never reached the screen"
        );
        assert!(
            changed.load(Ordering::Relaxed) > before,
            "the renderer was never told to repaint, so the paused viewer keeps \
             looking at a blank frame however ready the overlay is"
        );
        assert_eq!(showing(&engine).as_deref(), Some("After"));
    }

    /// The composition rule with the multi-active set: the tolerance fires ONLY
    /// when the active set at the frozen frame is EMPTY.
    ///
    /// A frame that already carries a cue is not the defect, however close the
    /// next one is, and pulling one in beside it would invent a two-line
    /// screen out of two one-line cues the file wrote in sequence.
    #[test]
    fn a_covered_frozen_frame_does_not_pull_the_next_cue_in_beside_it() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        // "Covering" runs through the frozen frame; "Next" starts 100 ms later,
        // well inside the tolerance.
        engine.submit(cue("Covering", 20_000, 1_000));
        engine.submit(cue("Next", 21_050, 1_000));

        engine.overlays_for(Some(ms(20_950)));
        assert_eq!(
            showing_paused(&engine),
            vec!["Covering"],
            "the frozen frame was already covered, so nothing may be added to it"
        );
    }

    /// An entry that occupies no time is stepped over, not adopted.
    ///
    /// The converters that leave the hole also emit zero-length records
    /// (`start == end`). [`merge_delivery`] folds the ones that repeat a real
    /// cue (hence the different text here, which is what keeps this one
    /// un-folded and in the list) and this is the backstop for the rest. It is
    /// load-bearing: a zero-length entry is not `cue_is_too_old` at a frame
    /// BEFORE its start, so adopting one would park an empty window on screen
    /// and, the active set no longer being empty, block the real cue behind it
    /// for as long as the viewer stayed paused.
    #[test]
    fn the_gap_tolerance_steps_over_an_entry_that_occupies_no_time() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Before", 20_000, 900));
        engine.submit(cue("TWIN", 20_970, 0));
        engine.submit(cue("REAL", 20_970, 2_000));

        engine.overlays_for(Some(ms(20_930)));
        assert_eq!(
            showing_paused(&engine),
            vec!["REAL"],
            "the zero-length twin took the screen the real cue was owed"
        );
    }

    /// Resuming is seamless: the early cue is genuinely scheduled, so frames
    /// starting to flow again find it already active and leave it alone.
    ///
    /// This is why the tolerance POPS the cue rather than peeking at it. A
    /// peeked cue would vanish from the first frame after the resume and come
    /// back at its real start, a blink at exactly the moment the viewer is
    /// looking.
    #[test]
    fn the_early_cue_survives_the_resume_without_blinking() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("Before", 20_000, 900));
        engine.submit(cue("After", 20_970, 2_000));

        engine.overlays_for(Some(ms(20_930)));
        assert_eq!(showing_paused(&engine), vec!["After"]);

        // Frames flow again, through the rest of the hole and past the cue's
        // real start. It never leaves.
        for rt in [20_940, 20_960, 20_970, 21_500] {
            assert_eq!(
                advance(&engine, ms(rt)),
                Some("After".to_owned()),
                "the early cue blinked out at {rt} ms"
            );
        }
        // And it still ends on time, at its own end and not a moment later.
        assert_eq!(advance(&engine, ms(22_970)), None);
    }

    /// The raster half: submit to pixels stays inside the gate once warm.
    ///
    /// p99 over distinct texts, because identical ones are served from the
    /// cache without reaching the worker at all (that path is
    /// `a_re_shown_cue_comes_back_from_the_cache_without_the_worker`) and a
    /// distribution full of cache hits would measure the wrong thing. Warm:
    /// the first raster pays fontconfig, which is a machine property measured
    /// separately in `the_fontmap_warm_up_is_measured`, so it is excluded here
    /// the way the plan words the gate.
    ///
    /// The integration half is `flapjack`'s
    /// `sink_subtitles::a_delivered_cue_covers_a_frame_within_the_cue_bound`.
    #[test]
    fn raster_latency_stays_under_the_gate_when_warm() {
        const GATE: Duration = Duration::from_millis(50);
        const SAMPLES: u64 = 100;

        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        // Warm first, and prove it: an unwarmed run measures fontconfig.
        engine.warm();
        assert!(wait_for(|| engine.warm_up_time().is_some()));

        for index in 0..SAMPLES {
            // Distinct text per sample, and each cue covers the frame that
            // follows it, so every one becomes ACTIVE and is really requested.
            engine.submit(cue(&format!("latency sample {index}"), index * 1_000, 500));
            engine.overlays_for(Some(ms(index * 1_000 + 10)));
            assert!(
                wait_for(|| !engine.current_overlays().is_empty()),
                "sample {index} never rasterized"
            );
        }

        let mut costs = engine.raster_latencies();
        assert!(
            costs.len() as u64 >= SAMPLES,
            "only {} of {SAMPLES} rasters reached the worker, so this measured the \
             cache and not the rasterizer",
            costs.len()
        );
        costs.sort();
        // The p99 of the warm distribution: index 99 of 100 samples is the
        // slowest one this test allows to be an outlier.
        let p99 = costs[(costs.len() * 99).div_ceil(100).saturating_sub(1)];
        println!(
            "cue raster: p50 {:?} p99 {p99:?} max {:?} over {} samples",
            costs[costs.len() / 2],
            costs[costs.len() - 1],
            costs.len()
        );
        assert!(
            p99 < GATE,
            "warm raster p99 is {p99:?}, over the {GATE:?} gate: a cue would miss the \
             frame it belongs on"
        );
    }

    /// The fontconfig/fontmap first-use cost. Logged rather than bounded, since
    /// it is a machine property.
    #[test]
    fn the_fontmap_warm_up_is_measured() {
        let engine = CueEngine::new();
        engine.warm();
        assert!(wait_for(|| engine.warm_up_time().is_some()));
        let elapsed = engine.warm_up_time().unwrap();
        println!("slint-cue fontmap warm-up: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(30));
    }

    // ---- the overlay pixels are SHARED, not copied ----

    /// A wall of text, so the raster is a real full-screen-sized allocation
    /// rather than a one-line strip: the copy this pins the absence of is
    /// proportional to the pixels, and a small raster would not show it.
    fn wall_of_text() -> String {
        (0..40)
            .map(|line| {
                format!(
                    "line {line}: the quick brown fox jumps over the lazy dog, and keeps jumping \
                     until the line wraps"
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn big_raster_engine() -> CueEngine {
        let engine = CueEngine::new();
        engine.set_canvas(1920, 1080);
        engine.submit(CueInput {
            format: SubtitleTextFormat::Utf8,
            text: wall_of_text(),
            start_rt: gst::ClockTime::ZERO,
            end_rt: Some(ms(60_000)),
        });
        engine.overlays_for(Some(ms(10)));
        assert!(
            wait_for(|| !engine.current_overlays().is_empty()),
            "the wall of text never rasterized"
        );
        engine
    }

    /// The STRUCTURAL half of the Arc-pixels contract: the bytes an
    /// overlay carries are the engine's own raster, by pointer, on every call.
    ///
    /// This is the assertion that cannot rot. A timing bound can be widened
    /// until it passes on a slow machine. `ptr_eq` either holds or the pixels
    /// were copied. Bitmap subtitles need it too: a page is the same order of
    /// magnitude as this raster and is composited the same way, so a per-frame
    /// clone at 60 Hz is megabytes a frame of pure memcpy.
    ///
    /// It reads the PIXEL LANE now rather than the active cue, because that is
    /// where the bytes live since the engine's own artifact became the scene.
    /// The claim is unchanged: what the overlay carries is the allocation the
    /// engine already had, not a copy made for the caller.
    #[test]
    fn overlay_pixels_are_the_engines_own_buffer_on_every_call() {
        let engine = big_raster_engine();

        let engine_pixels = {
            let state = engine.shared.state.lock();
            let active = state.sched.active().first().expect("a cue is active");
            active
                .payload
                .painted()
                .expect("the cue on screen has been painted")
                .pixels
                .clone()
        };

        let first = engine.overlays_for(Some(ms(20)));
        let second = engine.overlays_for(Some(ms(30)));
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(
            Arc::ptr_eq(&first[0].pixels, &engine_pixels),
            "the overlay carries a COPY of the engine's raster, not the raster"
        );
        assert!(
            Arc::ptr_eq(&first[0].pixels, &second[0].pixels),
            "two consecutive frames were handed two different allocations of the same cue"
        );
    }

    /// The MEASURED half: reading the overlay set is cheap enough to do
    /// per displayed frame.
    ///
    /// A reintroduced copy of a raster this size costs ~1-2 ms per call, so the
    /// 100 µs bound has ten to twenty times the headroom it needs against a
    /// loaded machine. It fails on a memcpy and on nothing else.
    #[test]
    fn reading_the_overlay_set_does_not_cost_a_copy() {
        const CALLS: u32 = 100;
        const BOUND: Duration = Duration::from_micros(100);

        let engine = big_raster_engine();
        let bytes = engine.current_overlays()[0].pixels.len();
        assert!(
            bytes >= 4 * 1024 * 1024,
            "the raster is only {bytes} bytes; too small for this bound to mean anything"
        );

        let started = Instant::now();
        let mut seen = 0usize;
        for _ in 0..CALLS {
            seen += engine.current_overlays().len();
        }
        let mean = started.elapsed() / CALLS;
        assert_eq!(seen, CALLS as usize, "the cue stopped showing mid-measure");
        println!(
            "current_overlays: mean {mean:?} over {CALLS} calls against a {} MiB raster",
            bytes / (1024 * 1024)
        );
        assert!(
            mean < BOUND,
            "current_overlays costs {mean:?} per call against a {} MiB raster, over the {BOUND:?} \
             bound: something on the path is copying the pixels again",
            bytes / (1024 * 1024)
        );
    }

    // ---- the bitmap side ----

    use crate::subpic::BitmapRegion;

    /// The decoder every bitmap test drives, and the reason there is one: step
    /// 2 lands the engine's bitmap machinery with NO format decoder behind it,
    /// so what is under test is what the ENGINE does with a display set, never
    /// how PGS bytes become one.
    ///
    /// The default decode rule, chosen so the assertions read as themselves: a
    /// packet's FIRST byte is its TAG, and it decodes to one region whose every
    /// pixel byte is that tag. A tag of 0 decodes to a scheduled clear (an
    /// update with no regions). The update runs from the packet's `rt` to
    /// `rt + duration`, and is open-ended when the packet has no duration.
    type DecodeRule = dyn Fn(&BitmapPacket) -> Vec<DisplayUpdate> + Send + Sync;

    #[derive(Clone)]
    struct DecoderRig {
        decode: Arc<DecodeRule>,
        pushed: Arc<Mutex<Vec<gst::Buffer>>>,
        codec_data: Arc<Mutex<Vec<Vec<u8>>>>,
        sizes: Arc<Mutex<Vec<(u32, u32)>>>,
        resets: Arc<AtomicU64>,
        builds: Arc<AtomicU64>,
    }

    impl Default for DecoderRig {
        fn default() -> Self {
            Self {
                decode: Arc::new(default_decode),
                pushed: Arc::default(),
                codec_data: Arc::default(),
                sizes: Arc::default(),
                resets: Arc::default(),
                builds: Arc::default(),
            }
        }
    }

    impl DecoderRig {
        fn with_decode(
            decode: impl Fn(&BitmapPacket) -> Vec<DisplayUpdate> + Send + Sync + 'static,
        ) -> Self {
            Self {
                decode: Arc::new(decode),
                ..Self::default()
            }
        }

        fn install(&self, engine: &CueEngine) {
            let rig = self.clone();
            engine.set_decoder_factory(move |_format| {
                rig.builds.fetch_add(1, Ordering::Relaxed);
                let decoder: Box<dyn SubpicDecoder> = Box::new(RigDecoder { rig: rig.clone() });
                Some(decoder)
            });
        }

        fn pushes(&self) -> usize {
            self.pushed.lock().len()
        }

        fn pushed_tags(&self) -> Vec<u8> {
            self.pushed
                .lock()
                .iter()
                .map(|buffer| {
                    buffer
                        .map_readable()
                        .expect("test buffers map")
                        .first()
                        .copied()
                        .unwrap_or(0)
                })
                .collect()
        }
    }

    struct RigDecoder {
        rig: DecoderRig,
    }

    impl SubpicDecoder for RigDecoder {
        fn set_codec_data(&mut self, data: &[u8]) {
            self.rig.codec_data.lock().push(data.to_vec());
        }

        fn set_video_size(&mut self, width: u32, height: u32) {
            self.rig.sizes.lock().push((width, height));
        }

        fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate> {
            self.rig.pushed.lock().push(packet.data.clone());
            (self.rig.decode)(packet)
        }

        fn reset(&mut self) {
            self.rig.resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many times the "no decoder for this format" warning has been
    /// printed, process-wide.
    ///
    /// A rate limit is only provable by counting the LINES, so this is a
    /// `tracing` subscriber rather than an added counter in production code:
    /// nothing about the engine changes to make the test possible. It is
    /// installed globally (a warn raised on the decode worker cannot be seen by
    /// a thread-local dispatcher) and counts exactly one message, so it can sit
    /// under every other test in the binary harmlessly.
    fn install_warn_counter() -> Arc<AtomicU64> {
        const NEEDLE: &str = "no decoder for this bitmap subtitle format";
        static HITS: std::sync::LazyLock<Arc<AtomicU64>> =
            std::sync::LazyLock::new(|| Arc::new(AtomicU64::new(0)));
        static INSTALL: std::sync::Once = std::sync::Once::new();

        struct Counter(Arc<AtomicU64>);
        impl tracing::Subscriber for Counter {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Find(bool);
                impl tracing::field::Visit for Find {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "message" && format!("{value:?}").contains(NEEDLE) {
                            self.0 = true;
                        }
                    }
                }
                let mut find = Find(false);
                event.record(&mut find);
                if find.0 {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        INSTALL.call_once(|| {
            // An `Err` means something else owns the global dispatcher; the
            // test that needs the count would then fail loudly on its own
            // assertion rather than silently measuring nothing.
            let _ = tracing::subscriber::set_global_default(Counter(HITS.clone()));
        });
        HITS.clone()
    }

    fn tag_of(packet: &BitmapPacket) -> u8 {
        packet
            .data
            .map_readable()
            .ok()
            .and_then(|map| map.first().copied())
            .unwrap_or(0)
    }

    fn bitmap_region(tag: u8, x: i32) -> BitmapRegion {
        BitmapRegion {
            pixels: Arc::new(vec![tag; 4 * 4 * 4]),
            width: 4,
            height: 4,
            x,
            y: 0,
            render_width: 4,
            render_height: 4,
        }
    }

    fn bitmap_set(start: gst::ClockTime, end: Option<gst::ClockTime>, tag: u8) -> DisplayUpdate {
        DisplayUpdate {
            start_rt: start,
            end_rt: end,
            regions: vec![bitmap_region(tag, 0)],
        }
    }

    fn default_decode(packet: &BitmapPacket) -> Vec<DisplayUpdate> {
        let tag = tag_of(packet);
        vec![DisplayUpdate {
            start_rt: packet.rt,
            end_rt: packet.duration.map(|duration| packet.rt + duration),
            regions: if tag == 0 {
                Vec::new()
            } else {
                vec![bitmap_region(tag, 0)]
            },
        }]
    }

    /// One packet, with a distinct buffer every call. The duplicate check is on
    /// buffer IDENTITY, so a test that wants a duplicate has to clone the
    /// buffer deliberately.
    fn bitmap_packet(tag: u8, rt: u64, duration: Option<u64>) -> BitmapPacket {
        bitmap_packet_of(BitmapSubFormat::Pgs, tag, rt, duration)
    }

    /// The same, for a NAMED format. Every test in this file installs its own
    /// decoder, so the format is only ever a routing key here, except in the
    /// one test that deliberately does not install anything and needs a
    /// format the production table still answers `None` for.
    fn bitmap_packet_of(
        format: BitmapSubFormat,
        tag: u8,
        rt: u64,
        duration: Option<u64>,
    ) -> BitmapPacket {
        BitmapPacket {
            format,
            data: gst::Buffer::from_slice(vec![tag, 0xAA, 0xBB]),
            codec_data: None,
            rt: ms(rt),
            duration: duration.map(ms),
        }
    }

    /// The tags of the bitmap regions on screen right now, read from the pixels
    /// the compositor would upload.
    fn showing_bitmap_tags(engine: &CueEngine) -> Vec<u8> {
        engine
            .current_overlays()
            .iter()
            .filter(|overlay| overlay.space == OverlaySpace::SrcFrame)
            .map(|overlay| overlay.pixels[0])
            .collect()
    }

    /// The same, for a frame at `rt`. This ADVANCES the schedule, as a real
    /// displayed frame does.
    fn bitmap_tags_at(engine: &CueEngine, rt: u64) -> Vec<u8> {
        engine
            .overlays_for(Some(ms(rt)))
            .iter()
            .filter(|overlay| overlay.space == OverlaySpace::SrcFrame)
            .map(|overlay| overlay.pixels[0])
            .collect()
    }

    /// Several regions from one set, on screen at the same time as a text cue:
    /// the coexistence the two overlay spaces exist for. A disc's forced
    /// subpicture track and a text track really do run together.
    #[test]
    fn a_multi_region_set_renders_beside_the_text_cue() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.set_video_size(1920, 1080);
        let rig = DecoderRig::with_decode(|packet| {
            vec![DisplayUpdate {
                start_rt: packet.rt,
                end_rt: None,
                regions: (0..3)
                    .map(|index| bitmap_region(tag_of(packet), index * 100))
                    .collect(),
            }]
        });
        rig.install(&engine);

        engine.submit(cue("A text cue", 0, 10_000));
        engine.overlays_for(Some(ms(10)));
        assert!(
            wait_for(|| engine.current_overlays().len() == 1),
            "the text cue never rasterized"
        );

        engine.submit_bitmap(bitmap_packet(7, 0, None));
        assert!(
            wait_for(|| engine.current_overlays().len() == 4),
            "the three bitmap regions never joined the text cue on screen"
        );

        let overlays = engine.current_overlays();
        let text: Vec<_> = overlays
            .iter()
            .filter(|overlay| overlay.space == OverlaySpace::Window)
            .collect();
        let bitmap: Vec<_> = overlays
            .iter()
            .filter(|overlay| overlay.space == OverlaySpace::SrcFrame)
            .collect();
        assert_eq!(
            text.len(),
            1,
            "the text cue stopped showing when the bitmap set arrived"
        );
        assert_eq!(bitmap.len(), 3);
        assert_eq!(
            bitmap.iter().map(|overlay| overlay.x).collect::<Vec<_>>(),
            vec![0, 100, 200],
            "the regions lost their placement"
        );
        assert!(bitmap.iter().all(|overlay| overlay.pixels[0] == 7));
    }

    /// WAVE 7's bitmap preservation, on the seam the desktop lane runs on.
    ///
    /// A scene consumer draws the display lists itself, so the text cue has to
    /// stay out of the overlay list; a subpicture is decoded pixels with no
    /// display list, so it has to be in it. Both halves are asserted, because
    /// getting one of them without the other is how the lane loses PGS, DVB or
    /// VobSub subtitles silently.
    #[test]
    fn a_scene_consumer_keeps_the_bitmap_overlays_and_drops_the_text_raster() {
        gst::init().unwrap();
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(1280, 720);
        engine.set_video_size(1920, 1080);
        DecoderRig::default().install(&engine);

        engine.submit(cue("A text cue", 0, 10_000));
        engine.submit_bitmap(bitmap_packet(7, 0, None));
        assert!(
            wait_for(|| {
                engine.scenes_for(Some(ms(10)));
                engine.shown_scenes().len() == 1 && !engine.current_overlays().is_empty()
            }),
            "the lane never put the text cue and the subpicture on screen together"
        );
        // Every chance to paint the text cue anyway: the engine asks for the
        // next wanted job after every read.
        for _ in 0..30 {
            engine.overlays_for(Some(ms(10)));
            std::thread::sleep(Duration::from_millis(10));
        }

        let overlays = engine.current_overlays();
        assert_eq!(
            overlays
                .iter()
                .filter(|overlay| overlay.space == OverlaySpace::Window)
                .count(),
            0,
            "the text cue is still an overlay on a scene consumer, so the lane would draw it \
             twice, once composited into the video and once in the scene"
        );
        let bitmap: Vec<_> = overlays
            .iter()
            .filter(|overlay| overlay.space == OverlaySpace::SrcFrame)
            .collect();
        assert_eq!(
            bitmap.len(),
            1,
            "the subpicture went away with the text raster, and nothing on this lane can \
             draw it back: a display list is exactly what it does not have"
        );
        assert_eq!(bitmap[0].pixels[0], 7, "the region lost its pixels");

        assert_eq!(
            engine.shown_scenes().len(),
            1,
            "the text cue must still be on the scene lane's own read"
        );
        assert!(
            engine
                .shown_scenes()
                .iter()
                .all(|shown| shown.scene.glyph_count() > 0),
            "the display list the lane would draw carries no glyphs"
        );
    }

    /// The read a scene consumer composites bitmap sets from: the same regions
    /// [`CueEngine::current_overlays`] would answer with, without building the
    /// overlay list to get at them.
    ///
    /// The desktop wgpu lane reads this once per frame and compares it against
    /// what it last composited, so the two answers have to agree region for
    /// region or the lane would rebuild for nothing (or, worse, not rebuild
    /// when it should).
    #[test]
    fn the_bitmap_read_matches_the_overlays_the_raster_lane_would_build() {
        gst::init().unwrap();
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(1280, 720);
        engine.set_video_size(1920, 1080);
        DecoderRig::default().install(&engine);

        assert_eq!(
            engine.with_shown_bitmaps(|regions| regions.len()),
            0,
            "an engine with nothing on screen answered with regions"
        );

        engine.submit_bitmap(bitmap_packet(9, 0, None));
        // The decode is asynchronous and the schedule only moves when someone
        // reads it, which `with_shown_bitmaps` deliberately does not do.
        engine.overlays_for(Some(ms(10)));
        assert!(
            wait_for(|| engine.current_overlays().len() == 1),
            "the set never reached the screen"
        );

        let overlays = engine.current_overlays();
        let (tags, rects) = engine.with_shown_bitmaps(|regions| {
            (
                regions.iter().map(|r| r.pixels[0]).collect::<Vec<_>>(),
                regions
                    .iter()
                    .map(|r| (r.x, r.y, r.render_width, r.render_height))
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(
            tags,
            overlays
                .iter()
                .map(|overlay| overlay.pixels[0])
                .collect::<Vec<_>>(),
            "the two reads disagree about which pixels are on screen"
        );
        assert_eq!(
            rects,
            overlays
                .iter()
                .map(|o| (o.x, o.y, o.render_width, o.render_height))
                .collect::<Vec<_>>(),
            "the two reads disagree about where the regions land"
        );

        // The Arc is the identity the consumer caches on, so it has to be the
        // one the overlay carries and it has to survive a second read.
        engine.with_shown_bitmaps(|regions| {
            assert!(
                Arc::ptr_eq(&regions[0].pixels, &overlays[0].pixels),
                "the read copied the pixels instead of sharing them"
            );
        });

        engine.clear();
        assert_eq!(
            engine.with_shown_bitmaps(|regions| regions.len()),
            0,
            "a cleared engine still shows a bitmap set, so the lane would never take it down"
        );
    }

    /// A RESIZE MUST NOT BLANK THE LINE.
    ///
    /// `set_canvas` and `set_video_rect` re-key every active cue, and they used
    /// to drop what was on screen with it (`Pending`). On the scene lanes that
    /// is one or two frames of nothing while the worker re-lays-out, and a
    /// window drag is a continuous stream of those, which is the "resizing
    /// while playing makes them flash" report.
    ///
    /// Asserted at both ends: the same allocation is still showing on the read
    /// immediately after the re-key, and a NEW one replaces it once the worker
    /// answers, so this is a hold and not a leak.
    #[test]
    fn a_resize_keeps_the_cue_up_until_the_new_layout_lands() {
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(1280, 720);
        engine.submit(cue("It's a little after 12", 1_000, 20_000));
        engine.scenes_for(Some(ms(2_000)));
        assert!(
            wait_for(|| engine.shown_scenes().len() == 1),
            "the cue never made it on screen, so the resize below proves nothing"
        );
        let before = Arc::clone(&engine.shown_scenes()[0].scene);

        engine.set_canvas(1920, 1080);
        let during = engine.shown_scenes();
        assert_eq!(during.len(), 1, "the resize blanked the line");
        assert!(
            Arc::ptr_eq(&during[0].scene, &before),
            "the resize swapped the display list before the new one existed"
        );

        // A video rect change is the other half of a resize and re-keys the
        // same way, so it must hold the same.
        engine.set_video_rect(Some(VideoRect {
            x: 0,
            y: 60,
            width: 1920,
            height: 960,
        }));
        assert_eq!(
            engine.shown_scenes().len(),
            1,
            "the picture rect change blanked the line"
        );

        assert!(
            wait_for(|| {
                let shown = engine.shown_scenes();
                shown.len() == 1 && !Arc::ptr_eq(&shown[0].scene, &before)
            }),
            "the stale display list was never replaced, so the hold is a leak"
        );
    }

    /// The other side of the hold: a cue that ENDS still goes away at once.
    ///
    /// A stale scene keeps a re-keyed cue up. An expiry is not a re-key
    /// (the cue leaves `active` entirely), so nothing about the hold may reach
    /// it, or every line would linger past its end time.
    #[test]
    fn a_cue_that_ends_clears_even_after_a_resize() {
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(1280, 720);
        engine.submit(cue("It's a little after 12", 1_000, 1_000));
        engine.scenes_for(Some(ms(1_500)));
        assert!(
            wait_for(|| engine.shown_scenes().len() == 1),
            "the cue never made it on screen"
        );

        engine.set_canvas(1920, 1080);
        assert_eq!(
            engine.shown_scenes().len(),
            1,
            "the resize blanked the line"
        );
        // Past the end, with the replacement still unbuilt.
        engine.scenes_for(Some(ms(2_500)));
        assert!(
            engine.shown_scenes().is_empty(),
            "the expired cue is still on screen, so the stale hold outlived its cue"
        );
    }

    /// The scheduled clear happens at its own running time, and the
    /// immediate `clear()` beats it.
    ///
    /// The two are deliberately different primitives: an empty display set is
    /// the STREAM saying "nothing from here", while `clear()` is the driver
    /// saying "this track is gone". Confusing them is how a subpicture survives
    /// a track switch.
    #[test]
    fn a_zero_region_set_clears_at_its_time_and_clear_beats_it() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);

        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| bitmap_tags_at(&engine, 0) == vec![1]));
        engine.submit_bitmap(bitmap_packet(0, 3_000, None));
        assert!(wait_for(|| rig.pushes() == 2));

        assert_eq!(
            bitmap_tags_at(&engine, 2_999),
            vec![1],
            "the scheduled clear took the page off early"
        );
        assert_eq!(
            bitmap_tags_at(&engine, 3_000),
            Vec::<u8>::new(),
            "the scheduled clear never fired"
        );

        // A page showing, with its clear still in the future: `clear()` takes it
        // now, and takes the scheduled one with it.
        engine.submit_bitmap(bitmap_packet(2, 4_000, None));
        assert!(wait_for(|| bitmap_tags_at(&engine, 4_000) == vec![2]));
        engine.submit_bitmap(bitmap_packet(0, 9_000, None));
        assert!(wait_for(|| rig.pushes() == 4));

        engine.clear();
        assert!(
            engine.current_overlays().is_empty(),
            "clear() left the bitmap page on screen -- a track switch would show the old track's \
             subtitles over the new one"
        );
        assert_eq!(bitmap_tags_at(&engine, 9_000), Vec::<u8>::new());
    }

    /// A later set supersedes the one showing at its own time, and a set
    /// with an end but no successor comes off at that end (the DVB
    /// `page_time_out` shape).
    #[test]
    fn a_later_set_supersedes_and_a_timeout_expires_with_no_successor() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);

        engine.submit_bitmap(bitmap_packet(1, 0, None));
        engine.submit_bitmap(bitmap_packet(2, 1_000, None));
        assert!(wait_for(|| rig.pushes() == 2));

        assert_eq!(bitmap_tags_at(&engine, 0), vec![1]);
        assert_eq!(bitmap_tags_at(&engine, 999), vec![1]);
        assert_eq!(
            bitmap_tags_at(&engine, 1_000),
            vec![2],
            "the later set never took over"
        );
        assert_eq!(
            bitmap_tags_at(&engine, 5_000),
            vec![2],
            "an open-ended set expired on its own"
        );

        engine.clear();
        engine.submit_bitmap(bitmap_packet(3, 6_000, Some(500)));
        assert!(wait_for(|| rig.pushes() == 3));
        assert!(
            rig.resets.load(Ordering::Relaxed) >= 1,
            "the epoch bumped and the decoder kept its accumulated state"
        );
        assert_eq!(bitmap_tags_at(&engine, 6_000), vec![3]);
        assert_eq!(bitmap_tags_at(&engine, 6_499), vec![3]);
        assert_eq!(
            bitmap_tags_at(&engine, 6_500),
            Vec::<u8>::new(),
            "the page outlived its timeout with nothing to replace it"
        );
    }

    /// The packet inbox overflows by RESETTING, not by skipping, and a reset
    /// that lands while packets are already queued discards what they
    /// decode to.
    ///
    /// The two halves are the same mechanism from both ends. Reset-not-skip is
    /// what keeps a stateful decoder from being handed a stream with a hole in
    /// it. The epoch check is what keeps the work already in flight from
    /// landing on the far side of a track switch.
    #[test]
    fn an_overflowing_inbox_resets_and_a_clear_discards_what_is_queued() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);

        let hold = engine.hold_decode_for_test();
        for index in 0..BITMAP_QUEUE_LIMIT as u64 {
            engine.submit_bitmap(bitmap_packet(1, index * 10, None));
        }
        assert_eq!(
            engine.bitmap_overflow_resets(),
            0,
            "reaching the limit is not overflowing it"
        );
        for index in 0..4u64 {
            engine.submit_bitmap(bitmap_packet(9, 10_000 + index * 10, None));
        }
        assert_eq!(
            engine.bitmap_overflow_resets(),
            1,
            "one drain for the overflow, not one per packet past the limit"
        );
        drop(hold);

        assert!(
            wait_for(|| rig.pushes() == 4),
            "the decoder saw {} packets; the pre-overflow ones were not dropped whole",
            rig.pushes()
        );
        assert!(
            rig.pushed_tags().iter().all(|tag| *tag == 9),
            "a pre-overflow packet reached the decoder: {:?}",
            rig.pushed_tags()
        );
        assert!(
            wait_for(|| bitmap_tags_at(&engine, 10_030) == vec![9]),
            "the stream never recovered after the reset"
        );

        // The epoch half.
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);
        engine.overlays_for(Some(ms(1_000)));
        let hold = engine.hold_decode_for_test();
        engine.submit_bitmap(bitmap_packet(4, 0, None));
        engine.submit_bitmap(bitmap_packet(5, 500, None));
        engine.clear();
        drop(hold);

        assert!(
            wait_for(|| rig.pushes() == 2),
            "the queued packets never reached the decoder"
        );
        assert_eq!(
            engine.bitmap_sets_decoded(),
            2,
            "the sets were decoded; it is the PUBLISH that has to drop them"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            engine.current_overlays().is_empty(),
            "a set decoded from a packet submitted before the clear reached the screen"
        );
    }

    /// The DECODED backlog is bounded too, and it trims in the order that costs
    /// least, the text path's policy applied to a queue whose entries are
    /// megabytes instead of bytes.
    #[test]
    fn the_decoded_backlog_spends_the_past_before_the_future_and_counts_both() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::with_decode(|packet| {
            let mut updates = Vec::new();
            match tag_of(packet) {
                1 => {
                    // Already over at the playhead.
                    for index in 1..=100u64 {
                        updates.push(bitmap_set(ms(index), Some(ms(index + 1)), 1));
                    }
                    // The one that covers it.
                    updates.push(bitmap_set(ms(9_000), None, 7));
                    // And a long future tail.
                    for index in 0..200u64 {
                        updates.push(bitmap_set(ms(20_000 + index * 100), None, 2));
                    }
                }
                2 => {
                    for index in 0..200u64 {
                        updates.push(bitmap_set(ms(60_000 + index * 100), None, 3));
                    }
                }
                _ => {}
            }
            updates
        });
        rig.install(&engine);

        // The playhead.
        engine.overlays_for(Some(ms(10_000)));

        // First, 301 sets arrive at once. The 100 already-over ones and every
        // set they were superseded by go, and NOTHING showable does.
        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| engine.bitmap_dropped_sets() == 100));
        assert_eq!(
            showing_bitmap_tags(&engine),
            vec![7],
            "the set covering the playhead was evicted while 200 future sets were kept"
        );
        assert_eq!(
            engine.shared.state.lock().sched.bitmap_pending().len(),
            200,
            "the trim spent more than the free ones"
        );

        // Then 200 more future sets, and now there is nothing free left to
        // give up, so the FURTHEST FUTURE goes, from the far end.
        engine.submit_bitmap(bitmap_packet(2, 0, None));
        assert!(wait_for(|| engine.bitmap_dropped_sets() == 244));
        {
            let state = engine.shared.state.lock();
            assert_eq!(state.sched.bitmap_pending_len(), flapjack::cue::BITMAP_PENDING_LIMIT);
            assert_eq!(
                state.sched.bitmap_pending().front().expect("non-empty").start_rt,
                ms(20_000),
                "the trim spent the near future instead of the far future"
            );
            assert_eq!(
                state.sched.bitmap_pending().back().expect("non-empty").start_rt,
                ms(65_500)
            );
        }
        assert_eq!(
            showing_bitmap_tags(&engine),
            vec![7],
            "what is on screen was disturbed by a trim"
        );

        // And the BYTE bound, which is the one that bites first for real pages:
        // ten 8 MiB sets against a 64 MiB budget.
        let engine = CueEngine::new();
        let rig = DecoderRig::with_decode(|_packet| {
            (0..10u64)
                .map(|index| DisplayUpdate {
                    start_rt: ms(30_000 + index * 1_000),
                    end_rt: None,
                    regions: vec![BitmapRegion {
                        pixels: Arc::new(vec![index as u8 + 1; 8 * 1024 * 1024]),
                        width: 1024,
                        height: 2048,
                        x: 0,
                        y: 0,
                        render_width: 1024,
                        render_height: 2048,
                    }],
                })
                .collect()
        });
        rig.install(&engine);
        engine.overlays_for(Some(ms(1_000)));
        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| engine.bitmap_dropped_sets() > 0));

        let held: usize = engine
            .shared
            .state
            .lock()
            .sched.bitmap_pending()
            .iter()
            .map(DisplayUpdate::pixel_bytes)
            .sum();
        assert!(
            held <= BITMAP_PENDING_PIXEL_BUDGET,
            "the backlog holds {held} bytes, over the {BITMAP_PENDING_PIXEL_BUDGET} byte budget"
        );
        assert_eq!(
            engine.bitmap_dropped_sets(),
            2,
            "80 MiB trimmed to a 64 MiB budget is exactly two 8 MiB sets"
        );
    }

    /// The transport's preroll/render redelivery reaches the decoder once.
    ///
    /// `build_text_consumer_tail` installs both `new_sample` and `new_preroll`,
    /// so the same buffer object really is handed over twice in a row. The text
    /// path absorbs that in its latest-wins scheduling; a reassembler fed the
    /// same fragment twice corrupts the object it is building.
    ///
    /// The second half is the one that found a real defect: written against
    /// `gst::Buffer`'s `==`, this check ate a genuine re-delivery, because
    /// gstreamer-rs compares buffers by CONTENT. Subtitle packets repeat their
    /// bytes constantly, so that is not a corner case (see [`same_buffer`]).
    #[test]
    fn the_same_buffer_delivered_twice_reaches_the_decoder_once() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);

        let preroll = bitmap_packet(1, 0, None);
        let render = preroll.clone();
        engine.submit_bitmap(preroll);
        engine.submit_bitmap(render);
        assert!(wait_for(|| rig.pushes() >= 1));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rig.pushes(),
            1,
            "the preroll and the render of one buffer both reached the decoder"
        );

        // Identity, not content: a genuine re-delivery carries the same bytes in
        // a DIFFERENT buffer and must get through.
        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(
            wait_for(|| rig.pushes() == 2),
            "a genuine re-delivery was eaten by the duplicate check"
        );
    }

    /// A set covering the FROZEN frame goes on screen with no frame flowing,
    /// and the renderer is told to repaint. The bitmap twin of
    /// `a_paused_cue_covering_the_frozen_frame_reaches_the_screen`.
    #[test]
    fn a_paused_bitmap_set_covering_the_frozen_frame_reaches_the_screen() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);
        let changed = Arc::new(AtomicU64::new(0));
        let counter = changed.clone();
        engine.set_on_change(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        // A frame goes by with nothing selected, and then playback stops.
        engine.overlays_for(Some(ms(4_100)));
        assert!(engine.current_overlays().is_empty());
        changed.store(0, Ordering::Relaxed);

        // The switch: the redelivery carries a set covering where the item
        // already is. No frame follows.
        engine.submit_bitmap(bitmap_packet(5, 4_000, Some(500)));

        // The REPAINT SIGNAL first, and deliberately before anything reads the
        // overlay set. Nothing in production polls this engine (the renderer
        // repaints when `overlays-changed` tells it to), so a test that waited
        // on `current_overlays()` would be satisfied by its own polling and
        // would pass with the publish-side notification deleted.
        assert!(
            wait_for(|| changed.load(Ordering::Relaxed) >= 1),
            "the renderer was never told to repaint, so a paused viewer would keep looking at the \
             old frame however ready the set is"
        );
        assert!(
            !engine.current_overlays().is_empty(),
            "the repaint fired but there was nothing on screen to repaint"
        );
        assert_eq!(showing_bitmap_tags(&engine), vec![5]);
        assert_eq!(engine.current_overlays()[0].space, OverlaySpace::SrcFrame);

        // A set that does NOT cover the frozen frame changes nothing, which is
        // what makes the assertion above about COVERING rather than arrival.
        engine.clear();
        let settled = changed.load(Ordering::Relaxed);
        engine.submit_bitmap(bitmap_packet(6, 9_000, Some(500)));
        assert!(wait_for(|| rig.pushes() == 2));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            changed.load(Ordering::Relaxed),
            settled,
            "a set starting after the frozen frame raised a repaint"
        );
        assert!(
            engine.current_overlays().is_empty(),
            "a set starting after the frozen frame was put on screen anyway"
        );
    }

    /// The set already showing, handed back unchanged, must not repaint.
    ///
    /// A stateful decoder re-emitting its current page (the same region
    /// allocation, the same bounds) is a normal thing for these formats to do,
    /// and a paused viewer that repaints for every one of them is the thrash
    /// this check exists to prevent.
    #[test]
    fn re_adopting_the_set_already_showing_does_not_repaint() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let pixels: Arc<Vec<u8>> = Arc::new(vec![3u8; 4 * 4 * 4]);
        let rig = DecoderRig::with_decode(move |_packet| {
            vec![DisplayUpdate {
                start_rt: ms(0),
                end_rt: None,
                regions: vec![BitmapRegion {
                    pixels: pixels.clone(),
                    width: 4,
                    height: 4,
                    x: 0,
                    y: 0,
                    render_width: 4,
                    render_height: 4,
                }],
            }]
        });
        rig.install(&engine);
        let changed = Arc::new(AtomicU64::new(0));
        let counter = changed.clone();
        engine.set_on_change(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        engine.overlays_for(Some(ms(1_000)));
        engine.submit_bitmap(bitmap_packet(3, 0, None));
        assert!(wait_for(|| !engine.current_overlays().is_empty()));
        let after_first = changed.load(Ordering::Relaxed);
        assert!(after_first >= 1);

        engine.submit_bitmap(bitmap_packet(3, 0, None));
        assert!(wait_for(|| rig.pushes() == 2));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            changed.load(Ordering::Relaxed),
            after_first,
            "re-adopting the identical set repainted anyway"
        );
        assert_eq!(showing_bitmap_tags(&engine), vec![3]);
    }

    /// The coded video size reaches the decoder once, and so does the
    /// `codec_data` (the two setup calls a real decoder needs before it can
    /// place a region at all) and BOTH are taught again after a reset.
    ///
    /// `SubpicDecoder::reset` is contracted to return the decoder to its
    /// just-constructed state, so a decoder that has been reset knows no video
    /// size. The worker used to forget only the `codec_data`, and every set
    /// decoded after a flush, a clear or an inbox overflow was then scaled
    /// against the default grid for the rest of the stream. "Applied once" is
    /// the wrong property to pin: it is applied once per DECODER LIFETIME, and
    /// a reset starts a new one.
    #[test]
    fn the_decoder_learns_its_setup_once_and_relearns_it_after_a_reset() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::default();
        rig.install(&engine);

        engine.set_video_size(0, 1080);
        engine.set_video_size(1920, 1080);

        let palette = gst::Buffer::from_slice(vec![0xDEu8, 0xAD, 0xBE, 0xEF]);
        for rt in [0u64, 1_000] {
            engine.submit_bitmap(BitmapPacket {
                codec_data: Some(palette.clone()),
                ..bitmap_packet(1, rt, None)
            });
        }
        assert!(wait_for(|| rig.pushes() == 2));

        assert_eq!(
            *rig.sizes.lock(),
            vec![(1920, 1080)],
            "the coded size never reached the decoder, or reached it more than once -- and a zero \
             dimension must never reach it at all"
        );
        assert_eq!(
            *rig.codec_data.lock(),
            vec![vec![0xDE, 0xAD, 0xBE, 0xEF]],
            "unchanged codec_data was applied again"
        );
        assert_eq!(
            rig.builds.load(Ordering::Relaxed),
            1,
            "the decoder was rebuilt between two packets of the same format"
        );

        // THE RESET. `clear()` bumps the epoch, the worker resets the decoder,
        // and the decoder on the far side of that call knows nothing about the
        // picture it is drawing onto.
        engine.clear();
        engine.submit_bitmap(BitmapPacket {
            codec_data: Some(palette.clone()),
            ..bitmap_packet(1, 2_000, None)
        });
        assert!(wait_for(|| rig.pushes() == 3));
        assert!(
            rig.resets.load(Ordering::Relaxed) >= 1,
            "the epoch bumped and the decoder was never reset"
        );

        assert_eq!(
            *rig.sizes.lock(),
            vec![(1920, 1080), (1920, 1080)],
            "the reset decoder was never told the coded size again, so every region it produces \
             from here on is scaled onto the default grid"
        );
        assert_eq!(
            *rig.codec_data.lock(),
            vec![vec![0xDE, 0xAD, 0xBE, 0xEF], vec![0xDE, 0xAD, 0xBE, 0xEF]],
            "the reset decoder was never given its codec_data again"
        );
        assert_eq!(
            rig.builds.load(Ordering::Relaxed),
            1,
            "a reset is not a rebuild"
        );
    }

    /// An expired set does not supersede the open-ended set behind it, not in
    /// `evaluate_bitmap` and therefore not in the trim either.
    ///
    /// The two functions answer the same question ("what would be on screen at
    /// `rt`") from opposite ends, and they disagreed: `evaluate_bitmap` skips
    /// an expired candidate without disturbing the one already found, while the
    /// trim called every due set but the LAST one superseded. Behind a full
    /// backlog the open set was evicted as superseded, the expired one dropped
    /// as expired, and the viewer got a blank screen instead of the page the
    /// schedule was still showing.
    #[test]
    fn an_expired_set_does_not_evict_the_open_ended_one_it_follows() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::with_decode(|_packet| {
            let mut updates = vec![
                // The page on screen: open-ended, its turn long past.
                bitmap_set(ms(10_000), None, 7),
                // Behind it, a set that timed out before the playhead reached
                // it. It supersedes nothing: it can never be shown.
                bitmap_set(ms(12_000), Some(ms(13_000)), 8),
            ];
            // ...and enough future backlog to force a trim.
            for index in 0..300u64 {
                updates.push(bitmap_set(ms(30_000 + index * 100), None, 9));
            }
            updates
        });
        rig.install(&engine);

        engine.overlays_for(Some(ms(20_000)));
        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| engine.bitmap_dropped_sets() > 0));

        assert_eq!(
            showing_bitmap_tags(&engine),
            vec![7],
            "the open-ended set was evicted by a set that had already expired -- the screen goes \
             blank instead of keeping the page the schedule still shows"
        );
        assert!(
            !engine
                .shared
                .state
                .lock()
                .sched.bitmap_pending()
                .iter()
                .any(|update| update.start_rt == ms(12_000)),
            "the expired set was kept"
        );
    }

    /// The byte budget charges an ALLOCATION, not a pointer to it.
    ///
    /// DVB paints into persistent region buffers, re-emits the page whenever
    /// any part of it changes, and caches the expansion until a paint, a
    /// composition or a palette makes it stale, so a page really does arrive
    /// as many updates sharing one `Arc`. Charged per update, twenty
    /// pointers to one 8 MiB page would read as 160 MiB and trim a stream
    /// using 8.
    #[test]
    fn the_backlog_charges_a_shared_page_once() {
        gst::init().unwrap();

        const PAGE: usize = 8 * 1024 * 1024;
        const SETS: u64 = 20;

        // Twenty updates, ONE allocation between them.
        let engine = CueEngine::new();
        let shared_page: Arc<Vec<u8>> = Arc::new(vec![5u8; PAGE]);
        let rig = DecoderRig::with_decode(move |_packet| {
            (0..SETS)
                .map(|index| DisplayUpdate {
                    start_rt: ms(30_000 + index * 1_000),
                    end_rt: None,
                    regions: vec![BitmapRegion {
                        pixels: shared_page.clone(),
                        width: 1024,
                        height: 2048,
                        x: 0,
                        y: 0,
                        render_width: 1024,
                        render_height: 2048,
                    }],
                })
                .collect()
        });
        rig.install(&engine);
        engine.overlays_for(Some(ms(1_000)));
        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| engine.bitmap_sets_decoded() == SETS));
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(
            engine.bitmap_dropped_sets(),
            0,
            "sharing one {PAGE}-byte page across {SETS} updates was charged {SETS} times: the \
             budget trimmed a backlog holding 8 MiB"
        );
        assert_eq!(
            engine.shared.state.lock().sched.bitmap_pending().len() as u64,
            SETS,
            "the backlog gave up sets it did not have to"
        );
        assert_eq!(
            engine.shared.state.lock().sched.bitmap_pending_bytes(),
            PAGE,
            "one allocation, one charge"
        );

        // The same twenty sets with an allocation EACH really are over the
        // budget -- so the assertion above is about sharing, not about the
        // budget having stopped biting.
        let engine = CueEngine::new();
        let rig = DecoderRig::with_decode(move |_packet| {
            (0..SETS)
                .map(|index| DisplayUpdate {
                    start_rt: ms(30_000 + index * 1_000),
                    end_rt: None,
                    regions: vec![BitmapRegion {
                        pixels: Arc::new(vec![index as u8 + 1; PAGE]),
                        width: 1024,
                        height: 2048,
                        x: 0,
                        y: 0,
                        render_width: 1024,
                        render_height: 2048,
                    }],
                })
                .collect()
        });
        rig.install(&engine);
        engine.overlays_for(Some(ms(1_000)));
        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| engine.bitmap_dropped_sets() > 0));
        assert_eq!(
            engine.bitmap_dropped_sets(),
            SETS - (BITMAP_PENDING_PIXEL_BUDGET / PAGE) as u64,
            "unshared, twenty 8 MiB pages trim to the 64 MiB budget"
        );
    }

    /// A new stream takes the open-ended sets with it, and leaves the
    /// bounded ones alone.
    ///
    /// STREAM_START is the VIDEO sink's, and the rule it follows for text is
    /// "scheduled cues are the producer's decision". An open-ended bitmap set
    /// cannot be left on that rule: it says "until something replaces me", and
    /// the thing that would have replaced it belonged to the item that just
    /// ended. A PGS page from the previous film would otherwise sit on the next
    /// one until its first subtitle arrived.
    #[test]
    fn a_new_stream_takes_the_open_ended_sets_and_leaves_the_bounded_ones() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        let rig = DecoderRig::with_decode(|packet| match tag_of(packet) {
            // The page on screen is open-ended; a bounded set waits behind it.
            1 => vec![
                bitmap_set(ms(0), None, 1),
                bitmap_set(ms(50_000), Some(ms(51_000)), 4),
            ],
            // A bounded page on screen, with an open-ended one behind it.
            _ => vec![
                bitmap_set(ms(0), Some(ms(60_000)), 2),
                bitmap_set(ms(50_000), None, 5),
            ],
        });
        rig.install(&engine);

        engine.submit_bitmap(bitmap_packet(1, 0, None));
        assert!(wait_for(|| bitmap_tags_at(&engine, 1_000) == vec![1]));

        engine.reset_timeline();
        assert!(
            engine.current_overlays().is_empty(),
            "an open-ended set survived STREAM_START and is painted over the next item"
        );
        {
            let state = engine.shared.state.lock();
            assert_eq!(
                state.sched.bitmap_pending_len(),
                1,
                "the bounded set behind it was dropped too"
            );
            assert_eq!(
                state.sched.bitmap_pending().front().expect("non-empty").end_rt,
                Some(ms(51_000))
            );
        }

        // The other way round: a BOUNDED set on screen stays, because it
        // carries its own end and cannot outlive it.
        let engine = CueEngine::new();
        rig.install(&engine);
        engine.submit_bitmap(bitmap_packet(2, 0, None));
        assert!(wait_for(|| bitmap_tags_at(&engine, 1_000) == vec![2]));

        engine.reset_timeline();
        assert_eq!(
            showing_bitmap_tags(&engine),
            vec![2],
            "a bounded set was dropped at STREAM_START; only open-ended ones are stranded"
        );
        assert!(
            engine.shared.state.lock().sched.bitmap_pending().is_empty(),
            "the open-ended set queued behind it survived into the next item"
        );
    }

    /// Every format is wired, so there is no longer a production format
    /// `decoder_for` answers `None` for.
    ///
    /// What this pins is the WIRING: every format the engine can be handed
    /// builds a decoder, the table and the implemented set agree, and the
    /// unwired path still behaves if a fourth format ever lands in the enum
    /// without one. The last of those is tested through the decoder factory,
    /// which is the only way to produce an unwired format now, and that is the
    /// point: the production table cannot produce one.
    #[test]
    fn every_format_is_wired_and_an_unwired_one_is_counted_and_warned_about_once() {
        gst::init().unwrap();

        // THE WIRING, for all three, from the engine's side.
        for format in BitmapSubFormat::ALL {
            assert!(
                crate::subpic::implemented(format),
                "{format:?} is not in the implemented set"
            );
            assert!(
                crate::subpic::decoder_for(format).is_some(),
                "{format:?} is implemented but builds no decoder"
            );
        }

        // AND THE UNWIRED PATH, which no production format can reach any more.
        // A factory that refuses is the only way to get there, and the
        // behaviour it guards (count every packet, warn once per format and
        // epoch) is what stops a mis-wired fourth format from being silent.
        let warns = install_warn_counter();
        let engine = CueEngine::new();
        engine.set_decoder_factory(|_format| None);
        let before = warns.load(Ordering::Relaxed);

        for index in 0..8u64 {
            engine.submit_bitmap(bitmap_packet_of(BitmapSubFormat::Dvb, 1, index * 10, None));
        }
        assert!(wait_for(|| engine.bitmap_decode_errors() == 8));
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(
            engine.bitmap_decode_errors(),
            8,
            "the per-packet counter stopped counting"
        );
        assert_eq!(
            warns.load(Ordering::Relaxed) - before,
            1,
            "eight packets of an unwired format printed one warning per packet"
        );

        // A new epoch is a new report: the condition may have been fixed, and a
        // silent second stream would be worse than a repeated line.
        engine.clear();
        engine.submit_bitmap(bitmap_packet_of(BitmapSubFormat::Dvb, 1, 100, None));
        assert!(wait_for(|| engine.bitmap_decode_errors() == 9));
        assert!(
            wait_for(|| warns.load(Ordering::Relaxed) - before == 2),
            "the warning never came back after a reset"
        );
    }
}
