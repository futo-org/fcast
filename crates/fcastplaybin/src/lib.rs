//! fcastplaybin: a receiver-owned replacement for playbin3/playsink.
//!
//! Topology (playsink's, minus its hidden reconfiguration state machine):
//!
//! ```text
//! input (urisourcebin | element)   -> decodebin3 -> streamsynchronizer -> chains
//! external subtitle (urisourcebin) -> decodebin3 (request pads, live attach/detach)
//!
//! video chain: ssync -> video sink
//! text  path : decodebin3 -> queue -> appsink -> subtitle consumer (policy-gated)
//! audio chain: ssync -> queue -> audioconvert -> audioresample
//!              -> fcastaudiostretch -> volume -> audio sink
//! ```
//!
//! Subtitles do not go through a compositor here. A selected text stream ends
//! in a per-stream `appsink` whose samples become cues on
//! [`FcastPlaybin::set_subtitle_consumer`], already resolved to running time,
//! and the caller's video sink draws them (subtitleoverlay and its one shared
//! subtitle seat are gone).
//!
//! The mechanism layer (urisourcebin/decodebin3/streamsynchronizer and the
//! decoders) stays stock. This crate owns policy:
//! when chains link, when text may join, how inputs attach and detach, and
//! how errors are attributed (every input carries a generation tag).
//!
//! The crate also owns the bus ([`FcastPlaybin::set_event_handler`] delivers
//! typed [`PlaybinEvent`]s) and a worker thread for the blocking operations
//! (the `_async` methods), so callers never touch raw GStreamer state
//! changes, seeks or bus messages.

