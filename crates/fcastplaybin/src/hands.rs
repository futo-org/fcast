//! The crate's HANDS: one effect executor with three FIFO lanes.
//!
//! An [`Envelope`] carries an [`EffectId`] and the queue epoch it was formed
//! under. Each lane is a FIFO on its own thread, revalidates immediately
//! before the irreversible send, and reports exactly one [`Outcome`] back to
//! the decider as a job.
//!
//! # The rules (each one grep-enforceable)
//!
//! 1. Lanes are not the worker. No `Job::SetState`, load, stop, or teardown is
//!    DISPATCHED here. Two sanctioned exceptions: `ChainJoin`'s element-level
//!    sink activation, and the pipeline descent a lane can carry as the last
//!    reference holder (so `Inner::drop` can run here).
//! 2. Lanes are not each other. One dedicated thread per lane (`fpb-select`,
//!    `fpb-replay`, `fpb-join`). `Inner::route_db3_pad` runs INLINE on the
//!    select lane when decodebin3 exposes a pad inside `send_event` and
//!    enqueues a chain join from there. A shared lane would queue that join
//!    behind the very send that produced it.
//! 3. The decider never blocks on a hand. Nothing joins a lane or waits for a
//!    completion. The only coupling is the in-flight table, whose mutex is a
//!    LEAF: nothing else is acquired while it is held, and it is never taken
//!    while holding `selection`. `routing` and the gates CAN be held across an
//!    enqueue (`route_db3_pad` queues its join under the route gate so the
//!    routing entry is visible to the join first).
//! 4. Every enqueue produces EXACTLY ONE outcome, with two edges. A lane whose
//!    `Weak` no longer upgrades stops without reporting. A lane that cannot
//!    reach the decider retires the entry itself and settles what is owed. A
//!    body that never finished owes what the EFFECT owes ([`LaneFallback::of`],
//!    armed before the body and covering an unwind). A finished body owes what
//!    its OUTCOME owes ([`Outcome::owed`]).
//! 5. Revalidation happens at EXECUTION, not at enqueue. A select whose queue
//!    epoch has moved is skipped WITH an outcome, never silently.
//!
//! Rules 4 and 5 fail silently (a lost `Done` is a permanent wedge warning
//! and a deadline that defers forever; a doubled one is a hold released
//! twice), so the `#[test]`s below pin the protocol on one schedule each and
//! the `loom_` tests pin the TABLE half on every schedule loom can build for
//! two threads. The lane bodies belong to the integration suites and
//! `tools/run-tsan.sh`.
//!
//! # Lever
//!
//! `FCAST_NO_HANDS=1` spawns the v1 loops instead
//! (`Inner::select_sender_loop` and friends). Chosen once at spawn time. The
//! enqueue side reads the `Hands::live` flag rather than the environment, so
//! a lane can never disagree with the executor actually running.

use std::{
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use gst::prelude::*;
use tracing::{debug, warn};

use crate::{
    ExternalSubId,
    jobs::{ChainJoinJob, ReplayJob, SelectJob},
    routing::StreamKind,
};

use sync::{AtomicU64, Mutex};

/// This module's synchronization primitives, swapped for loom's under
/// `--cfg loom`, so the `loom_` tests model-check the REAL `Hands`.
/// parking_lot's mutex is not loom-instrumentable, so the table and the
/// counters go through this alias instead.
///
/// Scoped to this module, so the cfg cannot leak into the crate's own locking
/// discipline. Under `--cfg loom` the crate still compiles but only the
/// `loom_` tests may RUN. A loom primitive touched outside a model panics.
mod sync {
    #[cfg(not(loom))]
    pub(crate) use std::sync::atomic::AtomicU64;

    #[cfg(loom)]
    pub(crate) use loom::sync::atomic::AtomicU64;

    /// parking_lot's mutex in an ordinary build.
    #[cfg(not(loom))]
    pub(crate) type Mutex<T> = parking_lot::Mutex<T>;

    /// loom's, wrapped so `lock()` yields the guard rather than a
    /// `LockResult` (the call sites are parking_lot's).
    #[cfg(loom)]
    #[derive(Debug, Default)]
    pub(crate) struct Mutex<T>(loom::sync::Mutex<T>);

    #[cfg(loom)]
    impl<T> Mutex<T> {
        pub(crate) fn new(value: T) -> Self {
            Self(loom::sync::Mutex::new(value))
        }

        pub(crate) fn lock(&self) -> loom::sync::MutexGuard<'_, T> {
            self.0.lock().expect("a poisoned lock inside a loom model")
        }
    }
}

/// Monotonic effect identity, never reused. A `Done` for an id the table no
/// longer holds is a double report, not a live effect.
pub(crate) type EffectId = u64;

/// How long an effect may sit in flight before the tick says so (see
/// [`Hands::wedged`]). Well above any healthy send.
pub(crate) const EFFECT_WEDGE_WARN: Duration = Duration::from_secs(10);

/// Skip reason: a load or a stop superseded the work the select was queued for.
pub(crate) const SKIPPED_STALE_EPOCH: &str = "a later load or stop superseded the queued work";

/// Skip reason: the lane never ran the select to completion (see
/// [`LaneFallback`]).
pub(crate) const SKIPPED_LANE_LOST: &str = "the lane abandoned the effect";

/// One unit of work for a lane.
pub(crate) enum Effect {
    SelectStreams(SelectJob),
    ReplaySeek(ReplayJob),
    ChainJoin(ChainJoinJob),
}

