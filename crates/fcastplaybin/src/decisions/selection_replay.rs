//! The selection-time external replay rule: what a `STREAMS_SELECTED` that
//! moves onto an external subtitle owes that input.
//!
//! The pad-reuse counterpart of the join-time replay. Switching between text
//! streams makes decodebin3 swap the stream on the already-linked output pad,
//! so no join fires and the join-time replay never runs. This rule is the only
//! thing that restarts a dead external, re-times a mis-aligned one and
//! re-pushes one whose multiqueue slot was reclaimed, at the one moment the
//! switch is known.
//!
//! The caller answers two questions before asking this one, both needing locks
//! this module must not take: that the selection MOVED (a same-sid re-assertion
//! is skipped so a redundant `SELECT_STREAMS` cannot blink the current cue) and
//! that the selected sid belongs to an attached external input. Everything the
//! rule itself reads is here, so each hazard is one row of a table instead of a
//! provoked live pipeline. See `Inner::selection_time_replay` for the
//! mechanism, the in-flight claim and the owed-hold protocol built on top.

/// What a selection that moved onto an external subtitle owes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionReplay {
    /// Flush-replay the input now. Destructive on purpose: what it would
    /// deliver next is wrong, so it must not reach the screen first.
    Replay,
    /// Nothing is wrong that is knowable yet. Arm the replay verification,
    /// which replays only if nothing arrives.
    ArmVerification,
}

/// Decide the selection-time replay from the three things that can make a
/// just-selected external unable to deliver correctly on its own.
///
/// * `task_dead`, its source task stopped while deselected (a transport race,
///   see `decisions::ExternalErrorAction::Recover`). A dead task pushes nothing
///   ever again; only the replay seek restarts it.
/// * `origin != last_origin`, the video timeline moved since this input last
///   aligned to it (`origin` is the position running time is measured from, see
///   `Inner::video_timeline`; `last_origin` is written per-input after its own
///   successful send). Its cues would render shifted.
/// * `!branch_live`, every text branch is parked, so there is no pad swap to
///   wait on and no join to replay from, and a drained external whose
///   multiqueue slot was reclaimed has no pad carrying its sid at all. Only a
///   re-push brings it back.
///
/// Any one of them replays; the reasons are independent and each alone is
/// enough. Otherwise the input is alive, aligned and sitting behind a live
/// branch: it delivers on its own, and a flush now would race the very pad swap
/// this selection started. The verification covers the case where it turns out
/// not to, by replaying only if nothing arrives.
pub(crate) fn selection_replay_action(
    task_dead: bool,
    origin: gst::ClockTime,
    last_origin: gst::ClockTime,
    branch_live: bool,
) -> SelectionReplay {
    if task_dead || origin != last_origin || !branch_live {
        SelectionReplay::Replay
    } else {
        SelectionReplay::ArmVerification
    }
}

#[cfg(test)]
mod tests {
    use super::{SelectionReplay::*, *};

    const T0: gst::ClockTime = gst::ClockTime::ZERO;
    const T5: gst::ClockTime = gst::ClockTime::from_seconds(5);

    /// One row per reason, per combination, and per documented hazard.
    /// (task_dead, origin, last_origin, branch_live) -> action.
    #[test]
    fn selection_replay_table() {
        let rows: &[(
            bool,
            gst::ClockTime,
            gst::ClockTime,
            bool,
            SelectionReplay,
            &str,
        )] = &[
            // The one arm that does NOT replay: alive, aligned, live branch.
            (
                false,
                T5,
                T5,
                true,
                ArmVerification,
                "an aligned live input behind a live branch delivers on its own",
            ),
            // Fresh attach at the head of an unsought item: both origins ZERO
            // is alignment, not a missing one.
            (
                false,
                T0,
                T0,
                true,
                ArmVerification,
                "zero against zero is aligned, not unaligned",
            ),
            // Reason 1: the task died deselected.
            (
                true,
                T5,
                T5,
                true,
                Replay,
                "a dead task never pushes again, however aligned it looks",
            ),
            // Reason 2, both directions: the comparison is equality, not
            // "moved forward". A seek back to the start moves the origin too.
            (
                false,
                T5,
                T0,
                true,
                Replay,
                "the timeline moved away from what this input aligned to",
            ),
            (
                false,
                T0,
                T5,
                true,
                Replay,
                "a seek back to the start is a realignment as much as one forward",
            ),
            // Reason 3: nothing is linked, so no join and no pad swap will
            // ever fire for this input.
            (
                false,
                T5,
                T5,
                false,
                Replay,
                "a parked or slotless external needs the re-push",
            ),
            // The reasons are independent: a parked branch does not wait for a
            // realignment and a realignment does not wait for a dead task.
            (true, T5, T5, false, Replay, "dead and parked"),
            (false, T0, T5, false, Replay, "realigned and parked"),
            (true, T0, T5, true, Replay, "dead and realigned"),
            (
                true,
                T0,
                T5,
                false,
                Replay,
                "every reason at once still replays exactly once",
            ),
        ];
        for &(task_dead, origin, last_origin, branch_live, want, why) in rows {
            assert_eq!(
                selection_replay_action(task_dead, origin, last_origin, branch_live),
                want,
                "{why}"
            );
        }
    }
}
