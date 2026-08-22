//! The external-subtitle replay chain's verdicts: what a sent replay came to,
//! what its verification concluded, and which text poke a tick owes.
//!
//! Pure over the facts the callers have already read. Every mutation stays on
//! their side of the boundary: the settle, the hold release and the emits in
//! [`crate::FcastPlaybin::replay_outcome`], the re-replay in
//! [`crate::FcastPlaybin::verify_replay`], the two queue_job calls in
//! `Inner::run_tick`.
//!
//! The facts are ENUMS wherever the callers read them lazily, so a verdict can
//! never be asked for a fact nobody read. That is not decoration: the
//! exhaustion facts cost three routing reads no other outcome needs, and
//! `run_tick`'s reads are one-mutex-at-a-time on the thread that has to
//! survive a wedged decider.

use crate::jobs::DRAIN_REPOKE_TICKS;

/// How many replays a single trigger may issue before the chain escalates
/// (see [`crate::FcastPlaybin::replay_outcome`]).
pub(crate) const REPLAY_ATTEMPTS: u32 = 3;

/// Whether a replay was REFUSED: pads were offered the seek and not one took
/// it, so the replay is postponed to a moment the pipeline can carry it (see
/// [`crate::FcastPlaybin::replay_outcome`]). Zero pads offered is NOT a
/// refusal, the input simply had no pads, and postponing there would owe a
/// replay nothing can discharge.
pub(crate) fn replay_refused(accepted: usize, total: usize) -> bool {
    accepted == 0 && total > 0
}

/// Whether the pipeline is settled at PLAYING, which is both the only state
/// that can carry a flushing seek and the only one whose stickies prove
/// anything about the CURRENT tenure of an input.
pub(crate) fn settled_playing(current: gst::State, pending: gst::State) -> bool {
    current == gst::State::Playing && pending == gst::State::VoidPending
}

/// Which question a finished replay leaves open, i.e. which facts its verdict
/// needs.
///
/// Separate from the verdict because the answer picks the reads: a refusal
/// asks the pipeline and the selection, an exhaustion walks the routed pads
/// three times, and the common recheck asks nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayAsk {
    /// Not one pad took the seek. A pipeline at rest in PAUSED refuses a
    /// flushing seek on every pad, every push logging `Failed to push event
    /// ... state="paused"`, and the verification then correctly saw the stream
    /// still unaligned and replayed again: four rounds of work that could not
    /// succeed by construction.
    Refusal,
    /// The seek travelled and attempts remain.
    Recheck,
    /// The seek travelled and this was the last attempt.
    Exhaustion,
}

/// Pick the question from the lane's observation and the attempt.
///
/// Decided from the OUTCOME rather than from the pipeline state: a state check
/// also matched a pipeline transiently at rest DURING a seek, where the seek
/// IS accepted, and postponing there left the input unaligned for good.
///
/// A REFUSAL OUTRANKS the attempt bound: a refused seek performed none of the
/// work an attempt stands for, so it may not spend one, and an input at the
/// last attempt that the pipeline simply could not carry must not escalate.
pub(crate) fn replay_ask(accepted: usize, total: usize, attempt: u32) -> ReplayAsk {
    if replay_refused(accepted, total) {
        ReplayAsk::Refusal
    } else if attempt < REPLAY_ATTEMPTS {
        ReplayAsk::Recheck
    } else {
        ReplayAsk::Exhaustion
    }
}

