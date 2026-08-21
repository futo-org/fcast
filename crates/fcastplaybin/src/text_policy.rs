//! The text policy pass: which text branches should be live, and the
//! deferred/reconcile work that gets them there.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gst::prelude::*;
use tracing::{debug, info, warn};

use crate::{
    Counters, FcastPlaybin, Inner,
    api::SubtitleFeedItem,
    decisions,
    decisions::text_seat::{self, ReclaimRule, Refusal, SeatContest, SeatVerdict},
    external::Hold,
    jobs::Job,
    routing::StreamKind,
};

/// Effects the subtitle-delivery reconcile pass has emitted
/// ([`Inner::reconcile_subtitle_delivery`]).
///
/// The number that makes "converged is a fixpoint" checkable: at a settled,
/// aligned PLAYING this must not move no matter how many passes run.
pub(crate) static RECONCILE_EMITS: AtomicU64 = AtomicU64::new(0);

/// The ways the text path degrades that are worth telling someone about once,
/// and the kind half of [`Inner::text_degradations`]'s key.
///
/// Three shapes, one table, because all three are keyed the same way (this
/// kind, a stream id, the load generation), live exactly as long as the load,
/// and want the same grace/dedupe answer from [`Inner::note_degradation`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TextDegradation {
    /// The selected subtitle stream carries caps the cue renderer cannot
    /// carry, or its branch could not be wired on those caps. Keyed by stream
    /// id, no grace: caps are a verdict, not a race, so the first sight is the
    /// report.
    ///
    /// THE one gate on [`crate::PlaybinEvent::SubtitleTrackUnsupported`], from
    /// both of its producers. See [`Inner::report_unsupported_subtitle`].
    Unsupported,
    /// A consumer branch that could not be WIRED, as opposed to refused on its
    /// caps. Keyed by [`Inner::unwirable_key`] (the stream id, or the pad for a
    /// stream that has none yet), no grace.
    ///
    /// A RETRY gate, not a report gate. The poll that builds the branch runs on
    /// every tick and a link GStreamer refuses will be refused again for the
    /// same reason every time: without this the crate rebuilds and tears down a
    /// queue once per tick for the rest of the item, logging a warning each
    /// time and never telling the caller anything. The caller is told through
    /// [`TextDegradation::Unsupported`] instead, which is the same user-visible
    /// outcome (the branch stays parked, no cue is ever shown).
    Unwirable,
    /// The text link loop's caps gate has been refusing a routed stream for
    /// want of a sticky CAPS (see [`CAPSLESS_TEXT_GRACE`]).
    ///
    /// The gate calls caps-absent "rare and transient" and refuses WITHOUT
    /// reporting, which is right for the millisecond a pad spends between being
    /// exposed and carrying its sticky. It is not right forever: a stream whose
    /// decodebin3 input never gets a multiqueue slot never carries one, and the
    /// gate then refuses the join for the life of the item, silently, about a
    /// hundred times a second, while the selection reads as confirmed and the
    /// caller sees a track that simply never appears. Measured at ~4025
    /// refusals over 40 s. This is the memory that turns that into ONE line
    /// naming the stream and the signature that says whether the break is
    /// upstream of the gate or in selection.
    CapslessStall,
}

/// One key's place in the grace/dedupe machine (see
/// [`Inner::note_degradation`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum DegradationMemo {
    /// First seen at this instant, still inside its grace, nothing said yet.
    Since(Instant),
    /// Escalated once. This key says nothing more under this load.
    Spoken,
}

/// What one [`Inner::note_degradation`] call means for its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DegradationEdge {
    /// First sight. The grace clock has started and there is nothing to say.
    First,
    /// The shape has outlasted its grace: report it, now, once. The only edge
    /// any caller acts on.
    Escalate,
    /// Inside the grace, or already spoken for.
    Silent,
}

/// One routed text stream the caps gate has been refusing for want of a sticky
/// CAPS for longer than [`CAPSLESS_TEXT_GRACE`], i.e. the payload of a
/// [`TextDegradation::CapslessStall`] escalation. Formed under the routing lock
/// and logged after it, the `refusals` discipline.
struct CapslessTextStall {
    caps_path: String,
    sid: String,
    pad: String,
    sticky_stream_start: bool,
    sticky_segment: bool,
    linked: bool,
    parked_on: Option<String>,
}

/// One refused link candidate, formatted LAZILY (see [`Refusal`]).
///
/// The pad is held by reference count rather than by name, and the two
/// pad-derived details are read back off it at format time. That is the whole
/// point: a poll that refuses several candidates for a debug line nobody has
/// enabled used to allocate one `String` per candidate per poll, measured at
/// roughly 600 a second at receiver cadence. The re-read is a diagnostic only,
/// so a sticky that changed since the decision prints its newer value and
/// nothing acts on it.
struct RefusedText<'a> {
    pad: gst::Pad,
    allowed: Option<&'a str>,
    refusal: Refusal,
}

impl std::fmt::Debug for RefusedText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pad = self.pad.name();
        match self.refusal {
            Refusal::SidNotAllowed => write!(
                f,
                "{pad}: sid {:?} is not the allowed {:?}",
                self.pad.stream_id().as_deref(),
                self.allowed
            ),
            Refusal::CapsUnsupported => write!(
                f,
                "{pad}: caps {:?} are not subtitles the renderer can carry",
                self.pad.current_caps().map(|caps| caps.to_string())
            ),
            other => write!(f, "{pad}: {other}"),
        }
    }
}

impl Inner {
    /// Whether ANY postponed text-branch work is pending. One predicate so
    /// the drain triggers and the drain itself can never disagree about what
    /// counts as pending. The old inline check tested only the (since deleted)
    /// eager-work slot and the disposals, so pending replays or input removals
    /// with nothing else pending were skipped by the very drain that owns
    /// them.
    ///
    /// # One lock at a time, deliberately
    ///
    /// Written as a chain of `||` this reads as sequential probes and is
    /// not: every `lock()` in one expression is a temporary that lives to the
    /// end of the STATEMENT, so the chain held every crate mutex it touched at
    /// once. fpb-tick is a caller, and its discipline is "crate mutexes one at
    /// a time, no gst object, no inline action" precisely so the liveness
    /// mechanism can never be the thing that is stuck. Each probe therefore
    /// gets its own statement, and the early return keeps the short-circuit.
    pub(crate) fn has_deferred_text_work(&self) -> bool {
        if !self.deferred_text_disposal.lock().is_empty() {
            return true;
        }
        !self.deferred_input_removal.lock().is_empty()
    }

    /// Whether the crate is holding an ITEM at all: an input registered, or a
    /// stream routed out of decodebin3.
    ///
    /// The cheapest possible statement of "there is something to reconcile",
    /// and an exact one for the text pokes: every input the crate owns, main
    /// and external alike, is in `routing.inputs` from before its first pad can
    /// appear ([`Inner::add_input`]) until the teardown removes it, and
    /// [`Inner::reconcile_subtitle_delivery`] returns at its second guard when
    /// no input carries the selected sid.
    ///
    /// EMPTINESS ONLY. `Input::stream_ids` would ask GStreamer for the
    /// element's pads, which the tick may not do; `Vec::is_empty` asks nobody.
    /// One lock, taken alone.
    pub(crate) fn holds_an_item(&self) -> bool {
        let routing = self.routing.lock();
        !routing.inputs.is_empty() || !routing.routed.is_empty()
    }

