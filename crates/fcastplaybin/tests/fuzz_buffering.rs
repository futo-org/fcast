//! Seeded stress driver for the BUFFERING path. Opt-in, like `fuzz_scenarios`.
//!
//! ```text
//! FCAST_FUZZ_SEED=7 FCAST_FUZZ_ITERS=4 FCAST_FUZZ_ACTIONS=14 \
//!   cargo test -p fcastplaybin --test fuzz_buffering -- --ignored --nocapture
//! ```
//!
//! What this covers that `fuzz_scenarios` does not. The consumer here is the
//! receiver's own machinery, a real [`StateMachine`] fed `PlaybinEvent`s the
//! exact way `receiver-core/src/player.rs` feeds it, and the generated media
//! declares BUFFERING behaviour through fcasttest's knob. That reaches the
//! whole class of states where the pipeline is parked in a buffering PAUSED
//! that the application (not the schedule) requested, while the schedule keeps
//! mutating tracks, seeking, reloading and tearing down. Every deadlock found
//! so far was an unbounded blocking call at a moment the pipeline could not
//! complete it, and a buffering dip is a new way to manufacture that moment
//! which no suite has fuzzed before.
//!
//! The invariants are liveness ones. No pipeline error, a worker that answers
//! a graph-dump round trip after every action, a machine that provably leaves
//! Buffering whenever no recovery gate is held, and a bounded teardown. The
//! timeline-alignment machinery stays in `fuzz_scenarios`, which owns it.

use std::{
    cell::{Cell, RefCell},
    fmt,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, BufferingStateResult, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent,
    RunningState, Seek, SelectionGate, Sinks, StartPoint, StateChangeResult, StateMachine,
    TrackSlot, TrackTarget,
};
use fcasttest::{
    prng::Prng,
    scenario::{ScenarioBuilder, ScenarioHandle, toml},
    sink::{FTestSink, Recording},
    spec::{
        BufferingDip, BufferingRecovery, BufferingSpec, CueSpec, DecoderKnobs, Fault, Pacing,
        StreamKind, StreamSpec,
    },
};
use gst::prelude::*;

/// Bound for anything the pipeline has to reach.
const EVENT_TIMEOUT: Duration = Duration::from_secs(25);
/// The worker must answer a queued graph dump inside this.
const WORKER_BOUND: Duration = Duration::from_secs(12);
/// Bound for the final shutdown.
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);
/// A machine in Buffering with no held gate must observably leave it inside
/// this. Every dip posts its 100 (AfterMs recoveries are all under a second,
/// and periodic dips leave a gap of at least 250 ms before the next low), so
/// a machine still parked after this long lost the completion.
const BUFFERING_LEAVE_BOUND: Duration = Duration::from_secs(12);
/// Bound for an attached input's stream to materialize before a selection can
/// name it. Soft, an input still coming up is simply not selected this step.
const MATERIALIZE_BOUND: Duration = Duration::from_secs(10);
/// Longest an anchor waits before the action just proceeds.
const ANCHOR_BOUND: Duration = Duration::from_millis(900);
/// Name of the stall gate on the main media's video stream.
const STALL_GATE: &str = "bufstall";
/// Name of the gated buffering dip's recovery sync point.
const BUF_GATE: &str = "bufrefill";
/// The documented default budget, used by the coverage floor to tell a full
/// run from a tiny replay.
const DEFAULT_ITERS: u64 = 4;
const DEFAULT_ACTIONS: u64 = 12;
/// Pooled external text scenarios per case.
const EXTERNALS: usize = 2;
const PREFIXES: [&str; EXTERNALS] = ["BFA", "BFB"];
const EXTERNAL_CLIP: gst::ClockTime = gst::ClockTime::from_seconds(60);

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

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Generated case
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Pause,
    Play,
    SeekZero,
    SeekTo(gst::ClockTime),
    ReleaseStallGate,
    /// Releases the gated buffering dip's recovery, when the case has one.
    ReleaseBufferingGate,
    AttachExternal(u8),
    SelectExternal(u8),
    DetachExternal(u8),
    SelectEmbeddedText,
    DisableSubtitles,
    DisableVideo,
    SelectVideo,
    StopReload,
    LoadSwap,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pause => f.write_str("pause"),
            Self::Play => f.write_str("play"),
            Self::SeekZero => f.write_str("seek_zero"),
            Self::SeekTo(position) => write!(f, "seek_to({} ms)", position.mseconds()),
            Self::ReleaseStallGate => f.write_str("release_stall_gate"),
            Self::ReleaseBufferingGate => f.write_str("release_buffering_gate"),
            Self::AttachExternal(slot) => write!(f, "attach_external(slot {slot})"),
            Self::SelectExternal(slot) => write!(f, "select_external(slot {slot})"),
            Self::DetachExternal(slot) => write!(f, "detach_external(slot {slot})"),
            Self::SelectEmbeddedText => f.write_str("select_embedded_text"),
            Self::DisableSubtitles => f.write_str("disable_subtitles"),
            Self::DisableVideo => f.write_str("disable_video"),
            Self::SelectVideo => f.write_str("select_video"),
            Self::StopReload => f.write_str("stop_reload"),
            Self::LoadSwap => f.write_str("load_swap"),
        }
    }
}

