//! Zero-copy android video: the slint backend's [`VideoSurface`] sits
//! behind the translucent window and MediaCodec renders into it through
//! flapjack's direct mode. Slint never sees a frame; this module owns the
//! geometry (letterboxed from the decoder's caps and the window size) and
//! the surface handoff. `FCAST_ANDROID_SW_VIDEO=1` selects the old
//! software bridge instead (android_video.rs).
//!
//! SURFACE LIFECYCLE PROTOCOL, every rule below exists because breaking it
//! produced a field bug:
//!
//! * A FRESH SURFACE PER ITEM. When an item's caps drop, or the receiver goes
//!   idle, the view is hidden, which destroys the surface. A surface kept
//!   across items (parked at 1x1) carries the previous item's last frame
//!   into the next: it showed stretched under the loading screen and again
//!   at the new rect until the new codec rendered. Parking existed because a
//!   destroy used to race the polled handoff; the handoff now follows the
//!   view's surface events, so a codec on a dying surface is taken off it in
//!   time and the new surface is always handed.
//! * The scene fills the video hole (`video-frame-pending`) from an item
//!   boundary until that item's first buffer reaches the sink. An empty new
//!   surface is not enough on Amlogic TV boxes: their video plane keeps the
//!   previous item's last frame below it, visible through the hole for as
//!   long as the next source takes to start (a UMP backoff, say).
//! * A show must not share a layout pass with the hide before it, or the old
//!   surface survives. `destroying_seq` marks a destroy in flight, and pre-open
//!   and relayout wait for its `Destroyed` event, which resumes them.
//! * Hand the window before the codec builds. A codec that starts windowless
//!   rebuilds when the window arrives and then drops every frame until the
//!   stream's next keyframe (seconds of frozen video). `preopen_current` runs
//!   at LoadingMedia for that; `preopen_pending` re-fires it once the old
//!   item's surface is destroyed, and the decoder is promised a window
//!   meanwhile.
//! * The item boundary (`item_boundary`) runs on the core thread from the
//!   core's item hook, right before every pipeline load, never from the UI
//!   thread's LoadingMedia command. The UI thread can lag the pipeline: a
//!   small still reaches its caps and its only buffer first, and a boundary
//!   landing after them wiped the new item's size and left the hole filled
//!   for good (black screen). Not inferred from app states either: a load
//!   over a still-loading item passes no state edge.
//! * Clear the player's window BEFORE the surface dies, never after. Both the
//!   caps-drop path and `app_visibility(false)` do set_video_window null first;
//!   the generation bump it causes is also what lets a codec that already
//!   grabbed the dying window recover instead of erroring.
//! * The view reports its surface's life (`SurfaceEvent`) on the android UI
//!   thread. A creation is handed to the player at once; a destruction takes
//!   the window off the player before it returns, which is before the
//!   surface dies. The seq identifies which surface the player holds, and a
//!   parked surface (still alive, taken off the player at caps drop) is handed
//!   again from the kept reference, since no new creation comes for it.
//! * All geometry runs on the slint UI thread; a 0x0 window means the activity
//!   is stopped and the relayout retries on a timer, because a resume to the
//!   pre-stop size emits no resize event.

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

use gst::prelude::*;
use i_slint_backend_android_activity::{SurfaceEvent, VideoSurface};
use slint::ComponentHandle;
use tracing::{info, warn};

/// What the sink pad has said about the current item. Process lifetime, the
/// sink outlives any UI.
#[derive(Default)]
struct Sink {
    video_size: Mutex<(u32, u32)>,
    /// Display rotation from the stream's image-orientation tag, degrees.
    /// 90/270 swap the fitted rect's aspect: the codec (direct) or the
    /// window transform (raw path) rotate the pixels, and this is the
    /// layout's half of the same fact.
    rotation: AtomicI32,
    /// Mirrors `video-frame-pending`: set at an item boundary, cleared by the
    /// item's first buffer at the sink.
    frame_pending: AtomicBool,
}

/// The view behind one attached UI, and its surface handoff.
pub struct SurfaceVideo {
    surface: VideoSurface,
    handoff: Mutex<Handoff>,
    ui: slint::Weak<crate::MainWindow>,
}

static SINK: OnceLock<Sink> = OnceLock::new();
// The attached UI's view, swapped per attach. Never hold the lock across a
// call that can reach back in here.
static VIEW: Mutex<Option<Arc<SurfaceVideo>>> = Mutex::new(None);

fn sink() -> &'static Sink {
    SINK.get_or_init(Sink::default)
}

