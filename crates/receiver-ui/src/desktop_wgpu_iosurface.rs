//! IOSurface import for the desktop wgpu video lane.
//!
//! VideoToolbox decodes into CVPixelBuffers, and every one of them is backed
//! by an IOSurface: a kernel handle the GPU can already read. `vtdec` hands
//! them downstream as one `GstAppleCoreVideoMemory` per plane, and mapping
//! one readable is what makes CoreVideo lock the surface and hand back CPU
//! pages. That map, plus the upload that follows it, is a whole frame across
//! the bus twice per picture, which at 4K60 is most of a streaming thread.
//!
//! This module takes the other route: each plane's surface is wrapped as its
//! own `MTLTexture` with `newTextureWithDescriptor:iosurface:plane:`, turned
//! into a `wgpu::Texture` on the device slint renders with, and handed to
//! `i-slint-video-wgpu` as frame planes. The pixels are never read by the
//! CPU. It is the mac half of what [`crate::desktop_wgpu_dmabuf`] does for a
//! VA-API decoder on linux.
//!
//! # The memory decides, not the caps
//!
//! `vtdec` advertises `video/x-raw(memory:IOSurface)` as well as plain system
//! memory, but the buffers are the same object either way: the caps feature
//! only tells downstream what it is allowed to assume. The lane therefore
//! offers system memory alone, exactly as it did before this module, and asks
//! the memory itself whether it carries a surface. Two things follow.
//!
//! Negotiation does not move, so nothing upstream of the sink sees a new
//! shape to intersect and no convert bin has to be taught a feature. And the
//! fallback is free: a surface the import refuses is mapped and uploaded like
//! any other frame, which is sound here in a way it is not for a VA surface,
//! because a CVPixelBuffer maps to ordinary pixels rather than the decoder's
//! tiling.
//!
//! # Per plane, not per frame
//!
//! An import is one `MTLTexture` per plane over memory that already exists,
//! and the decoder cycles a fixed pool of pixel buffers, so [`ImportCache`]
//! keeps one built frame per surface and the steady state is a key compare.
//! See its docs for what the identity is and when entries die.
//!
//! # Sync
//!
//! Nothing here expresses ordering and nothing needs to: VideoToolbox has
//! finished writing the surface before it hands the buffer out, and IOSurface
//! coherency between the decode engine and Metal is the kernel's, which is
//! why GStreamer's own GL path does no extra sync either.

use i_slint_video_wgpu::{Frame, FrameDesc, PixelFormat};
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};

/// Planes a format this module imports can have. Both of them are biplanar;
/// the third is slack so a planar format added to [`importable`] later fails
/// its geometry check rather than this array's bound.
pub const MAX_PLANES: usize = 3;

/// Force the lane back onto the upload arm. `FCAST_DESKTOP_WGPU_IOSURFACE=0`
/// is the A/B control for the whole path and the escape hatch on a driver
/// that mis-imports.
pub fn enabled() -> bool {
    !std::env::var("FCAST_DESKTOP_WGPU_IOSURFACE").is_ok_and(|v| v == "0")
}

/// Whether a frame of this format can be imported at all.
///
/// The two biplanar layouts VideoToolbox decodes into: NV12 for 8-bit and
/// P010 for 10-bit. Everything else in vtdec's template (the packed 64-bit
/// RGB formats, AV12) is content the receiver almost never sees, and each
/// would need its own Metal pixel format proof, so it takes the upload arm.
/// P010's planes are 16-bit norms, which is a device feature.
pub fn importable(format: PixelFormat, norm16: bool) -> bool {
    match format {
        PixelFormat::Nv12 => true,
        PixelFormat::P010 => norm16,
        _ => false,
    }
}