    /// Replay whatever had to be postponed, once the pipeline is playing and
    /// a blocking operation is most likely to complete. Only LIKELY: the
    /// note above `Inner::detach_text_parts`'s postponement says why a settled
    /// PLAYING is a filter and never a guarantee. WORKER-ONLY, through
    /// [`Job::DrainTextWork`], because everything in here can block behind a
    /// streaming thread and must therefore stay off the callers. That is a
    /// statement about the CALLERS and not a licence for the worker. The
    /// worker also owns [`Job::SetState`], so anything it blocks on that only
    /// PLAYING can release deadlocks the receiver (see [`ReplayJob`]). The
    /// flushes below are dispatched here because their pads are unlinked or
    /// being disposed of, not because blocking here is safe.
    ///
    /// The job is queued on every pipeline state edge and at every caller
    /// settle point, so no postponed work depends on the caller's poll
    /// cadence and none can gate its own drain condition.
    pub(crate) fn run_deferred_text_work(inner: &Arc<Inner>, queued_epoch: u64) {
        // The reconcile pass at the tail has no "something is remembered"
        // precondition - that is the whole point of it - so an empty drain is
        // no longer a wasted one. The lists below are all empty in that case
        // and every take is a no-op, which is why the body needs no second
        // shape. `drain_poke_parked` keeps its old accounting exactly: it is
        // about the deferred LISTS, and writing it for a drain that had
        // nothing to drain would change what the poll suppression means.
        let deferred = inner.has_deferred_text_work();
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::replay::settled_playing(current, pending) {
            // A no-op verdict. Until something changes it (a new postponed
            // item, or a state edge whose unconditional re-queue lands back
            // here), re-running this drain from the caller's poll can only
            // reach this same return, so the poll's re-poke is suppressed
            // (see `Inner::drain_poke_parked`). Recording the verdict AFTER
            // the state read is what makes a racing edge safe: an edge that
            // fires in between has already queued its own drain, which runs
            // after this one and refreshes the verdict.
            if deferred {
                inner.drain_poke_parked.store(true, Ordering::SeqCst);
            }
            return;
        }
        // The drain proceeds, so the last no-op verdict (if any) is stale.
        if deferred {
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
        }
        // Branches unlinked while paused, now safe to flush and drop. THE
        // non-teardown dispose path, and the other half of the interleaving
        // the phase closed: the disposal below and the link loop used to be
        // two threads deciding about the same renderer entry at once, and are
        // now this one thread deciding twice in a row (the poke a few lines
        // down is literally the second half).
        inner.decider_only("the postponed text-branch disposals");
        let disposals = std::mem::take(&mut *inner.deferred_text_disposal.lock());
        let disposed = !disposals.is_empty();
        for disposal in disposals {
            debug!("disposing of a text branch postponed while paused");
            inner.dispose_text_branch(disposal);
        }
        // A disposal used to hold the shared seat while it flushed, and the
        // LINK side refused to wait for it, so the caller thread was never
        // blocked behind a text branch. Nothing retried that skipped link
        // except the caller's next settle point, whenever that
        // came: the field showed a switched-to external whose branch never
        // joined at all while its replays burned out on a 400ms timer. The
        // thread that caused the skip retries it here instead.
        if disposed {
            Inner::poll_text_policy(inner);
        }
        // THE SECOND POKE, and the one an EMBEDDED track had no equivalent of.
        //
        // `Inner::reconcile_subtitle_delivery` re-derives replay for an
        // EXTERNAL and returns at its "not an external" guard for everything
        // else, so an embedded subtitle's only route to a consumer is an EDGE
        // that queues `Job::PollTextPolicy`. `dash-reenable-freeze.txt` is
        // what happens when the edges run out: a flushing `Job::RefreshSeek`
        // at a re-enable made decodebin3 expose `text_1` for the still-selected
        // stream, the two polls that followed the pad ran before the seat could
        // be reclaimed, and then nothing asked again - `DrainTextWork` ticked
        // at 1 Hz for the rest of the item while the track rendered nothing.
        //
        // The condition is the reclaim's own `waiting` predicate, so this
        // cannot become a 1 Hz busy poll: on a converged graph the selected
        // sid is carried by a JOINED entry and there is no unjoined text entry
        // holding it. It fires exactly while the graph is in the state the
        // reclaim and the link loop exist to leave.
        if !disposed {
            let wanted = inner.selection.lock().subtitle_sid();
            // One routing read produces the predicate AND the capture-grade
            // detail: WHICH pads are waiting, with the verdicts a reclaim left
            // on them, and whether the sid is ALREADY carried by a joined
            // branch. The poke deliberately still fires in the already-joined
            // shape: the waiting pad is then decodebin3's OLD output (the
            // EOS-seat reclaim keeps it unsuperseded on purpose, so a
            // walk-back can revive it), and this drain-cadence re-ask is the
            // only trigger that heals such a walk-back at real pace. The
            // link loop will refuse the corpse ("another text branch already
            // feeds the consumer") and suppress that refusal log because the
            // sid is already joined, so without these fields a capture shows
            // re-asks with no join and no reason.
            let waiting = wanted.as_deref().and_then(|allowed| {
                let routing = inner.routing.lock();
                let waiting: Vec<String> = routing
                    .routed
                    .iter()
                    .filter(|routed| {
                        routed.kind == StreamKind::Text
                            && routed.downstream.is_none()
                            // A superseded pad is out of the watch UNLESS its
                            // slot is demonstrably alive: decodebin3 walking
                            // back onto a condemned pad (a re-enable reusing
                            // the cleared slot keeps the output and re-points
                            // it, no pad-added) clears `saw_eos` with the
                            // fresh STREAM_START, and that pad is the one the
                            // flow reclaim exists to re-admit, which it can
                            // only do if something re-asks. Measured: a
                            // slow-paced re-select latched text_0 superseded
                            // one poll before the re-point and this watch then
                            // never fired again for the rest of the item.
                            && (!routed.superseded
                                || !routed.saw_eos.load(Ordering::SeqCst))
                            && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                    })
                    .map(|routed| {
                        format!(
                            "{}(eos={} evicted_dead={} superseded={})",
                            routed.db3_src_pad.name(),
                            routed.saw_eos.load(Ordering::SeqCst),
                            routed.evicted_dead,
                            routed.superseded,
                        )
                    })
                    .collect();
                if waiting.is_empty() {
                    return None;
                }
                let already_joined = routing.routed.iter().any(|routed| {
                    routed.kind == StreamKind::Text
                        && routed.downstream.is_some()
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                });
                Some((waiting, already_joined))
            });
            if let Some((waiting, already_joined)) = waiting {
                debug!(
                    sid = ?wanted.as_deref(),
                    ?waiting,
                    already_joined,
                    "the selected text stream has a routed pad and no consumer; re-asking the \
                     link policy"
                );
                Inner::poll_text_policy(inner);
            }
        }
        // Inputs a detach took out of routing but could not tear down.
        let inputs = std::mem::take(&mut *inner.deferred_input_removal.lock());
        for input in inputs {
            debug!(
                generation = input.generation,
                "removing an input postponed while paused"
            );
            Inner::remove_input(inner, input);
        }
        // THE RECONCILE PASS, the tail of every drain and the whole of the
        // postponed-replay story. It runs unconditionally, once: the pass is a
        // fixpoint, so a converged graph costs two sticky reads and emits
        // nothing.
        Inner::reconcile_subtitle_delivery(inner, queued_epoch);
    }

