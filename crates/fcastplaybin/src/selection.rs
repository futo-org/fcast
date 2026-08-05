//! The declarative track-selection engine.
//!
//! Callers state what each slot (video/audio/subtitle) SHOULD show
//! ([`TrackTarget`]); the engine owns everything between that intent and a
//! confirmed decodebin3 selection: dispatch serialization, seqnum/content
//! confirmation, re-assertion when decodebin3's own collection-default
//! auto-select stomps an explicit choice, and parking a subtitle request on
//! an external input whose stream has not materialized yet.
//!
//! Grown out of the receiver's `TrackOps` queue (its serialization and
//! confirmation rules are ported verbatim, tests included) plus the
//! application's post-attach enforcement (`FcastSubDesire`), which the
//! declarative model subsumes: "attach but keep showing what shows now" is
//! simply the desired state already held, re-asserted when the fresh
//! collection's auto-select diverges from it.
//!
//! # Dispatch protocol
//!
//! A `SELECT_STREAMS` is confirmed by a `STREAMS_SELECTED` message carrying
//! the event's seqnum (decodebin3 stamps it), so selections settle by exact
//! seqnum match, or by the reported selection matching what was asked for
//! (a superseded/coalesced/no-op request folds into another event's
//! seqnum). A refresh seek completes with a top-level `ASYNC_DONE`, which
//! CANNOT be seqnum-matched (`GstBin` aggregates with a fresh seqnum), so
//! the refresh settles by exclusivity: at most one async-causing operation
//! is in flight, making the next ASYNC_DONE its completion. New work is
//! held back until the pipeline is quiet, because overlapping re-prerolls
//! deadlock the pipeline, the failure mode all of this prevents.
//!
//! Paused is special (streaming threads are parked after preroll): a
//! dispatched selection won't confirm until data flows, so a parked
//! selection neither blocks a superseding one (no re-preroll to overlap
//! with) nor blocks the refresh flush, which is exactly what makes data
//! flow and the selection apply.
//!
//! # Division of labor
//!
//! The engine is pure state: it never touches the pipeline. Recording
//! happens where the crate translates bus traffic (before the caller sees
//! the event), dispatch happens only in [`FcastPlaybin::pump_selection`],
//! called by the OWNER of the transport state machine at its safe points.
//! The gate ([`SelectionGate`]) stays caller-provided on purpose: only the
//! transport machine knows about queued seeks and the mid-cascade
//! one-instant-quiet window that a pipeline query alone cannot see (a
//! selection dispatched into the seek dance's quiet instant once wedged
//! the pipeline for good).
//!
//! [`FcastPlaybin::pump_selection`]: crate::FcastPlaybin::pump_selection

use tracing::debug;

use crate::{ExternalSubId, StreamKind};

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
    /// the input's stream appears in the advertised collection, then
    /// resolves to its stream id. Cleared automatically if the input fails
    /// or is detached.
    ExternalSubtitle(ExternalSubId),
}

/// A full selection, keyed by GStreamer stream id (`None` = slot disabled).
/// Stream ids are stable across collections of the same load, unlike
/// stream-list indices, so a selection never needs remapping when a new
/// collection arrives. Indices exist only at the caller's protocol/GUI
/// edge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackSelection {
    pub video: Option<String>,
    pub audio: Option<String>,
    pub subtitle: Option<String>,
}

/// The transport conditions under which the engine may dispatch, snapshot
/// by the caller (the owner of the transport state machine) right before
/// [`FcastPlaybin::pump_selection`].
///
/// [`FcastPlaybin::pump_selection`]: crate::FcastPlaybin::pump_selection
#[derive(Debug, Clone, Copy)]
pub struct SelectionGate {
    /// No async state change in progress and the transport machine is
    /// settled running (not loading/buffering/seeking).
    pub quiet: bool,
    /// Settled paused: streaming threads are parked after preroll, so a
    /// dispatched selection won't apply (or confirm) until data flows
    /// again.
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
    /// `STREAMS_SELECTED` of its own: the only confirmations a caller can ever
    /// see in this mode are the ones this crate produces. See
    /// [`Command::ConfirmApplied`].
    pub(crate) upstream_owns: bool,
}

