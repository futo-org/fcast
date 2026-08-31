//! An AHardwareBuffer-backed allocator and buffer pool for android's
//! software-decode arm.
//!
//! This is the android twin of [`crate::desktop_wgpu_udmabuf`], and it exists
//! for the same reason. A software decoder writes its pixels with the CPU, so
//! it negotiates plain `video/x-raw` and allocates ordinary heap pages. The
//! renderer cannot sample those, so today every frame is converted to RGBA on
//! the streaming thread and uploaded as a texture, which at 4K is tens of
//! megabytes of memcpy per picture.
//!
//! An `AHardwareBuffer` is memory both sides can touch: gralloc allocates it,
//! `AHardwareBuffer_lockPlanes` hands the CPU real pointers and strides to
//! write through, and EGL imports the very same allocation as a texture. So
//! the sink answers the decoder's ALLOCATION query with a pool of them, the
//! decoder writes straight into GPU-visible memory, and
//! [`crate::android_ahb_gl`] samples it in place. No conversion, no upload.
//!
//! # Whose allocator
//!
//! Ours, unlike the desktop lane. gstreamer ships `GstUdmabufAllocator` for
//! linux but has nothing for AHardwareBuffer that a generic pipeline can
//! propose, so both halves are here: a `GstAllocator` subclass whose memories
//! own one AHardwareBuffer each, and a `GstBufferPool` subclass that gives
//! them the video meta the decoder writes through.
//!
//! # The lock cycle
//!
//! A gralloc buffer is mapped, not just addressed, and `unlock` is where the
//! CPU's writes become visible to the GPU on a device whose caches are not
//! coherent. So each buffer is locked for CPU write when it is allocated, the
//! sink unlocks it once the decoder has finished the picture and before the
//! import samples it, and the pool locks it again on its way back into the
//! free list. A re-lock that moves the mapping or changes a stride would make
//! the video meta a lie, so it is checked and latches the lane off instead.
//!
//! # Which formats
//!
//! `AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420` is a name, not a layout: the same
//! request comes back NV12 on one device, NV21 on another and I420 on a
//! third. [`available`] allocates one buffer at startup, reads the layout out
//! of it, and afterwards the lane proposes a pool only for the single gst
//! format that matches. 10-bit is excluded outright, see
//! [`crate::ahb_plan::ahb_format_for`].
//!
//! # Degradation
//!
//! Every failure is a fallback, never an error. A device whose gralloc
//! refuses `GPU_SAMPLED_IMAGE | CPU_WRITE_OFTEN` together, an API level below
//! 29, a layout with no gst name: all of them leave the query untouched, the
//! decoder allocates its own system memory, and the bridge converts as it
//! always did. `FCAST_ANDROID_AHB=0` does the same by hand.

use std::{
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use gst::{glib, glib::translate::*, prelude::*, subclass::prelude::*};
use gst_video::prelude::*;
use tracing::{info, warn};

use crate::ahb_plan::{self, AhbEnv, AhbFormat, PlaneLayout};

/// The pool's memories are whole frames and nothing downstream slices them,
/// so the gst name is only ever compared, never parsed.
const MEM_TYPE: &[u8] = b"FcastAHardwareBuffer\0";

/// CPU write is the decoder, GPU sampled is the import, CPU read rarely is
/// what keeps a gralloc from picking a write-combined mapping the odd
/// decoder read-back would crawl through.
const USAGE: u64 = (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN.0
    as u64)
    | (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_CPU_READ_RARELY.0 as u64)
    | (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE.0 as u64);

/// `AHardwareBuffer_lockPlanes` arrived in API 29, and this app declares 28.
///
/// Calling it through `ndk_sys` binds it at link time, and a GLOBAL undefined
/// symbol the device cannot resolve fails the whole `System.loadLibrary`: the
/// app dies before any Rust runs, with nothing but a dlopen error to show for
/// it. Every other NDK symbol the binary needs exists at 28, so this one name
/// decides whether the receiver starts at all on an API 28 phone.
///
/// Resolved at runtime instead. Absent means the AHB lane is unavailable,
/// which [`available`] already models as a probe outcome, so the caller has
/// somewhere to go. Looked up on `libnativewindow.so` rather than
/// `RTLD_DEFAULT`, whose value differs between 32- and 64-bit bionic; the
/// library is already loaded, so this only takes a reference.
type LockPlanesFn = unsafe extern "C" fn(
    *mut ndk_sys::AHardwareBuffer,
    u64,
    i32,
    *const ndk_sys::ARect,
    *mut ndk_sys::AHardwareBuffer_Planes,
) -> std::os::raw::c_int;

unsafe extern "C" {
    fn dlopen(
        filename: *const std::os::raw::c_char,
        flag: std::os::raw::c_int,
    ) -> *mut std::ffi::c_void;
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::os::raw::c_char,
    ) -> *mut std::ffi::c_void;
}