/// What an action waits for before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// Run while the machine is provably parked in Buffering. The anchor this
    /// driver exists for.
    WhileBuffering,
    /// Run after `n` more rendered buffers.
    Buffers(usize),
    /// Run after a short settle pump.
    Settled,
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WhileBuffering => f.write_str("while_buffering"),
            Self::Buffers(n) => write!(f, "after_{n}_buffers"),
            Self::Settled => f.write_str("settled"),
        }
    }
}

struct Case {
    iteration: u64,
    seed: u64,
    main: ScenarioHandle,
    replacement: ScenarioHandle,
    externals: [ScenarioHandle; EXTERNALS],
    /// Whether the main media's buffering has an OnSyncPoint recovery. While
    /// that gate is held a machine parked in Buffering is parked legitimately,
    /// so the leave-buffering invariant only applies once it is released.
    has_buffering_gate: bool,
    schedule: Vec<(Anchor, Action)>,
}

/// The draw table, repetition is the weight. External churn and transport
/// changes dominate because the deferred-while-paused machinery and the
/// buffering pause interleave through exactly those.
const DRAW: [u8; 26] = [
    0, 0, 1, 1, 2, 3, 3, 4, 5, 6, 6, 6, 7, 7, 7, 7, 8, 9, 10, 10, 11, 12, 13, 14, 14, 9,
];

