//! CUDA driver-API FFI for zero-copy NVDEC(CUDA)->Vulkan interop.
//!
//! The Vulkan side (libplacebo) creates exportable textures/semaphores and
//! gives us opaque fds; here we import those into CUDA, copy the decoded YUV
//! planes into them, and drive the shared timeline semaphore. Mirrors mpv's
//! `hwdec_cuda_vk`. Only compiled with the `fhs` feature; `libcuda` is
//! `dlopen`ed at runtime so the binary still links/runs without it (AMD/Intel).
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};

pub type CUdeviceptr = usize;
type CUresult = i32;
type CUexternalMemory = *mut c_void;
type CUmipmappedArray = *mut c_void;
pub type CUarray = *mut c_void;
type CUexternalSemaphore = *mut c_void;

const CUDA_SUCCESS: CUresult = 0;

// CUexternalMemoryHandleType
const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 1;
// CUDA_EXTERNAL_MEMORY_DEDICATED
const CUDA_EXTERNAL_MEMORY_DEDICATED: u32 = 1;
// CUexternalSemaphoreHandleType
const CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD: u32 = 9;
// CUarray_format
pub const CU_AD_FORMAT_UNSIGNED_INT8: u32 = 0x01;
pub const CU_AD_FORMAT_UNSIGNED_INT16: u32 = 0x02;
// CUmemorytype
const CU_MEMORYTYPE_DEVICE: u32 = 0x02;
const CU_MEMORYTYPE_ARRAY: u32 = 0x03;

// GstMapFlags: GST_MAP_READ = 1, GST_MAP_CUDA = 1 << 17.
const GST_MAP_READ: u32 = 1;
const GST_MAP_CUDA: u32 = 1 << 17;

#[repr(C)]
#[derive(Clone, Copy)]
struct Win32Handle {
    handle: *mut c_void,
    name: *const c_void,
}

#[repr(C)]
union ExtHandle {
    fd: i32,
    win32: Win32Handle,
    obj: *const c_void,
}