fn view() -> Option<Arc<SurfaceVideo>> {
    VIEW.lock().unwrap().clone()
}

/// Activity start/stop. The SurfaceView's surface dies with the activity;
/// clearing the player's window first bumps the surface generation, so a
/// codec mid-frame recovers through the swap path instead of erroring on
/// the dead surface. On return the relayout adopts the fresh surface (its
/// zero-size retry covers a window that is not up yet).
pub(crate) fn app_visibility(visible: bool) {
    let Some(this) = view() else {
        crate::set_app_visible(visible);
        return;
    };
    // Back with video loaded: the view's surface is on its way, so the
    // decoder waits for it instead of building a headless codec first.
    if visible && sink().video_size.lock().unwrap().0 != 0 {
        crate::set_video_window_pending(true);
    }
    // The decode throttle keys on this globally, before any surface exists.
    crate::set_app_visible(visible);
    if visible {
        this.relayout();
    } else {
        crate::set_video_window(std::ptr::null_mut());
        this.handoff.lock().unwrap().handed_seq = 0;
    }
}

/// Bottom system inset over the content frame, 0 while immersive or before
/// the surface exists. Subtitles keep clear of it.
pub(crate) fn safe_bottom_inset() -> u32 {
    match view() {
        Some(this) => this.surface.safe_bottom().max(0) as u32,
        None => 0,
    }
}

/// The content frame's origin inside the window. Slint overlays draw from
/// the window origin, the video view is positioned in the content frame;
/// content-space coordinates need this shift to land on the picture.
pub(crate) fn content_offset_in_window() -> (i32, i32) {
    match view() {
        Some(this) => this.surface.content_offset(),
        None => (0, 0),
    }
}

/// The space every video-anchored overlay must share with the SurfaceView:
/// the content frame's size when it is laid out, the slint window as the
/// pre-layout fallback. The slint size alone is wrong, see `relayout`.
pub(crate) fn effective_canvas(ui: &crate::MainWindow) -> (u32, u32) {
    if let Some(this) = view() {
        let (cw, ch) = this.surface.content_size();
        if cw > 0 && ch > 0 {
            return (cw as u32, ch as u32);
        }
    }
    let size = ui.window().size();
    (size.width, size.height)
}

/// Idle, nothing loading: drop any pending pre-open and zero the surface,
/// so an empty punch can never outlive the video and cull the UI.
pub(crate) fn park_current() {
    let Some(this) = view() else {
        return;
    };
    this.handoff.lock().unwrap().preopen_pending = false;
    // nothing is coming, a decoder must not wait for it
    crate::set_video_window_pending(false);
    this.retire_surface();
}

/// A retired surface is gone: whatever waited on it (the next item's caps, or
/// a pre-open) may show the view again now.
fn resume_after_retire() {
    let Some(this) = view() else {
        return;
    };
    if sink().video_size.lock().unwrap().0 != 0 {
        this.relayout();
    } else if this.handoff.lock().unwrap().preopen_pending {
        preopen_current();
    }
}

/// A new item: retire whatever the surface still shows. The previous item's
/// caps do not always drop at the boundary (a branch kept for the next
/// source, which can take a while to start, a UMP backoff for one), and its
/// last frame would sit under the loading screen the whole time. The core's
/// item hook, on the core thread right before the pipeline load, so ahead of
/// the new item's caps (see the lifecycle notes).
pub(crate) fn item_boundary() {
    set_frame_pending(true);
    *sink().video_size.lock().unwrap() = (0, 0);
    sink().rotation.store(0, Ordering::Relaxed);
    let Some(this) = view() else {
        return;
    };
    this.handoff.lock().unwrap().retired_at_boundary = true;
    this.retire_surface();
    // the load's surface comes after the destroy, its decoder waits
    crate::set_video_window_pending(true);
}