fn lock_planes_sym() -> Option<LockPlanesFn> {
    const RTLD_NOW: std::os::raw::c_int = 2;
    static SYM: OnceLock<Option<LockPlanesFn>> = OnceLock::new();
    *SYM.get_or_init(|| unsafe {
        let lib = dlopen(c"libnativewindow.so".as_ptr(), RTLD_NOW);
        if lib.is_null() {
            warn!("android ahb lane: libnativewindow.so did not open, lane off");
            return None;
        }
        let sym = dlsym(lib, c"AHardwareBuffer_lockPlanes".as_ptr());
        if sym.is_null() {
            info!("android ahb lane: no AHardwareBuffer_lockPlanes (needs API 29), lane off");
            return None;
        }
        Some(std::mem::transmute::<*mut std::ffi::c_void, LockPlanesFn>(
            sym,
        ))
    })
}

// The plan module spells the NDK's format numbers out so it can be read on a
// host with no NDK. Pin them to the real ones here.
const _: () = assert!(
    AhbFormat::Yuv420.raw()
        == ndk_sys::AHardwareBuffer_Format::AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420.0
);
const _: () = assert!(
    AhbFormat::Rgba8888.raw()
        == ndk_sys::AHardwareBuffer_Format::AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM.0
);

/// Set when an allocation, a lock or a pool config failed at runtime.
/// Nothing is retried after that: a gralloc that started refusing is a
/// device-level refusal, not something the next stream fixes, and the bridge
/// carries every frame meanwhile.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// `FCAST_ANDROID_AHB=0` leaves the appsink proposing nothing, so every
/// software frame takes the conversion path. Read per call, not cached, so a
/// test can flip it.
pub fn enabled() -> bool {
    !std::env::var("FCAST_ANDROID_AHB").is_ok_and(|v| v == "0")
}

fn refuse(reason: &str) {
    if !REFUSED.swap(true, Ordering::Relaxed) {
        warn!(
            reason,
            "android ahb lane: refused, no longer proposing a pool"
        );
    }
}

/// What the startup probe learned. `None` on either field means that half of
/// the lane is off for the life of the process.
#[derive(Clone, Copy, Default)]
pub struct Probe {
    /// The gst format this device's gralloc lays a `Y8Cb8Cr8_420` out as.
    pub yuv: Option<gst_video::VideoFormat>,
    pub rgba: bool,
}

/// The live gating inputs, so [`ahb_plan::plan`] can be tested on a host
/// against a literal instead of a device.
struct LiveEnv(Probe);

impl AhbEnv for LiveEnv {
    fn enabled(&self) -> bool {
        enabled()
    }
    fn refused(&self) -> bool {
        REFUSED.load(Ordering::Relaxed)
    }
    fn yuv_layout(&self) -> Option<gst_video::VideoFormat> {
        self.0.yuv
    }
    fn rgba_ok(&self) -> bool {
        self.0.rgba
    }
}

