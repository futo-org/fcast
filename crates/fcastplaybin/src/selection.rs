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

use crate::{ExternalSubId, routing::StreamKind};

/// Which stream slot a track request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSlot {
    Video,
    Audio,
    Subtitle,
}

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

/// How long the subtitle holdback waits for the collection to announce video
/// before dispatching anyway (see [`SelectionEngine::resolve`]). Long enough
/// to cover decodebin3 merging its inputs one collection at a time, short
/// enough that a media which never announces video still answers the request.
const SUBTITLE_HOLDBACK_GRACE: Duration = Duration::from_secs(1);

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

/// The advisory deadline for the in-flight `SELECT_STREAMS` (see
/// [`SelectionEngine::selection_deadline`]).
#[derive(Debug, Clone, Copy)]
struct SelectionDeadline {
    /// The dispatch this deadline belongs to. An advisory whose seqnum no
    /// longer names the live wait is dead - the whole invalidation mechanism.
    seqnum: gst::Seqnum,
    due: Instant,
    /// How many more times this target may be re-dispatched before the crate
    /// gives up and reports what is really playing. Carried on the advisory
    /// so a retry chain counts down across dispatches.
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

/// The subtitle slot's explicit desire (video/audio desires are plain
/// streams, so they are `Option<Option<String>>`: outer `None` = no
/// explicit request yet, follow the pipeline's own defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubtitleDesire {
    /// This stream (`None` = slot off), re-asserted if a fresh collection's
    /// auto-select stomps it.
    Stream(Option<String>),
    /// An external input's stream once advertised.
    External(ExternalSubId),
}

#[derive(Debug, Default)]
pub(crate) struct SelectionEngine {
    /// Explicit desires per slot. Outer `None` = unset (follow the
    /// pipeline), reset per load.
    desired_video: Option<Option<String>>,
    desired_audio: Option<Option<String>>,
    desired_subtitle: Option<SubtitleDesire>,
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
    superseded: Vec<(gst::Seqnum, TrackSelection)>,
    /// In-flight refresh seek, settled by the next `ASYNC_DONE`
    /// (attribution by exclusivity, see the module docs).
    refreshing: Option<gst::Seqnum>,
    /// A re-emit flush is due once the pipeline settles: a sparse text track
    /// renders no cue after a switch until the next cue boundary, so a
    /// flushing seek to the current position re-emits it. Safety is
    /// re-decided from the ctx at every pump.
    refresh_wanted: bool,
    /// The desired state may diverge from the applied one: set by requests,
    /// collection changes and overtaking foreign selections; cleared when the
    /// pump converges or dispatches. Dispatching ONLY on fresh events keeps
    /// the engine convergent (a refused selection cannot ping-pong).
    dirty: bool,
    /// A USER REQUEST has not been answered to the caller yet. Set only by
    /// [`Self::request`], consumed by the pump when it dispatches or finds the
    /// request already satisfied ([`Command::ConfirmApplied`]). Deliberately
    /// survives every None-returning gate: a request made while the transport
    /// is not quiet is answered at the first pump that can, not dropped.
    unanswered_request: bool,
    /// The last pump could not resolve the subtitle desire because its
    /// external input has not produced its text stream yet. That state reaches
    /// the engine only through the pump's `PumpCtx`, so NO event marks the
    /// moment it becomes resolvable and `dirty` alone would strand the desire;
    /// this makes the next pump reconsider.
    awaiting_external: bool,
    /// A `STREAMS_SELECTED` of this load has been adopted AND it could speak
    /// about the text slot. From then on an empty applied text slot is
    /// decodebin3's REAL state, not ignorance awaiting the auto-select, and
    /// `collection_changed` must not seed it.
    text_state_known: bool,
    /// decodebin3 has COMMITTED to a selection, i.e. some `STREAMS_SELECTED` of
    /// this load was adopted. Before that its auto-select is still ahead of us
    /// and covers whatever the collection ends up holding. See the seeding
    /// guard in [`Self::collection_changed`].
    reported_once: bool,
    /// Advisory deadline for the in-flight dispatch, armed by the pump right
    /// after [`Self::selection_dispatched`].
    ///
    /// TRUTH stays `selecting`. An advisory whose seqnum no longer names the
    /// live wait is dead, and [`Self::due_deadlines`] drops it lazily, so no
    /// clear path (`streams_selected`, `dispatch_failed`, `collection_changed`,
    /// `reset`) has to know deadlines exist and no drift between the wait and
    /// its deadline is representable.
    selection_deadline: Option<SelectionDeadline>,
    /// The same, for the in-flight refresh seek: its seqnum and when it runs
    /// out. Validated against `refreshing` the same lazy way.
    refresh_deadline: Option<(gst::Seqnum, Instant)>,
    /// When the subtitle holdback first deferred a resolution, i.e. how long
    /// the desire has been waiting for the collection to announce video.
    /// `Some` is the ONE deferral no event is guaranteed to lift, so it keeps
    /// the engine unconverged (the pump is poked while it is) and it bounds
    /// itself with [`SUBTITLE_HOLDBACK_GRACE`]. Cleared by every resolve that
    /// does not hold back.
    subtitle_held_since: Option<Instant>,
}

