//! The declarative track-selection engine.
//!
//! Callers state what each slot (video/audio/subtitle) SHOULD show
//! ([`TrackTarget`]); the engine owns everything between that intent and a
//! confirmed decodebin3 selection: dispatch serialization, seqnum/content
//! confirmation, re-assertion when decodebin3's own collection-default
//! auto-select stomps an explicit choice, and parking a subtitle request on
//! an external input whose stream has not materialized yet.
//!
//! # Dispatch protocol
//!
//! A `SELECT_STREAMS` is confirmed by a `STREAMS_SELECTED` carrying the
//! event's seqnum (decodebin3 stamps it), or by the reported selection
//! matching what was asked for (a superseded/coalesced/no-op request folds
//! into another event's seqnum). A refresh seek completes with a top-level
//! `ASYNC_DONE`, which CANNOT be seqnum-matched (`GstBin` aggregates with a
//! fresh seqnum), so it settles by exclusivity: at most one async-causing
//! operation is in flight, making the next ASYNC_DONE its completion. New
//! work is held back until the pipeline is quiet, because overlapping
//! re-prerolls deadlock the pipeline.
//!
//! Paused is special (streaming threads are parked after preroll): a
//! dispatched selection won't confirm until data flows, so a parked
//! selection neither blocks a superseding one (no re-preroll to overlap
//! with) nor blocks the refresh flush, which is exactly what makes data
//! flow and the selection apply.
//!
//! # Division of labor
//!
//! The engine is pure state: it never touches the pipeline. Recording happens
//! where the crate translates bus traffic; a dispatch is DECIDED only in
//! [`FcastPlaybin::pump_selection`], called by the OWNER of the transport
//! state machine at its safe points. Deciding and executing are two steps on
//! two threads: the pump records the wait and its deadline under one engine
//! lock, and the crate's deciding worker performs the eager text-branch work
//! and sends the event (`Job::DispatchSelection`).
//!
//! The gate ([`SelectionGate`]) stays caller-provided on purpose: only the
//! transport machine knows about queued seeks and the mid-cascade
//! one-instant-quiet window that a pipeline query alone cannot see (a
//! selection dispatched into the seek dance's quiet instant wedges the
//! pipeline for good).
//!
//! [`FcastPlaybin::pump_selection`]: crate::FcastPlaybin::pump_selection

use std::time::{Duration, Instant};

use tracing::debug;

use crate::{ExternalSubId, decisions::select, routing::StreamKind};

/// Which stream slot a track request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSlot {
    Video,
    Audio,
    Subtitle,
}

/// How many stream slots there are, i.e. the length of every per-slot array.
const SLOTS: usize = 3;

impl TrackSlot {
    /// Every slot, in the order the per-slot arrays are indexed. Iterating
    /// this is what keeps the engine's rules written once per RULE instead of
    /// once per slot; a fourth slot is a compile error until the arrays grow
    /// with it.
    pub(crate) const ALL: [TrackSlot; SLOTS] =
        [TrackSlot::Video, TrackSlot::Audio, TrackSlot::Subtitle];

    /// The collection stream kind this slot selects from.
    pub(crate) fn kind(self) -> StreamKind {
        match self {
            TrackSlot::Video => StreamKind::Video,
            TrackSlot::Audio => StreamKind::Audio,
            TrackSlot::Subtitle => StreamKind::Text,
        }
    }
}

/// `slot as usize` IS the array index, so reordering the variants above breaks
/// the build rather than silently swapping two slots.
const _: () = {
    assert!(TrackSlot::Video as usize == 0);
    assert!(TrackSlot::Audio as usize == 1);
    assert!(TrackSlot::Subtitle as usize == 2);
};

/// What a slot should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackTarget {
    /// A stream from the advertised collection by stream id, `None` = slot
    /// disabled.
    Stream(Option<String>),
    /// Subtitle slot only: an attached external input's stream. Parks until
    /// that stream appears in the advertised collection, then resolves to its
    /// stream id. Cleared automatically if the input fails or is detached.
    ExternalSubtitle(ExternalSubId),
}

/// A full selection, keyed by GStreamer stream id (`None` = slot disabled).
/// Stream ids are stable across collections of the same load, unlike
/// stream-list indices, so a selection never needs remapping.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackSelection {
    pub video: Option<String>,
    pub audio: Option<String>,
    pub subtitle: Option<String>,
}

impl TrackSelection {
    /// The stream id at `slot`. The three fields are the API; this is the
    /// engine's indexed view of the same three.
    fn slot(&self, slot: TrackSlot) -> &Option<String> {
        match slot {
            TrackSlot::Video => &self.video,
            TrackSlot::Audio => &self.audio,
            TrackSlot::Subtitle => &self.subtitle,
        }
    }

    /// No slot names a stream. Dispatching such a selection trips the
    /// GStreamer assertion `gst_event_new_select_streams: streams != NULL`.
    fn is_empty(&self) -> bool {
        TrackSlot::ALL.iter().all(|&slot| self.slot(slot).is_none())
    }
}

/// The transport conditions under which the engine may dispatch, snapshot by
/// the caller (the owner of the transport state machine) right before
/// [`FcastPlaybin::pump_selection`] and read at the DECISION; the execution
/// that follows runs on the worker and re-reads the pipeline for itself.
///
/// [`FcastPlaybin::pump_selection`]: crate::FcastPlaybin::pump_selection
#[derive(Debug, Clone, Copy)]
pub struct SelectionGate {
    /// No async state change in progress and the transport machine is
    /// settled running (not loading/buffering/seeking).
    pub quiet: bool,
    /// Settled paused: streaming threads are parked after preroll, so a
    /// dispatched selection won't apply (or confirm) until data flows again.
    pub paused: bool,
    /// Whether the media is known seekable. The re-emit flush is a seek, so
    /// an unseekable stream drops it rather than dispatching it to fail.
    pub seekable: bool,
}

/// One advertised stream of the current collection, as the engine sees it
/// (streams of kinds the engine doesn't select are simply not listed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollectionStream {
    pub(crate) sid: String,
    pub(crate) kind: StreamKind,
}

/// Everything the pump needs beyond the engine's own state, assembled by
/// `FcastPlaybin` from the gate and the routing table.
#[derive(Debug, Clone)]
pub(crate) struct PumpCtx {
    pub(crate) gate: SelectionGate,
    /// An external subtitle input is attached: the re-emit flush races the
    /// external inputs' reconfiguration and can freeze the play item, so
    /// the pump neither schedules nor dispatches one while this holds.
    /// (Externals need no engine flush anyway: the crate replays the
    /// input itself whenever its stream joins the overlay.)
    pub(crate) externals_attached: bool,
    /// Stream ids each attached external input has produced so far, for
    /// resolving [`TrackTarget::ExternalSubtitle`].
    pub(crate) externals: Vec<(ExternalSubId, Vec<String>)>,
    /// An adaptive demuxer owns stream selection, so decodebin3 posts NO
    /// `STREAMS_SELECTED` of its own: the only confirmations a caller sees in
    /// this mode are the ones this crate produces
    /// ([`Command::ConfirmApplied`]).
    pub(crate) upstream_owns: bool,
    /// The clock at this pump, for the bounded subtitle holdback. The engine
    /// reads no clock of its own (same rule as the deadline advisories, which
    /// arrive as absolute due times).
    pub(crate) now: Instant,
}

/// What the pump decided to dispatch next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    SelectStreams(TrackSelection),
    RefreshSeek,
    /// A USER REQUEST that is already satisfied: nothing to dispatch, and in
    /// upstream-selection mode nothing else will ever tell the caller so. The
    /// answer is the same `StreamsSelected` a dispatch would have produced,
    /// naming the applied set including the crate-merged subtitle sid.
    ///
    /// Fires once per user request, never for a `dirty` set by a collection
    /// change, a foreign report or engine-internal reseeding.
    ConfirmApplied(TrackSelection),
}

/// How many advisory deadlines the engine tracks, one per [`DeadlineKind`].
const DEADLINE_KINDS: usize = 2;

/// Which in-flight wait an advisory belongs to, and its index into
/// [`SelectionEngine::deadlines`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlineKind {
    Selection = 0,
    Refresh = 1,
}

