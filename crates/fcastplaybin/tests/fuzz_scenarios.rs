//! Seeded stress driver: random media, random action schedules, invariants after
//! every action. Opt-in, so a normal `cargo test` never pays for it.
//!
//! ```text
//! cargo test -p fcastplaybin --test fuzz_scenarios -- --ignored --nocapture
//! FCAST_FUZZ_SEED=7 FCAST_FUZZ_ITERS=50 FCAST_FUZZ_ACTIONS=24 \
//!   cargo test -p fcastplaybin --test fuzz_scenarios -- --ignored --nocapture
//! ```
//!
//! Everything is derived from `FCAST_FUZZ_SEED` through [`Prng`]: media, knobs,
//! action order, the slot and seek position every action carries, and the anchor
//! each action waits on. No `thread_rng`, no clock reads, so
//! `FCAST_FUZZ_SEED=<n> FCAST_FUZZ_ITERS=1` after a failure regenerates
//! byte-identical media and the same schedule.
//!
//! The track-lifecycle permutations are what this driver exists for: attach an
//! external, select it, attach another and switch, disable, come back to the
//! first, take the embedded text, drop video, seek, take video back, seek again,
//! attach a third without selecting it, detach, select the newest, seek, pause,
//! resume. The schedule is a random walk over exactly those moves, so every
//! interleaving of them is reachable from some seed.
//!
//! Two seek shapes are scheduled. `seek_zero` is the raw restart the receiver's
//! own zero-seek issues, and `seek_to` is the receiver's mid-stream dance
//! (pause, seek from a settled PAUSED, resume), which is the only shape that
//! moves the timeline origin away from zero and therefore the only one that can
//! expose a text branch rendering against the wrong timeline.
//!
//! The invariants after every action are: no pipeline error and no
//! `ExternalSubtitleFailed` (every generated URI is a registered scenario, so
//! either means the crate gave up on something healthy), a worker that still
//! answers, the sink sequence sweep at settled points, and the timeline
//! alignment of the subtitle branch (see [`Runner::check_timeline_alignment`]).
//!
//! A failing iteration is written to `target/fuzz-failures/` as the scenario files
//! plus the action list, so it replays without the fuzzer.
//!
//! Note on GStreamer: `cargo test` links the DYNAMIC system GStreamer, which
//! carries none of `xtask/patches/`. Schedules that churn text branches reach
//! upstream bugs the shipped (static, patched) receiver does not have, so a
//! failure here is worth checking against that patch list before it is filed as
//! a crate bug.

use std::{
    cell::{Cell, RefCell},
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent, Seek, SelectionGate, Sinks,
    StartPoint, TrackSlot, TrackTarget,
};
use fcasttest::{
    prng::Prng,
    scenario::{ScenarioBuilder, ScenarioHandle, check_all_named, toml, wait_quiescent},
    sink::{FTestSink, Recording},
    spec::{CueSpec, DecoderKnobs, Fault, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

/// Bound for anything the pipeline has to reach.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound for a stop or a load that has to tear down a parked source.
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);
/// The worker must answer a queued job inside this.
const WORKER_BOUND: Duration = Duration::from_secs(10);
/// Bound for an external input's stream to appear in the collection before a
/// selection can name it. Soft: an input that has not materialized yet is simply
/// not selected this step, which is a lost permutation and not a failure.
const MATERIALIZE_BOUND: Duration = WORKER_BOUND;
/// Bound for one leg of the receiver's seek dance. Soft, like
/// [`MATERIALIZE_BOUND`]: the transport is allowed to refuse a seek.
const SEEK_STEP_BOUND: Duration = WORKER_BOUND;
/// How long a timeline mismatch has to persist before it counts as one. The
/// realigning replay is asynchronous, so a snapshot taken right after a
/// selection legally still shows the old origin.
const ALIGN_BOUND: Duration = WORKER_BOUND;
/// No log growth for this long counts as quiescent for the invariant sweep.
const QUIESCENT_SETTLE: Duration = Duration::from_millis(200);
/// Longest an anchor waits before the action just proceeds. An anchor is a bias,
/// not a correctness wait: missing it is not a failure.
const ANCHOR_BOUND: Duration = Duration::from_millis(600);
/// How many CONSECUTIVE misaligned cues count as a flush window rather than a
/// branch stuck on the wrong timeline. A seek flushes the video and text
/// branches independently, so cues can legitimately cross before the realigned
/// segment overtakes them.
///
/// Waiting longer cannot rescue a window that already exceeded this: the figure
/// is the MAXIMUM run seen so far and never shrinks, so the check reports as
/// soon as it is exceeded rather than burning the bound first. That makes the
/// value the whole tolerance, so it is calibrated from data rather than taste.
/// The driver prints the observed `worst_run` per run: seeds 1 and 42, ten
/// iterations and sixteen judged sweeps, measured 0 EVERY time. Three therefore
/// leaves room for a flush burst that has never actually been seen, while
/// staying two orders of magnitude tighter than the regression it exists for (a
/// branch that misaligns every cue it delivers).
const FORGIVEN_STRAGGLERS: usize = 3;
/// How long a check waits for the FIRST cue when a text stream is selected but
/// nothing has crossed the overlay yet. Much shorter than [`ALIGN_BOUND`] (which
/// exists for a misalignment to converge) purely for cost: this look happens on
/// every selection sweep, including the many that legitimately have no cue due
/// yet, and burning the full bound on each would multiply the driver's runtime.
const STARVE_LOOK: Duration = Duration::from_secs(1);
/// Name every generated stall gate uses.
const GATE: &str = "fuzzgate";
/// The documented default budget. Named so the coverage floor at the end of
/// [`fuzz_action_schedules`] can tell a full run from a deliberately tiny
/// replay of a shrunk case.
const DEFAULT_ITERS: u64 = 5;
const DEFAULT_ACTIONS: u64 = 12;
/// Pre-registered text-only scenarios per case. Three is the smallest pool that
/// covers the moves under test: switch between two, then bring in a third while
/// the other two are still attached.
const EXTERNALS: usize = 3;
/// Payload prefix per external slot, so a dump names which input a cue came from.
const PREFIXES: [&str; EXTERNALS] = ["EXTA", "EXTB", "EXTC"];
/// Length of every pooled text scenario. It has to outlive the main item by a
/// wide margin: a switch back to an input that merely ENDED is an ordinary EOS,
/// while a switch back to one that is still alive (or died deselected) is the
/// reactivation the crate's selection-time replay exists for.
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
        // The receiver's part of the pipeline: fcastaudiostretch is built by
        // the fcastplaybin constructor but registered by the application.
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

/// An action without its drawn parameters. Only the generator uses this: the
/// schedule stores fully drawn [`Action`]s so a replay never needs the PRNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pause,
    Play,
    SeekZero,
    SeekTo,
    ReleaseGate,
    AttachExternal,
    SelectExternal,
    DetachExternal,
    SelectEmbeddedText,
    DisableSubtitles,
    DisableVideo,
    SelectVideo,
    StopReload,
    LoadSwap,
}

