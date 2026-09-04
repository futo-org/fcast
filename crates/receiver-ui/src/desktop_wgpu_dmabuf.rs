//! dmabuf import for the desktop wgpu video lane.
//!
//! A VA-API decoder hands its frames out as dmabuf fds behind
//! `video/x-raw(memory:DMABuf), format=DMA_DRM`. Mapping one of those readable
//! makes the iHD driver run `vaDeriveImage` and de-tile the whole surface on
//! the CPU, which is where a 4K60 stream spends 70% of a streaming thread. This
//! module takes the other route: each plane's fd is imported into Vulkan as its
//! own single-plane `VkImage` with the buffer's DRM modifier, wrapped as a
//! `wgpu::Texture`, and handed to `i-slint-video-wgpu` as frame planes. The
//! pixels are never read by the CPU.
//!
//! # Per plane, not per frame
//!
//! One `VkImage` per plane sidesteps `VK_IMAGE_CREATE_DISJOINT_BIT` entirely,
//! whether the planes share one fd at different offsets (what VA-API produces)
//! or live in separate buffers. It also keeps the plane format on the texture
//! rather than on a view, which is the only shape the crate's
//! `Frame::from_planes` can carry. The plane's byte offset and row pitch ride
//! in the explicit DRM modifier layout, so the image binds at memory offset 0
//! whatever its position in the buffer.
//!
//! # Cost, and why the import is cached
//!
//! An import is two `VkImage`s, two imported `VkDeviceMemory`, and (once the
//! renderer has them) two image views and a descriptor set. A decoder pool
//! hands the same handful of buffers round for the whole of a stream, so
//! [`ImportCache`] keeps one built frame per dmabuf and the steady state
//! costs an `fstat` per plane and a key compare. See its docs for what the
//! identity is and when entries die.
//!
//! # Sync
//!
//! Nothing here expresses ordering and nothing needs to on i915: the decoder's
//! write attaches an exclusive fence to the buffer's `dma_resv`, and ANV's
//! submit waits on the implicit fences of every imported BO it touches, so the
//! convert pass cannot start before the decode retires. That holds only while
//! decode and present sit on the same DRM device, which is why the lane opens
//! the low-power adapter, the one VA-API decodes on.

use ash::vk;
use i_slint_video_wgpu::{Frame, FrameDesc, PixelFormat};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};

/// Planes any format the lane imports can have, and so the size of every
/// per-frame plane array on this path. Nothing here allocates per frame.
pub const MAX_PLANES: usize = 3;

/// "the driver has not decided", which cannot be imported explicitly.
pub const DRM_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Force the lane back onto system memory. `FCAST_DESKTOP_WGPU_DMABUF=0`
/// leaves the appsink advertising sysmem only, which is the A/B control for
/// the whole path and the escape hatch on a driver that mis-imports.
pub fn enabled() -> bool {
    !std::env::var("FCAST_DESKTOP_WGPU_DMABUF").is_ok_and(|v| v == "0")
}

/// The plane texture formats the crate builds for a pixel format, as Vulkan
/// spells them. `None` for a format whose planes are not single-component
/// norms, which is nothing the crate produces today.
fn vk_plane_format(format: wgpu::TextureFormat) -> Option<vk::Format> {
    Some(match format {
        wgpu::TextureFormat::R8Unorm => vk::Format::R8_UNORM,
        wgpu::TextureFormat::Rg8Unorm => vk::Format::R8G8_UNORM,
        wgpu::TextureFormat::R16Unorm => vk::Format::R16_UNORM,
        wgpu::TextureFormat::Rg16Unorm => vk::Format::R16G16_UNORM,
        _ => return None,
    })
}

/// The gst video format a pixel format arrives as, which is also what its
/// DRM fourcc is derived from.
pub fn gst_format(format: PixelFormat) -> gst_video::VideoFormat {
    match format {
        PixelFormat::Nv12 => gst_video::VideoFormat::Nv12,
        PixelFormat::P010 => gst_video::VideoFormat::P01010le,
        PixelFormat::I420 => gst_video::VideoFormat::I420,
        // never offered as a dmabuf, see import_table
        PixelFormat::I420P10 => gst_video::VideoFormat::I42010le,
        PixelFormat::I420P12 => gst_video::VideoFormat::I42012le,
        // The crate carries more layouts than the lane negotiates, and one it
        // does not ask for has no fourcc to offer. `Unknown` makes
        // `drm_format_strings` come back empty, which is the same answer a
        // format with no DRM equivalent already gets.
        _ => gst_video::VideoFormat::Unknown,
    }
}