impl Case {
    fn generate(seed: u64, iteration: u64, actions: usize) -> Self {
        let mut prng = Prng::new(seed ^ iteration.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let tag = format!("bz{seed}_{iteration}");

        let (main, main_duration, has_buffering_gate) =
            generate_media(&mut prng, &format!("{tag}m"), true);
        let (replacement, _, _) = generate_media(&mut prng, &format!("{tag}r"), false);
        let externals = std::array::from_fn(|slot| {
            ScenarioBuilder::new(format!("{tag}s{slot}"))
                .text(
                    "text_0",
                    prefixed_cues(PREFIXES[slot], 60, gst::ClockTime::from_mseconds(400)),
                )
                .duration(EXTERNAL_CLIP)
                .pacing(Pacing::AsFastAsPossible)
                .register()
        });

        let mut attached = [false; EXTERNALS];
        let mut schedule: Vec<(Anchor, Action)> = (0..actions)
            .map(|_| {
                let action = draw_action(&mut prng, main_duration, &mut attached);
                let anchor = match prng.next_range(0..4) {
                    // Half the anchors chase the buffering window on purpose.
                    0 | 1 => Anchor::WhileBuffering,
                    2 => Anchor::Buffers(prng.next_range(1..4) as usize),
                    _ => Anchor::Settled,
                };
                (anchor, action)
            })
            .collect();
        // Guarantee one attach-then-select pair per schedule. A random walk
        // whose reloads keep wiping the pool can draw every select against
        // an empty slot, and a whole campaign run of such schedules trips
        // the select coverage floor without a single real switch.
        let pair_reaches_pool = schedule.iter().rev().take_while(|(_, action)| {
            !matches!(action, Action::StopReload | Action::LoadSwap)
        });
        let has_select = pair_reaches_pool.clone().any(|(_, action)| {
            matches!(action, Action::SelectExternal(_))
        });
        let has_attach = pair_reaches_pool
            .clone()
            .any(|(_, action)| matches!(action, Action::AttachExternal(_)));
        if !(has_attach && has_select) {
            schedule.push((Anchor::Settled, Action::AttachExternal(0)));
            schedule.push((Anchor::WhileBuffering, Action::SelectExternal(0)));
        }

        Self {
            iteration,
            seed,
            main,
            replacement,
            externals,
            has_buffering_gate,
            schedule,
        }
    }

    fn trace(&self) -> String {
        self.schedule
            .iter()
            .enumerate()
            .map(|(index, (anchor, action))| format!("{index:>3}: {anchor} -> {action}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn handles(&self) -> Vec<(String, &ScenarioHandle)> {
        let mut out = vec![
            ("main".to_owned(), &self.main),
            ("replacement".to_owned(), &self.replacement),
        ];
        for (slot, handle) in self.externals.iter().enumerate() {
            out.push((format!("external{slot}"), handle));
        }
        out
    }

    fn release_all(&self) {
        for (_, handle) in self.handles() {
            handle.release_all();
        }
    }

    fn unregister(&self) {
        for (_, handle) in self.handles() {
            handle.unregister();
        }
    }
}

fn draw_action(prng: &mut Prng, main_duration: gst::ClockTime, attached: &mut [bool]) -> Action {
    let pick_slot = |prng: &mut Prng, attached: &[bool], want: bool| -> u8 {
        let ignore = prng.next_range(0..4) == 0;
        let candidates: Vec<u8> = (0..EXTERNALS as u8)
            .filter(|slot| attached[*slot as usize] == want)
            .collect();
        if ignore || candidates.is_empty() {
            return prng.next_range(0..EXTERNALS as u64) as u8;
        }
        candidates[prng.next_range(0..candidates.len() as u64) as usize]
    };
    match DRAW[prng.next_range(0..DRAW.len() as u64) as usize] {
        0 => Action::Pause,
        1 => Action::Play,
        2 => Action::SeekZero,
        3 => {
            let span = main_duration.mseconds().max(1);
            Action::SeekTo(gst::ClockTime::from_mseconds(prng.next_range(0..span)))
        }
        4 => Action::ReleaseStallGate,
        5 => Action::ReleaseBufferingGate,
        6 => {
            let slot = pick_slot(prng, attached, false);
            attached[slot as usize] = true;
            Action::AttachExternal(slot)
        }
        7 => Action::SelectExternal(pick_slot(prng, attached, true)),
        8 => {
            let slot = pick_slot(prng, attached, true);
            attached[slot as usize] = false;
            Action::DetachExternal(slot)
        }
        9 => Action::SelectEmbeddedText,
        10 => Action::DisableSubtitles,
        11 => Action::DisableVideo,
        12 => Action::SelectVideo,
        13 => {
            attached.fill(false);
            Action::StopReload
        }
        _ => {
            attached.fill(false);
            Action::LoadSwap
        }
    }
}

/// Media with buffering declared. Every case gets at least one buffering
/// source (initial, periodic or anchored dips), because a case without one is
/// `fuzz_scenarios` territory. Returns whether a dip recovers on the held
/// [`BUF_GATE`] sync point.
fn generate_media(
    prng: &mut Prng,
    key: &str,
    allow_gate: bool,
) -> (ScenarioHandle, gst::ClockTime, bool) {
    let duration = gst::ClockTime::from_mseconds(prng.next_range(3500..8000));
    // Mostly realtime-ish pacing. Buffering pauses interleave with delivery
    // on the wall clock, which an as-fast-as-possible drain mostly skips.
    let pacing = match prng.next_range(0..5) {
        0 => Pacing::AsFastAsPossible,
        1 | 2 => Pacing::Realtime,
        _ => Pacing::Jitter {
            base_ms: prng.next_range(0..8),
            jitter_ms: prng.next_range(0..5),
        },
    };
    let fps = [25i32, 10, 5][prng.next_range(0..3) as usize];

    let mut video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(fps, 1),
            keyframe_interval: prng.next_range(1..6) as u32,
        },
    );
    let reorder_frames = prng.next_range(0..3) as u32;
    video = video.with_decoder(DecoderKnobs {
        latency: (prng.next_range(0..4) == 0)
            .then(|| gst::ClockTime::from_mseconds(prng.next_range(10..120))),
        jitter_ms: prng.next_range(0..4),
        reorder_frames,
        needs_keyframe_after_flush: prng.next_bool(),
        error_at_frame: None,
    });
    if allow_gate && prng.next_range(0..4) == 0 {
        // Same earliest-frame guard as fuzz_scenarios, a stall before the
        // reordering decoder can emit anything starves preroll by
        // construction.
        let earliest = u64::from(reorder_frames) + 2;
        video = video.with_fault(Fault::StallAt {
            buffer_index: prng.next_range(earliest..earliest.max(9)),
            sync_point: STALL_GATE.to_owned(),
        });
    }

    // The knob. Anchored dip indexes stay inside the clip's first 60 percent
    // so they fire while the schedule still has media to act on.
    let frames = duration.mseconds() * fps as u64 / 1000;
    let latest_anchor = (frames * 3 / 5).max(10);
    let mut buffering = BufferingSpec::new(prng.next_range(0..41) as i32);
    let mut gated = false;
    if prng.next_bool() {
        buffering = buffering.with_initial_ms(prng.next_range(50..400));
    }
    match prng.next_range(0..4) {
        0 | 1 => {
            buffering = buffering.with_periodic(
                prng.next_range(900..2200),
                prng.next_range(120..650),
            );
        }
        2 => {
            for _ in 0..prng.next_range(1..3) {
                buffering = buffering.with_dip(BufferingDip {
                    stream: "video_0".to_owned(),
                    buffer_index: prng.next_range(8..latest_anchor),
                    recovery: BufferingRecovery::AfterMs(prng.next_range(100..900)),
                });
            }
        }
        _ => {
            if allow_gate {
                gated = true;
                buffering = buffering.with_dip(BufferingDip {
                    stream: "video_0".to_owned(),
                    buffer_index: prng.next_range(8..latest_anchor.min(40)),
                    recovery: BufferingRecovery::OnSyncPoint(BUF_GATE.to_owned()),
                });
            } else {
                buffering = buffering.with_dip(BufferingDip {
                    stream: "video_0".to_owned(),
                    buffer_index: prng.next_range(8..latest_anchor),
                    recovery: BufferingRecovery::AfterMs(prng.next_range(100..900)),
                });
            }
        }
    }

    let mut builder = ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"))
        .buffering(buffering);
    if prng.next_bool() {
        // Spent inside the first half second, see the note in fuzz_scenarios
        // about deselected pads on a per-stream source.
        builder = builder.text("text_0", cues(4, gst::ClockTime::from_mseconds(100)));
    }
    let handle = builder
        .duration(duration)
        .bytes_per_buffer(if prng.next_bool() { 64 } else { 1024 })
        .pacing(pacing)
        .register();
    (handle, duration, gated)
}

fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("CUE{index:02}"))
        })
        .collect()
}