    /// THE reconcile pass for subtitle delivery: desired versus observed, at a
    /// settled PLAYING, with no memory of what was owed.
    ///
    /// This replaced two compensation lists, both now deleted. One remembered
    /// "the pipeline refused a seek, do it later", the other "I could not
    /// decide, ask later"; both were RECONSTRUCTIONS of a desired state that
    /// the graph can simply be asked about, and a remembered intention can go
    /// stale (wrong epoch, dead input, a seek that has since landed) in ways
    /// the graph cannot. The caller already runs at exactly the moments
    /// delivery becomes provable, so the pass re-derives instead of
    /// remembering:
    ///
    /// * DESIRED - read fresh, never carried: the engine's selected subtitle
    ///   sid, and which attached external (with its epoch) carries it.
    /// * OBSERVED - sticky reads only, under the routing lock alone: DELIVERED
    ///   is "some routed text branch's decodebin3 pad carries this input's
    ///   StreamStart" and ALIGNED is "the subtitle segment's origin equals
    ///   `video_timeline`'s". Both transferred verbatim from
    ///   [`FcastPlaybin::verify_replay`], which is the point: the predicates
    ///   were always observation, only their SCHEDULING was compensation.
    /// * EMIT - one `Job::ReplaySub`, behind both guards.
    ///
    /// # The two guards
    ///
    /// Guard 1 is desired != observed, which is the `if aligned { return }`
    /// below: a converged graph is a FIXPOINT and repeated passes emit
    /// nothing. Guard 2 is "no effect for this resource is in flight", which is
    /// the resource's own `replay_inflight` (a replay emitted and not yet
    /// settled) and `verification_armed` (a bounded re-check already
    /// outstanding), both read in the same acquisition that finds the resource.
    /// Together they are why a 1 Hz unconditional poll cannot oscillate.
    ///
    /// # Why alignment alone, when the verification also demands DELIVERY
    ///
    /// [`FcastPlaybin::verify_replay`] concludes only when a cue has reached
    /// the consumer since the replay's hand-off (see
    /// [`Inner::external_cues_fed`]); this pass deliberately does not ask.
    /// Silence is not divergence HERE: an external with no cue at the current
    /// position is silent and perfectly correct, and an unbounded 1 Hz pass
    /// acting on that would flush the branch for the rest of the item. The
    /// evidence term belongs to the CHAIN, which spends a bounded number of
    /// attempts on it and then escalates. That asymmetry is why a check which
    /// cannot decide RE-ARMS itself rather than handing its question to this
    /// pass (see `verify_replay`'s below-PLAYING re-ask): what this pass cannot
    /// see, nothing else would ask about again.
    ///
    /// # The anti-fight rule with the deadline machinery
    ///
    /// The reconciler emits nothing while a selection is mid-flight. That is
    /// implemented below as two reads - the engine's `selecting_seqnum` and
    /// the hands' `select_age` - and not merely asserted: a dispatch awaiting
    /// confirmation is about to change the routing this pass reads, and the
    /// deadline machinery owns what a lapsed one means. Deadline give-ups and
    /// probes remain the ONLY adopters of routed reality into the engine. The
    /// reconciler converges the GRAPH (seat, hold, replay); the engine
    /// converges the SELECTION. No shared field is written from both sides.
    ///
    /// # The evidence that the lists could go
    ///
    /// [`RECONCILE_EMITS`] counts what this pass has emitted, exported on the
    /// stats snapshot as `reconcile_emits`. It is the instrument for the
    /// fixpoint claim, and it is what made deleting remembered compensation
    /// checkable rather than hopeful: at a settled, aligned PLAYING the
    /// counter must not move however many passes run
    /// (`tests/regression_text_reconcile.rs`). Keep it.
    pub(crate) fn reconcile_subtitle_delivery(inner: &Arc<Inner>, queued_epoch: u64) {
        inner.decider_only("the subtitle delivery reconcile pass");
        // DESIRED, read fresh at run time. A carried copy is how F1 happened.
        let Some(sid) = inner.selection.lock().subtitle_sid() else {
            return;
        };
        // GUARD 2 rides along with the lookup, because both flags live on the
        // resource this walk already has in hand.
        let target = {
            let routing = inner.routing.lock();
            routing.inputs.iter().find_map(|input| {
                let external = input.external.as_ref()?;
                input.stream_ids().contains(&sid).then_some((
                    external.id,
                    external.epoch,
                    external.replay_inflight || external.verification_armed,
                ))
            })
        };
        // Not an external: an embedded track needs no replay, and there is no
        // resource to reconcile.
        let Some((id, epoch, effect_outstanding)) = target else {
            return;
        };
        // GUARD 2, checked first because it is the cheap one and because an
        // emit while an effect is outstanding is the failure mode that actually
        // hurts. Only advisory: the authority is `claim_replay`'s test-and-set
        // below, which re-asks under the same lock at the moment of the emit.
        if effect_outstanding {
            return;
        }
        // THE ANTI-FIGHT CONSULT, and it is a consult rather than a claim.
        //
        // A selection still awaiting its confirmation is about to change the
        // very routing this pass reads: the branch it would call unaligned may
        // be one decodebin3 is mid-swap on. Worse, the deadline machinery owns
        // what a lapsed selection MEANS, and a replay emitted underneath it
        // fights the give-up's own adoption of routed reality. So: if a
        // dispatch is in flight at all, or the select lane has not yet sent
        // it, this pass does nothing and re-asks a second later. Exactly the
        // pair of reads `Inner::confirm_upstream_selection` makes before it
        // manufactures a confirmation, for the same reason.
        //
        // Read one lock at a time, engine then hands, and nothing is held
        // across the emit below.
        let selecting = inner.selection.lock().selecting_seqnum();
        if let Some(seqnum) = selecting {
            let age = inner.hands.select_age(seqnum, Instant::now());
            debug!(
                ?id,
                ?seqnum,
                ?age,
                "reconcile: a selection is still in flight; not acting on routing it is \
                 about to change"
            );
            return;
        }
        // OBSERVED. Sticky reads only, routing lock alone (the
        // `probe_routed_selection` thread discipline verbatim).
        let (delivered, sids) = {
            let routing = inner.routing.lock();
            let Some(input) = routing.inputs.iter().find(|i| {
                i.external
                    .as_ref()
                    .is_some_and(|e| e.id == id && e.epoch == epoch)
            }) else {
                return;
            };
            let sids = input.stream_ids();
            (Inner::text_stream_delivered(&routing, &sids), sids)
        };
        let aligned = delivered && inner.subtitle_origin_matches_video();
        // GUARD 1: converged. THE fixpoint.
        if aligned {
            return;
        }
        // GUARD 3: SUPERSESSION. A stop or a load was requested after this
        // drain was queued (`Inner::queue_epoch` moves for nothing else), so
        // the divergence being measured belongs to an item the caller has
        // already left. Emitting into that window costs a flushing seek, and a
        // flushing seek costs decodebin3 a reconfigure - churn in exactly the
        // moments a teardown is about to deactivate those pads.
        //
        // HONESTLY LABELLED: this is exposure reduction, not the fix for the
        // 1800015 wedge. The captured stacks put that wedge's proximate cause
        // below this crate, and in the captured run the pass had not emitted
        // anywhere near the stop (the pipeline was in a buffering PAUSED, where
        // this function is not even reached). What this buys is a smaller
        // window, on a cheap read, for a job whose whole subject is an item
        // that no longer exists.
        //
        // AFTER guard 1 deliberately: a converged graph returns above and the
        // atomic is only paid on the path that would actually emit.
        //
        // The DRAIN is not gated by this and must not be. `Job::DrainTextWork`
        // is `StalePolicy::Run` because postponed disposals, removals and
        // flushes outlive an item change; only the re-derived emit is
        // item-scoped.
        let current = inner.queue_epoch.load(Ordering::SeqCst);
        if current != queued_epoch {
            debug!(
                ?id,
                queued_epoch,
                current,
                "reconcile: a stop or load was requested after this drain was queued; \
                 not replaying into the window it opened"
            );
            return;
        }
        debug!(
            ?id,
            epoch,
            delivered,
            ?sids,
            "reconcile: the selected external is not rendering aligned; replaying it"
        );
        RECONCILE_EMITS.fetch_add(1, Ordering::Relaxed);
        // A refused send discharges nothing and must not leave the bit behind
        // (it would suppress every future pass for this resource); a duplicate
        // still puts a replay for this key on the way, so the chain is armed
        // for it too. Both rules live in `Inner::claim_replay`.
        if !inner.claim_replay(id, epoch, "the reconcile pass").owed() {
            return;
        }
        inner.arm_replay_verification(id, epoch, 0);
    }

    /// The GRACE and the DEDUPE every text degradation report shares: answer
    /// [`DegradationEdge::Escalate`] exactly once per key, and only once the
    /// shape has outlasted `grace`.
    ///
    /// Both halves are needed and for different reasons, stated per kind on
    /// [`TextDegradation`]: the DEDUPE because the scans run on every poll for
    /// as long as the shape lasts (measured in the thousands of hits per
    /// second), the GRACE because the transient shapes are transient on the
    /// healthy path and a first-sight report would fire on every fast subtitle
    /// round trip and mean nothing.
    ///
    /// A ZERO grace escalates at first sight, which is what a verdict wants
    /// (caps the renderer cannot carry will not become carryable by waiting).
    ///
    /// The returned edge is the only thing any caller acts on, and the table's
    /// lock is released before it returns, so the expensive half (a pad walk, a
    /// warn, an emit into foreign code) is paid outside it. The REPAIRS beside
    /// these reports are gated by none of this and run on every poll, because a
    /// slot that can be given its caps back should get them at the first
    /// opportunity rather than after a grace.
    pub(crate) fn note_degradation(
        &self,
        kind: TextDegradation,
        key: &str,
        grace: Duration,
    ) -> DegradationEdge {
        let key = (kind, key.to_string(), self.current_generation());
        let mut table = self.text_degradations.lock();
        match table.get(&key).copied() {
            // Already escalated for this (kind, stream, load).
            Some(DegradationMemo::Spoken) => DegradationEdge::Silent,
            Some(DegradationMemo::Since(since)) if since.elapsed() >= grace => {
                table.insert(key, DegradationMemo::Spoken);
                DegradationEdge::Escalate
            }
            // Still inside the grace.
            Some(DegradationMemo::Since(_)) => DegradationEdge::Silent,
            // First sight starts the clock, and speaks at once where there is
            // no grace to wait out.
            None if grace.is_zero() => {
                table.insert(key, DegradationMemo::Spoken);
                DegradationEdge::Escalate
            }
            None => {
                table.insert(key, DegradationMemo::Since(Instant::now()));
                DegradationEdge::First
            }
        }
    }

    /// Whether [`Inner::note_degradation`] has already seen this key under the
    /// current load. The read half, for a gate that must not start a clock of
    /// its own: the link loop asks this once per candidate per poll and a
    /// writing ask would memo every stream it merely considered.
    pub(crate) fn noted_degradation(&self, kind: TextDegradation, key: &str) -> bool {
        let key = (kind, key.to_string(), self.current_generation());
        self.text_degradations.lock().contains_key(&key)
    }

    /// Whether the pipeline has come to rest at PAUSED, where a flush of the
    /// text branch cannot complete. See [`Inner::detach_text_parts`].
    pub(crate) fn resting_paused(pipeline: &gst::Pipeline) -> bool {
        let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
        current == gst::State::Paused && pending == gst::State::VoidPending
    }

    /// Park a text pad that has just lost its branch and record the parking
    /// sink on its routed entry, tearing the sink down if that entry vanished
    /// while the lock was down.
    ///
    /// THE KEEPING PARK in both callers, never the discarding one. What the
    /// park keeps is what the next join replays, and a discarding park here is
    /// the field bug "subtitles start a few seconds in": on a whole-period text
    /// Representation the item's opening seconds exist nowhere else.
    ///
    /// The park runs with the routing lock DOWN (it is bin surgery) and the
    /// entry is re-found afterwards, which is the whole reason the orphan arm
    /// exists: the stream can unroute in that window, leaving a parking sink
    /// with nothing to belong to.
    ///
    /// `what` names the caller for its two log lines.
    fn repark_or_orphan(inner: &Arc<Inner>, db3_src_pad: &gst::Pad, what: &str) {
        let parked = inner.park_text_stream(db3_src_pad);
        let orphaned = {
            let mut routing = inner.routing.lock();
            match routing
                .routed
                .iter_mut()
                .find(|routed| routed.db3_src_pad == *db3_src_pad)
            {
                Some(routed) => {
                    match parked {
                        Ok((sink, park)) => {
                            debug!(pad = %db3_src_pad.name(), what, "parked the text stream");
                            routed.park_sink = Some(sink);
                            routed.park_pad = Some(park);
                        }
                        Err(err) => warn!(?err, what, "failed to park the text stream"),
                    }
                    None
                }
                // The stream unrouted while the lock was down, so its parking
                // sink has nothing left to belong to.
                None => parked.ok(),
            }
        };
        if let Some((sink, park)) = orphaned {
            let _ = db3_src_pad.unlink(&park);
            let _ = sink.set_state(gst::State::Null);
            let _ = inner.pipeline.remove(&sink);
        }
    }

