//! The diagnostic census, as two snapshot structs.
//!
//! Every counter in the crate is an instrument for a SILENTLY-FAILING
//! invariant: a text track that is alive but undeliverable, a descent that
//! leaked its thread, a flush pair that latched a multiqueue slot. None of them
//! change behaviour, all of them answer "did the shape happen", and the tests
//! are the only readers.
//!
//! # Why a snapshot instead of one accessor per counter
//!
//! There used to be one `#[doc(hidden)] pub fn` per counter, each with its own
//! paragraph repeating what the counter's own declaration already says. Reading
//! two counters meant two calls that could not be taken at the same instant,
//! and the process-global ones came with a standing warning that their totals
//! are CUMULATIVE across a test binary, so only "fired" and "never fires" were
//! assertable. A snapshot fixes both: one read gives a consistent set, and
//! `after - before` around a gesture is finally a number that means something.
//!
//! # The zero verdict is part of the declaration
//!
//! Each entry carries `zero` or `any`. `zero` means a healthy run leaves the
//! DELTA at zero, which is the claim the counter's own doc makes in prose;
//! stating it here makes it checkable. [`GlobalStats::moved_from_zero`] turns
//! the whole set into one assertion, which is how the counters whose invariant
//! was never written down became live checks.

use std::sync::atomic::Ordering;

use crate::Inner;