use std::{
    collections::VecDeque,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

pub mod graph;
pub mod state_machine;

// Consumed only through the crate-root re-export below, never by module path.
mod selection;

mod hands;

use hands::Hands;

pub use selection::{SelectionGate, TrackSelection, TrackSlot, TrackTarget};
pub use state_machine::{
    BufferingStateResult, PlaybackState, RunningState, Seek, StateChangeResult, StateMachine,
};

mod api;
mod buffering;
mod bus;
mod decisions;
mod dispatch;
mod external;
mod flush;
mod gapless;
mod jobs;
mod pipeline;
mod routing;
mod stats;
mod teardown;
mod text;
mod text_disposal;
mod text_policy;

#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod tests;

pub use api::{
    AfterCancel, AudioSink, BitmapSubFormat, CueIr, ErrorOrigin, ExternalSubId, MediaInput,
    MessageHook, PlaybinEvent, Sinks, SourceDbg, StartOutcome, StartPoint, StreamIoStats,
    SubtitleFeedItem, SubtitleTextFormat, bitmap_format_implemented,
};

pub use buffering::{BufferedRange, BufferingInfo};
#[doc(hidden)]
pub use stats::{GlobalStats, Stats};

use crate::{
    api::{EventCallback, SubtitleConsumer},
    buffering::LevelProbes,
    dispatch::{REFRESH_DEADLINE, SELECT_DEFER_BUDGET, SELECTION_DEADLINE},
    external::EXTERNAL_SUB_TIMEOUT,
    gapless::{HeldActivation, PreparedNext, SwapGate},
    jobs::{QueuedJob, TimerEntry},
    routing::{Input, RoutingState},
    text_disposal::TextDisposal,
    text_policy::{DegradationMemo, TextDegradation},
};

/// The per-load dynamic core: decodebin3 + streamsynchronizer. Rebuilt FRESH
/// for every load. These are the only elements that accumulate per-media
/// state across items (decodebin3's multiqueue keeps its interleave-tuned
/// slot sizing, collections and selection bookkeeping), and that
/// accumulation wedges later prerolls: after a run of audio-only items, a
/// reused multiqueue's audio slot filled and blocked the demuxer before the
/// first video buffer, holding an A/V preroll below PAUSED forever. A fresh
/// pair per load makes every load independent of instance history.
struct Core {
    db3: gst::Element,
    ssync: gst::Element,
    pad_added_sig: gst::glib::SignalHandlerId,
    pad_removed_sig: gst::glib::SignalHandlerId,
}

struct Inner {
    pipeline: gst::Pipeline,
    /// See [`Core`]. `None` only during construction.
    core: Mutex<Option<Core>>,
    /// The preroll token: a permanent `appsrc ! fakesink(sync=false)` branch
    /// whose only job is keeping every load's READY->PAUSED honestly ASYNC.
    /// At load time NO output chain is in the pipeline yet (both join at
    /// route time), so without the token the transition completes instantly:
    /// running time starts before any media exists, chains then join a
    /// committed pipeline late against a stale base_time (the QoS drop-storm
    /// class), and the caller's state machine commits off bogus settles.
    /// The token fakesink returns ASYNC like any dataless sink. Once the
    /// first real chain joins, `finish_preroll_token` feeds the appsrc one
    /// buffer + EOS, prerolling the token out of the equation (the EOSed
    /// sink also satisfies the bin's EOS aggregation, so the token never
    /// blocks the real end-of-stream). READY resets both ends for the next
    /// load. Forged ASYNC_START messages do NOT work instead: gstbin
    /// ignores them while its target is at or below READY.
    token_src: gst::Element,
    /// Held across DOWNWARD pipeline transitions (stop, the load reset).
    /// `route_db3_pad` try-locks it and refuses pads while a teardown is in
    /// flight. A polling state-query gate alone is TOCTOU-racy: a pad
    /// exposed microseconds before a Stop's READY descent routed anyway and
    /// its chain activation deadlocked against the descending set_state.
    /// Always held through [`RouteGate`], whose release re-attempts
    /// `deferred_pads`.
    route_gate: Mutex<()>,
    /// The chain-join half of `route_gate`, held by `fpb-join` across a chain
    /// activation and by every downward transition (which takes BOTH, in that
    /// order). DELIBERATELY NOT the same lock as `route_gate`: a join can
    /// block for as long as a sink's transition takes, and a route that finds
    /// `route_gate` taken DEFERS rather than waits, so joining under the route
    /// gate made every multi-stream load defer its second stream. A deferred
    /// pad is unlinked while it waits, so decodebin3 pushes its first buffer
    /// into nothing and multiqueue drops it (measured: the audio sink's first
    /// buffer became pts=20ms with no DISCONT in 5 of 6 `toml_scenarios`
    /// runs). Two locks make joins exclusive with descents, which is what they
    /// need, without ever making a route wait.
    ///
    /// Lock order where both are taken: `route_gate` then `join_gate`, and
    /// [`Inner::gate`] is the only place that does it.
    join_gate: Mutex<()>,
    /// Text branches unlinked but not yet torn down, because the pipeline was
    /// at rest in PAUSED when they were detached. Drained by
    /// [`Inner::run_deferred_text_work`].
    deferred_text_disposal: Mutex<Vec<TextDisposal>>,
    /// Inputs a user DETACH took out of the routing state but could not tear
    /// down yet, because the pipeline was at rest in PAUSED. Drained by
    /// [`Inner::run_deferred_text_work`]. Teardown paths never use this, see
    /// [`Inner::remove_input_or_defer`].
    deferred_input_removal: Mutex<Vec<Input>>,
    /// External ids with a replay in flight, for the misaligned-cue gate in
    /// the text consumer's appsink callback and for NOTHING else.
    ///
    /// # The second home, and why the bit has two
    ///
    /// The in-flight bit itself lives on the resource
    /// ([`crate::external::ExternalInput::replay_inflight`]), where it cannot
    /// orphan. Every decider-side reader asks about `(id, epoch)` and already
    /// holds, or can take, the routing lock.
    ///
    /// The cue gate cannot. It runs per cue on the appsink STREAMING thread
    /// (`Inner::build_text_consumer`), where taking `routing` would put the
    /// crate's busiest lock on a path that has no business waiting for a
    /// selection or a teardown. It also asks a strictly weaker question: "is a
    /// replay in flight for THIS id", any epoch, because a cue arriving at the
    /// appsink names an input, not an incarnation.
    ///
    /// So this is a PROJECTION of the resource bits onto the id, never an
    /// independent record: [`Inner::sync_cue_gate`] recomputes it from the
    /// inputs under the routing lock at every site that writes a resource bit
    /// (and at the removal that takes a resource away). A forgotten sync
    /// therefore costs at most a stale drop of misaligned cues for one id,
    /// which the next replay's sync corrects; it can never suppress the
    /// reconcile pass, which is the failure this whole bit exists to make
    /// impossible.
    ///
    /// A LEAF lock: taken alone on the streaming thread, and innermost
    /// (under `routing`) everywhere else. Nothing is ever taken while it is
    /// held.
    replaying_externals: Mutex<std::collections::HashSet<ExternalSubId>>,
    /// Cues actually handed to the subtitle consumer, per external input.
    /// The delivery evidence `verify_replay` requires beside segment
    /// alignment: alignment cannot prove a buffer survived the trip (the
    /// multiqueue destroys in-flight items across a flush, and a buffer
    /// arriving ahead of its segment dies silently in `item_from_sample`),
    /// so a verify that concludes on alignment alone declares a silent
    /// branch converged and nothing ever redelivers. Bumped where an
    /// external's cue is fed (the appsink callback and the park replay),
    /// compared against `ExternalInput::fed_baseline`, cleared in
    /// `remove_input`.
    ///
    /// STAYS on `Inner` while the replay bits moved onto the resource, for the
    /// reason the cue gate above has a second home: the appsink callback bumps
    /// this per cue on a STREAMING thread, and folding it into `ExternalInput`
    /// would put the routing lock on that path. The baseline READ hoisted out
    /// of the routing lock instead (see `FcastPlaybin::replay_subtitle`), which
    /// removed the crate's only `routing` -> `external_cues_fed` nesting.
    external_cues_fed: Mutex<std::collections::HashMap<ExternalSubId, u64>>,
    /// Bounded timers waiting for their moment, drained by
    /// [`Inner::run_tick`] (see [`TimerEntry`]). Cleared at the load reset and
    /// at teardown like every other deferral slot; both jobs it holds are
    /// epoch-guarded anyway, so that is hygiene rather than correctness.
    pending_timers: Mutex<Vec<TimerEntry>>,
    /// Whether the LAST [`Job::DrainTextWork`] the worker ran was a no-op
    /// (postponed work pending, pipeline below a settled PLAYING). While
    /// set, [`Inner::poll_text_policy`] suppresses its per-poll re-poke,
    /// because a drain already ran against this exact situation and decided
    /// it cannot proceed, so re-running it on every 5ms poll is a busy loop
    /// (measured at one worker job per poll for the whole time a pipeline
    /// sat parked in Buffering). Cleared by every event that could change
    /// that verdict, which keeps the poke live exactly when it can matter.
    /// The recording of any new postponed item clears it, and the drain
    /// itself clears it when it proceeds. Pipeline state edges do not need
    /// to clear it because the bus translation queues the drain on every
    /// edge unconditionally, and that queued run refreshes the verdict.
    drain_poke_parked: AtomicBool,
    /// Whether the LAST selection this crate dispatched to decodebin3 turned
    /// video off entirely (a video-bearing collection with no video id in the
    /// selection). Written on the select-sender thread BEFORE the send, so a
    /// pad decodebin3 exposes inside the send already reads the intent that
    /// selection carries. Read by `route_db3_pad`, whose Video arm must not
    /// rebuild the video chain for a stream decodebin3's collection-default
    /// auto-select resurrected over an explicit deselect. The selection
    /// engine owns the real desire and self-corrects the divergence. This
    /// mirror exists only because the route decision runs on a streaming
    /// thread at pad-exposure time, where the engine's answer arrives too
    /// late (see the Video arm). Reset per load and at teardown, carried
    /// across gapless exactly like the engine's carried video-off desire.
    ///
    /// Deliberately an atomic rather than an accessor on the selection
    /// engine, and both halves of that are load-bearing. Reading the engine
    /// would take a mutex on a streaming thread, which is the lock-ordering
    /// hazard that wedged the pipeline against pad stream locks before the
    /// routing lock learned to collect and then act. It would also answer
    /// the wrong question, reporting the CURRENT desire when a pad exposed
    /// inline inside `send_event` needs the intent of the selection being
    /// dispatched right then.
    video_deselected: AtomicBool,
    /// A VIDEO stream was unrouted at least once since the load. The
    /// drained-resurrect park in `route_db3_pad` only applies to a
    /// RE-route. At the initial route of a fast-paced item the input can
    /// already be at EOS while decodebin3's multiqueue still holds the
    /// whole stream, and parking that first route would silence video for
    /// the item. Reset at every item boundary ([`Inner::reset_item_state`]).
    video_unrouted_once: AtomicBool,
    /// A teardown detached a rescue descent that never returned (see
    /// [`WakeRescue::disarm`]), so the pipeline is coming down on a thread this
    /// crate no longer waits for and no longer owns.
    ///
    /// ONE-WAY. Nothing clears it, because nothing can: the detached thread
    /// holds the route gate and is inside the pipeline's state change, so every
    /// pipeline call from here would queue behind it, unbounded, on the worker
    /// (the exact wedge the bound exists to end). The crate is dead and says
    /// so - [`FcastPlaybin::run_job`] refuses every job from here on, settling
    /// what each one owes its caller so nobody is left waiting.
    ///
    /// Not a `load` reset: a load is precisely one of the things that cannot
    /// happen any more.
    teardown_poisoned: AtomicBool,
    /// decodebin3 source pads from the CURRENT core that `route_db3_pad`
    /// refused because `route_gate` was momentarily held by a concurrent
    /// downward transition. Dropping them for good stalled the active load
    /// (audio routed but video lost -> never prerolls, the load-stall race).
    /// Every [`RouteGate`] release re-attempts them, and the routing guards
    /// re-reject any that are genuinely stale.
    deferred_pads: Mutex<Vec<gst::Pad>>,
    /// The generation of the CURRENT load: stamped on every emitted event
    /// and on every attached input. Callers compare against the value
    /// returned by [`FcastPlaybin::load_async`] to drop events from
    /// superseded loads exactly, and inputs whose generation is behind it
    /// classify as [`ErrorOrigin::Stale`].
    generation: AtomicU64,
    /// Allocator for `generation`: bumped when a load is REQUESTED (so the
    /// caller knows the tag up front), adopted by the load at its reset.
    next_generation: AtomicU64,
    /// Head of the audio chain (the decoupling queue's sink pad).
    audio_entry: gst::Element,
    volume: gst::Element,
    /// Head of the video chain: a small queue (`fpb-vqueue`, playsink's
    /// video-chain queue parity) in front of the caller's sink. Two jobs.
    /// LATENCY: a non-leaky queue with no time cap answers the latency query
    /// with max=unlimited, so the video branch never caps the pipeline's max
    /// latency below a live audio sink's min (field: live SABR, audio min
    /// 235ms vs video max 33ms, "Impossible to configure latency", the video
    /// sink then runs with zero processing latency and QoS-drops most frames).
    /// DECOUPLING: it absorbs the pushes a deselect parks, the same job
    /// `fpb-aqueue` does for audio (see `lift_deselected_video_sink`).
    /// Joins and leaves the pipeline in lockstep with `video_sink`; the
    /// internal `vqueue ! sink` edge is made on the first attach and kept
    /// across membership changes.
    video_entry: gst::Element,
    /// The video output chain's sink: the caller's video sink, behind
    /// [`Inner::video_entry`]'s queue (subtitleoverlay sat in front of it
    /// until its deletion; cues leave through [`Inner::subtitle_consumer`]
    /// now). It lives in
    /// the pipeline ONLY while the item has a routed video stream
    /// (`ensure_video_chain`/`remove_video_chain`), exactly like the
    /// per-load audio sink: an absent chain cannot hang a video-less
    /// preroll and never counts in the bin's EOS/STREAM_START aggregation,
    /// by construction. The preroll token (see `token_src`) keeps a load
    /// ASYNC while no chain has joined yet. The sink is caller-owned
    /// and GL/window-bound, so it parks at READY when out of the pipeline
    /// and is never NULLed (playbin3's own treatment of it).
    ///
    /// It is also the crate's VIDEO TIMELINE anchor: its sink pad carries the
    /// sticky SEGMENT [`Inner::video_timeline`] and
    /// [`Inner::sync_text_running_time`] read, which is the same event the
    /// overlay's `video_sink` carried before, one element further down the
    /// same branch.
    video_sink: gst::Element,
    /// How the audio sink is built: once per load, when audio routes, and
    /// the previous sink is dropped at the load reset. Reusing one sink
    /// across a session degrades: pulsesink holds its `pa_context` open at
    /// READY and a context carried across dozens of loads eventually returns
    /// "Disconnected: Bad state" on the READY->PAUSED that starts a load.
    /// A fresh element per load gives a fresh context, playsink's own
    /// behavior.
    audio: AudioSink,
    /// The audio sink built for the current load, linked `volume ! sink`.
    /// `None` between the load reset and the first audio route, or for a
    /// video-only item.
    audio_sink: Mutex<Option<gst::Element>>,
    /// The caller's event handler (see [`FcastPlaybin::set_event_handler`]).
    /// Events are silently dropped until one is installed.
    events: Mutex<Option<EventCallback>>,
    /// The caller's subtitle consumer (see
    /// [`FcastPlaybin::set_subtitle_consumer`]). Cues are silently dropped
    /// until one is installed. THE crate's only subtitle output: a caller that
    /// installs none renders no subtitles at all.
    subtitle_consumer: Mutex<Option<SubtitleConsumer>>,
    /// Every text degradation this load has noticed, keyed by (kind, stream id
    /// or pad name, load generation), valued by where that key stands in the
    /// grace/dedupe machine.
    ///
    /// ONE table for four shapes that were four, because all four key the same
    /// way, live exactly as long as the load, and want the same answer: say it
    /// once, and only once the shape has outlasted the grace its kind allows.
    /// Which shapes and why each needs remembering is on [`TextDegradation`];
    /// the machine itself is [`Inner::note_degradation`], the only writer.
    ///
    /// The generation in the key is what lets the SAME stream report again
    /// after a new load; the item-boundary reset clears the table anyway
    /// ([`Inner::reset_item_state`]), because a map that only ever grows is a
    /// slow leak in a receiver that plays for days - and a stop that idles is
    /// exactly the case a per-load clear alone never reaches.
    text_degradations:
        Mutex<std::collections::HashMap<(TextDegradation, String, u64), DegradationMemo>>,
    /// What the TEXT PARK consumed, keyed by the decodebin3 pad it is parked
    /// on, newest last, bounded by [`PARKED_TEXT_CUES`].
    ///
    /// # Why the park has to remember
    ///
    /// A routed text pad is linked to a parking sink the instant it appears,
    /// because an adaptive demuxer serves every track from ONE output loop and
    /// a text pad nobody consumes pins that loop for the whole element (see
    /// `Inner::park_stream`). The consumer branch is joined later, by
    /// `Inner::poll_text_policy`, and NOT before the pipeline settles at
    /// PAUSED, where `decisions::text_may_link` is the gate. So there is always
    /// a window, the whole of bring-up, in which the demuxer's cues cross
    /// into a sink whose only job is to throw them away.
    ///
    /// On a SEGMENTED text track that costs a cue or two that arrive again.
    /// On a whole-period Representation (one `<BaseURL>`, the entire track
    /// pushed once) the demuxer's output position races
    /// far ahead of the playhead while the sinks preroll, so a window worth
    /// milliseconds of wall clock swallows SECONDS of media, and a
    /// whole-period track has no second copy of them. Measured on
    /// `manifest-text.mpd` before this: the pad carried cues at pts 0, 1 and 2
    /// s within 8 ms of each other, all three into the park, and the first cue
    /// the viewer ever saw was the one for second 3. That is the field's
    /// "subtitles start a few seconds in, not from the beginning", entire.
    ///
    /// So the park CONSUMES (the demuxer must never be pinned) and KEEPS, and
    /// the join replays what is still worth showing.
    parked_text_cues: Mutex<std::collections::HashMap<String, VecDeque<(gst::Sample, Instant)>>>,
    /// decodebin3 text pads whose branch may skip exactly ONE `Clear`.
    ///
    /// Armed by a replay that delivered something, consumed by the first
    /// `Clear` after it. The join's own sticky STREAM_START would otherwise
    /// wipe the opening cues the replay had just restored, which is the
    /// difference, measured, between the first covered frame landing at 0.2 s
    /// and at 4.085 s.
    suppress_text_clear: Mutex<std::collections::HashSet<String>>,
    /// TEST FAULT INJECTION, absent until a test stages something. See
    /// [`TestStaging`], which is where the whole family lives and where the
    /// "per instance, not an env lever" argument is written down once.
    ///
    /// Production never initializes it, so every read site on the hot and
    /// streaming paths costs one null check rather than an atomic load.
    staging: OnceLock<Box<TestStaging>>,
    /// Feeds the worker thread (see [`Job`]). The worker owns the receiver
    /// and exits when this sender is dropped with `Inner`.
    work_tx: mpsc::Sender<QueuedJob>,
    /// Monotonic supersession counter for the WORK QUEUE: stamped on every
    /// job at enqueue ([`Inner::queue_job`]) and compared once when the
    /// worker picks the job up ([`stale_policy`]).
    ///
    /// Bumped by exactly the five entry points whose effect replaces the item
    /// a queued job was formed for ([`Inner::supersede_queued_work`]), and by
    /// nothing else. In particular the gapless activation NEVER bumps it: a
    /// prepared swap keeps the pipeline and everything queued against it
    /// alive, so a pause or a clock recovery straddling that boundary is
    /// still wanted, and the seam's own dual-generation window
    /// (`activate_prepared_now` adopts the prepared generation before the
    /// activation has finished) must not leak into a staleness predicate.
    ///
    /// Deliberately a counter of its own rather than `generation` or
    /// `next_generation`. `next_generation` moves on a PREPARE, and a prepare
    /// must not invalidate a queued load or pause; `generation` is the seam's
    /// and belongs to event attribution.
    ///
    /// Stamps and bumps are individually atomic and nothing more: a job
    /// enqueued concurrently with a bump lands on one side or the other,
    /// matching some serialization of the two calls. Where that differs from
    /// v1 is two loads racing from DIFFERENT threads: v1 finished on the
    /// last-ENQUEUED one, this finishes on the highest-EPOCH one and drops
    /// the other, so the losing caller never sees a `Loaded` for the
    /// generation it was handed. Single-caller assumption, and the receiver
    /// satisfies it: it drives the crate from one event loop, so its calls
    /// are serialized and the two orders coincide.
    queue_epoch: AtomicU64,
    /// A monotonic ticket handed out to text pads as buffers cross them, so
    /// two pads carrying the SAME stream id can be ordered by which one
    /// decodebin3 fed most recently ([`RoutedStream::last_buffer`]).
    ///
    /// A counter rather than a clock on purpose: the comparison this exists
    /// for is "which of these two pads is the live one", which is an ORDER,
    /// and an order needs no wall time, no monotonic clock and no tolerance
    /// for a pad that has been idle a long while because its stream is
    /// sparse. Zero means "never carried a buffer".
    text_flow_ticket: AtomicU64,
    /// Whether a [`Job::PollTextPolicy`] is already queued and unrun. The
    /// coalescing bit of [`Inner::request_text_policy_poll`]: a receiver
    /// polling every 5ms must not put 200 identical jobs a second on the
    /// worker, and it need not - the policy re-reads the whole world when it
    /// runs, so N queued polls and one queued poll decide the same thing.
    /// Cleared by the JOB, before it runs, so a poke that lands mid-run
    /// queues a fresh one instead of being folded into a decision that has
    /// already been taken.
    poll_queued: AtomicBool,
    /// The diagnostic census for this pipeline (see [`Counters`]).
    counters: Counters,
    /// THE deciding thread, recorded by [`Inner::worker_loop`] before it takes
    /// its first job.
    ///
    /// The claim is an ownership claim: the text branch's link, reclaim,
    /// evict, park and postponed-disposal decisions all happen on ONE thread,
    /// which is what retires the TOCTOU class the `TextSeat` was mitigating.
    /// A claim like that is worth exactly as much as its enforcement, so this
    /// is the enforcement - see [`Inner::decider_only`], which every one of
    /// those sites calls.
    ///
    /// `OnceLock` rather than a constructor argument because the worker is
    /// spawned with a `Weak` and only names itself once it runs; a site that
    /// asks before then gets `None` and asserts nothing, which is correct
    /// (nothing has been decided yet either).
    decider: OnceLock<ThreadId>,
    /// The three effect lanes and their in-flight table (see the [`hands`]
    /// module). Owns the senders that used to be three separate fields, with
    /// the same lifetime discipline as `work_tx`: dropping them with `Inner`
    /// is what ends the lane threads.
    hands: Hands,
    /// The `fpb-tick` hangup channel (see [`Inner::run_tick`]). Nothing is
    /// ever SENT on it and nothing reads it: dropping it with `Inner` is what
    /// ends the thread, exactly like the other senders here.
    #[allow(dead_code, reason = "held only so that dropping it hangs up fpb-tick")]
    tick_tx: mpsc::Sender<()>,
    /// How many ticks have run, for the rate-limited liveness re-poke (see
    /// [`DRAIN_REPOKE_TICKS`]). On `Inner` rather than thread-local because
    /// the counter belongs to the instance the ticks are for, not to the
    /// thread that happens to run them, and because that keeps it readable
    /// from anywhere `Inner` is.
    tick_count: AtomicU64,
    routing: Mutex<RoutingState>,
    /// The declarative track-selection engine (see the [`selection`] module
    /// docs). Recording happens at bus-translate time, dispatch only in
    /// [`FcastPlaybin::pump_selection`]. Lock order: `routing` before
    /// `selection`, never the reverse.
    selection: Mutex<selection::SelectionEngine>,
    /// The subtitle sid of the last APPLIED selection (StreamsSelected as
    /// reported, never the engine's optimistic in-flight state). Only the
    /// selection-time external replay reads it: the engine's `applied` is
    /// already the new target by the time the confirmation arrives, so it
    /// cannot say whether the slot MOVED. Reset wherever the engine resets.
    last_applied_subtitle: Mutex<Option<String>>,
    /// Whether the MAIN input answers the SELECTABLE query (an adaptive
    /// demuxer): decodebin3 then defers ALL selection upstream and never
    /// posts STREAMS_SELECTED (`is_selection_done` returns early). `None`
    /// until first asked, reset wherever the engine resets. See
    /// [`Inner::upstream_owns_selection`].
    upstream_selection: Mutex<Option<bool>>,
    /// The upstream-owned id set last SENT while upstream owns selection: an
    /// adaptive demuxer only confirms an activation EDGE, so a no-op re-send
    /// would never confirm and must be settled locally instead.
    ///
    /// Written from two facts and never from an intention: an observed report
    /// (the `StreamsSelected` arm) and a send the lane carried out
    /// (`Outcome::SelectSent`). A dispatch only COMPARES against it. Sorted,
    /// always: the comparison is set-shaped.
    last_upstream_ids: Mutex<Vec<String>>,
    /// The timeline the current item is MEANT to render against: rate and
    /// the position whose running time is zero, recorded when a load's
    /// start seek or a user seek is issued. `video_timeline` falls back to
    /// it while the video sink has no sticky segment yet, so an external
    /// replay that runs inside that window still lands on the right
    /// timeline instead of zero.
    intended_timeline: Mutex<(f64, gst::ClockTime)>,
    /// Serializes decodebin3 sink-pad requests. Concurrent
    /// `request_pad_simple("sink_%u")` calls (an input's pad-added streaming
    /// threads racing an inline `attach_subtitle`) can both draw the same
    /// name inside decodebin3; the second add fails ("Padname sink_0 is not
    /// unique") and the broken pad object panics the requesting thread in
    /// the bindings, which killed streaming threads mid-lock in the field.
    db3_pad_request: Mutex<()>,
    /// The four tunable waits (see [`Deadlines`]).
    deadlines: Mutex<Deadlines>,
    /// The pre-armed next item, if any (see [`PreparedNext`]). Lock order:
    /// take and RELEASE this before `routing`/`selection`, never hold it
    /// across them.
    prepared: Mutex<Option<PreparedNext>>,
    /// See [`SwapGate`].
    swap_gate: SwapGate,
    /// The group id of the item currently flowing OUT of decodebin3 (from
    /// STREAM_START on its output pads; reset at every item boundary,
    /// [`Inner::reset_item_state`]). A change while a
    /// prepared item is pending IS the gapless activation: decodebin3
    /// posts no new streams-selected for a same-slot continuation, so the
    /// data plane's group id is the reliable switch signal (uridecodebin3
    /// tracks output activation the same way).
    active_group: Mutex<Option<gst::GroupId>>,
    /// The group id the last gapless activation RETIRED (the previous
    /// item's). Output pads still carrying it have their EOS dropped even
    /// after the activation cleared the swap gate: the selection-side
    /// activation trigger can fire while the old item's tail is still
    /// draining out of decodebin3, and an old EOS reaching the sinks there
    /// can end the pipeline between items. Reset at every item boundary
    /// ([`Inner::reset_item_state`]).
    retired_group: Mutex<Option<gst::GroupId>>,
    /// The group whose EOS the output gate committed to LETTING THROUGH
    /// into streamsynchronizer. A short item's fastest stream (audio
    /// decodes a whole 2s clip in milliseconds) can push its EOS past the
    /// output gate BEFORE a pre-arm arms it. streamsynchronizer then parks
    /// that stream's pushing thread (the multiqueue slot task!) until the
    /// whole group is EOS, and the parked task can never deliver the next
    /// item's stream-start queued behind it. Dropping the group's REMAINING
    /// EOS at the output gate would leave the group forever incomplete and
    /// wedge the switch, so the gate is all-or-nothing per group: once one
    /// EOS of a group passed, its siblings pass too, streamsynchronizer
    /// completes the group and re-emits EOS on its src pads, where the
    /// post-ssync gate consumes them before they reach the sinks. Reset at
    /// every item boundary ([`Inner::reset_item_state`]), where no group can
    /// be mid-pass: the swap gate is disarmed and the inputs are gone, so the
    /// all-or-nothing rule has nothing left to be all-or-nothing about.
    /// Lock order: taken OUTSIDE `active_group`, `retired_group` and
    /// `swap_gate.state`, which [`Inner::gapless_eos_gate`] holds under it
    /// to decide and commit atomically. Never take it while holding any of
    /// those three.
    passing_eos_group: Mutex<Option<gst::GroupId>>,
    /// Stream ids whose INPUT-side stream has delivered EOS into decodebin3
    /// and has not been restarted by a flush since (recorded by a probe on
    /// every input pad linked into decodebin3, see `Inner::link_input_pad`).
    /// A deselected stream's end never reaches the output probes (its slot
    /// is gone), so `passing_eos_group` cannot see it, and re-routing such
    /// a stream builds a chain that can never preroll. The
    /// drained-resurrect park in `route_db3_pad` consults this next to the
    /// group mirror. Reset at every item boundary
    /// ([`Inner::reset_item_state`]), and only there: the EOS/FLUSH_STOP pair
    /// on the input probe maintains it in between, which is NOT the
    /// proof-of-life rule its output-side twin uses.
    input_eos_sids: Mutex<std::collections::HashSet<String>>,
    /// A gapless activation's user-facing events (PreparedActivated + the new
    /// item's collection), held back from decodebin3's output until the new
    /// item's audio crosses the decoupling queue to the sink. The switch is
    /// detected at decodebin3's output, one decoupling-queue ahead of the
    /// speakers, so emitting the title/duration there flips the UI before the
    /// sound. The `fpb-aqueue` src STREAM_START probe releases this when the
    /// item's audio actually reaches the sink, matching the sink-anchored
    /// playback position. Only set for items with audio (the release is
    /// anchored on the audio queue); audio-less items emit immediately. At
    /// most one is ever held: real media never runs two swaps within one
    /// queue depth, and a superseding activation flushes any prior hold.
    /// Cleared at every item boundary ([`Inner::reset_item_state`]).
    held_activation: Mutex<Option<HeldActivation>>,
    /// The DROP probe a mid-item video deselect leaves on the pad feeding the
    /// video chain (see `park_video_chain_for_deselect`). Removed when the
    /// chain rejoins or leaves the pipeline, so a re-selected video stream is
    /// never dropped. The pad is kept alongside the id because the probe has
    /// to be removed from the same pad it was added to, and by then the
    /// video sink may already be unlinked from it.
    video_park_probe: Mutex<Option<(gst::Pad, gst::PadProbeId)>>,
    /// Serialises the video chain's MEMBERSHIP change (see
    /// [`Inner::attach_video_chain`]). A leaf lock: nothing is taken under it.
    video_chain_membership: Mutex<()>,
    /// Where [`FcastPlaybin::buffered_ahead`] reads its levels (see
    /// [`LevelProbes`]). A LEAF lock, taken only by the polling caller.
    level_probes: Mutex<LevelProbes>,
    /// Whether [`Inner::level_probes`] has to be re-walked before its next
    /// read: the graph changed, or a load started one.
    ///
    /// An atomic and not part of the list itself because the writers are the
    /// pipeline's deep element-added/removed handlers, which run on whatever
    /// thread added the element (streaming threads included) and must not
    /// take a crate lock there. `Relaxed` is enough: a late edge costs one
    /// poll's worth of a stale probe list on a scrubber nub, never a wrong
    /// decision.
    level_probes_dirty: AtomicBool,
}

/// An RAII hold on [`Inner::route_gate`], [`Inner::join_gate`] or both.
/// Dropping it releases what it holds FIRST and then re-attempts
/// `deferred_pads`, so the invariant is simply "every gate release drains": a
/// pad deferred while any holder had the gate is re-routed the moment that
/// holder finishes, with no polling thread.
struct RouteGate<'a> {
    inner: &'a Arc<Inner>,
    guard: Option<parking_lot::MutexGuard<'a, ()>>,
    join: Option<parking_lot::MutexGuard<'a, ()>>,
}

