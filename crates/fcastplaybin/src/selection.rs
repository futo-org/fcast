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
}

/// What the pump decided to dispatch next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    SelectStreams(TrackSelection),
    RefreshSeek,
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
    /// Dispatches superseded before confirming (the paused supersede path).
    /// Their late confirmations are our own stale echoes, recognized here
    /// by seqnum or content so they neither settle the live request nor
    /// masquerade as an overtaking foreign selection. Cleared on settle.
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
    /// A `STREAMS_SELECTED` of this load has been adopted. From then on an
    /// empty applied text slot is decodebin3's REAL state, not ignorance
    /// awaiting the auto-select, and `collection_changed` must not seed it
    /// (see there).
    adopted: bool,
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
    }

    /// A new stream collection arrived. Reconcile: drop applied sids whose
    /// stream left, seed still-empty slots with the collection defaults
    /// (the first stream of each kind, mirroring decodebin3's own
    /// auto-select) so a change dispatched before the initial
    /// `STREAMS_SELECTED` keeps the other streams selected. Any in-flight
    /// confirmation targeted the previous collection and may never confirm,
    /// so abandon it deterministically.
    pub(crate) fn collection_changed(&mut self, collection: Vec<CollectionStream>) {
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
        // once ANY selection of this load was adopted, an EMPTY text slot
        // is its real state. Seeding it then makes the pump treat a
        // re-enable as already applied and dispatch nothing (the external
        // re-select-after-deselect wedge). A POPULATED slot whose stream
        // left the collection still falls back to the default, mirroring
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
            (text_was_applied || !self.adopted)
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
        self.adopted = true;
        self.applied = reported.clone();

        match self.selecting.take() {
            None => {
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
                    self.superseded.clear();
                    // Ours settled. Anything the report still diverges on
                    // (decodebin3 adjusting the request) is deliberately
                    // NOT re-asserted here: without a fresh event it would
                    // ping-pong against a selection the pipeline refuses.
                    return;
                }
                if self
                    .superseded
                    .iter()
                    .any(|(sn, sel)| *sn == seqnum || sel == reported)
                {
                    // A superseded dispatch's late confirmation: ours but
                    // stale, the live request's own confirmation is still
                    // en route.
                    self.selecting = Some((expected, desired_sel));
                    return;
                }
                debug!(?desired_sel, ?reported, "selection overtaken, re-asserting");
                self.dirty = true;
            }
        }
    }

    /// A top-level `ASYNC_DONE` arrived. Returns whether it finished our
    /// refresh seek (attribution by exclusivity, see the module docs).
    pub(crate) fn refresh_done(&mut self) -> bool {
        self.refreshing.take().is_some()
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
            // Resolution against the externals map happens in the pump;
            // here a parked external only counts once its own sid is known
            // to differ, which it cannot be without the map. Dirty is set
            // by the collection change instead.
            Some(SubtitleDesire::External(_)) => false,
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
        // An explicit stream that left the collection cannot be selected.
        // Fall back to what is applied rather than fabricating a disable.
        let resolve_slot =
            |desired: &Option<Option<String>>, applied: &Option<String>| match desired {
                None => applied.clone().filter(in_collection),
                Some(Some(sid)) if in_collection(sid) => Some(sid.clone()),
                Some(Some(_)) => applied.clone().filter(in_collection),
                Some(None) => None,
            };

        let video = resolve_slot(&self.desired_video, &self.applied.video);
        let audio = resolve_slot(&self.desired_audio, &self.applied.audio);
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
                    .and_then(|(_, sids)| sids.iter().find(|sid| in_collection(sid)))
                    .cloned();
                match resolved {
                    Some(sid) => Some(sid),
                    // Not materialized yet: keep showing what shows now.
                    // The collection change re-dirties when it appears.
                    None => self.applied.subtitle.clone().filter(in_collection),
                }
            }
        };

        // A text stream cannot be presented without a video stream:
        // deselecting video implicitly deselects subtitles.
        if video.is_none() && subtitle.is_some() {
            debug!("dropping the subtitle stream from a selection without video");
            subtitle = None;
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

        if self.dirty {
            self.dirty = false;
            if let Some(target) = self.resolve(ctx)
                && target != self.applied
            {
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
        }
    }

    fn ctx_ext(quiet: bool, paused: bool, externals: &[(ExternalSubId, &[&str])]) -> PumpCtx {
        PumpCtx {
            gate: SelectionGate {
                quiet,
                paused,
                seekable: true,
            },
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
    fn video_disable_implicitly_drops_subtitles() {
        let mut engine = settled_engine();
        engine.request(TrackSlot::Video, TrackTarget::Stream(None));
        assert_eq!(
            engine.pump(&ctx(true, false)),
            Some(Command::SelectStreams(sel(None, Some("a0"), None)))
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
}
