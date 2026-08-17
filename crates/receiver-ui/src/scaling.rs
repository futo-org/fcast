//! Applies the [`receiver_core::ui_scaling`] policy to the live window.
//!
//! The GUI is zoomed through the window's scale factor, so the .slint sources
//! stay authored in fixed `px` against one logical canvas and every resolution
//! renders the same layout. Slint's factor is runtime-writable, and the window
//! keeps its physical size across a change, so a rescale is: dispatch the new
//! factor, then restate the logical size it implies.
//!
//! Nothing here runs in desktop mode beyond the observation pass: the resolved
//! factor is then whatever the platform already reported.

use std::{cell::Cell, rc::Rc};

use receiver_core::ui_scaling::{self, Surface, UiScale};
use slint::ComponentHandle;
use tracing::{info, warn};

use crate::{Bridge, MainWindow};

/// Factors this close together are the same factor.
const EPSILON: f32 = 0.001;

/// `applied` before the first resolve, distinguishable from any real factor.
const UNSET: f32 = 0.0;

/// Owns the applied scale factor and re-derives it on every geometry change.
pub struct UiScaler {
    ui: slint::Weak<MainWindow>,
    mode: Cell<UiScale>,
    /// The factor the windowing system last reported, i.e. what the scale
    /// factor would be if we never touched it.
    system: Cell<f32>,
    /// What we last resolved, so a platform write is distinguishable from ours.
    applied: Cell<f32>,
    /// Coalesces the deferred re-apply; geometry changes arrive per axis.
    pending: Cell<bool>,
    /// Probed panel size, keyed by the resolution it was probed for. EDID
    /// reads hit sysfs, so they happen per resolution and not per resize.
    panel: Cell<Option<((u32, u32), Option<(u32, u32)>)>>,
}

/// Wire the scaler into `ui`: it re-derives the scale factor whenever the
/// window geometry changes.
pub fn install(ui: &MainWindow, mode: UiScale, forced_by_cli: bool) -> Rc<UiScaler> {
    let scaler = Rc::new(UiScaler {
        ui: ui.as_weak(),
        mode: Cell::new(mode),
        system: Cell::new(1.0),
        applied: Cell::new(UNSET),
        pending: Cell::new(false),
        panel: Cell::new(None),
    });

    let bridge = ui.global::<Bridge>();
    bridge.on_window_geometry_changed({
        let scaler = Rc::clone(&scaler);
        move || scaler.sync()
    });
    // Picking a new size in the settings drawer applies now, not on restart.
    // The drawer mirrors the config file, so with --ui-scale passed this would
    // otherwise undo the flag as soon as the mirror is seeded.
    bridge.on_ui_scale_changed({
        let scaler = Rc::clone(&scaler);
        move |value: slint::SharedString| {
            if forced_by_cli {
                info!(%value, "Ignoring the configured ui_scale, --ui-scale wins");
                return;
            }
            let mode = UiScale::parse(&value).unwrap_or_else(|| {
                warn!(%value, "Unrecognised ui_scale from the settings drawer");
                UiScale::Auto
            });
            info!(?mode, "UI scaling mode changed");
            scaler.mode.set(mode);
            scaler.sync();
        }
    });

    info!(?mode, "UI scaling installed");
    scaler
}

impl UiScaler {
    /// Recompute and apply, coalesced onto the event loop.
    ///
    /// Geometry changes arrive from inside Slint's layout pass, one per axis;
    /// dispatching a window event from there would re-enter the layout.
    pub fn sync(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let this = Rc::clone(self);
        // A zero-delay timer, not `invoke_from_event_loop`: this stays on the
        // event loop thread, so the scaler never has to be Send.
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            this.pending.set(false);
            this.apply();
        });
    }

    fn apply(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let window = ui.window();
        let size = window.size();
        if size.width == 0 || size.height == 0 {
            return;
        }

        let current = window.scale_factor();
        // We and the platform are the only writers of the factor, so one we
        // did not resolve is a fresh DPI report to fold into the policy. This
        // is also how the first real factor arrives.
        let applied = self.applied.get();
        if applied == UNSET || (current - applied).abs() > EPSILON {
            self.system.set(current);
        }

        let fullscreen = window.is_fullscreen();
        let surface = Surface {
            width: size.width,
            height: size.height,
            system_scale: self.system.get(),
            // Only a fullscreen window's size is the output's resolution.
            size_mm: self.panel_size_mm(fullscreen.then_some((size.width, size.height))),
            fullscreen,
        };

        let desired = ui_scaling::resolve(self.mode.get(), &surface);
        self.applied.set(desired);
        if (desired - current).abs() <= EPSILON {
            return;
        }

        info!(
            from = current,
            to = desired,
            width = size.width,
            height = size.height,
            system = surface.system_scale,
            panel_mm = ?surface.size_mm,
            "Rescaling the GUI"
        );

        if let Err(err) =
            window.try_dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                scale_factor: desired,
            })
        {
            warn!(?err, "Scale factor dispatch failed");
            return;
        }

        // The window keeps its physical size across the change, so the logical
        // size has to be restated or the layout keeps the old canvas.
        let logical =
            slint::LogicalSize::new(size.width as f32 / desired, size.height as f32 / desired);
        if let Err(err) =
            window.try_dispatch_event(slint::platform::WindowEvent::Resized { size: logical })
        {
            warn!(?err, "Resize dispatch after a rescale failed");
        }
        window.request_redraw();
    }

    fn panel_size_mm(&self, resolution: Option<(u32, u32)>) -> Option<(u32, u32)> {
        let key = resolution.unwrap_or((0, 0));
        if let Some((probed_for, size_mm)) = self.panel.get() {
            if probed_for == key {
                return size_mm;
            }
        }
        let size_mm = ui_scaling::probe_panel_size_mm(resolution);
        self.panel.set(Some((key, size_mm)));
        size_mm
    }
}