    /// Move live text streams back to the parking sink (video
    /// going away, or subtitles dropped). See `detach_text_branch`.
    pub(crate) fn park_text_streams(inner: &Arc<Inner>) {
        // The eager park is the decider's (see `Inner::decider_only`): it
        // takes the text branch out of the graph, which is a link decision
        // like any other.
        inner.decider_only("the text-branch park");
        // The pads come out of the entries under the lock, everything that
        // touches the pipeline runs with the lock released (see
        // `Inner::live_text_downstream_pads` for the deadlock this avoids).
        let detached: Vec<(
            gst::Pad,
            gst::Pad,
            Option<gst::Element>,
            Option<gst::Element>,
        )> = {
            let mut routing = inner.routing.lock();
            routing
                .routed
                .iter_mut()
                .filter(|r| r.kind == StreamKind::Text && r.downstream.is_some())
                .map(|routed| {
                    (
                        routed.db3_src_pad.clone(),
                        routed.downstream.take().expect("filtered on Some above"),
                        routed.tqueue.take(),
                        routed.appsink.take(),
                    )
                })
                .collect()
        };
        for (db3_src_pad, downstream, tqueue, appsink) in detached {
            Inner::detach_text_parts(inner, &db3_src_pad, &downstream, tqueue, appsink, false);
            // The KEEPING park, like the route-time one. A re-enable can make
            // decodebin3 re-point this very pad with no pad-added (the reuse
            // shape), and the re-fetched track then crosses THIS park before
            // any join can exist; a discarding park here is a whole-period
            // track's entire re-select burst lost. The 2 s replay window is
            // what keeps cues consumed mid-park from coming back stale.
            Self::repark_or_orphan(inner, &db3_src_pad, "the eager park");
        }
    }

    /// Ask the decider to re-drive the text link policy (see
    /// [`Job::PollTextPolicy`]).
    ///
    /// For every caller that is NOT the decider. The policy links, reclaims
    /// and evicts a dead text branch, which is bin surgery under
    /// the routing lock decided against a pipeline state read a moment
    /// earlier. Running it from four threads is the TOCTOU class the decider
    /// ownership closes. What each of those threads keeps is the right to
    /// SAY that something may have changed.
    ///
    /// Coalesced by [`Inner::queue_coalesced_text_poll`], which is the reason
    /// this is cheap enough to be called from a receiver's 5ms poll loop.
    pub(crate) fn request_text_policy_poll(inner: &Arc<Inner>) {
        inner.queue_coalesced_text_poll(None);
    }

    /// Queue ONE [`Job::PollTextPolicy`] behind the coalescing bit, the
    /// protocol every asker shares.
    ///
    /// The swap admits one queued poll at a time and folds the rest, and
    /// folding loses nothing because the job re-reads the world it decides
    /// from. The bit is cleared by the JOB before it runs, so a fold can only
    /// ever swallow a poke the pending run has not yet answered.
    ///
    /// THE SHARP EDGE, stated once here because it is what the two askers used
    /// to guard separately: a bit left set silences every later poll, for good.
    /// So a refused `queue_job` takes it straight back out. Nothing would clear
    /// it in that case (there is no decider left, the crate is dying), which
    /// changes nothing at the time, but the shape must not be reachable at all.
    ///
    /// `queued` is logged only when this call really queues, and only for the
    /// asker that wants a line; the 5 ms poll loop passes `None` and stays
    /// silent.
    pub(crate) fn queue_coalesced_text_poll(&self, queued: Option<&str>) {
        if self.poll_queued.swap(true, Ordering::SeqCst) {
            Counters::bump(&self.counters.poll_policy_coalesced);
            return;
        }
        if let Some(why) = queued {
            debug!(why, "queueing a text policy poll");
        }
        if !self.queue_job(Job::PollTextPolicy) {
            self.poll_queued.store(false, Ordering::SeqCst);
        }
    }