/// Which FIFO an effect belongs to. Total on [`Effect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    Select,
    Replay,
    Join,
}

impl Lane {
    /// The thread name this lane runs under; tests match on these names.
    pub(crate) fn thread_name(self) -> &'static str {
        match self {
            Lane::Select => "fpb-select",
            Lane::Replay => "fpb-replay",
            Lane::Join => "fpb-join",
        }
    }
}

/// Manual impl because the payloads carry GStreamer objects.
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::SelectStreams(job) => f
                .debug_struct("SelectStreams")
                .field("ids", &job.stream_ids)
                .finish(),
            Effect::ReplaySeek(job) => f
                .debug_struct("ReplaySeek")
                .field("id", &job.id)
                .field("epoch", &job.epoch)
                .field("attempt", &job.attempt)
                .finish(),
            Effect::ChainJoin(job) => f
                .debug_struct("ChainJoin")
                .field("kind", &job.kind)
                .finish(),
        }
    }
}

impl Effect {
    pub(crate) fn lane(&self) -> Lane {
        match self {
            Effect::SelectStreams(_) => Lane::Select,
            Effect::ReplaySeek(_) => Lane::Replay,
            Effect::ChainJoin(_) => Lane::Join,
        }
    }

    /// The dispatch this effect will confirm, for the deadline's in-flight
    /// consult (`FcastPlaybin::selection_deadline_fired`). Selects only.
    fn seqnum(&self) -> Option<gst::Seqnum> {
        match self {
            Effect::SelectStreams(job) => Some(job.event.seqnum()),
            Effect::ReplaySeek(_) | Effect::ChainJoin(_) => None,
        }
    }
}

/// An [`Effect`] with its identity and the queue epoch it was formed under.
pub(crate) struct Envelope {
    pub(crate) id: EffectId,
    /// Stamped at enqueue; the lane re-checks it against the live value
    /// immediately before the irreversible send.
    pub(crate) queue_epoch: u64,
    pub(crate) effect: Effect,
}

/// What a lane reports back. Every enqueue produces exactly one of these.
///
/// `Clone` but deliberately NOT `Copy`: an outcome that can be copied by
/// accident is an outcome that can be acted on twice.
#[derive(Debug, Clone)]
pub(crate) enum Outcome {
    SelectSent {
        seqnum: gst::Seqnum,
        /// The ids that went UPSTREAM, for `Inner::last_upstream_ids`. `None`
        /// when the event went to decodebin3. Carried rather than re-derived
        /// because only the lane knows what was actually sent (a superseded
        /// core or a refusal means no ids left the crate at all).
        upstream_ids: Option<Vec<String>>,
    },
    SelectRefused {
        seqnum: gst::Seqnum,
    },
    /// A superseded core or a stale queue epoch; never silent.
    SelectSkipped {
        seqnum: gst::Seqnum,
        reason: &'static str,
    },
    /// The replay's seek has been sent. The tail (hold release, refusal
    /// postponement, verification arming, exhaustion) is the DECIDER's
    /// (`FcastPlaybin::replay_outcome`), which is why the counts travel.
    ReplaySent {
        sub_id: ExternalSubId,
        epoch: u32,
        attempt: u32,
        /// How many of the input's pads took the seek.
        accepted: usize,
        /// How many were offered it. Zero means the input had no pads at all,
        /// which is NOT a refusal.
        total: usize,
    },
    JoinFinished {
        kind: StreamKind,
    },
}

impl Outcome {
    fn lane(&self) -> Lane {
        match self {
            Outcome::SelectSent { .. }
            | Outcome::SelectRefused { .. }
            | Outcome::SelectSkipped { .. } => Lane::Select,
            Outcome::ReplaySent { .. } => Lane::Replay,
            Outcome::JoinFinished { .. } => Lane::Join,
        }
    }

    /// What this outcome still owes when there is NO decider left to hand it
    /// to (rule 4's second edge), so the lane must settle it itself.
    ///
    /// Only the replay owes anything. An input still held at its source pads
    /// for a seek that has now been sent stays held forever otherwise.
    /// [`LaneFallback::Select`] is deliberately unreachable here because
    /// settling one goes THROUGH the decider (`Inner::run_lane_fallback`).
    pub(crate) fn owed(&self) -> Option<LaneFallback> {
        match *self {
            Outcome::ReplaySent { sub_id, epoch, .. } => {
                Some(LaneFallback::Replay { id: sub_id, epoch })
            }
            Outcome::SelectSent { .. }
            | Outcome::SelectRefused { .. }
            | Outcome::SelectSkipped { .. }
            | Outcome::JoinFinished { .. } => None,
        }
    }
}

/// The blocking probe holding one stream at the streamsynchronizer src pad
/// until its chain is up (see `Inner::hold_chain_entry`), in a slot two
/// parties can release from: the body under the join gate, or the lane's undo
/// ([`LaneFallback`]) when the body never gets there. Whoever takes it first
/// releases it, so "released exactly once" is a property of the type.
/// `gst::PadProbeId` is neither `Clone` nor `Copy` (a probe id may be spent
/// once), and this is how that is respected from two places.
#[derive(Clone, Default)]
pub(crate) struct JoinHold(Arc<Mutex<Option<(gst::Pad, gst::PadProbeId)>>>);

impl JoinHold {
    pub(crate) fn new(hold: Option<(gst::Pad, gst::PadProbeId)>) -> Self {
        Self(Arc::new(Mutex::new(hold)))
    }