impl DeadlineKind {
    /// Every kind, in the order [`SelectionEngine::due_deadlines`] reports
    /// fires. A new kind is a compile error until the array grows with it.
    const ALL: [DeadlineKind; DEADLINE_KINDS] = [DeadlineKind::Selection, DeadlineKind::Refresh];

    fn fire(self, seqnum: gst::Seqnum) -> DeadlineFire {
        match self {
            DeadlineKind::Selection => DeadlineFire::Selection(seqnum),
            DeadlineKind::Refresh => DeadlineFire::Refresh(seqnum),
        }
    }
}

/// The advisory deadline for one in-flight operation (see
/// [`SelectionEngine::deadlines`]).
#[derive(Debug, Clone, Copy)]
struct Advisory {
    /// The dispatch this deadline belongs to. An advisory whose seqnum no
    /// longer names the live wait is dead - the whole invalidation mechanism.
    seqnum: gst::Seqnum,
    due: Instant,
    /// How many more times this target may be re-dispatched before the crate
    /// gives up and reports what is really playing. Carried on the advisory
    /// so a retry chain counts down across dispatches. A refresh has no retry
    /// budget (its fire is terminal) and always stores 0.
    retries_left: u32,
}

/// A deadline that has come due (see [`SelectionEngine::due_deadlines`]). The
/// engine only ever says WHICH wait ran out; deciding what to do about it
/// needs the pipeline, and belongs to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadlineFire {
    Selection(gst::Seqnum),
    Refresh(gst::Seqnum),
}

/// What [`SelectionEngine::selection_timed_out`] decided about a dispatch the
/// worker has established will never confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TimeoutOutcome {
    /// Nothing is in flight under that seqnum any more: the confirmation raced
    /// the fire (the healthy and commonest case).
    NotInFlight,
    /// Re-dispatch `target` under a FRESH seqnum, arming its deadline with
    /// `retries_left`.
    Retry {
        target: TrackSelection,
        retries_left: u32,
    },
    /// Out of retries. The caller probes what is actually playing and reports
    /// that through [`SelectionEngine::selection_gave_up`].
    Exhausted { target: TrackSelection },
}

/// What a slot is explicitly asked to show, as every reader sees it.
///
/// A borrowed VIEW rather than the stored shape; [`Desires`] says why the
/// storage is split and this is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desire<'a> {
    /// This stream (`None` = slot off), re-asserted if a fresh collection's
    /// auto-select stomps it.
    Stream(&'a Option<String>),
    /// An external input's stream once advertised. SUBTITLE SLOT ONLY.
    External(ExternalSubId),
}

/// The subtitle slot's stored desire, the only cell an external can park on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TextDesire {
    Stream(Option<String>),
    External(ExternalSubId),
}

/// The engine's explicit desires, indexed by [`TrackSlot`]. A cell is `None`
/// while its slot is UNSET: no explicit request yet, follow the pipeline's
/// own defaults.
///
/// TYPE STORY. An external subtitle desire names an attached input's TEXT
/// stream, so the subtitle slot is the only one that can hold one. Three
/// cells of one wide enum would leave `(Video, External)` spellable and every
/// reader carrying a can't-happen arm; splitting the storage makes that state
/// unrepresentable rather than merely unreachable, and the refusal then lives
/// exactly once, in [`Self::set`], where the public `TrackTarget` crosses into
/// the engine. [`Self::get`] hands both cells back through one [`Desire`]
/// view, so readers still write each rule once and index by slot.
#[derive(Debug, Default)]
struct Desires {
    /// Video and audio, indexed by `slot as usize`.
    av: [Option<Option<String>>; 2],
    text: Option<TextDesire>,
}

impl Desires {
    /// The desire at `slot`, `None` while it is unset.
    fn get(&self, slot: TrackSlot) -> Option<Desire<'_>> {
        match slot {
            TrackSlot::Subtitle => Some(match self.text.as_ref()? {
                TextDesire::Stream(sid) => Desire::Stream(sid),
                TextDesire::External(id) => Desire::External(*id),
            }),
            _ => self.av[slot as usize].as_ref().map(Desire::Stream),
        }
    }

    /// State `slot`'s desire, latest wins. `false` says the target was
    /// refused: `TrackTarget::ExternalSubtitle` is meaningless on an A/V slot
    /// and no A/V cell can store it.
    fn set(&mut self, slot: TrackSlot, target: TrackTarget) -> bool {
        match (slot, target) {
            (TrackSlot::Subtitle, TrackTarget::Stream(sid)) => {
                self.text = Some(TextDesire::Stream(sid))
            }
            (TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id)) => {
                self.text = Some(TextDesire::External(id))
            }
            (_, TrackTarget::Stream(sid)) => self.av[slot as usize] = Some(sid),
            (slot, target) => {
                debug!(?slot, ?target, "ignoring external target on an A/V slot");
                return false;
            }
        }
        true
    }

    /// Back to UNSET (follow the pipeline), which is never the same as an
    /// explicit off.
    fn clear(&mut self, slot: TrackSlot) {
        match slot {
            TrackSlot::Subtitle => self.text = None,
            _ => self.av[slot as usize] = None,
        }
    }

    /// Whether `slot` is explicitly asked OFF.
    fn is_off(&self, slot: TrackSlot) -> bool {
        matches!(self.get(slot), Some(Desire::Stream(None)))
    }

    /// The external input the subtitle slot is parked on, if any.
    fn external(&self) -> Option<ExternalSubId> {
        match self.text {
            Some(TextDesire::External(id)) => Some(id),
            _ => None,
        }
    }
}

/// How far this load's `STREAMS_SELECTED` reports have got.
///
/// A LADDER: each rung is entered once and never left until a reset, and the
/// two questions the seeding guard in [`SelectionEngine::collection_changed`]
/// asks are rung comparisons. Fusing the two bools this replaces makes their
/// impossible fourth combination (text known without any report)
/// unrepresentable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum ReportProgress {
    /// No `STREAMS_SELECTED` of this load has been adopted. decodebin3's
    /// auto-select is still ahead of us and covers whatever the collection
    /// ends up holding.
    #[default]
    Unreported,
    /// decodebin3 has COMMITTED to a selection, but no text stream existed
    /// for that report to speak about, so an empty applied text slot is still
    /// ignorance awaiting the auto-select rather than a decision.
    Committed,
    /// A report that COULD speak about the text slot was adopted. From then
    /// on an empty applied text slot is decodebin3's REAL state and
    /// `collection_changed` must not seed it.
    TextKnown,
}

/// The engine's come-back-later flags, one bit each.
///
/// One field rather than four bools: [`SelectionEngine::unconverged`] - the
/// liveness question [`crate::Inner::run_tick`] asks - becomes a single test
/// that no future flag can be accidentally left out of, and the states that
/// matter are read as one mask instead of four field loads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Pending(u8);

impl Pending {
    /// The desired state may diverge from the applied one: set by requests,
    /// collection changes and overtaking foreign selections; cleared when the
    /// pump converges or dispatches. Dispatching ONLY on fresh events keeps
    /// the engine convergent (a refused selection cannot ping-pong).
    const DIRTY: u8 = 1 << 0;
    /// A re-emit flush is due once the pipeline settles: a sparse text track
    /// renders no cue after a switch until the next cue boundary, so a
    /// flushing seek to the current position re-emits it. Safety is
    /// re-decided from the ctx at every pump.
    const REFRESH: u8 = 1 << 1;
    /// A USER REQUEST has not been answered to the caller yet. Set only by
    /// [`SelectionEngine::request`], consumed by the pump when it dispatches
    /// or finds the request already satisfied ([`Command::ConfirmApplied`]).
    /// Deliberately survives every None-returning gate: a request made while
    /// the transport is not quiet is answered at the first pump that can, not
    /// dropped.
    const REQUEST: u8 = 1 << 2;
    /// The last pump could not resolve the subtitle desire because its
    /// external input has not produced its text stream yet. That state
    /// reaches the engine only through the pump's `PumpCtx`, so NO event
    /// marks the moment it becomes resolvable and `DIRTY` alone would strand
    /// the desire; this makes the next pump reconsider.
    const EXTERNAL: u8 = 1 << 3;