impl SelectionEngine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A new load: everything desired/applied/in-flight belonged to the
    /// previous item.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
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
        let subtitle_off = self.desired_subtitle == Some(SubtitleDesire::Stream(None));
        let video_off = self.desired_video == Some(None);
        let audio_off = self.desired_audio == Some(None);
        *self = Self::default();
        if subtitle_off {
            self.desired_subtitle = Some(SubtitleDesire::Stream(None));
        }
        if video_off {
            self.desired_video = Some(None);
        }
        if audio_off {
            self.desired_audio = Some(None);
        }
        // The carried desires must be reconciled against the incoming item's
        // collection. Explicit here so the intent does not depend on
        // `collection_changed` (called right after) marking dirty anyway.
        self.dirty = subtitle_off || video_off || audio_off;
    }

    /// State a slot's desired target (latest wins). `TrackTarget::
    /// ExternalSubtitle` is only meaningful for the subtitle slot and is
    /// ignored on the others.
    pub(crate) fn request(&mut self, slot: TrackSlot, target: TrackTarget) {
        match (slot, target) {
            (TrackSlot::Video, TrackTarget::Stream(sid)) => self.desired_video = Some(sid),
            (TrackSlot::Audio, TrackTarget::Stream(sid)) => self.desired_audio = Some(sid),
            (TrackSlot::Subtitle, TrackTarget::Stream(sid)) => {
                self.desired_subtitle = Some(SubtitleDesire::Stream(sid))
            }
            (TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id)) => {
                self.desired_subtitle = Some(SubtitleDesire::External(id))
            }
            (slot, target) => {
                debug!(?slot, ?target, "ignoring external target on an A/V slot");
                return;
            }
        }
        self.dirty = true;
        self.unanswered_request = true;
    }

    /// A new stream collection arrived. Reconcile: drop applied sids whose
    /// stream left, seed still-empty slots with the collection defaults
    /// (the first stream of each kind, mirroring decodebin3's own
    /// auto-select) so a change dispatched before the initial
    /// `STREAMS_SELECTED` keeps the other streams selected. Any in-flight
    /// confirmation targeted the previous collection and may never confirm,
    /// so abandon it deterministically.
    pub(crate) fn collection_changed(&mut self, collection: Vec<CollectionStream>) {
        debug!(
            ?collection,
            text_state_known = self.text_state_known,
            "collection changed"
        );
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
        // and must still be seeded (see `text_state_known`). A POPULATED slot
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
        // Lever: `FCAST_NO_NEW_KIND_RESEED_GUARD`.
        let guard_new_kinds =
            self.reported_once && std::env::var_os("FCAST_NO_NEW_KIND_RESEED_GUARD").is_none();
        let carried = |kind: StreamKind| {
            !guard_new_kinds || previous.iter().any(|stream| stream.kind == kind)
        };
        let text_was_applied = self.applied.subtitle.is_some();
        self.applied.video = Self::seed_slot(
            &self.collection,
            StreamKind::Video,
            self.applied.video.take(),
            carried(StreamKind::Video) && self.desired_video != Some(None),
        );
        self.applied.audio = Self::seed_slot(
            &self.collection,
            StreamKind::Audio,
            self.applied.audio.take(),
            carried(StreamKind::Audio) && self.desired_audio != Some(None),
        );
        self.applied.subtitle = Self::seed_slot(
            &self.collection,
            StreamKind::Text,
            self.applied.subtitle.take(),
            (text_was_applied || !self.text_state_known)
                && self.desired_subtitle != Some(SubtitleDesire::Stream(None)),
        );

        self.selecting = None;
        self.superseded.clear();
        self.refreshing = None;
        // A new collection re-seeds `applied` above, so any pre-dispatch
        // snapshot describes a graph that no longer exists.
        self.applied_before_dispatch = None;
        // The new collection can change what the desire resolves to (an
        // external materialized, an explicit sid appeared/left) and
        // decodebin3 will re-run its own auto-select for it, so converge.
        self.dirty = true;
    }

    /// The collection's default for a slot: keep `current` when its stream is
    /// still advertised, else (`allow_default`) the first stream of the kind,
    /// mirroring decodebin3's own auto-select. `allow_default` is false for a
    /// slot the desire explicitly disables (see `collection_changed`).
    fn seed_slot(
        collection: &[CollectionStream],
        kind: StreamKind,
        current: Option<String>,
        allow_default: bool,
    ) -> Option<String> {
        current
            .filter(|sid| collection.iter().any(|s| &s.sid == sid))
            .or_else(|| {
                if !allow_default {
                    return None;
                }
                collection
                    .iter()
                    .find(|s| s.kind == kind)
                    .map(|s| s.sid.clone())
            })
    }

    /// Whether the DESIRED subtitle is this external, whatever `applied`
    /// says. They diverge when decodebin3 retracts the external's stream
    /// (slot destroyed on side-input EOS): the re-dispatch applies
    /// subtitle-None while the caller still wants the external.
    pub(crate) fn desires_external(&self, id: ExternalSubId) -> bool {
        self.desired_subtitle == Some(SubtitleDesire::External(id))
    }

    /// Whether `sid` is in the advertised collection. The refusal gate for a
    /// caller-supplied selection: decodebin3 ignores a `SELECT_STREAMS`
    /// naming an unknown id wholesale and never confirms it, so an unknown
    /// id must be refused up front rather than queued into silence.
    pub(crate) fn knows_stream(&self, sid: &str) -> bool {
        self.collection.iter().any(|stream| stream.sid == sid)
    }

    /// An `ExternalSubtitleFailed` fired or the input was detached: a desire
    /// parked on it would otherwise park forever. Resets to UNSET (whatever is
    /// showing keeps showing), not to "off".
    pub(crate) fn external_gone(&mut self, id: ExternalSubId) {
        if self.desired_subtitle == Some(SubtitleDesire::External(id)) {
            debug!(?id, "dropping the subtitle desire for a failed external");
            self.desired_subtitle = None;
            self.dirty = true;
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
        // decodebin3 has committed to a selection, whatever it names. See the
        // seeding guard in `collection_changed`.
        self.reported_once = true;
        // A report speaks about the text slot only when a text stream was
        // there to be reported: decodebin3 merges one input's collection at a
        // time, so a report made before the text input joined leaves the slot
        // empty out of ignorance, not out of a decision.
        if reported.subtitle.is_some()
            || self
                .collection
                .iter()
                .any(|stream| stream.kind == StreamKind::Text)
        {
            self.text_state_known = true;
        }

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
                    self.dirty = true;
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
                self.dirty = true;
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
        let Some(pos) = self
            .superseded
            .iter()
            .position(|(sn, sel)| *sn == seqnum || sel == reported)
        else {
            return false;
        };
        self.superseded.drain(..=pos);
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
        self.refresh_wanted = false;
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
        self.desired_subtitle == Some(SubtitleDesire::Stream(None))
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
        self.refresh_wanted = false;
        // A refusal is not a skip: the event did not leave, so upstream's
        // selection is whatever it was and the optimistic `applied` is simply
        // false. Leaving it makes desired == applied and the engine converges
        // on a state that exists nowhere but in this struct. Reverting keeps
        // the desire divergent, so the next fresh event dispatches it again.
        // The ordering gate in `pump` is what stops that re-dispatch meeting
        // the same flush; this is the half that makes asking again possible at
        // all. Lever: `FCAST_NO_REFUSED_SELECTION_ROLLBACK`.
        if let Some(previous) = self.applied_before_dispatch.take()
            && std::env::var_os("FCAST_NO_REFUSED_SELECTION_ROLLBACK").is_none()
        {
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
            self.dirty = true;
        }
    }

    pub(crate) fn refresh_dispatched(&mut self, seqnum: gst::Seqnum) {
        self.refreshing = Some(seqnum);
    }

    /// Arm the advisory deadline for the dispatch just recorded (see
    /// [`Self::selection_deadline`]). Time arrives as an absolute `due`: the
    /// engine is pure state and its tests inject the clock. A no-op unless
    /// `seqnum` IS the live wait.
    pub(crate) fn arm_selection_deadline(
        &mut self,
        seqnum: gst::Seqnum,
        due: Instant,
        retries_left: u32,
    ) {
        if !self.selection_in_flight(seqnum) {
            return;
        }
        self.selection_deadline = Some(SelectionDeadline {
            seqnum,
            due,
            retries_left,
        });
    }

    /// Arm the advisory deadline for the refresh seek just dispatched. Same
    /// contract as [`Self::arm_selection_deadline`].
    pub(crate) fn arm_refresh_deadline(&mut self, seqnum: gst::Seqnum, due: Instant) {
        if !self.refresh_in_flight(seqnum) {
            return;
        }
        self.refresh_deadline = Some((seqnum, due));
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

        if let Some(deadline) = self.selection_deadline {
            if !self.selection_in_flight(deadline.seqnum) {
                self.selection_deadline = None;
            } else if deadline.due <= now {
                fires.push(DeadlineFire::Selection(deadline.seqnum));
                self.selection_deadline = Some(SelectionDeadline {
                    due: now + rearm,
                    ..deadline
                });
            }
        }

        if let Some((seqnum, due)) = self.refresh_deadline {
            if !self.refresh_in_flight(seqnum) {
                self.refresh_deadline = None;
            } else if due <= now {
                fires.push(DeadlineFire::Refresh(seqnum));
                self.refresh_deadline = Some((seqnum, now + rearm));
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
        let retries_left = match self.selection_deadline {
            Some(deadline) if deadline.seqnum == seqnum => {
                self.selection_deadline = None;
                deadline.retries_left
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
        if self
            .desired_video
            .as_ref()
            .is_some_and(|want| want != &actual.video)
        {
            self.desired_video = None;
        }
        if self
            .desired_audio
            .as_ref()
            .is_some_and(|want| want != &actual.audio)
        {
            self.desired_audio = None;
        }
        let subtitle_diverges = match &self.desired_subtitle {
            None => false,
            Some(SubtitleDesire::Stream(want)) => want != &actual.subtitle,
            // Unresolvable here (it needs the pump's externals map), and a
            // give-up is the last moment to keep insisting on it.
            Some(SubtitleDesire::External(_)) => true,
        };
        if subtitle_diverges {
            self.desired_subtitle = None;
        }
        self.selection_deadline = None;
        self.dirty = false;
        self.refresh_wanted = false;
        self.awaiting_external = false;
        // The give-up REPORTS the applied selection itself, so the request is
        // answered; leaving the flag set would answer a later, unrelated pump.
        self.unanswered_request = false;
        true
    }

    #[cfg(test)]
    pub(crate) fn has_dispatchable_work(&self) -> bool {
        self.dirty || self.refresh_wanted
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
        self.dirty
            || self.refresh_wanted
            || self.unanswered_request
            || self.awaiting_external
            || self.subtitle_held_since.is_some()
            || self.selecting.is_some()
            || self.refreshing.is_some()
    }

    /// The applied (or optimistically in-flight) selection.
    #[cfg(test)]
    pub(crate) fn applied(&self) -> &TrackSelection {
        &self.applied
    }

    /// Whether an explicit desire disagrees with a reported selection.
    /// Unset slots follow the pipeline and never disagree. An unresolved
    /// external cannot be compared (the collection pump handles it when it
    /// materializes).
    fn diverges_from_desired(&self, reported: &TrackSelection) -> bool {
        let stream_diverges = |desired: &Option<Option<String>>, actual: &Option<String>| {
            desired.as_ref().is_some_and(|want| want != actual)
        };
        if stream_diverges(&self.desired_video, &reported.video)
            || stream_diverges(&self.desired_audio, &reported.audio)
        {
            return true;
        }
        match &self.desired_subtitle {
            None => false,
            Some(SubtitleDesire::Stream(want)) => want != &reported.subtitle,
            // An external desire cannot be compared here: resolving it needs
            // the externals map, which only the pump has. "Never diverges"
            // would lose the re-assertion once the engine has converged and
            // decodebin3 auto-selects the embedded default over it (no
            // collection change follows to re-dirty). Deferring to the pump
            // is convergent: it dispatches only when the resolution really
            // differs, and each re-assertion needs a fresh foreign report.
            Some(SubtitleDesire::External(_)) => true,
        }
    }

    /// Whether the subtitle desire is parked on an external input that has not
    /// produced an advertised TEXT stream yet - the one resolution input that
    /// arrives outside the engine's own event stream. The collection change
    /// re-dirties the engine, but only the pump's `PumpCtx` says whether the
    /// INPUT produced the id, and the two need not arrive in that order.
    fn external_desire_unresolved(&self, ctx: &PumpCtx) -> bool {
        let Some(SubtitleDesire::External(id)) = &self.desired_subtitle else {
            return false;
        };
        !ctx.externals.iter().any(|(eid, sids)| {
            eid == id
                && sids.iter().any(|sid| {
                    self.collection
                        .iter()
                        .any(|s| &s.sid == sid && s.kind == StreamKind::Text)
                })
        })
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
        let in_collection = |sid: &String| self.collection.iter().any(|s| &s.sid == sid);
        let av_ok = |desired: &Option<Option<String>>| match desired {
            Some(Some(sid)) => in_collection(sid),
            _ => true,
        };
        if !av_ok(&self.desired_video) || !av_ok(&self.desired_audio) {
            return false;
        }
        match &self.desired_subtitle {
            Some(SubtitleDesire::Stream(Some(sid))) => self
                .collection
                .iter()
                .any(|s| &s.sid == sid && s.kind == StreamKind::Text),
            Some(SubtitleDesire::External(_)) => !self.external_desire_unresolved(ctx),
            _ => true,
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
        let in_collection = |sid: &String| self.collection.iter().any(|s| &s.sid == sid);
        // An external input can advertise more than its text stream, so the
        // subtitle slot must resolve against the KIND the collection reports,
        // not against mere membership.
        let advertised_text = |sid: &String| {
            self.collection
                .iter()
                .any(|s| &s.sid == sid && s.kind == StreamKind::Text)
        };
        // The collection's own default for a kind, mirroring decodebin3's
        // auto-select and `Self::seed_slot`.
        let default_of = |kind: StreamKind| {
            self.collection
                .iter()
                .find(|s| s.kind == kind)
                .map(|s| s.sid.clone())
        };
        // An explicit stream that left the collection cannot be selected: fall
        // back to what is applied rather than fabricating a disable.
        //
        // Omitting a kind from a `SELECT_STREAMS` asks decodebin3 to turn it
        // OFF, so an UNSET slot with nothing applied must resolve to the
        // collection default rather than to nothing. `applied` alone is not a
        // reliable stand-in: decodebin3 reports its selection while its merged
        // collection is still growing, so an early report can name audio alone
        // while the video stream sits right there in the collection.
        //
        // Lever: `FCAST_NO_SLOT_DEFAULTS` restores the applied-only fallback.
        let slot_defaults = std::env::var_os("FCAST_NO_SLOT_DEFAULTS").is_none();
        let fallback = |applied: &Option<String>, kind: StreamKind| {
            let kept = applied.clone().filter(in_collection);
            if slot_defaults {
                kept.or_else(|| default_of(kind))
            } else {
                kept
            }
        };
        let resolve_slot = |desired: &Option<Option<String>>,
                            applied: &Option<String>,
                            kind: StreamKind| match desired {
            None => fallback(applied, kind),
            Some(Some(sid)) if in_collection(sid) => Some(sid.clone()),
            Some(Some(_)) => fallback(applied, kind),
            Some(None) => None,
        };

        let video = resolve_slot(&self.desired_video, &self.applied.video, StreamKind::Video);
        let audio = resolve_slot(&self.desired_audio, &self.applied.audio, StreamKind::Audio);
        let mut subtitle = match &self.desired_subtitle {
            None => self.applied.subtitle.clone().filter(in_collection),
            Some(SubtitleDesire::Stream(None)) => None,
            Some(SubtitleDesire::Stream(Some(sid))) if in_collection(sid) => Some(sid.clone()),
            Some(SubtitleDesire::Stream(Some(_))) => {
                self.applied.subtitle.clone().filter(in_collection)
            }
            Some(SubtitleDesire::External(id)) => {
                let resolved = ctx
                    .externals
                    .iter()
                    .find(|(eid, _)| eid == id)
                    .and_then(|(_, sids)| sids.iter().find(|sid| advertised_text(sid)))
                    .cloned();
                match resolved {
                    Some(sid) => Some(sid),
                    // Not materialized yet: keep showing what shows now.
                    // The collection change re-dirties when it appears.
                    None => self.applied.subtitle.clone().filter(in_collection),
                }
            }
        };

        // A text stream cannot be presented without a video stream, so
        // deselecting video implicitly deselects subtitles. But "the selection
        // has no video" and "the collection has no video" differ, and only the
        // first is a deselect: the second is decodebin3's merged collection
        // still growing, or a media with no video at all, and there
        // `poll_text_policy` never links text into the overlay anyway.
        if video.is_none() && subtitle.is_some() {
            let collection_has_video = self
                .collection
                .iter()
                .any(|stream| stream.kind == StreamKind::Video);
            let video_is_being_turned_off =
                collection_has_video || self.desired_video == Some(None);
            // Whether this dispatch would carry nothing but the loss of the
            // subtitle.
            let only_the_subtitle_moves =
                video == self.applied.video && audio == self.applied.audio;
            if !video_is_being_turned_off
                && std::env::var_os("FCAST_NO_SUBTITLE_HOLDBACK").is_none()
            {
                // decodebin3 grows its merged collection as each input
                // reports, so a resolution early in a load can see audio and
                // text with no video yet. An event whose whole content is
                // dropping the text stream turns a request to ENABLE a
                // subtitle into one that disables it, and decodebin3 then
                // never auto-selects text again. Wait: the collection change
                // that brings video in re-dirties the desire.
                //
                // BOUNDED, because that premise fails on media that never
                // announces video at all: no collection change is coming, the
                // pump has already consumed `dirty`, and the desire (plus the
                // request it answers) was swallowed for the life of the item.
                // Past the grace the dispatch goes out with the subtitle kept,
                // exactly as the sibling arm below sends it.
                if only_the_subtitle_moves {
                    let since = held_since.unwrap_or(ctx.now);
                    if ctx.now.saturating_duration_since(since) < SUBTITLE_HOLDBACK_GRACE {
                        self.subtitle_held_since = Some(since);
                        debug!(
                            collection = ?self.collection,
                            "holding the subtitle selection back until the collection announces video"
                        );
                        return None;
                    }
                    debug!(
                        collection = ?self.collection,
                        "the collection never announced video; dispatching the held subtitle"
                    );
                }
                // Another slot has real work, so this dispatch cannot wait.
                // Send it with the subtitle KEPT. The video pinning the event
                // carries is unavoidable while the collection has no video id
                // to name, and dropping the text stream on top of that would
                // be a second deselect nobody asked for, one decodebin3 never
                // undoes.
                debug!(
                    collection = ?self.collection,
                    "keeping the subtitle in a selection the collection cannot give video to"
                );
            } else {
                debug!(
                    desired_video = ?self.desired_video,
                    applied_video = ?self.applied.video,
                    collection = ?self.collection,
                    "dropping the subtitle stream from a selection without video"
                );
                subtitle = None;
            }
        }

        let selection = TrackSelection {
            video,
            audio,
            subtitle,
        };
        // Never produce an empty selection: it trips the GStreamer assertion
        // `gst_event_new_select_streams: streams != NULL`.
        if selection.video.is_none() && selection.audio.is_none() && selection.subtitle.is_none() {
            debug!("refusing to resolve to an empty stream selection");
            return None;
        }
        Some(selection)
    }

    /// Decide the next operation to dispatch, if the transport allows one.
    pub(crate) fn pump(&mut self, ctx: &PumpCtx) -> Option<Command> {
        // A scheduled re-emit flush is hazardous once an external subtitle
        // input attaches (it races the external inputs' reconfiguration) and
        // pointless on an unseekable stream; re-decided at every pump.
        if self.refresh_wanted && (ctx.externals_attached || !ctx.gate.seekable) {
            self.refresh_wanted = false;
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
        let external_arrived = self.awaiting_external && !unresolved_external;
        self.awaiting_external = unresolved_external;

        // The subtitle holdback deferred a resolution and nothing is bound to
        // re-dirty it, so the pump has to come back to it on its own until the
        // grace runs out (see `resolve`).
        let holding_back = self.subtitle_held_since.is_some();

        if self.dirty || external_arrived || holding_back {
            self.dirty = false;
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
                && std::mem::take(&mut self.unanswered_request)
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
                self.unanswered_request = false;
                // A flushing seek to the current position drops the
                // deeply-buffered old track so a switch takes effect at once.
                // Scheduled only for a switch TO a real audio/subtitle track,
                // and never when:
                //   * an external subtitle is attached (any flush races the external inputs'
                //     reconfiguration and can freeze the item)
                //   * any slot is being DISABLED (Some -> None): flushing across a sink/branch
                //     teardown wedges (audio-off drops the pipeline clock, video-off freezes
                //     the audio clock, subtitle-off fails vaapi renegotiation).
                // Video switches never flush (it re-prerolls the video chain).
                //
                // A subtitle switch in UPSTREAM mode does not flush either: the
                // adaptive demuxer restarts a re-selected text track from the
                // current position itself (and the join replays the park), while
                // a flushing seek to "the current position" snaps back to the
                // fragment boundary. Measured on DASH: refresh at 5.633 s landed
                // the segment at 4.0 s, a user-visible position jump on every
                // subtitle enable. Audio keeps the flush: its switch lag is the
                // deep aqueue downstream, which no demuxer restart empties.
                let switching_to_track = (target.subtitle != self.applied.subtitle
                    && target.subtitle.is_some()
                    && !ctx.upstream_owns)
                    || (target.audio != self.applied.audio && target.audio.is_some());
                let disabling = (self.applied.audio.is_some() && target.audio.is_none())
                    || (self.applied.video.is_some() && target.video.is_none())
                    || (self.applied.subtitle.is_some() && target.subtitle.is_none());
                self.refresh_wanted = switching_to_track
                    && !disabling
                    && !ctx.externals_attached
                    && ctx.gate.seekable;
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
        // DEFERRED, not dropped: `refresh_wanted` stays set, so the first pump
        // after the confirmation (or the refusal, or the deadline advisory's
        // timeout - all three clear `selecting`) dispatches it. The re-emit is
        // late by one pump, which is invisible; the alternative is a track that
        // never plays. Its safety is re-decided at the top of every pump
        // anyway, so a deferral can only ever make the flush MORE conservative.
        //
        // Lever: `FCAST_NO_REFRESH_SELECTION_ORDERING`.
        if self.refresh_wanted
            && self.selecting.is_some()
            && std::env::var_os("FCAST_NO_REFRESH_SELECTION_ORDERING").is_none()
        {
            debug!("deferring the re-emit flush behind an unconfirmed selection");
            return None;
        }

        if self.refresh_wanted {
            self.refresh_wanted = false;
            return Some(Command::RefreshSeek);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(video: Option<&str>, audio: Option<&str>, subtitle: Option<&str>) -> TrackSelection {
        TrackSelection {
            video: video.map(str::to_string),
            audio: audio.map(str::to_string),
            subtitle: subtitle.map(str::to_string),
        }
    }

    fn collection(streams: &[(&str, StreamKind)]) -> Vec<CollectionStream> {
        streams
            .iter()
            .map(|(sid, kind)| CollectionStream {
                sid: sid.to_string(),
                kind: *kind,
            })
            .collect()
    }

    /// The standard A/V+text collection most tests run against.
    fn avt_collection() -> Vec<CollectionStream> {
        collection(&[
            ("v0", StreamKind::Video),
            ("a0", StreamKind::Audio),
            ("a1", StreamKind::Audio),
            ("t0", StreamKind::Text),
            ("t1", StreamKind::Text),
        ])
    }

    fn ctx(quiet: bool, paused: bool) -> PumpCtx {
        PumpCtx {
            gate: SelectionGate {
                quiet,
                paused,
                seekable: true,
            },
            externals_attached: false,
            externals: Vec::new(),
            // These cases predate the split and model db3-owned mode.
            upstream_owns: false,
            now: Instant::now(),
        }
    }

    /// [`ctx`] with the pump's clock pinned, for the cases that drive the
    /// bounded subtitle holdback.
    fn ctx_at(quiet: bool, paused: bool, now: Instant) -> PumpCtx {
        PumpCtx {
            now,
            ..ctx(quiet, paused)
        }
    }

    /// Upstream-selection mode, where decodebin3 posts no confirmations of its
    /// own and the crate owes the caller every answer.
    fn ctx_upstream(quiet: bool, paused: bool) -> PumpCtx {
        PumpCtx {
            upstream_owns: true,
            ..ctx(quiet, paused)
        }
    }

    /// A request for what is ALREADY applied still has to be answered in
    /// upstream-selection mode: nothing else ever will, and the caller keeps
    /// showing the previous track until it hears back.
    #[test]
    fn an_already_satisfied_request_is_confirmed_once_in_upstream_mode() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert_eq!(
            engine.pump(&ctx_upstream(true, false)),
            Some(Command::ConfirmApplied(sel(
                Some("v0"),
                Some("a0"),
                Some("t0")
            ))),
        );
        // Exactly once per request.
        assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
    }

    /// A request naming an external that has NOT materialized is NOT satisfied
    /// by whatever happens to be on screen.
    #[test]
    fn a_request_for_an_unmaterialized_external_is_not_confirmed() {
        const EXT: ExternalSubId = ExternalSubId(9);
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );

        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
        // Nothing advertised for it yet: no answer, and the request survives.
        assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
        assert_eq!(engine.pump(&ctx_upstream(true, false)), None);

        // It materializes, and the REAL dispatch answers the request.
        let mut collection = avt_collection();
        collection.push(CollectionStream {
            sid: "ext0".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(collection);
        let ctx = PumpCtx {
            upstream_owns: true,
            ..ctx_ext(true, false, &[(EXT, &["ext0"])])
        };
        assert_eq!(
            engine.pump(&ctx),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("ext0")
            ))),
        );
        // Consumed by that dispatch: no confirmation follows it.
        let seqnum = gst::Seqnum::next();
        engine.selection_dispatched(seqnum, sel(Some("v0"), Some("a0"), Some("ext0")));
        engine.streams_selected(seqnum, &sel(Some("v0"), Some("a0"), Some("ext0")));
        assert_eq!(engine.pump(&ctx), None);
    }

    /// A request for the external that IS already applied stays confirmable:
    /// the acceptance case, and the reason the gate tests resolvability rather
    /// than "is an external".
    #[test]
    fn a_request_for_the_already_applied_external_is_confirmed() {
        const EXT: ExternalSubId = ExternalSubId(9);
        let mut engine = SelectionEngine::default();
        let mut collection = avt_collection();
        collection.push(CollectionStream {
            sid: "ext0".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(collection);
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("ext0")),
        );

        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
        let ctx = PumpCtx {
            upstream_owns: true,
            ..ctx_ext(true, false, &[(EXT, &["ext0"])])
        };
        assert_eq!(
            engine.pump(&ctx),
            Some(Command::ConfirmApplied(sel(
                Some("v0"),
                Some("a0"),
                Some("ext0")
            ))),
        );
    }

    /// A request for a sid the collection does not carry is the same shape as
    /// the unmaterialized external: a fallback, not an answer.
    #[test]
    fn a_request_for_an_absent_sid_is_not_confirmed() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );

        engine.request(
            TrackSlot::Subtitle,
            TrackTarget::Stream(Some("gone".into())),
        );
        assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
    }

    /// The same request in db3-owned mode is answered by decodebin3, so the
    /// engine must not manufacture a second confirmation there.
    #[test]
    fn an_already_satisfied_request_is_not_confirmed_in_db3_owned_mode() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    /// `dirty` set by anything OTHER than a user request must never confirm, or
    /// the receiver relays unsolicited track changes.
    #[test]
    fn a_dirty_engine_without_a_request_confirms_nothing() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );
        // Collection churn, no request.
        engine.collection_changed(avt_collection());
        assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
    }

    /// A request that lands while the transport is not quiet is ANSWERED LATER,
    /// not dropped: the bookkeeping outlives every None-returning gate.
    #[test]
    fn a_request_made_while_not_quiet_is_answered_at_the_next_quiet_pump() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert_eq!(engine.pump(&ctx_upstream(false, false)), None);
        assert_eq!(
            engine.pump(&ctx_upstream(true, false)),
            Some(Command::ConfirmApplied(sel(
                Some("v0"),
                Some("a0"),
                Some("t0")
            ))),
        );
    }

    fn ctx_ext(quiet: bool, paused: bool, externals: &[(ExternalSubId, &[&str])]) -> PumpCtx {
        PumpCtx {
            gate: SelectionGate {
                quiet,
                paused,
                seekable: true,
            },
            upstream_owns: false,
            externals_attached: true,
            externals: externals
                .iter()
                .map(|(id, sids)| (*id, sids.iter().map(|s| s.to_string()).collect()))
                .collect(),
            now: Instant::now(),
        }
    }

    /// Engine with the standard collection adopted and its defaults confirmed
    /// as applied (the steady state most flows start from).
    fn settled_engine() -> SelectionEngine {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );
        // The collection change marked the engine dirty and the auto-select
        // matches every (unset) desire, so this pump converges without a
        // dispatch.
        assert_eq!(engine.pump(&ctx(true, false)), None);
        engine
    }

    #[test]
    fn collection_seeds_applied_with_defaults() {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(avt_collection());
        assert_eq!(engine.applied(), &sel(Some("v0"), Some("a0"), Some("t0")),);
    }

    #[test]
    fn selection_dispatches_immediately_when_quiet() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a1"),
                Some("t0")
            )))
        );
    }

    #[test]
    fn selection_waits_until_quiet() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(engine.pump(&ctx(false, false)), None);
        assert!(engine.has_dispatchable_work());
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a1"),
                Some("t0")
            )))
        );
    }

    #[test]
    fn noop_selection_is_not_dispatched() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert_eq!(engine.pump(&ctx(true, false)), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn playing_switch_serializes_and_coalesces_latest_wins() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());

        // Unconfirmed selection blocks everything while playing, including
        // the refresh the subtitle switch scheduled.
        assert_eq!(engine.pump(&ctx(true, false)), None);

        // Spammed changes compose latest-wins against the optimistic
        // applied state.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        assert_eq!(engine.pump(&ctx(true, false)), None);

        // The dispatched switch confirms and the composed latest goes out.
        engine.streams_selected(sn, &target);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(Some("v0"), Some("a0"), None)))
        );
    }

    #[test]
    fn selection_confirms_by_content_when_seqnum_is_lost() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, false)).is_some());
        engine.selection_dispatched(gst::Seqnum::next(), target.clone());

        // A confirmation under a *foreign* seqnum, but reporting exactly
        // the requested selection, settles it.
        engine.streams_selected(gst::Seqnum::next(), &target);
        assert!(engine.selecting.is_none());
        // No re-dispatch (converged). Only the switch's scheduled flush
        // remains, exactly once.
        assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn overtaken_selection_is_redispatched() {
        // decodebin3's own collection-default auto-select lands after ours and
        // stomps it. The overtaking STREAMS_SELECTED must re-assert the desire
        // instead of waiting forever on a confirmation that never comes.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let target = sel(Some("v0"), Some("a0"), None);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        engine.selection_dispatched(gst::Seqnum::next(), target.clone());

        // decodebin3's own auto-select arrives instead of our confirmation:
        // foreign seqnum, foreign content.
        let adopted = sel(Some("v0"), Some("a0"), Some("t0"));
        engine.streams_selected(gst::Seqnum::next(), &adopted);

        // The desire re-asserts with a fresh dispatch.
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        engine.selection_dispatched(gst::Seqnum::next(), target.clone());

        // This time it applies (content match settles under any seqnum) and
        // the engine converges.
        engine.streams_selected(gst::Seqnum::next(), &target);
        assert!(engine.selecting.is_none());
        assert_eq!(engine.pump(&ctx(true, false)), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn overtaken_selection_yields_to_a_newer_request() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        assert!(engine.pump(&ctx(true, false)).is_some());
        engine.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a0"), Some("t1")));

        // A newer request lands while the first is unconfirmed.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));

        // The overtaking event must not resurrect the old request over it:
        // the latest desire wins.
        let adopted = sel(Some("v0"), Some("a0"), Some("t0"));
        engine.streams_selected(gst::Seqnum::next(), &adopted);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(Some("v0"), Some("a0"), None)))
        );
    }

    #[test]
    fn foreign_autoselect_with_nothing_in_flight_is_reasserted() {
        // A fresh collection's auto-select stomping the applied state with NO
        // request in flight is detected and corrected.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let target = sel(Some("v0"), Some("a0"), None);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&ctx(true, false)), None);

        // Later, decodebin3 auto-selects the text stream on its own (fresh
        // collection default). The explicit "no subtitle" desire re-asserts.
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target))
        );
    }

    #[test]
    fn unset_slots_follow_the_pipeline() {
        // Without an explicit desire the engine never fights the pipeline's
        // own choices.
        let mut engine = settled_engine();
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a1"), None));
        assert_eq!(engine.pump(&ctx(true, false)), None);
        assert_eq!(engine.applied(), &sel(Some("v0"), Some("a1"), None));
    }

    #[test]
    fn refresh_dispatches_after_selection_settles_and_pipeline_quiets() {
        let mut engine = settled_engine();
        // Enabling a subtitle schedules a refresh.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        // Re-preroll in progress: refresh must hold.
        assert_eq!(engine.pump(&ctx(false, false)), None);
        // Settled: flush.
        assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
        // One flush only.
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn audio_switch_schedules_refresh() {
        let mut engine = settled_engine();
        // An audio switch must flush the deeply-buffered old track so it's
        // audible immediately.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let target = sel(Some("v0"), Some("a1"), Some("t0"));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn externals_attached_suppresses_the_switch_flush() {
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(7);
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let target = sel(Some("v0"), Some("a1"), Some("t0"));
        let ctx1 = ctx_ext(true, false, &[(ext, &[])]);
        assert_eq!(
            engine.pump(&ctx1),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&ctx1), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn external_attaching_before_the_flush_drops_it() {
        // A plain switch schedules its flush, an external attaches while
        // the selection is confirming: the dispatch-time ctx check drops
        // the now-hazardous flush.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, false)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);

        let ext = crate::ExternalSubId(3);
        assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);
        assert!(!engine.has_dispatchable_work());
        // Detaching later must not resurrect the dropped flush either.
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn unseekable_stream_never_flushes() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let mut c = ctx(true, false);
        c.gate.seekable = false;
        let target = sel(Some("v0"), Some("a1"), Some("t0"));
        assert_eq!(
            engine.pump(&c),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&c), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn subtitle_disable_cancels_refresh() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, false)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);

        // Disable before the refresh fired: no flush may follow (flushing
        // right after the text-branch teardown breaks renegotiation).
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let target = sel(Some("v0"), Some("a0"), None);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn2 = gst::Seqnum::next();
        engine.selection_dispatched(sn2, target.clone());
        engine.streams_selected(sn2, &target);
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn audio_switch_with_subtitle_disable_schedules_no_refresh() {
        let mut engine = settled_engine();
        // Switching audio while also disabling subtitles: the
        // subtitle-disable flush hazard wins (accept the audio drain in
        // this combo).
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let target = sel(Some("v0"), Some("a1"), None);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn user_seek_cancels_refresh() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, false)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);

        // The user's own flushing seek re-emits the cue already.
        engine.cancel_refresh();
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn paused_selection_parks_and_the_refresh_waits_for_it() {
        // RENAMED, and the old name is the point. It was
        // `paused_selection_parks_and_refresh_flushes_past_it`, and flushing
        // past a parked selection is the defect: the refresh is a FLUSHING
        // seek, `SELECT_STREAMS` is sent from the select lane, and a flush
        // landing mid-send makes that `send_event` return false. urisourcebin
        // refuses the event, the stream is never selected upstream, and the
        // crate is left with a linked text branch no data reaches. Pinned
        // end-to-end by
        // `dash_segmented_embedded_text_shows_on_a_first_select_while_paused`.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());

        // While paused the selection is parked (no STREAMS_SELECTED until data
        // flows), and the refresh waits behind it. DEFERRED, not dropped: the
        // want survives every pump that refuses it, so an arbitrarily long
        // parked window cannot lose the re-emit.
        for _ in 0..5 {
            assert_eq!(engine.pump(&ctx(true, true)), None);
        }
        assert!(
            engine.has_dispatchable_work(),
            "the deferred refresh was dropped"
        );

        // Confirmation releases it, once.
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&ctx(true, true)), Some(Command::RefreshSeek));
        engine.refresh_dispatched(gst::Seqnum::next());

        // Flush in flight: nothing else dispatches even though paused.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(engine.pump(&ctx(false, true)), None);
    }

    #[test]
    fn a_refused_paused_selection_drops_its_deferred_refresh() {
        // The other half of the deferral: the refresh exists to re-emit a cue
        // for a switch that HAPPENED. A refused dispatch never switched
        // anything, so releasing the deferred flush would put an orphan
        // flushing seek into the pipeline for no reason.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, true)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target);
        assert_eq!(engine.pump(&ctx(true, true)), None);

        engine.dispatch_failed(sn);
        assert_eq!(engine.pump(&ctx(true, true)), None);
        // And the refusal rolled the optimistic `applied` back, so the desire
        // is divergent again and a later fresh event still asks.
        assert_ne!(engine.applied.subtitle, Some("t1".to_string()));
    }

    #[test]
    fn paused_selection_can_be_superseded() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert!(engine.pump(&ctx(true, true)).is_some());
        let sn1 = gst::Seqnum::next();
        let first = sel(Some("v0"), Some("a1"), Some("t0"));
        engine.selection_dispatched(sn1, first.clone());

        // A parked selection has no re-preroll to overlap with, so the next
        // request replaces it instead of queueing behind it forever.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a0".into())));
        let second = sel(Some("v0"), Some("a0"), Some("t0"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second.clone()))
        );
        let sn2 = gst::Seqnum::next();
        engine.selection_dispatched(sn2, second.clone());

        // The stale confirmation (sn1, reporting the superseded audio) must
        // settle neither by its seqnum nor by content.
        engine.streams_selected(sn1, &first);
        assert!(engine.selecting.is_some());
        // The superseding one settles on its own seqnum.
        engine.streams_selected(sn2, &second);
        assert!(engine.selecting.is_none());
    }

    #[test]
    fn paused_switch_refreshes_exactly_once() {
        // A paused subtitle switch dispatches the selection, then ONE re-emit
        // flush: the flushing seek re-prerolls, so the cue composites before
        // ASYNC_DONE and no retry is needed.
        //
        // COUNTED across the whole window, not asserted at one chosen pump,
        // and that is the point of the rewrite. The deferral moved the refresh
        // from before the confirmation to after it, and "fires once, later" and
        // "fires once on the deferral AND again on the confirmation" are
        // indistinguishable to any test that only looks at a single pump - or
        // to the e2e reproduction, which asserts cue DELIVERY and would pass
        // just as well with a redundant flush in it.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, true)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());

        let mut refreshes = 0;
        let pump = |engine: &mut SelectionEngine, refreshes: &mut usize| {
            if engine.pump(&ctx(true, true)) == Some(Command::RefreshSeek) {
                *refreshes += 1;
                engine.refresh_dispatched(gst::Seqnum::next());
                assert!(engine.refresh_done());
            }
        };
        // Parked: pumped repeatedly, released by nothing.
        for _ in 0..3 {
            pump(&mut engine, &mut refreshes);
        }
        assert_eq!(
            refreshes, 0,
            "a refresh flushed past an unconfirmed selection"
        );
        // Confirmed: released, and only once however often the pump runs.
        engine.streams_selected(sn, &target);
        for _ in 0..3 {
            pump(&mut engine, &mut refreshes);
        }
        assert_eq!(refreshes, 1, "the paused switch re-emitted more than once");
        assert_eq!(engine.pump(&ctx(true, true)), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn async_done_settles_refresh_by_exclusivity() {
        let mut engine = settled_engine();
        // No refresh out: an unrelated ASYNC_DONE is not a refresh
        // completion.
        assert!(!engine.refresh_done());
        engine.refresh_dispatched(gst::Seqnum::next());
        assert!(engine.refresh_done());
        assert!(engine.refreshing.is_none());
    }

    #[test]
    fn new_collection_invalidates_in_flight_work() {
        // A new collection means an in-flight selection targeted stream ids
        // that may be gone, so the waits are abandoned deterministically.
        let mut engine = settled_engine();
        engine.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a1"), None));
        engine.refresh_dispatched(gst::Seqnum::next());
        assert!(engine.selecting.is_some());

        engine.collection_changed(avt_collection());
        assert!(engine.selecting.is_none());
        assert!(engine.refreshing.is_none());
    }

    #[test]
    fn collection_reconciles_applied_and_keeps_explicit_desires() {
        let mut engine = settled_engine();
        // Explicitly select t1, confirmed.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, false)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        engine.cancel_refresh();

        // t1 leaves the collection: applied falls back to the default text
        // stream, and the desire (t1) is unresolvable, so the engine
        // converges on what remains rather than fighting.
        engine.collection_changed(collection(&[
            ("v0", StreamKind::Video),
            ("a0", StreamKind::Audio),
            ("t0", StreamKind::Text),
        ]));
        assert_eq!(engine.applied(), &sel(Some("v0"), Some("a0"), Some("t0")));
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn external_desire_parks_until_materialized() {
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

        // Attached but no stream in the collection yet: keep showing what
        // shows now (no dispatch, the current selection already matches).
        assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);

        // The external's stream materializes in a new collection. The
        // desire resolves and dispatches.
        let mut streams = avt_collection();
        streams.push(CollectionStream {
            sid: "ext-t".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(streams);
        assert_eq!(
            engine.pump(&ctx_ext(true, false, &[(ext, &["ext-t"])])),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("ext-t")
            )))
        );
    }

    #[test]
    fn an_external_that_materializes_before_its_input_is_visible_is_not_stranded() {
        // Resolving the desire needs two independent things to line up:
        // decodebin3 must advertise the input's stream, and the routing table
        // must have seen that input produce the id. Only the first re-dirties
        // the engine, so in that order the dirty flag is spent on a resolution
        // that cannot see the input yet and only `awaiting_external` re-arms.
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

        let mut streams = avt_collection();
        streams.push(CollectionStream {
            sid: "ext-t".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(streams);

        // The pump runs before the routing table lists the input's ids.
        assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);

        // They appear. Nothing else happens, so this pump is the only chance
        // the desire gets.
        let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("ext-t")
            )))
        );
    }

    #[test]
    fn external_desire_survives_the_autoselect_stomp() {
        // The desire dispatches after materialization, decodebin3's
        // auto-select for the new collection stomps it, the engine re-asserts.
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

        let mut streams = avt_collection();
        streams.push(CollectionStream {
            sid: "ext-t".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(streams);
        let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];
        let target = sel(Some("v0"), Some("a0"), Some("ext-t"));
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(target.clone()))
        );
        engine.selection_dispatched(gst::Seqnum::next(), target.clone());

        // Auto-select stomps with the embedded default.
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(target.clone()))
        );
        engine.selection_dispatched(gst::Seqnum::next(), target.clone());
        engine.streams_selected(gst::Seqnum::next(), &target);
        assert_eq!(engine.pump(&ctx_ext(true, false, externals)), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn external_switch_never_schedules_the_refresh() {
        // No refresh flush while an external input is attached; the
        // crate's join-time input replay covers externals instead.
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

        let mut streams = avt_collection();
        streams.push(CollectionStream {
            sid: "ext-t".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(streams);
        let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];

        let target = sel(Some("v0"), Some("a0"), Some("ext-t"));
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert_eq!(engine.pump(&ctx_ext(true, false, externals)), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn failed_external_desire_keeps_the_current_subtitle() {
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));
        assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);

        // The input fails (watchdog/bus error): the parked desire must not
        // park forever, and whatever was showing before the request keeps
        // showing (no dispatch at all).
        engine.external_gone(ext);
        assert_eq!(engine.pump(&ctx(true, false)), None);
        assert!(!engine.has_dispatchable_work());
        assert_eq!(engine.applied(), &sel(Some("v0"), Some("a0"), Some("t0")));
    }

    #[test]
    fn external_desire_is_reasserted_after_a_foreign_autoselect() {
        // The external counterpart of
        // `foreign_autoselect_with_nothing_in_flight_is_reasserted`: after the
        // external's stream is confirmed and the engine converged, decodebin3
        // auto-selects the embedded text default and stomps it. The desire
        // must re-assert - no collection change follows such a stomp, so
        // nothing else would ever re-dirty the engine.
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

        let mut streams = avt_collection();
        streams.push(CollectionStream {
            sid: "ext-t".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(streams);
        let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];
        let target = sel(Some("v0"), Some("a0"), Some("ext-t"));
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        // Converged: nothing in flight, nothing dirty.
        assert_eq!(engine.pump(&ctx_ext(true, false, externals)), None);

        // decodebin3 selects the embedded text default on its own.
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(target))
        );
    }

    #[test]
    fn a_stale_superseded_echo_does_not_revert_applied() {
        // The paused supersede path: a dispatch's late confirmation is our own
        // stale echo, and must not be adopted as applied either - `applied`
        // (and with it `subtitle_sid`, which gates what may join the overlay)
        // would name the subtitle the superseding dispatch is turning OFF.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let first = sel(Some("v0"), Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(first.clone()))
        );
        let sn1 = gst::Seqnum::next();
        engine.selection_dispatched(sn1, first.clone());

        // Paused: the next request supersedes the parked dispatch.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let second = sel(Some("v0"), Some("a0"), None);
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second.clone()))
        );
        let sn2 = gst::Seqnum::next();
        engine.selection_dispatched(sn2, second.clone());

        // The superseded dispatch's late echo arrives.
        engine.streams_selected(sn1, &first);
        assert!(engine.selecting.is_some(), "the live wait must survive");
        assert_eq!(
            engine.applied(),
            &second,
            "a stale echo must not revert the applied selection"
        );
        assert_eq!(
            engine.subtitle_sid(),
            None,
            "a stale echo must not re-authorize the subtitle being turned off"
        );
    }

    #[test]
    fn external_resolution_picks_the_inputs_text_stream() {
        // An external input can advertise more than one stream, in source-pad
        // order, so resolving to its FIRST advertised stream would drop a
        // non-text id into the subtitle slot and deselect the text one.
        let mut engine = settled_engine();
        let ext = crate::ExternalSubId(1);
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

        let mut streams = avt_collection();
        streams.push(CollectionStream {
            sid: "ext-a".into(),
            kind: StreamKind::Audio,
        });
        streams.push(CollectionStream {
            sid: "ext-t".into(),
            kind: StreamKind::Text,
        });
        engine.collection_changed(streams);

        let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-a", "ext-t"])];
        assert_eq!(
            engine.pump(&ctx_ext(true, false, externals)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("ext-t")
            )))
        );
    }

    #[test]
    fn video_disable_implicitly_drops_subtitles() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Video, TrackTarget::Stream(None));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(None, Some("a0"), None)))
        );
    }

    /// decodebin3 reports its selection while its merged collection is still
    /// growing, so a report can name audio alone with the video stream
    /// already advertised. Composing against that empty video slot asks
    /// decodebin3 to turn video OFF, which nobody requested, and the rule
    /// above then strips the subtitle the request was about.
    #[test]
    fn an_unset_slot_left_empty_by_a_partial_report_resolves_to_the_default() {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(avt_collection());
        // decodebin3's own first report, before its video input reported.
        engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));
        assert_eq!(engine.applied().video, None);

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("t1")
            ))),
            "the unset video slot must follow the collection default, not compose a disable"
        );
    }

    /// The same growth window, one post earlier, when the collection has no
    /// video stream at all yet. Nothing can be composed that honours the
    /// request, so nothing is dispatched. The collection change that brings
    /// video in re-dirties the desire.
    #[test]
    fn a_subtitle_request_waits_for_a_collection_that_has_video() {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(collection(&[
            ("a0", StreamKind::Audio),
            ("t0", StreamKind::Text),
            ("t1", StreamKind::Text),
        ]));
        // decodebin3 auto-selected audio and the first text before its video
        // input reported, so a subtitle IS showing. Without that the stripped
        // target equals `applied` and the no-op check hides the defect.
        engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));

        // Switching to the other text stream must not be answered by turning
        // subtitles off, which is what stripping t1 and dispatching the
        // remainder amounts to.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            None,
            "a selection that strips the requested subtitle must not be dispatched"
        );

        // Video arrives. Now the request is expressible, and the collection
        // change re-dirtied it.
        engine.collection_changed(avt_collection());
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("t1")
            )))
        );
    }

    /// On a media with NO video at all the holdback's premise never holds: the
    /// collection change that brings video in is not coming, and `pump` clears
    /// `dirty` before `resolve` answers `None`. Nothing was dispatched, no
    /// `ConfirmApplied` was produced and `unanswered_request` stayed set for
    /// the life of the item, so the request was swallowed whole.
    ///
    /// The wait is bounded now: while it lasts the engine counts as
    /// unconverged (which is what keeps the pump poked at all), and past
    /// [`SUBTITLE_HOLDBACK_GRACE`] the switch goes out with the subtitle KEPT,
    /// exactly as the sibling arm sends it when another slot has work.
    #[test]
    fn a_video_less_item_eventually_dispatches_a_held_back_subtitle() {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(collection(&[
            ("a0", StreamKind::Audio),
            ("t0", StreamKind::Text),
            ("t1", StreamKind::Text),
        ]));
        engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));

        let t0 = Instant::now();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        // Held back, and `dirty` is consumed by the attempt: only the wait
        // itself brings the pump back.
        assert_eq!(engine.pump(&ctx_at(true, false, t0)), None);
        assert!(!engine.has_dispatchable_work());
        assert!(
            engine.unconverged(),
            "a held-back desire must keep the engine poked"
        );
        // Inside the grace the collection could still be growing a video
        // stream, so every pump keeps waiting.
        for step in 1..5 {
            let now = t0 + Duration::from_millis(100 * step);
            assert_eq!(engine.pump(&ctx_at(true, false, now)), None);
            assert_eq!(engine.pump(&ctx_at(true, true, now)), None);
        }
        assert_eq!(
            engine.applied().subtitle.as_deref(),
            Some("t0"),
            "the switch went out before the collection had its chance"
        );

        // No video ever came. The switch dispatches with the subtitle kept.
        let late = t0 + SUBTITLE_HOLDBACK_GRACE;
        let target = sel(None, Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx_at(true, false, late)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        engine.streams_selected(sn, &target);
        assert!(
            engine.subtitle_held_since.is_none(),
            "the wait outlived the dispatch that ended it"
        );
        assert!(
            !engine.unanswered_request,
            "the dispatch's own confirmation answers the request"
        );
    }

    /// The same wait, on the request the field actually loses: an external
    /// subtitle attached to an audio-only item with `select=true`.
    ///
    /// `collection_changed` seeds the text slot with the external's stream, so
    /// the resolution EQUALS `applied` and the only thing owed is the answer -
    /// which the holdback swallowed with the resolution, leaving the input held
    /// and the caller waiting for a confirmation that never came.
    #[test]
    fn a_video_less_item_answers_a_held_back_external_request() {
        let ext = ExternalSubId(3);
        let mut engine = SelectionEngine::new();
        engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
        engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), None));
        // The external input's stream is merged in.
        engine.collection_changed(collection(&[
            ("a0", StreamKind::Audio),
            ("ext-t", StreamKind::Text),
        ]));

        let t0 = Instant::now();
        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));
        let held = PumpCtx {
            now: t0,
            upstream_owns: true,
            ..ctx_ext(true, false, &[(ext, &["ext-t"])])
        };
        assert_eq!(engine.pump(&held), None);
        assert!(
            engine.unconverged(),
            "the request is owed an answer only this crate can give"
        );

        let late = PumpCtx {
            now: t0 + SUBTITLE_HOLDBACK_GRACE,
            ..held
        };
        assert_eq!(
            engine.pump(&late),
            Some(Command::ConfirmApplied(sel(
                None,
                Some("a0"),
                Some("ext-t")
            ))),
            "the held-back request was never answered"
        );
        assert!(!engine.unconverged());
    }

    /// A report that PREDATES half the collection must not leave the engine
    /// silent. `empty_text_stream.toml`'s wedge in twenty lines instead of a
    /// 52-second scenario sweep.
    ///
    /// `collection_changed` seeds still-empty slots with the collection
    /// default, mirroring decodebin3's own auto-select, and that premise fails
    /// when the auto-select ran EARLY against a partial merged collection and
    /// was never revisited. With `parse-streams=true` every elementary stream
    /// is its own input, so the merged collection arrives in increments.
    ///
    /// The sequence below is the one the scenario produced verbatim. The
    /// unguarded seed wrote the grown-in audio and video into `applied`, the
    /// pump found `target == applied` and dispatched NOTHING, so decodebin3
    /// kept playing text alone with no A/V chain built, no real sink joined,
    /// the preroll token holding the pipeline ASYNC and the load never reaching
    /// settled PLAYING.
    #[test]
    fn a_report_that_predates_half_the_collection_still_dispatches() {
        let mut engine = SelectionEngine::new();
        // decodebin3's text input reports first, and it auto-selects on what it
        // has.
        engine.collection_changed(collection(&[("t0", StreamKind::Text)]));
        engine.streams_selected(gst::Seqnum::next(), &sel(None, None, Some("t0")));
        assert_eq!(engine.applied(), &sel(None, None, Some("t0")));

        // The audio and video inputs report afterwards. No second
        // STREAMS_SELECTED follows, so nothing but this engine can notice.
        engine.collection_changed(collection(&[
            ("a0", StreamKind::Audio),
            ("t0", StreamKind::Text),
        ]));
        engine.collection_changed(collection(&[
            ("v0", StreamKind::Video),
            ("a0", StreamKind::Audio),
            ("t0", StreamKind::Text),
        ]));

        // A kind that is new since the report is ASKED about, not assumed, so
        // `applied` still says what decodebin3 actually confirmed...
        assert_eq!(
            engine.applied().video,
            None,
            "a kind the report predates must not be seeded as applied"
        );
        // ...and the pump therefore has real work, the full default selection,
        // which is what gets decodebin3 to build the A/V chains at all.
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("t0")
            )))
        );
    }

    /// The guard above must stay OFF before decodebin3 has committed to
    /// anything, or every load dispatches an explicit `SELECT_STREAMS` for its
    /// own defaults. Not merely noise, an explicit selection takes decodebin3
    /// out of auto-select for the item, and this crate depends on that
    /// auto-select for text (`collection_changed`'s `text_state_known` note).
    #[test]
    fn a_collection_before_any_report_is_still_seeded_whole() {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(collection(&[("t0", StreamKind::Text)]));
        engine.collection_changed(collection(&[
            ("v0", StreamKind::Video),
            ("a0", StreamKind::Audio),
            ("t0", StreamKind::Text),
        ]));
        assert_eq!(
            engine.applied(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
            "with no report yet, decodebin3's own auto-select still covers this"
        );
        assert_eq!(
            engine.pump(&ctx(true, false)),
            None,
            "and there is nothing to tell it"
        );
    }

    #[test]
    fn empty_selection_is_refused() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Video, TrackTarget::Stream(None));
        engine.request(TrackSlot::Audio, TrackTarget::Stream(None));
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        // All-off resolves to the empty selection, which decodebin3 asserts
        // on: nothing is dispatched.
        assert_eq!(engine.pump(&ctx(true, false)), None);
        assert!(!engine.has_dispatchable_work());
    }

    #[test]
    fn reset_forgets_everything() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        engine.reset();
        assert!(!engine.has_dispatchable_work());
        assert_eq!(engine.applied(), &TrackSelection::default());
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    #[test]
    fn dispatch_failure_clears_the_wait_and_the_flush() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, false)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target);
        engine.dispatch_failed(sn);
        assert!(engine.selecting.is_none());
        // The failed switch's flush must not fire as an orphan.
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    /// A failure report for a dispatch the engine is NOT waiting on must
    /// change nothing at all - the flush least of all. Every select the hands
    /// skip (a superseded core, a stale queue epoch) reports a failure for a
    /// seqnum a load or a swap has already moved past.
    #[test]
    fn a_failure_for_a_superseded_dispatch_leaves_the_live_one_alone() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        // The pump schedules the switch's re-emit flush.
        assert!(engine.pump(&ctx(true, false)).is_some());
        let stale = gst::Seqnum::next();
        engine.selection_dispatched(stale, sel(Some("v0"), Some("a0"), Some("t1")));
        assert!(engine.refresh_wanted, "the switch schedules its re-emit");

        // A newer dispatch replaces it (a caller's newer switch, or the
        // deadline's re-assertion): the old record is superseded, and only
        // the new one is being waited on.
        let live = gst::Seqnum::next();
        engine.selection_dispatched(live, sel(Some("v0"), Some("a0"), Some("t2")));

        engine.dispatch_failed(stale);
        assert!(
            engine.selection_in_flight(live),
            "a stale failure cleared the live wait"
        );
        assert!(
            engine.refresh_wanted,
            "a stale failure cancelled the live switch's re-emit flush"
        );

        // And the live one's own failure still does both.
        engine.dispatch_failed(live);
        assert!(engine.selecting.is_none());
        assert!(!engine.refresh_wanted);
    }

    #[test]
    fn a_superseded_echo_after_a_foreign_overtake_does_not_revert_applied() {
        // The overtake path drops the live tracking (`selecting` is taken and
        // never restored) while the superseded records stay, so the same echo
        // lands with nothing in flight and would be mistaken for a foreign
        // selection - re-arming `subtitle_sid` with the very subtitle the
        // newest dispatch is turning off.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let first = sel(Some("v0"), Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(first.clone()))
        );
        let sn1 = gst::Seqnum::next();
        engine.selection_dispatched(sn1, first.clone());

        // Paused, so the next request supersedes the parked dispatch.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let second = sel(Some("v0"), Some("a0"), None);
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second.clone()))
        );
        let sn2 = gst::Seqnum::next();
        engine.selection_dispatched(sn2, second.clone());

        // decodebin3's own auto-select lands before either confirmation:
        // neither seqnum nor content matches, so the live wait is abandoned
        // and the desire re-asserts. The superseded record for sn1 outlives it.
        let foreign = sel(Some("v0"), Some("a1"), None);
        engine.streams_selected(gst::Seqnum::next(), &foreign);
        assert_eq!(engine.subtitle_sid(), None);

        // Only now does the superseded dispatch's confirmation arrive.
        engine.streams_selected(sn1, &first);
        assert_eq!(
            engine.subtitle_sid(),
            None,
            "a superseded dispatch's late echo must not re-authorize its subtitle"
        );
        assert_eq!(
            engine.applied().subtitle,
            None,
            "a superseded dispatch's late echo must not revert the applied selection"
        );
    }

    /// A supersede chain whose SENT head confirms and whose superseding head
    /// is then refused: the engine must come to rest on what upstream really
    /// applied, not on the refused target and not on the pre-chain guess.
    ///
    /// The chain: paused dispatch sn1 (really sent, parked at decodebin3),
    /// superseded by sn2 (`applied` optimistic, anchor still the pre-chain
    /// state). The resume's flushing seek lands mid-send of sn2, so sn1
    /// confirms and sn2 is refused. Clearing the anchor on the way through the
    /// echo left `dispatch_failed` nothing to roll back to: `applied` kept the
    /// refused target, `desired == applied`, nothing dirty, no wait, no
    /// deadline, and `poll_text_policy` linking a subtitle upstream never
    /// selected.
    #[test]
    fn a_refusal_after_a_superseded_echo_rolls_back_to_the_report() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let first = sel(Some("v0"), Some("a1"), Some("t0"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(first.clone()))
        );
        let sn1 = gst::Seqnum::next();
        engine.selection_dispatched(sn1, first.clone());

        // Paused, so the next request supersedes the parked dispatch.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let second = sel(Some("v0"), Some("a1"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second.clone()))
        );
        let sn2 = gst::Seqnum::next();
        engine.selection_dispatched(sn2, second.clone());

        // The resume makes data flow and the parked sn1 confirms. The live
        // wait is restored, and nothing is adopted.
        engine.streams_selected(sn1, &first);
        assert!(engine.selection_in_flight(sn2), "the live wait was settled");

        // The same flush refused sn2 mid-send: it never left.
        engine.dispatch_failed(sn2);
        assert_eq!(
            engine.applied(),
            &first,
            "the refusal must put back what upstream last confirmed"
        );
    }

    /// The other order of the same chain: the refusal first, the sent head's
    /// confirmation after it.
    ///
    /// `dispatch_failed` rolls back to the pre-chain snapshot, which the sent
    /// sibling has already moved upstream past, and that sibling's own
    /// confirmation is then drained as a stale echo - skipping adoption AND
    /// the divergence check. The desire has to survive that, or the engine
    /// rests with `applied`, the desire and decodebin3 all disagreeing.
    #[test]
    fn a_refusal_inside_a_supersede_chain_keeps_the_desire_dispatchable() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let first = sel(Some("v0"), Some("a1"), Some("t0"));
        assert!(engine.pump(&ctx(true, true)).is_some());
        let sn1 = gst::Seqnum::next();
        engine.selection_dispatched(sn1, first.clone());

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let second = sel(Some("v0"), Some("a1"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second.clone()))
        );
        let sn2 = gst::Seqnum::next();
        engine.selection_dispatched(sn2, second.clone());

        engine.dispatch_failed(sn2);
        assert!(
            engine.has_dispatchable_work(),
            "a refusal with a sent sibling outstanding left nothing to re-assert"
        );

        // The sibling's late confirmation is ours but stale, so it is drained
        // rather than adopted, and it must not take the desire with it.
        engine.streams_selected(sn1, &first);
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second)),
            "the superseding desire was never asked for again"
        );
    }

    #[test]
    fn two_superseded_dispatches_are_recognized_and_then_forgotten() {
        // Three paused dispatches, each superseding the last, so two records
        // are outstanding. decodebin3 folds the first into the second and
        // confirms only the second, which must retire BOTH: a record that
        // outlives its dispatch goes on swallowing genuine foreign selections
        // that happen to name the same streams.
        let mut engine = settled_engine();

        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let first = sel(Some("v0"), Some("a1"), Some("t0"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(first.clone()))
        );
        engine.selection_dispatched(gst::Seqnum::next(), first.clone());

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let second = sel(Some("v0"), Some("a1"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(second.clone()))
        );
        let sn_second = gst::Seqnum::next();
        engine.selection_dispatched(sn_second, second.clone());

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        let third = sel(Some("v0"), Some("a1"), None);
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(third.clone()))
        );
        engine.selection_dispatched(gst::Seqnum::next(), third.clone());

        engine.streams_selected(sn_second, &second);
        assert!(
            engine.selecting.is_some(),
            "a stale echo must not settle the live wait"
        );
        assert_eq!(
            engine.applied(),
            &third,
            "a stale echo must not revert the applied selection"
        );

        // The first dispatch's streams are no longer ours to recognize. A
        // selection naming them now is decodebin3's own and must re-assert
        // the desire instead of being swallowed as an echo that can never
        // come.
        engine.streams_selected(gst::Seqnum::next(), &first);
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(third)),
            "a retired superseded record must not swallow a foreign selection"
        );
    }

    /// What a gapless boundary carries. A stream id names a stream of the item
    /// that just ended, so only an explicit DISABLE is item-independent and
    /// survives. An external desire goes with the rest: the activation removes
    /// the previous generation's inputs (`Job::FinishActivation`).
    #[test]
    fn a_gapless_boundary_carries_explicit_disables_only() {
        struct Case {
            name: &'static str,
            setup: fn(&mut SelectionEngine),
            applied: TrackSelection,
            dispatch: Option<TrackSelection>,
        }
        let cases = [
            Case {
                name: "nothing requested",
                setup: |_| {},
                applied: sel(Some("v0"), Some("a0"), Some("t0")),
                dispatch: None,
            },
            Case {
                name: "an explicit subtitle stream is dropped",
                setup: |engine| {
                    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())))
                },
                applied: sel(Some("v0"), Some("a0"), Some("t0")),
                dispatch: None,
            },
            Case {
                name: "an external subtitle desire is dropped with its input",
                setup: |engine| {
                    engine.request(
                        TrackSlot::Subtitle,
                        TrackTarget::ExternalSubtitle(crate::ExternalSubId(4)),
                    )
                },
                applied: sel(Some("v0"), Some("a0"), Some("t0")),
                dispatch: None,
            },
            Case {
                name: "subtitles off survive",
                setup: |engine| engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None)),
                applied: sel(Some("v0"), Some("a0"), None),
                dispatch: None,
            },
            Case {
                name: "video off survives and re-asserts",
                setup: |engine| engine.request(TrackSlot::Video, TrackTarget::Stream(None)),
                applied: sel(None, Some("a0"), Some("t0")),
                dispatch: Some(sel(None, Some("a0"), None)),
            },
            Case {
                name: "audio off survives",
                setup: |engine| engine.request(TrackSlot::Audio, TrackTarget::Stream(None)),
                applied: sel(Some("v0"), None, Some("t0")),
                dispatch: None,
            },
        ];

        for case in cases {
            let mut engine = settled_engine();
            (case.setup)(&mut engine);
            engine.reset_across_gapless();
            // The activation feeds the next item's collection right after.
            engine.collection_changed(avt_collection());
            assert_eq!(
                engine.applied(),
                &case.applied,
                "applied state after the boundary, case: {}",
                case.name
            );
            assert_eq!(
                engine.pump(&ctx(true, false)),
                case.dispatch.map(Command::SelectStreams),
                "dispatch after the boundary, case: {}",
                case.name
            );
        }
    }

    #[test]
    fn a_report_made_before_text_was_advertised_does_not_turn_text_off() {
        // A report made while the collection held no text stream says NOTHING
        // about the text slot. If it still suppressed seeding, the next
        // dispatch would compose an event that omits text - decodebin3's way
        // of being told to turn it off - and its auto-select never runs again.
        let mut engine = SelectionEngine::new();
        engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
        engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), None));

        // The video and text inputs report and the merged collection grows.
        engine.collection_changed(avt_collection());
        assert_eq!(
            engine.applied().subtitle,
            Some("t0".into()),
            "an empty text slot from a report made before text existed is not \
             a decision to keep text off"
        );

        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a1"),
                Some("t0")
            ))),
            "an audio switch must not deselect the text stream nobody touched"
        );
    }

    /// A decodebin3 stand-in for the exhaustive ordering search below: it
    /// applies verbatim what it is told and confirms with the dispatch's own
    /// seqnum, so any divergence the search reports is the engine's.
    struct Model {
        engine: SelectionEngine,
        /// Dispatched, not yet confirmed, oldest first.
        inflight: std::collections::VecDeque<(gst::Seqnum, TrackSelection)>,
        /// What the ops asked for, mirrored here so the oracle does not read
        /// the state it is checking.
        want_audio: Option<Option<&'static str>>,
        want_subtitle: SubWant,
        /// Whether any subtitle op ran at all, which is what makes an UNSET
        /// desire mean "never touched" rather than "reset by a failed
        /// external".
        subtitle_ever_requested: bool,
        want_video: Option<Option<&'static str>>,
        collection: Vec<CollectionStream>,
        /// Stream ids each attached external input has produced, exactly as
        /// `PumpCtx` carries them.
        externals: Vec<(ExternalSubId, Vec<String>)>,
    }

    /// The subtitle slot's desire as the ops set it, mirrored so the oracle
    /// does not read the state it is checking.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SubWant {
        Unset,
        Off,
        Stream(&'static str),
        External,
    }

    /// The one external input the searches use.
    const EXT: ExternalSubId = ExternalSubId(9);
    /// The text stream that external input advertises once it materializes.
    const EXT_SID: &str = "ext-t";

    /// The ops the search permutes. Each is something the crate really does
    /// to the engine.
    #[derive(Clone, Copy, Debug)]
    enum Op {
        Pump {
            paused: bool,
        },
        /// Deliver the oldest pending confirmation.
        Echo,
        /// decodebin3 auto-selects the collection defaults on its own.
        Foreign,
        WantSubtitle(Option<&'static str>),
        WantAudio(Option<&'static str>),
        WantVideo(Option<&'static str>),
        /// The merged collection grows to the full A/V+text one.
        Grow,
        /// The external input attaches: its pads exist and carry their
        /// stream ids, but decodebin3 has not merged them yet.
        AttachExternal,
        /// decodebin3 merges the external's stream into the collection.
        MaterializeExternal,
        WantExternal,
        /// The input failed or was detached.
        ExternalGone,
    }

    impl Model {
        fn new(collection: Vec<CollectionStream>) -> Self {
            let mut engine = SelectionEngine::new();
            engine.collection_changed(collection.clone());
            Self {
                engine,
                inflight: std::collections::VecDeque::new(),
                want_audio: None,
                want_subtitle: SubWant::Unset,
                subtitle_ever_requested: false,
                want_video: None,
                collection,
                externals: Vec::new(),
            }
        }

        fn ctx(&self, paused: bool) -> PumpCtx {
            PumpCtx {
                gate: SelectionGate {
                    quiet: true,
                    paused,
                    seekable: true,
                },
                externals_attached: !self.externals.is_empty(),
                externals: self.externals.clone(),
                upstream_owns: false,
                now: Instant::now(),
            }
        }

        /// Whether the external desire has everything it needs to resolve:
        /// the input has produced the id AND decodebin3 advertises it as
        /// text.
        fn external_resolves(&self) -> bool {
            self.externals
                .iter()
                .any(|(id, sids)| *id == EXT && sids.iter().any(|s| s == EXT_SID))
                && self
                    .collection
                    .iter()
                    .any(|s| s.sid == EXT_SID && s.kind == StreamKind::Text)
        }

        fn default_of(&self, kind: StreamKind) -> Option<String> {
            self.collection
                .iter()
                .find(|s| s.kind == kind)
                .map(|s| s.sid.clone())
        }

        fn pump_once(&mut self, paused: bool) -> bool {
            let ctx = self.ctx(paused);
            match self.engine.pump(&ctx) {
                None => false,
                // Unreachable in db3-owned mode, which is what this driver
                // models (`ctx` sets `upstream_owns: false`).
                Some(Command::ConfirmApplied(_)) => {
                    unreachable!("ConfirmApplied is scoped to upstream-selection mode")
                }
                Some(Command::SelectStreams(target)) => {
                    assert!(
                        target.video.is_some()
                            || target.audio.is_some()
                            || target.subtitle.is_some(),
                        "an empty selection was dispatched"
                    );
                    let seqnum = gst::Seqnum::next();
                    self.engine.selection_dispatched(seqnum, target.clone());
                    self.inflight.push_back((seqnum, target));
                    true
                }
                Some(Command::RefreshSeek) => {
                    self.engine.refresh_dispatched(gst::Seqnum::next());
                    self.engine.refresh_done();
                    true
                }
            }
        }

        fn echo(&mut self) -> bool {
            match self.inflight.pop_front() {
                Some((seqnum, target)) => {
                    self.engine.streams_selected(seqnum, &target);
                    true
                }
                None => false,
            }
        }

        fn apply(&mut self, op: Op) {
            match op {
                Op::Pump { paused } => {
                    self.pump_once(paused);
                }
                Op::Echo => {
                    self.echo();
                }
                Op::Foreign => {
                    let foreign = TrackSelection {
                        video: self.default_of(StreamKind::Video),
                        audio: self.default_of(StreamKind::Audio),
                        subtitle: self.default_of(StreamKind::Text),
                    };
                    self.engine.streams_selected(gst::Seqnum::next(), &foreign);
                }
                Op::WantSubtitle(sid) => {
                    self.want_subtitle = match sid {
                        Some(sid) => SubWant::Stream(sid),
                        None => SubWant::Off,
                    };
                    self.subtitle_ever_requested = true;
                    self.engine.request(
                        TrackSlot::Subtitle,
                        TrackTarget::Stream(sid.map(str::to_string)),
                    );
                }
                Op::WantAudio(sid) => {
                    self.want_audio = Some(sid);
                    self.engine.request(
                        TrackSlot::Audio,
                        TrackTarget::Stream(sid.map(str::to_string)),
                    );
                }
                Op::WantVideo(sid) => {
                    self.want_video = Some(sid);
                    self.engine.request(
                        TrackSlot::Video,
                        TrackTarget::Stream(sid.map(str::to_string)),
                    );
                }
                Op::Grow => {
                    // The external's stream, once merged, stays merged.
                    let ext = self.collection.iter().any(|s| s.sid == EXT_SID).then(|| {
                        CollectionStream {
                            sid: EXT_SID.into(),
                            kind: StreamKind::Text,
                        }
                    });
                    self.collection = avt_collection();
                    self.collection.extend(ext);
                    self.engine.collection_changed(self.collection.clone());
                }
                Op::AttachExternal => {
                    if !self.externals.iter().any(|(id, _)| *id == EXT) {
                        self.externals.push((EXT, vec![EXT_SID.to_string()]));
                    }
                }
                Op::MaterializeExternal => {
                    if !self.collection.iter().any(|s| s.sid == EXT_SID) {
                        self.collection.push(CollectionStream {
                            sid: EXT_SID.into(),
                            kind: StreamKind::Text,
                        });
                        self.engine.collection_changed(self.collection.clone());
                    }
                }
                Op::WantExternal => {
                    self.want_subtitle = SubWant::External;
                    self.subtitle_ever_requested = true;
                    self.engine
                        .request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
                }
                Op::ExternalGone => {
                    self.externals.retain(|(id, _)| *id != EXT);
                    self.collection.retain(|s| s.sid != EXT_SID);
                    if self.want_subtitle == SubWant::External {
                        // `external_gone` resets the desire to UNSET, and
                        // whatever was showing keeps showing.
                        self.want_subtitle = SubWant::Unset;
                    }
                    self.engine.external_gone(EXT);
                }
            }
        }

        /// Run the pipeline to a standstill: every confirmation delivered,
        /// every dispatch the engine still wants made and confirmed. Bounded,
        /// so a re-assertion that ping-pongs fails here rather than hanging.
        fn settle(&mut self, trace: &[Op]) {
            for _ in 0..64 {
                while self.echo() {}
                if !self.pump_once(false) && self.inflight.is_empty() {
                    assert!(
                        !self.engine.has_dispatchable_work(),
                        "settled with work still pending, trace {trace:?}"
                    );
                    return;
                }
            }
            panic!("the engine never converged, trace {trace:?}");
        }

        fn has(&self, sid: &str) -> bool {
            self.collection.iter().any(|s| s.sid == sid)
        }

        fn has_video(&self) -> bool {
            self.collection.iter().any(|s| s.kind == StreamKind::Video)
        }

        /// Every explicit desire the current collection CAN express must be
        /// the applied state once everything has settled.
        fn check_desires(&self, trace: &[Op]) {
            let applied = self.engine.applied();
            match self.want_video {
                Some(Some(sid)) if self.has(sid) => assert_eq!(
                    applied.video.as_deref(),
                    Some(sid),
                    "the video desire was not applied, trace {trace:?}, applied {applied:?}"
                ),
                Some(None) => assert_eq!(
                    applied.video, None,
                    "the video disable was not applied, trace {trace:?}, applied {applied:?}"
                ),
                _ => {}
            }
            match self.want_audio {
                Some(Some(sid)) if self.has(sid) => assert_eq!(
                    applied.audio.as_deref(),
                    Some(sid),
                    "the audio desire was not applied, trace {trace:?}, applied {applied:?}"
                ),
                Some(None) => assert_eq!(
                    applied.audio, None,
                    "the audio disable was not applied, trace {trace:?}, applied {applied:?}"
                ),
                _ => {}
            }
            // Text cannot be presented without video, so neither a collection
            // without a video stream nor a deselected video slot is something
            // a subtitle desire can be held to.
            let text_presentable = self.has_video() && applied.video.is_some();
            match self.want_subtitle {
                SubWant::Stream(sid) if self.has(sid) && text_presentable => assert_eq!(
                    applied.subtitle.as_deref(),
                    Some(sid),
                    "the subtitle desire was not applied, trace {trace:?}, applied {applied:?}"
                ),
                SubWant::External if self.external_resolves() && text_presentable => assert_eq!(
                    applied.subtitle.as_deref(),
                    Some(EXT_SID),
                    "the external subtitle desire was not applied, trace {trace:?}, \
                     applied {applied:?}"
                ),
                SubWant::Off => assert_eq!(
                    applied.subtitle, None,
                    "the subtitle disable was not applied, trace {trace:?}, applied {applied:?}"
                ),
                // An UNSET desire follows the pipeline, which auto-selects the
                // first stream of each kind, so ending with the text slot off
                // means a dispatch composed it away without anyone asking.
                // Only checked when no subtitle op ran at all: a desire RESET
                // by a failed external legitimately leaves an earlier disable.
                SubWant::Unset
                    if !self.subtitle_ever_requested
                        && text_presentable
                        && self.default_of(StreamKind::Text).is_some() =>
                {
                    assert!(
                        applied.subtitle.is_some(),
                        "text was deselected without a request, trace {trace:?}, \
                         applied {applied:?}"
                    )
                }
                _ => {}
            }
        }
    }

    /// Exhaustive over every ordering of a small op alphabet: whatever the
    /// interleaving of requests, confirmations, foreign auto-selects and
    /// collection growth, the engine must converge and apply every desire the
    /// collection can express. Catches lost desires, stale echoes overwriting
    /// the applied state, and re-assertion loops.
    #[test]
    fn every_ordering_converges_on_the_desired_state() {
        const OPS: &[Op] = &[
            Op::Pump { paused: false },
            Op::Pump { paused: true },
            Op::Echo,
            Op::Foreign,
            Op::WantSubtitle(Some("t1")),
            Op::WantSubtitle(None),
            Op::WantAudio(Some("a1")),
            Op::WantVideo(None),
            Op::Grow,
        ];
        // Three starting points: a collection still growing (audio only,
        // decodebin3 has merged one input), a media that genuinely has no
        // video stream and never will, and the full one.
        let starts = [
            collection(&[("a0", StreamKind::Audio)]),
            collection(&[
                ("a0", StreamKind::Audio),
                ("a1", StreamKind::Audio),
                ("t0", StreamKind::Text),
                ("t1", StreamKind::Text),
            ]),
            avt_collection(),
        ];

        for start in starts {
            search(OPS, &start, 6);
        }
    }

    /// The same exhaustive treatment for the external-subtitle desire, whose
    /// resolution needs TWO independent things to line up (the input producing
    /// its stream id, decodebin3 merging that id in as text) plus a reset path
    /// when the input dies.
    #[test]
    fn every_external_ordering_converges_on_the_desired_state() {
        const OPS: &[Op] = &[
            Op::Pump { paused: false },
            Op::Pump { paused: true },
            Op::Echo,
            Op::Foreign,
            Op::AttachExternal,
            Op::MaterializeExternal,
            Op::WantExternal,
            Op::ExternalGone,
            Op::WantSubtitle(None),
        ];
        search(OPS, &avt_collection(), 5);
    }

    /// Every op sequence up to `max_len`, each run against a fresh engine and
    /// then settled and checked.
    fn search(ops: &[Op], start: &[CollectionStream], max_len: usize) {
        for len in 1..=max_len {
            let total = ops.len().pow(len as u32);
            for n in 0..total {
                let mut trace = Vec::with_capacity(len);
                let mut rest = n;
                for _ in 0..len {
                    trace.push(ops[rest % ops.len()]);
                    rest /= ops.len();
                }
                let mut model = Model::new(start.to_vec());
                for op in &trace {
                    model.apply(*op);
                }
                model.settle(&trace);
                model.check_desires(&trace);
            }
        }
    }

    #[test]
    fn a_held_back_subtitle_must_not_starve_the_other_slots() {
        // The holdback keys off "the collection has no video", which is also
        // true for the whole life of a media that simply has none: every later
        // resolve would hit it and drop every other slot's request too.
        let mut engine = SelectionEngine::new();
        engine.collection_changed(collection(&[
            ("a0", StreamKind::Audio),
            ("a1", StreamKind::Audio),
            ("t0", StreamKind::Text),
            ("t1", StreamKind::Text),
        ]));
        engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));

        // Nothing can honour this while the collection has no video, so it
        // is held back rather than answered by an event that turns the
        // showing subtitle off.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        assert_eq!(engine.pump(&ctx(true, false)), None);

        // An audio switch must go out, carrying the subtitle: a collection
        // with no video id cannot express "video on" either way, and stripping
        // the text stream would be a second deselect nobody asked for.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(None, Some("a1"), Some("t1")))),
            "the holdback must defer only the subtitle, not every other slot"
        );
    }

    // The engine holds only the ADVISORY half of a deadline: when a wait ran
    // out, never what to do about it. The cases below inject both the clock
    // and the worker's decisions.

    /// The deadline every case below arms with, and the re-arm period handed
    /// to `due_deadlines`. Production values: `crate::SELECTION_DEADLINE` /
    /// `crate::REFRESH_DEADLINE`.
    const DEADLINE: Duration = Duration::from_secs(10);
    /// Comfortably past [`DEADLINE`].
    const LATE: Duration = Duration::from_secs(11);
    /// What the worker's probe finds a never-applied selection left playing:
    /// the state `settled_engine` starts from.
    fn reality() -> TrackSelection {
        sel(Some("v0"), Some("a0"), Some("t0"))
    }

    /// An advisory deadline is exactly as alive as the dispatch it names, and
    /// nothing disarms it: the next look drops an advisory whose seqnum no
    /// longer names a live wait.
    #[test]
    fn a_deadline_fires_only_while_its_dispatch_is_in_flight() {
        let t0 = Instant::now();
        let mut engine = settled_engine();
        let seqnum = gst::Seqnum::next();
        let target = sel(Some("v0"), Some("a1"), Some("t0"));
        engine.selection_dispatched(seqnum, target.clone());
        engine.arm_selection_deadline(seqnum, t0 + DEADLINE, 2);

        // Before the due time, nothing.
        assert!(
            engine
                .due_deadlines(t0 + Duration::from_secs(1), DEADLINE)
                .is_empty()
        );
        // Still in flight when it comes due: the positive control.
        assert_eq!(
            engine.due_deadlines(t0 + LATE, DEADLINE),
            vec![DeadlineFire::Selection(seqnum)]
        );

        // The confirmation arrives after all. Nobody tells the advisory.
        engine.streams_selected(seqnum, &target);
        assert!(
            engine
                .due_deadlines(t0 + Duration::from_secs(60), DEADLINE)
                .is_empty(),
            "a confirmed dispatch still had a live deadline"
        );
        assert!(
            engine
                .due_deadlines(t0 + Duration::from_secs(600), DEADLINE)
                .is_empty(),
            "the dead advisory was re-armed instead of dropped"
        );
    }

    /// The two paths that abandon in-flight work without any confirmation -
    /// a fresh collection and a load reset - take the deadlines with them.
    /// The sibling of `new_collection_invalidates_in_flight_work`.
    #[test]
    fn collection_change_and_reset_disarm_deadlines() {
        let arm = |engine: &mut SelectionEngine, t0: Instant| {
            let select = gst::Seqnum::next();
            engine.selection_dispatched(select, sel(Some("v0"), Some("a1"), Some("t0")));
            engine.arm_selection_deadline(select, t0 + DEADLINE, 2);
            let refresh = gst::Seqnum::next();
            engine.refresh_dispatched(refresh);
            engine.arm_refresh_deadline(refresh, t0 + DEADLINE);
        };

        let t0 = Instant::now();
        let mut engine = settled_engine();
        arm(&mut engine, t0);
        engine.collection_changed(avt_collection());
        assert!(
            engine.due_deadlines(t0 + LATE, DEADLINE).is_empty(),
            "a new collection left deadlines armed against waits it abandoned"
        );

        let mut engine = settled_engine();
        arm(&mut engine, t0);
        engine.reset();
        assert!(
            engine.due_deadlines(t0 + LATE, DEADLINE).is_empty(),
            "a load reset left the previous item's deadlines armed"
        );
    }

    /// A fire re-arms rather than clears, so a wait that is still there gets
    /// looked at again - but at most once per re-arm period, so a worker
    /// parked behind a long job never returns to a queue of fires.
    #[test]
    fn a_fired_deadline_rearms_and_does_not_storm() {
        let t0 = Instant::now();
        let mut engine = settled_engine();
        let seqnum = gst::Seqnum::next();
        engine.selection_dispatched(seqnum, sel(Some("v0"), Some("a1"), Some("t0")));
        engine.arm_selection_deadline(seqnum, t0 + DEADLINE, 2);

        assert_eq!(
            engine.due_deadlines(t0 + LATE, DEADLINE),
            vec![DeadlineFire::Selection(seqnum)]
        );
        // Inside the re-arm window, however often the tick looks.
        for at in [
            t0 + LATE + Duration::from_millis(1),
            t0 + LATE + Duration::from_secs(5),
            t0 + LATE + DEADLINE - Duration::from_millis(1),
        ] {
            assert!(
                engine.due_deadlines(at, DEADLINE).is_empty(),
                "a second fire inside one re-arm period"
            );
        }
        // And once more past it.
        assert_eq!(
            engine.due_deadlines(t0 + LATE + DEADLINE, DEADLINE),
            vec![DeadlineFire::Selection(seqnum)]
        );
    }

    /// A timed-out dispatch becomes a SUPERSEDED record, not a forgotten one:
    /// a confirmation that turns up late is then ours-but-stale, and neither
    /// settles the dispatch that replaced it nor rewinds `applied`.
    #[test]
    fn timed_out_dispatch_becomes_a_superseded_echo() {
        let t0 = Instant::now();
        let mut engine = settled_engine();
        let first = gst::Seqnum::next();
        let first_target = sel(Some("v0"), Some("a1"), Some("t0"));
        engine.selection_dispatched(first, first_target.clone());
        engine.arm_selection_deadline(first, t0 + DEADLINE, 2);
        assert_eq!(
            engine.due_deadlines(t0 + LATE, DEADLINE),
            vec![DeadlineFire::Selection(first)]
        );
        assert_eq!(
            engine.selection_timed_out(first),
            TimeoutOutcome::Retry {
                target: first_target.clone(),
                retries_left: 1,
            }
        );

        // The next dispatch. A user request composed onto the same slot set
        // in the meantime, so the two targets differ and a rewind would show.
        let second = gst::Seqnum::next();
        let second_target = sel(Some("v0"), Some("a1"), Some("t1"));
        engine.selection_dispatched(second, second_target.clone());
        engine.arm_selection_deadline(second, t0 + LATE + DEADLINE, 1);

        // And NOW the first dispatch's confirmation arrives.
        engine.streams_selected(first, &first_target);
        assert_eq!(
            engine.selecting_target(second),
            Some(second_target.clone()),
            "the stale echo settled the dispatch that superseded it"
        );
        assert_eq!(
            engine.applied(),
            &second_target,
            "the stale echo rewound the applied selection"
        );
    }

    /// The end of the retry ladder: reporting reality rather than the request
    /// must leave the engine CONVERGED, or it re-dispatches a selection the
    /// pipeline has already refused three times, forever.
    #[test]
    fn retries_exhaust_into_a_converged_give_up() {
        let t0 = Instant::now();
        let mut engine = settled_engine();
        // Two slots, so the give-up's neutralization is pinned on both the
        // plain-stream and the subtitle desire.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a1"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );

        let mut now = t0;
        let mut seqnum = gst::Seqnum::next();
        engine.selection_dispatched(seqnum, target.clone());
        engine.arm_selection_deadline(seqnum, now + DEADLINE, 2);
        // Two retries, modelling the worker: probe says not applied,
        // re-dispatch under a fresh seqnum and re-arm with what is left.
        for expected in [1, 0] {
            now += LATE;
            assert_eq!(
                engine.due_deadlines(now, DEADLINE),
                vec![DeadlineFire::Selection(seqnum)]
            );
            let TimeoutOutcome::Retry {
                target,
                retries_left,
            } = engine.selection_timed_out(seqnum)
            else {
                panic!("the retries were exhausted early");
            };
            assert_eq!(retries_left, expected);
            seqnum = gst::Seqnum::next();
            engine.selection_dispatched(seqnum, target);
            engine.arm_selection_deadline(seqnum, now + DEADLINE, retries_left);
        }

        now += LATE;
        assert_eq!(
            engine.due_deadlines(now, DEADLINE),
            vec![DeadlineFire::Selection(seqnum)]
        );
        assert_eq!(
            engine.selection_timed_out(seqnum),
            TimeoutOutcome::Exhausted {
                target: target.clone()
            }
        );

        assert!(
            engine.selection_gave_up(seqnum, &reality()),
            "nothing overtook this give-up, so it must have been adopted"
        );
        assert_eq!(engine.applied(), &reality());
        // Converged: the unhonourable desire is UNSET, not rewritten and not
        // turned off, so nothing re-asserts and nothing was torn down.
        assert_eq!(engine.pump(&ctx(true, false)), None);
        assert!(!engine.has_dispatchable_work());
        // Convergence has to survive the next event: a cleared `dirty` alone
        // would be re-set by any foreign report, and a desire still pointing
        // at the refused track would re-assert there.
        engine.streams_selected(gst::Seqnum::next(), &reality());
        assert_eq!(engine.pump(&ctx(true, false)), None);

        // Still a live engine: the next request dispatches normally, against
        // the adopted reality rather than against the abandoned request.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(
                Some("v0"),
                Some("a1"),
                Some("t0")
            )))
        );
    }

    /// The refreshing latch, and its cure. `refreshing` blocks EVERY dispatch
    /// and is cleared only by a top-level ASYNC_DONE, so a swallowed one
    /// freezes the whole selection channel for the rest of the item.
    #[test]
    fn refresh_deadline_unlatches_the_pump() {
        let t0 = Instant::now();
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        let target = sel(Some("v0"), Some("a1"), Some("t0"));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(target.clone()))
        );
        let select = gst::Seqnum::next();
        engine.selection_dispatched(select, target.clone());
        engine.streams_selected(select, &target);

        // The switch schedules the re-emit flush, which dispatches next.
        assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
        let refresh = gst::Seqnum::next();
        engine.refresh_dispatched(refresh);
        engine.arm_refresh_deadline(refresh, t0 + DEADLINE);

        // Its ASYNC_DONE never comes. Every later request is now blocked.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a0".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            None,
            "an in-flight refresh must block dispatch; this test proves nothing otherwise"
        );

        // The deadline fires and the worker (modelled here) fails the refresh.
        assert_eq!(
            engine.due_deadlines(t0 + LATE, DEADLINE),
            vec![DeadlineFire::Refresh(refresh)]
        );
        engine.refresh_failed(refresh);
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(reality())),
            "the request stranded behind the swallowed ASYNC_DONE never dispatched"
        );
    }

    /// Seeded splitmix64, mirroring `fcasttest::prng`: a seed fully
    /// determines a run, and no test here ever draws from a thread-local RNG.
    struct Prng(u64);

    impl Prng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Liveness, randomized: no reachable interleaving leaves a wait that
    /// nothing will ever look at again.
    ///
    /// 1. INVARIANT, after every step: an in-flight selection or refresh has a
    ///    matching advisory. A wait without one is a latch.
    /// 2. LIVENESS: from wherever the random ops left it, a BOUNDED number of
    ///    fires (worker decisions modelled) leaves nothing in flight and the
    ///    engine still dispatching for a fresh request.
    ///
    /// The op alphabet includes the swallow this all exists for: a
    /// confirmation, or an ASYNC_DONE, that simply never arrives.
    #[test]
    fn no_reachable_state_leaves_a_wait_without_a_deadline() {
        const RUNS: u64 = 400;
        const OPS_PER_RUN: u64 = 14;
        /// Generous: a retry ladder is three fires and a refresh is one.
        const FIRE_BOUND: usize = 12;

        for seed in 0..RUNS {
            let mut prng = Prng(seed);
            let mut engine = settled_engine();
            let mut now = Instant::now();
            let mut trace: Vec<&str> = Vec::new();

            let check = |engine: &SelectionEngine, trace: &[&str]| {
                if let Some((seqnum, _)) = &engine.selecting {
                    let armed = engine
                        .selection_deadline
                        .is_some_and(|deadline| deadline.seqnum == *seqnum);
                    assert!(
                        armed,
                        "a selection is in flight with no deadline: {trace:?}"
                    );
                }
                if let Some(seqnum) = engine.refreshing {
                    let armed = engine
                        .refresh_deadline
                        .is_some_and(|(tracked, _)| tracked == seqnum);
                    assert!(armed, "a refresh is in flight with no deadline: {trace:?}");
                }
            };

            for _ in 0..OPS_PER_RUN {
                match prng.below(8) {
                    0 => {
                        trace.push("want-audio");
                        let sid = if prng.next() & 1 == 0 { "a0" } else { "a1" };
                        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some(sid.into())));
                    }
                    1 => {
                        trace.push("want-subtitle");
                        let sid = match prng.below(3) {
                            0 => Some("t0".to_string()),
                            1 => Some("t1".to_string()),
                            _ => None,
                        };
                        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(sid));
                    }
                    2 => {
                        trace.push("pump");
                        // The pump and the arming that follows it are ONE
                        // step, exactly as `pump_selection` does them under
                        // one engine lock.
                        let paused = prng.next() & 1 == 0;
                        match engine.pump(&ctx(true, paused)) {
                            Some(Command::SelectStreams(target)) => {
                                let seqnum = gst::Seqnum::next();
                                engine.selection_dispatched(seqnum, target);
                                engine.arm_selection_deadline(seqnum, now + DEADLINE, 2);
                            }
                            Some(Command::RefreshSeek) => {
                                let seqnum = gst::Seqnum::next();
                                engine.refresh_dispatched(seqnum);
                                engine.arm_refresh_deadline(seqnum, now + DEADLINE);
                            }
                            // Never reached in db3-owned mode, and asserting
                            // that here would pin an unrelated contract.
                            Some(Command::ConfirmApplied(_)) | None => {}
                        }
                    }
                    3 => {
                        trace.push("confirm");
                        if let Some((seqnum, target)) = engine.selecting.clone() {
                            engine.streams_selected(seqnum, &target);
                        }
                    }
                    4 => {
                        // The whole point of the phase: the message dies.
                        trace.push("swallow");
                    }
                    5 => {
                        trace.push("collection");
                        engine.collection_changed(avt_collection());
                    }
                    6 => {
                        trace.push("reset");
                        engine.reset();
                        engine.collection_changed(avt_collection());
                    }
                    _ => {
                        trace.push("fire");
                        now += LATE;
                        for fire in engine.due_deadlines(now, DEADLINE) {
                            worker(&mut engine, fire, now);
                        }
                    }
                }
                check(&engine, &trace);
            }

            // Liveness. Nothing but time and the modelled worker from here.
            let mut fires = 0;
            while engine.selecting.is_some() || engine.refreshing.is_some() {
                assert!(
                    fires < FIRE_BOUND,
                    "a wait survived {FIRE_BOUND} deadline fires: {trace:?}"
                );
                fires += 1;
                now += LATE;
                let due = engine.due_deadlines(now, DEADLINE);
                assert!(
                    !due.is_empty(),
                    "something is in flight but no deadline came due: {trace:?}"
                );
                for fire in due {
                    worker(&mut engine, fire, now);
                }
                check(&engine, &trace);
            }

            // And the engine still works. A fresh collection puts it on known
            // ground, then a request for the audio track that is NOT applied
            // must dispatch.
            engine.collection_changed(avt_collection());
            let wanted = if engine.applied().audio.as_deref() == Some("a0") {
                "a1"
            } else {
                "a0"
            };
            engine.request(TrackSlot::Audio, TrackTarget::Stream(Some(wanted.into())));
            let command = engine.pump(&ctx(true, false));
            let Some(Command::SelectStreams(target)) = command else {
                panic!("the engine stopped dispatching after {trace:?}: {command:?}");
            };
            assert_eq!(target.audio.as_deref(), Some(wanted));
        }
    }

    /// The worker's half of a deadline fire, as the unit tests model it: the
    /// probe never finds the selection applied (the pessimistic case, which
    /// is the one that has to terminate), and a refresh that ran out failed.
    fn worker(engine: &mut SelectionEngine, fire: DeadlineFire, now: Instant) {
        match fire {
            DeadlineFire::Selection(seqnum) => match engine.selection_timed_out(seqnum) {
                TimeoutOutcome::NotInFlight => {}
                TimeoutOutcome::Retry {
                    target,
                    retries_left,
                } => {
                    let retry = gst::Seqnum::next();
                    engine.selection_dispatched(retry, target);
                    engine.arm_selection_deadline(retry, now + DEADLINE, retries_left);
                }
                TimeoutOutcome::Exhausted { .. } => {
                    // Nothing here can overtake the give-up, so the adopted
                    // answer is not interesting to this model.
                    let _ = engine.selection_gave_up(seqnum, &reality());
                }
            },
            DeadlineFire::Refresh(seqnum) => engine.refresh_failed(seqnum),
        }
    }

    /// With NOTHING refreshing, `refresh_superseded` is false (a caller may
    /// force a refresh the engine never tracked, and that job must run) while
    /// its `==` sibling `refresh_in_flight` is also false. The two differ
    /// ONLY on the None case, and collapsing them flips one of these.
    #[test]
    fn no_refresh_in_flight_is_not_supersession() {
        let engine = SelectionEngine::default();
        let seqnum = gst::Seqnum::next();
        assert!(!engine.refresh_superseded(seqnum));
        assert!(!engine.refresh_in_flight(seqnum));
    }

    /// With a refresh TRACKED, the two split exactly on the seqnum: the
    /// tracked one is in flight and not superseded, any other is superseded
    /// and not in flight.
    #[test]
    fn a_tracked_refresh_splits_in_flight_from_superseded_by_seqnum() {
        let mut engine = SelectionEngine::default();
        let ours = gst::Seqnum::next();
        let other = gst::Seqnum::next();
        engine.refresh_dispatched(ours);
        assert!(engine.refresh_in_flight(ours));
        assert!(!engine.refresh_superseded(ours));
        assert!(!engine.refresh_in_flight(other));
        assert!(engine.refresh_superseded(other));
        // A failure report for a foreign seqnum clears nothing.
        engine.refresh_failed(other);
        assert!(engine.refresh_in_flight(ours));
        // Ours does.
        engine.refresh_failed(ours);
        assert!(!engine.refresh_in_flight(ours));
        assert!(!engine.refresh_superseded(ours));
    }

    /// `selection_pending` and `selecting_seqnum` read the same live wait:
    /// empty until a dispatch, naming it while it waits, empty again once its
    /// confirmation settles it.
    #[test]
    fn the_live_wait_is_visible_from_dispatch_to_confirmation() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        assert!(!engine.selection_pending());
        assert_eq!(engine.selecting_seqnum(), None);

        let seqnum = gst::Seqnum::next();
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        engine.selection_dispatched(seqnum, target.clone());
        assert!(engine.selection_pending());
        assert_eq!(engine.selecting_seqnum(), Some(seqnum));

        engine.streams_selected(seqnum, &target);
        assert!(!engine.selection_pending());
        assert_eq!(engine.selecting_seqnum(), None);
    }

    /// An explicit subtitle OFF is a desire, not an applied state: an applied
    /// text slot that happens to be empty must not read as one.
    #[test]
    fn an_empty_applied_text_slot_is_not_an_explicit_off() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), None));
        assert_eq!(engine.subtitle_sid(), None, "applied text is empty");
        assert!(
            !engine.subtitle_explicitly_off(),
            "nobody asked for off; decodebin3 just reported none"
        );

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
        assert!(engine.subtitle_explicitly_off());

        // Any other desire ends the explicit off.
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert!(!engine.subtitle_explicitly_off());
        engine.request(
            TrackSlot::Subtitle,
            TrackTarget::ExternalSubtitle(ExternalSubId(4)),
        );
        assert!(!engine.subtitle_explicitly_off());
    }

    /// `desires_external` answers for the DESIRED external only: the right
    /// id, not another id, not a stream desire, and not after the external
    /// failed and the desire was dropped.
    #[test]
    fn desires_external_names_exactly_the_desired_id() {
        const EXT: ExternalSubId = ExternalSubId(7);
        const OTHER: ExternalSubId = ExternalSubId(8);
        let mut engine = SelectionEngine::default();
        assert!(!engine.desires_external(EXT));

        engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
        assert!(engine.desires_external(EXT));
        assert!(!engine.desires_external(OTHER));

        engine.external_gone(EXT);
        assert!(!engine.desires_external(EXT), "the failed desire is dropped");

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert!(!engine.desires_external(EXT));
    }
}