    /// Link any routed-but-unlinked text stream into a consumer tail, once
    /// the pipeline is SETTLED (at least PAUSED, no async transition
    /// pending) and a video stream is routed (text is consumed against video
    /// buffers, see `park_text_streams`). Triggered by the events that can
    /// change the answer - a routed pad, a finished video join, a state
    /// settle, a caller's own state-change handler - through
    /// [`Inner::request_text_policy_poll`] for everyone but the decider,
    /// which is the only thread that runs this.
    ///
    /// The `pending == VoidPending` requirement is load-bearing: splicing
    /// the text branch into a load's async preroll adds a
    /// reconfiguration that wedges it under churn. Linking at a SETTLED
    /// PAUSED is safe and necessary: a subtitle switch performed while
    /// paused never reaches PLAYING before the caller's re-emit flush, so
    /// requiring PLAYING would leave the new track's cue invisible until
    /// resume. The idle-video-block gst patch is what makes the branch
    /// reconfiguration reliable at steady PAUSED.
    pub(crate) fn poll_text_policy(inner: &Arc<Inner>) {
        // A settle point is a drain point too, but the drain gets its own
        // job rather than running here: its flush blocks until the branch's
        // streaming thread can be paused, and a drain inline here would put
        // that wait wherever this ran. The decider is the only carrier left
        // (the routing thread's tail asks now, it does not call), but the
        // separation still holds the decider itself out of a flush that a
        // link decision does not need.
        //
        // The poke is skipped while the last drain's no-op verdict stands
        // (see `Inner::drain_poke_parked`). Without that, a caller polling
        // every 5ms at a pipeline parked below PLAYING re-queued this job on
        // every poll, indefinitely, and the worker spent the whole park
        // spinning on drains that could only early-return (measured at 5099
        // jobs over one 39-second Buffering park on the fuzz driver's seed
        // 400009). The pipeline state edges still queue the drain
        // unconditionally on the bus, so a settled PLAYING still drains
        // promptly with no poll at all.
        if inner.has_deferred_text_work() && !inner.drain_poke_parked.load(Ordering::SeqCst) {
            inner.queue_job(Job::DrainTextWork);
        }
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::text_may_link(current, pending) {
            return;
        }
        // From here down this function performs SURGERY - it releases held
        // externals, evicts a dead branch and links a
        // fresh one into it - and surgery is the decider's alone (see
        // `Inner::decider_only`). Asserted below the gate rather than at the
        // top: a poll that finds the pipeline unsettled decides nothing, and
        // asking that question is allowed from anywhere.
        inner.decider_only("the text-policy surgery");
        // TEST FAULT INJECTION (see [`TestStaging::joins`]): bring up any
        // branch whose staged join window has expired. Here rather than in the
        // join, and off the sleep it used to be, so the rest of the graph keeps
        // running through the window, which is the only condition under which
        // anything can cross the staged link and reproduce the latch.
        inner.stage_release_due_joins();
        // TEST FAULT INJECTION (see [`TestStaging::text_caps_loss`]): take a
        // parked stream's sticky CAPS away here, ahead of the gate below, so
        // the poll that follows sees exactly what the caps loss leaves behind.
        Self::stage_text_caps_loss(inner);
        // Before anything links: a gapless boundary leaves the text branch on a
        // different running-time origin than A/V, and a branch linked onto the
        // wrong origin never renders (see there). This is the trigger for a text
        // stream that JOINS after the boundary, since `route_db3_pad` calls us
        // when its pad appears. A branch already live across the boundary never
        // comes through here; that one is trigger #2, the `video_sink` probe.
        inner.sync_text_running_time();
        // A re-attached external can materialize under the very stream id
        // the engine already has applied (stream ids are URI-derived, so
        // detaching a URL and attaching it again reuses the id). The engine
        // then has nothing to dispatch, no SELECT_STREAMS confirmation ever
        // arrives, and the confirmation-time hold release never runs. The
        // input's data stayed blocked at its source pads for good while the
        // selection reported success. Lift the hold here instead whenever
        // the held stream IS the confirmed subtitle (a cheap no-op when
        // nothing is held).
        //
        // OBSERVED versus the mirror. `last_applied_subtitle` is a remembered
        // write; `observed_seat_occupant` asks the graph who is actually in
        // the seat. Where they disagree the graph wins and the mirror's lie is
        // logged with both values - the narrow mirror retirement of plan
        // section 2.10. The mirror is not deleted: the dispatch-time
        // `replacing` read and the upstream adoption write below both answer
        // questions no probe can (an INTENT being dispatched, and a mode with
        // no confirmation channel where the write IS the protocol).
        let confirmed: Vec<String> = {
            let mirror = inner.last_applied_subtitle.lock().clone();
            let observed = inner.observed_seat_occupant();
            if observed.is_some() && mirror.is_some() && observed != mirror {
                warn!(
                    ?mirror,
                    ?observed,
                    "the live text branch disagrees with last_applied_subtitle"
                );
            }
            // BOTH, when they differ. Releasing a hold is idempotent and
            // cheap, and the two answers are about different failure modes: an
            // observed occupant the mirror never recorded is a branch that
            // linked without a confirmation, and a mirrored sid the graph has
            // not got is an external still waiting for its branch. Dropping
            // either would leave one of them held for good, which is the bug
            // this release exists to prevent - so the divergence is LOGGED and
            // both are acted on, rather than one of them being guessed at.
            let mut both: Vec<String> = observed.into_iter().chain(mirror).collect();
            both.dedup();
            both
        };
        if !confirmed.is_empty() {
            // Nothing is owed here: this path exists precisely because no
            // `STREAMS_SELECTED` arrived, so no replay was queued against it.
            inner.unblock_selected_externals(&confirmed, None);
        }
        // Upstream-selection mode has NO confirmation channel for an
        // external's sid at all (decodebin3 never posts there, the demuxer
        // cannot name a foreign stream), so the confirmed release above can
        // never fire for one. The engine's applied slot is the same authority
        // that gates the join below; it drives the hold too, adopted as the
        // confirmed slot, with the realigning replay the confirmation path
        // would have queued.
        if inner.upstream_owns_selection() {
            let applied = inner.selection.lock().subtitle_sid();
            let held = applied.and_then(|sid| {
                let routing = inner.routing.lock();
                routing.inputs.iter().find_map(|input| {
                    input.external.as_ref().and_then(|external| {
                        (external.hold == Hold::UntilSelected && input.stream_ids().contains(&sid))
                            .then(|| (external.id, external.epoch, sid.clone()))
                    })
                })
            });
            if let Some((id, epoch, sid)) = held {
                debug!(
                    ?id,
                    %sid,
                    "upstream owns selection; releasing the held external the applied slot names"
                );
                *inner.last_applied_subtitle.lock() = Some(sid.clone());
                // BEHIND the per-resource in-flight bit, like every other
                // emitter (see [`Inner::claim_replay`]). This site set the bit
                // and then queued REGARDLESS, so a reconcile emit for the same
                // applied sid one poll earlier left two `ReplaySub` jobs in the
                // queue for one `(id, epoch)`; the choke point inside
                // `replay_subtitle` can only collapse them while a seek is
                // TRAVELLING, and the first outcome landing before the second
                // job ran is the double flush the enqueue guard exists to
                // close.
                let claim = inner.claim_replay(id, epoch, "the upstream release");
                // Owed on a collapsed duplicate too (`ReplayClaim::owed`): the
                // hold is discharged by the outcome of a replay for this
                // `(id, epoch)` and the pending one carries the same key (the
                // selection-time emitter's rule, see
                // [`Inner::release_owed_hold`]).
                let owed = claim.owed().then_some((id, epoch));
                inner.unblock_selected_externals(std::slice::from_ref(&sid), owed);
            }
        }
        // WHICH stream may relink at all, snapshotted before the routing lock
        // (see [`text_seat::allowed_sid`], which carries the stomp guard's
        // whole argument).
        let allowed_sid = {
            let selection = inner.selection.lock();
            text_seat::allowed_sid(
                selection.subtitle_explicitly_off(),
                inner.stage_link_stomped_subtitle(),
                selection.subtitle_sid(),
            )
        };
        // THE STALEMATE BREAK, and it runs before every reclaim below because
        // all of them can only MOVE a seat that exists (see
        // [`text_seat::stalemate_broken`] for the 137-of-160 shape it answers).
        //
        // The lock is taken only when a stream is allowed, as before: with
        // nothing allowed the projection is all-foreign and the rule cannot
        // fire, so the acquisition would buy nothing.
        if let Some(allowed) = allowed_sid.as_deref() {
            let mut routing = inner.routing.lock();
            let view = routing.text_seat_view(Some(allowed));
            if text_seat::stalemate_broken(&view) {
                TEXT_SEAT_STALEMATES.fetch_add(1, Ordering::Relaxed);
                let pads: Vec<String> = view
                    .iter()
                    .filter(|entry| entry.same_sid)
                    .map(|entry| {
                        let routed = &mut routing.routed[entry.index];
                        let name = format!(
                            "{}(superseded={} evicted_dead={} segment={})",
                            routed.db3_src_pad.name(),
                            entry.superseded,
                            entry.evicted_dead,
                            entry.has_segment,
                        );
                        routed.superseded = false;
                        routed.evicted_dead = false;
                        name
                    })
                    .collect();
                warn!(
                    sid = %allowed,
                    ?pads,
                    "every routed pad for the selected text stream was latched out of \
                     contention with no branch holding the seat, so nothing could ever \
                     build one; clearing the latches so the link policy can re-run"
                );
            }
        }
        // THE SEAT RECLAIM: four rules whose ORDER is the rule, all four pure
        // over the projection. Each one's measured argument lives on
        // [`text_seat::select_victim`]; this side does the surgery only.
        let reclaim = {
            let mut routing = inner.routing.lock();
            let view = routing.text_seat_view(allowed_sid.as_deref());
            text_seat::select_victim(&view).map(|plan| {
                // The two rules that repair a silently wrong seat say so,
                // before anything moves.
                match plan.rule {
                    ReclaimRule::EndedSlot => {
                        info!(
                            held = %routing.routed[plan.held].db3_src_pad.name(),
                            "decodebin3 ended the slot this text branch is ghosting while another \
                             pad for the same stream is still live; moving the seat off the dead one"
                        );
                        TEXT_EOS_SEAT_RECLAIMS.fetch_add(1, Ordering::Relaxed);
                    }
                    ReclaimRule::FlowTicket => {
                        let winner = plan.revive.expect("the flow rule names its winner");
                        info!(
                            held = %routing.routed[plan.held].db3_src_pad.name(),
                            held_flow = plan.held_flow,
                            winner = %routing.routed[winner].db3_src_pad.name(),
                            winner_flow = plan.winner_flow,
                            "decodebin3 moved this text stream back to another pad, following the data"
                        );
                    }
                    ReclaimRule::SegmentlessHolder | ReclaimRule::SupersededOrder => {}
                }
                // The winner is the pad decodebin3 feeds, so whatever verdict
                // an earlier reclaim recorded against it is out of date and
                // must not survive into the link loop (which refuses
                // `superseded` outright, and `evicted_dead` while the pad is
                // segmentless). Only the flow rule names one.
                if let Some(winner) = plan.revive {
                    let winner = &mut routing.routed[winner];
                    winner.superseded = false;
                    winner.evicted_dead = false;
                }
                // THE TWO STORED FLAGS STAY TWO, and this is where the verdict
                // decides them. A holder that merely lost its segment may yet
                // revive, and `evicted_dead` alone is the right, clearable
                // verdict for it; a holder decodebin3 has REPLACED never will
                // (see [`RoutedStream::superseded`]).
                let superseded = plan.verdict == SeatVerdict::Replaced;
                let routed = &mut routing.routed[plan.held];
                // THE PAIR INVARIANT, asserted where it is relied on.
                // The rules filter on `downstream` alone, while the slot rule
                // this eviction exists to unblock is the PAIR
                // (`consumer_branch_live`: `appsink.is_some() &&
                // downstream.is_some()`). The two are written and cleared
                // together (the link loop sets both on a successful join,
                // every detach takes both) so a text entry cannot hold one
                // without the other. If it ever could, this eviction would be
                // aimed at a branch that was not occupying the slot, and the
                // stream it is clearing the way for would stay refused with
                // nothing in the log to say why.
                debug_assert!(
                    routed.appsink.is_some(),
                    "a routed text entry has a downstream but no appsink, so the \
                     eviction and `consumer_branch_live` disagree about which \
                     branch holds the one live text slot"
                );
                routed.evicted_dead = true;
                routed.superseded = superseded;
                (
                    routed.db3_src_pad.clone(),
                    routed.downstream.take().expect("the rules filtered on Some"),
                    routed.tqueue.take(),
                    routed.appsink.take(),
                    superseded,
                )
            })
        };
        if let Some((db3_src_pad, downstream, tqueue, appsink, superseded)) = reclaim {
            warn!(
                pad = %db3_src_pad.name(),
                superseded,
                "reclaiming the text slot from a dead branch"
            );
            Inner::detach_text_parts(inner, &db3_src_pad, &downstream, tqueue, appsink, false);
            // Parked rather than left dangling, exactly like
            // `park_text_streams`. Should the pad live on, decodebin3 must
            // be able to drain it, and the KEEPING park, because "live on"
            // is exactly the reuse shape: a reclaim can evict this pad one
            // poll before decodebin3 re-points it (measured),
            // and the re-select's burst then lands here before the flow
            // reclaim can re-admit the pad. What the park keeps is what that
            // join replays.
            Self::repark_or_orphan(inner, &db3_src_pad, "the slot reclaim");
        }
        let mut routing = inner.routing.lock();
        if !routing
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Video && r.downstream.is_some())
        {
            return;
        }
        let mut joined: Option<String> = None;
        // The external whose stream this call joined, so the park-replayed
        // cues below count as its delivery evidence (see
        // [`Inner::external_cues_fed`]).
        let mut joined_external: Option<crate::ExternalSubId> = None;
        // Why each candidate was refused, reported below when nothing joined.
        // A discriminant and a pad reference, rendered only if that line is
        // actually enabled (see [`RefusedText`]).
        let mut refusals: Vec<RefusedText<'_>> = Vec::new();
        // Degradation reports formed under the routing lock and emitted after it, the
        // `refusals` discipline applied to an event rather than a log line.
        let mut unsupported: Vec<(String, gst::Caps)> = Vec::new();
        // The same, for a branch that could not be WIRED (see the link arm).
        let mut unwirable: Vec<(String, Option<String>, gst::Caps)> = Vec::new();
        // The same, for a stream the caps gate has now been refusing for
        // longer than any transient could last (see [`CapslessTextStall`]).
        let mut stalled: Vec<CapslessTextStall> = Vec::new();
        // The same, for the join-window read: branches whose pads were
        // still inactive when the upstream link went in (see the link arm).
        let mut linked_before_active: Vec<String> = Vec::new();
        // What the park kept for a branch this call joins, fed PAST THE LOCK
        // with the other foreign calls: `feed_subtitle` runs the consumer,
        // and a consumer that blocks (its contract forbids it; a renderer
        // missing a frame deadline does it anyway) must wedge at most the
        // poller, never `pump_selection`, whose first read is `routing`
        // (`caller_bounded_switch` pins exactly that).
        let mut replay_cues: Vec<SubtitleFeedItem> = Vec::new();
        // What the rest of the contest says, snapshotted before the loop starts
        // mutating entries (see [`SeatContest`]).
        //
        // `consumer_branch_live` reads the PAIR, which is the slot rule the
        // one-live-branch arm enforces, and is updated when this call links a
        // branch. `live_rival_waiting` is THE OTHER HALF of the EOS reclaim
        // above: read once rather than per candidate, and blind to pads this
        // loop refuses itself, so the loop cannot refuse every pad on the
        // strength of a rival it is about to refuse as well.
        let mut contest = SeatContest {
            live_rival_waiting: routing
                .text_seat_entries(allowed_sid.as_deref())
                .any(|entry| entry.seat_ready_rival()),
            consumer_branch_live: routing.routed.iter().any(|r| {
                r.kind == StreamKind::Text && r.appsink.is_some() && r.downstream.is_some()
            }),
        };
        // Which text sids an EXTERNAL input serves (and whose), snapshotted
        // before the loop takes `routed` mutably: the misaligned-cue gates
        // apply only to streams the replay machinery can re-deliver, and the
        // live-feed gate must key on THIS input's replays (a replay in flight
        // for another external re-delivers nothing here).
        let external_sids: std::collections::HashMap<String, crate::ExternalSubId> = routing
            .inputs
            .iter()
            .filter_map(|input| Some((input, input.external.as_ref()?.id)))
            .flat_map(|(input, id)| {
                input
                    .text_stream_ids()
                    .into_iter()
                    .map(move |sid| (sid, id))
            })
            .collect();
        for (index, routed) in routing.routed.iter_mut().enumerate() {
            if routed.kind != StreamKind::Text || routed.downstream.is_some() {
                continue;
            }
            // ONE read of the pad's stream id, shared by the projection below
            // and by every report this loop forms: two reads can straddle a
            // STREAM_START and disagree about which stream this entry carries.
            let sid = routed.db3_src_pad.stream_id();
            // THE ADMISSION CASCADE: six arms whose order is itself the rule,
            // pure over the projection (see [`text_seat::admit`], which carries
            // each arm's argument, the EOS-before-evicted ordering included).
            //
            // The unwirable question goes in as a CLOSURE because answering it
            // costs a lock and a key allocation, and the five arms above it
            // filter almost every candidate.
            let entry = routed.text_view(index, sid.as_deref(), allowed_sid.as_deref());
            let admission = text_seat::admit(&entry, &contest, || {
                inner.noted_degradation(
                    TextDegradation::Unwirable,
                    &Self::unwirable_key(sid.as_deref(), &routed.db3_src_pad),
                )
            });
            // PROOF OF LIFE, applied even when a later arm refuses: a segment
            // on the pad means the branch revived, and clearing the latch here
            // is what lets it compete on the next poll, when the branch ahead
            // of it may be gone.
            if admission.revived {
                routed.evicted_dead = false;
            }
            if let Some(refusal) = admission.refusal {
                refusals.push(RefusedText {
                    pad: routed.db3_src_pad.clone(),
                    allowed: allowed_sid.as_deref(),
                    refusal,
                });
                continue;
            }
            // The caps gate, and its loud degradation. Caps absent means
            // the pad has not carried its sticky CAPS yet (a branch is
            // always parked before it may link, so this is rare and
            // transient): refuse this poll WITHOUT reporting, and let a
            // later one decide.
            let caps = routed.db3_src_pad.current_caps();
            if caps
                .as_ref()
                .and_then(|c| decisions::consumer_stream_format(c))
                .is_none()
            {
                refusals.push(RefusedText {
                    pad: routed.db3_src_pad.clone(),
                    allowed: allowed_sid.as_deref(),
                    refusal: Refusal::CapsUnsupported,
                });
                // Only a stream that HAS caps has been refused on its
                // merits; a pad still waiting for its sticky is not a
                // capability failure and must not be reported as one.
                //
                // COLLECTED, NOT EMITTED. The event handler is foreign
                // code and the routing lock is held right here; every
                // other diagnostic in this loop is deferred past the
                // lock for exactly that reason (`refusals`), and an
                // emit under a crate lock is the shape the rest of the
                // crate is written to avoid.
                if let (Some(caps), Some(sid)) = (caps.as_ref(), sid.as_deref()) {
                    unsupported.push((sid.to_string(), caps.clone()));
                }
                // THE STALL WATCH, for the caps-ABSENT half only. A stream
                // refused on caps it HAS is already reported through
                // `unsupported`; one with no caps at all is the silent case
                // this exists for. Deferred like everything else in this
                // loop.
                if caps.is_none()
                    && let Some(sid) = sid.as_deref()
                    && inner.note_degradation(
                        TextDegradation::CapslessStall,
                        sid,
                        CAPSLESS_TEXT_GRACE,
                    ) == DegradationEdge::Escalate
                {
                    let pad = &routed.db3_src_pad;
                    stalled.push(CapslessTextStall {
                        caps_path: Inner::caps_path_dump(pad),
                        sid: sid.to_string(),
                        pad: pad.name().to_string(),
                        sticky_stream_start: pad
                            .sticky_event::<gst::event::StreamStart>(0)
                            .is_some(),
                        sticky_segment: pad.sticky_event::<gst::event::Segment>(0).is_some(),
                        linked: pad.is_linked(),
                        parked_on: pad
                            .peer()
                            .and_then(|peer| peer.parent_element())
                            .map(|element| element.name().to_string()),
                    });
                }
                continue;
            }
            // Build the per-stream queue (see `RoutedStream::tqueue`) and
            // wire db3-text-pad -> queue -> appsink. The upstream link comes
            // last so data only flows once the chain is complete.
            // NAMED after the decodebin3 pad it serves. This is the only plain
            // `queue` the crate leaves unnamed anywhere in the pipeline, so
            // GStreamer auto-named it `queueN` and a field log reporting
            // "queue5: Failed to push event" took a source audit to attribute
            // (see `dash-start-seek-text-join-race.md`, which had to establish
            // it by exhaustion). A name costs nothing and makes the next such
            // log self-describing.
            let tqueue = match gst::ElementFactory::make("queue")
                .name(format!("fpb-tqueue-{}", routed.db3_src_pad.name()))
                .property("silent", true)
                .build()
            {
                Ok(q) => q,
                Err(err) => {
                    warn!(?err, "failed to create the text queue");
                    continue;
                }
            };
            if let Err(err) = inner.pipeline.add(&tqueue) {
                warn!(?err, "failed to add the text queue");
                continue;
            }
            let queue_entry = tqueue.static_pad("sink").expect("queue has a sink");
            // The branch's TAIL: a per-stream appsink that pulls cues out to
            // the consumer.
            let external = sid.as_deref().and_then(|s| external_sids.get(s).copied());
            let Some(appsink) =
                Inner::build_text_consumer_tail(inner, &routed.db3_src_pad, &tqueue, external)
            else {
                warn!("failed to wire the text queue into its consumer tail");
                let _ = tqueue.set_state(gst::State::Null);
                let _ = inner.pipeline.remove(&tqueue);
                continue;
            };
            // THE JOIN'S PRECONDITION, checked rather than assumed.
            //
            // The ordering above is deliberate and correct on its face: the
            // tail is linked and synced, then the queue is synced, and only
            // then does the upstream link happen. What it CANNOT assume is
            // that a returned `sync_state_with_parent` means the branch's pads
            // are active. Pads are activated in READY -> PAUSED, so a branch
            // synced against a parent that has not reached PAUSED (a load
            // still bringing the pipeline up, a buffering park, a bin mid
            // state-change) is added, linked, and still FLUSHING for want of
            // ever having been activated (gstpad.c:441).
            //
            // Linking a LIVE decodebin3 output to that is the join-window
            // latch: the first push across the new link returns FLUSHING into
            // `gst_single_queue_push_one`, the slot latches for good, and the
            // branch coming up a millisecond later does not revive it. The
            // captured failure is that shape: joined mid-transition, silent
            // for six seconds, and the demuxer's discard six seconds later is
            // simply the first push through a slot dead since the join.
            //
            // This crate cannot make `sync_state_with_parent` synchronous, so
            // it MEASURES instead: the repair for this is
            // `Inner::heal_latched_text_slots`, and this counter is how a
            // field capture tells "the join raced the activation" apart from
            // "something else latched the slot". Read under the lock (two
            // atomic pad-flag reads), reported after it, the `refusals`
            // discipline.
            let branch_active = queue_entry.is_active()
                && appsink
                    .static_pad("sink")
                    .is_some_and(|pad| pad.is_active());
            if !branch_active {
                JOINS_INTO_AN_INACTIVE_BRANCH.fetch_add(1, Ordering::Relaxed);
                linked_before_active.push(routed.db3_src_pad.name().to_string());
            }
            // Out of the park, into the renderer (through its queue). Text
            // bypasses ssync, so it links from the decodebin3 pad directly.
            //
            // The park's SINK outlives the unlink here on purpose (C8b): the
            // slot's loop thread is parked inside its preroll, and whichever
            // side of the new link is in place when that push is released is
            // where the push lands. Retired below, after the link, on both
            // arms (see `Inner::unpark_stream_for_join`).
            let retired_park = inner.unpark_stream_for_join(routed);
            let link = routed.db3_src_pad.link(&queue_entry);
            if let Some(sink) = &retired_park {
                inner.drop_parking_sink(sink);
            }
            match link {
                Ok(_) => {
                    info!(
                        pad = %routed.db3_src_pad.name(),
                        // Whether the pad can time its first cue. A join
                        // without one means the branch's first buffer crosses
                        // with no running time to compute against ("Got data
                        // flow before segment event"), which is a cue placed
                        // at the wrong instant rather than a missing one.
                        segment = routed
                            .db3_src_pad
                            .sticky_event::<gst::event::Segment>(0)
                            .is_some(),
                        "text stream joined its consumer tail"
                    );
                    // TEST FAULT INJECTION (see `TestStaging::join_hold_ms`).
                    // The branch was deliberately left at NULL so THIS link
                    // landed on inactive pads; hold it there long enough for
                    // the demuxer to put something across the new link (a
                    // sparse text track's GAP tick is once a second) and only
                    // then bring it up, exactly as a `sync_state_with_parent`
                    // that lost the race would have.
                    let hold = inner.stage_hold_join(&tqueue, &appsink);
                    if hold > 0 {
                        warn!(
                            pad = %routed.db3_src_pad.name(),
                            hold_ms = hold,
                            "TEST STAGING: holding a joined text branch at NULL"
                        );
                    }
                    // WHAT THE PARK HELD, now that there is somewhere to put
                    // it. The branch is linked but no cue has crossed it yet:
                    // everything from the pad's first buffer up to this
                    // instant went into the park, and on a whole-period text
                    // Representation that is the item's opening seconds with
                    // no second copy anywhere (see `Inner::parked_text_cues`).
                    //
                    // AFTER the link and not before, so a link that fails
                    // replays nothing into a branch that does not exist. The
                    // take also ARMS the branch's entry probe to skip its own
                    // STREAM_START clear, which would otherwise wipe these
                    // very cues moments from now. See
                    // `build_text_consumer_tail`, where that is the whole
                    // difference between the repair landing and not. The FEED
                    // is deferred past the lock (`replay_cues`).
                    replay_cues = inner.take_parked_text_cues(
                        &routed.db3_src_pad,
                        external.is_some(),
                        !routed.ever_joined,
                    );
                    routed.ever_joined = true;
                    routed.downstream = Some(queue_entry);
                    routed.tqueue = Some(tqueue);
                    contest.consumer_branch_live = true;
                    routed.appsink = Some(appsink);
                    joined = sid.map(|s| s.to_string());
                    joined_external = external;
                }
                Err(err) => {
                    warn!(
                        ?err,
                        pad = %routed.db3_src_pad.name(),
                        caps = ?routed.db3_src_pad.current_caps().map(|c| c.to_string()),
                        negotiable = %routed.db3_src_pad.query_caps(None),
                        "failed to link text stream into its renderer"
                    );
                    // LOUDLY, AND ONCE. A link GStreamer refuses will be
                    // refused again for the same reason on the next tick, so
                    // retrying is a build-and-tear-down loop that runs for the
                    // rest of the item and tells the caller nothing. It is the
                    // same user-visible outcome as an unrenderable track --
                    // the branch stays parked and no cue is ever shown -- so it
                    // takes the same degradation report, deduped per (sid, generation),
                    // and the stream is not attempted again under this load.
                    //
                    // The caps reported are the NEGOTIABLE ones, not the ones
                    // flowing: a link is decided on `gst_pad_query_caps`, and
                    // naming what actually blocked it is what makes the event
                    // actionable (`application/x-subtitle-vtt` for an embedded
                    // DASH WebVTT track whose buffers were already parsed).
                    unwirable.push((
                        Self::unwirable_key(sid.as_deref(), &routed.db3_src_pad),
                        sid.as_deref().map(|s| s.to_string()),
                        routed.db3_src_pad.query_caps(None),
                    ));
                    let _ = tqueue.set_state(gst::State::Null);
                    let _ = inner.pipeline.remove(&tqueue);
                    let _ = appsink.set_state(gst::State::Null);
                    let _ = inner.pipeline.remove(&appsink);
                    // The stream was already unparked. It must not stay
                    // unlinked (decodebin3 cannot drain a deselected sparse
                    // stream into an unlinked pad), so park it again, the
                    // keeping park, like every other text park.
                    match inner.park_text_stream(&routed.db3_src_pad) {
                        Ok((sink, park)) => {
                            routed.park_sink = Some(sink);
                            routed.park_pad = Some(park);
                        }
                        Err(err) => warn!(?err, "failed to re-park the text stream"),
                    }
                }
            }
        }
        // A wanted sid nothing carries is otherwise a SILENT wedge (every
        // mismatch above just `continue`s): name the parked inventory so a
        // field log identifies the blocker, e.g. a drained external whose
        // multiqueue slot decodebin3 reclaimed for another stream
        // (`ext-subtitle-regression-2.txt`).
        if joined.is_none()
            && let Some(allowed) = allowed_sid.as_deref()
            && !routing.routed.iter().any(|r| {
                r.kind == StreamKind::Text && r.db3_src_pad.stream_id().as_deref() == Some(allowed)
            })
        {
            let text_pads: Vec<String> = routing
                .routed
                .iter()
                .filter(|r| r.kind == StreamKind::Text)
                .map(|r| {
                    format!(
                        "{}={:?} linked={}",
                        r.db3_src_pad.name(),
                        r.db3_src_pad.stream_id().as_deref(),
                        r.downstream.is_some()
                    )
                })
                .collect();
            debug!(
                allowed,
                ?text_pads,
                "no routed text pad carries the allowed sid"
            );
        }
        // The OTHER silent shape, and the one the field hit: a carrier pad
        // exists, its input delivers, and the branch still did not join, so
        // every candidate was refused for one of the reasons above. Without
        // this the log shows a selection that confirmed and simply never
        // rendered, with nothing to point at.
        // NOT when the allowed sid is already linked. The link loop
        // only considers entries with `downstream: None`, so a branch that
        // joined on an EARLIER poll is invisible to it and every other text
        // entry lands in `refusals`: the first version of this line fired twice
        // on a field log where the wanted stream was joined and rendering, which
        // is worse than no diagnostic. `joined` means "linked by THIS call",
        // which is not the same question.
        let already_joined = allowed_sid.as_deref().is_some_and(|allowed| {
            routing.routed.iter().any(|r| {
                r.kind == StreamKind::Text
                    && r.downstream.is_some()
                    && r.db3_src_pad.stream_id().as_deref() == Some(allowed)
            })
        });
        if joined.is_none() && !refusals.is_empty() && allowed_sid.is_some() && !already_joined {
            debug!(
                allowed = ?allowed_sid.as_deref(),
                ?refusals,
                "no text branch joined its consumer tail"
            );
        }
        // EVERY join of an external stream replays its input: by join
        // time anything may have drained its data beyond reach (deselect
        // drains, the deselect-race death, auto-select releasing the hold
        // into a parked branch), no flag can track those orderings, and
        // the replay is idempotent. Queued only AFTER the link, so the
        // replayed data lands in the live branch and not in the parking sink.
        //
        // BEHIND THE IN-FLIGHT BIT (see [`Inner::claim_replay`], which carries
        // the protocol), and this was the emitter that was not.
        // `subtitle-reenable-freeze.txt` caught the cost: the selection-time
        // emitter (see `Inner::external_stream_slotless`'s caller) set the bit
        // and queued a `ReplaySub`; 1.5 ms later THIS site queued a second one
        // for the same `(id, epoch)` while the first was still being carried;
        // the first outcome (effect 12) cleared both this bit and
        // `ExternalInput::replay_seek_outstanding` BEFORE the second job ran,
        // so the choke point inside `replay_subtitle` let it through and effect
        // 13 flushed the branch a second time.
        //
        // `seated` is read before the `joined` sid is consumed below; see the
        // follow-up poll at the tail of this function for what it is for.
        let seated = joined.is_some();
        // COLLECTED under the lock, CLAIMED past it. The in-flight bit lives on
        // the resource, so `Inner::claim_replay` takes the routing lock this
        // scope is holding. Still strictly after the link either way, which is
        // the ordering that matters: the replayed data has to land in the live
        // branch and not in the parking sink.
        let join_replays: Vec<(crate::ExternalSubId, u32)> = match joined.as_ref() {
            Some(sid) => routing
                .inputs
                .iter()
                .filter_map(|input| {
                    let external = input.external.as_ref()?;
                    // TWO ARMS, ONE BODY, and the coincidence is deliberate.
                    // This asks "is the selection gate discharged", NOT "can
                    // buffers flow": `OwedToReplay` still has its probes in.
                    // The claim below collapses onto the outstanding replay in
                    // the common case (`ReplayClaim::Duplicate`, a log line and
                    // nothing else), and in the window where an owing has no
                    // replay left travelling it is a discharge path that
                    // matters: `FcastPlaybin::replay_subtitle`'s slotless early
                    // return clears the in-flight bit WITHOUT releasing the
                    // owed hold, and `Job::RetrySub`'s "reached decodebin3
                    // after all" arm returns without bumping the epoch, so the
                    // input can sit owed with nothing out. Skipping the join
                    // replay there leaves the probes installed on an input
                    // whose selection reads as confirmed.
                    match external.hold {
                        Hold::None => {}
                        Hold::OwedToReplay => {}
                        // Still gated on the selection, and the selection that
                        // lifts the gate carries the replay itself.
                        Hold::UntilSelected => return None,
                    }
                    input
                        .stream_ids()
                        .contains(sid)
                        .then_some((external.id, external.epoch))
                })
                .collect(),
            None => Vec::new(),
        };
        // PAST THE LOCK. `report_unsupported_subtitle` emits to the caller's
        // event handler, which is foreign code; nothing in this crate calls
        // into one while holding `routing`.
        drop(routing);
        for (id, epoch) in join_replays {
            inner.claim_replay(id, epoch, "the join");
        }
        // The consumer is foreign code too, and the FIRST of these calls out:
        // a join's replayed opening cues, taken (and their clear-suppression
        // armed) under the lock at the link, handed over now so a consumer
        // that stalls wedges this poller and nothing else. Bound-for-bound
        // the cues the branch would have delivered (`Inner::item_from_sample`
        // already ran).
        for item in replay_cues {
            if let (Some(id), SubtitleFeedItem::Cue { .. }) = (joined_external, &item) {
                // TEST FAULT INJECTION (see `TestStaging::cue_loss`).
                if inner.stage_consume_cue_loss() {
                    continue;
                }
                // Delivery evidence (see `Inner::external_cues_fed`).
                *inner.external_cues_fed.lock().entry(id).or_insert(0) += 1;
            }
            inner.feed_subtitle(item);
        }
        for (sid, caps) in unsupported {
            inner.report_unsupported_subtitle(&sid, &caps);
        }
        for (key, sid, caps) in unwirable {
            // Remembered BEFORE the report, so the two cannot disagree about
            // whether this stream has been given up on -- and remembered even
            // when there is no sid to report, because the retry has to stop
            // either way. The RETRY memo only: the event below is deduped in
            // exactly one place, `report_unsupported_subtitle`, and this must
            // not become a second gate on it.
            inner.note_degradation(TextDegradation::Unwirable, &key, Duration::ZERO);
            if let Some(sid) = sid {
                inner.report_unsupported_subtitle(&sid, &caps);
            }
        }
        // ONE LINE PER (stream, load), and it carries the whole discriminator
        // so a field capture answers upstream-vs-selection without a rebuild.
        for stall in stalled {
            CAPSLESS_TEXT_STALLS.fetch_add(1, Ordering::Relaxed);
            warn!(
                sid = %stall.sid,
                pad = %stall.pad,
                current_caps = "None",
                sticky_stream_start = stall.sticky_stream_start,
                sticky_segment = stall.sticky_segment,
                linked = stall.linked,
                parked_on = ?stall.parked_on,
                caps_path = %stall.caps_path,
                grace = ?CAPSLESS_TEXT_GRACE,
                "the selected text stream's decodebin3 pad has carried no CAPS for the whole \
                 grace period, so its branch cannot be built and the track will never render; \
                 sticky STREAM_START/SEGMENT with no CAPS means the break is UPSTREAM of this \
                 gate (nothing ever traversed the stream's decodebin3 slot), not in selection"
            );
        }
        // The join-window read. Loud, because if this fires the branch was
        // linked into a live slot before it could accept anything and the track
        // is one push away from being dead, and the repair below is the only
        // thing between that and silence.
        for pad in linked_before_active {
            warn!(
                pad = %pad,
                "a text branch was linked to its decodebin3 output while its own pads were \
                 still INACTIVE, so the first push across the link returns FLUSHING and \
                 latches the multiqueue slot for good"
            );
        }
        // THE SLOT HEAL, LAST, and outside every lock this function takes. A joined
        // text branch whose multiqueue slot has latched delivers nothing for
        // the rest of the item and says nothing about it; the poll that just
        // decided the branch should exist is the natural place to also check
        // that it can still be reached. See
        // [`Inner::heal_latched_text_slots`] for why the repair is placed on
        // the consequence rather than on any one trigger: a captured join
        // latched a slot that nothing touched again for six seconds.
        //
        // Deliberately after the diagnostics above: a branch this repairs was
        // about to become a silent dead track, and the loudest thing in the log
        // should be the repair.
        // Together with the same shape one layer over: the slot is reachable
        // but its sticky CAPS was destroyed in flight by a flush, so the gate
        // above can never admit the stream. That one runs AFTER the gate on
        // purpose, since a rescue is only ever needed for a stream the gate has
        // just refused, and putting it here means the very next poll is the one
        // that joins. Both walk the routing table's text entries, so they share
        // one survey and one lock acquisition; see `TextPadSurvey` in flush.rs.
        Self::heal_and_rescue_text_slots(inner);
        // A SEAT IS A REASON TO ASK AGAIN, and nothing used to say so.
        //
        // Every reclaim in this function lives in this function, and this
        // function only runs when something queues [`Job::PollTextPolicy`]. A
        // JOIN is the one edge that could not queue one: the routing thread
        // asks when a pad appears, the chain joiner asks when video arrives, the
        // drain asks when it has disposed of something. But the link loop
        // seating a branch asked nobody, on the reasoning that a seat is the
        // state everything else was working towards.
        //
        // It is not, when the seat is wrong. A capture has the crate join a
        // dead `text_0` with a live `text_4` routed beside it, and then NO POLL
        // AT ALL for the following 6.5 s, because that receiver polls on events
        // and no event followed. The reclaim that would have moved the seat
        // within one tick in a test never ran at all. This is the difference,
        // and it is one coalesced job.
        //
        // Cannot spin: `joined` is "linked by THIS call", the entry it linked
        // now has a `downstream`, and the loop only considers entries without
        // one. The follow-up poll re-runs the reclaims and links nothing, so it
        // asks for no further poll.
        if seated {
            TEXT_SEAT_FOLLOWUP_POLLS.fetch_add(1, Ordering::Relaxed);
            Inner::request_text_policy_poll(inner);
        }
    }
}

