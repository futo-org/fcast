//! Sink-side subtitle cue state: which cue is on screen right now
//! ([`CueEngine`]) and what it looks like (the `fvid-cue-raster` worker).
//!
//! The engine is fed running-time-scheduled cues from outside the sink and is
//! evaluated per displayed frame, so a cue's visibility is a pure function of
//! the frame's running time and the cue's window. No pipeline clock, no
//! waiting, and no dependency on a new video buffer to change what is shown,
//! which is what makes a paused subtitle switch possible.
//!
//! Timing semantics match `fcasttextoverlay`'s `wait_for_text_buf`, as the
//! pure functions [`cue_is_too_old`] and [`cue_is_in_future`]. Its blocking
//! handoff is deliberately not lifted.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use pango::prelude::*;
use parking_lot::{Condvar, Mutex};
use smallvec::SmallVec;
use tracing::{debug, info, warn};

use crate::{
    cue_ir::{self, CueIr, CueStyle, VideoRect},
    subpic::{BitmapFormat, BitmapPacket, DisplayUpdate, SubpicDecoder},
    video::{Overlay, OverlaySpace},
};

/// Cap on cues waiting for their turn, sized for a whole file because that is
/// what arrives: an external subtitle branch is unsynced by construction, so
/// the parser hands the entire file over at once. A whole film's cues cost
/// well under a megabyte, so this is a runaway-producer backstop rather than a
/// working bound.
const PENDING_LIMIT: usize = 4096;

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

/// Decoded bitmap sets allowed to wait for their turn.
///
/// The text path's 4096 does not transfer: a queued cue is a short string
/// while a decoded display set is megabytes of RGBA. The store is bounded by
/// count and by bytes, and whichever bites first wins.
const BITMAP_PENDING_LIMIT: usize = 256;

/// Pixel memory allowed in the decoded-set backlog. The per-decoder allocation
/// budget bounds one decoder's working set; this bounds how much of its output
/// may be held waiting.
const BITMAP_PENDING_PIXEL_BUDGET: usize = 64 * 1024 * 1024;

/// How many decode costs are kept for [`CueEngine::bitmap_decode_latencies`].
const BITMAP_LATENCY_WINDOW: usize = 256;

/// How long a worker waits with nothing to do before it retires.
///
/// Both engine workers are lazily spawned and lazily unspawned, so an idle
/// sink does not keep a thread parked for the process lifetime. The cost of
/// retiring too eagerly is one thread spawn off the streaming thread; the
/// timeout is long enough that a normal subtitle cadence never retires the
/// worker mid-track.
const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// How many text cues may be on screen at once.
///
/// Overlapping cues are real (WebVTT and SSA both allow them) but few in
/// practice. Eight is a backstop against a pathological file, sized so eight
/// stacked cues still fit on a 720p canvas.
///
/// Above it, the oldest-start cue goes, with a warning. The cues that just
/// arrived are the ones the viewer has not read yet.
const MAX_ACTIVE_CUES: usize = 8;

/// Whether the engine shows exactly one text cue at a time.
///
/// Lever: `FCAST_SINGLE_ACTIVE_CUE=1` (set = on). It restores the
/// `fcasttextoverlay` behaviour of holding exactly one text buffer: a cue
/// whose turn comes replaces whatever is showing. Overlapping cues then show
/// one at a time, but existing pixel and timing expectations were written
/// against this, so it stays reachable.
///
/// Read once, on first use. The engine keeps per-cue state whose shape depends
/// on the answer, and a lever changed under a running pipeline would leave
/// that state describing a policy no longer in force.
fn single_active_cues() -> bool {
    static SINGLE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("FCAST_SINGLE_ACTIVE_CUE").is_some_and(|value| value == "1")
    });
    *SINGLE
}

/// How far ahead of a frozen frame a cue may be pulled onto the screen.
///
/// Paused only, and a deliberate semantic choice: this shows a cue early.
/// Caption converters can leave a small hole between the end of one cue and
/// the start of the next. While playing that hole is invisible, but a viewer
/// who pauses can land inside it and the screen goes correctly, uselessly
/// blank, since no frame will ever arrive to bring the next cue in. Other
/// players fill that hole by reading the cue nearest the playhead, and 200 ms
/// gives that feel with room to spare.
///
/// Playing is excluded because nothing is gained there and something is lost:
/// the cue starts on time by itself, and pulling it in early would move every
/// cue boundary in the file, which is visible jitter against the audio.
///
/// Asymmetric on purpose: only a cue's start is relaxed. Expiry is read at the
/// exact frame time, so nothing ever leaves the screen early.
const PAUSED_CUE_LOOKAHEAD: gst::ClockTime = gst::ClockTime::from_mseconds(200);

/// The lookahead actually in force: [`PAUSED_CUE_LOOKAHEAD`], or none at all.
///
/// Lever: `FCAST_NO_PAUSED_CUE_LOOKAHEAD` (set = off). With it set the paused
/// schedule is exact again and a frame frozen in a gap stays blank.
///
/// Read once, on first use, like every other lever here. A tolerance changed
/// under a running pipeline would leave cues on screen that the policy now in
/// force would never have put there.
fn paused_cue_lookahead() -> gst::ClockTime {
    static OFF: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("FCAST_NO_PAUSED_CUE_LOOKAHEAD").is_some());
    if *OFF {
        gst::ClockTime::ZERO
    } else {
        PAUSED_CUE_LOOKAHEAD
    }
}

/// Rasters kept around after they stop being active. Small on purpose. It
/// exists so a re-show (a track toggled off and on, a seek back into the same
/// cue, a canvas that returns to a previous size) is instant, not so that a
/// whole subtitle file stays resident.
const RASTER_CACHE_LIMIT: usize = 8;

/// Layout constants, in one place (all derived from the window/canvas size, so
/// cues are sized against the real display, not the video's coded size).
mod layout {
    /// Font size as a fraction of canvas height.
    pub const FONT_HEIGHT_FRACTION: f64 = 0.045;
    /// Never smaller than this, however small the window gets.
    pub const MIN_FONT_PX: f64 = 12.0;
    /// Wrap width as a fraction of canvas width.
    pub const WRAP_WIDTH_FRACTION: f64 = 0.90;
    /// Distance from the bottom edge, as a fraction of canvas height.
    pub const BOTTOM_MARGIN_FRACTION: f64 = 0.04;
    pub const FONT_FAMILY: &str = "Sans";
    /// Refuse to allocate a raster larger than this in either dimension.
    pub const MAX_RASTER_PX: i32 = 8192;
}

/// The text formats the renderer accepts.
///
/// [`TextFormat::Utf8`] and [`TextFormat::PangoMarkup`] mirror the `format`
/// field of the production text caps (`text/x-raw, format={utf8,
/// pango-markup}`) and are rasterized by the pango/cairo [`RasterCtx`] below,
/// exactly as they always were.
///
/// [`TextFormat::CueIr`] is the third arm, added with `gst-subparse`'s
/// `text-format=cue-ir`. It is not a new caps format: those buffers still
/// negotiate `text/x-raw, format=utf8` and carry readable UTF-8 text, with the
/// styling travelling beside the payload in a `CueIrMeta`. It is rasterized by
/// [`crate::cue_ir`] (parley + vello_cpu), the only arm that understands
/// per-span styling, per-cue positioning and karaoke.
///
/// Not `Copy`/`Eq`/`Hash`: the IR is an `Arc` payload holding `f32`s, so there
/// is no lawful `Eq`. The raster cache is a linear-scan `Vec`, for which
/// `PartialEq` is enough.
#[derive(Debug, Clone, PartialEq)]
pub enum TextFormat {
    Utf8,
    PangoMarkup,
    /// A cue parsed with `text-format=cue-ir`.
    CueIr {
        /// The styled cue, shared with the driver's delivery (no copy).
        ir: Arc<CueIr>,
        /// The text buffer's pts. Karaoke reveal times in the IR are absolute
        /// on that timeline, so this is what anchors them to `start_rt`.
        /// `None` disables reveal stepping and shows the whole cue at once,
        /// which is always safe.
        pts_start: Option<gst::ClockTime>,
    },
}

impl TextFormat {
    /// The IR this cue is rendered from, when it has one.
    fn ir(&self) -> Option<&Arc<CueIr>> {
        match self {
            TextFormat::CueIr { ir, .. } => Some(ir),
            _ => None,
        }
    }
}

/// One cue, already converted to running time by the producer.
#[derive(Debug, Clone, PartialEq)]
pub struct CueInput {
    pub format: TextFormat,
    pub text: String,
    pub start_rt: gst::ClockTime,
    /// `None` means open-ended: the cue stays active until superseded or
    /// cleared.
    pub end_rt: Option<gst::ClockTime>,
}

/// A cue no longer covers a frame once the frame's running time has reached the
/// cue's end.
///
/// The overlay element's too-old rule (`text_running_time_end <=
/// vid_running_time`), with an open-ended cue (no end) never expiring.
pub fn cue_is_too_old(end_rt: Option<gst::ClockTime>, frame_rt: gst::ClockTime) -> bool {
    end_rt.is_some_and(|end| end <= frame_rt)
}

/// A cue has not begun while the frame's running time is before its start.
///
/// The overlay element states the same rule over the video buffer's whole
/// window. The sink evaluates per displayed frame *instant* rather than per
/// buffer window, which collapses that to `frame_rt < start_rt`. The only
/// difference is at most one frame of display quantization, which the element
/// already has.
pub fn cue_is_in_future(start_rt: gst::ClockTime, frame_rt: gst::ClockTime) -> bool {
    start_rt > frame_rt
}

/// A rendered cue: tightly packed RGBA with straight (non-premultiplied) alpha,
/// placed in window coordinates.
#[derive(Debug)]
pub struct Raster {
    /// Shared with every [`Overlay`] built from this raster. A cue strip is
    /// megabytes and `overlays_for` runs per displayed frame, so the buffer is
    /// refcount-shared rather than memcpy'd. The upload path only ever reads
    /// `&pixels[..]`, which derefs identically.
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
}

impl Raster {
    /// Cheap. The pixel buffer is refcount-shared with the overlay, so this is
    /// a handful of scalar copies per frame, not a memcpy.
    fn to_overlay(&self) -> Overlay {
        Overlay {
            pixels: self.pixels.clone(),
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            render_width: self.width,
            render_height: self.height,
            // Window space: the raster was laid out at display resolution, so
            // it must not be scaled (or rotated) with the video.
            space: OverlaySpace::Window,
        }
    }

    /// Texture dimensions, for tests and diagnostics.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Placement in window coordinates, for tests and diagnostics.
    pub fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// The RGBA bytes, for tests and diagnostics.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

/// What a raster is fully determined by: the cue's content, the canvas it is
/// laid out against, the house style, where the picture sits, and for karaoke
/// how many reveal steps have passed. Same key means byte-identical pixels,
/// which is what makes the cache sound.
///
/// `style`/`video_rect`/`step` only vary the cue-IR arm's output. Carrying
/// them for the pango arm too costs nothing but a cache flush on a style
/// change, which is a user-settings action, not a per-frame one.
///
/// Equality is structural with an `Arc` pointer fast path (a re-shown cue is
/// usually the same allocation). No `Eq`/`Hash`: [`CueIr`] and [`CueStyle`]
/// hold `f32`s, and the linear-scan cache never needed them.
#[derive(Debug, Clone)]
struct RasterKey {
    text: String,
    format: TextFormat,
    canvas: (u32, u32),
    style: Arc<CueStyle>,
    video_rect: Option<VideoRect>,
    /// Number of reveal thresholds at or before the frame clock (0 = only the
    /// un-timed spans are visible). Always 0 for non-karaoke cues.
    step: usize,
}

impl PartialEq for RasterKey {
    fn eq(&self, other: &Self) -> bool {
        self.canvas == other.canvas
            && self.video_rect == other.video_rect
            && self.step == other.step
            && self.text == other.text
            && (Arc::ptr_eq(&self.style, &other.style) || self.style == other.style)
            && match (&self.format, &other.format) {
                (TextFormat::CueIr { ir: a, .. }, TextFormat::CueIr { ir: b, .. }) => {
                    cue_ir::ir_eq(a, b)
                }
                (a, b) => a == b,
            }
    }
}

#[derive(Debug)]
enum RasterState {
    /// Requested (or requestable), no pixels yet. The frame renders bare and
    /// the completion signal repaints. The engine never waits.
    Pending,
    Ready(Arc<Raster>),
    /// Like `Pending` for the worker (a replacement is wanted), but the
    /// previous raster keeps showing meanwhile. Its placement is still valid,
    /// so a stale frame beats a blank one. Used when the same cue re-keys in
    /// place (a karaoke step, a style change). A canvas/video-rect change stays
    /// `Pending` instead, because the old placement is wrong in the new
    /// geometry.
    Stale(Arc<Raster>),
    /// The worker could not produce pixels (empty text, absurd size, cairo
    /// refusal). Remembered so the cue is not re-requested every frame.
    Failed,
}

impl RasterState {
    /// Re-key in place. Keep any pixels on screen while the replacement
    /// renders; otherwise start over as `Pending`.
    fn into_stale(self) -> RasterState {
        match self {
            RasterState::Ready(raster) | RasterState::Stale(raster) => RasterState::Stale(raster),
            _ => RasterState::Pending,
        }
    }
}

#[derive(Debug)]
struct Active {
    cue: CueInput,
    key: RasterKey,
    raster: RasterState,
    /// Reveal thresholds as running times (see [`cue_ir::reveal_steps`]); empty
    /// for the non-karaoke common case, which is every pango-arm cue.
    steps: Vec<gst::ClockTime>,
}

#[derive(Default)]
struct State {
    /// Cues waiting for their window, ordered by `start_rt`.
    pending: VecDeque<CueInput>,
    /// The cues on screen right now, ordered by `start_rt`, earliest first,
    /// which is also bottom-first on screen (see [`active_overlays`]). Bounded
    /// by [`MAX_ACTIVE_CUES`]; holds at most one entry under
    /// [`single_active_cues`].
    active: SmallVec<[Active; 2]>,
    /// Display size the rasters are laid out against.
    canvas: (u32, u32),
    /// Where the video sits inside that display (see [`VideoRect`]). `None`
    /// means unknown, and the whole window doubles as the picture, which is
    /// what the pango arm always did.
    video_rect: Option<VideoRect>,
    /// The house style cue-IR rasters are drawn with (see [`CueStyle`]).
    style: Arc<CueStyle>,
    /// The video segment as captured by the sink, for pts → running time.
    video_segment: Option<gst::Segment>,
    /// Running time of the most recently shown frame. While paused this is
    /// frozen, and it is what a newly arriving cue is evaluated against.
    last_shown_rt: Option<gst::ClockTime>,
    /// Orders raster requests by *state-lock* order: keys are computed under
    /// the state lock but written to the worker inbox after it, so two threads'
    /// writes can arrive inverted (see [`CueEngine::request_raster`]).
    request_seq: u64,

