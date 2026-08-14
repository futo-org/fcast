//! The text suites' probe point.
//!
//! Cues leave the crate through [`FcastPlaybin::set_subtitle_consumer`],
//! already resolved to running time, and nothing is composited inside the
//! crate at all. This module is what lets a suite ask the question it actually
//! cares about, "was a cue being shown when this video buffer went past?",
//! against that transport.
//!
//! # The answer is rejoined from two halves
//!
//! The video buffer says WHEN and the cue feed says WHAT, so this module keeps
//! the delivered cues in a window and, for each video buffer, asks whether a
//! cue covers that buffer's running time.
//!
//! That comparison is the renderer's, reproduced at its smallest:
//! `fcast-video`'s cue engine decides exactly this way (a cue is too old once
//! the frame reaches its end, in the future until the frame reaches its
//! start), and `fcasttextoverlay` decided the same way before it. What this
//! module deliberately does NOT reproduce is rasterizing, caching or
//! compositing: those live in `fcast-video` and are tested there. Here the
//! subject is the TRANSPORT, and the question is whether the right cue was
//! delivered, in the right timeline, at a time that covers the frame.
//!
//! # The video tap is not optional
//!
//! Several contracts are about the ABSENCE of a cue ("nothing rendered until
//! position X"), and an absence assertion is vacuous unless something is
//! known to have flowed. The video-buffer tap is what makes it non-vacuous.
//! Its pad is the caller's video sink, the last point every displayed frame
//! passes inside the crate, and, now deleted subtitleoverlay,
//! the only one.
//!
//! # What this module used to be
//!
//! Steps 2-7 carried TWO transports, and every function here dispatched on
//! `consumer_arm()`: an overlay arm that read cues off `subtitle_sink` and
//! rendered glyphs into the luma plane, and this one. The dual dispatch died
//! with the element. The shape of the questions is unchanged, which
//! is why the suites did not move.

#![allow(dead_code)]

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcastplaybin::{FcastPlaybin, SubtitleFeedItem};
use gst::{glib, prelude::*};

/// One tapped video buffer: when it went past, where it was in the media, and
/// whether a cue was being shown on it.
pub type TextSeen = Arc<Mutex<Vec<(Instant, Option<gst::ClockTime>, bool)>>>;

/// A cue the consumer received, in running time.
#[derive(Debug, Clone)]
struct Cue {
    /// When it was delivered, so a suite can scope a claim to cues a
    /// particular gesture produced (see [`TextTap::window_cues`]).
    at: Instant,
    start_rt: gst::ClockTime,
    end_rt: Option<gst::ClockTime>,
    /// The timeline the delivery was resolved against, off the item itself.
    origin: gst::ClockTime,
    text: String,
}

/// A suite-owned log of cue payloads in delivery order, the shape the
/// `tap_overlay_text` family has always used.
pub type CueTap = Arc<Mutex<Vec<(String, Instant)>>>;

/// A cue payload with the running-time ORIGIN of the branch that delivered it.
///
/// Not the cue's own timestamp: the origin is which TIMELINE the cue arrived
/// on, which is what the alignment contracts are about -- a switched-to
/// external that never got its replay renders its cues against the file's
/// origin instead of the item's, and the payload alone cannot tell you that.
///
/// Read off `SubtitleFeedItem::Cue::origin`, which the crate stamps from the
/// very segment the bounds were resolved against. It used to be a pad-sticky
/// read at delivery instant, which answered None for every park-replayed cue
/// (the fresh branch has not carried its segment yet when the join feeds).
pub type PositionTap = Arc<Mutex<Vec<(String, Option<gst::ClockTime>)>>>;

/// The running-time origin a branch's sticky SEGMENT establishes.
pub fn segment_origin(pad: &gst::Pad) -> Option<gst::ClockTime> {
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
    let rate = segment.rate();
    let start = segment.start().unwrap_or(gst::ClockTime::ZERO);
    let base =
        (segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as f64 * rate.abs()) as u64;
    Some(gst::ClockTime::from_nseconds(
        start.nseconds().saturating_sub(base),
    ))
}

