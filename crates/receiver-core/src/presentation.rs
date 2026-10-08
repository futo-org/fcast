//! What the screen shows across an item boundary, kept apart from the
//! playback state behind it. An end or a stop keeps the last item up for a
//! moment so a next item replaces it directly instead of passing through
//! the idle screen, and a load over an audio item loads behind that view.

use std::time::{Duration, Instant};

use crate::ui_types::{AppState, UiPlayerVariant};

/// How long an ended or stopped item stays up before the idle screen, long
/// enough for a sender's stop then load. Only the content changes after it.
pub const END_HOLD: Duration = Duration::from_millis(250);
/// The hold when the window hides or leaves fullscreen after it, where a next
/// item arriving late would bring the window back.
pub const RESTORE_HOLD: Duration = Duration::from_secs(1);
/// Bound on a hold the activity extended while it goes to the back, in case
/// the hidden edge never comes.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub const LEAVE_HOLD: Duration = Duration::from_secs(5);

/// The idle screen reset a hold defers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleReset {
    pub clear_playlist: bool,
}

impl IdleReset {
    fn merge(self, other: IdleReset) -> IdleReset {
        IdleReset {
            clear_playlist: self.clear_playlist || other.clear_playlist,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Hold {
    #[default]
    Off,
    Until { at: Instant, reset: IdleReset },
}

#[derive(Debug, Default)]
pub struct Presentation {
    hold: Hold,
    /// The activity will go to the back. An end can land after the signal
    /// (a stop after the last item's end), its hold lasts until hidden too.
    leaving: bool,
}

impl Presentation {
    /// An item ended or stopped. Returns the reset to apply now, which is
    /// at once when nobody can see the screen. `window_changes` when the
    /// reset hides the window or leaves fullscreen.
    pub fn end(
        &mut self,
        now: Instant,
        reset: IdleReset,
        ui_visible: bool,
        window_changes: bool,
    ) -> Option<IdleReset> {
        let hold = if self.leaving {
            LEAVE_HOLD
        } else if window_changes {
            RESTORE_HOLD
        } else {
            END_HOLD
        };
        let (at, reset) = match self.hold {
            Hold::Off => (now + hold, reset),
            Hold::Until { at, reset: held } => (at.max(now + hold), held.merge(reset)),
        };
        if !ui_visible {
            *self = Self::default();
            return Some(reset);
        }
        self.hold = Hold::Until { at, reset };
        None
    }

    /// The activity is moving to the back, the hold lasts until it is
    /// hidden. Returns the new deadline to arm.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn leaving(&mut self, now: Instant) -> Option<Instant> {
        self.leaving = true;
        let Hold::Until { at, reset } = self.hold else {
            return None;
        };
        let at = at.max(now + LEAVE_HOLD);
        self.hold = Hold::Until { at, reset };
        Some(at)
    }

    /// The UI went out of sight, a held reset applies now.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn hidden(&mut self) -> Option<IdleReset> {
        self.take()
    }

    /// A new item took over the screen (the activity cancels its leave on
    /// it), or the reset must apply now. Returns the dropped reset.
    pub fn take(&mut self) -> Option<IdleReset> {
        self.leaving = false;
        match std::mem::take(&mut self.hold) {
            Hold::Off => None,
            Hold::Until { reset, .. } => Some(reset),
        }
    }

    /// A timer fired. Early and stale wakes find nothing due.
    pub fn poll(&mut self, now: Instant) -> Option<IdleReset> {
        match self.hold {
            Hold::Until { at, .. } if now >= at => self.take(),
            _ => None,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        match self.hold {
            Hold::Off => None,
            Hold::Until { at, .. } => Some(at),
        }
    }
}

/// Whether a load keeps the audio view up instead of the loading screen:
/// the screen shows an audio item (playing, held after its end, or itself
/// loading behind) and the new item starts out as audio too.
pub fn load_behind(
    shown: AppState,
    shown_behind: bool,
    shown_variant: UiPlayerVariant,
    new_variant: UiPlayerVariant,
) -> bool {
    let audio_shown = match shown {
        AppState::Playing => true,
        AppState::LoadingMedia => shown_behind,
        AppState::Idle => false,
    };
    audio_shown && shown_variant == UiPlayerVariant::Audio && new_variant == UiPlayerVariant::Audio
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEP: IdleReset = IdleReset {
        clear_playlist: false,
    };
    const CLEAR: IdleReset = IdleReset {
        clear_playlist: true,
    };

    #[test]
    fn an_end_holds_then_resets_at_the_deadline() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        assert_eq!(p.end(t0, KEEP, true, false), None);
        assert_eq!(p.deadline(), Some(t0 + END_HOLD));
        assert_eq!(p.poll(t0 + END_HOLD / 2), None, "early wake");
        assert_eq!(p.poll(t0 + END_HOLD), Some(KEEP));
        assert_eq!(p.poll(t0 + END_HOLD * 2), None, "stale wake");
        assert_eq!(p.deadline(), None);
    }

    #[test]
    fn a_window_that_changes_holds_longer() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        assert_eq!(p.end(t0, KEEP, true, true), None);
        assert_eq!(p.deadline(), Some(t0 + RESTORE_HOLD));
        assert_eq!(p.poll(t0 + END_HOLD), None, "the short hold does not apply");
        assert_eq!(p.poll(t0 + RESTORE_HOLD), Some(KEEP));
    }