/// Whether this device can hand out importable AHardwareBuffers at all, and
/// in what layout.
///
/// Probed exactly once, with two real allocations that are freed immediately.
/// Doing it lazily per query instead would put a failing gralloc round trip
/// on every caps negotiation of a device that can never satisfy one, and
/// asking `AHardwareBuffer_isSupported` is not enough: it answers about the
/// descriptor, not about whether `lockPlanes` will produce a layout that has
/// a gst name.
pub fn available() -> Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    *PROBE.get_or_init(|| {
        let yuv = probe_yuv();
        let rgba = probe_rgba();
        // one line, once, mirroring the desktop lane's udmabuf=true/false
        match yuv {
            Some(format) => info!(
                ahb = true,
                %format,
                rgba,
                "android ahb lane: gralloc hands out importable frames"
            ),
            None => info!(
                ahb = false,
                rgba, "android ahb lane: no usable yuv layout, software frames take the bridge"
            ),
        }
        Probe { yuv, rgba }
    })
}

fn probe_yuv() -> Option<gst_video::VideoFormat> {
    // 64x64 rather than the smallest legal size: a gralloc is free to refuse
    // or to lay out a 2x2 buffer differently from a real frame, and the
    // layout is exactly what is being read here.
    let buffer = RawBuffer::allocate(64, 64, AhbFormat::Yuv420)?;
    let planes = buffer.lock_planes()?;
    if planes.n < 3 {
        warn!(
            planes = planes.n,
            "android ahb lane: a yuv buffer with too few planes"
        );
        return None;
    }
    let layout = PlaneLayout {
        chroma_pixel_stride: planes.pixel_stride[1],
        cr_first: planes.data[2] < planes.data[1],
    };
    buffer.unlock();
    let format = ahb_plan::layout_format(layout);
    if format.is_none() {
        warn!(
            chroma_pixel_stride = layout.chroma_pixel_stride,
            cr_first = layout.cr_first,
            "android ahb lane: gralloc layout has no gst format"
        );
    }
    format
}

fn probe_rgba() -> bool {
    let Some(buffer) = RawBuffer::allocate(16, 16, AhbFormat::Rgba8888) else {
        return false;
    };
    let locked = buffer.lock_planes().is_some();
    if locked {
        buffer.unlock();
    }
    locked
}

// ---------------------------------------------------------------------------
// the raw handle
// ---------------------------------------------------------------------------

/// One `AHardwareBuffer`, released on drop. Owns exactly one reference.
struct RawBuffer(ptr::NonNull<ndk_sys::AHardwareBuffer>);

// gralloc handles are process-wide and refcounted, not thread affine.
unsafe impl Send for RawBuffer {}

/// What one lock reported. Fixed size, no allocation, so the pool can hold it
/// inline in the memory struct.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Planes {
    n: u32,
    data: [usize; 4],
    pixel_stride: [u32; 4],
    row_stride: [u32; 4],
}

