//! Experimental Wayland subsurface video sink (prototype).
//!
//! Instead of compositing the decoded video frame into Slint's own GL surface (the way
//! [`crate::SwapchainSink`] does), this sink renders the frame with libplacebo into a
//! dmabuf-backed texture and hands that dmabuf to a dedicated `wl_subsurface` that lives
//! *underneath* Slint's main `wl_surface`. The Wayland compositor then composites the video
//! independently of the GUI — and, when nothing overlaps it, can promote the subsurface to a
//! hardware overlay plane (direct scanout), avoiding a GPU composition pass entirely.
//!
//! ## HDR
//!
//! When the compositor advertises `wp_color_manager_v1` with parametric image descriptions plus
//! the PQ transfer function and BT.2020 primaries, HDR sources (PQ/HLG) are rendered into a
//! 10-bit PQ/BT.2020 dmabuf and an image description carrying the mastering-display primaries,
//! mastering luminance and MaxCLL/MaxFALL is attached to the subsurface — so the *compositor*
//! tone-maps to the actual display rather than us crushing to SDR. Without that support (or for
//! SDR content) we render to 8-bit sRGB and leave the surface description unset (sRGB default).
//!
//! ## Threading model (deliberately single-reader)
//!
//! We share winit's `wl_display` (via [`Backend::from_foreign_display`]) but create our *own*
//! `EventQueue`. We are only ever a *writer* plus a non-blocking `dispatch_pending` (and the
//! occasional `roundtrip` for an image-description handshake); winit remains the sole *reader* of
//! the socket, which avoids the classic multi-reader deadlock between two libraries that don't
//! know about each other. Everything runs on the Slint render-notifier thread.
//!
//! ## Prototype limitations
//!
//! - **The Slint window must be created with a transparent background**, and the video-player
//!   view must leave its center transparent, otherwise the opaque GUI surface fully occludes the
//!   subsurface below it. Where the compositor supports the single-pixel-buffer and viewporter
//!   extensions we stack an opaque-black backdrop below the video subsurface, so a transparent GUI
//!   region (or the gap after the video unmaps on EOS) shows black rather than the desktop.
//! - We rely on libplacebo's dmabuf *export* picking a format/modifier the compositor can import.
//!   We verify the exported (fourcc, modifier) against the compositor's advertised set before
//!   calling `create_immed` (an unsupported one is a *fatal* protocol error that would also tear
//!   down Slint's connection), and gracefully downgrade HDR→SDR if the 10-bit buffer is rejected.
//! - No `wl_buffer.release` tracking yet; we rely on double-buffering.

use std::cell::RefCell;
use std::collections::HashSet;
use std::os::fd::BorrowedFd;
use std::rc::Rc;

use anyhow::{Result, anyhow};
use gst_video::prelude::*;
use libplacebo::libplacebo_sys::*;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use tracing::{debug, error, info, warn};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::{Backend, ObjectId},
    delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry::WlRegistry,
        wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_v1::{self, WpImageDescriptionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};

use crate::{
    placebo::PlaceboContext,
    video::{ContentLightLevel, Frame, FrameData},
    video_sink::VideoSink,
};

/// Capabilities and async-handshake results collected from Wayland events. Shared between the
/// sink and the transient [`WaylandState`] used during dispatch.
#[derive(Default)]
struct Tracking {
    // wp_color_manager_v1 advertised capabilities.
    feat_parametric: bool,
    feat_mastering: bool,
    // Named transfer functions / primaries the compositor advertises as supported. We only need
    // PQ + BT.2020 for HDR forwarding, but tracking the full advertised set keeps the capability
    // model honest (and ready should we forward other transfers/primaries later).
    supported_transfer_functions: HashSet<TransferFunction>,
    supported_primaries: HashSet<Primaries>,
    // zwp_linux_dmabuf advertised (fourcc, modifier) import combinations. Populated either from
    // the v4 feedback tranches (preferred) or the legacy v3 `modifier` events.
    dmabuf_formats: HashSet<(u32, u64)>,
    // The v4 feedback format table (index -> (fourcc, modifier)), referenced by tranche indices.
    dmabuf_table: Vec<(u32, u64)>,
    // wp_image_description_v1 readiness handshake.
    desc_ready: bool,
    desc_failed: bool,
    desc_fail_msg: Option<String>,
    // Output HDR-capability evaluation, read from the surface feedback's preferred image
    // description. `output_eval_pending` is set whenever the preferred description changes.
    output_eval_pending: bool,
    info_is_hdr: bool,
    info_max_lum: u32,
}

impl Tracking {
    fn hdr_supported(&self) -> bool {
        self.feat_parametric
            && self
                .supported_transfer_functions
                .contains(&TransferFunction::St2084Pq)
            && self.supported_primaries.contains(&Primaries::Bt2020)
    }
}