/// Every DRM modifier the adapter can sample this vk format with through a
/// single-plane image. A modifier whose layout needs more than one plane
/// cannot describe one of our per-plane images, and one without
/// `SAMPLED_IMAGE` cannot be read by the convert pass, so both are dropped
/// here rather than failing at import time.
fn sampled_modifiers(
    instance: &ash::Instance,
    phd: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<u64> {
    unsafe {
        // two calls: the first only reports the count, the second fills
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(phd, format, &mut props);
        let count = list.drm_format_modifier_count as usize;
        if count == 0 {
            return Vec::new();
        }
        let mut store = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut store);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(phd, format, &mut props);
        store
            .iter()
            .filter(|m| {
                m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            })
            .map(|m| m.drm_format_modifier)
            .collect()
    }
}

/// The modifiers this device can import a whole frame of `format` with: the
/// intersection over every plane, because one plane the driver refuses makes
/// the frame unimportable. Sorted, tiled before linear, so the caps offer the
/// decoder's own layout first and a linear copy last.
pub fn importable_modifiers(device: &wgpu::Device, format: PixelFormat) -> Vec<u64> {
    let (geom, n) = i_slint_video_wgpu::gpu::plane_geometry(&probe_desc(format));
    let hal = match unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() } {
        Some(hal) => hal,
        None => return Vec::new(),
    };
    let instance = hal.shared_instance().raw_instance().clone();
    let phd = hal.raw_physical_device();
    drop(hal);

    let mut shared: Option<Vec<u64>> = None;
    for g in geom.iter().take(n) {
        let Some(vkf) = vk_plane_format(g.format) else {
            return Vec::new();
        };
        let mods = sampled_modifiers(&instance, phd, vkf);
        shared = Some(match shared {
            None => mods,
            Some(prev) => prev.into_iter().filter(|m| mods.contains(m)).collect(),
        });
    }
    let mut out = shared.unwrap_or_default();
    out.retain(|m| *m != DRM_MOD_INVALID);
    // descending puts the vendor-tiled layouts (high bits set) ahead of
    // linear (0), which is the order a decoder should be offered them in
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.dedup();
    out
}

/// A desc whose colour fields are irrelevant, for the plane table alone.
fn probe_desc(format: PixelFormat) -> FrameDesc {
    FrameDesc {
        format,
        matrix: i_slint_video_wgpu::Matrix::Bt709,
        range: i_slint_video_wgpu::Range::Limited,
        transfer: i_slint_video_wgpu::Transfer::Bt1886,
        tonemap: i_slint_video_wgpu::TonemapCurve::Spline,
        primaries: i_slint_video_wgpu::Primaries::Bt709,
        chroma_location: i_slint_video_wgpu::ChromaLocation::Left,
        width: 64,
        height: 64,
        hdr: i_slint_video_wgpu::HdrMetadata {
            max_mastering_nits: 0.0,
            max_cll: 0.0,
        },
    }
}

/// `drm-format` strings for the caps, `FOURCC:modifier` as
/// `gst_video_dma_drm_fourcc_to_string` spells them. Empty when the format
/// has no DRM fourcc, which is what keeps an unmappable format out of the
/// dmabuf structure instead of into it with a zero fourcc.
pub fn drm_format_strings(format: PixelFormat, modifiers: &[u64]) -> Vec<String> {
    let Ok(fourcc) = gst_video::dma_drm_fourcc_from_format(gst_format(format)) else {
        return Vec::new();
    };
    modifiers
        .iter()
        .filter(|m| **m != DRM_MOD_INVALID)
        .map(|m| gst_video::dma_drm_fourcc_to_string(fourcc, *m).to_string())
        .collect()
}

/// What makes two frames the same decoder surface.
///
/// The dma_buf's inode is the identity of the underlying buffer object: every
/// fd exported from or dup'd off it shares the inode, and the number cannot be
/// reused by another buffer while something holds the dma_buf open, which an
/// import does for as long as its entry is cached. The `GstMemory` address
/// would be cheaper, but it is not ours to pin: a VA allocator recycles its
/// surface when the memory drops, so holding a ref to keep the address
/// meaningful would starve the decoder's own pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlaneKey {
    dev: u64,
    inode: u64,
    offset: u64,
    stride: u64,
}

