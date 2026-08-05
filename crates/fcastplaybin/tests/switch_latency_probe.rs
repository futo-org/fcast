//! Where the external -> external subtitle switch latency actually goes
//! (`db3-switch-apply-latency-plan.md`), measured rather than reasoned about.
//!
//! ## What was measured
//!
//! Every case adds an IDLE probe of its own to decodebin3's multiqueue text
//! src pad at the instant the switch is requested. `gst_pad_add_probe` runs an
//! IDLE probe INLINE when the pad is idle, so a probe that fires late proves
//! the pad was inside `gst_pad_push` for exactly that long — and that pad
//! being mid-push is the only thing that can hold up decodebin3's own
//! `mq_slot_unassign_probe`, which is what applies the switch.
//!
//! Across every case measured, `confirm - mq_idle` was 1-2 ms, whether the
//! total was 2 ms or 8.6 s: the switch completes the moment that pad idles,
//! and NOTHING else in decodebin3 contributes. The pad is mid-push into the crate's text `queue`, which for a
//! sparse text stream is "full" on its default 1 s `max-size-time` while
//! holding zero buffers and zero bytes — a GAP event advances the queue's time
//! level (`apply_gap` -> `update_time_level` in gstqueue.c) without adding
//! data, so dead air alone back-pressures the switch.
//!
//! ## What was ruled out, by measurement
//!
//! * the outgoing INPUT's next cue being the trigger (the plan's stated
//!   cause): the realtime control has ZERO items arriving and confirms in
//!   2 ms, while `queue=no-time-limit` has 111 items arriving and also
//!   confirms in 2 ms. Item supply is neither necessary nor sufficient;
//!   downstream acceptance is both.
//! * widening the queue (`no-time-limit`): confirms in 2 ms but strands the
//!   incoming stream behind a 58-cue outgoing backlog, so its first cue never
//!   arrives. Not a fix.
//! * tightening it (`no-time,1buf`): no help at all. One queued cue is enough
//!   to keep the pad mid-push.
//! * `leaky=downstream` armed for the switch: no effect. GstQueue signals
//!   waiters from the `max-size-*` setters (`queue_capacity_change`) but NOT
//!   from `PROP_LEAKY`, so a chain call already parked on the condition is
//!   never re-evaluated.
//! * transient queue headroom: confirms in 2 ms, but the cues it admits are
//!   the OUTGOING stream's, so the incoming stream's first cue lands later
//!   than doing nothing. It moves the wait rather than removing it.
//!
//! ## What works
//!
//! `FIX park-first`: issue the switch as DISABLE-then-SELECT. That is the
//! crate's own park path (`pump_selection` -> `park_text_streams`, lib.rs
//! ~1820, today gated on `target.subtitle.is_none()`), which flushes the text
//! branch and moves the pad onto a `fakesink sync=false`. The pad idles at
//! once AND the outgoing backlog is discarded instead of rendered.
//!
//! Run with `--nocapture --test-threads=1`: the numbers are the point.