/// Shows the surface full-window and hands its window to the player ahead
/// of a load, so the first codec already builds in direct mode.
pub(crate) fn preopen_current() {
    let Some(this) = view() else {
        return;
    };
    if sink().video_size.lock().unwrap().0 != 0 {
        // the item's caps beat the UI here and drove a real layout
        return;
    }
    this.handoff.lock().unwrap().preopen_pending = true;
    let ui = this.ui.clone();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        // same space note as in relayout
        let win = {
            let (cw, ch) = this.surface.content_size();
            if cw > 0 && ch > 0 {
                slint::PhysicalSize::new(cw as u32, ch as u32)
            } else {
                ui.window().size()
            }
        };
        if win.width == 0 || win.height == 0 {
            // backgrounded; the caps-time relayout retry recovers later
            return;
        }
        if sink().video_size.lock().unwrap().0 != 0 {
            // caps already drove a real layout, nothing to pre-open
            return;
        }
        {
            // check and show under the lock, see `retire_surface`
            let mut st = this.handoff.lock().unwrap();
            if st.destroying_seq != 0 {
                // the old surface's destruction re-fires this
                return;
            }
            let _ = this
                .surface
                .set_rect(0, 0, win.width as i32, win.height as i32);
            let _ = this.surface.set_visible(true);
        }
        this.ensure_window_handoff();
    });
}

/// Mirrors `video-frame-pending` into the attached UI, on change only.
fn set_frame_pending(pending: bool) {
    if sink().frame_pending.swap(pending, Ordering::Relaxed) == pending {
        return;
    }
    if let Some(this) = view() {
        let _ = this.ui.upgrade_in_event_loop(move |ui| {
            ui.global::<crate::Bridge>().set_video_frame_pending(pending);
        });
    }
}

pub(crate) fn relayout_current() {
    if let Some(this) = view() {
        this.relayout();
    }
}

/// The attached UI went away with its view. The player already let go of its
/// window at activity stop (`app_visibility`).
pub(crate) fn detach_view() {
    VIEW.lock().unwrap().take();
}

/// A window reference we own, released on drop.
struct WindowRef(std::ptr::NonNull<std::ffi::c_void>);

// ANativeWindow reference counting is thread-safe.
unsafe impl Send for WindowRef {}

impl Drop for WindowRef {
    fn drop(&mut self) {
        unsafe { ndk_sys::ANativeWindow_release(self.0.as_ptr().cast()) };
    }
}

/// The surface handoff state, fed by the view's surface events.
#[derive(Default)]
struct Handoff {
    // the surface the platform has right now, with its seq
    live: Option<(WindowRef, i32)>,
    // seq of the surface flapjack holds, 0 for none
    handed_seq: i32,
    // seq of a surface we hid and whose destruction is still coming, 0 for none
    destroying_seq: i32,
    // a new item retired the surface before the old item's caps dropped, so
    // that drop, when it comes, is not the new item's to act on
    retired_at_boundary: bool,
    relayout_queued: bool,
    // a load wants the surface up before its codec builds
    preopen_pending: bool,
    // bumped per window promise, so a give-up timer only drops its own
    promise_gen: u32,
}

impl Handoff {
    fn hand_live(&mut self) {
        if let Some((window, seq)) = &self.live
            && self.handed_seq != *seq
        {
            info!(seq, "video surface live, handing it to the player");
            // takes its own reference, and fulfils the pending promise
            crate::set_video_window(window.0.as_ptr().cast());
            self.handed_seq = *seq;
        }
    }
}

/// How long a view that is up may go without a surface before the promise to
/// the decoder is dropped and it builds headless.
const SURFACE_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) use crate::video_math::letterbox;