#[derive(Default)]
struct Window {
    cues: Vec<Cue>,
    /// Cue texts in delivery order, for suites that assert about payload.
    delivered: Vec<(Instant, String)>,
    clears: Vec<Instant>,
    /// Payload logs handed out by [`tap_cue_payloads`], fanned out to because
    /// the driver takes only ONE consumer per pipeline.
    payload_taps: Vec<CueTap>,
    /// The same, for taps that also want where in the timeline the cue sits.
    position_taps: Vec<PositionTap>,
    /// See [`throttle_cue_delivery`]. `None` is every suite but the one that
    /// arms it.
    delivery_delay: Option<Duration>,
}

/// Pace this pipeline's cue delivery, so a test can hold the attach burst IN
/// FLIGHT on purpose instead of waiting for machine load to do it.
///
/// The consumer branch is UNSYNCED: an external subtitle is parsed and handed
/// over in ONE burst within milliseconds of the branch linking (see [`arm`]).
/// Every window that opens between "the first cue arrived" and "the burst is
/// finished" is therefore shut on an idle machine and wide open under parallel
/// load -- which is exactly the shape a suite cannot test, because the
/// reproduction depends on the scheduler rather than on the test. Slowing the
/// consumer makes that window a PARAMETER: at `delay` per cue a 70-cue file
/// cannot drain inside the quiescence a settled baseline waits for, on any
/// machine, so the burst is provably still running when the test looks.
///
/// Costs the crate nothing when unarmed -- the delay is read off the window
/// the consumer already locks, and the lock is RELEASED before the sleep so a
/// paced consumer never holds the feed against the test thread.
pub fn throttle_cue_delivery(playbin: &FcastPlaybin, delay: Duration) {
    cue_window_for(playbin).lock().unwrap().delivery_delay = Some(delay);
}

impl Window {
    /// The cue covering `rt`, latest start wins -- `fcasttextoverlay`'s rule,
    /// and the cue engine's.
    fn covering(&self, rt: gst::ClockTime) -> Option<&Cue> {
        self.cues
            .iter()
            .filter(|cue| cue.start_rt <= rt && cue.end_rt.is_none_or(|end| end > rt))
            .next_back()
    }
}

/// ONE cue feed per pipeline, however many taps ask for it.
///
/// `set_subtitle_consumer` installs a single callback and a second call
/// REPLACES the first -- which is right for the driver (there is one renderer)
/// and a trap for a suite: several of these tests install a tap more than once
/// (a helper installs one, the test body installs another), and with one
/// consumer per tap the earlier taps would keep receiving video buffers while
/// their cue window silently went dead. Every tap on a pipeline therefore
/// shares one window, fed by the one consumer installed the first time.
///
/// Keyed by the pipeline's IDENTITY, held weakly.
///
/// NOT by name: the crate names every pipeline it builds `fcastplaybin`, so a
/// name key silently merged every concurrently running test in the binary into
/// one cue feed -- which passes standalone and fails under `cargo test`'s
/// thread pool, the worst shape a test instrument can have. A weak reference
/// also lets a finished test's entry go, so a later pipeline reusing the
/// address cannot inherit its cues.
fn cue_window_for(playbin: &FcastPlaybin) -> Arc<Mutex<Window>> {
    static WINDOWS: Mutex<Vec<(glib::WeakRef<gst::Pipeline>, Arc<Mutex<Window>>)>> =
        Mutex::new(Vec::new());
    let pipeline = playbin.pipeline().clone();
    let mut windows = WINDOWS.lock().unwrap();
    windows.retain(|(weak, _)| weak.upgrade().is_some());
    if let Some((_, window)) = windows
        .iter()
        .find(|(weak, _)| weak.upgrade().is_some_and(|p| p == pipeline))
    {
        return window.clone();
    }
    let window: Arc<Mutex<Window>> = Default::default();
    let weak = glib::WeakRef::new();
    weak.set(Some(&pipeline));
    windows.push((weak, window.clone()));
    drop(windows);

    {
        let feed = window.clone();
        playbin.set_subtitle_consumer(move |item| {
            // Optional pacing (see `throttle_cue_delivery`). Read the delay
            // and DROP the lock before sleeping: a consumer holding the feed
            // across its own delay would block every test-side read of the
            // tap, which is the opposite of what the pacing is for.
            let delay = match &item {
                SubtitleFeedItem::Cue { .. } => feed.lock().unwrap().delivery_delay,
                _ => None,
            };
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            let mut feed = feed.lock().unwrap();
            match item {
                SubtitleFeedItem::Cue {
                    text,
                    start_rt,
                    end_rt,
                    // The delivery's own timeline, stamped by the crate from
                    // the segment the bounds were resolved against. This used
                    // to be a pad-sticky walk at delivery instant, and it
                    // recorded None for every park-replayed cue: the replay
                    // feeds the caller at the join, before the fresh branch
                    // has carried its segment, while the cue itself was
                    // resolved against a real (and, measured, aligned) one.
                    origin,
                    ..
                } => {
                    let at = Instant::now();
                    feed.delivered.push((at, text.clone()));
                    for tap in &feed.payload_taps {
                        tap.lock().unwrap().push((text.clone(), at));
                    }
                    for tap in &feed.position_taps {
                        tap.lock().unwrap().push((text.clone(), Some(origin)));
                    }
                    feed.cues.push(Cue {
                        at,
                        start_rt,
                        end_rt,
                        origin,
                        text,
                    });
                }
                SubtitleFeedItem::Clear => {
                    feed.clears.push(Instant::now());
                    feed.cues.clear();
                }
                // This harness is the TEXT arm's: a bitmap packet is not
                // a cue and must not be counted as one. Unreachable while the
                // driver's implemented set is empty.
                SubtitleFeedItem::Bitmap { .. } => {}
            }
        });
    }
    window
}