    /// Let the held stream flow. Returns whether THIS call was the one that
    /// released it. A second call is a no-op.
    pub(crate) fn release(&self, why: &'static str) -> bool {
        let Some((pad, id)) = self.0.lock().take() else {
            return false;
        };
        debug!(pad = %pad.name(), why, "releasing a held stream");
        pad.remove_probe(id);
        true
    }
}

/// What an effect still OWES when nobody runs it to completion (the lane
/// unwound out of the body, is gone before the envelope could ship, or cannot
/// run it). Derived from the effect BEFORE the body consumes it. Each variant
/// is the minimum that keeps the crate unstuck, and each is idempotent
/// against the body having got there first.
pub(crate) enum LaneFallback {
    /// Tell the engine the dispatch is not coming, or it waits out its
    /// deadline for a send that will never happen.
    Select { seqnum: gst::Seqnum },
    /// Release the hold the replay owes on EVERY outcome. Covers the body not
    /// reaching its own release.
    Replay { id: ExternalSubId, epoch: u32 },
    /// Let the held stream flow. A stream held at the streamsynchronizer
    /// forever is a freeze.
    Join { hold: JoinHold },
}

impl LaneFallback {
    pub(crate) fn of(effect: &Effect) -> Self {
        match effect {
            Effect::SelectStreams(job) => LaneFallback::Select {
                seqnum: job.event.seqnum(),
            },
            Effect::ReplaySeek(job) => LaneFallback::Replay {
                id: job.id,
                epoch: job.epoch,
            },
            Effect::ChainJoin(job) => LaneFallback::Join {
                hold: job.hold.clone(),
            },
        }
    }
}

/// One effect the executor has accepted and not yet completed.
struct InFlight {
    id: EffectId,
    lane: Lane,
    enqueued: Instant,
    /// Present for select effects. Consulted by the selection deadline
    /// before it adopts a routed reality.
    seqnum: Option<gst::Seqnum>,
    /// One WARN per wedged effect, not one per tick.
    warned: bool,
}

/// A wedged effect, as the tick reports it.
pub(crate) struct Wedged {
    pub(crate) id: EffectId,
    pub(crate) lane: Lane,
    pub(crate) age: Duration,
}

/// The executor: three lane channels plus the in-flight table.
pub(crate) struct Hands {
    /// `false` under `FCAST_NO_HANDS`. The v1 loops read the same channels
    /// and nothing is registered in flight (nobody would report a `Done`).
    live: bool,
    select_tx: mpsc::Sender<Envelope>,
    replay_tx: mpsc::Sender<Envelope>,
    join_tx: mpsc::Sender<Envelope>,
    next_id: AtomicU64,
    /// Short by construction, so a linear scan is the right structure.
    in_flight: Mutex<Vec<InFlight>>,
    /// How many effects the tick has reported as wedged. Diagnostic, read
    /// through `FcastPlaybin::hands_wedge_warnings`.
    wedge_warnings: AtomicU64,
}

impl Hands {
    pub(crate) fn new(
        live: bool,
        select_tx: mpsc::Sender<Envelope>,
        replay_tx: mpsc::Sender<Envelope>,
        join_tx: mpsc::Sender<Envelope>,
    ) -> Self {
        Self {
            live,
            select_tx,
            replay_tx,
            join_tx,
            next_id: AtomicU64::new(0),
            in_flight: Mutex::new(Vec::new()),
            wedge_warnings: AtomicU64::new(0),
        }
    }

    /// Register the effect in flight and hand it to its lane.
    ///
    /// Table FIRST, send second. A `Done` for an id the table has never seen
    /// is then impossible, and the only failure the order can leave behind is
    /// a table entry for an effect that never shipped, which the send-failure
    /// path below removes.
    ///
    /// `Err` hands the effect back so the caller can run its own inline
    /// fallback. `queue_chain_join`'s must never be skipped because a lost
    /// join owes a hold release.
    pub(crate) fn enqueue(&self, effect: Effect, queue_epoch: u64) -> Result<EffectId, Effect> {
        let lane = effect.lane();
        let id = self.next_id();
        if self.live {
            self.register(id, lane, effect.seqnum());
        }
        let tx = match lane {
            Lane::Select => &self.select_tx,
            Lane::Replay => &self.replay_tx,
            Lane::Join => &self.join_tx,
        };
        match tx.send(Envelope {
            id,
            queue_epoch,
            effect,
        }) {
            Ok(()) => Ok(id),
            Err(mpsc::SendError(envelope)) => {
                self.complete(id);
                Err(envelope.effect)
            }
        }
    }

    /// The next never-reused identity. Split from [`Hands::enqueue`] so the
    /// `loom_` models can allocate one without a GStreamer-bearing effect.
    fn next_id(&self) -> EffectId {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Put an accepted effect in the table. Split out for the same reason as
    /// [`Hands::next_id`].
    fn register(&self, id: EffectId, lane: Lane, seqnum: Option<gst::Seqnum>) {
        self.in_flight.lock().push(InFlight {
            id,
            lane,
            enqueued: Instant::now(),
            seqnum,
            warned: false,
        });
    }

    /// Remove an effect from the table. Returns whether it was there, which
    /// is how a double report would announce itself.
    pub(crate) fn complete(&self, id: EffectId) -> bool {
        let mut table = self.in_flight.lock();
        let Some(index) = table.iter().position(|entry| entry.id == id) else {
            return false;
        };
        table.remove(index);
        true
    }

    /// How many effects are in flight. Diagnostic, read through
    /// `FcastPlaybin::effects_in_flight`.
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.lock().len()
    }

