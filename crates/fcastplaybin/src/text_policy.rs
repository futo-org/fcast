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
    FcastPlaybin, Inner,
    api::SubtitleFeedItem,
    decisions,
    jobs::Job,
    routing::{RoutedStream, StreamKind},
};

/// Effects the subtitle-delivery reconcile pass has emitted
/// ([`Inner::reconcile_subtitle_delivery`]).
///
/// The number that makes "converged is a fixpoint" checkable: at a settled,
/// aligned PLAYING this must not move no matter how many passes run.
pub(crate) static RECONCILE_EMITS: AtomicU64 = AtomicU64::new(0);

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
    /// Written as a chain of `||` this reads as four sequential probes and is
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
        if !self.deferred_input_removal.lock().is_empty() {
            return true;
        }
        // Lever-only now: on the default arm both lists stay
        // empty, and the reconcile pass is driven by the tick's periodic poll
        // rather than by "is something remembered".
        if !Self::text_reconcile_levered() {
            return false;
        }
        if !self.deferred_replays.lock().is_empty() {
            return true;
        }
        !self.deferred_verifications.lock().is_empty()
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
        if !deferred && Inner::text_reconcile_levered() {
            return;
        }
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if current != gst::State::Playing || pending != gst::State::VoidPending {
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
        // Lever: `FCAST_NO_DRAIN_TEXT_POLICY_POKE`.
        if disposed && std::env::var_os("FCAST_NO_DRAIN_TEXT_POLICY_POKE").is_none() {
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
        // Lever: `FCAST_NO_DRAIN_TEXT_JOIN_POKE`.
        if !disposed && std::env::var_os("FCAST_NO_DRAIN_TEXT_JOIN_POKE").is_none() {
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
        // Replays that could not be delivered while paused.
        let owed = std::mem::take(&mut *inner.deferred_replays.lock());
        for (id, epoch, attempt) in owed {
            debug!(
                ?id,
                epoch, attempt, "replaying an input postponed while paused"
            );
            inner.queue_job(Job::ReplaySub { id, epoch, attempt });
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
        // THE RECONCILE PASS RUNS FIRST, and is gated by its OWN lever only.
        //
        // It used to sit below the `FCAST_NO_REPLAY_VERDICT_DEFERRAL` return,
        // which made that lever do two unrelated things: restore v1's
        // conclude-anywhere verdict AND switch the reconciler off. Worse, the
        // combination lost work outright - `replay_outcome`'s refusal path
        // writes `deferred_replays` only when the RECONCILE lever is set, so
        // with only the verdict lever set a refused replay was neither
        // remembered nor re-derived. One lever, one behaviour.
        if !Inner::text_reconcile_levered() {
            Inner::reconcile_subtitle_delivery(inner, queued_epoch);
        }
        // The rest of the drain is the verdict-deferral change, gated whole
        // by the same lever as its `verify_replay` half.
        if std::env::var_os("FCAST_NO_REPLAY_VERDICT_DEFERRAL").is_some() {
            return;
        }
        if Inner::text_reconcile_levered() {
            // v1: drain the two remembered lists. Verdicts held while the
            // pipeline could not produce evidence are re-armed rather than
            // decided inline, so each check fires one verification interval
            // AFTER the work above ran, against a branch whose delivery is
            // real and not a leftover sticky.
            let held = std::mem::take(&mut *inner.deferred_verifications.lock());
            for (id, epoch, attempt) in held {
                debug!(
                    ?id,
                    epoch, attempt, "re-arming a replay verification whose verdict was held"
                );
                inner.arm_replay_verification(id, epoch, attempt);
            }
            let selected = inner.selection.lock().subtitle_sid();
            if let Some(sid) = selected {
                let target = {
                    let routing = inner.routing.lock();
                    routing.inputs.iter().find_map(|input| {
                        let external = input.external.as_ref()?;
                        input
                            .stream_ids()
                            .contains(&sid)
                            .then_some((external.id, external.epoch))
                    })
                };
                if let Some((id, epoch)) = target {
                    inner.arm_replay_verification(id, epoch, 0);
                }
            }
            return;
        }
        Inner::reconcile_subtitle_delivery(inner, queued_epoch);
    }

    /// THE reconcile pass for subtitle delivery: desired versus observed, at a
    /// settled PLAYING, with no memory of what was owed.
    ///
    /// This replaces two compensation lists. `deferred_replays` remembered
    /// "the pipeline refused a seek, do it later" and `deferred_verifications`
    /// remembered "I could not decide, ask later"; both are RECONSTRUCTIONS of
    /// a desired state that the graph can simply be asked about. The caller
    /// already runs at exactly the moments delivery becomes provable, so the
    /// pass re-derives instead of remembering:
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
    /// nothing. Guard 2 is "no effect for this resource is in flight", which
    /// is `replay_inflight` (a replay emitted and not yet settled) and
    /// `replay_checks_armed` (a bounded re-check already outstanding).
    /// Together they are why a 1 Hz unconditional poll cannot oscillate.
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
    /// Lever: `FCAST_NO_TEXT_RECONCILE`.
    pub(crate) fn reconcile_subtitle_delivery(inner: &Arc<Inner>, queued_epoch: u64) {
        inner.decider_only("the subtitle delivery reconcile pass");
        // DESIRED, read fresh at run time. A carried copy is how F1 happened.
        let Some(sid) = inner.selection.lock().subtitle_sid() else {
            return;
        };
        let target = {
            let routing = inner.routing.lock();
            routing.inputs.iter().find_map(|input| {
                let external = input.external.as_ref()?;
                input
                    .stream_ids()
                    .contains(&sid)
                    .then_some((external.id, external.epoch))
            })
        };
        // Not an external: an embedded track needs no replay, and there is no
        // resource to reconcile.
        let Some((id, epoch)) = target else {
            return;
        };
        // GUARD 2 first, because it is the cheap one and because an emit while
        // an effect is outstanding is the failure mode that actually hurts.
        if inner.replay_inflight.lock().contains(&(id, epoch)) {
            return;
        }
        if inner.replay_checks_armed.lock().contains(&(id, epoch)) {
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
            let delivered = routing.routed.iter().any(|routed| {
                routed.kind == StreamKind::Text
                    && routed.downstream.is_some()
                    && routed
                        .db3_src_pad
                        .sticky_event::<gst::event::StreamStart>(0)
                        .is_some_and(|event| sids.iter().any(|sid| *sid == event.stream_id()))
            });
            (delivered, sids)
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
        // Lever: `FCAST_NO_RECONCILE_SUPERSESSION_GATE`.
        let current = inner.queue_epoch.load(Ordering::SeqCst);
        if current != queued_epoch
            && std::env::var_os("FCAST_NO_RECONCILE_SUPERSESSION_GATE").is_none()
        {
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
        RECONCILE_EMITS.fetch_add(1, Ordering::SeqCst);
        inner.replay_inflight.lock().insert((id, epoch));
        if !inner.queue_job(Job::ReplaySub {
            id,
            epoch,
            attempt: 0,
        }) {
            // A refused send discharges nothing, so the bit must not linger:
            // it would suppress every future pass for this resource.
            inner.replay_inflight.lock().remove(&(id, epoch));
            return;
        }
        inner.arm_replay_verification(id, epoch, 0);
    }

    /// Whether the pipeline has come to rest at PAUSED, where a flush of the
    /// text branch cannot complete. See [`Inner::detach_text_parts`].
    pub(crate) fn resting_paused(pipeline: &gst::Pipeline) -> bool {
        let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
        current == gst::State::Paused && pending == gst::State::VoidPending
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
            let parked = inner.park_text_stream(&db3_src_pad);
            let orphaned = {
                let mut routing = inner.routing.lock();
                match routing
                    .routed
                    .iter_mut()
                    .find(|r| r.db3_src_pad == db3_src_pad)
                {
                    Some(routed) => {
                        match parked {
                            Ok((sink, park)) => {
                                debug!(pad = %db3_src_pad.name(), "parked text stream");
                                routed.park_sink = Some(sink);
                                routed.park_pad = Some(park);
                            }
                            Err(err) => warn!(?err, "failed to park the text stream"),
                        }
                        None
                    }
                    // The stream unrouted while the lock was down, so its
                    // parking sink has nothing left to belong to.
                    None => parked.ok(),
                }
            };
            if let Some((sink, park)) = orphaned {
                let _ = db3_src_pad.unlink(&park);
                let _ = sink.set_state(gst::State::Null);
                let _ = inner.pipeline.remove(&sink);
            }
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
    /// Coalesced, and the coalescing is the reason this is cheap enough to be
    /// called from a receiver's 5ms poll loop: the swap admits one queued
    /// poll at a time and folds the rest, and folding loses nothing because
    /// the job re-reads the world it decides from. The bit is cleared by the
    /// JOB before it runs, so the fold can only ever swallow a poke that the
    /// pending run has not yet answered.
    ///
    /// Levers: `FCAST_INLINE_TEXT_POLL` restores the direct call on the
    /// asking thread for every caller; `FCAST_INLINE_ROUTE_TEXT_POLL` does it
    /// for the routing thread alone (`Inner::route_db3_pad`'s tail, the
    /// instant-text path), which is the one site whose hop the switch-latency
    /// probe gates and the one worth rolling back on its own.
    pub(crate) fn request_text_policy_poll(inner: &Arc<Inner>) {
        if std::env::var_os("FCAST_INLINE_TEXT_POLL").is_some() {
            Inner::poll_text_policy(inner);
            return;
        }
        if inner.poll_queued.swap(true, Ordering::SeqCst) {
            inner.poll_policy_coalesced.fetch_add(1, Ordering::SeqCst);
            return;
        }
        if !inner.queue_job(Job::PollTextPolicy) {
            // No decider to clear it, so clear it here: the crate is dying
            // and this changes nothing, but a bit left set is the shape that
            // silences every later poll and it must not be reachable at all.
            inner.poll_queued.store(false, Ordering::SeqCst);
        }
    }

    /// A waiting same-sid text pad the seat could actually move to. Scopes
    /// the EOS seat reclaim and the link loop's EOS refusal (one rule, one
    /// lever), so it must not count a pad that loop refuses itself: an
    /// evicted, still-segmentless husk as the "live rival" left the seat
    /// EMPTY forever (mid-play detach + same-URL re-attach, where the husk's
    /// input is gone and the newcomer's slot EOSed before its first poll).
    fn seat_ready_rival(routed: &RoutedStream, allowed: &str) -> bool {
        routed.kind == StreamKind::Text
            && routed.downstream.is_none()
            && !routed.superseded
            && !routed.saw_eos.load(Ordering::SeqCst)
            && !(routed.evicted_dead
                && routed
                    .db3_src_pad
                    .sticky_event::<gst::event::Segment>(0)
                    .is_none())
            && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
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
        // link decision does not need, and the levers can put this function
        // back on a foreign thread.
        //
        // The poke is skipped while the last drain's no-op verdict stands
        // (see `Inner::drain_poke_parked`). Without that, a caller polling
        // every 5ms at a pipeline parked below PLAYING re-queued this job on
        // every poll, indefinitely, and the worker spent the whole park
        // spinning on drains that could only early-return (measured at 5099
        // jobs over one 39-second Buffering park on the fuzz driver's seed
        // 400009). The pipeline state edges still queue the drain
        // unconditionally on the bus, so a settled PLAYING still drains
        // promptly with no poll at all. The lever restores the
        // poke-on-every-poll behavior for interleaved A/B measurement and
        // gates this whole change, since the flag it ignores is written
        // nowhere else that behavior can reach.
        if inner.has_deferred_text_work()
            && (!inner.drain_poke_parked.load(Ordering::SeqCst)
                || std::env::var_os("FCAST_NO_DRAIN_POKE_SUPPRESS").is_some())
        {
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
        // the levers are allowed to ask that question from anywhere.
        inner.decider_only("the text-policy surgery");
        // TEST FAULT INJECTION (see [`Inner::staged_joins`]): bring up any
        // branch whose staged join window has expired. Here rather than in the
        // join, and off the sleep it used to be, so the rest of the graph keeps
        // running through the window, which is the only condition under which
        // anything can cross the staged link and reproduce the latch.
        if inner.stage_join_hold_ms.load(Ordering::SeqCst) > 0 {
            let due: Vec<(gst::Element, gst::Element)> = {
                let mut staged = inner.staged_joins.lock();
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
                warn!(
                    tqueue = %tqueue.name(),
                    "TEST STAGING: releasing a held text branch into its live link"
                );
                let _ = appsink.sync_state_with_parent();
                let _ = tqueue.sync_state_with_parent();
            }
        }
        // TEST FAULT INJECTION (see [`Inner::stage_text_caps_loss`]): take a
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
        // would have queued. Lever: `FCAST_NO_UPSTREAM_SELECTION_SPLIT`.
        if inner.upstream_owns_selection() {
            let applied = inner.selection.lock().subtitle_sid();
            let held = applied.and_then(|sid| {
                let routing = inner.routing.lock();
                routing.inputs.iter().find_map(|input| {
                    input.external.as_ref().and_then(|external| {
                        (external.hold_until_selected && input.stream_ids().contains(&sid))
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
                // The per-resource in-flight bit, set before the emit like
                // every other one (see [`Inner::replay_inflight`]).
                inner.replay_inflight.lock().insert((id, epoch));
                let sent = inner.queue_job(Job::ReplaySub {
                    id,
                    epoch,
                    attempt: 0,
                });
                if !sent {
                    inner.replay_inflight.lock().remove(&(id, epoch));
                }
                let owed = sent.then_some((id, epoch));
                inner.unblock_selected_externals(std::slice::from_ref(&sid), owed);
            }
        }
        // Only the selected subtitle stream may relink. A disabled stream
        // stays routed until decodebin3 removes its pad, and relinking it
        // here would resurrect the cue the eager detach just cleared.
        // Snapshot before taking the routing lock.
        //
        // A stream the applied selection names while the DESIRED state is an
        // explicit subtitle-off is decodebin3's collection-default
        // auto-select stomping that off (an attach makes it re-select over
        // the applied state), the TEXT twin of the video resurrect
        // `route_db3_pad` guards against. Splicing it into a fresh branch
        // adds a reconfiguration to whatever transition is in flight, that
        // transition never completes, and the receiver's pump gate
        // (`quiet = running && !has_async_transition()`) then holds the
        // engine's corrective re-assert back forever, so the only thing that
        // would undo the stomp is postponed behind the stall the stomp
        // caused. Leaving the stream parked keeps the pipeline settled, and
        // the re-assert dispatches on the next pump and makes decodebin3 drop
        // the pad. `fuzz_buffering` seed 400009, whose schedule ends
        // `disable_subtitles` then `attach_external`. The lever restores the
        // unconditional relink and gates this whole change.
        let allowed_sid = {
            let selection = inner.selection.lock();
            if selection.subtitle_explicitly_off()
                && std::env::var_os("FCAST_LINK_STOMPED_SUBTITLE").is_none()
            {
                None
            } else {
                selection.subtitle_sid()
            }
        };
        // THE STALEMATE BREAK, and it runs before every reclaim below
        // because all of them can only MOVE a seat that exists.
        //
        // `superseded` and `evicted_dead` are permanent latches that hold a
        // pad out of contention, and both are only ever justified BY A
        // COMPETITOR: they exist so two same-sid entries cannot trade the one
        // consumer branch back and forth once per poll. With NOTHING holding
        // the seat there is no competitor and nothing to protect, and the
        // latches stop being a tie-break and become a lock-out.
        //
        // How both get set with no survivor, measured on
        // `external_subtitle_lifecycle::reattaching_the_same_url_while_paused_
        // renders_after_resume` (137 failures in 160 runs at 16-way load,
        // 46.4 ms to 46.9 ms of one capture):
        //
        //   text_0 joins and holds the seat
        //   the same URL is detached, then re-attached while PAUSED
        //   decodebin3 exposes text_1 for the SAME sid
        //   the superseded reclaim evicts text_0   -> text_0.superseded
        //   text_1 joins and holds the seat
        //   decodebin3 RECYCLES the pad name text_0 for the re-attached input
        //     (the walk-back), so a FRESH routed entry for text_0 is
        //     appended with clean flags, after text_1
        //   the superseded reclaim now reads that fresh entry as the newest
        //     and evicts text_1              -> text_1.superseded
        //   the late detach disposes text_0's branch, text_0 rejoins
        //     segmentless, and the segmentless-holder reclaim takes it out
        //                                    -> text_0.evicted_dead
        //
        // End state: every same-sid entry latched out, `downstream: None` on
        // all of them, and the link loop refusing both forever - `text_0:
        // seat-evicted and still segmentless`, `text_1: decodebin3 replaced
        // this pad for the same stream`, 4024 times over 40 s, with the
        // selection CONFIRMED and the caller shown a track that never
        // appears. No reclaim can heal it: each one needs a holder to move.
        //
        // So: no holder, and nothing admissible, means the latches have
        // outlived their argument. Clear them and let the link loop re-run
        // the contest; whichever pad is actually carrying data wins the seat
        // back through the flow reclaim on the next poll, which is the
        // self-stabilising predicate that already exists.
        //
        // Lever: `FCAST_NO_TEXT_SEAT_STALEMATE_BREAK`.
        if let Some(allowed) = allowed_sid.as_deref()
            && std::env::var_os("FCAST_NO_TEXT_SEAT_STALEMATE_BREAK").is_none()
        {
            let mut routing = inner.routing.lock();
            let same_sid = |routed: &RoutedStream| {
                routed.kind == StreamKind::Text
                    && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
            };
            let seated = routing
                .routed
                .iter()
                .any(|r| same_sid(r) && r.downstream.is_some());
            let candidates = routing.routed.iter().filter(|r| same_sid(r)).count();
            let locked_out = routing
                .routed
                .iter()
                .filter(|r| same_sid(r))
                .all(|r| r.superseded || r.evicted_dead);
            if !seated && candidates > 0 && locked_out {
                TEXT_SEAT_STALEMATES.fetch_add(1, Ordering::SeqCst);
                let pads: Vec<String> = routing
                    .routed
                    .iter_mut()
                    .filter(|r| {
                        r.kind == StreamKind::Text
                            && r.db3_src_pad.stream_id().as_deref() == Some(allowed)
                    })
                    .map(|r| {
                        let name = format!(
                            "{}(superseded={} evicted_dead={} segment={})",
                            r.db3_src_pad.name(),
                            r.superseded,
                            r.evicted_dead,
                            r.db3_src_pad
                                .sticky_event::<gst::event::Segment>(0)
                                .is_some(),
                        );
                        r.superseded = false;
                        r.evicted_dead = false;
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
        // The overlay's one subtitle seat may still be held by a DEAD
        // branch. A detached input's decodebin3 output pad can linger
        // linked to a live branch past `remove_input` (the id stays in the
        // collection while a same-id stream re-materializes, so no
        // pad-removed fires), its sticky segment wiped by the removal's
        // flush with nothing upstream left to ever send another one. Left
        // alone it holds the ONE live text slot forever and the
        // `consumer_branch_live` check in the link loop below refuses every
        // later text stream with no error surfaced anywhere. A branch that
        // will render again always gets a segment re-sent by its own
        // reconfigure, so a holder WITHOUT one, while a parked stream of the
        // selected sid is waiting, is beyond recovery and gets evicted. The
        // pads come out of the entry under the lock and the detach runs with
        // the lock released, like every other text detach.
        //
        // KEPT when subtitleoverlay went. The rule was written against the
        // overlay's geometry, where "holds the seat" meant "occupies
        // `subtitle_sink`". The consumer transport
        // states the same scarcity itself (`consumer_branch_live`), so a
        // dead branch blocks its successor exactly as before and
        // `external_subtitle_lifecycle::reattaching_the_same_url_after_a_
        // detach_renders_again` still pins it.
        // THE SECOND SHAPE OF A DEAD HOLDER, and the one
        // `dash-reenable-freeze.txt` hit: decodebin3 EXPOSED A NEW PAD FOR THE
        // SAME STREAM. `db_output_stream_new` names outputs off per-type
        // counters that are never decremented (gstdecodebin3.c:4761-4784), and
        // `find_free_compatible_output` refuses to re-use an existing output
        // whose slot's stream is still REQUESTED (3169-3183). So a flushing
        // seek that re-slots a subtitle stream which stays selected - a
        // `Job::RefreshSeek` at a re-enable is exactly that - produces a
        // SECOND text pad, `text_1`, beside a `text_0` that will never carry
        // another buffer.
        //
        // The segment test below cannot see it: whether `text_0`'s pad kept
        // its pre-seek sticky depends on whether the flush reached it, so the
        // holder can be stone dead with a segment still on it. ROUTED ORDER
        // can: `routing.routed` is append-only per route, so a waiting entry
        // that comes AFTER the holder and carries the same stream id is
        // decodebin3's replacement for it. Without this the field log's shape
        // is permanent: `consumer_branch_live` refuses `text_1` on every
        // later poll, the diagnostic below is suppressed because
        // `already_joined` is true of the dead holder, and the track renders
        // nothing for the rest of the item.
        //
        // # The two constraints that keep it from eating the graph
        //
        // Measured, not argued: without them `dash_testbed`'s
        // `dash_embedded_text_track_plays` evicted `text_0` and then `text_1`
        // 10 ms later and rendered nothing at all. The eviction FEEDS the
        // predicate - an evicted holder becomes a waiting entry of the allowed
        // sid - so
        //
        // * only a LATER entry may supersede an earlier one (`newest > held`), which is
        //   the direction decodebin3 actually replaces in, and
        // * an entry this reclaim already took out (`superseded`) or evicted
        //   (`evicted_dead`) cannot be the one that justifies the next eviction.
        //
        // Lever: `FCAST_NO_SUPERSEDED_TEXT_PAD_RECLAIM`.
        let reclaim = {
            let mut routing = inner.routing.lock();
            let waiting = allowed_sid.as_deref().is_some_and(|allowed| {
                routing.routed.iter().any(|routed| {
                    routed.kind == StreamKind::Text
                        && routed.downstream.is_none()
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                })
            });
            // The segment-less holder, exactly as before.
            let mut victim = waiting
                .then(|| {
                    routing.routed.iter().position(|routed| {
                        routed.kind == StreamKind::Text
                            && routed.downstream.is_some()
                            && routed
                                .db3_src_pad
                                .sticky_event::<gst::event::Segment>(0)
                                .is_none()
                    })
                })
                .flatten();
            let mut superseded = false;
            // THE SEAT decodebin3 HAS ALREADY ENDED, and the shape none of the
            // three rules below can see.
            //
            // On a re-select in upstream-selection mode decodebin3 builds a
            // FRESH slot for the re-added input and CLEARS + EOSes the old one
            // (`remove_input_stream` -> "Sending EOS to unused slot",
            // gstdecodebin3.c:1232/1313), leaving the OUTPUT pad this entry
            // holds ghosted onto a slot that is over. What the other rules can
            // read about that pad:
            //
            // * its sticky SEGMENT survives the clear, so the segmentless-holder rule above
            //   says it is healthy;
            // * routed ORDER says nothing, because the replacement output can be built
            //   either side of it (decodebin3 recycles outputs in both directions) and in
            //   one capture there was no new entry at all for seconds;
            // * its `last_buffer` ticket is whatever it was, and a rival that has not
            //   carried a buffer YET stamps zero, so the flow reclaim cannot order a dead
            //   pad against a live-but-idle one, which is precisely a sparse subtitle
            //   track's normal condition.
            //
            // [`RoutedStream::saw_eos`] asks decodebin3 instead. A holder whose
            // slot has ended can never deliver again, and the ONLY thing that
            // was keeping it in the seat is that nothing said so.
            //
            // SCOPED TO A LIVE RIVAL, which is what keeps a legitimate
            // end-of-item EOS from being a repair trigger: at the end of an item
            // every same-sid pad is EOS and this finds no `live`, so it does
            // nothing at all and the branch stays up to carry the EOS through.
            // It fires only where there is somewhere better for the seat to go.
            //
            // `superseded` is deliberately NOT set on the victim: that is the
            // verdict for a pad decodebin3 has REPLACED, and this one may yet be
            // re-pointed at a live slot, at which point its STREAM_START
            // clears the flag by itself. The link loop's own EOS refusal is what
            // keeps the evicted pad out of contention meanwhile, and it is
            // scoped identically, so the pair cannot trade the seat back and
            // forth: the flag only clears on proof of life.
            //
            // Lever: `FCAST_NO_TEXT_EOS_SEAT_RECLAIM`.
            if victim.is_none()
                && std::env::var_os("FCAST_NO_TEXT_EOS_SEAT_RECLAIM").is_none()
                && let Some(allowed) = allowed_sid.as_deref()
            {
                let same_sid = |routed: &RoutedStream| {
                    routed.kind == StreamKind::Text
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                };
                let held = routing
                    .routed
                    .iter()
                    .position(|routed| same_sid(routed) && routed.downstream.is_some());
                let live = routing
                    .routed
                    .iter()
                    .any(|routed| Self::seat_ready_rival(routed, allowed));
                if let Some(held) = held
                    && routing.routed[held].saw_eos.load(Ordering::SeqCst)
                    && live
                {
                    info!(
                        held = %routing.routed[held].db3_src_pad.name(),
                        "decodebin3 ended the slot this text branch is ghosting while another \
                         pad for the same stream is still live; moving the seat off the dead one"
                    );
                    TEXT_EOS_SEAT_RECLAIMS.fetch_add(1, Ordering::SeqCst);
                    victim = Some(held);
                }
            }
            if victim.is_none()
                && std::env::var_os("FCAST_NO_SUPERSEDED_TEXT_PAD_RECLAIM").is_none()
                && let Some(allowed) = allowed_sid.as_deref()
            {
                let newest = routing.routed.iter().rposition(|routed| {
                    routed.kind == StreamKind::Text
                        && routed.downstream.is_none()
                        && !routed.superseded
                        && !routed.evicted_dead
                        // ALIVE, or the verdict is wrong on its face: this rule
                        // exists for "decodebin3 replaced the holder with THIS
                        // pad", and a rival whose own slot has ended is not a
                        // replacement for anything, just a second corpse.
                        // Measured: a re-select found both same-sid pads
                        // EOSed, this rule flipped the seat from one dead pad
                        // to the other and LATCHED the first superseded,
                        // 0.5 s before decodebin3 re-pointed that
                        // very pad (the reuse shape), which then had to be
                        // re-admitted through the flow reclaim. With no live
                        // rival there is nothing to move the seat FOR.
                        && !routed.saw_eos.load(Ordering::SeqCst)
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                });
                let held = routing.routed.iter().position(|routed| {
                    routed.kind == StreamKind::Text
                        && routed.downstream.is_some()
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                });
                if let (Some(newest), Some(held)) = (newest, held)
                    && held < newest
                {
                    victim = Some(held);
                    superseded = true;
                }
            }
            // THE THIRD SHAPE, the walk-back: decodebin3 walked BACK onto
            // a pad this reclaim had already condemned.
            //
            // Both rules above are ordered on `routing.routed`, which is
            // append-only, so both can only ever move the seat FORWARD. That
            // was believed to be the only direction decodebin3 replaces in.
            // Measured against `manifest-text-seg.mpd`, it is not: at the
            // SECOND off/on the old text input has drained and been released
            // before the re-enable's new input is added, so slot 2 is free,
            // `gst_decodebin_get_slot_for_input_stream_locked` takes it as the
            // lowest-indexed unused compatible slot ("Re-using existing unused
            // slot 2", gstdecodebin3.c:3886) and `db_output_stream_reconfigure`
            // re-points the ORIGINAL pad `text_0` at it. At the first off/on
            // the old input was still on slot 2, so decodebin3 built slot 3 and
            // a new pad `text_1` instead. Forward then back, and the crate,
            // holding `text_1` with `text_0` marked permanently superseded,
            // refused the only pad still carrying cues for the rest of the
            // item: 40 s of `queue_2` pushing one buffer per second into the
            // parking fakesink while `fpb-tqueue-text_1` saw nothing and every
            // adaptivedemux2 push returned `ok`.
            //
            // So the seat follows the DATA, which is the thing routed order was
            // only ever a proxy for. A waiting same-sid pad that has carried a
            // buffer MORE RECENTLY than the holder is the pad decodebin3 is
            // feeding now, whichever side of the holder it sits on, and no
            // sticky can argue otherwise (a superseded pad's segment is
            // whatever it held before the flush; a buffer is not).
            //
            // Why this cannot thrash, which is what the ordering constraint was
            // there to prevent: the predicate is self-stabilising. It seats the
            // pad that is carrying data and leaves the loser carrying none, and
            // a pad carrying none can never satisfy `>` against one that is. An
            // eviction therefore cannot feed the next one, which is exactly the
            // failure (`text_0` evicted, then `text_1` 10 ms later, nothing
            // rendered) the `superseded`/`evicted_dead` guards were added for.
            //
            // Lever: `FCAST_NO_TEXT_PAD_FLOW_RECLAIM`.
            if victim.is_none()
                && std::env::var_os("FCAST_NO_TEXT_PAD_FLOW_RECLAIM").is_none()
                && let Some(allowed) = allowed_sid.as_deref()
            {
                let same_sid = |routed: &RoutedStream| {
                    routed.kind == StreamKind::Text
                        && routed.db3_src_pad.stream_id().as_deref() == Some(allowed)
                };
                let held = routing
                    .routed
                    .iter()
                    .position(|routed| same_sid(routed) && routed.downstream.is_some());
                if let Some(held) = held {
                    let held_flow = routing.routed[held].last_buffer.load(Ordering::Relaxed);
                    // The freshest waiting pad, so a third pad cannot leave the
                    // seat on a stale one.
                    let fresher = routing
                        .routed
                        .iter()
                        .enumerate()
                        .filter(|(_, routed)| same_sid(routed) && routed.downstream.is_none())
                        // SEAT-READY, not merely alive. The freshness test
                        // above can be satisfied by a ticket stamped BEFORE
                        // the re-enable's flushing seek, so on its own it will
                        // seat the winner inside the window where that flush
                        // has stripped the pad's stickies and the new segment
                        // has not arrived yet. The branch is built and linked
                        // there, and the first buffer out of the slot then
                        // crosses it with no segment to compute a running time
                        // from: measured as one `fpb-tqueue-text_0` "Got data
                        // flow before segment event" per re-seat, and a cue
                        // timed off a missing segment is exactly the silent
                        // corruption this whole path exists to prevent.
                        //
                        // Waiting for the segment costs one poll (10 ms) and
                        // is the same proof of life the `evicted_dead` rule
                        // already clears on.
                        .filter(|(_, routed)| {
                            routed
                                .db3_src_pad
                                .sticky_event::<gst::event::Segment>(0)
                                .is_some()
                        })
                        .map(|(index, routed)| (index, routed.last_buffer.load(Ordering::Relaxed)))
                        .filter(|(_, flow)| *flow > held_flow)
                        .max_by_key(|(_, flow)| *flow);
                    if let Some((winner, flow)) = fresher {
                        info!(
                            held = %routing.routed[held].db3_src_pad.name(),
                            held_flow,
                            winner = %routing.routed[winner].db3_src_pad.name(),
                            winner_flow = flow,
                            "decodebin3 moved this text stream back to another pad, following the data"
                        );
                        // The winner is the pad decodebin3 feeds, so whatever
                        // verdict an earlier reclaim recorded against it is
                        // out of date and must not survive into the link loop
                        // (which refuses `superseded` outright, and
                        // `evicted_dead` while the pad is segmentless).
                        let winner = &mut routing.routed[winner];
                        winner.superseded = false;
                        winner.evicted_dead = false;
                        victim = Some(held);
                        superseded = true;
                    }
                }
            }
            victim.map(|index| {
                let routed = &mut routing.routed[index];
                // THE PAIR INVARIANT, asserted where it is relied on.
                // The search above filters on `downstream` alone, while
                // the slot rule this eviction exists to unblock is the
                // PAIR (`consumer_branch_live`: `appsink.is_some() &&
                // downstream.is_some()`). The two are written and
                // cleared together (the link loop sets both on a
                // successful join, every detach takes both) so a text
                // entry cannot hold one without the other. If it ever
                // could, this eviction would be aimed at a branch that
                // was not occupying the slot, and the stream it is
                // clearing the way for would stay refused with nothing
                // in the log to say why.
                debug_assert!(
                    routed.appsink.is_some(),
                    "a routed text entry has a downstream but no appsink, so the \
                     eviction and `consumer_branch_live` disagree about which \
                     branch holds the one live text slot"
                );
                routed.evicted_dead = true;
                // WHICH of the two verdicts this is. A holder that merely
                // lost its segment may yet revive, and `evicted_dead` alone
                // is the right, clearable verdict for it; a holder decodebin3
                // has REPLACED never will (see [`RoutedStream::superseded`]).
                routed.superseded = superseded;
                (
                    routed.db3_src_pad.clone(),
                    routed.downstream.take().expect("filtered on Some above"),
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
            let parked = inner.park_text_stream(&db3_src_pad);
            let orphaned = {
                let mut routing = inner.routing.lock();
                match routing
                    .routed
                    .iter_mut()
                    .find(|routed| routed.db3_src_pad == db3_src_pad)
                {
                    Some(routed) => {
                        match parked {
                            Ok((sink, park)) => {
                                routed.park_sink = Some(sink);
                                routed.park_pad = Some(park);
                            }
                            Err(err) => warn!(?err, "failed to park the evicted text branch"),
                        }
                        None
                    }
                    // The stream unrouted while the lock was down, so its
                    // parking sink has nothing left to belong to.
                    None => parked.ok(),
                }
            };
            if let Some((sink, park)) = orphaned {
                let _ = db3_src_pad.unlink(&park);
                let _ = sink.set_state(gst::State::Null);
                let _ = inner.pipeline.remove(&sink);
            }
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
        // Why each candidate was refused, reported below when nothing joined.
        let mut refusals: Vec<String> = Vec::new();
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
        // Whether a consumer branch is already feeding (see the arm split
        // below). Snapshotted before the loop starts mutating entries, and
        // updated when this call links one.
        let mut consumer_branch_live = routing
            .routed
            .iter()
            .any(|r| r.kind == StreamKind::Text && r.appsink.is_some() && r.downstream.is_some());
        // THE OTHER HALF of the EOS reclaim above, snapshotted before the loop
        // starts mutating entries: whether a SEAT-READY candidate for the
        // allowed sid exists (see `seat_ready_rival`). Read once rather than
        // per candidate, and blind to pads this loop refuses itself, so the
        // loop cannot refuse every pad on the strength of a rival it is about
        // to refuse as well.
        let live_rival_waiting = allowed_sid.as_deref().is_some_and(|allowed| {
            routing
                .routed
                .iter()
                .any(|r| Self::seat_ready_rival(r, allowed))
        });
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
        for routed in routing
            .routed
            .iter_mut()
            .filter(|r| r.kind == StreamKind::Text && r.downstream.is_none())
        {
            // No stream id yet means wait for a later poll.
            let sid = routed.db3_src_pad.stream_id();
            if sid.is_none() || sid.as_deref() != allowed_sid.as_deref() {
                // Silent until a field log needed it: a switched-to external
                // whose branch never joined left no trace of WHY, and every
                // refusal in this loop is a `continue`.
                refusals.push(format!(
                    "{}: sid {:?} is not the allowed {:?}",
                    routed.db3_src_pad.name(),
                    sid.as_deref(),
                    allowed_sid.as_deref()
                ));
                continue;
            }
            // A pad decodebin3 has REPLACED never competes again, and no
            // sticky can argue otherwise (see [`RoutedStream::superseded`]).
            // Without this the reclaim below and this loop trade the consumer
            // back and forth once per poll, which is a busier version of the
            // wedge the reclaim exists to break.
            if routed.superseded {
                refusals.push(format!(
                    "{}: decodebin3 replaced this pad for the same stream",
                    routed.db3_src_pad.name()
                ));
                continue;
            }
            // A pad whose decodebin3 slot has ENDED cannot deliver another
            // buffer, so seating it is the silent wrong seat this whole
            // defect is: the branch joins, reports success, and the
            // refuses every live rival for the life of the item.
            //
            // BEFORE the `evicted_dead` arm on purpose. That arm CLEARS its
            // verdict for any pad carrying a segment, and a pad decodebin3
            // EOSed keeps the segment it had, so letting it run first would
            // hand the seat straight back to the pad the reclaim above just
            // took it from.
            //
            // Scoped exactly as that reclaim is: only while a rival with a
            // LIVE slot is waiting. With no rival this refuses nothing, so a
            // track whose only pad has EOSed keeps its branch (and its
            // end-of-item EOS reaches the consumer) instead of the caller
            // being told the stream is unrenderable.
            // Lever: `FCAST_NO_TEXT_EOS_SEAT_RECLAIM`, with the reclaim it
            // pairs with, since the two are one rule and move together.
            if live_rival_waiting
                && routed.saw_eos.load(Ordering::SeqCst)
                && std::env::var_os("FCAST_NO_TEXT_EOS_SEAT_RECLAIM").is_none()
            {
                refusals.push(format!(
                    "{}: its decodebin3 slot ended",
                    routed.db3_src_pad.name()
                ));
                continue;
            }
            // An entry the seat reclaim evicted stays out of contention
            // while its pad remains segmentless. Relinked, it would only
            // win the seat back from the same-sid stream that can render
            // (routed order is stable, and the evicted pad comes first). A
            // segment on the pad means the branch revived and may compete
            // again.
            if routed.evicted_dead {
                if routed
                    .db3_src_pad
                    .sticky_event::<gst::event::Segment>(0)
                    .is_none()
                {
                    refusals.push(format!(
                        "{}: seat-evicted and still segmentless",
                        routed.db3_src_pad.name()
                    ));
                    continue;
                }
                routed.evicted_dead = false;
            }
            // THE ADMISSION GATE. What a branch must satisfy before it may be
            // built; everything above this point (routing, sids, the
            // evicted-branch rule) is about WHICH stream, and everything from
            // the queue down is construction.
            //
            // ONE live consumer branch, by construction. This used to come
            // free from subtitleoverlay's single `subtitle_sink` being
            // physically occupied. A per-stream appsink has no such natural
            // limit, so the rule is stated. Without it a
            // poll that runs before an outgoing branch's disposal has landed
            // can link a SECOND branch, and both then feed the one consumer:
            // two tracks interleaved on screen, and a `Clear` from either
            // wiping the other.
            if consumer_branch_live {
                refusals.push(format!(
                    "{}: another text branch already feeds the consumer",
                    routed.db3_src_pad.name()
                ));
                continue;
            }
            // A stream whose branch could not be WIRED under this load is
            // not tried again: the link is decided on caps GStreamer has
            // already refused once, and the poll runs every tick. See
            // [`Inner::unwirable_text_streams`]; the caller was told at
            // the first refusal.
            if inner.unwirable_text_streams.lock().contains(&(
                Self::unwirable_key(sid.as_deref(), &routed.db3_src_pad),
                inner.current_generation(),
            )) {
                refusals.push(format!(
                    "{}: its branch could not be wired under this load",
                    routed.db3_src_pad.name()
                ));
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
                .and_then(|c| decisions::consumer_stream_format(c, inner.bitmap_subs))
                .is_none()
            {
                refusals.push(format!(
                    "{}: caps {:?} are not subtitles the renderer can carry",
                    routed.db3_src_pad.name(),
                    caps.as_ref().map(|c| c.to_string())
                ));
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
                {
                    let key = (sid.to_string(), inner.current_generation());
                    let mut watch = inner.capsless_text_since.lock();
                    match watch.get(&key) {
                        // Already escalated: say nothing more for this
                        // (stream, load).
                        Some(None) => {}
                        Some(Some(since)) if since.elapsed() >= CAPSLESS_TEXT_GRACE => {
                            watch.insert(key, None);
                            drop(watch);
                            let pad = &routed.db3_src_pad;
                            stalled.push(CapslessTextStall {
                                caps_path: Inner::caps_path_dump(pad),
                                sid: sid.to_string(),
                                pad: pad.name().to_string(),
                                sticky_stream_start: pad
                                    .sticky_event::<gst::event::StreamStart>(0)
                                    .is_some(),
                                sticky_segment: pad
                                    .sticky_event::<gst::event::Segment>(0)
                                    .is_some(),
                                linked: pad.is_linked(),
                                parked_on: pad
                                    .peer()
                                    .and_then(|peer| peer.parent_element())
                                    .map(|element| element.name().to_string()),
                            });
                        }
                        Some(Some(_)) => {}
                        None => {
                            watch.insert(key, Some(Instant::now()));
                        }
                    }
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
                JOINS_INTO_AN_INACTIVE_BRANCH.fetch_add(1, Ordering::SeqCst);
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
                    // TEST FAULT INJECTION (see `Inner::stage_join_hold_ms`).
                    // The branch was deliberately left at NULL so THIS link
                    // landed on inactive pads; hold it there long enough for
                    // the demuxer to put something across the new link (a
                    // sparse text track's GAP tick is once a second) and only
                    // then bring it up, exactly as a `sync_state_with_parent`
                    // that lost the race would have.
                    let hold = inner.stage_join_hold_ms.load(Ordering::SeqCst);
                    if hold > 0 {
                        warn!(
                            pad = %routed.db3_src_pad.name(),
                            hold_ms = hold,
                            "TEST STAGING: holding a joined text branch at NULL"
                        );
                        inner.staged_joins.lock().push((
                            tqueue.clone(),
                            appsink.clone(),
                            Instant::now() + Duration::from_millis(hold),
                        ));
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
                    consumer_branch_live = true;
                    routed.appsink = Some(appsink);
                    joined = sid.map(|s| s.to_string());
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
        // BEHIND THE IN-FLIGHT BIT, like every other emitter, and this was the
        // one that was not. `subtitle-reenable-freeze.txt` caught the cost:
        // the selection-time emitter (see `Inner::external_stream_slotless`'s
        // caller) set the bit and queued a `ReplaySub`; 1.5 ms later THIS site
        // queued a second one for the same `(id, epoch)` while the first was
        // still being carried; the first outcome (effect 12) cleared both this
        // bit and `ExternalInput::replay_seek_outstanding` BEFORE the second
        // job ran, so the choke point inside `replay_subtitle` let it through
        // and effect 13 flushed the branch a second time. That choke point can
        // only collapse triggers while a seek is TRAVELLING; a queue that
        // already holds one is a strictly earlier question, and this is where
        // it gets asked.
        //
        // The bit is a HashSet keyed on `(id, epoch)`, so "already queued" and
        // "already in flight" are the same read, and both are discharged by
        // the single tail in `FcastPlaybin::replay_outcome`. A refused
        // `queue_job` takes the bit straight back out - leaving it set would
        // silence the reconcile pass for this resource permanently.
        // Read before the `joined` sid is consumed below; see the follow-up
        // poll at the tail of this function for what it is for.
        let seated = joined.is_some();
        if let Some(sid) = joined {
            for input in routing.inputs.iter() {
                let Some(external) = input.external.as_ref() else {
                    continue;
                };
                if !external.hold_until_selected && input.stream_ids().contains(&sid) {
                    let key = (external.id, external.epoch);
                    if !inner.replay_inflight.lock().insert(key) {
                        debug!(
                            id = ?key.0,
                            epoch = key.1,
                            "a replay for this input is already queued or in flight; the join \
                             does not add a second"
                        );
                        continue;
                    }
                    if !inner.queue_job(Job::ReplaySub {
                        id: external.id,
                        epoch: external.epoch,
                        attempt: 0,
                    }) {
                        inner.replay_inflight.lock().remove(&key);
                    }
                }
            }
        }
        // PAST THE LOCK. `report_unsupported_subtitle` emits to the caller's
        // event handler, which is foreign code; nothing in this crate calls
        // into one while holding `routing`.
        drop(routing);
        // The consumer is foreign code too, and the FIRST of these calls out:
        // a join's replayed opening cues, taken (and their clear-suppression
        // armed) under the lock at the link, handed over now so a consumer
        // that stalls wedges this poller and nothing else. Bound-for-bound
        // the cues the branch would have delivered (`Inner::item_from_sample`
        // already ran).
        for item in replay_cues {
            inner.feed_subtitle(item);
        }
        for (sid, caps) in unsupported {
            inner.report_unsupported_subtitle(&sid, &caps);
        }
        for (key, sid, caps) in unwirable {
            // Remembered BEFORE the report, so the two cannot disagree about
            // whether this stream has been given up on -- and remembered even
            // when there is no sid to report, because the retry has to stop
            // either way.
            inner
                .unwirable_text_streams
                .lock()
                .insert((key, inner.current_generation()));
            if let Some(sid) = sid {
                inner.report_unsupported_subtitle(&sid, &caps);
            }
        }
        // ONE LINE PER (stream, load), and it carries the whole discriminator
        // so a field capture answers upstream-vs-selection without a rebuild.
        for stall in stalled {
            CAPSLESS_TEXT_STALLS.fetch_add(1, Ordering::SeqCst);
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
        Self::heal_latched_text_slots(inner);
        // The same shape one layer over: the slot is reachable but its
        // sticky CAPS was destroyed in flight by a flush, so the gate above can
        // never admit the stream. Runs AFTER the gate on purpose, since a rescue is
        // only ever needed for a stream the gate has just refused, and putting
        // it here means the very next poll is the one that joins.
        Self::rescue_lost_text_slot_caps(inner);
        // THE SHAPE WITH NO PAD TO WALK IN FROM, one layer over again: the
        // selected stream is in a decodebin3 multiqueue slot that has no OUTPUT
        // at all, so it is invisible to every rule above, all of which reason
        // about routed entries, and there is no routed entry for a slot
        // decodebin3 never ghosted. Both repairs above start at a routed pad
        // and so cannot reach it; see [`Inner::adopt_outputless_text_slot`].
        //
        // The trigger is "no LIVE pad carries the selection": either nothing
        // routed carries the sid, or everything that does has EOSed. On a
        // healthy item that is false and this costs one routing-lock read.
        let stranded = allowed_sid.as_deref().filter(|allowed| {
            let routing = inner.routing.lock();
            !routing.routed.iter().any(|routed| {
                routed.kind == StreamKind::Text
                    && !routed.saw_eos.load(Ordering::SeqCst)
                    && routed.db3_src_pad.stream_id().as_deref() == Some(*allowed)
            })
        });
        if let Some(allowed) = stranded {
            Self::adopt_outputless_text_slot(inner, allowed);
        }
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
        // Lever: `FCAST_NO_TEXT_SEAT_FOLLOWUP_POLL`.
        if seated && std::env::var_os("FCAST_NO_TEXT_SEAT_FOLLOWUP_POLL").is_none() {
            TEXT_SEAT_FOLLOWUP_POLLS.fetch_add(1, Ordering::SeqCst);
            Inner::request_text_policy_poll(inner);
        }
    }
}

/// One routed text stream the caps gate has been refusing for want of a sticky
/// CAPS for longer than [`CAPSLESS_TEXT_GRACE`]. Formed under the routing lock
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

/// Text-branch joins whose own pads were still INACTIVE when the upstream
/// link went in, i.e. the join-window latch's precondition.
///
/// ZERO is the intent (the join links downstream-first and syncs both elements
/// before the upstream link) and a nonzero count says that intent did not
/// hold, because `sync_state_with_parent` returning is not the same as the
/// branch's pads being active. It is the discriminator a capture needs to
/// tell "the join raced the activation" apart from any other way a slot can
/// latch, and it is read by `dash_testbed`'s slot-latch test.
static JOINS_INTO_AN_INACTIVE_BRANCH: AtomicU64 = AtomicU64::new(0);

/// How long the caps gate may refuse a routed text stream for want of a sticky
/// CAPS before it says so (see [`Inner::capsless_text_since`]).
///
/// Generous on purpose. The gate's transient really is sub-millisecond, but a
/// loaded machine can stretch a pad's first sticky by a lot, and this line is
/// meant to be believed when it fires: nothing that could still resolve on its
/// own should reach it.
const CAPSLESS_TEXT_GRACE: Duration = Duration::from_secs(5);

/// Routed text streams the caps gate gave up on for want of a sticky CAPS (see
/// [`CAPSLESS_TEXT_GRACE`]). One per (stream id, load generation).
static CAPSLESS_TEXT_STALLS: AtomicU64 = AtomicU64::new(0);

/// Times the seat stalemate break had to clear the contention latches because
/// every routed pad for the selected text stream was locked out with no branch
/// holding the seat (see its site in [`Inner::poll_text_policy`]).
///
/// Zero on a healthy item. Positive means the crate walked into a state no
/// reclaim could heal - every reclaim moves an EXISTING seat - and had to undo
/// its own bookkeeping to get out.
static TEXT_SEAT_STALEMATES: AtomicU64 = AtomicU64::new(0);

/// Times the seat was taken off a text branch whose decodebin3 slot had ENDED
/// while another pad for the same stream was still live (see
/// [`RoutedStream::saw_eos`] and its rule in [`Inner::poll_text_policy`]).
///
/// A REPAIR count. Every one is a re-selected subtitle track that would
/// otherwise have rendered nothing for the rest of the item with the crate
/// reporting the selection as applied.
static TEXT_EOS_SEAT_RECLAIMS: AtomicU64 = AtomicU64::new(0);

/// Follow-up polls the link loop asked for after seating a branch (see the tail
/// of [`Inner::poll_text_policy`]). One per join, and the number a caller that
/// polls on EVENTS ONLY depends on: in the field capture it is the only poll
/// that would have run in the 6.5 s after the bad seat.
static TEXT_SEAT_FOLLOWUP_POLLS: AtomicU64 = AtomicU64::new(0);

impl FcastPlaybin {
    /// How many text-branch surgeries ran off the deciding thread (see
    /// [`Inner::decider_only`]). Zero on a default-arm run - a debug build
    /// panics rather than counting - and positive under the inline levers,
    /// which is how a test proves the assertion is wired to anything at all.
    #[doc(hidden)]
    pub fn text_surgery_off_decider(&self) -> u64 {
        self.inner.text_surgery_off_decider.load(Ordering::SeqCst)
    }

    /// Routed text streams the caps gate gave up on for want of a sticky CAPS
    /// (see [`CAPSLESS_TEXT_STALLS`]). Process-global, so read it as a delta.
    /// Not part of the public API.
    #[doc(hidden)]
    pub fn capsless_text_stalls() -> u64 {
        CAPSLESS_TEXT_STALLS.load(Ordering::SeqCst)
    }

    /// Times the text seat stalemate break fired (see
    /// [`TEXT_SEAT_STALEMATES`]). Process-global, so read it as a delta. Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn text_seat_stalemates() -> u64 {
        TEXT_SEAT_STALEMATES.load(Ordering::SeqCst)
    }

    /// Times the seat moved off a text branch whose decodebin3 slot had ended
    /// (see [`TEXT_EOS_SEAT_RECLAIMS`]). Process-global, so read it as a delta.
    /// Not part of the public API.
    #[doc(hidden)]
    pub fn text_eos_seat_reclaims() -> u64 {
        TEXT_EOS_SEAT_RECLAIMS.load(Ordering::SeqCst)
    }

    /// Follow-up polls asked for after a seat (see
    /// [`TEXT_SEAT_FOLLOWUP_POLLS`]). Process-global, so read it as a delta.
    /// Not part of the public API.
    #[doc(hidden)]
    pub fn text_seat_followup_polls() -> u64 {
        TEXT_SEAT_FOLLOWUP_POLLS.load(Ordering::SeqCst)
    }

    /// TEST FAULT INJECTION: hold every text branch this instance joins at
    /// NULL for `hold` after its upstream link, staging the field's join window
    /// (see [`Inner::stage_join_hold_ms`]). Per instance, so it is safe under a
    /// test binary's thread pool. Not part of the public API.
    #[doc(hidden)]
    pub fn stage_join_before_active(&self, hold: Duration) {
        self.inner
            .stage_join_hold_ms
            .store(hold.as_millis() as u64, Ordering::SeqCst);
    }

    /// TEST FAULT INJECTION: destroy the next parked text stream's sticky CAPS
    /// on its decodebin3 ghost and on the multiqueue slot behind it, staging
    /// The staged caps loss (see [`Inner::stage_text_caps_loss`]). One shot,
    /// and per instance so it is safe under a test binary's thread pool.
    /// Not part of the public API.
    #[doc(hidden)]
    pub fn stage_text_caps_loss(&self) {
        self.inner
            .stage_text_caps_loss
            .store(true, Ordering::SeqCst);
    }

    /// Text-branch joins that linked into a still-INACTIVE branch (see
    /// [`JOINS_INTO_AN_INACTIVE_BRANCH`]). Process-global, so read it as a
    /// delta. Not part of the public API.
    #[doc(hidden)]
    pub fn joins_into_an_inactive_branch() -> u64 {
        JOINS_INTO_AN_INACTIVE_BRANCH.load(Ordering::SeqCst)
    }

    /// Cues the text park held through bring-up and the join handed back (see
    /// [`Inner::take_parked_text_cues`]). Process-global, so read it as a
    /// delta. Not part of the public API.
    #[doc(hidden)]
    pub fn parked_text_cues_replayed() -> u64 {
        crate::routing::parked_text_cues_replayed()
    }

    /// How many text-policy polls were folded into an already-queued one
    /// (see [`Inner::request_text_policy_poll`]). Not part of the public API.
    #[doc(hidden)]
    pub fn poll_policy_coalesced(&self) -> u64 {
        self.inner.poll_policy_coalesced.load(Ordering::SeqCst)
    }

    /// How many [`Job::PollTextPolicy`] jobs the worker has received. With
    /// the counter above this is the whole accounting a polling caller can
    /// be held to: every poll is either a job or a fold, and the jobs must
    /// never outnumber the polls. Not part of the public API.
    #[doc(hidden)]
    pub fn poll_policy_job_count(&self) -> u64 {
        self.inner.poll_jobs_seen.load(Ordering::SeqCst)
    }

    /// How many effects the subtitle-delivery reconcile pass has emitted (see
    /// [`Inner::reconcile_subtitle_delivery`]). A converged, aligned pipeline
    /// is a fixpoint: this must not move however often the pass runs. Not part
    /// of the public API.
    #[doc(hidden)]
    pub fn reconcile_emits() -> u64 {
        RECONCILE_EMITS.load(Ordering::SeqCst)
    }

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

    /// How many [`Job::DrainTextWork`] jobs the worker has received so far.
    /// A diagnostic counter for the busy-loop regression test, which pins
    /// that a caller polling at a pipeline parked below PLAYING does not
    /// re-queue the drain on every poll. Not part of the public API.
    #[doc(hidden)]
    pub fn drain_text_job_count(&self) -> u64 {
        self.inner.drain_jobs_seen.load(Ordering::SeqCst)
    }
}