use std::{
    cell::{Cell, RefCell},
    sync::{
        Arc, Mutex, mpsc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::{FTestSink, Recording},
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// Long enough that nothing ends while a case works.
const CLIP: gst::ClockTime = gst::ClockTime::from_seconds(30);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// Cues `gap` apart, each on screen for `gap`/2, payload prefixed so a tap can
/// tell the streams apart.
fn prefixed_cues(prefix: &str, count: u32, gap: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = gap * u64::from(index + 1);
            CueSpec::new(start, start + gap / 2, format!("{prefix}{index:02}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Harness (trimmed copy of tests/scenarios.rs)
// ---------------------------------------------------------------------------

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    paused: Cell<bool>,
    _video: Recording,
    _audio: Arc<Mutex<Vec<Recording>>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let audio: Arc<Mutex<Vec<Recording>>> = Arc::new(Mutex::new(Vec::new()));
        let audio_slot = audio.clone();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(move || {
                let sink = FTestSink::new();
                audio_slot
                    .lock()
                    .expect("audio recording slot")
                    .push(sink.recording());
                Ok(sink.upcast())
            })),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin,
            events,
            log: RefCell::new(Vec::new()),
            paused: Cell::new(false),
            _video: video,
            _audio: audio,
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        }
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok((event, _generation)) = self.events.try_recv() {
            self.log.borrow_mut().push(event);
        }
    }

    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, _generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!("pipeline error while waiting for {what}: {error}");
                    }
                    let hit = pred(&event);
                    self.log.borrow_mut().push(event);
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("event channel closed while waiting for {what}")
                }
            }
        }
    }

    fn load_and_play(&self, uri: &str) {
        self.drain_events();
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        self.playbin.play().expect("play");
        self.wait_for("settled PLAYING", |event| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        });
    }

    fn last_selected_subtitle(&self) -> Option<Option<String>> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.clone()),
                _ => None,
            })
    }

    fn overlay(&self) -> gst::Element {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(overlay) = self.playbin.pipeline().by_name("fpb-suboverlay") {
                return overlay;
            }
            assert!(Instant::now() < deadline, "subtitleoverlay never joined");
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn overlay_subtitle_pad(&self) -> gst::Pad {
        self.overlay()
            .static_pad("subtitle_sink")
            .expect("subtitleoverlay has a subtitle_sink pad")
    }

    fn wait_subtitle_branch(&self, what: &str) {
        let pad = self.overlay_subtitle_pad();
        let start = Instant::now();
        while !pad.is_linked() {
            assert!(
                start.elapsed() < EVENT_TIMEOUT,
                "the subtitle branch never reached the overlay ({what})"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + TEARDOWN_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "shutdown never finished");
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the worker died during shutdown")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline introspection
// ---------------------------------------------------------------------------

fn find_element(harness: &Harness, factory: &str) -> Option<gst::Element> {
    harness
        .playbin
        .pipeline()
        .iterate_recurse()
        .into_iter()
        .flatten()
        .find(|element| {
            element
                .factory()
                .is_some_and(|f| f.name().as_str() == factory)
        })
}

/// decodebin3's internal multiqueue src pad carrying the CURRENTLY OUTPUT text
/// stream. A deselected external has a slot too (the crate seeds one with a
/// GAP so the stream is selectable at all), but only the assigned slot's src
/// pad is linked to an output — and only that one can hold a switch up.
fn mq_text_src_pad(harness: &Harness) -> Option<gst::Pad> {
    let mq = find_element(harness, "multiqueue")?;
    mq.iterate_src_pads()
        .into_iter()
        .flatten()
        .find(|pad| pad_is_text(pad) && pad.is_linked())
}

/// The multiqueue sink pad feeding `src_pad` (multiqueue pairs `sink_N` with
/// `src_N`), i.e. where the OUTGOING external input's data enters.
fn mq_sink_pad_for(harness: &Harness, src_pad: &gst::Pad) -> Option<gst::Pad> {
    let mq = find_element(harness, "multiqueue")?;
    let index = src_pad.name().as_str().strip_prefix("src_")?.to_owned();
    mq.static_pad(&format!("sink_{index}"))
}

fn pad_is_text(pad: &gst::Pad) -> bool {
    pad.current_caps()
        .and_then(|caps| {
            caps.structure(0)
                .map(|s| s.name().as_str().starts_with("text/"))
        })
        .unwrap_or(false)
}

/// The per-stream `queue` fcastplaybin puts between decodebin3 and
/// subtitleoverlay (`tqueue`), found by its peer rather than by name: the
/// crate builds it unnamed.
fn text_queue(harness: &Harness) -> Option<gst::Element> {
    let overlay_sink = harness.overlay_subtitle_pad();
    let peer = overlay_sink.peer()?;
    peer.parent_element()
}

fn level_time(queue: &gst::Element) -> u64 {
    queue.property::<u64>("current-level-time")
}

fn level_buffers(queue: &gst::Element) -> u32 {
    queue.property::<u32>("current-level-buffers")
}

fn level_bytes(queue: &gst::Element) -> u32 {
    queue.property::<u32>("current-level-bytes")
}

// ---------------------------------------------------------------------------
// Text tap
// ---------------------------------------------------------------------------

type TextTap = Arc<Mutex<Vec<(String, Instant)>>>;

fn tap_overlay_text(harness: &Harness) -> TextTap {
    let seen: TextTap = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    harness
        .overlay_subtitle_pad()
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let payload = buffer
                    .map_readable()
                    .map(|map| String::from_utf8_lossy(map.as_slice()).into_owned())
                    .unwrap_or_default();
                recorder
                    .lock()
                    .expect("text tap")
                    .push((payload, Instant::now()));
            }
            gst::PadProbeReturn::Ok
        });
    seen
}