fn prefixed_cues(prefix: &str, count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{prefix}{index:02}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Receiver-shaped consumer
// ---------------------------------------------------------------------------

/// The receiver's view of the player, derived like receiver-core's
/// `player_state()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverState {
    Stopped,
    Paused,
    Playing,
    Buffering,
}

struct Failure {
    step: usize,
    message: String,
}

type Checked = Result<(), Failure>;

/// Per-case tally, printed on the pass line. `buffering_entered` is the one
/// that matters, a run that never once entered Buffering exercised nothing
/// this driver exists for.
#[derive(Default, Clone, Copy)]
struct Coverage {
    buffering_posts: usize,
    buffering_entered: usize,
    acted_while_buffering: usize,
    attached: usize,
    selected: usize,
    detached: usize,
    seeks_issued: usize,
    reloads: usize,
}

impl Coverage {
    fn add(&mut self, other: Coverage) {
        self.buffering_posts += other.buffering_posts;
        self.buffering_entered += other.buffering_entered;
        self.acted_while_buffering += other.acted_while_buffering;
        self.attached += other.attached;
        self.selected += other.selected;
        self.detached += other.detached;
        self.seeks_issued += other.seeks_issued;
        self.reloads += other.reloads;
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "buffering posts {} entered {} acted_in {} attach {} select {} detach {} \
             seeks {} reloads {}",
            self.buffering_posts,
            self.buffering_entered,
            self.acted_while_buffering,
            self.attached,
            self.selected,
            self.detached,
            self.seeks_issued,
            self.reloads
        )
    }
}

/// Whether the driver drops load-scoped events from a superseded load the way
/// `application.rs` `handle_player_event` does. Set the lever to restore the
/// old generation-blind behaviour for an A/B.
fn generation_gate_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("FCAST_FUZZ_NO_GENERATION_GATE").is_err())
}

/// Mirror of `application.rs` `player_event_is_load_scoped`: everything except
/// the control-plane events belongs to one load and must be dropped when its
/// generation is not the current one.
fn event_is_load_scoped(event: &PlaybinEvent) -> bool {
    !matches!(
        event,
        PlaybinEvent::VolumeChanged(_)
            | PlaybinEvent::RequestState(_)
            | PlaybinEvent::ClockLost
            // Stamped with the PREPARED (future) generation by design, and
            // validated against the pre-arm bookkeeping, not the current load.
            | PlaybinEvent::PreparedActivated
            | PlaybinEvent::PreparedCancelled { .. }
            | PlaybinEvent::PreparedCancelDeclined { .. }
    )
}

/// The exact machinery receiver-core's player runs, not a mock of it. Every
/// `PlaybinEvent` is handled the way `player.rs` handles it and every state
/// the machine dispatches is applied to the pipeline.
struct Runner {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    sm: RefCell<StateMachine>,
    desired_transport: Cell<RunningState>,
    video: Recording,
    seekable: bool,
    /// Set when the receiver stopped on EndOfStream. Actions keep running
    /// (they must stay harmless against a stopped pipeline) but the end-of-run
    /// resume invariant no longer applies.
    eos_stopped: Cell<bool>,
    attached: RefCell<[Option<ExternalSubId>; EXTERNALS]>,
    problem: RefCell<Option<String>>,
    /// True between a low buffering post and its 100. The leave-buffering
    /// invariant judges THIS, not the receiver-visible state, because
    /// `running() == None` also covers a machine mid state change (a held
    /// stall gate legitimately parks preroll forever), while a low post's
    /// 100 always comes on the wall clock or the released gate.
    outstanding_low: Cell<bool>,
    /// The generation of the load this driver expects events for, exactly as
    /// `player.rs` keeps it. `None` while stopped. Load-scoped events from any
    /// other generation are a superseded load's stragglers and are dropped in
    /// [`Runner::pump`], the way `application.rs` drops them.
    expected_generation: Cell<Option<u64>>,
    step: Cell<usize>,
    coverage: Cell<Coverage>,
    collections: RefCell<Option<gst::StreamCollection>>,
}

