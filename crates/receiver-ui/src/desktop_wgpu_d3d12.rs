//! Direct3D 12 import for the desktop wgpu video lane.
//!
//! A hardware decoder on windows decodes into an `ID3D12Resource`: the
//! `d3d12h264dec` family (and their HEVC, AV1, VP9 and MPEG-2 siblings) hand
//! that texture downstream as one `GstD3D12Memory`. Mapping it readable is
//! what makes GStreamer allocate a staging texture, copy the whole picture
//! into it on the GPU and then read it back over the bus, which at 4K60 is
//! most of a streaming thread before a single pixel has been uploaded again.
//!
//! This module takes the other route: the memory's resource is opened on the
//! device slint renders with, each of its planes is wrapped as its own
//! `wgpu::Texture`, and the pair is handed to `i-slint-video-wgpu` as frame
//! planes. The pixels are never read by the CPU. It is the windows half of
//! what [`crate::desktop_wgpu_dmabuf`] does for a VA-API decoder on linux and
//! [`crate::desktop_wgpu_iosurface`] does for VideoToolbox on mac.
//!
//! # The caps decide here, unlike on mac
//!
//! `vtdec` hands the same IOSurface-backed buffer out whether the caps say so
//! or not, which is why the mac arm can leave negotiation alone and ask the
//! memory. A d3d12 decoder does not: with a system-memory sink downstream it
//! downloads every frame into ordinary pages itself, and the texture never
//! leaves the decoder. So the appsink has to offer `memory:D3D12Memory` ahead
//! of the system-memory structures, exactly as the linux arm offers
//! `memory:DMABuf`, and a refused import narrows the offer back.
//!
//! # One memory, every plane
//!
//! This is the shape that differs most from the other two arms. A
//! `GstD3D12Memory` is ONE resource holding every plane as a D3D12 *plane
//! slice*, so a buffer carries a single memory rather than one per plane, and
//! the import is one `OpenSharedHandle` followed by one wgpu texture per
//! plane over that same resource. `wgpu::hal::dx12::Texture::with_plane_slice`
//! is what makes a single-plane texture out of a plane of a planar resource:
//! it pins the `PlaneSlice` of every view derived from it, so plane 0 samples
//! as `R8Unorm` luma and plane 1 as `Rg8Unorm` chroma off one NV12 resource.
//!
//! # Two devices, one adapter
//!
//! GStreamer's `GstD3D12Device` is not wgpu's `ID3D12Device` and cannot be
//! made to be: libgstd3d12 has no "wrap this device" entry point. The
//! resource therefore crosses as a shared NT handle, which is what
//! `gst_d3d12_memory_get_nt_handle` exists for, and which the decoder's
//! output pool is always allocated for (`D3D12_HEAP_FLAG_SHARED`, beside
//! `ALLOW_SIMULTANEOUS_ACCESS`).
//!
//! A shared handle only opens on the adapter it was created on, so the two
//! devices have to agree about which one that is. [`context_query`] is that
//! agreement: the sink answers the `GST_QUERY_CONTEXT` every d3d12 element
//! runs before it picks a device, naming a `GstD3D12Device` built for wgpu's
//! own adapter LUID. Without it a hybrid box would put the decoder on the
//! integrated GPU and wgpu on the discrete one, and every import would fail
//! into the upload arm.
//!
//! # Sync
//!
//! Unlike an IOSurface or a dmabuf, a d3d12 buffer arrives with decode work
//! possibly still in flight, and it says so: the memory carries a fence.
//! [`sync`] waits for it before the frame is handed over. That is a CPU wait
//! on the streaming thread, which is where GStreamer's own map would have put
//! it, and by the time a buffer has travelled the decoder's output queue it
//! has normally already been signalled. Waiting on wgpu's own queue instead
//! would need the fence reopened on our device and an `ID3D12CommandQueue::
//! Wait` slipped in ahead of slint's submit; it is the better answer and it
//! is not needed to be correct.
//!
//! # State
//!
//! The planes are imported as `TextureUses::RESOURCE` rather than
//! `UNINITIALIZED`, which is deliberate and load bearing. wgpu emits a
//! transition whenever the state it tracks differs from the state a use
//! wants, and it transitions `D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES` — so
//! two plane wrappers over one resource would each transition the whole
//! resource, the second one claiming a "before" state the first had already
//! left. Declaring the state the planes are actually used in means no barrier
//! is emitted at all, and D3D12's own common-state promotion covers the
//! transition: the resource is in `COMMON` when it crosses devices, and
//! `COMMON` promotes to a shader-read state on first use and decays back at
//! the end of the command list. `ALLOW_SIMULTANEOUS_ACCESS`, which the
//! decoder sets, makes that promotion unconditional.
//!
//! # Per resource, not per frame
//!
//! An import is one open plus one texture per plane over memory that already
//! exists, and the decoder cycles a fixed pool, so [`ImportCache`] keeps one
//! built frame per resource and the steady state is a key compare. See its
//! docs for what the identity is and when entries die.