impl RawBuffer {
    fn allocate(width: u32, height: u32, format: AhbFormat) -> Option<Self> {
        let desc = ndk_sys::AHardwareBuffer_Desc {
            width,
            height,
            layers: 1,
            format: format.raw(),
            usage: USAGE,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        let mut raw: *mut ndk_sys::AHardwareBuffer = ptr::null_mut();
        let rc = unsafe { ndk_sys::AHardwareBuffer_allocate(&desc, &mut raw) };
        if rc != 0 {
            warn!(
                rc,
                width, height, "android ahb lane: gralloc refused an allocation"
            );
            return None;
        }
        ptr::NonNull::new(raw).map(Self)
    }

    fn as_ptr(&self) -> *mut ndk_sys::AHardwareBuffer {
        self.0.as_ptr()
    }

    /// Locks for CPU write and reports what gralloc laid out. A single-plane
    /// format goes through the same call: `lockPlanes` reports one plane for
    /// RGBA, with the pixel stride in bytes.
    fn lock_planes(&self) -> Option<Planes> {
        let mut raw = ndk_sys::AHardwareBuffer_Planes {
            planeCount: 0,
            planes: [ndk_sys::AHardwareBuffer_Plane {
                data: ptr::null_mut(),
                pixelStride: 0,
                rowStride: 0,
            }; 4],
        };
        let lock_planes = lock_planes_sym()?;
        let rc = unsafe { lock_planes(self.as_ptr(), USAGE, -1, ptr::null(), &mut raw) };
        if rc != 0 {
            warn!(rc, "android ahb lane: lockPlanes refused");
            return None;
        }
        let mut planes = Planes {
            n: raw.planeCount.min(4),
            ..Planes::default()
        };
        for i in 0..planes.n as usize {
            planes.data[i] = raw.planes[i].data as usize;
            planes.pixel_stride[i] = raw.planes[i].pixelStride;
            planes.row_stride[i] = raw.planes[i].rowStride;
        }
        if planes.n == 0 || planes.data[0] == 0 {
            unsafe { ndk_sys::AHardwareBuffer_unlock(self.as_ptr(), ptr::null_mut()) };
            return None;
        }
        Some(planes)
    }

    /// Publishes the CPU's writes. The fence is not taken: the import that
    /// follows happens on the render thread after the sink has already
    /// handed the frame over, which is later than any fence would signal.
    fn unlock(&self) {
        unsafe { ndk_sys::AHardwareBuffer_unlock(self.as_ptr(), ptr::null_mut()) };
    }
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        unsafe { ndk_sys::AHardwareBuffer_release(self.as_ptr()) };
    }
}

// ---------------------------------------------------------------------------
// the memory
// ---------------------------------------------------------------------------

/// A `GstMemory` whose storage is one AHardwareBuffer.
///
/// `mem` has to be first: gst casts the two into each other freely and the
/// allocator's free hook gets the base pointer back.
#[repr(C)]
struct AhbMemory {
    mem: gst::ffi::GstMemory,
    buffer: *mut ndk_sys::AHardwareBuffer,
    planes: Planes,
    /// Base of the mapping, which is plane 0. Zero while unlocked, so a map
    /// after the sink handed the frame over, or after a re-lock that failed,
    /// returns null and the caller's map fails instead of being handed a
    /// pointer to memory gralloc no longer guarantees.
    data: AtomicUsize,
    size: usize,
    locked: AtomicBool,
}

const MEM_LAYOUT: std::alloc::Layout = std::alloc::Layout::new::<AhbMemory>();

impl AhbMemory {
    /// Re-locks a buffer on its way back into the pool's free list.
    ///
    /// gralloc keeps one mapping per buffer per process, so a re-lock returns
    /// the same address and the same strides. If it ever does not, the video
    /// meta already handed to the decoder describes a layout that no longer
    /// exists, and the only safe answer is to stop proposing.
    unsafe fn relock(&self) -> bool {
        if self.locked.load(Ordering::Acquire) {
            return true;
        }
        let raw = unsafe {
            std::mem::ManuallyDrop::new(RawBuffer(ptr::NonNull::new_unchecked(self.buffer)))
        };
        let Some(planes) = raw.lock_planes() else {
            refuse("a pooled buffer would not lock again");
            return false;
        };
        if planes != self.planes {
            raw.unlock();
            refuse("a re-lock moved the mapping, the video meta would be a lie");
            return false;
        }
        self.data.store(planes.data[0], Ordering::Relaxed);
        self.locked.store(true, Ordering::Release);
        true
    }

    /// Publishes the decoder's writes. Idempotent, because the appsink drops
    /// frames it never delivers and those come back through the pool having
    /// missed the sink entirely.
    unsafe fn unlock(&self) {
        if !self.locked.swap(false, Ordering::AcqRel) {
            return;
        }
        self.data.store(0, Ordering::Relaxed);
        let raw = unsafe {
            std::mem::ManuallyDrop::new(RawBuffer(ptr::NonNull::new_unchecked(self.buffer)))
        };
        raw.unlock();
    }
}