/// What [`crate::FcastPlaybin::replay_outcome`] read for the question
/// [`replay_ask`] gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayFacts {
    Refusal {
        /// The pipeline is below a settled PLAYING, so the refusal is about
        /// the PIPELINE and not about the branch.
        parked: bool,
        /// The input is still attached under this epoch and its stream is
        /// still what the selection wants (`Inner::replay_chain_wanted`).
        /// Read only when `parked`, and false otherwise, which cannot change
        /// the verdict.
        chain_wanted: bool,
    },
    Recheck,
    Exhaustion {
        /// No routed pad carries any of this input's text sids, so no pad can
        /// ever carry this stream and only a fresh input gets a slot back.
        /// `Inner::external_stream_outputless` and not the slotless read,
        /// because slotless implies it (the same no-routed-pad test plus a
        /// drain requirement) and outputless also covers the fully wedged
        /// attach, which records no drain at all.
        unservable: bool,
        /// A JOINED branch whose tail never received a SEGMENT after a whole
        /// chain of flushing replays is just as unservable: the multiqueue
        /// destroyed the segment in flight (C12 family, the sibling of the
        /// rescued CAPS) and a replay cannot re-send what dies inside the
        /// slot, so left alone the reconcile pass re-emits a doomed chain
        /// every 2 s for the rest of the item.
        segmentless: bool,
        /// `epoch > 0`, i.e. a re-attach already had its chance
        /// (`external_stream_slotless` hands the first occurrence to
        /// `Job::RetrySub`, which bumps the epoch).
        reattached: bool,
    },
}

/// What a sent replay means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayVerdict {
    /// Refused while parked, with the chain still wanted: arm the
    /// verification, with the SAME attempt (a refused seek performed none).
    ///
    /// A refusal at rest in PAUSED is about the PIPELINE, not about the
    /// branch, so the question this replay was asked for is still open - and
    /// the reconcile pass can only carry it while the branch reads UNALIGNED.
    /// A replay the verification asked for because the branch was SILENT
    /// leaves an aligned one behind, which that pass calls converged, so
    /// deferring to it there loses the chain and the silence becomes
    /// permanent. The check holds its own verdict below a settled PLAYING
    /// (see [`verify_verdict`]), so this does not rediscover the same thing
    /// four times over: it waits with the delivery-evidence term intact for
    /// the first moment a flushing seek could be accepted, which is the same
    /// moment the pass would have re-emitted at.
    RearmSameAttempt,
    /// Refused with nothing left to carry the question, so it is owed to
    /// nobody. The refusal left the input unaligned, and unaligned is exactly
    /// what the reconcile pass observes at the next settled PLAYING - which is
    /// also the first moment a flushing seek could be accepted. Remembering
    /// the attempt would only reproduce that schedule from memory.
    LeaveToReconcile,
    /// Sent, attempts left: arm the verification. A replay can race the very
    /// slot swap that requested it (the re-delivery drains into a slot
    /// decodebin3 is still relinking), so check back once, on a bounded timer
    /// exactly like the sub watchdog.
    ArmVerification,
    /// Exhausted on an input that cannot deliver AFTER a re-attach already had
    /// its chance.
    Fail,
    /// Exhausted on an input that cannot deliver on its first tenure:
    /// re-attach it, which is the only way to get a decodebin3 slot back.
    Retry,
    /// Exhausted but servable: loud, left ATTACHED, and the join it is waiting
    /// on gets poked.
    PokeJoin,
}

impl ReplayVerdict {
    /// Whether this verdict leaves a postponed item behind, which invalidates
    /// the last drain's no-op verdict (see `Inner::drain_poke_parked`).
    pub(crate) fn postponed(&self) -> bool {
        matches!(self, Self::RearmSameAttempt | Self::LeaveToReconcile)
    }
}