fn tapped_with_prefix(tap: &TextTap, prefix: &str) -> usize {
    tap.lock()
        .expect("text tap")
        .iter()
        .filter(|(payload, _)| payload.starts_with(prefix))
        .count()
}

fn wait_for_prefixed_cue(harness: &Harness, tap: &TextTap, prefix: &str, already: usize, what: &str) {
    assert!(
        try_wait_for_prefixed_cue(harness, tap, prefix, already, EVENT_TIMEOUT),
        "no {prefix} cue reached the overlay ({what})"
    );
}

/// Bounded, non-fatal: a configuration that confirms the switch fast but keeps
/// rendering the OUTGOING stream is a real (bad) outcome the table must be able
/// to report rather than abort on.
fn try_wait_for_prefixed_cue(
    harness: &Harness,
    tap: &TextTap,
    prefix: &str,
    already: usize,
    bound: Duration,
) -> bool {
    let deadline = Instant::now() + bound;
    loop {
        if tapped_with_prefix(tap, prefix) > already {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Measured {
    /// request_track -> the StreamsSelected naming the new stream.
    confirm: Duration,
    /// How long an IDLE probe added to decodebin3's multiqueue text src pad at
    /// request time took to fire. ~0 means the pad was idle; anything larger is
    /// the pad being mid-push, which is exactly what stalls decodebin3's own
    /// `mq_slot_unassign_probe`.
    mq_idle: Duration,
    /// Text-queue fill at request time.
    queue_time_ms: u64,
    queue_buffers: u32,
    queue_bytes: u32,
    /// request -> first cue of the newly selected stream at the overlay.
    /// `None` when the outgoing stream's backlog kept rendering instead.
    first_new_cue: Option<Duration>,
    /// Serialized items that reached the OUTGOING slot's multiqueue input
    /// between the request and the confirmation.
    inbound_during: usize,
}

/// How the text `queue` is configured for a case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueCfg {
    /// What the crate builds today: `queue` defaults, i.e. 200 buffers /
    /// 10 MB / **1 s**.
    Default,
    /// Only the TIME limit lifted. Buffer and byte limits stay at the
    /// defaults, so this isolates which of the three limits back-pressures.
    NoTimeLimit,
    /// Time limit lifted and the buffer limit tightened to ONE cue: the
    /// lookahead a text branch actually needs (the overlay holds a pending
    /// buffer of its own), so the stale-cue exposure is no worse than the 1 s
    /// time limit gives today, while dead air can no longer fill the queue.
    NoTimeLimitOneBuffer,
    /// Steady state untouched. `leaky=downstream` is armed at the instant the
    /// switch is requested and disarmed the instant it confirms: for that
    /// window the queue cannot block its upstream (a leaky queue drops instead
    /// of waiting), so the outgoing slot's src pad reaches idle, and what it
    /// drops is the OUTGOING subtitle's backlog, which the switch is
    /// discarding anyway.
    TransientLeaky,
    /// Same window, headroom instead of leaking: two extra buffers of room,
    /// no time limit, restored on confirmation.
    TransientHeadroom,
    /// Nothing is reconfigured. The switch is issued as DISABLE-then-SELECT
    /// instead of as a replace, which is the crate's own park path
    /// (`pump_selection` -> `park_text_streams` when the dispatched target has
    /// no subtitle): the text pad moves onto a `fakesink sync=false`, which
    /// cannot back-pressure, so the outgoing slot's src pad reaches idle at
    /// once and the queued outgoing cues go to the fake sink instead of the
    /// screen. Public API only — this is the proposed crate fix, measured.
    ViaDisableFirst,
}

struct Case {
    /// Gap between the OUTGOING subtitle's cues.
    gap: gst::ClockTime,
    queue: QueueCfg,
    pacing: Pacing,
    label: &'static str,
}

/// Identical settle applied to EVERY case, so no case gets more time to reach
/// steady state than another (an earlier revision widened the queue and then
/// settled, which confounded the queue effect with the settle).
const SETTLE: Duration = Duration::from_millis(800);

fn settle_for(harness: &Harness, how_long: Duration) {
    let until = Instant::now() + how_long;
    while Instant::now() < until {
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run_case(key: &str, case: &Case) -> Measured {
    init();
    let main = ScenarioBuilder::new(&format!("{key}main"))
        .video("video_0")
        .audio("audio_0")
        .duration(CLIP)
        .bytes_per_buffer(64)
        .pacing(Pacing::AsFastAsPossible)
        .register();
    // A is the OUTGOING stream: its cue gap is the knob the cadence theory
    // predicts the latency from.
    let sub_a = ScenarioBuilder::new(&format!("{key}a"))
        .text("text_0", prefixed_cues("AAA", 60, case.gap))
        .duration(CLIP)
        .pacing(case.pacing)
        .register();
    let sub_b = ScenarioBuilder::new(&format!("{key}b"))
        .text(
            "text_0",
            prefixed_cues("BBB", 120, gst::ClockTime::from_mseconds(250)),
        )
        .duration(CLIP)
        .pacing(case.pacing)
        .register();

    let harness = Harness::new();
    harness.load_and_play(&main.uri());
    let tap = tap_overlay_text(&harness);

    let (_id_a, _sid_a) = attach_and_select(&harness, &sub_a.uri());
    harness.wait_subtitle_branch("first selection of A");
    // One cue in: the branch is warm and the queue has reached the steady
    // state a field switch happens from.
    wait_for_prefixed_cue(&harness, &tap, "AAA", 0, "warming A");

    let (id_b, sid_b) = attach_only(&harness, &sub_b.uri());

    let tqueue = text_queue(&harness).expect("the text queue is wired");
    // Let the attach settle before the queue is reconfigured, so a case that
    // changes nothing gets the same head start as one that does.
    settle_for(&harness, Duration::from_millis(500));
    match case.queue {
        QueueCfg::Default
        | QueueCfg::TransientLeaky
        | QueueCfg::TransientHeadroom
        | QueueCfg::ViaDisableFirst => {}
        QueueCfg::NoTimeLimit => tqueue.set_property("max-size-time", 0u64),
        QueueCfg::NoTimeLimitOneBuffer => {
            tqueue.set_property("max-size-time", 0u64);
            tqueue.set_property("max-size-buffers", 1u32);
        }
    }
    settle_for(&harness, SETTLE);

    let mq_pad = mq_text_src_pad(&harness).expect("decodebin3's text multiqueue pad");
    let queue_time_ms = level_time(&tqueue) / 1_000_000;
    let queue_buffers = level_buffers(&tqueue);
    let queue_bytes = level_bytes(&tqueue);

    // Independent handle on "the outgoing INPUT's next item is what triggers
    // the reassignment": count everything reaching the outgoing slot's
    // multiqueue INPUT between the request and the confirmation. Compare the
    // realtime control (0 items, confirms in 2 ms) against `no-time-limit`
    // (111 items, also 2 ms): item supply is neither necessary nor sufficient.
    let inbound = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    if let Some(sink) = mq_sink_pad_for(&harness, &mq_pad) {
        let counter = inbound.clone();
        sink.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, _info| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            },
        );
    }

    let seen_b_before = tapped_with_prefix(&tap, "BBB");
    // Every confirmation the wait below accepts must be THIS switch's.
    harness.drain_events();
    inbound.store(0, Ordering::Relaxed);
    let started = Instant::now();

    // The proposed fixes: armed exactly when the switch is requested.
    match case.queue {
        QueueCfg::TransientLeaky => tqueue.set_property_from_str("leaky", "downstream"),
        QueueCfg::TransientHeadroom => {
            tqueue.set_property("max-size-time", 0u64);
            tqueue.set_property("max-size-buffers", level_buffers(&tqueue) + 2);
        }
        _ => {}
    }

    // A probe of our own, added at the same instant the crate asks for the
    // switch. It answers the only question that matters for the IDLE-probe
    // theory: is that pad in use right now, and for how much longer?
    let fired = Arc::new(Mutex::new(None::<Instant>));
    let fired_slot = fired.clone();
    let seen = Arc::new(AtomicBool::new(false));
    mq_pad.add_probe(gst::PadProbeType::IDLE, move |_pad, _info| {
        if !seen.swap(true, Ordering::SeqCst) {
            *fired_slot.lock().expect("idle slot") = Some(Instant::now());
        }
        gst::PadProbeReturn::Remove
    });

    if case.queue == QueueCfg::ViaDisableFirst {
        // Drop the slot first: this is the dispatch that makes the crate park
        // the text pad onto its fake sink. Then ask for B.
        harness
            .playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
        harness.playbin.pump_selection(harness.gate());
    }
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id_b));
    harness.playbin.pump_selection(harness.gate());

    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(sid_b.clone())) {
            break;
        }
        assert!(Instant::now() < deadline, "the switch to B never confirmed");
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(2));
    }
    let confirm = started.elapsed();
    let inbound_during = inbound.load(Ordering::Relaxed);

    // Disarm the moment the switch confirms, exactly as the production hook
    // would from the StreamsSelected handler.
    match case.queue {
        QueueCfg::TransientLeaky => tqueue.set_property_from_str("leaky", "no"),
        QueueCfg::TransientHeadroom => {
            tqueue.set_property("max-size-time", 1_000_000_000u64);
            tqueue.set_property("max-size-buffers", 200u32);
        }
        _ => {}
    }

    let fresh = try_wait_for_prefixed_cue(
        &harness,
        &tap,
        "BBB",
        seen_b_before,
        Duration::from_secs(12),
    );
    let first_new_cue = fresh.then(|| started.elapsed());

    let mq_idle = fired
        .lock()
        .expect("idle slot")
        .map(|at| at.saturating_duration_since(started))
        .unwrap_or(Duration::MAX);

    harness.shutdown();
    main.unregister();
    sub_a.unregister();
    sub_b.unregister();

    let measured = Measured {
        confirm,
        mq_idle,
        queue_time_ms,
        queue_buffers,
        queue_bytes,
        first_new_cue,
        inbound_during,
    };
    println!(
        "[{label}] confirm={confirm:?} mq_pad_idle_after={mq_idle:?} \
         queue_at_request={queue_time_ms}ms/{queue_buffers}buf/{queue_bytes}B \
         outgoing_input_items_during_wait={inbound_during} \
         first_new_cue={first_new_cue:?}",
        label = case.label,
        first_new_cue = measured.first_new_cue,
    );
    measured
}

