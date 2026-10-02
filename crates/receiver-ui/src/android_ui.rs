//! The attached UI, one at a time. Core-side code (sink probes, the cue
//! engine, the overlay grant) reaches the window through this slot and never
//! holds a `Weak` of its own: a `Weak` must not outlive its UI thread.

use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use crate::MainWindow;

struct Attached {
    generation: u64,
    ui: slint::Weak<MainWindow>,
    app: slint::android::AndroidApp,
}

static CURRENT: Mutex<Option<Attached>> = Mutex::new(None);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Makes `ui`, shown by `app`'s activity, the attached UI and returns its
/// generation.
pub(crate) fn attach(ui: &MainWindow, app: &slint::android::AndroidApp) -> u64 {
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    *CURRENT.lock().unwrap() = Some(Attached {
        generation,
        ui: slint::ComponentHandle::as_weak(ui),
        app: app.clone(),
    });
    generation
}

/// Clears the slot if `generation` still holds it.
pub(crate) fn detach(generation: u64) {
    let mut current = CURRENT.lock().unwrap();
    if current.as_ref().is_some_and(|a| a.generation == generation) {
        *current = None;
    }
}

pub(crate) fn current() -> Option<slint::Weak<MainWindow>> {
    CURRENT.lock().unwrap().as_ref().map(|a| a.ui.clone())
}

pub(crate) fn generation() -> Option<u64> {
    CURRENT.lock().unwrap().as_ref().map(|a| a.generation)
}

/// The attached UI's activity.
pub(crate) fn app() -> Option<slint::android::AndroidApp> {
    CURRENT.lock().unwrap().as_ref().map(|a| a.app.clone())
}