/// Text-branch joins whose own pads were still INACTIVE when the upstream
/// link went in, i.e. the join-window latch's precondition.
///
/// ZERO is the intent (the join links downstream-first and syncs both elements
/// before the upstream link) and a nonzero count says that intent did not
/// hold, because `sync_state_with_parent` returning is not the same as the
/// branch's pads being active. It is the discriminator a capture needs to
/// tell "the join raced the activation" apart from any other way a slot can
/// latch, and it is read by `dash_testbed`'s slot-latch test.
pub(crate) static JOINS_INTO_AN_INACTIVE_BRANCH: AtomicU64 = AtomicU64::new(0);

/// How long the caps gate may refuse a routed text stream for want of a sticky
/// CAPS before it says so (see [`TextDegradation::CapslessStall`]).
///
/// Generous on purpose. The gate's transient really is sub-millisecond, but a
/// loaded machine can stretch a pad's first sticky by a lot, and this line is
/// meant to be believed when it fires: nothing that could still resolve on its
/// own should reach it.
const CAPSLESS_TEXT_GRACE: Duration = Duration::from_secs(5);

/// Routed text streams the caps gate gave up on for want of a sticky CAPS (see
/// [`CAPSLESS_TEXT_GRACE`]). One per (stream id, load generation).
pub(crate) static CAPSLESS_TEXT_STALLS: AtomicU64 = AtomicU64::new(0);