    fn has(self, bits: u8) -> bool {
        self.0 & bits != 0
    }

    /// Whether ANY flag is set. The engine's half of the liveness question.
    fn any(self) -> bool {
        self.0 != 0
    }

    fn set(&mut self, bits: u8) {
        self.0 |= bits;
    }

    fn clear(&mut self, bits: u8) {
        self.0 &= !bits;
    }

    fn set_to(&mut self, bits: u8, on: bool) {
        if on {
            self.set(bits);
        } else {
            self.clear(bits);
        }
    }

    /// Clear `bits` and answer whether any of them was set (`mem::take` over
    /// a flag).
    fn take(&mut self, bits: u8) -> bool {
        let was = self.has(bits);
        self.clear(bits);
        was
    }
}

/// How many superseded dispatches the engine remembers.
///
/// A record is pushed only where a LIVE `selecting` is displaced, and
/// [`SelectionEngine::pump`] refuses to dispatch over a live wait unless the
/// transport is settled PAUSED, so the set grows along exactly two paths: the
/// deadline retry ladder, bounded by its own budget at
/// `SELECTION_DEADLINE_RETRIES + 1` = 3 rungs before the give-up ends the
/// chain, and one record per paused user switch that lands between two
/// confirmations. Eight holds a full ladder with five paused switches on top.
/// Every confirmation, every collection change and every load reset empties
/// the set.
const SUPERSEDED_CAP: usize = 8;

/// Dispatches superseded before confirming (the paused supersede path),
/// oldest first, stored inline.
///
/// A fixed array rather than a `Vec`: there is one engine, the set is empty
/// almost always, and the only removal is a PREFIX drain, which a plain array
/// plus a length does in one pass with no allocator behind it.
#[derive(Debug, Default)]
struct SupersededSet {
    slots: [Option<(gst::Seqnum, TrackSelection)>; SUPERSEDED_CAP],
    len: usize,
}

impl SupersededSet {
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        for slot in &mut self.slots[..self.len] {
            *slot = None;
        }
        self.len = 0;
    }

    /// Record a superseded dispatch.
    ///
    /// OVERFLOW POLICY: a full set forgets its OLDEST record. `Vec` could not
    /// overflow, so this is the one behaviour with no predecessor, and it is
    /// the safe direction. The set is drained head-first anyway, so the
    /// oldest record is the one already nearest to being dropped, and the
    /// property the drain exists for - a stale record must not swallow a
    /// later foreign selection - can only be broken by keeping records too
    /// long, never by dropping one. If a forgotten record's echo does arrive,
    /// it reads as a foreign selection: `applied` adopts it and the desire is
    /// re-asserted, which is convergent and is exactly what the engine does
    /// with any report it does not recognize.
    fn push(&mut self, entry: (gst::Seqnum, TrackSelection)) {
        if self.len == SUPERSEDED_CAP {
            debug!(
                cap = SUPERSEDED_CAP,
                "the superseded record set is full; forgetting its oldest record"
            );
            self.drain_through(0);
        }
        self.slots[self.len] = Some(entry);
        self.len += 1;
    }

    /// Where the record `seqnum`/`reported` confirms sits, if any.
    fn position(&self, seqnum: gst::Seqnum, reported: &TrackSelection) -> Option<usize> {
        self.slots[..self.len].iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|(sn, sel)| *sn == seqnum || sel == reported)
        })
    }

    /// Drop records `..=pos`, keeping the rest in order.
    fn drain_through(&mut self, pos: usize) {
        debug_assert!(
            pos < self.len,
            "draining past the end of the superseded set"
        );
        let dropped = pos + 1;
        for i in 0..self.len {
            let moved = if i + dropped < self.len {
                self.slots[i + dropped].take()
            } else {
                None
            };
            self.slots[i] = moved;
        }
        self.len -= dropped;
    }
}

#[derive(Debug)]
pub(crate) struct SelectionEngine {
    /// Explicit desires, indexed by [`TrackSlot`]. Unset = follow the
    /// pipeline; reset per load.
    desired: Desires,
    /// The selection currently applied (or optimistically in flight): adopted
    /// verbatim from `STREAMS_SELECTED`, filtered + default-seeded per
    /// collection, set at dispatch so a second change arriving before
    /// confirmation composes instead of reverting.
    applied: TrackSelection,
    /// The advertised collection (empty before the first one of a load).
    collection: Vec<CollectionStream>,
    /// In-flight `SELECT_STREAMS`: the seqnum its `STREAMS_SELECTED` will
    /// carry and the selection asked of decodebin3. Settles on an exact seqnum
    /// match OR on a report matching this selection; a report matching NEITHER
    /// is decodebin3's own auto-select racing ours, and marks the desire dirty
    /// for re-dispatch.
    selecting: Option<(gst::Seqnum, TrackSelection)>,
    /// What [`Self::applied`] held before the in-flight dispatch overwrote it,
    /// so a REFUSED dispatch can put it back.
    ///
    /// The optimism in `applied` is load-bearing for composition; what was
    /// missing is its counterpart. A refused `send_event` means the selection
    /// NEVER LEFT, so leaving the optimistic value makes the engine believe a
    /// track is on that upstream has never heard of - and desired == applied
    /// then means there is nothing left to re-dispatch. Captured on the FIRST
    /// dispatch of a chain and held across supersedes, so a refusal rolls back
    /// to the last state upstream actually confirmed rather than to another
    /// guess.
    applied_before_dispatch: Option<TrackSelection>,
    /// Dispatches superseded before confirming (the paused supersede path),
    /// oldest first. Their late confirmations are our own stale echoes, so
    /// they must neither settle the live request nor masquerade as an
    /// overtaking foreign selection (see `take_superseded_echo`).
    superseded: SupersededSet,
    /// In-flight refresh seek, settled by the next `ASYNC_DONE`
    /// (attribution by exclusivity, see the module docs).
    refreshing: Option<gst::Seqnum>,
    /// Everything the engine still owes somebody, one bit per reason (see
    /// [`Pending`]). Read as a whole by [`Self::unconverged`].
    pending: Pending,
    /// How far decodebin3's reports of this load have got, i.e. what an empty
    /// applied text slot MEANS. See the seeding guard in
    /// [`Self::collection_changed`].
    report: ReportProgress,
    /// Advisory deadlines for the in-flight waits, indexed by
    /// [`DeadlineKind`]: the selection one is armed by the pump right after
    /// [`Self::selection_dispatched`], the refresh one right after
    /// [`Self::refresh_dispatched`].
    ///
    /// TRUTH stays `selecting` / `refreshing`. An advisory whose seqnum no
    /// longer names its live wait is dead, and [`Self::due_deadlines`] drops
    /// it lazily, so no clear path (`streams_selected`, `dispatch_failed`,
    /// `collection_changed`, `reset`) has to know deadlines exist and no
    /// drift between a wait and its deadline is representable.
    deadlines: [Option<Advisory>; DEADLINE_KINDS],
    /// When the subtitle holdback first deferred a resolution, i.e. how long
    /// the desire has been waiting for the collection to announce video.
    /// `Some` is the ONE deferral no event is guaranteed to lift, so it keeps
    /// the engine unconverged (the pump is poked while it is) and it bounds
    /// itself with [`select::SUBTITLE_HOLDBACK_GRACE`]. Cleared by every
    /// resolve that does not hold back.
    ///
    /// The fifth come-back-later reason, outside [`Pending`] because it is
    /// the only one carrying a payload: the bit alone cannot answer when the
    /// grace runs out.
    subtitle_held_since: Option<Instant>,
}

impl Default for SelectionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectionEngine {
    pub(crate) fn new() -> Self {
        Self {
            desired: Desires::default(),
            applied: TrackSelection::default(),
            collection: Vec::new(),
            selecting: None,
            applied_before_dispatch: None,
            superseded: SupersededSet::default(),
            refreshing: None,
            pending: Pending::default(),
            report: ReportProgress::Unreported,
            deadlines: [None; DEADLINE_KINDS],
            subtitle_held_since: None,
        }
    }