// libgstiosurface-1.0, unstable GStreamer API ("Since: 1.30",
// gst/iosurface/gstiosurface.h). The static gstreamer-full exports it on
// macOS (`gst-full-libraries` carries gstreamer-iosurface-1.0), so there is
// nothing to link here. Re-diff the header against these two on a gst bump.
unsafe extern "C" {
    fn gst_is_iosurface_memory(mem: *mut gst::ffi::GstMemory) -> gst::glib::ffi::gboolean;
    fn gst_iosurface_memory_peek_surface(
        mem: *mut gst::ffi::GstMemory,
        surface: *mut *const IOSurfaceRef,
        plane: *mut u32,
    ) -> gst::glib::ffi::gboolean;
}

/// Whether this buffer's memory carries an IOSurface, which is what opens
/// the import route. One call on the first memory: a CVPixelBuffer's planes
/// all come from the same allocator, so they answer alike.
pub fn is_iosurface(buffer: &gst::BufferRef) -> bool {
    buffer.n_memory() > 0
        && unsafe { gst_is_iosurface_memory(buffer.peek_memory(0).as_mut_ptr()) } != 0
}

/// What makes two frames the same decoder surface.
///
/// The `IOSurfaceRef` address is the identity of the underlying surface, and
/// it cannot be reused by another one while something holds a reference,
/// which a cached import does: `newTextureWithDescriptor:iosurface:plane:`
/// retains the surface for the life of the texture. The `GstMemory` address
/// would be cheaper and is not ours to pin, the same way it is not on the
/// dmabuf path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlaneKey {
    surface: usize,
    plane: u32,
}

/// One plane's surface, plus the identity a cached import matches on. The
/// reference is borrowed from the gst memory and is only good while the
/// buffer it was read off is alive, which is why nothing stores one.
#[derive(Clone, Copy)]
pub struct PlaneSource {
    surface: *const IOSurfaceRef,
    key: PlaneKey,
}

impl PlaneSource {
    pub const EMPTY: PlaneSource = PlaneSource {
        surface: std::ptr::null(),
        key: PlaneKey {
            surface: 0,
            plane: 0,
        },
    };

    pub fn plane(&self) -> u32 {
        self.key.plane
    }
}

/// Locates each plane's surface in the buffer's memories.
///
/// `gst_core_video_wrap_pixel_buffer` appends one memory per plane, in plane
/// order, so the two lists line up; the plane index the query reports is used
/// rather than the position, since it is what the surface itself is indexed
/// by. A buffer shaped any other way is refused instead of guessed at.
///
/// Writes into the caller's array and returns how many planes it filled, so
/// the per-frame path allocates nothing.
pub fn plane_sources(
    buffer: &gst::BufferRef,
    info: &gst_video::VideoInfo,
    out: &mut [PlaneSource; MAX_PLANES],
) -> Result<usize, &'static str> {
    let n = info.n_planes() as usize;
    if n > MAX_PLANES {
        return Err("more planes than the import path carries");
    }
    if buffer.n_memory() != n {
        return Err("an iosurface buffer must carry one memory per plane");
    }
    for (i, slot) in out.iter_mut().take(n).enumerate() {
        let mut surface: *const IOSurfaceRef = std::ptr::null();
        let mut plane: u32 = 0;
        let got = unsafe {
            gst_iosurface_memory_peek_surface(
                buffer.peek_memory(i).as_mut_ptr(),
                &mut surface,
                &mut plane,
            )
        };
        if got == 0 || surface.is_null() {
            return Err("buffer memory exposes no iosurface");
        }
        *slot = PlaneSource {
            surface,
            key: PlaneKey {
                surface: surface as usize,
                plane,
            },
        };
    }
    Ok(n)
}

/// The Metal pixel format for a plane texture, as the crate's plane table
/// spells it in wgpu's vocabulary. `None` for anything [`importable`] does
/// not offer.
fn metal_format(format: wgpu::TextureFormat) -> Option<MTLPixelFormat> {
    Some(match format {
        wgpu::TextureFormat::R8Unorm => MTLPixelFormat::R8Unorm,
        wgpu::TextureFormat::Rg8Unorm => MTLPixelFormat::RG8Unorm,
        wgpu::TextureFormat::R16Unorm => MTLPixelFormat::R16Unorm,
        wgpu::TextureFormat::Rg16Unorm => MTLPixelFormat::RG16Unorm,
        _ => return None,
    })
}