/// The verdict on one finished replay.
///
/// The exhaustion arm used to end in SILENCE, which left the receiver
/// reporting a subtitle track that renders nothing (in upstream-selection mode
/// the crate's own merged report has already named this sid, so the caller
/// believes the switch worked). It must NOT end in a detach either, which is
/// what the first version of the escalation did and what the field then
/// punished: exhaustion on an input that materialized and is DELIVERING into
/// decodebin3 means "its branch has not joined yet", not "the file is bad".
/// The join runs from `poll_text_policy` on the CALLER's cadence while the
/// attempts run on a 400ms worker timer, so a slow caller loses the race and a
/// perfectly servable external got detached and dropped from the user's track
/// list (`ResourceNotFound` at the sender).
pub(crate) fn replay_verdict(facts: ReplayFacts) -> ReplayVerdict {
    match facts {
        // A refusal at rest in PAUSED is about the pipeline, so the question
        // this replay was asked for is still open. The chain carries it only
        // while there is something to carry it FOR; otherwise the pass owns
        // it, which costs nothing a refusal had.
        ReplayFacts::Refusal {
            parked,
            chain_wanted,
        } => {
            if parked && chain_wanted {
                ReplayVerdict::RearmSameAttempt
            } else {
                ReplayVerdict::LeaveToReconcile
            }
        }
        // A replay can race the very slot swap that requested it, so check
        // back once.
        ReplayFacts::Recheck => ReplayVerdict::ArmVerification,
        // Only the case NOTHING can serve is failed. Anything with a carrier
        // is left attached: exhaustion on an input that is delivering into
        // decodebin3 means "its branch has not joined yet", not "the file is
        // bad", and detaching there dropped perfectly servable externals from
        // the user's track list.
        ReplayFacts::Exhaustion {
            unservable,
            segmentless,
            reattached,
        } => {
            if !(unservable || segmentless) {
                ReplayVerdict::PokeJoin
            } else if reattached {
                ReplayVerdict::Fail
            } else {
                ReplayVerdict::Retry
            }
        }
    }
}

/// What [`crate::FcastPlaybin::verify_replay`] read.
///
/// The split IS the anti-finding: below a settled PLAYING there is no evidence
/// to read, only the question of whether the chain still has a subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyFacts {
    Unsettled {
        /// `Inner::replay_chain_wanted`, the same two questions the settled
        /// arm asks before it concludes.
        chain_wanted: bool,
    },
    Settled(SettledFacts),
}

/// The evidence a settled PLAYING makes readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SettledFacts {
    /// The incarnation this check was decided against is still attached.
    pub(crate) attached: bool,
    /// `Inner::selection_wants_external` for this input's sids.
    pub(crate) selection_wants: bool,
    /// A live routed text branch carries one of this input's sids
    /// (`Inner::text_stream_delivered`).
    pub(crate) delivered: bool,
    /// The subtitle segment sits on the video's origin
    /// (`Inner::subtitle_origin_matches_video`). Read only when `delivered`,
    /// and false otherwise, which cannot change the verdict.
    pub(crate) origin_matches: bool,
    /// THE DELIVERY-EVIDENCE TERM: a cue reached the consumer since this
    /// replay's hand-off (`Inner::external_cues_fed` past the input's
    /// `fed_baseline`). Alignment alone cannot prove a cue survived the trip,
    /// and a burst the multiqueue destroyed leaves a seated, aligned, SILENT
    /// branch that every weaker test calls converged.
    pub(crate) progressed: bool,
}

/// What one replay verification concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyVerdict {
    /// No verdict below a settled PLAYING and the chain still has a subject:
    /// RE-ASK. Nothing is remembered - the ARMING is the question, held open
    /// across a window that cannot answer it - but the question itself must
    /// not be handed on, which is what returning used to do.
    ///
    /// `Inner::reconcile_subtitle_delivery` asks a strictly WEAKER one. Its
    /// convergence test is `delivered && aligned`, two sticky reads that a
    /// joined branch and a post-seek segment satisfy on their own, and it has
    /// no delivery-evidence term at all. A burst the multiqueue destroyed in
    /// flight leaves a seated, aligned, SILENT branch, which reads as
    /// converged there on every later pass, so a replay deferred to it was
    /// never re-derived and the track rendered nothing for the rest of the
    /// item. The evidence term lives on THIS chain
    /// ([`SettledFacts::progressed`]), and so does the attempt bound that
    /// makes asking about silence safe at all: silence is also what an
    /// external with no cue at this position looks like, and a 1 Hz pass
    /// acting on that would flush the branch forever. So the chain is what has
    /// to survive the window.
    ///
    /// The SAME attempt: a verdict that was not reached spends none.
    RearmSameAttempt,
    /// No verdict and nothing wants this input any more: the chain ends here.
    ///
    /// A check re-armed across a window that cannot decide it must not outlive
    /// its subject, or a detached input leaves a timer re-arming itself for
    /// the rest of the item. Bounded by the RESOURCE rather than by a counter:
    /// the re-arm stops the moment the pipeline can answer, and a load reset
    /// drops the timer with its dedupe key (see
    /// `Inner::clear_pending_timers`).
    ChainEnds,
    /// The incarnation is gone: nothing to verify and nothing that could
    /// answer.
    Gone,
    /// The selection moved on, and it owns its own replay.
    SelectionMovedOn,
    /// Aligned delivery WITH evidence a cue reached the consumer.
    Converged,
    /// Seated but unaligned, or aligned but silent: replay again, one attempt
    /// further.
    ReplayAgain,
}

