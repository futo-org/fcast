//! The display between two modes. Refresh-rate matching switches the HDMI
//! mode back when a cast ends, and a TV shows nothing for a second or more
//! while it locks onto the new one, which is exactly when the idle screen
//! comes up. The activity says for how long to hold what should be seen
//! arriving, and the idle contents wait that out before they fade in.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::{Bridge, MainWindow};

// The latest end asked for. A shorter wait asked for later ends nothing early.
static UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// The display is unsettled for another `wait`, from the activity.
pub(crate) fn settling(wait: Duration) {
    let until = Instant::now() + wait;
    {
        let mut latest = UNTIL.lock().unwrap();
        if latest.is_some_and(|known| known >= until) {
            return;
        }
        *latest = Some(until);
    }
    if let Some(ui) = crate::android_ui::current() {
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.global::<Bridge>().set_display_settling(true);
            settle_at(&ui, until);
        });
    }
}

/// Clears the flag at `until`, unless a later end was asked for meanwhile,
/// whose own timer does it then.
fn settle_at(ui: &MainWindow, until: Instant) {
    let weak = ui.as_weak();
    let wait = until.saturating_duration_since(Instant::now());
    slint::Timer::single_shot(wait, move || {
        let mut latest = UNTIL.lock().unwrap();
        if *latest != Some(until) {
            return;
        }
        *latest = None;
        if let Some(ui) = weak.upgrade() {
            ui.global::<Bridge>().set_display_settling(false);
        }
    });
}