/// The draw table: repetition IS the weight. Attaching and selecting externals
/// dominate because the permutations under test need a populated pool before a
/// selection can switch between two live inputs, and a uniform draw over
/// fourteen kinds populates one too rarely to reach the interesting states
/// inside a schedule of realistic length.
const DRAW: [Kind; 24] = [
    Kind::Pause,
    Kind::Play,
    Kind::SeekZero,
    Kind::SeekTo,
    Kind::SeekTo,
    Kind::ReleaseGate,
    Kind::AttachExternal,
    Kind::AttachExternal,
    Kind::AttachExternal,
    Kind::AttachExternal,
    Kind::SelectExternal,
    Kind::SelectExternal,
    Kind::SelectExternal,
    Kind::SelectExternal,
    Kind::SelectExternal,
    Kind::DetachExternal,
    Kind::SelectEmbeddedText,
    Kind::SelectEmbeddedText,
    Kind::DisableSubtitles,
    Kind::DisableSubtitles,
    Kind::DisableVideo,
    Kind::SelectVideo,
    Kind::StopReload,
    Kind::LoadSwap,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Pause,
    Play,
    /// Flushing seek to zero, the receiver's own restart shape.
    SeekZero,
    /// The receiver's mid-stream seek: pause, seek from a settled PAUSED,
    /// resume. Position drawn at generation time, so it is data like everything
    /// else in the schedule.
    SeekTo(gst::ClockTime),
    /// Lets a push parked on the generated stall gate continue.
    ReleaseGate,
    /// Attach the pool's slot-th text scenario WITHOUT selecting it. Attaching
    /// an already attached slot is a no-op.
    AttachExternal(u8),
    /// Point the subtitle slot at an attached external input.
    SelectExternal(u8),
    DetachExternal(u8),
    /// Select the item's own text stream, when the generated media has one.
    SelectEmbeddedText,
    DisableSubtitles,
    DisableVideo,
    SelectVideo,
    /// stop() and load the same media again.
    StopReload,
    /// Load the replacement media over whatever is playing.
    LoadSwap,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pause => f.write_str("pause"),
            Self::Play => f.write_str("play"),
            Self::SeekZero => f.write_str("seek_zero"),
            Self::SeekTo(position) => write!(f, "seek_to({} ms)", position.mseconds()),
            Self::ReleaseGate => f.write_str("release_gate"),
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

/// What an action waits for before it runs. Uniform sleeps find nothing: the bugs
/// this replaces all lived at a lifecycle edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// Run while a push is provably parked on the stall gate.
    GateHeld,
    /// Run after `n` more buffers rendered, so mid-stream and not mid-preroll.
    Buffers(usize),
    /// Run once the sinks stopped growing.
    Quiescent,
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GateHeld => f.write_str("gate_held"),
            Self::Buffers(n) => write!(f, "after_{n}_buffers"),
            Self::Quiescent => f.write_str("quiescent"),
        }
    }
}

/// One generated iteration: the media, the external-subtitle pool and the
/// schedule over them.
struct Case {
    iteration: u64,
    seed: u64,
    main: ScenarioHandle,
    replacement: ScenarioHandle,
    /// Text-only scenarios, one per slot, distinguished by cue payload prefix.
    externals: [ScenarioHandle; EXTERNALS],
    schedule: Vec<(Anchor, Action)>,
}

