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
//! * ONE SURFACE, kept across items. It used to be destroyed between them (the
//!   view was hidden on caps drop) and that teardown raced every codec that
//!   configured against it: the codec would adopt a surface already abandoned,
//!   never dequeue a buffer, and the item would never start, permanently,
//!   because the handoff saw its own seq unchanged and never re-handed. Now the
//!   view is parked at 1x1 instead, which keeps the surface alive (only a zero
//!   size or a hide destroys it) while showing none of the last frame.
//! * The one exception is a CPU-connected surface. After any
//!   ANativeWindow_lock (the sink's raw blit path, which stills and software
//!   formats take) a MediaCodec can never connect to that surface again
//!   (media_status_t -10000), so `video_window_cpu_locked` forces the old
//!   hide-and-remake for exactly that case and nothing else.
//! * Hand the window before the codec builds. A codec that starts windowless
//!   rebuilds when the window arrives and then drops every frame until the
//!   stream's next keyframe (seconds of frozen video). `preopen_current` runs
//!   at LoadingMedia for that; `preopen_pending` re-fires it when the old
//!   item's caps-drop teardown races it.
//! * Clear the player's window BEFORE the surface dies, never after. Both the
//!   caps-drop path and `app_visibility(false)` do set_video_window null first;
//!   the generation bump it causes is also what lets a codec that already
//!   grabbed the dying window recover instead of erroring.
//! * The surface seq (from the Java view) identifies which surface the player
//!   holds; a changed seq means the surface was destroyed or remade and the
//!   window must be re-acquired. The handoff never re-hands an unchanged seq,
//!   so a window adopted under a stale seq would never be corrected: the
//!   acquire is seq-checked on both sides of the JNI read for that reason.
//! * All geometry runs on the slint UI thread; a 0x0 window means the activity
//!   is stopped and the relayout retries on a timer, because a resume to the
//!   pre-stop size emits no resize event.

use std::sync::{Arc, Mutex, OnceLock};

use gst::prelude::*;
use i_slint_backend_android_activity::VideoSurface;
use slint::ComponentHandle;
use tracing::{info, warn};

pub struct SurfaceVideo {
    surface: VideoSurface,
    video_size: Mutex<(u32, u32)>,
    /// Display rotation from the stream's image-orientation tag, degrees.
    /// 90/270 swap the fitted rect's aspect: the codec (direct) or the
    /// window transform (raw path) rotate the pixels, and this is the
    /// layout's half of the same fact.
    rotation: std::sync::atomic::AtomicI32,
    handoff: Mutex<Handoff>,
}

// setup's instance, for the app-state hook in gui.rs
static CURRENT: OnceLock<(Arc<SurfaceVideo>, slint::Weak<crate::MainWindow>)> = OnceLock::new();

/// Shows the surface full-window and hands its window to the player ahead
/// of a load, so the first codec already builds in direct mode. Mostly
/// matters for the first cast after launch; later items inherit the
/// surface, it survives between items.
/// Activity start/stop. The SurfaceView's surface dies with the activity;
/// clearing the player's window first bumps the surface generation, so a
/// codec mid-frame recovers through the swap path instead of erroring on
/// the dead surface. On return the relayout adopts the fresh surface (its
/// zero-size retry covers a window that is not up yet).
pub(crate) fn app_visibility(visible: bool) {
    // The decode throttle keys on this globally, before any surface exists.
    crate::set_app_visible(visible);
    let Some((this, ui)) = CURRENT.get() else {
        return;
    };
    if visible {
        this.relayout(ui);
    } else {
        crate::set_video_window(std::ptr::null_mut());
        this.handoff.lock().unwrap().handed_seq = 0;
    }
}

/// Bottom system inset over the content frame, 0 while immersive or before
/// the surface exists. Subtitles keep clear of it.
pub(crate) fn safe_bottom_inset() -> u32 {
    match CURRENT.get() {
        Some((this, _)) => this.surface.safe_bottom().max(0) as u32,
        None => 0,
    }
}

/// The content frame's origin inside the window. Slint overlays draw from
/// the window origin, the video view is positioned in the content frame;
/// content-space coordinates need this shift to land on the picture.
pub(crate) fn content_offset_in_window() -> (i32, i32) {
    match CURRENT.get() {
        Some((this, _)) => this.surface.content_offset(),
        None => (0, 0),
    }
}