/// Where one plane's bytes live, plus the identity a cached import matches
/// on. The fd is borrowed from the gst memory; [`import`] dups it, because
/// Vulkan takes what it is given on a successful import and closes it on a
/// failed one. It is only good while the buffer it was read off is alive,
/// which is why nothing stores one.
#[derive(Clone, Copy, Debug)]
pub struct PlaneSource {
    pub fd: RawFd,
    pub key: PlaneKey,
}

impl PlaneSource {
    pub const EMPTY: PlaneSource = PlaneSource {
        fd: -1,
        key: PlaneKey {
            dev: 0,
            inode: 0,
            offset: 0,
            stride: 0,
        },
    };

    pub fn offset(&self) -> u64 {
        self.key.offset
    }

    pub fn stride(&self) -> u64 {
        self.key.stride
    }
}

/// `st_dev` and `st_ino` of an open fd. One `fstat`, no allocation: the file
/// is borrowed back from the raw fd and never closed.
fn fd_identity(fd: RawFd) -> Result<(u64, u64), &'static str> {
    use std::os::{fd::FromRawFd, unix::fs::MetadataExt};
    let file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    let meta = file
        .metadata()
        .map_err(|_| "stat of the dmabuf fd failed")?;
    Ok((meta.dev(), meta.ino()))
}

/// Locates each plane in the buffer's dmabuf memories. Planes may share one
/// memory at different offsets, which is what VA-API produces, or sit in one
/// memory each; `find_memory` resolves both without the caller knowing which.
///
/// The geometry has to come off the `VideoMeta`, and its absence is refused
/// rather than guessed at. A decoder's padded pitch and its plane offsets live
/// nowhere else, and the caps' packed layout is only right for a linear
/// buffer, so falling back to it would render a tiled frame as garbage instead
/// of failing. The allocation query asks every upstream for the meta, so a
/// buffer without one is upstream ignoring the contract.
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
    let meta = buffer
        .meta::<gst_video::VideoMeta>()
        .ok_or("the dmabuf buffer carries no video meta")?;
    if meta.n_planes() as usize != n {
        return Err("video meta plane count does not match the caps");
    }
    let (offsets, strides) = (meta.offset(), meta.stride());
    if offsets.len() < n || strides.len() < n {
        return Err("fewer plane offsets or strides than the format has planes");
    }
    // a bottom-up plane would wrap the unsigned pitch the import is given
    if strides[..n].iter().any(|s| *s <= 0) {
        return Err("a plane stride is not positive");
    }

    for i in 0..n {
        let offset = offsets[i];
        let (range, skip) = buffer
            .find_memory(offset..offset + 1)
            .ok_or("plane offset is past the end of the buffer")?;
        let mem = buffer.peek_memory(range.start);
        let dma = mem
            .downcast_memory_ref::<gst_allocators::DmaBufMemory>()
            .ok_or("buffer memory is not a dmabuf")?;
        // gst maps a dmabuf memory at mem.offset() into the fd, so the plane
        // sits that much further in again
        let in_fd = (mem.offset() + skip) as u64;
        let fd = dma.fd();
        let (dev, inode) = fd_identity(fd)?;
        out[i] = PlaneSource {
            fd,
            key: PlaneKey {
                dev,
                inode,
                offset: in_fd,
                stride: strides[i] as u64,
            },
        };
    }
    Ok(n)
}

/// The gst memory keeps its own fd, so the import gets a duplicate.
/// `try_clone_to_owned` is `F_DUPFD_CLOEXEC`, so a fork between here and the
/// import cannot leak it.
fn dup_fd(raw: RawFd) -> Result<OwnedFd, &'static str> {
    unsafe { BorrowedFd::borrow_raw(raw) }
        .try_clone_to_owned()
        .map_err(|_| "dup of the dmabuf fd failed")
}