#[repr(C)]
struct CudaExternalMemoryHandleDesc {
    type_: u32,
    handle: ExtHandle,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

#[repr(C)]
struct CudaArray3dDescriptor {
    width: usize,
    height: usize,
    depth: usize,
    format: u32,
    num_channels: u32,
    flags: u32,
}

#[repr(C)]
struct CudaExternalMemoryMipmappedArrayDesc {
    offset: u64,
    array_desc: CudaArray3dDescriptor,
    num_levels: u32,
    reserved: [u32; 16],
}

#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

#[repr(C)]
struct CudaExternalSemaphoreHandleDesc {
    type_: u32,
    handle: ExtHandle,
    flags: u32,
    reserved: [u32; 16],
}

// The nested `params` struct is over-approximated: fence.value lives at offset
// 0 (all we set); the rest is zeroed and sized >= the C layout (72 bytes).
#[repr(C)]
struct SemParamsInner {
    fence_value: u64,
    _pad: [u32; 16],
}

#[repr(C)]
struct CudaExternalSemaphoreParams {
    params: SemParamsInner,
    flags: u32,
    reserved: [u32; 16],
}

// Mirrors GstMapInfo so we can map with the custom GST_MAP_CUDA flag.
#[repr(C)]
struct GstMapInfo {
    memory: *mut c_void,
    flags: u32,
    data: *mut u8,
    size: usize,
    maxsize: usize,
    user_data: [*mut c_void; 4],
    _gst_reserved: [*mut c_void; 4],
}

const CUDA_ERROR_NOT_LOADED: CUresult = 999;

type FnImportExternalMemory =
    unsafe extern "C" fn(*mut CUexternalMemory, *const CudaExternalMemoryHandleDesc) -> CUresult;
type FnExternalMemoryGetMappedMipmappedArray = unsafe extern "C" fn(
    *mut CUmipmappedArray,
    CUexternalMemory,
    *const CudaExternalMemoryMipmappedArrayDesc,
) -> CUresult;
type FnMipmappedArrayGetLevel =
    unsafe extern "C" fn(*mut CUarray, CUmipmappedArray, u32) -> CUresult;
type FnMipmappedArrayDestroy = unsafe extern "C" fn(CUmipmappedArray) -> CUresult;
type FnDestroyExternalMemory = unsafe extern "C" fn(CUexternalMemory) -> CUresult;
type FnMemcpy2DAsync = unsafe extern "C" fn(*const CudaMemcpy2D, *mut c_void) -> CUresult;
type FnImportExternalSemaphore =
    unsafe extern "C" fn(*mut CUexternalSemaphore, *const CudaExternalSemaphoreHandleDesc) -> CUresult;
type FnDestroyExternalSemaphore = unsafe extern "C" fn(CUexternalSemaphore) -> CUresult;
type FnExternalSemaphoresAsync = unsafe extern "C" fn(
    *const CUexternalSemaphore,
    *const CudaExternalSemaphoreParams,
    u32,
    *mut c_void,
) -> CUresult;

struct CudaDriver {
    import_external_memory: FnImportExternalMemory,
    external_memory_get_mapped_mipmapped_array: FnExternalMemoryGetMappedMipmappedArray,
    mipmapped_array_get_level: FnMipmappedArrayGetLevel,
    mipmapped_array_destroy: FnMipmappedArrayDestroy,
    destroy_external_memory: FnDestroyExternalMemory,
    memcpy_2d_async: FnMemcpy2DAsync,
    import_external_semaphore: FnImportExternalSemaphore,
    destroy_external_semaphore: FnDestroyExternalSemaphore,
    wait_external_semaphores_async: FnExternalSemaphoresAsync,
    signal_external_semaphores_async: FnExternalSemaphoresAsync,
}
unsafe impl Send for CudaDriver {}
unsafe impl Sync for CudaDriver {}

static CUDA_DRIVER: OnceLock<Option<CudaDriver>> = OnceLock::new();

fn cuda_driver() -> Option<&'static CudaDriver> {
    CUDA_DRIVER
        .get_or_init(|| unsafe {
            let lib = libloading::Library::new("libcuda.so.1")
                .or_else(|_| libloading::Library::new("libcuda.so"))
                .ok()?;
            let d = CudaDriver {
                import_external_memory: *lib.get(b"cuImportExternalMemory\0").ok()?,
                external_memory_get_mapped_mipmapped_array: *lib
                    .get(b"cuExternalMemoryGetMappedMipmappedArray\0")
                    .ok()?,
                mipmapped_array_get_level: *lib.get(b"cuMipmappedArrayGetLevel\0").ok()?,
                mipmapped_array_destroy: *lib.get(b"cuMipmappedArrayDestroy\0").ok()?,
                destroy_external_memory: *lib.get(b"cuDestroyExternalMemory\0").ok()?,
                memcpy_2d_async: *lib.get(b"cuMemcpy2DAsync_v2\0").ok()?,
                import_external_semaphore: *lib.get(b"cuImportExternalSemaphore\0").ok()?,
                destroy_external_semaphore: *lib.get(b"cuDestroyExternalSemaphore\0").ok()?,
                wait_external_semaphores_async: *lib
                    .get(b"cuWaitExternalSemaphoresAsync\0")
                    .ok()?,
                signal_external_semaphores_async: *lib
                    .get(b"cuSignalExternalSemaphoresAsync\0")
                    .ok()?,
            };
            std::mem::forget(lib);
            Some(d)
        })
        .as_ref()
}