unsafe extern "C" fn mem_map(
    mem: *mut gst::ffi::GstMemory,
    _maxsize: usize,
    _flags: gst::ffi::GstMapFlags,
) -> glib::ffi::gpointer {
    unsafe {
        let mem = mem as *mut AhbMemory;
        // `mem.offset` is added by gst_memory_map itself
        (*mem).data.load(Ordering::Acquire) as glib::ffi::gpointer
    }
}

unsafe extern "C" fn mem_unmap(_mem: *mut gst::ffi::GstMemory) {}

// ---------------------------------------------------------------------------
// the allocator
// ---------------------------------------------------------------------------

mod allocator_imp {
    use super::*;

    #[derive(Default)]
    pub struct AhbAllocator;

    #[glib::object_subclass]
    impl ObjectSubclass for AhbAllocator {
        const NAME: &'static str = "FcastAhbAllocator";
        type Type = super::AhbAllocator;
        type ParentType = gst::Allocator;

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            unsafe {
                let allocator = obj.as_ptr() as *mut gst::ffi::GstAllocator;
                (*allocator).mem_type = MEM_TYPE.as_ptr() as *const _;
                (*allocator).mem_map = Some(mem_map);
                (*allocator).mem_unmap = Some(mem_unmap);
                // mem_share is left at the base class fallback and every
                // memory carries NO_SHARE, so a slice is copied rather than
                // aliased. Nothing downstream of a video sink slices frames.
            }
        }
    }

    impl ObjectImpl for AhbAllocator {}
    impl GstObjectImpl for AhbAllocator {}

    unsafe impl AllocatorImpl for AhbAllocator {
        /// A sized allocation means nothing here: gralloc needs a width, a
        /// height and a format, and there is no way to recover them from a
        /// byte count. Callers use [`super::AhbAllocator::alloc_video`].
        unsafe fn alloc(
            &self,
            _size: usize,
            _params: &gst::AllocationParams,
        ) -> *mut gst::ffi::GstMemory {
            ptr::null_mut()
        }

        unsafe fn free(&self, mem: *mut gst::ffi::GstMemory) {
            unsafe {
                let mem = mem as *mut AhbMemory;
                (*mem).unlock();
                ndk_sys::AHardwareBuffer_release((*mem).buffer);
                std::alloc::dealloc(mem as *mut u8, MEM_LAYOUT);
            }
        }
    }
}

glib::wrapper! {
    pub struct AhbAllocator(ObjectSubclass<allocator_imp::AhbAllocator>) @extends gst::Allocator, gst::Object;
}

impl Default for AhbAllocator {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl AhbAllocator {
    /// The one instance the lane proposes. Kept alive for the process so
    /// [`is_ahb`] can identify a memory by comparing allocator pointers,
    /// which costs one load and no allocation per frame.
    fn get() -> &'static Self {
        static ALLOCATOR: OnceLock<AhbAllocator> = OnceLock::new();
        ALLOCATOR.get_or_init(Self::default)
    }

    /// Allocates one frame's worth of gralloc memory, already locked for the
    /// decoder to write through, and reports the layout gralloc chose.
    fn alloc_video(
        &self,
        width: u32,
        height: u32,
        format: AhbFormat,
    ) -> Option<(gst::Memory, Planes)> {
        let raw = RawBuffer::allocate(width, height, format)?;
        let planes = raw.lock_planes()?;

        let base = planes.data[0];
        let size =
            ahb_plan::frame_size(format, height, planes.n, &planes.data, &planes.row_stride)?;

        let mem = unsafe {
            let mem = std::alloc::alloc(MEM_LAYOUT) as *mut AhbMemory;
            if mem.is_null() {
                return None;
            }
            gst::ffi::gst_memory_init(
                ptr::addr_of_mut!((*mem).mem),
                gst::MemoryFlags::NO_SHARE.into_glib(),
                self.as_ptr() as *mut gst::ffi::GstAllocator,
                ptr::null_mut(),
                size,
                0,
                0,
                size,
            );
            ptr::write(ptr::addr_of_mut!((*mem).buffer), raw.as_ptr());
            ptr::write(ptr::addr_of_mut!((*mem).planes), planes);
            ptr::write(ptr::addr_of_mut!((*mem).data), AtomicUsize::new(base));
            ptr::write(ptr::addr_of_mut!((*mem).size), size);
            ptr::write(ptr::addr_of_mut!((*mem).locked), AtomicBool::new(true));
            // the memory owns the handle now
            std::mem::forget(raw);
            gst::Memory::from_glib_full(mem as *mut gst::ffi::GstMemory)
        };
        Some((mem, planes))
    }
}