struct WaylandState {
    t: Rc<RefCell<Tracking>>,
}

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
            state
                .t
                .borrow_mut()
                .dmabuf_formats
                .insert((format, modifier));
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use std::io::Read;
        match event {
            zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, size } => {
                // A new feedback round starts with the format table; reset our collected set.
                let mut file = std::fs::File::from(fd);
                let mut buf = vec![0u8; size as usize];
                let mut t = state.t.borrow_mut();
                t.dmabuf_table.clear();
                t.dmabuf_formats.clear();
                if file.read_exact(&mut buf).is_ok() {
                    // Each entry is 16 bytes: u32 format, u32 padding, u64 modifier (native order).
                    for e in buf.chunks_exact(16) {
                        let format = u32::from_ne_bytes(e[0..4].try_into().unwrap());
                        let modifier = u64::from_ne_bytes(e[8..16].try_into().unwrap());
                        t.dmabuf_table.push((format, modifier));
                    }
                }
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheFormats { indices } => {
                // indices are 16-bit indexes (native endianness) into the format table.
                let mut t = state.t.borrow_mut();
                for idx in indices.chunks_exact(2) {
                    let i = u16::from_ne_bytes([idx[0], idx[1]]) as usize;
                    if let Some(&entry) = t.dmabuf_table.get(i) {
                        t.dmabuf_formats.insert(entry);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagerV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WpColorManagerV1,
        event: wp_color_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut t = state.t.borrow_mut();
        match event {
            wp_color_manager_v1::Event::SupportedFeature {
                feature: WEnum::Value(feature),
            } => match feature {
                Feature::Parametric => t.feat_parametric = true,
                Feature::SetMasteringDisplayPrimaries => t.feat_mastering = true,
                _ => {}
            },
            wp_color_manager_v1::Event::SupportedTfNamed {
                tf: WEnum::Value(tf),
            } => {
                t.supported_transfer_functions.insert(tf);
            }
            wp_color_manager_v1::Event::SupportedPrimariesNamed {
                primaries: WEnum::Value(primaries),
            } => {
                t.supported_primaries.insert(primaries);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WpImageDescriptionV1,
        event: wp_image_description_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_image_description_v1::Event::Ready { .. } => state.t.borrow_mut().desc_ready = true,
            wp_image_description_v1::Event::Failed { msg, .. } => {
                let mut t = state.t.borrow_mut();
                t.desc_failed = true;
                t.desc_fail_msg = Some(msg);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WpColorManagementSurfaceFeedbackV1,
        event: wp_color_management_surface_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_color_management_surface_feedback_v1::Event::PreferredChanged { .. } = event {
            state.t.borrow_mut().output_eval_pending = true;
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WpImageDescriptionInfoV1,
        event: wp_image_description_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut t = state.t.borrow_mut();
        match event {
            wp_image_description_info_v1::Event::TfNamed {
                tf: WEnum::Value(tf),
            } => {
                if matches!(tf, TransferFunction::St2084Pq | TransferFunction::Hlg) {
                    t.info_is_hdr = true;
                }
            }
            wp_image_description_info_v1::Event::Luminances { max_lum, .. }
            | wp_image_description_info_v1::Event::TargetLuminance { max_lum, .. } => {
                t.info_max_lum = t.info_max_lum.max(max_lum);
            }
            _ => {}
        }
    }
}

delegate_noop!(WaylandState: ignore WlCompositor);
delegate_noop!(WaylandState: ignore WlSubcompositor);
delegate_noop!(WaylandState: ignore WlSurface);
delegate_noop!(WaylandState: ignore WlSubsurface);
delegate_noop!(WaylandState: ignore ZwpLinuxBufferParamsV1);
delegate_noop!(WaylandState: ignore WlBuffer);
delegate_noop!(WaylandState: ignore WpColorManagementSurfaceV1);
delegate_noop!(WaylandState: ignore WpImageDescriptionCreatorParamsV1);
delegate_noop!(WaylandState: ignore WpSinglePixelBufferManagerV1);
delegate_noop!(WaylandState: ignore WpViewporter);
delegate_noop!(WaylandState: ignore WpViewport);

struct Wayland {
    conn: Connection,
    event_queue: EventQueue<WaylandState>,
    qh: QueueHandle<WaylandState>,
    tracking: Rc<RefCell<Tracking>>,
    dmabuf: ZwpLinuxDmabufV1,
    /// Kept alive so the compositor keeps sending updated import-format feedback.
    _dmabuf_feedback: Option<ZwpLinuxDmabufFeedbackV1>,
    surface: WlSurface,
    subsurface: WlSubsurface,
    color_surface: Option<WpColorManagementSurfaceV1>,
    color_manager: Option<WpColorManagerV1>,
    /// Feedback on the *parent* surface, used to learn the output's preferred image description
    /// (and thus whether the output is HDR-capable).
    feedback: Option<WpColorManagementSurfaceFeedbackV1>,
    /// Opaque-black backdrop subsurface, stacked below the video subsurface, so a transparent GUI
    /// region (or the gap after the video unmaps on EOS) never reveals the desktop behind the
    /// receiver. `None` when the compositor lacks single-pixel-buffer / viewporter support.
    background: Option<Background>,
    _compositor: WlCompositor,
    _subcompositor: WlSubcompositor,
    _parent: WlSurface,
}

/// A solid opaque-black floor built from a 1×1 single-pixel buffer scaled to the window via
/// `wp_viewport`. Lives below the video subsurface; resized (via the viewport) as the window does.
struct Background {
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    /// The 1×1 buffer, kept alive while attached.
    _buffer: WlBuffer,
    /// Current viewport destination size; `(0, 0)` until the first `size` call maps it.
    size: (u32, u32),
}

impl Background {
    /// Scale the backdrop to `width`×`height` and (re)commit. Idempotent for an unchanged size.
    fn size(&mut self, width: u32, height: u32) {
        if self.size == (width, height) {
            return;
        }
        self.viewport.set_destination(width as i32, height as i32);
        self.surface.damage(0, 0, width as i32, height as i32);
        self.surface.commit();
        self.size = (width, height);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TargetFormat {
    Rgba8,
    Rgb10a2,
}

impl TargetFormat {
    fn pl_name(self) -> &'static str {
        match self {
            TargetFormat::Rgba8 => "rgba8",
            TargetFormat::Rgb10a2 => "rgb10a2",
        }
    }
}

struct Target {
    tex: pl_tex,
    buffer: WlBuffer,
    width: u32,
    height: u32,
    format: TargetFormat,
}

/// The color treatment applied to the surface, used to avoid redundant protocol traffic.
#[derive(Clone, PartialEq)]
enum ColorSig {
    Sdr,
    Hdr {
        mastering_primaries: Option<[i32; 8]>,
        mastering_lum: Option<(u32, u32)>,
        max_cll: Option<u32>,
        max_fall: Option<u32>,
    },
}

struct Decision {
    format: TargetFormat,
    color: pl_color_space,
    sig: ColorSig,
}

pub struct WaylandSubsurfaceSink {
    wl: Option<Wayland>,
    targets: [Option<Target>; 2],
    current: usize,
    /// Last color signature applied to the surface (None = nothing applied yet).
    applied_color: Option<ColorSig>,
    /// Set if a 10-bit HDR buffer was rejected by the compositor; pins us to SDR thereafter.
    hdr_buffer_unsupported: bool,
    /// Set if any color-management request errored; disables the whole color-management path so we
    /// never risk re-triggering a (fatal, connection-shared) protocol error.
    color_mgmt_failed: bool,
    /// User opt-out (`--disable-hdr-output`): always tone-map to SDR with libplacebo instead of
    /// forwarding HDR to the compositor.
    disable_hdr: bool,
    /// Whether the surface's current output is HDR-capable (re-evaluated when the compositor's
    /// preferred image description changes). HDR is only forwarded when this is true; otherwise
    /// libplacebo tone-maps to SDR.
    output_is_hdr: bool,
    /// Set when no dma-buf the compositor can import is available (e.g. a cross-GPU PRIME setup
    /// where our render GPU differs from the compositor's). We then render into Slint's own
    /// surface via the libplacebo swapchain — not zero-copy, but it works everywhere.
    use_swapchain_fallback: bool,
    swapchain_size: (u32, u32),
}

impl WaylandSubsurfaceSink {
    pub fn new(disable_hdr: bool) -> Self {
        Self {
            wl: None,
            targets: [None, None],
            current: 0,
            applied_color: None,
            hdr_buffer_unsupported: false,
            color_mgmt_failed: false,
            disable_hdr,
            output_is_hdr: false,
            use_swapchain_fallback: false,
            swapchain_size: (0, 0),
        }
    }

    /// Render the frame into Slint's own surface via the libplacebo swapchain — the same path
    /// `SwapchainSink` uses. Used as a fallback when we can't share a dma-buf with the compositor.
    fn render_via_swapchain(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &Frame,
        target_size: (u32, u32),
    ) -> Result<()> {
        if target_size != self.swapchain_size {
            placebo.resize_swapchain(target_size.0 as i32, target_size.1 as i32);
            self.swapchain_size = target_size;
        }
        let Some(swframe) = placebo.start_frame() else {
            return Ok(());
        };
        placebo
            .render_frame(&swframe, frame)
            .map_err(|err| anyhow!("placebo swapchain render failed: {err}"))?;
        placebo.submit_frame();

        // libplacebo leaves its own FBO + viewport bound. Restore the default framebuffer so that
        // (a) Slint renders its UI into the right target afterwards, and (b) lib.rs's per-tick
        // `gl.clear` on the *next* tick — including the EOS tick, where no frame is rendered —
        // clears fb0 rather than libplacebo's leftover FBO, which would otherwise leave the final
        // video frame on screen after the stream ends.
        unsafe {
            use glow::HasContext;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, target_size.0 as i32, target_size.1 as i32);
        }
        Ok(())
    }

    /// Permanently switch to in-surface rendering and render this frame that way. Called once when
    /// we discover no compositor-importable dma-buf exists.
    fn enter_swapchain_fallback(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &Frame,
        target_size: (u32, u32),
        err: anyhow::Error,
    ) -> Result<()> {
        warn!(
            ?err,
            "No compositor-importable dma-buf (likely a cross-GPU setup); falling back to \
             in-surface rendering — video will display but without the zero-copy subsurface path"
        );
        self.use_swapchain_fallback = true;
        // Unmap the subsurface so it can't cover the swapchain output.
        if let Some(wl) = self.wl.as_ref() {
            wl.surface.attach(None, 0, 0);
            wl.surface.commit();
            let _ = wl.conn.flush();
        }
        self.render_via_swapchain(placebo, gl, frame, target_size)
    }

    fn init_wayland(
        display_ptr: *mut std::ffi::c_void,
        parent_ptr: *mut std::ffi::c_void,
    ) -> Result<Wayland> {
        // SAFETY: `display_ptr` is winit's live `*mut wl_display`. The backend is created in
        // "guest" mode and will not close the connection on drop.
        let backend = unsafe { Backend::from_foreign_display(display_ptr as *mut _) };
        let conn = Connection::from_backend(backend);

        let (globals, event_queue) = registry_queue_init::<WaylandState>(&conn)
            .map_err(|err| anyhow!("failed to init wayland registry: {err}"))?;
        let qh = event_queue.handle();

        let compositor: WlCompositor = globals
            .bind(&qh, 1..=6, ())
            .map_err(|err| anyhow!("compositor unavailable: {err}"))?;
        let subcompositor: WlSubcompositor = globals
            .bind(&qh, 1..=1, ())
            .map_err(|err| anyhow!("wl_subcompositor unavailable: {err}"))?;
        // Prefer v4+ so we can read the real importable format/modifier set from feedback
        // tranches; fall back to v3's legacy `modifier` events otherwise.
        let dmabuf: ZwpLinuxDmabufV1 = globals
            .bind(&qh, 3..=5, ())
            .map_err(|err| anyhow!("zwp_linux_dmabuf_v1 unavailable: {err}"))?;

        // Optional: color management for HDR passthrough.
        let color_manager: Option<WpColorManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

        // SAFETY: `parent_ptr` is winit's live `*mut wl_surface` from the same connection.
        let parent_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), parent_ptr as *mut _) }
            .map_err(|err| anyhow!("invalid parent wl_surface pointer: {err}"))?;
        let parent = WlSurface::from_id(&conn, parent_id)
            .map_err(|err| anyhow!("failed to wrap parent wl_surface: {err}"))?;

        // Per-surface feedback reflects the importable formats for the output the window is on
        // (handles hybrid GPU setups where the external output is on a different GPU than the
        // compositor's main device); fall back to v3 legacy `modifier` events on older servers.
        let dmabuf_feedback = if dmabuf.version() >= 4 {
            Some(dmabuf.get_surface_feedback(&parent, &qh, ()))
        } else {
            None
        };

        let surface = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&surface, &parent, &qh, ());
        subsurface.set_desync();
        subsurface.place_below(&parent);
        subsurface.set_position(0, 0);

        // Optional opaque-black backdrop below the video subsurface, so a transparent GUI region —
        // or the gap after the video unmaps on EOS — shows black instead of the desktop. Needs the
        // single-pixel-buffer extension (for a 1×1 solid buffer) and viewporter (to scale it to the
        // window). Its destination size is set lazily on the first render, once we know the window.
        let background = (|| {
            let spb: WpSinglePixelBufferManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
            let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ()).ok()?;
            let bg_surface = compositor.create_surface(&qh, ());
            let bg_subsurface = subcompositor.get_subsurface(&bg_surface, &parent, &qh, ());
            bg_subsurface.set_desync();
            // Stack the backdrop directly below the video subsurface (GUI > video > backdrop).
            bg_subsurface.place_below(&surface);
            bg_subsurface.set_position(0, 0);
            // Opaque black: zero color channels, full-range alpha.
            let buffer = spb.create_u32_rgba_buffer(0, 0, 0, u32::MAX, &qh, ());
            let viewport = viewporter.get_viewport(&bg_surface, &qh, ());
            bg_surface.attach(Some(&buffer), 0, 0);
            debug!("Created opaque-black backdrop (single-pixel buffer + viewport)");
            Some(Background {
                surface: bg_surface,
                subsurface: bg_subsurface,
                viewport,
                _buffer: buffer,
                size: (0, 0),
            })
        })();

        let color_surface = color_manager
            .as_ref()
            .map(|cm| cm.get_surface(&surface, &qh, ()));
        // Feedback on the parent (which is mapped on an output) tells us the output's preferred
        // image description, hence whether the output is HDR-capable.
        let feedback = color_manager
            .as_ref()
            .map(|cm| cm.get_surface_feedback(&parent, &qh, ()));

        let tracking = Rc::new(RefCell::new(Tracking::default()));
        // Evaluate output HDR capability on the first render tick.
        tracking.borrow_mut().output_eval_pending = true;

        let mut wl = Wayland {
            conn,
            event_queue,
            qh,
            tracking,
            dmabuf,
            _dmabuf_feedback: dmabuf_feedback,
            surface,
            subsurface,
            color_surface,
            color_manager,
            feedback,
            background,
            _compositor: compositor,
            _subcompositor: subcompositor,
            _parent: parent,
        };

        // Collect dmabuf format/modifier advertisements and color-manager capabilities, which are
        // sent right after binding.
        let mut state = WaylandState {
            t: wl.tracking.clone(),
        };
        wl.event_queue
            .roundtrip(&mut state)
            .map_err(|err| anyhow!("initial wayland roundtrip failed: {err}"))?;

        {
            let t = wl.tracking.borrow();
            debug!(
                color_manager = wl.color_manager.is_some(),
                hdr_supported = t.hdr_supported(),
                mastering = t.feat_mastering,
                dmabuf_formats = t.dmabuf_formats.len(),
                "Wayland subsurface video sink initialized"
            );
        }
        if wl.color_manager.is_some() && !wl.tracking.borrow().hdr_supported() {
            // We bound the manager but it lacks the bits we need; tear it all down so we don't try.
            if let Some(cs) = wl.color_surface.take() {
                cs.destroy();
            }
            if let Some(fb) = wl.feedback.take() {
                fb.destroy();
            }
            wl.color_manager = None;
            wl.tracking.borrow_mut().output_eval_pending = false;
        }

        Ok(wl)
    }

    /// Re-evaluate whether the surface's current output is HDR-capable, by reading the preferred
    /// image description's transfer function / luminance. Updates [`Self::output_is_hdr`].
    fn eval_output_hdr(&mut self) {
        if self.color_mgmt_failed {
            return;
        }
        match self.query_output_hdr() {
            Ok(Some(is_hdr)) => {
                if is_hdr != self.output_is_hdr {
                    info!(output_is_hdr = is_hdr, "Output HDR capability changed");
                }
                self.output_is_hdr = is_hdr;
            }
            Ok(None) => {
                // Couldn't determine (no color management, or preferred description unavailable);
                // assume SDR so we let libplacebo tone-map.
                self.output_is_hdr = false;
            }
            Err(err) => {
                error!(?err, "Output HDR query failed; disabling color management");
                self.color_mgmt_failed = true;
                self.output_is_hdr = false;
            }
        }
    }

    /// Returns `Ok(Some(is_hdr))` on a successful read, `Ok(None)` if it couldn't be determined,
    /// `Err` on a (fatal) protocol error. Borrows only `self.wl` so the caller can update other
    /// fields afterwards.
    fn query_output_hdr(&mut self) -> Result<Option<bool>> {
        let Some(wl) = self.wl.as_mut() else {
            return Ok(None);
        };
        let Some(feedback) = wl.feedback.clone() else {
            return Ok(None);
        };
        let qh = wl.qh.clone();

        {
            let mut t = wl.tracking.borrow_mut();
            t.desc_ready = false;
            t.desc_failed = false;
            t.info_is_hdr = false;
            t.info_max_lum = 0;
        }

        // get_preferred_parametric yields a parametric description that immediately becomes ready
        // (or fails with low_version) and explicitly permits get_information.
        let desc = feedback.get_preferred_parametric(&qh, ());
        let mut state = WaylandState {
            t: wl.tracking.clone(),
        };
        wl.event_queue
            .roundtrip(&mut state)
            .map_err(|err| anyhow!("preferred-description roundtrip failed: {err}"))?;

        let (ready, failed) = {
            let t = wl.tracking.borrow();
            (t.desc_ready, t.desc_failed)
        };
        if failed || !ready {
            desc.destroy();
            return Ok(None);
        }

        let _info = desc.get_information(&qh, ());
        wl.event_queue
            .roundtrip(&mut state)
            .map_err(|err| anyhow!("preferred-description info roundtrip failed: {err}"))?;
        let (is_hdr, max_lum) = {
            let t = wl.tracking.borrow();
            (t.info_is_hdr, t.info_max_lum)
        };
        desc.destroy();

        // HDR if the preferred transfer is PQ/HLG, or the target peak luminance is well above SDR.
        let hdr = is_hdr || max_lum > 300;
        debug!(tf_is_hdr = is_hdr, max_lum, hdr, "Read output preferred image description");
        Ok(Some(hdr))
    }

    fn ensure_target(
        &mut self,
        placebo: &PlaceboContext,
        width: u32,
        height: u32,
        format: TargetFormat,
    ) -> Result<()> {
        if let Some(t) = &self.targets[self.current]
            && t.width == width
            && t.height == height
            && t.format == format
        {
            return Ok(());
        }

        if let Some(old) = self.targets[self.current].take() {
            destroy_target(placebo, old);
        }

        let wl = self.wl.as_ref().expect("ensure_target requires wayland");
        let target = create_target(placebo, wl, width, height, format)?;
        self.targets[self.current] = Some(target);
        Ok(())
    }

    /// (Re)apply the color description to the surface if it changed. On *any* failure we set
    /// [`Self::color_mgmt_failed`] and never touch color management again, because a color-mgmt
    /// protocol error is fatal to the (shared) connection — we must not risk re-triggering it.
    fn apply_color(&mut self, sig: &ColorSig) -> Result<()> {
        if self.color_mgmt_failed || self.applied_color.as_ref() == Some(sig) {
            return Ok(());
        }
        // Without color management there is nothing to forward (SDR sRGB is the default).
        if self.wl.as_ref().and_then(|w| w.color_surface.as_ref()).is_none() {
            self.applied_color = Some(sig.clone());
            return Ok(());
        }

        match self.apply_color_inner(sig) {
            Ok(()) => {
                self.applied_color = Some(sig.clone());
                Ok(())
            }
            Err(err) => {
                error!(?err, "Color-management request failed; disabling HDR forwarding");
                self.color_mgmt_failed = true;
                Err(err)
            }
        }
    }

    fn apply_color_inner(&mut self, sig: &ColorSig) -> Result<()> {
        match sig {
            ColorSig::Sdr => {
                let wl = self.wl.as_ref().unwrap();
                wl.color_surface.as_ref().unwrap().unset_image_description();
                let _ = wl.conn.flush();
                debug!("Applied SDR surface color (unset image description, sRGB default)");
            }
            ColorSig::Hdr {
                mastering_primaries,
                mastering_lum,
                max_cll,
                max_fall,
            } => {
                let wl = self.wl.as_mut().unwrap();
                // Clone the cheap proxy handles so we can mutably borrow the event queue below.
                let manager = wl.color_manager.clone().unwrap();
                let cm_surface = wl.color_surface.clone().unwrap();
                let qh = wl.qh.clone();

                {
                    let mut t = wl.tracking.borrow_mut();
                    t.desc_ready = false;
                    t.desc_failed = false;
                    t.desc_fail_msg = None;
                }

                // set_mastering_* require the set_mastering_display_primaries feature; sending them
                // without it is an `unsupported_feature` protocol error.
                let feat_mastering = wl.tracking.borrow().feat_mastering;

                let params = manager.create_parametric_creator(&qh, ());
                params.set_tf_named(TransferFunction::St2084Pq);
                params.set_primaries_named(Primaries::Bt2020);
                if feat_mastering {
                    if let Some(p) = mastering_primaries {
                        params.set_mastering_display_primaries(
                            p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7],
                        );
                    }
                    if let Some((min_lum, max_lum)) = mastering_lum {
                        params.set_mastering_luminance(*min_lum, *max_lum);
                    }
                }
                // Content light levels are only forwarded when consistent with the mastering
                // range: the compositor (e.g. sway) requires max_cll <= mastering max L, and
                // CTA-861 has max_fall <= max_cll. Forwarding them without an explicit mastering
                // range risks the same comparison against the compositor's default max L, so we
                // only send them when we've set a mastering luminance we can compare against.
                if feat_mastering
                    && let Some((_, max_lum)) = mastering_lum
                    && let Some(cll) = (*max_cll).filter(|c| *c <= *max_lum)
                {
                    params.set_max_cll(cll);
                    if let Some(fall) = (*max_fall).filter(|f| *f <= cll) {
                        params.set_max_fall(fall);
                    }
                }
                let desc = params.create(&qh, ());

                // The image description must reach the "ready" state before it can be used. A
                // protocol error here surfaces as the roundtrip's DispatchError, which carries the
                // offending interface + code + message — far more useful than libwayland's raw
                // "Protocol error N on object @M".
                let mut state = WaylandState {
                    t: wl.tracking.clone(),
                };
                wl.event_queue
                    .roundtrip(&mut state)
                    .map_err(|err| anyhow!("color-management roundtrip failed: {err}"))?;

                let (ready, failed, fail_msg) = {
                    let t = wl.tracking.borrow();
                    (t.desc_ready, t.desc_failed, t.desc_fail_msg.clone())
                };
                if failed || !ready {
                    desc.destroy();
                    // A `failed` event is recoverable (not a protocol error), but we still stop
                    // trying for this stream.
                    return Err(anyhow!(
                        "compositor rejected HDR image description: {}",
                        fail_msg.as_deref().unwrap_or("not ready")
                    ));
                }

                cm_surface.set_image_description(&desc, RenderIntent::Perceptual);
                // The surface keeps the description's data; we can release our handle.
                desc.destroy();
                let _ = wl.conn.flush();
                info!(
                    has_mastering_primaries = mastering_primaries.is_some(),
                    ?mastering_lum,
                    ?max_cll,
                    ?max_fall,
                    "Applied HDR image description to subsurface (PQ/BT.2020)"
                );
            }
        }
        Ok(())
    }
}