    /// How long a select carrying `seqnum` has been in flight, or `None` if
    /// its `SELECT_STREAMS` has already left the crate.
    ///
    /// THE reason the table exists. Routed pads, engine records and bus
    /// silence all read the same for a refused selection and for one still
    /// waiting on a lane, and those two want opposite answers. The AGE
    /// travels because "not sent yet" only justifies waiting while the lane
    /// might still deliver (`FcastPlaybin::selection_deadline_fired`).
    pub(crate) fn select_age(&self, seqnum: gst::Seqnum, now: Instant) -> Option<Duration> {
        self.in_flight
            .lock()
            .iter()
            .find(|entry| entry.seqnum == Some(seqnum))
            .map(|entry| now.saturating_duration_since(entry.enqueued))
    }

    /// Effects older than `older_than` that have not been reported yet, each
    /// returned ONCE (see [`InFlight::warned`]). Supervision, not
    /// self-rescue. This only makes a wedged lane visible.
    pub(crate) fn wedged(&self, now: Instant, older_than: Duration) -> Vec<Wedged> {
        let mut table = self.in_flight.lock();
        let mut wedged = Vec::new();
        for entry in table.iter_mut() {
            let age = now.saturating_duration_since(entry.enqueued);
            if entry.warned || age < older_than {
                continue;
            }
            entry.warned = true;
            wedged.push(Wedged {
                id: entry.id,
                lane: entry.lane,
                age,
            });
        }
        if !wedged.is_empty() {
            self.wedge_warnings
                .fetch_add(wedged.len() as u64, Ordering::SeqCst);
        }
        wedged
    }

    pub(crate) fn wedge_warnings(&self) -> u64 {
        self.wedge_warnings.load(Ordering::SeqCst)
    }

    /// A lane's report: hand the outcome to the decider, or, when there is
    /// no decider left to take it, retire the entry here and settle what the
    /// outcome still owes ([`Outcome::owed`]).
    ///
    /// `to_decider` answers whether the decider took it. `settle` performs
    /// the undo. Both are the caller's, so a test can drive the decision
    /// without a pipeline.
    pub(crate) fn report(
        &self,
        id: EffectId,
        outcome: Outcome,
        to_decider: impl FnOnce(Outcome) -> bool,
        settle: impl FnOnce(LaneFallback),
    ) {
        // Both taken BEFORE the hand-off. The decider consumes the outcome,
        // and this side still owes an undo and a log line if it does not.
        let owed = outcome.owed();
        let echo = outcome.clone();
        if to_decider(outcome) {
            return;
        }
        warn!(id, outcome = ?echo, "no decider left to report an effect to");
        self.complete(id);
        if let Some(owed) = owed {
            settle(owed);
        }
    }
}

/// Run one envelope's effect, revalidating first, so a superseded select
/// never reaches `run`. Split out of the lane loop so that is testable
/// without a pipeline.
///
/// Only the SELECT lane consults the queue epoch. The other two carry sharper
/// tokens they re-check inside their bodies (the replay's input id + epoch,
/// the join's core identity / still-routed / accepting re-checks under the
/// join gate), and a coarse "some load happened" filter in front of those
/// would only drop work that still owes a hold release.
pub(crate) fn execute<F>(envelope: Envelope, current_epoch: u64, run: F) -> Outcome
where
    F: FnOnce(Effect) -> Outcome,
{
    let Envelope {
        id: _,
        queue_epoch,
        effect,
    } = envelope;
    if queue_epoch != current_epoch
        && let Effect::SelectStreams(job) = &effect
    {
        return Outcome::SelectSkipped {
            seqnum: job.event.seqnum(),
            reason: SKIPPED_STALE_EPOCH,
        };
    }
    let lane = effect.lane();
    let outcome = run(effect);
    debug_assert_eq!(
        outcome.lane(),
        lane,
        "a lane reported another lane's outcome"
    );
    outcome
}

/// Structurally impossible (the enqueue routes by [`Effect::lane`]). Logged
/// rather than asserted because this is the fallback of a fallback.
pub(crate) fn wrong_lane(lane: Lane) {
    warn!(?lane, "an effect reached the wrong hands lane");
    debug_assert!(false, "an effect reached the wrong hands lane");
}