impl Drop for RouteGate<'_> {
    fn drop(&mut self) {
        // Release the mutexes before draining: the drain re-enters
        // `route_db3_pad`, which must be able to take the gate itself.
        self.join.take();
        self.guard.take();
        Inner::drain_deferred_pads(self.inner);
    }
}

impl Inner {
    /// Take BOTH gates (blocking): a downward transition excludes routes and
    /// chain joins alike. See [`RouteGate`] and [`Inner::join_gate`].
    fn gate(inner: &Arc<Inner>) -> RouteGate<'_> {
        let guard = inner.route_gate.lock();
        let join = inner.join_gate.lock();
        RouteGate {
            inner,
            guard: Some(guard),
            join: Some(join),
        }
    }

    /// Take the route gate without blocking. See [`RouteGate`].
    fn try_gate(inner: &Arc<Inner>) -> Option<RouteGate<'_>> {
        inner.route_gate.try_lock().map(|guard| RouteGate {
            inner,
            guard: Some(guard),
            join: None,
        })
    }

    /// Take the JOIN gate only (blocking), which excludes downward transitions
    /// without making a concurrent route defer. See [`Inner::join_gate`].
    fn join_hold(inner: &Arc<Inner>) -> RouteGate<'_> {
        RouteGate {
            inner,
            guard: None,
            join: Some(inner.join_gate.lock()),
        }
    }

    /// Clear everything that belonged to the ITEM. THE list, for both
    /// boundaries that end one: a load (which then wires the next item up) and
    /// a stop (which does not).
    ///
    /// # Why one function and not a list per boundary
    ///
    /// It was a list per boundary and they diverged: the stop's copy had
    /// drifted to 9 of the load's 17 clears, so a receiver that stopped and
    /// idled kept the ended item's degradation memos and drained-stream
    /// ids. State that only ever grows and is only ever emptied by the NEXT
    /// load is a slow leak in a receiver that plays for days - which is
    /// exactly what the load's copy documented itself as preventing, for a
    /// stop that may never come. One list makes that divergence
    /// unrepresentable rather than merely discouraged.
    ///
    /// # What is deliberately NOT here
    ///
    /// * **The generation store.** The load's alone, and it stays ahead of this
    ///   call, where it is: everything after it belongs to the new item, and a
    ///   stop allocates no generation (see [`Inner::generation`]).
    /// * **`swap_gate.abort()` and `prepared = None`.** Both boundaries do
    ///   them, and both do them BEFORE their own state change, which joins
    ///   streaming threads: a prepared-input thread parked on the swap gate
    ///   would never be joined. That ordering is the point, so it stays at the
    ///   call sites.
    /// * **The gapless boundary's reset**, which is a third list on purpose
    ///   (`Inner::activate_prepared_now`): it keeps the user's own track intent
    ///   (`SelectionEngine::reset_across_gapless`) and keeps the text-park
    ///   memos, which key on a decodebin3 core that boundary does not replace.
    ///
    /// Runs with no gate held, after the boundary's own state change and
    /// element removals.
    pub(crate) fn reset_item_state(&self) {
        // A fresh core outputs a fresh group; the first STREAM_START records
        // it (see [`Inner::active_group`]).
        *self.active_group.lock() = None;
        *self.retired_group.lock() = None;
        *self.passing_eos_group.lock() = None;
        // The input-side drained set names streams of inputs this boundary has
        // just removed. Its own rule (EOS marks drained, FLUSH_STOP marks
        // restarted) is untouched here and stays distinct from the output-side
        // twin's proof-of-life rule (`RoutedStream::saw_eos`); this is only the
        // item ending, which ends both facts.
        self.input_eos_sids.lock().clear();
        // A boundary supersedes any gapless activation still held for the sink
        // boundary; its events belong to the play item that is ending.
        *self.held_activation.lock() = None;
        // Track desires are per-item: the next item starts on the pipeline's own
        // defaults. The ending item's collection goes with them (nothing else
        // caches it, see `SelectionEngine::video_ids`). Branch disposals, input
        // removals and replays stay PENDING across this: their targets outlive
        // the item and their drains no-op when stale. Shared verbatim with the
        // gapless boundary bar the engine reset (`Inner::reset_track_state`).
        self.reset_track_state(false);
        // The dedupe keys already carry the generation, so nothing here could
        // suppress a report for the NEXT item. Cleared for the leak the doc
        // above is about (see [`Inner::text_degradations`]).
        self.text_degradations.lock().clear();
        // The two text-park memos. Unlike the one above these are not
        // generation-keyed, they key on the decodebin3 output pad NAME, which
        // is per-ELEMENT, so the next load's fresh core hands out `text_0`
        // again. A straggler (`Inner::teardown_core`'s case, where pad-removed
        // never came so `forget_parked_text_cues` never ran) would then be
        // addressed by the NEW item's first text pad, replaying the previous
        // item's cues into it and eating its opening `Clear`.
        self.parked_text_cues.lock().clear();
        self.suppress_text_clear.lock().clear();
        // Every armed timer was armed for an input this boundary has just
        // removed.
        self.clear_pending_timers();
        *self.intended_timeline.lock() = (1.0, gst::ClockTime::ZERO);
        self.video_deselected.store(false, Ordering::SeqCst);
        self.video_unrouted_once.store(false, Ordering::SeqCst);
        // The item's graph is gone with its core, and so is every level probe
        // walked out of it (the boundary's own element removals say so too;
        // this is the reset saying it in one place).
        self.invalidate_level_probes();
    }
}