/// The player's video sink, once per process. Its probes reach whichever view
/// is attached, and keep the item state while none is.
pub fn make_sink() -> Option<gst::Element> {
    let sink_el = match gst::ElementFactory::make("amcsurfacesink").build() {
        Ok(sink) => sink,
        Err(err) => {
            warn!(?err, "no amcsurfacesink, falling back to software video");
            return None;
        }
    };
    // the decoder's negotiated caps carry the display size, geometry
    // follows them
    let Some(pad) = sink_el.static_pad("sink") else {
        warn!("amcsurfacesink without a sink pad?");
        return None;
    };
    info!("surface video: sink ready, waiting for caps");
    // The first buffer after a boundary reaches the sink just before it is
    // presented (preroll shows it at once, a playing sink is at most a
    // frame ahead), which is when the hole may open again.
    pad.add_probe(gst::PadProbeType::BUFFER, |_pad, info| {
        // the decoder's hidden throttle sends empty GAP buffers that
        // only keep time, nothing lands on the surface for them
        let gap = matches!(&info.data, Some(gst::PadProbeData::Buffer(b))
            if b.flags().contains(gst::BufferFlags::GAP));
        if !gap && sink().frame_pending.load(Ordering::Relaxed) {
            set_frame_pending(false);
        }
        gst::PadProbeReturn::Ok
    });
    // The display rotation arrives as a TAG, not in caps, on the same
    // pad. 90/270 swap the fitted aspect below.
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_pad, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data {
            if let gst::EventView::Tag(tag) = event.view() {
                if let Some(o) = tag.tag().index::<gst::tags::ImageOrientation>(0) {
                    let degrees = match o.get() {
                        "rotate-90" => 90,
                        "rotate-180" => 180,
                        "rotate-270" => 270,
                        _ => 0,
                    };
                    let prev = sink().rotation.swap(degrees, Ordering::Relaxed);
                    if prev != degrees {
                        info!(degrees, "surface video: display rotation");
                        relayout_current();
                    }
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    pad.connect_notify(Some("caps"), |pad, _| {
        let caps = pad.current_caps();
        info!(?caps, "surface video: sink caps notify");
        let view = view();
        let Some(caps) = caps else {
            if let Some(this) = &view
                && std::mem::take(&mut this.handoff.lock().unwrap().retired_at_boundary)
            {
                // the new item already retired this item's surface, and
                // the window the player holds may be the new one
                return;
            }
            // Caps drop when the item unlinks. Take the window away
            // from the player first, the next codec must not configure
            // against a surface that is about to die.
            crate::set_video_window(std::ptr::null_mut());
            let load_waiting = view.as_ref().is_some_and(|this| {
                let mut st = this.handoff.lock().unwrap();
                st.handed_seq = 0;
                st.preopen_pending
            });
            // also ends a zero-size relayout retry chain
            *sink().video_size.lock().unwrap() = (0, 0);
            // the next item is unrotated until its own tag arrives;
            // this state is process-wide and would stick otherwise
            sink().rotation.store(0, Ordering::Relaxed);
            if load_waiting {
                // a load is on its way and its surface comes after the
                // destroy, the decoder waits for it instead of going
                // headless
                crate::set_video_window_pending(true);
            }
            set_frame_pending(true);
            info!("surface video: item ended, retiring its surface");
            if let Some(this) = &view {
                this.retire_surface();
            }
            return;
        };
        let Some(s) = caps.structure(0) else { return };
        let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else {
            return;
        };
        if w > 0 && h > 0 {
            if let Some(this) = &view {
                let mut st = this.handoff.lock().unwrap();
                st.preopen_pending = false;
                // a new item's caps: a later drop is its own
                st.retired_at_boundary = false;
            }
            *sink().video_size.lock().unwrap() = (w as u32, h as u32);
            if let Some(this) = &view {
                this.relayout();
            }
        }
    });
    Some(sink_el)
}

impl SurfaceVideo {
    /// Creates the surface behind this UI's window and makes it the attached
    /// view. `None` when the backend has no video surface.
    pub fn attach(ui: &crate::MainWindow, app: &slint::android::AndroidApp) -> Option<Arc<Self>> {
        info!("surface video: creating the backing surface view");
        let surface = match VideoSurface::new(app) {
            Ok(surface) => surface,
            Err(err) => {
                warn!(?err, "no video surface");
                return None;
            }
        };
        let this = Arc::new(Self {
            surface,
            handoff: Mutex::new(Handoff::default()),
            ui: ui.as_weak(),
        });
        // Before the view is ever shown: the fork does not replay a surface
        // that already exists.
        this.surface.set_surface_handler({
            let this = Arc::downgrade(&this);
            move |event| {
                if let Some(this) = this.upgrade() {
                    this.on_surface(event);
                }
            }
        });
        *VIEW.lock().unwrap() = Some(this.clone());
        // catch the new UI up on the item the sink already knows
        let pending = sink().frame_pending.load(Ordering::Relaxed);
        ui.global::<crate::Bridge>().set_video_frame_pending(pending);
        if sink().video_size.lock().unwrap().0 != 0 {
            this.relayout();
        }
        Some(this)
    }

    /// Re-fit the surface to the window; runs the math on the ui thread
    /// where the window size lives.
    pub fn relayout(self: &Arc<Self>) {
        let this = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            // The view is margin-positioned inside android.R.id.content, so
            // the letterbox must fit THAT frame. The slint window size is
            // the window surface, which carries shadow insets (a 1440x2960
            // screen reports 1520x3040) and made every rect oversized and
            // off-center. Fall back to the window only before the content
            // frame's first layout.
            let win = {
                let (cw, ch) = this.surface.content_size();
                if cw > 0 && ch > 0 {
                    slint::PhysicalSize::new(cw as u32, ch as u32)
                } else {
                    ui.window().size()
                }
            };
            let (vw, vh) = *sink().video_size.lock().unwrap();
            if vw == 0 {
                return;
            }
            // A quarter-turn rotation shows the picture with its sides
            // swapped; the pixels are rotated by the codec or the window
            // transform, the fit must follow.
            let (vw, vh) = match sink().rotation.load(Ordering::Relaxed) {
                90 | 270 => (vh, vw),
                _ => (vw, vh),
            };
            if win.width == 0 || win.height == 0 {
                // The activity is stopped (a cast can land while the app is
                // backgrounded) and there is no window to size against. A
                // resume to the pre-stop size emits no resize event, so
                // poll until the window is real again.
                let retry = {
                    let mut st = this.handoff.lock().unwrap();
                    !std::mem::replace(&mut st.relayout_queued, true)
                };
                if retry {
                    let this = this.clone();
                    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
                        this.handoff.lock().unwrap().relayout_queued = false;
                        this.relayout();
                    });
                }
                return;
            }
            let (x, y, w, h) = letterbox(vw, vh, win.width, win.height);
            {
                // check and show under the lock, see `retire_surface`
                let mut st = this.handoff.lock().unwrap();
                if st.destroying_seq != 0 {
                    // the old surface's destruction re-runs this
                    return;
                }
                info!(vw, vh, x, y, w, h, "surface video: rect");
                crate::android_immersive::set_video_aspect(vw, vh);
                let _ = this.surface.set_rect(x, y, w, h);
                let _ = this.surface.set_visible(true);
            }
            // The hole only exists in a frame slint actually painted after
            // the player view took over; nothing else changes here, so ask
            // for one instead of waiting for the next input
            ui.window().request_redraw();
            this.ensure_window_handoff();
        });
    }

    /// Hides the view, which destroys its surface (see the lifecycle notes).
    /// Callers span threads (core, streaming, slint), so the state change
    /// and the hide are posted under the lock that relayout and pre-open
    /// check `destroying_seq` and post their show under: a show checked
    /// before this hide can never be posted after it, which would share its
    /// layout pass and keep the old surface. The posts only queue on the
    /// android UI thread, none of these run on it.
    fn retire_surface(&self) {
        let mut st = self.handoff.lock().unwrap();
        if st.handed_seq != 0 {
            crate::set_video_window(std::ptr::null_mut());
            st.handed_seq = 0;
        }
        // Its frames belong to the leaving item: forgotten now, not when
        // the async hide lands. A destroy only follows for a live surface.
        if let Some((_, seq)) = st.live.take() {
            st.destroying_seq = seq;
        }
        let _ = self.surface.set_visible(false);
    }

    /// The view's surface events, on the android UI thread.
    fn on_surface(&self, event: SurfaceEvent) {
        let mut resume = false;
        let mut st = self.handoff.lock().unwrap();
        match event {
            SurfaceEvent::Created { window, seq } => {
                // the view is only up for video, a new surface goes straight on
                st.live = Some((WindowRef(window), seq));
                st.hand_live();
            }
            SurfaceEvent::Destroyed { seq } => {
                if st.handed_seq == seq {
                    // off the player before the surface dies, so a codec on it
                    // recovers through the generation bump instead of erroring
                    crate::set_video_window(std::ptr::null_mut());
                    st.handed_seq = 0;
                }
                if st.live.as_ref().is_some_and(|(_, live)| *live == seq) {
                    st.live = None;
                }
                if st.destroying_seq == seq {
                    st.destroying_seq = 0;
                    resume = true;
                }
            }
        }
        drop(st);
        if resume {
            resume_after_retire();
        }
    }

    /// The view is up: hand its surface, or promise the decoder one is on its
    /// way (a layout pass and a SurfaceFlinger round trip, ~100ms), so a codec
    /// that builds in the meantime waits for it instead of starting headless
    /// and losing video until the next keyframe. UI thread.
    fn ensure_window_handoff(self: &Arc<Self>) {
        {
            let mut st = self.handoff.lock().unwrap();
            if st.live.is_some() {
                st.hand_live();
                return;
            }
        }
        crate::set_video_window_pending(true);
        let promise = {
            let mut st = self.handoff.lock().unwrap();
            st.promise_gen = st.promise_gen.wrapping_add(1);
            st.promise_gen
        };
        let this = self.clone();
        slint::Timer::single_shot(SURFACE_GIVE_UP, move || {
            let st = this.handoff.lock().unwrap();
            // a later item's promise is not this timer's to drop
            if st.live.is_none() && st.promise_gen == promise {
                warn!("video surface never materialized, video stays headless");
                crate::set_video_window_pending(false);
            }
        });
    }
}