/// The executor's protocol under a model checker. The `#[test]`s below run
/// one schedule each, these run every schedule loom can construct for two
/// threads. They cover the three properties whose failure is silent: a lost
/// `Done` (a permanent wedge warning and a deadline that defers forever), a
/// double `Done` (a hold released twice) and a phantom table entry (both).
///
/// The subject is the REAL [`Hands`], reached through its own methods with
/// the module's primitives swapped for loom's (see [`sync`]). The payload
/// path (GStreamer-bearing effects inside real `send_event` calls) stays the
/// shipping tests' subject. The TABLE protocol is this one's.
///
/// Run:
///
/// ```sh
/// FCAST_LOOM=1 cargo test -p fcastplaybin --lib --release loom_
/// ```
///
/// The filter is not optional. Under `--cfg loom` the crate's ordinary tests
/// would touch loom primitives outside a model and panic. `--release` because
/// an unoptimized model explores the same states several times slower.
#[cfg(all(loom, test))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    /// A `Hands` with three channels nobody reads. The caller holds the
    /// receivers so `enqueue` still succeeds.
    fn hands() -> (Hands, [mpsc::Receiver<Envelope>; 3]) {
        let (select_tx, select_rx) = mpsc::channel();
        let (replay_tx, replay_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();
        (
            Hands::new(true, select_tx, replay_tx, join_tx),
            [select_rx, replay_rx, join_rx],
        )
    }

    fn replay_outcome(sub_id: ExternalSubId) -> Outcome {
        Outcome::ReplaySent {
            sub_id,
            epoch: 0,
            attempt: 0,
            accepted: 1,
            total: 1,
        }
    }

    /// Rule 4, the half a double report would break: an effect is retired
    /// EXACTLY once, whichever thread gets there first. Two reporters for one
    /// id is the ordinary case. The decider retires on `Job::EffectDone`, the
    /// lane retires its own entry when it finds no decider ([`Hands::report`]).
    #[test]
    fn loom_an_effect_is_retired_exactly_once() {
        loom::model(|| {
            let (hands, _rx) = hands();
            let hands = Arc::new(hands);
            let id = hands.next_id();
            hands.register(id, Lane::Replay, None);

            let first = {
                let hands = hands.clone();
                loom::thread::spawn(move || hands.complete(id))
            };
            let second = {
                let hands = hands.clone();
                loom::thread::spawn(move || hands.complete(id))
            };
            let (first, second) = (first.join().unwrap(), second.join().unwrap());

            assert!(
                first ^ second,
                "exactly one of the two reports may retire the effect"
            );
            assert_eq!(hands.in_flight(), 0, "and the table ends empty");
        });
    }

    /// Rule 4, the half a lost enqueue would break: two lanes enqueuing at
    /// once get two identities and two table entries. Concurrent enqueues are
    /// ordinary (the select lane enqueues a join from INSIDE its own send),
    /// and a shared id would make the first `Done` retire both.
    #[test]
    fn loom_concurrent_enqueues_never_share_an_identity() {
        loom::model(|| {
            let (hands, _rx) = hands();
            let hands = Arc::new(hands);

            let select = {
                let hands = hands.clone();
                loom::thread::spawn(move || {
                    let id = hands.next_id();
                    hands.register(id, Lane::Select, None);
                    id
                })
            };
            let join = {
                let hands = hands.clone();
                loom::thread::spawn(move || {
                    let id = hands.next_id();
                    hands.register(id, Lane::Join, None);
                    id
                })
            };
            let (select, join) = (select.join().unwrap(), join.join().unwrap());

            assert_ne!(select, join, "identities are never reused");
            assert_eq!(hands.in_flight(), 2, "both effects are in the table");
            assert!(hands.complete(select));
            assert!(hands.complete(join));
            assert_eq!(hands.in_flight(), 0);
        });
    }

    /// Rule 4's second edge: a shutdown racing a lane's report leaves the
    /// effect either handled by the decider or settled by the lane, never
    /// both and never neither.
    ///
    /// The inbox stands in for the worker's channel with the property that
    /// matters. Closing it and taking what it holds is ONE atomic step, so a
    /// deposit either lands or fails and the lane owes the undo itself. That
    /// is what `mpsc` gives when the receiver drops, and the assumption
    /// `Hands::report` is built on.
    #[test]
    fn loom_a_shutdown_leaves_every_effect_done_or_settled() {
        loom::model(|| {
            let (hands, _rx) = hands();
            let hands = Arc::new(hands);
            let id = hands.next_id();
            hands.register(id, Lane::Replay, None);

            // `Some` = the decider still takes work; `None` = it is gone.
            let inbox: Arc<Mutex<Option<Vec<EffectId>>>> = Arc::new(Mutex::new(Some(Vec::new())));
            let settled = Arc::new(Mutex::new(0usize));

            let lane = {
                let hands = hands.clone();
                let inbox = inbox.clone();
                let settled = settled.clone();
                loom::thread::spawn(move || {
                    hands.report(
                        id,
                        replay_outcome(ExternalSubId(1)),
                        |_outcome| match &mut *inbox.lock() {
                            Some(queue) => {
                                queue.push(id);
                                true
                            }
                            None => false,
                        },
                        |owed| {
                            assert!(matches!(owed, LaneFallback::Replay { .. }));
                            *settled.lock() += 1;
                        },
                    );
                })
            };
            let decider = {
                let hands = hands.clone();
                let inbox = inbox.clone();
                loom::thread::spawn(move || {
                    let taken = inbox.lock().take().unwrap_or_default();
                    let mut handled = 0;
                    for id in taken {
                        if hands.complete(id) {
                            handled += 1;
                        }
                    }
                    handled
                })
            };
            lane.join().unwrap();
            let handled = decider.join().unwrap();

            assert_eq!(
                handled + *settled.lock(),
                1,
                "the effect is either the decider's or the lane's, exactly once"
            );
            assert_eq!(
                hands.in_flight(),
                0,
                "and nothing is left in flight for the tick to call wedged"
            );
        });
    }

    /// The supervision scan is an OBSERVER. A `Done` landing mid-scan must
    /// still retire the effect, with no resurrection, duplicate or
    /// double-count. `Hands::wedged` marks the entries it reports, so it is a
    /// writer racing the retirement. A phantom entry reads to the tick as a
    /// permanently wedged lane and to the deadline as an unsent dispatch.
    #[test]
    fn loom_a_wedge_scan_never_leaves_a_phantom() {
        loom::model(|| {
            let (hands, _rx) = hands();
            let hands = Arc::new(hands);
            let id = hands.next_id();
            hands.register(id, Lane::Select, None);
            let now = Instant::now();

            let tick = {
                let hands = hands.clone();
                loom::thread::spawn(move || {
                    let first = hands.wedged(now + EFFECT_WEDGE_WARN, EFFECT_WEDGE_WARN);
                    // A second scan in the same tick window must find nothing
                    // new whether or not the Done has landed yet.
                    let second = hands.wedged(now + EFFECT_WEDGE_WARN, EFFECT_WEDGE_WARN);
                    assert!(second.is_empty(), "one warning per wedged effect");
                    first.len()
                })
            };
            let done = {
                let hands = hands.clone();
                loom::thread::spawn(move || hands.complete(id))
            };
            let warned = tick.join().unwrap();
            let retired = done.join().unwrap();

            assert!(retired, "the Done always retires the effect it names");
            assert_eq!(hands.in_flight(), 0, "no phantom survives the scan");
            assert!(warned <= 1);
            assert_eq!(
                hands.wedge_warnings(),
                warned as u64,
                "the counter matches what the scan reported"
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select_effect(seqnum: gst::Seqnum) -> Effect {
        let db3 = gst::ElementFactory::make("fakesink")
            .build()
            .expect("a fakesink stands in for decodebin3");
        let event = gst::event::SelectStreams::builder(["audio_0"])
            .seqnum(seqnum)
            .build();
        Effect::SelectStreams(SelectJob {
            db3: db3.clone(),
            target: db3,
            event,
            stream_ids: vec!["audio_0".to_string()],
            text_sid: None,
        })
    }

    fn replay_effect() -> Effect {
        Effect::ReplaySeek(ReplayJob {
            pads: Vec::new(),
            seek: gst::event::Seek::builder(
                1.0,
                gst::SeekFlags::FLUSH,
                gst::SeekType::Set,
                gst::ClockTime::ZERO,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            )
            .build(),
            id: ExternalSubId(1),
            epoch: 0,
            attempt: 0,
            origin: gst::ClockTime::ZERO,
            rate: 1.0,
            pipeline: None,
        })
    }

    fn join_effect() -> Effect {
        let element = gst::ElementFactory::make("fakesink")
            .build()
            .expect("a fakesink stands in for decodebin3");
        let pad = element.static_pad("sink").expect("fakesink has a sink pad");
        Effect::ChainJoin(ChainJoinJob {
            db3: element,
            pad,
            kind: StreamKind::Audio,
            hold: JoinHold::default(),
        })
    }

    fn hands() -> (
        Hands,
        mpsc::Receiver<Envelope>,
        mpsc::Receiver<Envelope>,
        mpsc::Receiver<Envelope>,
    ) {
        let (select_tx, select_rx) = mpsc::channel();
        let (replay_tx, replay_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();
        (
            Hands::new(true, select_tx, replay_tx, join_tx),
            select_rx,
            replay_rx,
            join_rx,
        )
    }

    /// THE executor invariant: one enqueue, one outcome, and a table that ends
    /// empty. The lanes are mocks because the subject is the protocol
    /// (register, route, report, retire), not the effect bodies. Each effect
    /// must reach ITS OWN lane.
    #[test]
    fn every_enqueued_effect_yields_exactly_one_outcome() {
        gst::init().unwrap();
        let (hands, select_rx, replay_rx, join_rx) = hands();
        let seqnum = gst::Seqnum::next();

        assert!(hands.enqueue(select_effect(seqnum), 0).is_ok());
        assert!(hands.enqueue(replay_effect(), 0).is_ok());
        assert!(hands.enqueue(join_effect(), 0).is_ok());
        assert_eq!(hands.in_flight(), 3, "every enqueue registers");
        assert!(
            hands.select_age(seqnum, Instant::now()).is_some(),
            "the select's dispatch must be visible to the deadline"
        );

        // Each mock lane drains its own channel, so a misrouted effect shows
        // up as a missing outcome below.
        let mut outcomes = Vec::new();
        for (rx, lane) in [
            (select_rx, Lane::Select),
            (replay_rx, Lane::Replay),
            (join_rx, Lane::Join),
        ] {
            let envelope = rx.try_recv().expect("the effect reached its lane");
            assert_eq!(envelope.effect.lane(), lane);
            let id = envelope.id;
            let outcome = execute(envelope, 0, |effect| match effect {
                Effect::SelectStreams(job) => Outcome::SelectSent {
                    seqnum: job.event.seqnum(),
                    upstream_ids: None,
                },
                Effect::ReplaySeek(job) => Outcome::ReplaySent {
                    sub_id: job.id,
                    epoch: job.epoch,
                    attempt: job.attempt,
                    accepted: 0,
                    total: 0,
                },
                Effect::ChainJoin(job) => Outcome::JoinFinished { kind: job.kind },
            });
            assert_eq!(outcome.lane(), lane);
            assert!(hands.complete(id), "the worker retires it exactly once");
            assert!(
                !hands.complete(id),
                "a second report for the same id must find nothing"
            );
            outcomes.push(outcome);
            assert!(rx.try_recv().is_err(), "one enqueue, one envelope");
        }

        assert_eq!(outcomes.len(), 3);
        assert_eq!(hands.in_flight(), 0, "the table ends empty");
        assert!(hands.select_age(seqnum, Instant::now()).is_none());
    }

    /// Rule 5: the queue epoch is checked at EXECUTION, and a select a later
    /// load or stop superseded is skipped WITH an outcome, never in silence.
    #[test]
    fn a_stale_epoch_select_is_skipped_and_never_sent() {
        gst::init().unwrap();
        let (hands, select_rx, _replay_rx, _join_rx) = hands();
        let seqnum = gst::Seqnum::next();
        hands.enqueue(select_effect(seqnum), 3).expect("enqueued");
        let envelope = select_rx.try_recv().expect("the effect reached its lane");
        let id = envelope.id;

        // Epoch 4 is what the lane finds. A load bumped it after the enqueue.
        let outcome = execute(envelope, 4, |_| panic!("the send must not happen"));
        match outcome {
            Outcome::SelectSkipped {
                seqnum: got,
                reason,
            } => {
                assert_eq!(got, seqnum, "the skip names the dispatch it strands");
                assert_eq!(reason, SKIPPED_STALE_EPOCH);
            }
            other => panic!("expected a skip, got {other:?}"),
        }
        assert!(hands.complete(id));
    }

    /// The same envelope under an unchanged epoch runs, so the skip above is
    /// the epoch's doing and not the mechanism's.
    #[test]
    fn a_current_epoch_select_runs() {
        gst::init().unwrap();
        let (hands, select_rx, _replay_rx, _join_rx) = hands();
        let seqnum = gst::Seqnum::next();
        hands.enqueue(select_effect(seqnum), 3).expect("enqueued");
        let envelope = select_rx.try_recv().expect("the effect reached its lane");
        let ran = execute(envelope, 3, |_| Outcome::SelectSent {
            seqnum,
            upstream_ids: None,
        });
        assert!(matches!(ran, Outcome::SelectSent { .. }));
    }

    /// A stale epoch is NOT a reason to drop a replay or a join. Both carry
    /// sharper tokens, and a dropped join owes a hold release nobody would
    /// make.
    #[test]
    fn a_stale_epoch_still_runs_the_other_lanes() {
        gst::init().unwrap();
        let (hands, _select_rx, replay_rx, join_rx) = hands();
        hands.enqueue(replay_effect(), 1).expect("enqueued");
        hands.enqueue(join_effect(), 1).expect("enqueued");
        let replay = execute(replay_rx.try_recv().expect("replay"), 9, |effect| {
            let Effect::ReplaySeek(job) = effect else {
                panic!("wrong lane")
            };
            Outcome::ReplaySent {
                sub_id: job.id,
                epoch: job.epoch,
                attempt: job.attempt,
                accepted: 1,
                total: 1,
            }
        });
        assert!(matches!(replay, Outcome::ReplaySent { .. }));
        let join = execute(join_rx.try_recv().expect("join"), 9, |effect| {
            let Effect::ChainJoin(job) = effect else {
                panic!("wrong lane")
            };
            Outcome::JoinFinished { kind: job.kind }
        });
        assert!(matches!(join, Outcome::JoinFinished { .. }));
    }

    /// Under `FCAST_NO_HANDS` the v1 loops report nothing, so nothing may be
    /// registered. An unretirable entry wedges the tick and the deadline.
    #[test]
    fn the_v1_arm_registers_nothing() {
        gst::init().unwrap();
        let (select_tx, select_rx) = mpsc::channel();
        let (replay_tx, _replay_rx) = mpsc::channel();
        let (join_tx, _join_rx) = mpsc::channel();
        let hands = Hands::new(false, select_tx, replay_tx, join_tx);
        let seqnum = gst::Seqnum::next();
        hands.enqueue(select_effect(seqnum), 0).expect("enqueued");
        assert_eq!(hands.in_flight(), 0);
        assert!(hands.select_age(seqnum, Instant::now()).is_none());
        assert!(select_rx.try_recv().is_ok(), "the effect still ships");
    }

    /// A lane that is gone must not leave its effect in the table, and the
    /// caller must get the effect back for its inline fallback.
    #[test]
    fn a_dead_lane_hands_the_effect_back() {
        gst::init().unwrap();
        let (select_tx, select_rx) = mpsc::channel();
        let (replay_tx, replay_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();
        let hands = Hands::new(true, select_tx, replay_tx, join_tx);
        drop(select_rx);
        drop(replay_rx);
        drop(join_rx);
        let returned = hands.enqueue(join_effect(), 0);
        assert!(matches!(returned, Err(Effect::ChainJoin(_))));
        assert_eq!(hands.in_flight(), 0, "no phantom entry survives");
    }

    /// A hold is released exactly once, by whichever of the two parties gets
    /// there first (see [`JoinHold`]). A `gst::PadProbeId` may be spent once,
    /// and both the body and the lane's undo have to be able to spend it.
    #[test]
    fn a_join_hold_is_released_exactly_once() {
        gst::init().unwrap();
        let element = gst::ElementFactory::make("fakesink")
            .build()
            .expect("a fakesink to hold a probe");
        let pad = element.static_pad("sink").expect("fakesink has a sink pad");
        let probe = pad
            .add_probe(gst::PadProbeType::BUFFER, |_, _| gst::PadProbeReturn::Ok)
            .expect("adding the hold probe");
        let hold = JoinHold::new(Some((pad, probe)));
        let copy = hold.clone();

        assert!(hold.release("the body"), "the first release is the one");
        assert!(
            !copy.release("the undo"),
            "a second release must find the slot empty rather than spend the id twice"
        );

        // An empty hold is releasable and reports it did nothing, so the undo
        // path is total.
        assert!(!JoinHold::default().release("nothing held"));
    }

    /// The undo is derived from the effect BEFORE the body consumes it, and
    /// it names what that effect owes.
    #[test]
    fn every_effect_knows_what_it_owes() {
        gst::init().unwrap();
        let seqnum = gst::Seqnum::next();
        assert!(matches!(
            LaneFallback::of(&select_effect(seqnum)),
            LaneFallback::Select { seqnum: owed } if owed == seqnum
        ));
        assert!(matches!(
            LaneFallback::of(&replay_effect()),
            LaneFallback::Replay { id, epoch: 0 } if id == ExternalSubId(1)
        ));
        // The join's undo shares the body's slot rather than copying it.
        let join = join_effect();
        let Effect::ChainJoin(job) = &join else {
            panic!("wrong effect")
        };
        let body = job.hold.clone();
        let LaneFallback::Join { hold } = LaneFallback::of(&join) else {
            panic!("a join owes its hold")
        };
        assert!(!body.release("empty"), "this fixture holds no probe");
        assert!(!hold.release("empty"));
    }

    /// A replay whose report finds no decider settles its own owed hold
    /// release, exactly once, and retires its own entry. The input's data is
    /// blocked at its source pads until somebody lifts it. Driven through the
    /// shipping [`Hands::report`] with the decider answering "no".
    #[test]
    fn a_lost_decider_leaves_the_replay_owing_its_hold_release() {
        gst::init().unwrap();
        let (hands, _select_rx, replay_rx, join_rx) = hands();
        let id = hands.enqueue(replay_effect(), 0).expect("enqueued");
        let envelope = replay_rx.try_recv().expect("the effect reached its lane");
        let outcome = execute(envelope, 0, |effect| {
            let Effect::ReplaySeek(job) = effect else {
                panic!("wrong lane")
            };
            Outcome::ReplaySent {
                sub_id: job.id,
                epoch: job.epoch,
                attempt: job.attempt,
                accepted: 1,
                total: 1,
            }
        });

        let mut settled = Vec::new();
        hands.report(id, outcome, |_| false, |owed| settled.push(owed));
        assert_eq!(
            settled.len(),
            1,
            "the lane settles what it owes exactly once"
        );
        assert!(matches!(
            settled[0],
            LaneFallback::Replay { id, epoch: 0 } if id == ExternalSubId(1)
        ));
        assert_eq!(hands.in_flight(), 0, "the lane retires its own entry");

        // A LIVE decider settles nothing. The tail is its job, and running it
        // on both sides would be the double release.
        let join = hands.enqueue(join_effect(), 0).expect("enqueued");
        let kind = StreamKind::Audio;
        let mut also_settled = Vec::new();
        hands.report(
            join,
            Outcome::JoinFinished { kind },
            |_| true,
            |owed| also_settled.push(owed),
        );
        assert!(also_settled.is_empty());
        assert_eq!(hands.in_flight(), 1, "the decider retires it, not the lane");
        drop(join_rx);
    }

    /// Only the replay owes anything to a lost decider, and it owes the input
    /// the effect was formed for.
    #[test]
    fn a_lost_decider_is_owed_nothing_by_the_other_lanes() {
        gst::init().unwrap();
        let seqnum = gst::Seqnum::next();
        assert!(matches!(
            Outcome::ReplaySent {
                sub_id: ExternalSubId(7),
                epoch: 3,
                attempt: 1,
                accepted: 0,
                total: 2,
            }
            .owed(),
            Some(LaneFallback::Replay { id, epoch: 3 }) if id == ExternalSubId(7)
        ));
        // A select answers an engine the dying crate resets. A join released
        // its hold in the body this follows.
        assert!(
            Outcome::SelectSent {
                seqnum,
                upstream_ids: None,
            }
            .owed()
            .is_none()
        );
        assert!(Outcome::SelectRefused { seqnum }.owed().is_none());
        assert!(
            Outcome::SelectSkipped {
                seqnum,
                reason: SKIPPED_LANE_LOST,
            }
            .owed()
            .is_none()
        );
        assert!(
            Outcome::JoinFinished {
                kind: StreamKind::Video,
            }
            .owed()
            .is_none()
        );
    }

    /// The wedge scan reports each stuck effect once, counts it once, and
    /// leaves it in flight (only a `Done` retires an effect).
    #[test]
    fn the_wedge_scan_warns_once_per_effect() {
        gst::init().unwrap();
        let (hands, _select_rx, _replay_rx, _join_rx) = hands();
        let id = hands.enqueue(join_effect(), 0).expect("enqueued");
        let now = Instant::now();

        assert!(
            hands.wedged(now, EFFECT_WEDGE_WARN).is_empty(),
            "a fresh effect is not wedged"
        );
        let wedged = hands.wedged(now + EFFECT_WEDGE_WARN, EFFECT_WEDGE_WARN);
        assert_eq!(wedged.len(), 1);
        assert_eq!(wedged[0].id, id);
        assert_eq!(wedged[0].lane, Lane::Join);
        assert_eq!(hands.wedge_warnings(), 1);

        assert!(
            hands
                .wedged(now + EFFECT_WEDGE_WARN * 2, EFFECT_WEDGE_WARN)
                .is_empty(),
            "the same effect must not warn on every tick"
        );
        assert_eq!(hands.wedge_warnings(), 1);
        assert_eq!(hands.in_flight(), 1, "a warning is not a retirement");
        assert!(hands.complete(id));
    }
}