fn frame_colorimetry(frame: &Frame) -> gst_video::VideoColorimetry {
    match &frame.data {
        FrameData::SystemMemory { frame } => frame.info().colorimetry(),
        FrameData::DmaBuf { dma_info, .. } => dma_info.colorimetry(),
    }
}

fn frame_bit_depth(frame: &Frame) -> u32 {
    let depth = match &frame.data {
        FrameData::SystemMemory { frame } => frame.info().format_info().depth().iter().copied().max(),
        FrameData::DmaBuf { dma_info, .. } => dma_info
            .to_video_info()
            .ok()
            .and_then(|info| info.format_info().depth().iter().copied().max()),
    };
    depth.unwrap_or(8)
}

/// Decide render format, libplacebo target colorspace and the surface color signature for a frame.
fn decide_color(frame: &Frame, allow_hdr: bool) -> Decision {
    let colorimetry = frame_colorimetry(frame);
    let transfer = colorimetry.transfer();
    let is_hdr = matches!(
        transfer,
        gst_video::VideoTransferFunction::Smpte2084 | gst_video::VideoTransferFunction::AribStdB67
    );

    if is_hdr && allow_hdr {
        // Render HDR into PQ/BT.2020 and let the compositor tone-map. (HLG sources are converted
        // to PQ too, matching the fhs sink, because HLG carries values outside [0,1].)
        let color = pl_color_space {
            primaries: pl_color_primaries::PL_COLOR_PRIM_BT_2020,
            transfer: pl_color_transfer::PL_COLOR_TRC_PQ,
            hdr: unsafe { std::mem::zeroed() },
        };

        // Only forward mastering chromaticities when they're sane (each in (0,1)); a bad value
        // wouldn't raise a protocol error but would mislead the compositor.
        let mastering_primaries = frame.mastering_display_info.as_ref().and_then(|mdi| {
            let p = &mdi.display_primaries;
            let coords = [
                p[0].x, p[0].y, p[1].x, p[1].y, p[2].x, p[2].y,
                mdi.white_point.x, mdi.white_point.y,
            ];
            if coords.iter().all(|c| c.is_finite() && *c > 0.0 && *c < 1.0) {
                let s = |v: f32| (v * 1_000_000.0).round() as i32;
                Some(coords.map(s))
            } else {
                None
            }
        });
        // The protocol raises the *fatal* `invalid_luminance` error unless max_L > min_L (in its
        // units: max_lum * 10000 > min_lum). Validate exactly that, plus finiteness/sane range,
        // and skip the request entirely on any doubt — a bad value would tear down the shared
        // connection (and with it Slint).
        let mastering_lum = frame.mastering_display_info.as_ref().and_then(|mdi| {
            let min_nits = mdi.min_luminance_as_nits();
            let max_nits = mdi.max_luminance_as_nits();
            if !min_nits.is_finite() || !max_nits.is_finite() {
                return None;
            }
            if !(0.0..=100_000.0).contains(&max_nits) || max_nits < 1.0 || min_nits < 0.0 {
                return None;
            }
            let min_lum = (min_nits * 10_000.0).round() as u32;
            let max_lum = max_nits.round() as u32;
            if (max_lum as u64) * 10_000 <= (min_lum as u64) {
                debug!(min_lum, max_lum, "Skipping invalid mastering luminance (max <= min)");
                return None;
            }
            Some((min_lum, max_lum))
        });
        // max_cll/max_fall are "undefined by default"; only send them when positive.
        let max_cll = frame
            .content_light_level
            .as_ref()
            .map(|cll: &ContentLightLevel| cll.max_content_light_level)
            .filter(|v| *v > 0)
            .map(|v| v as u32);
        let max_fall = frame
            .content_light_level
            .as_ref()
            .map(|cll: &ContentLightLevel| cll.max_frame_average_light_level)
            .filter(|v| *v > 0)
            .map(|v| v as u32);

        Decision {
            format: TargetFormat::Rgb10a2,
            color,
            sig: ColorSig::Hdr {
                mastering_primaries,
                mastering_lum,
                max_cll,
                max_fall,
            },
        }
    } else {
        // SDR (or HDR we can't forward): render to sRGB, tone-mapping HDR down if needed.
        let color = pl_color_space {
            primaries: pl_color_primaries::PL_COLOR_PRIM_BT_709,
            transfer: pl_color_transfer::PL_COLOR_TRC_SRGB,
            hdr: unsafe { std::mem::zeroed() },
        };
        let format = if frame_bit_depth(frame) > 8 {
            TargetFormat::Rgb10a2
        } else {
            TargetFormat::Rgba8
        };
        Decision {
            format,
            color,
            sig: ColorSig::Sdr,
        }
    }
}