    /// A new load: everything desired/applied/in-flight belonged to the
    /// previous item.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// A GAPLESS activation: the ITEM changed but the USER's intent did not.
    ///
    /// Everything applied/in-flight belonged to the retired item and is dropped
    /// exactly like [`Self::reset`], and so are the stream-id desires, since a
    /// sid names a stream of the item that just ended. An explicit slot DISABLE
    /// is item-INDEPENDENT and MUST survive: a queue transition is the crate's
    /// own decision, not a new user request, and nothing re-applies the intent
    /// afterwards. Resetting it turns subtitles the user switched OFF back on
    /// at every gapless boundary (`collection_changed` seeds the text slot with
    /// the new collection's default and the pump dispatches it).
    pub(crate) fn reset_across_gapless(&mut self) {
        let off = TrackSlot::ALL.map(|slot| self.desired.is_off(slot));
        *self = Self::new();
        for (slot, off) in TrackSlot::ALL.into_iter().zip(off) {
            if off {
                self.desired.set(slot, TrackTarget::Stream(None));
            }
        }
        // The carried desires must be reconciled against the incoming item's
        // collection. Explicit here so the intent does not depend on
        // `collection_changed` (called right after) marking dirty anyway.
        self.pending.set_to(Pending::DIRTY, off.contains(&true));
    }

    /// State a slot's desired target (latest wins). `TrackTarget::
    /// ExternalSubtitle` is only meaningful for the subtitle slot and is
    /// ignored on the others.
    pub(crate) fn request(&mut self, slot: TrackSlot, target: TrackTarget) {
        if self.desired.set(slot, target) {
            self.pending.set(Pending::DIRTY | Pending::REQUEST);
        }
    }

    /// A new stream collection arrived. Reconcile: drop applied sids whose
    /// stream left, seed still-empty slots with the collection defaults
    /// (the first stream of each kind, mirroring decodebin3's own
    /// auto-select) so a change dispatched before the initial
    /// `STREAMS_SELECTED` keeps the other streams selected. Any in-flight
    /// confirmation targeted the previous collection and may never confirm,
    /// so abandon it deterministically.
    pub(crate) fn collection_changed(&mut self, collection: Vec<CollectionStream>) {
        debug!(?collection, report = ?self.report, "collection changed");
        // The kinds the collection carried BEFORE this update, for the seeding
        // guard below.
        let previous = std::mem::replace(&mut self.collection, collection);
        // A slot the desire explicitly disables must NOT be seeded with the
        // collection default: the pipeline honors the disable across
        // collection re-posts, so seeding would fabricate an applied state the
        // pipeline is not in, and the pump would dispatch a no-op decodebin3
        // never confirms, starving every later change.
        //
        // The text slot needs one more guard: decodebin3 auto-selects text
        // only until it has seen an explicit `SELECT_STREAMS`, so once a
        // selection of this load was adopted that could speak about text, an
        // EMPTY text slot is its real state and seeding it makes the pump
        // treat a re-enable as already applied. A report made while the
        // collection held no text stream left the slot empty out of ignorance
        // and must still be seeded (the `Committed` rung). A POPULATED slot
        // whose stream left the collection still falls back to the default.
        // A/V get the same guard, stated over the COLLECTION rather than over
        // the report. Seeding only mirrors decodebin3's own auto-select while
        // that auto-select could have SEEN the kind, and with
        // `parse-streams=true` the merged collection arrives in increments, so
        // a report can predate half of it. Recording a new kind's default
        // anyway makes the pump find `target == applied` and dispatch nothing,
        // so no chain is built for the rest and the load never leaves ASYNC
        // (`huge_collection.toml`, half of `empty_text_stream.toml`). Before
        // the first report the seed is harmless, decodebin3 has not decided
        // either.
        let guard_new_kinds = self.report >= ReportProgress::Committed;
        let carried = |kind: StreamKind| {
            !guard_new_kinds || previous.iter().any(|stream| stream.kind == kind)
        };
        let text_was_applied = self.applied.subtitle.is_some();
        let [video, audio, subtitle] = TrackSlot::ALL.map(|slot| {
            // The text guard is stated over the REPORT ladder, the A/V ones
            // over the COLLECTION; a populated slot falls back either way.
            let seen = match slot {
                TrackSlot::Subtitle => text_was_applied || self.report < ReportProgress::TextKnown,
                _ => carried(slot.kind()),
            };
            let current = self.applied.slot(slot).clone();
            self.seed_slot(slot.kind(), current, seen && !self.desired.is_off(slot))
        });
        self.applied = TrackSelection {
            video,
            audio,
            subtitle,
        };

        self.selecting = None;
        self.superseded.clear();
        self.refreshing = None;
        // A new collection re-seeds `applied` above, so any pre-dispatch
        // snapshot describes a graph that no longer exists.
        self.applied_before_dispatch = None;
        // The new collection can change what the desire resolves to (an
        // external materialized, an explicit sid appeared/left) and
        // decodebin3 will re-run its own auto-select for it, so converge.
        self.pending.set(Pending::DIRTY);
    }

    /// The collection's default for a slot: keep `current` when its stream is
    /// still advertised, else (`allow_default`) the first stream of the kind,
    /// mirroring decodebin3's own auto-select. `allow_default` is false for a
    /// slot the desire explicitly disables (see `collection_changed`).
    fn seed_slot(
        &self,
        kind: StreamKind,
        current: Option<String>,
        allow_default: bool,
    ) -> Option<String> {
        current
            .filter(|sid| self.knows_stream(sid))
            .or_else(|| allow_default.then(|| self.default_of(kind)).flatten())
    }

    /// The collection's own default for a kind: its first stream, mirroring
    /// decodebin3's auto-select.
    fn default_of(&self, kind: StreamKind) -> Option<String> {
        self.collection
            .iter()
            .find(|s| s.kind == kind)
            .map(|s| s.sid.clone())
    }

    /// Whether the collection advertises `sid` as a stream of `kind`. An
    /// external input can advertise more than its text stream, so the subtitle
    /// slot resolves against the KIND rather than against mere membership.
    fn advertises(&self, sid: &str, kind: StreamKind) -> bool {
        self.collection
            .iter()
            .any(|s| s.sid == sid && s.kind == kind)
    }

    /// Whether the collection carries any stream of `kind` at all.
    fn advertises_kind(&self, kind: StreamKind) -> bool {
        self.collection.iter().any(|s| s.kind == kind)
    }

    /// Whether the DESIRED subtitle is this external, whatever `applied`
    /// says. They diverge when decodebin3 retracts the external's stream
    /// (slot destroyed on side-input EOS): the re-dispatch applies
    /// subtitle-None while the caller still wants the external.
    pub(crate) fn desires_external(&self, id: ExternalSubId) -> bool {
        self.desired.external() == Some(id)
    }

    /// Whether `sid` is in the advertised collection. The refusal gate for a
    /// caller-supplied selection: decodebin3 ignores a `SELECT_STREAMS`
    /// naming an unknown id wholesale and never confirms it, so an unknown
    /// id must be refused up front rather than queued into silence.
    pub(crate) fn knows_stream(&self, sid: &str) -> bool {
        self.collection.iter().any(|stream| stream.sid == sid)
    }

    /// The stream ids of the VIDEO streams in the advertised collection, in
    /// collection order.
    ///
    /// An empty result means kinds are unknowable (nothing advertised yet:
    /// before a load's first collection, and between a gapless reset and the
    /// incoming item's collection), so it never counts as "no video". See
    /// [`crate::decisions::deselects_video`], which states the same rule.
    ///
    /// The one source of these ids. `RoutingState` used to mirror them off the
    /// same bus message, which put a routing-lock hold in front of every
    /// reader.
    pub(crate) fn video_ids(&self) -> Vec<String> {
        self.collection
            .iter()
            .filter(|stream| stream.kind == StreamKind::Video)
            .map(|stream| stream.sid.clone())
            .collect()
    }

