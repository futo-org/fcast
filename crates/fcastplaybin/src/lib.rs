// TODO: remove env var override crap
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
//! audio chain: ssync -> queue -> audioconvert -> audioresample -> scaletempo
//!              -> volume -> audio sink
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
        atomic::{AtomicBool, AtomicU64},
        mpsc,
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

pub mod graph;
pub mod selection;
pub mod state_machine;

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
mod levers;
mod pipeline;
mod routing;
mod teardown;
mod text;
mod text_disposal;
mod text_policy;

#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod tests;

pub use api::{
    AudioSink, AudioSinkFactory, BitmapSubFormat, CueIr, ErrorOrigin, ExternalSubId, MediaInput,
    MessageHook, PlaybinEvent, Sinks, SourceDbg, StartOutcome, StartPoint, StreamIoStats,
    SubtitleConsumer, SubtitleFeedItem, SubtitleTextFormat, bitmap_format_implemented,
};

pub use buffering::{BufferedRange, BufferingInfo};
pub use levers::cue_ir_enabled;

use crate::{
    api::EventCallback,
    gapless::{HeldActivation, PreparedNext, SwapGate},
    jobs::{QueuedJob, TimerEntry},
    levers::BitmapSubsEnabled,
    routing::{Input, RoutingState},
    text_disposal::TextDisposal,
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
    /// Inputs with a replay verification already armed, so a second arming
    /// cannot start a rival chain. See [`Inner::arm_replay_verification`].
    replay_checks_armed: Mutex<std::collections::HashSet<(ExternalSubId, u32)>>,
    /// Replays this crate has emitted and not yet seen the outcome of, keyed
    /// by `(id, epoch)`.
    ///
    /// THE per-resource in-flight bit. The reconcile pass
    /// may emit an effect for a resource only when no effect for that resource
    /// is already in flight, and the hands' table cannot answer this: it is
    /// per-EFFECT and per-LANE, so it says "a replay is on the replay lane",
    /// never "a replay for THIS external is outstanding". Without it a pass
    /// that runs while a replay is mid-seek would emit a second one against
    /// the same input, and the two would fight over its segment.
    ///
    /// Keyed by (id, EPOCH) and not by id alone, which is the F1 lesson made
    /// structural: a gapless activation clears the mirrors WITHOUT bumping any
    /// epoch, so nothing in the reconciler may key on "the current item". A
    /// re-attach bumps the epoch, and the old epoch's bit can then never
    /// suppress the new epoch's replay.
    ///
    /// # Where it is set and cleared
    ///
    /// SET at the choke point, [`FcastPlaybin::replay_subtitle`], which every
    /// replay funnels through - the reconcile pass, the selection-time replay,
    /// the upstream adoption, `verify_replay`'s re-replay and the levered
    /// drain. It is also set at the sites that QUEUE a `Job::ReplaySub`,
    /// because the job runs later and the window between queueing and running
    /// is a window in which the pass would otherwise see both guards clear.
    /// Setting a set twice is free; missing one is not.
    ///
    /// CLEARED in [`FcastPlaybin::replay_outcome`] (the decider tail every
    /// outcome reaches, including the refusal), on each of `replay_subtitle`'s
    /// early returns, in `run_lane_fallback`'s `Replay` arm (an abandoned
    /// effect reports no outcome), and in [`Inner::remove_input`] - the one
    /// function every removal path funnels through, so an epoch that dies
    /// takes its bit with it.
    replay_inflight: Mutex<std::collections::HashSet<(ExternalSubId, u32)>>,
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
    external_cues_fed: Mutex<std::collections::HashMap<ExternalSubId, u64>>,
    /// Replays owed once the pipeline is playing again, because a flushing
    /// seek cannot be delivered to one at rest in PAUSED. See
    /// [`FcastPlaybin::replay_subtitle`].
    ///
    /// LEVER-ONLY now. The reconcile pass
    /// ([`Inner::reconcile_subtitle_delivery`]) re-derives this from the graph
    /// at every settled PLAYING, so a REMEMBERED list of owed work is exactly
    /// the compensation the reconciler replaces. Kept, unwritten, so
    /// `FCAST_NO_TEXT_RECONCILE` restores v1 rather than approximating it -
    /// the same rule the levered Flush machinery lives under.
    deferred_replays: Mutex<Vec<(ExternalSubId, u32, u32)>>,
    /// Replay verdicts held because the check fired at a pipeline below a
    /// settled PLAYING, where nothing flows and the branch stickies it would
    /// read are leftovers of the input's previous tenure. See
    /// [`FcastPlaybin::verify_replay`].
    ///
    /// LEVER-ONLY now, for the same reason as
    /// `deferred_replays`: a held verdict is a remembered intention to re-ask,
    /// and the pass re-asks unconditionally.
    deferred_verifications: Mutex<Vec<(ExternalSubId, u32, u32)>>,
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
    /// Suppression is disabled by the FCAST_NO_DRAIN_POKE_SUPPRESS lever,
    /// which restores the poke-on-every-poll behavior. The flag is the only
    /// thing the lever's branch reads, so the lever covers the whole
    /// change.
    drain_poke_parked: AtomicBool,
    /// Diagnostic count of [`Job::DrainTextWork`] jobs the worker received.
    /// Read through [`FcastPlaybin::drain_text_job_count`] by the busy-loop
    /// regression test. Not behavior.
    drain_jobs_seen: AtomicU64,
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
    /// the item. Reset per load.
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
    /// The video output chain, which is now the caller's
    /// video sink and nothing else (subtitleoverlay sat in front of it until
    /// then; cues leave through [`Inner::subtitle_consumer`] now). It lives in
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
    /// (stream id, load generation) pairs already reported as
    /// [`PlaybinEvent::SubtitleTrackUnsupported`]. The link poll re-runs on
    /// every tick and would otherwise re-report the same unrenderable track
    /// forever; the generation in the key is what lets the SAME sid report
    /// again after a new load.
    unsupported_text_reported: Mutex<std::collections::HashSet<(String, u64)>>,
    /// (stream id, load generation) pairs whose consumer branch could not be
    /// WIRED, as opposed to refused on its caps. The poll that builds the
    /// branch runs on every tick, and a link GStreamer refuses will be refused
    /// again for the same reason every time: without this the crate rebuilds
    /// and tears down a queue once per tick for the rest of the item, logging
    /// a warning each time and never telling the caller anything. Same key
    /// shape and same lifetime as [`Inner::unsupported_text_reported`], so a
    /// new load tries again.
    unwirable_text_streams: Mutex<std::collections::HashSet<(String, u64)>>,
    /// When the outputless-slot scan FIRST saw the selected text stream
    /// stranded in a decodebin3 multiqueue slot with no output pad, and whether
    /// it has said so (see [`Inner::adopt_outputless_text_slot`]).
    /// `Some(first_seen)` inside the grace, `None` once described.
    ///
    /// Both halves are needed and for different reasons. The DEDUPE, because
    /// the scan runs on every poll for as long as the shape lasts, measured at
    /// 3990 hits over one 2 s window, at 400 characters a line. The GRACE,
    /// because the shape is now TRANSIENT on the healthy path: the re-select
    /// drain interlock ([`Inner::await_text_input_drain`]) holds the send while
    /// exactly this is true, so a bare first-sight report would fire a warning
    /// on every fast subtitle round trip and mean nothing. Same key shape and
    /// lifetime as [`Inner::unsupported_text_reported`].
    ///
    /// The REPAIR is gated by neither: it runs on every poll, because a slot
    /// that can be given its caps back should get them at the first opportunity
    /// rather than after a grace.
    outputless_text_slots_reported:
        Mutex<std::collections::HashMap<(String, u64), Option<Instant>>>,
    /// When the text link loop's caps gate FIRST refused a routed stream for
    /// want of a sticky CAPS, and whether that stall has been reported.
    /// `Some(first_seen)` while it is still inside the grace period, `None`
    /// once the escalation has been logged. Same key shape and lifetime as
    /// [`Inner::unsupported_text_reported`].
    ///
    /// The gate calls caps-absent "rare and transient" and refuses WITHOUT
    /// reporting, which is right for the millisecond a pad spends between
    /// being exposed and carrying its sticky. It is not right forever: a
    /// stream whose decodebin3 input never gets a multiqueue slot never
    /// carries one, and the gate then refuses the join for the life of the
    /// item, silently, about a hundred times a second, while the selection
    /// reads as confirmed and the caller sees a track that simply never
    /// appears. Measured at ~4025 refusals over 40 s. This is the memory that
    /// turns that into ONE line naming the stream and the signature that says
    /// whether the break is upstream of the gate or in selection.
    capsless_text_since: Mutex<std::collections::HashMap<(String, u64), Option<Instant>>>,
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
    /// the join replays what is still worth showing. Lever:
    /// `FCAST_NO_PARKED_TEXT_REPLAY` restores the discarding park.
    parked_text_cues: Mutex<std::collections::HashMap<String, VecDeque<(gst::Sample, Instant)>>>,
    /// decodebin3 text pads whose branch may skip exactly ONE `Clear`.
    ///
    /// Armed by a replay that delivered something, consumed by the first
    /// `Clear` after it. The join's own sticky STREAM_START would otherwise
    /// wipe the opening cues the replay had just restored, which is the
    /// difference, measured, between the first covered frame landing at 0.2 s
    /// and at 4.085 s.
    suppress_text_clear: Mutex<std::collections::HashSet<String>>,
    /// Which bitmap subtitle formats this instance may carry. Read once, here.
    /// See [`BitmapSubsEnabled`] for why one read and not four lookups at the
    /// gate.
    bitmap_subs: BitmapSubsEnabled,
    /// TEST FAULT INJECTION, in milliseconds; `0` is off.
    ///
    /// Holds a text branch at NULL for this long AFTER its upstream link goes
    /// in, which is the join window made reproducible: linked to a live
    /// decodebin3 output while its own pads are still inactive, so the first
    /// thing across the link (a sticky forward, a sparse stream's GAP, the
    /// first buffer) returns FLUSHING and latches the multiqueue slot for good
    /// (see [`Inner::heal_latched_text_slots`]).
    ///
    /// PER INSTANCE and not an env lever, deliberately. Every other knob in
    /// this crate is `FCAST_*`, but an env var is process-global and this one
    /// would corrupt every other pipeline in a test binary's thread pool, the
    /// exact failure mode `text_arm::cue_window_for` was rewritten to avoid.
    /// The A/B partner (`FCAST_NO_SLOT_UNLATCH`) stays an env lever because it
    /// turns the FIX off, which a whole-binary run is the right granularity
    /// for.
    stage_join_hold_ms: AtomicU64,
    /// TEST FAULT INJECTION, one-shot; `false` is off.
    ///
    /// Destroys the sticky CAPS of the first parked text stream that has one,
    /// on its decodebin3 ghost AND on the multiqueue slot behind it, leaving
    /// the slot's SINK pad untouched. That is bit-for-bit the state left
    /// behind when multiqueue unrefs a popped CAPS in `out_flushing`
    /// ([`Inner::rescue_lost_text_slot_caps`]), a race between two GStreamer
    /// threads that no test can win on demand, so the RECOVERY gets the
    /// staging, exactly as `FCAST_FORCE_SLOT_SEED_REFUSAL` does for the
    /// seeding.
    ///
    /// PER INSTANCE for the reason [`Inner::stage_join_hold_ms`] gives at
    /// length: an env var would strip the caps of every other pipeline in the
    /// test binary's thread pool.
    stage_text_caps_loss: std::sync::atomic::AtomicBool,
    /// TEST FAULT INJECTION: swallow the next N external cues at the feed
    /// sites instead of delivering them, staging in-flight destruction
    /// (buffers lost, events kept). Per instance, one unit per cue.
    stage_cue_loss: std::sync::atomic::AtomicU32,
    /// Branches [`Inner::stage_join_hold_ms`] is holding at NULL, with the
    /// instant each may be brought up. Released by the next text poll.
    ///
    /// A LIST AND A DEADLINE, not a sleep. The first version of the staging
    /// slept on the decider inside the join, and that froze the whole pipeline
    /// for the hold: with a 4 s hold, the first data flow anywhere
    /// in the graph (audio included) was 11 ms after the hold ended, so nothing
    /// could cross the staged link and the staging reproduced nothing. The
    /// window has to be one the rest of the graph keeps running through, which
    /// is also what the field's window was.
    staged_joins: Mutex<Vec<(gst::Element, gst::Element, Instant)>>,
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
    /// How many jobs the epoch gate has dropped. Diagnostic only, read by
    /// tests through [`FcastPlaybin::stale_job_drops`].
    stale_jobs_dropped: AtomicU64,
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
    /// How many selection/refresh deadlines [`Inner::run_tick`] has fired.
    /// Diagnostic only; a healthy run leaves this at zero, which is what
    /// makes it worth reading in a soak.
    deadline_fires: AtomicU64,
    /// How many fires ended in a synthetic confirmation (the probe found the
    /// selection applied and only its message lost). Diagnostic only.
    deadline_confirms: AtomicU64,
    /// How many fires exhausted their retries and reported reality instead of
    /// the request. Diagnostic only.
    deadline_giveups: AtomicU64,
    /// How many fires deferred to a select still on its lane (see
    /// [`FcastPlaybin::selection_deadline_fired`]). Diagnostic, and
    /// the only way a test can tell a deferral from a fire that found
    /// everything healthy.
    deadline_deferrals: AtomicU64,
    /// How long a deadline defers to an unsent select before timing it out
    /// anyway ([`SELECT_DEFER_BUDGET`]). A field so tests can shorten it, the
    /// way they shorten the deadlines themselves.
    select_defer_budget: Mutex<Duration>,
    /// Whether a [`Job::PollTextPolicy`] is already queued and unrun. The
    /// coalescing bit of [`Inner::request_text_policy_poll`]: a receiver
    /// polling every 5ms must not put 200 identical jobs a second on the
    /// worker, and it need not - the policy re-reads the whole world when it
    /// runs, so N queued polls and one queued poll decide the same thing.
    /// Cleared by the JOB, before it runs, so a poke that lands mid-run
    /// queues a fresh one instead of being folded into a decision that has
    /// already been taken.
    poll_queued: AtomicBool,
    /// How many text-policy polls were coalesced into an already-queued one.
    /// Read through [`FcastPlaybin::poll_policy_coalesced`] by the busy-loop
    /// regression test, which pins that a polling caller neither accumulates
    /// jobs nor loses an edge. Diagnostic only.
    poll_policy_coalesced: AtomicU64,
    /// How many [`Job::PollTextPolicy`] jobs the worker has received. The
    /// other half of that accounting (see [`Inner::drain_jobs_seen`], which
    /// this mirrors). Diagnostic only.
    poll_jobs_seen: AtomicU64,
    /// How many dispatches the in-flight guard dropped without sending (see
    /// [`FcastPlaybin::dispatch_selection`]). Diagnostic only, and expected
    /// to stay at zero on a run with no collection churn.
    dispatch_guard_skips: AtomicU64,
    /// The LONGEST enqueue-to-run delay any [`Job::DispatchSelection`] has
    /// seen, in microseconds. The mirror of `hands::select_age` for the hop
    /// this phase added in front of the lane: a switch that feels slow is
    /// either queued here or queued there, and now both are readable.
    /// Diagnostic only.
    dispatch_queue_age_us: AtomicU64,
    /// Whether the replay's outcome tail runs on the LANE, as v1 ran it,
    /// instead of on the decider. Lever: `FCAST_INLINE_REPLAY_OUTCOME`.
    ///
    /// Read ONCE, here, and read from here by both sides - the same reason
    /// `Hands::live` is a flag rather than an environment lookup. Two
    /// independent reads could disagree, and one of the two disagreements is
    /// the tail running NOWHERE: an external held at its source pads for a
    /// seek that has already been sent, for good.
    inline_replay_outcome: bool,
    /// Whether the text-policy poll at the tail of `Inner::route_db3_pad`
    /// runs INLINE on the routing streaming thread, as v1 ran it, instead of
    /// asking the decider. Lever: `FCAST_INLINE_ROUTE_TEXT_POLL`.
    ///
    /// Its own lever rather than `FCAST_INLINE_TEXT_POLL`'s, because this is
    /// the one poll site on the instant-text-in-paused path: it is the move
    /// the switch-latency probe gates, and a field regression must be able to
    /// roll back THIS hop without putting the receiver's own 5ms poll back on
    /// the caller's thread. Read once, like the flag above.
    inline_route_text_poll: bool,
    /// Whether a dispatched selection is EXECUTED on the pumping caller's
    /// thread, as v1 executed it, instead of on the decider (see
    /// [`Job::DispatchSelection`]). Lever: `FCAST_INLINE_DISPATCH`.
    ///
    /// Read once for the same reason as the two flags above, and with a
    /// sharper consequence: the pump decides whether to queue the job, and
    /// the job body would decide nothing at all if it disagreed - a flag
    /// flipped mid-instance could otherwise leave one selection parked on a
    /// queue nobody drains, or run one twice.
    inline_dispatch: bool,
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
    /// Whether any lever has put text-branch surgery back on a foreign
    /// thread, which turns [`Inner::decider_only`] from an assertion into an
    /// observation.
    ///
    /// The six that can: `FCAST_INLINE_TEXT_POLL` and
    /// `FCAST_INLINE_ROUTE_TEXT_POLL` (the policy runs on the asking thread),
    /// `FCAST_INLINE_DISPATCH` (the eager park runs on the pumping caller),
    /// `FCAST_INLINE_REPLAY_OUTCOME` (the exhaustion poke runs on the replay
    /// lane), `FCAST_INLINE_VIDEO_CHAIN_TEARDOWN` (the park runs on the
    /// pad-removed streaming thread) and `FCAST_NO_HANDS` - which is the one
    /// that is easy to miss, because it looks like it is only about the lane
    /// loops: `Inner::replay_sender_loop` runs the WHOLE v1 tail on
    /// `fpb-replay`, exhaustion poke included, so the policy's surgery lands
    /// there too (found by the parity arm, which is what parity arms are
    /// for). Each of those IS the v1 threading, so asserting against it would
    /// only prove the lever works; the counter still moves, which is how a
    /// test proves the instrument is real rather than vacuously silent.
    ///
    /// Read once at construction, like the levers themselves.
    text_ownership_levered: bool,
    /// How many text-branch surgeries ran off the deciding thread (see
    /// [`Inner::decider_only`]). Zero on every default-arm run - a debug build
    /// panics before it can be anything else - and positive under the levers
    /// above, which is the A/B that proves the instrument measures.
    /// Read through [`FcastPlaybin::text_surgery_off_decider`].
    text_surgery_off_decider: AtomicU64,
    /// The three effect lanes and their in-flight table (see the [`hands`]
    /// module). Owns the senders that used to be three separate fields, with
    /// the same lifetime discipline as `work_tx`: dropping them with `Inner`
    /// is what ends the lane threads.
    hands: Hands,
    /// The `fpb-tick` hangup channel (see [`Inner::run_tick`]). Nothing is
    /// ever SENT on it: dropping it with `Inner` is what ends the thread,
    /// exactly like the other senders here. `None` under `FCAST_NO_TICK`,
    /// which is also how every arming site asks whether the tick is live.
    tick_tx: Option<mpsc::Sender<()>>,
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
    /// The external-subtitle materialization timeout, normally
    /// [`EXTERNAL_SUB_TIMEOUT`]. Mutable only so tests can shorten it
    /// ([`FcastPlaybin::set_external_sub_timeout`]).
    sub_timeout: Mutex<Duration>,
    /// The selection-confirmation deadline, normally [`SELECTION_DEADLINE`].
    /// Mutable only so tests can shorten it
    /// ([`FcastPlaybin::set_selection_deadline`]); shortening THIS rather
    /// than [`TICK_INTERVAL`] is what keeps a deadline test off the tick's
    /// own timing.
    selection_deadline_dur: Mutex<Duration>,
    /// The refresh-seek deadline, normally [`REFRESH_DEADLINE`]. Mutable for
    /// the same reason ([`FcastPlaybin::set_refresh_deadline`]).
    refresh_deadline_dur: Mutex<Duration>,
    /// The pre-armed next item, if any (see [`PreparedNext`]). Lock order:
    /// take and RELEASE this before `routing`/`selection`, never hold it
    /// across them.
    prepared: Mutex<Option<PreparedNext>>,
    /// See [`SwapGate`].
    swap_gate: SwapGate,
    /// The group id of the item currently flowing OUT of decodebin3 (from
    /// STREAM_START on its output pads; reset per load). A change while a
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
    /// can end the pipeline between items. Reset per load.
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
    /// post-ssync gate consumes them before they reach the sinks. Reset per
    /// load.
    passing_eos_group: Mutex<Option<gst::GroupId>>,
    /// Stream ids whose INPUT-side stream has delivered EOS into decodebin3
    /// and has not been restarted by a flush since (recorded by a probe on
    /// every input pad linked into decodebin3, see `Inner::link_input_pad`).
    /// A deselected stream's end never reaches the output probes (its slot
    /// is gone), so `passing_eos_group` cannot see it, and re-routing such
    /// a stream builds a chain that can never preroll. The
    /// drained-resurrect park in `route_db3_pad` consults this next to the
    /// group mirror. Reset per load. Maintained only while the park's lever
    /// is unset (`FCAST_NO_DRAINED_RESURRECT_PARK`).
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
    /// Cleared per load.
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