use gst::prelude::*;
use i_slint_video_wgpu::{Frame, FrameDesc, PixelFormat};
use std::ffi::c_void;
use std::sync::Arc;
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, LUID, RECT};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Resource};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::D3D12CreateDevice;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_P016,
};

/// Planes a format this module imports can have. Both of them are biplanar;
/// the third is slack so a planar format added to [`importable`] later fails
/// its geometry check rather than this array's bound.
pub const MAX_PLANES: usize = 3;

/// The caps feature a d3d12 buffer travels under,
/// `GST_CAPS_FEATURE_MEMORY_D3D12_MEMORY`.
pub const CAPS_FEATURE_MEMORY_D3D12: &str = "memory:D3D12Memory";

/// Keeps the D3D12 runtime loaded for the life of the process.
///
/// `D3D12Core.dll` is loaded when the first device is created and unloaded
/// with the last one, and something in the stack keeps calling into it after
/// that: on a box whose only D3D12 adapter is WARP, wgpu's DX12 pass declines
/// the CPU adapter and drops its instance, the GL floor then comes up on
/// ANGLE -- which is D3D backed itself -- and the process dies with an access
/// violation inside an unloaded `D3D12Core.dll` before the receiver has
/// logged its first line.
///
/// One device held forever is all it takes: the refcount never reaches zero,
/// so no unload happens and there is no stale mapping to jump into. It is
/// created on the same default adapter D3D12 would pick, and falls back to
/// WARP so that the pin exists on exactly the boxes that need it. Failure is
/// not an error -- a box with no D3D12 at all has nothing to unload.
///
/// Called before the DX12 backend is instantiated, which is the first moment
/// anything here can load it.
pub fn pin_runtime() {
    static PINNED: std::sync::OnceLock<Pinned> = std::sync::OnceLock::new();

    /// The held device. Only ever created and kept.
    struct Pinned(#[allow(dead_code)] Option<ID3D12Device>);
    // SAFETY: an ID3D12Device is free threaded, and this one is never called
    // through -- it exists to hold a reference.
    unsafe impl Send for Pinned {}
    unsafe impl Sync for Pinned {}

    PINNED.get_or_init(|| {
        let mut device: Option<ID3D12Device> = None;
        // The default adapter first, WARP second, which is what the runtime
        // itself would fall back to.
        let made = unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device) }.is_ok();
        if !made {
            if let Ok(factory) = unsafe { CreateDXGIFactory1() } {
                let factory: IDXGIFactory4 = factory;
                if let Ok(warp) = unsafe { factory.EnumWarpAdapter::<IDXGIAdapter1>() } {
                    let _ = unsafe {
                        D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_11_0, &mut device)
                    };
                }
            }
        }
        if device.is_none() {
            tracing::debug!("wgpu video lane: no d3d12 device to pin the runtime with");
        }
        Pinned(device)
    });
}

/// Force the lane back onto the upload arm. `FCAST_DESKTOP_WGPU_D3D12=0`
/// leaves the appsink advertising system memory only, which is the A/B
/// control for the whole path and the escape hatch on a driver that
/// mis-imports.
pub fn enabled() -> bool {
    !std::env::var("FCAST_DESKTOP_WGPU_D3D12").is_ok_and(|v| v == "0")
}