/// The four waits a caller may tune, in one lock.
///
/// Together because they are all the same thing: a length production takes
/// from a constant and tests shorten so a suite does not sit out a 10-second
/// deadline. None of them is read on a hot path, both readers that want two
/// of them want them at the same instant, and one guard is one guard whether
/// it covers one `Duration` or four (`Inner::run_tick`'s one-mutex-at-a-time
/// discipline is satisfied by a single `Deadlines` read).
///
/// The engine is never handed a length: it wants an ABSOLUTE due time and
/// never reads a clock, so these only ever feed an `Instant::now() + dur`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    /// The external-subtitle materialization timeout, normally
    /// [`EXTERNAL_SUB_TIMEOUT`]
    /// ([`FcastPlaybin::set_external_sub_timeout`]).
    sub_timeout: Duration,
    /// The selection-confirmation deadline, normally [`SELECTION_DEADLINE`]
    /// ([`FcastPlaybin::set_selection_deadline`]). Shortening THIS rather
    /// than [`TICK_INTERVAL`] is what keeps a deadline test off the tick's
    /// own timing.
    selection: Duration,
    /// The refresh-seek deadline, normally [`REFRESH_DEADLINE`]
    /// ([`FcastPlaybin::set_refresh_deadline`]).
    refresh: Duration,
    /// How long a deadline defers to an unsent select before timing it out
    /// anyway, normally [`SELECT_DEFER_BUDGET`]
    /// ([`FcastPlaybin::set_select_defer_budget`]).
    select_defer_budget: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            sub_timeout: EXTERNAL_SUB_TIMEOUT,
            selection: SELECTION_DEADLINE,
            refresh: REFRESH_DEADLINE,
            select_defer_budget: SELECT_DEFER_BUDGET,
        }
    }
}