impl VerifyVerdict {
    /// Whether this verdict leaves a postponed item behind, which invalidates
    /// the last drain's no-op verdict (see `Inner::drain_poke_parked`).
    pub(crate) fn postponed(&self) -> bool {
        matches!(self, Self::RearmSameAttempt | Self::ChainEnds)
    }
}

/// The verdict on one replay verification.
pub(crate) fn verify_verdict(facts: VerifyFacts) -> VerifyVerdict {
    match facts {
        VerifyFacts::Unsettled { chain_wanted } => {
            if chain_wanted {
                VerifyVerdict::RearmSameAttempt
            } else {
                VerifyVerdict::ChainEnds
            }
        }
        VerifyFacts::Settled(SettledFacts {
            attached,
            selection_wants,
            delivered,
            origin_matches,
            progressed,
        }) => {
            if !attached {
                return VerifyVerdict::Gone;
            }
            if !selection_wants {
                return VerifyVerdict::SelectionMovedOn;
            }
            // Delivered is not enough: an input that joined WITHOUT a replay
            // carries its own file-origin segment and renders shifted whenever
            // the video's origin moved. Aligned is not enough either.
            let aligned = delivered && origin_matches;
            if aligned && progressed {
                VerifyVerdict::Converged
            } else {
                VerifyVerdict::ReplayAgain
            }
        }
    }
}

/// Whether this tick is one of the once-a-second poke ticks.
///
/// Named rather than inlined so `run_tick` can skip the three liveness reads
/// on the other ticks without restating the gate [`tick_pokes`] applies.
pub(crate) fn repoke_due(tick: u64) -> bool {
    tick.is_multiple_of(DRAIN_REPOKE_TICKS)
}

/// The liveness facts `run_tick` reads for a poke tick, one mutex at a time.
///
/// A later fact may be left false once an earlier one has already decided the
/// answer (`deferred_work` dominates, and either liveness arm suffices), which
/// is what keeps the reads off the ticks that cannot use them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TickFacts {
    /// `Inner::has_deferred_text_work`: postponed work is remembered.
    pub(crate) deferred_work: bool,
    /// `Inner::holds_an_item`: an item is live.
    pub(crate) holds_an_item: bool,
    /// The selection engine still owes an answer.
    pub(crate) unconverged: bool,
}

/// Which text poke a tick owes. ONE value, because the two pokes are mutually
/// exclusive by construction: both queue `Job::DrainTextWork`, and firing both
/// would give the worker two drains a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pokes {
    /// Nothing: not a poke tick, or an idle crate.
    None,
    /// (3) The liveness re-poke for the postponed-work drain, once a second.
    /// Every other poke for it is EDGE-triggered, and all of them can miss at
    /// once (a parked verdict on a pipeline that never crosses another edge).
    /// `drain_poke_parked` is deliberately NOT consulted; that parked verdict
    /// with no following edge IS the hole this closes. The drain's own gate
    /// makes each poke a cheap no-op below a settled PLAYING.
    DrainDeferred,
    /// (3b) The reconcile trigger, once a second while the crate is live.
    ///
    /// The reconcile pass exists for divergences no edge is coming for (a
    /// selected external delivering unaligned on a settled pipeline). The poke
    /// above cannot serve it, because its condition is "something is
    /// remembered" and the pass's whole point is that nothing is. Gated on
    /// liveness because that is the weakest condition still admitting a
    /// divergence nobody wrote down.
    DrainReconcile,
}