unsafe fn cuImportExternalMemory(
    ext_mem: *mut CUexternalMemory,
    desc: *const CudaExternalMemoryHandleDesc,
) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.import_external_memory)(ext_mem, desc) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuExternalMemoryGetMappedMipmappedArray(
    mipmap: *mut CUmipmappedArray,
    ext_mem: CUexternalMemory,
    desc: *const CudaExternalMemoryMipmappedArrayDesc,
) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.external_memory_get_mapped_mipmapped_array)(mipmap, ext_mem, desc) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMipmappedArrayGetLevel(
    level_array: *mut CUarray,
    mipmap: CUmipmappedArray,
    level: u32,
) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.mipmapped_array_get_level)(level_array, mipmap, level) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMipmappedArrayDestroy(mipmap: CUmipmappedArray) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.mipmapped_array_destroy)(mipmap) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuDestroyExternalMemory(ext_mem: CUexternalMemory) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.destroy_external_memory)(ext_mem) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMemcpy2DAsync_v2(copy: *const CudaMemcpy2D, stream: *mut c_void) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.memcpy_2d_async)(copy, stream) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuImportExternalSemaphore(
    ext_sem: *mut CUexternalSemaphore,
    desc: *const CudaExternalSemaphoreHandleDesc,
) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.import_external_semaphore)(ext_sem, desc) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuDestroyExternalSemaphore(ext_sem: CUexternalSemaphore) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.destroy_external_semaphore)(ext_sem) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuWaitExternalSemaphoresAsync(
    ext_sems: *const CUexternalSemaphore,
    params: *const CudaExternalSemaphoreParams,
    num: u32,
    stream: *mut c_void,
) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.wait_external_semaphores_async)(ext_sems, params, num, stream) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}
unsafe fn cuSignalExternalSemaphoresAsync(
    ext_sems: *const CUexternalSemaphore,
    params: *const CudaExternalSemaphoreParams,
    num: u32,
    stream: *mut c_void,
) -> CUresult {
    match cuda_driver() {
        Some(d) => unsafe { (d.signal_external_semaphores_async)(ext_sems, params, num, stream) },
        None => CUDA_ERROR_NOT_LOADED,
    }
}

type FnIsCudaMemory = unsafe extern "C" fn(*mut c_void) -> i32;
type FnCudaMemorySync = unsafe extern "C" fn(*mut c_void);

struct GstCudaMem {
    is_cuda_memory: FnIsCudaMemory,
    memory_sync: FnCudaMemorySync,
}
unsafe impl Send for GstCudaMem {}
unsafe impl Sync for GstCudaMem {}

static GST_CUDA_MEM: OnceLock<Option<GstCudaMem>> = OnceLock::new();

fn gst_cuda_mem() -> Option<&'static GstCudaMem> {
    GST_CUDA_MEM
        .get_or_init(|| unsafe {
            let lib = libloading::Library::new("libgstcuda-1.0.so.0")
                .or_else(|_| libloading::Library::new("libgstcuda-1.0.so"))
                .ok()?;
            let g = GstCudaMem {
                is_cuda_memory: *lib.get(b"gst_is_cuda_memory\0").ok()?,
                memory_sync: *lib.get(b"gst_cuda_memory_sync\0").ok()?,
            };
            std::mem::forget(lib);
            Some(g)
        })
        .as_ref()
}

unsafe fn gst_is_cuda_memory(mem: *mut c_void) -> i32 {
    match gst_cuda_mem() {
        Some(g) => unsafe { (g.is_cuda_memory)(mem) },
        None => 0,
    }
}
unsafe fn gst_cuda_memory_sync(mem: *mut c_void) {
    if let Some(g) = gst_cuda_mem() {
        unsafe { (g.memory_sync)(mem) }
    }
}

#[link(name = "gstreamer-1.0")]
unsafe extern "C" {
    fn gst_buffer_map(buffer: *mut c_void, info: *mut GstMapInfo, flags: u32) -> i32;
    fn gst_buffer_unmap(buffer: *mut c_void, info: *mut GstMapInfo);
}

/// A CUDA array aliasing a libplacebo-exported Vulkan texture's memory. The
/// decoded YUV plane is copied into this via [`copy_into`].
pub struct CudaExtImage {
    ext_mem: CUexternalMemory,
    mipmap: CUmipmappedArray,
    array: CUarray,
}