/// Imports one frame's planes as wgpu textures. Each fd is dup'd, since
/// Vulkan takes ownership of what it is handed.
///
/// The textures own their `VkImage` and the imported `VkDeviceMemory` through
/// wgpu-hal's dedicated-memory arm, so dropping them frees both. What they do
/// not own is the frame the decoder wrote, so the caller must keep the gst
/// buffer alive for as long as the textures can still be sampled.
pub fn import(
    device: &wgpu::Device,
    desc: &FrameDesc,
    modifier: u64,
    sources: &[PlaneSource],
) -> Result<Vec<wgpu::Texture>, String> {
    if modifier == DRM_MOD_INVALID {
        return Err("the buffer carries no explicit drm modifier".into());
    }
    let (geom, n) = i_slint_video_wgpu::gpu::plane_geometry(desc);
    if sources.len() != n {
        return Err(format!(
            "{} plane sources for a {n} plane format",
            sources.len()
        ));
    }
    let mut out = Vec::with_capacity(n);
    for (i, (src, g)) in sources.iter().zip(geom.iter().take(n)).enumerate() {
        let size = wgpu::Extent3d {
            width: g.width,
            height: g.height,
            depth_or_array_layers: 1,
        };
        let row = g.width as u64 * g.bytes_per_texel as u64;
        if src.stride() < row {
            return Err(format!(
                "plane {i} stride {} is under a row {row}",
                src.stride()
            ));
        }
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("fcast-dmabuf-plane"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: g.format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let fd = dup_fd(src.fd).map_err(str::to_string)?;
        let hal_tex = {
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }
                .ok_or_else(|| "device is not vulkan".to_string())?;
            let tex = unsafe {
                hal.texture_from_dmabuf_fd(fd, &hal_desc, modifier, src.stride(), src.offset())
            };
            drop(hal);
            tex.map_err(|err| format!("plane {i} import: {err}"))?
        };
        out.push(unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("fcast-dmabuf-plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: g.format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                // the import just created the image, so its layout is
                // VK_IMAGE_LAYOUT_UNDEFINED and wgpu records the transition
                // to SHADER_READ_ONLY itself
                wgpu::TextureUses::UNINITIALIZED,
            )
        });
    }
    Ok(out)
}

/// Imported frames kept at once.
///
/// This has to cover a decoder's whole output pool, because the pool is
/// walked in a cycle: at one entry short of it every lookup lands on the
/// entry just evicted and the cache hits nothing at all. A VA h264 decoder
/// negotiates its DPB plus whatever downstream asks to hold, measured at 18
/// to 25 distinct dmabufs for one 640x480 stream into a sixteen deep appsink,
/// and the sink's own three held frames ride on top. The lane's appsink is
/// two deep, so this is well above what playback asks for. An import is only
/// an image and a memory handle over memory that already exists, so keeping
/// them is cheap; a pool past this still renders, it just stops hitting,
/// which [`ImportCache`] says once in the log.
///
/// The renderer's convert bind cache has to be at least as large or the
/// descriptor set is rebuilt per frame anyway.
pub const MAX_IMPORTS: usize = 48;

/// One dmabuf's imported planes, built once and rendered from for as long as
/// the decoder keeps handing that buffer round.
struct Entry {
    keys: [PlaneKey; MAX_PLANES],
    n: usize,
    frame: Frame,
}