/// Establish the cue feed for this pipeline, before anything can flow.
///
/// MUST be called as soon as the playbin exists. The text branch is UNSYNCED:
/// an external subtitle file is parsed and handed over in full within
/// milliseconds of the branch linking (subtitleoverlay used to pace delivery
/// with playback by prefetch-blocking the text push until video reached the
/// cue, and nothing does that now). A tap installed after that -- as several
/// suites do, once playback has reached a position -- would find the entire
/// feed already in the past and conclude nothing ever rendered.
pub fn arm(playbin: &FcastPlaybin) {
    let _ = cue_window_for(playbin);
}

/// "Was a cue being shown on this video buffer?", for a suite that runs its own
/// video-buffer probe.
///
/// Nothing is drawn on the buffer, so the answer is rejoined: the buffer says
/// WHEN (its running time, from the sticky segment on the pad it is crossing)
/// and the cue feed says WHAT, and the question becomes the renderer's own --
/// does a delivered cue cover that running time? (subtitleoverlay used to let
/// the buffer answer by itself, through a `GstVideoOverlayCompositionMeta` or
/// white glyphs in the luma plane.)
///
/// The segment is read STICKY, per buffer, rather than captured by an event
/// probe: these probes go in once playback is already running, so the segment
/// they need went past before they existed. The sticky copy is the same event,
/// still on the pad, and re-reading it per buffer also survives the segment
/// CHANGING under a seek.
pub fn cue_oracle(
    playbin: &FcastPlaybin,
) -> Arc<dyn Fn(&gst::Pad, &gst::BufferRef) -> bool + Send + Sync> {
    let window = cue_window_for(playbin);
    Arc::new(move |pad: &gst::Pad, buffer: &gst::BufferRef| {
        let Some(rt) = buffer.pts().and_then(|pts| {
            pad.sticky_event::<gst::event::Segment>(0)
                .and_then(|event| {
                    event
                        .segment()
                        .downcast_ref::<gst::ClockTime>()
                        .and_then(|seg| seg.to_running_time(pts))
                })
        }) else {
            return false;
        };
        window.lock().unwrap().covering(rt).is_some()
    })
}

/// The arm-dispatched text probe.
#[derive(Clone)]
pub struct TextTap {
    seen: TextSeen,
    window: Arc<Mutex<Window>>,
}

impl TextTap {
    /// Install on whichever transport is active.
    ///
    /// On the consumer arm this installs the subtitle consumer, so it must run
    /// before cues start flowing -- which is where every existing tap already
    /// sits (right after the pipeline is built, or before the track is
    /// selected).
    pub fn install(playbin: &FcastPlaybin) -> Self {
        let tap = Self {
            seen: Default::default(),
            window: cue_window_for(playbin),
        };
        tap.install_video_tap(playbin);
        tap
    }