    /// An `ExternalSubtitleFailed` fired or the input was detached: a desire
    /// parked on it would otherwise park forever. Resets to UNSET (whatever is
    /// showing keeps showing), not to "off".
    pub(crate) fn external_gone(&mut self, id: ExternalSubId) {
        if self.desires_external(id) {
            debug!(?id, "dropping the subtitle desire for a failed external");
            self.desired.clear(TrackSlot::Subtitle);
            self.pending.set(Pending::DIRTY);
        }
    }

    /// A `STREAMS_SELECTED` arrived reporting `reported` as the now-active
    /// selection. Adopt it as applied, settle the in-flight selection when
    /// it is ours (by the stamped seqnum, or by content when the seqnum was
    /// lost to superseding/coalescing/a no-op), and mark the desire dirty
    /// when the report is an overtaking foreign selection (decodebin3's
    /// collection-default auto-select landing after ours stomps it, and the
    /// pump re-asserts). Re-assertion converges: each re-dispatch needs a
    /// fresh non-matching `STREAMS_SELECTED` to fire again, and decodebin3
    /// auto-selects at most once per collection.
    pub(crate) fn streams_selected(&mut self, seqnum: gst::Seqnum, reported: &TrackSelection) {
        // decodebin3 has committed to a selection, whatever it names. It
        // speaks about the text slot only when a text stream was there to be
        // reported: decodebin3 merges one input's collection at a time, so a
        // report made before the text input joined leaves the slot empty out
        // of ignorance, not out of a decision. The rungs only ever go up (a
        // later, poorer report cannot un-know the text state).
        let rung = if reported.subtitle.is_some() || self.advertises_kind(StreamKind::Text) {
            ReportProgress::TextKnown
        } else {
            ReportProgress::Committed
        };
        self.report = self.report.max(rung);

        // A report is upstream's own word, so `applied` is set from evidence
        // rather than optimism on every arm that adopts it: nothing left to
        // roll back to. The ONE arm that adopts nothing (the superseded echo
        // restoring a still-live wait) re-points the anchor at the report
        // instead of clearing it, since that wait is still optimistic.
        match self.selecting.take() {
            None => {
                // Nothing in flight, so no optimistic `applied` is anchored on
                // the snapshot whatever this report turns out to be.
                self.applied_before_dispatch = None;
                // The overtake path below abandons the live wait without
                // clearing the superseded records, so an echo of ours can
                // land here with nothing in flight.
                if self.take_superseded_echo(seqnum, reported) {
                    return;
                }
                self.applied = reported.clone();
                // Nothing in flight: this is decodebin3 selecting on its
                // own (a fresh collection's auto-select). If an explicit
                // desire diverges, re-assert it.
                if self.diverges_from_desired(reported) {
                    debug!(
                        ?reported,
                        "foreign selection diverges from the desired state"
                    );
                    self.pending.set(Pending::DIRTY);
                }
            }
            Some((expected, desired_sel)) => {
                if expected == seqnum || &desired_sel == reported {
                    self.applied_before_dispatch = None;
                    self.applied = reported.clone();
                    self.superseded.clear();
                    // Ours settled. Anything the report still diverges on
                    // (decodebin3 adjusting the request) is deliberately NOT
                    // re-asserted: without a fresh event it would ping-pong.
                    return;
                }
                if self.take_superseded_echo(seqnum, reported) {
                    // A superseded dispatch's late confirmation. It is ours
                    // but stale, and the live request's own confirmation is
                    // still en route, so it must not be adopted as `applied`.
                    //
                    // It IS upstream's latest word though, so it becomes the
                    // ROLLBACK ANCHOR of the still-optimistic live wait.
                    // Wiping the anchor here left a refusal of that wait with
                    // nothing to put back (`applied` kept the refused target);
                    // keeping the pre-chain snapshot would put back a state
                    // this very report says upstream has left.
                    self.applied_before_dispatch = Some(reported.clone());
                    self.selecting = Some((expected, desired_sel));
                    return;
                }
                self.applied_before_dispatch = None;
                self.applied = reported.clone();
                debug!(?desired_sel, ?reported, "selection overtaken, re-asserting");
                self.pending.set(Pending::DIRTY);
            }
        }
    }

    /// Whether `reported` is the late confirmation of a dispatch that was
    /// superseded before it confirmed, by seqnum or by content (the seqnum
    /// is lost when decodebin3 coalesces or no-ops a request).
    ///
    /// Such an echo must NOT be adopted as applied: the superseding dispatch
    /// is the newer truth, and rewinding `applied` re-arms `subtitle_sid` with
    /// the very subtitle that dispatch is turning off.
    ///
    /// Matching drains the record and every older one - decodebin3 confirms in
    /// dispatch order, so a record still unmatched behind this one never will
    /// be - which keeps a stale record from swallowing a genuinely foreign
    /// selection naming the same streams much later.
    fn take_superseded_echo(&mut self, seqnum: gst::Seqnum, reported: &TrackSelection) -> bool {
        let Some(pos) = self.superseded.position(seqnum, reported) else {
            return false;
        };
        self.superseded.drain_through(pos);
        true
    }

    /// A top-level `ASYNC_DONE` arrived. Returns whether it finished our
    /// refresh seek (attribution by exclusivity, see the module docs).
    pub(crate) fn refresh_done(&mut self) -> bool {
        self.refreshing.take().is_some()
    }

    /// Whether the dispatch stamped `seqnum` is STILL awaiting its
    /// confirmation. The upstream-selection split's bounded fallback asks this
    /// before manufacturing one, so a confirmation that arrived normally is
    /// never duplicated.
    pub(crate) fn selection_in_flight(&self, seqnum: gst::Seqnum) -> bool {
        self.selecting
            .as_ref()
            .is_some_and(|(tracked, _)| *tracked == seqnum)
    }

    /// The seqnum of the dispatch currently awaiting confirmation, if any.
    ///
    /// [`Self::selection_in_flight`] answers "is THIS one still in flight",
    /// which needs a seqnum to ask about. The subtitle reconcile pass has no
    /// seqnum - it is not following a particular dispatch, it is deciding
    /// whether ANY selection is mid-flight before it acts on a graph that
    /// selection is about to change. Same field, the other question.
    pub(crate) fn selecting_seqnum(&self) -> Option<gst::Seqnum> {
        self.selecting.as_ref().map(|(seqnum, _)| *seqnum)
    }

    /// Whether the engine has moved on from the refresh `seqnum` names, making
    /// this job a superseded flushing seek that must not be performed. `None`
    /// in flight is not supersession: a caller may force a refresh through
    /// `FcastPlaybin::refresh_seek_async` without the engine tracking it.
    pub(crate) fn refresh_superseded(&self, seqnum: gst::Seqnum) -> bool {
        self.refreshing.is_some_and(|tracked| tracked != seqnum)
    }

    /// The refresh-seek job reported failure for `seqnum`.
    pub(crate) fn refresh_failed(&mut self, seqnum: gst::Seqnum) {
        if self.refreshing == Some(seqnum) {
            self.refreshing = None;
        }
    }

    /// A user-initiated flushing seek re-emits the current cue by itself,
    /// so a separately queued refresh flush would be redundant.
    pub(crate) fn cancel_refresh(&mut self) {
        self.pending.clear(Pending::REFRESH);
    }

    /// The subtitle stream the applied (or in-flight) selection shows; only
    /// this stream may join the overlay. `None` means off, possibly still
    /// draining, when `Inner::poll_text_policy` must not relink it.
    pub(crate) fn subtitle_sid(&self) -> Option<String> {
        self.applied.subtitle.clone()
    }

    /// Whether the DESIRED subtitle state is an explicit off.
    ///
    /// [`Self::subtitle_sid`] cannot answer this: it reads `applied`, adopted
    /// verbatim from `STREAMS_SELECTED`, so decodebin3's auto-select stomping
    /// an explicit subtitle-off makes it name a stream the caller turned off.
    /// The corrective re-assert only dispatches from a QUIET pump, and
    /// `Inner::poll_text_policy` must not join the stomped stream to the
    /// overlay in that window.
    pub(crate) fn subtitle_explicitly_off(&self) -> bool {
        self.desired.is_off(TrackSlot::Subtitle)
    }