impl Case {
    fn generate(seed: u64, iteration: u64, actions: usize) -> Self {
        let mut prng = Prng::new(seed ^ iteration.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let tag = format!("fz{seed}_{iteration}");

        let (main, main_duration) = generate_media(&mut prng, &format!("{tag}m"), true);
        let (replacement, _) = generate_media(&mut prng, &format!("{tag}r"), false);
        // Text-only media, attached as external subtitle sources. Longer than
        // the main item on purpose: an input that outlives the video is what
        // keeps a switch back to it a reactivation and not an EOS. Unpaced, so
        // a selected input reaches the overlay at once instead of trickling: an
        // external whose segment never arrives is an external the timeline
        // invariant cannot say anything about.
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

        let mut pool = Pool::default();
        let schedule = (0..actions)
            .map(|_| {
                let action = draw_action(&mut prng, main_duration, &mut pool);
                let anchor = match prng.next_range(0..3) {
                    0 => Anchor::GateHeld,
                    1 => Anchor::Buffers(prng.next_range(1..4) as usize),
                    _ => Anchor::Quiescent,
                };
                (anchor, action)
            })
            .collect();

        Self {
            iteration,
            seed,
            main,
            replacement,
            externals,
            schedule,
        }
    }

    fn with_schedule(&self, schedule: Vec<(Anchor, Action)>) -> Self {
        Self {
            iteration: self.iteration,
            seed: self.seed,
            main: self.main.clone(),
            replacement: self.replacement.clone(),
            externals: self.externals.clone(),
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

    /// Every handle the case registered, with the name its dump file carries.
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

/// A purely syntactic model of the runner's slot bookkeeping, carried along
/// while the schedule is drawn. It reads nothing from a pipeline (there is none
/// yet) and only biases the slot draws toward slots the runner will plausibly be
/// able to act on: a schedule that keeps selecting slots it never attached is a
/// schedule of no-ops, and the permutations under test all need a populated
/// pool. The model can be wrong at run time (the crate may refuse an attach),
/// which costs coverage and never correctness.
#[derive(Default, Clone, Copy)]
struct Pool {
    attached: [bool; EXTERNALS],
}

impl Pool {
    /// A slot that is (or is not) attached according to the model. Exactly two
    /// draws either way, so the PRNG stream does not depend on the branch.
    fn pick(&self, prng: &mut Prng, want_attached: bool) -> u8 {
        // A quarter of the draws ignore the model, keeping the no-op edges
        // (selecting a slot nobody attached, attaching one twice) in the space.
        let ignore_model = prng.next_range(0..4) == 0;
        let candidates: Vec<u8> = (0..EXTERNALS as u8)
            .filter(|slot| self.attached[*slot as usize] == want_attached)
            .collect();
        if ignore_model || candidates.is_empty() {
            return prng.next_range(0..EXTERNALS as u64) as u8;
        }
        candidates[prng.next_range(0..candidates.len() as u64) as usize]
    }
}

/// One schedule entry, parameters included. Every draw happens here, so the
/// schedule a replay runs is exactly the schedule the failing run ran.
fn draw_action(prng: &mut Prng, main_duration: gst::ClockTime, pool: &mut Pool) -> Action {
    let kind = DRAW[prng.next_range(0..DRAW.len() as u64) as usize];
    match kind {
        Kind::Pause => Action::Pause,
        Kind::Play => Action::Play,
        Kind::SeekZero => Action::SeekZero,
        Kind::SeekTo => {
            // Inside the main item. A position past the end is not an error
            // (the source just ends), but it costs the rest of the schedule
            // its media.
            let span = main_duration.mseconds().max(1);
            Action::SeekTo(gst::ClockTime::from_mseconds(prng.next_range(0..span)))
        }
        Kind::ReleaseGate => Action::ReleaseGate,
        Kind::AttachExternal => {
            let slot = pool.pick(prng, false);
            pool.attached[slot as usize] = true;
            Action::AttachExternal(slot)
        }
        Kind::SelectExternal => Action::SelectExternal(pool.pick(prng, true)),
        Kind::DetachExternal => {
            let slot = pool.pick(prng, true);
            pool.attached[slot as usize] = false;
            Action::DetachExternal(slot)
        }
        Kind::SelectEmbeddedText => Action::SelectEmbeddedText,
        Kind::DisableSubtitles => Action::DisableSubtitles,
        Kind::DisableVideo => Action::DisableVideo,
        Kind::SelectVideo => Action::SelectVideo,
        // Externals are per play item, so both of these empty the pool.
        Kind::StopReload => {
            pool.attached = [false; EXTERNALS];
            Action::StopReload
        }
        Kind::LoadSwap => {
            pool.attached = [false; EXTERNALS];
            Action::LoadSwap
        }
    }
}

/// Media with a video and an audio stream, sometimes an embedded text stream, and
/// sometimes a stall gate on the video. No error or EOS fault is ever injected, so
/// "the pipeline posted an error" stays an unambiguous failure. Returns the
/// duration too: the seek positions in the schedule are drawn against it.
fn generate_media(
    prng: &mut Prng,
    key: &str,
    allow_stall: bool,
) -> (ScenarioHandle, gst::ClockTime) {
    // Long enough that the item is still playing several actions in. A clip
    // that ends by action three spends the rest of the schedule on a drained
    // pipeline, where a track switch has nothing left to switch.
    let duration = gst::ClockTime::from_mseconds(prng.next_range(2500..9000));
    let pacing = match prng.next_range(0..3) {
        0 => Pacing::AsFastAsPossible,
        1 => Pacing::Realtime,
        _ => Pacing::Jitter {
            base_ms: prng.next_range(0..8),
            jitter_ms: prng.next_range(0..5),
        },
    };
    let fps = prng.next_range(0..3);
    let fps = [25i32, 10, 5][fps as usize];

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
    // A gate makes "the action landed mid-push" a fact instead of a probability.
    if allow_stall && prng.next_range(0..3) == 0 {
        // The reordering decoder holds `reorder_frames` back, so a stall
        // that close to the start would starve the video sink's preroll and
        // make the initial settled PLAYING unreachable by construction
        // (seed 6 iteration 1 drew stall 2 against reorder 2). At least one
        // frame must be able to REACH the sink with the gate held.
        let earliest = u64::from(reorder_frames) + 2;
        video = video.with_fault(Fault::StallAt {
            buffer_index: prng.next_range(earliest..earliest.max(9)),
            sync_point: GATE.to_owned(),
        });
    }

    let mut builder = ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"));
    if prng.next_bool() {
        // Deliberately spent inside the first half second, before the schedule
        // can run. ftestsrc models every stream of a scenario as its own source
        // with its own task and NO flow combiner across them, so ONE pad going
        // not-linked kills the whole input after `NOT_LINKED_BOUND` (2s). A
        // real multi-stream source aggregates instead (matroskademux only fails
        // when EVERY pad is not-linked), so a demuxer whose subtitle pad is
        // deselected keeps playing. Selecting an external subtitle makes the
        // embedded text stream slotless in decodebin3, which is exactly a
        // deselected pad: cues that are still pending then take the whole main
        // input down and report a bug the field does not have. Gaps are pushed
        // with `push_event` and their failure is ignored, so a spent text
        // stream is safe to deselect at any point.
        builder = builder.text("text_0", cues(4, gst::ClockTime::from_mseconds(100)));
    }
    let handle = builder
        .duration(duration)
        .bytes_per_buffer(if prng.next_bool() { 64 } else { 1024 })
        .pacing(pacing)
        .register();
    (handle, duration)
}

fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("CUE{index:02}"))
        })
        .collect()
}

/// Cues with a per-slot payload prefix, so a dumped case says which external
/// input a rendered cue came from.
fn prefixed_cues(prefix: &str, count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{prefix}{index:02}"))
        })
        .collect()
}