    // ---- the bitmap side: a PARALLEL state, sharing only this lock ----
    //
    // Nothing below is read by any text codepath, and `active` above is not
    // read by any bitmap one. The two meet in exactly three places: the reset
    // hooks (`clear`/`flush`/`reset_timeline`), the schedule advance in
    // `overlays_for`/`current_overlays`, and `active_overlays`, which
    // concatenates what both sides have to show.
    /// Decoded display sets waiting for their turn, ordered by `start_rt`.
    bitmap_pending: VecDeque<DisplayUpdate>,
    /// The set currently on screen, if any.
    bitmap_active: Option<DisplayUpdate>,
    /// Coded video size decoders pre-scale their regions to (see
    /// [`CueEngine::set_video_size`]). `(0, 0)` until the sink negotiates caps.
    video_size: (u32, u32),
    /// Bumped by every reset (clear, flush, new stream, inbox overflow). A
    /// packet is stamped with the epoch it was submitted under and a decoded
    /// set is only published if the epoch still matches, which is the single
    /// serialization point between the decode worker and everything else.
    bitmap_epoch: u64,
    /// The buffer the last accepted `submit_bitmap` carried, for the
    /// consecutive-duplicate check. Cleared on every epoch bump, so a genuine
    /// replay after a flush is never mistaken for a redelivery.
    last_bitmap_buffer: Option<gst::Buffer>,
}

type OnChange = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cache: Mutex<RasterCache>,
    /// How long a worker idles before retiring, in nanoseconds; zero means
    /// [`WORKER_IDLE_TIMEOUT`]. Only a test ever writes it.
    worker_idle_nanos: AtomicU64,
    worker: Mutex<Option<WorkerHandle>>,
    on_change: Mutex<Option<OnChange>>,
    dirty: AtomicBool,
    dropped: AtomicU64,
    /// Fontmap warm-up cost in nanoseconds; 0 until the worker has warmed.
    warm_nanos: AtomicU64,
    /// How long each raster the worker produced took, newest last, bounded.
    /// Cache hits never reach the worker, so counting them would report the
    /// cache rather than the rasterizer.
    raster_latencies: Mutex<VecDeque<Duration>>,

    // ---- the bitmap side ----
    /// The `fvid-sub-decode` worker, spawned on the first bitmap packet.
    decode_worker: Mutex<Option<DecodeHandle>>,
    /// Times the packet inbox overflowed and reset the decoder. Pathological:
    /// the phase gate asserts this stays 0 across the whole battery.
    bitmap_overflow_resets: AtomicU64,
    /// Decoded sets given up because the pending store was full. Also expected
    /// to stay 0 in a healthy run.
    bitmap_dropped_sets: AtomicU64,
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
        if let Some(handle) = self.worker.lock().take() {
            handle.stop();
        }
        if let Some(handle) = self.decode_worker.lock().take() {
            handle.stop();
        }
    }
}

impl Shared {
    fn worker_idle(&self) -> Duration {
        match self.worker_idle_nanos.load(Ordering::Relaxed) {
            0 => WORKER_IDLE_TIMEOUT,
            nanos => Duration::from_nanos(nanos),
        }
    }
}

type DecoderFactory = dyn Fn(BitmapFormat) -> Option<Box<dyn SubpicDecoder>> + Send + Sync;

/// Sink-side cue scheduler. Cheap to clone (an `Arc` handle); every method is
/// non-blocking.
#[derive(Clone, Default)]
pub struct CueEngine {
    shared: Arc<Shared>,
}