fn attach_only(harness: &Harness, uri: &str) -> (fcastplaybin::ExternalSubId, String) {
    let id = harness
        .playbin
        .attach_subtitle(uri)
        .expect("attaching the external input");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let sid = loop {
        if let Some(sid) = harness.playbin.subtitle_stream_ids(id).into_iter().next() {
            break sid;
        }
        assert!(
            Instant::now() < deadline,
            "the external stream never materialized"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    };
    (id, sid)
}

fn attach_and_select(harness: &Harness, uri: &str) -> (fcastplaybin::ExternalSubId, String) {
    let (id, sid) = attach_only(harness, uri);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(sid.clone())) {
            return (id, sid);
        }
        assert!(Instant::now() < deadline, "the selection of {sid} never confirmed");
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Bound for a healthy switch-apply. The measured floor is 2-5 ms (both the
/// park-first route and the no-back-pressure control land there), so this is
/// two orders of margin and cannot flake on a loaded box.
const APPLY_BOUND: Duration = Duration::from_millis(500);

/// Bound for the newly selected subtitle actually reaching the overlay. The
/// measured floor is 24-35 ms.
const CUE_BOUND: Duration = Duration::from_millis(1500);

/// The outgoing subtitle's dead air must not price the switch.
///
/// A wide-gap outgoing stream parks decodebin3's text slot src pad inside a
/// push into the text queue (which counts GAP time, not data), and decodebin3
/// applies a slot reassignment from an IDLE probe on exactly that pad. Issued
/// as DISABLE-then-SELECT the crate parks the text pad on its fake sink first,
/// so the pad idles immediately and the outgoing backlog is discarded rather
/// than rendered.
///
/// Guards the park path: if `park_text_streams` stops being reached on the
/// dispatch of a subtitle-dropping selection, this goes back to seconds.
#[test]
fn parking_first_makes_the_switch_immediate() {
    let measured = run_case(
        "swlatpark",
        &Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::ViaDisableFirst,
            pacing: Pacing::AsFastAsPossible,
            label: "assert park-first",
        },
    );
    assert!(
        measured.confirm < APPLY_BOUND,
        "the switch took {:?} to apply (bound {APPLY_BOUND:?}); the outgoing \
         text slot's multiqueue src pad was mid-push for {:?}",
        measured.confirm,
        measured.mq_idle,
    );
    assert!(
        measured.first_new_cue.is_some_and(|at| at < CUE_BOUND),
        "the new subtitle reached the overlay at {:?} (bound {CUE_BOUND:?})",
        measured.first_new_cue,
    );
}

/// The same switch issued as a plain REPLACE, which is what the receiver does.
///
/// `pump_selection` used to park the text branch only when the dispatched
/// selection had NO subtitle (`crates/fcastplaybin/src/lib.rs`, `if
/// target.subtitle.is_none()`). On a replace the branch stayed wired, the
/// outgoing slot's src pad stayed mid-push into the back-pressured text queue,
/// and the switch waited out the outgoing stream's cue cadence: measured 1.59 s
/// at a 2 s cue period and 4.56 s at 4 s, against 5 ms for the same media
/// through the park path. The condition is now "the dispatched subtitle differs
/// from the APPLIED one", so a replace takes the park path too.
#[test]
fn a_direct_replace_switch_is_immediate() {
    let measured = run_case(
        "swlatrepl",
        &Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::Default,
            pacing: Pacing::AsFastAsPossible,
            label: "assert direct replace",
        },
    );
    assert!(
        measured.confirm < APPLY_BOUND,
        "the switch took {:?} to apply (bound {APPLY_BOUND:?}); the outgoing \
         text slot's multiqueue src pad was mid-push for {:?}",
        measured.confirm,
        measured.mq_idle,
    );
    assert!(
        measured.first_new_cue.is_some_and(|at| at < CUE_BOUND),
        "the new subtitle reached the overlay at {:?} (bound {CUE_BOUND:?})",
        measured.first_new_cue,
    );
}