/// Whether a frame of this format can be imported at all.
///
/// The biplanar layouts a d3d12 decoder decodes into: NV12 for 8-bit, P010
/// for 10-bit and P016 for 12/16-bit. Nothing else in the decoders' src
/// template is content the receiver sees, and each would need its own plane
/// format proof, so it takes the upload arm. The wide ones are 16-bit norm
/// planes, which is a device feature.
pub fn importable(format: PixelFormat, norm16: bool) -> bool {
    match format {
        PixelFormat::Nv12 => true,
        PixelFormat::P010 | PixelFormat::P016 => norm16,
        _ => false,
    }
}

/// The DXGI format a buffer of this pixel format must really be, so a
/// resource whose planes are laid out differently than the caps claim is
/// refused instead of sampled as the wrong thing.
fn dxgi_format(format: PixelFormat) -> Option<DXGI_FORMAT> {
    Some(match format {
        PixelFormat::Nv12 => DXGI_FORMAT_NV12,
        PixelFormat::P010 => DXGI_FORMAT_P010,
        PixelFormat::P016 => DXGI_FORMAT_P016,
        _ => return None,
    })
}

// libgstd3d12-1.0. The static gstreamer-full exports it on windows
// (`gst-full-libraries` carries gstreamer-d3d12-1.0, the way it carries
// gstreamer-iosurface-1.0 on mac), so there is nothing to link here.
// Re-diff gst/d3d12/gstd3d12memory.h and gstd3d12utils.h against these on a
// gst bump.
//
// Every one of these takes a `GstD3D12Memory *`, which is a `GstMemory`
// subclass, so the pointer is the one gst-rs already hands out.
unsafe extern "C" {
    fn gst_is_d3d12_memory(mem: *mut gst::ffi::GstMemory) -> gst::glib::ffi::gboolean;
    fn gst_d3d12_memory_sync(mem: *mut gst::ffi::GstMemory) -> gst::glib::ffi::gboolean;
    fn gst_d3d12_memory_get_resource_handle(mem: *mut gst::ffi::GstMemory) -> *mut c_void;
    fn gst_d3d12_memory_get_nt_handle(
        mem: *mut gst::ffi::GstMemory,
        handle: *mut HANDLE,
    ) -> gst::glib::ffi::gboolean;
    fn gst_d3d12_memory_get_plane_count(mem: *mut gst::ffi::GstMemory) -> u32;
    fn gst_d3d12_memory_get_plane_rectangle(
        mem: *mut gst::ffi::GstMemory,
        plane: u32,
        rect: *mut RECT,
    ) -> gst::glib::ffi::gboolean;
    fn gst_d3d12_memory_get_subresource_index(
        mem: *mut gst::ffi::GstMemory,
        plane: u32,
        index: *mut u32,
    ) -> gst::glib::ffi::gboolean;

    fn gst_d3d12_device_new_for_adapter_luid(luid: i64) -> *mut gst::ffi::GstObject;
    fn gst_d3d12_luid_to_int64(luid: *const LUID) -> i64;
    fn gst_d3d12_handle_context_query(
        element: *mut gst::ffi::GstElement,
        query: *mut gst::ffi::GstQuery,
        device: *mut gst::ffi::GstObject,
    ) -> gst::glib::ffi::gboolean;
}

/// Whether this buffer's memory is a d3d12 resource, which is what opens the
/// import route. One call on the first memory: every plane of a d3d12 buffer
/// lives in that one memory, so there is nothing else to ask.
pub fn is_d3d12(buffer: &gst::BufferRef) -> bool {
    buffer.n_memory() > 0
        && unsafe { gst_is_d3d12_memory(buffer.peek_memory(0).as_mut_ptr()) } != 0
}

/// Waits for the decode work behind this buffer, which is what makes the
/// planes safe to sample. Cheap once the fence has been signalled, which by
/// the time a buffer has crossed the decoder's output queue it usually has.
///
/// Called per frame rather than per import: a cache hit is the same resource
/// with new pixels in it, and those pixels are what the fence is about.
pub fn sync(buffer: &gst::BufferRef) -> Result<(), &'static str> {
    if buffer.n_memory() == 0 {
        return Err("a d3d12 buffer must carry a memory");
    }
    if unsafe { gst_d3d12_memory_sync(buffer.peek_memory(0).as_mut_ptr()) } == 0 {
        return Err("waiting for the decoder's fence failed");
    }
    Ok(())
}