    /// The video-buffer half: the caller's video sink is the last point every
    /// displayed frame passes inside the crate.
    fn install_video_tap(&self, playbin: &FcastPlaybin) {
        let Some(src) = video_tap_pad(playbin) else {
            return;
        };
        let seen = self.seen.clone();
        let oracle = cue_oracle(playbin);
        src.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(buffer) = info.buffer() {
                let has_text = oracle(pad, buffer);
                seen.lock()
                    .unwrap()
                    .push((Instant::now(), buffer.pts(), has_text));
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// The per-buffer log every existing assertion reads.
    pub fn seen(&self) -> TextSeen {
        self.seen.clone()
    }

    /// One entry per tapped video buffer: when it went past, and whether a cue
    /// was being shown on it, rejoined from the cue feed.
    pub fn rendered(&self) -> Vec<(Instant, bool)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(at, _pts, text)| (*at, *text))
            .collect()
    }

    /// Cue payloads in delivery order.
    pub fn delivered_texts(&self) -> Vec<String> {
        self.window
            .lock()
            .unwrap()
            .delivered
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }

    /// How many `Clear`s the consumer received.
    pub fn clears(&self) -> usize {
        self.window.lock().unwrap().clears.len()
    }

    /// Snapshot of the cues currently in the window: payload, delivery
    /// instant, and the running-time span the cue covers.
    ///
    /// This exists for claims that need the covering cue's IDENTITY, not just
    /// its existence. A boolean "some cue covers this frame" is satisfiable by
    /// the WRONG cue: a mistimed pad offset shifts EVERY cue window by the
    /// same amount, so the cue for media `t + offset` lands exactly on the
    /// playhead, and an unsynced text branch delivers the demuxer's readahead
    /// immediately, so that future cue is reliably there to cover for the one
    /// that expired. Payload-named fixture cues plus this snapshot let a suite
    /// insist the cue covering second N IS the cue named for second N, and the
    /// delivery instant lets it insist a particular gesture (not an earlier
    /// leg's leftovers) delivered it.
    pub fn window_cues(&self) -> Vec<(String, Instant, gst::ClockTime, Option<gst::ClockTime>)> {
        self.window
            .lock()
            .unwrap()
            .cues
            .iter()
            .map(|cue| (cue.text.clone(), cue.at, cue.start_rt, cue.end_rt))
            .collect()
    }
}

/// The pad every displayed frame crosses: the caller's video sink's own sink
/// pad, while the video chain is in the pipeline.
///
/// `None` means there is no video chain right now, which is a real answer (an
/// audio-only item, a mid-item video deselect) and not a failure. The suites
/// used to ask `by_name("fpb-suboverlay")` and got the same
/// `None` for the same reason.
pub fn video_tap_pad(playbin: &FcastPlaybin) -> Option<gst::Pad> {
    let sink = playbin.video_sink();
    sink.parent()?;
    sink.static_pad("sink")
}

/// Whether a text branch is currently wired to its consumer tail.
///
/// LINKED, not merely present. A detach unlinks the branch immediately and can
/// leave the disposal for later, so the appsink can outlive the branch. The
/// driver links exactly one branch by construction.
pub fn text_branch_linked(playbin: &FcastPlaybin) -> bool {
    text_tail_pads(playbin).iter().any(|pad| pad.is_linked())
}

/// The sink pads at the tail of every live text branch: the per-stream
/// appsinks' sink pads. The census suites watch these.
pub fn text_tail_pads(playbin: &FcastPlaybin) -> Vec<gst::Pad> {
    let pipeline = playbin.pipeline();
    let mut pads = Vec::new();
    let mut iter = pipeline.iterate_elements();
    while let Ok(Some(element)) = iter.next() {
        if element.name().starts_with("fpb-textsink-")
            && let Some(pad) = element.static_pad("sink")
        {
            pads.push(pad);
        }
    }
    pads
}

/// The running-time ORIGIN the live text branch renders against, or `None`
/// while no branch is wired.
///
/// The alignment contracts compare this against the video chain's origin, and
/// they compare it REPEATEDLY while the crate's replays converge, so the pad
/// is re-resolved on every call rather than captured once: on the consumer arm
/// a realignment disposes the branch and builds a new appsink, and a captured
/// pad would keep answering with the segment of a branch that has left the
/// graph.
pub fn live_text_origin(playbin: &FcastPlaybin) -> Option<gst::ClockTime> {
    text_tail_pads(playbin)
        .iter()
        .find(|pad| pad.is_linked())
        .and_then(segment_origin)
}