// ---------------------------------------------------------------------------
// the pool
// ---------------------------------------------------------------------------

/// What the pool learned from its config, filled in once per stream.
#[derive(Clone, Copy)]
struct PoolSetup {
    ahb_format: AhbFormat,
    video_format: gst_video::VideoFormat,
    /// The picture, without padding. Always at the allocation's origin, see
    /// [`ahb_plan::alignment_is_supported`].
    width: u32,
    height: u32,
    /// What gralloc is asked for, picture plus the decoder's trailing
    /// padding.
    alloc_width: u32,
    alloc_height: u32,
    align: gst_video::VideoAlignment,
}

mod pool_imp {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct AhbPool {
        pub(super) setup: Mutex<Option<PoolSetup>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AhbPool {
        const NAME: &'static str = "FcastAhbPool";
        type Type = super::AhbPool;
        type ParentType = gst::BufferPool;
    }

    impl ObjectImpl for AhbPool {}
    impl GstObjectImpl for AhbPool {}

    impl BufferPoolImpl for AhbPool {
        /// Both are mandatory rather than optional: without the video meta
        /// the decoder cannot see gralloc's strides, and without the
        /// alignment option a decoder that needs padding has no way to ask
        /// for it and would write past the picture.
        fn options() -> &'static [&'static str] {
            &[
                "GstBufferPoolOptionVideoMeta",
                "GstBufferPoolOptionVideoAlignment",
            ]
        }

        fn set_config(&self, config: &mut gst::BufferPoolConfigRef) -> bool {
            let Some((Some(caps), _, min, max)) = config.params() else {
                return false;
            };
            let Ok(info) = gst_video::VideoInfo::from_caps(&caps) else {
                return false;
            };
            let Some(ahb_format) = ahb_plan::ahb_format_for(info.format()) else {
                return false;
            };
            let align = config.video_alignment().unwrap_or_else(|| {
                gst_video::VideoAlignment::new(0, 0, 0, 0, &[0; gst_video::VIDEO_MAX_PLANES])
            });
            if !ahb_plan::alignment_is_supported(align.padding_left(), align.padding_top()) {
                // Not a refusal of the lane, only of this decoder's request:
                // it keeps its own buffers and the next stream may still fit.
                info!(
                    left = align.padding_left(),
                    top = align.padding_top(),
                    "android ahb lane: leading padding asked for, leaving the decoder its own pool"
                );
                return false;
            }
            let (alloc_width, alloc_height) = ahb_plan::padded_geometry(
                info.width(),
                info.height(),
                align.padding_right(),
                align.padding_bottom(),
            );

            // gralloc picks the stride, so the only honest way to know the
            // size the config must carry is to allocate one buffer and look.
            // It costs a single gralloc round trip per stream and it is what
            // keeps the decoder from being told a size that is not real.
            let Some((mem, _)) =
                AhbAllocator::get().alloc_video(alloc_width, alloc_height, ahb_format)
            else {
                refuse("the pool could not allocate its probe buffer");
                return false;
            };
            let size = mem.size() as u32;
            drop(mem);

            *self.setup.lock().unwrap() = Some(PoolSetup {
                ahb_format,
                video_format: info.format(),
                width: info.width(),
                height: info.height(),
                alloc_width,
                alloc_height,
                align,
            });
            config.set_params(Some(&caps), size, min, max);
            config.add_option("GstBufferPoolOptionVideoMeta");
            self.parent_set_config(config)
        }