/// What the pump decided to dispatch next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    SelectStreams(TrackSelection),
    RefreshSeek,
    /// A USER REQUEST that is already satisfied: nothing to dispatch, and in
    /// upstream-selection mode nothing else will ever tell the caller so.
    ///
    /// Without this the receiver never relays a `StreamsSelected` and never
    /// sends SetTrackIds, so the UI keeps showing the previous track while the
    /// requested one is already active (field: attaching a subtitle with select
    /// on a paused adaptive item). The caller-visible answer is the same
    /// `StreamsSelected` a dispatch would have produced, naming the applied set
    /// including the crate-merged subtitle sid.
    ///
    /// Fires once per user request, never for a `dirty` set by a collection
    /// change, a foreign report or engine-internal reseeding: those are not
    /// questions anyone asked.
    ConfirmApplied(TrackSelection),
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
    /// The selection currently applied (or optimistically in flight):
    /// adopted verbatim from `STREAMS_SELECTED`, filtered + default-seeded
    /// per collection, optimistically set at dispatch so a second change
    /// arriving before confirmation composes instead of reverting.
    applied: TrackSelection,
    /// The advertised collection (empty before the first one of a load).
    collection: Vec<CollectionStream>,
    /// In-flight `SELECT_STREAMS`: the seqnum its `STREAMS_SELECTED` will
    /// carry and the selection asked of decodebin3. Settles on an exact
    /// seqnum match OR on a report matching this selection. A report
    /// matching NEITHER means decodebin3 selected on its own (its
    /// collection-default auto-select racing ours): the desire is marked
    /// dirty for re-dispatch. No timeout: a slow selection stays in flight
    /// until its confirmation arrives.
    selecting: Option<(gst::Seqnum, TrackSelection)>,
    /// Dispatches superseded before confirming (the paused supersede path),
    /// oldest first. Their late confirmations are our own stale echoes,
    /// recognized here by seqnum or content so they neither settle the live
    /// request nor masquerade as an overtaking foreign selection. Cleared on
    /// settle, and drained up to each match (see `take_superseded_echo`).
    superseded: Vec<(gst::Seqnum, TrackSelection)>,
    /// In-flight refresh seek, settled by the next `ASYNC_DONE`
    /// (attribution by exclusivity, see the module docs).
    refreshing: Option<gst::Seqnum>,
    /// A re-emit flush is due once the pipeline settles: a sparse text
    /// track doesn't render its current cue after a switch until the next
    /// cue boundary, so a flushing seek to the current position re-emits
    /// it. Safety is re-decided from the ctx at every pump.
    refresh_wanted: bool,
    /// The desired state may diverge from the applied one: set by requests,
    /// by collection changes (external materialization, dropped sids) and
    /// by overtaking foreign selections. Cleared when the pump finds them
    /// converged or dispatches. Dispatching ONLY on fresh events keeps the
    /// engine convergent (a selection decodebin3 refuses cannot ping-pong).
    dirty: bool,
    /// A USER REQUEST has not been answered to the caller yet. Set only by
    /// [`Self::request`], consumed by the pump when it either dispatches (the
    /// dispatch's confirmation is the answer) or finds the request already
    /// satisfied (see [`Command::ConfirmApplied`]). Deliberately survives every
    /// None-returning gate: a request made while the transport is not quiet is
    /// answered at the first pump that can answer it, not dropped.
    unanswered_request: bool,
    /// The last pump could not resolve the subtitle desire because its
    /// external input has not produced its text stream yet. Unlike every
    /// other reason a resolution is deferred, this one turns on state that
    /// reaches the engine only through the pump's `PumpCtx` (the routing
    /// table's view of the input's pads), so NO event marks the moment it
    /// becomes resolvable and `dirty` alone would strand the desire. Makes
    /// the next pump reconsider instead.
    awaiting_external: bool,
    /// A `STREAMS_SELECTED` of this load has been adopted AND it could
    /// speak about the text slot (a text stream was advertised, or the
    /// report named one). From then on an empty applied text slot is
    /// decodebin3's REAL state, not ignorance awaiting the auto-select, and
    /// `collection_changed` must not seed it (see there).
    text_state_known: bool,
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
    /// is the one desire that is item-INDEPENDENT, and it must survive: a queue
    /// transition is the crate's own decision, not a new user request, and
    /// nothing re-applies the intent afterwards (receiver-core's
    /// `apply_subtitle_target` is reachable only from an incoming sender packet,
    /// never from its `GaplessActivated` handler, and receiver-core holds no
    /// subtitle desire of its own to replay, it mirrors the player's).
    ///
    /// Resetting it outright turned subtitles the user had switched OFF back on
    /// at every gapless boundary: with `desired_subtitle` unset,
    /// `collection_changed` seeds the text slot with the new collection's
    /// default, the pump dispatches it, and `Inner::poll_text_policy` relinks
    /// the branch.
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
        // collection. `collection_changed` (called right after this) marks
        // dirty anyway; explicit here so the intent does not depend on that.
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
        self.collection = collection;
        // A slot the desire explicitly disables must NOT be seeded with the
        // collection default: the pipeline honors the disable across
        // collection re-posts (text auto-select is off, and A/V inherit the
        // previous selection), so seeding would fabricate an applied state
        // the pipeline is not in. The pump would then dispatch a selection
        // decodebin3 already has, a no-op it never confirms, and that
        // phantom in-flight request starves every later change (seen when
        // re-attaching an external subtitle re-posts the collection while
        // subtitles are switched off).
        //
        // The text slot needs one more guard: decodebin3 auto-selects
        // text only until it has seen an explicit `SELECT_STREAMS`, so
        // once a selection of this load was adopted that could speak about
        // text, an EMPTY text slot is its real state. Seeding it then makes
        // the pump treat a re-enable as already applied and dispatch
        // nothing (the external re-select-after-deselect wedge). A report
        // made while the collection held no text stream is not one of those.
        // It left the slot empty out of ignorance and the seeding must still
        // happen (see `text_state_known`). A POPULATED slot whose stream left
        // the collection still falls back to the default, mirroring
        // decodebin3 replacing a vanished selected stream.
        let text_was_applied = self.applied.subtitle.is_some();
        self.applied.video = Self::seed_slot(
            &self.collection,
            StreamKind::Video,
            self.applied.video.take(),
            self.desired_video != Some(None),
        );
        self.applied.audio = Self::seed_slot(
            &self.collection,
            StreamKind::Audio,
            self.applied.audio.take(),
            self.desired_audio != Some(None),
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
        // The new collection can change what the desire resolves to (an
        // external materialized, an explicit sid appeared/left) and
        // decodebin3 will re-run its own auto-select for it, so converge.
        self.dirty = true;
    }

    /// The collection's default for a slot: keep `current` when its stream
    /// is still advertised, else (`allow_default`) the first stream of the
    /// kind (mirroring decodebin3's own auto-select), so a change
    /// dispatched before the initial `STREAMS_SELECTED` composes against a
    /// full selection. `allow_default` is false for a slot the desire
    /// explicitly disables (see `collection_changed`).
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

    /// An `ExternalSubtitleFailed` fired or the input was detached: a
    /// desire parked on it would otherwise park forever. The desire resets
    /// to UNSET (whatever is showing keeps showing), not to "off": the
    /// failed request should not tear down the subtitle the user had
    /// before it.
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
        // A report speaks about the text slot only when a text stream was
        // there to be reported. decodebin3 merges one input's collection at
        // a time and reports against what it has merged so far, so a report
        // made before the text input joined leaves the slot empty out of
        // ignorance, not out of a decision (see `collection_changed`).
        if reported.subtitle.is_some()
            || self
                .collection
                .iter()
                .any(|stream| stream.kind == StreamKind::Text)
        {
            self.text_state_known = true;
        }

        match self.selecting.take() {
            None => {
                // A superseded dispatch's echo is ours and stale in this
                // branch too. The overtake path below abandons the live wait
                // without clearing the superseded records, so the same echo
                // can land here with nothing in flight.
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
                    self.applied = reported.clone();
                    self.superseded.clear();
                    // Ours settled. Anything the report still diverges on
                    // (decodebin3 adjusting the request) is deliberately
                    // NOT re-asserted here: without a fresh event it would
                    // ping-pong against a selection the pipeline refuses.
                    return;
                }
                if self.take_superseded_echo(seqnum, reported) {
                    // A superseded dispatch's late confirmation. It is ours
                    // but stale, and the live request's own confirmation is
                    // still en route.
                    self.selecting = Some((expected, desired_sel));
                    return;
                }
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
    /// Such an echo must NOT be adopted as applied. The superseding
    /// dispatch is the newer truth, and rewinding `applied` to the stale
    /// one re-arms `subtitle_sid` with the very subtitle that dispatch is
    /// turning off, letting `poll_text_policy` relink the cue the eager
    /// detach just cleared (and making the next request compose against a
    /// reverted state).
    ///
    /// Matching drains the record and every older one. decodebin3 confirms
    /// in dispatch order, so a record still unmatched behind this one never
    /// will be, its request having been folded into a later dispatch.
    /// Draining is what keeps a stale record from swallowing a genuinely
    /// foreign selection that names the same streams much later.
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

    /// Whether the engine has moved on from the refresh `seqnum` names: it is
    /// waiting on a DIFFERENT one, so this job is a superseded flushing seek
    /// and must not be performed. `None` in flight is not supersession, a
    /// caller may force a refresh through
    /// [`FcastPlaybin::refresh_seek_async`] without the engine tracking it.
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

    /// The subtitle stream the applied (or in-flight) selection shows.
    /// Only this stream may join the overlay. `None` means off, possibly
    /// still draining, when `Inner::poll_text_policy` must not relink a
    /// detached stream.
    pub(crate) fn subtitle_sid(&self) -> Option<String> {
        self.applied.subtitle.clone()
    }

    /// Whether the DESIRED subtitle state is an explicit off.
    ///
    /// [`Self::subtitle_sid`] cannot answer this. It reads `applied`, which is
    /// adopted verbatim from `STREAMS_SELECTED`, so decodebin3's
    /// collection-default auto-select stomping an explicit subtitle-off makes
    /// it name a stream the caller turned off. [`Self::streams_selected`]
    /// records that divergence and marks `dirty`, but the corrective
    /// re-assert only dispatches from a QUIET pump, so between the stomp and
    /// the next settle point `applied` is not the caller's intent.
    /// `Inner::poll_text_policy` must not join the stomped stream to the
    /// overlay in that window (see there). `collection_changed` already
    /// consults the desire the same way when it seeds the text slot.
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
        self.applied = target;
    }

    /// The dispatch could not be sent (no core, selection refused): there
    /// is no completion to wait for, and a refresh scheduled for the switch
    /// must not fire as an orphan flush.
    pub(crate) fn dispatch_failed(&mut self, seqnum: gst::Seqnum) {
        if let Some((expected, _)) = &self.selecting
            && *expected == seqnum
        {
            self.selecting = None;
        }
        self.refresh_wanted = false;
    }

    pub(crate) fn refresh_dispatched(&mut self, seqnum: gst::Seqnum) {
        self.refreshing = Some(seqnum);
    }

    #[cfg(test)]
    pub(crate) fn has_dispatchable_work(&self) -> bool {
        self.dirty || self.refresh_wanted
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
            // An external desire cannot be compared here: resolving it
            // needs the externals map, which only the pump has. Treating
            // it as "never diverges" loses the re-assertion entirely once
            // the engine has converged (the desire is applied, nothing in
            // flight, nothing dirty) and decodebin3 then auto-selects the
            // embedded default over it: no collection change follows to
            // re-dirty, so the external would stay silently deselected.
            // Deferring the comparison to the pump instead is convergent:
            // it resolves against the map and dispatches only when the
            // resolution actually differs from what was just reported, and
            // each re-assertion needs a fresh foreign `STREAMS_SELECTED`.
            Some(SubtitleDesire::External(_)) => true,
        }
    }

    /// Whether the subtitle desire is parked on an external input that has
    /// not produced an advertised TEXT stream yet, the one resolution input
    /// that arrives outside the engine's own event stream.
    ///
    /// The collection change that merges the external's stream re-dirties
    /// the engine, but only the pump's `PumpCtx` says whether the INPUT has
    /// produced the id, and the two need not arrive in that order. When they
    /// do not, `dirty` has already been spent on a resolution that could not
    /// see the input yet, and nothing else ever re-arms it.
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
    /// cannot honour yet (an external that has not materialized, a sid the
    /// collection does not carry) so the pipeline keeps showing something. That
    /// fallback must never be mistaken for a satisfied REQUEST: the field had a
    /// select-true attach answered, in the same millisecond and before the
    /// attach job even ran, with the EMBEDDED track the item was already
    /// showing, which the receiver then reported to the sender as the confirmed
    /// selection. An unresolvable desire leaves the request armed instead, and
    /// the dispatch that follows materialization answers it.
    ///
    /// An explicit OFF (`Some(None)`) is always honourable. An UNSET slot is
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
    fn resolve(&self, ctx: &PumpCtx) -> Option<TrackSelection> {
        if self.collection.is_empty() {
            return None;
        }
        let in_collection = |sid: &String| self.collection.iter().any(|s| &s.sid == sid);
        // An external input is a plain urisourcebin over a caller-supplied
        // URI: nothing constrains it to a single stream (a container
        // handed in as "the subtitle" advertises its A/V streams too), and
        // its ids arrive in source-pad order. The subtitle slot may only
        // ever hold a TEXT stream, so resolve against the kind the
        // collection advertises, not against mere membership.
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
        // An explicit stream that left the collection cannot be selected.
        // Fall back to what is applied rather than fabricating a disable.
        //
        // An UNSET slot means "follow the pipeline", and the pipeline
        // auto-selects the first stream of each kind. Omitting a kind from a
        // `SELECT_STREAMS` asks decodebin3 to turn it OFF, so an unset slot
        // with nothing applied must resolve to the collection default rather
        // than to nothing. `applied` is not a reliable stand-in on its own.
        // It is adopted verbatim from `STREAMS_SELECTED`, and decodebin3
        // reports its selection while its merged collection is still growing
        // (one post per input as each parsebin reports), so an early report
        // can name audio alone and leave this slot empty with the video
        // stream sitting right there in the collection.
        // A/B lever for bisecting regressions without a rebuild, like
        // FCAST_NO_SELECTION_REPLAY.
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
        // deselecting video implicitly deselects subtitles.
        //
        // "The selection has no video" and "the collection has no video" are
        // different situations, and only the first one is a deselect. The
        // second is decodebin3's merged collection still growing towards its
        // video stream, or a media that simply has none. Nothing is routing
        // video there, so `poll_text_policy` never links text into the
        // overlay in the first place (it returns early without a live video
        // stream) and no renegotiation is riding on the text slot.
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
                if only_the_subtitle_moves {
                    // Nothing to gain and everything to lose. decodebin3
                    // grows its merged collection as each input reports, so
                    // a resolution early in a load can see audio and text
                    // with no video yet, and an event whose whole content is
                    // dropping the text stream turns a request to ENABLE a
                    // subtitle into one that disables it. decodebin3 honours
                    // that and never auto-selects text again (the field
                    // symptom was `sent SELECT_STREAMS ids=["...audio_0"]`
                    // followed by `selection drops video, parking the video
                    // chain at READY` during a load that asked for neither).
                    // Wait. The collection change that brings video in
                    // re-dirties the desire.
                    debug!(
                        collection = ?self.collection,
                        "holding the subtitle selection back until the collection announces video"
                    );
                    return None;
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
        // An empty selection would trip a GStreamer assertion
        // (`gst_event_new_select_streams: streams != NULL`) and leave the
        // pipeline in an undefined state. Never produce one.
        if selection.video.is_none() && selection.audio.is_none() && selection.subtitle.is_none() {
            debug!("refusing to resolve to an empty stream selection");
            return None;
        }
        Some(selection)
    }

    /// Decide the next operation to dispatch, if the transport allows one.
    pub(crate) fn pump(&mut self, ctx: &PumpCtx) -> Option<Command> {
        // A scheduled re-emit flush becomes hazardous the moment an
        // external subtitle input attaches (the flush races the external
        // inputs' reconfiguration), and pointless on an unseekable stream;
        // re-decided at every pump, not just when the switch scheduled it.
        if self.refresh_wanted && (ctx.externals_attached || !ctx.gate.seekable) {
            self.refresh_wanted = false;
        }
        if !ctx.gate.quiet {
            return None;
        }
        // A refresh flush is an async re-preroll. Never dispatch on top of
        // it.
        if self.refreshing.is_some() {
            return None;
        }
        // An unconfirmed selection blocks new work while data flows (its
        // reconfigure may still be about to re-preroll). While paused it is
        // merely parked: superseding it, or flushing past it, is safe
        // (nothing is re-prerolling).
        if self.selecting.is_some() && !ctx.gate.paused {
            return None;
        }

        // Retry a resolution that was deferred on an external input ONLY at
        // the moment that input's stream actually shows up. Retrying on
        // every pump would dispatch outside a fresh event, and dispatching
        // only on fresh events is what keeps a selection decodebin3 adjusts
        // from ping-ponging (it answers the dispatch with a different
        // selection, which the engine deliberately does not re-assert).
        let unresolved_external = self.external_desire_unresolved(ctx);
        let external_arrived = self.awaiting_external && !unresolved_external;
        self.awaiting_external = unresolved_external;

        if self.dirty || external_arrived {
            self.dirty = false;
            // An already-satisfied USER request still has to be answered, and
            // only this crate can answer it in upstream-selection mode (see
            // `Command::ConfirmApplied`). Taken either way: in db3-owned mode a
            // request that changes nothing needs no synthetic confirmation
            // (decodebin3 owns that channel there), and leaving the flag set
            // would answer a later, unrelated pump.
            // `desires_resolvable` FIRST: an unresolvable desire must not even
            // consume the flag, or the request is lost.
            if let Some(target) = self.resolve(ctx)
                && target == self.applied
                && self.desires_resolvable(ctx)
                && std::mem::take(&mut self.unanswered_request)
                && ctx.upstream_owns
            {
                debug!(?target, "a user request is already satisfied; confirming it locally");
                return Some(Command::ConfirmApplied(target));
            }
            if let Some(target) = self.resolve(ctx)
                && target != self.applied
            {
                // The dispatch's own confirmation answers the request.
                self.unanswered_request = false;
                // A flushing seek to the current position drops the
                // deeply-buffered old track (decoded audio piled up in
                // fpb-aqueue, video frames still carrying the old
                // subtitle's overlay meta) so a switch takes effect
                // immediately instead of after that buffer drains.
                // Scheduled only for a switch TO a real audio/subtitle
                // track, and never when:
                //   * an external subtitle is attached (any flush races the
                //     external inputs' reconfiguration and can freeze the
                //     item)
                //   * any slot is being DISABLED (Some -> None): flushing
                //     across a sink/branch teardown wedges (audio-off drops
                //     the pipeline clock, video-off freezes the audio
                //     clock, subtitle-off fails vaapi renegotiation) and
                //     there is no incoming track to make immediate anyway.
                // Video switches keep the pre-existing no-flush behaviour
                // (rare, and a flush re-prerolls the whole video chain).
                let switching_to_track = (target.subtitle != self.applied.subtitle
                    && target.subtitle.is_some())
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
    /// showing the previous track until it hears back (the field's paused
    /// attach-with-select).
    #[test]
    fn an_already_satisfied_request_is_confirmed_once_in_upstream_mode() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("t0")));

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
    /// by whatever happens to be on screen. The field answered a select-true
    /// attach with the embedded track, in the same millisecond, before the
    /// attach job had even run.
    #[test]
    fn a_request_for_an_unmaterialized_external_is_not_confirmed() {
        const EXT: ExternalSubId = ExternalSubId(9);
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("t0")));

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
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("ext0")));

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
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("t0")));

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("gone".into())));
        assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
    }

    /// The same request in db3-owned mode is answered by decodebin3, so the
    /// engine must not manufacture a second confirmation there.
    #[test]
    fn an_already_satisfied_request_is_not_confirmed_in_db3_owned_mode() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("t0")));

        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
        assert_eq!(engine.pump(&ctx(true, false)), None);
    }

    /// `dirty` set by anything OTHER than a user request must never confirm: a
    /// collection change is not a question anyone asked, and answering it would
    /// have the receiver relay unsolicited track changes.
    #[test]
    fn a_dirty_engine_without_a_request_confirms_nothing() {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(avt_collection());
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("t0")));
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
        engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), Some("t0")));

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
        }
    }

    /// Engine with the standard collection adopted and the collection
    /// defaults confirmed as applied (the steady state most flows start
    /// from).
    fn settled_engine() -> SelectionEngine {
        let mut engine = SelectionEngine::new();
        engine.collection_changed(avt_collection());
        engine.streams_selected(
            gst::Seqnum::next(),
            &sel(Some("v0"), Some("a0"), Some("t0")),
        );
        // The collection change marked the engine dirty, and the
        // auto-select matches every (unset) desire, so the first pump
        // converges with no dispatch.
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
        // The external_sub_add_unselected class: an explicit no-subtitle
        // desire is dispatched against a fresh collection, and decodebin3's
        // own collection-default auto-select (fresh text stream included)
        // lands after ours and stomps it. The overtaking STREAMS_SELECTED
        // must re-assert the desire instead of waiting forever on a
        // confirmation that never comes.
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
        // The declarative win over the old edge-triggered queue: a fresh
        // collection's auto-select stomping the applied state with NO
        // request in flight (the old FcastSubDesire enforcement case) is
        // detected and corrected.
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
    fn paused_selection_parks_and_refresh_flushes_past_it() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert_eq!(
            engine.pump(&ctx(true, true)),
            Some(Command::SelectStreams(target.clone()))
        );
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target);

        // While paused the selection is parked (no STREAMS_SELECTED until
        // data flows). The refresh must dispatch anyway: it is what wakes
        // the pipeline and makes the selection apply.
        assert_eq!(engine.pump(&ctx(true, true)), Some(Command::RefreshSeek));
        engine.refresh_dispatched(gst::Seqnum::next());

        // Flush in flight: nothing else dispatches even though paused.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(engine.pump(&ctx(false, true)), None);
    }

    #[test]
    fn paused_selection_can_be_superseded() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert!(engine.pump(&ctx(true, true)).is_some());
        let sn1 = gst::Seqnum::next();
        let first = sel(Some("v0"), Some("a1"), Some("t0"));
        engine.selection_dispatched(sn1, first.clone());

        // A parked selection has no re-preroll to overlap with, the next
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
        // A paused subtitle switch dispatches the selection, then a single
        // re-emit flush once the pipeline settles. The flushing seek
        // re-prerolls, so the cue composites before ASYNC_DONE: one flush
        // is enough, no retry.
        let mut engine = settled_engine();
        engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
        let target = sel(Some("v0"), Some("a0"), Some("t1"));
        assert!(engine.pump(&ctx(true, true)).is_some());
        let sn = gst::Seqnum::next();
        engine.selection_dispatched(sn, target.clone());
        assert_eq!(engine.pump(&ctx(true, true)), Some(Command::RefreshSeek));
        engine.refresh_dispatched(gst::Seqnum::next());

        // The selection confirms and the flush completes, nothing is
        // re-queued.
        engine.streams_selected(sn, &target);
        assert!(engine.refresh_done());
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
        // that may be gone, so the waits are abandoned deterministically
        // (the job the long-removed watchdog used to do on a timeout).
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
        // the engine. Arriving in that order, the pump spends the dirty flag
        // on a resolution that cannot see the input yet, and without
        // `awaiting_external` nothing ever re-arms it: no further collection
        // change is due, so the desire parks for the rest of the load.
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
        // The full external_sub_add_unselected sequence, declaratively: the
        // desire dispatches after materialization, decodebin3's auto-select
        // for the new collection stomps it, and the engine re-asserts.
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
        // `foreign_autoselect_with_nothing_in_flight_is_reasserted`: the
        // external's stream is selected and CONFIRMED (nothing in flight,
        // the engine converged), then decodebin3 auto-selects the embedded
        // text default on its own and stomps it. The desire must
        // re-assert, exactly as an explicit `Stream` desire does. No
        // collection change follows such a stomp, so nothing else would
        // ever re-dirty the engine.
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
        // The paused supersede path: a dispatch's late confirmation is our
        // own stale echo. It is recognized as such for the in-flight wait,
        // but must not be adopted as the applied state either: `applied`
        // (and with it `subtitle_sid`, which gates what may join the
        // overlay) would name the subtitle the superseding dispatch is in
        // the middle of turning OFF, so `poll_text_policy` could relink the
        // cue the eager detach just cleared.
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
        // An external subtitle input is a plain urisourcebin over a
        // caller-supplied URI: nothing forces it to hold exactly one
        // stream (a container handed in as "the subtitle" advertises its
        // A/V streams too), and its stream ids arrive in source-pad order.
        // Resolving the desire to the input's FIRST advertised stream
        // therefore drops a non-text id into the subtitle slot, selecting
        // the wrong stream and deselecting the text one entirely.
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

    #[test]
    fn a_superseded_echo_after_a_foreign_overtake_does_not_revert_applied() {
        // `a_stale_superseded_echo_does_not_revert_applied` covers the echo
        // arriving while the superseding dispatch is still tracked. The
        // overtake path drops that tracking (`selecting` is taken and never
        // restored) while the superseded records stay, so the same echo then
        // lands with nothing in flight and is mistaken for a foreign
        // selection. Adopting it re-arms `subtitle_sid` with the very
        // subtitle the newest dispatch is turning off, which is what lets
        // `poll_text_policy` relink the cue the eager park just cleared.
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
        // and the desire re-asserts. The superseded record for sn1 outlives
        // it.
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

    #[test]
    fn two_superseded_dispatches_are_recognized_and_then_forgotten() {
        // Three paused dispatches, each superseding the last, so two records
        // are outstanding at once. decodebin3 folds the first into the second
        // and confirms the second only, which must retire BOTH records: an
        // echo that has not arrived by the time a later one has never will,
        // and a record that outlives its dispatch goes on swallowing genuine
        // foreign selections that happen to name the same streams.
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

    /// What a gapless boundary carries. A stream id names a stream of the
    /// item that just ended, so only an explicit DISABLE is item-independent
    /// and survives. An external desire goes with the rest because the
    /// activation removes the previous generation's inputs, external subtitle
    /// inputs included (`Job::FinishActivation`), so there is no input left
    /// for it to name.
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
        // decodebin3 merges one input's collection at a time and reports its
        // selection against whatever it has merged so far. A report made
        // while the collection held no text stream says NOTHING about the
        // text slot, yet it flips `adopted`, and from then on
        // `collection_changed` refuses to seed the slot. The next dispatch
        // (here an ordinary audio switch) then composes an event that omits
        // text, which is decodebin3's way of being told to turn it off, and
        // its own auto-select never runs again.
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
                Some(Command::ConfirmApplied(_)) => unreachable!(
                    "ConfirmApplied is scoped to upstream-selection mode"
                ),
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
                // An UNSET desire follows the pipeline, and the pipeline
                // auto-selects the first stream of each kind. Ending with the
                // text slot off means a dispatch composed it away, which is
                // the shape of the partial-report bug: nobody asked for it and
                // decodebin3's auto-select never runs again once it has been
                // told explicitly. Only checked when no subtitle op ran at
                // all, since a desire RESET by a failed external legitimately
                // leaves an earlier disable applied.
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
    /// collection growth, the engine must converge and end up applying every
    /// desire the collection can express. This is where a lost desire, a
    /// stale echo overwriting the applied state, or a re-assertion loop shows
    /// up without anyone having to guess the ordering that triggers it.
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
    /// resolution needs TWO independent things to line up (the input having
    /// produced its stream id, and decodebin3 having merged that id into the
    /// collection as text) plus a reset path when the input dies. Every
    /// ordering of those against dispatch, confirmation and a foreign
    /// auto-select must still end on the external's stream.
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
        // The holdback added for the collection-growth window keys off "the
        // collection has no video", which is also true for the whole life of
        // a media that simply has none. The subtitle desire then stays set,
        // every later resolve hits the holdback and returns None, and every
        // other slot's request is dropped with it for the rest of the load.
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

        // An audio switch has nothing to do with the subtitle and must go
        // out. It carries the subtitle, because a collection with no video
        // id to name cannot express "video on" either way and stripping the
        // text stream would be a second deselect nobody asked for.
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(None, Some("a1"), Some("t1")))),
            "the holdback must defer only the subtitle, not every other slot"
        );
    }
}