/// The guaranteed-safe fallback: 8-bit sRGB, which every compositor can import (ABGR8888) and
/// which needs no color-management plumbing.
fn sdr_rgba8_decision() -> Decision {
    Decision {
        format: TargetFormat::Rgba8,
        color: pl_color_space {
            primaries: pl_color_primaries::PL_COLOR_PRIM_BT_709,
            transfer: pl_color_transfer::PL_COLOR_TRC_SRGB,
            hdr: unsafe { std::mem::zeroed() },
        },
        sig: ColorSig::Sdr,
    }
}

fn create_target(
    placebo: &PlaceboContext,
    wl: &Wayland,
    width: u32,
    height: u32,
    format: TargetFormat,
) -> Result<Target> {
    let gpu = placebo.gpu();

    // SAFETY: gpu pointer is valid for the lifetime of the PlaceboContext.
    unsafe {
        if (*gpu).export_caps.tex & pl_handle_type_PL_HANDLE_DMA_BUF as u64 == 0 {
            return Err(anyhow!("libplacebo GPU cannot export textures as dma-buf"));
        }
    }

    let fmt_name = std::ffi::CString::new(format.pl_name()).unwrap();
    let fmt = unsafe { pl_find_named_fmt(gpu, fmt_name.as_ptr()) };
    if fmt.is_null() {
        return Err(anyhow!("libplacebo has no '{}' format", format.pl_name()));
    }
    let fourcc = unsafe { (*fmt).fourcc };
    if fourcc == 0 {
        return Err(anyhow!("'{}' has no DRM fourcc", format.pl_name()));
    }

    let mut tex_params: pl_tex_params = unsafe { std::mem::zeroed() };
    tex_params.w = width as i32;
    tex_params.h = height as i32;
    tex_params.format = fmt;
    tex_params.sampleable = true;
    tex_params.renderable = true;
    tex_params.blit_dst =
        unsafe { (*fmt).caps as u32 } & pl_fmt_caps::PL_FMT_CAP_BLITTABLE as u32 != 0;
    tex_params.export_handle = pl_handle_type_PL_HANDLE_DMA_BUF;

    let tex = unsafe { pl_tex_create(gpu, &tex_params) };
    if tex.is_null() {
        return Err(anyhow!("pl_tex_create (dma-buf export) failed for {format:?}"));
    }

    let cleanup_tex = |mut tex: pl_tex| unsafe { pl_tex_destroy(gpu, &mut tex) };

    let shared = unsafe { (*tex).shared_mem };
    let fd = unsafe { shared.handle.fd };
    if fd < 0 {
        cleanup_tex(tex);
        return Err(anyhow!("exported dma-buf has no fd"));
    }
    let modifier = shared.drm_format_mod;
    let stride = shared.stride_w as u32;
    let offset = shared.offset as u32;

    // Guard against a fatal protocol error: only hand the compositor a (fourcc, modifier) it has
    // advertised as importable.
    if !wl.tracking.borrow().dmabuf_formats.contains(&(fourcc, modifier)) {
        cleanup_tex(tex);
        let t = wl.tracking.borrow();
        let advertised: Vec<String> = t
            .dmabuf_formats
            .iter()
            .filter(|(f, _)| *f == fourcc)
            .map(|(_, m)| format!("{m:#018x}"))
            .collect();
        return Err(anyhow!(
            "compositor does not advertise dma-buf import for {format:?} \
             (fourcc {fourcc:#010x}, modifier {modifier:#018x}); \
             advertised modifiers for this fourcc: {advertised:?} \
             (total advertised combos: {})",
            t.dmabuf_formats.len()
        ));
    }

    debug!(
        ?format,
        width,
        height,
        fourcc = format!("{fourcc:#010x}"),
        modifier = format!("{modifier:#018x}"),
        stride,
        offset,
        "Created dma-buf export render target"
    );

    let params = wl.dmabuf.create_params(&wl.qh, ());
    // SAFETY: fd is owned by the texture and outlives this borrow.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    params.add(
        borrowed,
        0,
        offset,
        stride,
        (modifier >> 32) as u32,
        (modifier & 0xffff_ffff) as u32,
    );
    let buffer = params.create_immed(
        width as i32,
        height as i32,
        fourcc,
        zwp_linux_buffer_params_v1::Flags::empty(),
        &wl.qh,
        (),
    );
    params.destroy();

    Ok(Target {
        tex,
        buffer,
        width,
        height,
        format,
    })
}