        fn alloc_buffer(
            &self,
            params: Option<&gst::BufferPoolAcquireParams>,
        ) -> Result<gst::Buffer, gst::FlowError> {
            let setup = (*self.setup.lock().unwrap()).ok_or(gst::FlowError::NotNegotiated)?;
            let _ = params;
            let (mem, planes) = AhbAllocator::get()
                .alloc_video(setup.alloc_width, setup.alloc_height, setup.ahb_format)
                .ok_or_else(|| {
                    refuse("the pool ran out of gralloc memory");
                    gst::FlowError::Error
                })?;

            let mut buffer = gst::Buffer::new();
            {
                let buffer = buffer.get_mut().ok_or(gst::FlowError::Error)?;
                buffer.append_memory(mem);
                let (offsets, strides) = ahb_plan::plane_geometry(
                    setup.video_format,
                    planes.n,
                    &planes.data,
                    &planes.row_stride,
                );
                gst_video::VideoMeta::add_full(
                    buffer,
                    gst_video::VideoFrameFlags::empty(),
                    setup.video_format,
                    setup.width,
                    setup.height,
                    &offsets,
                    &strides,
                )
                .map_err(|_| gst::FlowError::Error)?
                .set_alignment(&setup.align)
                .map_err(|_| gst::FlowError::Error)?;
            }
            Ok(buffer)
        }

        /// Back into the free list, so lock it again for the next decoder
        /// write. A frame that reached the sink was unlocked there; one the
        /// appsink dropped never was, and the re-lock is a no-op for it.
        fn reset_buffer(&self, buffer: &mut gst::BufferRef) {
            if let Some(mem) = ahb_memory(buffer) {
                unsafe {
                    if !(*mem).relock() {
                        // the meta no longer describes the mapping; the pool
                        // is poisoned but the buffer must still go back
                        warn!("android ahb lane: a pooled buffer lost its mapping");
                    }
                }
            }
            self.parent_reset_buffer(buffer);
        }
    }
}

glib::wrapper! {
    pub struct AhbPool(ObjectSubclass<pool_imp::AhbPool>) @extends gst::BufferPool, gst::Object;
}

impl Default for AhbPool {
    fn default() -> Self {
        glib::Object::new()
    }
}

// ---------------------------------------------------------------------------
// the sink's side
// ---------------------------------------------------------------------------

/// Whether this buffer's memory came out of the lane's pool.
///
/// One pointer compare against the process's single allocator. No downcast,
/// no map, no allocation, which is what makes it cheap enough to ask on every
/// frame, and it is the whole detection: a gralloc pool's buffers arrive
/// under ordinary `video/x-raw` caps, so the caps say nothing and only the
/// memory's allocator does.
fn ahb_memory(buffer: &gst::BufferRef) -> Option<*const AhbMemory> {
    if buffer.n_memory() != 1 {
        return None;
    }
    let mem = buffer.peek_memory(0);
    let raw = mem.as_ptr();
    let ours = AhbAllocator::get().as_ptr() as *mut gst::ffi::GstAllocator;
    (unsafe { (*raw).allocator } == ours).then_some(raw as *const AhbMemory)
}

/// The AHardwareBuffer behind a pooled frame, for [`crate::android_ahb_gl`].
///
/// Borrowed, not owned: the import takes its own gralloc reference and the
/// sink keeps the gst buffer alive for as long as the frame is on screen, so
/// nothing here hands out a pointer whose lifetime is not already pinned by
/// the caller's buffer.
pub fn hardware_buffer(buffer: &gst::BufferRef) -> Option<*mut ndk_sys::AHardwareBuffer> {
    ahb_memory(buffer).map(|mem| unsafe { (*mem).buffer })
}