/// The whole discriminator table in one run, so the cases share a machine and
/// the numbers are comparable.
#[test]
fn switch_latency_discriminator_table() {
    let cases = [
        // --- cadence discriminator: unpaced (the FIELD shape: an external
        // subtitle file is parsed and pushed as fast as the chain accepts,
        // it is not realtime), outgoing cue period swept.
        Case {
            gap: gst::ClockTime::from_mseconds(800),
            queue: QueueCfg::Default,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=0.8s  queue=default",
        },
        Case {
            gap: gst::ClockTime::from_mseconds(2000),
            queue: QueueCfg::Default,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=2.0s  queue=default",
        },
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::Default,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=4.0s  queue=default",
        },
        // --- the A/B: same media, only the text queue's TIME limit lifted.
        // Confirms fast, but the 200-buffer default then holds the WHOLE
        // outgoing backlog, so the new stream never reaches the screen. A
        // measured refutation of "just disable the time limit".
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::NoTimeLimit,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=4.0s  queue=no-time-limit",
        },
        // Tightening instead of widening does not help either: one queued cue
        // is enough to keep the outgoing slot's pad mid-push.
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::NoTimeLimitOneBuffer,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=4.0s  queue=no-time,1buf",
        },
        // --- the candidate fixes: steady state untouched, the branch only
        // stops back-pressuring for the duration of the switch.
        Case {
            gap: gst::ClockTime::from_mseconds(2000),
            queue: QueueCfg::TransientLeaky,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=2.0s  FIX transient-leaky",
        },
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::TransientLeaky,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=4.0s  FIX transient-leaky",
        },
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::TransientHeadroom,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=4.0s  FIX transient-headroom",
        },
        Case {
            gap: gst::ClockTime::from_mseconds(2000),
            queue: QueueCfg::ViaDisableFirst,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=2.0s  FIX park-first",
        },
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::ViaDisableFirst,
            pacing: Pacing::AsFastAsPossible,
            label: "unpaced gap=4.0s  FIX park-first",
        },
        // --- control: realtime pacing never lets the queue build a lead, so
        // the same media switches instantly with the SAME default queue.
        Case {
            gap: gst::ClockTime::from_mseconds(4000),
            queue: QueueCfg::Default,
            pacing: Pacing::Realtime,
            label: "realtime gap=4.0s queue=default",
        },
    ];
    let mut out = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        out.push((case.label, run_case(&format!("swlat{index}"), case)));
    }
    println!("\n==== switch-apply latency ====");
    for (label, m) in &out {
        println!(
            "{label:38} confirm={:>8} mq_idle={:>8} q={:>6}ms/{:>3}buf/{:>4}B in={:>3} newcue={}",
            format!("{}ms", m.confirm.as_millis()),
            format!("{}ms", m.mq_idle.as_millis()),
            m.queue_time_ms,
            m.queue_buffers,
            m.queue_bytes,
            m.inbound_during,
            m.first_new_cue
                .map(|d| format!("{}ms", d.as_millis()))
                .unwrap_or_else(|| "NEVER(>12s)".to_owned())
        );
    }
}