/// The space every video-anchored overlay must share with the SurfaceView:
/// the content frame's size when it is laid out, the slint window as the
/// pre-layout fallback. The slint size alone is wrong, see `relayout`.
pub(crate) fn effective_canvas(ui: &crate::MainWindow) -> (u32, u32) {
    if let Some((this, _)) = CURRENT.get() {
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
    let Some((this, _)) = CURRENT.get() else {
        return;
    };
    this.handoff.lock().unwrap().preopen_pending = false;
    // nothing is coming, a decoder must not wait for it
    crate::set_video_window_pending(false);
    if crate::video_window_cpu_locked() {
        let _ = this.surface.set_visible(false);
    } else {
        // parked, not destroyed; see the lifecycle notes at the top
        let _ = this.surface.set_rect(0, 0, 1, 1);
    }
}

pub(crate) fn preopen_current() {
    let Some((this, ui)) = CURRENT.get() else {
        return;
    };
    let this = this.clone();
    this.handoff.lock().unwrap().preopen_pending = true;
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
        if this.video_size.lock().unwrap().0 != 0 {
            // caps already drove a real layout, nothing to pre-open
            return;
        }
        let _ = this
            .surface
            .set_rect(0, 0, win.width as i32, win.height as i32);
        let _ = this.surface.set_visible(true);
        this.ensure_window_handoff();
    });
}

/// The surface handoff state. Hiding the view between items destroys the
/// surface (that is what clears the previous video's last frame), so the
/// handoff keys on the surface's creation seq, not a one-shot flag.
#[derive(Default)]
struct Handoff {
    // seq of the surface flapjack holds, 0 for none
    handed_seq: i32,
    polling: bool,
    relayout_queued: bool,
    // a load wants the surface up before its codec builds
    preopen_pending: bool,
}

pub(crate) use crate::video_math::letterbox;