fn destroy_target(placebo: &PlaceboContext, target: Target) {
    target.buffer.destroy();
    let mut tex = target.tex;
    unsafe { pl_tex_destroy(placebo.gpu(), &mut tex) };
}

impl VideoSink for WaylandSubsurfaceSink {
    fn setup(&mut self, window: &slint::Window) {
        use i_slint_backend_winit::WinitWindowAccessor;

        if self.wl.is_some() {
            return;
        }

        let handles = window.with_winit_window(|win| {
            let display = win.display_handle().ok().map(|h| h.as_raw());
            let surface = win.window_handle().ok().map(|h| h.as_raw());
            (display, surface)
        });

        let Some((Some(display), Some(surface))) = handles else {
            error!(
                "WaylandSubsurfaceSink: no winit window / raw handles available; \
                 falling back to a no-op sink"
            );
            return;
        };

        let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
            (display, surface)
        else {
            error!("WaylandSubsurfaceSink requires a Wayland session; got a non-Wayland handle");
            return;
        };

        match Self::init_wayland(display.display.as_ptr(), surface.surface.as_ptr()) {
            Ok(wl) => self.wl = Some(wl),
            Err(err) => error!(?err, "Failed to initialize Wayland subsurface sink"),
        }
    }

    fn render(
        &mut self,
        placebo: &mut PlaceboContext,
        gl: &glow::Context,
        frame: &Frame,
        target_size: (u32, u32),
    ) -> Result<()> {
        if self.wl.is_none() {
            return Ok(());
        }

        let (width, height) = target_size;
        if width == 0 || height == 0 {
            return Ok(());
        }

        // No importable dma-buf available (e.g. cross-GPU): render into Slint's own surface.
        if self.use_swapchain_fallback {
            return self.render_via_swapchain(placebo, gl, frame, target_size);
        }

        // Reap pending events (buffer releases, preferred-description changes) that winit's reader
        // already pulled in.
        {
            let wl = self.wl.as_mut().unwrap();
            let mut state = WaylandState {
                t: wl.tracking.clone(),
            };
            let _ = wl.event_queue.dispatch_pending(&mut state);
        }

        // Keep the opaque-black backdrop scaled to the window (maps it on the first render).
        {
            let wl = self.wl.as_mut().unwrap();
            if let Some(bg) = wl.background.as_mut() {
                bg.size(width, height);
                let _ = wl.conn.flush();
            }
        }

        // Re-evaluate output HDR capability if the compositor's preferred description changed.
        let eval_pending = self
            .wl
            .as_ref()
            .map(|w| w.tracking.borrow().output_eval_pending)
            .unwrap_or(false);
        if eval_pending {
            if let Some(w) = self.wl.as_ref() {
                w.tracking.borrow_mut().output_eval_pending = false;
            }
            self.eval_output_hdr();
        }

        let hdr_ok = !self.disable_hdr
            && self.output_is_hdr
            && !self.color_mgmt_failed
            && !self.hdr_buffer_unsupported
            && self
                .wl
                .as_ref()
                .map(|w| w.color_manager.is_some())
                .unwrap_or(false);

        let mut decision = decide_color(frame, hdr_ok);

        // Allocate the target. If a 10-bit buffer (HDR, or >8-bit SDR) isn't importable by the
        // compositor, retry as plain 8-bit sRGB. If even that can't be imported (cross-GPU: no
        // common modifier), permanently fall back to rendering into Slint's own surface.
        if let Err(err) = self.ensure_target(placebo, width, height, decision.format) {
            if decision.format != TargetFormat::Rgba8 {
                warn!(?err, format = ?decision.format, "dma-buf target rejected; downgrading to 8-bit sRGB");
                self.hdr_buffer_unsupported = true;
                decision = sdr_rgba8_decision();
                if let Err(err) = self.ensure_target(placebo, width, height, decision.format) {
                    return self.enter_swapchain_fallback(placebo, gl, frame, target_size, err);
                }
            } else {
                return self.enter_swapchain_fallback(placebo, gl, frame, target_size, err);
            }
        }

        // Apply the surface color description before presenting the matching buffer.
        if let Err(err) = self.apply_color(&decision.sig) {
            warn!(?err, "Failed to apply HDR color description; downgrading to 8-bit sRGB");
            self.hdr_buffer_unsupported = true;
            decision = sdr_rgba8_decision();
            self.ensure_target(placebo, width, height, decision.format)?;
            let _ = self.apply_color(&decision.sig);
        }

        let target = self.targets[self.current]
            .as_ref()
            .expect("target exists after ensure_target");

        // Opaque-black clear so letterbox bars aren't see-through and reused buffers don't show a
        // stale frame underneath the new one.
        let clear = [0.0f32, 0.0, 0.0, 1.0];
        unsafe { pl_tex_clear(placebo.gpu(), target.tex, clear.as_ptr()) };

        placebo
            .render_frame_to_tex(
                target.tex,
                target.width as i32,
                target.height as i32,
                decision.color,
                frame,
            )
            .map_err(|err| anyhow!("placebo render to dma-buf failed: {err}"))?;

        unsafe { pl_gpu_flush(placebo.gpu()) };

        // libplacebo leaves its own FBO + viewport bound; restore the default framebuffer that
        // Slint expects to render its UI into afterwards.
        unsafe {
            use glow::HasContext;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, width as i32, height as i32);
        }