/// GStreamer's device for the adapter wgpu ended up on, built once and
/// handed to every d3d12 element that asks.
///
/// `None` when the device is not a D3D12 one (the lane fell through to
/// vulkan or gl), or when GStreamer cannot open that adapter, and then the
/// appsink offers no d3d12 structures and never answers a context query.
pub struct Device {
    /// `GstD3D12Device`, held as the `GstObject` it derives from so the
    /// refcount and `Send`/`Sync` are gst-rs's rather than hand rolled.
    gst: gst::Object,
    /// wgpu's own device handle, which is what a resource is opened on.
    raw: ID3D12Device,
}

// SAFETY: both halves are refcounted handles that are safe to move between
// threads. `gst::Object` already says so; `ID3D12Device` is a COM object
// whose methods are free threaded, and the only one used off the UI thread
// is `OpenSharedHandle`.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Builds the pairing for a wgpu device, or `None` when there is no
    /// import route on it.
    pub fn new(device: &wgpu::Device) -> Option<Device> {
        // Cloned out of the guard: the import holds the device across calls
        // that can allocate, and the hal guard is a lock on wgpu's side.
        let raw = {
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }?;
            hal.raw_device().clone()
        };
        let luid = unsafe { raw.GetAdapterLuid() };
        let luid = unsafe { gst_d3d12_luid_to_int64(&luid) };
        let gst = unsafe { gst_d3d12_device_new_for_adapter_luid(luid) };
        if gst.is_null() {
            tracing::warn!(luid, "wgpu video lane: gstreamer has no d3d12 device for this adapter");
            return None;
        }
        // transfer full, which is what makes this the owning handle
        let gst = unsafe { gst::glib::translate::from_glib_full(gst) };
        Some(Device { gst, raw })
    }

    /// Answers the `GST_QUERY_CONTEXT` a d3d12 element runs before it picks
    /// its device, which is how the decoder ends up on wgpu's adapter.
    ///
    /// `true` when the query was ours to answer, which is what the pad's
    /// handler returns instead of chaining to the default.
    pub fn context_query(&self, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        unsafe {
            gst_d3d12_handle_context_query(
                element.as_ptr(),
                query.as_mut_ptr(),
                self.gst.as_ptr(),
            ) != 0
        }
    }
}

/// Puts the sink in the way of the `GST_QUERY_CONTEXT` every d3d12 element
/// runs before it picks a device, which is how the decoder ends up on wgpu's
/// adapter.
///
/// A d3d12 element queries downstream and upstream first and only posts a
/// `NEED_CONTEXT` message when neither answered. That message goes to the
/// pipeline's bus, which belongs to the player and not to the lane, so this
/// query is the one moment the lane can be heard at all.
pub fn answer_context_query(pad: &gst::Pad, element: &gst::Element, device: Arc<Device>) {
    let element = element.clone();
    pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Query(query)) = &mut info.data else {
            return gst::PadProbeReturn::Ok;
        };
        if !matches!(query.view(), gst::QueryView::Context(_)) {
            return gst::PadProbeReturn::Ok;
        }
        if device.context_query(&element, query) {
            // answered here, so nothing downstream of the probe sees it
            return gst::PadProbeReturn::Handled;
        }
        gst::PadProbeReturn::Ok
    });
}

/// What makes two frames the same decoder texture.
///
/// The `ID3D12Resource` address is the identity of the texture, and a cached
/// entry pins it: the entry holds a reference of its own, so the address
/// cannot be recycled for a different resource while the cache could still
/// match on it. Holding it does not keep the `GstMemory` alive and is not
/// meant to; the decoder handing the same texture round with new pixels in
/// it is exactly the case the cache exists for. The `GstMemory` address
/// would be cheaper and is not ours to pin, the same way it is not on the
/// dmabuf path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ResourceKey(usize);

/// One buffer's d3d12 texture, plus the identity a cached import matches on.
/// The handles are borrowed from the gst memory and are only good while the
/// buffer they were read off is alive, which is why nothing stores one.
#[derive(Clone, Copy)]
pub struct FrameSource {
    /// The gst-side `ID3D12Resource`, borrowed. Only ever compared and
    /// reference counted, never called.
    resource: *mut c_void,
    /// The shareable NT handle, owned by the memory.
    handle: HANDLE,
    /// The memory the two came off, for the per-plane geometry queries.
    mem: *mut gst::ffi::GstMemory,
    /// How many planes the resource says it has.
    planes: usize,
    key: ResourceKey,
}