impl CueEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a cue. Called from the text delivery thread; never blocks and
    /// never rasterizes inline.
    pub fn submit(&self, cue: CueInput) {
        let mut changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();

            // Ordered by start time (delivery is normally in order; a re-send
            // after a seek may not be). `partition_point` because a whole-file
            // burst is mostly sorted and the insert point must not cost a walk
            // over thousands of queued cues.
            let at = state
                .pending
                .partition_point(|queued| queued.start_rt <= cue.start_rt);
            // A cue that repeats one already queued (or already showing) is
            // folded into it rather than appended. See `merge_delivery`.
            if !merge_delivery(&mut state, at, &cue) {
                state.pending.insert(at, cue);
                trim_pending(&mut state, &self.shared.dropped);
            }

            // A cue that covers the frame already on screen becomes visible
            // without a new frame. This is the paused path, so it evaluates
            // with the gap tolerance ([`PAUSED_CUE_LOOKAHEAD`]).
            changed = match state.last_shown_rt {
                Some(rt) => evaluate_paused(&mut state, rt),
                None => false,
            };
            let (want, filled) = self.resolve_raster(&mut state);
            changed |= filled;
            fetch = want;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
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
            if state
                .last_bitmap_buffer
                .as_ref()
                .is_some_and(|last| packet.as_ref().is_some_and(|p| same_buffer(last, &p.data)))
            {
                debug!(
                    rt = ?packet.as_ref().map(|p| p.rt),
                    "dropping a repeat of the packet just submitted (preroll then render)"
                );
                return;
            }
            let Some(packet) = packet.take() else { return };
            state.last_bitmap_buffer = Some(packet.data.clone());

            if slot.queue.len() >= BITMAP_QUEUE_LIMIT {
                let dropped = slot.queue.len();
                slot.queue.clear();
                state.bitmap_epoch += 1;
                let total = self
                    .shared
                    .bitmap_overflow_resets
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                warn!(
                    dropped,
                    epoch = state.bitmap_epoch,
                    total,
                    "bitmap decode inbox full; reset the decoder rather than decode a stream with a \
                     hole in it -- subtitles resume at the next complete display set"
                );
            }
            slot.queue.push_back((state.bitmap_epoch, packet));
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
            state.pending.clear();
            let changed = !state.active.is_empty();
            state.active.clear();
            changed | reset_bitmap_state(&mut state, true)
        };
        if changed {
            self.mark_changed();
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

        let changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();
            if state.canvas == (width, height) {
                return;
            }
            state.canvas = (width, height);

            // Every active cue's raster was laid out against the old size.
            for active in state.active.iter_mut() {
                active.key.canvas = (width, height);
                active.raster = RasterState::Pending;
            }
            // The old-canvas rasters stay in the cache. A canvas that returns
            // to a previous size is one of the re-shows [`RASTER_CACHE_LIMIT`]
            // exists to make instant; LRU is the eviction policy.
            let (want, filled) = self.resolve_raster(&mut state);
            fetch = want;
            changed = filled;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
    }

    /// Set the rectangle the video occupies inside the window, in window
    /// coordinates (`None` = unknown; the whole window then doubles as the
    /// picture, which is what the pango arm has always assumed). Update it
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

        let changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();
            if state.video_rect == rect {
                return;
            }
            state.video_rect = rect;

            // The active cues' rasters were laid out against the old frame, so
            // their placement is now wrong. Pending, not Stale.
            for active in state.active.iter_mut() {
                active.key.video_rect = rect;
                active.raster = RasterState::Pending;
            }
            // Rasters for the old rect stay cached, as in `set_canvas`. LRU is
            // the policy.
            let (want, filled) = self.resolve_raster(&mut state);
            fetch = want;
            changed = filled;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
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
        let changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();
            if *state.style == style {
                return;
            }
            let style = Arc::new(style);
            state.style = style.clone();

            // The active cues' rasters were drawn with the old style; keep
            // showing them (their placement is still valid) until the re-styled
            // ones land, rather than blinking blank on a settings toggle.
            for active in state.active.iter_mut() {
                active.key.style = style.clone();
                let raster = std::mem::replace(&mut active.raster, RasterState::Pending);
                active.raster = raster.into_stale();
            }
            // Old-style rasters stay cached too (LRU is the policy), so a
            // style toggled back is instant as well.
            let (want, filled) = self.resolve_raster(&mut state);
            fetch = want;
            changed = filled;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
    }

    /// The house style in force (see [`CueStyle`]).
    pub fn style(&self) -> CueStyle {
        (*self.shared.state.lock().style).clone()
    }

    /// Record the video segment the sink is running, so frame pts can be turned
    /// into the running time cues are scheduled in.
    pub fn set_video_segment(&self, segment: &gst::Segment) {
        self.shared.state.lock().video_segment = Some(segment.clone());
    }

    /// FLUSH_STOP. Both sides of the comparison are invalid: cues from before
    /// the flush must not be shown after it, and the timeline anchor is gone.
    pub fn flush(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            state.pending.clear();
            state.video_segment = None;
            state.last_shown_rt = None;
            let changed = !state.active.is_empty();
            state.active.clear();
            changed | reset_bitmap_state(&mut state, true)
        };
        if changed {
            self.mark_changed();
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
            state.video_segment = None;
            state.last_shown_rt = None;
            let mut changed = reset_bitmap_state(&mut state, false);

            let before = state.bitmap_pending.len();
            state
                .bitmap_pending
                .retain(|update| update.end_rt.is_some());
            let stranded = before - state.bitmap_pending.len();
            if stranded > 0 {
                debug!(
                    stranded,
                    "dropped open-ended bitmap sets at a new stream; they had no end of their own \
                     and nothing in the next item would have superseded them"
                );
            }
            if state
                .bitmap_active
                .as_ref()
                .is_some_and(|active| active.end_rt.is_none())
            {
                state.bitmap_active = None;
                changed = true;
            }
            changed
        };
        if changed {
            self.mark_changed();
        }
    }

    /// Running time of a frame with this pts, under the captured video segment.
    pub fn video_running_time(&self, pts: Option<gst::ClockTime>) -> Option<gst::ClockTime> {
        let pts = pts?;
        let state = self.shared.state.lock();
        let segment = state.video_segment.as_ref()?;
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
    pub fn overlays_for(&self, frame_rt: Option<gst::ClockTime>) -> SmallVec<[Overlay; 1]> {
        let mut changed = false;
        let fetch;
        let overlays;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = frame_rt {
                state.last_shown_rt = Some(rt);
                changed = evaluate(&mut state, rt);
                changed |= evaluate_bitmap(&mut state, rt);
            }
            let (want, filled) = self.resolve_raster(&mut state);
            changed |= filled;
            fetch = want;
            overlays = active_overlays(&state);
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
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
    pub fn current_overlays(&self) -> SmallVec<[Overlay; 1]> {
        let mut changed = false;
        let fetch;
        let overlays;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = state.last_shown_rt {
                changed = evaluate_paused(&mut state, rt);
                changed |= evaluate_bitmap(&mut state, rt);
            }
            let (want, filled) = self.resolve_raster(&mut state);
            changed |= filled;
            fetch = want;
            overlays = active_overlays(&state);
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
        overlays
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
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Rasters currently held in the cache.
    pub fn cached_rasters(&self) -> usize {
        self.shared.cache.lock().len()
    }

    /// Times the bitmap decode inbox overflowed and reset the decoder.
    /// Pathological by construction. See [`BITMAP_QUEUE_LIMIT`].
    pub fn bitmap_overflow_resets(&self) -> u64 {
        self.shared.bitmap_overflow_resets.load(Ordering::Relaxed)
    }

    /// Decoded display sets given up because the pending store was full.
    pub fn bitmap_dropped_sets(&self) -> u64 {
        self.shared.bitmap_dropped_sets.load(Ordering::Relaxed)
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
        self.with_raster_inbox(|inbox, slot| {
            slot.warm = true;
            inbox.cv.notify_all();
        });
    }

    /// What the last rasters cost, in order, from the request that reached the
    /// worker to the pixels being ready.
    ///
    /// A warm engine must put a cue on screen well inside a frame. Cache hits
    /// never reach the worker, so this measures the rasterizer rather than the
    /// cache in front of it.
    pub fn raster_latencies(&self) -> Vec<Duration> {
        self.shared
            .raster_latencies
            .lock()
            .iter()
            .copied()
            .collect()
    }

    pub fn warm_up_time(&self) -> Option<Duration> {
        match self.shared.warm_nanos.load(Ordering::Acquire) {
            0 => None,
            nanos => Some(Duration::from_nanos(nanos)),
        }
    }

    /// Cache lookup for the active cues' rasters. Returns the sequenced key to
    /// hand to the worker (a miss, or a karaoke prefetch) and whether any
    /// active raster was filled from cache. Must be called with `state` locked;
    /// takes the cache lock underneath it, which is the only order this pair is
    /// ever taken in.
    ///
    /// One key per call even when several cues want one. The worker asks for
    /// the next wanted key after every publish (see [`worker_main`]), so a
    /// stack of cues fills in one behind the other without the newest-wins
    /// slot losing any of them. A need always outranks a karaoke prefetch,
    /// whichever cue each belongs to.
    fn resolve_raster(&self, state: &mut State) -> (Option<(u64, RasterKey)>, bool) {
        let mut filled = false;
        let mut need = None;
        let mut prefetch = None;
        for active in state.active.iter_mut() {
            match active.raster {
                RasterState::Pending | RasterState::Stale(_) => {
                    if let Some(raster) = self.shared.cache.lock().get(&active.key) {
                        active.raster = RasterState::Ready(raster);
                        filled = true;
                    } else if need.is_none() {
                        need = Some(active.key.clone());
                    }
                }
                // Karaoke: while the current step shows, warm the next one so
                // crossing a reveal threshold is a cache hit instead of a
                // raster latency. `publish` files a prefetch under its own key
                // without touching what is on screen. Once cached this is one
                // short cache probe per frame, and only for karaoke cues.
                RasterState::Ready(_)
                    if prefetch.is_none() && active.key.step < active.steps.len() =>
                {
                    let next = RasterKey {
                        step: active.key.step + 1,
                        ..active.key.clone()
                    };
                    if self.shared.cache.lock().get(&next).is_none() {
                        prefetch = Some(next);
                    }
                }
                _ => {}
            }
        }

        // The cue boundary, same trick as the karaoke prefetch: while the
        // current cue shows, warm the cue that comes next, so the frame that
        // crosses the boundary is a cache hit instead of a raster latency.
        //
        // Without it, a cue whose predecessor ends exactly where it starts is
        // adopted `Pending` and skipped by `active_overlays`, so the boundary
        // frame carries nothing and the line reappears one frame later. On a
        // file whose cues are contiguous that is a visible flash at every
        // boundary; a gap between cues merely hides it.
        //
        // Costs nothing in steady state. The cue is rastered once either way;
        // this only moves the work earlier, onto an idle worker. Ranked below
        // both the need and the karaoke prefetch, since anything a visible cue
        // wants outranks warming one that is not up yet.
        if need.is_none()
            && prefetch.is_none()
            && let Some(next) = state.pending.front()
        {
            let rate = state.video_segment.as_ref().map_or(1.0, |s| s.rate());
            let (_, step) = reveal_plan(next, rate);
            let key = RasterKey {
                text: next.text.clone(),
                format: next.format.clone(),
                canvas: state.canvas,
                style: state.style.clone(),
                video_rect: state.video_rect,
                step,
            };
            if self.shared.cache.lock().get(&key).is_none() {
                prefetch = Some(key);
            }
        }

        match need.or(prefetch) {
            Some(key) => {
                state.request_seq += 1;
                (Some((state.request_seq, key)), filled)
            }
            None => (None, filled),
        }
    }

    /// Ask for the next raster the active set wants, if any, and repaint if a
    /// cache hit filled one in. Called by the raster worker after every
    /// publish, so a stack of cues resolves without waiting for the next frame
    /// (while paused, without waiting at all).
    ///
    /// Takes no lock across the request: state lock, then release, then the
    /// inbox, the same order every other caller uses.
    fn pump_rasters(&self) {
        let (fetch, filled) = {
            let mut state = self.shared.state.lock();
            self.resolve_raster(&mut state)
        };
        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if filled {
            self.mark_changed();
        }
    }

    fn request_raster(&self, (seq, request): (u64, RasterKey)) {
        self.with_raster_inbox(|inbox, slot| {
            // Newest-wins by *state-lock order*, not thread arrival order. The
            // key was computed under the state lock but is written here after
            // releasing it, so a preempted thread can deliver a stale key late.
            // Without the sequence check it would clobber a newer request and,
            // since `publish` rightly rejects the stale raster, leave the
            // active cue Pending with an empty inbox. Self-healing in one
            // frame during playback, but stuck indefinitely while paused.
            if slot.request.as_ref().is_none_or(|(s, ..)| *s < seq) {
                slot.request = Some((seq, request.clone(), Instant::now()));
                inbox.cv.notify_all();
            }
        });
    }

    /// Run `f` under the raster inbox's lock, against an inbox whose worker is
    /// still alive.
    ///
    /// The retirement handshake, and the reason it is a loop. A worker retires
    /// only while holding this lock, so `f` can never be handed to a dead
    /// inbox. Either this call gets the lock first (and the retirement then
    /// finds work waiting and abandons itself), or the retirement got there
    /// first and left `retired` set, in which case the next `worker_inbox()`
    /// spawns a fresh worker. One retry at most, and only in the window around
    /// a retirement.
    fn with_raster_inbox<R>(&self, mut f: impl FnMut(&Arc<Inbox>, &mut Slot) -> R) -> R {
        loop {
            let inbox = self.worker_inbox();
            let mut slot = inbox.slot.lock();
            if slot.retired {
                continue;
            }
            return f(&inbox, &mut slot);
        }
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

    /// The worker is spawned on first use, so a sink that never shows a cue
    /// never pays for a thread.
    fn worker_inbox(&self) -> Arc<Inbox> {
        let mut worker = self.shared.worker.lock();
        if let Some(handle) = worker.as_ref() {
            return handle.inbox.clone();
        }
        let inbox = Arc::new(Inbox::default());
        let weak = Arc::downgrade(&self.shared);
        let thread_inbox = inbox.clone();
        // A respawn inherits the warm-up. The fontmap went with the retired
        // worker's thread, and building one on the cue path can cost seconds
        // (why [`CueEngine::warm`] exists). A sink warmed once stays warmed
        // across retirements: the new worker rebuilds its context before it is
        // asked for pixels rather than during.
        if self.shared.warm_nanos.load(Ordering::Acquire) != 0 {
            inbox.slot.lock().warm = true;
        }
        let spawned = std::thread::Builder::new()
            .name("fvid-cue-raster".to_owned())
            .spawn(move || worker_main(weak, thread_inbox));
        match spawned {
            Ok(_) => {
                *worker = Some(WorkerHandle {
                    inbox: inbox.clone(),
                })
            }
            Err(err) => warn!(%err, "failed to spawn the cue raster thread"),
        }
        inbox
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

    /// Shorten the window an idle worker waits before retiring.
    ///
    /// Tests only. Production uses [`WORKER_IDLE_TIMEOUT`]. A test that wants
    /// to watch a retirement cannot wait that long.
    #[doc(hidden)]
    pub fn set_worker_idle_for_test(&self, idle: Duration) {
        self.shared
            .worker_idle_nanos
            .store(idle.as_nanos().max(1) as u64, Ordering::Relaxed);
    }

    /// Whether each engine worker thread currently exists.
    ///
    /// `(raster, decode)`. Doc-hidden and for the retirement tests: the
    /// handles are what a submitter consults, so this is the engine's own
    /// answer to "is there a worker", beside the operating system's.
    #[doc(hidden)]
    pub fn workers_live(&self) -> (bool, bool) {
        (
            self.shared.worker.lock().is_some(),
            self.shared.decode_worker.lock().is_some(),
        )
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
        factory: impl Fn(BitmapFormat) -> Option<Box<dyn SubpicDecoder>> + Send + Sync + 'static,
    ) {
        *self.shared.decoder_factory.lock() = Some(Arc::new(factory));
    }

    fn mark_changed(&self) {
        self.shared.dirty.store(true, Ordering::Release);
        let callback = self.shared.on_change.lock().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// Everything on screen right now, as overlays: one per active text cue, then
/// the bitmap set's regions.
///
/// THE STACK. Each cue's raster already carries the placement its content asked
/// for (bottom-centre of the picture by house policy, or wherever the file put
/// it on the cue-IR arm: `line:`/`position:`, SSA `\pos`, an `{\an8}` anchor).
/// That placement is honoured as the cue's first choice, and cues are placed
/// in start order, so the earliest-starting cue keeps the spot it asked for
/// and a later one that would land on top of it moves up until it does not.
/// That is bottom-up stacking for the ordinary case and browser-like for the
/// positioned case: the WebVTT rendering algorithm also moves a cue box that
/// would overlap an existing one.
///
/// Two known limits, both deliberate:
///
///  * a stack tall enough to run off the top of the canvas clamps at 0 and the
///    cues there do overlap. [`MAX_ACTIVE_CUES`] keeps that out of reach for
///    real files.
///  * an unpositioned cue moving up may still land in a positioned cue's space
///    if the positioned cue starts later (it has not been placed yet when the
///    earlier one is). Fixing that means placing positioned cues first, which
///    reorders the stack, and the ordering is worth more.
fn active_overlays(state: &State) -> SmallVec<[Overlay; 1]> {
    let mut overlays = SmallVec::new();
    // What has been placed, in placement order: (x, y, width, height).
    let mut placed: SmallVec<[(i32, i32, u32, u32); 2]> = SmallVec::new();
    for active in state.active.iter() {
        // `Stale` counts: its pixels are a previous step or style of the same
        // cue in the same place, which beats blanking the line while the
        // replacement renders.
        let (RasterState::Ready(raster) | RasterState::Stale(raster)) = &active.raster else {
            continue;
        };
        let mut overlay = raster.to_overlay();
        overlay.y = stacked_y(&placed, &overlay);
        placed.push((overlay.x, overlay.y, overlay.width, overlay.height));
        overlays.push(overlay);
    }
    // The bitmap set rides beside the text cue, not instead of it: a source
    // can carry a subpicture track and a text track at once, and the
    // compositor already mixes the two spaces per overlay. These bypass the
    // raster path entirely. They are pixels already, so there is no key, cache
    // or worker between the decoder and the screen.
    if let Some(update) = state.bitmap_active.as_ref() {
        overlays.extend(update.regions.iter().map(|region| region.to_overlay()));
    }
    overlays
}

/// Where an overlay ends up once it has moved out of the way of the cues
/// already placed: its own `y`, or far enough above whatever it collided with.
///
/// Rectangles, not lines: two cues at different horizontal positions do not
/// collide and neither moves. The loop is bounded by the number of cues that
/// can be placed, since each pass either settles or clears one more rectangle.
fn stacked_y(placed: &[(i32, i32, u32, u32)], overlay: &Overlay) -> i32 {
    let (x, w, h) = (overlay.x, overlay.width as i32, overlay.height as i32);
    let mut y = overlay.y;
    for _ in 0..MAX_ACTIVE_CUES {
        let mut moved = false;
        for &(px, py, pw, ph) in placed {
            let overlaps_x = x < px + pw as i32 && px < x + w;
            let overlaps_y = y < py + ph as i32 && py < y + h;
            if overlaps_x && overlaps_y {
                y = py - h;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    // Off the top of the canvas is worse than overlapping. A cue nobody can
    // see is not a subtitle.
    y.max(0)
}

/// Drop the bitmap state a reset invalidates and bump the epoch. Returns
/// whether what is on screen changed.
///
/// `drop_decoded` distinguishes the two kinds of reset: `clear`/`flush` are
/// "this track's pictures are wrong now", so decoded sets go too; STREAM_START
/// is "the video timeline restarted", which invalidates the DECODER (its
/// half-assembled set belongs to the old stream) but says nothing about sets
/// the producer already scheduled.
///
/// The epoch bump always happens, and always takes the duplicate memory with
/// it. A replay that re-delivers the same bytes after a flush must not be
/// mistaken for the transport's preroll/render redelivery.
fn reset_bitmap_state(state: &mut State, drop_decoded: bool) -> bool {
    state.bitmap_epoch += 1;
    state.last_bitmap_buffer = None;
    if !drop_decoded {
        return false;
    }
    state.bitmap_pending.clear();
    state.bitmap_active.take().is_some()
}

/// Whether two handles name the same buffer object.
///
/// Not `==`. gstreamer-rs implements `PartialEq for BufferRef` as a content
/// comparison, and subtitle packets repeat their bytes constantly, so a
/// duplicate check written on `==` would swallow a real re-delivery.
///
/// The miniobject address is the identity. Holding a strong reference to the
/// previous buffer keeps its address from being recycled under the comparison.
fn same_buffer(a: &gst::Buffer, b: &gst::Buffer) -> bool {
    a.as_ptr() == b.as_ptr()
}

/// Whether two display updates describe the same picture in the same window.
///
/// Pixel buffers are compared by pointer, not content. The only way two
/// updates can legitimately share an allocation is that one came from the
/// other, which is exactly the duplicate this exists to recognize. A decoder
/// that produced the same image twice from different bytes is a new picture,
/// and re-adopting it costs one repaint.
fn same_update(a: &DisplayUpdate, b: &DisplayUpdate) -> bool {
    a.start_rt == b.start_rt
        && a.end_rt == b.end_rt
        && a.regions.len() == b.regions.len()
        && a.regions.iter().zip(b.regions.iter()).all(|(x, y)| {
            x.x == y.x
                && x.y == y.y
                && x.width == y.width
                && x.height == y.height
                && x.render_width == y.render_width
                && x.render_height == y.render_height
                && Arc::ptr_eq(&x.pixels, &y.pixels)
        })
}

/// Advance the bitmap schedule to `rt`. Returns whether the set on screen
/// changed.
///
/// The bitmap twin of [`evaluate`], deliberately the same shape: pop
/// everything whose turn has come, keep the last one (a display set lives
/// until the next one replaces it), and expire what is showing when its end
/// has passed.
///
/// Two differences from the text rule, both from [`DisplayUpdate`]'s contract:
///
///  * a popped update with no regions is the scheduled clear. It takes the
///    active set away instead of becoming it.
///  * expiry is a real event here rather than an edge case. Some formats' pages
///    carry a timeout and must come off the screen with nothing to replace
///    them.
///
/// The rule, stated once so [`trim_bitmap_pending`] can state the same one:
/// **what survives a run of due sets is the last non-expired one.** An expired
/// set in that run is skipped without disturbing the candidate already found.
/// A set that timed out before its turn supersedes nothing, so an earlier
/// open-ended set behind it stays on screen rather than being replaced by
/// blank.
fn evaluate_bitmap(state: &mut State, rt: gst::ClockTime) -> bool {
    let mut candidate = None;
    while let Some(next) = state.bitmap_pending.front() {
        if cue_is_in_future(next.start_rt, rt) {
            break;
        }
        let update = state
            .bitmap_pending
            .pop_front()
            .expect("front() just returned an update");
        // Superseded between two frames, or timed out before its turn. It can
        // never be shown, so it is not a candidate for anything.
        if cue_is_too_old(update.end_rt, rt) {
            debug!(start = %update.start_rt, %rt, "bitmap set expired before it could be shown");
            continue;
        }
        candidate = Some(update);
    }

    match candidate {
        Some(update) if update.regions.is_empty() => {
            // The scheduled clear, at its own running time.
            state.bitmap_active.take().is_some()
        }
        Some(update) => {
            // No-op adoption check: a redelivery that decodes to the picture
            // already showing must not report a change, or a paused viewer
            // repaints for nothing.
            if state
                .bitmap_active
                .as_ref()
                .is_some_and(|active| same_update(active, &update))
            {
                false
            } else {
                state.bitmap_active = Some(update);
                true
            }
        }
        None => {
            let expired = state
                .bitmap_active
                .as_ref()
                .is_some_and(|active| cue_is_too_old(active.end_rt, rt));
            if expired {
                debug!(%rt, "bitmap set timed out with nothing to replace it");
                state.bitmap_active = None;
            }
            expired
        }
    }
}

/// Bring `bitmap_pending` back under [`BITMAP_PENDING_LIMIT`] sets AND
/// [`BITMAP_PENDING_PIXEL_BUDGET`] bytes, in the order that costs least.
///
/// Same spend order as [`trim_pending`]: give up what can never be shown
/// first, and only then give up the future, from the far end, so the sets
/// about to be shown survive.
///
/// "Can never be shown" is a bigger class here than for text, and it is
/// [`evaluate_bitmap`]'s rule read backwards. The two must agree, or the trim
/// evicts a set the schedule would have shown. A set is unshowable at `rt` if
/// it has expired, and also if a later non-expired set already starts at or
/// before `rt`, because that is the one `evaluate_bitmap` adopts.
///
/// The "non-expired" qualifier matters: an expired set superseding an
/// open-ended one is not a supersession at all, and treating it as one would
/// blank a page the schedule was still showing.
///
/// Trim, never reset (unlike the packet inbox). Every [`DisplayUpdate`] is a
/// complete picture, so dropping one costs exactly that picture and nothing
/// downstream of it.
fn trim_bitmap_pending(state: &mut State, dropped: &AtomicU64) {
    if !bitmap_pending_over_budget(state) {
        return;
    }

    // 1. The free ones.
    if let Some(rt) = state.last_shown_rt {
        let before = state.bitmap_pending.len();
        // The last non-expired entry whose turn has already come is the one
        // `evaluate_bitmap` would adopt. Everything before it in that run is
        // superseded, and everything expired is dead wherever it sits. When
        // the whole due run has expired there is nothing to keep from it, so
        // the survivors start at the first future entry.
        let due = state
            .bitmap_pending
            .partition_point(|update| !cue_is_in_future(update.start_rt, rt));
        let keep_from = state
            .bitmap_pending
            .iter()
            .take(due)
            .rposition(|update| !cue_is_too_old(update.end_rt, rt))
            .unwrap_or(due);
        let mut index = 0;
        state.bitmap_pending.retain(|update| {
            let at = index;
            index += 1;
            at >= keep_from && !cue_is_too_old(update.end_rt, rt)
        });
        let evicted = (before - state.bitmap_pending.len()) as u64;
        if evicted > 0 {
            let total = dropped.fetch_add(evicted, Ordering::Relaxed) + evicted;
            warn!(
                evicted,
                %rt,
                total,
                "bitmap backlog full; gave up sets already past the playhead or superseded"
            );
        }
        if !bitmap_pending_over_budget(state) {
            return;
        }
    }

    // 2. The costly ones, from the far end. One set always survives: a single
    // set over the byte budget on its own is the decoder's allocation to
    // bound, and emptying the queue over it would give up a picture that is
    // about to be shown.
    let furthest = state
        .bitmap_pending
        .back()
        .expect("over the limit, so non-empty")
        .start_rt;
    let mut over = 0u64;
    let mut horizon = furthest;
    while bitmap_pending_over_budget(state) && state.bitmap_pending.len() > 1 {
        let gone = state
            .bitmap_pending
            .pop_back()
            .expect("length checked above");
        // Popping from the back means the last one dropped is the earliest,
        // which is the horizon the warning has to name.
        horizon = gone.start_rt;
        over += 1;
    }
    if over > 0 {
        let total = dropped.fetch_add(over, Ordering::Relaxed) + over;
        warn!(
            over,
            %horizon,
            %furthest,
            total,
            "bitmap backlog full; gave up the furthest-future sets -- nothing from the horizon \
             out to the furthest set will be shown"
        );
    }
}

/// Whether the decoded-set backlog has passed either of its bounds.
fn bitmap_pending_over_budget(state: &State) -> bool {
    state.bitmap_pending.len() > BITMAP_PENDING_LIMIT
        || bitmap_pending_pixel_bytes(state) > BITMAP_PENDING_PIXEL_BUDGET
}

/// What the decoded-set backlog actually costs in pixel memory: every distinct
/// allocation once, however many regions and updates point at it.
///
/// Sharing is the point. A [`BitmapRegion`]'s pixels are an `Arc`, and a
/// decoder with persistent region buffers may emit many updates pointing at
/// one allocation. Charging a shared page N times would trim such a stream at
/// a fraction of the budget it is actually using. Formats that build one
/// picture per display set share nothing and are charged in full.
///
/// Identity is the `Arc`'s pointer. A `Vec` that two `Arc`s hold separately is
/// two allocations and is charged twice, which is correct, since dropping one
/// does not free the other.
fn bitmap_pending_pixel_bytes(state: &State) -> usize {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut total = 0;
    for update in &state.bitmap_pending {
        for region in &update.regions {
            if seen.insert(Arc::as_ptr(&region.pixels) as usize) {
                total += region.pixel_bytes();
            }
        }
    }
    total
}

/// Whether two deliveries describe the same cue: same text, same format, same
/// start. Only the end may differ, and [`merge_end`] settles that.
fn same_cue(a: &CueInput, b: &CueInput) -> bool {
    a.start_rt == b.start_rt && a.format == b.format && a.text == b.text
}

/// The end a merged pair keeps: the LATER of the two. `None` (open-ended,
/// "until superseded or cleared") counts as the latest end there is.
///
/// Never the earlier one: a zero-length twin must not be able to expire the cue
/// it duplicates.
fn merge_end(a: Option<gst::ClockTime>, b: Option<gst::ClockTime>) -> Option<gst::ClockTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        // Either side open-ended makes the merged cue open-ended.
        _ => None,
    }
}

/// Fold a redelivery into the entry it repeats. Returns whether it was folded.
///
/// Two facts make this load-bearing rather than tidy:
///
///  * **Files carry each cue twice.** Caption converters emit, for every cue, a
///    zero-length record (`start == end`) immediately before the real one. Both
///    are real records and the parser and transport carry both faithfully (`pts
///    + 0` is `pts`), so 401 cues can arrive as 783 deliveries. Un-merged, the
///    twins double the backlog and the degenerate copy can expire the cue it
///    duplicates, since `end <= rt` is true of a zero-length window at every rt
///    at or after its start.
///  * **A replay re-delivers the whole file.** The subtitle input is seeked and
///    re-parsed from its origin whenever the branch has to be realigned, so the
///    engine sees the same cues again. Merging makes that redelivery a no-op
///    instead of a second copy of the file.
///
/// `at` is the insertion point [`CueEngine::submit`] computed, i.e. one past
/// the last entry whose start is `<= cue.start_rt`.
fn merge_delivery(state: &mut State, at: usize, cue: &CueInput) -> bool {
    // The equal-start run ends at `at`. It is one or two entries long in
    // practice, so walking it backwards costs nothing worth indexing away.
    let mut idx = at;
    while idx > 0 && state.pending[idx - 1].start_rt == cue.start_rt {
        idx -= 1;
        if same_cue(&state.pending[idx], cue) {
            state.pending[idx].end_rt = merge_end(state.pending[idx].end_rt, cue.end_rt);
            return true;
        }
    }
    // The repeat may also be of a cue ON SCREEN: it left `pending` when it
    // activated, so a replay's copy would otherwise re-queue it, and a
    // degenerate twin arriving behind it would then expire it early. With
    // several cues on screen the question is the same one, asked of each.
    if let Some(active) = state
        .active
        .iter_mut()
        .find(|active| same_cue(&active.cue, cue))
    {
        active.cue.end_rt = merge_end(active.cue.end_rt, cue.end_rt);
        return true;
    }
    false
}

/// Bring `pending` back under [`PENDING_LIMIT`], giving up the least useful
/// cues first.
///
/// WHICH cues go matters. Drop-oldest, under a whole-file burst, discards the
/// cue about to be shown in order to admit one an hour away: the tail of the
/// file evicts the head of it and the viewer sees nothing. So:
///
///  1. Cues already PAST the playhead go first. They can never be shown again,
///     so giving them up costs nothing at all.
///  2. Only if that is not enough does something showable go, and then it is
///     the FURTHEST FUTURE (the cues whose turn is last), with the horizon that
///     costs named in the warning.
///
/// Both kinds count toward `dropped_cues`, which stays what it always was: how
/// many cues the engine was handed and will not show.
fn trim_pending(state: &mut State, dropped: &AtomicU64) {
    if state.pending.len() <= PENDING_LIMIT {
        return;
    }

    // 1. The free ones.
    if let Some(rt) = state.last_shown_rt {
        let before = state.pending.len();
        state.pending.retain(|cue| !cue_is_too_old(cue.end_rt, rt));
        let evicted = (before - state.pending.len()) as u64;
        if evicted > 0 {
            let total = dropped.fetch_add(evicted, Ordering::Relaxed) + evicted;
            warn!(
                evicted,
                %rt,
                total,
                "cue backlog full; gave up cues already past the playhead"
            );
        }
        if state.pending.len() <= PENDING_LIMIT {
            return;
        }
    }

    // 2. The costly ones, from the far end.
    let over = state.pending.len() - PENDING_LIMIT;
    let horizon = state.pending[PENDING_LIMIT].start_rt;
    let furthest = state
        .pending
        .back()
        .expect("over the limit, so non-empty")
        .start_rt;
    state.pending.truncate(PENDING_LIMIT);
    let total = dropped.fetch_add(over as u64, Ordering::Relaxed) + over as u64;
    warn!(
        over,
        %horizon,
        %furthest,
        total,
        "cue backlog full; gave up the furthest-future cues -- nothing from the horizon out to \
         the furthest cue will be shown"
    );
}

/// Advance the schedule to `rt`. Returns whether what is on screen changed.
///
/// MULTI-ACTIVE: every cue whose window covers `rt` is on screen, and each one
/// leaves on its own end. Overlapping cues are normal in a subtitle file (a
/// speaker label under a line of dialogue, a sign translated while someone
/// talks over it). The single-active engine this one grew out of showed them
/// one at a time, because `fcasttextoverlay` holds exactly one text buffer and
/// a newer one replaces it.
///
/// LATEST-START-WINS is not gone. It stopped being a REPLACEMENT policy and
/// became an ORDERING one. The active set is kept in start order and
/// [`active_overlays`] stacks it bottom-up, so the earliest-starting cue holds
/// the bottom line and later ones sit above it, which is what a browser does
/// with the same file.
///
/// Unchanged from the element: a cue that started AND ended between two frames
/// is dropped without ever being shown (the too-old pop), the end is exclusive,
/// and a cue with no end never expires on its own.
///
/// Under [`single_active_cues`] this delegates to [`evaluate_single_active`],
/// which is the old rule kept whole rather than reconstructed out of the new
/// one.
fn evaluate(state: &mut State, rt: gst::ClockTime) -> bool {
    let mut changed = if single_active_cues() {
        evaluate_single_active(state, rt)
    } else {
        evaluate_multi_active(state, rt)
    };
    changed |= advance_karaoke(state, rt);
    changed
}

/// Advance the schedule to a FROZEN `rt`, then fill a gap the playhead stopped
/// inside. Returns whether what is on screen changed.
///
/// The paused twin of [`evaluate`]: that function plus one rule. See
/// [`PAUSED_CUE_LOOKAHEAD`] for why the rule exists and why it is paused-only.
/// The exact evaluation runs FIRST and unmodified, so expiry, overlap, ordering
/// and karaoke are unchanged. The lookahead can only ever ADD a cue that the
/// exact rule left off the screen.
///
/// THE COMPOSITION RULES, three of them, all in the narrow direction:
///
///  * **Only when the screen is empty.** The lookahead fires only if the active
///    set at `rt` is EMPTY, which is exactly the gap-landing case and nothing
///    else. A frame that already carries a cue is not a defect however close
///    the next cue is, and pulling one in beside it would turn a file's
///    ordinary cue boundary into an overlap the file never wrote, inventing a
///    two-line screen out of two one-line cues.
///  * **One cue, the nearest.** Not every cue inside the window: the policy
///    exists to fill a hole, and a hole is filled by the cue that comes next.
///  * **Nothing unshowable.** A cue that occupies no time (`start == end`) can
///    never be shown at any running time, so it is skipped and the scan
///    continues. That is not hypothetical: the converters that leave the gap
///    also put a zero-length twin in front of every real cue, so the twin is
///    usually the very first thing in the window.
///
/// The chosen cue is POPPED rather than peeked, so it is genuinely on screen
/// with an ordinary [`Active`] entry, an ordinary raster and an ordinary
/// expiry. That is what makes resuming seamless: the cue the viewer is already
/// looking at stays put as frames start flowing again, instead of blinking out
/// and coming back when its real start arrives.
///
/// BITMAPS ARE EXCLUDED, so [`evaluate_bitmap`] is called at the exact `rt` on
/// this path too. Their semantics are supersession, not windows: a display set
/// lives until the next one replaces it, so a bitmap track has no gaps of the
/// converter kind to fill. What it does have is the SCHEDULED CLEAR (a set with
/// no regions, whose whole purpose is to blank the screen at a chosen instant),
/// and a lookahead would let the picture after a clear jump the clear that was
/// put there to end it.
fn evaluate_paused(state: &mut State, rt: gst::ClockTime) -> bool {
    let mut changed = evaluate(state, rt);
    changed |= lookahead_into_gap(state, rt);
    changed
}

/// The paused-only rule of [`evaluate_paused`]: adopt the nearest showable cue
/// starting within [`PAUSED_CUE_LOOKAHEAD`] of a frozen `rt`, when nothing at
/// all covers `rt`.
///
/// Assumes the exact evaluation has already run, which is what makes the scan
/// cheap and the bounds simple: every cue still in `pending` starts strictly
/// after `rt`, so the window to search is a short prefix and the first showable
/// entry in it is the nearest one.
fn lookahead_into_gap(state: &mut State, rt: gst::ClockTime) -> bool {
    let tolerance = paused_cue_lookahead();
    if tolerance.is_zero() || !state.active.is_empty() {
        return false;
    }
    // Saturating: a running time within 200 ms of the end of the ClockTime range
    // is not reachable, but the arithmetic should not be the thing that decides
    // that.
    let horizon = rt.saturating_add(tolerance);

    // `take_while` bounds the scan to the window. `pending` is start-ordered,
    // so the first cue past the horizon ends it and nothing behind that one is
    // any nearer. `position` then picks the first SHOWABLE entry inside it,
    // stepping over the zero-length twins.
    let Some(at) = state
        .pending
        .iter()
        .take_while(|cue| !cue_is_in_future(cue.start_rt, horizon))
        .position(|cue| !cue_is_too_old(cue.end_rt, cue.start_rt))
    else {
        return false;
    };

    let cue = state
        .pending
        .remove(at)
        .expect("position() just returned this index");
    debug!(
        ?cue,
        %rt,
        ahead = %cue.start_rt.saturating_sub(rt),
        "paused frame landed in a gap; showing the next cue early"
    );
    let canvas = state.canvas;
    let video_rect = state.video_rect;
    let style = state.style.clone();
    let rate = state.video_segment.as_ref().map_or(1.0, |s| s.rate());
    // A plain `push` keeps both invariants the active set has, because it is
    // empty: start order is trivial with one entry, and `MAX_ACTIVE_CUES` is
    // nowhere near. The same emptiness makes this lawful under
    // `single_active_cues()`, which allows exactly one.
    state
        .active
        .push(activate(cue, canvas, video_rect, style, rate));
    true
}

/// Pop everything whose turn has come, dropping what expired before it could be
/// shown.
fn take_due(state: &mut State, rt: gst::ClockTime) -> SmallVec<[CueInput; 2]> {
    let mut due = SmallVec::new();
    while let Some(next) = state.pending.front() {
        if cue_is_in_future(next.start_rt, rt) {
            break;
        }
        let cue = state
            .pending
            .pop_front()
            .expect("front() just returned a cue");
        if cue_is_too_old(cue.end_rt, rt) {
            debug!(?cue, %rt, "cue expired before it could be shown");
            continue;
        }
        due.push(cue);
    }
    due
}

fn evaluate_multi_active(state: &mut State, rt: gst::ClockTime) -> bool {
    let due = take_due(state, rt);

    // Expiry is per cue and does not wait for a successor: this is the half the
    // single-active rule could not express, since there the arrival of any cue
    // ended whatever was showing.
    let before = state.active.len();
    state
        .active
        .retain(|active| !cue_is_too_old(active.cue.end_rt, rt));
    let mut changed = state.active.len() != before;

    let canvas = state.canvas;
    let video_rect = state.video_rect;
    let style = state.style.clone();
    let rate = state.video_segment.as_ref().map_or(1.0, |s| s.rate());
    for cue in due {
        // The same cue handed back (a redelivery that got past
        // `merge_delivery`, a re-evaluation of an unchanged set) must not
        // re-enter: it would re-key its raster and blink the line.
        if state.active.iter().any(|active| active.cue == cue) {
            continue;
        }
        // Start order, not arrival order. An out-of-order delivery whose start
        // is behind a cue already showing belongs BELOW it, not on top.
        let at = state
            .active
            .partition_point(|active| active.cue.start_rt <= cue.start_rt);
        state
            .active
            .insert(at, activate(cue, canvas, video_rect, style.clone(), rate));
        changed = true;
    }

    // The backstop. Dropping the oldest START is the only sane direction: the
    // cue that has been on screen longest is the one the viewer has had the
    // most time to read.
    while state.active.len() > MAX_ACTIVE_CUES {
        let gone = state.active.remove(0);
        warn!(
            cap = MAX_ACTIVE_CUES,
            text = gone.cue.text,
            start = %gone.cue.start_rt,
            %rt,
            "more overlapping cues than the screen holds; gave up the oldest one still showing"
        );
        changed = true;
    }

    changed
}

/// The engine's original rule, kept whole for the lever: ONE cue on screen,
/// latest start wins, and the arrival of a cue ends whatever was showing
/// regardless of how much of its window is left.
///
/// `fcasttextoverlay` holds exactly one text buffer and a newer one replaces
/// it; every timing expectation written before the multi-active change was
/// written against this.
fn evaluate_single_active(state: &mut State, rt: gst::ClockTime) -> bool {
    // Latest-start-wins: only the last of the due run survives it.
    let candidate = take_due(state, rt).pop();

    let canvas = state.canvas;
    let video_rect = state.video_rect;
    let style = state.style.clone();
    let rate = state.video_segment.as_ref().map_or(1.0, |s| s.rate());
    match candidate {
        Some(cue) => {
            if state.active.first().is_some_and(|act| act.cue == cue) {
                false
            } else {
                state.active.clear();
                state
                    .active
                    .push(activate(cue, canvas, video_rect, style, rate));
                true
            }
        }
        None => {
            let expired = state
                .active
                .first()
                .is_some_and(|act| cue_is_too_old(act.cue.end_rt, rt));
            if expired {
                state.active.clear();
            }
            expired
        }
    }
}

/// Turn a scheduled cue into an active one: its raster key, plus (for karaoke)
/// the reveal thresholds it will re-key on.
fn activate(
    cue: CueInput,
    canvas: (u32, u32),
    video_rect: Option<VideoRect>,
    style: Arc<CueStyle>,
    rate: f64,
) -> Active {
    let (steps, step) = reveal_plan(&cue, rate);
    Active {
        key: RasterKey {
            text: cue.text.clone(),
            format: cue.format.clone(),
            canvas,
            style,
            video_rect,
            step,
        },
        steps,
        cue,
        raster: RasterState::Pending,
    }
}

/// The reveal schedule a cue activates with: its karaoke step times and the
/// step it starts on.
///
/// Split out of [`activate`] so the boundary prefetch in
/// [`CueEngine::resolve_raster`] can compute the key a cue WILL activate under
/// without activating it. The two must not drift: a prefetch filed under a
/// different key from the one activation asks for warms nothing and the cue
/// still blinks.
fn reveal_plan(cue: &CueInput, rate: f64) -> (Vec<gst::ClockTime>, usize) {
    // Karaoke, cue-IR only: reveal times are absolute on the media timeline and
    // the engine's clock is running time, so they are anchored by the buffer's
    // pts and scaled by the segment rate.
    match cue.format.ir() {
        Some(ir) => {
            let pts_start = match &cue.format {
                TextFormat::CueIr { pts_start, .. } => *pts_start,
                _ => None,
            };
            let steps = cue_ir::reveal_steps(ir, cue.start_rt, pts_start, rate);
            // Timed spans without a usable anchor (no pts, a non-forward rate):
            // show the whole cue at once, per the documented `pts_start`
            // contract. A pinned step of 0 would hide every timed span forever
            // instead.
            let step = if steps.is_empty() && cue_ir::has_reveals(ir) {
                usize::MAX
            } else {
                0
            };
            (steps, step)
        }
        None => (Vec::new(), 0),
    }
}

/// Karaoke: a raster is keyed on how many reveal steps the clock has passed;
/// crossing one re-keys it (usually a cache hit thanks to the prefetch in
/// [`CueEngine::resolve_raster`]). If the replacement is not ready yet, the
/// previous step keeps showing (`Stale`). Otherwise the whole line blinks off
/// for a frame at every syllable on the first pass.
///
/// Per cue, since each cue on screen carries its own reveal schedule.
fn advance_karaoke(state: &mut State, rt: gst::ClockTime) -> bool {
    let mut changed = false;
    for active in state.active.iter_mut() {
        if active.steps.is_empty() {
            continue;
        }
        let step = active.steps.partition_point(|s| *s <= rt);
        if step != active.key.step {
            active.key.step = step;
            let raster = std::mem::replace(&mut active.raster, RasterState::Pending);
            active.raster = raster.into_stale();
            // What is on screen only changes if we stopped showing pixels; a
            // Stale raster is the same image until the replacement lands.
            changed |= !matches!(active.raster, RasterState::Stale(_));
        }
    }
    changed
}

/// Most-recently-used-last, capacity [`RASTER_CACHE_LIMIT`].
#[derive(Default)]
struct RasterCache {
    entries: Vec<(RasterKey, Arc<Raster>)>,
}

impl RasterCache {
    fn get(&mut self, key: &RasterKey) -> Option<Arc<Raster>> {
        let idx = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(idx);
        let raster = entry.1.clone();
        self.entries.push(entry);
        Some(raster)
    }

    fn insert(&mut self, key: RasterKey, raster: Arc<Raster>) {
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(idx);
        }
        self.entries.push((key, raster));
        while self.entries.len() > RASTER_CACHE_LIMIT {
            self.entries.remove(0);
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Default)]
struct Slot {
    /// Newest-wins by ENGINE SEQUENCE NUMBER, not arrival order (see
    /// [`CueEngine::request_raster`]): older requests are stale by
    /// construction. The instant is when the request was made, so the worker
    /// can report what the wait cost (see [`CueEngine::raster_latencies`]).
    request: Option<(u64, RasterKey, Instant)>,
    warm: bool,
    quit: bool,
    /// The worker that drained this inbox has retired, so nothing will ever
    /// read it again. Set by the worker under BOTH locks (see
    /// [`retire_raster_worker`]), which is what makes it impossible for a
    /// submitter to lose work to a retirement: a submitter holding this lock
    /// blocks the retirement, and a submitter that arrives after one sees this
    /// flag and asks for a fresh inbox.
    retired: bool,
}

#[derive(Default)]
struct Inbox {
    slot: Mutex<Slot>,
    cv: Condvar,
}

struct WorkerHandle {
    inbox: Arc<Inbox>,
}

impl WorkerHandle {
    fn stop(&self) {
        let mut slot = self.inbox.slot.lock();
        slot.quit = true;
        self.inbox.cv.notify_all();
    }
}

/// How long this worker should wait with nothing to do before it offers to
/// retire. Reads the engine's setting each time so a test can shorten it, and
/// answers the default when the engine is already gone (the next wait ends the
/// thread anyway).
fn worker_idle(shared: &Weak<Shared>) -> Duration {
    shared
        .upgrade()
        .map_or(WORKER_IDLE_TIMEOUT, |shared| shared.worker_idle())
}

/// Retire the raster worker if it is STILL idle. Answers whether the caller
/// should end its thread.
///
/// # The handshake, once, for both workers
///
/// Both engine workers are lazily spawned, and this is what makes them lazily
/// unspawned: a thread parked on a condvar for the lifetime of a receiver that
/// has shown its last subtitle costs a stack for nothing, and a device that
/// plays item after item accumulates one per sink that ever showed a cue.
///
/// The only thing that could go wrong here is LOSING WORK (a submitter that
/// hands a request to an inbox nobody will ever read again), and the flag plus
/// the lock order is the whole answer to it:
///
///  * a submitter takes the inbox lock to write, and this takes the same lock
///    to retire, so the two cannot interleave. Whoever gets there first wins:
///    if the submitter does, the check below sees the work and abandons the
///    retirement; if the retirement does, `retired` is set before the submitter
///    can look at the slot;
///  * `retired` is set in the SAME critical section that clears the engine's
///    handle, so a submitter holding a stale `Arc` sees the flag, and every
///    submitter that arrives afterwards gets a freshly spawned worker from the
///    now-empty handle. `CueEngine::with_raster_inbox` and
///    `CueEngine::submit_bitmap` are the two places that read it;
///  * the handle is cleared only if it still points at THIS inbox, so a worker
///    that somehow outlived its own replacement cannot unregister it.
///
/// Lock order is the engine's own: the handle first, then the inbox. Neither
/// worker ever takes the state lock while holding the inbox lock, which is what
/// keeps this pair out of the engine's other pair.
fn retire_raster_worker(shared: &Weak<Shared>, inbox: &Arc<Inbox>) -> bool {
    // The engine is gone: nothing will ever ask again.
    let Some(shared) = shared.upgrade() else {
        return true;
    };
    let mut handle = shared.worker.lock();
    let mut slot = inbox.slot.lock();
    if slot.quit {
        return true;
    }
    if slot.request.is_some() || slot.warm {
        return false;
    }
    if handle
        .as_ref()
        .is_some_and(|live| Arc::ptr_eq(&live.inbox, inbox))
    {
        *handle = None;
    }
    slot.retired = true;
    debug!("the cue raster worker retired after an idle period");
    true
}

/// The bitmap decode worker's half of the handshake documented on
/// [`retire_raster_worker`].
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

fn worker_main(shared: Weak<Shared>, inbox: Arc<Inbox>) {
    // Built on this thread, on first use, and never moved off it: pango and
    // cairo objects stay thread-local, and the expensive fontconfig walk
    // happens here rather than on a streaming or event-loop thread.
    let mut ctx: Option<RasterCtx> = None;

    'work: loop {
        let (request, warm) = {
            let mut slot = inbox.slot.lock();
            loop {
                if slot.quit {
                    break 'work;
                }
                if slot.request.is_some() || slot.warm {
                    break;
                }
                // A TIMED wait, so a sink that has stopped showing cues stops
                // paying for a thread. Everything about the retirement is in
                // `retire_worker`; what matters here is that it happens with
                // this lock released and is re-checked under it.
                if inbox
                    .cv
                    .wait_for(&mut slot, worker_idle(&shared))
                    .timed_out()
                {
                    drop(slot);
                    if retire_raster_worker(&shared, &inbox) {
                        return;
                    }
                    slot = inbox.slot.lock();
                }
            }
            (slot.request.take(), std::mem::take(&mut slot.warm))
        };

        if warm {
            let started = Instant::now();
            let ctx = ctx.get_or_insert_with(RasterCtx::new);
            ctx.warm();
            let elapsed = started.elapsed();
            info!(?elapsed, "cue raster fontmap warmed");
            if let Some(shared) = shared.upgrade() {
                shared
                    .warm_nanos
                    .store(elapsed.as_nanos().max(1) as u64, Ordering::Release);
            }
        }

        let Some((_seq, key, requested_at)) = request else {
            continue;
        };
        let Some(shared) = shared.upgrade() else {
            break;
        };

        // The engine re-requests a Pending key every frame while a raster is in
        // flight, so after publishing, the slot often holds a stale copy of the
        // request just completed. Serve it from the cache instead of rendering
        // the same pixels twice (which halves worker throughput exactly when
        // rasters are slowest). Cache hits are not recorded in the latency
        // window: it measures the rasterizer.
        let cached = shared.cache.lock().get(&key);
        if let Some(raster) = cached {
            let changed = publish(&shared, key, Some(raster));
            let engine = CueEngine { shared };
            if changed {
                engine.mark_changed();
            }
            // The rest of the stack, if there is one (see `pump_rasters`).
            engine.pump_rasters();
            continue;
        }

        // A panic inside the render stack (a parley/vello assert, a geometry
        // guard someone forgets) must not kill this thread: the handle would
        // still be held, every future request would go to a dead worker, and
        // subtitles would silently stop for the process lifetime. Convert
        // panics into a Failed raster and rebuild the (possibly poisoned)
        // contexts on the next request.
        let rendered = {
            let ctx = ctx.get_or_insert_with(RasterCtx::new);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.render(&key)))
        };
        let raster = match rendered {
            Ok(raster) => raster.map(Arc::new),
            Err(_) => {
                warn!(
                    step = key.step,
                    "cue raster panicked; rebuilding the raster context"
                );
                ctx = None;
                None
            }
        };
        record_raster_latency(&shared, requested_at.elapsed());
        let changed = publish(&shared, key, raster);
        let engine = CueEngine { shared };
        if changed {
            engine.mark_changed();
        }
        // ASK FOR THE NEXT ONE. With several cues on screen the frame path
        // hands over one key at a time (the inbox is a newest-wins slot), so
        // the worker feeding itself is what fills a stack in without waiting
        // for the next frame, and while PAUSED there is no next frame. It
        // cannot spin: a key that has been published is no longer wanted, and a
        // key that failed is remembered as Failed.
        engine.pump_rasters();
    }
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
    let mut decoder: Option<(BitmapFormat, Box<dyn SubpicDecoder>)> = None;
    let mut applied_codec_data: Option<gst::Buffer> = None;
    let mut applied_size: Option<(u32, u32)> = None;
    let mut current_epoch: Option<u64> = None;
    // The `(format, epoch)` the "no decoder" warning was last raised for. The
    // COUNTER stays per packet, since it measures the defect, but the log line
    // does not: an unwired format would otherwise print once per packet for the
    // whole stream.
    let mut warned_undecodable: Option<(BitmapFormat, u64)> = None;

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
                    .wait_for(&mut slot, worker_idle(&shared))
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
            let engine = CueEngine { shared };
            engine.mark_changed();
        }
    }
}

/// Build the decoder for a format. Production reads the implemented set from
/// [`crate::subpic::decoder_for`]; tests may install their own factory through
/// [`CueEngine::set_decoder_factory`].
fn build_decoder(shared: &Arc<Shared>, format: BitmapFormat) -> Option<Box<dyn SubpicDecoder>> {
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
    if state.bitmap_epoch != epoch {
        debug!(
            epoch,
            current = state.bitmap_epoch,
            sets = updates.len(),
            "dropping bitmap sets decoded before a reset"
        );
        return false;
    }

    for update in updates {
        // Insert-sorted, defensively: the FIFO worker publishes in stream
        // order, so this is normally a push to the back. `partition_point`
        // finds that in log time instead of walking the backlog.
        let at = state
            .bitmap_pending
            .partition_point(|queued| queued.start_rt <= update.start_rt);
        state.bitmap_pending.insert(at, update);
    }
    trim_bitmap_pending(&mut state, &shared.bitmap_dropped_sets);

    // A set that covers the frame already on screen becomes visible without a
    // new frame: the paused path, identical to the text one.
    match state.last_shown_rt {
        Some(rt) => evaluate_bitmap(&mut state, rt),
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

/// How many raster costs are kept for [`CueEngine::raster_latencies`].
const RASTER_LATENCY_WINDOW: usize = 256;

/// Record what one raster cost, oldest dropped. Bounded because this runs for
/// the whole life of a sink and nothing ever drains it.
fn record_raster_latency(shared: &Arc<Shared>, cost: Duration) {
    let mut latencies = shared.raster_latencies.lock();
    if latencies.len() == RASTER_LATENCY_WINDOW {
        latencies.pop_front();
    }
    latencies.push_back(cost);
}

/// Hand a finished raster to the engine. Returns whether it changed what is on
/// screen (a raster for a cue that has since been replaced only warms the
/// cache). Takes the state lock without holding the inbox lock, since the
/// worker must never hold both, since the engine takes them in the opposite
/// order.
fn publish(shared: &Arc<Shared>, key: RasterKey, raster: Option<Arc<Raster>>) -> bool {
    if let Some(raster) = raster.as_ref() {
        shared.cache.lock().insert(key.clone(), raster.clone());
    }

    let mut state = shared.state.lock();
    let mut changed = false;
    // EVERY active cue under this key, not the first: two cues on screen can
    // legitimately share one (the same line delivered at two times renders to
    // the same pixels), and serving only one of them would leave the other
    // Pending against a key already answered.
    for active in state.active.iter_mut() {
        if active.key != key
            || !matches!(active.raster, RasterState::Pending | RasterState::Stale(_))
        {
            continue;
        }
        active.raster = match raster.as_ref() {
            Some(raster) => RasterState::Ready(raster.clone()),
            None => RasterState::Failed,
        };
        changed |= matches!(active.raster, RasterState::Ready(_));
    }
    changed
}

/// Append a rounded rectangle to `cr`'s current path.
///
/// cairo has no rounded-rect primitive, so it is four arcs joined by the
/// implicit lines `arc` draws from the current point. The radius is clamped to
/// half the shorter side, which is what keeps a tiny box (a one-character cue
/// at a small window size) from turning inside out.
fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// The worker's rasterizer state: BOTH arms, on the one raster thread.
///
/// pango/cairo objects are thread-local by construction (pangocairo's default
/// fontmap is per-thread) and parley's contexts cache font data and layouts, so
/// each is built once here and kept for the thread's lifetime.
struct RasterCtx {
    context: pango::Context,
    // Kept alive for the context's lifetime; pangocairo's default fontmap is
    // per-thread, so this one belongs to the raster thread alone.
    _fontmap: pango::FontMap,
    /// The cue-IR arm, built on first use: a pipeline that never carries a
    /// cue-IR stream never pays for parley's font collection.
    cue_ir: Option<cue_ir::RasterCtx>,
}

impl RasterCtx {
    fn new() -> Self {
        let fontmap = pangocairo::FontMap::default();
        let context = fontmap.create_context();
        Self {
            context,
            _fontmap: fontmap,
            cue_ir: None,
        }
    }

    /// Force the font stacks to actually load: a throwaway layout per arm,
    /// measured and rasterized exactly like a real cue.
    ///
    /// Both arms warm here, on the dedicated thread, at sink construction,
    /// never on a streaming or event-loop thread and never mid-cue.
    /// fontconfig's walk is the expensive one. parley's font enumeration is
    /// much cheaper but still not free.
    fn warm(&mut self) {
        let key = RasterKey {
            text: "Warming the font stack".to_owned(),
            format: TextFormat::Utf8,
            canvas: (640, 360),
            style: Arc::new(CueStyle::default()),
            video_rect: None,
            step: 0,
        };
        if self.render(&key).is_none() {
            warn!("cue raster warm-up produced no pixels");
        }
        self.cue_ir
            .get_or_insert_with(cue_ir::RasterCtx::new)
            .warm();
    }

    fn render(&mut self, key: &RasterKey) -> Option<Raster> {
        // The cue-IR arm: styled spans, per-cue placement, karaoke. Everything
        // else goes to pango below, unchanged.
        if let TextFormat::CueIr { ir, .. } = &key.format {
            let out = self
                .cue_ir
                .get_or_insert_with(cue_ir::RasterCtx::new)
                .render(ir, &key.style, key.canvas, key.video_rect, key.step)?;
            return Some(Raster {
                pixels: Arc::new(out.pixels),
                width: out.width,
                height: out.height,
                x: out.x,
                y: out.y,
            });
        }

        let (canvas_w, canvas_h) = key.canvas;
        if canvas_w == 0 || canvas_h == 0 {
            return None;
        }

        let font_px = (canvas_h as f64 * layout::FONT_HEIGHT_FRACTION).max(layout::MIN_FONT_PX);

        // THE HOUSE STYLE GOVERNS BOTH ARMS. `key.style` is the same
        // `cue_ir::CueStyle` the parley/vello rasterizer reads, so a
        // `set_style` call moves the two together and neither can drift into
        // its own look. Only the readability knobs are read here (box,
        // outline); the rest of this arm's geometry stays the constants in
        // `layout`, whose values the style's defaults already match exactly.
        let house = &*key.style;
        let outline = house.outline;
        let outline_px = outline
            .map(|o| (font_px * o.width_fraction as f64).max(1.0))
            .unwrap_or(0.0);
        // The tinted rounded box behind the cue, in pixels. `edge_softness` is
        // NOT honoured on this arm: the vello rasterizer feathers the rim with
        // `fill_blurred_rounded_rect` and cairo has no equivalent primitive, so
        // the box is drawn hard-edged here. The default softness is 0.0, so the
        // two arms agree unless someone asks for a feathered box.
        let background = house.background;
        let box_pad = background.map_or(0.0, |b| (b.padding as f64 * font_px).max(0.0));
        let box_radius = background.map_or(0.0, |b| (b.corner_radius as f64 * font_px).max(0.0));
        // With a box the padding has to cover it, plus a pixel so the rounded
        // corners have somewhere to fade out; without one, this is byte-for-byte
        // the padding this arm always used.
        let pad = match background {
            Some(_) => outline_px.max(box_pad).ceil() as i32 + 1,
            None => outline_px.ceil() as i32,
        };
        let wrap_px = ((canvas_w as f64 * layout::WRAP_WIDTH_FRACTION) as i32).max(1);

        let layout = pango::Layout::new(&self.context);
        let mut font = pango::FontDescription::new();
        font.set_family(layout::FONT_FAMILY);
        font.set_weight(pango::Weight::Bold);
        font.set_absolute_size(font_px * pango::SCALE as f64);
        layout.set_font_description(Some(&font));
        layout.set_alignment(pango::Alignment::Center);
        layout.set_wrap(pango::WrapMode::WordChar);
        layout.set_width(wrap_px * pango::SCALE);

        match key.format {
            // Returned above; the cue-IR arm never reaches pango.
            TextFormat::CueIr { .. } => return None,
            TextFormat::Utf8 => layout.set_text(&key.text),
            TextFormat::PangoMarkup => match pango::parse_markup(&key.text, '\0') {
                Ok((attrs, text, _)) => {
                    layout.set_text(text.as_str());
                    layout.set_attributes(Some(&attrs));
                }
                Err(err) => {
                    // Never drop a cue for bad markup: show its WORDS.
                    // `Layout::set_markup` would silently leave the layout
                    // empty (plus a g_warning), which is why parsing is done
                    // here instead.
                    //
                    // The text is sanitized rather than shown raw. Handing
                    // `key.text` straight to `set_text` puts the markup on
                    // screen as literal angle brackets: a WebVTT `<v Speaker>`
                    // cue is rejected by pango (voice spans are kept in the
                    // pango-markup output on purpose, to stay byte-identical to
                    // the C subparse) and the viewer reads
                    // `<v Voice1>Hello there</v>`.
                    //
                    // This is NOT only the parser's problem to fix upstream:
                    // matroskademux emits `format=pango-markup` directly for
                    // S_TEXT/UTF8 tracks, so cues reach this arm that never
                    // pass through a parser element at all and can never be
                    // switched to cue-ir.
                    warn!(%err, "cue markup did not parse, rendering its text without the markup");
                    layout.set_text(&cue_ir::plain_text_of_markup(&key.text));
                }
            },
        }

        // With centred alignment every line is centred inside `wrap_px`, so the
        // widest line determines the tight texture width and the whole layout
        // is shifted left by the widest line's left inset.
        let mut text_w = 0;
        for line in layout.lines() {
            let (_, logical) = line.pixel_extents();
            text_w = text_w.max(logical.width());
        }
        let text_h = layout.pixel_size().1;
        if text_w <= 0 || text_h <= 0 {
            debug!(text = key.text, "cue laid out to nothing");
            return None;
        }

        let surface_w = text_w + 2 * pad;
        let surface_h = text_h + 2 * pad;
        if surface_w > layout::MAX_RASTER_PX || surface_h > layout::MAX_RASTER_PX {
            warn!(surface_w, surface_h, "cue raster too large, skipping");
            return None;
        }

        let surface = match cairo::ImageSurface::create(cairo::Format::ARgb32, surface_w, surface_h)
        {
            Ok(surface) => surface,
            Err(err) => {
                warn!(%err, surface_w, surface_h, "cue surface allocation failed");
                return None;
            }
        };

        let draw = |surface: &cairo::ImageSurface| -> Result<(), cairo::Error> {
            let cr = cairo::Context::new(surface)?;
            let origin_x = (pad - (wrap_px - text_w) / 2) as f64;
            let origin_y = pad as f64;

            // The readability box first, under everything: the cue's ink
            // (which starts at `pad`, since the layout is shifted left by the
            // centring inset) grown by the box padding on every side.
            if let Some(bg) = background {
                let [r, g, b, a] = bg.color;
                rounded_rect(
                    &cr,
                    pad as f64 - box_pad,
                    pad as f64 - box_pad,
                    text_w as f64 + 2.0 * box_pad,
                    text_h as f64 + 2.0 * box_pad,
                    box_radius,
                );
                cr.set_source_rgba(
                    r as f64 / 255.0,
                    g as f64 / 255.0,
                    b as f64 / 255.0,
                    a as f64 / 255.0,
                );
                cr.fill()?;
            }

            // Then the outline, then the fill on top of it.
            if outline_px > 0.0 {
                let [r, g, b, a] = outline.map_or([0, 0, 0, 217], |o| o.color);
                cr.move_to(origin_x, origin_y);
                pangocairo::functions::layout_path(&cr, &layout);
                cr.set_source_rgba(
                    r as f64 / 255.0,
                    g as f64 / 255.0,
                    b as f64 / 255.0,
                    a as f64 / 255.0,
                );
                cr.set_line_width(outline_px);
                cr.set_line_join(cairo::LineJoin::Round);
                cr.set_line_cap(cairo::LineCap::Round);
                cr.stroke()?;
            }

            cr.move_to(origin_x, origin_y);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            pangocairo::functions::show_layout(&cr, &layout);
            Ok(())
        };
        if let Err(err) = draw(&surface) {
            warn!(%err, "cue rasterization failed");
            return None;
        }

        let mut surface = surface;
        let stride = surface.stride() as usize;
        let data = match surface.data() {
            Ok(data) => data,
            Err(err) => {
                warn!(%err, "cue surface data unavailable");
                return None;
            }
        };
        let pixels = argb32_to_rgba(&data, stride, surface_w as u32, surface_h as u32);
        drop(data);

        // Bottom-centre, a margin up from the bottom edge.
        let margin = (canvas_h as f64 * layout::BOTTOM_MARGIN_FRACTION) as i32;
        let x = ((canvas_w as i32 - surface_w) / 2).max(0);
        let y = (canvas_h as i32 - margin - surface_h).max(0);

        Some(Raster {
            pixels: Arc::new(pixels),
            width: surface_w as u32,
            height: surface_h as u32,
            x,
            y,
        })
    }
}

/// Cairo's ARGB32 is native-endian premultiplied; overlays are tightly packed
/// straight-alpha RGBA (`Overlay::pixels`, uploaded as `PL_ALPHA_INDEPENDENT`).
fn argb32_to_rgba(data: &[u8], stride: usize, width: u32, height: u32) -> Vec<u8> {
    #[cfg(target_endian = "little")]
    const IDX: [usize; 4] = [2, 1, 0, 3]; // R, G, B, A within a BGRA byte quad
    #[cfg(target_endian = "big")]
    const IDX: [usize; 4] = [1, 2, 3, 0];

    let row_bytes = width as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let line = &data[row * stride..row * stride + row_bytes];
        for px in line.as_chunks::<4>().0 {
            let alpha = px[IDX[3]];
            if alpha == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let unpremultiply = |value: u8| -> u8 {
                let scaled = (value as u32 * 255 + alpha as u32 / 2) / alpha as u32;
                scaled.min(255) as u8
            };
            out.push(unpremultiply(px[IDX[0]]));
            out.push(unpremultiply(px[IDX[1]]));
            out.push(unpremultiply(px[IDX[2]]));
            out.push(alpha);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> gst::ClockTime {
        gst::ClockTime::from_mseconds(value)
    }

    fn cue(text: &str, start: u64, duration: u64) -> CueInput {
        CueInput {
            format: TextFormat::Utf8,
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
            .active
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

    // ---- timing, ported from fcasttextoverlay's harness tests ----

    /// `test_basic_passthrough` (`fcasttextoverlay.rs:1036`): a frame with no
    /// cue in flight carries nothing.
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
    /// Latest-start-wins survives as ORDERING (the later cue is above the
    /// earlier one) and `showing()` still answers with the topmost cue, which
    /// is why every test that predates this one reads the same as it did.
    #[test]
    fn two_cues_covering_the_frame_both_show_earliest_at_the_bottom() {
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
            "the latest start is the top of the stack"
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
    /// belongs below it, not on top: the stack is ordered by start time, not by
    /// arrival.
    #[test]
    fn a_late_delivered_earlier_cue_joins_the_stack_underneath() {
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
            engine.shared.state.lock().pending.len(),
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
            format: TextFormat::Utf8,
            text: "bottom".to_owned(),
            start_rt: ms(0),
            end_rt: Some(ms(0)),
        });
        assert_eq!(engine.shared.state.lock().pending.len(), 0);
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
            format: TextFormat::Utf8,
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
        assert_eq!(engine.shared.state.lock().last_shown_rt, None);
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
        assert_eq!(engine.shared.state.lock().last_shown_rt, Some(ms(10)));
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
        assert_eq!(engine.shared.state.lock().pending.len(), CUES as usize);

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
        assert_eq!(state.pending.len(), PENDING_LIMIT);
        // The survivors are the SOONEST ones: the next cue up is still there,
        // and it is the four furthest out that went.
        assert_eq!(state.pending.front().unwrap().text, "cue 0");
        assert_eq!(
            state.pending.back().unwrap().text,
            format!("cue {}", PENDING_LIMIT - 1)
        );
    }

    /// The eviction ORDER, at the one place it is decided: cues already past
    /// the playhead cost nothing to give up, so they go before any cue that
    /// could still be shown.
    #[test]
    fn trimming_spends_the_past_before_it_spends_the_future() {
        let mut state = State {
            last_shown_rt: Some(ms(50_000)),
            ..State::default()
        };
        // Over the limit by less than the past holds, so spending the past
        // alone is enough and no future cue need be touched.
        let past = 8usize;
        for index in 0..past as u64 {
            state
                .pending
                .push_back(cue(&format!("past {index}"), index * 100, 50));
        }
        let future = PENDING_LIMIT - past + 2;
        for index in 0..future as u64 {
            state
                .pending
                .push_back(cue(&format!("future {index}"), 60_000 + index * 100, 50));
        }
        assert!(state.pending.len() > PENDING_LIMIT);
        let dropped = AtomicU64::new(0);
        trim_pending(&mut state, &dropped);

        // Only the past was spent, and every future cue survived.
        assert_eq!(dropped.load(Ordering::Relaxed), past as u64);
        assert_eq!(state.pending.len(), future);
        assert!(
            state
                .pending
                .iter()
                .all(|cue| cue.text.starts_with("future")),
            "a future cue was dropped while the past was still holding slots"
        );
    }

    /// Converter output carries every cue twice: a zero-length
    /// record (`start == end`) and then the real one. Both are faithful
    /// deliveries; the engine is where they become one cue.
    #[test]
    fn a_zero_length_twin_merges_into_the_cue_it_repeats() {
        let engine = CueEngine::new();
        let degenerate = CueInput {
            format: TextFormat::Utf8,
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
            assert_eq!(state.pending.len(), 1, "the twins are one cue");
            assert_eq!(state.pending[0].end_rt, Some(ms(4185)));
        }
        // And it is shown for its REAL window rather than expiring on arrival.
        assert_eq!(advance(&engine, ms(3000)).as_deref(), Some("twin"));

        // The other order merges too: a degenerate copy arriving second must
        // not shorten what it repeats.
        let engine = CueEngine::new();
        engine.submit(real);
        engine.submit(degenerate);
        let state = engine.shared.state.lock();
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].end_rt, Some(ms(4185)));
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
            engine.shared.state.lock().pending.len(),
            0,
            "the cue on screen was queued a second time"
        );
        assert_eq!(showing(&engine).as_deref(), Some("showing"));

        // A zero-length twin of it lands the same way, and cannot shorten it.
        engine.submit(CueInput {
            format: TextFormat::Utf8,
            text: "showing".to_owned(),
            start_rt: ms(1000),
            end_rt: Some(ms(1000)),
        });
        assert_eq!(engine.shared.state.lock().pending.len(), 0);
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
        let after_first = engine.shared.state.lock().pending.len();
        assert_eq!(after_first, 500);

        burst();
        assert_eq!(
            engine.shared.state.lock().pending.len(),
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

    fn raster_key(text: &str, format: TextFormat, canvas: (u32, u32)) -> RasterKey {
        RasterKey {
            text: text.to_owned(),
            format,
            canvas,
            style: Arc::new(CueStyle::default()),
            video_rect: None,
            step: 0,
        }
    }

    /// pangocairo draws into an image surface: no display server, no GL, no
    /// window. These tests pass under `env -u DISPLAY -u WAYLAND_DISPLAY`.
    #[test]
    fn raster_smoke() {
        let mut ctx = RasterCtx::new();
        let canvas = (1920, 1080);
        let raster = ctx
            .render(&raster_key("Hello, subtitles", TextFormat::Utf8, canvas))
            .expect("a plain cue rasterizes");

        let (width, height) = raster.size();
        assert!(width > 0 && height > 0);
        assert_eq!(raster.pixels().len(), width as usize * height as usize * 4);
        // Sized against the canvas, not the video, and it fits inside it. One
        // line is roughly `FONT_HEIGHT_FRACTION` of the canvas height plus the
        // outline padding, 49px of font at 1080p.
        assert!(width <= canvas.0, "raster {width} wider than the canvas");
        assert!(
            (40..160).contains(&height),
            "unexpected line height {height}"
        );

        // Bottom-centre placement, inside the canvas.
        let (x, y) = raster.position();
        assert!(x >= 0 && x as u32 + width <= canvas.0);
        assert!(y as u32 > canvas.1 / 2);
        assert!(y as u32 + height <= canvas.1);

        // Actual glyphs: some pixels are opaque, some are fully transparent.
        let alphas: Vec<u8> = raster
            .pixels()
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
            ctx.render(&raster_key("", TextFormat::Utf8, (1920, 1080)))
                .is_none()
        );
    }

    #[test]
    fn markup_renders_differently_from_the_same_source_as_utf8() {
        let mut ctx = RasterCtx::new();
        let canvas = (1280, 720);
        let source = "<i>Bonjour</i>";

        let markup = ctx
            .render(&raster_key(source, TextFormat::PangoMarkup, canvas))
            .expect("markup rasterizes");
        let utf8 = ctx
            .render(&raster_key(source, TextFormat::Utf8, canvas))
            .expect("utf8 rasterizes");

        // The utf8 rendering shows the tags literally, so it is wider.
        assert_ne!(markup.size(), utf8.size());
        assert!(utf8.size().0 > markup.size().0);
    }

    #[test]
    fn invalid_markup_falls_back_to_plain_text_instead_of_dropping_the_cue() {
        let mut ctx = RasterCtx::new();
        let canvas = (1280, 720);
        let broken = "<b>unclosed";

        let fallback = ctx
            .render(&raster_key(broken, TextFormat::PangoMarkup, canvas))
            .expect("a cue is never dropped for bad markup");
        // The fallback shows the WORDS, not the source. This assertion used to
        // compare against the raw string rendered as utf8 -- i.e. it pinned
        // "put the markup on screen", which is the defect the sanitizer fixes.
        let words = ctx
            .render(&raster_key("unclosed", TextFormat::Utf8, canvas))
            .expect("utf8 rasterizes");
        let raw = ctx
            .render(&raster_key(broken, TextFormat::Utf8, canvas))
            .expect("utf8 rasterizes");

        assert_eq!(fallback.size(), words.size());
        assert_eq!(fallback.pixels(), words.pixels());
        assert_ne!(
            fallback.pixels(),
            raw.pixels(),
            "the fallback must not render the markup source"
        );
    }

    /// THE FIELD CASE: a WebVTT voice span reaches this arm as
    /// `<v Voice1>...</v>`, pango rejects it ("expected a `=` after attribute
    /// name"), and before the sanitizer the viewer read the tags themselves.
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

        // Precondition: pango really does refuse this, so the test is about
        // the fallback and not about pango quietly coping.
        assert!(
            pango::parse_markup(voiced, '\0').is_err(),
            "the premise is that pango rejects a voice span"
        );

        let rendered = ctx
            .render(&raster_key(voiced, TextFormat::PangoMarkup, canvas))
            .expect("a cue is never dropped for bad markup");
        let words = ctx
            .render(&raster_key(
                "Hello there and more",
                TextFormat::Utf8,
                canvas,
            ))
            .expect("utf8 rasterizes");
        let with_tags = ctx
            .render(&raster_key(voiced, TextFormat::Utf8, canvas))
            .expect("utf8 rasterizes");

        assert_eq!(
            rendered.pixels(),
            words.pixels(),
            "the viewer must read only the words"
        );
        // The tags are strictly wider than the words, so this is also a
        // guard against the two rasters coinciding by accident.
        assert!(with_tags.size().0 > words.size().0);
        assert_ne!(rendered.pixels(), with_tags.pixels());
    }

    // ---- the default readability box ----

    /// A pixel of a raster, as straight RGBA.
    fn px_at(raster: &Raster, x: u32, y: u32) -> [u8; 4] {
        let (w, _) = raster.size();
        let at = ((y * w + x) * 4) as usize;
        raster.pixels()[at..at + 4].try_into().expect("in bounds")
    }

    /// A `CueStyle`-keyed raster key, for the box tests.
    fn styled_key(
        text: &str,
        format: TextFormat,
        canvas: (u32, u32),
        style: CueStyle,
    ) -> RasterKey {
        RasterKey {
            text: text.to_owned(),
            format,
            canvas,
            style: Arc::new(style),
            video_rect: None,
            step: 0,
        }
    }

    /// The cue-IR form of the same plain text.
    fn ir_format(text: &str) -> TextFormat {
        TextFormat::CueIr {
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
            ("pango", TextFormat::Utf8),
            ("cue-ir", ir_format("Hello, subtitles")),
        ] {
            let raster = ctx
                .render(&styled_key(
                    "Hello, subtitles",
                    format,
                    canvas,
                    CueStyle::default(),
                ))
                .unwrap_or_else(|| panic!("{arm}: rasterizes"));
            let (_, h) = raster.size();

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
                .pixels()
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
            ("pango", TextFormat::Utf8),
            ("cue-ir", ir_format("Hello, subtitles")),
        ] {
            let boxed = ctx
                .render(&styled_key(
                    "Hello, subtitles",
                    format.clone(),
                    canvas,
                    CueStyle::default(),
                ))
                .unwrap_or_else(|| panic!("{arm}: rasterizes"));
            let bare = ctx
                .render(&styled_key(
                    "Hello, subtitles",
                    format,
                    canvas,
                    CueStyle::outline_only(),
                ))
                .unwrap_or_else(|| panic!("{arm}: rasterizes"));

            // Nothing is painted where the box was.
            let (_, h) = bare.size();
            assert_eq!(
                px_at(&bare, 3, h / 2)[3],
                0,
                "{arm}: no box means nothing behind the text"
            );
            // ...and the cue is physically smaller without the box padding.
            assert!(
                bare.size().0 < boxed.size().0 && bare.size().1 < boxed.size().1,
                "{arm}: the box adds padding, so {:?} must be smaller than {:?}",
                bare.size(),
                boxed.size()
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
        let raster = ctx
            .render(&styled_key(
                "Hello, subtitles",
                TextFormat::CueIr {
                    ir: Arc::new(ir),
                    pts_start: None,
                },
                canvas,
                CueStyle::default(),
            ))
            .expect("rasterizes");

        let (_, h) = raster.size();
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
        let raster = ctx
            .render(&styled_key(
                "Hello, subtitles",
                TextFormat::CueIr {
                    ir: Arc::new(ir),
                    pts_start: None,
                },
                canvas,
                CueStyle::default(),
            ))
            .expect("rasterizes");

        let (_, h) = raster.size();
        let inside = px_at(&raster, 3, h / 2);
        assert!(
            inside[3] > 120 && inside[3] < 200 && inside[0] < 40,
            "a coloured-text cue still gets the black house box, got {inside:?}"
        );
        let red = raster
            .pixels()
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] > 180 && px[1] < 60 && px[2] < 60)
            .count();
        assert!(red > 50, "the text is still red, got {red} px");
    }

    /// The sanitizer parses rather than strips, which is what keeps a
    /// less-than that is NOT a tag: `I <3 you` must survive intact, and
    /// entities must not be traded for a different piece of literal noise.
    #[test]
    fn the_markup_sanitizer_keeps_text_that_only_looks_like_tags() {
        assert_eq!(
            cue_ir::plain_text_of_markup("<v Voice1>Hello there</v> and <c.yellow>yellow</c>"),
            "Hello there and yellow"
        );
        assert_eq!(cue_ir::plain_text_of_markup("I <3 you"), "I <3 you");
        assert_eq!(
            cue_ir::plain_text_of_markup("Tom &amp; Jerry &lt;3"),
            "Tom & Jerry <3"
        );
        assert_eq!(cue_ir::plain_text_of_markup("</stray>text"), "text");
        assert_eq!(cue_ir::plain_text_of_markup("<v Ann>unclosed"), "unclosed");
        // Line breaks are the cue's shape, not markup: a two-line subtitle
        // must still be two lines after sanitizing, or the fallback would
        // silently reflow it into one.
        assert_eq!(
            cue_ir::plain_text_of_markup("<v Ann>first line</v>\n<i>second line</i>"),
            "first line\nsecond line"
        );
    }

    #[test]
    fn a_larger_canvas_rasters_larger_text() {
        let mut ctx = RasterCtx::new();
        let small = ctx
            .render(&raster_key("Same text", TextFormat::Utf8, (640, 360)))
            .expect("rasterizes");
        let large = ctx
            .render(&raster_key("Same text", TextFormat::Utf8, (1920, 1080)))
            .expect("rasterizes");

        assert!(large.size().0 > small.size().0);
        assert!(large.size().1 > small.size().1);
    }

    #[test]
    fn the_raster_cache_is_lru_bounded() {
        let mut cache = RasterCache::default();
        let raster = || {
            Arc::new(Raster {
                pixels: Arc::new(vec![0; 4]),
                width: 1,
                height: 1,
                x: 0,
                y: 0,
            })
        };

        for index in 0..RASTER_CACHE_LIMIT {
            cache.insert(
                raster_key(&format!("{index}"), TextFormat::Utf8, (1, 1)),
                raster(),
            );
        }
        assert_eq!(cache.len(), RASTER_CACHE_LIMIT);

        // Touch the oldest so it is no longer the eviction candidate.
        assert!(
            cache
                .get(&raster_key("0", TextFormat::Utf8, (1, 1)))
                .is_some()
        );
        cache.insert(raster_key("new", TextFormat::Utf8, (1, 1)), raster());

        assert_eq!(cache.len(), RASTER_CACHE_LIMIT);
        assert!(
            cache
                .get(&raster_key("0", TextFormat::Utf8, (1, 1)))
                .is_some(),
            "the touched entry survived"
        );
        assert!(
            cache
                .get(&raster_key("1", TextFormat::Utf8, (1, 1)))
                .is_none(),
            "the least recently used entry was evicted"
        );
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
    /// overlays at two heights, and neither covers the other.
    ///
    /// Both rasters are laid out bottom-centre by the house policy, so they ask
    /// for the SAME strip; what separates them is `active_overlays`, and the
    /// separation has to be visible in the numbers a compositor uploads rather
    /// than in engine state.
    #[test]
    fn two_cues_on_screen_are_two_overlays_at_two_heights() {
        let engine = CueEngine::new();
        engine.set_canvas(1280, 720);
        engine.submit(cue("The bottom line", 0, 8_000));
        engine.submit(cue("The line above it", 1_000, 2_000));

        engine.overlays_for(Some(ms(2_000)));
        assert!(
            wait_for(|| engine.current_overlays().len() == 2),
            "two cues cover this frame and {} overlay(s) reached the screen",
            engine.current_overlays().len()
        );

        let overlays = engine.current_overlays();
        let (bottom, top) = (&overlays[0], &overlays[1]);
        assert!(
            overlays.iter().all(|o| o.space == OverlaySpace::Window),
            "text cues are laid out at display resolution and stay in window space"
        );
        assert!(
            bottom.y > top.y,
            "the earlier-starting cue must be the LOWER one: bottom at y={}, top at y={}",
            bottom.y,
            top.y
        );
        assert!(
            top.y + top.height as i32 <= bottom.y,
            "the two cues overlap vertically: top spans {}..{}, bottom starts at {}",
            top.y,
            top.y + top.height as i32,
            bottom.y
        );
        assert!(
            bottom.y + bottom.height as i32 <= 720,
            "the bottom cue hangs off the canvas"
        );
        // Both are real pictures, not empty strips.
        for overlay in overlays.iter() {
            assert!(
                overlay.pixels.iter().skip(3).step_by(4).any(|a| *a > 0),
                "an overlay carries no ink at all"
            );
        }

        // The one on top ends first. THE OTHER STAYS -- under the single-active
        // rule the top cue's arrival ended the bottom one, so this frame showed
        // nothing at all -- and it stays without re-rastering, in the same
        // allocation and at the same height.
        let bottom_pixels = bottom.pixels.clone();
        let bottom_y = bottom.y;
        engine.overlays_for(Some(ms(4_000)));
        let after = engine.current_overlays();
        assert_eq!(
            after.len(),
            1,
            "the surviving cue went with the expired one"
        );
        assert!(
            Arc::ptr_eq(&after[0].pixels, &bottom_pixels),
            "the surviving cue was re-rastered rather than left alone"
        );
        assert_eq!(
            after[0].y, bottom_y,
            "the surviving cue moved when the one above it left"
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
        println!("fvid-cue-raster fontmap warm-up: {elapsed:?}");
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
            format: TextFormat::Utf8,
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
    #[test]
    fn overlay_pixels_are_the_engines_own_buffer_on_every_call() {
        let engine = big_raster_engine();

        let engine_pixels = {
            let state = engine.shared.state.lock();
            match &state.active.first().expect("a cue is active").raster {
                RasterState::Ready(raster) => raster.pixels.clone(),
                other => panic!("expected a ready raster, got {other:?}"),
            }
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
        bitmap_packet_of(BitmapFormat::Pgs, tag, rt, duration)
    }

    /// The same, for a NAMED format. Every test in this file installs its own
    /// decoder, so the format is only ever a routing key here, except in the
    /// one test that deliberately does not install anything and needs a
    /// format the production table still answers `None` for.
    fn bitmap_packet_of(
        format: BitmapFormat,
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
            engine.shared.state.lock().bitmap_pending.len(),
            200,
            "the trim spent more than the free ones"
        );

        // Then 200 more future sets, and now there is nothing free left to
        // give up, so the FURTHEST FUTURE goes, from the far end.
        engine.submit_bitmap(bitmap_packet(2, 0, None));
        assert!(wait_for(|| engine.bitmap_dropped_sets() == 244));
        {
            let state = engine.shared.state.lock();
            assert_eq!(state.bitmap_pending.len(), BITMAP_PENDING_LIMIT);
            assert_eq!(
                state.bitmap_pending.front().expect("non-empty").start_rt,
                ms(20_000),
                "the trim spent the near future instead of the far future"
            );
            assert_eq!(
                state.bitmap_pending.back().expect("non-empty").start_rt,
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
            .bitmap_pending
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
                .bitmap_pending
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
            engine.shared.state.lock().bitmap_pending.len() as u64,
            SETS,
            "the backlog gave up sets it did not have to"
        );
        assert_eq!(
            bitmap_pending_pixel_bytes(&engine.shared.state.lock()),
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
                state.bitmap_pending.len(),
                1,
                "the bounded set behind it was dropped too"
            );
            assert_eq!(
                state.bitmap_pending.front().expect("non-empty").end_rt,
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
            engine.shared.state.lock().bitmap_pending.is_empty(),
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
        for format in BitmapFormat::ALL {
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
            engine.submit_bitmap(bitmap_packet_of(BitmapFormat::Dvb, 1, index * 10, None));
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
        engine.submit_bitmap(bitmap_packet_of(BitmapFormat::Dvb, 1, 100, None));
        assert!(wait_for(|| engine.bitmap_decode_errors() == 9));
        assert!(
            wait_for(|| warns.load(Ordering::Relaxed) - before == 2),
            "the warning never came back after a reset"
        );
    }
}