/// Times the seat stalemate break had to clear the contention latches because
/// every routed pad for the selected text stream was locked out with no branch
/// holding the seat (see its site in [`Inner::poll_text_policy`]).
///
/// Zero on a healthy item. Positive means the crate walked into a state no
/// reclaim could heal - every reclaim moves an EXISTING seat - and had to undo
/// its own bookkeeping to get out.
pub(crate) static TEXT_SEAT_STALEMATES: AtomicU64 = AtomicU64::new(0);

/// Times the seat was taken off a text branch whose decodebin3 slot had ENDED
/// while another pad for the same stream was still live (see
/// [`RoutedStream::saw_eos`] and its rule in [`Inner::poll_text_policy`]).
///
/// A REPAIR count. Every one is a re-selected subtitle track that would
/// otherwise have rendered nothing for the rest of the item with the crate
/// reporting the selection as applied.
pub(crate) static TEXT_EOS_SEAT_RECLAIMS: AtomicU64 = AtomicU64::new(0);

/// Follow-up polls the link loop asked for after seating a branch (see the tail
/// of [`Inner::poll_text_policy`]). One per join, and the number a caller that
/// polls on EVENTS ONLY depends on: in the field capture it is the only poll
/// that would have run in the 6.5 s after the bad seat.
pub(crate) static TEXT_SEAT_FOLLOWUP_POLLS: AtomicU64 = AtomicU64::new(0);

impl FcastPlaybin {
    /// Re-drive the text link policy (link routed text into a consumer tail
    /// when a video stream is present). The crate re-checks on its own
    /// events, so this is a belt-and-suspenders hook for a caller's
    /// state-change handler and a no-op when nothing is pending.
    ///
    /// Fire-and-forget, as documented, and now literally so: the call hands
    /// the question to the worker and returns (see
    /// `Inner::request_text_policy_poll`), which is what takes the
    /// pipeline surgery off the caller's event loop. Repeated polls against
    /// one unanswered question cost an atomic swap each.
    ///
    /// The ordering a caller can still rely on is the one it establishes
    /// itself: this and [`Self::pump_selection`]'s dispatch land on the SAME
    /// worker queue, so a poll issued before a pump is decided before it.
    pub fn poll_text_policy(&self) {
        Inner::request_text_policy_poll(&self.inner);
    }
}