impl FrameSource {
    /// One plane's size as the resource itself reports it, which is the
    /// ALLOCATED size: a decoder rounds the coded size up to its codec's
    /// alignment, so a 1080p stream arrives in a 1920x1088 texture and this
    /// says 1088.
    fn plane_rect(&self, plane: u32) -> Option<(u32, u32)> {
        let mut rect = RECT::default();
        if unsafe { gst_d3d12_memory_get_plane_rectangle(self.mem, plane, &mut rect) } == 0 {
            return None;
        }
        Some((
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    }

    /// The D3D12 plane slice for a plane, which for the single-mip,
    /// non-array textures a decoder outputs is the subresource index itself.
    fn plane_slice(&self, plane: u32) -> Option<u32> {
        let mut index = 0u32;
        if unsafe { gst_d3d12_memory_get_subresource_index(self.mem, plane, &mut index) } == 0 {
            return None;
        }
        Some(index)
    }
}

/// Locates the buffer's d3d12 texture.
///
/// A d3d12 buffer is one memory holding every plane, so this checks that
/// shape rather than walking memories the way the other two arms do. A
/// buffer shaped any other way is refused instead of guessed at.
pub fn frame_source(
    buffer: &gst::BufferRef,
    info: &gst_video::VideoInfo,
) -> Result<FrameSource, &'static str> {
    let n = info.n_planes() as usize;
    if n > MAX_PLANES {
        return Err("more planes than the import path carries");
    }
    if buffer.n_memory() != 1 {
        return Err("a d3d12 buffer must carry exactly one memory");
    }
    let mem = buffer.peek_memory(0).as_mut_ptr();
    if unsafe { gst_is_d3d12_memory(mem) } == 0 {
        return Err("buffer memory is not d3d12");
    }
    let planes = unsafe { gst_d3d12_memory_get_plane_count(mem) } as usize;
    if planes != n {
        return Err("the resource has a different plane count than the caps");
    }
    let resource = unsafe { gst_d3d12_memory_get_resource_handle(mem) };
    if resource.is_null() {
        return Err("buffer memory exposes no d3d12 resource");
    }
    let mut handle = HANDLE(std::ptr::null_mut());
    if unsafe { gst_d3d12_memory_get_nt_handle(mem, &mut handle) } == 0 || handle.is_invalid() {
        return Err("the decoder's texture is not shareable");
    }
    Ok(FrameSource {
        resource,
        handle,
        mem,
        planes,
        key: ResourceKey(resource as usize),
    })
}

/// Imports one frame's planes as wgpu textures.
///
/// Opening the handle gives this device its own `ID3D12Resource` over the
/// decoder's memory; the plane textures hold it, so dropping them is what
/// lets it go. What they do not keep alive is the gst buffer, so the caller
/// holds that for as long as the planes can still be sampled.
///
/// The resource's own geometry has to cover what the frame desc says, or the
/// renderer would sample a texture describing a different picture than the
/// caps do. Larger is allowed and normal, see [`FrameSource::plane_rect`];
/// smaller, a different DXGI format, an array or a mip chain are errors
/// rather than clamps, and the caller uploads that frame instead.
pub fn import(
    device: &Device,
    wgpu_device: &wgpu::Device,
    desc: &FrameDesc,
    source: &FrameSource,
) -> Result<Vec<wgpu::Texture>, String> {
    let (geom, n) = i_slint_video_wgpu::gpu::plane_geometry(desc);
    if source.planes != n {
        return Err(format!(
            "{} planes on the resource for a {n} plane format",
            source.planes
        ));
    }
    let Some(want) = dxgi_format(desc.format) else {
        return Err(format!("{:?} has no d3d12 import", desc.format));
    };

    let mut opened: Option<ID3D12Resource> = None;
    unsafe { device.raw.OpenSharedHandle(source.handle, &mut opened) }
        .map_err(|e| format!("the decoder's texture would not open here: {e}"))?;
    let opened = opened.ok_or_else(|| "OpenSharedHandle gave nothing back".to_string())?;

    // What the wrappers below assume: one mip, one slice, and the layout the
    // caps claim. A decoder's output pool is allocated this way (its DPB may
    // be an array, but that pool is reference-only and never travels), so a
    // resource shaped otherwise is something else entirely.
    let rdesc = unsafe { opened.GetDesc() };
    if rdesc.Format != want {
        return Err(format!(
            "the resource is {:?}, the caps say {:?}",
            rdesc.Format, desc.format
        ));
    }
    if rdesc.MipLevels != 1 || rdesc.DepthOrArraySize != 1 {
        return Err(format!(
            "the resource is a {} slice, {} mip texture",
            rdesc.DepthOrArraySize, rdesc.MipLevels
        ));
    }

    let mut out = Vec::with_capacity(n);
    for (i, g) in geom.iter().take(n).enumerate() {
        let plane = i as u32;
        let (width, height) = source
            .plane_rect(plane)
            .ok_or_else(|| format!("plane {i} has no rectangle"))?;
        // The decoder's alignment padding lives here: the texture covers the
        // picture and may be taller or wider than it. The renderer scales
        // its uv onto the real size; anything SHORTER than the picture would
        // be sampled off the end of the plane.
        if width < g.width || height < g.height {
            return Err(format!(
                "plane {i} is {width}x{height}, smaller than the {}x{} the caps say",
                g.width, g.height
            ));
        }
        let slice = source
            .plane_slice(plane)
            .ok_or_else(|| format!("plane {i} has no subresource index"))?;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let hal = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                opened.clone(),
                g.format,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            )
        }
        .with_plane_slice(slice);
        out.push(unsafe {
            wgpu_device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                hal,
                &wgpu::TextureDescriptor {
                    label: Some("fcast-d3d12-plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: g.format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                // The state the planes are used in, NOT `UNINITIALIZED`: see
                // the module docs. Declaring it is what keeps wgpu from
                // emitting a whole-resource barrier per plane wrapper.
                wgpu::TextureUses::RESOURCE,
            )
        });
    }
    Ok(out)
}