/// A name for the tail pad, for census reporting.
pub fn text_tail_pad_name(pad: &gst::Pad) -> String {
    let element = pad
        .parent_element()
        .map(|e| e.name().to_string())
        .unwrap_or_else(|| "?".into());
    format!("{element}:{}", pad.name())
}

/// The cue-payload tap the `tap_overlay_text` family wants.
///
/// Reads the payloads out of the cue feed. (The overlay arm read the raw text
/// buffers arriving at `subtitle_sink` -- the same bytes, one element
/// earlier.)
///
/// Cues ALREADY DELIVERED are backfilled, so a tap installed mid-playback is
/// not blind to the burst an unsynced branch handed over the instant it linked
/// (see [`arm`]).
pub fn tap_cue_payloads(playbin: &FcastPlaybin) -> CueTap {
    let tap: CueTap = Arc::new(Mutex::new(Vec::new()));
    let window = cue_window_for(playbin);
    let mut window = window.lock().unwrap();
    let backfill: Vec<(String, Instant)> = window
        .delivered
        .iter()
        .map(|(at, text)| (text.clone(), *at))
        .collect();
    tap.lock().unwrap().extend(backfill);
    window.payload_taps.push(tap.clone());
    tap
}

/// [`tap_cue_payloads`] for suites that assert about a cue's place in the
/// timeline as well as its text. See [`PositionTap`] for what the number is.
pub fn tap_cue_positions(playbin: &FcastPlaybin) -> PositionTap {
    let tap: PositionTap = Arc::new(Mutex::new(Vec::new()));
    let window = cue_window_for(playbin);
    let mut window = window.lock().unwrap();
    // Backfill, for the same reason `tap_cue_payloads` does. Each cue kept
    // the origin it was delivered against, so the backfill is exact.
    let backfill: Vec<(String, Option<gst::ClockTime>)> = window
        .cues
        .iter()
        .map(|cue| (cue.text.clone(), Some(cue.origin)))
        .collect();
    tap.lock().unwrap().extend(backfill);
    window.position_taps.push(tap.clone());
    tap
}