/// The timeline a pad renders against: the stream position whose running time is
/// zero, read off the pad's sticky segment exactly like the crate's
/// `overlay_timeline` (and like `segment_origin` in `tests/scenarios.rs`). Cues
/// sync against video iff both pads report the same origin.
fn segment_origin(pad: &gst::Pad) -> Option<gst::ClockTime> {
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

/// Longest run of consecutive entries whose two origins differ. This is what
/// separates a flush window (a couple in a row, then the realigned segment
/// overtakes them) from a branch pinned to the wrong timeline (every cue).
fn longest_misaligned_run(window: &[(gst::ClockTime, gst::ClockTime)]) -> usize {
    let mut worst = 0usize;
    let mut run = 0usize;
    for (video, text) in window {
        if video == text {
            run = 0;
        } else {
            run += 1;
            worst = worst.max(run);
        }
    }
    worst
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// A failure carries everything needed to write a replayable case. Checks return
/// this instead of panicking so the driver can dump and, optionally, shrink.
struct Failure {
    /// Index into the schedule, or the length when the failure came after it.
    step: usize,
    message: String,
}

type Checked = Result<(), Failure>;

/// Per-case tally of the moves that actually took effect, printed next to the
/// pass line. A schedule whose external actions all no-op (nothing attached, so
/// nothing to select) is a green case that proves nothing, and only a counter
/// distinguishes that from a schedule that really switched inputs around.
/// `aligned` is the one that matters most: it counts the sweeps where the
/// timeline invariant had a real pair of origins to compare.
#[derive(Default, Clone, Copy)]
struct Coverage {
    attached: usize,
    selected: usize,
    detached: usize,
    embedded_text: usize,
    video_off: usize,
    video_on: usize,
    /// Seek dances that actually COMPLETED every leg. A dance whose PAUSED,
    /// seek-answer or PLAYING wait timed out is counted in `seeks_refused`
    /// instead: counting it here would let the run-level floor be satisfied by
    /// seeks that never happened, which is what it used to do.
    seeks: usize,
    seeks_refused: usize,
    aligned: usize,
    /// Sweeps where a text stream WAS selected and yet no cue crossed the
    /// overlay within the bound. Not a failure (an input may legitimately be
    /// between segments), but an all-starved run means the timeline invariant
    /// never had anything to judge and must not look like a covered one.
    starved: usize,
    /// Longest run of CONSECUTIVE misaligned cues tolerated over the run. The
    /// forgiveness threshold is calibrated against this, so a regression that
    /// widens flush windows shows up as a number rather than as silence.
    worst_misaligned_run: usize,
}

impl Coverage {
    /// Fold one iteration's tally into the run total.
    fn add(&mut self, other: Coverage) {
        self.attached += other.attached;
        self.selected += other.selected;
        self.detached += other.detached;
        self.embedded_text += other.embedded_text;
        self.video_off += other.video_off;
        self.video_on += other.video_on;
        self.seeks += other.seeks;
        self.seeks_refused += other.seeks_refused;
        self.aligned += other.aligned;
        self.starved += other.starved;
        self.worst_misaligned_run = self.worst_misaligned_run.max(other.worst_misaligned_run);
    }

    /// Counters that stayed at zero for the whole run, i.e. moves the driver
    /// documents as its reason for existing that it never actually made.
    fn missed(&self) -> Vec<&'static str> {
        [
            ("attach", self.attached),
            ("select", self.selected),
            ("detach", self.detached),
            ("embedded", self.embedded_text),
            ("video_off", self.video_off),
            ("video_on", self.video_on),
            ("seek_to", self.seeks),
            ("aligned", self.aligned),
        ]
        .into_iter()
        .filter_map(|(name, count)| (count == 0).then_some(name))
        .collect()
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "attach {} select {} detach {} embedded {} video off/on {}/{} \
             seek_to {}(+{} refused) aligned {} starved {} worst_run {}",
            self.attached,
            self.selected,
            self.detached,
            self.embedded_text,
            self.video_off,
            self.video_on,
            self.seeks,
            self.seeks_refused,
            self.aligned,
            self.starved,
            self.worst_misaligned_run
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

struct Runner {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: Mutex<Vec<PlaybinEvent>>,
    video: Recording,
    audio: Arc<Mutex<Vec<Recording>>>,
    paused: Cell<bool>,
    /// One entry per pooled text scenario. Externals are per play item, so a
    /// stop or a load clears the whole array.
    attached: RefCell<[Option<ExternalSubId>; EXTERNALS]>,
    /// Loads issued so far. The caller-owned video log only belongs to one load, so
    /// the sweep drops it once a second load happened (a swap's teardown legally
    /// leaves the log ending mid-flush).
    loads: Cell<u32>,
    /// The generation of the load this driver expects events for, exactly as
    /// `player.rs` keeps it. `None` while stopped. Load-scoped events from any
    /// other generation are a superseded load's stragglers and are dropped in
    /// [`Runner::admit`], the way `application.rs` drops them.
    expected_generation: Cell<Option<u64>>,
    step: Cell<usize>,
    coverage: Cell<Coverage>,
    /// One entry per cue that crossed subtitleoverlay: the video's timeline
    /// origin at that instant and the cue's own. See [`Runner::tap_overlay`].
    alignment: Arc<Mutex<Vec<(gst::ClockTime, gst::ClockTime)>>>,
    /// How much of `alignment` a passing check has already accepted. Everything
    /// past it is the window the next check judges, so a misaligned burst
    /// cannot be hidden by two clean cues landing after it.
    alignment_cursor: Cell<usize>,
    /// The overlay instance the tap is installed on, so a rebuilt one is tapped
    /// again and a surviving one is not tapped twice.
    tapped: RefCell<Option<gst::Element>>,
}

impl Runner {
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
            log: Mutex::new(Vec::new()),
            video,
            audio,
            paused: Cell::new(false),
            attached: RefCell::new([None; EXTERNALS]),
            loads: Cell::new(0),
            expected_generation: Cell::new(None),
            step: Cell::new(0),
            coverage: Cell::new(Coverage::default()),
            alignment: Arc::new(Mutex::new(Vec::new())),
            alignment_cursor: Cell::new(0),
            tapped: RefCell::new(None),
        }
    }

    /// Bump one counter of the case tally, see [`Coverage`].
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

    fn audio_log(&self) -> Option<Recording> {
        self.audio
            .lock()
            .expect("audio recording slot")
            .last()
            .cloned()
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        });
    }

    /// Events the driver treats as a case failure on sight. No generated fault
    /// injects an error, so any error at all is one. `ExternalSubtitleFailed` is
    /// the same kind of statement about the externals: every generated URI is
    /// valid and resolves to a registered scenario, so the crate reporting one
    /// dead means it gave up on a live input. A death handled by the recover
    /// path never reaches this event, so it is not noisy.
    fn problem(event: &PlaybinEvent) -> Option<String> {
        match event {
            PlaybinEvent::Error {
                origin,
                error,
                failed_uri,
            } => Some(format!(
                "the pipeline posted an error: {error} (origin {origin:?}, uri {failed_uri:?})"
            )),
            PlaybinEvent::ExternalSubtitleFailed { id } => Some(format!(
                "the crate gave up on external subtitle {id:?}, whose URI is a \
                 registered scenario"
            )),
            _ => None,
        }
    }

    /// `application.rs` `handle_player_event`: a load-scoped event from a
    /// superseded (or stopped) load is a straggler and is dropped in one
    /// place, before any handler sees it. Without this the dying item's EOS,
    /// error or state edge is acted on as if it belonged to the item now
    /// playing, which the receiver never does.
    ///
    /// Returns whether the event may be handled at all.
    fn admit(&self, event: &PlaybinEvent, generation: u64) -> bool {
        if !generation_gate_enabled()
            || !event_is_load_scoped(event)
            || self.expected_generation.get() == Some(generation)
        {
            return true;
        }
        // Named on stderr because the gate makes the driver LESS sensitive: a
        // genuine crate bug that manifests as a wrongly stamped event is now
        // dropped, and a triage has to be able to see that it was.
        eprintln!(
            "fuzz_scenarios: step {} dropped {event:?} from a superseded load \
             (generation {generation}, expected {:?})",
            self.step.get(),
            self.expected_generation.get()
        );
        false
    }

    /// Moves every pending event into the log. Returns whether `pred` matched one
    /// of them, plus the first problem it saw.
    fn drain_matching(
        &self,
        mut pred: impl FnMut(&PlaybinEvent) -> bool,
    ) -> (bool, Option<String>) {
        let mut hit = false;
        let mut problem = None;
        let mut log = self.log.lock().expect("event log");
        while let Ok((event, generation)) = self.events.try_recv() {
            if !self.admit(&event, generation) {
                continue;
            }
            if problem.is_none() {
                problem = Self::problem(&event);
            }
            hit |= pred(&event);
            log.push(event);
        }
        (hit, problem)
    }

    /// Moves every pending event into the log. Returns the first problem it saw.
    fn drain(&self) -> Option<String> {
        self.drain_matching(|_| false).1
    }

    /// Drain and report. Never discard the result of a drain: an event already
    /// moved into the log is not re-examined by a later one.
    fn check_events(&self) -> Checked {
        match self.drain() {
            None => Ok(()),
            Some(problem) => Err(self.fail(problem)),
        }
    }

    /// A graph-dump round-trip proves the worker is not wedged inside a job.
    fn check_worker_alive(&self) -> Checked {
        let (tx, rx) = mpsc::channel();
        self.playbin.debug_graph_async(Box::new(move |_| {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + WORKER_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return Err(self.fail("the worker never answered a graph dump"));
                    }
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.fail("the worker died"));
                }
            }
        }
    }

    /// Sinks whose log provably belongs to the current load, see [`Runner::loads`].
    fn sweepable(&self) -> Vec<(&'static str, Recording)> {
        let mut out = Vec::new();
        if self.loads.get() <= 1 {
            out.push(("video", self.video.clone()));
        }
        if let Some(audio) = self.audio_log() {
            out.push(("audio", audio));
        }
        out
    }

    /// The sequence sweep, only ever run at a quiescent point: a snapshot taken
    /// mid-flushing-seek legally ends with an unmatched FLUSH_START.
    fn check_sequences(&self) -> Checked {
        let owned = self.sweepable();
        if owned.is_empty() {
            return Ok(());
        }
        let borrowed: Vec<(&str, &Recording)> = owned
            .iter()
            .map(|(name, recording)| (*name, recording))
            .collect();
        if !wait_quiescent(&borrowed, QUIESCENT_SETTLE, EVENT_TIMEOUT) {
            return Err(self.fail("the sinks never went quiescent"));
        }
        check_all_named(&borrowed).map_err(|violations| self.fail(violations))
    }

    /// The latest advertised collection.
    fn last_collection(&self) -> Option<gst::StreamCollection> {
        self.log
            .lock()
            .expect("event log")
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => Some(collection.clone()),
                _ => None,
            })
    }

    /// Every stream id the attached external inputs produced. The collection
    /// carries these next to the item's own streams, so this is what tells an
    /// embedded text stream apart from an external one.
    fn external_sids(&self) -> Vec<String> {
        self.attached
            .borrow()
            .iter()
            .flatten()
            .flat_map(|id| self.playbin.subtitle_stream_ids(*id))
            .collect()
    }

    /// First stream of `kind` in the latest collection that no attached external
    /// input owns, so this always names one of the play item's own streams.
    fn item_sid(&self, kind: gst::StreamType) -> Option<String> {
        let external = self.external_sids();
        let collection = self.last_collection()?;
        collection.iter().find_map(|stream| {
            if !stream.stream_type().contains(kind) {
                return None;
            }
            let sid = stream.stream_id()?.to_string();
            (!external.contains(&sid)).then_some(sid)
        })
    }

    /// Tap the overlay's subtitle input so the timeline invariant judges DATA
    /// and not pad state. Comparing the two pads' sticky segments directly is
    /// unsound: a branch whose stream produced nothing since the last flush
    /// legally keeps the segment it had (a flush clears it only on the pads the
    /// flush reaches), and no cue can ever render against a stale one, because
    /// a stream that resumes pushes its own segment ahead of its first buffer.
    /// What a viewer actually sees is the origin governing each cue AS IT
    /// CROSSES, so that is what gets recorded.
    ///
    /// Idempotent per overlay instance: a video disable takes the element out
    /// and a re-select brings a fresh one in, which needs its own probe.
    fn tap_overlay(&self) {
        let Some(overlay) = self.playbin.pipeline().by_name("fpb-suboverlay") else {
            return;
        };
        if self.tapped.borrow().as_ref() == Some(&overlay) {
            return;
        }
        let (Some(text), Some(video)) = (
            overlay.static_pad("subtitle_sink"),
            overlay.static_pad("video_sink"),
        ) else {
            return;
        };
        let record = self.alignment.clone();
        text.add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
            // Both origins or neither: a video pad with no segment is a branch
            // mid-flush, which has no timeline to be compared against yet.
            if let (Some(text_origin), Some(video_origin)) =
                (segment_origin(pad), segment_origin(&video))
            {
                record
                    .lock()
                    .expect("alignment tap")
                    .push((video_origin, text_origin));
            }
            gst::PadProbeReturn::Ok
        });
        *self.tapped.borrow_mut() = Some(overlay);
    }

    /// A cue must render against the SAME timeline origin as the video. The
    /// regression this guards: switching between two externals reuses the
    /// already linked text pad, so a crate that only replays on a JOIN leaves
    /// the new input on its own `[0, ..)` segment and every cue it delivers
    /// renders shifted by the video's origin.
    ///
    /// The realign is asynchronous and a seek flushes the two branches
    /// independently, so a SHORT burst of misaligned cues around a flush is
    /// legal. A branch that KEEPS delivering against the wrong origin is the
    /// bug, and the two are told apart by the longest run of CONSECUTIVE
    /// misaligned cues, not by the last entry alone.
    ///
    /// This used to look only at `record[len-2..]`, which meant 400 misaligned
    /// cues followed by two aligned ones passed: the tail was clean, so the
    /// window that contained the whole regression was never read. The check now
    /// walks every cue recorded since the last passing check (`alignment_cursor`)
    /// and forgives at most [`FORGIVEN_STRAGGLERS`] in a row.
    ///
    /// It also no longer treats "no cue crossed at all" as success when a text
    /// stream IS selected: that is the shape of a dead branch, so it waits the
    /// bound out and books the outcome as `starved` rather than passing
    /// instantly and silently.
    fn check_timeline_alignment(&self) -> Checked {
        self.tap_overlay();
        let deadline = Instant::now() + ALIGN_BOUND;
        let starve_deadline = Instant::now() + STARVE_LOOK;
        let text_selected = self.subtitle_selected();
        let mut counted = false;
        loop {
            self.check_events()?;
            let cursor = self.alignment_cursor.get();
            let (window, total) = {
                let record = self.alignment.lock().expect("alignment tap");
                let cursor = cursor.min(record.len());
                (record[cursor..].to_vec(), record.len())
            };

            let Some((video, text)) = window.last().copied() else {
                // Nothing new since the last passing check. With no text
                // selected there is nothing to be starved OF, so this is an
                // honest pass. With one selected it is the shape of a dead
                // branch, so look for a first cue before giving up, and book
                // the outcome instead of passing silently.
                if !text_selected || Instant::now() >= starve_deadline {
                    if text_selected && total == 0 {
                        self.count(|coverage| coverage.starved += 1);
                    }
                    return Ok(());
                }
                self.settle_pump();
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };

            if !counted {
                counted = true;
                self.count(|coverage| coverage.aligned += 1);
            }

            let worst = longest_misaligned_run(&window);
            self.count(|coverage| {
                coverage.worst_misaligned_run = coverage.worst_misaligned_run.max(worst)
            });

            // Settled and clean: the tail is aligned and no forgiven-length
            // burst was exceeded anywhere in the window.
            if video == text && worst <= FORGIVEN_STRAGGLERS {
                self.alignment_cursor.set(total);
                return Ok(());
            }
            if worst > FORGIVEN_STRAGGLERS {
                return Err(self.fail(format!(
                    "{worst} CONSECUTIVE cues rendered against a different origin than \
                     the video (last pair: video {video}, text {text}); at most \
                     {FORGIVEN_STRAGGLERS} in a row are forgiven as a flush window, so \
                     the selected text input was not realigned onto the item's timeline"
                )));
            }
            if Instant::now() >= deadline {
                if window.len() < 2 || window[window.len() - 2].0 == window[window.len() - 2].1 {
                    // One straggler from a flush window, not a stuck branch.
                    self.alignment_cursor.set(total);
                    return Ok(());
                }
                return Err(self.fail(format!(
                    "cues keep rendering against origin {text} while the video \
                     renders against {video}: the selected text input was never \
                     realigned onto the item's timeline"
                )));
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether the latest confirmed selection names a subtitle stream.
    fn subtitle_selected(&self) -> bool {
        self.log
            .lock()
            .expect("event log")
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.is_some()),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Everything that only holds once the pipeline settled.
    fn check_quiescent(&self) -> Checked {
        self.check_sequences()?;
        self.check_timeline_alignment()
    }

    /// Wait until `pred` matches a newly received event, pumping between polls.
    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) -> Checked {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return Err(self.fail(format!("timed out waiting for {what}")));
            }
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, generation)) => {
                    if !self.admit(&event, generation) {
                        continue;
                    }
                    let problem = Self::problem(&event);
                    let hit = pred(&event);
                    self.log.lock().expect("event log").push(event);
                    if let Some(problem) = problem {
                        return Err(self.fail(format!("{problem} (waiting for {what})")));
                    }
                    if hit {
                        return Ok(());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.fail(format!("the event channel closed waiting for {what}")));
                }
            }
        }
    }

    /// Bounded poll for a settled pipeline state. Reaching it is a bias and not a
    /// correctness property (the transport is allowed to refuse), so the bound
    /// expiring proceeds rather than fails. It no longer proceeds SILENTLY:
    /// the bool says whether the state was actually reached, and the caller
    /// must not book unperformed work as coverage. A problem event is still a
    /// failure.
    fn await_state(&self, want: gst::State, bound: Duration) -> Result<bool, Failure> {
        let deadline = Instant::now() + bound;
        loop {
            self.check_events()?;
            let (current, pending) = self.playbin.state_summary();
            if current == want && pending == gst::State::VoidPending {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Bounded wait for the worker to answer a queued seek. Performed, refused
    /// and handed back all count as an ANSWER; only the bound expiring with no
    /// answer at all returns false (see [`Runner::await_state`]).
    fn await_seek(&self, bound: Duration) -> Result<bool, Failure> {
        let deadline = Instant::now() + bound;
        loop {
            let (answered, problem) = self.drain_matching(|event| {
                matches!(
                    event,
                    PlaybinEvent::AsyncDone | PlaybinEvent::SeekFailed | PlaybinEvent::QueueSeek(_)
                )
            });
            if let Some(problem) = problem {
                return Err(self.fail(problem));
            }
            if answered {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn load_and_play(&self, uri: &str) -> Checked {
        // Every wait below must be satisfied by THIS load's events, so the queue
        // is emptied first. Reported, never discarded: a problem sitting in the
        // queue belongs to the step that produced it.
        self.check_events()?;
        self.loads.set(self.loads.get() + 1);
        // Origins recorded against the previous item say nothing about this one.
        self.alignment.lock().expect("alignment tap").clear();
        self.alignment_cursor.set(0);
        *self.tapped.borrow_mut() = None;
        // `player.rs` `set_source`: the returned generation is what the
        // application scopes every following load-scoped event to.
        let generation = self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.expected_generation.set(Some(generation));
        self.wait_for("Loaded", |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        })?;
        self.paused.set(false);
        self.playbin
            .play()
            .map_err(|err| self.fail(format!("play() failed: {err}")))?;
        self.wait_for("settled PLAYING", |event| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        })?;
        // The overlay only joins once the item's video routes, so the tap goes
        // in here and not before the load.
        self.tap_overlay();
        Ok(())
    }

    /// stop() on a helper thread, so a wedge is a failure and not a hung suite.
    fn stop_bounded(&self, case: &Case) -> Checked {
        // `player.rs` `go_to_stopped_state` clears the expected generation as
        // part of the same call that issues the stop, so nothing is loaded and
        // no load-scoped event belongs to this player until the next load. The
        // events already sitting in the queue are dropped too, exactly as the
        // application drops them: it tests currency when it HANDLES an event,
        // which for anything queued behind the stop is after the clear.
        self.expected_generation.set(None);
        let playbin = self.playbin.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(playbin.stop().is_ok());
        });
        match rx.recv_timeout(TEARDOWN_BOUND) {
            Ok(true) => {
                self.paused.set(false);
                Ok(())
            }
            Ok(false) => Err(self.fail("stop() returned an error")),
            Err(_) => {
                // A parked push holds a streaming thread, so a wedged stop would
                // otherwise hang. Unpark everything before reporting.
                case.main.release_all();
                Err(self.fail("stop() never returned"))
            }
        }
    }

    /// Bias only. Reaching the anchor is not required, so a missed one is silent.
    fn reach(&self, anchor: Anchor, case: &Case) {
        match anchor {
            Anchor::GateHeld => {
                let gate = case.main.sync_point(GATE);
                if !gate.is_released() {
                    gate.wait_for_arrival(ANCHOR_BOUND);
                    return;
                }
                self.pump_for(ANCHOR_BOUND / 4);
            }
            Anchor::Buffers(count) => {
                let recording = self.audio_log().unwrap_or_else(|| self.video.clone());
                let target = recording.buffer_count() + count;
                let deadline = Instant::now() + ANCHOR_BOUND;
                while recording.buffer_count() < target && Instant::now() < deadline {
                    self.settle_pump();
                    recording.wait_for_buffers(target, Duration::from_millis(20));
                }
            }
            Anchor::Quiescent => {
                let owned = self.sweepable();
                let borrowed: Vec<(&str, &Recording)> = owned
                    .iter()
                    .map(|(name, recording)| (*name, recording))
                    .collect();
                if borrowed.is_empty() {
                    self.pump_for(ANCHOR_BOUND / 4);
                    return;
                }
                wait_quiescent(&borrowed, QUIESCENT_SETTLE, ANCHOR_BOUND);
            }
        }
    }

    fn pump_for(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The stream id an attached slot produced, once it materialized. Bounded and
    /// soft: an input still coming up is simply not selected this step.
    fn materialized_sid(&self, id: ExternalSubId) -> Option<String> {
        let deadline = Instant::now() + MATERIALIZE_BOUND;
        loop {
            if let Some(sid) = self.playbin.subtitle_stream_ids(id).into_iter().next() {
                return Some(sid);
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The receiver's mid-stream seek. A seek issued straight from PLAYING parks
    /// waiting for the caller's state machine to re-drive it, so the dance is the
    /// only shape that actually moves the timeline (see
    /// `switching_external_subtitles_realigns_the_new_input` in
    /// `tests/scenarios.rs`). Every intermediate wait is soft, the two transport
    /// calls are not.
    /// Returns whether every leg of the dance actually completed. A false here
    /// is not a failure (the transport may refuse), but it must not be booked
    /// as a performed seek: `seek_to N` used to count dances whose every wait
    /// had silently timed out, which made the run-level `seeks > 0` floor
    /// satisfiable without a single seek ever happening.
    fn seek_dance(&self, position: gst::ClockTime) -> Result<bool, Failure> {
        self.playbin
            .pause()
            .map_err(|err| self.fail(format!("pause() before the seek failed: {err}")))?;
        self.paused.set(true);
        let paused = self.await_state(gst::State::Paused, SEEK_STEP_BOUND)?;
        self.playbin.seek_async(Seek {
            position: Some(position),
            rate: None,
        });
        let answered = self.await_seek(SEEK_STEP_BOUND)?;
        self.playbin
            .play()
            .map_err(|err| self.fail(format!("play() after the seek failed: {err}")))?;
        self.paused.set(false);
        let playing = self.await_state(gst::State::Playing, SEEK_STEP_BOUND)?;
        Ok(paused && answered && playing)
    }

    /// Externals belong to the play item and die with the load's input reset, so
    /// every load starts from an empty pool.
    fn forget_externals(&self) {
        *self.attached.borrow_mut() = [None; EXTERNALS];
    }

    fn run_action(&self, action: Action, case: &Case) -> Checked {
        // Reads below (the collection, the last selection) come out of the log,
        // which has to be current first.
        self.check_events()?;
        match action {
            Action::Pause => {
                self.playbin
                    .pause()
                    .map_err(|err| self.fail(format!("pause() failed: {err}")))?;
                self.paused.set(true);
            }
            Action::Play => {
                self.playbin
                    .play()
                    .map_err(|err| self.fail(format!("play() failed: {err}")))?;
                self.paused.set(false);
            }
            Action::SeekZero => {
                // Refused while the pipeline has no media, which is not a failure.
                let _ = self.playbin.seek(gst::ClockTime::ZERO);
            }
            Action::SeekTo(position) => {
                // Counted AFTER the dance and only when it completed, see
                // [`Runner::seek_dance`].
                if self.seek_dance(position)? {
                    self.count(|coverage| coverage.seeks += 1);
                } else {
                    self.count(|coverage| coverage.seeks_refused += 1);
                }
            }
            Action::ReleaseGate => case.main.release(GATE),
            Action::AttachExternal(slot) => {
                let slot = slot as usize;
                // Read the slot out before the attach: holding the borrow across
                // it would run the whole call inside a live RefCell borrow.
                let free = self.attached.borrow()[slot].is_none();
                // A refusal is not a failure: the crate rejects an attach mid-dance
                // by design. Neither is attaching a slot that already is.
                if free && let Ok(id) = self.playbin.attach_subtitle(&case.externals[slot].uri()) {
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
                    self.settle_pump();
                    self.count(|coverage| coverage.selected += 1);
                }
            }
            Action::DetachExternal(slot) => {
                let id = self.attached.borrow_mut()[slot as usize].take();
                if let Some(id) = id {
                    let _ = self.playbin.detach_subtitle(id);
                    self.count(|coverage| coverage.detached += 1);
                }
            }
            Action::SelectEmbeddedText => {
                // No-op when the generated media has no text stream of its own.
                if let Some(sid) = self.item_sid(gst::StreamType::TEXT) {
                    self.playbin
                        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
                    self.settle_pump();
                    self.count(|coverage| coverage.embedded_text += 1);
                }
            }
            Action::DisableSubtitles => {
                self.playbin
                    .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
                self.settle_pump();
            }
            Action::DisableVideo => {
                self.playbin
                    .request_track(TrackSlot::Video, TrackTarget::Stream(None));
                self.settle_pump();
                self.count(|coverage| coverage.video_off += 1);
            }
            Action::SelectVideo => {
                if let Some(sid) = self.item_sid(gst::StreamType::VIDEO) {
                    self.playbin
                        .request_track(TrackSlot::Video, TrackTarget::Stream(Some(sid)));
                    self.settle_pump();
                    self.count(|coverage| coverage.video_on += 1);
                }
            }
            Action::StopReload => {
                self.stop_bounded(case)?;
                self.forget_externals();
                self.load_and_play(&case.main.uri())?;
            }
            Action::LoadSwap => {
                self.forget_externals();
                self.load_and_play(&case.replacement.uri())?;
            }
        }
        Ok(())
    }
}

/// Runs one case start to finish. Every action is followed by the invariant set.
/// A pass returns the case's [`Coverage`]; a failure is the failure.
fn run_case(case: &Case) -> Result<Coverage, Failure> {
    let runner = Runner::new();
    runner.load_and_play(&case.main.uri())?;
    runner.check_events()?;
    runner.check_worker_alive()?;

    for (index, (anchor, action)) in case.schedule.iter().enumerate() {
        runner.step.set(index);
        runner.reach(*anchor, case);
        runner.run_action(*action, case)?;
        runner.check_events()?;
        runner.check_worker_alive()?;
        // The sweep needs a quiescent log, which only a settled point gives.
        if matches!(
            action,
            Action::Pause | Action::StopReload | Action::LoadSwap
        ) {
            runner.check_quiescent()?;
        } else if matches!(
            action,
            Action::SelectExternal(_) | Action::SelectEmbeddedText | Action::SeekTo(_)
        ) {
            // The timeline invariant's richest moments: a fresh text selection,
            // and the seek that moves the origin the selection has to follow.
            // Safe outside a quiescent point because the check converges rather
            // than snapshots: only origins that stay apart for the whole bound
            // are reported, so a mid-flush divergence passes by construction.
            runner.check_timeline_alignment()?;
        }
    }

    runner.step.set(case.schedule.len());
    // The richest moment for the timeline invariant: every selection the
    // schedule made has landed and nothing is being torn down yet. The sweep
    // below runs after the stop, where the overlay is already gone.
    runner.check_timeline_alignment()?;
    // Teardown is the last invariant: a parked push must not outlive the pipeline.
    case.release_all();
    runner.stop_bounded(case)?;
    runner.check_events()?;
    runner.check_worker_alive()?;
    runner.check_quiescent()?;
    Ok(runner.coverage.get())
}

// ---------------------------------------------------------------------------
// Failure dumps and shrinking
// ---------------------------------------------------------------------------

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

/// Writes the case as scenario files plus the action list. Returns the directory,
/// or the reason it could not be written (never a failure of the run itself).
fn dump(case: &Case, failure: &Failure) -> Result<PathBuf, String> {
    let dir = failure_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    // The report is meant to be pasted, so resolve `../..` out of the path.
    let dir = dir.canonicalize().unwrap_or(dir);
    let stem = format!("seed{}-iter{}", case.seed, case.iteration);
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

/// Greedy drop-one shrink. Timing changes when an action goes, so a candidate that
/// stops failing proves nothing and is simply kept out of the result.
fn shrink(case: &Case, budget: Duration) -> Vec<(Anchor, Action)> {
    let deadline = Instant::now() + budget;
    let mut best = case.schedule.clone();
    let mut index = 0;
    while index < best.len() {
        if Instant::now() >= deadline {
            eprintln!("shrink: out of budget with {} actions left", best.len());
            break;
        }
        let mut candidate = best.clone();
        let dropped = candidate.remove(index);
        if run_case(&case.with_schedule(candidate.clone())).is_err() {
            eprintln!("shrink: dropped {} {}", dropped.0, dropped.1);
            best = candidate;
        } else {
            index += 1;
        }
    }
    best
}

/// The window rule the driver's timeline invariant rests on, tested directly so
/// it cannot rot behind the `#[ignore]`d driver (which is the only other thing
/// in this file, so without this a plain `cargo test` runs NOTHING here).
///
/// The case that matters is the third one: it is exactly what the old
/// `record[len - 2..]` check let through. Reading only the last two entries, a
/// branch that misaligned four hundred consecutive cues and then happened to
/// deliver two clean ones was indistinguishable from a branch that was never
/// misaligned at all.
#[test]
fn the_alignment_window_reads_more_than_its_tail() {
    let zero = gst::ClockTime::ZERO;
    let off = gst::ClockTime::from_seconds(1);
    let aligned = (zero, zero);
    let misaligned = (zero, off);

    assert_eq!(longest_misaligned_run(&[]), 0);
    assert_eq!(longest_misaligned_run(&[aligned; 5]), 0);

    // A short burst around a flush: forgiven.
    let burst = [aligned, misaligned, misaligned, aligned, aligned];
    assert_eq!(longest_misaligned_run(&burst), 2);
    assert!(
        longest_misaligned_run(&burst) <= FORGIVEN_STRAGGLERS,
        "a two-cue flush window must stay forgiven"
    );

    // The regression: a long misaligned run with a clean tail.
    let mut hidden = vec![misaligned; 400];
    hidden.push(aligned);
    hidden.push(aligned);
    assert_eq!(
        &hidden[hidden.len() - 2..],
        &[aligned, aligned],
        "the tail is clean, which is precisely why the old check passed this"
    );
    assert_eq!(longest_misaligned_run(&hidden), 400);
    assert!(
        longest_misaligned_run(&hidden) > FORGIVEN_STRAGGLERS,
        "400 consecutive misaligned cues must be reported however clean the tail is"
    );

    // Runs are consecutive, not cumulative: scattered singles stay forgiven.
    let scattered = [misaligned, aligned, misaligned, aligned, misaligned];
    assert_eq!(longest_misaligned_run(&scattered), 1);
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// The whole driver. Ignored by default: it runs real pipelines for as long as the
/// iteration count says.
#[test]
#[ignore = "stress driver, run with --ignored"]
fn fuzz_action_schedules() {
    init();
    let seed = env_u64("FCAST_FUZZ_SEED", 1);
    let iters = env_u64("FCAST_FUZZ_ITERS", DEFAULT_ITERS);
    let actions = env_u64("FCAST_FUZZ_ACTIONS", DEFAULT_ACTIONS) as usize;
    let shrinking = env_u64("FCAST_FUZZ_SHRINK", 0) != 0;
    eprintln!(
        "fuzz: seed={seed} iters={iters} actions={actions} shrink={shrinking} \
         (override with FCAST_FUZZ_SEED / FCAST_FUZZ_ITERS / FCAST_FUZZ_ACTIONS / FCAST_FUZZ_SHRINK)"
    );

    let mut total = Coverage::default();
    for iteration in 0..iters {
        let case = Case::generate(seed, iteration, actions);
        let started = Instant::now();
        let outcome = run_case(&case);
        match outcome {
            Ok(coverage) => {
                eprintln!(
                    "fuzz: iteration {iteration} passed {} actions in {:?} ({coverage})",
                    case.schedule.len(),
                    started.elapsed()
                );
                total.add(coverage);
                case.unregister();
            }
            Err(failure) => {
                let media = case
                    .handles()
                    .iter()
                    .map(|(name, handle)| format!("{} ({name})", handle.key()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut report = format!(
                    "fuzz FAILED\n  seed: {seed}\n  iteration: {iteration}\n  \
                     step: {} of {}\n  what: {}\n  media: {media}\n  schedule:\n{}\n",
                    failure.step,
                    case.schedule.len(),
                    failure.message,
                    case.trace()
                );
                match dump(&case, &failure) {
                    Ok(dir) => report.push_str(&format!(
                        "  written to: {}\n  replay: FCAST_FUZZ_SEED={seed} \
                         FCAST_FUZZ_ITERS={} FCAST_FUZZ_ACTIONS={actions} cargo test \
                         -p fcastplaybin --test fuzz_scenarios -- --ignored --nocapture\n",
                        dir.display(),
                        iteration + 1
                    )),
                    Err(err) => report.push_str(&format!("  (could not write the dump: {err})\n")),
                }
                if shrinking {
                    let shrunk = shrink(&case, Duration::from_secs(120));
                    report.push_str(&format!(
                        "  shrunk to {} actions:\n{}\n",
                        shrunk.len(),
                        case.with_schedule(shrunk).trace()
                    ));
                }
                case.unregister();
                panic!("{report}");
            }
        }
    }

    // A run where every action no-opped is green and proves nothing: the
    // schedule is a random walk, the pool starts empty, and a selection of a
    // slot nobody attached does nothing at all. [`Coverage`] already says so
    // in the pass line, but nothing read it, so a driver that quietly stopped
    // reaching its own permutations stayed indistinguishable from one that
    // covered them. Now it is a failure.
    //
    // Only checked at (at least) the documented default budget: a replay of a
    // shrunk case is deliberately tiny and legitimately covers almost nothing.
    let full_budget = iters >= DEFAULT_ITERS && actions >= DEFAULT_ACTIONS as usize;
    eprintln!("fuzz: run total ({total}){}", {
        let missed = total.missed();
        if missed.is_empty() {
            String::new()
        } else {
            format!("; NEVER REACHED: {}", missed.join(", "))
        }
    });
    if full_budget {
        // The four the module header calls the point of the driver: a
        // populated pool, a selection onto it, the mid-stream seek that moves
        // the timeline origin, and at least one sweep where the timeline
        // invariant had a real pair of origins to compare.
        for (name, count) in [
            ("attach", total.attached),
            ("select", total.selected),
            ("seek_to", total.seeks),
            ("aligned", total.aligned),
        ] {
            assert!(
                count > 0,
                "the whole run never once reached `{name}`, so every green iteration \
                 proved nothing about it: {total}. Either the draw table or the \
                 runner's precondition for that move regressed."
            );
        }
    }
}