/// This pipeline's diagnostic census (see the [`stats`] module for what the
/// counters are FOR and how a test reads them).
///
/// One field instead of nine on `Inner`, because none of them is state: they
/// are instruments, written with `Relaxed` from wherever the shape they
/// count happens, read only as a [`Stats`] snapshot, and branched on nowhere.
/// Keeping them together is what lets the god object's field forest stay
/// about state.
#[derive(Default)]
pub(crate) struct Counters {
    /// How many selection/refresh deadlines [`Inner::run_tick`] has fired.
    /// A healthy run leaves this at zero, which is what makes it worth
    /// reading in a soak.
    deadline_fires: AtomicU64,
    /// How many fires ended in a synthetic confirmation (the probe found the
    /// selection applied and only its message lost).
    deadline_confirms: AtomicU64,
    /// How many fires exhausted their retries and reported reality instead of
    /// the request.
    deadline_giveups: AtomicU64,
    /// How many fires deferred to a select still on its lane (see
    /// [`FcastPlaybin::selection_deadline_fired`]). The only way a test can
    /// tell a deferral from a fire that found everything healthy.
    deadline_deferrals: AtomicU64,
    /// How many jobs the epoch gate has dropped. Read by tests through
    /// [`Stats::stale_job_drops`].
    stale_jobs_dropped: AtomicU64,
    /// How many activations took the arm-time spent-edge branch (see
    /// [`Stats::arm_time_activation_releases`]). PER INSTANCE so a test can
    /// pin the branch in its own pipeline instead of in a log buffer the
    /// whole test binary shares.
    arm_time_releases: AtomicU64,
    /// How many [`Job::DrainTextWork`] jobs the worker received. Read through
    /// [`Stats::drain_text_job_count`] by the busy-loop regression test.
    drain_jobs_seen: AtomicU64,
    /// How many text-policy polls were coalesced into an already-queued one.
    /// Read through [`Stats::poll_policy_coalesced`] by the busy-loop
    /// regression test, which pins that a polling caller neither accumulates
    /// jobs nor loses an edge.
    poll_policy_coalesced: AtomicU64,
    /// How many [`Job::PollTextPolicy`] jobs the worker has received. The
    /// other half of that accounting (see [`Counters::drain_jobs_seen`],
    /// which this mirrors).
    poll_jobs_seen: AtomicU64,
    /// The LONGEST enqueue-to-run delay any [`Job::DispatchSelection`] has
    /// seen, in microseconds. The mirror of `hands::select_age` for the hop
    /// this phase added in front of the lane: a switch that feels slow is
    /// either queued here or queued there, and now both are readable.
    dispatch_queue_age_us: AtomicU64,
}