    /// Record a dispatched `SELECT_STREAMS` (the pump's caller sent it
    /// stamped with `seqnum`). The target becomes the optimistic applied
    /// selection so later requests compose with it instead of reverting it.
    pub(crate) fn selection_dispatched(&mut self, seqnum: gst::Seqnum, target: TrackSelection) {
        if let Some(old) = self.selecting.replace((seqnum, target.clone())) {
            self.superseded.push(old);
        }
        // Only the FIRST of a supersede chain: rolling back to a superseded
        // dispatch's optimistic value would just be a different guess.
        if self.applied_before_dispatch.is_none() {
            self.applied_before_dispatch = Some(self.applied.clone());
        }
        self.applied = target;
    }

    /// The dispatch could not be sent (no core, selection refused, a skip on
    /// the select lane): there is no completion to wait for, and a refresh
    /// scheduled for the switch must not fire as an orphan flush.
    ///
    /// EVERYTHING here is guarded by the seqnum, the flush included: a failure
    /// report for a dispatch this engine is not waiting on says nothing about
    /// the one it IS waiting on, and clearing the flush unconditionally
    /// cancels the newer switch's re-emit. Late failures for already-replaced
    /// seqnums are routine around loads and gapless swaps (the hands' skip
    /// outcomes: a superseded core, a stale queue epoch).
    pub(crate) fn dispatch_failed(&mut self, seqnum: gst::Seqnum) {
        if !self.selection_in_flight(seqnum) {
            return;
        }
        self.selecting = None;
        self.pending.clear(Pending::REFRESH);
        // A refusal is not a skip: the event did not leave, so upstream's
        // selection is whatever it was and the optimistic `applied` is simply
        // false. Leaving it makes desired == applied and the engine converges
        // on a state that exists nowhere but in this struct. Reverting keeps
        // the desire divergent, so the next fresh event dispatches it again.
        // The ordering gate in `pump` is what stops that re-dispatch meeting
        // the same flush; this is the half that makes asking again possible at
        // all.
        if let Some(previous) = self.applied_before_dispatch.take() {
            debug!(
                ?previous,
                "rolling back a refused selection's optimistic applied"
            );
            self.applied = previous;
        }
        // A SUPERSEDE CHAIN: an older sibling really left, so the rollback
        // above puts back a state upstream has already moved past, and that
        // sibling's own late confirmation is drained as a stale echo rather
        // than re-asserting anything. Without this the engine rests with
        // `applied` diverged from what is playing, `dirty` false and no
        // deadline. Re-dirtying keeps the desire dispatchable; it costs at
        // most one re-dispatch, which the chain's own confirmations settle.
        if !self.superseded.is_empty() {
            debug!("a refusal inside a supersede chain; re-asserting the desire");
            self.pending.set(Pending::DIRTY);
        }
    }

    pub(crate) fn refresh_dispatched(&mut self, seqnum: gst::Seqnum) {
        self.refreshing = Some(seqnum);
    }

    /// The advisory armed for `kind`, if any.
    fn advisory(&self, kind: DeadlineKind) -> Option<Advisory> {
        self.deadlines[kind as usize]
    }

    /// Whether the wait `kind` speaks for is still waiting on `seqnum`. The
    /// ONE thing that keeps an advisory alive.
    fn wait_lives(&self, kind: DeadlineKind, seqnum: gst::Seqnum) -> bool {
        match kind {
            DeadlineKind::Selection => self.selection_in_flight(seqnum),
            DeadlineKind::Refresh => self.refresh_in_flight(seqnum),
        }
    }

    /// Arm the advisory deadline for the dispatch just recorded (see
    /// [`Self::deadlines`]). Time arrives as an absolute `due`: the engine is
    /// pure state and its tests inject the clock. A no-op unless `seqnum` IS
    /// the live wait.
    pub(crate) fn arm_selection_deadline(
        &mut self,
        seqnum: gst::Seqnum,
        due: Instant,
        retries_left: u32,
    ) {
        self.arm(
            DeadlineKind::Selection,
            Advisory {
                seqnum,
                due,
                retries_left,
            },
        );
    }

    /// Arm the advisory deadline for the refresh seek just dispatched. Same
    /// contract as [`Self::arm_selection_deadline`]; a refresh fire is
    /// terminal, so it carries no retry budget.
    pub(crate) fn arm_refresh_deadline(&mut self, seqnum: gst::Seqnum, due: Instant) {
        self.arm(
            DeadlineKind::Refresh,
            Advisory {
                seqnum,
                due,
                retries_left: 0,
            },
        );
    }

    fn arm(&mut self, kind: DeadlineKind, advisory: Advisory) {
        if !self.wait_lives(kind, advisory.seqnum) {
            return;
        }
        self.deadlines[kind as usize] = Some(advisory);
    }

    /// Which waits have run out at `now`, and re-arm each fire `rearm` into
    /// the future.
    ///
    /// The only place advisories are invalidated: one whose seqnum no longer
    /// names the live wait is dropped here, so a confirmation, a failed
    /// dispatch, a fresh collection and a reset all disarm by doing nothing.
    /// RE-ARMING rather than clearing bounds the fire rate to one per family
    /// per `rearm` period, without abandoning a wait that is still there.
    pub(crate) fn due_deadlines(&mut self, now: Instant, rearm: Duration) -> Vec<DeadlineFire> {
        let mut fires = Vec::new();
        for kind in DeadlineKind::ALL {
            let Some(advisory) = self.advisory(kind) else {
                continue;
            };
            if !self.wait_lives(kind, advisory.seqnum) {
                self.deadlines[kind as usize] = None;
            } else if advisory.due <= now {
                fires.push(kind.fire(advisory.seqnum));
                self.deadlines[kind as usize] = Some(Advisory {
                    due: now + rearm,
                    ..advisory
                });
            }
        }
        fires
    }

    /// The selection the dispatch stamped `seqnum` asked for, if it is STILL
    /// in flight. `None` says the confirmation arrived after all, which is
    /// how a deadline racing a healthy switch ends.
    pub(crate) fn selecting_target(&self, seqnum: gst::Seqnum) -> Option<TrackSelection> {
        self.selecting
            .as_ref()
            .filter(|(tracked, _)| *tracked == seqnum)
            .map(|(_, target)| target.clone())
    }

    /// Whether the refresh seek stamped `seqnum` is still awaiting the
    /// `ASYNC_DONE` that settles it (sibling of
    /// [`Self::selection_in_flight`]).
    pub(crate) fn refresh_in_flight(&self, seqnum: gst::Seqnum) -> bool {
        self.refreshing == Some(seqnum)
    }

    /// Whether ANY dispatch is awaiting confirmation. Asked by a worker that
    /// timed a dispatch out, released the lock and is about to re-assert it:
    /// a caller pump can have dispatched something newer in between, and that
    /// newer wait owns the engine.
    pub(crate) fn selection_pending(&self) -> bool {
        self.selecting.is_some()
    }

    /// The worker established that the dispatch stamped `seqnum` will never
    /// be confirmed, and asks what to do about it.
    ///
    /// The timed-out entry moves into `superseded`, which is load-bearing: a
    /// real confirmation turning up late is then recognized as ours-but-stale
    /// and neither settles the retry that replaced it nor rewinds `applied`
    /// (see [`Self::take_superseded_echo`]).
    ///
    /// Retries are counted on the advisory, so a chain of re-dispatches counts
    /// down instead of restarting; a timeout with no advisory exhausts at once.
    pub(crate) fn selection_timed_out(&mut self, seqnum: gst::Seqnum) -> TimeoutOutcome {
        if !self.selection_in_flight(seqnum) {
            return TimeoutOutcome::NotInFlight;
        }
        let retries_left = match self.advisory(DeadlineKind::Selection) {
            Some(advisory) if advisory.seqnum == seqnum => {
                self.deadlines[DeadlineKind::Selection as usize] = None;
                advisory.retries_left
            }
            // Unreachable through the tick, the only caller. A future caller
            // timing a dispatch out on OTHER evidence must bring its own
            // retry budget rather than land here and silently get none.
            _ => {
                debug_assert!(
                    false,
                    "a selection timed out with no advisory to account against"
                );
                0
            }
        };
        let entry = self.selecting.take().expect("in flight, checked above");
        let target = entry.1.clone();
        self.superseded.push(entry);
        if retries_left == 0 {
            debug!(
                ?target,
                "a dispatched selection timed out with no retries left"
            );
            return TimeoutOutcome::Exhausted { target };
        }
        debug!(?target, retries_left, "a dispatched selection timed out");
        TimeoutOutcome::Retry {
            target,
            retries_left: retries_left - 1,
        }
    }