/// Imported plane textures, one entry per dmabuf the decoder cycles.
///
/// A miss costs one `VkImage` and one imported `VkDeviceMemory` per plane,
/// plus the views and the descriptor set the renderer then builds. A hit
/// costs an `fstat` per plane and a key compare, which is the whole per-frame
/// cost of the zero-copy route once a stream has settled.
///
/// # What makes reuse sound
///
/// The decoder writes new pixels into the same dmabuf between two hits, and
/// the cached `VkImage` reads that memory, so a hit shows the new frame. Both
/// the layout and the memory are the same objects the first import created,
/// so nothing about them goes stale; it is the same thing a wayland
/// compositor does with a client's buffer across commits. Ordering is the
/// implicit fence on the buffer's `dma_resv`, exactly as it was for a
/// per-frame import, and the caller still holds the gst buffer for a few
/// frames so the pool cannot hand it back mid-render. wgpu tracks the
/// textures like any other resource, so reuse across submissions is what it
/// already does for every scratch texture in the renderer.
///
/// # Invalidation
///
/// - A desc or modifier change clears everything: plane geometry, format and
///   layout all come from those, so no entry can outlive them.
/// - End of stream, a refused import and the sink dropping its held buffers
///   clear it, which is also what closes the dup'd fds and frees the imported
///   memory.
/// - An entry is only ever matched by the dma_buf inode its own import holds
///   open, so a buffer the decoder has freed cannot be mistaken for a live one.
/// - Full is round-robin: a pool larger than [`MAX_IMPORTS`] keeps rendering
///   and stops hitting, it never renders the wrong frame.
pub struct ImportCache {
    entries: Vec<Entry>,
    next: usize,
    /// What every entry was built for.
    built_for: Option<(FrameDesc, u64)>,
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
    /// The cached frame for these planes, importing it first if this dmabuf
    /// has not been seen. The index stays valid until the next call.
    pub fn get_or_import(
        &mut self,
        device: &wgpu::Device,
        desc: &FrameDesc,
        modifier: u64,
        sources: &[PlaneSource],
    ) -> Result<usize, String> {
        if self.built_for != Some((*desc, modifier)) {
            self.clear();
            self.built_for = Some((*desc, modifier));
        }
        let n = sources.len();
        if let Some(i) = self.find(sources) {
            self.hits += 1;
            return Ok(i);
        }
        self.misses += 1;
        let planes = import(device, desc, modifier, sources)?;
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

    /// How many distinct dmabufs are held, which is the decoder's pool size
    /// once a stream has cycled it. Only the tests ask.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drops every import, which closes its fds and frees the imported
    /// memory. wgpu keeps a texture alive until the submissions that read it
    /// have retired, so this cannot pull one out from under the gpu.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next = 0;
        self.built_for = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device with an import route. `None` on the GL floor as well as on
    /// no adapter at all: the questions below are about what a device that
    /// CAN import reports, and GL cannot.
    fn device() -> Option<wgpu::Device> {
        crate::desktop_wgpu_video::create_shared_device()
            .filter(|s| s.dmabuf)
            .map(|s| s.device)
    }

    /// The whole caps offer rests on this: the adapter must be able to sample
    /// both NV12 plane formats through some shared modifier, or the decoder
    /// would be offered a layout the import then refuses.
    #[test]
    fn the_adapter_reports_importable_nv12_modifiers() {
        let Some(device) = device() else {
            eprintln!("no dmabuf import route on this adapter, skipping");
            return;
        };
        let mods = importable_modifiers(&device, PixelFormat::Nv12);
        eprintln!(
            "nv12 importable modifiers: {:?}",
            mods.iter().map(|m| format!("{m:#x}")).collect::<Vec<_>>()
        );
        assert!(
            !mods.is_empty(),
            "an adapter with the dmabuf feature must import something"
        );
        // linear is the one every driver has to be able to describe
        assert!(mods.contains(&0), "linear must be importable");
        // and it must come last, so the decoder is offered its own tiling first
        assert_eq!(*mods.last().unwrap(), 0, "linear must be the last offer");
    }

    /// The caps text the appsink advertises, spelled the way
    /// `gst_video_info_dma_drm_from_caps` parses it back.
    #[test]
    fn drm_format_strings_are_fourcc_colon_modifier() {
        gst::init().unwrap();
        let s = drm_format_strings(PixelFormat::Nv12, &[0x0100_0000_0000_0002, 0]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], "NV12:0x0100000000000002");
        // linear is spelled without a modifier suffix
        assert_eq!(s[1], "NV12");
        for text in &s {
            let (fourcc, _) = gst_video::dma_drm_fourcc_from_str(text).expect("must parse back");
            assert_eq!(
                gst_video::dma_drm_fourcc_to_format(fourcc).unwrap(),
                gst_video::VideoFormat::Nv12
            );
        }
        // a modifier of "invalid" is never offered
        assert!(drm_format_strings(PixelFormat::Nv12, &[DRM_MOD_INVALID]).is_empty());
    }

    /// P010 and I420 have to spell out too, since the caps list them whenever
    /// the device took the 16-bit norm feature.
    #[test]
    fn every_pixel_format_has_a_fourcc() {
        gst::init().unwrap();
        for format in [PixelFormat::Nv12, PixelFormat::P010, PixelFormat::I420] {
            assert!(
                !drm_format_strings(format, &[0]).is_empty(),
                "{format:?} has no drm fourcc"
            );
        }
    }
}