impl Counters {
    /// One more of whatever the counter counts. The census's one write idiom,
    /// so the ordering argument above is made once instead of at nine sites.
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Faults a test stages on ONE pipeline (see [`Inner::staging`]).
///
/// PER INSTANCE and not process-global, deliberately: a global would corrupt
/// the other pipelines in a test binary's thread pool, the exact failure mode
/// `text_arm::cue_window_for` was rewritten to avoid.
///
/// Allocated by the first `stage_*` setter and never otherwise, so a pipeline
/// nobody staged anything on reads one null pointer per site instead of an
/// atomic per site.
///
/// The setters are driven from INTEGRATION tests, which are separate
/// compilation units, so `#[cfg(test)]` does not apply to any of this; the
/// setters are `#[doc(hidden)]` on [`FcastPlaybin`] instead.
///
/// Orderings stay `SeqCst` throughout. These are branched on rather than
/// counted, and unlike the diagnostic counters they have to be seen by the
/// streaming thread the very next time it passes the site.
#[derive(Default)]
pub(crate) struct TestStaging {
    /// Hold a text branch at NULL for this many milliseconds AFTER its
    /// upstream link goes in; `0` is off.
    ///
    /// That is the join window made reproducible: linked to a live decodebin3
    /// output while its own pads are still inactive, so the first thing across
    /// the link (a sticky forward, a sparse stream's GAP, the first buffer)
    /// returns FLUSHING and latches the multiqueue slot for good (see
    /// [`Inner::heal_latched_text_slots`]).
    join_hold_ms: AtomicU64,
    /// Branches [`TestStaging::join_hold_ms`] is holding at NULL, with the
    /// instant each may be brought up. Released by the next text poll (see
    /// [`TestStaging::release_due_joins`]).
    ///
    /// A LIST AND A DEADLINE, not a sleep. The first version of the staging
    /// slept on the decider inside the join, and that froze the whole pipeline
    /// for the hold: with a 4 s hold, the first data flow anywhere in the graph
    /// (audio included) was 11 ms after the hold ended, so nothing could cross
    /// the staged link and the staging reproduced nothing. The window has to be
    /// one the rest of the graph keeps running through, which is also what the
    /// field's window was.
    joins: Mutex<Vec<(gst::Element, gst::Element, Instant)>>,
    /// One-shot; `false` is off. Destroys the sticky CAPS of the first parked
    /// text stream that has one, on its decodebin3 ghost AND on the multiqueue
    /// slot behind it, leaving the slot's SINK pad untouched.
    ///
    /// That is bit-for-bit the state left behind when multiqueue unrefs a
    /// popped CAPS in `out_flushing` ([`Inner::rescue_lost_text_slot_caps`]), a
    /// race between two GStreamer threads that no test can win on demand, so
    /// the RECOVERY gets the staging.
    text_caps_loss: AtomicBool,
    /// Swallow the next N external cues at the feed sites instead of
    /// delivering them, staging in-flight destruction (buffers lost, events
    /// kept). One unit per cue.
    cue_loss: AtomicU32,
    /// Sleep this many milliseconds at the top of `activate_prepared_now`;
    /// `0` is off.
    ///
    /// Stages the R1 window reproducibly. The selection-side activation
    /// trigger runs on a bus posting thread with no ordering against the audio
    /// data plane, so on a reused slot the new item's STREAM_START can cross a
    /// near-empty `fpb-aqueue` before the activation arms `held_activation`.
    /// Exactly one STREAM_START crosses per item, so an arm after that edge
    /// would wait forever (the queue_autoplay tracks-never-advertised boundary
    /// wedge) without the arm-time sticky check in `activate_prepared_now`.
    ///
    /// The sleep holds no crate lock and the data plane keeps flowing through
    /// it, which is exactly the field's window.
    activation_delay_ms: AtomicU64,
    /// Make every slot-seeding GAP push read as refused; persistent while set
    /// (see [`Inner::seed_slot_for_held_pad`]).
    ///
    /// A real refusal comes from the pad going FLUSHING under the push, the
    /// realigning replay's own seek, which is a window no test can hit on
    /// demand, so the RECOVERY gets the staging rather than going unpinned.
    slot_seed_refusal: AtomicBool,
    /// Make every forwarded seek to a live external read as refused;
    /// persistent while set (see `Inner::forward_seek_to_live_externals`).
    ///
    /// A real refusal comes from a source's own seek handling deep inside a
    /// parser chain (the field's was `rssubparse` converting the TIME seek to
    /// BYTES and its upstream failing that), which no test can arrange from
    /// the outside, so the RECOVERY gets the staging.
    forward_seek_refusal: AtomicBool,
    /// Relink a subtitle stream the applied selection names even while the
    /// DESIRED state is an explicit subtitle-off, restoring the unconditional
    /// relink the stomp guard in `Inner::poll_text_policy` replaced.
    link_stomped_subtitle: AtomicBool,
    /// Rebuild a chain for a video pad that appears while the dispatched
    /// selection has video off, restoring the unconditional rebuild the
    /// resurrect guard in `Inner::route_db3_pad` replaced.
    route_deselected_video: AtomicBool,
    /// Put this instance's text-branch disposals and input removals back on
    /// the calling thread rather than the worker's drain.
    ///
    /// PER INSTANCE: a test whose subject is the INLINE path on an
    /// already-running pipeline (the pair-D geometry in `sink_subtitles`)
    /// must not put every other pipeline in the binary on the same path.
    text_work_deferral_off: AtomicBool,
}

impl TestStaging {
    /// Bring up every branch whose staged join window has expired.
    fn release_due_joins(&self) {
        use gst::prelude::{ElementExt, GstObjectExt};

        if self.join_hold_ms.load(Ordering::SeqCst) == 0 {
            return;
        }
        let due: Vec<(gst::Element, gst::Element)> = {
            let mut staged = self.joins.lock();
            let now = Instant::now();
            let (due, waiting) = std::mem::take(&mut *staged)
                .into_iter()
                .partition::<Vec<_>, _>(|(_, _, at)| *at <= now);
            *staged = waiting;
            due.into_iter()
                .map(|(tqueue, appsink, _)| (tqueue, appsink))
                .collect()
        };
        for (tqueue, appsink) in due {
            tracing::warn!(
                tqueue = %tqueue.name(),
                "TEST STAGING: releasing a held text branch into its live link"
            );
            let _ = appsink.sync_state_with_parent();
            let _ = tqueue.sync_state_with_parent();
        }
    }
}

/// Read and write sides of [`Inner::staging`]. Every reader goes through here
/// so the "nobody staged anything" null check lives in exactly one place.
impl Inner {
    /// The staged faults, `None` on every production pipeline.
    fn staging(&self) -> Option<&TestStaging> {
        self.staging.get().map(|staged| &**staged)
    }