    #[test]
    fn a_short_end_after_a_long_one_never_shortens() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        p.end(t0, KEEP, true, true);
        p.end(t0, CLEAR, true, false);
        assert_eq!(p.deadline(), Some(t0 + RESTORE_HOLD));
    }

    #[test]
    fn a_hidden_ui_resets_at_once() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        assert_eq!(p.end(t0, CLEAR, false, false), Some(CLEAR));
        assert_eq!(p.deadline(), None);
    }

    #[test]
    fn a_load_during_the_hold_takes_the_reset_over() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        p.end(t0, CLEAR, true, false);
        assert_eq!(p.take(), Some(CLEAR), "the load applies what it must");
        assert_eq!(p.poll(t0 + END_HOLD), None, "and the timer finds nothing");
    }

    #[test]
    fn a_stop_after_an_end_merges_and_never_shortens() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        p.end(t0, KEEP, true, false);
        let t1 = t0 + END_HOLD / 2;
        assert_eq!(p.end(t1, CLEAR, true, false), None);
        assert_eq!(p.deadline(), Some(t1 + END_HOLD));
        assert_eq!(p.poll(t1 + END_HOLD), Some(CLEAR));
    }

    #[test]
    fn leaving_holds_until_hidden() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        assert_eq!(p.leaving(t0), None, "nothing held, nothing to extend");
        p.end(t0, KEEP, true, false);
        assert_eq!(p.leaving(t0), Some(t0 + LEAVE_HOLD));
        assert_eq!(p.poll(t0 + END_HOLD), None, "the plain hold no longer applies");
        assert_eq!(p.hidden(), Some(KEEP));
        assert_eq!(p.hidden(), None);
    }

    #[test]
    fn leaving_before_the_end_carries_into_its_hold() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        assert_eq!(p.leaving(t0), None);
        assert_eq!(p.end(t0, CLEAR, true, false), None);
        assert_eq!(p.deadline(), Some(t0 + LEAVE_HOLD));
        assert_eq!(p.hidden(), Some(CLEAR));
        // spent: the next end holds the plain time
        p.end(t0, KEEP, true, false);
        assert_eq!(p.deadline(), Some(t0 + END_HOLD));
    }

    #[test]
    fn a_new_item_cancels_a_pending_leave() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        p.leaving(t0);
        assert_eq!(p.take(), None);
        p.end(t0, KEEP, true, false);
        assert_eq!(p.deadline(), Some(t0 + END_HOLD));
    }

    #[test]
    fn leaving_is_bounded() {
        let t0 = Instant::now();
        let mut p = Presentation::default();
        p.end(t0, KEEP, true, false);
        p.leaving(t0);
        assert_eq!(p.poll(t0 + LEAVE_HOLD), Some(KEEP));
    }

    #[test]
    fn only_audio_over_audio_loads_behind() {
        use AppState::*;
        use UiPlayerVariant::*;
        assert!(load_behind(Playing, false, Audio, Audio));
        assert!(load_behind(LoadingMedia, true, Audio, Audio), "a skip while loading behind");
        assert!(!load_behind(LoadingMedia, false, Audio, Audio), "the loading screen is up");
        assert!(!load_behind(Idle, false, Audio, Audio));
        assert!(!load_behind(Playing, false, Video, Audio));
        assert!(!load_behind(Playing, false, Audio, Image));
        assert!(!load_behind(Playing, false, Image, Image));
    }
}
