//! The overlay grant row in the settings drawer: whether a cast arriving in
//! the background may bring the receiver to the front, and the way to the
//! system page that grants it. The system holds the grant, not the config.
//! The activity pushes it on every window focus gain, which is how the user
//! comes back from that page.

use std::sync::{
    OnceLock,
    atomic::{AtomicU8, Ordering},
};

use slint::ComponentHandle;

use crate::{Bridge, MainWindow, android_immersive::with_activity};

static UI: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();

const GRANTED: u8 = 1;
const CAN_OPEN_SETTINGS: u8 = 2;
// Pushes can come before the UI exists; init applies the last one.
static STATE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn init(ui: &MainWindow) {
    let _ = UI.set(ui.as_weak());
    ui.global::<Bridge>().on_open_overlay_settings(|| {
        with_activity("openOverlaySettings", |env, activity| {
            env.call_method(activity, "openOverlaySettings", "()V", &[])
                .map(drop)
        });
    });
    apply(ui, STATE.load(Ordering::Acquire));
}

pub(crate) fn set_state(granted: bool, can_open_settings: bool) {
    let state = granted as u8 * GRANTED | can_open_settings as u8 * CAN_OPEN_SETTINGS;
    STATE.store(state, Ordering::Release);
    if let Some(ui) = UI.get() {
        let _ = ui.upgrade_in_event_loop(move |ui| apply(&ui, state));
    }
}

fn apply(ui: &MainWindow, state: u8) {
    let bridge = ui.global::<Bridge>();
    bridge.set_overlay_permission(state & GRANTED != 0);
    bridge.set_overlay_permission_settings(state & CAN_OPEN_SETTINGS != 0);
}