/// The gralloc allocation's own extent and whether it has to bind to
/// `GL_TEXTURE_EXTERNAL_OES`.
///
/// This is the texture's size, not the picture's: the decoder's trailing
/// padding is part of the allocation and the renderer clips it away with a
/// source rect. The picture is always at the origin, which is what
/// [`ahb_plan::alignment_is_supported`] buys.
pub fn texture_geometry(buffer: &gst::BufferRef) -> Option<(u32, u32, bool)> {
    let mem = ahb_memory(buffer)?;
    let mut desc = ndk_sys::AHardwareBuffer_Desc {
        width: 0,
        height: 0,
        layers: 0,
        format: 0,
        usage: 0,
        stride: 0,
        rfu0: 0,
        rfu1: 0,
    };
    unsafe { ndk_sys::AHardwareBuffer_describe((*mem).buffer, &mut desc) };
    if desc.width == 0 || desc.height == 0 {
        return None;
    }
    // Only RGBA binds as an ordinary 2D texture. Every YUV layout is external
    // by the EGL spec, whatever the driver privately does with it.
    let external = desc.format != AhbFormat::Rgba8888.raw();
    Some((desc.width, desc.height, external))
}

/// Publishes the decoder's writes before the GPU reads them.
///
/// Called once per frame at the sink, which is the first moment the picture
/// is known to be complete and the last one before the import samples it. On
/// a device with coherent gralloc this is free; on one without it, it is the
/// difference between the frame and half of it.
pub fn finish_write(buffer: &gst::BufferRef) {
    if let Some(mem) = ahb_memory(buffer) {
        unsafe { (*mem).unlock() };
    }
}

// ---------------------------------------------------------------------------
// the proposal
// ---------------------------------------------------------------------------

/// Answers a software decoder's ALLOCATION query with a pool of gralloc
/// buffers.
///
/// Returns whether the query gained a pool. The decoder is free to ignore it,
/// and then nothing changes: it allocates its own system memory and the frame
/// takes the conversion path. Every gate is in [`ahb_plan::plan`], which is
/// tested on the host.
pub fn propose(query: &mut gst::query::Allocation) -> bool {
    // owned, because the query is written to below and the caps borrow it
    let (Some(caps), need_pool) = query.get_owned() else {
        return false;
    };
    let env = LiveEnv(available());
    let Some(plan) = ahb_plan::plan(&env, &caps, need_pool) else {
        return false;
    };

    let pool = AhbPool::default();
    let mut config = pool.config();
    config.set_params(Some(&caps), plan.size_estimate, plan.min_buffers, 0);
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
    if pool.set_config(config).is_err() {
        refuse("the pool would not take the lane's config");
        return false;
    }
    // set_config replaced the estimate with what gralloc really gave
    let Some((_, size, _, _)) = pool.config().params() else {
        refuse("the pool lost its own config");
        return false;
    };

    query.add_allocation_pool(Some(&pool), size, plan.min_buffers, 0);
    if query
        .find_allocation_meta::<gst_video::VideoMeta>()
        .is_none()
    {
        query.add_allocation_meta::<gst_video::VideoMeta>(None);
    }
    info!(
        %plan.video_format,
        plan.width,
        plan.height,
        size,
        "android ahb lane: proposed a gralloc pool"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan module spells the NDK's numbers out by hand so it can be read
    /// on a host. The const asserts above pin them at compile time; this
    /// keeps a readable failure if they ever drift.
    #[test]
    fn the_format_numbers_match_the_ndk() {
        assert_eq!(
            AhbFormat::Yuv420.raw(),
            ndk_sys::AHardwareBuffer_Format::AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420.0
        );
        assert_eq!(
            AhbFormat::Rgba8888.raw(),
            ndk_sys::AHardwareBuffer_Format::AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM.0
        );
    }

    /// The three usage bits the lane needs, and no others. A gralloc picks
    /// its layout from these, so an extra bit is a different allocation.
    #[test]
    fn the_usage_is_exactly_cpu_write_cpu_read_and_gpu_sampled() {
        assert_eq!(
            USAGE,
            (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN.0 as u64)
                | (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_CPU_READ_RARELY.0
                    as u64)
                | (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE.0
                    as u64)
        );
    }
}