    /// The retries are exhausted: `actual` is what the pipeline is really
    /// playing, probed rather than reported. Adopt it and CONVERGE, so the
    /// engine has nothing left to ping-pong about and the next real request
    /// still dispatches.
    ///
    /// Every desire that diverges from `actual` is neutralized to UNSET rather
    /// than rewritten or turned off ([`Self::external_gone`]'s rule: a request
    /// that could not be honoured must not tear down what the user had).
    ///
    /// Returns whether the give-up was ADOPTED. `false` says a dispatch
    /// overtook it inside the window where the worker released the engine
    /// lock; the caller must then stay quiet as well as leave the engine
    /// alone, or it reports a reality the newer dispatch already left.
    pub(crate) fn selection_gave_up(
        &mut self,
        seqnum: gst::Seqnum,
        actual: &TrackSelection,
    ) -> bool {
        if let Some((tracked, _)) = &self.selecting
            && *tracked != seqnum
        {
            debug!(
                ?seqnum,
                "a newer dispatch overtook the give-up; leaving the engine to it"
            );
            return false;
        }
        debug!(
            ?actual,
            "giving up on a selection; adopting what is playing"
        );
        self.applied = actual.clone();
        for slot in TrackSlot::ALL {
            if self.slot_diverges(slot, actual) {
                self.desired.clear(slot);
            }
        }
        self.deadlines[DeadlineKind::Selection as usize] = None;
        // Every come-back-later flag, spelled out rather than blanket-cleared:
        // the give-up owes an answer for exactly these four (the REQUEST one
        // because the give-up REPORTS the applied selection itself, so leaving
        // it set would answer a later, unrelated pump), and a future fifth
        // reason must be decided on its own merits rather than swept here.
        // Deliberately untouched, same as before: the refresh advisory,
        // `refreshing`, the superseded records and the subtitle holdback.
        self.pending
            .clear(Pending::DIRTY | Pending::REFRESH | Pending::REQUEST | Pending::EXTERNAL);
        true
    }

    #[cfg(test)]
    pub(crate) fn has_dispatchable_work(&self) -> bool {
        self.pending.has(Pending::DIRTY | Pending::REFRESH)
    }

    /// Whether the engine still owes an answer: a desire not yet dispatched, a
    /// dispatch not yet confirmed, a refresh in flight or pending, or a user
    /// request nobody has replied to.
    ///
    /// `SelectionEngine::has_dispatchable_work` is the narrower sibling (what
    /// the pump would ACT on right now); this is the one a liveness question
    /// wants. A pure field read: [`crate::Inner::run_tick`] asks it, and the
    /// tick may hold no second lock and touch no GStreamer object.
    ///
    /// FALSE AT REST is the property that matters: every field below is reset
    /// by [`Self::reset`] and [`Self::reset_across_gapless`], so a stopped
    /// crate answers `false` and keeps answering it.
    pub(crate) fn unconverged(&self) -> bool {
        self.pending.any()
            || self.subtitle_held_since.is_some()
            || self.selecting.is_some()
            || self.refreshing.is_some()
    }