/// Declare a snapshot struct over a set of `AtomicU64` counters.
///
/// Each entry is `<verdict> <field> = <atomic expression>;`. The field name is
/// what tests read, the expression is evaluated against the struct's source
/// binding, and the verdict says whether a healthy run may move it.
macro_rules! counters {
    // A counter a healthy run must leave alone contributes a delta check.
    (@verdict zero, $out:ident, $after:ident, $before:ident, $field:ident) => {
        let delta = $after.$field.saturating_sub($before.$field);
        if delta != 0 {
            $out.push((stringify!($field), delta));
        }
    };
    // One that legitimately moves contributes nothing.
    (@verdict any, $out:ident, $after:ident, $before:ident, $field:ident) => {};

    (
        $(#[$struct_meta:meta])*
        struct $name:ident from $src:ident: $src_ty:ty {
            $(
                $(#[$field_meta:meta])*
                $verdict:ident $field:ident = $read:expr;
            )*
        }
    ) => {
        $(#[$struct_meta])*
        #[doc(hidden)]
        #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
        pub struct $name {
            $(
                $(#[$field_meta])*
                pub $field: u64,
            )*
        }

        impl $name {
            /// Read every counter once. Not atomic as a set (nothing orders
            /// against a census counter), just taken close together.
            pub(crate) fn take($src: $src_ty) -> Self {
                let _ = &$src;
                Self {
                    $( $field: $read.load(Ordering::Relaxed), )*
                }
            }

            /// Counters declared `zero` that moved since `before`, as
            /// `(name, delta)` pairs in declaration order.
            ///
            /// Empty is the healthy answer. A caller that knows a particular
            /// counter is expected to fire in its own scenario filters that
            /// name out and says why.
            #[doc(hidden)]
            pub fn moved_from_zero(&self, before: &Self) -> Vec<(&'static str, u64)> {
                // Both unread when every counter in the set is `any`.
                let _ = before;
                #[allow(unused_mut)]
                let mut out: Vec<(&'static str, u64)> = Vec::new();
                $( { counters!(@verdict $verdict, out, self, before, $field); } )*
                out
            }
        }
    };
}

counters! {
    /// Per-instance counters, snapshotted by [`crate::FcastPlaybin::stats`].
    ///
    /// PER INSTANCE is the point for most of these: the crate's tracing goes to
    /// one process-global subscriber, so a test binary running several
    /// pipelines at once cannot tell whose line it is reading, and a counter
    /// hanging off `Inner` can.
    struct Stats from inner: &Inner {
        /// Deadlines that fired, counting BOTH families: a selection waiting
        /// for its `STREAMS_SELECTED` and a refresh seek waiting for its
        /// `ASYNC_DONE`. The name reads narrower than it is and the tests rely
        /// on the wider meaning; the confirm/give-up pair below is
        /// selection-only, because a refresh has no such outcomes.
        any selection_deadline_fires = inner.counters.deadline_fires;
        /// Deadline fires that ended in a synthetic confirmation built from
        /// the routing probe.
        any selection_deadline_confirms = inner.counters.deadline_confirms;
        /// Deadline fires that exhausted their retries and reported what is
        /// actually playing instead of what was asked for.
        any selection_deadline_giveups = inner.counters.deadline_giveups;
        /// Deadline fires that deferred to a select the hands had not sent yet.
        any selection_deadline_deferrals = inner.counters.deadline_deferrals;
        /// Gapless activations that found the audio boundary already crossed
        /// at arm time and released the held events right there.
        any arm_time_activation_releases = inner.counters.arm_time_releases;
        /// Queued jobs the supersession gate dropped.
        any stale_job_drops = inner.counters.stale_jobs_dropped;
        /// Text-policy polls folded into an already-queued one.
        any poll_policy_coalesced = inner.counters.poll_policy_coalesced;
        /// `Job::PollTextPolicy` jobs the worker received. With the fold count
        /// above this is the whole accounting a polling caller can be held to:
        /// every poll is either a job or a fold, and the jobs must never
        /// outnumber the polls.
        any poll_policy_job_count = inner.counters.poll_jobs_seen;
        /// `Job::DrainTextWork` jobs the worker received.
        any drain_text_job_count = inner.counters.drain_jobs_seen;
    }
}

counters! {
    /// Process-global counters, snapshotted by
    /// [`crate::FcastPlaybin::global_stats`].
    ///
    /// Global because several of them are incremented after the `Inner` that
    /// caused them is gone (the teardown flush pairs are the reason the whole
    /// census is global), so there is no handle left to hang them off at the
    /// moment they matter most. The consequence is that a test binary's totals
    /// are CUMULATIVE across its tests, which is what makes the delta idiom
    /// (`take` before the gesture, `take` after) the only honest way to read
    /// them.
    ///
    /// The two array-shaped censuses, [`crate::FcastPlaybin::crate_flush_pairs_for`]
    /// and [`crate::FcastPlaybin::flow_census_flushing_for`], stay out of here
    /// deliberately: they are per-reason and per-stage, keyed by name, and a
    /// sum over them cannot say which one moved.
    struct GlobalStats from unit: () {
        /// Text re-selects the drain interlock held back.
        any text_drain_interlocks = crate::dispatch::TEXT_DRAIN_INTERLOCKS;
        /// Drain interlocks that hit their budget and sent anyway.
        zero text_drain_interlock_timeouts = crate::dispatch::TEXT_DRAIN_INTERLOCK_TIMEOUTS;
        /// Teardown descents that blew their budget and were detached: a
        /// leaked thread plus a leaked graph, logged at error.
        zero teardown_descent_stuck = crate::teardown::TEARDOWN_DESCENT_STUCK;
        /// Rescue descents that blew their budget and were detached, poisoning
        /// the crate. Nonzero means one teardown could not take its pipeline
        /// down and every job since has been refused.
        zero rescue_disarm_timeouts = crate::teardown::RESCUE_DISARM_TIMEOUTS;
        /// Side-input flushes that reset the pipeline's start time and had it
        /// put back. A repair count.
        any start_time_restores = crate::teardown::START_TIME_RESTORES;
        /// Flush pairs whose target pad went from active to INACTIVE across
        /// the pair, where gstpad discards the FLUSH_STOP and the pad stays
        /// flushing for good.
        zero flush_pair_activity_transitions = crate::flush::FLUSH_PAIR_ACTIVITY_TRANSITIONS;
        /// decodebin3 multiqueue slots this crate's own flush pair latched and
        /// the un-latch then re-activated. A REPAIR count, not a failure count.
        any slot_unlatches = crate::flush::SLOT_UNLATCHES;
        /// Disposals whose slot was NOT latched, so a zero in `slot_unlatches`
        /// can be told apart from an un-latch that never ran at all.
        any slot_unlatch_clean = crate::flush::SLOT_UNLATCH_CLEAN;
        /// Latched slots the re-activation did not clear. Nonzero means a text
        /// track is dead with the repair in place.
        zero slot_unlatch_failures = crate::flush::SLOT_UNLATCH_FAILURES;
        /// Text slots whose in-flight sticky CAPS multiqueue destroyed and the
        /// rescue put back. A REPAIR count.
        any text_caps_rescues = crate::flush::TEXT_CAPS_RESCUES;
        /// Lost text CAPS the restore did NOT recover.
        zero text_caps_rescue_failures = crate::flush::TEXT_CAPS_RESCUE_FAILURES;
        /// Mid-play disposals that fell back to the v1 queue pair because the
        /// branch would not quiesce inside its budget. The counted residual,
        /// not a failure.
        any disposal_quiesce_timeouts = crate::text_disposal::DISPOSAL_QUIESCE_TIMEOUTS;
        /// Effects the subtitle-delivery reconcile pass emitted. A converged,
        /// aligned pipeline is a fixpoint, so this must not move however often
        /// the pass runs, but the pass legitimately emits while converging.
        any reconcile_emits = crate::text_policy::RECONCILE_EMITS;
        /// Text-branch joins that linked into a still-INACTIVE branch.
        any joins_into_an_inactive_branch = crate::text_policy::JOINS_INTO_AN_INACTIVE_BRANCH;
        /// Routed text streams the caps gate gave up on for want of a sticky
        /// CAPS. The grace in front of it is generous on purpose, so nothing
        /// that could still resolve on its own reaches it.
        zero capsless_text_stalls = crate::text_policy::CAPSLESS_TEXT_STALLS;
        /// Times the text seat stalemate break fired. Zero on a healthy item:
        /// positive means the crate walked into a state no reclaim could heal
        /// and had to undo its own bookkeeping to get out.
        zero text_seat_stalemates = crate::text_policy::TEXT_SEAT_STALEMATES;
        /// Times the seat moved off a text branch whose decodebin3 slot had
        /// ended.
        any text_eos_seat_reclaims = crate::text_policy::TEXT_EOS_SEAT_RECLAIMS;
        /// Follow-up polls the link loop asked for after seating a branch. One
        /// per join, and the number a caller that polls on EVENTS ONLY depends
        /// on.
        any text_seat_followup_polls = crate::text_policy::TEXT_SEAT_FOLLOWUP_POLLS;
        /// Cues the text park held through bring-up and the join handed back.
        any parked_text_cues_replayed = crate::routing::PARKED_TEXT_CUES_REPLAYED;
        /// `remove_input` pads that got the flush pair. Every one briefly
        /// de-PLAYs the pipeline, so the count is the size of a problem that
        /// has NOT been solved.
        any remove_input_pairs_sent = crate::routing::REMOVE_INPUT_PAIRS_SENT;
        /// Replay flushing seeks actually handed off.
        any replay_seeks_sent = crate::external::REPLAY_SEEKS_SENT;
        /// `Job::ReplaySub` jobs that reached the worker queue.
        any replay_jobs_queued = crate::external::REPLAY_JOBS_QUEUED;
        /// Forwarded seeks an external input refused.
        any forward_seek_refusals = crate::external::FORWARD_SEEK_REFUSALS;
        /// Slot-seeding GAPs pushed for held external inputs.
        any slot_seed_pushes = crate::external::SLOT_SEED_PUSHES;
        /// Slot-seeding GAPs an external input refused.
        any slot_seed_refusals = crate::external::SLOT_SEED_REFUSALS;
    }
}

impl crate::FcastPlaybin {
    /// This pipeline's diagnostic counters, read together. Not part of the
    /// public API.
    #[doc(hidden)]
    pub fn stats(&self) -> Stats {
        Stats::take(&self.inner)
    }

    /// The crate's process-global diagnostic counters, read together.
    ///
    /// An associated function because the counters outlive any one pipeline
    /// (see [`GlobalStats`]), and cumulative for the same reason: take one
    /// before the gesture and subtract. Not part of the public API.
    #[doc(hidden)]
    pub fn global_stats() -> GlobalStats {
        GlobalStats::take(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_verdict_counter_reports_its_delta_and_an_any_one_does_not() {
        let before = GlobalStats::default();
        let mut after = GlobalStats::default();
        after.teardown_descent_stuck = 3;
        after.slot_unlatches = 9;

        let moved = after.moved_from_zero(&before);
        assert_eq!(moved, vec![("teardown_descent_stuck", 3)]);
    }

    #[test]
    fn a_flat_snapshot_pair_reports_nothing() {
        let snapshot = GlobalStats {
            teardown_descent_stuck: 4,
            slot_unlatch_failures: 2,
            ..GlobalStats::default()
        };
        assert!(snapshot.moved_from_zero(&snapshot).is_empty());
    }

    #[test]
    fn a_counter_that_went_backwards_is_not_a_violation() {
        // Cannot happen against real statics, but the subtraction must not
        // panic in a debug build if a caller pairs snapshots out of order.
        let before = GlobalStats {
            teardown_descent_stuck: 5,
            ..GlobalStats::default()
        };
        let after = GlobalStats::default();
        assert!(after.moved_from_zero(&before).is_empty());
    }
}