/// One plane's size and texel width as the surface itself reports them.
/// Every format [`importable`] lists is planar, so the plane index is always
/// a real one.
fn surface_plane(surface: &IOSurfaceRef, plane: usize) -> (u32, u32, u32) {
    (
        surface.width_of_plane(plane) as u32,
        surface.height_of_plane(plane) as u32,
        surface.bytes_per_element_of_plane(plane) as u32,
    )
}

/// Imports one frame's planes as wgpu textures.
///
/// Each texture retains its surface, so dropping the textures is what lets
/// the pixel buffer go back to VideoToolbox's pool. What they do not keep
/// alive is the gst buffer, so the caller holds that for as long as the
/// planes can still be sampled.
///
/// The surface's own plane geometry has to match what the frame desc says,
/// or the renderer would sample a texture that describes a different picture
/// than the caps do. A mismatch is an error rather than a clamp: the caller
/// uploads that frame instead, which is always correct.
pub fn import(
    device: &wgpu::Device,
    desc: &FrameDesc,
    sources: &[PlaneSource],
) -> Result<Vec<wgpu::Texture>, String> {
    let (geom, n) = i_slint_video_wgpu::gpu::plane_geometry(desc);
    if sources.len() != n {
        return Err(format!(
            "{} plane sources for a {n} plane format",
            sources.len()
        ));
    }
    // Cloned out of the guard: the import holds the device across calls that
    // can allocate, and the hal guard is a lock on wgpu's side.
    let raw = {
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
            .ok_or_else(|| "device is not metal".to_string())?;
        hal.raw_device().clone()
    };
    // What the descriptor has to declare for a texture over shared storage:
    // an IOSurface is CPU-visible memory, so Private is refused outright, and
    // a discrete GPU needs the managed mirror.
    let storage = if raw.hasUnifiedMemory() {
        MTLStorageMode::Shared
    } else {
        MTLStorageMode::Managed
    };
    let mut out = Vec::with_capacity(n);
    for (i, (src, g)) in sources.iter().zip(geom.iter().take(n)).enumerate() {
        let surface = unsafe { &*src.surface };
        let plane = src.plane() as usize;
        let (width, height, bytes) = surface_plane(surface, plane);
        if (width, height) != (g.width, g.height) {
            return Err(format!(
                "plane {i} surface is {width}x{height}, the caps say {}x{}",
                g.width, g.height
            ));
        }
        if bytes != g.bytes_per_texel {
            return Err(format!(
                "plane {i} surface has {bytes} bytes per texel, the format has {}",
                g.bytes_per_texel
            ));
        }
        let Some(pixel_format) = metal_format(g.format) else {
            return Err(format!(
                "plane {i} format {:?} has no metal texture",
                g.format
            ));
        };
        let td = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                pixel_format,
                width as usize,
                height as usize,
                false,
            )
        };
        td.setUsage(MTLTextureUsage::ShaderRead);
        td.setStorageMode(storage);
        let texture = raw
            .newTextureWithDescriptor_iosurface_plane(&td, surface, plane)
            .ok_or_else(|| format!("plane {i}: metal refused the surface"))?;
        let size = wgpu::Extent3d {
            width: g.width,
            height: g.height,
            depth_or_array_layers: 1,
        };
        let hal_tex = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                texture,
                g.format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width: g.width,
                    height: g.height,
                    depth: 1,
                },
                None,
            )
        };
        out.push(unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("fcast-iosurface-plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: g.format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                // Metal has no layouts to transition, and the decoder's
                // pixels are already there; this only tells wgpu it need not
                // initialize the texture itself.
                wgpu::TextureUses::UNINITIALIZED,
            )
        });
    }
    Ok(out)
}