/// The tick's poke interlock. Exactly one drain per second while an item is
/// live, and none at rest.
pub(crate) fn tick_pokes(tick: u64, facts: TickFacts) -> Pokes {
    if !repoke_due(tick) {
        return Pokes::None;
    }
    if facts.deferred_work {
        return Pokes::DrainDeferred;
    }
    // Liveness is the UNION of the things the two pokes can act on, not a
    // per-poke condition, because (3b)'s subject is not remembered anywhere.
    // `deferred_work` is the third arm of that union and is spent above.
    if facts.holds_an_item || facts.unconverged {
        return Pokes::DrainReconcile;
    }
    Pokes::None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The postponement rule of [`crate::FcastPlaybin::replay_outcome`]: only a
    /// real refusal (offered pads, zero takers) postpones. An input with no
    /// pads at all is not refused, and any accepted pad means the seek
    /// travelled.
    #[test]
    fn a_replay_is_refused_only_when_offered_pads_all_declined() {
        let cases: &[(usize, usize, bool)] = &[
            (0, 0, false),
            (0, 1, true),
            (0, 5, true),
            (1, 1, false),
            (1, 5, false),
            (5, 5, false),
        ];
        for (accepted, total, expected) in cases {
            assert_eq!(
                replay_refused(*accepted, *total),
                *expected,
                "accepted {accepted} of {total}"
            );
        }
    }

    #[test]
    fn settled_playing_table() {
        use gst::State::{Null, Paused, Playing, VoidPending};
        assert!(settled_playing(Playing, VoidPending), "flowing");
        assert!(!settled_playing(Paused, VoidPending), "at rest");
        assert!(!settled_playing(Playing, Paused), "going down");
        assert!(!settled_playing(Paused, Playing), "coming up");
        assert!(!settled_playing(Null, VoidPending), "nothing there");
    }

    // ---- replay_ask ------------------------------------------------------

    #[test]
    fn replay_ask_table() {
        // (accepted, total, attempt) -> ask
        let cases: &[(usize, usize, u32, ReplayAsk)] = &[
            // A destroyed burst offers pads and gets no takers.
            (0, 1, 0, ReplayAsk::Refusal),
            (0, 3, 1, ReplayAsk::Refusal),
            // A refusal outranks the attempt bound: it spends no attempt, so
            // the last one may not escalate on it.
            (0, 3, REPLAY_ATTEMPTS, ReplayAsk::Refusal),
            (0, 3, REPLAY_ATTEMPTS + 9, ReplayAsk::Refusal),
            // A partial take is a travelled seek, never a refusal.
            (1, 5, 0, ReplayAsk::Recheck),
            (1, 5, REPLAY_ATTEMPTS - 1, ReplayAsk::Recheck),
            (1, 5, REPLAY_ATTEMPTS, ReplayAsk::Exhaustion),
            (5, 5, REPLAY_ATTEMPTS, ReplayAsk::Exhaustion),
            // No pads offered at all: not a refusal, so the bound applies.
            (0, 0, 0, ReplayAsk::Recheck),
            (0, 0, REPLAY_ATTEMPTS, ReplayAsk::Exhaustion),
        ];
        for (accepted, total, attempt, expected) in cases {
            assert_eq!(
                replay_ask(*accepted, *total, *attempt),
                *expected,
                "accepted {accepted} of {total} at attempt {attempt}"
            );
        }
    }

    // ---- replay_verdict --------------------------------------------------

    fn exhaustion(unservable: bool, segmentless: bool, reattached: bool) -> ReplayFacts {
        ReplayFacts::Exhaustion {
            unservable,
            segmentless,
            reattached,
        }
    }

    #[test]
    fn replay_verdict_table() {
        // (facts, verdict, why)
        let cases: &[(ReplayFacts, ReplayVerdict, &str)] = &[
            (
                ReplayFacts::Refusal {
                    parked: true,
                    chain_wanted: true,
                },
                ReplayVerdict::RearmSameAttempt,
                "a parked pipeline refused it and the chain has a subject",
            ),
            (
                ReplayFacts::Refusal {
                    parked: true,
                    chain_wanted: false,
                },
                ReplayVerdict::LeaveToReconcile,
                "nothing wants the input, so the chain must not outlive it",
            ),
            (
                ReplayFacts::Refusal {
                    parked: false,
                    chain_wanted: true,
                },
                ReplayVerdict::LeaveToReconcile,
                "refused while flowing: the pass re-asks at the next settle",
            ),
            (
                ReplayFacts::Refusal {
                    parked: false,
                    chain_wanted: false,
                },
                ReplayVerdict::LeaveToReconcile,
                "neither term holds",
            ),
            (
                ReplayFacts::Recheck,
                ReplayVerdict::ArmVerification,
                "the seek travelled and attempts remain",
            ),
            (
                exhaustion(false, false, false),
                ReplayVerdict::PokeJoin,
                "servable and delivering: the join is what is missing",
            ),
            (
                exhaustion(false, false, true),
                ReplayVerdict::PokeJoin,
                "a re-attached but servable input is still not failed",
            ),
            (
                exhaustion(true, false, false),
                ReplayVerdict::Retry,
                "no carrier pad on the first tenure: re-attach",
            ),
            (
                exhaustion(true, false, true),
                ReplayVerdict::Fail,
                "no carrier pad after a re-attach already had its chance",
            ),
            (
                exhaustion(false, true, false),
                ReplayVerdict::Retry,
                "the destroyed segment (C12): a replay cannot re-send it",
            ),
            (
                exhaustion(false, true, true),
                ReplayVerdict::Fail,
                "a re-attached branch whose segment died in flight again",
            ),
            (
                exhaustion(true, true, true),
                ReplayVerdict::Fail,
                "both unservable terms",
            ),
        ];
        for (facts, expected, why) in cases {
            assert_eq!(replay_verdict(*facts), *expected, "{why}");
        }
    }

    #[test]
    fn only_the_refusals_invalidate_the_drain_verdict() {
        assert!(ReplayVerdict::RearmSameAttempt.postponed());
        assert!(ReplayVerdict::LeaveToReconcile.postponed());
        assert!(!ReplayVerdict::ArmVerification.postponed());
        assert!(!ReplayVerdict::Fail.postponed());
        assert!(!ReplayVerdict::Retry.postponed());
        assert!(!ReplayVerdict::PokeJoin.postponed());
    }

    // ---- verify_verdict --------------------------------------------------

    /// A settled row. Defaults are the converged case, so each row states only
    /// the term it is about.
    fn converged() -> SettledFacts {
        SettledFacts {
            attached: true,
            selection_wants: true,
            delivered: true,
            origin_matches: true,
            progressed: true,
        }
    }

    #[test]
    fn verify_verdict_table() {
        let cases: &[(VerifyFacts, VerifyVerdict, &str)] = &[
            (
                VerifyFacts::Unsettled { chain_wanted: true },
                VerifyVerdict::RearmSameAttempt,
                "no evidence below a settled PLAYING; the chain survives it",
            ),
            (
                VerifyFacts::Unsettled {
                    chain_wanted: false,
                },
                VerifyVerdict::ChainEnds,
                "no evidence and no subject",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    attached: false,
                    ..converged()
                }),
                VerifyVerdict::Gone,
                "the incarnation is gone",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    selection_wants: false,
                    ..converged()
                }),
                VerifyVerdict::SelectionMovedOn,
                "the selection moved on and owns its own replay",
            ),
            (
                VerifyFacts::Settled(converged()),
                VerifyVerdict::Converged,
                "aligned delivery with a cue past the baseline",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    progressed: false,
                    ..converged()
                }),
                VerifyVerdict::ReplayAgain,
                "THE DESTROYED BURST: seated, aligned and silent is not converged",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    origin_matches: false,
                    ..converged()
                }),
                VerifyVerdict::ReplayAgain,
                "joined without a replay: delivering against its own origin",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    delivered: false,
                    origin_matches: false,
                    ..converged()
                }),
                VerifyVerdict::ReplayAgain,
                "not delivered, however the origins compare",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    delivered: false,
                    origin_matches: false,
                    progressed: false,
                    ..converged()
                }),
                VerifyVerdict::ReplayAgain,
                "nothing at all reached the branch",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    selection_wants: false,
                    progressed: false,
                    ..converged()
                }),
                VerifyVerdict::SelectionMovedOn,
                "the desire term outranks the evidence terms",
            ),
            (
                VerifyFacts::Settled(SettledFacts {
                    attached: false,
                    selection_wants: false,
                    ..converged()
                }),
                VerifyVerdict::Gone,
                "a gone incarnation is answered before the desire term",
            ),
        ];
        for (facts, expected, why) in cases {
            assert_eq!(verify_verdict(*facts), *expected, "{why}");
        }
    }

    #[test]
    fn only_the_unsettled_exits_invalidate_the_drain_verdict() {
        assert!(VerifyVerdict::RearmSameAttempt.postponed());
        assert!(VerifyVerdict::ChainEnds.postponed());
        assert!(!VerifyVerdict::Gone.postponed());
        assert!(!VerifyVerdict::SelectionMovedOn.postponed());
        assert!(!VerifyVerdict::Converged.postponed());
        assert!(!VerifyVerdict::ReplayAgain.postponed());
    }

    // ---- tick_pokes ------------------------------------------------------

    #[test]
    fn tick_pokes_table() {
        let all = TickFacts {
            deferred_work: true,
            holds_an_item: true,
            unconverged: true,
        };
        // (tick, facts, pokes, why)
        let cases: &[(u64, TickFacts, Pokes, &str)] = &[
            (
                1,
                all,
                Pokes::None,
                "not a poke tick, however live the crate is",
            ),
            (
                DRAIN_REPOKE_TICKS - 1,
                all,
                Pokes::None,
                "the tick before the poke tick",
            ),
            (
                0,
                all,
                Pokes::DrainDeferred,
                "the first tick is a poke tick",
            ),
            (
                DRAIN_REPOKE_TICKS,
                all,
                Pokes::DrainDeferred,
                "remembered work outranks the reconcile trigger; never both",
            ),
            (
                DRAIN_REPOKE_TICKS,
                TickFacts {
                    deferred_work: false,
                    holds_an_item: true,
                    unconverged: false,
                },
                Pokes::DrainReconcile,
                "an item is live, so a divergence nobody wrote down is possible",
            ),
            (
                DRAIN_REPOKE_TICKS,
                TickFacts {
                    deferred_work: false,
                    holds_an_item: false,
                    unconverged: true,
                },
                Pokes::DrainReconcile,
                "the engine still owes an answer",
            ),
            (
                DRAIN_REPOKE_TICKS,
                TickFacts::default(),
                Pokes::None,
                "an idle crate queues nothing",
            ),
            (
                DRAIN_REPOKE_TICKS * 4,
                TickFacts {
                    deferred_work: true,
                    holds_an_item: false,
                    unconverged: false,
                },
                Pokes::DrainDeferred,
                "remembered work is itself an arm of the liveness union",
            ),
        ];
        for (tick, facts, expected, why) in cases {
            assert_eq!(tick_pokes(*tick, *facts), *expected, "tick {tick}: {why}");
        }
    }
}