/// The LIVE text branch's tail pad: the one a cue actually crosses on its way
/// out of the crate.
///
/// Waits for a branch to be wired, because that is what every caller has just
/// finished waiting for anyway, and panics rather than returning something
/// unwired -- a probe installed on the wrong pad reports silence and silence
/// reads as a bug in the code under test.
pub fn live_text_tail_pad(playbin: &FcastPlaybin) -> gst::Pad {
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(pad) = text_tail_pads(playbin)
            .into_iter()
            .find(|pad| pad.is_linked())
        {
            return pad;
        }
        assert!(
            Instant::now() < deadline,
            "no text branch is wired, so its tail pad cannot be probed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// How many cues have reached the renderer.
///
/// Counts deliveries into the cue feed -- which must be ARMED at construction
/// ([`arm`]), because an unsynced branch hands an external subtitle over in one
/// burst and a counter installed afterwards would report zero for a branch that
/// delivered everything it had.
pub struct TextArrivals(#[allow(private_interfaces)] Arc<Mutex<Window>>);

impl TextArrivals {
    pub fn count(&self) -> usize {
        self.0.lock().unwrap().delivered.len()
    }

    /// How many cues reached the renderer after `after`.
    ///
    /// The distinction a suite buys with this: "no cue rendered" splits into
    /// "cues arrived and were not shown" and "nothing arrived at all", which
    /// are different bugs in different components.
    pub fn since(&self, after: Instant) -> usize {
        self.0
            .lock()
            .unwrap()
            .delivered
            .iter()
            .filter(|(at, _)| *at > after)
            .count()
    }
}

pub fn count_text_arrivals(playbin: &FcastPlaybin) -> TextArrivals {
    TextArrivals(cue_window_for(playbin))
}

/// A manufactured obstacle inside the text renderer.
///
/// What a suite wants from this: the text branch's streaming thread STUCK
/// inside the renderer, on demand, while the pipeline stays where it is. That
/// is what makes a flush pair, a `gst_pad_pause_task`, a disposal or a
/// teardown block, and real media only produces it once in a while.
///
/// # AT PLAYING ONLY
///
/// The hold keeps the appsink's preroll lock while it sleeps, so the sink's
/// own PLAYING->PAUSED transition waits behind it: a suite that pauses after
/// engaging the hold does not settle until the hold expires, and then measures
/// a branch that was released while it was setting up. Measured in
/// `teardown_races`, which is why that suite was deleted with subtitleoverlay
/// rather than ported. [`TailHold::holding`] is the check that caught it, and
/// is what any future paused staging must consult.
///
/// A subtitle consumer whose callback sleeps. Cues arrive on the branch's
/// streaming thread inside the appsink's `new_sample`, so the park is inside
/// the branch -- and unlike the overlay arm's sleeping pad probe it is a shape
/// the product can actually produce: the crate documents exactly this hazard
/// on [`FcastPlaybin::set_subtitle_consumer`] ("IT MUST NOT BLOCK"), and a
/// renderer that misses its frame deadline is what would break it.
///
/// # Where to install it
///
/// BEFORE the subtitle track is selected: it must beat the first cue, and an
/// unsynced external hands its whole file over within milliseconds of
/// linking.
///
/// # Not with the cue taps
///
/// It takes `set_subtitle_consumer`, which [`arm`] and the tap family also
/// want, and the crate allows one consumer. A suite manufactures the obstacle
/// or reads the cues, not both.
pub struct TailHold {
    arrivals: Arc<std::sync::atomic::AtomicUsize>,
    engaged: Arc<std::sync::atomic::AtomicBool>,
    released: Arc<std::sync::atomic::AtomicBool>,
}

impl TailHold {
    /// Cues that have reached the renderer, the hold's own count.
    pub fn arrivals(&self) -> usize {
        self.arrivals.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the sleeping renderer has taken hold. A test that asserts
    /// about a blocked branch must wait for this first, or it measures an
    /// obstacle that was never there.
    pub fn engaged(&self) -> bool {
        self.engaged.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the renderer is holding the branch RIGHT NOW.
    ///
    /// [`Self::engaged`] latches; this does not. A suite that stages
    /// something between engaging the hold and measuring under it -- a pause,
    /// a settle, a second wait -- has to check this at the moment of
    /// measurement, or it can pass having measured a branch that was released
    /// while it was setting up. That is the vacuity the whole dual-arm rework
    /// exists to keep out.
    pub fn holding(&self) -> bool {
        self.engaged() && !self.released.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Hold the text branch inside the renderer for `hold`, once, on the cue after
/// `after` have already crossed. See [`TailHold`].
pub fn hold_the_text_tail(
    playbin: &FcastPlaybin,
    after: usize,
    hold: std::time::Duration,
) -> TailHold {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let arrivals = Arc::new(AtomicUsize::new(0));
    let engaged = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));

    let seen = arrivals.clone();
    let took = engaged.clone();
    let gone = released.clone();
    playbin.set_subtitle_consumer(move |item| {
        if !matches!(item, SubtitleFeedItem::Cue { .. }) {
            return;
        }
        if seen.fetch_add(1, Ordering::SeqCst) == after {
            took.store(true, Ordering::SeqCst);
            std::thread::sleep(hold);
            gone.store(true, Ordering::SeqCst);
        }
    });
    TailHold {
        arrivals,
        engaged,
        released,
    }
}

/// How many buffers the live text branch's queue is holding on to.
///
/// A branch whose queue keeps a backlog is a branch whose loop task cannot
/// hand its push on -- it is inside the renderer, holding `queue:src`'s stream
/// lock, which is the state every "does this operation block behind a
/// streaming thread" test needs to be in. The queue is `fpb-tqueue-<pad>`.
pub fn text_branch_backlog(playbin: &FcastPlaybin) -> u32 {
    let mut iter = playbin.pipeline().iterate_elements();
    let mut worst = 0;
    while let Ok(Some(element)) = iter.next() {
        if element.name().starts_with("fpb-tqueue-") {
            worst = worst.max(element.property::<u32>("current-level-buffers"));
        }
    }
    worst
}