impl CudaExtImage {
    /// Import an opaque-fd Vulkan texture (from `pl_tex.shared_mem`) as a CUDA
    /// array. `ad_format`/`num_channels` describe the plane (e.g. R16 =>
    /// UNSIGNED_INT16 x1, RG16 => UNSIGNED_INT16 x2). Must be called with the
    /// CUDA context pushed.
    ///
    /// # Safety
    /// `fd` must be a valid opaque-fd export of a Vulkan image of the given
    /// dimensions/format; ownership of `fd` transfers to CUDA on success.
    pub unsafe fn import(
        fd: i32,
        size: u64,
        width: usize,
        height: usize,
        ad_format: u32,
        num_channels: u32,
        dedicated: bool,
    ) -> Result<Self> {
        let mem_desc = CudaExternalMemoryHandleDesc {
            type_: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            handle: ExtHandle { fd },
            size,
            flags: if dedicated {
                CUDA_EXTERNAL_MEMORY_DEDICATED
            } else {
                0
            },
            reserved: [0; 16],
        };
        let mut ext_mem: CUexternalMemory = std::ptr::null_mut();
        let r = unsafe { cuImportExternalMemory(&mut ext_mem, &mem_desc) };
        if r != CUDA_SUCCESS {
            return Err(anyhow!("cuImportExternalMemory failed: {r}"));
        }

        let arr_desc = CudaExternalMemoryMipmappedArrayDesc {
            offset: 0,
            array_desc: CudaArray3dDescriptor {
                width,
                height,
                depth: 0,
                format: ad_format,
                num_channels,
                flags: 0,
            },
            num_levels: 1,
            reserved: [0; 16],
        };
        let mut mipmap: CUmipmappedArray = std::ptr::null_mut();
        let r =
            unsafe { cuExternalMemoryGetMappedMipmappedArray(&mut mipmap, ext_mem, &arr_desc) };
        if r != CUDA_SUCCESS {
            unsafe { cuDestroyExternalMemory(ext_mem) };
            return Err(anyhow!("cuExternalMemoryGetMappedMipmappedArray failed: {r}"));
        }
        let mut array: CUarray = std::ptr::null_mut();
        let r = unsafe { cuMipmappedArrayGetLevel(&mut array, mipmap, 0) };
        if r != CUDA_SUCCESS {
            unsafe {
                cuMipmappedArrayDestroy(mipmap);
                cuDestroyExternalMemory(ext_mem);
            }
            return Err(anyhow!("cuMipmappedArrayGetLevel failed: {r}"));
        }
        Ok(Self {
            ext_mem,
            mipmap,
            array,
        })
    }

    /// Copy a decoded plane (device pointer + pitch) into this array.
    /// Must run with the CUDA context pushed.
    ///
    /// # Safety
    /// `src_device`..`+src_pitch*height` must be valid device memory.
    pub unsafe fn copy_into(
        &self,
        src_device: CUdeviceptr,
        src_pitch: usize,
        width_in_bytes: usize,
        height: usize,
    ) -> Result<()> {
        let mut copy: CudaMemcpy2D = unsafe { std::mem::zeroed() };
        copy.src_memory_type = CU_MEMORYTYPE_DEVICE;
        copy.src_device = src_device;
        copy.src_pitch = src_pitch;
        copy.dst_memory_type = CU_MEMORYTYPE_ARRAY;
        copy.dst_array = self.array;
        copy.width_in_bytes = width_in_bytes;
        copy.height = height;
        let r = unsafe { cuMemcpy2DAsync_v2(&copy, std::ptr::null_mut()) };
        if r != CUDA_SUCCESS {
            return Err(anyhow!("cuMemcpy2DAsync failed: {r}"));
        }
        Ok(())
    }
}

impl Drop for CudaExtImage {
    fn drop(&mut self) {
        unsafe {
            cuMipmappedArrayDestroy(self.mipmap);
            cuDestroyExternalMemory(self.ext_mem);
        }
    }
}