        let wl = self.wl.as_ref().unwrap();
        wl.surface.attach(Some(&target.buffer), 0, 0);
        wl.surface.damage(0, 0, width as i32, height as i32);
        wl.surface.commit();
        let _ = wl.conn.flush();

        self.current = (self.current + 1) % self.targets.len();
        Ok(())
    }

    fn clear(&mut self) {
        debug!(fallback = self.use_swapchain_fallback, "EOS: clearing video surface");
        // Detach the last buffer so the compositor stops showing the previous stream's final
        // frame; attaching a null buffer unmaps the subsurface until we attach a real one again.
        // (In the swapchain-fallback path the subsurface is already unmapped and the final frame
        // lives in Slint's own surface, which lib.rs's per-tick `gl.clear` blanks to black.)
        if let Some(wl) = self.wl.as_ref() {
            wl.surface.attach(None, 0, 0);
            wl.surface.commit();
            let _ = wl.conn.flush();
        }
        // Force fresh buffers and re-application of the color description for the next stream.
        self.current = 0;
        self.applied_color = None;
    }

    fn needs_render_every_repaint(&self) -> bool {
        // A mapped subsurface keeps showing its last committed buffer across Slint repaints, so we
        // only need to re-render on a new frame or a resize — not on focus/cursor repaints. The
        // in-surface swapchain fallback shares Slint's surface (cleared every repaint), so it does.
        self.use_swapchain_fallback
    }

    fn get_clear_color(&self) -> [f32; 4] {
        if self.use_swapchain_fallback {
            // Video is composited into Slint's own surface; keep it opaque (black letterbox).
            [0.0, 0.0, 0.0, 1.0]
        } else {
            // Slint's surface must be transparent where the video shows through to the subsurface
            // below it. (Requires a transparent window + transparent video-player view background.)
            [0.0, 0.0, 0.0, 0.0]
        }
    }

    fn teardown(&mut self, placebo: &mut PlaceboContext) {
        for slot in self.targets.iter_mut() {
            if let Some(target) = slot.take() {
                destroy_target(placebo, target);
            }
        }
        if let Some(wl) = self.wl.take() {
            if let Some(bg) = wl.background {
                bg.viewport.destroy();
                bg.subsurface.destroy();
                bg.surface.destroy();
            }
            wl.subsurface.destroy();
            wl.surface.destroy();
            let _ = wl.conn.flush();
        }
    }
}