/// Imported frames kept at once.
///
/// This has to cover the decoder's whole output pool, because the pool is
/// walked in a cycle: at one entry short of it every lookup lands on the
/// entry just evicted and the cache hits nothing at all. The pool is the
/// codec's DPB plus what downstream holds, which for the lane's two-deep
/// appsink and slint's three generations leaves this well above what
/// playback asks for. A pool past it still renders, it just stops hitting,
/// which [`ImportCache`] says once in the log.
pub const MAX_IMPORTS: usize = 32;

/// One resource's imported planes, built once and rendered from for as long
/// as the decoder keeps handing that texture round.
struct Entry {
    key: ResourceKey,
    frame: Frame,
    /// A reference on the gst-side resource, held only so its address cannot
    /// be reused by a different one while [`Entry::key`] could still match.
    _pin: ID3D12Resource,
}

/// Imported plane textures, one entry per resource the decoder cycles.
///
/// A miss costs one `OpenSharedHandle` and one texture per plane, plus the
/// views and the bind group the renderer then builds. A hit costs a key
/// compare, which is the whole per-frame cost of the zero-copy route once a
/// stream has settled, beside the fence wait every frame pays.
///
/// # What makes reuse sound
///
/// The decoder writes new pixels into the same resource between two hits and
/// the cached textures read that memory, so a hit shows the new frame. They
/// are views onto the resource, not copies of its contents, and nothing
/// about them goes stale while it lives. wgpu tracks them like any other
/// resource, so reuse across submissions is what it already does for every
/// scratch texture in the renderer.
///
/// # Invalidation
///
/// - A desc change clears everything: plane geometry and format both come
///   from it, so no entry can outlive it.
/// - End of stream, a refused import and the sink dropping its held buffers
///   clear it, which is also what releases the opened resources.
/// - An entry is only ever matched by the resource its own import pinned, so
///   a texture the decoder has freed cannot be mistaken for a live one.
/// - Full is round-robin: a pool larger than [`MAX_IMPORTS`] keeps rendering
///   and stops hitting, it never renders the wrong frame.
pub struct ImportCache {
    entries: Vec<Entry>,
    next: usize,
    /// What every entry was built for.
    built_for: Option<FrameDesc>,
    /// Lookups that found an entry and lookups that had to import, for the
    /// steady-state tests and the log line.
    pub hits: u64,
    pub misses: u64,
    /// Imports dropped to make room, which should stay zero.
    pub evictions: u64,
}