    /// The staged faults, allocating on first use. Setters only.
    fn staging_or_init(&self) -> &TestStaging {
        self.staging
            .get_or_init(|| Box::new(TestStaging::default()))
    }

    /// The staged text-join hold in milliseconds; `0` is off. See
    /// [`TestStaging::join_hold_ms`].
    pub(crate) fn stage_join_hold_ms(&self) -> u64 {
        self.staging()
            .map_or(0, |staged| staged.join_hold_ms.load(Ordering::SeqCst))
    }

    /// Park a joined text branch at NULL for the staged hold, returning the
    /// hold so the caller can log it. `0` means nothing was staged.
    pub(crate) fn stage_hold_join(&self, tqueue: &gst::Element, appsink: &gst::Element) -> u64 {
        let Some(staged) = self.staging() else {
            return 0;
        };
        let hold = staged.join_hold_ms.load(Ordering::SeqCst);
        if hold > 0 {
            staged.joins.lock().push((
                tqueue.clone(),
                appsink.clone(),
                Instant::now() + Duration::from_millis(hold),
            ));
        }
        hold
    }

    /// Bring up any staged branch whose hold has expired. See
    /// [`TestStaging::release_due_joins`].
    pub(crate) fn stage_release_due_joins(&self) {
        if let Some(staged) = self.staging() {
            staged.release_due_joins();
        }
    }