impl SurfaceVideo {
    /// Builds the player's video sink and the surface behind the window.
    /// Returns the sink element for [`flapjack::Sinks`].
    pub fn setup(
        ui: &crate::MainWindow,
        app: &slint::android::AndroidApp,
    ) -> Option<(Arc<Self>, gst::Element)> {
        info!("surface video: creating the backing surface view");
        let surface = match VideoSurface::new(app) {
            Ok(surface) => surface,
            Err(err) => {
                warn!(?err, "no video surface, falling back to software video");
                return None;
            }
        };
        let this = Arc::new(Self {
            surface,
            video_size: Mutex::new((0, 0)),
            rotation: std::sync::atomic::AtomicI32::new(0),
            handoff: Mutex::new(Handoff::default()),
        });

        let sink = match gst::ElementFactory::make("amcsurfacesink").build() {
            Ok(sink) => sink,
            Err(err) => {
                warn!(?err, "no amcsurfacesink, falling back to software video");
                return None;
            }
        };
        // the decoder's negotiated caps carry the display size, geometry
        // follows them
        let Some(pad) = sink.static_pad("sink") else {
            warn!("amcsurfacesink without a sink pad?");
            return None;
        };
        info!("surface video: sink ready, waiting for caps");
        let _ = CURRENT.set((this.clone(), ui.as_weak()));
        // The display rotation arrives as a TAG, not in caps, on the same
        // pad. 90/270 swap the fitted aspect below.
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
            let this = this.clone();
            let ui = ui.as_weak();
            move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    if let gst::EventView::Tag(tag) = event.view() {
                        if let Some(o) = tag.tag().index::<gst::tags::ImageOrientation>(0) {
                            let degrees = match o.get() {
                                "rotate-90" => 90,
                                "rotate-180" => 180,
                                "rotate-270" => 270,
                                _ => 0,
                            };
                            let prev = this
                                .rotation
                                .swap(degrees, std::sync::atomic::Ordering::Relaxed);
                            if prev != degrees {
                                info!(degrees, "surface video: display rotation");
                                this.relayout(&ui);
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            }
        });
        pad.connect_notify(Some("caps"), {
            let this = this.clone();
            let ui = ui.as_weak();
            move |pad, _| {
                let caps = pad.current_caps();
                info!(?caps, "surface video: sink caps notify");
                let Some(caps) = caps else {
                    // Caps drop when the item unlinks. The surface must be
                    // destroyed between items: hiding clears the previous
                    // frame AND sheds any CPU producer connection the raw
                    // blit path made (a codec can never connect after one).
                    // Take the window away from the player first, the next
                    // codec must not configure against a dead surface.
                    // Ask before the null: the flag belongs to the window the
                    // player still holds, and set_video_window clears it.
                    let poisoned = crate::video_window_cpu_locked();
                    crate::set_video_window(std::ptr::null_mut());
                    {
                        let mut st = this.handoff.lock().unwrap();
                        // zeroed either way: the next codec needs the window
                        // handed to it again, same surface or not
                        st.handed_seq = 0;
                    }
                    // also ends a zero-size relayout retry chain
                    *this.video_size.lock().unwrap() = (0, 0);
                    // the next item is unrotated until its own tag arrives;
                    // this state is process-wide and would stick otherwise
                    this.rotation
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    if poisoned {
                        // The blit CPU-connected this surface and no codec can
                        // ever attach to it again. Only here is the teardown
                        // race worth taking.
                        info!("surface video: surface was CPU-locked, remaking it");
                        let _ = this.surface.set_visible(false);
                    } else {
                        // Park, do not destroy: 1x1 keeps the surface and its
                        // BufferQueue alive for the next codec while showing
                        // effectively none of the last frame.
                        let _ = this.surface.set_rect(0, 0, 1, 1);
                    }
                    // A load racing this teardown pre-opened the surface for
                    // its codec; give it a fresh one instead of leaving it
                    // windowless (that costs a rebuild and a keyframe wait).
                    // Deferred one layout beat: the zero-size park must
                    // actually destroy the old surface first, or the re-open
                    // coalesces into the same layout pass, the surface
                    // survives, and the previous item's last frame flashes
                    // under the next one.
                    if poisoned && this.handoff.lock().unwrap().preopen_pending {
                        let _ = ui.upgrade_in_event_loop(move |_| {
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(80),
                                || {
                                    let pending = CURRENT.get().is_some_and(|(t, _)| {
                                        t.handoff.lock().unwrap().preopen_pending
                                    });
                                    if pending {
                                        preopen_current();
                                    }
                                },
                            );
                        });
                    }
                    return;
                };
                let Some(s) = caps.structure(0) else { return };
                let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else {
                    return;
                };
                if w > 0 && h > 0 {
                    this.handoff.lock().unwrap().preopen_pending = false;
                    *this.video_size.lock().unwrap() = (w as u32, h as u32);
                    this.relayout(&ui);
                }
            }
        });
        Some((this, sink))
    }

    /// Re-fit the surface to the window; runs the math on the ui thread
    /// where the window size lives.
    pub fn relayout(self: &Arc<Self>, ui: &slint::Weak<crate::MainWindow>) {
        let this = self.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
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
            let (vw, vh) = *this.video_size.lock().unwrap();
            if vw == 0 {
                return;
            }
            // A quarter-turn rotation shows the picture with its sides
            // swapped; the pixels are rotated by the codec or the window
            // transform, the fit must follow.
            let (vw, vh) = match this.rotation.load(std::sync::atomic::Ordering::Relaxed) {
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
                    let ui = ui.as_weak();
                    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
                        this.handoff.lock().unwrap().relayout_queued = false;
                        this.relayout(&ui);
                    });
                }
                return;
            }
            let (x, y, w, h) = letterbox(vw, vh, win.width, win.height);
            info!(vw, vh, x, y, w, h, "surface video: rect");
            crate::android_immersive::set_video_aspect(vw, vh);
            let _ = this.surface.set_rect(x, y, w, h);
            let _ = this.surface.set_visible(true);
            // The hole only exists in a frame slint actually painted after
            // the player view took over; nothing else changes here, so ask
            // for one instead of waiting for the next input
            ui.window().request_redraw();
            this.ensure_window_handoff();
        });
    }

    /// The SurfaceView's surface materializes asynchronously after the
    /// first non-zero visible layout; poll it off-thread and hand it to
    /// flapjack whenever its creation seq moved past the one already
    /// handed. A codec that started in copy mode, or against a since-dead
    /// surface, rebuilds itself on the surface generation bump.
    fn ensure_window_handoff(self: &Arc<Self>) {
        {
            let mut st = self.handoff.lock().unwrap();
            if st.polling {
                return;
            }
            st.polling = true;
        }
        // The view is up, so a window is on its way: a codec that builds in
        // the meantime waits for it (bounded) instead of starting headless
        // and losing video until the next keyframe. The handoff below clears
        // the promise, as does giving up.
        crate::set_video_window_pending(true);
        let this = self.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                let seq = this.surface.surface_seq();
                if seq != 0 {
                    let handed = this.handoff.lock().unwrap().handed_seq;
                    if seq == handed {
                        // the surface flapjack holds is still the live one
                        crate::set_video_window_pending(false);
                        this.handoff.lock().unwrap().polling = false;
                        return;
                    }
                    if let Some(window) = this.surface.acquire_native_window() {
                        // Seqlock: the seq and the surface are two reads, and
                        // the view can be hidden and remade between them. A
                        // window that outlived its seq belongs to an abandoned
                        // BufferQueue, and handing it wedges the codec for the
                        // life of the process, so drop it and look again.
                        if this.surface.surface_seq() != seq {
                            unsafe {
                                ndk_sys::ANativeWindow_release(window.as_ptr().cast())
                            };
                            std::thread::sleep(std::time::Duration::from_millis(25));
                            continue;
                        }
                        info!(seq, "video surface live, handing it to the player");
                        crate::set_video_window(window.as_ptr().cast());
                        // acquire_native_window acquired a reference for US
                        // and set_video_window took its own; without this
                        // release every item leaks a window (and its
                        // BufferQueue, tens of MB at 4K)
                        unsafe { ndk_sys::ANativeWindow_release(window.as_ptr().cast()) };
                        let mut st = this.handoff.lock().unwrap();
                        st.handed_seq = seq;
                        st.polling = false;
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            warn!("video surface never materialized, video stays headless");
            crate::set_video_window_pending(false);
            this.handoff.lock().unwrap().polling = false;
        });
    }
}