impl Runner {
    fn new(seekable: bool) -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin,
            events,
            sm: RefCell::new(StateMachine::new()),
            desired_transport: Cell::new(RunningState::Playing),
            video,
            seekable,
            eos_stopped: Cell::new(false),
            attached: RefCell::new([None; EXTERNALS]),
            problem: RefCell::new(None),
            outstanding_low: Cell::new(false),
            expected_generation: Cell::new(None),
            step: Cell::new(0),
            coverage: Cell::new(Coverage::default()),
            collections: RefCell::new(None),
        }
    }

    fn count(&self, field: impl Fn(&mut Coverage)) {
        let mut coverage = self.coverage.get();
        field(&mut coverage);
        self.coverage.set(coverage);
    }

    fn fail(&self, message: impl Into<String>) -> Failure {
        Failure {
            step: self.step.get(),
            message: message.into(),
        }
    }

    fn state(&self) -> ReceiverState {
        let sm = self.sm.borrow();
        if sm.is_stopped() {
            return ReceiverState::Stopped;
        }
        match sm.running() {
            Some(RunningState::Paused) => ReceiverState::Paused,
            Some(RunningState::Playing) => ReceiverState::Playing,
            None => ReceiverState::Buffering,
        }
    }

    fn apply(&self, state: gst::State) {
        // The receiver dispatches to the worker, never a blocking set_state
        // from its own thread.
        self.playbin.set_state_async(state);
    }

    /// `player.rs` `uri_loaded`.
    fn on_loaded(&self) {
        let desired = self.desired_transport.get();
        let dispatched = self.sm.borrow_mut().set_playback_state(desired);
        if let Some(state) = dispatched {
            self.apply(state);
        } else if self.sm.borrow().running() != Some(desired) {
            self.apply(desired.into());
        }
    }

    /// `player.rs` `buffering`.
    fn on_buffering(&self, percent: i32) {
        self.count(|coverage| coverage.buffering_posts += 1);
        // Dips serialize (each posts one low, then its 100), so a plain
        // level tracks the outstanding pair.
        self.outstanding_low.set(percent < 100);
        let result = self.sm.borrow_mut().buffering(percent);
        match result {
            BufferingStateResult::Started(state) => {
                // The machine's own entered-buffering edge, not to be
                // confused with the broader unsettled window state() reports.
                self.count(|coverage| coverage.buffering_entered += 1);
                self.apply(state);
            }
            BufferingStateResult::Buffering => {}
            BufferingStateResult::FinishedWithSeek(seek) => self.playbin.seek_async(seek),
            BufferingStateResult::FinishedButWaitingSeek => {}
            BufferingStateResult::Finished(state) => {
                if let Some(state) = state {
                    self.apply(state);
                }
            }
        }
        self.pump_selection();
    }

    /// `player.rs` `state_changed`.
    fn on_state_changed(&self, old: gst::State, current: gst::State, pending: gst::State) {
        self.playbin.poll_text_policy();
        let result = self.sm.borrow_mut().state_changed(old, current, pending);
        match result {
            StateChangeResult::NewPlaybackState(_) | StateChangeResult::Waiting => {}
            StateChangeResult::Seek(seek) => self.playbin.seek_async(seek),
            StateChangeResult::ChangeState(state) => self.apply(state),
        }
        // The application pumps at the end of its StateChanged cascade.
        self.pump_selection();
    }

    /// `player.rs` `go_to_stopped_state` without the shutdown arm.
    fn on_stop(&self) {
        self.desired_transport.set(RunningState::Playing);
        if self.sm.borrow().current_state != gst::State::Null {
            self.playbin.stop_async();
        }
        self.sm.borrow_mut().clear_state();
        *self.attached.borrow_mut() = [None; EXTERNALS];
        // The source owing the 100 died with the stop.
        self.outstanding_low.set(false);
        // `player.rs` `reset_for_load`: nothing is loaded, so no load-scoped
        // event belongs to this player until the next load.
        self.expected_generation.set(None);
    }

    fn note_problem(&self, message: String) {
        let mut slot = self.problem.borrow_mut();
        if slot.is_none() {
            *slot = Some(message);
        }
    }

    fn handle(&self, event: PlaybinEvent) {
        match event {
            PlaybinEvent::Loaded { .. } => self.on_loaded(),
            PlaybinEvent::Buffering(percent) => self.on_buffering(percent),
            PlaybinEvent::StateChanged {
                old,
                current,
                pending,
            } => self.on_state_changed(old, current, pending),
            PlaybinEvent::AsyncDone => {
                self.playbin.poll_text_policy();
                self.pump_selection();
            }
            PlaybinEvent::QueueSeek(seek) => self.sm.borrow_mut().queue_seek(seek),
            PlaybinEvent::SeekFailed => {
                let dispatched = self.sm.borrow_mut().seek_failed();
                if let Some(state) = dispatched {
                    self.apply(state);
                }
            }
            PlaybinEvent::RefreshSeekFailed { .. } => self.pump_selection(),
            PlaybinEvent::RequestState(state) => self.apply(state),
            PlaybinEvent::EndOfStream => {
                self.eos_stopped.set(true);
                self.on_stop();
            }
            PlaybinEvent::StreamCollection(collection) => {
                *self.collections.borrow_mut() = Some(collection);
            }
            PlaybinEvent::Error {
                origin,
                error,
                failed_uri,
            } => self.note_problem(format!(
                "the pipeline posted an error: {error} (origin {origin:?}, uri {failed_uri:?})"
            )),
            PlaybinEvent::ExternalSubtitleFailed { id } => self.note_problem(format!(
                "the crate gave up on external subtitle {id:?}, whose URI is a \
                 registered scenario"
            )),
            _ => {}
        }
    }

    /// `player.rs` `pump_selection`, gate derived the same way.
    fn pump_selection(&self) {
        let async_busy = self.playbin.has_async_transition();
        let (running, paused) = match self.sm.borrow().running() {
            Some(state) => (true, state == RunningState::Paused),
            None => (false, false),
        };
        self.playbin.pump_selection(SelectionGate {
            quiet: running && !async_busy,
            paused,
            seekable: self.seekable,
        });
    }

    fn pump(&self) -> Checked {
        self.playbin.poll_text_policy();
        self.pump_selection();
        while let Ok((event, generation)) = self.events.try_recv() {
            // `application.rs` `handle_player_event`: a load-scoped event from
            // a superseded (or stopped) load is a straggler and is dropped in
            // one place, before any handler sees it. Without this the dying
            // item's EOS arrives after the next load was requested and stops
            // the fresh item.
            if generation_gate_enabled()
                && event_is_load_scoped(&event)
                && self.expected_generation.get() != Some(generation)
            {
                // Only visible on a failing case (libtest captures a passing
                // one), which is exactly when a dropped straggler is the
                // thing a triage needs to see.
                eprintln!(
                    "fuzz_buffering: step {} dropped {event:?} from a superseded \
                     load (generation {generation}, expected {:?})",
                    self.step.get(),
                    self.expected_generation.get()
                );
                continue;
            }
            self.handle(event);
        }
        match self.problem.borrow_mut().take() {
            None => Ok(()),
            Some(problem) => Err(self.fail(problem)),
        }
    }

    fn pump_for(&self, duration: Duration) -> Checked {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.pump()?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// `player.rs` transport entry points.
    fn transport(&self, state: RunningState) {
        self.desired_transport.set(state);
        let dispatched = self.sm.borrow_mut().set_playback_state(state);
        if let Some(target) = dispatched {
            self.apply(target);
        }
    }

    /// `player.rs` `seek_internal` with seekability known.
    fn seek(&self, position: gst::ClockTime) {
        self.playbin.cancel_selection_refresh();
        let dispatched = self.sm.borrow_mut().seek_internal(
            Seek {
                position: Some(position),
                rate: None,
            },
            None,
        );
        if let Some(seek) = dispatched {
            self.playbin.seek_async(seek);
        }
        self.count(|coverage| coverage.seeks_issued += 1);
    }

    fn load(&self, uri: &str) -> Checked {
        self.pump()?;
        self.eos_stopped.set(false);
        // The previous item's source dies with the load, taking any owed
        // 100 with it.
        self.outstanding_low.set(false);
        *self.attached.borrow_mut() = [None; EXTERNALS];
        self.desired_transport.set(RunningState::Playing);
        self.sm.borrow_mut().begin_load();
        // `player.rs` `load`: the returned generation is what the application
        // scopes every following load-scoped event to.
        let generation = self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.expected_generation.set(Some(generation));
        // The load must prove live. Either it reaches Playing or a buffering
        // dip legitimately parks it first, both are progress.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            self.pump()?;
            match self.state() {
                ReceiverState::Playing | ReceiverState::Buffering => return Ok(()),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(self.fail(format!(
                    "the load never reached Playing nor Buffering (receiver {:?}, \
                     pipeline {:?})",
                    self.state(),
                    self.playbin.state_summary()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A graph-dump round trip proves the worker is not wedged inside a job.
    fn check_worker_alive(&self, bound: Duration) -> Checked {
        // Debug-only: stretch the bound so a wedged worker HANGS rather than
        // failing, which is what makes a gdb attach at native speed possible
        // (launching under gdb perturbs the timing enough to hide the race).
        // Opt-in, so the default measurement is unchanged.
        let bound = match std::env::var("FCAST_DEBUG_WORKER_BOUND_S") {
            Ok(s) => s.parse().map(Duration::from_secs).unwrap_or(bound),
            Err(_) => bound,
        };
        let (tx, rx) = mpsc::channel();
        self.playbin.debug_graph_async(Box::new(move |_| {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + bound;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return Err(self.fail(format!(
                            "the worker never answered a graph dump within {bound:?} \
                             (receiver {:?}, pipeline {:?})",
                            self.state(),
                            self.playbin.state_summary()
                        )));
                    }
                    self.pump()?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.fail("the worker died"));
                }
            }
        }
    }

    /// A low buffering post must be followed by its 100. Every dip posts
    /// both (AfterMs on the wall clock, OnSyncPoint once released), so a low
    /// still outstanding after the bound means the completion was lost
    /// somewhere between ftestsrc, the crate's bus translation and the
    /// machine. The check deliberately judges the POST pair and not the
    /// receiver-visible state, because `running() == None` also covers a
    /// machine mid state change, and a held stall gate can legitimately park
    /// a preroll forever.
    fn check_buffering_leaves(&self, case: &Case, gate_released: bool) -> Checked {
        if !self.outstanding_low.get() {
            return Ok(());
        }
        if case.has_buffering_gate && !gate_released {
            return Ok(());
        }
        let deadline = Instant::now() + BUFFERING_LEAVE_BOUND;
        loop {
            self.pump()?;
            if !self.outstanding_low.get() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(self.fail(format!(
                    "a low buffering post was never followed by its 100 within \
                     {BUFFERING_LEAVE_BOUND:?} with no recovery gate held \
                     (receiver {:?}, pipeline {:?})",
                    self.state(),
                    self.playbin.state_summary()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn materialized_sid(&self, id: ExternalSubId) -> Option<String> {
        let deadline = Instant::now() + MATERIALIZE_BOUND;
        loop {
            if let Some(sid) = self.playbin.subtitle_stream_ids(id).into_iter().next() {
                return Some(sid);
            }
            if Instant::now() >= deadline {
                return None;
            }
            if self.pump().is_err() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn external_sids(&self) -> Vec<String> {
        self.attached
            .borrow()
            .iter()
            .flatten()
            .flat_map(|id| self.playbin.subtitle_stream_ids(*id))
            .collect()
    }

    /// First stream of `kind` in the latest collection owned by the item.
    fn item_sid(&self, kind: gst::StreamType) -> Option<String> {
        let external = self.external_sids();
        let collection = self.collections.borrow().clone()?;
        collection.iter().find_map(|stream| {
            if !stream.stream_type().contains(kind) {
                return None;
            }
            let sid = stream.stream_id()?.to_string();
            (!external.contains(&sid)).then_some(sid)
        })
    }

    fn reach(&self, anchor: Anchor) {
        let deadline = Instant::now() + ANCHOR_BOUND;
        match anchor {
            Anchor::WhileBuffering => {
                while Instant::now() < deadline {
                    if self.state() == ReceiverState::Buffering {
                        self.count(|coverage| coverage.acted_while_buffering += 1);
                        return;
                    }
                    let _ = self.pump();
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Anchor::Buffers(count) => {
                let target = self.video.buffer_count() + count;
                while self.video.buffer_count() < target && Instant::now() < deadline {
                    let _ = self.pump();
                    self.video.wait_for_buffers(target, Duration::from_millis(20));
                }
            }
            Anchor::Settled => {
                let _ = self.pump_for(Duration::from_millis(150));
            }
        }
    }

    fn run_action(&self, action: Action, case: &Case, buf_gate_released: &mut bool) -> Checked {
        self.pump()?;
        match action {
            Action::Pause => self.transport(RunningState::Paused),
            Action::Play => self.transport(RunningState::Playing),
            Action::SeekZero => self.seek(gst::ClockTime::ZERO),
            Action::SeekTo(position) => self.seek(position),
            Action::ReleaseStallGate => case.main.release(STALL_GATE),
            Action::ReleaseBufferingGate => {
                *buf_gate_released = true;
                case.main.release(BUF_GATE);
            }
            Action::AttachExternal(slot) => {
                let slot = slot as usize;
                let free = self.attached.borrow()[slot].is_none();
                if free {
                    let id = self.playbin.allocate_subtitle_id();
                    self.playbin
                        .attach_subtitle_async(id, case.externals[slot].uri());
                    self.attached.borrow_mut()[slot] = Some(id);
                    self.count(|coverage| coverage.attached += 1);
                }
            }
            Action::SelectExternal(slot) => {
                let id = self.attached.borrow()[slot as usize];
                if let Some(id) = id
                    && self.materialized_sid(id).is_some()
                {
                    self.playbin
                        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
                    self.pump_selection();
                    self.count(|coverage| coverage.selected += 1);
                }
            }
            Action::DetachExternal(slot) => {
                let id = self.attached.borrow_mut()[slot as usize].take();
                if let Some(id) = id {
                    self.playbin.detach_subtitle_async(id);
                    self.count(|coverage| coverage.detached += 1);
                }
            }
            Action::SelectEmbeddedText => {
                if let Some(sid) = self.item_sid(gst::StreamType::TEXT) {
                    self.playbin
                        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
                    self.pump_selection();
                }
            }
            Action::DisableSubtitles => {
                self.playbin
                    .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
                self.pump_selection();
            }
            Action::DisableVideo => {
                self.playbin
                    .request_track(TrackSlot::Video, TrackTarget::Stream(None));
                self.pump_selection();
            }
            Action::SelectVideo => {
                if let Some(sid) = self.item_sid(gst::StreamType::VIDEO) {
                    self.playbin
                        .request_track(TrackSlot::Video, TrackTarget::Stream(Some(sid)));
                    self.pump_selection();
                }
            }
            Action::StopReload => {
                self.on_stop();
                // A stopped worker must still answer, and the stop must not
                // wedge the queue the reload is about to use.
                self.check_worker_alive(TEARDOWN_BOUND)?;
                self.load(&case.main.uri())?;
                self.count(|coverage| coverage.reloads += 1);
            }
            Action::LoadSwap => {
                *self.attached.borrow_mut() = [None; EXTERNALS];
                self.load(&case.replacement.uri())?;
                self.count(|coverage| coverage.reloads += 1);
            }
        }
        Ok(())
    }

    /// Bounded shutdown through the worker, the receiver's own teardown path.
    fn shutdown_bounded(&self, case: &Case) -> Checked {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + TEARDOWN_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        // Unpark everything so the report is a failure and
                        // not a hung suite.
                        case.release_all();
                        return Err(self.fail("the shutdown never finished"));
                    }
                    let _ = self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.fail("the worker died during shutdown"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

fn run_case(case: &Case) -> Result<Coverage, Failure> {
    // Half the cases run with the refresh-seek path armed, drawn from the
    // case identity so a replay keeps it.
    let seekable = (case.seed ^ case.iteration) % 2 == 0;
    let runner = Runner::new(seekable);
    let mut buf_gate_released = false;

    runner.load(&case.main.uri())?;
    runner.check_worker_alive(WORKER_BOUND)?;

    for (index, (anchor, action)) in case.schedule.iter().enumerate() {
        runner.step.set(index);
        runner.reach(*anchor);
        runner.run_action(*action, case, &mut buf_gate_released)?;
        runner.pump()?;
        runner.check_worker_alive(WORKER_BOUND)?;
        runner.check_buffering_leaves(case, buf_gate_released)?;
    }

    runner.step.set(case.schedule.len());
    // End of schedule. Release every gate, then the machine must be able to
    // leave Buffering and, unless the media already ended, the pipeline must
    // actually run again.
    case.release_all();
    runner.check_buffering_leaves(case, true)?;
    if !runner.eos_stopped.get() && runner.state() != ReceiverState::Stopped {
        runner.transport(RunningState::Playing);
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            runner.pump()?;
            if runner.state() == ReceiverState::Playing {
                break;
            }
            if runner.eos_stopped.get() {
                break;
            }
            if Instant::now() >= deadline {
                // A pending state means SOME element never finished its
                // transition. Name it: the pipeline summary alone cannot tell
                // a stalled preroll apart from anything else, and every
                // triage of this class started by having to reproduce it
                // under a debugger to find out which element was stuck.
                return Err(runner.fail(format!(
                    "after releasing every gate the receiver never came back to \
                     Playing (receiver {:?}, pipeline {:?}, unsettled {:?}, machine {})\n  \
                     elements: {:?}",
                    runner.state(),
                    runner.playbin.state_summary(),
                    runner.playbin.unsettled_elements(),
                    runner.sm.borrow().debug_model(),
                    runner.playbin.element_states()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    runner.check_worker_alive(WORKER_BOUND)?;
    runner.shutdown_bounded(case)?;
    Ok(runner.coverage.get())
}

fn failure_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FCAST_FUZZ_OUT") {
        return PathBuf::from(dir);
    }
    let root = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target")
        });
    root.join("fuzz-failures")
}

fn dump(case: &Case, failure: &Failure) -> Result<PathBuf, String> {
    let dir = failure_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let dir = dir.canonicalize().unwrap_or(dir);
    let stem = format!("bufseed{}-iter{}", case.seed, case.iteration);
    for (suffix, handle) in case.handles() {
        let path = dir.join(format!("{stem}-{suffix}.toml"));
        std::fs::write(&path, toml::to_toml(handle))
            .map_err(|err| format!("{}: {err}", path.display()))?;
    }
    let path = dir.join(format!("{stem}-actions.txt"));
    let body = format!(
        "seed = {}\niteration = {}\nfailed at step {} of {}\n{}\n\nschedule:\n{}\n",
        case.seed,
        case.iteration,
        failure.step,
        case.schedule.len(),
        failure.message,
        case.trace()
    );
    std::fs::write(&path, body).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(dir)
}

#[test]
#[ignore = "stress driver, run with --ignored"]
fn fuzz_buffering_schedules() {
    init();
    let seed = env_u64("FCAST_FUZZ_SEED", 1);
    let iters = env_u64("FCAST_FUZZ_ITERS", DEFAULT_ITERS);
    let actions = env_u64("FCAST_FUZZ_ACTIONS", DEFAULT_ACTIONS) as usize;
    eprintln!(
        "fuzz_buffering: seed={seed} iters={iters} actions={actions} \
         (override with FCAST_FUZZ_SEED / FCAST_FUZZ_ITERS / FCAST_FUZZ_ACTIONS)"
    );

    let mut total = Coverage::default();
    for iteration in 0..iters {
        let case = Case::generate(seed, iteration, actions);
        let started = Instant::now();
        match run_case(&case) {
            Ok(coverage) => {
                eprintln!(
                    "fuzz_buffering: iteration {iteration} passed {} actions in {:?} \
                     ({coverage})",
                    case.schedule.len(),
                    started.elapsed()
                );
                total.add(coverage);
                case.unregister();
            }
            Err(failure) => {
                let mut report = format!(
                    "fuzz_buffering FAILED\n  seed: {seed}\n  iteration: {iteration}\n  \
                     step: {} of {}\n  what: {}\n  schedule:\n{}\n",
                    failure.step,
                    case.schedule.len(),
                    failure.message,
                    case.trace()
                );
                match dump(&case, &failure) {
                    Ok(dir) => report.push_str(&format!(
                        "  written to: {}\n  replay: FCAST_FUZZ_SEED={seed} \
                         FCAST_FUZZ_ITERS={} FCAST_FUZZ_ACTIONS={actions} cargo test \
                         -p fcastplaybin --test fuzz_buffering -- --ignored --nocapture\n",
                        dir.display(),
                        iteration + 1
                    )),
                    Err(err) => report.push_str(&format!("  (could not write the dump: {err})\n")),
                }
                case.unregister();
                panic!("{report}");
            }
        }
    }

    // A run that never entered Buffering exercised nothing this driver is
    // for, and one that never attached or selected proved nothing about the
    // interleavings under test. Only enforced at the full budget, a shrunk
    // replay is deliberately tiny.
    if iters >= DEFAULT_ITERS && actions >= DEFAULT_ACTIONS as usize {
        eprintln!("fuzz_buffering: run total ({total})");
        for (name, count) in [
            ("buffering_posts", total.buffering_posts),
            ("attach", total.attached),
            ("select", total.selected),
            ("seeks", total.seeks_issued),
        ] {
            assert!(
                count > 0,
                "the whole run never once reached `{name}`, so every green \
                 iteration proved nothing about it: {total}"
            );
        }
    }
}