/// A CUDA view of a libplacebo-exported Vulkan timeline semaphore, for
/// ordering the CUDA copy against libplacebo's Vulkan render.
pub struct CudaExtSemaphore {
    ext_sem: CUexternalSemaphore,
}

impl CudaExtSemaphore {
    /// # Safety
    /// `fd` must be an opaque-fd export of a Vulkan timeline semaphore;
    /// ownership transfers to CUDA on success.
    pub unsafe fn import(fd: i32) -> Result<Self> {
        let desc = CudaExternalSemaphoreHandleDesc {
            type_: CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD,
            handle: ExtHandle { fd },
            flags: 0,
            reserved: [0; 16],
        };
        let mut ext_sem: CUexternalSemaphore = std::ptr::null_mut();
        let r = unsafe { cuImportExternalSemaphore(&mut ext_sem, &desc) };
        if r != CUDA_SUCCESS {
            return Err(anyhow!("cuImportExternalSemaphore failed: {r}"));
        }
        Ok(Self { ext_sem })
    }

    /// Enqueue a wait for the timeline to reach `value` (on the default stream).
    pub fn wait(&self, value: u64) -> Result<()> {
        let mut params: CudaExternalSemaphoreParams = unsafe { std::mem::zeroed() };
        params.params.fence_value = value;
        let r = unsafe {
            cuWaitExternalSemaphoresAsync(&self.ext_sem, &params, 1, std::ptr::null_mut())
        };
        if r != CUDA_SUCCESS {
            return Err(anyhow!("cuWaitExternalSemaphoresAsync failed: {r}"));
        }
        Ok(())
    }

    /// Enqueue a signal of the timeline to `value` (on the default stream).
    pub fn signal(&self, value: u64) -> Result<()> {
        let mut params: CudaExternalSemaphoreParams = unsafe { std::mem::zeroed() };
        params.params.fence_value = value;
        let r = unsafe {
            cuSignalExternalSemaphoresAsync(&self.ext_sem, &params, 1, std::ptr::null_mut())
        };
        if r != CUDA_SUCCESS {
            return Err(anyhow!("cuSignalExternalSemaphoresAsync failed: {r}"));
        }
        Ok(())
    }
}

impl Drop for CudaExtSemaphore {
    fn drop(&mut self) {
        unsafe { cuDestroyExternalSemaphore(self.ext_sem) };
    }
}

/// True if `mem` (raw `*mut GstMemory`) is CUDA memory.
///
/// # Safety
/// `mem` must be a valid `*mut GstMemory`.
pub unsafe fn is_cuda_memory(mem: *mut c_void) -> bool {
    unsafe { gst_is_cuda_memory(mem) != 0 }
}

/// Wait for the decoder's writes to `mem` to complete.
///
/// # Safety
/// `mem` must be a valid CUDA `*mut GstMemory`.
pub unsafe fn sync_memory(mem: *mut c_void) {
    unsafe { gst_cuda_memory_sync(mem) };
}

/// Maps a CUDA buffer, yielding the base device pointer. Unmaps on drop.
pub struct CudaBufferMap {
    buffer: *mut c_void,
    info: GstMapInfo,
    pub base: CUdeviceptr,
}

impl CudaBufferMap {
    /// # Safety
    /// `buffer` must be a live `*mut GstBuffer` backed by CUDA memory.
    pub unsafe fn map(buffer: *mut c_void) -> Result<Self> {
        let mut info: GstMapInfo = unsafe { std::mem::zeroed() };
        let ok = unsafe { gst_buffer_map(buffer, &mut info, GST_MAP_READ | GST_MAP_CUDA) };
        if ok == 0 {
            return Err(anyhow!("gst_buffer_map(GST_MAP_CUDA) failed"));
        }
        let base = info.data as CUdeviceptr;
        Ok(Self {
            buffer,
            info,
            base,
        })
    }
}

impl Drop for CudaBufferMap {
    fn drop(&mut self) {
        unsafe { gst_buffer_unmap(self.buffer, &mut self.info) };
    }
}