impl Default for ImportCache {
    fn default() -> Self {
        Self {
            entries: Vec::with_capacity(MAX_IMPORTS),
            next: 0,
            built_for: None,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }
}

impl ImportCache {
    /// The cached frame for this resource, importing it first if the texture
    /// has not been seen. The index stays valid until the next call.
    pub fn get_or_import(
        &mut self,
        device: &Device,
        wgpu_device: &wgpu::Device,
        desc: &FrameDesc,
        source: &FrameSource,
    ) -> Result<usize, String> {
        if self.built_for != Some(*desc) {
            self.clear();
            self.built_for = Some(*desc);
        }
        if let Some(i) = self.entries.iter().position(|e| e.key == source.key) {
            self.hits += 1;
            return Ok(i);
        }
        self.misses += 1;
        let planes = import(device, wgpu_device, desc, source)?;
        let frame = Frame::from_planes(*desc, planes).map_err(|e| e.to_string())?;
        // An owned reference on the decoder's resource, taken only after the
        // import succeeded so a refusal leaves nothing pinned.
        let pin = unsafe { <ID3D12Resource as Interface>::from_raw_borrowed(&source.resource) }
            .ok_or_else(|| "the decoder's resource went away mid import".to_string())?
            .clone();
        let entry = Entry {
            key: source.key,
            frame,
            _pin: pin,
        };
        if self.entries.len() < MAX_IMPORTS {
            self.entries.push(entry);
            return Ok(self.entries.len() - 1);
        }
        // A pool larger than the cache is walked in a cycle, so every lookup
        // lands on what was just dropped and nothing hits again. Worth a
        // line, since the only fix is a bigger cache.
        self.evictions += 1;
        if self.evictions == 1 {
            tracing::warn!(
                kept = MAX_IMPORTS,
                "wgpu video lane: the decoder's pool is larger than the import cache, \
                 frames will be re-imported"
            );
        }
        let slot = self.next;
        self.entries[slot] = entry;
        self.next = (slot + 1) % MAX_IMPORTS;
        Ok(slot)
    }

    pub fn frame(&self, index: usize) -> &Frame {
        &self.entries[index].frame
    }

    /// How many distinct resources are held, which is the decoder's pool
    /// size once a stream has cycled it. Only the tests ask.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drops every import, which releases the resources it opened. wgpu
    /// keeps a texture alive until the submissions that read it have
    /// retired, so this cannot pull one out from under the gpu.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next = 0;
        self.built_for = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The import list and the DXGI table have to agree: every format the
    /// lane offers to a decoder must have a resource layout the import can
    /// name, or a negotiated stream would reach `import` and be refused
    /// every frame.
    #[test]
    fn every_importable_format_has_a_dxgi_layout() {
        for format in [
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::P016,
            PixelFormat::I420,
            PixelFormat::Rgba,
        ] {
            if importable(format, true) {
                assert!(
                    dxgi_format(format).is_some(),
                    "{format:?} is offered but has no dxgi format"
                );
            }
        }
    }

    /// The wide formats ride on the 16-bit norm device feature, so an
    /// adapter without it must not be offered them.
    #[test]
    fn the_wide_formats_need_the_norm16_feature() {
        assert!(importable(PixelFormat::Nv12, false));
        assert!(!importable(PixelFormat::P010, false));
        assert!(!importable(PixelFormat::P016, false));
        assert!(importable(PixelFormat::P010, true));
    }

    /// Both importable layouts are biplanar, so the plane arrays sized off
    /// [`MAX_PLANES`] cannot overflow for anything the lane offers.
    #[test]
    fn importable_formats_fit_the_plane_array() {
        for format in [PixelFormat::Nv12, PixelFormat::P010, PixelFormat::P016] {
            assert!(format.plane_count() <= MAX_PLANES, "{format:?}");
        }
    }
}
