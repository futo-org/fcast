//! What the receiver does about a stall flapjack could not repair.
//!
//! Detection is not here, and neither is the flushing seek. Both belong to
//! `flapjack::watchdog`, and for a reason that survives being restated: every
//! fact the decision needs is the driver's own bookkeeping. Settled at
//! PLAYING, buffering open, a seek in flight, an async transition running,
//! within a second of the duration, live, a still image, a load in flight --
//! a consumer that judges a parked playhead either mirrors all of it or
//! guesses, and this crate used to mirror all of it, down to the same
//! five-second window and the same one-second end margin.
//!
//! So the driver detects, and the driver spends its own rungs first: up to two
//! flushing seeks in place, each given a window to work in. `ErrorKind::Stalled`
//! is what it concludes when they did not take, and it says so once per wedge
//! rather than once per tick.
//!
//! What is left is the half flapjack's own docs hand back -- reload, skip, or
//! tell the user. This is that ladder: reload the item where it stopped, and
//! if it stalls again, say so. Capped per item, because a stream that wedges
//! every time must reach the error instead of reloading forever.

use crate::MediaItemId;

/// What to do about one `Stalled` report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallAction {
    /// Reload the item at the position it stopped on.
    Reload,
    /// This item has had its reload. Report a media error.
    GiveUp,
}

/// How far up the ladder this item has been taken. Per ITEM and not per
/// stall: the reload is itself a load and mints a new item id, so a ladder
/// that rearmed on the id it just caused would reload the same wedge forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Fresh,
    Reloaded,
}

#[derive(Debug)]
pub(crate) struct StallRecovery {
    /// Lever: `FCAST_NO_STALL_RECOVERY` (set = a stall is fatal on the spot).
    /// flapjack's detection and its flushing repairs are upstream of this and
    /// are not affected by it.
    enabled: bool,
    item: MediaItemId,
    stage: Stage,
}

impl StallRecovery {
    pub(crate) fn new() -> Self {
        Self {
            enabled: std::env::var_os("FCAST_NO_STALL_RECOVERY").is_none(),
            item: 0,
            stage: Stage::Fresh,
        }
    }

    /// Fold in one `ErrorKind::Stalled` for `item` and say what to do.
    pub(crate) fn on_stall(&mut self, item: MediaItemId) -> StallAction {
        if self.item != item {
            self.item = item;
            self.stage = Stage::Fresh;
        }
        if !self.enabled {
            return StallAction::GiveUp;
        }
        match self.stage {
            Stage::Fresh => {
                self.stage = Stage::Reloaded;
                StallAction::Reload
            }
            Stage::Reloaded => StallAction::GiveUp,
        }
    }

    /// Adopt the item id the recovery reload creates, WITHOUT rearming. The
    /// cap is what stops a permanently wedging stream from reloading on a
    /// loop, and the reload bumping the id is exactly the case it guards.
    pub(crate) fn note_recovery_reload(&mut self, item: MediaItemId) {
        self.item = item;
    }

    /// Whether this item has had its reload. Read at the give-up, where it is
    /// the difference between a wedge that survived a full rebuild and one the
    /// lever never let the receiver try.
    pub(crate) fn reload_spent(&self) -> bool {
        self.stage == Stage::Reloaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_stall_reloads_and_the_second_gives_up() {
        let mut recovery = StallRecovery::new();
        assert_eq!(recovery.on_stall(1), StallAction::Reload);
        assert_eq!(recovery.on_stall(1), StallAction::GiveUp);
        // And it stays given up for this item.
        assert_eq!(recovery.on_stall(1), StallAction::GiveUp);
    }

    /// The recovery reload bumps the item id; if that rearmed the ladder, a
    /// stream that wedges on every load would reload forever.
    #[test]
    fn a_recovery_reload_keeps_the_per_item_cap() {
        let mut recovery = StallRecovery::new();
        assert_eq!(recovery.on_stall(1), StallAction::Reload);
        recovery.note_recovery_reload(2);
        assert_eq!(recovery.on_stall(2), StallAction::GiveUp);
    }

    #[test]
    fn a_new_item_rearms_the_ladder() {
        let mut recovery = StallRecovery::new();
        assert_eq!(recovery.on_stall(1), StallAction::Reload);
        assert_eq!(recovery.on_stall(1), StallAction::GiveUp);
        assert_eq!(recovery.on_stall(7), StallAction::Reload);
    }

    /// A new item rearms even mid-ladder, and the rearm is the id changing
    /// rather than anything the caller has to say.
    #[test]
    fn an_item_change_rearms_before_the_cap_is_spent() {
        let mut recovery = StallRecovery::new();
        assert_eq!(recovery.on_stall(1), StallAction::Reload);
        assert_eq!(recovery.on_stall(2), StallAction::Reload);
    }

    #[test]
    fn the_lever_makes_a_stall_fatal_on_the_spot() {
        // The constructor reads the environment, so build a disabled one here.
        let mut recovery = StallRecovery {
            enabled: false,
            item: 0,
            stage: Stage::Fresh,
        };
        assert_eq!(recovery.on_stall(1), StallAction::GiveUp);
        // And the give-up can tell that apart from a spent reload.
        assert!(!recovery.reload_spent());
    }

    #[test]
    fn a_spent_reload_is_visible_at_the_give_up() {
        let mut recovery = StallRecovery::new();
        assert!(!recovery.reload_spent());
        assert_eq!(recovery.on_stall(1), StallAction::Reload);
        assert_eq!(recovery.on_stall(1), StallAction::GiveUp);
        assert!(recovery.reload_spent());
    }
}