    /// The applied (or optimistically in-flight) selection.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> &TrackSelection {
        &self.applied
    }

    /// Whether `slot`'s explicit desire disagrees with a reported selection.
    /// An UNSET slot follows the pipeline and never disagrees.
    ///
    /// An external desire cannot be compared here: resolving it needs the
    /// externals map, which only the pump has. "Never diverges" would lose the
    /// re-assertion once the engine has converged and decodebin3 auto-selects
    /// the embedded default over it (no collection change follows to
    /// re-dirty). Deferring to the pump is convergent: it dispatches only when
    /// the resolution really differs, and each re-assertion needs a fresh
    /// foreign report. [`Self::selection_gave_up`] reads it the same way: the
    /// give-up is the last moment to keep insisting on an external.
    fn slot_diverges(&self, slot: TrackSlot, reported: &TrackSelection) -> bool {
        match self.desired.get(slot) {
            None => false,
            Some(Desire::Stream(want)) => want != reported.slot(slot),
            Some(Desire::External(_)) => true,
        }
    }

    /// Whether ANY explicit desire disagrees with a reported selection.
    fn diverges_from_desired(&self, reported: &TrackSelection) -> bool {
        TrackSlot::ALL
            .iter()
            .any(|&slot| self.slot_diverges(slot, reported))
    }

    /// Whether the subtitle desire is parked on an external input that has not
    /// produced an advertised TEXT stream yet - the one resolution input that
    /// arrives outside the engine's own event stream. The collection change
    /// re-dirties the engine, but only the pump's `PumpCtx` says whether the
    /// INPUT produced the id, and the two need not arrive in that order.
    fn external_desire_unresolved(&self, ctx: &PumpCtx) -> bool {
        let Some(id) = self.desired.external() else {
            return false;
        };
        self.external_text_sid(id, ctx).is_none()
    }

    /// The TEXT stream external input `id` has advertised, if it has. Every
    /// entry carrying the id is scanned, so "is it resolvable" and "what does
    /// it resolve to" are answered by ONE walk and cannot disagree.
    fn external_text_sid(&self, id: ExternalSubId, ctx: &PumpCtx) -> Option<String> {
        ctx.externals
            .iter()
            .filter(|(eid, _)| *eid == id)
            .find_map(|(_, sids)| {
                sids.iter()
                    .find(|sid| self.advertises(sid, StreamKind::Text))
            })
            .cloned()
    }

    /// Whether every EXPLICIT desire can be honoured against the current
    /// collection, i.e. whether [`Self::resolve`] answers with what was asked
    /// for rather than with a fallback.
    ///
    /// `resolve` deliberately falls back to the applied stream for a desire it
    /// cannot honour yet (an external not yet materialized, a sid the
    /// collection does not carry) so the pipeline keeps showing something.
    /// That fallback must NEVER be mistaken for a satisfied REQUEST, or a
    /// select-true attach gets answered with the embedded track the item was
    /// already showing. An unresolvable desire leaves the request armed, and
    /// the dispatch that follows materialization answers it.
    ///
    /// An explicit OFF (`Some(None)`) is always honourable; an UNSET slot is
    /// nobody's request.
    fn desires_resolvable(&self, ctx: &PumpCtx) -> bool {
        TrackSlot::ALL
            .into_iter()
            .all(|slot| match self.desired.get(slot) {
                // The text slot demands the KIND, not mere membership: an
                // external input advertises more than its text stream, and an
                // A/V sid answering a subtitle REQUEST would be nonsense.
                Some(Desire::Stream(Some(sid))) if slot == TrackSlot::Subtitle => {
                    self.advertises(sid, StreamKind::Text)
                }
                Some(Desire::Stream(Some(sid))) => self.knows_stream(sid),
                Some(Desire::External(_)) => !self.external_desire_unresolved(ctx),
                _ => true,
            })
    }

    /// The stream `slot` should show, resolved against the current collection.
    ///
    /// An explicit stream that left the collection cannot be selected: fall
    /// back to what is applied rather than fabricating a disable.
    ///
    /// Omitting a kind from a `SELECT_STREAMS` asks decodebin3 to turn it OFF,
    /// so an UNSET A/V slot with nothing applied must resolve to the
    /// collection default rather than to nothing. `applied` alone is not a
    /// reliable stand-in: decodebin3 reports its selection while its merged
    /// collection is still growing, so an early report can name audio alone
    /// while the video stream sits right there in the collection.
    ///
    /// The TEXT slot takes no such default: seeding it would turn subtitles on
    /// by itself, which is the one thing an unset slot must never do.
    fn resolve_slot(&self, slot: TrackSlot, ctx: &PumpCtx) -> Option<String> {
        let default_ok = slot != TrackSlot::Subtitle;
        let fallback = || {
            self.applied
                .slot(slot)
                .clone()
                .filter(|sid| self.knows_stream(sid))
                .or_else(|| default_ok.then(|| self.default_of(slot.kind())).flatten())
        };
        match self.desired.get(slot) {
            None => fallback(),
            Some(Desire::Stream(None)) => None,
            Some(Desire::Stream(Some(sid))) if self.knows_stream(sid) => Some(sid.clone()),
            Some(Desire::Stream(Some(_))) => fallback(),
            // Not materialized yet: keep showing what shows now. The
            // collection change re-dirties when it appears.
            Some(Desire::External(id)) => self.external_text_sid(id, ctx).or_else(fallback),
        }
    }

    /// Resolve the desired state against the current collection into the
    /// concrete selection to dispatch. `None` when nothing is dispatchable
    /// (no collection yet, or the resolution is the empty selection, which
    /// decodebin3 asserts on).
    fn resolve(&mut self, ctx: &PumpCtx) -> Option<TrackSelection> {
        // Re-armed below only if the holdback defers again, so every other
        // outcome (including the ones returning early) ends the wait.
        let held_since = self.subtitle_held_since.take();
        if self.collection.is_empty() {
            return None;
        }
        let [video, audio, subtitle] = TrackSlot::ALL.map(|slot| self.resolve_slot(slot, ctx));
        let mut selection = TrackSelection {
            video,
            audio,
            subtitle,
        };

        // A text stream cannot be presented without a video stream, so
        // deselecting video implicitly deselects subtitles - but only when the
        // missing video really is a deselect (see
        // [`decisions::select::subtitle_holdback`]).
        if selection.video.is_none() && selection.subtitle.is_some() {
            let since = held_since.unwrap_or(ctx.now);
            let facts = select::HoldbackFacts {
                collection_has_video: self.advertises_kind(StreamKind::Video),
                video_explicitly_off: self.desired.is_off(TrackSlot::Video),
                only_the_subtitle_moves: selection.video == self.applied.video
                    && selection.audio == self.applied.audio,
                held_for: ctx.now.saturating_duration_since(since),
            };
            match select::subtitle_holdback(facts) {
                select::SubtitleHoldback::Defer => {
                    self.subtitle_held_since = Some(since);
                    debug!(
                        collection = ?self.collection,
                        "holding the subtitle selection back until the collection announces video"
                    );
                    return None;
                }
                select::SubtitleHoldback::KeepSubtitle => {
                    if facts.only_the_subtitle_moves {
                        debug!(
                            collection = ?self.collection,
                            "the collection never announced video; dispatching the held subtitle"
                        );
                    }
                    debug!(
                        collection = ?self.collection,
                        "keeping the subtitle in a selection the collection cannot give video to"
                    );
                }
                select::SubtitleHoldback::DropSubtitle => {
                    debug!(
                        desired_video = ?self.desired.get(TrackSlot::Video),
                        applied_video = ?self.applied.video,
                        collection = ?self.collection,
                        "dropping the subtitle stream from a selection without video"
                    );
                    selection.subtitle = None;
                }
            }
        }

        // Never produce an empty selection: decodebin3 asserts on it.
        if selection.is_empty() {
            debug!("refusing to resolve to an empty stream selection");
            return None;
        }
        Some(selection)
    }

    /// Decide the next operation to dispatch, if the transport allows one.
    pub(crate) fn pump(&mut self, ctx: &PumpCtx) -> Option<Command> {
        let transport = select::RefreshTransport {
            upstream_owns: ctx.upstream_owns,
            externals_attached: ctx.externals_attached,
            seekable: ctx.gate.seekable,
        };
        // A scheduled re-emit flush's safety is re-decided at every pump, so a
        // flush scheduled before an external attached is dropped rather than
        // sent (see [`decisions::select::refresh_still_safe`]).
        if self.pending.has(Pending::REFRESH) && !select::refresh_still_safe(transport) {
            self.pending.clear(Pending::REFRESH);
        }
        if !ctx.gate.quiet {
            return None;
        }
        // A refresh flush is an async re-preroll: never dispatch on top of it.
        if self.refreshing.is_some() {
            return None;
        }
        // An unconfirmed selection blocks new work while data flows (its
        // reconfigure may still re-preroll). While paused it is merely parked,
        // so SUPERSEDING it is safe - a later SELECT_STREAMS simply replaces
        // it. Flushing past it is not; see the refresh gate at the bottom.
        if self.selecting.is_some() && !ctx.gate.paused {
            return None;
        }

        // Retry a resolution deferred on an external input ONLY at the moment
        // that input's stream shows up: dispatching only on fresh events is
        // what keeps a selection decodebin3 adjusts from ping-ponging.
        let unresolved_external = self.external_desire_unresolved(ctx);
        let external_arrived = self.pending.has(Pending::EXTERNAL) && !unresolved_external;
        self.pending.set_to(Pending::EXTERNAL, unresolved_external);

        // The subtitle holdback deferred a resolution and nothing is bound to
        // re-dirty it, so the pump has to come back to it on its own until the
        // grace runs out (see `resolve`).
        let holding_back = self.subtitle_held_since.is_some();

        // Consumed whether or not the branch runs, exactly as before: a pump
        // that looks at the desire has looked at it.
        let dirty = self.pending.take(Pending::DIRTY);
        if dirty || external_arrived || holding_back {
            // ONE resolution per pump: `resolve` ends the holdback wait it
            // owns, so asking twice would answer the second question against
            // state the first one changed.
            let resolved = self.resolve(ctx);
            // An already-satisfied USER request still has to be answered, and
            // only this crate can in upstream-selection mode. The flag is
            // taken either way: in db3-owned mode decodebin3 owns that channel,
            // and leaving it set would answer a later, unrelated pump.
            // `desires_resolvable` FIRST: an unresolvable desire must not even
            // consume the flag, or the request is lost.
            if let Some(target) = &resolved
                && *target == self.applied
                && self.desires_resolvable(ctx)
                && self.pending.take(Pending::REQUEST)
                && ctx.upstream_owns
            {
                debug!(
                    ?target,
                    "a user request is already satisfied; confirming it locally"
                );
                return Some(Command::ConfirmApplied(target.clone()));
            }
            if let Some(target) = resolved
                && target != self.applied
            {
                // The dispatch's own confirmation answers the request.
                self.pending.clear(Pending::REQUEST);
                self.pending.set_to(
                    Pending::REFRESH,
                    select::schedule_refresh(&self.applied, &target, transport),
                );
                return Some(Command::SelectStreams(target));
            }
        }

        // THE ORDERING CONTRACT: a refresh flush must not be dispatched while a
        // selection dispatch is unconfirmed.
        //
        // The paused exemption above lets a pump run past an in-flight
        // selection, which is right for SUPERSEDING it and wrong for flushing
        // past it. A `SELECT_STREAMS` is sent from the select lane, not from
        // here, so "in flight" includes "still being sent"; the refresh is a
        // FLUSHING seek on the whole pipeline, and a flush landing mid-send
        // makes that `send_event` return false. urisourcebin then refuses the
        // event, `Inner::send_select_streams` reports `SelectRefused`, and the
        // stream is never selected upstream at all - while
        // `selection_dispatched` has already set `applied` optimistically, so
        // the crate builds and links a text branch that no data will ever
        // reach. That is the field's "subtitles never show on a first mid-play
        // enable, but appear after a seek" (the seek only helps because a flush
        // makes decodebin3 re-report, and the divergence path re-asserts).
        //
        // DEFERRED, not dropped: `Pending::REFRESH` stays set, so the first pump
        // after the confirmation (or the refusal, or the deadline advisory's
        // timeout - all three clear `selecting`) dispatches it. The re-emit is
        // late by one pump, which is invisible; the alternative is a track that
        // never plays. Its safety is re-decided at the top of every pump
        // anyway, so a deferral can only ever make the flush MORE conservative.
        if self.pending.has(Pending::REFRESH) && self.selecting.is_some() {
            debug!("deferring the re-emit flush behind an unconfirmed selection");
            return None;
        }

        if self.pending.take(Pending::REFRESH) {
            return Some(Command::RefreshSeek);
        }

        None
    }
}

#[cfg(test)]
mod tests;