/// Imported frames kept at once.
///
/// This has to cover VideoToolbox's whole pixel buffer pool, because the pool
/// is walked in a cycle: at one entry short of it every lookup lands on the
/// entry just evicted and the cache hits nothing at all. The pool is the
/// codec's DPB plus what downstream holds, which for the lane's two-deep
/// appsink and slint's three generations leaves this well above what playback
/// asks for. A pool past it still renders, it just stops hitting, which
/// [`ImportCache`] says once in the log.
pub const MAX_IMPORTS: usize = 32;

/// One surface's imported planes, built once and rendered from for as long as
/// VideoToolbox keeps handing that pixel buffer round.
struct Entry {
    keys: [PlaneKey; MAX_PLANES],
    n: usize,
    frame: Frame,
}

/// Imported plane textures, one entry per surface the decoder cycles.
///
/// A miss costs one `MTLTexture` per plane plus the views and the bind group
/// the renderer then builds. A hit costs a key compare, which is the whole
/// per-frame cost of the zero-copy route once a stream has settled.
///
/// # What makes reuse sound
///
/// VideoToolbox writes new pixels into the same surface between two hits and
/// the cached texture reads that memory, so a hit shows the new frame. The
/// texture is a view onto the surface, not a copy of its contents, and
/// nothing about it goes stale while the surface lives. wgpu tracks the
/// textures like any other resource, so reuse across submissions is what it
/// already does for every scratch texture in the renderer.
///
/// # Invalidation
///
/// - A desc change clears everything: plane geometry and format both come from
///   it, so no entry can outlive it.
/// - End of stream, a refused import and the sink dropping its held buffers
///   clear it, which is also what releases the surfaces.
/// - An entry is only ever matched by the surface its own import retains, so a
///   pixel buffer VideoToolbox has freed cannot be mistaken for a live one.
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
    /// The cached frame for these planes, importing it first if this surface
    /// has not been seen. The index stays valid until the next call.
    pub fn get_or_import(
        &mut self,
        device: &wgpu::Device,
        desc: &FrameDesc,
        sources: &[PlaneSource],
    ) -> Result<usize, String> {
        if self.built_for != Some(*desc) {
            self.clear();
            self.built_for = Some(*desc);
        }
        let n = sources.len();
        if let Some(i) = self.find(sources) {
            self.hits += 1;
            return Ok(i);
        }
        self.misses += 1;
        let planes = import(device, desc, sources)?;
        let frame = Frame::from_planes(*desc, planes).map_err(|e| e.to_string())?;
        let mut keys = [PlaneKey::default(); MAX_PLANES];
        for (dst, src) in keys.iter_mut().zip(sources) {
            *dst = src.key;
        }
        let entry = Entry { keys, n, frame };
        if self.entries.len() < MAX_IMPORTS {
            self.entries.push(entry);
            return Ok(self.entries.len() - 1);
        }
        // A pool larger than the cache is walked in a cycle, so every lookup
        // lands on what was just dropped and nothing hits again. Worth a line,
        // since the only fix is a bigger cache.
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

    fn find(&self, sources: &[PlaneSource]) -> Option<usize> {
        self.entries.iter().position(|e| {
            e.n == sources.len() && e.keys.iter().zip(sources).all(|(k, s)| *k == s.key)
        })
    }

    pub fn frame(&self, index: usize) -> &Frame {
        &self.entries[index].frame
    }

    /// How many distinct surfaces are held, which is the decoder's pool size
    /// once a stream has cycled it. Only the tests ask.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drops every import, which releases the surfaces it retained. wgpu
    /// keeps a texture alive until the submissions that read it have retired,
    /// so this cannot pull one out from under the gpu.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next = 0;
        self.built_for = None;
    }
}