    /// Whether a one-shot caps loss is armed. See
    /// [`TestStaging::text_caps_loss`].
    pub(crate) fn stage_text_caps_loss_armed(&self) -> bool {
        self.staging()
            .is_some_and(|staged| staged.text_caps_loss.load(Ordering::SeqCst))
    }

    /// Disarm the one-shot caps loss once it has landed on a stream.
    pub(crate) fn stage_disarm_text_caps_loss(&self) {
        if let Some(staged) = self.staging() {
            staged.text_caps_loss.store(false, Ordering::SeqCst);
        }
    }

    /// TEST FAULT INJECTION: swallow one cue if a staged loss is armed (see
    /// [`TestStaging::cue_loss`]). True means the caller drops the cue.
    pub(crate) fn stage_consume_cue_loss(&self) -> bool {
        self.staging().is_some_and(|staged| {
            staged
                .cue_loss
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        })
    }

    /// The staged gapless-activation delay in milliseconds; `0` is off. See
    /// [`TestStaging::activation_delay_ms`].
    pub(crate) fn stage_activation_delay_ms(&self) -> u64 {
        self.staging().map_or(0, |staged| {
            staged.activation_delay_ms.load(Ordering::SeqCst)
        })
    }

    /// See [`TestStaging::slot_seed_refusal`].
    pub(crate) fn stage_slot_seed_refusal(&self) -> bool {
        self.staging()
            .is_some_and(|staged| staged.slot_seed_refusal.load(Ordering::SeqCst))
    }

    /// See [`TestStaging::forward_seek_refusal`].
    pub(crate) fn stage_forward_seek_refusal(&self) -> bool {
        self.staging()
            .is_some_and(|staged| staged.forward_seek_refusal.load(Ordering::SeqCst))
    }

    /// See [`TestStaging::text_work_deferral_off`].
    pub(crate) fn stage_text_work_deferral_off(&self) -> bool {
        self.staging()
            .is_some_and(|staged| staged.text_work_deferral_off.load(Ordering::SeqCst))
    }

    /// See [`TestStaging::link_stomped_subtitle`].
    pub(crate) fn stage_link_stomped_subtitle(&self) -> bool {
        self.staging()
            .is_some_and(|staged| staged.link_stomped_subtitle.load(Ordering::SeqCst))
    }

    /// See [`TestStaging::route_deselected_video`].
    pub(crate) fn stage_route_deselected_video(&self) -> bool {
        self.staging()
            .is_some_and(|staged| staged.route_deselected_video.load(Ordering::SeqCst))
    }
}

/// The `stage_*` setters. All `#[doc(hidden)]`, none part of the public API,
/// and all per instance for the reason [`TestStaging`] gives at length.
impl FcastPlaybin {
    /// TEST FAULT INJECTION: hold every text branch this instance joins at
    /// NULL for `hold` after its upstream link, staging the field's join
    /// window (see [`TestStaging::join_hold_ms`]).
    #[doc(hidden)]
    pub fn stage_join_before_active(&self, hold: Duration) {
        self.inner
            .staging_or_init()
            .join_hold_ms
            .store(hold.as_millis() as u64, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: destroy the next parked text stream's sticky CAPS
    /// on its decodebin3 ghost and on the multiqueue slot behind it. One shot
    /// (see [`TestStaging::text_caps_loss`]).
    #[doc(hidden)]
    pub fn stage_text_caps_loss(&self) {
        self.inner
            .staging_or_init()
            .text_caps_loss
            .store(true, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: swallow the next `count` external cues at the
    /// feed sites instead of delivering them, staging the multiqueue's
    /// in-flight destruction (see [`TestStaging::cue_loss`]).
    #[doc(hidden)]
    pub fn stage_text_cue_loss(&self, count: u32) {
        self.inner
            .staging_or_init()
            .cue_loss
            .store(count, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: delay this instance's next gapless activation by
    /// `delay`, staging the window between the boundary's data flow and the
    /// activation's arm of `held_activation` (see
    /// [`TestStaging::activation_delay_ms`]).
    #[doc(hidden)]
    pub fn stage_activation_delay(&self, delay: Duration) {
        self.inner
            .staging_or_init()
            .activation_delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: make this instance's slot-seeding GAP pushes read
    /// as refused until cleared (see [`TestStaging::slot_seed_refusal`]).
    #[doc(hidden)]
    pub fn stage_slot_seed_refusal(&self, refuse: bool) {
        self.inner
            .staging_or_init()
            .slot_seed_refusal
            .store(refuse, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: make this instance's forwarded seeks to live
    /// externals read as refused until cleared (see
    /// [`TestStaging::forward_seek_refusal`]).
    #[doc(hidden)]
    pub fn stage_forward_seek_refusal(&self, refuse: bool) {
        self.inner
            .staging_or_init()
            .forward_seek_refusal
            .store(refuse, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: run this instance's text-branch disposals and
    /// input removals inline on the calling thread until cleared (see
    /// [`TestStaging::text_work_deferral_off`]).
    #[doc(hidden)]
    pub fn stage_text_work_deferral_off(&self, off: bool) {
        self.inner
            .staging_or_init()
            .text_work_deferral_off
            .store(off, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: restore the unconditional relink of a subtitle
    /// stream stomped over an explicit subtitle-off (see
    /// [`TestStaging::link_stomped_subtitle`]).
    #[doc(hidden)]
    pub fn stage_link_stomped_subtitle(&self, link: bool) {
        self.inner
            .staging_or_init()
            .link_stomped_subtitle
            .store(link, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: restore the unconditional chain rebuild for a
    /// video pad resurrected over a dispatched video-off (see
    /// [`TestStaging::route_deselected_video`]).
    #[doc(hidden)]
    pub fn stage_route_deselected_video(&self, route: bool) {
        self.inner
            .staging_or_init()
            .route_deselected_video
            .store(route, Ordering::SeqCst);
    }
}

/// The playback orchestrator. `Clone` is a cheap handle onto the same
/// pipeline. Internal callbacks run on GStreamer streaming threads and only
/// touch `RoutingState` under its lock.
///
/// # Threading
///
/// Every method is callable from any thread EXCEPT a GStreamer streaming
/// thread or the event callback: the state-changing calls
/// ([`play`](Self::play)/[`pause`](Self::pause)/[`stop`](Self::stop)/
/// [`load`](Self::load)/[`set_pipeline_state`](Self::set_pipeline_state))
/// wrap `gst_element_set_state`, which is MT-safe but may wait on the very
/// streaming threads it reconfigures (the standard GStreamer self-deadlock).
/// From event loops, bus callbacks, or anywhere blocking is unacceptable,
/// use the `_async` variants: they queue onto the crate's worker thread,
/// which also keeps the operations ordered. Downward transitions take the
/// internal route gate (`stop`, `set_pipeline_state`, the worker's jobs).
/// `play`/`pause` are upward and need none.
#[derive(Clone)]
pub struct FcastPlaybin {
    inner: Arc<Inner>,
}
